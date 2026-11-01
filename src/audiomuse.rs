//! AudioMuse mixes + Channels scores for Cinder.

use serde_json::Value;

use crate::storage;
use crate::sync::AppConfig;
use crate::walkman;

pub const PLAYLIST_IMPORT: &str = "cinder_playlist_import.tsv";
pub const SCORE_IMPORT: &str = "cinder_audiomuse_import.tsv";
const AUDIOMUSE_ID_PREFIX: &str = "am:";
const MAX_TRACKS: usize = 4096;
const ENERGY_MIN: f64 = 0.01;
const ENERGY_MAX: f64 = 0.15;
const MOOD_FLOOR: f64 = 0.05;
const PAGE_LIMIT: u32 = 500;
const USER_AGENT: &str = "sony-sync/audiomuse (+walkman)";

#[derive(Clone, Debug, Default)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub album: String,
}

#[derive(Clone, Debug, Default)]
pub struct Playlist {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub tracks: Vec<Track>,
}

#[derive(Clone, Debug, Default)]
pub struct Score {
    pub id: String,
    pub fp: String,
    pub artist: String,
    pub title: String,
    pub album: String,
    pub valence: f64,
    pub arousal: f64,
    pub bpm: f64,
    pub dance: f64,
    pub moods: String,
}

#[derive(Clone, Debug, Default)]
pub struct PullResult {
    pub skipped: String,
    pub error: String,
    pub mixes: Vec<Playlist>,
    pub scores: Vec<Score>,
    pub playlist_body: String,
    pub score_body: String,
    pub playlist_path: String,
    pub score_path: String,
    pub written: bool,
    pub logs: Vec<String>,
}

impl PullResult {
    pub fn log(&mut self, m: impl Into<String>) {
        self.logs.push(m.into());
    }
}

pub fn configured(config: &AppConfig) -> bool {
    !config.audiomuse_url.trim().is_empty()
}

fn normalize_base(url: &str) -> String {
    let text = url.trim().trim_end_matches('/');
    if text.is_empty() {
        return String::new();
    }
    if text.starts_with("http://") || text.starts_with("https://") {
        text.to_string()
    } else {
        format!("http://{text}")
    }
}

fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify(name: &str) -> (String, String) {
    let text = clean(name);
    if text == "Sonic Fingerprint by AudioMuse-AI" {
        return ("fingerprint".into(), text);
    }
    for (suffix, kind) in [
        ("_automatic", "automatic"),
        ("_instant", "instant"),
        ("_radio", "radio"),
    ] {
        if let Some(rest) = text.strip_suffix(suffix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return (kind.into(), rest.to_string());
            }
        }
    }
    (
        "playlist".into(),
        if text.is_empty() {
            "Playlist".into()
        } else {
            text
        },
    )
}

fn json_str(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> String {
    for key in keys {
        if let Some(Value::String(s)) = obj.get(*key) {
            return s.clone();
        }
    }
    String::new()
}

fn as_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

pub fn playlists_from_json(body: &str) -> Result<Vec<Playlist>, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or("audiomuse playlists: expected an object")?;
    let mut out = Vec::new();
    let mut seen: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for playlists in playlist_groups(obj) {
        append_playlists(playlists, &mut out, &mut seen);
    }
    Ok(out)
}

/// Current AudioMuse groups mixes per server. Older servers return one object of
/// mix name → track array. The `servers` array itself is not a playlist.
fn playlist_groups(
    obj: &serde_json::Map<String, Value>,
) -> Vec<&serde_json::Map<String, Value>> {
    let Some(servers) = obj.get("servers").and_then(|v| v.as_array()) else {
        return vec![obj];
    };
    let groups: Vec<_> = servers
        .iter()
        .filter_map(|server| server.get("playlists").and_then(|v| v.as_object()))
        .collect();
    if groups.is_empty() {
        vec![obj]
    } else {
        groups
    }
}

fn append_playlists(
    playlists: &serde_json::Map<String, Value>,
    out: &mut Vec<Playlist>,
    seen: &mut std::collections::HashMap<String, u32>,
) {
    for (name, tracks) in playlists {
        let Some(arr) = tracks.as_array() else { continue };
        let (kind, display) = classify(name);
        let mut list_id = format!("{AUDIOMUSE_ID_PREFIX}{}", clean(name));
        if list_id == AUDIOMUSE_ID_PREFIX {
            continue;
        }
        let count = seen.entry(list_id.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            list_id = format!("{list_id}#{count}");
        }
        let mut parsed = Vec::new();
        for item in arr {
            let Some(map) = item.as_object() else { continue };
            let artist = clean(&json_str(map, &["author", "artist", "albumartist"]));
            let title = clean(&json_str(map, &["title", "name"]));
            if artist.is_empty() || title.is_empty() {
                continue;
            }
            parsed.push(Track {
                artist,
                title,
                album: clean(&json_str(map, &["album"])),
            });
            if parsed.len() >= MAX_TRACKS {
                break;
            }
        }
        if parsed.is_empty() {
            continue;
        }
        out.push(Playlist {
            id: list_id,
            kind,
            name: display,
            tracks: parsed,
        });
    }
}

fn mood_labels(raw: &str) -> String {
    let mut labels = Vec::new();
    for part in raw.split(',') {
        let Some((name, score)) = part.split_once(':') else { continue };
        let name = name.trim().to_lowercase();
        let Ok(n) = score.trim().parse::<f64>() else { continue };
        if n >= MOOD_FLOOR && !name.is_empty() {
            labels.push(name);
        }
    }
    labels.join(",")
}

fn map_track(track: &serde_json::Map<String, Value>) -> Option<Score> {
    let artist = clean(track.get("artist")?.as_str()?);
    let title = clean(track.get("title")?.as_str()?);
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    let tempo = as_float(track.get("tempo")?)?;
    let energy = as_float(track.get("energy")?)?;
    let features_raw = track
        .get("other_features")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut happy = 0.0;
    let mut sad = 0.0;
    let mut dance = 0.0;
    for part in features_raw.split(',') {
        let Some((name, score)) = part.split_once(':') else { continue };
        let Ok(n) = score.trim().parse::<f64>() else { continue };
        match name.trim().to_lowercase().as_str() {
            "happy" => happy = n,
            "sad" => sad = n,
            "danceable" => dance = n,
            _ => {}
        }
    }
    let arousal = (energy - ENERGY_MIN) / (ENERGY_MAX - ENERGY_MIN);
    let valence = (happy - sad + 1.0) / 2.0;
    Some(Score {
        id: track.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
        fp: track.get("fp").and_then(|v| v.as_str()).unwrap_or("").into(),
        artist,
        title,
        album: clean(track.get("album").and_then(|v| v.as_str()).unwrap_or("")),
        valence: valence.clamp(0.0, 1.0),
        arousal: arousal.clamp(0.0, 1.0),
        bpm: tempo,
        dance: dance.clamp(0.0, 1.0),
        moods: mood_labels(track.get("mood_vector").and_then(|v| v.as_str()).unwrap_or("")),
    })
}

pub fn scores_from_json(body: &str) -> Result<(Vec<Score>, bool), String> {
    let value: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let tracks = value
        .get("tracks")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows = Vec::new();
    for item in tracks {
        if let Some(map) = item.as_object() {
            if let Some(score) = map_track(map) {
                rows.push(score);
            }
        }
    }
    let has_more = value.get("has_more").and_then(|v| v.as_bool()).unwrap_or(false);
    Ok((rows, has_more))
}

fn render_playlists(lists: &[Playlist]) -> String {
    let mut lines = vec![
        "# navidrome playlists — sony-sync".to_string(),
        "# playlist\tid\tkind\tname".into(),
        "# track\tid\tartist\ttitle\talbum".into(),
    ];
    for item in lists {
        lines.push(format!(
            "playlist\t{}\t{}\t{}",
            clean(&item.id),
            item.kind,
            clean(&item.name)
        ));
        for track in item.tracks.iter().take(MAX_TRACKS) {
            lines.push(format!(
                "track\t{}\t{}\t{}\t{}",
                clean(&item.id),
                track.artist,
                track.title,
                track.album
            ));
        }
    }
    lines.join("\n") + "\n"
}

fn render_scores(scores: &[Score]) -> String {
    let mut lines = vec!["# audiomuse scores — sony-sync".to_string()];
    for s in scores {
        lines.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{:.1}\t{:.4}\t{}",
            s.id, s.fp, s.artist, s.title, s.album, s.valence, s.arousal, s.bpm, s.dance, s.moods
        ));
    }
    lines.join("\n") + "\n"
}

fn fetch(url: &str, token: &str) -> Result<String, String> {
    let mut req = ureq::get(url).set("User-Agent", USER_AGENT);
    if !token.is_empty() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    req.call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

pub fn android_blocks_audiomuse(config: &AppConfig) -> bool {
    config.is_android()
}

pub fn prepare(config: &AppConfig, progress: &mut dyn FnMut(&str)) -> PullResult {
    let mut result = PullResult::default();
    if android_blocks_audiomuse(config) {
        result.skipped = "AudioMuse skipped (android-walkman)".into();
        return result;
    }
    if !configured(config) {
        result.skipped = "AudioMuse not configured".into();
        return result;
    }
    let dest = config.internal_drive();
    if dest.is_empty() || !storage::isdir(&dest) {
        result.error = "Walkman internal volume is not connected".into();
        return result;
    }
    let root = walkman::volume_root(&dest);
    result.playlist_path = storage::join(&root, &[PLAYLIST_IMPORT]);
    result.score_path = storage::join(&root, &[SCORE_IMPORT]);
    let base = normalize_base(&config.audiomuse_url);
    progress("Fetching AudioMuse playlists...");
    match fetch(&format!("{base}/api/playlists"), &config.audiomuse_token) {
        Ok(body) => match playlists_from_json(&body) {
            Ok(mixes) => {
                result.log(format!("{} mix(es)", mixes.len()));
                result.mixes = mixes;
            }
            Err(err) => result.error = err,
        },
        Err(err) => result.error = err,
    }
    if !result.error.is_empty() {
        return result;
    }
    progress("Fetching AudioMuse scores...");
    let mut page = 1u32;
    loop {
        match fetch(
            &format!("{base}/api/sync?include_embeddings=false&page={page}&limit={PAGE_LIMIT}"),
            &config.audiomuse_token,
        ) {
            Ok(body) => match scores_from_json(&body) {
                Ok((rows, more)) => {
                    result.scores.extend(rows);
                    if !more {
                        break;
                    }
                    page += 1;
                }
                Err(err) => {
                    result.error = err;
                    break;
                }
            },
            Err(err) => {
                result.error = err;
                break;
            }
        }
        if page > 40 {
            break;
        }
    }
    result.playlist_body = render_playlists(&result.mixes);
    result.score_body = render_scores(&result.scores);
    result.log(format!("{} score(s)", result.scores.len()));
    result
}

pub fn commit(preview: &PullResult, dry_run: bool) -> PullResult {
    let mut result = preview.clone();
    if !result.error.is_empty() || !result.skipped.is_empty() {
        return result;
    }
    if dry_run {
        result.log("dry run — nothing written");
        return result;
    }
    if !result.playlist_path.is_empty() {
        if let Err(err) = storage::write_text(&result.playlist_path, &result.playlist_body) {
            result.error = err.to_string();
            return result;
        }
        result.log(format!("wrote {}", result.playlist_path));
    }
    if !result.score_path.is_empty() {
        if let Err(err) = storage::write_text(&result.score_path, &result.score_body) {
            result.error = err.to_string();
            return result;
        }
        result.log(format!("wrote {}", result.score_path));
    }
    result.written = true;
    result
}

pub fn preview_rows(result: &PullResult) -> Vec<String> {
    let mut rows = result.logs.clone();
    if !result.skipped.is_empty() {
        rows.insert(0, result.skipped.clone());
    }
    if !result.error.is_empty() {
        rows.insert(0, result.error.clone());
    }
    rows.push(format!("{} mix(es), {} score(s)", result.mixes.len(), result.scores.len()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_servers_become_separate_mixes() {
        let body = r#"{
          "multi_server": true,
          "servers": [{
            "server_name": "Default",
            "is_default": true,
            "playlists": {
              "Late_automatic": [
                {"item_id": "1", "title": "Night", "author": "Queen", "album": "Opera"}
              ],
              "Bus_instant": [
                {"item_id": "2", "title": "One", "artist": "U2"}
              ],
              "Sonic Fingerprint by AudioMuse-AI": [
                {"item_id": "3", "title": "Stay", "author": "Rihanna", "album": "Talk"}
              ]
            }
          }]
        }"#;
        let lists = playlists_from_json(body).unwrap();
        assert_eq!(lists.len(), 3, "{lists:?}");
        assert!(lists.iter().all(|item| item.id.starts_with("am:")));
        assert_eq!(
            lists.iter().find(|item| item.kind == "automatic").unwrap().name,
            "Late"
        );
        assert_eq!(
            lists.iter().find(|item| item.kind == "instant").unwrap().name,
            "Bus"
        );
        let rendered = render_playlists(&lists);
        let playlist_rows = rendered
            .lines()
            .filter(|line| line.starts_with("playlist\t"))
            .count();
        assert_eq!(playlist_rows, 3);
        assert!(rendered.contains("track\tam:Late_automatic\tQueen\tNight\tOpera"));
        assert!(rendered.contains("track\tam:Bus_instant\tU2\tOne\t"));
    }

    #[test]
    fn flat_playlist_map_still_parses() {
        let body = r#"{
          "Late_automatic": [
            {"title": "Night", "author": "Queen", "album": "Opera"},
            {"title": "", "author": "Queen"}
          ],
          "Sonic Fingerprint by AudioMuse-AI": [
            {"title": "One", "artist": "U2"}
          ]
        }"#;
        let lists = playlists_from_json(body).unwrap();
        assert_eq!(lists.len(), 2);
        let late = lists.iter().find(|item| item.name == "Late").unwrap();
        assert_eq!(late.tracks.len(), 1);
        assert_eq!(late.id, "am:Late_automatic");
    }
}
