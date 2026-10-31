//! Library plan, copy jobs, skip-if-present, cleanup.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog::{self, Catalog, FileRow, RefreshOpts};
use crate::manifest::Manifest;
use crate::storage;
use crate::walkman::{self, SyncSettings};

const MTIME_TOLERANCE: f64 = 2.0;
const MANAGED_PLAYLISTS: &[&str] = &["liked songs.m3u8", "liked songs.m3u"];

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub settings: SyncSettings,
    pub lastfm_api_key: String,
    pub lastfm_api_secret: String,
    pub lastfm_username: String,
    pub lastfm_session_key: String,
    pub audiomuse_url: String,
    pub audiomuse_token: String,
    pub new_walkman: String,
}

impl AppConfig {
    pub fn load() -> Self {
        let mut cfg = Self::load_without_detect();
        cfg.refresh_drives();
        cfg
    }

    /// Read settings and `.env` without touching USB/ADB. The TUI uses this so the
    /// first frame can paint before drive detection and the library walk.
    pub fn load_without_detect() -> Self {
        crate::paths::load_dotenv();
        let settings = SyncSettings::load();
        Self {
            settings,
            lastfm_api_key: std::env::var("LASTFM_API_KEY").unwrap_or_default(),
            lastfm_api_secret: std::env::var("LASTFM_API_SECRET").unwrap_or_default(),
            lastfm_username: std::env::var("LASTFM_USERNAME").unwrap_or_default(),
            lastfm_session_key: std::env::var("LASTFM_SESSION_KEY").unwrap_or_default(),
            audiomuse_url: std::env::var("AUDIOMUSE_URL").unwrap_or_default(),
            audiomuse_token: std::env::var("AUDIOMUSE_TOKEN").unwrap_or_default(),
            new_walkman: String::new(),
        }
    }

    pub fn source_dir(&self) -> String {
        walkman::resolve_library_path(&self.settings.source_dir)
    }
    pub fn playlist_dir(&self) -> String {
        walkman::resolve_library_path(&self.settings.playlist_dir)
    }
    pub fn internal_drive(&self) -> String {
        self.settings.internal_drive()
    }
    pub fn sd_drive(&self) -> String {
        self.settings.sd_drive()
    }
    pub fn dest_drive(&self) -> String {
        walkman::pick_dest(
            &self.settings.dest(),
            &self.internal_drive(),
            &self.sd_drive(),
        )
    }
    pub fn other_drive(&self) -> String {
        let dest = self.dest_drive();
        if storage::fold_path(&dest) == storage::fold_path(&self.internal_drive()) {
            self.sd_drive()
        } else {
            self.internal_drive()
        }
    }
    pub fn is_android(&self) -> bool {
        self.settings.device_kind() == walkman::KIND_ANDROID
    }
    pub fn dest_is_connected(&self) -> bool {
        let d = self.dest_drive();
        !d.is_empty() && storage::isdir(&d)
    }
    pub fn dest_is_internal(&self) -> bool {
        storage::fold_path(&self.dest_drive()) == storage::fold_path(&self.internal_drive())
    }
    pub fn device_name(&self) -> String {
        self.settings.device_name()
    }
    pub fn persist(&self) -> std::io::Result<()> {
        self.settings.save()
    }

    pub fn dest_choice(&self) -> String {
        self.settings.dest()
    }
    pub fn set_dest(&mut self, dest: &str) {
        self.settings.active_profile_mut().dest = dest.to_string();
    }
    pub fn set_device_kind(&mut self, kind: &str) {
        self.settings.active_profile_mut().device_kind = kind.to_string();
    }
    pub fn set_device_name(&mut self, name: &str) {
        self.settings.active_profile_mut().name = name.to_string();
    }
    pub fn set_internal_drive(&mut self, path: &str) {
        self.settings.active_profile_mut().internal_drive = path.to_string();
        self.settings.auto_detect_drives = false;
    }
    pub fn set_sd_drive(&mut self, path: &str) {
        self.settings.active_profile_mut().sd_drive = path.to_string();
        self.settings.auto_detect_drives = false;
    }
    pub fn set_internal_max_gb(&mut self, value: f64) {
        self.settings.active_profile_mut().internal_max_gb = value;
    }
    pub fn set_sd_max_gb(&mut self, value: f64) {
        self.settings.active_profile_mut().sd_max_gb = value;
    }
    pub fn pull_listen_credentials(&mut self) {
        walkman::apply_listen_credentials(&self.internal_drive(), &self.sd_drive());
        self.lastfm_api_key = std::env::var("LASTFM_API_KEY").unwrap_or(self.lastfm_api_key.clone());
        self.lastfm_username =
            std::env::var("LASTFM_USERNAME").unwrap_or(self.lastfm_username.clone());
        self.lastfm_session_key =
            std::env::var("LASTFM_SESSION_KEY").unwrap_or(self.lastfm_session_key.clone());
        self.lastfm_api_secret =
            std::env::var("LASTFM_API_SECRET").unwrap_or(self.lastfm_api_secret.clone());
        self.audiomuse_url = std::env::var("AUDIOMUSE_URL").unwrap_or(self.audiomuse_url.clone());
        self.audiomuse_token =
            std::env::var("AUDIOMUSE_TOKEN").unwrap_or(self.audiomuse_token.clone());
    }

    pub fn refresh_drives(&mut self) -> bool {
        if !self.settings.auto_detect_drives {
            return false;
        }
        let android = self.is_android();
        let mut paths = walkman::detect_walkman_paths(android);
        if paths.0.is_empty() && paths.1.is_empty() {
            paths = storage::detect_android_mounts();
        }
        if paths.0.is_empty() && paths.1.is_empty() {
            walkman::apply_listen_credentials(&self.internal_drive(), &self.sd_drive());
            return false;
        }
        let inferred = walkman::infer_device_kind(&paths.0, &paths.1);
        let (created, changed) =
            walkman::bind_live_device(&mut self.settings, &paths.0, &paths.1, &inferred);
        if created {
            self.new_walkman = self.device_name();
        }
        walkman::apply_listen_credentials(&self.internal_drive(), &self.sd_drive());
        self.lastfm_api_key = std::env::var("LASTFM_API_KEY").unwrap_or(self.lastfm_api_key.clone());
        self.lastfm_username =
            std::env::var("LASTFM_USERNAME").unwrap_or(self.lastfm_username.clone());
        self.lastfm_session_key =
            std::env::var("LASTFM_SESSION_KEY").unwrap_or(self.lastfm_session_key.clone());
        self.lastfm_api_secret =
            std::env::var("LASTFM_API_SECRET").unwrap_or(self.lastfm_api_secret.clone());
        self.audiomuse_url = std::env::var("AUDIOMUSE_URL").unwrap_or(self.audiomuse_url.clone());
        self.audiomuse_token =
            std::env::var("AUDIOMUSE_TOKEN").unwrap_or(self.audiomuse_token.clone());
        let _ = self.persist();
        created || changed
    }

    pub fn dest_budget_bytes(&self, held: u64) -> u64 {
        let dest = self.dest_drive();
        let profile = self.settings.active_profile();
        if self.dest_is_internal() {
            if let Some(p) = profile {
                if p.internal_max_gb > 0.0 {
                    return (p.internal_max_gb * 1024.0 * 1024.0 * 1024.0) as u64;
                }
            }
        } else if let Some(p) = profile {
            if p.sd_max_gb > 0.0 {
                return (p.sd_max_gb * 1024.0 * 1024.0 * 1024.0) as u64;
            }
        }
        walkman::live_budget_bytes(&dest, held)
    }

    pub fn excluded_albums(&self) -> Vec<String> {
        self.settings
            .excluded_albums
            .split([',', '\n', ';'])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct FileEntry {
    pub rel_path: String,
    pub size: u64,
    pub mtime: f64,
    pub content_key: String,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceScan {
    pub root: String,
    pub files: HashMap<String, (u64, f64)>,
    pub content_keys: HashMap<String, String>,
    pub artist_files: HashMap<String, Vec<String>>,
    pub playlists: HashSet<String>,
}

impl DeviceScan {
    pub fn total_size(&self) -> u64 {
        self.files.values().map(|(s, _)| *s).sum()
    }
    pub fn lookup(&self, rel: &str) -> Option<(u64, f64)> {
        self.files
            .get(&storage::fold_rel(rel))
            .copied()
            .or_else(|| self.files.get(rel).copied())
    }
    pub fn rel_for_key(&self, key: &str) -> Option<String> {
        self.content_keys
            .iter()
            .find(|(_, v)| *v == key)
            .map(|(k, _)| k.clone())
    }
}

#[derive(Clone, Debug, Default)]
pub struct LibraryPlan {
    pub assignments: HashMap<String, String>,
    pub source_files: HashMap<String, Vec<FileEntry>>,
    pub source_artists: HashSet<String>,
    pub playlist: HashMap<String, Vec<String>>,
    pub playlist_drive: HashMap<String, String>,
    pub desired_files: HashSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyOp {
    #[default]
    Add,
    Replace,
    Adopt,
}

impl CopyOp {
    pub fn mark(self) -> &'static str {
        match self {
            CopyOp::Add => "+",
            CopyOp::Replace | CopyOp::Adopt => "~",
        }
    }

    fn transfer_rank(self) -> u8 {
        match self {
            CopyOp::Adopt => 0,
            CopyOp::Replace => 1,
            CopyOp::Add => 2,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CopyJob {
    pub src: String,
    pub dst: String,
    pub rel_path: String,
    pub transfer_bytes: u64,
    pub source_mtime: f64,
    pub content_key: String,
    pub adopt_from: String,
    pub op: CopyOp,
}

impl CopyJob {
    pub fn progress_label(&self) -> String {
        format!("{} {}", self.op.mark(), self.rel_path)
    }
}

const LOG_CAP: usize = 400;

#[derive(Clone, Debug, Default)]
pub struct SyncResult {
    pub logs: Vec<String>,
    pub copied: u32,
    pub skipped: u32,
    pub failed: u32,
    pub artists_internal: u32,
    pub artists_sd: u32,
    pub artists_skipped: u32,
    omitted: u32,
}

impl SyncResult {
    pub fn log(&mut self, m: impl Into<String>) {
        if self.logs.len() < LOG_CAP {
            self.logs.push(m.into());
            return;
        }
        self.omitted = self.omitted.saturating_add(1);
        let summary = format!("… and {} more", self.omitted);
        if self.logs.len() == LOG_CAP {
            self.logs.push(summary);
        } else if let Some(last) = self.logs.last_mut() {
            *last = summary;
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub plan: Option<LibraryPlan>,
    pub result: SyncResult,
    pub device_scans: HashMap<String, DeviceScan>,
    pub dest_walked: bool,
    pub source_total_bytes: u64,
    pub catalog_coverage: HashMap<String, Option<catalog::VolumeInfo>>,
    pub stale_files: Vec<String>,
}

pub fn device_scan_from_catalog(cat: &Catalog, kind: &str, root: &str) -> DeviceScan {
    let mut scan = DeviceScan {
        root: root.to_string(),
        ..DeviceScan::default()
    };
    if root.is_empty() {
        return scan;
    }
    for row in cat.files_for(kind, Some(root)) {
        let name = Path::new(&row.rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let lower = name.to_ascii_lowercase();
        let parts: Vec<_> = Path::new(&row.rel).components().collect();
        if parts.len() == 1 {
            if storage::is_playlist(name) && !MANAGED_PLAYLISTS.contains(&lower.as_str()) {
                scan.playlists.insert(
                    Path::new(&row.rel)
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            continue;
        }
        if !storage::is_audio(name) {
            continue;
        }
        let folded = storage::fold_rel(&row.rel);
        scan.files.insert(folded.clone(), (row.size, row.mtime));
        if !row.content_key.is_empty() {
            scan.content_keys.insert(folded, row.content_key.clone());
        }
        if let Some(folder) = Path::new(&row.rel).components().next() {
            scan.artist_files
                .entry(folder.as_os_str().to_string_lossy().into_owned())
                .or_default()
                .push(row.rel);
        }
    }
    scan
}

pub fn source_files_from_catalog(
    cat: &Catalog,
    source_dir: &str,
    folders: &HashSet<String>,
) -> HashMap<String, Vec<FileEntry>> {
    let mut grouped: HashMap<String, Vec<FileEntry>> =
        folders.iter().map(|f| (f.clone(), Vec::new())).collect();
    let fold_map: HashMap<String, String> = folders
        .iter()
        .map(|f| (storage::fold_rel(f), f.clone()))
        .collect();
    for row in cat.files_for(catalog::KIND_SOURCE, Some(source_dir)) {
        let name = Path::new(&row.rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if !storage::is_audio(name) {
            continue;
        }
        let Some(first) = Path::new(&row.rel).components().next() else {
            continue;
        };
        let folder_name = first.as_os_str().to_string_lossy();
        let Some(folder) = fold_map.get(&storage::fold_rel(&folder_name)) else {
            continue;
        };
        grouped.entry(folder.clone()).or_default().push(FileEntry {
            rel_path: row.rel,
            size: row.size,
            mtime: row.mtime,
            content_key: row.content_key,
        });
    }
    grouped
}

pub fn resolve_source_folders(source_dir: &str) -> HashSet<String> {
    let mut folders = HashSet::new();
    let Ok(read) = fs::read_dir(source_dir) else {
        return folders;
    };
    for entry in read.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if storage::is_ignored_dir(&name) {
                continue;
            }
            folders.insert(name);
        }
    }
    folders
}

pub fn index_library(config: &AppConfig, progress: &mut dyn FnMut(&str, u64, u64)) -> SyncResult {
    let mut result = SyncResult::default();
    let src = config.source_dir();
    if !Path::new(&src).is_dir() {
        result.log(format!("Source directory does not exist: {src}"));
        return result;
    }
    let mut cat = Catalog::open(None).expect("catalog");
    let stats = cat.refresh_source(
        &src,
        RefreshOpts {
            walk: true,
            enrich: true,
        },
        Some(&mut |folder, i, t| {
            progress(&format!("library: {folder}"), i as u64, t as u64);
        }),
    );
    result.log(format!(
        "Library index: +{} updated {} unchanged {} removed {} keyed {}",
        stats.added, stats.updated, stats.unchanged, stats.removed, stats.keyed
    ));
    result
}

pub fn index_walkman(config: &AppConfig, progress: &mut dyn FnMut(&str, u64, u64)) -> SyncResult {
    let mut result = SyncResult::default();
    let mut cat = Catalog::open(None).expect("catalog");
    let mut mounted = false;
    for (label, root, kind) in [
        ("internal", config.internal_drive(), catalog::KIND_INTERNAL),
        ("sd", config.sd_drive(), catalog::KIND_SD),
    ] {
        if root.is_empty() {
            result.log(format!("{label}: unplugged"));
            continue;
        }
        let (ok, reason) = cat.confirm_live(kind, &root);
        if !ok {
            result.log(format!("{label}: {reason}"));
            continue;
        }
        mounted = true;
        let stats = cat.refresh_dest(
            &root,
            kind,
            RefreshOpts {
                walk: true,
                enrich: true,
            },
            Some(&mut |folder, i, t| {
                progress(&format!("{label}: {folder}"), i as u64, t as u64);
            }),
        );
        result.log(format!(
            "{label} index: +{} updated {} unchanged {} removed {} keyed {}",
            stats.added, stats.updated, stats.unchanged, stats.removed, stats.keyed
        ));
    }
    if !mounted {
        result.log("Walkman not connected — plug it in and try again.");
    }
    result
}

pub fn build_scan_report(
    config: &AppConfig,
    dest_walk: bool,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> ScanReport {
    let mut report = ScanReport::default();
    let mut cat = match Catalog::open(None) {
        Ok(c) => c,
        Err(e) => {
            report.result.log(format!("catalog: {e}"));
            return report;
        }
    };
    let src = config.source_dir();
    progress("Indexing library...", 0, 0);
    let _ = cat.refresh_source(
        &src,
        RefreshOpts {
            walk: true,
            enrich: false,
        },
        Some(&mut |folder, i, t| {
            progress(&format!("library: {folder}"), i as u64, t as u64);
        }),
    );

    let mut dest_walked = false;
    let mut dest_refreshed = false;
    for (label, root, kind) in [
        ("internal", config.internal_drive(), catalog::KIND_INTERNAL),
        ("sd", config.sd_drive(), catalog::KIND_SD),
    ] {
        if root.is_empty() {
            continue;
        }
        let (ok, reason) = cat.confirm_live(kind, &root);
        if !ok {
            report.result.log(format!("{label}: {reason}"));
            continue;
        }
        let skip = !dest_walk && cat.should_skip_dest_walk(kind, &root);
        if skip {
            progress(&format!("{label}: using catalog"), 0, 0);
            continue;
        }
        progress(&format!("Indexing Walkman {label}..."), 0, 0);
        let _ = cat.refresh_dest(
            &root,
            kind,
            RefreshOpts {
                walk: true,
                enrich: false,
            },
            Some(&mut |folder, i, t| {
                progress(&format!("{label}: {folder}"), i as u64, t as u64);
            }),
        );
        if storage::fold_path(&root) == storage::fold_path(&config.dest_drive()) {
            dest_walked = true;
            dest_refreshed = true;
        }
    }
    report.dest_walked = dest_walked || dest_walk;
    report.catalog_coverage = cat.coverage(&config.internal_drive(), &config.sd_drive());
    report.device_scans.insert(
        config.internal_drive(),
        device_scan_from_catalog(&cat, catalog::KIND_INTERNAL, &config.internal_drive()),
    );
    report.device_scans.insert(
        config.sd_drive(),
        device_scan_from_catalog(&cat, catalog::KIND_SD, &config.sd_drive()),
    );
    progress("Planning from catalog...", 0, 0);
    report.plan = build_library_plan(
        config,
        &mut report.result,
        &report.device_scans,
        &cat,
        Some(progress),
    );
    if let Some(plan) = &report.plan {
        report.source_total_bytes = plan
            .source_files
            .values()
            .flatten()
            .map(|e| e.size)
            .sum();
        let dest = config.dest_drive();
        if let Some(scan) = report.device_scans.get(&dest) {
            collect_stale_files(plan, scan, &mut report.stale_files, progress);
        }
        if dest_refreshed && !dest.is_empty() {
            let already: HashSet<String> = report
                .stale_files
                .iter()
                .map(|s| storage::fold_rel(s))
                .collect();
            for rel in storage::walk_dest_unwanted(&dest) {
                if already.contains(&storage::fold_rel(&rel)) {
                    continue;
                }
                report.stale_files.push(rel);
            }
        }
    }
    report
}

fn collect_stale_files(
    plan: &LibraryPlan,
    scan: &DeviceScan,
    stale: &mut Vec<String>,
    progress: &mut dyn FnMut(&str, u64, u64),
) {
    let desired_fold: HashSet<String> = plan
        .desired_files
        .iter()
        .map(|d| storage::fold_rel(d))
        .collect();
    let mut orig_by_fold: HashMap<String, String> = HashMap::new();
    for rels in scan.artist_files.values() {
        for rel in rels {
            orig_by_fold
                .entry(storage::fold_rel(rel))
                .or_insert_with(|| rel.clone());
        }
    }
    let total = scan.files.len() as u64;
    progress("Planning: stale files", 0, total.max(1));
    for (index, folded) in scan.files.keys().enumerate() {
        if index == 0 || (index + 1) % 1024 == 0 || index + 1 == scan.files.len() {
            progress(
                "Planning: stale files",
                (index + 1) as u64,
                total.max(1),
            );
        }
        if desired_fold.contains(folded) {
            continue;
        }
        let orig = orig_by_fold
            .get(folded)
            .cloned()
            .unwrap_or_else(|| folded.clone());
        let name = Path::new(&orig)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let nested = Path::new(&orig).components().count() > 1;
        if storage::is_playlist(name) && !nested {
            continue;
        }
        stale.push(orig);
    }
}

pub fn catalog_index_line(config: &AppConfig, report: &ScanReport) -> String {
    let src = report.catalog_coverage.get(catalog::KIND_SOURCE).and_then(|v| v.as_ref());
    let dest_kind = if config.dest_is_internal() {
        catalog::KIND_INTERNAL
    } else {
        catalog::KIND_SD
    };
    let dest = report.catalog_coverage.get(dest_kind).and_then(|v| v.as_ref());
    let lib_age = catalog::format_age(src.map(|s| s.scanned_at).unwrap_or(0.0));
    let wm_age = catalog::format_age(dest.map(|s| s.scanned_at).unwrap_or(0.0));
    let lib_cov = src
        .map(|s| format!("{}/{}", s.key_count, s.file_count))
        .unwrap_or_else(|| "0/0".into());
    let wm_cov = dest
        .map(|s| format!("{}/{}", s.key_count, s.file_count))
        .unwrap_or_else(|| "0/0".into());
    let plugged = if config.dest_is_connected() {
        "connected"
    } else {
        "unplugged"
    };
    let stale = if report.dest_walked {
        ""
    } else {
        " — press w to refresh Walkman"
    };
    format!(
        "Library index {lib_age} ({lib_cov} keys) · Walkman index {wm_age} ({wm_cov}) {plugged}{stale}"
    )
}

pub fn build_library_plan(
    config: &AppConfig,
    result: &mut SyncResult,
    device_scans: &HashMap<String, DeviceScan>,
    cat: &Catalog,
    mut progress: Option<&mut dyn FnMut(&str, u64, u64)>,
) -> Option<LibraryPlan> {
    let src = config.source_dir();
    let playlists = config.playlist_dir();
    if !Path::new(&src).is_dir() {
        result.log(format!("Source directory does not exist: {src}"));
        return None;
    }
    if let Some(cb) = progress.as_mut() {
        cb("Planning: listing albums…", 0, 0);
    }
    let mut source_artists = resolve_source_folders(&src);
    let excluded = config.excluded_albums();
    source_artists.retain(|f| {
        !excluded.iter().any(|e| storage::fold_rel(e) == storage::fold_rel(f))
    });
    let mut source_files = source_files_from_catalog(cat, &src, &source_artists);
    if source_artists.iter().any(|f| source_files.get(f).map(|v| v.is_empty()).unwrap_or(true))
        && source_files.values().all(|v| v.is_empty())
    {
        // empty catalog: walk folders
        for folder in &source_artists {
            let tree = storage::walk_files(&storage::join(&src, &[folder]), false);
            source_files.insert(
                folder.clone(),
                tree.into_iter()
                    .map(|(rel, size, mtime)| FileEntry {
                        rel_path: storage::join(folder, &[&rel]),
                        size,
                        mtime,
                        content_key: String::new(),
                    })
                    .collect(),
            );
        }
    }
    source_artists.retain(|f| source_files.get(f).is_some_and(|v| !v.is_empty()));
    let dest_drive = config.dest_drive();
    let dest_label = if config.dest_is_internal() {
        "Internal"
    } else {
        "SD Card"
    };
    let held = device_scans
        .get(&dest_drive)
        .map(|s| s.total_size())
        .unwrap_or(0);
    let budget = config.dest_budget_bytes(held);
    result.log(format!(
        "Destination: {dest_label} ({dest_drive}), budget {}",
        walkman::human_size(budget)
    ));
    let mut used = 0u64;
    let mut plan = LibraryPlan {
        source_artists: source_artists.clone(),
        source_files: source_files.clone(),
        ..LibraryPlan::default()
    };
    let mut folder_sizes: Vec<(String, u64)> = source_artists
        .iter()
        .map(|f| {
            (
                f.clone(),
                source_files.get(f).map(|v| v.iter().map(|e| e.size).sum()).unwrap_or(0),
            )
        })
        .collect();
    folder_sizes.sort_by(|a, b| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase()));
    for (folder, size) in folder_sizes {
        if used + size > budget && budget > 0 {
            result.artists_skipped += 1;
            result.log(format!(
                "Skipping {folder}: {} exceeds remaining {dest_label} capacity.",
                walkman::human_size(size)
            ));
            continue;
        }
        used += size;
        plan.assignments.insert(folder.clone(), dest_drive.clone());
        if config.dest_is_internal() {
            result.artists_internal += 1;
        } else {
            result.artists_sd += 1;
        }
        result.log(format!(
            "{folder} -> {dest_label} ({})",
            walkman::human_size(size)
        ));
        if let Some(files) = source_files.get(&folder) {
            for e in files {
                plan.desired_files.insert(e.rel_path.clone());
            }
        }
    }
    let desired_fold: HashSet<String> = plan
        .desired_files
        .iter()
        .map(|d| storage::fold_rel(d))
        .collect();
    if Path::new(&playlists).is_dir() {
        let playlist_files: Vec<_> = fs::read_dir(&playlists)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let lower = name.to_string_lossy().to_ascii_lowercase();
                storage::is_playlist(&lower) && !MANAGED_PLAYLISTS.contains(&lower.as_str())
            })
            .collect();
        let total = playlist_files.len() as u64;
        if let Some(cb) = progress.as_mut() {
            cb("Planning: matching playlists", 0, total.max(1));
        }
        for (index, entry) in playlist_files.iter().enumerate() {
            if let Some(cb) = progress.as_mut() {
                if index == 0 || (index + 1) % 8 == 0 || index + 1 == playlist_files.len() {
                    cb(
                        "Planning: matching playlists",
                        (index + 1) as u64,
                        total.max(1),
                    );
                }
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let tracks = playlist_tracks(&entry.path(), &src);
            let kept: Vec<String> = tracks
                .into_iter()
                .filter(|t| desired_fold.contains(&storage::fold_rel(t)))
                .collect();
            if kept.is_empty() {
                continue;
            }
            plan.playlist.insert(name.clone(), kept);
            plan.playlist_drive.insert(name, dest_drive.clone());
        }
    }
    Some(plan)
}

/// Map a playlist line onto a source-relative path without touching the network.
/// Prefer paths that already look catalog-relative; otherwise strip the source prefix
/// as a string. Never `canonicalize` (UNC round-trips hang planning).
fn playlist_tracks(path: &Path, source_dir: &str) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut tracks = Vec::new();
    let src = PathBuf::from(source_dir);
    let playlist_dir = path.parent().unwrap_or(&src);
    for raw in text.lines() {
        let line = raw.trim().trim_matches('"');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let normalized = line.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR);
        let candidate = PathBuf::from(&normalized);
        if let Some(rel) = strip_source_prefix(&candidate, source_dir) {
            tracks.push(rel);
            continue;
        }
        if !candidate.is_absolute() {
            if let Some(rel) = strip_source_prefix(&src.join(&candidate), source_dir) {
                tracks.push(rel);
                continue;
            }
            let joined = playlist_dir.join(&candidate);
            if let Some(rel) = strip_source_prefix(&joined, source_dir) {
                tracks.push(rel);
                continue;
            }
        }
        tracks.push(normalized);
    }
    tracks
}

fn strip_source_prefix(path: &Path, source_dir: &str) -> Option<String> {
    let folded = storage::fold_path(&path.to_string_lossy());
    let src = storage::fold_path(source_dir);
    let src = src.trim_end_matches(['\\', '/']);
    let rest = folded.strip_prefix(src)?;
    let rest = rest.trim_start_matches(['\\', '/']);
    if rest.is_empty() {
        return None;
    }
    Some(rest.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR))
}

pub fn build_copy_jobs(
    config: &AppConfig,
    plan: &LibraryPlan,
    device_scans: &HashMap<String, DeviceScan>,
) -> Vec<CopyJob> {
    let dest_drive = config.dest_drive();
    let other_drive = config.other_drive();
    let empty_scan = DeviceScan::default();
    let dest_scan = device_scans.get(&dest_drive).unwrap_or(&empty_scan);
    let other_scan = device_scans.get(&other_drive).unwrap_or(&empty_scan);
    let dest_manifest = Manifest::load(&dest_drive);
    let mut jobs = Vec::new();
    let src_root = config.source_dir();
    for (folder, assigned) in &plan.assignments {
        if storage::fold_path(assigned) != storage::fold_path(&dest_drive) {
            continue;
        }
        let Some(entries) = plan.source_files.get(folder) else {
            continue;
        };
        for entry in entries {
            let rel = &entry.rel_path;
            let src = storage::join(&src_root, &[rel]);
            let source_key = if entry.content_key.is_empty() {
                None
            } else {
                Some(entry.content_key.as_str())
            };
            let dest_meta = dest_scan.lookup(rel);
            let dest_size = dest_meta.map(|(s, _)| s);
            let dest_key = dest_scan
                .content_keys
                .get(&storage::fold_rel(rel))
                .cloned()
                .or_else(|| dest_manifest.lookup(rel).map(|r| r.content_key.clone()))
                .unwrap_or_default();
            if dest_manifest.up_to_date(rel, entry.size, entry.mtime, dest_size, source_key) {
                continue;
            }
            if let Some((dst_size, dst_mtime)) = dest_meta {
                if source_key.is_some() && dest_key == source_key.unwrap_or("") {
                    continue;
                }
                if entry.size == dst_size && entry.mtime <= dst_mtime + MTIME_TOLERANCE {
                    continue;
                }
            }
            let mut adopt_from = String::new();
            if dest_meta.is_none() {
                if let Some(key) = source_key {
                    if let Some(held) = dest_scan.rel_for_key(key) {
                        adopt_from = storage::join(&dest_drive, &[&held]);
                    } else if let Some(held) = other_scan.rel_for_key(key) {
                        adopt_from = storage::join(&other_drive, &[&held]);
                    }
                }
            }
            let op = if !adopt_from.is_empty() {
                CopyOp::Adopt
            } else if dest_meta.is_some() {
                CopyOp::Replace
            } else {
                CopyOp::Add
            };
            jobs.push(CopyJob {
                src,
                dst: storage::join(&dest_drive, &[rel]),
                rel_path: rel.clone(),
                transfer_bytes: entry.size,
                source_mtime: entry.mtime,
                content_key: source_key.unwrap_or("").to_string(),
                adopt_from,
                op,
            });
        }
    }
    sort_copy_jobs(&mut jobs);
    jobs
}

fn sort_copy_jobs(jobs: &mut [CopyJob]) {
    jobs.sort_by_key(|job| job.op.transfer_rank());
}

fn dest_relative(path: &str, dest: &str) -> Option<String> {
    let folded = storage::fold_path(path);
    let dest = storage::fold_path(dest);
    let dest = dest.trim_end_matches(['\\', '/']);
    let rest = folded.strip_prefix(dest)?;
    let rest = rest.trim_start_matches(['\\', '/']);
    if rest.is_empty() {
        None
    } else {
        Some(rest.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR))
    }
}

fn dest_adopt_held(jobs: &[CopyJob], dest: &str) -> HashSet<String> {
    jobs.iter()
        .filter(|job| job.op == CopyOp::Adopt && !job.adopt_from.is_empty())
        .filter_map(|job| dest_relative(&job.adopt_from, dest))
        .map(|rel| storage::fold_rel(&rel))
        .collect()
}

fn partition_stale<'a>(stale: &'a [String], held: &HashSet<String>) -> (Vec<&'a String>, Vec<&'a String>) {
    let mut ready = Vec::new();
    let mut deferred = Vec::new();
    for rel in stale {
        if held.contains(&storage::fold_rel(rel)) {
            deferred.push(rel);
        } else {
            ready.push(rel);
        }
    }
    (ready, deferred)
}

#[derive(Clone, Debug, Default)]
pub struct AlbumChange {
    pub name: String,
    pub add_files: u32,
    pub remove_files: u32,
    pub add_bytes: u64,
    pub remove_bytes: u64,
}

impl AlbumChange {
    pub fn net_bytes(&self) -> i64 {
        self.add_bytes as i64 - self.remove_bytes as i64
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlanDiff {
    pub added: Vec<AlbumChange>,
    pub modified: Vec<AlbumChange>,
    pub removed: Vec<AlbumChange>,
    pub unchanged: usize,
    pub unchanged_bytes: u64,
}

impl PlanDiff {
    pub fn add_files(&self) -> u32 {
        self.added
            .iter()
            .chain(self.modified.iter())
            .map(|a| a.add_files)
            .sum()
    }

    pub fn remove_files(&self) -> u32 {
        self.modified
            .iter()
            .chain(self.removed.iter())
            .map(|a| a.remove_files)
            .sum()
    }

    pub fn add_bytes(&self) -> u64 {
        self.added
            .iter()
            .chain(self.modified.iter())
            .map(|a| a.add_bytes)
            .sum()
    }

    pub fn remove_bytes(&self) -> u64 {
        self.modified
            .iter()
            .chain(self.removed.iter())
            .map(|a| a.remove_bytes)
            .sum()
    }

    pub fn net_bytes(&self) -> i64 {
        self.add_bytes() as i64 - self.remove_bytes() as i64
    }
}

fn album_folder(rel: &str) -> String {
    Path::new(rel)
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_else(|| rel.to_string())
}

fn sort_changes(rows: &mut [AlbumChange]) {
    rows.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
}

/// Album-level add / modify / remove for the Plan screen and dashboard.
pub fn plan_diff(config: &AppConfig, report: &ScanReport) -> PlanDiff {
    let dest = config.dest_drive();
    let jobs = match &report.plan {
        Some(plan) => build_copy_jobs(config, plan, &report.device_scans),
        None => Vec::new(),
    };
    classify_plan_diff(
        report.plan.as_ref(),
        report.device_scans.get(&dest),
        &dest,
        &jobs,
        &report.stale_files,
    )
}

pub fn classify_plan_diff(
    plan: Option<&LibraryPlan>,
    dest_scan: Option<&DeviceScan>,
    dest_root: &str,
    jobs: &[CopyJob],
    stale: &[String],
) -> PlanDiff {
    let dest_fold = storage::fold_path(dest_root);
    let dest_scan = dest_scan.cloned().unwrap_or_default();
    let planned: HashMap<String, String> = plan
        .map(|p| {
            p.assignments
                .keys()
                .map(|name| (storage::fold_rel(name), name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let dest_albums: HashMap<String, String> = dest_scan
        .artist_files
        .keys()
        .map(|name| (storage::fold_rel(name), name.clone()))
        .collect();

    let mut work: HashMap<String, AlbumChange> = HashMap::new();
    let ensure = |map: &mut HashMap<String, AlbumChange>, fold: &str, fallback: &str| {
        map.entry(fold.to_string()).or_insert_with(|| AlbumChange {
            name: planned
                .get(fold)
                .cloned()
                .or_else(|| dest_albums.get(fold).cloned())
                .unwrap_or_else(|| fallback.to_string()),
            ..AlbumChange::default()
        });
    };

    for job in jobs {
        let folder = album_folder(&job.rel_path);
        let fold = storage::fold_rel(&folder);
        ensure(&mut work, &fold, &folder);
        let row = work.get_mut(&fold).unwrap();
        row.add_files += 1;
        match job.op {
            CopyOp::Add => row.add_bytes += job.transfer_bytes,
            CopyOp::Replace => {
                row.add_bytes += job.transfer_bytes;
                if let Some((old, _)) = dest_scan.lookup(&job.rel_path) {
                    row.remove_bytes += old;
                }
            }
            CopyOp::Adopt => {
                if !storage::fold_path(&job.adopt_from).starts_with(&dest_fold) {
                    row.add_bytes += job.transfer_bytes;
                }
            }
        }
    }

    let removed_folds: HashSet<String> = dest_albums
        .keys()
        .filter(|k| !planned.contains_key(*k))
        .cloned()
        .collect();

    for rel in stale {
        let folder = album_folder(rel);
        let fold = storage::fold_rel(&folder);
        if removed_folds.contains(&fold) {
            continue;
        }
        ensure(&mut work, &fold, &folder);
        let row = work.get_mut(&fold).unwrap();
        row.remove_files += 1;
        if let Some((sz, _)) = dest_scan.lookup(rel) {
            row.remove_bytes += sz;
        }
    }

    let mut diff = PlanDiff::default();
    for (fold, name) in &dest_albums {
        if planned.contains_key(fold) {
            continue;
        }
        let mut change = AlbumChange {
            name: name.clone(),
            ..AlbumChange::default()
        };
        let mut seen: HashSet<String> = HashSet::new();
        if let Some(files) = dest_scan.artist_files.get(name) {
            for rel in files {
                let key = storage::fold_rel(rel);
                if !seen.insert(key) {
                    continue;
                }
                change.remove_files += 1;
                if let Some((sz, _)) = dest_scan.lookup(rel) {
                    change.remove_bytes += sz;
                }
            }
        }
        for rel in stale {
            let folder = album_folder(rel);
            if storage::fold_rel(&folder) != *fold {
                continue;
            }
            let key = storage::fold_rel(rel);
            if !seen.insert(key) {
                continue;
            }
            change.remove_files += 1;
            if let Some((sz, _)) = dest_scan.lookup(rel) {
                change.remove_bytes += sz;
            }
        }
        diff.removed.push(change);
    }

    for (fold, name) in &planned {
        let on_dest = dest_albums.contains_key(fold);
        let change = work.remove(fold).unwrap_or_else(|| AlbumChange {
            name: name.clone(),
            ..AlbumChange::default()
        });
        if !on_dest {
            diff.added.push(change);
            continue;
        }
        if change.add_files > 0 || change.remove_files > 0 {
            diff.modified.push(change);
        } else {
            diff.unchanged += 1;
            diff.unchanged_bytes += plan
                .and_then(|p| p.source_files.get(name))
                .map(|v| v.iter().map(|e| e.size).sum::<u64>())
                .unwrap_or(0);
        }
    }

    for (_, change) in work {
        if change.add_files > 0 || change.remove_files > 0 {
            diff.removed.push(change);
        }
    }

    sort_changes(&mut diff.added);
    sort_changes(&mut diff.modified);
    sort_changes(&mut diff.removed);
    diff
}

/// Bytes the dest volume will gain and lose once this sync finishes.
pub fn dest_space_delta(config: &AppConfig, report: &ScanReport) -> (u64, u64) {
    let dest = config.dest_drive();
    let dest_fold = storage::fold_path(&dest);
    let jobs = match &report.plan {
        Some(plan) => build_copy_jobs(config, plan, &report.device_scans),
        None => Vec::new(),
    };
    let add = jobs
        .iter()
        .map(|job| match job.op {
            CopyOp::Add => job.transfer_bytes,
            CopyOp::Replace => 0,
            CopyOp::Adopt => {
                if storage::fold_path(&job.adopt_from).starts_with(&dest_fold) {
                    0
                } else {
                    job.transfer_bytes
                }
            }
        })
        .sum();
    let scan = report.device_scans.get(&dest);
    let remove = report
        .stale_files
        .iter()
        .map(|rel| scan.and_then(|s| s.lookup(rel).map(|(sz, _)| sz)).unwrap_or(0))
        .sum();
    (add, remove)
}

pub fn execute_copies(
    config: &AppConfig,
    jobs: &[CopyJob],
    progress: &mut dyn FnMut(&str, u64, u64),
) -> SyncResult {
    let mut ordered = jobs.to_vec();
    sort_copy_jobs(&mut ordered);
    execute_copies_ordered(config, &ordered, progress)
}

fn execute_copies_ordered(
    config: &AppConfig,
    jobs: &[CopyJob],
    progress: &mut dyn FnMut(&str, u64, u64),
) -> SyncResult {
    let mut result = SyncResult::default();
    let dest = config.dest_drive();
    let mut manifest = Manifest::load(&dest);
    let cat = Catalog::open(None).ok();
    let dest_kind = if config.dest_is_internal() {
        catalog::KIND_INTERNAL
    } else {
        catalog::KIND_SD
    };
    let total: u64 = jobs.iter().map(|j| j.transfer_bytes).sum();
    let mut done = 0u64;
    for job in jobs {
        let label = job.progress_label();
        progress(&label, done, total);
        if !job.adopt_from.is_empty() && !storage::is_remote(&job.adopt_from) {
            if let Some(parent) = Path::new(&job.dst).parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::rename(&job.adopt_from, &job.dst) {
                Ok(()) => {
                    result.copied += 1;
                    let size = fs::metadata(&job.dst).map(|m| m.len()).unwrap_or(job.transfer_bytes);
                    manifest.remember(
                        &job.rel_path,
                        job.transfer_bytes,
                        job.source_mtime,
                        size,
                        &job.content_key,
                    );
                    if let Some(cat) = &cat {
                        cat.upsert_dest_file(
                            dest_kind,
                            &dest,
                            &job.rel_path,
                            size,
                            storage::now_mtime(&job.dst),
                            &job.content_key,
                        );
                        if let Some(old_rel) = dest_relative(&job.adopt_from, &dest) {
                            cat.forget_dest_file(dest_kind, &dest, &old_rel);
                        }
                    }
                    done += job.transfer_bytes;
                    progress(&label, done, total);
                    continue;
                }
                Err(_) => {}
            }
        }
        match storage::copy_file_progress(&job.src, &job.dst, &mut |n| {
            progress(&label, done.saturating_add(n), total);
        }) {
            Ok(copied) => {
                result.copied += 1;
                manifest.remember(
                    &job.rel_path,
                    job.transfer_bytes,
                    job.source_mtime,
                    copied,
                    &job.content_key,
                );
                if let Some(cat) = &cat {
                    cat.upsert_dest_file(
                        dest_kind,
                        &dest,
                        &job.rel_path,
                        copied,
                        storage::now_mtime(&job.dst),
                        &job.content_key,
                    );
                }
            }
            Err(e) => {
                result.failed += 1;
                result.log(format!("copy failed {}: {e}", job.rel_path));
            }
        }
        done += job.transfer_bytes;
        progress(&label, done, total);
    }
    let _ = manifest.save(&dest);
    write_playlists(config);
    result
}

fn write_playlists(config: &AppConfig) {
    // Playlists are written by the caller with a plan; this is a no-op placeholder
    // used after copy. Real playlist write happens in sync_library.
    let _ = config;
}

pub fn write_playlist_files(config: &AppConfig, plan: &LibraryPlan) {
    let dest = config.dest_drive();
    for (name, tracks) in &plan.playlist {
        let drive = plan.playlist_drive.get(name).unwrap_or(&dest);
        let path = storage::join(drive, &[name]);
        let mut body = String::from("#EXTM3U\n");
        for t in tracks {
            body.push_str(&t.replace('\\', "/"));
            body.push('\n');
        }
        let _ = storage::write_text(&path, &body);
        if !config.is_android() {
            let root = walkman::volume_root(drive);
            let cinder = storage::join(&root, &["cinder_playlists", name]);
            let prefix = if config.dest_is_internal() {
                "/MUSIC/"
            } else {
                "/WALKMAN_EXT/MUSIC/"
            };
            let mut cinder_body = String::from("#EXTM3U\n");
            for t in tracks {
                cinder_body.push_str(prefix);
                cinder_body.push_str(&t.replace('\\', "/"));
                cinder_body.push('\n');
            }
            let _ = storage::write_text(&cinder, &cinder_body);
        }
    }
}

pub fn cleanup_stale(
    config: &AppConfig,
    report: &ScanReport,
    dry_run: bool,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> SyncResult {
    let result = cleanup_stale_files(config, &report.stale_files, dry_run, true, progress);
    if !dry_run {
        refresh_dest_usage(config);
    }
    result
}

fn cleanup_stale_files(
    config: &AppConfig,
    stale: &[String],
    dry_run: bool,
    log_missing: bool,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> SyncResult {
    let mut result = SyncResult::default();
    let dest = config.dest_drive();
    let dest_kind = if config.dest_is_internal() {
        catalog::KIND_INTERNAL
    } else {
        catalog::KIND_SD
    };
    let cat = Catalog::open(None).ok();
    let mut manifest = Manifest::load(&dest);
    let total = stale.len() as u64;
    if total == 0 {
        progress("nothing to remove", 1, 1);
        return result;
    }
    progress("removing stale files", 0, total);
    for (index, rel) in stale.iter().enumerate() {
        let line = format!("- {rel}");
        let done = index as u64;
        progress(&line, done, total);
        let path = storage::join(&dest, &[rel]);
        if !dry_run && !log_missing && !storage::exists(&path) {
            manifest.forget(rel);
            if let Some(cat) = &cat {
                cat.forget_dest_file(dest_kind, &dest, rel);
            }
            progress(&line, done + 1, total);
            continue;
        }
        result.log(line.clone());
        if !dry_run {
            if storage::remove(&path).is_ok() {
                manifest.forget(rel);
                if let Some(cat) = &cat {
                    cat.forget_dest_file(dest_kind, &dest, rel);
                }
            }
        }
        progress(&line, done + 1, total);
    }
    if !dry_run {
        let _ = manifest.save(&dest);
    }
    result
}

pub fn sync_library(
    config: &AppConfig,
    report: &ScanReport,
    dry_run: bool,
    progress: &mut dyn FnMut(&str, u64, u64),
) -> SyncResult {
    let Some(plan) = &report.plan else {
        let mut r = SyncResult::default();
        r.log("No plan.");
        return r;
    };
    let jobs = build_copy_jobs(config, plan, &report.device_scans);
    let modify = jobs
        .iter()
        .filter(|j| j.op != CopyOp::Add)
        .count();
    let add = jobs.len().saturating_sub(modify);
    if dry_run {
        let mut r = SyncResult::default();
        r.log(format!(
            "{} file(s) would remove, {} would modify, {} would add",
            report.stale_files.len(),
            modify,
            add
        ));
        for rel in report.stale_files.iter().take(80) {
            r.log(format!("- {rel}"));
        }
        if report.stale_files.len() > 80 {
            r.log(format!("... {} more removals", report.stale_files.len() - 80));
        }
        for j in jobs.iter().take(80) {
            r.log(j.progress_label());
        }
        if jobs.len() > 80 {
            r.log(format!("... {} more copies", jobs.len() - 80));
        }
        return r;
    }
    let dest = config.dest_drive();
    let held = dest_adopt_held(&jobs, &dest);
    let (ready, deferred): (Vec<String>, Vec<String>) = {
        let (ready, deferred) = partition_stale(&report.stale_files, &held);
        (
            ready.into_iter().cloned().collect(),
            deferred.into_iter().cloned().collect(),
        )
    };
    let mut result = if ready.is_empty() {
        SyncResult::default()
    } else {
        cleanup_stale_files(config, &ready, false, true, progress)
    };
    let copies = execute_copies_ordered(config, &jobs, progress);
    result.copied += copies.copied;
    result.skipped += copies.skipped;
    result.failed += copies.failed;
    for line in copies.logs {
        result.log(line);
    }
    if !deferred.is_empty() {
        let leftovers = cleanup_stale_files(config, &deferred, false, false, progress);
        for line in leftovers.logs {
            result.log(line);
        }
    }
    write_playlist_files(config, plan);
    refresh_dest_usage(config);
    result
}

fn refresh_dest_usage(config: &AppConfig) {
    let dest = config.dest_drive();
    if dest.is_empty() {
        return;
    }
    let _ = storage::refresh_disk_usage(&dest);
}

#[derive(Clone, Debug)]
pub struct ScrobbleFile {
    pub path: String,
    pub pending: usize,
}

pub fn list_scrobble_files(config: &AppConfig) -> Vec<ScrobbleFile> {
    let mut files = Vec::new();
    for root in [config.internal_drive(), config.sd_drive()] {
        if root.is_empty() {
            continue;
        }
        let vol = walkman::volume_root(&root);
        for name in [".scrobbler.log", "scrobble.logs"] {
            let path = storage::join(&vol, &[name]);
            if storage::exists(&path) {
                let pending = storage::read_text(&path)
                    .map(|t| {
                        t.lines()
                            .filter(|l| !l.starts_with('#') && !l.is_empty())
                            .count()
                    })
                    .unwrap_or(0);
                files.push(ScrobbleFile { path, pending });
            }
        }
    }
    files
}

pub fn upload_scrobbles(config: &AppConfig) -> SyncResult {
    let mut result = SyncResult::default();
    if config.lastfm_session_key.is_empty() {
        result.log("Last.fm is not signed in.");
        return result;
    }
    let files = list_scrobble_files(config);
    if files.is_empty() {
        result.log("No scrobble log on the Walkman.");
        return result;
    }
    for file in files {
        result.log(format!("{}: {} pending", file.path, file.pending));
        // Full scrobble POST is in likes/lastfm; keep a summary here.
    }
    result
}

pub fn file_row_to_entry(row: FileRow) -> FileEntry {
    FileEntry {
        rel_path: row.rel,
        size: row.size,
        mtime: row.mtime,
        content_key: row.content_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sep(parts: &[&str]) -> String {
        parts.join(std::path::MAIN_SEPARATOR_STR)
    }

    #[test]
    fn stale_pass_is_hashset_lookup() {
        let mut plan = LibraryPlan::default();
        plan.desired_files.insert(sep(&["Artist", "keep.flac"]));
        let mut scan = DeviceScan::default();
        let keep = sep(&["Artist", "Keep.flac"]);
        let drop = sep(&["Artist", "gone.flac"]);
        let playlist = sep(&["Playlists", "mood.m3u8"]);
        let root_list = "Mix.m3u8".to_string();
        scan.files
            .insert(storage::fold_rel(&keep), (1, 0.0));
        scan.files
            .insert(storage::fold_rel(&drop), (1, 0.0));
        scan.files
            .insert(storage::fold_rel(&playlist), (1, 0.0));
        scan.files
            .insert(storage::fold_rel(&root_list), (1, 0.0));
        scan.artist_files
            .insert("Artist".into(), vec![keep.clone(), drop.clone()]);
        scan.artist_files
            .insert("Playlists".into(), vec![playlist.clone()]);
        let mut stale = Vec::new();
        let mut notes = Vec::new();
        collect_stale_files(&plan, &scan, &mut stale, &mut |s, _, _| notes.push(s.to_string()));
        let stale_set: HashSet<String> = stale.into_iter().collect();
        assert_eq!(
            stale_set,
            HashSet::from([drop, playlist]),
            "nested playlists are stale; root Mix.m3u8 is not"
        );
        assert!(notes.iter().any(|n| n.contains("Planning: stale files")));
    }

    fn job(rel: &str, op: CopyOp, adopt_from: &str) -> CopyJob {
        CopyJob {
            rel_path: rel.into(),
            adopt_from: adopt_from.into(),
            op,
            ..CopyJob::default()
        }
    }

    #[test]
    fn copy_jobs_run_adopt_replace_then_add() {
        let mut jobs = vec![
            job("new.flac", CopyOp::Add, ""),
            job("old.flac", CopyOp::Replace, ""),
            job("moved.flac", CopyOp::Adopt, r"E:\MUSIC\held.flac"),
            job("later.flac", CopyOp::Add, ""),
        ];
        sort_copy_jobs(&mut jobs);
        assert_eq!(
            jobs.iter().map(|j| j.op).collect::<Vec<_>>(),
            vec![CopyOp::Adopt, CopyOp::Replace, CopyOp::Add, CopyOp::Add]
        );
    }

    #[test]
    fn adopt_sources_are_deferred_until_after_rename() {
        let dest = r"E:\MUSIC";
        let held = dest_adopt_held(
            &[job(
                "Artist\\track.flac",
                CopyOp::Adopt,
                &storage::join(dest, &["OldArtist", "track.flac"]),
            )],
            dest,
        );
        let stale = vec![
            sep(&["Gone", "x.flac"]),
            sep(&["OldArtist", "track.flac"]),
        ];
        let (ready, deferred) = partition_stale(&stale, &held);
        assert_eq!(ready, vec![&stale[0]]);
        assert_eq!(deferred, vec![&stale[1]]);
    }

    #[test]
    fn sync_preview_lists_removals_before_copies() {
        let dest = "MUSIC".to_string();
        let mut plan = LibraryPlan::default();
        plan.assignments.insert("Keep".into(), dest.clone());
        plan.source_files.insert(
            "Keep".into(),
            vec![
                FileEntry {
                    rel_path: sep(&["Keep", "old.flac"]),
                    size: 10,
                    mtime: 9.0,
                    content_key: String::new(),
                },
                FileEntry {
                    rel_path: sep(&["Keep", "new.flac"]),
                    size: 4,
                    mtime: 9.0,
                    content_key: String::new(),
                },
            ],
        );
        let mut scan = DeviceScan::default();
        scan.files
            .insert(storage::fold_rel(&sep(&["Keep", "old.flac"])), (3, 0.0));
        scan.files
            .insert(storage::fold_rel(&sep(&["Gone", "x.flac"])), (8, 0.0));
        let mut report = ScanReport::default();
        report.plan = Some(plan);
        report.stale_files = vec![sep(&["Gone", "x.flac"])];
        report.device_scans.insert(dest.clone(), scan);

        let mut settings = crate::walkman::SyncSettings::default();
        settings.auto_detect_drives = false;
        settings.active_device = "test".into();
        settings.devices.insert(
            "test".into(),
            crate::walkman::DeviceProfile {
                name: "test".into(),
                internal_drive: dest,
                dest: "internal".into(),
                ..crate::walkman::DeviceProfile::default()
            },
        );
        let config = AppConfig {
            settings,
            lastfm_api_key: String::new(),
            lastfm_api_secret: String::new(),
            lastfm_username: String::new(),
            lastfm_session_key: String::new(),
            audiomuse_url: String::new(),
            audiomuse_token: String::new(),
            new_walkman: String::new(),
        };
        let preview = sync_library(&config, &report, true, &mut |_, _, _| {});
        let logs = preview.logs.join("\n");
        let remove_at = logs.find("- ").expect(&logs);
        let replace_at = logs.find("~ ").expect(&logs);
        let add_at = logs.find("+ ").expect(&logs);
        assert!(
            remove_at < replace_at && replace_at < add_at,
            "{logs}"
        );
        assert!(logs.contains("would remove"));
        assert!(logs.contains("would modify"));
        assert!(logs.contains("would add"));
    }

    #[test]
    fn sync_deletes_before_replace_before_add() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("catalog.sqlite");
        std::env::set_var("SONY_SYNC_CATALOG", catalog.to_str().unwrap());
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                std::env::remove_var("SONY_SYNC_CATALOG");
            }
        }
        let _clear = Clear;

        let src = tmp.path().join("lib");
        let dest = tmp.path().join("MUSIC");
        fs::create_dir_all(src.join("Keep")).unwrap();
        fs::create_dir_all(dest.join("Keep")).unwrap();
        fs::create_dir_all(dest.join("Gone")).unwrap();
        fs::write(src.join("Keep").join("old.flac"), b"replaced").unwrap();
        fs::write(src.join("Keep").join("new.flac"), b"added!").unwrap();
        fs::write(dest.join("Keep").join("old.flac"), b"old").unwrap();
        fs::write(dest.join("Gone").join("x.flac"), b"stale").unwrap();

        let dest_s = dest.to_string_lossy().into_owned();
        let src_s = src.to_string_lossy().into_owned();
        let keep_old = sep(&["Keep", "old.flac"]);
        let keep_new = sep(&["Keep", "new.flac"]);
        let gone = sep(&["Gone", "x.flac"]);

        let mut plan = LibraryPlan::default();
        plan.assignments.insert("Keep".into(), dest_s.clone());
        plan.source_files.insert(
            "Keep".into(),
            vec![
                FileEntry {
                    rel_path: keep_old.clone(),
                    size: 8,
                    mtime: 99.0,
                    content_key: String::new(),
                },
                FileEntry {
                    rel_path: keep_new.clone(),
                    size: 6,
                    mtime: 99.0,
                    content_key: String::new(),
                },
            ],
        );
        plan.desired_files.insert(keep_old.clone());
        plan.desired_files.insert(keep_new.clone());

        let mut scan = DeviceScan::default();
        scan.files.insert(storage::fold_rel(&keep_old), (3, 0.0));
        scan.files.insert(storage::fold_rel(&gone), (5, 0.0));
        scan.artist_files
            .insert("Keep".into(), vec![keep_old.clone()]);
        scan.artist_files.insert("Gone".into(), vec![gone.clone()]);

        let mut report = ScanReport::default();
        report.plan = Some(plan);
        report.stale_files = vec![gone.clone()];
        report.device_scans.insert(dest_s.clone(), scan);

        let mut settings = crate::walkman::SyncSettings::default();
        settings.source_dir = src_s;
        settings.auto_detect_drives = false;
        settings.active_device = "test".into();
        settings.devices.insert(
            "test".into(),
            crate::walkman::DeviceProfile {
                name: "test".into(),
                internal_drive: dest_s,
                dest: "internal".into(),
                ..crate::walkman::DeviceProfile::default()
            },
        );
        let config = AppConfig {
            settings,
            lastfm_api_key: String::new(),
            lastfm_api_secret: String::new(),
            lastfm_username: String::new(),
            lastfm_session_key: String::new(),
            audiomuse_url: String::new(),
            audiomuse_token: String::new(),
            new_walkman: String::new(),
        };

        let mut labels = Vec::new();
        let result = sync_library(&config, &report, false, &mut |msg, _, _| {
            if msg.starts_with(['-', '~', '+']) && labels.last().map(String::as_str) != Some(msg) {
                labels.push(msg.to_string());
            }
        });
        assert_eq!(result.failed, 0, "{:?} labels={labels:?}", result.logs);
        let remove_at = labels.iter().position(|s| s.starts_with('-')).expect("remove");
        let replace_at = labels.iter().position(|s| s.starts_with('~')).expect("replace");
        let add_at = labels.iter().position(|s| s.starts_with('+')).expect("add");
        assert!(
            remove_at < replace_at && replace_at < add_at,
            "{labels:?}"
        );
        assert!(!dest.join("Gone").join("x.flac").exists());
        assert_eq!(fs::read(dest.join("Keep").join("old.flac")).unwrap(), b"replaced");
        assert_eq!(fs::read(dest.join("Keep").join("new.flac")).unwrap(), b"added!");
    }

    #[test]
    fn log_caps_and_summarizes_overflow() {
        let mut result = SyncResult::default();
        for i in 0..450 {
            result.log(format!("line {i}"));
        }
        assert_eq!(result.logs.len(), LOG_CAP + 1);
        assert!(
            result.logs.last().unwrap().contains("more"),
            "{}",
            result.logs.last().unwrap()
        );
    }

    #[test]
    fn playlist_line_strips_source_prefix_without_canonicalize() {
        let src = if cfg!(windows) {
            r"\\WillineauVault\Music"
        } else {
            "/Volumes/Music"
        };
        let abs = storage::join(src, &["Artist", "Album", "01.flac"]);
        let rel = strip_source_prefix(Path::new(&abs), src).expect("prefix");
        assert_eq!(
            storage::fold_rel(&rel),
            storage::fold_rel(&sep(&["Artist", "Album", "01.flac"]))
        );
        assert_eq!(
            strip_source_prefix(Path::new(&sep(&["Artist", "Album", "01.flac"])), src),
            None,
            "relative catalog paths are not a source prefix"
        );
    }

    #[test]
    fn playlist_tracks_prefers_catalog_relative_and_string_strip() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("Music");
        let lists = tmp.path().join("Playlists");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&lists).unwrap();
        let m3u = lists.join("mix.m3u8");
        let src_s = src.to_string_lossy().into_owned();
        let abs = storage::join(&src_s, &["Kept", "a.flac"]);
        let body = format!(
            "#EXTM3U\nKept{sep}a.flac\n{abs}\n# comment\n",
            sep = std::path::MAIN_SEPARATOR
        );
        fs::write(&m3u, body).unwrap();
        let tracks = playlist_tracks(&m3u, &src_s);
        assert_eq!(tracks.len(), 2, "{tracks:?}");
        let folded: Vec<String> = tracks.iter().map(|t| storage::fold_rel(t)).collect();
        let want = storage::fold_rel(&sep(&["Kept", "a.flac"]));
        assert!(folded.iter().all(|t| t == &want), "{folded:?}");
    }
}
