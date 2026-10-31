"""Configuration for the likes sync.

Deliberately separate from `sync.py`'s `AppConfig`: this component is standalone — it runs on its
own (`python -m likesync`), it can run when no music sync is planned, and it must not drag in the
copy planner. Last.fm credentials come from the Walkman's `listen_sources.conf` when present,
otherwise the same `.env` as `sync.py`. Settings only this package has live in `likesync.json`.
"""
from __future__ import annotations

import json
import os
from dataclasses import dataclass, asdict, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
ENV_PATH = REPO_ROOT / ".env"
SETTINGS_PATH = REPO_ROOT / "likesync.json"
STATE_DIR = REPO_ROOT / ".likesync"

# Written by Cinder on the device (player/cinder-ffi/src/lib.rs, liked_export_tsv).
DEVICE_LOVED_TSV = "cinder_loved.tsv"
# Read by Cinder on the next boot; this package writes it.
DEVICE_IMPORT_TSV = "cinder_liked_import.tsv"
# The playlist the device (and anything else that reads the drive) can actually play.
LIKED_PLAYLIST = "Liked Songs.m3u8"


def load_env(path: Path = ENV_PATH) -> None:
    """Same loader shape as sync.py — KEY="value" lines, existing environment wins."""
    if not path.exists():
        return
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        os.environ.setdefault(key.strip(), value.strip().strip('"').strip("'"))


def save_env_value(key: str, value: str, path: Path = ENV_PATH) -> None:
    lines = path.read_text(encoding="utf-8").splitlines() if path.exists() else []
    out: list[str] = []
    written = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith(f"{key}=") or stripped.startswith(f"{key} ="):
            out.append(f'{key}="{value}"')
            written = True
        else:
            out.append(line)
    if not written:
        out.append(f'{key}="{value}"')
    path.write_text("\n".join(out) + "\n", encoding="utf-8")
    os.environ[key] = value


# ───────────────────────────────────────────────────────── drive detection

def detect_walkman_roots(*, use_env: bool = True) -> tuple[str, str]:
    """(internal_root, sd_root) — the *drive* roots, not the Music folders."""
    try:
        from walkman import detect_walkman_roots as shared
        return shared(use_env=use_env)
    except Exception:
        return os.environ.get("LIKESYNC_INTERNAL", ""), os.environ.get("LIKESYNC_SD", "")


# ───────────────────────────────────────────────────────── settings

@dataclass
class LikesConfig:
    source_dir: str = r"\\WillineauVault\Music"
    playlist_dir: str = r"\\WillineauVault\Music"
    internal_root: str = ""
    sd_root: str = ""
    # MusicBee hands its loves over as an exported playlist: point MusicBee at a playlist
    # (an auto-playlist on Love/Rating works well) and tick "auto-export as m3u".
    musicbee_loved_playlist: str = ""
    # Also treat a file tag rating of >= this many stars as a like (0 disables).
    # MusicBee only writes ratings into files when "save ratings to file tags" is on.
    musicbee_rating_threshold: float = 0.0
    # Write the merged list back out as a playlist MusicBee can show.
    musicbee_export_playlist: bool = True
    lastfm_enabled: bool = True
    # Conflict rule when a track was liked in one place and unliked in another since the last
    # run. "liked" keeps it (nothing is ever silently lost); "latest" trusts the newest change.
    conflict: str = "liked"
    dest: str = "auto"
    # Volume serial of the active Walkman; keys the likes device snapshot.
    device_id: str = ""
    # Emit the liked playlist onto the dest drive only (music sync's single destination).
    device_playlist: bool = True
    # Write cinder_liked_import.tsv so the on-device heart matches (needs Cinder >= the
    # liked-import build; harmless on older ones, which simply ignore the file).
    device_import: bool = True
    excluded_artists: list[str] = field(default_factory=list)

    @classmethod
    def load(cls, path: Path = SETTINGS_PATH) -> "LikesConfig":
        config = cls()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
            for key, value in data.items():
                if hasattr(config, key):
                    setattr(config, key, value)
        except (OSError, ValueError):
            pass
        # Path overrides, for running this on a machine that is not the Windows box (tests,
        # WSL) without editing the saved settings.
        for env_key, attribute in (("LIKESYNC_SOURCE", "source_dir"),
                                   ("LIKESYNC_PLAYLISTS", "playlist_dir")):
            override = os.environ.get(env_key, "")
            if override:
                setattr(config, attribute, override)
        try:
            from walkman import resolve_library_path
            config.source_dir = resolve_library_path(config.source_dir)
            config.playlist_dir = resolve_library_path(config.playlist_dir)
        except Exception:
            pass
        internal, sd = detect_walkman_roots(use_env=True)
        try:
            from walkman import (
                KIND_ANDROID, SyncSettings, bind_live_device, detect_walkman_paths,
                infer_device_kind, volume_root,
            )
            from storage import detect_android_mounts, is_remote
            settings = SyncSettings.load()
            android = settings.device_kind == KIND_ANDROID
            music_internal, music_sd = detect_walkman_paths(use_env=False, android=android)
            if not music_internal and not music_sd:
                remote_internal, remote_sd = detect_android_mounts()
                if remote_internal or remote_sd:
                    music_internal, music_sd = remote_internal, remote_sd
            if music_internal or music_sd:
                inferred = infer_device_kind(
                    music_internal if is_remote(music_internal) else (internal or ""),
                    music_sd if is_remote(music_sd) else (sd or ""),
                )
                bind_live_device(settings, music_internal, music_sd, inferred_kind=inferred)
                internal = volume_root(settings.internal_drive) if settings.internal_drive else internal
                sd = volume_root(settings.sd_drive) if settings.sd_drive else sd
            if settings.device_kind == KIND_ANDROID:
                config.device_import = False
            if not config.dest or config.dest == "auto":
                config.dest = settings.dest or config.dest
            config.device_id = settings.active_device
            if not internal and not sd:
                internal = volume_root(settings.internal_drive) if settings.internal_drive else ""
                sd = volume_root(settings.sd_drive) if settings.sd_drive else ""
        except Exception:
            pass
        if internal:
            config.internal_root = internal
        if sd:
            config.sd_root = sd
        elif internal:
            config.sd_root = sd
        try:
            from walkman import apply_listen_credentials
            apply_listen_credentials(config.internal_root, config.sd_root)
        except Exception:
            pass
        return config

    def save(self, path: Path = SETTINGS_PATH) -> None:
        path.write_text(json.dumps(asdict(self), indent=2), encoding="utf-8")

    @property
    def device_roots(self) -> list[str]:
        try:
            from storage import isdir
        except Exception:
            isdir = os.path.isdir
        return [root for root in (self.internal_root, self.sd_root) if root and isdir(root)]

    @property
    def lastfm_ready(self) -> bool:
        return all(os.environ.get(key, "").strip() for key in
                   ("LASTFM_API_KEY", "LASTFM_API_SECRET", "LASTFM_SESSION_KEY"))

    @property
    def device_snapshot_key(self) -> str:
        return f"device:{self.device_id}" if self.device_id else "device"
