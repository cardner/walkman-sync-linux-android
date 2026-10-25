"""Launcher tests — command lines and hold/spawn decisions, no real console window."""
from __future__ import annotations

import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import launch
import sync
import tui
import walkman as walkman_mod


class TestEnvPath(unittest.TestCase):
    def test_default_env_path_is_repo_root(self) -> None:
        self.assertEqual(sync.ENV_PATH, walkman_mod.REPO_ROOT / ".env")
        self.assertEqual(sync.ENV_PATH, Path(__file__).resolve().parent.parent / ".env")

    def test_save_env_value_writes_given_path(self) -> None:
        key = "SONY_SYNC_TEST_KEY"
        previous = os.environ.pop(key, None)
        try:
            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / ".env"
                sync.save_env_value(key, "unit", env_path=path)
                self.assertIn(f'{key}="unit"', path.read_text(encoding="utf-8"))
                self.assertEqual(os.environ.get(key), "unit")
        finally:
            if previous is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = previous


class TestWindowCommand(unittest.TestCase):
    def test_prefers_windows_terminal(self) -> None:
        repo = Path(r"C:\sync")
        python = r"C:\Python\python.exe"
        cmd = launch.window_command(wt=r"C:\wt.exe", python=python, repo=repo)
        self.assertIsNotNone(cmd)
        assert cmd is not None
        self.assertEqual(cmd[0], r"C:\wt.exe")
        self.assertIn("--suppressApplicationTitle", cmd)
        self.assertIn("--title", cmd)
        self.assertIn(launch.APP_TITLE, cmd)
        self.assertIn("-d", cmd)
        self.assertIn(str(repo), cmd)
        self.assertIn("--", cmd)
        self.assertIn(python, cmd)
        self.assertTrue(cmd[-1].endswith("sync.py"))

    def test_without_wt_is_none(self) -> None:
        self.assertIsNone(launch.window_command(
            wt=None, python="python", repo=Path(".")))

    def test_console_command_runs_sync_py(self) -> None:
        repo = Path(r"C:\sync")
        cmd = launch.console_command(python=r"C:\Python\python.exe", repo=repo)
        self.assertEqual(cmd, [r"C:\Python\python.exe", str(repo / "sync.py")])

    def test_shortcut_spec_uses_wt_when_present(self) -> None:
        repo = Path(r"C:\Users\Admin\Dev\Sony-sync")
        python = r"C:\Python\python.exe"
        target, args = launch.shortcut_spec(wt=r"C:\wt.exe", python=python, repo=repo)
        self.assertEqual(target, r"C:\wt.exe")
        self.assertIn("--suppressApplicationTitle", args)
        self.assertIn(str(repo), args)
        self.assertIn(python, args)
        self.assertIn("sync.py", args)

    def test_shortcut_spec_falls_back_to_python(self) -> None:
        repo = Path(r"C:\sync")
        python = r"C:\Python\python.exe"
        target, args = launch.shortcut_spec(wt=None, python=python, repo=repo)
        self.assertEqual(target, python)
        self.assertIn("sync.py", args)

    def test_macos_command_runs_launch_in_place(self) -> None:
        repo = Path("/Users/me/Sony-sync")
        python = "/usr/local/bin/python3"
        body = launch.macos_command_script(python=python, repo=repo)
        self.assertIn("#!/bin/bash", body)
        self.assertIn(str(repo), body)
        self.assertIn("--in-place", body)
        self.assertIn("launch.py", body)
        self.assertIn(python, body)


class TestNeedsNewWindow(unittest.TestCase):
    def test_in_place_flag(self) -> None:
        self.assertFalse(launch.needs_new_window(
            ["--in-place"], parent="explorer.exe", console=True, environ={}))

    def test_install_shortcut_flag(self) -> None:
        self.assertFalse(launch.needs_new_window(
            ["--install-shortcut"], parent="explorer.exe", console=True, environ={}))

    def test_in_place_env(self) -> None:
        self.assertFalse(launch.needs_new_window(
            [], parent="explorer.exe", console=True,
            environ={"WALKMAN_SYNC_IN_PLACE": "1"}))

    def test_explorer_with_console_wants_a_window(self) -> None:
        self.assertTrue(launch.needs_new_window(
            [], parent="explorer.exe", console=True, environ={}))

    def test_pythonw_without_console_wants_a_window(self) -> None:
        self.assertTrue(launch.needs_new_window(
            [], parent="pythonw.exe", console=False, environ={}))

    def test_finder_wants_a_window(self) -> None:
        self.assertTrue(launch.needs_new_window(
            [], parent="Finder", console=True, environ={}))
        self.assertFalse(launch.needs_new_window(
            ["--in-place"], parent="Finder", console=True, environ={}))

    def test_existing_terminal_stays_in_process(self) -> None:
        self.assertFalse(launch.needs_new_window(
            [], parent="WindowsTerminal.exe", console=True, environ={}))
        self.assertFalse(launch.needs_new_window(
            [], parent="powershell.exe", console=True, environ={}))
        self.assertFalse(launch.needs_new_window(
            [], parent="cmd.exe", console=True, environ={}))


class TestHoldConsole(unittest.TestCase):
    def test_hold_for_explorer(self) -> None:
        previous = os.environ.pop("WALKMAN_SYNC_HOLD", None)
        try:
            if not (sys.stdin and sys.stdin.isatty()):
                self.skipTest("stdin is not a tty")
            self.assertTrue(tui.should_hold_console(parent="explorer.exe"))
            self.assertFalse(tui.should_hold_console(parent="powershell.exe"))
        finally:
            if previous is not None:
                os.environ["WALKMAN_SYNC_HOLD"] = previous

    def test_hold_disabled_by_env(self) -> None:
        previous = os.environ.get("WALKMAN_SYNC_HOLD")
        os.environ["WALKMAN_SYNC_HOLD"] = "0"
        try:
            self.assertFalse(tui.should_hold_console(parent="explorer.exe"))
        finally:
            if previous is None:
                os.environ.pop("WALKMAN_SYNC_HOLD", None)
            else:
                os.environ["WALKMAN_SYNC_HOLD"] = previous

    def test_run_with_hold_returns_function_code(self) -> None:
        self.assertEqual(tui.run_with_hold(lambda: 0), 0)
        self.assertEqual(tui.run_with_hold(lambda: None), 0)

    def test_run_with_hold_catches_exceptions(self) -> None:
        os.environ["WALKMAN_SYNC_HOLD"] = "0"
        buf = io.StringIO()
        try:
            with contextlib.redirect_stderr(buf):
                self.assertEqual(tui.run_with_hold(_raise), 1)
        finally:
            os.environ.pop("WALKMAN_SYNC_HOLD", None)
        self.assertIn("RuntimeError: boom", buf.getvalue())


def _raise() -> int:
    raise RuntimeError("boom")


class TestPythonExecutable(unittest.TestCase):
    def test_current_interpreter_is_a_real_path(self) -> None:
        exe = launch.python_executable()
        self.assertTrue(Path(exe).name.lower().startswith("python"))
        self.assertNotEqual(Path(exe).name.lower(), "pythonw.exe")


class TestAppIcon(unittest.TestCase):
    def test_png_and_ico_ship_in_repo(self) -> None:
        self.assertTrue(launch.ICON_PNG.is_file(), launch.ICON_PNG)
        self.assertTrue(launch.ICON_ICO.is_file(), launch.ICON_ICO)
        self.assertEqual(launch.ICON_PNG.read_bytes()[:8], b"\x89PNG\r\n\x1a\n")
        self.assertEqual(launch.ICON_ICO.read_bytes()[:4], b"\x00\x00\x01\x00")

    def test_windows_shortcut_uses_ico(self) -> None:
        icon = launch.icon_for_shortcut(windows=True)
        self.assertEqual(icon, launch.ICON_ICO)
        script = launch.shortcut_powershell(
            Path(r"C:\Users\me\Desktop\Walkman Sync.lnk"),
            target=r"C:\Python\python.exe",
            arguments='"sync.py"',
            workdir=Path(r"C:\sync"),
            icon=icon,
        )
        self.assertIn("IconLocation", script)
        self.assertIn("walkman-sync.ico,0", script)

    def test_macos_shortcut_uses_png(self) -> None:
        self.assertEqual(launch.icon_for_shortcut(windows=False), launch.ICON_PNG)

    def test_macos_icon_helpers_are_noops_off_darwin(self) -> None:
        if sys.platform == "darwin":
            self.skipTest("would stamp a real Finder icon")
        with tempfile.TemporaryDirectory() as tmp:
            fake = Path(tmp) / "Walkman Sync.command"
            fake.write_text("#!/bin/bash\n", encoding="utf-8")
            launch.apply_macos_file_icon(fake, launch.ICON_PNG)
            launch.apply_macos_app_icon(Path(tmp) / "Walkman Sync.app", launch.ICON_PNG)


if __name__ == "__main__":
    unittest.main(verbosity=2)
