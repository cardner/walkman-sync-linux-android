//! Locate or fetch Google platform-tools `adb` so Android dests work without PATH setup.

use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use crate::paths;

const WINDOWS_URL: &str = "https://dl.google.com/android/repository/platform-tools-latest-windows.zip";
const DARWIN_URL: &str = "https://dl.google.com/android/repository/platform-tools-latest-darwin.zip";
const LINUX_URL: &str = "https://dl.google.com/android/repository/platform-tools-latest-linux.zip";

static RESOLVED: OnceLock<String> = OnceLock::new();

fn adb_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

fn platform_tools_url() -> &'static str {
    if cfg!(windows) {
        WINDOWS_URL
    } else if cfg!(target_os = "macos") {
        DARWIN_URL
    } else {
        LINUX_URL
    }
}

pub fn managed_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("Walkman Sync").join("platform-tools"))
}

fn adb_path_in(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(adb_name());
    path.is_file().then_some(path)
}

fn skip_fetch() -> bool {
    if cfg!(test) {
        return true;
    }
    match std::env::var("SONY_SYNC_NO_ADB_FETCH") {
        Ok(v) => !v.is_empty() && v != "0",
        Err(_) => false,
    }
}

/// Path used for every `adb` spawn. Fetches platform-tools once per process if needed.
pub fn adb_bin() -> String {
    RESOLVED
        .get_or_init(|| {
            find_existing_adb()
                .or_else(|| {
                    if skip_fetch() {
                        None
                    } else {
                        fetch_managed_adb()
                    }
                })
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "adb".into())
        })
        .clone()
}

pub fn find_existing_adb() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SONY_SYNC_ADB") {
        let path = PathBuf::from(p.trim());
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(found) = adb_path_in(&dir.join("platform-tools")) {
                return Some(found);
            }
        }
    }
    if let Some(found) = adb_path_in(&paths::data_root().join("platform-tools")) {
        return Some(found);
    }
    if let Some(dir) = managed_dir() {
        if let Some(found) = adb_path_in(&dir) {
            return Some(found);
        }
    }
    if let Some(local) = dirs::data_local_dir() {
        let sdk = local
            .join("Android")
            .join("Sdk")
            .join("platform-tools")
            .join(adb_name());
        if sdk.is_file() {
            return Some(sdk);
        }
    }
    which_adb()
}

fn which_adb() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if let Some(found) = adb_path_in(&dir) {
            return Some(found);
        }
    }
    None
}

fn fetch_managed_adb() -> Option<PathBuf> {
    let dir = managed_dir()?;
    if fetch_platform_tools(&dir).is_err() {
        return None;
    }
    adb_path_in(&dir)
}

pub fn fetch_platform_tools(dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;
    let resp = ureq::get(platform_tools_url())
        .timeout(Duration::from_secs(120))
        .call()
        .map_err(|e| io::Error::other(e.to_string()))?;
    if resp.status() != 200 {
        return Err(io::Error::other(format!(
            "platform-tools download HTTP {}",
            resp.status()
        )));
    }
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes)?;
    extract_platform_tools(&bytes, dest)?;
    #[cfg(unix)]
    chmod_adb(dest)?;
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(dest)
            .status();
    }
    if adb_path_in(dest).is_none() {
        return Err(io::Error::other("platform-tools zip did not contain adb"));
    }
    Ok(())
}

#[cfg(unix)]
fn chmod_adb(dest: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let adb = dest.join("adb");
    if adb.is_file() {
        let mut perms = fs::metadata(&adb)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&adb, perms)?;
    }
    Ok(())
}

pub fn extract_platform_tools(bytes: &[u8], dest: &Path) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| io::Error::other(e.to_string()))?;
    fs::create_dir_all(dest)?;
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let Some(rel) = enclosed_platform_tools_path(file.name()) else {
            continue;
        };
        let out = dest.join(rel);
        if file.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut dest_file = fs::File::create(&out)?;
        io::copy(&mut file, &mut dest_file)?;
        dest_file.flush()?;
    }
    Ok(())
}

fn enclosed_platform_tools_path(name: &str) -> Option<PathBuf> {
    let normalized = name.replace('\\', "/");
    let trimmed = normalized.trim_start_matches("./");
    let rest = trimmed
        .strip_prefix("platform-tools/")
        .unwrap_or(trimmed);
    if rest.is_empty() || rest.ends_with('/') {
        return None;
    }
    let path = Path::new(rest);
    if path.is_absolute() {
        return None;
    }
    if path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) {
        return None;
    }
    Some(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::FileOptions;

    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_writes_adb_and_windows_dlls() {
        let bytes = zip_with_entries(&[
            ("platform-tools/adb.exe", b"adb"),
            ("platform-tools/AdbWinApi.dll", b"api"),
            ("platform-tools/AdbWinUsbApi.dll", b"usb"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        extract_platform_tools(&bytes, dir.path()).unwrap();
        assert_eq!(
            fs::read(dir.path().join("adb.exe")).unwrap(),
            b"adb"
        );
        assert!(dir.path().join("AdbWinApi.dll").is_file());
        assert!(dir.path().join("AdbWinUsbApi.dll").is_file());
    }

    #[test]
    fn extract_skips_zip_slip() {
        let bytes = zip_with_entries(&[
            ("platform-tools/adb", b"ok"),
            ("platform-tools/../../evil", b"nope"),
            ("../outside", b"nope"),
        ]);
        let dir = tempfile::tempdir().unwrap();
        extract_platform_tools(&bytes, dir.path()).unwrap();
        assert!(dir.path().join("adb").is_file());
        assert!(!dir.path().join("evil").is_file());
        assert!(!dir.path().parent().unwrap().join("evil").is_file());
        assert!(!dir.path().parent().unwrap().join("outside").is_file());
    }

    #[test]
    fn enclosed_path_strips_prefix() {
        assert_eq!(
            enclosed_platform_tools_path("platform-tools/adb.exe").unwrap(),
            PathBuf::from("adb.exe")
        );
        assert_eq!(
            enclosed_platform_tools_path("platform-tools\\AdbWinApi.dll").unwrap(),
            PathBuf::from("AdbWinApi.dll")
        );
        assert!(enclosed_platform_tools_path("platform-tools/../adb").is_none());
        assert!(enclosed_platform_tools_path("/tmp/adb").is_none());
    }

    #[test]
    fn adb_path_in_requires_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(adb_path_in(dir.path()).is_none());
        fs::write(dir.path().join(adb_name()), b"x").unwrap();
        assert_eq!(
            adb_path_in(dir.path()).unwrap().file_name().unwrap(),
            adb_name()
        );
    }

    #[test]
    #[ignore]
    fn live_fetch_platform_tools_contains_adb() {
        let dir = tempfile::tempdir().unwrap();
        fetch_platform_tools(dir.path()).expect("download Google platform-tools");
        assert!(
            adb_path_in(dir.path()).is_some(),
            "expected {} in {}",
            adb_name(),
            dir.path().display()
        );
        #[cfg(windows)]
        {
            assert!(dir.path().join("AdbWinApi.dll").is_file());
            assert!(dir.path().join("AdbWinUsbApi.dll").is_file());
        }
    }
}
