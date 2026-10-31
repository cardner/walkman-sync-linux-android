"""Dest URI helpers and ADB/MTP parsing — no live device required."""
from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import storage
import walkman as walkman_mod


class TestStoragePaths(unittest.TestCase):
    def test_join_and_parent_remote(self) -> None:
        uri = "adb://ZX507/sdcard/Music"
        self.assertEqual(
            storage.join(uri, "Artist - Album", "01.flac"),
            "adb://ZX507/sdcard/Music/Artist - Album/01.flac",
        )
        self.assertEqual(
            storage.parent("adb://ZX507/sdcard/Music/Artist - Album/01.flac"),
            "adb://ZX507/sdcard/Music/Artist - Album",
        )
        self.assertTrue(storage.within(
            "adb://ZX507/sdcard/Music/a/b.flac", "adb://ZX507/sdcard/Music"))
        self.assertFalse(storage.within(
            "adb://ZX507/sdcard/Other/a.flac", "adb://ZX507/sdcard/Music"))

    def test_relative_remote(self) -> None:
        rel = storage.relative(
            "adb://ZX507/sdcard/Music/Artist - Album/01.flac",
            "adb://ZX507/sdcard/Music",
        )
        self.assertEqual(rel.replace("\\", "/"), "Artist - Album/01.flac")

    def test_adb_parse_device_path(self) -> None:
        store = storage._Adb("adb://SERIAL99/sdcard/Music")
        self.assertEqual(store.device, "SERIAL99")
        self.assertEqual(store.devpath, "/sdcard/Music")
        self.assertEqual(store.serial(), "adb-SERIAL99")

    def test_mtp_parse(self) -> None:
        store = storage._Mtp("mtp://WALKMAN/Internal shared storage/Music")
        self.assertEqual(store.device, "WALKMAN")
        self.assertEqual(store.rel, "Internal shared storage/Music")
        self.assertEqual(store.serial(), "mtp-WALKMAN")

    def test_filesystem_walk(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            album = os.path.join(tmp, "Artist - Album")
            os.makedirs(album)
            Path(album, "01.flac").write_bytes(b"x")
            Path(tmp, "cinder_loved.tsv").write_text("a\tb\n", encoding="utf-8")
            rows = storage.walk_files(tmp, dest=True)
            names = {rel.replace("\\", "/") for rel, _size, _mtime in rows}
            self.assertIn("Artist - Album/01.flac", names)
            self.assertTrue(all("cinder_" not in name for name in names))

    def test_detect_adb_builds_uris(self) -> None:
        def fake_run(args, **kwargs):
            result = mock.Mock()
            result.returncode = 0
            if args[1:] == ["devices"] or args[-1] == "devices":
                result.stdout = "List of devices\nABC123\tdevice\n"
            else:
                result.stdout = ""
            result.stderr = ""
            return result

        def fake_shell(serial, command):
            if "getprop" in command:
                return "NW-ZX507\n"
            if "/sdcard/Music" in command:
                return "1\n"
            if "ls /storage" in command:
                return "emulated\nself\n0000-1111\n"
            if "0000-1111" in command:
                return "1\n"
            return ""

        with mock.patch.object(storage.subprocess, "run", fake_run), \
                mock.patch.object(storage, "_adb_shell", fake_shell):
            internal, sd = storage._detect_adb()
        self.assertEqual(internal, "adb://ABC123/sdcard/Music")
        self.assertEqual(sd, "adb://ABC123/storage/0000-1111/Music")

    def test_looks_like_walkman_model(self) -> None:
        self.assertTrue(storage._looks_like_walkman_model("NW-ZX507"))
        self.assertTrue(storage._looks_like_walkman_model("NW-A105"))
        self.assertFalse(storage._looks_like_walkman_model("Pixel 8"))

    def test_volume_root_strips_remote_music(self) -> None:
        self.assertEqual(
            walkman_mod.volume_root("adb://ZX507/sdcard/Music"),
            "adb://ZX507/sdcard",
        )
        self.assertEqual(
            walkman_mod.resolve_music_path("adb://ZX507/sdcard/Music", android=True),
            "adb://ZX507/sdcard/Music",
        )

    def test_mtp_push_stages_dest_filename(self) -> None:
        calls: list[str] = []

        def fake_ps(script: str, timeout: int = 120) -> str:
            calls.append(script)
            return ""

        src = os.path.join(tempfile.gettempdir(), "sony-sync-mtp-src.bin")
        Path(src).write_bytes(b"abc")
        try:
            with mock.patch.object(storage, "_ps_run", fake_ps):
                store = storage._Mtp("mtp://WALKMAN/Internal shared storage/Music/Liked Songs.m3u8")
                store.push(src)
        finally:
            try:
                os.remove(src)
            except OSError:
                pass
        self.assertTrue(any("Liked Songs.m3u8" in script for script in calls))


class TestWalkmanKind(unittest.TestCase):
    def test_kinds_are_stable(self) -> None:
        self.assertEqual(walkman_mod.KIND_ANDROID, "android-walkman")
        self.assertIn(walkman_mod.KIND_CINDER, walkman_mod.DEVICE_KINDS)
