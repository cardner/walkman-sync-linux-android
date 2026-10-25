"""AudioMuse JSON → Cinder import TSV. No live server."""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import audiomuse
import walkman as walkman_mod

ENV_KEYS = (
    "AUDIOMUSE_URL", "AUDIOMUSE_TOKEN", "WALKMAN_SYNC_AUDIOMUSE_FROM",
)

PLAYLISTS_JSON = """{
  "Late_automatic": [
    {"item_id": "1", "title": "Night", "author": "Queen", "album": "Opera"},
    {"item_id": "2", "title": "", "author": "Queen"}
  ],
  "Sonic Fingerprint by AudioMuse-AI": [
    {"item_id": "3", "title": "One", "artist": "U2"}
  ]
}"""

QUEEN_TRACK = {
    "id": "s1",
    "fp": "aa",
    "artist": "Queen",
    "title": "Bohemian Rhapsody",
    "album": "A Night at the Opera",
    "tempo": 144.23,
    "energy": 0.122,
    "mood_vector": "rock:0.219,ambient:0.04,pop:0.099",
    "other_features": "danceable:0.25,aggressive:0.25,happy:0.55,party:0.19,relaxed:0.47,sad:0.57",
}

SCORES_JSON = (
    '{"tracks": ['
    '{"id": "s1", "fp": "aa", "artist": "Queen", "title": "Bohemian Rhapsody", '
    '"album": "A Night at the Opera", "tempo": 144.23, "energy": 0.122, '
    '"mood_vector": "rock:0.219,ambient:0.04,pop:0.099", '
    '"other_features": "danceable:0.25,aggressive:0.25,happy:0.55,party:0.19,relaxed:0.47,sad:0.57"}'
    '], "has_more": false}'
)


def _config(tmp: str, **kwargs) -> SimpleNamespace:
    music = os.path.join(tmp, "Music")
    os.makedirs(music, exist_ok=True)
    values = dict(
        audiomuse_url="http://muse.local",
        audiomuse_token="",
        internal_drive=music,
        is_android=False,
        device_kind=walkman_mod.KIND_CINDER,
    )
    values.update(kwargs)
    return SimpleNamespace(**values)


class TestPlaylists(unittest.TestCase):
    def test_classifies_mixes_and_am_ids(self) -> None:
        lists = audiomuse.playlists_from_json(PLAYLISTS_JSON)
        late = next(item for item in lists if item.name == "Late")
        self.assertEqual(late.kind, "automatic")
        self.assertEqual(late.id, "am:Late_automatic")
        self.assertEqual(len(late.tracks), 1)
        self.assertEqual(late.tracks[0].album, "Opera")
        fingerprint = next(item for item in lists if item.kind == "fingerprint")
        self.assertEqual(fingerprint.tracks[0].artist, "U2")
        self.assertEqual(fingerprint.tracks[0].album, "")

    def test_message_object_is_not_an_empty_playlist_set(self) -> None:
        with self.assertRaises(audiomuse.AudioMuseError):
            audiomuse.playlists_from_json('{"message":"unauthorized"}')
        self.assertEqual(audiomuse.playlists_from_json("{}"), [])

    def test_classify_suffixes(self) -> None:
        self.assertEqual(audiomuse.classify("Night Drive"), ("playlist", "Night Drive"))
        self.assertEqual(audiomuse.classify("Late_automatic"), ("automatic", "Late"))
        self.assertEqual(audiomuse.classify("Bus_instant"), ("instant", "Bus"))
        self.assertEqual(audiomuse.classify("Dawn_radio"), ("radio", "Dawn"))
        self.assertEqual(
            audiomuse.classify("Sonic Fingerprint by AudioMuse-AI"),
            ("fingerprint", "Sonic Fingerprint by AudioMuse-AI"),
        )


class TestScores(unittest.TestCase):
    def test_energy_maps_onto_unit_arousal(self) -> None:
        score = audiomuse.map_track(QUEEN_TRACK)
        self.assertIsNotNone(score)
        assert score is not None
        self.assertLess(abs(score.arousal - 0.8), 0.02, f"arousal {score.arousal} is still raw energy")
        self.assertGreater(abs(score.arousal - 0.122), 0.1)
        self.assertEqual(score.moods, "rock,pop")
        self.assertNotIn("ambient", score.moods)
        self.assertLess(abs(score.bpm - 144.23), 0.01)

    def test_payload_query_turns_embeddings_off(self) -> None:
        self.assertIn("include_embeddings=false", audiomuse.payload_query(1))
        self.assertIn("limit=500", audiomuse.payload_query(2))


class TestTsv(unittest.TestCase):
    def test_playlist_render_parse_round_trip(self) -> None:
        lists = audiomuse.playlists_from_json(PLAYLISTS_JSON)
        body = audiomuse.render_playlists(lists)
        self.assertTrue(body.startswith("# navidrome playlists"))
        parsed = audiomuse.parse_playlists(body)
        self.assertEqual(len(parsed), len(lists))
        by_id = {item.id: item for item in parsed}
        self.assertEqual(by_id["am:Late_automatic"].kind, "automatic")
        self.assertEqual(by_id["am:Late_automatic"].tracks[0].title, "Night")

    def test_merge_keeps_navidrome_replaces_am_drops_name_collisions(self) -> None:
        previous = audiomuse.render_playlists([
            audiomuse.Playlist(id="nd1", kind="playlist", name="Night Drive",
                               tracks=[audiomuse.Track("A", "One")]),
            audiomuse.Playlist(id="am:Late_automatic", kind="automatic", name="Late",
                               tracks=[audiomuse.Track("B", "Old")]),
            audiomuse.Playlist(id="nd-late", kind="automatic", name="Late",
                               tracks=[]),
        ])
        incoming = audiomuse.playlists_from_json(PLAYLISTS_JSON)
        merged = audiomuse.merge_playlist_import(previous, incoming)
        navidrome, am_lists = audiomuse.split_rendered(audiomuse.render_playlists(merged))
        self.assertEqual(len(navidrome), 1)
        self.assertEqual(navidrome[0].name, "Night Drive")
        self.assertEqual(navidrome[0].id, "nd1")
        late = next(item for item in am_lists if item.name == "Late")
        self.assertEqual(late.tracks[0].title, "Night")
        self.assertFalse(any(item.id == "nd-late" for item in merged))

    def test_score_tsv_has_cinder_header_mark(self) -> None:
        score = audiomuse.map_track(QUEEN_TRACK)
        assert score is not None
        body = audiomuse.render_scores([score])
        self.assertTrue(body.startswith("# audiomuse scores"))
        self.assertIn("Queen\tBohemian Rhapsody\tA Night at the Opera\t", body)
        self.assertIn("0.8000", body)


class TestPull(unittest.TestCase):
    def setUp(self) -> None:
        self._saved = {key: os.environ.get(key) for key in ENV_KEYS}
        for key in ENV_KEYS:
            os.environ.pop(key, None)

    def tearDown(self) -> None:
        for key, value in self._saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value

    def test_missing_url_is_a_noop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = _config(tmp, audiomuse_url="")
            result = audiomuse.pull(config)
            self.assertTrue(result.skipped)
            self.assertFalse(result.written)
            root = walkman_mod.volume_root(config.internal_drive)
            self.assertFalse(os.path.exists(os.path.join(root, audiomuse.PLAYLIST_IMPORT)))
            self.assertFalse(os.path.exists(os.path.join(root, audiomuse.SCORE_IMPORT)))

    def test_android_walkman_is_a_noop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = _config(tmp, is_android=True)
            result = audiomuse.pull(config)
            self.assertIn("android-walkman", result.skipped)
            self.assertFalse(result.written)

    def test_http_error_does_not_wipe_existing_import(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = _config(tmp)
            root = walkman_mod.volume_root(config.internal_drive)
            previous = audiomuse.render_playlists([
                audiomuse.Playlist(id="nd1", kind="playlist", name="Night Drive",
                                   tracks=[audiomuse.Track("A", "One")]),
            ])
            playlist_path = os.path.join(root, audiomuse.PLAYLIST_IMPORT)
            Path(playlist_path).write_text(previous, encoding="utf-8")
            with patch("audiomuse.http_get", side_effect=audiomuse.AudioMuseError("HTTP 503 from AudioMuse")):
                result = audiomuse.pull(config)
            self.assertEqual(result.error, "HTTP 503 from AudioMuse")
            self.assertFalse(result.written)
            kept = Path(playlist_path).read_text(encoding="utf-8")
            self.assertEqual(kept, previous)
            self.assertIn("Night Drive", kept)

    def test_dry_run_lists_mixes_without_writing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = _config(tmp)

            def fake_get(url: str, token: str = "") -> str:
                if url.endswith("/api/playlists"):
                    return PLAYLISTS_JSON
                return SCORES_JSON

            with patch("audiomuse.http_get", side_effect=fake_get):
                result = audiomuse.pull(config, dry_run=True)
            self.assertFalse(result.written)
            self.assertFalse(result.error)
            self.assertEqual(len(result.mixes), 2)
            self.assertEqual(len(result.scores), 1)
            names = {item.name for item in result.mixes}
            self.assertIn("Late", names)
            root = walkman_mod.volume_root(config.internal_drive)
            self.assertFalse(os.path.exists(os.path.join(root, audiomuse.PLAYLIST_IMPORT)))
            self.assertFalse(os.path.exists(os.path.join(root, audiomuse.SCORE_IMPORT)))

    def test_write_merges_navidrome_and_pages_scores(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            config = _config(tmp, audiomuse_token="tok")
            root = walkman_mod.volume_root(config.internal_drive)
            Path(root, audiomuse.PLAYLIST_IMPORT).write_text(
                audiomuse.render_playlists([
                    audiomuse.Playlist(id="nd1", kind="playlist", name="Night Drive",
                                       tracks=[audiomuse.Track("A", "One")]),
                ]),
                encoding="utf-8",
            )
            calls: list[str] = []

            def fake_get(url: str, token: str = "") -> str:
                calls.append(url)
                self.assertEqual(token, "tok")
                if url.endswith("/api/playlists"):
                    return PLAYLISTS_JSON
                if "page=1" in url:
                    page = dict(QUEEN_TRACK)
                    return '{"tracks": [' + _track_json(page) + '], "has_more": true}'
                if "page=2" in url:
                    other = dict(QUEEN_TRACK, id="s2", title="Somebody to Love")
                    return '{"tracks": [' + _track_json(other) + '], "has_more": false}'
                raise audiomuse.AudioMuseError(f"unexpected {url}")

            with patch("audiomuse.http_get", side_effect=fake_get):
                result = audiomuse.pull(config)
            self.assertTrue(result.written)
            self.assertEqual(result.navidrome_kept, 1)
            self.assertEqual(len(result.scores), 2)
            body = Path(root, audiomuse.PLAYLIST_IMPORT).read_text(encoding="utf-8")
            self.assertIn("playlist\tnd1\tplaylist\tNight Drive", body)
            self.assertIn("playlist\tam:Late_automatic\tautomatic\tLate", body)
            scores = Path(root, audiomuse.SCORE_IMPORT).read_text(encoding="utf-8")
            self.assertTrue(scores.startswith("# audiomuse scores"))
            self.assertIn("Somebody to Love", scores)
            self.assertTrue(any("include_embeddings=false" in url for url in calls))
            self.assertTrue(any("/api/sync?" in url and "page=2" in url for url in calls))


def _track_json(track: dict) -> str:
    return json.dumps(track)


if __name__ == "__main__":
    unittest.main(verbosity=2)
