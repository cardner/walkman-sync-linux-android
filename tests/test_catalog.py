"""Persistent library/dest catalog: incremental scans, skip-if-present, serial reuse."""
from __future__ import annotations

import os
import shutil
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import catalog as catalog_mod
import contentkey
import manifest as manifest_mod
import sync
import walkman as walkman_mod


def _minimal_flac(md5: bytes, comments: list[bytes] | None = None) -> bytes:
    assert len(md5) == 16
    packed = (44100 << 44) | (0 << 41) | (15 << 36) | 16
    streaminfo = (
        (16).to_bytes(2, "big")
        + (16).to_bytes(2, "big")
        + (0).to_bytes(3, "big")
        + (0).to_bytes(3, "big")
        + packed.to_bytes(8, "big")
        + md5
    )
    comments = comments or []
    last = 0x80 if not comments else 0x00
    blocks = bytes([last | 0]) + (34).to_bytes(3, "big") + streaminfo
    if comments:
        vendor = b"t"
        body = struct.pack("<I", len(vendor)) + vendor + struct.pack("<I", len(comments))
        for entry in comments:
            body += struct.pack("<I", len(entry)) + entry
        blocks += bytes([0x80 | 4]) + len(body).to_bytes(3, "big") + body
    return b"fLaC" + blocks + b"\x00" * 16


def _config(tmp: str, source: str, internal: str, sd: str) -> sync.AppConfig:
    playlists = os.path.join(tmp, "pl")
    os.makedirs(playlists, exist_ok=True)
    Path(playlists, "Mix.m3u8").write_text("\n", encoding="utf-8")
    return sync.AppConfig(
        source_dir=source, playlist_dir=playlists,
        internal_drive=internal, sd_drive=sd, dest="internal",
        internal_max_gb=1, sd_max_gb=1,
    )


class TestCatalog(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.db = os.path.join(self.root, "catalog.sqlite")
        self.source = os.path.join(self.root, "src")
        self.internal = os.path.join(self.root, "internal", "Music")
        self.sd = os.path.join(self.root, "sd", "Music")
        for folder in (self.source, self.internal, self.sd):
            os.makedirs(folder)
        album = os.path.join(self.source, "Keep - Album")
        os.makedirs(album)
        self.md5 = bytes(range(16))
        Path(album, "01.flac").write_bytes(_minimal_flac(self.md5, [b"ARTIST=Keep", b"TITLE=Song"]))
        self.cat = catalog_mod.Catalog(self.db)

    def tearDown(self) -> None:
        self.cat.close()
        self.tmp.cleanup()

    def test_second_scan_is_noop_and_does_not_reopen(self) -> None:
        first = self.cat.refresh_source(self.source)
        self.assertEqual(first.added, 1)
        self.assertEqual(first.updated, 0)
        self.assertGreaterEqual(first.keyed, 1)
        opened: list[str] = []
        original = contentkey.content_key

        def wrapped(path: str):
            opened.append(path)
            return original(path)

        contentkey.content_key = wrapped  # type: ignore[assignment]
        try:
            second = self.cat.refresh_source(self.source)
        finally:
            contentkey.content_key = original  # type: ignore[assignment]
        self.assertEqual(second.added, 0)
        self.assertEqual(second.updated, 0)
        self.assertEqual(second.removed, 0)
        self.assertEqual(second.unchanged, 1)
        self.assertEqual(opened, [])

    def test_retag_updates_source_but_copy_is_skipped(self) -> None:
        dest_album = os.path.join(self.internal, "Keep - Album")
        os.makedirs(dest_album)
        dest = os.path.join(dest_album, "01.flac")
        shutil.copy2(os.path.join(self.source, "Keep - Album", "01.flac"), dest)
        Path(dest).write_bytes(_minimal_flac(self.md5, [b"TITLE=tagged"]))
        self.cat.refresh_source(self.source)
        self.cat.refresh_dest(self.internal, catalog_mod.KIND_INTERNAL)
        src = Path(self.source, "Keep - Album", "01.flac")
        src.write_bytes(_minimal_flac(self.md5, [b"ARTIST=Keep", b"TITLE=Song", b"ALBUM=Keep"]))
        self.cat.refresh_source(self.source)
        source_row = self.cat.files_for(catalog_mod.KIND_SOURCE, self.source)[0]
        dest_row = self.cat.files_for(catalog_mod.KIND_INTERNAL, self.internal)[0]
        self.assertTrue(source_row.content_key)
        self.assertEqual(source_row.content_key, dest_row.content_key)
        self.assertNotEqual(source_row.size, dest_row.size)
        config = _config(self.root, self.source, self.internal, self.sd)
        result = sync.SyncResult()
        plan = sync.build_library_plan(config, result, catalog=self.cat)
        self.assertIsNotNone(plan)
        jobs = sync.build_copy_jobs(config, plan, {
            config.internal_drive: sync.device_scan_from_catalog(
                self.cat, catalog_mod.KIND_INTERNAL, self.internal),
            config.sd_drive: sync.device_scan_from_catalog(
                self.cat, catalog_mod.KIND_SD, self.sd),
        })
        self.assertFalse(any(job.rel_path.endswith("01.flac") for job in jobs))

    def test_dest_letter_change_reuses_serial(self) -> None:
        dest_a = os.path.join(self.root, "D", "Music")
        dest_b = os.path.join(self.root, "E", "Music")
        os.makedirs(os.path.join(dest_a, "Keep - Album"))
        shutil.copy2(
            os.path.join(self.source, "Keep - Album", "01.flac"),
            os.path.join(dest_a, "Keep - Album", "01.flac"),
        )
        original = walkman_mod.get_volume_serial
        walkman_mod.get_volume_serial = lambda path: "deadbeef"  # type: ignore[assignment]
        try:
            self.cat.refresh_dest(dest_a, catalog_mod.KIND_INTERNAL)
            first = self.cat.volume_for_root(catalog_mod.KIND_INTERNAL, dest_a)
            shutil.copytree(dest_a, dest_b)
            self.cat.refresh_dest(dest_b, catalog_mod.KIND_INTERNAL)
            second = self.cat.volume_for_root(catalog_mod.KIND_INTERNAL, dest_b)
            dest_files = self.cat.files_for(catalog_mod.KIND_INTERNAL, dest_b)
        finally:
            walkman_mod.get_volume_serial = original  # type: ignore[assignment]
        self.assertIsNotNone(first)
        self.assertIsNotNone(second)
        self.assertEqual(first.id, second.id)
        self.assertEqual(second.last_root, dest_b)
        self.assertEqual(len(dest_files), 1)

    def test_deleted_dest_file_is_dropped_and_to_copy(self) -> None:
        dest_album = os.path.join(self.internal, "Keep - Album")
        os.makedirs(dest_album)
        dest = os.path.join(dest_album, "01.flac")
        shutil.copy2(os.path.join(self.source, "Keep - Album", "01.flac"), dest)
        self.cat.refresh_source(self.source)
        self.cat.refresh_dest(self.internal, catalog_mod.KIND_INTERNAL)
        os.remove(dest)
        stats = self.cat.refresh_dest(self.internal, catalog_mod.KIND_INTERNAL)
        self.assertEqual(stats.removed, 1)
        self.assertEqual(self.cat.files_for(catalog_mod.KIND_INTERNAL, self.internal), [])
        config = _config(self.root, self.source, self.internal, self.sd)
        result = sync.SyncResult()
        plan = sync.build_library_plan(config, result, catalog=self.cat)
        jobs = sync.build_copy_jobs(config, plan, {
            config.internal_drive: sync.device_scan_from_catalog(
                self.cat, catalog_mod.KIND_INTERNAL, self.internal),
            config.sd_drive: sync.device_scan_from_catalog(
                self.cat, catalog_mod.KIND_SD, self.sd),
        })
        self.assertTrue(any(job.rel_path.endswith("01.flac") for job in jobs))

    def test_duplicate_inbox_uses_cached_tags(self) -> None:
        config = _config(self.root, self.source, self.internal, self.sd)
        result = sync.SyncResult()
        plan = sync.build_library_plan(config, result)
        assert plan is not None
        plan.source_files["Keep - Album"] = [
            sync.FileEntry(os.path.join("Keep - Album", "01.flac"), 10, 1.0,
                           artist="Keep", title="Song"),
            sync.FileEntry(os.path.join("Keep - Album", "01b.flac"), 10, 1.0,
                           artist="Keep", title="Song"),
        ]
        groups = sync.build_duplicate_groups(
            config, plan,
            {
                config.internal_drive: sync.DeviceScan(root=self.internal),
                config.sd_drive: sync.DeviceScan(root=self.sd),
            },
            dest_manifest=manifest_mod.Manifest(),
            other_manifest=manifest_mod.Manifest(),
        )
        likes = [group for group in groups if group.kind == "likes-key"]
        self.assertEqual(len(likes), 1)
        self.assertEqual(len(likes[0].paths), 2)

    def test_dest_mp3_is_not_frame_hashed(self) -> None:
        dest_album = os.path.join(self.internal, "Keep - Album")
        os.makedirs(dest_album)
        Path(dest_album, "01.mp3").write_bytes(b"ID3" + b"\x00" * 64 + b"\xff\xfb" + b"\x00" * 32)
        opened: list[str] = []
        original = contentkey.content_key

        def wrapped(path: str):
            opened.append(path)
            return original(path)

        contentkey.content_key = wrapped  # type: ignore[assignment]
        try:
            self.cat.refresh_dest(self.internal, catalog_mod.KIND_INTERNAL)
        finally:
            contentkey.content_key = original  # type: ignore[assignment]
        self.assertEqual(opened, [])
        row = self.cat.files_for(catalog_mod.KIND_INTERNAL, self.internal)[0]
        self.assertEqual(row.content_key, "")

    def test_case_fold_collisions_do_not_crash_refresh(self) -> None:
        """A case-sensitive NAS can list two paths that Windows folds to one rel_fold."""
        live = [
            (os.path.join("Keep - Album", "cover.jpg"), 10, 1.0),
            (os.path.join("keep - album", "Cover.jpg"), 12, 1.0),
            (os.path.join("Keep - Album", "01.flac"), 20, 1.0),
        ]
        with mock.patch("catalog.storage_mod.walk_files", return_value=live):
            stats = self.cat.refresh_source(self.source)
        self.assertGreaterEqual(stats.added + stats.unchanged + stats.updated, 2)
        rows = self.cat.files_for(catalog_mod.KIND_SOURCE, self.source)
        folds = [os.path.normcase(os.path.normpath(row.rel)) for row in rows]
        self.assertEqual(len(folds), len(set(folds)))
        with mock.patch("catalog.storage_mod.walk_files", return_value=live):
            again = self.cat.refresh_source(self.source)
        self.assertEqual(again.added, 0)

    def test_progress_callback_fires(self) -> None:
        seen: list[tuple[str, int, int]] = []
        self.cat.refresh_source(self.source, lambda folder, index, total: seen.append((folder, index, total)))
        self.assertTrue(seen)
        self.assertEqual(seen[-1][1], seen[-1][2])


if __name__ == "__main__":
    unittest.main()
