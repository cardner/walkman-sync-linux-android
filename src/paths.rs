//! Where settings, catalog, and likes state live.
//!
//! A shortcut may start in System32. Prefer `SONY_SYNC_ROOT`, then a directory that
//! already holds `sync.json`, then the executable's directory (or its parents, so
//! `cargo run` from `target/debug` still finds the repo).

use std::env;
use std::path::{Path, PathBuf};

pub fn data_root() -> PathBuf {
    if let Ok(value) = env::var("SONY_SYNC_ROOT") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Ok(cwd) = env::current_dir() {
        if cwd.join("sync.json").is_file() || cwd.join("Cargo.toml").is_file() {
            return cwd;
        }
    }
    if let Ok(exe) = env::current_exe() {
        let mut dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
        for _ in 0..6 {
            if dir.join("sync.json").is_file() || dir.join("assets").join("walkman-sync.ico").is_file()
            {
                return dir;
            }
            if !dir.pop() {
                break;
            }
        }
        if let Some(parent) = exe.parent() {
            return parent.to_path_buf();
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    data_root().join("sync.json")
}

pub fn catalog_path() -> PathBuf {
    if let Ok(value) = env::var("SONY_SYNC_CATALOG") {
        if !value.trim().is_empty() {
            return PathBuf::from(value);
        }
    }
    data_root().join(".sony-sync").join("catalog.sqlite")
}

pub fn likes_state_path() -> PathBuf {
    data_root().join(".likesync").join("state.json")
}

pub fn env_path() -> PathBuf {
    data_root().join(".env")
}

pub fn assets_dir() -> PathBuf {
    data_root().join("assets")
}

pub fn save_env_value(key: &str, value: &str) {
    save_env_value_to(&env_path(), key, value);
}

pub fn save_env_value_to(path: &Path, key: &str, value: &str) {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut written = false;
    for line in existing.lines() {
        let stripped = line.trim();
        if stripped.starts_with(&format!("{key}=")) || stripped.starts_with(&format!("{key} =")) {
            out.push(format!("{key}=\"{value}\""));
            written = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !written {
        out.push(format!("{key}=\"{value}\""));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, out.join("\n") + "\n");
    env::set_var(key, value);
}

pub fn load_dotenv() {
    let path = env_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let mut value = value.trim().to_string();
        if (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if env::var_os(key).is_none() {
            env::set_var(key, value);
        }
    }
}
