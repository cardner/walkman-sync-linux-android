"""Multiple Walkman profiles: shared library, dest and likes isolated by serial."""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import catalog as catalog_mod
import walkman as walkman_mod
from likesync import engine
from likesync.config import LikesConfig
from likesync.keys import track_key
from likesync.library import Entry, LibraryIndex
from likesync.state import State


class TestSettingsMigration(unittest.TestCase):
    def test_flat_sync_json_migrates_into_devices(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            music = os.path.join(tmp, "internal", "Music")
            os.makedirs(music)
            path = Path(tmp, "sync.json")
            path.write_text(json.dumps({
                "source_dir": r"\\WillineauVault\Music",
                "playlist_dir": r"\\WillineauVault\Playlists",
                "internal_drive": music,
                "sd_drive": "",
                "dest": "internal",
                "excluded_albums": "Skip - Album",
                "copy_workers": 2,
                "auto_detect_drives": True,
                "device_kind": "cinder",
                "internal_max_gb": 0,
                "sd_max_gb": 0,
            }), encoding="utf-8")
            with patch("walkman.get_volume_serial", return_value="aaa11111"):
                settings = walkman_mod.SyncSettings.load(path)
            self.assertEqual(settings.source_dir, r"\\WillineauVault\Music")
            self.assertEqual(settings.excluded_albums, "Skip - Album")
            self.assertEqual(settings.copy_workers, 2)
            self.assertEqual(settings.active_device, "aaa11111")
            profile = settings.active_profile()
            self.assertIsNotNone(profile)
            assert profile is not None
            self.assertEqual(profile.dest, "internal")
            self.assertEqual(profile.device_kind, "cinder")
            self.assertEqual(profile.internal_drive, music)
            settings.save(path)
            data = json.loads(path.read_text(encoding="utf-8"))
            self.assertIn("devices", data)
            self.assertNotIn("dest", data)
            self.assertNotIn("internal_drive", data)
            self.assertEqual(data["excluded_albums"], "Skip - Album")
            self.assertIn("aaa11111", data["devices"])


class TestBindLiveDevice(unittest.TestCase):
    def test_matching_serial_reuses_profile_and_updates_letters(self) -> None:
        settings = walkman_mod.SyncSettings()
        settings.devices["serialA"] = walkman_mod.DeviceProfile(
            name="A50", internal_serial="serialA",
            internal_drive=r"F:\MUSIC", dest="sd", device_kind="cinder",
        )
        settings.active_device = "serialA"
        new_path = r"H:\MUSIC"
        with patch("walkman.get_volume_serial", return_value="serialA"):
            created, changed = walkman_mod.bind_live_device(settings, new_path, "")
        self.assertFalse(created)
        self.assertTrue(changed)
        self.assertEqual(len(settings.devices), 1)
        self.assertEqual(settings.devices["serialA"].internal_drive, new_path)
        self.assertEqual(settings.devices["serialA"].dest, "sd")

    def test_new_serial_creates_a_second_profile(self) -> None:
        settings = walkman_mod.SyncSettings()
        settings.devices["serialA"] = walkman_mod.DeviceProfile(
            name="A50", internal_serial="serialA",
            internal_drive=r"F:\MUSIC", dest="internal",
        )
        settings.active_device = "serialA"
        with patch("walkman.get_volume_serial", return_value="serialB"):
            created, changed = walkman_mod.bind_live_device(
                settings, r"E:\Music", "", inferred_kind=walkman_mod.KIND_ANDROID)
        self.assertTrue(created)
        self.assertTrue(changed)
        self.assertEqual(len(settings.devices), 2)
        self.assertEqual(settings.active_device, "serialB")
        self.assertEqual(settings.devices["serialA"].name, "A50")
        self.assertEqual(settings.devices["serialA"].dest, "internal")
        self.assertEqual(settings.devices["serialB"].device_kind, walkman_mod.KIND_ANDROID)

    def test_pending_profile_is_filled_not_toasted_as_new(self) -> None:
        settings = walkman_mod.SyncSettings()
        settings.devices[walkman_mod.PENDING_DEVICE_ID] = walkman_mod.DeviceProfile(
            name="Walkman", dest="auto")
        settings.active_device = walkman_mod.PENDING_DEVICE_ID
        with patch("walkman.get_volume_serial", return_value="serialA"):
            created, _changed = walkman_mod.bind_live_device(settings, r"F:\MUSIC", "")
        self.assertFalse(created)
        self.assertNotIn(walkman_mod.PENDING_DEVICE_ID, settings.devices)
        self.assertEqual(settings.active_device, "serialA")


class TestCatalogIsolation(unittest.TestCase):
    def test_confirm_live_uses_path_serial_not_latest_kind(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path_a = os.path.join(tmp, "walkman-a", "Music")
            path_b = os.path.join(tmp, "walkman-b", "Music")
            os.makedirs(path_a)
            os.makedirs(path_b)
            Path(path_a, "a.txt").write_text("a", encoding="utf-8")
            Path(path_b, "b.txt").write_text("b", encoding="utf-8")
            db = os.path.join(tmp, "catalog.sqlite")

            def fake_serial(path: str) -> str:
                folded = os.path.normcase(path)
                if "walkman-a" in folded:
                    return "serialA"
                if "walkman-b" in folded:
                    return "serialB"
                return "other"

            cat = catalog_mod.Catalog(db)
            try:
                with patch("storage.serial", side_effect=fake_serial), \
                     patch("walkman.get_volume_serial", side_effect=fake_serial):
                    cat.refresh_dest(path_b, catalog_mod.KIND_INTERNAL)
                    cat.refresh_dest(path_a, catalog_mod.KIND_INTERNAL)
                    latest = cat.volume_for_kind(catalog_mod.KIND_INTERNAL)
                    self.assertEqual(latest.serial, "serialA")
                    ok, reason = cat.confirm_live(catalog_mod.KIND_INTERNAL, path_b)
                    self.assertTrue(ok, reason)
                    coverage = cat.coverage(path_b, "")
                    self.assertEqual(coverage[catalog_mod.KIND_INTERNAL].serial, "serialB")
                    files_a = cat.files_for(catalog_mod.KIND_INTERNAL, path_a)
                    files_b = cat.files_for(catalog_mod.KIND_INTERNAL, path_b)
                    self.assertTrue(files_a)
                    self.assertTrue(files_b)
                    self.assertNotEqual(
                        {row.rel for row in files_a}, {row.rel for row in files_b})
            finally:
                cat.close()


class TestLikesDeviceIsolation(unittest.TestCase):
    def test_new_walkman_is_additive_against_the_other_players_snapshot(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            index = LibraryIndex("src")
            index.add(Entry(rel_path=os.path.join("Artist - Album", "01.flac"),
                            artist="Artist", title="One", album="Album",
                            track_no=1, size=10, rating=0.0, duration=100))
            one = track_key("Artist", "One")
            state = State(path=Path(tmp) / "state.json")
            state.liked = {one: {"artist": "Artist", "title": "One"}}
            state.set_snapshot("device:serialA", dict(state.liked))
            state.set_snapshot("lastfm", dict(state.liked))
            config = LikesConfig(
                source_dir=tmp, playlist_dir=tmp,
                internal_root=os.path.join(tmp, "walkman-b"),
                lastfm_enabled=True, musicbee_export_playlist=False,
                device_id="serialB",
            )
            os.makedirs(os.path.join(config.internal_root, "Music"), exist_ok=True)
            Path(config.internal_root, "cinder_loved.tsv").write_text(
                "# artist\ttitle\n", encoding="utf-8")
            views = engine.gather(config, index, None, state)
            views["lastfm"].available = True
            views["lastfm"].tracks = dict(state.liked)
            plan = engine.build_plan(config, index, views, state)
            self.assertIn(one, plan.desired)
            self.assertEqual(plan.sinks["lastfm"].remove, [])

    def test_legacy_device_snapshot_moves_onto_active_id(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            state = State(path=Path(tmp) / "state.json")
            one = track_key("Artist", "One")
            state.set_snapshot("device", {one: {"artist": "Artist", "title": "One"}})
            self.assertTrue(state.migrate_legacy_device_snapshot("serialA"))
            self.assertNotIn("device", state.snapshots)
            self.assertIn("device:serialA", state.snapshots)
            self.assertFalse(state.migrate_legacy_device_snapshot("serialA"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
