//! Album art from sidecars or embedded tags, rasterized for the progress TUI.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{DynamicImage, RgbaImage};

use crate::storage;

const DECODE_MAX: u32 = 512;
const EMBEDDED_ART_MAX: usize = 2 * 1024 * 1024;

const HALF_BLOCK: char = '▀';
const MATTE: (u8, u8, u8) = (16, 16, 16);
const BAYER4: [[i16; 4]; 4] = [
    [0, 8, 2, 10],
    [12, 4, 14, 6],
    [3, 11, 1, 9],
    [15, 7, 13, 5],
];

const SIDECAR_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "album.jpg",
    "album.jpeg",
    "album.png",
    "albumart.jpg",
    "albumart.jpeg",
    "albumart.png",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtCell {
    pub bg: (u8, u8, u8),
    pub fg: (u8, u8, u8),
    pub ch: char,
}

#[derive(Clone, Debug, Default)]
pub struct ArtRaster {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<ArtCell>,
}

impl ArtRaster {
    pub fn cell(&self, x: u16, y: u16) -> Option<&ArtCell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells.get((y as usize) * (self.width as usize) + x as usize)
    }
}

/// Largest cell grid that fits in `width`×`height` and stays visually square.
/// Terminal cells are ~1:2, and each cell is two stacked pixels (`▀`), so a
/// square cover uses `cols == 2 * rows`.
pub fn square_cells(width: u16, height: u16) -> (u16, u16) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let mut rows = height;
    let mut cols = rows.saturating_mul(2);
    if cols > width {
        cols = width;
        rows = (cols / 2).max(1);
    }
    (cols.max(1), rows.max(1))
}

fn downscale_cover(img: DynamicImage) -> RgbaImage {
    let w = img.width();
    let h = img.height();
    if w.max(h) > DECODE_MAX {
        img.resize(DECODE_MAX, DECODE_MAX, FilterType::Triangle)
            .to_rgba8()
    } else {
        img.to_rgba8()
    }
}

pub fn load_album_art(path: &Path) -> Option<RgbaImage> {
    if let Some(dir) = path.parent() {
        if let Some(sidecar) = find_sidecar(dir) {
            if let Ok(img) = image::open(&sidecar) {
                return Some(downscale_cover(img));
            }
        }
    }
    if let Some(bytes) = embedded_picture(path) {
        if let Ok(img) = image::load_from_memory(&bytes) {
            return Some(downscale_cover(img));
        }
    }
    None
}

pub fn find_sidecar(dir: &Path) -> Option<PathBuf> {
    let read = fs::read_dir(dir).ok()?;
    let mut best: Option<(usize, PathBuf)> = None;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        let Some(idx) = SIDECAR_NAMES.iter().position(|n| *n == name) else {
            continue;
        };
        if idx == 0 {
            return Some(entry.path());
        }
        if best.as_ref().map(|(i, _)| idx < *i).unwrap_or(true) {
            best = Some((idx, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

pub fn rasterize(img: &RgbaImage, width: u16, height: u16) -> ArtRaster {
    let (cols, rows) = square_cells(width, height);
    if cols == 0 || rows == 0 {
        return ArtRaster {
            width: cols,
            height: rows,
            cells: Vec::new(),
        };
    }
    let w = img.width();
    let h = img.height();
    let side = w.min(h).max(1);
    let x = (w - side) / 2;
    let y = (h - side) / 2;
    let cropped = image::imageops::crop_imm(img, x, y, side, side);
    let px_w = cols as u32;
    let px_h = rows as u32 * 2;
    let pixels = image::imageops::resize(cropped.inner(), px_w, px_h, FilterType::Lanczos3);
    let mut cells = Vec::with_capacity(cols as usize * rows as usize);
    for y in 0..rows {
        for x in 0..cols {
            let top = pixels.get_pixel(x as u32, y as u32 * 2).0;
            let bot = pixels.get_pixel(x as u32, y as u32 * 2 + 1).0;
            let fg = quantize_565(composite(top), x, y as u32 * 2);
            let bg = quantize_565(composite(bot), x, y as u32 * 2 + 1);
            cells.push(ArtCell {
                fg,
                bg,
                ch: HALF_BLOCK,
            });
        }
    }
    ArtRaster {
        width: cols,
        height: rows,
        cells,
    }
}

fn composite(p: [u8; 4]) -> (u8, u8, u8) {
    if p[3] >= 250 {
        return (p[0], p[1], p[2]);
    }
    let a = p[3] as u16;
    let inv = 255 - a;
    (
        ((p[0] as u16 * a + MATTE.0 as u16 * inv) / 255) as u8,
        ((p[1] as u16 * a + MATTE.1 as u16 * inv) / 255) as u8,
        ((p[2] as u16 * a + MATTE.2 as u16 * inv) / 255) as u8,
    )
}

/// 16-bit RGB 5-6-5 with a light Bayer dither so gradients stay crisp, not blurry.
fn quantize_565(rgb: (u8, u8, u8), x: u16, y: u32) -> (u8, u8, u8) {
    let t = BAYER4[(y as usize) & 3][(x as usize) & 3];
    let r = dither_chan(rgb.0, t, 8);
    let g = dither_chan(rgb.1, t, 4);
    let b = dither_chan(rgb.2, t, 8);
    let r5 = r >> 3;
    let g6 = g >> 2;
    let b5 = b >> 3;
    (
        (r5 << 3) | (r5 >> 2),
        (g6 << 2) | (g6 >> 4),
        (b5 << 3) | (b5 >> 2),
    )
}

fn dither_chan(value: u8, bayer: i16, step: i16) -> u8 {
    let noise = (bayer - 8) * step / 16;
    (value as i16 + noise).clamp(0, 255) as u8
}

fn embedded_picture(path: &Path) -> Option<Vec<u8>> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !storage::is_audio(name) {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".flac") {
        return flac_picture(path);
    }
    if lower.ends_with(".mp3") {
        return id3_picture(path);
    }
    None
}

fn flac_picture(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut hdr = [0u8; 4];
    file.read_exact(&mut hdr).ok()?;
    if &hdr != b"fLaC" {
        return None;
    }
    let mut fallback: Option<Vec<u8>> = None;
    loop {
        let mut block = [0u8; 4];
        if file.read_exact(&mut block).is_err() {
            break;
        }
        let last = block[0] & 0x80 != 0;
        let kind = block[0] & 0x7f;
        let len = u32::from_be_bytes([0, block[1], block[2], block[3]]) as usize;
        if kind != 6 || len > EMBEDDED_ART_MAX {
            if file.seek(SeekFrom::Current(len as i64)).is_err() {
                break;
            }
            if last {
                break;
            }
            continue;
        }
        let mut body = vec![0u8; len];
        if file.read_exact(&mut body).is_err() {
            break;
        }
        if let Some((pic_type, data)) = parse_flac_picture(&body) {
            if data.len() > EMBEDDED_ART_MAX {
                if last {
                    break;
                }
                continue;
            }
            if pic_type == 3 {
                return Some(data);
            }
            if fallback.is_none() {
                fallback = Some(data);
            }
        }
        if last {
            break;
        }
    }
    fallback
}

fn parse_flac_picture(body: &[u8]) -> Option<(u32, Vec<u8>)> {
    let mut offset = 0usize;
    let pic_type = read_be_u32(body, &mut offset)?;
    let mime_len = read_be_u32(body, &mut offset)? as usize;
    offset = offset.checked_add(mime_len)?;
    let desc_len = read_be_u32(body, &mut offset)? as usize;
    offset = offset.checked_add(desc_len)?;
    offset = offset.checked_add(16)?;
    let data_len = read_be_u32(body, &mut offset)? as usize;
    if offset.checked_add(data_len)? > body.len() {
        return None;
    }
    Some((pic_type, body[offset..offset + data_len].to_vec()))
}

fn read_be_u32(body: &[u8], offset: &mut usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    if end > body.len() {
        return None;
    }
    let value = u32::from_be_bytes(body[*offset..end].try_into().ok()?);
    *offset = end;
    Some(value)
}

fn id3_picture(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let mut head = [0u8; 10];
    file.read_exact(&mut head).ok()?;
    if &head[0..3] != b"ID3" {
        return None;
    }
    let major = head[3];
    let flags = head[5];
    let size = syncsafe(&head[6..10]) as usize;
    if size == 0 || size > EMBEDDED_ART_MAX {
        return None;
    }
    let mut body = vec![0u8; size];
    file.read_exact(&mut body).ok()?;
    let mut offset = 0usize;
    if flags & 0x40 != 0 && body.len() >= 4 {
        offset = if major == 4 {
            syncsafe(&body[0..4]) as usize
        } else {
            4 + u32::from_be_bytes(body[0..4].try_into().unwrap_or([0; 4])) as usize
        };
    }
    let (id_len, size_len) = if major == 2 { (3, 3) } else { (4, 4) };
    let mut fallback: Option<Vec<u8>> = None;
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
        if offset + frame_size > body.len() || frame_size == 0 || frame_size > EMBEDDED_ART_MAX {
            break;
        }
        let payload = &body[offset..offset + frame_size];
        offset += frame_size;
        let parsed = match frame_id.as_str() {
            "APIC" => parse_apic(payload),
            "PIC" => parse_pic(payload),
            _ => continue,
        };
        let Some((pic_type, data)) = parsed else {
            continue;
        };
        if pic_type == 3 {
            return Some(data);
        }
        if fallback.is_none() {
            fallback = Some(data);
        }
    }
    fallback
}

fn parse_apic(payload: &[u8]) -> Option<(u8, Vec<u8>)> {
    if payload.len() < 4 {
        return None;
    }
    let enc = payload[0];
    let mut i = 1usize;
    while i < payload.len() && payload[i] != 0 {
        i += 1;
    }
    i += 1;
    if i >= payload.len() {
        return None;
    }
    let pic_type = payload[i];
    i += 1;
    i = skip_id3_description(payload, i, enc)?;
    if i >= payload.len() {
        return None;
    }
    Some((pic_type, payload[i..].to_vec()))
}

fn parse_pic(payload: &[u8]) -> Option<(u8, Vec<u8>)> {
    if payload.len() < 6 {
        return None;
    }
    let enc = payload[0];
    let mut i = 4usize;
    if i >= payload.len() {
        return None;
    }
    let pic_type = payload[i];
    i += 1;
    i = skip_id3_description(payload, i, enc)?;
    if i >= payload.len() {
        return None;
    }
    Some((pic_type, payload[i..].to_vec()))
}

fn skip_id3_description(payload: &[u8], mut i: usize, enc: u8) -> Option<usize> {
    if enc == 1 || enc == 2 {
        while i + 1 < payload.len() && !(payload[i] == 0 && payload[i + 1] == 0) {
            i += 2;
        }
        i += 2;
    } else {
        while i < payload.len() && payload[i] != 0 {
            i += 1;
        }
        i += 1;
    }
    if i > payload.len() {
        None
    } else {
        Some(i)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use tempfile::TempDir;

    fn solid(r: u8, g: u8, b: u8, w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([r, g, b, 255]))
    }

    #[test]
    fn sidecar_cover_jpg_wins() {
        let tmp = TempDir::new().unwrap();
        let album = tmp.path().join("Keep - Album");
        fs::create_dir_all(&album).unwrap();
        solid(12, 34, 56, 4, 4)
            .save(album.join("Cover.JPG"))
            .unwrap();
        solid(1, 2, 3, 4, 4)
            .save(album.join("folder.png"))
            .unwrap();
        fs::write(album.join("01.flac"), b"fLaC").unwrap();
        let found = find_sidecar(&album).unwrap();
        assert_eq!(
            found.file_name().unwrap().to_string_lossy().to_ascii_lowercase(),
            "cover.jpg"
        );
        let img = load_album_art(&album.join("01.flac")).expect("sidecar art");
        assert!(img.width() >= 1 && img.height() >= 1);
    }

    #[test]
    fn load_album_art_from_png_sidecar() {
        let tmp = TempDir::new().unwrap();
        let album = tmp.path().join("Keep - Album");
        fs::create_dir_all(&album).unwrap();
        solid(12, 34, 56, 4, 4)
            .save(album.join("cover.png"))
            .unwrap();
        fs::write(album.join("01.flac"), b"fLaC").unwrap();
        let img = load_album_art(&album.join("01.flac")).expect("png sidecar");
        assert_eq!(img.get_pixel(0, 0), &image::Rgba([12, 34, 56, 255]));
    }

    #[test]
    fn missing_art_is_none() {
        let tmp = TempDir::new().unwrap();
        let album = tmp.path().join("Empty");
        fs::create_dir_all(&album).unwrap();
        fs::write(album.join("01.mp3"), b"ID3").unwrap();
        assert!(find_sidecar(&album).is_none());
        assert!(load_album_art(&album.join("01.mp3")).is_none());
    }

    #[test]
    fn square_cells_keep_cover_aspect() {
        assert_eq!(square_cells(56, 28), (56, 28));
        assert_eq!(square_cells(40, 28), (40, 20));
        assert_eq!(square_cells(0, 10), (0, 0));
    }

    #[test]
    fn raster_uses_half_blocks_and_stays_square() {
        let img = solid(200, 16, 16, 32, 32);
        let raster = rasterize(&img, 16, 12);
        assert_eq!((raster.width, raster.height), (16, 8));
        assert_eq!(raster.cells.len(), 128);
        for cell in &raster.cells {
            assert_eq!(cell.ch, '▀');
            assert!(
                cell.fg.0 > 150 && cell.bg.0 > 150,
                "red cover should stay red, got fg={:?} bg={:?}",
                cell.fg,
                cell.bg
            );
            assert!(cell.fg.1 < 50 && cell.bg.1 < 50);
        }
    }

    #[test]
    fn raster_center_crops_instead_of_stretching() {
        let mut img = RgbaImage::new(16, 8);
        for y in 0..8 {
            for x in 0..16 {
                let px = if x < 8 {
                    Rgba([220, 20, 20, 255])
                } else {
                    Rgba([20, 20, 220, 255])
                };
                img.put_pixel(x, y, px);
            }
        }
        let raster = rasterize(&img, 8, 4);
        assert_eq!((raster.width, raster.height), (8, 4));
        let left = raster.cell(0, 1).unwrap();
        let right = raster.cell(7, 1).unwrap();
        assert!(
            left.fg.0 > left.fg.2 && right.fg.2 > right.fg.0,
            "center crop should keep red|blue, left={:?} right={:?}",
            left.fg,
            right.fg
        );
    }

    #[test]
    fn large_cover_downscales_before_raster() {
        let img = downscale_cover(DynamicImage::ImageRgba8(solid(20, 180, 40, 800, 600)));
        assert!(img.width() <= DECODE_MAX && img.height() <= DECODE_MAX);
        let raster = rasterize(&img, 16, 8);
        assert_eq!((raster.width, raster.height), (16, 8));
        assert_eq!(raster.cells.len(), 128);
    }

    #[test]
    fn high_contrast_pattern_stays_readable() {
        let mut img = RgbaImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let on = (x / 8 + y / 8) % 2 == 0;
                let px = if on {
                    Rgba([240, 240, 240, 255])
                } else {
                    Rgba([16, 16, 16, 255])
                };
                img.put_pixel(x, y, px);
            }
        }
        let raster = rasterize(&img, 16, 8);
        let a = raster.cell(1, 1).unwrap().bg;
        let b = raster.cell(6, 1).unwrap().bg;
        let lum = |c: (u8, u8, u8)| c.0 as i16 + c.1 as i16 + c.2 as i16;
        assert!(
            (lum(a) - lum(b)).abs() > 200,
            "checkerboard should survive as 16-bit pixels, {a:?} vs {b:?}"
        );
    }
}
