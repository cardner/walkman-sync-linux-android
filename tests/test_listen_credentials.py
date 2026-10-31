"""Device listen_sources.conf wins over PC .env, per field."""
from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import walkman as walkman_mod


ENV_KEYS = (
    "LASTFM_USERNAME", "LASTFM_API_KEY", "LASTFM_API_SECRET", "LASTFM_SESSION_KEY",
    "AUDIOMUSE_URL", "AUDIOMUSE_TOKEN",
    "WALKMAN_SYNC_LASTFM_FROM", "WALKMAN_SYNC_AUDIOMUSE_FROM",
    "CINDER_LISTEN_CONFIG",
)


class TestListenCredentials(unittest.TestCase):
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

    def test_parse_strips_quotes_and_skips_empty(self) -> None:
        parsed = walkman_mod.parse_listen_sources(
            "# comment\n"
            "lastfm_user=\"Superwils0\"\n"
            "lastfm_api_key=\n"
            "lastfm_api_secret=secret\n"
            "audiomuse_url=http://muse.local\n"
            "opensubsonic_url=http://navidrome.local\n"
        )
        self.assertEqual(parsed["lastfm_user"], "Superwils0")
        self.assertEqual(parsed["lastfm_api_secret"], "secret")
        self.assertEqual(parsed["audiomuse_url"], "http://muse.local")
        self.assertNotIn("lastfm_api_key", parsed)
        self.assertNotIn("opensubsonic_url", parsed)

    def test_device_overwrites_env_when_present(self) -> None:
        os.environ["LASTFM_API_KEY"] = "pc-key"
        os.environ["LASTFM_SESSION_KEY"] = "pc-session"
        os.environ["AUDIOMUSE_URL"] = "http://pc"
        with tempfile.TemporaryDirectory() as tmp:
            Path(tmp, "listen_sources.conf").write_text(
                "lastfm_api_key=device-key\n"
                "lastfm_session_key=\n"
                "audiomuse_url=http://device\n"
                "audiomuse_token=tok\n",
                encoding="utf-8",
            )
            applied = walkman_mod.apply_listen_credentials(tmp)
        self.assertEqual(applied["LASTFM_API_KEY"], "device-key")
        self.assertEqual(os.environ["LASTFM_API_KEY"], "device-key")
        self.assertEqual(os.environ["LASTFM_SESSION_KEY"], "pc-session")
        self.assertNotIn("LASTFM_SESSION_KEY", applied)
        self.assertEqual(os.environ["AUDIOMUSE_URL"], "http://device")
        self.assertEqual(os.environ["AUDIOMUSE_TOKEN"], "tok")
        self.assertEqual(os.environ.get("WALKMAN_SYNC_LASTFM_FROM"), "device")
        self.assertEqual(os.environ.get("WALKMAN_SYNC_AUDIOMUSE_FROM"), "device")

    def test_missing_file_leaves_pc_env(self) -> None:
        os.environ["LASTFM_API_KEY"] = "pc-key"
        with tempfile.TemporaryDirectory() as tmp:
            applied = walkman_mod.apply_listen_credentials(tmp)
        self.assertEqual(applied, {})
        self.assertEqual(os.environ["LASTFM_API_KEY"], "pc-key")
        self.assertNotEqual(os.environ.get("WALKMAN_SYNC_LASTFM_FROM"), "device")

    def test_looks_next_to_music_folder(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            music = os.path.join(tmp, "Music")
            os.makedirs(music)
            Path(tmp, "listen_sources.conf").write_text(
                "lastfm_user=from-root\n", encoding="utf-8")
            applied = walkman_mod.apply_listen_credentials(music)
        self.assertEqual(applied.get("LASTFM_USERNAME"), "from-root")
        self.assertEqual(os.environ.get("LASTFM_USERNAME"), "from-root")
