//! Persistent source + dest file catalog.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::keys;
use crate::manifest;
use crate::paths;
use crate::storage;
use crate::walkman;

pub const KIND_SOURCE: &str = "source";
pub const KIND_INTERNAL: &str = "internal";
pub const KIND_SD: &str = "sd";
pub const MTIME_TOLERANCE: f64 = 2.0;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS volumes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    serial TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    last_root TEXT NOT NULL DEFAULT '',
    scanned_at REAL NOT NULL DEFAULT 0,
    UNIQUE (kind, serial)
);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    volume_id INTEGER NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    rel_fold TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    content_key TEXT NOT NULL DEFAULT '',
    artist TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    album TEXT NOT NULL DEFAULT '',
    track_no INTEGER NOT NULL DEFAULT 0,
    suffix TEXT NOT NULL DEFAULT '',
    seen_at REAL NOT NULL DEFAULT 0,
    UNIQUE (volume_id, rel_fold)
);
CREATE INDEX IF NOT EXISTS files_key ON files(content_key) WHERE content_key != '';
CREATE INDEX IF NOT EXISTS files_tags ON files(artist, title) WHERE artist != '' AND title != '';
"#;

#[derive(Clone, Debug, Default)]
pub struct RefreshStats {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub unchanged: u32,
    pub keyed: u32,
}

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub id: i64,
    pub kind: String,
    pub serial: String,
    pub label: String,
    pub last_root: String,
    pub scanned_at: f64,
    pub file_count: i64,
    pub key_count: i64,
}

#[derive(Clone, Debug, Default)]
pub struct FileRow {
    pub rel: String,
    pub size: u64,
    pub mtime: f64,
    pub content_key: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub track_no: i32,
    pub suffix: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RefreshOpts {
    pub walk: bool,
    pub enrich: bool,
}

impl Default for RefreshOpts {
    fn default() -> Self {
        Self {
            walk: true,
            enrich: true,
        }
    }
}

pub struct Catalog {
    pub path: PathBuf,
    db: Connection,
}

impl Catalog {
    pub fn open(path: Option<&Path>) -> rusqlite::Result<Self> {
        let path = path
            .map(|p| p.to_path_buf())
            .unwrap_or_else(paths::catalog_path);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let db = Connection::open(&path)?;
        db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { path, db })
    }

    fn checkpoint(&self) {
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }

    pub fn volume_for_kind(&self, kind: &str) -> Option<VolumeInfo> {
        self.db
            .query_row(
                "SELECT v.*, \
                 (SELECT COUNT(*) FROM files WHERE volume_id = v.id) AS file_count, \
                 (SELECT COUNT(*) FROM files WHERE volume_id = v.id AND content_key != '') AS key_count \
                 FROM volumes v WHERE kind = ? ORDER BY scanned_at DESC LIMIT 1",
                params![kind],
                row_volume,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn volume_for_root(&self, kind: &str, root: &str) -> Option<VolumeInfo> {
        let serial = if kind == KIND_SOURCE {
            walkman::source_serial(root)
        } else {
            storage::serial(root)
        };
        self.db
            .query_row(
                "SELECT v.*, \
                 (SELECT COUNT(*) FROM files WHERE volume_id = v.id) AS file_count, \
                 (SELECT COUNT(*) FROM files WHERE volume_id = v.id AND content_key != '') AS key_count \
                 FROM volumes v WHERE kind = ? AND serial = ?",
                params![kind, serial],
                row_volume,
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn coverage(&self, internal_root: &str, sd_root: &str) -> HashMap<String, Option<VolumeInfo>> {
        let mut map = HashMap::new();
        map.insert(KIND_SOURCE.to_string(), self.volume_for_kind(KIND_SOURCE));
        map.insert(
            KIND_INTERNAL.to_string(),
            if internal_root.is_empty() {
                None
            } else {
                self.volume_for_root(KIND_INTERNAL, internal_root)
            },
        );
        map.insert(
            KIND_SD.to_string(),
            if sd_root.is_empty() {
                None
            } else {
                self.volume_for_root(KIND_SD, sd_root)
            },
        );
        map
    }

    pub fn files_for(&self, kind: &str, root: Option<&str>) -> Vec<FileRow> {
        let info = match root {
            Some(r) => self.volume_for_root(kind, r),
            None => self.volume_for_kind(kind),
        };
        let Some(info) = info else {
            return Vec::new();
        };
        let mut stmt = self
            .db
            .prepare(
                "SELECT rel, size, mtime, content_key, artist, title, album, track_no, suffix \
                 FROM files WHERE volume_id = ?",
            )
            .unwrap();
        stmt.query_map(params![info.id], |row| {
            Ok(FileRow {
                rel: row.get(0)?,
                size: row.get::<_, i64>(1)? as u64,
                mtime: row.get(2)?,
                content_key: row.get(3)?,
                artist: row.get(4)?,
                title: row.get(5)?,
                album: row.get(6)?,
                track_no: row.get(7)?,
                suffix: row.get(8)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    pub fn confirm_live(&self, kind: &str, path: &str) -> (bool, String) {
        if path.is_empty() {
            return (false, "no destination set".into());
        }
        if !storage::isdir(path) {
            return (false, "destination is not connected".into());
        }
        let live = storage::serial(path);
        let info = self.volume_for_root(kind, path);
        match info {
            None => (true, String::new()),
            Some(info) if info.serial.is_empty() => (true, String::new()),
            Some(info) if info.serial != live => (
                false,
                format!(
                    "{kind} path {path} is a different volume (catalog {}, live {live}) — refusing to write",
                    info.serial
                ),
            ),
            Some(_) => (true, String::new()),
        }
    }

    pub fn should_skip_dest_walk(&self, kind: &str, path: &str) -> bool {
        let (ok, _) = self.confirm_live(kind, path);
        if !ok {
            return false;
        }
        self.volume_for_root(kind, path)
            .map(|v| v.file_count > 0)
            .unwrap_or(false)
    }

    fn ensure_volume(&self, kind: &str, root: &str) -> rusqlite::Result<i64> {
        let (serial, label) = if kind == KIND_SOURCE {
            (walkman::source_serial(root), "library".to_string())
        } else {
            (storage::serial(root), storage::label(root))
        };
        if let Some(id) = self
            .db
            .query_row(
                "SELECT id FROM volumes WHERE kind = ? AND serial = ?",
                params![kind, serial],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            self.db.execute(
                "UPDATE volumes SET last_root = ?, label = ? WHERE id = ?",
                params![root, label, id],
            )?;
            return Ok(id);
        }
        self.db.execute(
            "INSERT INTO volumes(kind, serial, label, last_root) VALUES (?, ?, ?, ?)",
            params![kind, serial, label, root],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn refresh_source(
        &mut self,
        source_dir: &str,
        opts: RefreshOpts,
        progress: Option<&mut dyn FnMut(&str, usize, usize)>,
    ) -> RefreshStats {
        self.refresh(KIND_SOURCE, source_dir, false, opts, progress)
    }

    pub fn refresh_dest(
        &mut self,
        music_path: &str,
        kind: &str,
        opts: RefreshOpts,
        progress: Option<&mut dyn FnMut(&str, usize, usize)>,
    ) -> RefreshStats {
        self.refresh(kind, music_path, true, opts, progress)
    }

    fn refresh(
        &mut self,
        kind: &str,
        root: &str,
        dest: bool,
        opts: RefreshOpts,
        mut progress: Option<&mut dyn FnMut(&str, usize, usize)>,
    ) -> RefreshStats {
        let mut stats = RefreshStats::default();
        if root.is_empty() {
            return stats;
        }
        if let Some(cb) = progress.as_mut() {
            cb("opening…", 0, 1);
        }
        if !storage::isdir(root) {
            return stats;
        }
        let Ok(volume_id) = self.ensure_volume(kind, root) else {
            return stats;
        };
        if !opts.walk {
            if opts.enrich {
                self.enrich_missing(volume_id, root, dest, &mut stats, progress);
            }
            return stats;
        }
        let stamp = now();
        let live_list = storage::walk_files(root, dest);
        let mut live_by_fold: HashMap<String, (String, u64, f64)> = HashMap::new();
        for (rel, size, mtime) in live_list {
            live_by_fold
                .entry(storage::fold_rel(&rel))
                .or_insert((rel, size, mtime));
        }
        let live: Vec<(String, u64, f64)> = live_by_fold.into_values().collect();
        let remote = storage::is_remote(root);
        let mut existing: HashMap<String, Existing> = HashMap::new();
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT id, rel_fold, size, mtime, content_key, artist, title, album, track_no \
             FROM files WHERE volume_id = ?",
        ) {
            if let Ok(rows) = stmt.query_map(params![volume_id], |row| {
                Ok(Existing {
                    id: row.get(0)?,
                    rel_fold: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    mtime: row.get(3)?,
                    content_key: row.get(4)?,
                    artist: row.get(5)?,
                    title: row.get(6)?,
                    album: row.get(7)?,
                    track_no: row.get(8)?,
                })
            }) {
                for row in rows.flatten() {
                    existing.insert(row.rel_fold.clone(), row);
                }
            }
        }
        let dest_manifest = if dest {
            Some(manifest::Manifest::load(root))
        } else {
            None
        };
        let total = live.len().max(1);
        let mut unchanged_ids: Vec<(f64, String, i64)> = Vec::new();
        let mut to_enrich: Vec<EnrichJob> = Vec::new();
        for (index, (rel, size, mtime)) in live.iter().enumerate() {
            if let Some(cb) = progress.as_mut() {
                if index == 0 || (index + 1) % 64 == 0 || index + 1 == live.len() {
                    let folder = Path::new(rel)
                        .components()
                        .next()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .unwrap_or_else(|| rel.clone());
                    cb(&folder, index + 1, total);
                }
            }
            let folded = storage::fold_rel(rel);
            let suffix = Path::new(rel)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy().to_ascii_lowercase()))
                .unwrap_or_default();
            let prev = existing.remove(&folded);
            let unchanged = prev.as_ref().is_some_and(|p| {
                p.size == *size && (remote || (p.mtime - mtime).abs() <= MTIME_TOLERANCE)
            });
            if unchanged {
                if let Some(p) = prev {
                    unchanged_ids.push((stamp, rel.clone(), p.id));
                    stats.unchanged += 1;
                }
                continue;
            }
            to_enrich.push(EnrichJob {
                prev,
                rel: rel.clone(),
                folded,
                size: *size,
                mtime: *mtime,
                suffix,
            });
        }
        if !unchanged_ids.is_empty() {
            let tx = self.db.transaction().ok();
            if let Some(tx) = tx {
                for (stamp, rel, id) in &unchanged_ids {
                    let _ = tx.execute(
                        "UPDATE files SET seen_at = ?, rel = ? WHERE id = ?",
                        params![stamp, rel, id],
                    );
                }
                let _ = tx.commit();
            }
        }

        let remote_src = storage::is_remote(root);
        let use_rayon = opts.enrich && !dest && !remote_src && to_enrich.len() > 4;
        let enriched: Vec<(EnrichJob, String, keys::Tags)> = if use_rayon {
            use rayon::prelude::*;
            to_enrich
                .into_par_iter()
                .map(|job| {
                    let (key, tags) = enrich_one(root, dest, &job, dest_manifest.as_ref());
                    (job, key, tags)
                })
                .collect()
        } else {
            to_enrich
                .into_iter()
                .map(|job| {
                    let (key, tags) = if opts.enrich {
                        enrich_one(root, dest, &job, dest_manifest.as_ref())
                    } else {
                        let key = job
                            .prev
                            .as_ref()
                            .map(|p| p.content_key.clone())
                            .unwrap_or_default();
                        let tags = keys::Tags {
                            artist: job.prev.as_ref().map(|p| p.artist.clone()).unwrap_or_default(),
                            title: job.prev.as_ref().map(|p| p.title.clone()).unwrap_or_default(),
                            album: job.prev.as_ref().map(|p| p.album.clone()).unwrap_or_default(),
                            track_no: job.prev.as_ref().map(|p| p.track_no).unwrap_or(0),
                            album_artist: String::new(),
                        };
                        (key, tags)
                    };
                    (job, key, tags)
                })
                .collect()
        };

        if let Ok(tx) = self.db.transaction() {
            for (job, key, tags) in enriched {
                if !key.is_empty() {
                    stats.keyed += 1;
                }
                if job.prev.is_none() {
                    let _ = tx.execute(
                        "INSERT INTO files(volume_id, rel, rel_fold, size, mtime, content_key, \
                         artist, title, album, track_no, suffix, seen_at) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT(volume_id, rel_fold) DO UPDATE SET \
                         rel = excluded.rel, size = excluded.size, mtime = excluded.mtime, \
                         content_key = excluded.content_key, artist = excluded.artist, \
                         title = excluded.title, album = excluded.album, \
                         track_no = excluded.track_no, suffix = excluded.suffix, \
                         seen_at = excluded.seen_at",
                        params![
                            volume_id,
                            job.rel,
                            job.folded,
                            job.size as i64,
                            job.mtime,
                            key,
                            tags.best_artist(),
                            tags.title,
                            tags.album,
                            tags.track_no,
                            job.suffix,
                            stamp
                        ],
                    );
                    stats.added += 1;
                } else {
                    let _ = tx.execute(
                        "UPDATE files SET rel = ?, size = ?, mtime = ?, content_key = ?, \
                         artist = ?, title = ?, album = ?, track_no = ?, suffix = ?, seen_at = ? \
                         WHERE id = ?",
                        params![
                            job.rel,
                            job.size as i64,
                            job.mtime,
                            key,
                            tags.best_artist(),
                            tags.title,
                            tags.album,
                            tags.track_no,
                            job.suffix,
                            stamp,
                            job.prev.as_ref().unwrap().id
                        ],
                    );
                    stats.updated += 1;
                }
            }
            let leftover: Vec<i64> = existing.values().map(|e| e.id).collect();
            for id in leftover {
                let _ = tx.execute("DELETE FROM files WHERE id = ?", params![id]);
                stats.removed += 1;
            }
            let _ = tx.execute(
                "UPDATE volumes SET scanned_at = ? WHERE id = ?",
                params![stamp, volume_id],
            );
            let _ = tx.commit();
        }
        self.checkpoint();
        stats
    }

    fn enrich_missing(
        &mut self,
        volume_id: i64,
        root: &str,
        dest: bool,
        stats: &mut RefreshStats,
        mut progress: Option<&mut dyn FnMut(&str, usize, usize)>,
    ) {
        let dest_manifest = if dest {
            Some(manifest::Manifest::load(root))
        } else {
            None
        };
        let mut jobs = Vec::new();
        if let Ok(mut stmt) = self.db.prepare(
            "SELECT id, rel, rel_fold, size, mtime, content_key, artist, title, album, track_no, suffix \
             FROM files WHERE volume_id = ? AND (content_key = '' OR (artist = '' AND title = ''))",
        ) {
            if let Ok(rows) = stmt.query_map(params![volume_id], |row| {
                Ok(EnrichJob {
                    prev: Some(Existing {
                        id: row.get(0)?,
                        rel_fold: row.get(2)?,
                        size: row.get::<_, i64>(3)? as u64,
                        mtime: row.get(4)?,
                        content_key: row.get(5)?,
                        artist: row.get(6)?,
                        title: row.get(7)?,
                        album: row.get(8)?,
                        track_no: row.get(9)?,
                    }),
                    rel: row.get(1)?,
                    folded: row.get(2)?,
                    size: row.get::<_, i64>(3)? as u64,
                    mtime: row.get(4)?,
                    suffix: row.get(10)?,
                })
            }) {
                jobs.extend(rows.flatten());
            }
        }
        let total = jobs.len().max(1);
        let remote = storage::is_remote(root);
        let use_rayon = !dest && !remote && jobs.len() > 4;
        let enriched: Vec<(EnrichJob, String, keys::Tags)> = if use_rayon {
            use rayon::prelude::*;
            jobs.into_par_iter()
                .map(|job| {
                    let (key, tags) = enrich_one(root, dest, &job, dest_manifest.as_ref());
                    (job, key, tags)
                })
                .collect()
        } else {
            jobs.into_iter()
                .enumerate()
                .map(|(index, job)| {
                    if let Some(cb) = progress.as_mut() {
                        if index == 0 || (index + 1) % 64 == 0 {
                            cb(&job.rel, index + 1, total);
                        }
                    }
                    let (key, tags) = enrich_one(root, dest, &job, dest_manifest.as_ref());
                    (job, key, tags)
                })
                .collect()
        };
        {
            if let Ok(tx) = self.db.transaction() {
                for (job, key, tags) in enriched {
                    if key.is_empty() {
                        continue;
                    }
                    stats.keyed += 1;
                    let _ = tx.execute(
                        "UPDATE files SET content_key = ?, artist = ?, title = ?, album = ?, track_no = ? \
                         WHERE id = ?",
                        params![
                            key,
                            tags.best_artist(),
                            tags.title,
                            tags.album,
                            tags.track_no,
                            job.prev.as_ref().map(|p| p.id).unwrap_or(0)
                        ],
                    );
                }
                let _ = tx.commit();
            }
        }
        self.checkpoint();
    }

    pub fn upsert_dest_file(
        &self,
        kind: &str,
        root: &str,
        rel: &str,
        size: u64,
        mtime: f64,
        content_key: &str,
    ) {
        let Ok(volume_id) = self.ensure_volume(kind, root) else {
            return;
        };
        let folded = storage::fold_rel(rel);
        let suffix = Path::new(rel)
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy().to_ascii_lowercase()))
            .unwrap_or_default();
        let _ = self.db.execute(
            "INSERT INTO files(volume_id, rel, rel_fold, size, mtime, content_key, suffix, seen_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(volume_id, rel_fold) DO UPDATE SET \
             rel = excluded.rel, size = excluded.size, mtime = excluded.mtime, \
             content_key = CASE WHEN excluded.content_key != '' THEN excluded.content_key \
             ELSE files.content_key END, suffix = excluded.suffix, seen_at = excluded.seen_at",
            params![
                volume_id,
                rel,
                folded,
                size as i64,
                mtime,
                content_key,
                suffix,
                now()
            ],
        );
    }

    pub fn forget_dest_file(&self, kind: &str, root: &str, rel: &str) {
        let Some(info) = self.volume_for_root(kind, root) else {
            return;
        };
        let _ = self.db.execute(
            "DELETE FROM files WHERE volume_id = ? AND rel_fold = ?",
            params![info.id, storage::fold_rel(rel)],
        );
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.checkpoint();
    }
}

struct Existing {
    id: i64,
    rel_fold: String,
    size: u64,
    mtime: f64,
    content_key: String,
    artist: String,
    title: String,
    album: String,
    track_no: i32,
}

struct EnrichJob {
    prev: Option<Existing>,
    rel: String,
    folded: String,
    size: u64,
    mtime: f64,
    suffix: String,
}

fn enrich_one(
    root: &str,
    dest: bool,
    job: &EnrichJob,
    dest_manifest: Option<&manifest::Manifest>,
) -> (String, keys::Tags) {
    let mut key = job
        .prev
        .as_ref()
        .map(|p| p.content_key.clone())
        .unwrap_or_default();
    let mut tags = keys::Tags {
        artist: job.prev.as_ref().map(|p| p.artist.clone()).unwrap_or_default(),
        title: job.prev.as_ref().map(|p| p.title.clone()).unwrap_or_default(),
        album: job.prev.as_ref().map(|p| p.album.clone()).unwrap_or_default(),
        track_no: job.prev.as_ref().map(|p| p.track_no).unwrap_or(0),
        album_artist: String::new(),
    };
    let abs = storage::join(root, &[&job.rel]);
    if dest {
        let name = Path::new(&job.rel)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if name.starts_with('.') && name.ends_with(manifest::PARTIAL_SUFFIX) {
            key.clear();
        } else if let Some(record) = dest_manifest.and_then(|m| m.lookup(&job.rel)) {
            if !record.content_key.is_empty() {
                key = record.content_key.clone();
            }
        } else if job.suffix == ".flac" && key.is_empty() && !storage::is_remote(&abs) {
            if let Some(local) = storage::local_file(&abs) {
                if let Some(k) = keys::flac_header_key(&local) {
                    key = k;
                }
            }
        }
    } else {
        let audio = storage::AUDIO_EXT.iter().any(|e| job.suffix == *e);
        if job.suffix == ".flac" || job.suffix == ".mp3" {
            if let Some(local) = storage::local_file(&abs) {
                if let Some(e) = keys::enrich(&local) {
                    if !e.key.is_empty() {
                        key = e.key;
                    }
                    if !e.tags.best_artist().is_empty() || !e.tags.title.is_empty() {
                        tags = e.tags;
                    }
                }
            }
        } else if audio {
            tags = keys::tags_from_filename(&job.rel);
        }
    }
    (key, tags)
}

fn row_volume(row: &rusqlite::Row<'_>) -> rusqlite::Result<VolumeInfo> {
    Ok(VolumeInfo {
        id: row.get("id")?,
        kind: row.get("kind")?,
        serial: row.get("serial")?,
        label: row.get("label")?,
        last_root: row.get("last_root")?,
        scanned_at: row.get("scanned_at")?,
        file_count: row.get("file_count")?,
        key_count: row.get("key_count")?,
    })
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn format_age(scanned_at: f64) -> String {
    if scanned_at == 0.0 {
        return "never".into();
    }
    let elapsed = now() - scanned_at;
    if elapsed < 60.0 {
        "just now".into()
    } else if elapsed < 3600.0 {
        format!("{}m ago", (elapsed / 60.0) as i64)
    } else if elapsed < 86400.0 {
        format!("{}h ago", (elapsed / 3600.0) as i64)
    } else {
        format!("{}d ago", (elapsed / 86400.0) as i64)
    }
}

#[allow(dead_code)]
fn import_json_keys(_path: &Path) -> HashMap<String, Value> {
    HashMap::new()
}
