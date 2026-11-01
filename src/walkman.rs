//! Volume detection, live disk budgets, and persisted sync settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::paths;
use crate::storage;

pub const HEADROOM_BYTES: u64 = 512 * 1024 * 1024;
pub const COPY_SPACE_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
pub const DEST_INTERNAL: &str = "internal";
pub const DEST_SD: &str = "sd";
pub const DEST_AUTO: &str = "auto";
pub const KIND_CINDER: &str = "cinder";
pub const KIND_ANDROID: &str = "android-walkman";
pub const PENDING_DEVICE_ID: &str = "pending";
pub const APP_TITLE: &str = "Walkman Sync";

const DRIVE_REMOVABLE: u32 = 2;
const WALKMAN_MARKERS: &[&str] = &[
    ".scrobbler.log",
    "cinder_loved.tsv",
    "cinder_liked.conf",
    "cinder_liked_import.tsv",
    "listen_sources.conf",
    "OMGAUDIO",
    "default-capability.xml",
    "DevIcon.fil",
    "DevLogo.fil",
];
const SD_LABELS: &[&str] = &["32 GB", "SD", "SD CARD", "SD_CARD"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub internal_serial: String,
    #[serde(default)]
    pub sd_serial: String,
    #[serde(default)]
    pub internal_drive: String,
    #[serde(default)]
    pub sd_drive: String,
    #[serde(default = "default_dest")]
    pub dest: String,
    #[serde(default = "default_kind")]
    pub device_kind: String,
    #[serde(default)]
    pub internal_max_gb: f64,
    #[serde(default)]
    pub sd_max_gb: f64,
}

fn default_dest() -> String {
    DEST_AUTO.to_string()
}
fn default_kind() -> String {
    KIND_CINDER.to_string()
}

impl Default for DeviceProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            internal_serial: String::new(),
            sd_serial: String::new(),
            internal_drive: String::new(),
            sd_drive: String::new(),
            dest: DEST_AUTO.to_string(),
            device_kind: KIND_CINDER.to_string(),
            internal_max_gb: 0.0,
            sd_max_gb: 0.0,
        }
    }
}

impl DeviceProfile {
    pub fn primary_serial(&self) -> &str {
        if !self.internal_serial.is_empty() {
            &self.internal_serial
        } else {
            &self.sd_serial
        }
    }

    pub fn matches(&self, internal_serial: &str, sd_serial: &str) -> bool {
        let live: Vec<&str> = [internal_serial, sd_serial]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        if live.is_empty() {
            return false;
        }
        live.iter().any(|s| {
            (!self.internal_serial.is_empty() && self.internal_serial == *s)
                || (!self.sd_serial.is_empty() && self.sd_serial == *s)
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncSettings {
    #[serde(default = "default_source")]
    pub source_dir: String,
    #[serde(default = "default_source")]
    pub playlist_dir: String,
    #[serde(default)]
    pub excluded_albums: String,
    #[serde(default = "default_workers")]
    pub copy_workers: u32,
    #[serde(default = "default_true")]
    pub auto_detect_drives: bool,
    #[serde(default)]
    pub active_device: String,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceProfile>,
}

fn default_source() -> String {
    r"\\WillineauVault\Music".into()
}
fn default_workers() -> u32 {
    1
}
fn default_true() -> bool {
    true
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            source_dir: default_source(),
            playlist_dir: default_source(),
            excluded_albums: String::new(),
            copy_workers: 1,
            auto_detect_drives: true,
            active_device: String::new(),
            devices: BTreeMap::new(),
        }
    }
}

impl SyncSettings {
    pub fn load() -> Self {
        Self::load_from(&paths::settings_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_default();
        let data: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
        let mut settings = SyncSettings::default();
        if let Some(s) = data.get("source_dir").and_then(|v| v.as_str()) {
            settings.source_dir = s.to_string();
        }
        if let Some(s) = data.get("playlist_dir").and_then(|v| v.as_str()) {
            settings.playlist_dir = s.to_string();
        }
        if let Some(s) = data.get("excluded_albums").and_then(|v| v.as_str()) {
            settings.excluded_albums = s.to_string();
        }
        if let Some(n) = data.get("copy_workers").and_then(|v| v.as_u64()) {
            settings.copy_workers = n.max(1) as u32;
        }
        if let Some(b) = data.get("auto_detect_drives").and_then(|v| v.as_bool()) {
            settings.auto_detect_drives = b;
        }
        if let Some(s) = data.get("active_device").and_then(|v| v.as_str()) {
            settings.active_device = s.to_string();
        }
        if let Some(map) = data.get("devices").and_then(|v| v.as_object()) {
            if !map.is_empty() {
                for (key, value) in map {
                    if let Ok(profile) = serde_json::from_value::<DeviceProfile>(value.clone()) {
                        settings.devices.insert(key.clone(), profile);
                    }
                }
            }
        }
        if settings.devices.is_empty() {
            settings.migrate_flat(&data);
        }
        settings.normalize();
        settings
    }

    fn migrate_flat(&mut self, data: &serde_json::Value) {
        let internal = data
            .get("internal_drive")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sd = data
            .get("sd_drive")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let (internal_serial, sd_serial) = live_serials(&internal, &sd);
        let key = if !internal_serial.is_empty() {
            internal_serial.clone()
        } else if !sd_serial.is_empty() {
            sd_serial.clone()
        } else {
            PENDING_DEVICE_ID.to_string()
        };
        self.devices.insert(
            key.clone(),
            DeviceProfile {
                name: default_device_name(&internal, &sd, &[]),
                internal_serial,
                sd_serial,
                internal_drive: internal,
                sd_drive: sd,
                dest: data
                    .get("dest")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEST_AUTO)
                    .to_string(),
                device_kind: data
                    .get("device_kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or(KIND_CINDER)
                    .to_string(),
                internal_max_gb: data
                    .get("internal_max_gb")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                sd_max_gb: data
                    .get("sd_max_gb")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
            },
        );
        self.active_device = key;
    }

    fn normalize(&mut self) {
        self.copy_workers = self.copy_workers.max(1);
        if !self.active_device.is_empty() && !self.devices.contains_key(&self.active_device) {
            if let Some(first) = self.devices.keys().next().cloned() {
                self.active_device = first;
            }
        }
        if self.active_device.is_empty() {
            if let Some(first) = self.devices.keys().next().cloned() {
                self.active_device = first;
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&paths::settings_path())
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        fs::write(path, text + "\n")
    }

    pub fn active_profile(&self) -> Option<&DeviceProfile> {
        self.devices.get(&self.active_device)
    }

    pub fn active_profile_mut(&mut self) -> &mut DeviceProfile {
        if !self.devices.contains_key(&self.active_device) {
            let key = if self.active_device.is_empty() {
                PENDING_DEVICE_ID.to_string()
            } else {
                self.active_device.clone()
            };
            self.devices.insert(
                key.clone(),
                DeviceProfile {
                    name: "Walkman".into(),
                    ..DeviceProfile::default()
                },
            );
            self.active_device = key;
        }
        self.devices.get_mut(&self.active_device).unwrap()
    }

    pub fn internal_drive(&self) -> String {
        self.active_profile()
            .map(|p| p.internal_drive.clone())
            .unwrap_or_default()
    }
    pub fn sd_drive(&self) -> String {
        self.active_profile()
            .map(|p| p.sd_drive.clone())
            .unwrap_or_default()
    }
    pub fn dest(&self) -> String {
        self.active_profile()
            .map(|p| p.dest.clone())
            .unwrap_or_else(|| DEST_AUTO.into())
    }
    pub fn device_kind(&self) -> String {
        self.active_profile()
            .map(|p| p.device_kind.clone())
            .unwrap_or_else(|| KIND_CINDER.into())
    }
    pub fn device_name(&self) -> String {
        self.active_profile()
            .map(|p| p.name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "walkman".into())
    }
    pub fn other_device_names(&self) -> Vec<String> {
        self.devices
            .iter()
            .filter(|(k, _)| *k != &self.active_device)
            .map(|(k, p)| {
                if p.name.is_empty() {
                    k.clone()
                } else {
                    p.name.clone()
                }
            })
            .collect()
    }
}

/// Tests drop `.sony-sync-serial` in a volume so two temp dirs can be distinct serials
/// even when they share a Windows drive letter.
fn serial_marker(path: &str) -> Option<String> {
    let mut current = PathBuf::from(path);
    for _ in 0..8 {
        let marker = current.join(".sony-sync-serial");
        if let Ok(text) = fs::read_to_string(&marker) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if !current.pop() {
            break;
        }
    }
    None
}

pub fn source_serial(path: &str) -> String {
    let folded = storage::fold_path(&std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)).to_string_lossy());
    let mut hasher = Sha1::new();
    hasher.update(folded.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

pub fn get_volume_serial(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Some(marker) = serial_marker(path) {
        return marker;
    }
    if path.starts_with("adb://") {
        let id = path[6..].split('/').next().unwrap_or("");
        return format!("adb-{id}");
    }
    if path.starts_with("mtp://") {
        let id = path[6..].split('/').next().unwrap_or("");
        return format!("mtp-{id}");
    }
    let root = drive_root_of(path);
    let (_, serial) = volume_information(&root);
    if serial != 0 {
        return format!("{serial:08x}");
    }
    if let Some(uuid) = macos_volume_uuid(&root) {
        return uuid;
    }
    source_serial(path)
}

pub fn get_volume_label(drive_root: &str) -> String {
    if drive_root.is_empty() || drive_root.starts_with("adb://") || drive_root.starts_with("mtp://")
    {
        return String::new();
    }
    let (label, _) = volume_information(drive_root);
    if !label.is_empty() {
        return label;
    }
    if cfg!(not(windows)) {
        return Path::new(&drive_root_of(drive_root))
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    String::new()
}

pub fn volume_label_for(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    get_volume_label(&drive_root_of(path))
}

fn volume_information(root: &str) -> (String, u32) {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
        if root.is_empty() {
            return (String::new(), 0);
        }
        let wide: Vec<u16> = std::ffi::OsStr::new(root)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut label = [0u16; 261];
        let mut serial = 0u32;
        let mut max_comp = 0u32;
        let mut flags = 0u32;
        let mut fs = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                &mut max_comp,
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return (String::new(), 0);
        }
        let len = label.iter().position(|&c| c == 0).unwrap_or(label.len());
        (
            String::from_utf16_lossy(&label[..len]).trim().to_string(),
            serial,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = root;
        (String::new(), 0)
    }
}

fn macos_volume_uuid(root: &str) -> Option<String> {
    if !cfg!(target_os = "macos") || root.is_empty() {
        return None;
    }
    let out = Command::new("diskutil")
        .args(["info", root])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.split("Volume UUID:").nth(1) {
            let uuid = rest.trim();
            if !uuid.is_empty() {
                return Some(uuid.to_string());
            }
        }
    }
    None
}

pub fn drive_root_of(path: &str) -> String {
    if path.is_empty() || path.starts_with("adb://") || path.starts_with("mtp://") {
        return path.to_string();
    }
    #[cfg(windows)]
    {
        let p = PathBuf::from(path);
        p.components()
            .next()
            .map(|c| {
                let mut s = c.as_os_str().to_string_lossy().into_owned();
                if !s.ends_with('\\') {
                    s.push('\\');
                }
                s
            })
            .unwrap_or_else(|| path.to_string())
    }
    #[cfg(not(windows))]
    {
        let mut current = PathBuf::from(path);
        if current.is_file() {
            current = current.parent().unwrap_or(&current).to_path_buf();
        }
        let Ok(meta) = fs::metadata(&current) else {
            return path.to_string();
        };
        use std::os::unix::fs::MetadataExt;
        let dev = meta.dev();
        loop {
            let Some(parent) = current.parent() else { break };
            let Ok(pm) = fs::metadata(parent) else { break };
            if pm.dev() != dev {
                break;
            }
            current = parent.to_path_buf();
        }
        current.to_string_lossy().into_owned()
    }
}

pub fn resolve_library_path(path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    if Path::new(path).is_dir() || Path::new(path).is_file() {
        return path.to_string();
    }
    if cfg!(windows) {
        return path.to_string();
    }
    let folded = path.replace('/', "\\");
    if !folded.starts_with("\\\\") {
        return path.to_string();
    }
    let parts: Vec<&str> = folded.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return path.to_string();
    }
    let share = parts[1];
    let rest = &parts[2..];
    let mut candidates = vec![
        PathBuf::from("/Volumes").join(share),
        PathBuf::from("/Volumes").join(share).join(rest.iter().collect::<PathBuf>()),
    ];
    candidates.push(PathBuf::from("/Volumes").join(parts[0]).join(share));
    for c in candidates {
        if c.is_dir() {
            return c.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

pub fn music_folder_names(android: bool) -> &'static [&'static str] {
    if android {
        &["Music", "MUSIC"]
    } else {
        &["MUSIC", "Music"]
    }
}

pub fn resolve_music_path(drive_root: &str, android: bool) -> String {
    let names = music_folder_names(android);
    if drive_root.starts_with("adb://") || drive_root.starts_with("mtp://") {
        let folded = drive_root.replace('\\', "/");
        let leaf = folded.rsplit('/').next().unwrap_or("");
        if leaf.eq_ignore_ascii_case("music") {
            return drive_root.trim_end_matches(['/', '\\']).to_string();
        }
        for name in names {
            let c = storage::join(drive_root, &[name]);
            if storage::isdir(&c) {
                return c;
            }
        }
        return storage::join(drive_root, &[names[0]]);
    }
    for name in names {
        let c = Path::new(drive_root).join(name);
        if c.is_dir() {
            return c.to_string_lossy().into_owned();
        }
    }
    Path::new(drive_root)
        .join(names[0])
        .to_string_lossy()
        .into_owned()
}

pub fn volume_root(music_path: &str) -> String {
    if music_path.starts_with("adb://") || music_path.starts_with("mtp://") {
        let folded = music_path.replace('\\', "/");
        let folded = folded.trim_end_matches('/');
        if folded.rsplit('/').next().unwrap_or("").eq_ignore_ascii_case("music") {
            return storage::parent(folded);
        }
        return folded.to_string();
    }
    Path::new(music_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| drive_root_of(music_path))
}

pub fn has_music_folder(drive_root: &str) -> bool {
    Path::new(drive_root).join("MUSIC").is_dir() || Path::new(drive_root).join("Music").is_dir()
}

pub fn has_walkman_marker(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    for dir in marker_dirs(path) {
        for name in WALKMAN_MARKERS {
            if storage::exists(&storage::join(&dir, &[name])) {
                return true;
            }
        }
    }
    false
}

fn marker_dirs(path: &str) -> Vec<String> {
    let mut dirs = vec![path.to_string()];
    let leaf = Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if leaf.eq_ignore_ascii_case("music") {
        let parent = storage::parent(path);
        if !parent.is_empty() && storage::fold_path(&parent) != storage::fold_path(path) {
            dirs.push(parent);
        }
    }
    dirs
}

fn volume_present(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    storage::isdir(path) || marker_dirs(path).iter().any(|dir| storage::isdir(dir))
}

/// Kind from files on a mounted volume. The volume label and the path
/// (`D:\`, `adb://`, `mtp://`) are not evidence.
pub fn evident_device_kind(internal_root: &str, sd_root: &str) -> Option<&'static str> {
    let mounted: Vec<&str> = [internal_root, sd_root]
        .into_iter()
        .filter(|path| volume_present(path))
        .collect();
    if mounted.is_empty() {
        return None;
    }
    if mounted.iter().any(|path| has_walkman_marker(path)) {
        Some(KIND_CINDER)
    } else {
        Some(KIND_ANDROID)
    }
}

pub fn infer_device_kind(internal_root: &str, sd_root: &str) -> String {
    evident_device_kind(internal_root, sd_root)
        .unwrap_or(KIND_CINDER)
        .to_string()
}

#[derive(Clone, Debug)]
pub struct VolumeCandidate {
    pub root: String,
    pub label: String,
    pub total: u64,
    pub free: u64,
    pub drive_type: u32,
}

fn collect_candidates() -> Vec<VolumeCandidate> {
    #[cfg(windows)]
    {
        collect_windows()
    }
    #[cfg(target_os = "macos")]
    {
        collect_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Vec::new()
    }
}

#[cfg(windows)]
fn collect_windows() -> Vec<VolumeCandidate> {
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let mask = unsafe { GetLogicalDrives() };
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let system_root = if system.ends_with(':') {
        format!("{system}\\")
    } else {
        system
    };
    let mut found = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let drive = format!("{letter}:\\");
        if storage::fold_path(&drive) == storage::fold_path(&system_root) {
            continue;
        }
        let Some((total, free)) = storage::disk_usage(&drive) else {
            continue;
        };
        let wide: Vec<u16> = drive.encode_utf16().chain(Some(0)).collect();
        let drive_type = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if matches!(drive_type, 1 | 4 | 5 | 6) {
            continue;
        }
        found.push(VolumeCandidate {
            root: drive.clone(),
            label: get_volume_label(&drive),
            total,
            free,
            drive_type,
        });
    }
    found
}

#[cfg(target_os = "macos")]
fn collect_macos() -> Vec<VolumeCandidate> {
    use std::os::unix::fs::MetadataExt;
    let volumes = Path::new("/Volumes");
    if !volumes.is_dir() {
        return Vec::new();
    }
    let boot_dev = fs::metadata("/").map(|m| m.dev()).unwrap_or(u64::MAX);
    let mut found = Vec::new();
    let Ok(read) = fs::read_dir(volumes) else {
        return found;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else { continue };
        if !meta.is_dir() || meta.dev() == boot_dev {
            continue;
        }
        let root = path.to_string_lossy().into_owned();
        let Some((total, free)) = storage::disk_usage(&root) else {
            continue;
        };
        found.push(VolumeCandidate {
            label: entry.file_name().to_string_lossy().into_owned(),
            root,
            total,
            free,
            drive_type: DRIVE_REMOVABLE,
        });
    }
    found
}

fn is_walkman(c: &VolumeCandidate) -> bool {
    has_walkman_marker(&c.root)
}

pub fn detect_walkman_roots() -> (String, String) {
    let candidates = collect_candidates();
    let walkman = candidates.iter().find(|c| is_walkman(c));
    let internal = walkman.map(|c| c.root.clone()).unwrap_or_default();
    let others: Vec<&VolumeCandidate> = candidates
        .iter()
        .filter(|c| internal.is_empty() || storage::fold_path(&c.root) != storage::fold_path(&internal))
        .collect();
    let mut card: Option<&VolumeCandidate> = None;
    if !internal.is_empty() {
        card = others
            .iter()
            .copied()
            .filter(|c| c.drive_type == DRIVE_REMOVABLE)
            .max_by_key(|c| c.total);
    }
    if card.is_none() {
        card = others
            .iter()
            .copied()
            .filter(|c| {
                SD_LABELS
                    .iter()
                    .any(|l| c.label.eq_ignore_ascii_case(l))
            })
            .max_by_key(|c| c.total);
    }
    if card.is_none() {
        card = others
            .iter()
            .copied()
            .filter(|c| c.drive_type == DRIVE_REMOVABLE && has_music_folder(&c.root))
            .max_by_key(|c| c.total);
    }
    let sd = card.map(|c| c.root.clone()).unwrap_or_default();
    (internal, sd)
}

pub fn detect_walkman_paths(android: bool) -> (String, String) {
    let (internal_root, sd_root) = detect_walkman_roots();
    let internal = if internal_root.is_empty() {
        String::new()
    } else {
        resolve_music_path(&internal_root, android)
    };
    let sd = if sd_root.is_empty() {
        String::new()
    } else {
        resolve_music_path(&sd_root, android)
    };
    (internal, sd)
}

pub fn live_serials(internal_drive: &str, sd_drive: &str) -> (String, String) {
    (
        if internal_drive.is_empty() {
            String::new()
        } else {
            get_volume_serial(internal_drive)
        },
        if sd_drive.is_empty() {
            String::new()
        } else {
            get_volume_serial(sd_drive)
        },
    )
}

pub fn default_device_name(internal_drive: &str, sd_drive: &str, existing: &[String]) -> String {
    let mut label = String::new();
    for path in [internal_drive, sd_drive] {
        if path.is_empty() {
            continue;
        }
        if path.starts_with("mtp://") {
            if let Some(id) = path[6..].split('/').next() {
                if !id.is_empty() {
                    label = id.to_string();
                    break;
                }
            }
            continue;
        }
        if path.starts_with("adb://") {
            continue;
        }
        label = get_volume_label(&drive_root_of(path));
        if !label.is_empty() {
            break;
        }
    }
    let base = if label.trim().is_empty() {
        "Walkman".to_string()
    } else {
        label.trim().to_string()
    };
    let taken: Vec<String> = existing.iter().map(|s| s.to_ascii_lowercase()).collect();
    if !taken.contains(&base.to_ascii_lowercase()) {
        return base;
    }
    let mut index = 2;
    loop {
        let name = format!("{base} {index}");
        if !taken.contains(&name.to_ascii_lowercase()) {
            return name;
        }
        index += 1;
    }
}

pub fn bind_live_device(
    settings: &mut SyncSettings,
    internal_drive: &str,
    sd_drive: &str,
    inferred_kind: &str,
) -> (bool, bool) {
    if internal_drive.is_empty() && sd_drive.is_empty() {
        return (false, false);
    }
    let (internal_serial, sd_serial) = live_serials(internal_drive, sd_drive);
    let primary = if !internal_serial.is_empty() {
        internal_serial.clone()
    } else {
        sd_serial.clone()
    };
    if primary.is_empty() {
        return (false, false);
    }
    let matches: Vec<String> = settings
        .devices
        .iter()
        .filter(|(_, p)| p.matches(&internal_serial, &sd_serial))
        .map(|(k, _)| k.clone())
        .collect();
    let mut created = false;
    let key = if !matches.is_empty() {
        if matches.contains(&settings.active_device) {
            settings.active_device.clone()
        } else if !internal_serial.is_empty() {
            matches
                .iter()
                .find(|k| settings.devices[*k].internal_serial == internal_serial)
                .cloned()
                .unwrap_or_else(|| matches[0].clone())
        } else {
            matches[0].clone()
        }
    } else if settings.devices.contains_key(PENDING_DEVICE_ID) {
        let profile = settings.devices.remove(PENDING_DEVICE_ID).unwrap();
        settings.devices.insert(primary.clone(), profile);
        primary.clone()
    } else {
        created = true;
        let names: Vec<String> = settings.devices.values().map(|p| p.name.clone()).collect();
        settings.devices.insert(
            primary.clone(),
            DeviceProfile {
                name: default_device_name(internal_drive, sd_drive, &names),
                device_kind: if inferred_kind == KIND_ANDROID {
                    KIND_ANDROID.into()
                } else {
                    KIND_CINDER.into()
                },
                dest: DEST_AUTO.into(),
                ..DeviceProfile::default()
            },
        );
        primary.clone()
    };
    let before = {
        let profile = settings.devices.get_mut(&key).unwrap();
        let before = (
            settings.active_device.clone(),
            profile.internal_drive.clone(),
            profile.sd_drive.clone(),
            profile.internal_serial.clone(),
            profile.sd_serial.clone(),
        );
        if !internal_drive.is_empty() {
            profile.internal_drive = internal_drive.to_string();
            if !internal_serial.is_empty() {
                profile.internal_serial = internal_serial;
            }
        } else {
            profile.internal_drive.clear();
        }
        if !sd_drive.is_empty() {
            profile.sd_drive = sd_drive.to_string();
            if !sd_serial.is_empty() {
                profile.sd_serial = sd_serial;
            }
        } else {
            profile.sd_drive.clear();
        }
        if let Some(kind) = evident_device_kind(internal_drive, sd_drive) {
            profile.device_kind = kind.to_string();
        } else if created {
            profile.device_kind = inferred_kind.to_string();
        }
        before
    };
    if settings.devices.get(&key).map(|p| p.name.is_empty()).unwrap_or(false) {
        let names: Vec<String> = settings
            .devices
            .iter()
            .filter(|(k, _)| *k != &key)
            .map(|(_, p)| p.name.clone())
            .collect();
        let name = default_device_name(internal_drive, sd_drive, &names);
        settings.devices.get_mut(&key).unwrap().name = name;
    }
    settings.active_device = key.clone();
    let profile = settings.devices.get(&key).unwrap();
    let after = (
        settings.active_device.clone(),
        profile.internal_drive.clone(),
        profile.sd_drive.clone(),
        profile.internal_serial.clone(),
        profile.sd_serial.clone(),
    );
    (created, created || before != after)
}

pub fn live_budget_bytes(path: &str, held: u64) -> u64 {
    let Some((_, free)) = storage::disk_usage(path) else {
        return 0;
    };
    (free + held).saturating_sub(HEADROOM_BYTES)
}

pub fn live_total_bytes(path: &str) -> u64 {
    storage::disk_usage(path).map(|(t, _)| t).unwrap_or(0)
}

pub fn pick_dest(dest: &str, internal_drive: &str, sd_drive: &str) -> String {
    match dest {
        DEST_SD => sd_drive.to_string(),
        DEST_INTERNAL => internal_drive.to_string(),
        _ => {
            let sd_total = live_total_bytes(sd_drive);
            let internal_total = live_total_bytes(internal_drive);
            if sd_total > internal_total {
                sd_drive.to_string()
            } else if sd_total == 0
                && internal_total == 0
                && !sd_drive.is_empty()
                && (storage::is_remote(sd_drive) || storage::is_remote(internal_drive))
            {
                // MTP/ADB cannot report capacity; Android internals are tiny, so prefer SD.
                sd_drive.to_string()
            } else {
                internal_drive.to_string()
            }
        }
    }
}

pub fn apply_listen_credentials(internal: &str, sd: &str) -> Vec<String> {
    let keys = [
        ("lastfm_user", "LASTFM_USERNAME"),
        ("lastfm_api_key", "LASTFM_API_KEY"),
        ("lastfm_api_secret", "LASTFM_API_SECRET"),
        ("lastfm_session_key", "LASTFM_SESSION_KEY"),
        ("audiomuse_url", "AUDIOMUSE_URL"),
        ("audiomuse_token", "AUDIOMUSE_TOKEN"),
    ];
    let mut applied = Vec::new();
    for root in [internal, sd] {
        if root.is_empty() {
            continue;
        }
        let vol = volume_root(root);
        for candidate in [
            storage::join(root, &["listen_sources.conf"]),
            storage::join(&vol, &["listen_sources.conf"]),
        ] {
            let Ok(text) = storage::read_text(&candidate) else {
                continue;
            };
            let parsed = parse_listen_sources(&text);
            if parsed.is_empty() {
                continue;
            }
            for (src, env) in keys {
                if let Some(value) = parsed.get(src) {
                    if !value.is_empty() {
                        std::env::set_var(env, value);
                        applied.push(env.to_string());
                    }
                }
            }
            return applied;
        }
    }
    applied
}

pub fn parse_listen_sources(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        if !value.is_empty() {
            values.insert(key.trim().to_string(), value.to_string());
        }
    }
    values
}

pub fn describe_mount(path: &str) -> String {
    if path.is_empty() {
        return "(none)".into();
    }
    if storage::is_remote(path) {
        return path.to_string();
    }
    let label = storage::label(path);
    let serial = storage::serial(path);
    if label.is_empty() {
        format!("{path}  [{serial}]")
    } else {
        format!("{path}  {label} [{serial}]")
    }
}

pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

pub fn human_rate(bps: f64) -> String {
    let b = bps.max(0.0);
    if b < 1024.0 {
        format!("{b:.0} B/s")
    } else if b < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", b / 1024.0)
    } else if b < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB/s", b / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB/s", b / (1024.0 * 1024.0 * 1024.0))
    }
}

pub fn human_elapsed(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0s".into();
    }
    let s = secs.round() as u64;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    }
}

pub fn human_eta(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "estimating…".into();
    }
    let s = secs.round() as u64;
    if s < 1 {
        "<1s left".into()
    } else if s < 60 {
        format!("{s}s left")
    } else if s < 3600 {
        format!("{}:{:02} left", s / 60, s % 60)
    } else {
        format!("{}:{:02}:{:02} left", s / 3600, (s / 60) % 60, s % 60)
    }
}

pub fn human_eta_display(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "estimating…".into();
    }
    let quantized = if secs < 60.0 {
        secs.round()
    } else if secs < 600.0 {
        (secs / 5.0).round() * 5.0
    } else {
        (secs / 15.0).round() * 15.0
    };
    human_eta(quantized)
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn mounted_music_without_cinder_files_is_android() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("Music")).unwrap();
        let path = root.join("Music").to_string_lossy().into_owned();
        assert_eq!(evident_device_kind(&path, ""), Some(KIND_ANDROID));
        assert_eq!(infer_device_kind(&path, ""), KIND_ANDROID);
    }

    #[test]
    fn unmounted_path_is_not_classified() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing").join("MUSIC");
        let path = missing.to_string_lossy().into_owned();
        assert_eq!(evident_device_kind(&path, ""), None);
    }

    #[test]
    fn cinder_markers_beside_music_override_android_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("MUSIC")).unwrap();
        fs::write(root.join("cinder_loved.tsv"), "x").unwrap();
        let music = root.join("MUSIC").to_string_lossy().into_owned();
        assert_eq!(infer_device_kind(&music, ""), KIND_CINDER);
        assert_eq!(evident_device_kind(&music, ""), Some(KIND_CINDER));

        fs::write(root.join(".sony-sync-serial"), "silver").unwrap();
        let mut settings = SyncSettings::default();
        settings.devices.insert(
            "silver".into(),
            DeviceProfile {
                name: "Silver Walkman".into(),
                device_kind: KIND_ANDROID.into(),
                internal_serial: "silver".into(),
                ..DeviceProfile::default()
            },
        );
        settings.active_device = "silver".into();
        bind_live_device(&mut settings, &music, "", KIND_CINDER);
        assert_eq!(settings.device_kind(), KIND_CINDER);
    }

    #[test]
    fn kind_ignores_volume_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("MUSIC")).unwrap();
        fs::write(root.join("default-capability.xml"), "<c/>").unwrap();
        let music = root.join("MUSIC").to_string_lossy().into_owned();
        assert_eq!(evident_device_kind(&music, ""), Some(KIND_CINDER));
    }
}

#[cfg(test)]
mod eta_tests {
    use super::{human_elapsed, human_eta, human_eta_display, human_rate};

    #[test]
    fn formats_eta() {
        assert_eq!(human_eta(0.2), "<1s left");
        assert_eq!(human_eta(12.0), "12s left");
        assert_eq!(human_eta(90.0), "1:30 left");
        assert_eq!(human_eta(3661.0), "1:01:01 left");
    }

    #[test]
    fn display_eta_quantizes_above_a_minute() {
        assert_eq!(human_eta_display(12.0), "12s left");
        assert_eq!(human_eta_display(222.0), "3:40 left");
        assert_eq!(human_eta_display(3661.0), "1:01:00 left");
    }

    #[test]
    fn formats_elapsed() {
        assert_eq!(human_elapsed(0.2), "0s");
        assert_eq!(human_elapsed(12.0), "12s");
        assert_eq!(human_elapsed(90.0), "1:30");
        assert_eq!(human_elapsed(3661.0), "1:01:01");
    }

    #[test]
    fn formats_rate() {
        assert_eq!(human_rate(800.0), "800 B/s");
        assert_eq!(human_rate(12.5 * 1024.0 * 1024.0), "12.5 MB/s");
    }
}
