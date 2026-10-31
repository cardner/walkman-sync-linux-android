//! Per-volume skip manifest.

use crate::storage;

pub const MANIFEST_NAME: &str = ".sony-sync-manifest.tsv";
pub const PARTIAL_SUFFIX: &str = ".partial";
const MTIME_TOLERANCE: f64 = 2.0;

#[derive(Clone, Debug)]
pub struct Record {
    pub source_size: u64,
    pub source_mtime: f64,
    pub copy_size: u64,
    pub content_key: String,
}

#[derive(Clone, Debug, Default)]
pub struct Manifest {
    records: std::collections::BTreeMap<String, Record>,
    by_case: std::collections::HashMap<String, String>,
    by_key: std::collections::HashMap<String, String>,
}

impl Manifest {
    pub fn path_for(volume_music: &str) -> String {
        storage::join(volume_music, &[MANIFEST_NAME])
    }

    pub fn load(volume_music: &str) -> Self {
        let mut manifest = Self::default();
        if volume_music.is_empty() {
            return manifest;
        }
        let path = Self::path_for(volume_music);
        let Ok(text) = storage::read_text(&path) else {
            return manifest;
        };
        for line in text.lines().skip(1) {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 4 {
                continue;
            }
            let rel = parts[0].to_string();
            let Ok(src_size) = parts[1].parse() else { continue };
            let Ok(src_mtime) = parts[2].parse() else { continue };
            let Ok(copy_size) = parts[3].parse() else { continue };
            let key = parts.get(4).copied().unwrap_or("").to_string();
            manifest.insert(
                rel,
                Record {
                    source_size: src_size,
                    source_mtime: src_mtime,
                    copy_size,
                    content_key: key,
                },
            );
        }
        manifest
    }

    fn insert(&mut self, rel: String, record: Record) {
        let folded = storage::fold_rel(&rel);
        if !record.content_key.is_empty() {
            self.by_key.insert(record.content_key.clone(), rel.clone());
        }
        self.by_case.insert(folded, rel.clone());
        self.records.insert(rel, record);
    }

    pub fn lookup(&self, rel: &str) -> Option<&Record> {
        let original = self
            .by_case
            .get(&storage::fold_rel(rel))
            .cloned()
            .unwrap_or_else(|| rel.to_string());
        self.records.get(&original)
    }

    pub fn rel_for_key(&self, content_key: &str) -> Option<&str> {
        self.by_key.get(content_key).map(|s| s.as_str())
    }

    pub fn up_to_date(
        &self,
        rel: &str,
        source_size: u64,
        source_mtime: f64,
        dest_size: Option<u64>,
        source_key: Option<&str>,
    ) -> bool {
        let Some(dest_size) = dest_size else {
            return false;
        };
        if let Some(record) = self.lookup(rel) {
            let source_ok = record.source_size == source_size
                && source_mtime <= record.source_mtime + MTIME_TOLERANCE;
            let dest_ok = record.copy_size == dest_size;
            if source_ok && dest_ok {
                return true;
            }
            if let Some(key) = source_key {
                if !record.content_key.is_empty() && record.content_key == key && dest_ok {
                    return true;
                }
            }
        }
        if let Some(key) = source_key {
            if let Some(other) = self.rel_for_key(key) {
                if let Some(held) = self.records.get(other) {
                    return held.copy_size == dest_size && held.content_key == key;
                }
            }
        }
        false
    }

    pub fn remember(
        &mut self,
        rel: &str,
        source_size: u64,
        source_mtime: f64,
        copy_size: u64,
        content_key: &str,
    ) {
        let prev_key = self.lookup(rel).map(|p| p.content_key.clone()).unwrap_or_default();
        if !prev_key.is_empty() {
            self.by_key.remove(&prev_key);
        }
        let original = self
            .by_case
            .get(&storage::fold_rel(rel))
            .cloned()
            .unwrap_or_else(|| rel.to_string());
        if original != rel {
            self.records.remove(&original);
        }
        self.insert(
            rel.to_string(),
            Record {
                source_size,
                source_mtime,
                copy_size,
                content_key: content_key.to_string(),
            },
        );
    }

    pub fn forget(&mut self, rel: &str) {
        let original = self
            .by_case
            .remove(&storage::fold_rel(rel))
            .unwrap_or_else(|| rel.to_string());
        if let Some(record) = self.records.remove(&original) {
            if !record.content_key.is_empty() {
                self.by_key.remove(&record.content_key);
            }
        }
    }

    pub fn save(&self, volume_music: &str) -> std::io::Result<()> {
        if volume_music.is_empty() {
            return Ok(());
        }
        let path = Self::path_for(volume_music);
        let mut lines = vec![
            "# sony-sync manifest\tsource_size\tsource_mtime\tcopy_size\tcontent_key".to_string(),
        ];
        for (rel, record) in &self.records {
            lines.push(format!(
                "{rel}\t{}\t{:.6}\t{}\t{}",
                record.source_size, record.source_mtime, record.copy_size, record.content_key
            ));
        }
        storage::write_text(&path, &(lines.join("\n") + "\n"))
    }
}
