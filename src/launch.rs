//! Desktop / Start Menu / Applications shortcuts. The binary is the target.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::paths;
use crate::walkman::APP_TITLE;

pub const ICON_ICO: &[u8] = include_bytes!("../assets/walkman-sync.ico");
pub const ICON_PNG: &[u8] = include_bytes!("../assets/walkman-sync.png");

pub fn find_wt() -> Option<PathBuf> {
    which("wt.exe").or_else(|| which("wt"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

pub fn exe_path() -> Result<PathBuf> {
    std::env::current_exe().context("current_exe")
}

pub fn workdir() -> PathBuf {
    paths::data_root()
}

pub fn ensure_icons_next_to_exe() -> Result<(PathBuf, PathBuf)> {
    let exe = exe_path()?;
    let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    let ico = dir.join("walkman-sync.ico");
    let png = dir.join("walkman-sync.png");
    if !ico.is_file() {
        fs::write(&ico, ICON_ICO).context("write ico")?;
    }
    if !png.is_file() {
        fs::write(&png, ICON_PNG).context("write png")?;
    }
    // Also keep copies in the data root so a cargo-run from target/ still finds assets/.
    let assets = paths::assets_dir();
    let _ = fs::create_dir_all(&assets);
    if !assets.join("walkman-sync.ico").is_file() {
        let _ = fs::write(assets.join("walkman-sync.ico"), ICON_ICO);
    }
    if !assets.join("walkman-sync.png").is_file() {
        let _ = fs::write(assets.join("walkman-sync.png"), ICON_PNG);
    }
    Ok((ico, png))
}

pub fn shortcut_spec(wt: Option<&Path>, exe: &Path, workdir: &Path) -> (String, String) {
    if let Some(wt) = wt {
        let args = format!(
            "--title \"{APP_TITLE}\" --suppressApplicationTitle -d \"{}\" -- \"{}\"",
            workdir.display(),
            exe.display()
        );
        return (wt.to_string_lossy().into_owned(), args);
    }
    (exe.to_string_lossy().into_owned(), String::new())
}

pub fn window_command(wt: Option<&Path>, exe: &Path, workdir: &Path) -> Option<Vec<String>> {
    let wt = wt?;
    Some(vec![
        wt.to_string_lossy().into_owned(),
        "--title".into(),
        APP_TITLE.into(),
        "--suppressApplicationTitle".into(),
        "-d".into(),
        workdir.to_string_lossy().into_owned(),
        "--".into(),
        exe.to_string_lossy().into_owned(),
    ])
}

pub fn macos_shell_command(exe: &Path, workdir: &Path) -> String {
    format!(
        "cd {} && exec {}",
        shlex(&workdir.to_string_lossy()),
        shlex(&exe.to_string_lossy())
    )
}

fn shlex(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn shortcut_powershell(
    path: &Path,
    target: &str,
    arguments: &str,
    workdir: &Path,
    icon: Option<&Path>,
) -> String {
    let mut parts = vec![
        format!(
            "$s = (New-Object -ComObject WScript.Shell).CreateShortcut({});",
            ps_quote(&path.to_string_lossy())
        ),
        format!("$s.TargetPath = {};", ps_quote(target)),
        format!("$s.Arguments = {};", ps_quote(arguments)),
        format!(
            "$s.WorkingDirectory = {};",
            ps_quote(&workdir.to_string_lossy())
        ),
        "$s.WindowStyle = 1;".into(),
        format!("$s.Description = {};", ps_quote(APP_TITLE)),
    ];
    if let Some(icon) = icon {
        parts.push(format!(
            "$s.IconLocation = {};",
            ps_quote(&format!("{},0", icon.display()))
        ));
    }
    parts.push("$s.Save()".into());
    parts.join(" ")
}

pub fn write_windows_shortcut(
    path: &Path,
    target: &str,
    arguments: &str,
    workdir: &Path,
    icon: Option<&Path>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let script = shortcut_powershell(path, target, arguments, workdir, icon);
    let completed = Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", &script])
        .output()
        .context("powershell")?;
    if !completed.status.success() {
        let detail = String::from_utf8_lossy(&completed.stderr);
        bail!("{}", detail.trim());
    }
    if !path.exists() {
        bail!("shortcut was not created: {}", path.display());
    }
    Ok(())
}

pub fn macos_command_script(exe: &Path, workdir: &Path) -> String {
    format!("#!/bin/bash\n{}\n", macos_shell_command(exe, workdir))
}

pub fn write_macos_command(path: &Path, exe: &Path, workdir: &Path, png: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, macos_command_script(exe, workdir))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(path, perms)?;
    }
    apply_macos_file_icon(path, png);
    Ok(())
}

pub fn write_macos_app(path: &Path, exe: &Path, workdir: &Path, png: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let script = macos_shell_command(exe, workdir);
    let source = format!(
        "tell application \"Terminal\"\n  do script {}\n  activate\nend tell\n",
        osascript_string(&script)
    );
    let completed = Command::new("osacompile")
        .args(["-o", &path.to_string_lossy()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(source.as_bytes())?;
            }
            child.wait_with_output()
        });
    match completed {
        Ok(out) if out.status.success() && path.exists() => {
            apply_macos_app_icon(path, png);
            Ok(())
        }
        Ok(out) => {
            let detail = String::from_utf8_lossy(&out.stderr);
            bail!("{}", detail.trim());
        }
        Err(err) => Err(err.into()),
    }
}

fn osascript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn apply_macos_file_icon(path: &Path, png: &Path) {
    if !cfg!(target_os = "macos") || !png.is_file() || !path.exists() {
        return;
    }
    let source = format!(
        "ObjC.import('AppKit');\nvar img = $.NSImage.alloc.initWithContentsOfFile({});\n$.NSWorkspace.sharedWorkspace.setIconForFileOptions(img, {}, 0);\n",
        serde_json::json!(path_str(png)),
        serde_json::json!(path_str(path)),
    );
    let _ = Command::new("osascript")
        .args(["-l", "JavaScript", "-e", &source])
        .output();
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn apply_macos_app_icon(app: &Path, png: &Path) {
    if !cfg!(target_os = "macos") || !png.is_file() {
        return;
    }
    let dest = app.join("Contents").join("Resources").join("applet.icns");
    if let Some(icns) = png_to_icns(png) {
        if dest.parent().map(|p| p.is_dir()).unwrap_or(false) {
            let _ = fs::copy(&icns, &dest);
        }
        if let Some(parent) = icns.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
    apply_macos_file_icon(app, png);
}

fn png_to_icns(png: &Path) -> Option<PathBuf> {
    if which("sips").is_none() || which("iconutil").is_none() {
        return None;
    }
    let tmp = std::env::temp_dir().join(format!("walkman-sync-icon-{}", std::process::id()));
    let iconset = tmp.join("icon.iconset");
    let _ = fs::create_dir_all(&iconset);
    let pairs = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_512x512@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ];
    for (px, name) in pairs {
        let out = iconset.join(name);
        let status = Command::new("sips")
            .args([
                "-z",
                &px.to_string(),
                &px.to_string(),
                &png.to_string_lossy(),
                "--out",
                &out.to_string_lossy(),
            ])
            .status()
            .ok()?;
        if !status.success() {
            let _ = fs::remove_dir_all(&tmp);
            return None;
        }
    }
    let icns = tmp.join("icon.icns");
    let status = Command::new("iconutil")
        .args([
            "-c",
            "icns",
            &iconset.to_string_lossy(),
            "-o",
            &icns.to_string_lossy(),
        ])
        .status()
        .ok()?;
    if !status.success() || !icns.is_file() {
        let _ = fs::remove_dir_all(&tmp);
        return None;
    }
    Some(icns)
}

pub fn default_shortcut_paths() -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        let desktop = dirs::desktop_dir().context("desktop")?;
        let programs = dirs::data_dir()
            .map(|p| p.join("Microsoft").join("Windows").join("Start Menu").join("Programs"))
            .context("start menu")?;
        Ok(vec![
            desktop.join(format!("{APP_TITLE}.lnk")),
            programs.join(format!("{APP_TITLE}.lnk")),
        ])
    }
    #[cfg(not(windows))]
    {
        let home = dirs::home_dir().context("home")?;
        Ok(vec![
            home.join("Desktop").join(format!("{APP_TITLE}.command")),
            home.join("Applications").join(format!("{APP_TITLE}.app")),
        ])
    }
}

/// Install shortcuts. `dest_override` is for tests (temp dirs); production uses Desktop/Start Menu.
pub fn install_shortcut_to(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let exe = exe_path()?;
    let workdir = workdir();
    let (ico, png) = ensure_icons_next_to_exe()?;
    let mut written = Vec::new();
    if cfg!(windows) {
        let wt = find_wt();
        let (target, arguments) = shortcut_spec(wt.as_deref(), &exe, &workdir);
        for path in paths {
            write_windows_shortcut(path, &target, &arguments, &workdir, Some(&ico))?;
            written.push(path.clone());
        }
        return Ok(written);
    }
    if let Some(command) = paths.first() {
        write_macos_command(command, &exe, &workdir, &png)?;
        written.push(command.clone());
    }
    if let Some(app) = paths.get(1) {
        match write_macos_app(app, &exe, &workdir, &png) {
            Ok(()) => written.push(app.clone()),
            Err(_) => {}
        }
    }
    Ok(written)
}

pub fn install_shortcut() -> Result<Vec<PathBuf>> {
    let paths = default_shortcut_paths()?;
    install_shortcut_to(&paths)
}
