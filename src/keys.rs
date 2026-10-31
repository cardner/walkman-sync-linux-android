//! FLAC STREAMINFO + cheap MP3 header identity, and tags from the same open.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: i32,
}

impl Tags {
    pub fn best_artist(&self) -> &str {
        if !self.artist.is_empty() {
            &self.artist
        } else {
            &self.album_artist
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Enrichment {
    pub key: String,
    pub tags: Tags,
}

pub fn flac_header_key(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    flac_key(&mut file, false).map(|(key, _)| key)
}

pub fn content_key(path: &Path) -> Option<String> {
    enrich(path).map(|e| e.key).filter(|k| !k.is_empty())
}

pub fn enrich(path: &Path) -> Option<Enrichment> {
    let mut file = File::open(path).ok()?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    if &magic == b"fLaC" {
        let (key, tags) = flac_key(&mut file, true)?;
        return Some(Enrichment { key, tags });
    }
    if magic.starts_with(b"ID3") || magic[0] == 0xff && (magic[1] & 0xe0) == 0xe0 {
        return mp3_enrich(&mut file);
    }
    None
}

fn flac_key(file: &mut File, allow_raw: bool) -> Option<(String, Tags)> {
    let mut hdr = [0u8; 4];
    file.read_exact(&mut hdr).ok()?;
    if &hdr != b"fLaC" {
        return None;
    }
    let mut tags = Tags::default();
    let mut md5 = [0u8; 16];
    let mut samples: u64 = 0;
    let mut audio_offset: u64 = 4;
    loop {
        let mut block = [0u8; 4];
        if file.read_exact(&mut block).is_err() {
            return None;
        }
        let last = block[0] & 0x80 != 0;
        let kind = block[0] & 0x7f;
        let len = u32::from_be_bytes([0, block[1], block[2], block[3]]) as usize;
        audio_offset += 4 + len as u64;
        if kind == 0 || kind == 4 {
            let mut body = vec![0u8; len];
            file.read_exact(&mut body).ok()?;
            if kind == 0 && body.len() >= 34 {
                let bits = u64::from_be_bytes(body[10..18].try_into().ok()?);
                samples = bits & ((1u64 << 36) - 1);
                md5.copy_from_slice(&body[18..34]);
            } else if kind == 4 {
                apply_vorbis(&mut tags, &body);
            }
        } else {
            file.seek(SeekFrom::Current(len as i64)).ok()?;
        }
        if last {
            break;
        }
    }
    if md5.iter().any(|&b| b != 0) {
        return Some((format!("flac-{}-{samples}", hex::encode(md5)), tags));
    }
    if !allow_raw {
        return None;
    }
    let remaining = remaining(file, audio_offset)?;
    let digest = hash_from(file, audio_offset, remaining)?;
    Some((format!("flacraw-{digest}-{remaining}"), tags))
}

fn apply_vorbis(tags: &mut Tags, body: &[u8]) {
    if body.len() < 4 {
        return;
    }
    let vendor_len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let mut offset = 4 + vendor_len;
    if offset + 4 > body.len() {
        return;
    }
    let count = u32::from_le_bytes(body[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;
    for _ in 0..count {
        if offset + 4 > body.len() {
            return;
        }
        let size = u32::from_le_bytes(body[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + size > body.len() {
            return;
        }
        let chunk = &body[offset..offset + size];
        offset += size;
        let text = String::from_utf8_lossy(chunk);
        let Some((name, value)) = text.split_once('=') else {
            continue;
        };
        set_field(tags, &name.to_ascii_uppercase(), value.trim());
    }
}

fn set_field(tags: &mut Tags, name: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    match name {
        "ARTIST" if tags.artist.is_empty() => tags.artist = value.to_string(),
        "TITLE" if tags.title.is_empty() => tags.title = value.to_string(),
        "ALBUM" if tags.album.is_empty() => tags.album = value.to_string(),
        "ALBUMARTIST" | "ALBUM ARTIST" if tags.album_artist.is_empty() => {
            tags.album_artist = value.to_string()
        }
        "TRACKNUMBER" | "TRACK" if tags.track_no == 0 => {
            let head = value.split('/').next().unwrap_or("").trim();
            tags.track_no = head.parse().unwrap_or(0);
        }
        _ => {}
    }
}

fn mp3_enrich(file: &mut File) -> Option<Enrichment> {
    let mut tags = Tags::default();
    let start = id3v2_len(file)?;
    if start > 0 {
        parse_id3(file, start, &mut tags);
    }
    let total = file.seek(SeekFrom::End(0)).ok()?;
    let mut end = total;
    if total >= start + 128 {
        file.seek(SeekFrom::Start(total - 128)).ok()?;
        let mut tag = [0u8; 3];
        if file.read_exact(&mut tag).is_ok() && &tag == b"TAG" {
            end -= 128;
        }
    }
    let length = end.saturating_sub(start);
    if length == 0 {
        return None;
    }
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut head = [0u8; 4];
    file.read_exact(&mut head).ok()?;
    if head[0] != 0xff || (head[1] & 0xe0) != 0xe0 {
        return Some(Enrichment {
            key: format!("mp3h-0-{length}-unknown"),
            tags,
        });
    }
    let version = (head[1] >> 3) & 0x03;
    let layer = (head[1] >> 1) & 0x03;
    let bitrate_idx = (head[2] >> 4) & 0x0f;
    let sr_idx = (head[2] >> 2) & 0x03;
    let channels = (head[3] >> 6) & 0x03;
    let (frames, bytes) = xing_or_length(file, start, length, version, channels);
    let key = format!("mp3h-{frames}-{bytes}-{version}-{layer}-{sr_idx}-{channels}-{bitrate_idx}");
    Some(Enrichment { key, tags })
}

fn xing_or_length(
    file: &mut File,
    start: u64,
    length: u64,
    version: u8,
    channels: u8,
) -> (u32, u64) {
    let mpeg1 = version == 0b11;
    let mono = channels == 0b11;
    let side = match (mpeg1, mono) {
        (true, false) => 32,
        (true, true) => 17,
        (false, false) => 17,
        (false, true) => 9,
    };
    let xing_at = start + 4 + side;
    if file.seek(SeekFrom::Start(xing_at)).is_err() {
        return (0, length);
    }
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return (0, length);
    }
    if &magic != b"Xing" && &magic != b"Info" {
        // VBRI sits at a fixed offset 32 from the frame start.
        if file.seek(SeekFrom::Start(start + 36)).is_ok() {
            let mut vbri = [0u8; 4];
            if file.read_exact(&mut vbri).is_ok() && &vbri == b"VBRI" {
                let mut skip = [0u8; 6];
                let _ = file.read_exact(&mut skip);
                let mut frames_b = [0u8; 4];
                let mut bytes_b = [0u8; 4];
                if file.read_exact(&mut frames_b).is_ok() && file.read_exact(&mut bytes_b).is_ok() {
                    return (
                        u32::from_be_bytes(frames_b),
                        u32::from_be_bytes(bytes_b) as u64,
                    );
                }
            }
        }
        return (0, length);
    }
    let mut flags_b = [0u8; 4];
    if file.read_exact(&mut flags_b).is_err() {
        return (0, length);
    }
    let flags = u32::from_be_bytes(flags_b);
    let mut frames = 0u32;
    let mut bytes = length;
    if flags & 0x0001 != 0 {
        let mut b = [0u8; 4];
        if file.read_exact(&mut b).is_ok() {
            frames = u32::from_be_bytes(b);
        }
    }
    if flags & 0x0002 != 0 {
        let mut b = [0u8; 4];
        if file.read_exact(&mut b).is_ok() {
            bytes = u32::from_be_bytes(b) as u64;
        }
    }
    (frames, bytes)
}

fn id3v2_len(file: &mut File) -> Option<u64> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut head = [0u8; 10];
    file.read_exact(&mut head).ok()?;
    if &head[0..3] != b"ID3" {
        file.seek(SeekFrom::Start(0)).ok()?;
        return Some(0);
    }
    Some(10 + syncsafe(&head[6..10]) as u64)
}

fn parse_id3(file: &mut File, tag_len: u64, tags: &mut Tags) {
    if tag_len < 10 {
        return;
    }
    if file.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let mut head = [0u8; 10];
    if file.read_exact(&mut head).is_err() {
        return;
    }
    let major = head[3];
    let flags = head[5];
    let size = syncsafe(&head[6..10]) as usize;
    let mut body = vec![0u8; size];
    if file.read_exact(&mut body).is_err() {
        return;
    }
    let mut offset = 0usize;
    if flags & 0x40 != 0 && body.len() >= 4 {
        offset = if major == 4 {
            syncsafe(&body[0..4]) as usize
        } else {
            4 + u32::from_be_bytes(body[0..4].try_into().unwrap_or([0; 4])) as usize
        };
    }
    let (id_len, size_len) = if major == 2 { (3, 3) } else { (4, 4) };
    while offset + id_len + size_len <= body.len() {
        let frame_id = match std::str::from_utf8(&body[offset..offset + id_len]) {
            Ok(s) => s.to_string(),
            Err(_) => break,
        };
        if frame_id.as_bytes().iter().all(|&b| b == 0) {
            break;
        }
        let raw = &body[offset + id_len..offset + id_len + size_len];
        let frame_size = if major == 4 {
            syncsafe(raw) as usize
        } else if size_len == 3 {
            ((raw[0] as usize) << 16) | ((raw[1] as usize) << 8) | raw[2] as usize
        } else {
            u32::from_be_bytes(raw.try_into().unwrap_or([0; 4])) as usize
        };
        offset += id_len + size_len + if major == 2 { 0 } else { 2 };
        if offset + frame_size > body.len() || frame_size == 0 {
            break;
        }
        let payload = &body[offset..offset + frame_size];
        offset += frame_size;
        let field = match frame_id.as_str() {
            "TPE1" | "TP1" => "artist",
            "TIT2" | "TT2" => "title",
            "TALB" | "TAL" => "album",
            "TPE2" => "album_artist",
            "TRCK" | "TRK" => "track_no",
            _ => continue,
        };
        let text = decode_id3_text(payload);
        match field {
            "artist" if tags.artist.is_empty() => tags.artist = text,
            "title" if tags.title.is_empty() => tags.title = text,
            "album" if tags.album.is_empty() => tags.album = text,
            "album_artist" if tags.album_artist.is_empty() => tags.album_artist = text,
            "track_no" if tags.track_no == 0 => {
                let head = text.split('/').next().unwrap_or("").trim();
                tags.track_no = head.parse().unwrap_or(0);
            }
            _ => {}
        }
    }
}

fn decode_id3_text(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let encoding = payload[0];
    let rest = &payload[1..];
    let text = match encoding {
        0 => String::from_utf8_lossy(rest).into_owned(),
        1 => decode_utf16(rest, true),
        2 => decode_utf16(rest, false),
        _ => String::from_utf8_lossy(rest).into_owned(),
    };
    text.split('\0').next().unwrap_or("").trim().to_string()
}

fn decode_utf16(bytes: &[u8], bom: bool) -> String {
    let (be, data) = if bom && bytes.len() >= 2 {
        if bytes[0] == 0xff && bytes[1] == 0xfe {
            (false, &bytes[2..])
        } else if bytes[0] == 0xfe && bytes[1] == 0xff {
            (true, &bytes[2..])
        } else {
            (true, bytes)
        }
    } else {
        (true, bytes)
    };
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| {
            if be {
                u16::from_be_bytes([c[0], c[1]])
            } else {
                u16::from_le_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

fn syncsafe(bytes: &[u8]) -> u32 {
    if bytes.len() < 4 {
        return 0;
    }
    ((bytes[0] as u32 & 0x7f) << 21)
        | ((bytes[1] as u32 & 0x7f) << 14)
        | ((bytes[2] as u32 & 0x7f) << 7)
        | (bytes[3] as u32 & 0x7f)
}

fn remaining(file: &mut File, offset: u64) -> Option<u64> {
    let end = file.seek(SeekFrom::End(0)).ok()?;
    Some(end.saturating_sub(offset))
}

fn hash_from(file: &mut File, offset: u64, length: u64) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2sVar;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut hasher = Blake2sVar::new(8).ok()?;
    let mut left = length;
    let mut buf = [0u8; 65536];
    while left > 0 {
        let n = std::cmp::min(left, buf.len() as u64) as usize;
        let read = file.read(&mut buf[..n]).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        left -= read as u64;
    }
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

pub fn tags_from_filename(rel: &str) -> Tags {
    let name = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .replace('_', " ");
    let parts: Vec<&str> = name.splitn(2, " - ").collect();
    if parts.len() == 2 {
        Tags {
            artist: parts[0].trim().to_string(),
            title: parts[1].trim().to_string(),
            ..Tags::default()
        }
    } else {
        Tags {
            title: name,
            ..Tags::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn flac_with_picture(md5: [u8; 16], picture_len: usize) -> Vec<u8> {
        let packed: u64 = (44100u64 << 44) | (15u64 << 36) | 16;
        let mut streaminfo = Vec::new();
        streaminfo.extend_from_slice(&16u16.to_be_bytes());
        streaminfo.extend_from_slice(&16u16.to_be_bytes());
        streaminfo.extend_from_slice(&[0u8; 6]);
        streaminfo.extend_from_slice(&packed.to_be_bytes());
        streaminfo.extend_from_slice(&md5);
        let mut out = b"fLaC".to_vec();
        out.push(0x00);
        out.extend_from_slice(&(34u32).to_be_bytes()[1..]);
        out.extend_from_slice(&streaminfo);
        out.push(0x86);
        let pic_len = picture_len as u32;
        out.extend_from_slice(&pic_len.to_be_bytes()[1..]);
        out.resize(out.len() + picture_len, 0);
        out
    }

    #[test]
    fn flac_key_skips_picture_block() {
        let md5 = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let bytes = flac_with_picture(md5, 64 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic.flac");
        File::create(&path).unwrap().write_all(&bytes).unwrap();
        let key = flac_header_key(&path).expect("key");
        assert!(key.starts_with("flac-"), "{key}");
        assert!(key.contains(&hex::encode(md5)));
    }
}
