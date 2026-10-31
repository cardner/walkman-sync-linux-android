"""Music-sync identity, dest, and copy-skip tests."""
from __future__ import annotations

import os
import shutil
import struct
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import catalog as catalog_mod
import contentkey
import manifest as manifest_mod
import sync
import walkman as walkman_mod


def _minimal_flac(md5: bytes, comment: bytes = b"") -> bytes:
    """A tiny FLAC with STREAMINFO (and optional VORBIS_COMMENT). Audio is 1 silent block."""
    assert len(md5) == 16
    # min/max block 16, min/max frame 0, 44100Hz 1ch 16bit, 16 samples
    packed = ((44100 << 44) | (0 << 41) | (15 << 36) | 16)
    streaminfo = (
        (16).to_bytes(2, "big")
        + (16).to_bytes(2, "big")
        + (0).to_bytes(3, "big")
        + (0).to_bytes(3, "big")
        + packed.to_bytes(8, "big")
        + md5
    )
    last = 0x80 if not comment else 0x00
    blocks = bytes([last | 0]) + (34).to_bytes(3, "big") + streaminfo
    if comment:
        vendor = b"t"
        body = struct.pack("<I", len(vendor)) + vendor + struct.pack("<I", 1)
        entry = b"TITLE=x"
        body += struct.pack("<I", len(entry)) + entry
        body += comment
        blocks += bytes([0x80 | 4]) + len(body).to_bytes(3, "big") + body
    # one empty frame is not required for the content key (MD5 path).
    return b"fLaC" + blocks + b"\x00" * 16


class TestContentKey(unittest.TestCase):
    def test_flac_md5_ignores_vorbis_comment_growth(self) -> None:
        md5 = bytes(range(16))
        with tempfile.TemporaryDirectory() as tmp:
            a = Path(tmp, "a.flac")
            b = Path(tmp, "b.flac")
            a.write_bytes(_minimal_flac(md5))
            b.write_bytes(_minimal_flac(md5, comment=b"padding" * 40))
            self.assertEqual(contentkey.content_key(str(a)), contentkey.content_key(str(b)))
            self.assertTrue(contentkey.content_key(str(a)).startswith("flac-"))


class TestManifestSkip(unittest.TestCase):
    def test_retag_is_skipped_when_manifest_key_matches(self) -> None:
        md5 = bytes(range(16))
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp, "src", "Album")
            dest = Path(tmp, "dest", "Album")
            source.mkdir(parents=True)
            dest.mkdir(parents=True)
            src_file = source / "01.flac"
            dst_file = dest / "01.flac"
            src_file.write_bytes(_minimal_flac(md5))
            dst_file.write_bytes(_minimal_flac(md5, comment=b"tags"))
            rel = os.path.join("Album", "01.flac")
            man = manifest_mod.Manifest()
            man.remember(rel, src_file.stat().st_size, src_file.stat().st_mtime,
                         dst_file.stat().st_size, contentkey.content_key(str(src_file)))
            grown = source / "01.flac"
            grown.write_bytes(_minimal_flac(md5, comment=b"more tags again"))
            key = contentkey.content_key(str(grown))
            self.assertTrue(man.up_to_date(rel, grown.stat().st_size, grown.stat().st_mtime,
                                           dst_file.stat().st_size, key))


class TestSingleDest(unittest.TestCase):
    def test_every_album_goes_to_dest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source = os.path.join(tmp, "src")
            playlists = os.path.join(tmp, "pl")
            internal = os.path.join(tmp, "internal", "Music")
            sd = os.path.join(tmp, "sd", "Music")
            for folder in (source, playlists, internal, sd):
                os.makedirs(folder)
            for album in ("A - One", "B - Two"):
                os.makedirs(os.path.join(source, album))
                Path(source, album, "01.flac").write_bytes(b"0" * 100)
            Path(playlists, "Mix.m3u8").write_text(
                os.path.join(source, "A - One", "01.flac") + "\n", encoding="utf-8")
            config = sync.AppConfig(
                source_dir=source, playlist_dir=playlists,
                internal_drive=internal, sd_drive=sd, dest="sd",
                internal_max_gb=1, sd_max_gb=1,
            )
            result = sync.SyncResult()
            plan = sync.build_library_plan(config, result)
            self.assertIsNotNone(plan)
            self.assertTrue(all(d == sd for d in plan.assignments.values()))
            self.assertEqual(set(plan.playlist_drive.values()), {sd})

    def test_other_volume_is_stale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source = os.path.join(tmp, "src")
            playlists = os.path.join(tmp, "pl")
            internal = os.path.join(tmp, "internal", "Music")
            sd = os.path.join(tmp, "sd", "Music")
            for folder in (source, playlists, internal, sd):
                os.makedirs(folder)
            os.makedirs(os.path.join(source, "Keep - Album"))
            Path(source, "Keep - Album", "01.flac").write_bytes(b"0" * 50)
            os.makedirs(os.path.join(internal, "Keep - Album"))
            Path(internal, "Keep - Album", "01.flac").write_bytes(b"0" * 50)
            Path(playlists, "Empty.m3u8").write_text("\n", encoding="utf-8")
            config = sync.AppConfig(
                source_dir=source, playlist_dir=playlists,
                internal_drive=internal, sd_drive=sd, dest="sd",
                internal_max_gb=1, sd_max_gb=1,
            )
            prev = os.environ.get("SONY_SYNC_CATALOG")
            os.environ["SONY_SYNC_CATALOG"] = os.path.join(tmp, "catalog.sqlite")
            try:
                report = sync.build_scan_report(config)
            finally:
                if prev is None:
                    os.environ.pop("SONY_SYNC_CATALOG", None)
                else:
                    os.environ["SONY_SYNC_CATALOG"] = prev
            self.assertTrue(any("internal" in path.lower() or os.path.normcase(internal) in os.path.normcase(path)
                                for path in report.stale_music_files))

    def test_case_mismatch_is_treated_as_present(self) -> None:
        scan = sync.DeviceScan(root="x")
        scan.add_file("Album\\Song.flac", 10, 1.0)
        self.assertIsNotNone(scan.lookup("album\\song.flac") or scan.lookup("Album/Song.flac")
                             or scan.lookup(os.path.normpath("Album/Song.flac")))

    def test_partial_is_stale(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "Music")
            os.makedirs(os.path.join(dest, "Album"))
            Path(dest, "Album", ".01.flac.partial").write_bytes(b"half")
            scan = sync.scan_device(dest)
            self.assertTrue(any(p.endswith(".partial") for p in scan.partials))


class TestCinderPreserve(unittest.TestCase):
    def test_cinder_files_are_not_playlists(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "Music")
            os.makedirs(dest)
            Path(dest, "cinder_stats.tsv").write_text("x", encoding="utf-8")
            Path(dest, "Mix.m3u8").write_text("a\n", encoding="utf-8")
            scan = sync.scan_device(dest)
            self.assertIn("Mix.m3u8", scan.playlists)
            self.assertNotIn("cinder_stats.tsv", scan.playlists)

    def test_cinder_playlists_dir_is_not_swept(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "Music")
            os.makedirs(os.path.join(dest, "cinder_playlists"))
            Path(dest, "cinder_playlists", "Mine.m3u8").write_text("#EXTM3U\n", encoding="utf-8")
            Path(dest, "cinder_audiomuse.tsv").write_text("# audiomuse scores\n", encoding="utf-8")
            scan = sync.scan_device(dest)
            self.assertNotIn("cinder_playlists", scan.artist_files)
            self.assertEqual(scan.playlists, set())

    def test_pull_strips_cinder_edited_stamp(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            internal = os.path.join(tmp, "internal", "Music")
            sd = os.path.join(tmp, "sd", "Music")
            playlists = os.path.join(tmp, "pl")
            os.makedirs(internal)
            os.makedirs(sd)
            os.makedirs(playlists)
            cinder = os.path.join(tmp, "internal", "cinder_playlists")
            os.makedirs(cinder)
            Path(cinder, "Mine.m3u8").write_text(
                "#EXTM3U\n#CINDER-EDITED:1700000000\n/contents/MUSIC/a.flac\n",
                encoding="utf-8",
            )
            config = sync.AppConfig(
                source_dir=playlists, playlist_dir=playlists,
                internal_drive=internal, sd_drive=sd, dest="internal",
            )
            result = sync.SyncResult()
            sync.pull_cinder_playlists(config, result, dry_run=False)
            pulled = Path(playlists, "from-walkman", "Mine.m3u8").read_text(encoding="utf-8")
            self.assertNotIn("#CINDER-EDITED:", pulled)
            self.assertIn("a.flac", pulled)


class TestWalkmanDest(unittest.TestCase):
    def test_pick_dest_honours_setting(self) -> None:
        self.assertEqual(walkman_mod.pick_dest("sd", "I", "S"), "S")
        self.assertEqual(walkman_mod.pick_dest("internal", "I", "S"), "I")

    def test_auto_prefers_larger_total_when_held_unknown(self) -> None:
        original = walkman_mod.live_total_bytes
        try:
            walkman_mod.live_total_bytes = lambda path: 64 if path == "I" else 128  # type: ignore[assignment]
            self.assertEqual(walkman_mod.pick_dest("auto", "I", "S"), "S")
        finally:
            walkman_mod.live_total_bytes = original  # type: ignore[assignment]

    def test_detect_finds_walkman_label_and_other_removable(self) -> None:
        fake = [
            walkman_mod.VolumeCandidate("C:\\", "", 500, 100, walkman_mod.DRIVE_FIXED),
            walkman_mod.VolumeCandidate("F:\\", "WALKMAN", 54_000_000_000, 10_000, walkman_mod.DRIVE_REMOVABLE),
            walkman_mod.VolumeCandidate("G:\\", "32 GB", 128_000_000_000, 20_000, walkman_mod.DRIVE_REMOVABLE),
        ]
        original = walkman_mod._collect_candidates
        try:
            walkman_mod._collect_candidates = lambda: fake  # type: ignore[assignment]
            internal, sd = walkman_mod.detect_walkman_roots(use_env=False)
        finally:
            walkman_mod._collect_candidates = original  # type: ignore[assignment]
        self.assertEqual(internal, "F:\\")
        self.assertEqual(sd, "G:\\")

    def test_detect_does_not_invent_d_drive(self) -> None:
        original = walkman_mod._collect_candidates
        try:
            walkman_mod._collect_candidates = lambda: []  # type: ignore[assignment]
            internal, sd = walkman_mod.detect_walkman_roots(use_env=False)
        finally:
            walkman_mod._collect_candidates = original  # type: ignore[assignment]
        self.assertEqual(internal, "")
        self.assertEqual(sd, "")

    def test_detect_macos_volumes(self) -> None:
        fake = [
            walkman_mod.VolumeCandidate("/Volumes/WALKMAN", "WALKMAN", 54_000_000_000, 10_000,
                                        walkman_mod.DRIVE_REMOVABLE),
            walkman_mod.VolumeCandidate("/Volumes/32 GB", "32 GB", 128_000_000_000, 20_000,
                                        walkman_mod.DRIVE_REMOVABLE),
        ]
        original = walkman_mod._collect_candidates
        try:
            walkman_mod._collect_candidates = lambda: fake  # type: ignore[assignment]
            internal, sd = walkman_mod.detect_walkman_roots(use_env=False)
        finally:
            walkman_mod._collect_candidates = original  # type: ignore[assignment]
        self.assertEqual(internal, "/Volumes/WALKMAN")
        self.assertEqual(sd, "/Volumes/32 GB")

    def test_drive_root_of_leaves_remote_uris(self) -> None:
        self.assertEqual(
            walkman_mod.drive_root_of("adb://ZX507/sdcard/Music"),
            "adb://ZX507/sdcard/Music",
        )
        self.assertEqual(
            walkman_mod.drive_root_of("mtp://WALKMAN/Internal shared storage/Music"),
            "mtp://WALKMAN/Internal shared storage/Music",
        )

    def test_drive_root_of_posix_mount(self) -> None:
        if os.name == "nt":
            self.skipTest("POSIX mount walk")
        with tempfile.TemporaryDirectory() as tmp:
            mount = Path(tmp, "Volumes", "WALKMAN")
            music = mount / "MUSIC"
            music.mkdir(parents=True)
            self.assertEqual(walkman_mod.drive_root_of(str(music)), str(mount))

    def test_unc_fallback_uses_volumes_share(self) -> None:
        original_name = walkman_mod.os.name
        original_isdir = walkman_mod.os.path.isdir
        try:
            walkman_mod.os.name = "posix"
            walkman_mod.os.path.isdir = (  # type: ignore[method-assign]
                lambda path, **_: str(path).replace("\\", "/") == "/Volumes/Music"
            )
            self.assertEqual(
                walkman_mod.resolve_library_path(r"\\WillineauVault\Music"),
                os.path.join("/Volumes", "Music"),
            )
        finally:
            walkman_mod.os.name = original_name
            walkman_mod.os.path.isdir = original_isdir  # type: ignore[method-assign]


class TestCopyIdentity(unittest.TestCase):
    def _config(self, tmp: str) -> sync.AppConfig:
        source = os.path.join(tmp, "src")
        playlists = os.path.join(tmp, "pl")
        internal = os.path.join(tmp, "internal", "Music")
        sd = os.path.join(tmp, "sd", "Music")
        for folder in (source, playlists, internal, sd):
            os.makedirs(folder)
        album = os.path.join(source, "Keep - Album")
        os.makedirs(album)
        Path(album, "01.flac").write_bytes(_minimal_flac(bytes(range(16))))
        Path(playlists, "Mix.m3u8").write_text("\n", encoding="utf-8")
        return sync.AppConfig(
            source_dir=source, playlist_dir=playlists,
            internal_drive=internal, sd_drive=sd, dest="internal",
            internal_max_gb=1, sd_max_gb=1,
        )

    def test_size_mismatch_is_recopied(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = self._config(tmp)
            dest_album = os.path.join(config.internal_drive, "Keep - Album")
            os.makedirs(dest_album)
            Path(dest_album, "01.flac").write_bytes(b"truncated")
            result = sync.SyncResult()
            plan = sync.build_library_plan(config, result)
            jobs = sync.build_copy_jobs(config, plan, {
                config.internal_drive: sync.scan_device(config.internal_drive),
                config.sd_drive: sync.scan_device(config.sd_drive),
            })
            self.assertTrue(any(job.rel_path.endswith("01.flac") for job in jobs))

    def test_dest_larger_than_source_is_skipped_when_manifest_agrees(self) -> None:
        md5 = bytes(range(16))
        with tempfile.TemporaryDirectory() as tmp:
            config = self._config(tmp)
            src = os.path.join(config.source_dir, "Keep - Album", "01.flac")
            dest_album = os.path.join(config.internal_drive, "Keep - Album")
            os.makedirs(dest_album)
            dest = os.path.join(dest_album, "01.flac")
            Path(dest).write_bytes(_minimal_flac(md5, comment=b"sensme" * 80))
            rel = os.path.join("Keep - Album", "01.flac")
            man = manifest_mod.Manifest()
            src_stat = os.stat(src)
            man.remember(rel, src_stat.st_size, src_stat.st_mtime,
                         os.path.getsize(dest), contentkey.content_key(src) or "")
            man.save(config.internal_drive)
            result = sync.SyncResult()
            plan = sync.build_library_plan(config, result)
            jobs = sync.build_copy_jobs(
                config, plan,
                {
                    config.internal_drive: sync.scan_device(config.internal_drive),
                    config.sd_drive: sync.scan_device(config.sd_drive),
                },
                dest_manifest=manifest_mod.Manifest.load(config.internal_drive),
            )
            self.assertFalse(any(job.rel_path.endswith("01.flac") for job in jobs))

    def test_partial_is_not_treated_as_done(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = self._config(tmp)
            dest_album = os.path.join(config.internal_drive, "Keep - Album")
            os.makedirs(dest_album)
            Path(dest_album, ".01.flac.partial").write_bytes(b"half")
            scan = sync.scan_device(config.internal_drive)
            self.assertIsNone(scan.lookup(os.path.join("Keep - Album", "01.flac")))
            result = sync.SyncResult()
            plan = sync.build_library_plan(config, result)
            jobs = sync.build_copy_jobs(config, plan, {
                config.internal_drive: scan,
                config.sd_drive: sync.scan_device(config.sd_drive),
            })
            self.assertTrue(any(job.rel_path.endswith("01.flac") for job in jobs))

    def test_adopt_from_other_volume(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = self._config(tmp)
            config.dest = "sd"
            src = os.path.join(config.source_dir, "Keep - Album", "01.flac")
            other_album = os.path.join(config.internal_drive, "Keep - Album")
            os.makedirs(other_album)
            shutil.copy2(src, os.path.join(other_album, "01.flac"))
            result = sync.SyncResult()
            plan = sync.build_library_plan(config, result)
            jobs = sync.build_copy_jobs(config, plan, {
                config.internal_drive: sync.scan_device(config.internal_drive),
                config.sd_drive: sync.scan_device(config.sd_drive),
            })
            self.assertTrue(jobs)
            self.assertTrue(jobs[0].adopt_from)
            self.assertTrue(os.path.normcase(jobs[0].adopt_from).startswith(
                os.path.normcase(config.internal_drive)))


def _stub_config() -> sync.AppConfig:
    return sync.AppConfig(
        source_dir="src", playlist_dir="pl",
        internal_drive=os.path.join("I:", "Music"),
        sd_drive=os.path.join("S:", "Music"),
        dest="internal", auto_detect_drives=False,
    )


def _stub_plan(**kwargs) -> sync.LibraryPlan:
    fields = dict(
        assignments={}, file_locations={},
        desired_files_by_drive={}, expected_playlists_by_drive={},
        playlist_names=set(), source_artists=set(),
        playlist_entries={}, playlist_drive={},
        folder_sizes={}, folder_playlists={}, excluded_folders=set(),
        source_files={},
    )
    fields.update(kwargs)
    return sync.LibraryPlan(**fields)


def _entry(rel: str, artist: str = "Artist", title: str = "Song") -> sync.FileEntry:
    return sync.FileEntry(rel, size=10, mtime=1.0, artist=artist, title=title)


class TestDuplicateGroups(unittest.TestCase):
    def _groups(self, *, source_files=None, dest_files=None, other_files=None,
                dest_keys=None):
        config = _stub_config()
        dest = sync.DeviceScan(root=config.dest_drive)
        other = sync.DeviceScan(root=config.other_drive)
        dest_manifest = manifest_mod.Manifest()
        for rel in dest_files or []:
            key = (dest_keys or {}).get(rel, "")
            dest.add_file(rel, 10, 1.0, content_key=key)
            if key:
                dest_manifest.remember(rel, 10, 1.0, 10, key)
        for rel in other_files or []:
            other.add_file(rel, 10, 1.0)
        plan = _stub_plan(source_files=source_files or {})
        return sync.build_duplicate_groups(
            config, plan,
            {config.dest_drive: dest, config.other_drive: other},
            dest_manifest, manifest_mod.Manifest(),
        ), config

    def test_same_title_on_two_albums_is_not_a_likes_duplicate(self) -> None:
        a = os.path.join("Artist - Studio", "01 - Artist - Song.flac")
        b = os.path.join("Artist - Hits", "04 - Artist - Song.flac")
        groups, _ = self._groups(source_files={
            "Artist - Studio": [_entry(a)],
            "Artist - Hits": [_entry(b)],
        })
        self.assertFalse([g for g in groups if g.kind == "likes-key"])

    def test_same_title_twice_on_one_album_is_a_likes_duplicate(self) -> None:
        a = os.path.join("Artist - Studio", "01 - Artist - Song.flac")
        b = os.path.join("Artist - Studio", "01 - Artist - Song (1).flac")
        groups, _ = self._groups(source_files={
            "Artist - Studio": [_entry(a), _entry(b)],
        })
        likes = [g for g in groups if g.kind == "likes-key"]
        self.assertEqual(len(likes), 1)
        self.assertFalse(likes[0].auto_fix)
        folded = {os.path.normcase(p) for p in likes[0].paths}
        self.assertEqual(folded, {os.path.normcase(a), os.path.normcase(b)})

    def test_two_on_album_a_and_one_on_b_keeps_only_a(self) -> None:
        a1 = os.path.join("Artist - Studio", "01 - Artist - Song.flac")
        a2 = os.path.join("Artist - Studio", "01 - Artist - Song (1).flac")
        b = os.path.join("Artist - Hits", "04 - Artist - Song.flac")
        groups, _ = self._groups(source_files={
            "Artist - Studio": [_entry(a1), _entry(a2)],
            "Artist - Hits": [_entry(b)],
        })
        likes = [g for g in groups if g.kind == "likes-key"]
        self.assertEqual(len(likes), 1)
        folded = {os.path.normcase(p) for p in likes[0].paths}
        self.assertEqual(folded, {os.path.normcase(a1), os.path.normcase(a2)})
        self.assertNotIn(os.path.normcase(b), folded)

    def test_same_content_key_on_two_albums_is_not_a_duplicate(self) -> None:
        a = os.path.join("Artist - Studio", "01.flac")
        b = os.path.join("Artist - Hits", "04.flac")
        groups, _ = self._groups(
            dest_files=[a, b],
            dest_keys={a: "flac-same", b: "flac-same"},
        )
        self.assertFalse([g for g in groups if g.kind == "content-key"])

    def test_same_content_key_twice_on_one_album_auto_fixes(self) -> None:
        a = os.path.join("Artist - Studio", "01.flac")
        b = os.path.join("Artist - Studio", "01 (1).flac")
        groups, config = self._groups(
            dest_files=[a, b],
            dest_keys={a: "flac-same", b: "flac-same"},
        )
        content = [g for g in groups if g.kind == "content-key"]
        self.assertEqual(len(content), 1)
        self.assertTrue(content[0].auto_fix)
        folded = {os.path.normcase(p) for p in content[0].paths}
        self.assertEqual(folded, {
            os.path.normcase(os.path.join(config.dest_drive, a)),
            os.path.normcase(os.path.join(config.dest_drive, b)),
        })

    def test_cross_drive_same_rel_still_groups(self) -> None:
        rel = os.path.join("Artist - Studio", "01.flac")
        groups, config = self._groups(dest_files=[rel], other_files=[rel])
        cross = [g for g in groups if g.kind == "cross-drive"]
        self.assertEqual(len(cross), 1)
        self.assertTrue(cross[0].auto_fix)
        folded = {os.path.normcase(p) for p in cross[0].paths}
        self.assertEqual(folded, {
            os.path.normcase(os.path.join(config.dest_drive, rel)),
            os.path.normcase(os.path.join(config.other_drive, rel)),
        })


class TestAndroidProfile(unittest.TestCase):
    def test_dsd_and_aiff_are_audio(self) -> None:
        for suffix in (".dsf", ".dff", ".aiff", ".aif"):
            self.assertIn(suffix, sync.AUDIO_EXT)
            self.assertIn(suffix, walkman_mod.AUDIO_EXT)

    def test_android_skips_cinder_playlists(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "Music")
            os.makedirs(dest)
            plan = sync.LibraryPlan(
                assignments={},
                file_locations={},
                desired_files_by_drive={dest: set()},
                expected_playlists_by_drive={dest: {"Mix.m3u8"}},
                playlist_names={"Mix.m3u8"},
                source_artists=set(),
                playlist_entries={"Mix.m3u8": ["Artist - Album/01.flac"]},
                playlist_drive={"Mix.m3u8": dest},
                folder_sizes={},
                folder_playlists={},
                excluded_folders=set(),
            )
            config = sync.AppConfig(
                source_dir=tmp, playlist_dir=tmp,
                internal_drive=dest, sd_drive="", dest="internal",
                device_kind=walkman_mod.KIND_ANDROID,
            )
            result = sync.SyncResult()
            sync.process_playlist("Mix.m3u8", plan, config, result, dry_run=False)
            self.assertTrue(os.path.isfile(os.path.join(dest, "Mix.m3u8")))
            self.assertFalse(os.path.isdir(os.path.join(tmp, "cinder_playlists")))

    def test_cinder_still_writes_player_playlists(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = os.path.join(tmp, "Music")
            os.makedirs(dest)
            plan = sync.LibraryPlan(
                assignments={},
                file_locations={},
                desired_files_by_drive={dest: set()},
                expected_playlists_by_drive={dest: {"Mix.m3u8"}},
                playlist_names={"Mix.m3u8"},
                source_artists=set(),
                playlist_entries={"Mix.m3u8": ["Artist - Album/01.flac"]},
                playlist_drive={"Mix.m3u8": dest},
                folder_sizes={},
                folder_playlists={},
                excluded_folders=set(),
            )
            config = sync.AppConfig(
                source_dir=tmp, playlist_dir=tmp,
                internal_drive=dest, sd_drive="", dest="internal",
                device_kind=walkman_mod.KIND_CINDER,
            )
            result = sync.SyncResult()
            sync.process_playlist("Mix.m3u8", plan, config, result, dry_run=False)
            self.assertTrue(os.path.isdir(os.path.join(tmp, "cinder_playlists")))

    def test_dest_connected_follows_sd(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            sd = os.path.join(tmp, "sd", "Music")
            os.makedirs(sd)
            config = sync.AppConfig(
                source_dir=tmp, playlist_dir=tmp,
                internal_drive=os.path.join(tmp, "missing", "Music"),
                sd_drive=sd, dest="sd",
            )
            self.assertTrue(config.dest_is_connected())
            config.dest = "internal"
            self.assertFalse(config.dest_is_connected())

    def test_serial_mismatch_refuses_to_sweep(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = self._android_config(tmp)
            leftover = os.path.join(config.internal_drive, "Gone - Album")
            os.makedirs(leftover)
            Path(leftover, "01.flac").write_bytes(b"0" * 40)
            prev = os.environ.get("SONY_SYNC_CATALOG")
            os.environ["SONY_SYNC_CATALOG"] = os.path.join(tmp, "catalog.sqlite")
            original = catalog_mod.Catalog.confirm_live
            try:
                with catalog_mod.Catalog() as cat:
                    cat.refresh_dest(config.internal_drive, catalog_mod.KIND_INTERNAL)

                def refuse(self, kind, path):
                    return False, "destination letter/path is a different volume"

                catalog_mod.Catalog.confirm_live = refuse  # type: ignore[method-assign]
                result = sync.sync_library(config, dry_run=False)
                self.assertTrue(any("different volume" in line for line in result.logs))
                self.assertTrue(os.path.isfile(os.path.join(leftover, "01.flac")))
            finally:
                catalog_mod.Catalog.confirm_live = original  # type: ignore[method-assign]
                if prev is None:
                    os.environ.pop("SONY_SYNC_CATALOG", None)
                else:
                    os.environ["SONY_SYNC_CATALOG"] = prev

    def test_infer_android_from_music_without_cinder_markers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            card = os.path.join(tmp, "card")
            os.makedirs(os.path.join(card, "Music"))
            self.assertEqual(walkman_mod.infer_device_kind("", card), walkman_mod.KIND_ANDROID)

    def test_infer_cinder_from_markers(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = os.path.join(tmp, "walkman")
            os.makedirs(os.path.join(root, "MUSIC"))
            Path(root, "cinder_loved.tsv").write_text("a\tb\n", encoding="utf-8")
            self.assertEqual(walkman_mod.infer_device_kind(root, ""), walkman_mod.KIND_CINDER)

    def test_android_prefers_music_folder(self) -> None:
        self.assertEqual(walkman_mod.music_folder_names(android=True)[0], "Music")
        self.assertEqual(walkman_mod.music_folder_names(android=False)[0], "MUSIC")

    def _android_config(self, tmp: str) -> sync.AppConfig:
        source = os.path.join(tmp, "src")
        playlists = os.path.join(tmp, "pl")
        internal = os.path.join(tmp, "internal", "Music")
        for folder in (source, playlists, internal):
            os.makedirs(folder)
        album = os.path.join(source, "Keep - Album")
        os.makedirs(album)
        Path(album, "01.flac").write_bytes(_minimal_flac(bytes(range(16))))
        Path(playlists, "Mix.m3u8").write_text("\n", encoding="utf-8")
        return sync.AppConfig(
            source_dir=source, playlist_dir=playlists,
            internal_drive=internal, sd_drive="", dest="internal",
            device_kind=walkman_mod.KIND_ANDROID,
            internal_max_gb=1, sd_max_gb=1,
        )
