//! Dest I/O: local filesystem, adb://, or mtp://.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::walkman;

pub const AUDIO_EXT: &[&str] = &[
    ".flac", ".wav", ".mp3", ".m4a", ".aac", ".alac", ".dsf", ".dff", ".aiff", ".aif",
];
pub const PLAYLIST_EXT: &[&str] = &[".m3u", ".m3u8"];

const SKIP_NAMES: &[&str] = &[
    ".sony-sync-manifest.tsv",
    "liked songs.m3u8",
    "liked songs.m3u",
    ".scrobbler.log",
    "scrobble.logs",
    "cinder_playlists",
    "cinder_palettes",
    "listen_sources.conf",
    "listen_sync_state.json",
];

const IGNORED_DIRS: &[&str] = &[
    "$recycle.bin",
    "#recycle",
    "@eadir",
    ".trashes",
    ".spotlight-v100",
    ".temporaryitems",
    "system volume information",
    ".git",
];

const IGNORED_NAMES: &[&str] = &[".ds_store", "thumbs.db", "desktop.ini"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileKeep {
    Audio,
    Unwanted,
}

/// Audio that belongs in the catalog / copy plan. Rejects AppleDouble, `.partial`,
/// and OS junk even when the name happens to end with an audio suffix.
pub fn is_audio(name: &str) -> bool {
    if is_ignored_name(name) {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    AUDIO_EXT.iter().any(|e| lower.ends_with(e))
}

pub fn is_playlist(name: &str) -> bool {
    if name.to_ascii_lowercase().starts_with("._") {
        return false;
    }
    let lower = name.to_ascii_lowercase();
    PLAYLIST_EXT.iter().any(|e| lower.ends_with(e))
}

pub fn is_ignored_dir(name: &str) -> bool {
    IGNORED_DIRS.contains(&name.to_ascii_lowercase().as_str())
}

pub fn is_preserved_dest(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("cinder_")
        || lower.starts_with("listen_")
        || SKIP_NAMES.contains(&lower.as_str())
}

fn is_ignored_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("._")
        || lower.ends_with(".partial")
        || IGNORED_NAMES.contains(&lower.as_str())
}

fn rel_file_name(rel: &str) -> &str {
    Path::new(rel)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(rel)
}

fn rel_is_nested(rel: &str) -> bool {
    Path::new(rel).components().count() > 1
}

fn rel_has_skipped_component(rel: &str, dest: bool) -> bool {
    Path::new(rel).components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        is_ignored_dir(&name) || (dest && is_preserved_dest(&name))
    })
}

fn keep_walk_entry(rel: &str, dest: bool, keep: FileKeep) -> bool {
    if rel_has_skipped_component(rel, dest) {
        return false;
    }
    let name = rel_file_name(rel);
    if dest && is_preserved_dest(name) {
        return false;
    }
    match keep {
        FileKeep::Audio => is_audio(name),
        FileKeep::Unwanted => {
            if is_audio(name) {
                return false;
            }
            // Root MUSIC playlists are rewritten by playlist_dir; leave them.
            rel_is_nested(rel)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Fs,
    Adb,
    Mtp,
}

pub fn is_remote(path: &str) -> bool {
    path.starts_with("adb://") || path.starts_with("mtp://")
}

pub fn scheme(path: &str) -> Scheme {
    if path.starts_with("adb://") {
        Scheme::Adb
    } else if path.starts_with("mtp://") {
        Scheme::Mtp
    } else {
        Scheme::Fs
    }
}

pub fn norm(path: &str) -> String {
    if is_remote(path) {
        path.replace('\\', "/").trim_end_matches('/').to_string()
    } else {
        fold_path(path)
    }
}

pub fn fold_path(path: &str) -> String {
    let p = PathBuf::from(path);
    #[cfg(windows)]
    {
        p.to_string_lossy().replace('/', "\\").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        p.to_string_lossy().into_owned()
    }
}

pub fn fold_rel(rel: &str) -> String {
    let n = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
    #[cfg(windows)]
    {
        n.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        n
    }
}

pub fn join(base: &str, parts: &[&str]) -> String {
    let extra: Vec<String> = parts
        .iter()
        .copied()
        .filter(|p| !p.is_empty())
        .map(|p| p.replace('\\', "/").trim_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if is_remote(base) {
        if extra.is_empty() {
            return base.trim_end_matches('/').to_string();
        }
        format!(
            "{}/{}",
            base.replace('\\', "/").trim_end_matches('/'),
            extra.join("/")
        )
    } else if extra.is_empty() {
        base.to_string()
    } else {
        let mut p = PathBuf::from(base);
        for part in extra {
            p.push(part);
        }
        p.to_string_lossy().into_owned()
    }
}

pub fn parent(path: &str) -> String {
    if is_remote(path) {
        let folded = path.replace('\\', "/");
        let folded = folded.trim_end_matches('/');
        match folded.rsplit_once('/') {
            Some((head, _)) if head.matches('/').count() > 2 => head.to_string(),
            Some((head, _)) => head.to_string(),
            None => path.to_string(),
        }
    } else {
        Path::new(path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

pub fn isdir(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    match scheme(path) {
        Scheme::Adb => adb_isdir(path),
        Scheme::Mtp => mtp_isdir(path),
        Scheme::Fs => Path::new(path).is_dir(),
    }
}

pub fn exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    match scheme(path) {
        Scheme::Adb => adb_exists(path),
        Scheme::Mtp => mtp_isdir(path),
        Scheme::Fs => Path::new(path).exists(),
    }
}

pub fn serial(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    match scheme(path) {
        Scheme::Adb => {
            let rest = &path[6..];
            let id = rest.split('/').next().unwrap_or(rest);
            format!("adb-{id}")
        }
        Scheme::Mtp => {
            let rest = &path[6..];
            let id = rest.split('/').next().unwrap_or(rest);
            format!("mtp-{id}")
        }
        Scheme::Fs => walkman::get_volume_serial(path),
    }
}

pub fn label(path: &str) -> String {
    match scheme(path) {
        Scheme::Adb => "adb".into(),
        Scheme::Mtp => "WALKMAN".into(),
        Scheme::Fs => walkman::volume_label_for(path),
    }
}

pub fn disk_usage(path: &str) -> Option<(u64, u64)> {
    if path.is_empty() {
        return None;
    }
    match scheme(path) {
        Scheme::Fs => fs2_usage(path),
        Scheme::Mtp => mtp_disk_usage(path),
        Scheme::Adb => adb_disk_usage(path),
    }
}

/// Drop a cached ADB/MTP capacity row so the next `disk_usage` hits the device.
pub fn invalidate_disk_usage(path: &str) {
    if path.is_empty() {
        return;
    }
    match scheme(path) {
        Scheme::Adb => {
            let key = fold_path(path);
            if let Ok(mut cache) = ADB_USAGE.lock() {
                cache.retain(|(k, _)| k != &key);
            }
        }
        Scheme::Mtp => invalidate_mtp_usage(path),
        Scheme::Fs => {}
    }
}

/// Re-read volume capacity after dest files change. Safe to call from a worker
/// after copies/deletes finish (not during an in-flight `adb push`).
pub fn refresh_disk_usage(path: &str) -> Option<(u64, u64)> {
    invalidate_disk_usage(path);
    disk_usage(path)
}

fn invalidate_mtp_usage(path: &str) {
    #[cfg(windows)]
    {
        let Some((device, _)) = parse_mtp(path) else {
            return;
        };
        if let Ok(mut cache) = MTP_USAGE.lock() {
            if cache.as_ref().is_some_and(|hit| hit.device == device) {
                *cache = None;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

fn fs2_usage(path: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = std::ffi::OsStr::new(path)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut free = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free)
        };
        if ok == 0 {
            None
        } else {
            Some((total, free))
        }
    }
    #[cfg(unix)]
    {
        let out = Command::new("df").args(["-k", "-P", path]).output().ok()?;
        let line = String::from_utf8_lossy(&out.stdout).lines().nth(1)?.to_string();
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            return None;
        }
        let total = cols[1].parse::<u64>().ok()?.saturating_mul(1024);
        let avail = cols[3].parse::<u64>().ok()?.saturating_mul(1024);
        Some((total, avail))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        None
    }
}

pub fn walk_files(root: &str, dest: bool) -> Vec<(String, u64, f64)> {
    walk_entries(root, dest, FileKeep::Audio)
}

/// Nested non-audio leftovers on dest MUSIC (cover art, sidecars, nested playlists).
/// Root-level `.m3u`/`.m3u8` and preserved `cinder_*` / likes / scrobble files are omitted.
pub fn walk_dest_unwanted(root: &str) -> Vec<String> {
    walk_entries(root, true, FileKeep::Unwanted)
        .into_iter()
        .map(|(rel, _, _)| rel)
        .collect()
}

fn walk_entries(root: &str, dest: bool, keep: FileKeep) -> Vec<(String, u64, f64)> {
    match scheme(root) {
        Scheme::Adb => adb_walk(root, dest, keep),
        Scheme::Mtp => mtp_walk(root, dest, keep),
        Scheme::Fs => walk_fs(root, dest, keep),
    }
}

fn walk_fs(root: &str, dest: bool, keep: FileKeep) -> Vec<(String, u64, f64)> {
    let mut entries = Vec::new();
    let root_path = Path::new(root);
    if !root_path.is_dir() {
        return entries;
    }
    let mut stack = vec![root_path.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = fs::read_dir(&current) else {
            continue;
        };
        for item in read.flatten() {
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            let Ok(ft) = item.file_type() else {
                continue;
            };
            if ft.is_dir() {
                if is_ignored_dir(&name) || (dest && is_preserved_dest(&name)) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() {
                let rel = path
                    .strip_prefix(root_path)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if !keep_walk_entry(&rel, dest, keep) {
                    continue;
                }
                let Ok(meta) = item.metadata() else {
                    continue;
                };
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                entries.push((rel, meta.len(), mtime));
            }
        }
    }
    entries
}

pub fn read_text(path: &str) -> io::Result<String> {
    match scheme(path) {
        Scheme::Fs => fs::read_to_string(path),
        Scheme::Adb => adb_read(path),
        Scheme::Mtp => Err(io::Error::other("mtp read not implemented in rust path")),
    }
}

pub fn write_text(path: &str, body: &str) -> io::Result<()> {
    match scheme(path) {
        Scheme::Fs => {
            if let Some(dir) = Path::new(path).parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, body)
        }
        Scheme::Adb => adb_write(path, body),
        Scheme::Mtp => Err(io::Error::other("mtp write not implemented in rust path")),
    }
}

pub fn remove(path: &str) -> io::Result<()> {
    match scheme(path) {
        Scheme::Fs => fs::remove_file(path),
        Scheme::Adb => adb_remove(path),
        Scheme::Mtp => Err(io::Error::other("mtp remove not implemented")),
    }
}

pub fn makedirs(path: &str) -> io::Result<()> {
    match scheme(path) {
        Scheme::Fs => fs::create_dir_all(path),
        Scheme::Adb => adb_mkdir(path),
        Scheme::Mtp => Ok(()),
    }
}

pub fn local_file(path: &str) -> Option<PathBuf> {
    if is_remote(path) {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

pub fn now_mtime(path: &str) -> f64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0)
        })
}

fn adb_bin() -> String {
    crate::adb::adb_bin()
}

fn parse_adb(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("adb://")?;
    let (serial, path) = rest.split_once('/')?;
    Some((serial.to_string(), format!("/{path}")))
}

fn adb_command() -> Command {
    let mut cmd = Command::new(adb_bin());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

fn adb_shell(serial: &str, command: &str) -> String {
    adb_command()
        .args(["-s", serial, "shell", command])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).replace("\r\n", "\n"))
        .unwrap_or_default()
}

fn df_block_bytes(header: &str) -> u64 {
    let h = header.to_ascii_lowercase();
    if h.contains("512-blocks") || h.contains("512-byte") {
        512
    } else {
        1024
    }
}

/// POSIX `df -P` reports 512-byte blocks; `df -kP` reports 1024-byte blocks.
fn parse_df_p(raw: &str) -> Option<(u64, u64)> {
    let mut unit = 1024u64;
    let mut data: Option<&str> = None;
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.to_ascii_lowercase().starts_with("filesystem") {
            unit = df_block_bytes(t);
            continue;
        }
        data = Some(t);
    }
    let cols: Vec<&str> = data?.split_whitespace().collect();
    if cols.len() < 4 {
        return None;
    }
    let total = cols[1].parse::<u64>().ok()?.saturating_mul(unit);
    let free = cols[3].parse::<u64>().ok()?.saturating_mul(unit);
    (total > 0).then_some((total, free))
}

struct AdbUsageCache {
    total: u64,
    free: u64,
    at: Instant,
}

const REMOTE_STAT_TTL: Duration = Duration::from_secs(30);

static ADB_USAGE: Mutex<Vec<(String, AdbUsageCache)>> = Mutex::new(Vec::new());

struct AdbDirCache {
    present: bool,
    at: Instant,
}

static ADB_DIR: Mutex<Vec<(String, AdbDirCache)>> = Mutex::new(Vec::new());

fn adb_disk_usage(uri: &str) -> Option<(u64, u64)> {
    let key = fold_path(uri);
    if let Ok(cache) = ADB_USAGE.lock() {
        if let Some((_, hit)) = cache.iter().find(|(k, _)| k == &key) {
            if hit.at.elapsed() < REMOTE_STAT_TTL {
                return Some((hit.total, hit.free));
            }
        }
    }
    let (serial, path) = parse_adb(uri)?;
    let raw = adb_shell(&serial, &format!("df -kP \"{path}\" 2>/dev/null"));
    let (total, free) = parse_df_p(&raw).or_else(|| {
        parse_df_p(&adb_shell(
            &serial,
            &format!("df -P \"{path}\" 2>/dev/null"),
        ))
    })?;
    if let Ok(mut cache) = ADB_USAGE.lock() {
        cache.retain(|(k, _)| k != &key);
        cache.push((
            key,
            AdbUsageCache {
                total,
                free,
                at: Instant::now(),
            },
        ));
        if cache.len() > 8 {
            cache.remove(0);
        }
    }
    Some((total, free))
}

fn adb_cached_dir(uri: &str) -> bool {
    let key = fold_path(uri);
    if let Ok(cache) = ADB_DIR.lock() {
        if let Some((_, hit)) = cache.iter().find(|(k, _)| k == &key) {
            if hit.at.elapsed() < REMOTE_STAT_TTL {
                return hit.present;
            }
        }
    }
    let Some((serial, path)) = parse_adb(uri) else {
        return false;
    };
    let present = adb_shell(&serial, &format!("if [ -d \"{path}\" ]; then echo 1; fi")).trim() == "1";
    if let Ok(mut cache) = ADB_DIR.lock() {
        cache.retain(|(k, _)| k != &key);
        cache.push((
            key,
            AdbDirCache {
                present,
                at: Instant::now(),
            },
        ));
        if cache.len() > 8 {
            cache.remove(0);
        }
    }
    present
}

fn adb_isdir(uri: &str) -> bool {
    adb_cached_dir(uri)
}

fn adb_exists(uri: &str) -> bool {
    let Some((serial, path)) = parse_adb(uri) else {
        return false;
    };
    adb_shell(&serial, &format!("if [ -e \"{path}\" ]; then echo 1; fi")).trim() == "1"
}

fn adb_read(uri: &str) -> io::Result<String> {
    let (serial, path) = parse_adb(uri).ok_or_else(|| io::Error::other("bad adb uri"))?;
    let out = adb_command()
        .args(["-s", &serial, "exec-out", "cat", &path])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("adb cat failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn adb_write(uri: &str, body: &str) -> io::Result<()> {
    let (serial, path) = parse_adb(uri).ok_or_else(|| io::Error::other("bad adb uri"))?;
    let tmp = std::env::temp_dir().join("walkman-sync-adb.tmp");
    fs::write(&tmp, body)?;
    let status = adb_command()
        .args(["-s", &serial, "push", &tmp.to_string_lossy(), &path])
        .status()?;
    let _ = fs::remove_file(&tmp);
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("adb push failed"))
    }
}

fn adb_remove(uri: &str) -> io::Result<()> {
    let (serial, path) = parse_adb(uri).ok_or_else(|| io::Error::other("bad adb uri"))?;
    adb_shell(&serial, &format!("rm -f \"{path}\""));
    Ok(())
}

fn adb_mkdir(uri: &str) -> io::Result<()> {
    let (serial, path) = parse_adb(uri).ok_or_else(|| io::Error::other("bad adb uri"))?;
    adb_shell(&serial, &format!("mkdir -p \"{path}\""));
    Ok(())
}

fn parse_adb_stat_listing(
    raw: &str,
    root: &str,
    dest: bool,
    keep: FileKeep,
) -> Vec<(String, u64, f64)> {
    let prefix = format!("{}/", root.trim_end_matches('/'));
    let mut entries = Vec::new();
    for line in raw.lines() {
        let mut parts = line.trim().splitn(3, ' ');
        let Some(size_s) = parts.next() else { continue };
        let Some(mtime_s) = parts.next() else { continue };
        let Some(abs_path) = parts.next() else { continue };
        let Ok(size) = size_s.parse::<u64>() else {
            continue;
        };
        let mtime = mtime_s.parse::<f64>().unwrap_or(0.0);
        let rel = if let Some(stripped) = abs_path.strip_prefix(&prefix) {
            stripped.to_string()
        } else if abs_path == root || abs_path == root.trim_end_matches('/') {
            continue;
        } else {
            Path::new(abs_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let rel = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
        if rel.is_empty() || !keep_walk_entry(&rel, dest, keep) {
            continue;
        }
        entries.push((rel, size, mtime));
    }
    entries
}

fn adb_walk(root: &str, dest: bool, keep: FileKeep) -> Vec<(String, u64, f64)> {
    let Some((serial, path)) = parse_adb(root) else {
        return Vec::new();
    };
    let stat_out = adb_shell(
        &serial,
        &format!(
            "toybox find \"{path}\" -type f -exec toybox stat -c \"%s %Y %n\" {{}} + 2>/dev/null"
        ),
    );
    let mut entries = parse_adb_stat_listing(&stat_out, &path, dest, keep);
    if !entries.is_empty() {
        return entries;
    }
    let listing = adb_shell(
        &serial,
        &format!("find \"{path}\" -type f -printf '%P\\t%s\\t%T@\\n' 2>/dev/null"),
    );
    entries = parse_mtp_walk_output(&listing)
        .into_iter()
        .filter(|(rel, _, _)| keep_walk_entry(rel, dest, keep))
        .collect();
    entries
}

pub fn detect_android_mounts() -> (String, String) {
    let adb = detect_adb();
    if !adb.0.is_empty() || !adb.1.is_empty() {
        return adb;
    }
    detect_mtp()
}

fn looks_like_walkman_model(model: &str) -> bool {
    let upper = model.to_ascii_uppercase();
    upper.starts_with("NW-") || upper.contains("WALKMAN") || upper.contains("ZX")
}

fn looks_like_sd_storage(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "emulated" | "self" | "sdcard0" | "sdcard1"
    ) {
        return false;
    }
    if lower.contains("sd") {
        return true;
    }
    let b = name.as_bytes();
    b.len() == 9
        && b[4] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_hexdigit())
}

fn detect_adb() -> (String, String) {
    let output = adb_command().arg("devices").output().ok();
    let Some(output) = output else {
        return (String::new(), String::new());
    };
    if !output.status.success() {
        return (String::new(), String::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut serials = Vec::new();
    for line in text.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(id) = parts.next() else { continue };
        let Some(state) = parts.next() else { continue };
        if state == "device" {
            serials.push(id.to_string());
        }
    }
    let mut chosen = String::new();
    for serial in &serials {
        let model = adb_shell(serial, "getprop ro.product.model");
        if looks_like_walkman_model(&model) {
            chosen = serial.clone();
            break;
        }
        if serials.len() == 1 {
            chosen = serial.clone();
        }
    }
    if chosen.is_empty() {
        return (String::new(), String::new());
    }
    let mut internal = String::new();
    for candidate in ["/sdcard/Music", "/storage/emulated/0/Music"] {
        if adb_shell(&chosen, &format!("if [ -d \"{candidate}\" ]; then echo 1; fi")).trim()
            == "1"
        {
            internal = format!("adb://{chosen}{candidate}");
            break;
        }
        adb_shell(&chosen, &format!("mkdir -p \"{candidate}\""));
        internal = format!("adb://{chosen}{candidate}");
        break;
    }
    let mut sd = String::new();
    let listing = adb_shell(&chosen, "ls /storage 2>/dev/null");
    for name in listing.split_whitespace() {
        if !looks_like_sd_storage(name) {
            continue;
        }
        let music = format!("/storage/{name}/Music");
        let probe = format!("if [ -d \"{music}\" ] || [ -d \"/storage/{name}\" ]; then echo 1; fi");
        if adb_shell(&chosen, &probe).trim() == "1" {
            adb_shell(&chosen, &format!("mkdir -p \"{music}\""));
            sd = format!("adb://{chosen}{music}");
            break;
        }
    }
    (internal, sd)
}

/// Parse Explorer "This PC" MTP listing: `DEVICE\\tstorage\\tstorage...`.
pub fn parse_mtp_listing(raw: &str) -> (String, String) {
    let mut internal = String::new();
    let mut sd = String::new();
    for line in raw.lines() {
        let mut parts = line.split('\t').map(str::trim).filter(|s| !s.is_empty());
        let Some(device) = parts.next() else {
            continue;
        };
        let names: Vec<&str> = parts.collect();
        if names.is_empty() {
            continue;
        }
        for name in &names {
            let music = format!("mtp://{device}/{name}/Music");
            let lower = name.to_ascii_lowercase();
            if lower.contains("internal") || lower.contains("shared") || lower == "phone" {
                internal = music;
            } else {
                sd = music;
            }
        }
        if internal.is_empty() {
            internal = format!("mtp://{device}/{}/Music", names[0]);
        }
        if !internal.is_empty() {
            break;
        }
    }
    (internal, sd)
}

fn detect_mtp() -> (String, String) {
    #[cfg(windows)]
    {
        detect_mtp_windows()
    }
    #[cfg(not(windows))]
    {
        (String::new(), String::new())
    }
}

#[cfg(windows)]
fn powershell_output(script: &str) -> Option<String> {
    powershell_invoke(["-NoProfile", "-STA", "-Command", script])
}

#[cfg(windows)]
struct TempPath(PathBuf);

#[cfg(windows)]
impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
fn powershell_file(script: &str) -> Option<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "walkman-sync-mtp-{}-{nanos}.ps1",
        std::process::id()
    ));
    fs::write(&path, script).ok()?;
    let _guard = TempPath(path.clone());
    powershell_invoke([
        "-NoProfile",
        "-STA",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        path.to_str()?,
    ])
}

#[cfg(windows)]
fn powershell_invoke<'a>(args: impl IntoIterator<Item = &'a str>) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("powershell")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn detect_mtp_windows() -> (String, String) {
    const SCRIPT: &str = r#"
$shell = New-Object -ComObject Shell.Application
$ns = $shell.NameSpace(0x11)
if (-not $ns) { exit }
foreach ($i in $ns.Items()) {
  $n = $i.Name
  if ($n -match 'WALKMAN|NW-') {
    $storages = @()
    try { foreach ($s in $i.GetFolder.Items()) { $storages += $s.Name } } catch {}
    Write-Output ($n + "`t" + ($storages -join "`t"))
  }
}
"#;
    let Some(text) = powershell_output(SCRIPT) else {
        return (String::new(), String::new());
    };
    parse_mtp_listing(&text)
}

fn parse_mtp(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("mtp://")?;
    let (device, rel) = match rest.split_once('/') {
        Some((d, r)) => (d, r.replace('\\', "/")),
        None => (rest, String::new()),
    };
    if device.is_empty() {
        None
    } else {
        Some((device.to_string(), rel))
    }
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn mtp_isdir(uri: &str) -> bool {
    #[cfg(windows)]
    {
        mtp_probe(uri)
    }
    #[cfg(not(windows))]
    {
        let _ = uri;
        false
    }
}

#[cfg(windows)]
struct MtpPresentCache {
    device: String,
    present: bool,
    at: Instant,
}

#[cfg(windows)]
static MTP_PRESENT: Mutex<Option<MtpPresentCache>> = Mutex::new(None);

#[cfg(windows)]
fn mtp_probe(uri: &str) -> bool {
    let Some((device, _rel)) = parse_mtp(uri) else {
        return false;
    };
    if let Ok(cache) = MTP_PRESENT.lock() {
        if let Some(hit) = cache.as_ref() {
            if hit.device == device && hit.at.elapsed() < REMOTE_STAT_TTL {
                return hit.present;
            }
        }
    }
    let script = format!(
        "$s = New-Object -ComObject Shell.Application; \
         $n = $s.NameSpace(17); if (-not $n) {{ '0'; exit }}; \
         $hit = $n.Items() | Where-Object {{ $_.Name -eq {} }}; \
         if ($hit) {{ '1' }} else {{ '0' }}",
        ps_quote(&device)
    );
    let present = powershell_output(&script)
        .map(|o| o.trim() == "1")
        .unwrap_or(false);
    if let Ok(mut cache) = MTP_PRESENT.lock() {
        *cache = Some(MtpPresentCache {
            device,
            present,
            at: Instant::now(),
        });
    }
    present
}

fn parse_mtp_usage(raw: &str) -> Vec<(String, u64, u64)> {
    let mut rows = Vec::new();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(name) = parts.next() else { continue };
        let Some(total_s) = parts.next() else { continue };
        let Some(free_s) = parts.next() else { continue };
        let Ok(total) = total_s.parse::<u64>() else {
            continue;
        };
        let Ok(free) = free_s.parse::<u64>() else {
            continue;
        };
        if name.is_empty() || total == 0 {
            continue;
        }
        rows.push((name.to_string(), total, free));
    }
    rows
}

fn mtp_storage_name(rel: &str) -> Option<&str> {
    rel.split('/').find(|part| !part.is_empty())
}

fn mtp_disk_usage(uri: &str) -> Option<(u64, u64)> {
    #[cfg(windows)]
    {
        mtp_disk_usage_windows(uri)
    }
    #[cfg(not(windows))]
    {
        let _ = uri;
        None
    }
}

#[cfg(windows)]
struct MtpUsageCache {
    device: String,
    rows: Vec<(String, u64, u64)>,
    at: Instant,
}

#[cfg(windows)]
static MTP_USAGE: Mutex<Option<MtpUsageCache>> = Mutex::new(None);

#[cfg(windows)]
fn mtp_usage_script(device: &str) -> String {
    format!(
        "$shell = New-Object -ComObject Shell.Application; \
         $ns = $shell.NameSpace(0x11); if (-not $ns) {{ exit }}; \
         $dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {device}) {{ $dev = $i; break }} }}; \
         if (-not $dev) {{ exit }}; \
         foreach ($s in $dev.GetFolder.Items()) {{ \
           $cap = 0; $free = 0; \
           try {{ $cap = [uint64]$s.ExtendedProperty('System.Capacity') }} catch {{}}; \
           try {{ $free = [uint64]$s.ExtendedProperty('System.FreeSpace') }} catch {{}}; \
           if ($cap -gt 0) {{ Write-Output ($s.Name + \"`t\" + $cap + \"`t\" + $free) }} \
         }}",
        device = ps_quote(device),
    )
}

#[cfg(windows)]
fn mtp_usage_rows(device: &str) -> Vec<(String, u64, u64)> {
    if let Ok(cache) = MTP_USAGE.lock() {
        if let Some(hit) = cache.as_ref() {
            if hit.device == device && hit.at.elapsed() < REMOTE_STAT_TTL {
                return hit.rows.clone();
            }
        }
    }
    let rows = powershell_output(&mtp_usage_script(device))
        .map(|raw| parse_mtp_usage(&raw))
        .unwrap_or_default();
    if let Ok(mut cache) = MTP_USAGE.lock() {
        *cache = Some(MtpUsageCache {
            device: device.to_string(),
            rows: rows.clone(),
            at: Instant::now(),
        });
    }
    rows
}

#[cfg(windows)]
fn mtp_disk_usage_windows(uri: &str) -> Option<(u64, u64)> {
    let (device, rel) = parse_mtp(uri)?;
    let storage = mtp_storage_name(&rel)?;
    mtp_usage_rows(&device)
        .into_iter()
        .find(|(name, _, _)| name.eq_ignore_ascii_case(storage))
        .map(|(_, total, free)| (total, free))
}

fn parse_mtp_walk_output(raw: &str) -> Vec<(String, u64, f64)> {
    let mut entries = Vec::new();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let Some(rel) = parts.next() else { continue };
        let Some(size_s) = parts.next() else { continue };
        let Some(mtime_s) = parts.next() else { continue };
        let Ok(size) = size_s.parse::<u64>() else {
            continue;
        };
        let mtime = mtime_s.parse::<f64>().unwrap_or(0.0);
        let rel = rel.replace('/', std::path::MAIN_SEPARATOR_STR);
        if rel.is_empty() {
            continue;
        }
        entries.push((rel, size, mtime));
    }
    entries
}

fn mtp_walk(root: &str, dest: bool, keep: FileKeep) -> Vec<(String, u64, f64)> {
    #[cfg(windows)]
    {
        mtp_walk_windows(root, dest, keep)
    }
    #[cfg(not(windows))]
    {
        let _ = (root, dest, keep);
        Vec::new()
    }
}

#[cfg(windows)]
fn mtp_list_script(device: &str, rel: &str) -> String {
    let parts: Vec<String> = rel
        .split('/')
        .filter(|p| !p.is_empty())
        .map(ps_quote)
        .collect();
    let parts_ps = parts.join(", ");
    format!(
        r#"$ErrorActionPreference = 'Continue'
$shell = New-Object -ComObject Shell.Application
$ns = $shell.NameSpace(0x11)
if (-not $ns) {{ exit 1 }}
$dev = $null
foreach ($i in $ns.Items()) {{ if ($i.Name -eq {device}) {{ $dev = $i; break }} }}
if (-not $dev) {{ exit 1 }}
$root = $dev.GetFolder
$parts = @({parts})
foreach ($part in $parts) {{
  $hit = $null
  foreach ($c in $root.Items()) {{ if ($c.Name -eq $part) {{ $hit = $c; break }} }}
  if (-not $hit) {{ exit 1 }}
  $root = $hit.GetFolder
}}
$out = Join-Path $env:TEMP ('walkman-sync-mtp-' + [guid]::NewGuid().ToString() + '.tsv')
$utf8 = New-Object System.Text.UTF8Encoding $false
$sw = New-Object System.IO.StreamWriter $out, $false, $utf8
function Walk($folder, $prefix) {{
  foreach ($item in $folder.Items()) {{
    $name = $item.Name
    if (-not $item.IsFolder) {{
      try {{
        $fn = [string]$item.ExtendedProperty('System.FileName')
        if ($fn) {{ $name = $fn }}
      }} catch {{}}
    }}
    $rel = if ($prefix) {{ $prefix + '/' + $name }} else {{ $name }}
    if ($item.IsFolder) {{
      Walk $item.GetFolder $rel
    }} else {{
      $size = 0
      try {{
        $sz = $item.ExtendedProperty('System.Size')
        if ($null -ne $sz) {{ $size = [int64]$sz }}
      }} catch {{}}
      if ($size -eq 0) {{
        try {{ $size = [int64]$item.Size }} catch {{ $size = 0 }}
      }}
      $mtime = 0
      try {{
        $mtime = [DateTimeOffset]$item.ModifyDate.ToUniversalTime()
        $mtime = $mtime.ToUnixTimeSeconds()
      }} catch {{}}
      $sw.WriteLine($rel + "`t" + $size + "`t" + $mtime)
    }}
  }}
}}
try {{
  Walk $root ''
}} finally {{
  $sw.Close()
}}
Write-Output $out
"#,
        device = ps_quote(device),
        parts = parts_ps,
    )
}

#[cfg(windows)]
fn mtp_walk_windows(root: &str, dest: bool, keep: FileKeep) -> Vec<(String, u64, f64)> {
    let Some((device, rel)) = parse_mtp(root) else {
        return Vec::new();
    };
    let Some(raw) = powershell_file(&mtp_list_script(&device, &rel)) else {
        return Vec::new();
    };
    let list_path = raw.trim();
    if list_path.is_empty() {
        return Vec::new();
    }
    let _guard = TempPath(PathBuf::from(list_path));
    let text = fs::read_to_string(list_path).unwrap_or_default();
    parse_mtp_walk_output(&text)
        .into_iter()
        .filter(|(rel, _, _)| keep_walk_entry(rel, dest, keep))
        .collect()
}

pub fn copy_file(src: &str, dst: &str) -> io::Result<u64> {
    copy_file_progress(src, dst, &mut |_| {})
}

pub fn copy_file_progress(
    src: &str,
    dst: &str,
    progress: &mut dyn FnMut(u64),
) -> io::Result<u64> {
    if is_remote(dst) || is_remote(src) {
        if scheme(dst) == Scheme::Adb {
            let (serial, path) = parse_adb(dst).ok_or_else(|| io::Error::other("bad adb"))?;
            let status = adb_command()
                .args(["-s", &serial, "push", src, &path])
                .status()?;
            if !status.success() {
                return Err(io::Error::other("adb push failed"));
            }
            return Ok(fs::metadata(src)?.len());
        }
        return Err(io::Error::other("remote copy not supported"));
    }
    crate::copy::copy_atomic_progress(Path::new(src), Path::new(dst), progress)
}

pub fn write_bytes(path: &str, data: &[u8]) -> io::Result<()> {
    match scheme(path) {
        Scheme::Fs => {
            if let Some(dir) = Path::new(path).parent() {
                fs::create_dir_all(dir)?;
            }
            let mut f = fs::File::create(path)?;
            f.write_all(data)
        }
        _ => write_text(path, &String::from_utf8_lossy(data)),
    }
}

pub fn read_bytes(path: &str) -> io::Result<Vec<u8>> {
    match scheme(path) {
        Scheme::Fs => fs::read(path),
        Scheme::Adb => Ok(adb_read(path)?.into_bytes()),
        Scheme::Mtp => Err(io::Error::other("mtp")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walkman;

    #[test]
    fn parse_mtp_listing_splits_internal_and_sd() {
        let raw = "WALKMAN\tSlvrWalkman\tInternal shared storage\n";
        let (internal, sd) = parse_mtp_listing(raw);
        assert_eq!(
            internal,
            "mtp://WALKMAN/Internal shared storage/Music"
        );
        assert_eq!(sd, "mtp://WALKMAN/SlvrWalkman/Music");
        assert_eq!(
            walkman::infer_device_kind(&internal, &sd),
            walkman::KIND_ANDROID
        );
        assert_eq!(serial(&internal), "mtp-WALKMAN");
    }

    #[test]
    fn parse_mtp_listing_falls_back_to_first_storage() {
        let (internal, sd) = parse_mtp_listing("NW-ZX507\tSD card\n");
        assert_eq!(internal, "mtp://NW-ZX507/SD card/Music");
        assert_eq!(sd, "mtp://NW-ZX507/SD card/Music");
    }

    #[test]
    fn parse_mtp_splits_device_and_rel() {
        let (device, rel) = parse_mtp("mtp://WALKMAN/Internal shared storage/Music").unwrap();
        assert_eq!(device, "WALKMAN");
        assert_eq!(rel, "Internal shared storage/Music");
        assert_eq!(mtp_storage_name(&rel), Some("Internal shared storage"));
    }

    #[test]
    fn parse_mtp_usage_reads_capacity() {
        let raw = "SlvrWalkman\t1025289224192\t322692448256\nInternal shared storage\t53521575936\t45753921536\n";
        let rows = parse_mtp_usage(raw);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("SlvrWalkman".into(), 1025289224192, 322692448256));
        assert_eq!(
            rows[1],
            ("Internal shared storage".into(), 53521575936, 45753921536)
        );
    }

    #[test]
    fn parse_mtp_walk_output_reads_rows() {
        let raw = "Artist - Album/01.flac\t4096\t1700000000\nfolder.jpg\t80\t1\n";
        let rows = parse_mtp_walk_output(raw);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.replace('\\', "/").ends_with("01.flac"));
        assert_eq!(rows[0].1, 4096);
        let audio: Vec<_> = rows
            .into_iter()
            .filter(|(rel, _, _)| keep_walk_entry(rel, true, FileKeep::Audio))
            .collect();
        assert_eq!(audio.len(), 1);
    }

    #[test]
    fn parse_df_p_reads_posix_df() {
        let raw = "\
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/fuse         999424     654336    345088      66% /storage/XXXX-XXXX
";
        let (total, free) = parse_df_p(raw).unwrap();
        assert_eq!(total, 999424 * 1024);
        assert_eq!(free, 345088 * 1024);
    }

    #[test]
    fn parse_df_p_reads_toybox_512_blocks() {
        let internal = "\
Filesystem     512-blocks      Used Available Capacity Mounted on
/dev/fuse      104534328   15171180   89363128      15% /storage/emulated/0
";
        let (total, free) = parse_df_p(internal).unwrap();
        assert_eq!(total, 53_521_575_936);
        assert_eq!(free, 45_753_921_536);
        assert_eq!(walkman::human_size(total), "49.85 GB");

        let sd = "\
Filesystem     512-blocks      Used Available Capacity Mounted on
/dev/fuse     2002518016  1371667968  630850048      69% /storage/75D7-DC5F
";
        let (total, _) = parse_df_p(sd).unwrap();
        assert_eq!(total, 1_025_289_224_192);
        assert_eq!(walkman::human_size(total), "954.88 GB");
    }

    #[test]
    fn parse_df_p_skips_header_only() {
        assert!(parse_df_p("Filesystem 1K-blocks Used Available\n").is_none());
        assert!(parse_df_p("").is_none());
    }

    #[test]
    fn parse_adb_stat_listing_keeps_audio() {
        let raw = "\
4096 1700000000 /sdcard/Music/Artist - Album/01.flac
80 1 /sdcard/Music/folder.jpg
";
        let rows = parse_adb_stat_listing(raw, "/sdcard/Music", true, FileKeep::Audio);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].0.replace('\\', "/").ends_with("01.flac"));
        assert_eq!(rows[0].1, 4096);
    }

    #[test]
    fn looks_like_sd_storage_accepts_volume_uuid() {
        assert!(looks_like_sd_storage("0000-1111"));
        assert!(looks_like_sd_storage("12AB-CDEF"));
        assert!(!looks_like_sd_storage("emulated"));
        assert!(!looks_like_sd_storage("self"));
    }

    #[test]
    #[ignore]
    fn live_adb_walk_lists_audio() {
        let found = detect_adb();
        let root = if !found.1.is_empty() {
            found.1.as_str()
        } else {
            found.0.as_str()
        };
        assert!(!root.is_empty(), "expected adb:// dest, got {found:?}");
        let files = walk_files(root, true);
        assert!(
            !files.is_empty(),
            "expected audio files under {root}, got 0"
        );
    }

    #[test]
    #[ignore]
    fn live_adb_disk_usage_reports_capacity() {
        let found = detect_adb();
        let root = if !found.1.is_empty() {
            found.1.as_str()
        } else {
            found.0.as_str()
        };
        assert!(!root.is_empty(), "expected adb:// dest, got {found:?}");
        let (total, free) = disk_usage(root).expect("adb df");
        assert!(
            total > 0 && free <= total,
            "expected adb capacity, got total={total} free={free} for {root}"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn live_mtp_detect_sees_walkman_when_plugged_in() {
        let found = detect_mtp();
        assert!(
            !found.0.is_empty() || !found.1.is_empty(),
            "expected This PC\\WALKMAN over MTP, got {found:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn live_mtp_disk_usage_reports_sd_capacity() {
        let found = detect_mtp();
        let sd = found.1;
        assert!(!sd.is_empty(), "expected SD MTP URI");
        let (total, free) = disk_usage(&sd).expect("MTP capacity");
        assert!(
            total > free && total > 1_000_000_000,
            "expected SD capacity > 1 GB, got total={total} free={free}"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn live_mtp_walk_lists_audio_on_plugged_walkman() {
        let found = detect_mtp();
        let music = if !found.1.is_empty() {
            found.1
        } else {
            found.0
        };
        assert!(!music.is_empty(), "expected MTP Music URI");
        let sample = format!("{music}/Phillip-Michael Scales/Sinner-Songwriter");
        let files = walk_files(&sample, true);
        assert!(
            files
                .iter()
                .any(|(rel, size, _)| rel.to_ascii_lowercase().contains(".flac") && *size > 1000),
            "expected flacs with sizes under {sample}, got {files:?}"
        );
    }
}
