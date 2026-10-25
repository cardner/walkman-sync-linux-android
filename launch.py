"""Bootstrap Walkman Sync into its own terminal window.

Double-click this (or `launch.pyw`) and the TUI opens in a dedicated window:
Windows Terminal on Windows when installed, Terminal.app on macOS. Run it from a
terminal you already have and it stays in-process — same as `python sync.py`.

    python launch.py --install-shortcut    # Desktop + Start Menu / Applications
    python launch.py --in-place            # never re-exec, even from Explorer/Finder
"""
from __future__ import annotations

import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import tui

REPO_ROOT = Path(__file__).resolve().parent
APP_TITLE = tui.APP_TITLE
CREATE_NEW_CONSOLE = 0x00000010
CSIDL_DESKTOPDIRECTORY = 0x0010
CSIDL_PROGRAMS = 0x0002
ICON_PNG = REPO_ROOT / "assets" / "walkman-sync.png"
ICON_ICO = REPO_ROOT / "assets" / "walkman-sync.ico"


def python_executable() -> str:
    """Console Python, never pythonw — the child needs a console."""
    exe = Path(sys.executable)
    if exe.name.lower() == "pythonw.exe":
        candidate = exe.with_name("python.exe")
        if candidate.exists():
            return str(candidate)
    if exe.exists() and exe.name.lower() not in {"pythonw.exe", "pythonw"}:
        return str(exe)
    return shutil.which("python3") or shutil.which("python") or sys.executable


def find_wt() -> str | None:
    return shutil.which("wt.exe") or shutil.which("wt")


def window_command(*, wt: str | None, python: str, repo: Path) -> list[str] | None:
    """Argv that opens a new window. None means 'Popen python with CREATE_NEW_CONSOLE'."""
    if not wt:
        return None
    return [
        wt,
        "--title", APP_TITLE,
        "--suppressApplicationTitle",
        "-d", str(repo),
        "--",
        python,
        str(repo / "sync.py"),
    ]


def console_command(*, python: str, repo: Path) -> list[str]:
    return [python, str(repo / "sync.py")]


def shortcut_spec(*, wt: str | None, python: str, repo: Path) -> tuple[str, str]:
    """(.lnk TargetPath, .lnk Arguments) for the pinned Windows shortcut."""
    script = str(repo / "sync.py")
    if wt:
        args = (
            f'--title "{APP_TITLE}" --suppressApplicationTitle '
            f'-d "{repo}" -- "{python}" "{script}"'
        )
        return wt, args
    return python, f'"{script}"'


def macos_shell_command(*, python: str, repo: Path) -> str:
    """cd + exec line used by .command files and Terminal.app 'do script'."""
    return (
        f"cd {shlex.quote(str(repo))} && "
        f"WALKMAN_SYNC_IN_PLACE=1 exec {shlex.quote(python)} "
        f"{shlex.quote(str(repo / 'launch.py'))} --in-place"
    )


def macos_command_script(*, python: str, repo: Path) -> str:
    return "#!/bin/bash\n" + macos_shell_command(python=python, repo=repo) + "\n"


def needs_new_window(
    argv: list[str] | None = None,
    *,
    parent: str | None = None,
    console: bool | None = None,
    environ: dict[str, str] | None = None,
) -> bool:
    args = sys.argv[1:] if argv is None else argv
    env = os.environ if environ is None else environ
    if "--in-place" in args or "--install-shortcut" in args:
        return False
    if env.get("WALKMAN_SYNC_IN_PLACE"):
        return False
    attached = tui.has_console() if console is None else console
    if not attached:
        return True
    name = (tui.parent_process_name() if parent is None else parent).lower()
    return name in tui.DETACH_PARENTS


def spawn_window(*, wt: str | None = None, python: str | None = None,
                 repo: Path | None = None) -> int:
    repo = repo or REPO_ROOT
    python = python or python_executable()
    env = os.environ.copy()
    env["WALKMAN_SYNC_IN_PLACE"] = "1"
    if os.name != "nt":
        script = macos_shell_command(python=python, repo=repo)
        subprocess.Popen(
            ["osascript", "-e",
             "tell application \"Terminal\" to do script " + _osascript_string(script)],
            env=env,
            close_fds=True,
        )
        return 0
    wt = find_wt() if wt is None else wt
    cmd = window_command(wt=wt, python=python, repo=repo)
    kwargs: dict = {"cwd": str(repo), "env": env, "close_fds": True}
    if cmd is None:
        cmd = console_command(python=python, repo=repo)
        kwargs["creationflags"] = CREATE_NEW_CONSOLE
    subprocess.Popen(cmd, **kwargs)
    return 0


def _osascript_string(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def _csidl(folder: int) -> Path:
    import ctypes
    buf = ctypes.create_unicode_buffer(260)
    ctypes.windll.shell32.SHGetFolderPathW(None, folder, None, 0, buf)
    path = Path(buf.value)
    if not path.is_dir():
        raise FileNotFoundError(f"shell folder {folder} is missing: {path}")
    return path


def shortcut_paths() -> list[Path]:
    if os.name == "nt":
        return [
            _csidl(CSIDL_DESKTOPDIRECTORY) / f"{APP_TITLE}.lnk",
            _csidl(CSIDL_PROGRAMS) / f"{APP_TITLE}.lnk",
        ]
    home = Path.home()
    return [
        home / "Desktop" / f"{APP_TITLE}.command",
        home / "Applications" / f"{APP_TITLE}.app",
    ]


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def icon_for_shortcut(*, windows: bool | None = None) -> Path | None:
    use_ico = os.name == "nt" if windows is None else windows
    path = ICON_ICO if use_ico else ICON_PNG
    return path if path.is_file() else None


def shortcut_powershell(
    path: Path,
    *,
    target: str,
    arguments: str,
    workdir: Path,
    icon: Path | None = None,
) -> str:
    parts = [
        f"$s = (New-Object -ComObject WScript.Shell).CreateShortcut({_ps_quote(str(path))});",
        f"$s.TargetPath = {_ps_quote(target)};",
        f"$s.Arguments = {_ps_quote(arguments)};",
        f"$s.WorkingDirectory = {_ps_quote(str(workdir))};",
        "$s.WindowStyle = 1;",
        f"$s.Description = {_ps_quote(APP_TITLE)};",
    ]
    if icon is not None:
        parts.append(f"$s.IconLocation = {_ps_quote(f'{icon},0')};")
    parts.append("$s.Save()")
    return " ".join(parts)


def write_shortcut(
    path: Path,
    *,
    target: str,
    arguments: str,
    workdir: Path,
    icon: Path | None = None,
) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    script = shortcut_powershell(
        path, target=target, arguments=arguments, workdir=workdir, icon=icon,
    )
    completed = subprocess.run(
        ["powershell", "-NoProfile", "-STA", "-Command", script],
        capture_output=True, text=True,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "powershell failed").strip()
        raise RuntimeError(detail)
    if not path.exists():
        raise RuntimeError(f"shortcut was not created: {path}")


def write_macos_command(path: Path, *, python: str, repo: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(macos_command_script(python=python, repo=repo), encoding="utf-8")
    path.chmod(path.stat().st_mode | 0o111)
    apply_macos_file_icon(path, ICON_PNG)


def write_macos_app(path: Path, *, python: str, repo: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    script = macos_shell_command(python=python, repo=repo)
    source = (
        "tell application \"Terminal\"\n"
        f"  do script {_osascript_string(script)}\n"
        "  activate\n"
        "end tell\n"
    )
    completed = subprocess.run(
        ["osacompile", "-o", str(path)],
        input=source, capture_output=True, text=True,
    )
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout or "osacompile failed").strip()
        raise RuntimeError(detail)
    if not path.exists():
        raise RuntimeError(f"shortcut was not created: {path}")
    apply_macos_app_icon(path, ICON_PNG)


def apply_macos_file_icon(path: Path, png: Path) -> None:
    """Set the Finder icon on a file or .app via AppKit. No-op off macOS."""
    if sys.platform != "darwin" or not png.is_file() or not path.exists():
        return
    source = (
        "ObjC.import('AppKit');\n"
        f"var img = $.NSImage.alloc.initWithContentsOfFile({json.dumps(str(png))});\n"
        f"$.NSWorkspace.sharedWorkspace.setIconForFileOptions("
        f"img, {json.dumps(str(path))}, 0);\n"
    )
    subprocess.run(
        ["osascript", "-l", "JavaScript", "-e", source],
        capture_output=True, text=True, check=False,
    )


def apply_macos_app_icon(app: Path, png: Path) -> None:
    """Replace osacompile's applet.icns, then stamp the bundle's Finder icon."""
    if sys.platform != "darwin" or not png.is_file():
        return
    dest = app / "Contents" / "Resources" / "applet.icns"
    icns = _png_to_icns(png)
    if icns is not None:
        try:
            if dest.parent.is_dir():
                shutil.copy2(icns, dest)
        except OSError:
            pass
        shutil.rmtree(icns.parent, ignore_errors=True)
    apply_macos_file_icon(app, png)


def _png_to_icns(png: Path) -> Path | None:
    if shutil.which("sips") is None or shutil.which("iconutil") is None:
        return None
    tmp = Path(tempfile.mkdtemp(prefix="walkman-sync-icon-"))
    iconset = tmp / "icon.iconset"
    iconset.mkdir()
    pairs = [
        (16, "icon_16x16.png"),
        (32, "icon_16x16@2x.png"),
        (32, "icon_32x32.png"),
        (64, "icon_32x32@2x.png"),
        (128, "icon_128x128.png"),
        (256, "icon_128x128@2x.png"),
        (256, "icon_256x256.png"),
        (512, "icon_256x256@2x.png"),
        (512, "icon_512x512.png"),
        (1024, "icon_512x512@2x.png"),
    ]
    for px, name in pairs:
        out = iconset / name
        completed = subprocess.run(
            ["sips", "-z", str(px), str(px), str(png), "--out", str(out)],
            capture_output=True, text=True, check=False,
        )
        if completed.returncode != 0 or not out.is_file():
            shutil.rmtree(tmp, ignore_errors=True)
            return None
    icns = tmp / "icon.icns"
    completed = subprocess.run(
        ["iconutil", "-c", "icns", str(iconset), "-o", str(icns)],
        capture_output=True, text=True, check=False,
    )
    if completed.returncode != 0 or not icns.is_file():
        shutil.rmtree(tmp, ignore_errors=True)
        return None
    return icns


def install_shortcut(*, wt: str | None = None, python: str | None = None,
                     repo: Path | None = None) -> list[Path]:
    repo = repo or REPO_ROOT
    python = python or python_executable()
    written: list[Path] = []
    if os.name == "nt":
        wt = find_wt() if wt is None else wt
        target, arguments = shortcut_spec(wt=wt, python=python, repo=repo)
        icon = icon_for_shortcut(windows=True)
        for path in shortcut_paths():
            write_shortcut(
                path, target=target, arguments=arguments, workdir=repo, icon=icon,
            )
            written.append(path)
        return written
    desktop, app = shortcut_paths()
    write_macos_command(desktop, python=python, repo=repo)
    written.append(desktop)
    try:
        write_macos_app(app, python=python, repo=repo)
        written.append(app)
    except Exception:
        # Desktop .command is enough to double-click; Dock pin needs the .app.
        pass
    return written


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if "--install-shortcut" in args:
        try:
            paths = install_shortcut()
        except Exception as exc:
            print(f"could not install the shortcut: {exc}", file=sys.stderr)
            return 1
        for path in paths:
            print(path)
        if os.name == "nt":
            print("Pin Walkman Sync from the Start Menu or the desktop.")
        else:
            print("Double-click Walkman Sync on the Desktop, or drag it from Applications to the Dock.")
        return 0
    if needs_new_window(args):
        return spawn_window()
    import app
    return tui.run_with_hold(app.main)


if __name__ == "__main__":
    raise SystemExit(main())
