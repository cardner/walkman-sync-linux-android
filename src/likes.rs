//! Three-way likes: device ⇄ Last.fm ⇄ MusicBee, keyed per Walkman serial.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::catalog::{self, Catalog};
use crate::keys as audio_keys;
use crate::paths;
use crate::storage;
use crate::walkman::{self, KIND_ANDROID, KIND_STOCK};

pub const STATE_VERSION: u32 = 2;
pub const DEVICE_LOVED_TSV: &str = "cinder_loved.tsv";
pub const DEVICE_IMPORT_TSV: &str = "cinder_liked_import.tsv";
pub const LIKED_PLAYLIST: &str = "Liked Songs.m3u8";
pub const STOCK_LIKED_PLAYLIST: &str = "Liked Songs.m3u";
pub const IMPORT_HEADER: &str = "# artist\ttitle — liked list pushed from the PC (likesync).\n# Cinder replaces cinder_liked.conf on the next start and renames this file .done.\n";
const UNIT: char = '\u{241F}';
const LASTFM_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const USER_AGENT: &str = "likesync/1.0 (+walkman)";
const MIN_INTERVAL: f64 = 0.22;
const SOURCES: [&str; 3] = ["device", "lastfm", "musicbee"];

// ── track keys ─────────────────────────────────────────────────

fn base(value: &str) -> String {
    let punct: Vec<(char, char)> = [
        ('‘', '\''),
        ('’', '\''),
        ('‛', '\''),
        ('′', '\''),
        ('“', '"'),
        ('”', '"'),
        ('‐', '-'),
        ('‑', '-'),
        ('‒', '-'),
        ('–', '-'),
        ('—', '-'),
        ('―', '-'),
        ('−', '-'),
        ('\u{00a0}', ' '),
        ('\u{200b}', '\0'),
    ]
    .into_iter()
    .collect();
    let nfkc: String = value.nfkc().collect();
    let mapped: String = nfkc
        .chars()
        .filter(|c| !unicodedata_combining(*c))
        .map(|c| {
            punct
                .iter()
                .find(|(from, _)| *from == c)
                .map(|(_, to)| *to)
                .unwrap_or(c)
        })
        .filter(|c| *c != '\0')
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unicodedata_combining(c: char) -> bool {
    u8::from(unicode_normalization::char::canonical_combining_class(c)) != 0
}

fn noise_re() -> Regex {
    Regex::new(
        r"(?ix)\s*[\(\[\-–—]\s*(
        \d{4}\s+)?(re-?master(ed)?(\s+\d{4})?
        | remaster
        | explicit(\s+version)?
        | clean(\s+version)?
        | album\s+version
        | single\s+version
        | original\s+version
        | bonus\s+track
        | deluxe(\s+edition)?
        | expanded(\s+edition)?
        | anniversary(\s+edition)?
        | mono | stereo
        | \d{4}\s+(stereo\s+|mono\s+)?mix
        | (stereo|mono)\s+mix(\s+\d{4})?
    )\s*[\)\]]?\s*$",
    )
    .expect("noise")
}

fn feat_re() -> Regex {
    Regex::new(r"(?i)\s*[\(\[]?\s*(feat\.?|ft\.?|featuring|with)\s+[^\)\]]*[\)\]]?\s*$")
        .expect("feat")
}

fn multi_artist_re() -> Regex {
    Regex::new(r"(?i)\s*(?:;|/|\bvs\.?\b|\band\b|&|,)\s*").expect("multi")
}

pub fn norm_title(value: &str) -> String {
    let noise = noise_re();
    let feat = feat_re();
    let mut text = base(value);
    for _ in 0..3 {
        let stripped = feat.replace(&noise.replace(&text, ""), "").trim().to_string();
        if stripped == text {
            break;
        }
        text = if stripped.is_empty() { text } else { stripped };
    }
    text.to_lowercase()
}

pub fn norm_artist(value: &str) -> String {
    feat_re()
        .replace(&base(value), "")
        .trim()
        .to_lowercase()
}

pub fn primary_artist(value: &str) -> String {
    let n = norm_artist(value);
    multi_artist_re()
        .splitn(&n, 2)
        .next()
        .unwrap_or(&n)
        .trim()
        .to_string()
}

pub fn track_key(artist: &str, title: &str) -> String {
    format!("{}{UNIT}{}", norm_artist(artist), norm_title(title))
}

pub fn loose_key(artist: &str, title: &str) -> String {
    format!("{}{UNIT}{}", primary_artist(artist), norm_title(title))
}

pub fn split_key(key: &str) -> (String, String) {
    match key.split_once(UNIT) {
        Some((a, t)) => (a.to_string(), t.to_string()),
        None => (key.to_string(), String::new()),
    }
}

pub fn artist_candidates(value: &str) -> Vec<String> {
    let full = norm_artist(value);
    let mut seen = Vec::new();
    if !full.is_empty() {
        seen.push(full.clone());
    }
    for part in multi_artist_re().split(&full) {
        let part = part.trim();
        if !part.is_empty() && !seen.iter().any(|s| s == part) {
            seen.push(part.to_string());
        }
    }
    let raw = base(value).to_lowercase();
    if !raw.is_empty() && !seen.contains(&raw) {
        seen.push(raw);
    }
    seen
}

// ── state ──────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub keys: BTreeMap<String, TrackMeta>,
    #[serde(default)]
    pub taken: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrackMeta {
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub when: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(default)]
    pub snapshots: BTreeMap<String, Snapshot>,
    #[serde(default)]
    pub liked: BTreeMap<String, TrackMeta>,
    #[serde(default)]
    pub last_sync: f64,
    #[serde(default = "state_version")]
    pub version: u32,
    /// Relative paths last written to each stock volume's Liked Songs.m3u.
    /// Keyed `stock:<device id>:<internal|sd>`.
    #[serde(default)]
    pub stock_playlist_paths: BTreeMap<String, Vec<String>>,
}

fn state_version() -> u32 {
    STATE_VERSION
}

impl Default for State {
    fn default() -> Self {
        Self {
            path: paths::likes_state_path(),
            snapshots: BTreeMap::new(),
            liked: BTreeMap::new(),
            last_sync: 0.0,
            version: STATE_VERSION,
            stock_playlist_paths: BTreeMap::new(),
        }
    }
}

impl State {
    pub fn load(path: &Path) -> Self {
        let mut state = Self {
            path: path.to_path_buf(),
            ..Self::default()
        };
        let Ok(text) = fs::read_to_string(path) else {
            return state;
        };
        let Ok(data) = serde_json::from_str::<State>(&text) else {
            return state;
        };
        if data.version > STATE_VERSION {
            return state;
        }
        state.snapshots = data.snapshots;
        state.liked = data.liked;
        state.last_sync = data.last_sync;
        state.version = data.version;
        state.stock_playlist_paths = data.stock_playlist_paths;
        state
    }

    pub fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)
    }

    pub fn snapshot(&self, name: &str) -> Snapshot {
        self.snapshots.get(name).cloned().unwrap_or_default()
    }

    pub fn set_snapshot(&mut self, name: &str, keys: BTreeMap<String, TrackMeta>) {
        let taken = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        self.snapshots.insert(
            name.to_string(),
            Snapshot { keys, taken },
        );
    }

    pub fn migrate_legacy_device_snapshot(&mut self, device_id: &str) -> bool {
        if device_id.is_empty() || !self.snapshots.contains_key("device") {
            return false;
        }
        let keyed = format!("device:{device_id}");
        let legacy = self.snapshots.remove("device").unwrap();
        self.snapshots.entry(keyed).or_insert(legacy);
        true
    }

    pub fn first_run(&self) -> bool {
        self.snapshots.is_empty()
    }
}

// ── config ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LikesConfig {
    #[serde(default = "default_source")]
    pub source_dir: String,
    #[serde(default = "default_source")]
    pub playlist_dir: String,
    #[serde(default)]
    pub internal_root: String,
    #[serde(default)]
    pub sd_root: String,
    #[serde(default)]
    pub musicbee_loved_playlist: String,
    #[serde(default)]
    pub musicbee_rating_threshold: f64,
    #[serde(default = "default_true")]
    pub musicbee_export_playlist: bool,
    #[serde(default = "default_true")]
    pub lastfm_enabled: bool,
    #[serde(default = "default_conflict")]
    pub conflict: String,
    #[serde(default = "default_dest")]
    pub dest: String,
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_true")]
    pub device_playlist: bool,
    #[serde(default = "default_true")]
    pub device_import: bool,
    #[serde(default)]
    pub device_kind: String,
}

fn default_source() -> String {
    r"\\WillineauVault\Music".into()
}
fn default_true() -> bool {
    true
}
fn default_conflict() -> String {
    "liked".into()
}
fn default_dest() -> String {
    "auto".into()
}

impl Default for LikesConfig {
    fn default() -> Self {
        Self {
            source_dir: default_source(),
            playlist_dir: default_source(),
            internal_root: String::new(),
            sd_root: String::new(),
            musicbee_loved_playlist: String::new(),
            musicbee_rating_threshold: 0.0,
            musicbee_export_playlist: true,
            lastfm_enabled: true,
            conflict: default_conflict(),
            dest: default_dest(),
            device_id: String::new(),
            device_playlist: true,
            device_import: true,
            device_kind: String::new(),
        }
    }
}

impl LikesConfig {
    pub fn load() -> Self {
        let path = paths::data_root().join("likesync.json");
        let mut config = if let Ok(text) = fs::read_to_string(&path) {
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Self::default()
        };
        config.source_dir = walkman::resolve_library_path(&config.source_dir);
        config.playlist_dir = walkman::resolve_library_path(&config.playlist_dir);
        let settings = walkman::SyncSettings::load();
        config.device_kind = settings.device_kind();
        if config.device_kind == KIND_ANDROID || config.device_kind == KIND_STOCK {
            config.device_import = false;
        }
        if config.dest.is_empty() || config.dest == "auto" {
            config.dest = settings.dest();
        }
        config.device_id = settings.active_device.clone();
        if config.internal_root.is_empty() {
            config.internal_root = walkman::volume_root(&settings.internal_drive());
        }
        if config.sd_root.is_empty() {
            config.sd_root = walkman::volume_root(&settings.sd_drive());
        }
        walkman::apply_listen_credentials(&config.internal_root, &config.sd_root);
        config
    }

    pub fn save(&self) {
        let path = paths::data_root().join("likesync.json");
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, text);
        }
    }

    pub fn device_snapshot_key(&self) -> String {
        if self.device_id.is_empty() {
            "device".into()
        } else {
            format!("device:{}", self.device_id)
        }
    }

    pub fn is_stock(&self) -> bool {
        self.device_kind == KIND_STOCK
    }
}

// ── library index ──────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Entry {
    pub rel_path: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub track_no: i32,
}

fn rel_key(rel: &str) -> String {
    rel.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase()
}

#[derive(Clone, Debug, Default)]
pub struct LibraryIndex {
    pub source_dir: String,
    pub entries: Vec<Entry>,
    by_key: HashMap<String, usize>,
    by_loose: HashMap<String, usize>,
    by_folder: HashMap<String, usize>,
    by_rel: HashMap<String, usize>,
}

impl LibraryIndex {
    pub fn from_catalog(source_dir: &str) -> Self {
        let mut index = Self {
            source_dir: source_dir.to_string(),
            ..Self::default()
        };
        let Ok(cat) = Catalog::open(None) else {
            return index;
        };
        for row in cat.files_for(catalog::KIND_SOURCE, Some(source_dir)) {
            let suffix = Path::new(&row.rel)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy().to_ascii_lowercase()))
                .unwrap_or_default();
            if !storage::AUDIO_EXT.contains(&suffix.as_str()) {
                continue;
            }
            let mut artist = row.artist;
            let mut title = row.title;
            if artist.is_empty() || title.is_empty() {
                let tags = audio_keys::tags_from_filename(&row.rel);
                if artist.is_empty() {
                    artist = tags.best_artist().to_string();
                }
                if title.is_empty() {
                    title = tags.title;
                }
            }
            index.add(Entry {
                rel_path: row.rel,
                artist,
                title,
                album: row.album,
                track_no: row.track_no,
            });
        }
        index
    }

    fn add(&mut self, entry: Entry) {
        let key = track_key(&entry.artist, &entry.title);
        let loose = loose_key(&entry.artist, &entry.title);
        let folder = Path::new(&entry.rel_path)
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        let folder_artist = folder.split(" - ").next().unwrap_or(&folder);
        let idx = self.entries.len();
        self.by_key.entry(key).or_insert(idx);
        self.by_loose.entry(loose).or_insert(idx);
        self.by_folder
            .entry(format!("{}{UNIT}{}", folder_artist.to_lowercase(), norm_title(&entry.title)))
            .or_insert(idx);
        self.by_rel
            .entry(rel_key(&entry.rel_path))
            .or_insert(idx);
        self.entries.push(entry);
    }

    pub fn by_rel_path(&self, rel: &str) -> Option<&Entry> {
        self.by_rel
            .get(&rel_key(rel))
            .and_then(|&i| self.entries.get(i))
    }

    pub fn resolve(&self, artist: &str, title: &str) -> Option<&Entry> {
        let exact = track_key(artist, title);
        if let Some(&i) = self.by_key.get(&exact) {
            return self.entries.get(i);
        }
        let loose = loose_key(artist, title);
        if let Some(&i) = self.by_loose.get(&loose) {
            return self.entries.get(i);
        }
        for cand in artist_candidates(artist) {
            let k = format!("{cand}{UNIT}{}", norm_title(title));
            if let Some(&i) = self.by_key.get(&k).or_else(|| self.by_folder.get(&k)) {
                return self.entries.get(i);
            }
        }
        None
    }
}

// ── device ─────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct DeviceVolume {
    pub root: String,
    pub label: String,
    android: bool,
    stock: bool,
}

impl DeviceVolume {
    pub fn music_dir(&self) -> String {
        let leaf = self
            .root
            .replace('\\', "/")
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        if leaf.eq_ignore_ascii_case("music") {
            return self.root.clone();
        }
        for name in walkman::music_folder_names(self.android) {
            let c = storage::join(&self.root, &[name]);
            if storage::isdir(&c) {
                return c;
            }
        }
        storage::join(&self.root, &[walkman::music_folder_names(self.android)[0]])
    }
    pub fn loved_path(&self) -> String {
        storage::join(&self.root, &[DEVICE_LOVED_TSV])
    }
    pub fn import_path(&self) -> String {
        storage::join(&self.root, &[DEVICE_IMPORT_TSV])
    }
    pub fn liked_playlist_name(&self) -> &'static str {
        if self.stock {
            STOCK_LIKED_PLAYLIST
        } else {
            LIKED_PLAYLIST
        }
    }
    pub fn playlist_path(&self) -> String {
        storage::join(&self.music_dir(), &[self.liked_playlist_name()])
    }
    pub fn present(&self) -> bool {
        !self.root.is_empty() && storage::isdir(&self.root)
    }
    pub fn import_pending(&self) -> bool {
        storage::exists(&self.import_path())
    }
    pub fn import_is_foreign(&self) -> bool {
        if !storage::exists(&self.import_path()) {
            return false;
        }
        let Ok(head) = storage::read_text(&self.import_path()) else {
            return false;
        };
        let head = head[..head.len().min(800)].to_ascii_lowercase();
        if head.contains("likesync") {
            return false;
        }
        head.contains("listen_pull") || head.contains("flint")
    }
}

pub fn volumes(internal_root: &str, sd_root: &str) -> Vec<DeviceVolume> {
    let kind = walkman::SyncSettings::load().device_kind();
    volumes_of(internal_root, sd_root, &kind)
}

fn volumes_of(internal_root: &str, sd_root: &str, kind: &str) -> Vec<DeviceVolume> {
    let android = kind == KIND_ANDROID;
    let stock = kind == KIND_STOCK;
    let mut found = Vec::new();
    for (root, label) in [(internal_root, "internal"), (sd_root, "sd")] {
        if !root.is_empty() {
            found.push(DeviceVolume {
                root: root.to_string(),
                label: label.to_string(),
                android,
                stock,
            });
        }
    }
    found
}

fn config_volumes(config: &LikesConfig) -> Vec<DeviceVolume> {
    let kind = if config.device_kind.is_empty() {
        walkman::SyncSettings::load().device_kind()
    } else {
        config.device_kind.clone()
    };
    volumes_of(&config.internal_root, &config.sd_root, &kind)
        .into_iter()
        .filter(|v| v.present())
        .collect()
}

fn read_loved(volume: &DeviceVolume) -> BTreeMap<String, TrackMeta> {
    let mut result = BTreeMap::new();
    for candidate in [
        volume.loved_path(),
        storage::join(&volume.music_dir(), &[DEVICE_LOVED_TSV]),
    ] {
        if !storage::exists(&candidate) {
            continue;
        }
        let Ok(body) = storage::read_text(&candidate) else {
            continue;
        };
        for line in body.lines() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.splitn(2, '\t');
            let Some(artist) = parts.next() else { continue };
            let Some(title) = parts.next() else { continue };
            let artist = artist.trim();
            let title = title.trim();
            if !artist.is_empty() && !title.is_empty() {
                result.insert(
                    track_key(artist, title),
                    TrackMeta {
                        artist: artist.to_string(),
                        title: title.to_string(),
                        when: 0,
                    },
                );
            }
        }
        break;
    }
    result
}

fn read_all_loved(vols: &[DeviceVolume]) -> BTreeMap<String, TrackMeta> {
    let mut merged = BTreeMap::new();
    for volume in vols {
        if volume.present() {
            merged.append(&mut read_loved(volume));
        }
    }
    merged
}

fn write_import(volume: &DeviceVolume, tracks: &[TrackMeta], dry_run: bool) -> usize {
    if dry_run {
        return tracks.len();
    }
    let mut body = IMPORT_HEADER.to_string();
    for track in tracks {
        body.push_str(&track.artist);
        body.push('\t');
        body.push_str(&track.title);
        body.push('\n');
    }
    let _ = storage::write_text(&volume.import_path(), &body);
    tracks.len()
}

fn write_device_playlist(volume: &DeviceVolume, rel_paths: &[String], dry_run: bool) -> usize {
    if dry_run {
        return rel_paths.len();
    }
    let mut body = String::from("#EXTM3U\n");
    for path in rel_paths {
        body.push_str(&path.replace('\\', "/"));
        body.push('\n');
    }
    let _ = storage::write_text(&volume.playlist_path(), &body);
    if volume.stock {
        let sibling = storage::join(&volume.music_dir(), &[LIKED_PLAYLIST]);
        if storage::fold_path(&sibling) != storage::fold_path(&volume.playlist_path()) {
            let _ = storage::remove(&sibling);
        }
    }
    rel_paths.len()
}

fn stock_playlist_key(device_id: &str, label: &str) -> String {
    let id = if device_id.is_empty() { "device" } else { device_id };
    format!("stock:{id}:{label}")
}

fn m3u_track_paths(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = text
        .lines()
        .map(|line| line.trim().trim_matches('"'))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.replace('\\', "/"))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

struct StockRead {
    tracks: BTreeMap<String, TrackMeta>,
    matched_last_write: bool,
}

fn read_stock_likes(
    config: &LikesConfig,
    vols: &[DeviceVolume],
    index: &LibraryIndex,
    state: &State,
) -> Option<StockRead> {
    let mut saw = false;
    let mut all_match = true;
    let mut resolved = BTreeMap::new();
    for volume in vols {
        let path = volume.playlist_path();
        if !storage::exists(&path) {
            continue;
        }
        let Ok(text) = storage::read_text(&path) else {
            continue;
        };
        saw = true;
        let paths = m3u_track_paths(&text);
        let stored = state
            .stock_playlist_paths
            .get(&stock_playlist_key(&config.device_id, &volume.label));
        if stored.map(Vec::as_slice) != Some(paths.as_slice()) {
            all_match = false;
        }
        for rel in &paths {
            if let Some(entry) = index.by_rel_path(rel) {
                resolved.insert(
                    track_key(&entry.artist, &entry.title),
                    TrackMeta {
                        artist: entry.artist.clone(),
                        title: entry.title.clone(),
                        when: 0,
                    },
                );
            }
        }
    }
    if !saw {
        return None;
    }
    let snap_name = config.device_snapshot_key();
    if all_match && state.snapshots.contains_key(&snap_name) {
        return Some(StockRead {
            tracks: state.snapshot(&snap_name).keys,
            matched_last_write: true,
        });
    }
    Some(StockRead {
        tracks: resolved,
        matched_last_write: false,
    })
}

fn remove_playlist(volume: &DeviceVolume, dry_run: bool) -> bool {
    if !storage::exists(&volume.playlist_path()) {
        return false;
    }
    if !dry_run {
        return storage::remove(&volume.playlist_path()).is_ok();
    }
    true
}

// ── Last.fm ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Lastfm {
    pub api_key: String,
    pub api_secret: String,
    pub session_key: String,
    pub username: String,
    last_call: Instant,
}

#[derive(Debug)]
pub struct LastfmError(pub String);

impl std::fmt::Display for LastfmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for LastfmError {}

impl Lastfm {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("LASTFM_API_KEY").unwrap_or_default();
        let api_secret = std::env::var("LASTFM_API_SECRET").unwrap_or_default();
        if api_key.is_empty() || api_secret.is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            api_secret,
            session_key: std::env::var("LASTFM_SESSION_KEY").unwrap_or_default(),
            username: std::env::var("LASTFM_USERNAME").unwrap_or_default(),
            last_call: Instant::now() - Duration::from_secs(1),
        })
    }

    fn throttle(&mut self) {
        let gap = self.last_call.elapsed().as_secs_f64();
        if gap < MIN_INTERVAL {
            std::thread::sleep(Duration::from_secs_f64(MIN_INTERVAL - gap));
        }
        self.last_call = Instant::now();
    }

    fn signature(&self, params: &BTreeMap<String, String>) -> String {
        let mut base = String::new();
        for (k, v) in params {
            base.push_str(k);
            base.push_str(v);
        }
        base.push_str(&self.api_secret);
        hex::encode(&*md5::compute(base.as_bytes()))
    }

    fn call(
        &mut self,
        mut params: BTreeMap<String, String>,
        post: bool,
        signed: bool,
    ) -> Result<String, LastfmError> {
        params.insert("api_key".into(), self.api_key.clone());
        if signed {
            params.insert("sk".into(), self.session_key.clone());
            let sig = self.signature(&params);
            params.insert("api_sig".into(), sig);
        }
        let body: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut last_err = LastfmError("Last.fm request failed".into());
        for attempt in 0..4 {
            self.throttle();
            let result = if post {
                ureq::post(LASTFM_URL)
                    .set("User-Agent", USER_AGENT)
                    .set(
                        "Content-Type",
                        "application/x-www-form-urlencoded; charset=utf-8",
                    )
                    .send_string(&body)
            } else {
                ureq::get(&format!("{LASTFM_URL}?{body}"))
                    .set("User-Agent", USER_AGENT)
                    .call()
            };
            match result {
                Ok(resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    if text.contains("status=\"ok\"") || text.contains("status='ok'") {
                        return Ok(text);
                    }
                    last_err = LastfmError(xml_error(&text));
                    if last_err.0.contains("error 29") {
                        std::thread::sleep(Duration::from_secs(2u64.pow(attempt) + 1));
                        continue;
                    }
                    return Err(last_err);
                }
                Err(err) => {
                    last_err = LastfmError(err.to_string());
                    std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
                }
            }
        }
        Err(last_err)
    }

    pub fn authenticate(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<(String, String), LastfmError> {
        let mut params = BTreeMap::new();
        params.insert("method".into(), "auth.getMobileSession".into());
        params.insert("username".into(), username.into());
        params.insert("password".into(), password.into());
        let xml = self.call(params, true, true)?;
        let name = xml_tag(&xml, "name").ok_or_else(|| LastfmError("no session name".into()))?;
        let key = xml_tag(&xml, "key").ok_or_else(|| LastfmError("no session key".into()))?;
        self.username = name.clone();
        self.session_key = key.clone();
        Ok((name, key))
    }

    pub fn loved_tracks(&mut self) -> Result<BTreeMap<String, TrackMeta>, LastfmError> {
        if self.username.is_empty() {
            return Err(LastfmError("no Last.fm username configured".into()));
        }
        let mut loved = BTreeMap::new();
        let mut page = 1u32;
        let mut pages = 1u32;
        while page <= pages {
            let mut params = BTreeMap::new();
            params.insert("method".into(), "user.getLovedTracks".into());
            params.insert("user".into(), self.username.clone());
            params.insert("limit".into(), "1000".into());
            params.insert("page".into(), page.to_string());
            let xml = self.call(params, false, false)?;
            if let Some(total) = attr_after(&xml, "totalPages=\"") {
                pages = total.parse().unwrap_or(1);
            }
            for track in xml.split("<track").skip(1) {
                let artist = artist_from_track(track).unwrap_or_default();
                let title = xml_first_name(track).unwrap_or_default();
                if artist.is_empty() || title.is_empty() {
                    continue;
                }
                loved.insert(
                    track_key(&artist, &title),
                    TrackMeta {
                        artist,
                        title,
                        when: 0,
                    },
                );
            }
            page += 1;
        }
        Ok(loved)
    }

    pub fn love(&mut self, artist: &str, title: &str) -> Result<(), LastfmError> {
        let mut params = BTreeMap::new();
        params.insert("method".into(), "track.love".into());
        params.insert("artist".into(), artist.into());
        params.insert("track".into(), title.into());
        self.call(params, true, true).map(|_| ())
    }

    pub fn unlove(&mut self, artist: &str, title: &str) -> Result<(), LastfmError> {
        let mut params = BTreeMap::new();
        params.insert("method".into(), "track.unlove".into());
        params.insert("artist".into(), artist.into());
        params.insert("track".into(), title.into());
        self.call(params, true, true).map(|_| ())
    }

    pub fn scrobble_batch(
        &mut self,
        entries: &[(String, String, i64)],
    ) -> Result<(u32, Vec<String>), LastfmError> {
        let mut params = BTreeMap::new();
        params.insert("method".into(), "track.scrobble".into());
        for (i, (artist, title, ts)) in entries.iter().enumerate() {
            params.insert(format!("artist[{i}]"), artist.clone());
            params.insert(format!("track[{i}]"), title.clone());
            params.insert(format!("timestamp[{i}]"), ts.to_string());
        }
        let xml = self.call(params, true, true)?;
        let accepted = attr_after(&xml, "accepted=\"")
            .and_then(|s| s.parse().ok())
            .unwrap_or(entries.len() as u32);
        Ok((accepted, Vec::new()))
    }
}

fn artist_from_track(track: &str) -> Option<String> {
    let start = track.find("<artist")?;
    let slice = &track[start..];
    xml_tag(slice, "name").or_else(|| {
        slice
            .find('>')
            .map(|i| slice[i + 1..].split('<').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn xml_first_name(track: &str) -> Option<String> {
    xml_tag(track, "name")
}

fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let start = xml.find(&open)?;
    let after = xml[start..].find('>')? + start + 1;
    let close = format!("</{tag}>");
    let end = xml[after..].find(&close)? + after;
    Some(xml[after..end].trim().to_string())
}

fn xml_error(xml: &str) -> String {
    xml_tag(xml, "error").unwrap_or_else(|| "unknown Last.fm error".into())
}

fn attr_after<'a>(xml: &'a str, prefix: &str) -> Option<String> {
    let start = xml.find(prefix)? + prefix.len();
    let rest = &xml[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── MusicBee ───────────────────────────────────────────────────

fn find_loved_playlist(playlist_dir: &str) -> String {
    let Ok(read) = fs::read_dir(playlist_dir) else {
        return String::new();
    };
    let wanted = ["loved", "love", "liked", "likes", "favourite", "favorite", "hearts"];
    for entry in read.flatten() {
        let path = entry.path();
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(ext.as_str(), "m3u" | "m3u8") && wanted.contains(&stem.as_str()) {
            return path.to_string_lossy().into_owned();
        }
    }
    String::new()
}

fn read_loved_playlist(
    path: &str,
    source_dir: &str,
    index: &LibraryIndex,
) -> BTreeMap<String, TrackMeta> {
    let Ok(body) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let playlist_dir = Path::new(path).parent().unwrap_or(Path::new("."));
    let source = PathBuf::from(source_dir);
    let mut loved = BTreeMap::new();
    for raw in body.lines() {
        let line = raw.trim().trim_matches('"');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let candidate = PathBuf::from(line.replace('\\', std::path::MAIN_SEPARATOR_STR));
        let resolved = if candidate.is_absolute() {
            candidate
        } else {
            playlist_dir.join(&candidate)
        };
        if let Ok(rel) = resolved.strip_prefix(&source) {
            let rel_s = rel.to_string_lossy();
            if let Some(entry) = index
                .entries
                .iter()
                .find(|e| storage::fold_rel(&e.rel_path) == storage::fold_rel(&rel_s))
            {
                loved.insert(
                    track_key(&entry.artist, &entry.title),
                    TrackMeta {
                        artist: entry.artist.clone(),
                        title: entry.title.clone(),
                        when: 0,
                    },
                );
            }
        }
    }
    loved
}

fn write_musicbee_playlist(path: &Path, entries: &[&Entry], source_dir: &str, dry_run: bool) -> usize {
    if dry_run {
        return entries.len();
    }
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut lines = vec!["#EXTM3U".to_string()];
    for entry in entries {
        let absolute = Path::new(source_dir).join(&entry.rel_path);
        let relative = path
            .parent()
            .and_then(|p| pathdiff(&absolute, p))
            .unwrap_or_else(|| absolute.to_string_lossy().into_owned());
        lines.push(relative.replace('/', "\\"));
    }
    let _ = fs::write(path, lines.join("\n") + "\n");
    entries.len()
}

fn pathdiff(path: &Path, base: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    let base = base.canonicalize().ok()?;
    path.strip_prefix(base)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

// ── engine ─────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SourceView {
    pub name: String,
    pub available: bool,
    pub tracks: BTreeMap<String, TrackMeta>,
    pub note: String,
    pub additive_only: bool,
}

impl SourceView {
    pub fn count(&self) -> usize {
        self.tracks.len()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SinkPlan {
    pub add: Vec<String>,
    pub remove: Vec<String>,
    pub enabled: bool,
    pub note: String,
}

#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub desired: BTreeMap<String, TrackMeta>,
    pub views: BTreeMap<String, SourceView>,
    pub sinks: BTreeMap<String, SinkPlan>,
    pub matched: BTreeMap<String, Entry>,
    pub unmatched: Vec<String>,
    pub playlist: BTreeMap<String, Vec<String>>,
    pub off_device: Vec<String>,
    pub first_run: bool,
    pub notes: Vec<String>,
    pub conflicts: Vec<String>,
}

impl Plan {
    pub fn label(&self, key: &str) -> String {
        if let Some(meta) = self.desired.get(key) {
            return format!("{} — {}", meta.artist, meta.title);
        }
        let (a, t) = split_key(key);
        format!("{a} — {t}")
    }
}

#[derive(Clone, Debug, Default)]
pub struct ApplyResult {
    pub loved: u32,
    pub unloved: u32,
    pub lastfm_failed: Vec<(String, String)>,
    pub logs: Vec<String>,
}

impl ApplyResult {
    pub fn log(&mut self, m: impl Into<String>) {
        self.logs.push(m.into());
    }
}

pub fn gather(
    config: &LikesConfig,
    index: &LibraryIndex,
    lastfm: Option<&mut Lastfm>,
    state: &State,
) -> BTreeMap<String, SourceView> {
    let mut views: BTreeMap<String, SourceView> = SOURCES
        .iter()
        .map(|n| {
            (
                (*n).to_string(),
                SourceView {
                    name: (*n).to_string(),
                    ..SourceView::default()
                },
            )
        })
        .collect();

    let vols = config_volumes(config);
    if !vols.is_empty() && config.device_import {
        let view = views.get_mut("device").unwrap();
        view.available = true;
        view.tracks = read_all_loved(&vols);
        if vols.iter().any(|v| v.import_is_foreign()) {
            view.additive_only = true;
            view.note = "foreign liked import — additive only".into();
        } else if vols.iter().any(|v| v.import_pending()) {
            view.additive_only = true;
            view.note = "import still pending on the device".into();
        } else {
            view.note = format!("{} volume(s)", vols.len());
        }
    } else if !vols.is_empty() && config.is_stock() {
        let view = views.get_mut("device").unwrap();
        match read_stock_likes(config, &vols, index, state) {
            None => view.note = "no Liked Songs.m3u".into(),
            Some(read) => {
                view.available = true;
                view.tracks = read.tracks;
                view.note = if read.matched_last_write {
                    "playlist matches last write".into()
                } else {
                    "Liked Songs.m3u".into()
                };
            }
        }
    } else if !vols.is_empty() {
        views.get_mut("device").unwrap().note = "playlist only".into();
    } else {
        views.get_mut("device").unwrap().note = "no Walkman volume found".into();
    }

    let view = views.get_mut("lastfm").unwrap();
    if !config.lastfm_enabled {
        view.note = "disabled in settings".into();
    } else if lastfm.as_ref().map(|l| l.username.is_empty()).unwrap_or(true) {
        view.note = "not signed in".into();
    } else if let Some(lfm) = lastfm {
        match lfm.loved_tracks() {
            Ok(tracks) => {
                view.tracks = tracks;
                view.available = true;
                view.note = format!("as {}", lfm.username);
            }
            Err(err) => view.note = err.to_string(),
        }
    }

    let view = views.get_mut("musicbee").unwrap();
    #[cfg(windows)]
    {
        let playlist_path = if config.musicbee_loved_playlist.is_empty() {
            find_loved_playlist(&config.playlist_dir)
        } else {
            config.musicbee_loved_playlist.clone()
        };
        if !playlist_path.is_empty() && Path::new(&playlist_path).is_file() {
            view.tracks = read_loved_playlist(&playlist_path, &config.source_dir, index);
            view.available = true;
            view.note = Path::new(&playlist_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
        } else {
            view.note = "no loved playlist".into();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = index;
        view.note = "MusicBee is Windows-only".into();
    }
    views
}

pub fn build_plan(
    config: &LikesConfig,
    index: &LibraryIndex,
    views: BTreeMap<String, SourceView>,
    state: &State,
) -> Plan {
    let mut plan = Plan {
        views,
        first_run: state.first_run(),
        ..Plan::default()
    };
    let mut added_by: HashMap<String, HashSet<String>> = HashMap::new();
    let mut removed_by: HashMap<String, HashSet<String>> = HashMap::new();
    for (name, view) in &plan.views {
        if !view.available {
            continue;
        }
        let snap_name = if name == "device" {
            config.device_snapshot_key()
        } else {
            name.clone()
        };
        let previous = state.snapshot(&snap_name);
        let seen_before = !previous.keys.is_empty() || state.snapshots.contains_key(&snap_name);
        for key in view.tracks.keys() {
            if !seen_before || !previous.keys.contains_key(key) {
                added_by.entry(key.clone()).or_default().insert(name.clone());
            }
        }
        if seen_before && !view.additive_only {
            for key in previous.keys.keys() {
                if !view.tracks.contains_key(key) {
                    removed_by.entry(key.clone()).or_default().insert(name.clone());
                }
            }
        }
    }
    let mut desired = state.liked.clone();
    if desired.is_empty() {
        for name in SOURCES {
            if let Some(view) = plan.views.get(name) {
                if view.available {
                    for (key, meta) in &view.tracks {
                        desired.entry(key.clone()).or_insert_with(|| meta.clone());
                    }
                }
            }
        }
    }
    let mut conflicts = Vec::new();
    let all_keys: HashSet<String> = added_by.keys().chain(removed_by.keys()).cloned().collect();
    for key in all_keys {
        let adders = added_by.get(&key).cloned().unwrap_or_default();
        let mut removers = removed_by.get(&key).cloned().unwrap_or_default();
        if !adders.is_empty() && !removers.is_empty() {
            conflicts.push(key.clone());
            if config.conflict != "latest" {
                removers.clear();
            }
        }
        if !adders.is_empty() && removers.is_empty() {
            let meta = meta_for(&key, &plan.views, &adders);
            desired.entry(key).or_insert(meta);
        } else if !removers.is_empty() {
            desired.remove(&key);
        }
    }
    plan.desired = desired;
    plan.conflicts = conflicts;
    for name in SOURCES {
        let view = plan.views.get(name).cloned().unwrap_or_default();
        let mut sink = SinkPlan {
            enabled: view.available,
            note: view.note.clone(),
            ..SinkPlan::default()
        };
        if view.available {
            sink.add = plan
                .desired
                .keys()
                .filter(|k| !view.tracks.contains_key(*k))
                .cloned()
                .collect();
            sink.remove = view
                .tracks
                .keys()
                .filter(|k| !plan.desired.contains_key(*k))
                .cloned()
                .collect();
        }
        plan.sinks.insert(name.to_string(), sink);
    }
    if !config.lastfm_enabled {
        if let Some(s) = plan.sinks.get_mut("lastfm") {
            s.enabled = false;
        }
    }
    if !config.musicbee_export_playlist {
        if let Some(s) = plan.sinks.get_mut("musicbee") {
            s.enabled = false;
        }
    }
    let vols = config_volumes(config);
    for volume in &vols {
        plan.playlist.insert(volume.root.clone(), Vec::new());
    }
    let desired_keys: Vec<String> = plan.desired.keys().cloned().collect();
    for key in desired_keys {
        let meta = plan.desired[&key].clone();
        match index.resolve(&meta.artist, &meta.title) {
            None => plan.unmatched.push(key),
            Some(entry) => {
                let entry = entry.clone();
                let mut placed = false;
                for volume in &vols {
                    if storage::exists(&storage::join(&volume.music_dir(), &[&entry.rel_path])) {
                        plan.playlist
                            .entry(volume.root.clone())
                            .or_default()
                            .push(entry.rel_path.clone());
                        placed = true;
                        break;
                    }
                }
                if !vols.is_empty() && !placed {
                    plan.off_device.push(key.clone());
                }
                plan.matched.insert(key, entry);
            }
        }
    }
    if plan.first_run {
        plan.notes
            .push("First run: nothing is removed anywhere — the three lists are merged.".into());
    }
    plan
}

fn meta_for(
    key: &str,
    views: &BTreeMap<String, SourceView>,
    sources: &HashSet<String>,
) -> TrackMeta {
    for name in ["device", "musicbee", "lastfm"] {
        if sources.contains(name) {
            if let Some(meta) = views.get(name).and_then(|v| v.tracks.get(key)) {
                return meta.clone();
            }
        }
    }
    let (artist, title) = split_key(key);
    TrackMeta {
        artist,
        title,
        when: 0,
    }
}

pub fn apply_plan(
    config: &LikesConfig,
    plan: &Plan,
    state: &mut State,
    lastfm: Option<&mut Lastfm>,
    dry_run: bool,
) -> ApplyResult {
    let mut result = ApplyResult::default();
    let sink = plan.sinks.get("lastfm").cloned().unwrap_or_default();
    let mut lastfm_ok = true;
    if sink.enabled {
        if let Some(lfm) = lastfm {
            for key in &sink.add {
                let meta = &plan.desired[key];
                if dry_run {
                    result.loved += 1;
                    continue;
                }
                match lfm.love(&meta.artist, &meta.title) {
                    Ok(()) => result.loved += 1,
                    Err(err) => {
                        lastfm_ok = false;
                        result.lastfm_failed.push((key.clone(), err.to_string()));
                    }
                }
            }
            for key in &sink.remove {
                let meta = plan
                    .views
                    .get("lastfm")
                    .and_then(|v| v.tracks.get(key))
                    .cloned()
                    .unwrap_or_default();
                if dry_run {
                    result.unloved += 1;
                    continue;
                }
                match lfm.unlove(&meta.artist, &meta.title) {
                    Ok(()) => result.unloved += 1,
                    Err(err) => {
                        lastfm_ok = false;
                        result.lastfm_failed.push((key.clone(), err.to_string()));
                    }
                }
            }
            result.log(format!(
                "Last.fm: {} loved, {} unloved, {} failed",
                result.loved,
                result.unloved,
                result.lastfm_failed.len()
            ));
        }
    }
    let vols = config_volumes(config);
    let tracks: Vec<TrackMeta> = plan.desired.values().cloned().collect();
    let device_sink = plan.sinks.get("device").cloned().unwrap_or_default();
    let device_needs = !device_sink.add.is_empty() || !device_sink.remove.is_empty();
    let dest_root = dest_volume_root(config, &vols, plan);
    for (position, volume) in vols.iter().enumerate() {
        if config.device_import && position == 0 {
            if volume.import_is_foreign() {
                result.log("Device: leaving foreign liked import in place");
            } else if device_needs {
                write_import(volume, &tracks, dry_run);
                result.log(format!(
                    "Device: wrote {} track(s) to {}",
                    tracks.len(),
                    DEVICE_IMPORT_TSV
                ));
            } else if volume.import_pending() && !dry_run {
                let _ = storage::remove(&volume.import_path());
                result.log("Device: dropped a redundant import");
            }
        }
        if config.device_playlist {
            let is_dest = dest_root
                .as_ref()
                .map(|r| storage::fold_path(r) == storage::fold_path(&volume.root))
                .unwrap_or(false);
            let rel_paths = plan.playlist.get(&volume.root).cloned().unwrap_or_default();
            if is_dest && !rel_paths.is_empty() {
                write_device_playlist(volume, &rel_paths, dry_run);
                if volume.stock && !dry_run {
                    state.stock_playlist_paths.insert(
                        stock_playlist_key(&config.device_id, &volume.label),
                        m3u_track_paths(&rel_paths.join("\n")),
                    );
                }
                result.log(format!(
                    "Device: {} — {} track(s)",
                    volume.playlist_path(),
                    rel_paths.len()
                ));
            } else if is_dest && rel_paths.is_empty() {
                if remove_playlist(volume, dry_run) {
                    result.log("Device: removed empty Liked Songs playlist");
                }
            } else if !is_dest && remove_playlist(volume, dry_run) {
                result.log("Device: removed Liked Songs from unused volume");
            }
        }
    }
    if config.musicbee_export_playlist {
        let target = Path::new(&config.playlist_dir).join(LIKED_PLAYLIST);
        let entries: Vec<&Entry> = plan
            .desired
            .keys()
            .filter_map(|k| plan.matched.get(k))
            .collect();
        let n = write_musicbee_playlist(&target, &entries, &config.source_dir, dry_run);
        result.log(format!("MusicBee: {} — {n} track(s)", target.display()));
    }
    if !dry_run {
        state.liked = plan.desired.clone();
        state.last_sync = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        if plan.views.get("lastfm").map(|v| v.available).unwrap_or(false) && lastfm_ok {
            state.set_snapshot("lastfm", plan.desired.clone());
        }
        if plan.views.get("device").map(|v| v.available).unwrap_or(false) {
            let keys = if config.device_import {
                plan.desired.clone()
            } else {
                plan.views["device"].tracks.clone()
            };
            state.set_snapshot(&config.device_snapshot_key(), keys);
        }
        if plan.views.get("musicbee").map(|v| v.available).unwrap_or(false)
            || config.musicbee_export_playlist
        {
            let keys = if config.musicbee_export_playlist {
                plan.desired.clone()
            } else {
                plan.views["musicbee"].tracks.clone()
            };
            state.set_snapshot("musicbee", keys);
        }
        let _ = state.save();
    }
    result
}

fn dest_volume_root(config: &LikesConfig, vols: &[DeviceVolume], plan: &Plan) -> Option<String> {
    if vols.is_empty() {
        return None;
    }
    match config.dest.as_str() {
        "sd" => Some(vols.get(1).unwrap_or(&vols[0]).root.clone()),
        "internal" => Some(vols[0].root.clone()),
        _ => vols
            .iter()
            .find(|v| plan.playlist.get(&v.root).is_some_and(|p| !p.is_empty()))
            .map(|v| v.root.clone())
            .or_else(|| Some(vols[0].root.clone())),
    }
}

// ── session ────────────────────────────────────────────────────

pub struct Session {
    pub config: LikesConfig,
    pub state: State,
    pub index: Option<LibraryIndex>,
    pub lastfm: Option<Lastfm>,
    pub plan: Option<Plan>,
}

impl Session {
    pub fn new() -> Self {
        paths::load_dotenv();
        let config = LikesConfig::load();
        let mut state = State::load(&paths::likes_state_path());
        if state.migrate_legacy_device_snapshot(&config.device_id) {
            let _ = state.save();
        }
        let lastfm = Lastfm::from_env();
        Self {
            config,
            state,
            index: None,
            lastfm,
            plan: None,
        }
    }

    pub fn refresh(&mut self, status: &mut dyn FnMut(&str)) -> Plan {
        status("Indexing library for likes");
        if self.index.is_none() {
            self.index = Some(LibraryIndex::from_catalog(&self.config.source_dir));
        }
        status("Reading likes from device, Last.fm and MusicBee");
        let views = gather(
            &self.config,
            self.index.as_ref().unwrap(),
            self.lastfm.as_mut(),
            &self.state,
        );
        let plan = build_plan(
            &self.config,
            self.index.as_ref().unwrap(),
            views,
            &self.state,
        );
        self.plan = Some(plan.clone());
        plan
    }

    pub fn apply(&mut self, dry_run: bool, status: &mut dyn FnMut(&str)) -> ApplyResult {
        if self.plan.is_none() {
            self.refresh(status);
        }
        let plan = self.plan.clone().unwrap();
        apply_plan(
            &self.config,
            &plan,
            &mut self.state,
            self.lastfm.as_mut(),
            dry_run,
        )
    }

    pub fn sign_in(&mut self, username: &str, password: &str) -> Result<String, LastfmError> {
        let lfm = self
            .lastfm
            .as_mut()
            .ok_or_else(|| LastfmError("Last.fm API key and secret are not set".into()))?;
        let (name, key) = lfm.authenticate(username, password)?;
        paths::save_env_value("LASTFM_USERNAME", &name);
        paths::save_env_value("LASTFM_SESSION_KEY", &key);
        Ok(name)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

// ── scrobbles ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ScrobbleEntry {
    pub artist: String,
    pub album: String,
    pub track: String,
    pub timestamp: i64,
    pub source_flag: String,
}

pub fn parse_scrobbler_log(text: &str) -> (Vec<String>, Vec<ScrobbleEntry>) {
    let mut header = Vec::new();
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') {
            header.push(line.to_string());
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        entries.push(ScrobbleEntry {
            artist: parts[0].to_string(),
            album: parts[1].to_string(),
            track: parts[2].to_string(),
            timestamp: parts[6].parse().unwrap_or(0),
            source_flag: parts.get(5).copied().unwrap_or("L").to_string(),
        });
    }
    (header, entries)
}

pub fn is_uploadable_scrobble(entry: &ScrobbleEntry) -> bool {
    entry.source_flag.eq_ignore_ascii_case("L")
        && !entry.artist.trim().is_empty()
        && !entry.track.trim().is_empty()
        && entry.timestamp != 0
}

pub fn count_uploadable_scrobbles(text: &str) -> usize {
    let (_, entries) = parse_scrobbler_log(text);
    entries.iter().filter(|entry| is_uploadable_scrobble(entry)).count()
}

fn scrobble_log_body(header: &[String], entries: &[ScrobbleEntry]) -> String {
    let mut body = header.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    for entry in entries {
        body.push_str(&format!(
            "{}\t{}\t{}\t\t\t{}\t{}\n",
            entry.artist, entry.album, entry.track, entry.source_flag, entry.timestamp
        ));
    }
    body
}

pub fn upload_scrobbles(
    config: &crate::sync::AppConfig,
    dry_run: bool,
) -> crate::sync::SyncResult {
    let mut result = crate::sync::SyncResult::default();
    let mut lastfm = match Lastfm::from_env() {
        Some(l) if !l.session_key.is_empty() => l,
        _ => {
            result.log("Last.fm is not signed in.");
            return result;
        }
    };
    let files = crate::sync::list_scrobble_files(config);
    if files.is_empty() {
        result.log(crate::sync::missing_scrobble_message(config.is_stock()));
        return result;
    }
    for file in files {
        let Ok(text) = storage::read_text(&file.path) else {
            continue;
        };
        let (header, entries) = parse_scrobbler_log(&text);
        let listened: Vec<_> = entries
            .iter()
            .filter(|entry| is_uploadable_scrobble(entry))
            .cloned()
            .collect();
        result.log(format!("{}: {} pending", file.path, listened.len()));
        if dry_run || listened.is_empty() {
            continue;
        }
        let batch: Vec<(String, String, i64)> = listened
            .iter()
            .map(|e| (e.artist.clone(), e.track.clone(), e.timestamp))
            .collect();
        match lastfm.scrobble_batch(&batch) {
            Ok((accepted, _)) => {
                result.log(format!("uploaded {accepted}"));
                let remaining: Vec<_> = entries
                    .into_iter()
                    .filter(|entry| !is_uploadable_scrobble(entry))
                    .collect();
                let _ = storage::write_text(&file.path, &scrobble_log_body(&header, &remaining));
            }
            Err(err) => result.log(format!("scrobble failed: {err}")),
        }
    }
    result
}

#[cfg(test)]
mod stock_tests {
    use std::fs;
    use std::path::Path;

    use super::*;

    fn index_with(rel: &str, artist: &str, title: &str) -> LibraryIndex {
        let mut index = LibraryIndex::default();
        index.add(Entry {
            rel_path: rel.to_string(),
            artist: artist.to_string(),
            title: title.to_string(),
            album: String::new(),
            track_no: 1,
        });
        index
    }

    fn stock_config(root: &str) -> LikesConfig {
        LikesConfig {
            internal_root: root.to_string(),
            device_kind: KIND_STOCK.into(),
            device_import: false,
            device_id: "wm1a".into(),
            device_playlist: true,
            dest: "internal".into(),
            lastfm_enabled: false,
            musicbee_export_playlist: false,
            ..LikesConfig::default()
        }
    }

    fn meta(artist: &str, title: &str) -> (String, TrackMeta) {
        (
            track_key(artist, title),
            TrackMeta {
                artist: artist.to_string(),
                title: title.to_string(),
                when: 0,
            },
        )
    }

    #[test]
    fn missing_m3u_does_not_unlike() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_string_lossy().into_owned();
        fs::create_dir_all(Path::new(&root).join("MUSIC")).unwrap();
        let config = stock_config(&root);
        let index = index_with("Artist/01.flac", "Queen", "Night");
        let (key, track) = meta("Queen", "Night");
        let mut state = State::default();
        state.liked.insert(key.clone(), track.clone());
        state.set_snapshot(&config.device_snapshot_key(), state.liked.clone());
        let views = gather(&config, &index, None, &state);
        assert!(!views["device"].available);
        let plan = build_plan(&config, &index, views, &state);
        assert!(plan.desired.contains_key(&key));
    }

    #[test]
    fn matching_last_write_is_not_a_device_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let music = root.join("MUSIC");
        fs::create_dir_all(&music).unwrap();
        fs::write(music.join("Liked Songs.m3u"), "#EXTM3U\nArtist/01.flac\n").unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let config = stock_config(&root_s);
        let index = index_with("Artist/01.flac", "Queen", "Night");
        let (key, track) = meta("Queen", "Night");
        let mut state = State::default();
        state.liked.insert(key.clone(), track.clone());
        state.set_snapshot(&config.device_snapshot_key(), state.liked.clone());
        state.stock_playlist_paths.insert(
            stock_playlist_key(&config.device_id, "internal"),
            vec!["Artist/01.flac".into()],
        );
        let views = gather(&config, &index, None, &state);
        assert!(views["device"].available);
        assert_eq!(views["device"].note, "playlist matches last write");
        let plan = build_plan(&config, &index, views, &state);
        let sink = &plan.sinks["device"];
        assert!(sink.add.is_empty(), "{:?}", sink.add);
        assert!(sink.remove.is_empty(), "{:?}", sink.remove);
        assert!(plan.desired.contains_key(&key));
    }

    #[test]
    fn changed_m3u_removes_a_track() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let music = root.join("MUSIC");
        fs::create_dir_all(&music).unwrap();
        fs::write(music.join("Liked Songs.m3u"), "#EXTM3U\nArtist/01.flac\n").unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let config = stock_config(&root_s);
        let mut index = index_with("Artist/01.flac", "Queen", "Night");
        index.add(Entry {
            rel_path: "Artist/02.flac".into(),
            artist: "Queen".into(),
            title: "Day".into(),
            album: String::new(),
            track_no: 2,
        });
        let (keep, keep_meta) = meta("Queen", "Night");
        let (drop, drop_meta) = meta("Queen", "Day");
        let mut liked = std::collections::BTreeMap::new();
        liked.insert(keep.clone(), keep_meta);
        liked.insert(drop.clone(), drop_meta);
        let mut state = State::default();
        state.liked = liked.clone();
        state.set_snapshot(&config.device_snapshot_key(), liked);
        state.stock_playlist_paths.insert(
            stock_playlist_key(&config.device_id, "internal"),
            vec!["Artist/01.flac".into(), "Artist/02.flac".into()],
        );
        let views = gather(&config, &index, None, &state);
        assert_eq!(views["device"].note, "Liked Songs.m3u");
        let plan = build_plan(&config, &index, views, &state);
        assert!(plan.desired.contains_key(&keep));
        assert!(!plan.desired.contains_key(&drop));
    }

    #[test]
    fn stock_write_is_m3u_and_skips_cinder_import() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("MUSIC")).unwrap();
        fs::write(root.join("MUSIC").join("Liked Songs.m3u8"), "old").unwrap();
        let root_s = root.to_string_lossy().into_owned();
        let config = stock_config(&root_s);
        let mut state = State::default();
        state.path = root.join("state.json");
        let mut plan = Plan::default();
        plan.playlist
            .insert(root_s.clone(), vec!["Artist/01.flac".into()]);
        plan.views.insert(
            "device".into(),
            SourceView {
                name: "device".into(),
                available: true,
                ..SourceView::default()
            },
        );
        apply_plan(&config, &plan, &mut state, None, false);
        let body = fs::read_to_string(root.join("MUSIC").join("Liked Songs.m3u")).unwrap();
        assert!(body.contains("Artist/01.flac"), "{body}");
        assert!(!root.join("MUSIC").join("Liked Songs.m3u8").exists());
        assert!(!root.join(DEVICE_IMPORT_TSV).exists());
        let stored = state
            .stock_playlist_paths
            .get(&stock_playlist_key("wm1a", "internal"))
            .cloned()
            .unwrap_or_default();
        assert_eq!(stored, vec!["Artist/01.flac".to_string()]);
    }

    #[test]
    fn scrobble_log_uploads_listens_and_keeps_skips() {
        let text = "\
#AUDIOSCROBBLER/1.1
#TZ/UTC
Queen\tOpera\tNight\t1\t200\tL\t1700000000
\tOpera\tBlank Artist\t1\t200\tL\t1700000001
Queen\tOpera\t\t1\t200\tL\t1700000002
Queen\tOpera\tZero\t1\t200\tL\t0
Queen\tOpera\tSkipped\t1\t200\tS\t1700000003
";
        assert_eq!(count_uploadable_scrobbles(text), 1);
        let (header, entries) = parse_scrobbler_log(text);
        let uploadable: Vec<_> = entries
            .iter()
            .filter(|entry| is_uploadable_scrobble(entry))
            .collect();
        assert_eq!(uploadable.len(), 1);
        assert_eq!(uploadable[0].track, "Night");
        let remaining: Vec<_> = entries
            .into_iter()
            .filter(|entry| !is_uploadable_scrobble(entry))
            .collect();
        let body = scrobble_log_body(&header, &remaining);
        assert!(body.starts_with("#AUDIOSCROBBLER/1.1\n"), "{body}");
        assert!(body.contains("Skipped"), "{body}");
        assert!(body.contains("\tS\t"), "{body}");
        assert!(!body.contains("Night"), "{body}");
    }
}
