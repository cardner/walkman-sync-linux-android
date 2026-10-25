from __future__ import annotations
import os
import datetime
import shutil
import sys
import hashlib
import getpass
import ctypes
import time
import unicodedata
import threading
from collections import defaultdict, deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable
from urllib import error, parse, request
import xml.etree.ElementTree as ET
from ctypes import wintypes

import catalog as catalog_mod
import copyutil
import contentkey as contentkey_mod
import manifest as manifest_mod
import storage as storage_mod
import walkman as walkman_mod


ENV_PATH = walkman_mod.REPO_ROOT / ".env"


def load_local_env(env_path: str | Path | None = None) -> None:
    path = Path(env_path) if env_path is not None else ENV_PATH
    if not path.exists():
        return

    for raw_line in path.read_text(encoding="utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        os.environ.setdefault(key, value)


load_local_env()


def save_env_value(key: str, value: str, env_path: str | Path | None = None) -> None:
    """Write or update a single KEY="value" entry in the .env file."""
    path = Path(env_path) if env_path is not None else ENV_PATH
    lines = path.read_text(encoding="utf-8").splitlines() if path.exists() else []
    updated = False
    new_lines: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped.startswith(f"{key}=") or stripped.startswith(f"{key} ="):
            new_lines.append(f'{key}="{value}"')
            updated = True
        else:
            new_lines.append(line)
    if not updated:
        new_lines.append(f'{key}="{value}"')
    path.write_text("\n".join(new_lines) + "\n", encoding="utf-8")
    # Sync the live environment so the current process also sees the update.
    os.environ[key] = value


iter_windows_drives = walkman_mod.iter_windows_drives
get_volume_label = walkman_mod.get_volume_label
resolve_music_path = walkman_mod.resolve_music_path
detect_walkman_paths = walkman_mod.detect_walkman_paths
describe_mount = walkman_mod.describe_mount

# ================= DEFAULT CONFIGURATION =================
_SETTINGS = walkman_mod.SyncSettings.load()
DEFAULT_SOURCE_DIR = r"\\WillineauVault\Music"
DEFAULT_PLAYLIST_DIR = r"\\WillineauVault\Music"
DEFAULT_INTERNAL_DRIVE, DEFAULT_SD_DRIVE = detect_walkman_paths(use_env=True)
DEFAULT_SOURCE_DIR = walkman_mod.resolve_library_path(
    os.environ.get("SYNC_SOURCE_DIR", _SETTINGS.source_dir or DEFAULT_SOURCE_DIR))
DEFAULT_PLAYLIST_DIR = walkman_mod.resolve_library_path(
    os.environ.get("SYNC_PLAYLIST_DIR", _SETTINGS.playlist_dir or DEFAULT_PLAYLIST_DIR))
DEFAULT_AUTO_DETECT_DRIVES = bool(getattr(_SETTINGS, "auto_detect_drives", True))
if not DEFAULT_AUTO_DETECT_DRIVES:
    if _SETTINGS.internal_drive:
        DEFAULT_INTERNAL_DRIVE = _SETTINGS.internal_drive
    if _SETTINGS.sd_drive:
        DEFAULT_SD_DRIVE = _SETTINGS.sd_drive
    DEFAULT_INTERNAL_DRIVE = os.environ.get("SYNC_INTERNAL_DRIVE", DEFAULT_INTERNAL_DRIVE)
    DEFAULT_SD_DRIVE = os.environ.get("SYNC_SD_DRIVE", DEFAULT_SD_DRIVE)
else:
    # Last-known letters if the Walkman is unplugged this boot.
    if not DEFAULT_INTERNAL_DRIVE and _SETTINGS.internal_drive:
        DEFAULT_INTERNAL_DRIVE = _SETTINGS.internal_drive
    if not DEFAULT_SD_DRIVE and _SETTINGS.sd_drive:
        DEFAULT_SD_DRIVE = _SETTINGS.sd_drive
walkman_mod.apply_listen_credentials(DEFAULT_INTERNAL_DRIVE, DEFAULT_SD_DRIVE)
DEFAULT_LASTFM_API_KEY = os.environ.get("LASTFM_API_KEY", "")
DEFAULT_LASTFM_API_SECRET = os.environ.get("LASTFM_API_SECRET", "")
DEFAULT_LASTFM_USERNAME = os.environ.get("LASTFM_USERNAME", "")
DEFAULT_LASTFM_SESSION_KEY = os.environ.get("LASTFM_SESSION_KEY", "")
DEFAULT_AUDIOMUSE_URL = os.environ.get("AUDIOMUSE_URL", "")
DEFAULT_AUDIOMUSE_TOKEN = os.environ.get("AUDIOMUSE_TOKEN", "")
DEFAULT_DEST = _SETTINGS.dest or walkman_mod.DEST_AUTO
DEFAULT_INTERNAL_MAX_GB = float(_SETTINGS.internal_max_gb or 0)
DEFAULT_INTERNAL_SAFETY_BUFFER_GB = 0
DEFAULT_SD_MAX_GB = float(_SETTINGS.sd_max_gb or 0)
DEFAULT_EXCLUDED_ALBUMS = os.environ.get("SYNC_EXCLUDED_ALBUMS", _SETTINGS.excluded_albums)
DEFAULT_ACTIVE_DEVICE = getattr(_SETTINGS, "active_device", "") or ""
_ACTIVE_PROFILE = _SETTINGS.active_profile() if hasattr(_SETTINGS, "active_profile") else None
DEFAULT_DEVICE_NAME = _ACTIVE_PROFILE.name if _ACTIVE_PROFILE and _ACTIVE_PROFILE.name else ""
LASTFM_API_URL = "https://ws.audioscrobbler.com/2.0/"

# Supported audio extensions
AUDIO_EXT = walkman_mod.AUDIO_EXT
DEFAULT_DEVICE_KIND = getattr(_SETTINGS, "device_kind", walkman_mod.KIND_CINDER) or walkman_mod.KIND_CINDER
PLAYLIST_EXT = (".m3u", ".m3u8")
# Playlists owned by `likesync`, not by this planner. Excluded from the source playlist
# scan and from the stale sweep so likesync can write Liked Songs on dest without us
# deleting it on the next run.
MANAGED_PLAYLISTS = {"liked songs.m3u8", "liked songs.m3u"}
CINDER_PLAYLISTS_DIR = "cinder_playlists"
CINDER_PRESERVE_PREFIXES = ("cinder_", "listen_")
CINDER_PRESERVE_NAMES = {
    "listen_sources.conf",
    "listen_sync_state.json",
    ".scrobbler.log",
    "scrobble.logs",
    manifest_mod.MANIFEST_NAME,
}
ALBUM_SELECTION_PAGE_SIZE = 15
# One writer onto the dest LUN by default. Extra writers on the same flash often lose.
COPY_WORKERS = max(int(os.environ.get("SYNC_COPY_WORKERS", _SETTINGS.copy_workers or 1)), 1)
COPY_SPACE_RESERVE_BYTES = walkman_mod.COPY_SPACE_RESERVE_BYTES
HEADROOM_BYTES = walkman_mod.HEADROOM_BYTES
# exFAT/FAT store modification times at 2-second granularity, so a freshly copied file
# can read back up to ~2s off its source. Allow that slack before calling a file "newer".
MTIME_TOLERANCE_SECONDS = 2
MANIFEST_SAVE_EVERY = 50
# =======================================================


ProgressCallback = Callable[[int, int, str, float | None], None]


@dataclass
class AppConfig:
    source_dir: str = DEFAULT_SOURCE_DIR
    playlist_dir: str = DEFAULT_PLAYLIST_DIR
    internal_drive: str = DEFAULT_INTERNAL_DRIVE
    sd_drive: str = DEFAULT_SD_DRIVE
    dest: str = DEFAULT_DEST
    lastfm_api_key: str = DEFAULT_LASTFM_API_KEY
    lastfm_api_secret: str = DEFAULT_LASTFM_API_SECRET
    lastfm_username: str = DEFAULT_LASTFM_USERNAME
    lastfm_session_key: str = DEFAULT_LASTFM_SESSION_KEY
    audiomuse_url: str = DEFAULT_AUDIOMUSE_URL
    audiomuse_token: str = DEFAULT_AUDIOMUSE_TOKEN
    internal_max_gb: float = DEFAULT_INTERNAL_MAX_GB
    internal_safety_buffer_gb: float = DEFAULT_INTERNAL_SAFETY_BUFFER_GB
    sd_max_gb: float = DEFAULT_SD_MAX_GB
    excluded_albums: str = DEFAULT_EXCLUDED_ALBUMS
    copy_workers: int = COPY_WORKERS
    auto_detect_drives: bool = DEFAULT_AUTO_DETECT_DRIVES
    device_kind: str = DEFAULT_DEVICE_KIND
    active_device: str = DEFAULT_ACTIVE_DEVICE
    device_name: str = DEFAULT_DEVICE_NAME
    new_walkman: str = ""

    @property
    def is_android(self) -> bool:
        return self.device_kind == walkman_mod.KIND_ANDROID

    @property
    def is_cinder(self) -> bool:
        return not self.is_android

    def dest_is_connected(self) -> bool:
        return bool(self.dest_drive) and storage_mod.isdir(self.dest_drive)

    def other_device_names(self) -> list[str]:
        return _SETTINGS.other_device_names()

    def apply_device_profile(self, settings: walkman_mod.SyncSettings | None = None) -> None:
        """Copy the active Walkman profile onto this config's dest/kind/mounts/budgets."""
        settings = settings or _SETTINGS
        profile = settings.active_profile()
        self.active_device = settings.active_device
        if profile is None:
            return
        self.internal_drive = profile.internal_drive
        self.sd_drive = profile.sd_drive
        self.dest = profile.dest
        self.device_kind = profile.device_kind
        self.internal_max_gb = profile.internal_max_gb
        self.sd_max_gb = profile.sd_max_gb
        self.device_name = profile.name

    def refresh_drives(self) -> bool:
        """Replace mount points from a live Walkman scan. Keeps last-known if unplugged."""
        if not self.auto_detect_drives:
            return False
        android = self.is_android
        internal, sd = walkman_mod.detect_walkman_paths(use_env=False, android=android)
        if not internal and not sd:
            remote_internal, remote_sd = storage_mod.detect_android_mounts()
            if remote_internal or remote_sd:
                internal, sd = remote_internal, remote_sd
                android = True
        if not internal and not sd:
            return False
        roots = walkman_mod.detect_walkman_roots(use_env=False)
        inferred = walkman_mod.infer_device_kind(
            internal if storage_mod.is_remote(internal) else roots[0],
            sd if storage_mod.is_remote(sd) else roots[1],
        )
        if android and not inferred:
            inferred = walkman_mod.KIND_ANDROID
        created, changed = walkman_mod.bind_live_device(
            _SETTINGS, internal, sd, inferred_kind=inferred)
        self.apply_device_profile(_SETTINGS)
        if created:
            self.new_walkman = self.device_name
        self.pull_listen_credentials()
        return changed or created

    def pull_listen_credentials(self) -> dict[str, str]:
        """Walkman `listen_sources.conf` wins; empty fields stay on the PC `.env`."""
        applied = walkman_mod.apply_listen_credentials(self.internal_drive, self.sd_drive)
        self.lastfm_api_key = os.environ.get("LASTFM_API_KEY", "") or self.lastfm_api_key
        self.lastfm_api_secret = os.environ.get("LASTFM_API_SECRET", "") or self.lastfm_api_secret
        self.lastfm_username = os.environ.get("LASTFM_USERNAME", "") or self.lastfm_username
        self.lastfm_session_key = os.environ.get("LASTFM_SESSION_KEY", "") or self.lastfm_session_key
        self.audiomuse_url = os.environ.get("AUDIOMUSE_URL", "") or self.audiomuse_url
        self.audiomuse_token = os.environ.get("AUDIOMUSE_TOKEN", "") or self.audiomuse_token
        return applied

    @property
    def dest_drive(self) -> str:
        return walkman_mod.pick_dest(self.dest, self.internal_drive, self.sd_drive)

    @property
    def other_drive(self) -> str:
        dest = self.dest_drive
        return self.sd_drive if os.path.normcase(dest) == os.path.normcase(self.internal_drive) else self.internal_drive

    def dest_is_internal(self) -> bool:
        return os.path.normcase(self.dest_drive) == os.path.normcase(self.internal_drive)

    @property
    def internal_max_bytes(self) -> int:
        return self._cap_bytes(self.internal_drive, self.internal_max_gb)

    @property
    def internal_music_budget_bytes(self) -> int:
        if self.internal_max_gb > 0:
            usable_gb = max(self.internal_max_gb - self.internal_safety_buffer_gb, 0)
            return int(usable_gb * 1024 * 1024 * 1024)
        return walkman_mod.live_budget_bytes(self.internal_drive)

    @property
    def sd_max_bytes(self) -> int:
        return self._cap_bytes(self.sd_drive, self.sd_max_gb)

    def dest_budget_bytes(self, held_bytes: int = 0) -> int:
        if self.dest_is_internal():
            if self.internal_max_gb > 0:
                return self.internal_music_budget_bytes
        elif self.sd_max_gb > 0:
            return self.sd_max_bytes
        return walkman_mod.live_budget_bytes(self.dest_drive, held_bytes)

    def _cap_bytes(self, path: str, max_gb: float) -> int:
        if max_gb > 0:
            return int(max_gb * 1024 * 1024 * 1024)
        total = walkman_mod.live_total_bytes(path)
        return total if total else 0

    @property
    def excluded_album_entries(self) -> list[str]:
        return parse_album_exclusion_entries(self.excluded_albums)

    def persist(self) -> None:
        source_dir = self.source_dir
        playlist_dir = self.playlist_dir
        saved_source = _SETTINGS.source_dir
        saved_playlists = _SETTINGS.playlist_dir
        if saved_source and os.path.normcase(walkman_mod.resolve_library_path(saved_source)) == os.path.normcase(source_dir):
            source_dir = saved_source
        if saved_playlists and os.path.normcase(walkman_mod.resolve_library_path(saved_playlists)) == os.path.normcase(playlist_dir):
            playlist_dir = saved_playlists
        settings = _SETTINGS
        settings.source_dir = source_dir
        settings.playlist_dir = playlist_dir
        settings.excluded_albums = self.excluded_albums
        settings.copy_workers = self.copy_workers
        settings.auto_detect_drives = self.auto_detect_drives
        if self.active_device and self.active_device in settings.devices:
            settings.active_device = self.active_device
        profile = settings.ensure_active_profile()
        profile.name = self.device_name or profile.name
        profile.internal_drive = self.internal_drive
        profile.sd_drive = self.sd_drive
        profile.dest = self.dest if self.dest in (
            walkman_mod.DEST_INTERNAL, walkman_mod.DEST_SD, walkman_mod.DEST_AUTO
        ) else walkman_mod.DEST_AUTO
        profile.device_kind = self.device_kind if self.device_kind in walkman_mod.DEVICE_KINDS \
            else walkman_mod.KIND_CINDER
        profile.internal_max_gb = self.internal_max_gb
        profile.sd_max_gb = self.sd_max_gb
        if self.internal_drive:
            serial = walkman_mod.get_volume_serial(self.internal_drive)
            if serial:
                profile.internal_serial = serial
        if self.sd_drive:
            serial = walkman_mod.get_volume_serial(self.sd_drive)
            if serial:
                profile.sd_serial = serial
        if profile.primary_serial and settings.active_device == walkman_mod.PENDING_DEVICE_ID:
            settings.devices[profile.primary_serial] = settings.devices.pop(
                walkman_mod.PENDING_DEVICE_ID)
            settings.active_device = profile.primary_serial
            self.active_device = profile.primary_serial
        settings.save()
        try:
            save_env_value("SYNC_EXCLUDED_ALBUMS", self.excluded_albums)
            save_env_value("SYNC_COPY_WORKERS", str(self.copy_workers))
        except OSError:
            pass


@dataclass
class SyncResult:
    files_copied: int = 0
    playlists_written: int = 0
    files_deleted: int = 0
    playlists_deleted: int = 0
    scrobbles_uploaded: int = 0
    scrobbles_ignored: int = 0
    scrobbles_rejected: int = 0
    scrobble_logs_cleared: int = 0
    artists_internal: int = 0
    artists_sd: int = 0
    artists_excluded: int = 0
    artists_skipped: int = 0
    skipped_entries: int = 0
    logs: list[str] = field(default_factory=list)

    def log(self, message: str) -> None:
        self.logs.append(message)


@dataclass
class FileEntry:
    """One file discovered during a scandir walk, path relative to the scan base."""
    rel_path: str
    size: int
    mtime: float
    content_key: str = ""
    artist: str = ""
    title: str = ""
    album: str = ""
    track_no: int = 0


@dataclass
class DeviceScan:
    """Single-pass snapshot of a destination drive (built once, reused everywhere)."""
    root: str
    artist_files: dict[str, list[str]] = field(default_factory=dict)   # artist folder -> [rel_path]
    files: dict[str, tuple[int, float]] = field(default_factory=dict)  # original rel -> (size, mtime)
    by_case: dict[str, str] = field(default_factory=dict)              # normcase rel -> original rel
    playlists: set[str] = field(default_factory=set)
    partials: list[str] = field(default_factory=list)
    content_keys: dict[str, str] = field(default_factory=dict)  # normcase rel -> key

    def add_file(self, rel: str, size: int, mtime: float, content_key: str = "") -> None:
        self.files[rel] = (size, mtime)
        self.by_case[os.path.normcase(rel)] = rel
        if content_key:
            self.content_keys[os.path.normcase(rel)] = content_key

    def rel_for_key(self, key: str) -> str | None:
        if not key:
            return None
        for folded, stored in self.content_keys.items():
            if stored == key:
                return self.by_case.get(folded, folded)
        return None

    def lookup(self, rel: str) -> tuple[int, float] | None:
        original = self.by_case.get(os.path.normcase(rel))
        if original is None:
            return None
        return self.files.get(original)

    def original_rel(self, rel: str) -> str | None:
        return self.by_case.get(os.path.normcase(rel))

    @property
    def artist_folders(self) -> list[str]:
        return sorted(self.artist_files)

    @property
    def total_size(self) -> int:
        return sum(size for size, _ in self.files.values())

    @property
    def audio_paths(self) -> set[str]:
        return {rel for rel in self.files if Path(rel).suffix.lower() in AUDIO_EXT}


@dataclass
class CopyJob:
    src: str
    dst: str
    dest_drive: str
    transfer_bytes: int   # full source size (throughput accounting)
    extra_bytes: int      # additional free space the copy consumes on the device
    rel_path: str = ""
    source_mtime: float = 0.0
    content_key: str = ""
    adopt_from: str = ""  # other-volume path to copy instead of SMB, if set


@dataclass
class LibraryPlan:
    assignments: dict[str, str]
    file_locations: dict[str, str]
    desired_files_by_drive: dict[str, set[str]]
    expected_playlists_by_drive: dict[str, set[str]]
    playlist_names: set[str]
    source_artists: set[str]
    playlist_entries: dict[str, list[str]]
    playlist_drive: dict[str, str]
    folder_sizes: dict[str, int]
    folder_playlists: dict[str, set[str]]
    excluded_folders: set[str]
    source_files: dict[str, list[FileEntry]] = field(default_factory=dict)


@dataclass
class FolderGroup:
    folders: set[str]
    playlist_names: set[str]
    size_bytes: int


@dataclass
class PendingCopyBudget:
    files: int = 0
    bytes_needed: int = 0
    transfer_bytes: int = 0


@dataclass
class DriveSpaceProjection:
    files_to_copy: int = 0
    bytes_to_copy: int = 0
    free_bytes: int | None = None
    reclaimable_bytes: int = 0
    available_after_cleanup: int | None = None
    shortfall_bytes: int = 0
    error: str | None = None


@dataclass
class DuplicateGroup:
    kind: str          # "cross-drive" | "content-key" | "likes-key" | "device-only"
    key: str
    paths: list[str]
    auto_fix: bool


@dataclass
class ScanReport:
    plan: LibraryPlan | None
    result: SyncResult
    duplicate_tracks: list[str]
    stale_music_files: list[str]
    stale_playlists: list[str]
    albums_to_add: int = 0
    albums_to_remove: int = 0
    album_diff: AlbumDiff | None = None
    space_projection: dict[str, DriveSpaceProjection] = field(default_factory=dict)
    device_scans: dict[str, DeviceScan] = field(default_factory=dict)
    source_total_bytes: int = 0
    duplicate_groups: list[DuplicateGroup] = field(default_factory=list)
    catalog_coverage: dict = field(default_factory=dict)


PUNCT_TRANSLATION = str.maketrans({
    "\u2018": "'",
    "\u2019": "'",
    "\u201b": "'",
    "\u2032": "'",
    "\u2010": "-",
    "\u2011": "-",
    "\u2012": "-",
    "\u2013": "-",
    "\u2014": "-",
    "\u2015": "-",
    "\u2212": "-",
    "\u00a0": " ",
})


# ── ANSI colour helpers (Windows VT processing) ──────────────────

def _enable_vt_processing() -> bool:
    """Attempt to enable virtual-terminal processing on Windows."""
    if os.name != "nt":
        return True
    try:
        kernel32 = ctypes.windll.kernel32
        handle = kernel32.GetStdHandle(-11)  # STD_OUTPUT_HANDLE
        mode = wintypes.DWORD()
        kernel32.GetConsoleMode(handle, ctypes.byref(mode))
        kernel32.SetConsoleMode(handle, mode.value | 0x0004)
        return True
    except Exception:
        return False


_COLOURS_OK = _enable_vt_processing()

_ANSI_GREEN = "\033[32m"
_ANSI_RED = "\033[31m"
_ANSI_YELLOW = "\033[33m"
_ANSI_CYAN = "\033[36m"
_ANSI_BOLD = "\033[1m"
_ANSI_DIM = "\033[2m"
_ANSI_RESET = "\033[0m"


def _green(text: str) -> str:
    return f"{_ANSI_GREEN}{text}{_ANSI_RESET}" if _COLOURS_OK else text


def _red(text: str) -> str:
    return f"{_ANSI_RED}{text}{_ANSI_RESET}" if _COLOURS_OK else text


def _yellow(text: str) -> str:
    return f"{_ANSI_YELLOW}{text}{_ANSI_RESET}" if _COLOURS_OK else text


def _bold(text: str) -> str:
    return f"{_ANSI_BOLD}{text}{_ANSI_RESET}" if _COLOURS_OK else text


def _dim(text: str) -> str:
    return f"{_ANSI_DIM}{text}{_ANSI_RESET}" if _COLOURS_OK else text


def _cyan(text: str) -> str:
    return f"{_ANSI_CYAN}{text}{_ANSI_RESET}" if _COLOURS_OK else text


@dataclass
class ScrobbleEntry:
    artist: str
    album: str
    track: str
    track_number: str
    duration: int
    source_flag: str
    timestamp: int
    raw_line: str


@dataclass
class ScrobbleFile:
    device_label: str
    path: Path
    header_lines: list[str]
    entries: list[ScrobbleEntry]


class ProgressTracker:
    def __init__(
        self,
        total: int,
        callback: ProgressCallback | None = None,
        ui: object | None = None,
    ) -> None:
        self.total = max(total, 1)
        self.current = 0
        self.callback = callback
        self.ui = ui
        self.last_step_time: float | None = None
        self.recent_step_seconds: deque[float] = deque(maxlen=6)
        self.copy_phase_active = False
        self.copy_bytes_total = 0
        self.copy_bytes_done = 0
        self.recent_copy_samples: deque[tuple[float, int]] = deque(maxlen=6)
        self.dest_label = ""
        self._rate_window_start = 0.0
        self._rate_window_bytes = 0
        self._lock = threading.Lock()

    def begin_copy_phase(self, total_bytes: int, dest_label: str = "") -> None:
        self.copy_phase_active = True
        self.copy_bytes_total = max(total_bytes, 0)
        self.copy_bytes_done = 0
        self.dest_label = dest_label
        self.recent_step_seconds.clear()
        self.recent_copy_samples.clear()
        self.last_step_time = time.monotonic()
        self._rate_window_start = self.last_step_time
        self._rate_window_bytes = 0
        ui = self.ui
        begin = getattr(ui, "begin_copy", None) if ui is not None else None
        if callable(begin):
            begin(dest_label, self.copy_bytes_total)

    def end_copy_phase(self) -> None:
        self.copy_phase_active = False
        self.copy_bytes_total = 0
        self.copy_bytes_done = 0
        self.recent_copy_samples.clear()
        self.recent_step_seconds.clear()
        self.last_step_time = time.monotonic()
        ui = self.ui
        end = getattr(ui, "end_copy", None) if ui is not None else None
        if callable(end):
            end()

    def estimate_generic_eta(self) -> float | None:
        if len(self.recent_step_seconds) < 3:
            return None
        average_step = sum(self.recent_step_seconds) / len(self.recent_step_seconds)
        remaining_steps = max(self.total - (self.current + 1), 0)
        return average_step * remaining_steps

    def copy_rate(self) -> float | None:
        if len(self.recent_copy_samples) < 2:
            return None
        total_time = sum(sample_time for sample_time, _ in self.recent_copy_samples)
        total_bytes = sum(sample_bytes for _, sample_bytes in self.recent_copy_samples)
        if total_time <= 0 or total_bytes <= 0:
            return None
        return total_bytes / total_time

    def estimate_copy_eta(self) -> float | None:
        rate = self.copy_rate()
        if rate is None:
            return None
        remaining_bytes = max(self.copy_bytes_total - self.copy_bytes_done, 0)
        return remaining_bytes / rate

    def _push_callback(self, message: str) -> None:
        eta_seconds = self.estimate_copy_eta() if self.copy_phase_active else None
        if eta_seconds is None:
            eta_seconds = self.estimate_generic_eta()
        if self.callback:
            current = self.copy_bytes_done if self.copy_phase_active else self.current
            total = self.copy_bytes_total if self.copy_phase_active else self.total
            self.callback(current, max(total, 1), message, eta_seconds)
        ui = self.ui
        report = getattr(ui, "report_copy", None) if ui is not None else None
        if callable(report) and self.copy_phase_active:
            report(self.copy_bytes_done, self.copy_bytes_total, self.copy_rate(), self.dest_label)

    def add_copy_bytes(self, nbytes: int) -> None:
        if nbytes <= 0:
            return
        now = time.monotonic()
        with self._lock:
            self.copy_bytes_done += nbytes
            self._rate_window_bytes += nbytes
            elapsed = now - self._rate_window_start
            if elapsed >= 0.25:
                self.recent_copy_samples.append((elapsed, self._rate_window_bytes))
                self._rate_window_start = now
                self._rate_window_bytes = 0

    def set_worker(self, slot_id: str, label: str, filename: str, done: int, total: int) -> None:
        ui = self.ui
        setter = getattr(ui, "set_worker", None) if ui is not None else None
        if callable(setter):
            setter(slot_id, label, filename, done, total)

    def clear_worker(self, slot_id: str) -> None:
        ui = self.ui
        clearer = getattr(ui, "clear_worker", None) if ui is not None else None
        if callable(clearer):
            clearer(slot_id)

    def step(self, message: str, bytes_processed: int | None = None) -> None:
        now = time.monotonic()
        if self.last_step_time is not None:
            step_duration = now - self.last_step_time
            self.recent_step_seconds.append(step_duration)
            if self.copy_phase_active and bytes_processed and bytes_processed > 0:
                self.recent_copy_samples.append((step_duration, bytes_processed))
                self.copy_bytes_done += bytes_processed
        self.last_step_time = now
        self.current += 1
        self._push_callback(message)


@dataclass
class AlbumDiff:
    """GitHub-style diff of albums between current device state and planned state."""
    additions: list[tuple[str, int, str]]           # (folder, size_bytes, dest_drive)
    deletions: list[tuple[str, int, str]]           # (folder, size_bytes, src_drive)
    moves: list[tuple[str, int, str, str]]          # (folder, size_bytes, from_drive, to_drive)
    unchanged_count: int

    @property
    def net_count(self) -> int:
        return len(self.additions) - len(self.deletions)

    @property
    def net_size(self) -> int:
        return sum(a[1] for a in self.additions) - sum(d[1] for d in self.deletions)

    @property
    def additions_size(self) -> int:
        return sum(a[1] for a in self.additions)

    @property
    def deletions_size(self) -> int:
        return sum(d[1] for d in self.deletions)

    @property
    def moves_size(self) -> int:
        return sum(m[1] for m in self.moves)


def _scan_tree(root: str, base: str) -> list[FileEntry]:
    """Walk *root* with os.scandir (stat is cached on Windows) → entries relative to *base*.

    A single os.scandir pass yields size/mtime without an extra stat() syscall per file,
    which is the whole point: it replaces the per-file Path.stat() storm that made every
    scan crawl over USB flash.
    """
    entries: list[FileEntry] = []
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as iterator:
                for entry in iterator:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            stat_result = entry.stat()
                            rel = os.path.normpath(os.path.relpath(entry.path, base))
                            entries.append(FileEntry(rel, stat_result.st_size, stat_result.st_mtime))
                    except OSError:
                        continue
        except OSError:
            continue
    return entries


def scan_source_folders(source_dir: str, folders: list[str] | set[str]) -> dict[str, list[FileEntry]]:
    """One scandir pass per source artist folder, reused for sizing, planning and copy jobs."""
    return {
        folder: _scan_tree(os.path.join(source_dir, folder), source_dir)
        for folder in folders
    }


def _is_preserved_name(name: str) -> bool:
    lower = name.lower()
    if lower in CINDER_PRESERVE_NAMES or lower in MANAGED_PLAYLISTS:
        return True
    if lower.startswith(CINDER_PRESERVE_PREFIXES):
        return True
    if lower == CINDER_PLAYLISTS_DIR or lower == "cinder_palettes":
        return True
    return False


def _is_partial_name(name: str) -> bool:
    return name.startswith(".") and name.endswith(manifest_mod.PARTIAL_SUFFIX)


def scan_device(drive_root: str) -> DeviceScan:
    """One scandir pass over a destination drive: artist files (+size/mtime) and playlists."""
    scan = DeviceScan(root=drive_root)
    if not os.path.isdir(drive_root):
        return scan
    try:
        with os.scandir(drive_root) as iterator:
            top_entries = list(iterator)
    except OSError:
        return scan

    for entry in top_entries:
        try:
            if entry.is_dir(follow_symlinks=False):
                if _is_preserved_name(entry.name):
                    continue
                folder_entries = _scan_tree(entry.path, drive_root)
                rels: list[str] = []
                for fe in folder_entries:
                    name = os.path.basename(fe.rel_path)
                    if _is_partial_name(name):
                        scan.partials.append(fe.rel_path)
                        continue
                    if name.lower() == manifest_mod.MANIFEST_NAME:
                        continue
                    scan.add_file(fe.rel_path, fe.size, fe.mtime)
                    rels.append(fe.rel_path)
                scan.artist_files[entry.name] = rels
            elif entry.is_file(follow_symlinks=False):
                if _is_partial_name(entry.name):
                    scan.partials.append(entry.name)
                    continue
                if _is_preserved_name(entry.name):
                    continue
                if entry.name.lower().endswith(PLAYLIST_EXT):
                    if entry.name.lower() not in MANAGED_PLAYLISTS:
                        scan.playlists.add(entry.name)
        except OSError:
            continue
    return scan


def dest_kind_for(config: AppConfig, drive: str) -> str:
    if drive and os.path.normcase(drive) == os.path.normcase(config.internal_drive):
        return catalog_mod.KIND_INTERNAL
    return catalog_mod.KIND_SD


def _guard_device_scans(
    config: AppConfig,
    cat: catalog_mod.Catalog,
    device_scans: dict[str, DeviceScan],
    result: SyncResult | None = None,
) -> dict[str, DeviceScan]:
    """Drop scans for volumes that are unplugged or no longer the catalogued disk."""
    guarded: dict[str, DeviceScan] = {}
    for path, kind in (
        (config.internal_drive, catalog_mod.KIND_INTERNAL),
        (config.sd_drive, catalog_mod.KIND_SD),
    ):
        scan = device_scans.get(path) or DeviceScan(root=path)
        if not path:
            guarded[path] = scan
            continue
        ok, reason = cat.confirm_live(kind, path)
        if not ok:
            if result is not None and reason and "not connected" not in reason:
                result.log(reason)
            guarded[path] = DeviceScan(root=path)
            continue
        guarded[path] = scan
    return guarded


def require_live_dest(
    config: AppConfig,
    cat: catalog_mod.Catalog | None,
    result: SyncResult,
) -> bool:
    dest = config.dest_drive
    if not dest:
        result.log("No destination set.")
        return False
    if cat is None:
        if not storage_mod.isdir(dest):
            result.log("Walkman not connected.")
            return False
        return True
    ok, reason = cat.confirm_live(dest_kind_for(config, dest), dest)
    if not ok:
        result.log(reason)
        return False
    return True


def _catalog_progress(status: Callable[[str], None] | None, label: str):
    def progress(folder: str, index: int, total: int) -> None:
        if status:
            status(f"{label}: {folder}  {index}/{total}")
    return progress


def refresh_catalog(
    config: AppConfig,
    status: Callable[[str], None] | None = None,
    *,
    source: bool = True,
    dest: bool = True,
    catalog: catalog_mod.Catalog | None = None,
) -> catalog_mod.Catalog:
    """Incrementally update the PC catalog. Missing dest roots are skipped (Walkman unplugged)."""
    cat = catalog or catalog_mod.Catalog()

    def note(message: str) -> None:
        if status:
            status(message)

    if dest:
        for label, root, kind in (
            ("internal", config.internal_drive, catalog_mod.KIND_INTERNAL),
            ("sd", config.sd_drive, catalog_mod.KIND_SD),
        ):
            ok, reason = cat.confirm_live(kind, root) if root else (False, "")
            if not ok:
                if root and reason:
                    note(f"{label}: {reason}")
                continue
            note(f"Indexing Walkman {label}...")
            cat.refresh_dest(root, kind, _catalog_progress(status, label))
    if source and config.source_dir and os.path.isdir(config.source_dir):
        note("Indexing library...")
        cat.refresh_source(config.source_dir, _catalog_progress(status, "library"))
    return cat


def device_scan_from_catalog(
    cat: catalog_mod.Catalog, kind: str, root: str,
) -> DeviceScan:
    scan = DeviceScan(root=root)
    if not root:
        return scan
    for row in cat.files_for(kind, root):
        name = os.path.basename(row.rel)
        if _is_partial_name(name):
            scan.partials.append(row.rel)
            continue
        parts = Path(row.rel).parts
        if len(parts) == 1:
            lower = name.lower()
            if lower.endswith(PLAYLIST_EXT) and lower not in MANAGED_PLAYLISTS and not _is_preserved_name(name):
                scan.playlists.add(name)
            continue
        scan.add_file(row.rel, row.size, row.mtime, row.content_key)
        scan.artist_files.setdefault(parts[0], []).append(row.rel)
    return scan


def source_files_from_catalog(
    cat: catalog_mod.Catalog, source_dir: str, folders: set[str] | list[str],
) -> dict[str, list[FileEntry]]:
    grouped: dict[str, list[FileEntry]] = {folder: [] for folder in folders}
    fold_map = {os.path.normcase(folder): folder for folder in folders}
    for row in cat.files_for(catalog_mod.KIND_SOURCE, source_dir):
        parts = Path(row.rel).parts
        if not parts:
            continue
        folder = fold_map.get(os.path.normcase(parts[0]))
        if folder is None:
            continue
        grouped[folder].append(FileEntry(
            row.rel, row.size, row.mtime,
            content_key=row.content_key, artist=row.artist, title=row.title,
            album=row.album, track_no=row.track_no,
        ))
    return grouped


def index_library(
    config: AppConfig,
    status: Callable[[str], None] | None = None,
) -> SyncResult:
    result = SyncResult()
    if not config.source_dir or not os.path.isdir(config.source_dir):
        result.log(f"Source directory does not exist: {config.source_dir}")
        return result
    with catalog_mod.Catalog() as cat:
        stats = cat.refresh_source(config.source_dir, _catalog_progress(status, "library"))
    result.log(
        f"Library index: +{stats.added} updated {stats.updated} "
        f"unchanged {stats.unchanged} removed {stats.removed} keyed {stats.keyed}"
    )
    return result


def index_walkman(
    config: AppConfig,
    status: Callable[[str], None] | None = None,
) -> SyncResult:
    result = SyncResult()
    mounted = False
    with catalog_mod.Catalog() as cat:
        for label, root, kind in (
            ("internal", config.internal_drive, catalog_mod.KIND_INTERNAL),
            ("sd", config.sd_drive, catalog_mod.KIND_SD),
        ):
            ok, reason = cat.confirm_live(kind, root) if root else (False, "unplugged")
            if not ok:
                result.log(f"{label}: {reason or 'unplugged'}")
                continue
            mounted = True
            stats = cat.refresh_dest(root, kind, _catalog_progress(status, label))
            result.log(
                f"{label} index: +{stats.added} updated {stats.updated} "
                f"unchanged {stats.unchanged} removed {stats.removed} keyed {stats.keyed}"
            )
    if not mounted:
        result.log("Walkman not connected — plug it in and try again.")
    return result


def get_dir_size(path: str | Path) -> int:
    root = str(path)
    if not os.path.isdir(root):
        return 0
    return sum(entry.size for entry in _scan_tree(root, root))


def human_size(size_bytes: int) -> str:
    size = float(size_bytes)
    units = ["B", "KB", "MB", "GB", "TB"]
    unit = units[0]
    for unit in units:
        if size < 1024 or unit == units[-1]:
            break
        size /= 1024
    return f"{size:.1f} {unit}"


def sanitise_name(value: str) -> str:
    cleaned = "".join(char if char.isalnum() or char in ("-", "_") else "_" for char in value)
    return cleaned.strip("_") or "device"


def count_playlists(path: str | Path) -> int:
    root = Path(path)
    if not root.exists():
        return 0
    return sum(
        1
        for file in root.iterdir()
        if file.is_file() and file.suffix.lower() in PLAYLIST_EXT
    )


def get_device_root(path: str) -> Path:
    root = Path(path)
    return Path(root.anchor) if root.anchor else root


def is_path_within_root(path: str | Path, root: str | Path) -> bool:
    return storage_mod.within(str(path), str(root))


def get_free_bytes(path: str) -> int:
    usage = storage_mod.disk_usage(path)
    if usage is not None:
        return usage[1]
    return walkman_mod.live_free_bytes(path)


def build_scrobble_targets(config: AppConfig) -> list[tuple[str, Path]]:
    targets: list[tuple[str, Path]] = []
    seen: set[str] = set()
    if config.is_android:
        return targets

    for label, music_path in (("internal", config.internal_drive), ("sd", config.sd_drive)):
        music_root = Path(music_path)
        device_root = get_device_root(music_path)
        for candidate in (
            device_root / "scrobble.logs",
            device_root / ".scrobbler.log",
            music_root / "scrobble.logs",
            music_root / ".scrobbler.log",
        ):
            candidate_key = os.path.normcase(str(candidate))
            if candidate_key in seen:
                continue
            seen.add(candidate_key)
            targets.append((label, candidate))

    return targets


def mask_secret(value: str) -> str:
    stripped = value.strip()
    if not stripped:
        return "(not set)"
    if len(stripped) <= 8:
        return "*" * len(stripped)
    return f"{stripped[:4]}...{stripped[-4:]}"


def load_scrobble_file(path: Path, device_label: str, result: SyncResult | None = None) -> ScrobbleFile:
    header_lines: list[str] = []
    entries: list[ScrobbleEntry] = []

    with path.open("r", encoding="utf-8-sig", errors="replace") as handle:
        for line_number, raw_line in enumerate(handle, start=1):
            if raw_line.startswith("#"):
                header_lines.append(raw_line)
                continue

            stripped = raw_line.strip()
            if not stripped:
                continue

            parts = raw_line.rstrip("\r\n").split("\t")
            if len(parts) < 7:
                if result:
                    result.log(f"Skipping malformed scrobble line {line_number} in {path}")
                continue

            try:
                duration = int(parts[4])
                timestamp = int(parts[6])
            except ValueError:
                if result:
                    result.log(f"Skipping invalid scrobble line {line_number} in {path}")
                continue

            entries.append(
                ScrobbleEntry(
                    artist=parts[0].strip(),
                    album=parts[1].strip(),
                    track=parts[2].strip(),
                    track_number=parts[3].strip(),
                    duration=duration,
                    source_flag=parts[5].strip(),
                    timestamp=timestamp,
                    raw_line=raw_line,
                )
            )

    return ScrobbleFile(
        device_label=device_label,
        path=path,
        header_lines=header_lines,
        entries=entries,
    )


def list_scrobble_files(config: AppConfig, result: SyncResult | None = None) -> list[ScrobbleFile]:
    files: list[ScrobbleFile] = []
    for device_label, path in build_scrobble_targets(config):
        if path.is_file():
            files.append(load_scrobble_file(path, device_label, result))
    return files


def count_scrobble_logs(config: AppConfig) -> int:
    return len(list_scrobble_files(config))


def count_pending_scrobbles(config: AppConfig) -> int:
    return sum(
        1
        for scrobble_file in list_scrobble_files(config)
        for entry in scrobble_file.entries
        if entry.source_flag.upper() == "L"
    )


def build_lastfm_api_sig(params: dict[str, str], api_secret: str) -> str:
    signature_base = "".join(f"{key}{params[key]}" for key in sorted(params))
    return hashlib.md5((signature_base + api_secret).encode("utf-8")).hexdigest()


def post_lastfm(params: dict[str, str]) -> ET.Element:
    body = parse.urlencode(params).encode("utf-8")
    req = request.Request(
        LASTFM_API_URL,
        data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded; charset=utf-8"},
        method="POST",
    )
    with request.urlopen(req, timeout=30) as response:
        payload = response.read()
    return ET.fromstring(payload)


def fetch_lastfm_session(config: AppConfig, username: str, password: str) -> tuple[str, str]:
    params = {
        "method": "auth.getMobileSession",
        "username": username,
        "password": password,
        "api_key": config.lastfm_api_key,
    }
    params["api_sig"] = build_lastfm_api_sig(params, config.lastfm_api_secret)

    root = post_lastfm(params)
    if root.attrib.get("status") != "ok":
        error_node = root.find("error")
        if error_node is None:
            raise RuntimeError("Last.fm authentication failed without an error message.")
        raise RuntimeError(
            f"Last.fm error {error_node.attrib.get('code', '?')}: {(error_node.text or '').strip()}"
        )

    session_node = root.find("session")
    if session_node is None:
        raise RuntimeError("Last.fm authentication did not return a session.")

    name_node = session_node.find("name")
    key_node = session_node.find("key")
    if name_node is None or key_node is None or not (name_node.text and key_node.text):
        raise RuntimeError("Last.fm authentication returned an incomplete session.")

    return name_node.text.strip(), key_node.text.strip()


def iter_scrobble_batches(entries: list[ScrobbleEntry], batch_size: int = 50) -> list[list[ScrobbleEntry]]:
    return [entries[index:index + batch_size] for index in range(0, len(entries), batch_size)]


def submit_scrobble_batch(
    entries: list[ScrobbleEntry],
    config: AppConfig,
) -> tuple[int, int, list[tuple[ScrobbleEntry, str]]]:
    params: dict[str, str] = {
        "method": "track.scrobble",
        "api_key": config.lastfm_api_key,
        "sk": config.lastfm_session_key,
    }

    for index, entry in enumerate(entries):
        params[f"artist[{index}]"] = entry.artist
        params[f"track[{index}]"] = entry.track
        params[f"timestamp[{index}]"] = str(entry.timestamp)
        if entry.album:
            params[f"album[{index}]"] = entry.album
        if entry.track_number:
            params[f"trackNumber[{index}]"] = entry.track_number
        if entry.duration > 0:
            params[f"duration[{index}]"] = str(entry.duration)

    params["api_sig"] = build_lastfm_api_sig(params, config.lastfm_api_secret)

    root = post_lastfm(params)
    if root.attrib.get("status") != "ok":
        error_node = root.find("error")
        if error_node is None:
            raise RuntimeError("Last.fm request failed without an error message.")
        raise RuntimeError(
            f"Last.fm error {error_node.attrib.get('code', '?')}: {(error_node.text or '').strip()}"
        )

    scrobbles_node = root.find("scrobbles")
    if scrobbles_node is None:
        raise RuntimeError("Last.fm response did not include scrobble results.")

    accepted = int(scrobbles_node.attrib.get("accepted", "0"))
    ignored = int(scrobbles_node.attrib.get("ignored", "0"))
    scrobble_nodes = scrobbles_node.findall("scrobble")
    if len(scrobble_nodes) != len(entries):
        raise RuntimeError(
            f"Last.fm returned {len(scrobble_nodes)} results for a batch of {len(entries)} scrobbles."
        )

    # Per-scrobble ignoredMessage: code "0" == accepted, anything else == rejected by
    # Last.fm (e.g. timestamp too old, artist/track ignored). Map each rejection back to
    # its source entry so the caller can retry or keep it instead of silently dropping it.
    rejected: list[tuple[ScrobbleEntry, str]] = []
    for entry, node in zip(entries, scrobble_nodes):
        ignored_message = node.find("ignoredMessage")
        if ignored_message is None:
            continue
        code = ignored_message.attrib.get("code", "0")
        if code == "0":
            continue
        reason = (ignored_message.text or "").strip() or "rejected by Last.fm"
        rejected.append((entry, f"{reason} (code {code})"))

    return accepted, ignored, rejected


def rewrite_scrobble_file(path: Path, header_lines: list[str], remaining_entries: list[ScrobbleEntry]) -> None:
    with path.open("w", encoding="utf-8", newline="") as handle:
        for line in header_lines:
            handle.write(line if line.endswith(("\n", "\r")) else line + "\n")
        for entry in remaining_entries:
            handle.write(entry.raw_line if entry.raw_line.endswith(("\n", "\r")) else entry.raw_line + "\n")


def list_artist_folders(path: str) -> list[str]:
    if not os.path.isdir(path):
        return []
    return sorted(
        name for name in os.listdir(path)
        if os.path.isdir(os.path.join(path, name))
        and not _is_preserved_name(name)
    )


def list_playlist_files(path: str) -> list[str]:
    if not os.path.isdir(path):
        return []
    return sorted(
        name for name in os.listdir(path)
        if os.path.isfile(os.path.join(path, name))
        and name.lower().endswith(PLAYLIST_EXT)
        and name.lower() not in MANAGED_PLAYLISTS
    )


def canonical_folder_key(name: str) -> str:
    normalized = unicodedata.normalize("NFKC", name).translate(PUNCT_TRANSLATION)
    return " ".join(normalized.split()).casefold()


def parse_album_exclusion_entries(raw_value: str) -> list[str]:
    entries: list[str] = []
    seen: set[str] = set()
    normalized = raw_value.replace(";", "\n").replace(",", "\n")
    for raw_entry in normalized.splitlines():
        entry = " ".join(raw_entry.strip().split())
        if not entry:
            continue
        entry_key = canonical_folder_key(entry)
        if entry_key in seen:
            continue
        seen.add(entry_key)
        entries.append(entry)
    return entries


def format_album_exclusion_summary(entries: list[str], limit: int = 3) -> str:
    if not entries:
        return "(none)"
    preview = ", ".join(entries[:limit])
    if len(entries) > limit:
        return f"{preview}, ... ({len(entries)} total)"
    return preview


def resolve_excluded_source_folders(
    source_folders: set[str],
    config: AppConfig,
    result: SyncResult | None = None,
) -> set[str]:
    configured_entries = config.excluded_album_entries
    if not configured_entries:
        return set()

    source_lookup = {
        canonical_folder_key(folder_name): folder_name
        for folder_name in source_folders
    }
    excluded_folders: set[str] = set()
    missing_entries: list[str] = []

    for entry in configured_entries:
        matched_folder = source_lookup.get(canonical_folder_key(entry))
        if matched_folder is None:
            missing_entries.append(entry)
            continue
        excluded_folders.add(matched_folder)

    if result is not None:
        if excluded_folders:
            summary = format_album_exclusion_summary(
                sorted(excluded_folders, key=str.lower),
                limit=6,
            )
            result.log(f"Album exclusions active: {summary}")
        if missing_entries:
            summary = format_album_exclusion_summary(
                sorted(missing_entries, key=str.lower),
                limit=6,
            )
            result.log(f"Configured exclusions not found in source: {summary}")

    return excluded_folders


def choose_preferred_folder(source_dir: str, folder_names: list[str]) -> str:
    def folder_score(folder_name: str) -> tuple[int, int, str]:
        folder_path = os.path.join(source_dir, folder_name)
        size = get_dir_size(folder_path)
        file_count = sum(len(files) for _, _, files in os.walk(folder_path))
        return (size, file_count, folder_name)

    return max(folder_names, key=folder_score)


def resolve_source_folders(source_dir: str, result: SyncResult) -> list[str]:
    grouped: dict[str, list[str]] = {}
    for folder_name in list_artist_folders(source_dir):
        grouped.setdefault(canonical_folder_key(folder_name), []).append(folder_name)

    resolved: list[str] = []
    for folder_names in grouped.values():
        preferred = choose_preferred_folder(source_dir, folder_names)
        resolved.append(preferred)
        ignored = sorted(name for name in folder_names if name != preferred)
        if ignored:
            result.log(
                "Source duplicates detected for "
                f"{preferred}; ignoring: {', '.join(ignored)}"
            )
    return sorted(resolved, key=str.lower)


def normalise_playlist_entry(entry: str, music_root: str, playlist_dir: str) -> str | None:
    stripped = entry.strip().strip('"')
    if not stripped:
        return None

    music_root_path = Path(music_root).resolve()
    playlist_dir_path = Path(playlist_dir).resolve()
    candidate = Path(stripped)

    try:
        if candidate.is_absolute():
            resolved = candidate.resolve(strict=False)
        else:
            music_candidate = (music_root_path / candidate).resolve(strict=False)
            if music_candidate.exists():
                resolved = music_candidate
            else:
                resolved = (playlist_dir_path / candidate).resolve(strict=False)

        relative = resolved.relative_to(music_root_path)
    except Exception:
        return None

    return os.path.normpath(str(relative))


def get_playlist_track_paths(playlist_path: str, config: AppConfig) -> list[str]:
    track_paths: list[str] = []
    with open(playlist_path, "r", encoding="utf-8-sig") as handle:
        for raw_line in handle.readlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            path_norm = normalise_playlist_entry(line, config.source_dir, config.playlist_dir)
            if path_norm is not None:
                track_paths.append(path_norm)
    return track_paths


def build_folder_groups(
    folder_sizes: dict[str, int],
    playlist_folders: dict[str, set[str]],
) -> list[FolderGroup]:
    parents = {folder: folder for folder in folder_sizes}

    def find(folder: str) -> str:
        while parents[folder] != folder:
            parents[folder] = parents[parents[folder]]
            folder = parents[folder]
        return folder

    def union(left: str, right: str) -> None:
        left_root = find(left)
        right_root = find(right)
        if left_root != right_root:
            parents[right_root] = left_root

    for folders in playlist_folders.values():
        folders_list = sorted(folders)
        if not folders_list:
            continue
        first = folders_list[0]
        for folder in folders_list[1:]:
            union(first, folder)

    grouped_folders: dict[str, set[str]] = {}
    grouped_playlists: dict[str, set[str]] = {}
    for folder in folder_sizes:
        root = find(folder)
        grouped_folders.setdefault(root, set()).add(folder)

    for playlist_name, folders in playlist_folders.items():
        if not folders:
            continue
        root = find(sorted(folders)[0])
        grouped_playlists.setdefault(root, set()).add(playlist_name)

    groups: list[FolderGroup] = []
    for root, folders in grouped_folders.items():
        size_bytes = sum(folder_sizes[folder] for folder in folders)
        groups.append(
            FolderGroup(
                folders=folders,
                playlist_names=grouped_playlists.get(root, set()),
                size_bytes=size_bytes,
            )
        )

    groups.sort(
        key=lambda group: (
            -len(group.playlist_names),
            -group.size_bytes,
            min(name.lower() for name in group.folders),
        )
    )
    return groups


def dest_held_bytes(config: AppConfig, device_scans: dict[str, DeviceScan] | None) -> int:
    if not device_scans:
        return 0
    scan = device_scans.get(config.dest_drive)
    return scan.total_size if scan is not None else 0


def build_library_plan(
    config: AppConfig,
    result: SyncResult,
    device_scans: dict[str, DeviceScan] | None = None,
    source_files: dict[str, list[FileEntry]] | None = None,
    catalog: catalog_mod.Catalog | None = None,
) -> LibraryPlan | None:
    source_path = Path(config.source_dir)
    playlist_path = Path(config.playlist_dir)

    if not source_path.exists():
        result.log(f"Source directory does not exist: {config.source_dir}")
        return None

    if not playlist_path.exists():
        result.log(f"Playlist directory does not exist: {config.playlist_dir}")
        return None

    source_artists = set(resolve_source_folders(config.source_dir, result))
    excluded_folders = resolve_excluded_source_folders(source_artists, config, result)
    if excluded_folders:
        result.artists_excluded = len(excluded_folders)
        source_artists -= excluded_folders
    if source_files is None and catalog is not None:
        source_files = source_files_from_catalog(catalog, config.source_dir, source_artists)
        if source_artists and not any(source_files.values()):
            source_files = None
    if source_files is None:
        source_files = scan_source_folders(config.source_dir, source_artists)
    else:
        source_files = {folder: list(source_files.get(folder, [])) for folder in source_artists}
    folder_sizes = {
        folder: sum(entry.size for entry in entries)
        for folder, entries in source_files.items()
    }

    # Empty source album folders (failed/incomplete downloads) carry no files, so the
    # copy phase never creates them on the device — yet they would still count as
    # "albums to add" forever. Drop them so the plan reflects only real content.
    empty_folders = {folder for folder, entries in source_files.items() if not entries}
    if empty_folders:
        result.log(
            "Ignoring empty source album folder(s): "
            f"{format_album_exclusion_summary(sorted(empty_folders, key=str.lower), limit=6)}"
        )
        source_artists -= empty_folders
        for folder in empty_folders:
            source_files.pop(folder, None)
            folder_sizes.pop(folder, None)

    playlist_names = set(list_playlist_files(config.playlist_dir))
    playlist_tracks: dict[str, list[str]] = {}
    playlist_folders: dict[str, set[str]] = {}
    folder_playlists = {folder: set() for folder in source_artists}

    for playlist_name in sorted(playlist_names, key=str.lower):
        playlist_pathname = os.path.join(config.playlist_dir, playlist_name)
        tracks = get_playlist_track_paths(playlist_pathname, config)
        playlist_tracks[playlist_name] = tracks
        folders: set[str] = set()
        for track in tracks:
            parts = Path(track).parts
            if parts:
                folders.add(parts[0])
        playlist_folders[playlist_name] = {folder for folder in folders if folder in source_artists}
        for folder in playlist_folders[playlist_name]:
            folder_playlists.setdefault(folder, set()).add(playlist_name)

    groups = build_folder_groups(folder_sizes, playlist_folders)

    dest_drive = config.dest_drive
    dest_label = drive_label(dest_drive, config)
    held = dest_held_bytes(config, device_scans)
    budget = config.dest_budget_bytes(held)
    result.log(f"Destination: {dest_label} ({dest_drive}), budget {human_size(budget)}")

    assignments: dict[str, str] = {}
    file_locations: dict[str, str] = {}
    desired_files_by_drive = {
        config.internal_drive: set(),
        config.sd_drive: set(),
    }
    expected_playlists_by_drive = {
        config.internal_drive: set(),
        config.sd_drive: set(),
    }
    playlist_entries: dict[str, list[str]] = {}
    playlist_drive: dict[str, str] = {}

    used = 0
    for group in groups:
        if used + group.size_bytes > budget:
            result.artists_skipped += len(group.folders)
            result.log(
                "Skipping group "
                f"{', '.join(sorted(group.folders))}: {human_size(group.size_bytes)} exceeds"
                f" remaining {dest_label} capacity."
            )
            continue
        used += group.size_bytes

        for folder in sorted(group.folders, key=str.lower):
            assignments[folder] = dest_drive
            if config.dest_is_internal():
                result.artists_internal += 1
            else:
                result.artists_sd += 1
            result.log(f"{folder} -> {dest_label} ({human_size(folder_sizes[folder])})")

            for entry in source_files.get(folder, []):
                rel_path = entry.rel_path
                desired_files_by_drive[dest_drive].add(rel_path)
                if rel_path.lower().endswith(AUDIO_EXT):
                    file_locations[rel_path] = dest_drive

        for playlist_name in group.playlist_names:
            track_lines = [
                track
                for track in playlist_tracks.get(playlist_name, [])
                if track in file_locations
            ]
            if not track_lines:
                continue
            playlist_entries[playlist_name] = track_lines
            playlist_drive[playlist_name] = dest_drive
            expected_playlists_by_drive[dest_drive].add(playlist_name)

    return LibraryPlan(
        assignments=assignments,
        file_locations=file_locations,
        desired_files_by_drive=desired_files_by_drive,
        expected_playlists_by_drive=expected_playlists_by_drive,
        playlist_names=playlist_names,
        source_artists=source_artists,
        playlist_entries=playlist_entries,
        playlist_drive=playlist_drive,
        folder_sizes=folder_sizes,
        folder_playlists=folder_playlists,
        excluded_folders=excluded_folders,
        source_files=source_files,
    )


def build_copy_jobs(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
    dest_manifest: manifest_mod.Manifest | None = None,
    other_manifest: manifest_mod.Manifest | None = None,
    key_cache: contentkey_mod.KeyCache | None = None,
) -> list[CopyJob]:
    """Decide what to copy using cached source + device metadata — no per-file stat()."""
    dest_drive = config.dest_drive
    other_drive = config.other_drive
    dest_scan = device_scans.get(dest_drive) or DeviceScan(root=dest_drive)
    other_scan = device_scans.get(other_drive) or DeviceScan(root=other_drive)
    dest_manifest = dest_manifest or manifest_mod.Manifest()
    other_manifest = other_manifest or manifest_mod.Manifest()
    key_cache = key_cache or contentkey_mod.KeyCache()
    jobs: list[CopyJob] = []
    for artist_folder, assigned in plan.assignments.items():
        if os.path.normcase(assigned) != os.path.normcase(dest_drive):
            continue
        for entry in plan.source_files.get(artist_folder, []):
            rel = entry.rel_path
            src = os.path.join(config.source_dir, rel)
            suffix = Path(rel).suffix.lower()
            source_key = entry.content_key or None
            if not source_key and suffix in (".flac", ".mp3"):
                source_key = key_cache.resolve(src, entry.size, entry.mtime)
            dest_meta = dest_scan.lookup(rel)
            dest_size = dest_meta[0] if dest_meta else None
            dest_key = dest_scan.content_keys.get(os.path.normcase(rel), "")
            if not dest_key:
                dest_record = dest_manifest.lookup(rel)
                dest_key = dest_record.content_key if dest_record else ""
            if dest_manifest.up_to_date(rel, entry.size, entry.mtime, dest_size, source_key):
                continue
            if dest_meta is not None:
                dst_size, dst_mtime = dest_meta
                if source_key and dest_key == source_key:
                    continue
                if (source_key and dest_manifest.lookup(rel)
                        and dest_manifest.lookup(rel).content_key == source_key):
                    continue
                if entry.size == dst_size and entry.mtime <= dst_mtime + MTIME_TOLERANCE_SECONDS:
                    continue
            extra_bytes = entry.size if dest_size is None else max(entry.size - dest_size, 0)
            adopt_from = ""
            if dest_meta is None and source_key:
                dest_held_rel = dest_scan.rel_for_key(source_key) or dest_manifest.rel_for_key(source_key)
                if dest_held_rel and dest_scan.lookup(dest_held_rel):
                    adopt_from = storage_mod.join(dest_drive, dest_held_rel)
                    extra_bytes = 0
            if not adopt_from:
                other_meta = other_scan.lookup(rel)
                if dest_meta is None and other_meta is not None:
                    other_rel = other_scan.original_rel(rel) or rel
                    other_record = other_manifest.lookup(other_rel)
                    other_key = other_scan.content_keys.get(os.path.normcase(other_rel), "")
                    if not other_key and other_record:
                        other_key = other_record.content_key
                    if (source_key and other_key == source_key) or (
                        other_meta[0] == entry.size
                        and entry.mtime <= other_meta[1] + MTIME_TOLERANCE_SECONDS
                    ):
                        adopt_from = storage_mod.join(other_drive, other_rel)
                elif dest_meta is None and source_key:
                    other_rel = other_scan.rel_for_key(source_key) or other_manifest.rel_for_key(source_key)
                    if other_rel and other_scan.lookup(other_rel):
                        adopt_from = storage_mod.join(other_drive, other_rel)
            jobs.append(
                CopyJob(
                    src=src,
                    dst=storage_mod.join(dest_drive, rel),
                    dest_drive=dest_drive,
                    transfer_bytes=entry.size,
                    extra_bytes=extra_bytes,
                    rel_path=rel,
                    source_mtime=entry.mtime,
                    content_key=source_key or "",
                    adopt_from=adopt_from,
                )
            )
    try:
        key_cache.save()
    except OSError:
        pass
    return jobs


def budgets_from_jobs(config: AppConfig, jobs: list[CopyJob]) -> dict[str, PendingCopyBudget]:
    budgets = {
        config.internal_drive: PendingCopyBudget(),
        config.sd_drive: PendingCopyBudget(),
    }
    for job in jobs:
        budget = budgets.setdefault(job.dest_drive, PendingCopyBudget())
        budget.files += 1
        budget.bytes_needed += job.extra_bytes
        budget.transfer_bytes += job.transfer_bytes
    return budgets


def calculate_pending_copy_budgets(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
) -> dict[str, PendingCopyBudget]:
    return budgets_from_jobs(config, build_copy_jobs(config, plan, device_scans))


def _album_batches(jobs: list[CopyJob]) -> list[list[CopyJob]]:
    grouped: dict[str, list[CopyJob]] = {}
    order: list[str] = []
    for job in jobs:
        folder = Path(job.rel_path or job.src).parts[0] if (job.rel_path or job.src) else ""
        if folder not in grouped:
            order.append(folder)
            grouped[folder] = []
        grouped[folder].append(job)
    return [grouped[name] for name in order]


def _execute_copies(
    jobs: list[CopyJob],
    result: SyncResult,
    dry_run: bool,
    progress: ProgressTracker | None,
    dest_manifest: manifest_mod.Manifest | None = None,
    dest_label: str = "",
    workers: int = 1,
    catalog: catalog_mod.Catalog | None = None,
    dest_kind: str = "",
) -> None:
    """Copy planned files onto the dest LUN only, album-batched, atomic."""
    if not jobs:
        return
    dest_manifest = dest_manifest or manifest_mod.Manifest()
    label = dest_label or "dest"
    worker_count = max(workers, 1)

    def record(job: CopyJob, copied: int | None = None) -> None:
        result.files_copied += 1
        source = "adopt" if job.adopt_from else "copy"
        result.log(f"{source}: {os.path.basename(job.src)}")
        if progress:
            if copied is None:
                progress.step(f"Syncing {os.path.basename(job.src)}",
                              bytes_processed=job.transfer_bytes or None)
            else:
                progress.step(f"Syncing {os.path.basename(job.src)}")

    if dry_run:
        for job in jobs:
            record(job)
        return

    for directory in sorted({storage_mod.parent(job.dst) for job in jobs}):
        if directory:
            storage_mod.makedirs(directory)

    lock = threading.Lock()
    errors: list[tuple[CopyJob, OSError]] = []
    unsaved = 0
    dest_root = jobs[0].dest_drive

    def copy_one(job: CopyJob, slot_id: str) -> None:
        nonlocal unsaved
        src = job.adopt_from or job.src
        filename = os.path.basename(job.src)
        try:
            if progress:
                progress.set_worker(slot_id, label, filename, 0, job.transfer_bytes)

            def on_bytes(chunk: int, done: int, total: int) -> None:
                if progress:
                    progress.add_copy_bytes(chunk)
                    progress.set_worker(slot_id, label, filename, done, total)
                    progress._push_callback(f"{label}  {filename}")

            copied = _transfer_job(job, src, on_bytes)
        except OSError as exc:
            with lock:
                errors.append((job, exc))
            return
        with lock:
            dest_manifest.remember(
                job.rel_path or os.path.basename(job.dst),
                job.transfer_bytes,
                job.source_mtime,
                copied,
                job.content_key,
            )
            if catalog is not None and dest_kind:
                catalog.upsert_dest_file(
                    dest_kind, dest_root,
                    job.rel_path or os.path.basename(job.dst),
                    copied, job.source_mtime, job.content_key,
                )
            if job.adopt_from and is_path_within_root(job.adopt_from, job.dest_drive):
                old_rel = os.path.normpath(os.path.relpath(job.adopt_from, job.dest_drive))
                if os.path.normcase(old_rel) != os.path.normcase(job.rel_path or ""):
                    dest_manifest.forget(old_rel)
                    if catalog is not None and dest_kind:
                        catalog.forget_dest_file(dest_kind, dest_root, old_rel)
            record(job, copied=copied)
            unsaved += 1
            if unsaved >= MANIFEST_SAVE_EVERY:
                try:
                    dest_manifest.save(dest_root)
                except OSError:
                    pass
                unsaved = 0
        if progress:
            progress.clear_worker(slot_id)

    if worker_count == 1:
        for batch in _album_batches(jobs):
            for job in batch:
                copy_one(job, "dest")
                if errors:
                    break
            if errors:
                break
    else:
        # Opt-in extra writers, still dest-LUN only, album batches kept together.
        from concurrent.futures import ThreadPoolExecutor
        with ThreadPoolExecutor(max_workers=worker_count) as pool:
            index = 0
            for batch in _album_batches(jobs):
                futs = []
                for job in batch:
                    slot = f"dest-{index % worker_count}"
                    index += 1
                    futs.append(pool.submit(copy_one, job, slot))
                for fut in futs:
                    fut.result()
                if errors:
                    break

    try:
        dest_manifest.save(dest_root)
    except OSError:
        pass

    if errors:
        job, exc = errors[0]
        if getattr(exc, "winerror", None) == 112:
            raise OSError(
                f"Not enough free space while copying {os.path.basename(job.src)} to {job.dst}. "
                "Stale files are cleaned first, but this device still ran out of space "
                "during the copy phase."
            ) from exc
        raise exc


def _transfer_job(job: CopyJob, src: str, on_bytes) -> int:
    """Rename on the dest LUN when adopting; otherwise atomic copy (SMB or other volume)."""
    if (
        job.adopt_from
        and os.path.normcase(job.adopt_from) != os.path.normcase(job.dst)
        and is_path_within_root(job.adopt_from, job.dest_drive)
        and storage_mod.exists(job.adopt_from)
    ):
        parent_dir = storage_mod.parent(job.dst)
        if parent_dir:
            storage_mod.makedirs(parent_dir)
        storage_mod.replace(job.adopt_from, job.dst)
        size = job.transfer_bytes
        if on_bytes:
            on_bytes(size, size, size)
        return size
    return storage_mod.copy_file(src, job.dst, on_bytes)


def ensure_copy_space_available(
    config: AppConfig,
    plan: LibraryPlan,
    result: SyncResult,
    copy_budgets: dict[str, PendingCopyBudget] | None = None,
    device_scans: dict[str, DeviceScan] | None = None,
) -> None:
    shortages: list[str] = []
    if copy_budgets is None:
        copy_budgets = calculate_pending_copy_budgets(config, plan, device_scans or {})

    dest = config.dest_drive
    for drive, budget in copy_budgets.items():
        if budget.files == 0:
            continue
        if os.path.normcase(drive) != os.path.normcase(dest):
            continue

        free_bytes = get_free_bytes(drive)
        required_bytes = budget.bytes_needed + COPY_SPACE_RESERVE_BYTES
        if required_bytes > free_bytes:
            shortfall = required_bytes - free_bytes
            shortages.append(
                f"{drive_label(drive, config)}: need {human_size(budget.bytes_needed)} for "
                f"{budget.files} pending file(s) plus {human_size(COPY_SPACE_RESERVE_BYTES)} reserve, "
                f"but only {human_size(free_bytes)} is free ({human_size(shortfall)} short)."
            )

    if shortages:
        for message in shortages:
            result.log(message)
        raise OSError("Not enough free space after cleanup to finish the copy phase.\n" + "\n".join(shortages))


def build_space_projection(
    config: AppConfig,
    plan: LibraryPlan,
    stale_music_files: list[str],
    stale_playlists: list[str],
    device_scans: dict[str, DeviceScan],
) -> dict[str, DriveSpaceProjection]:
    projections: dict[str, DriveSpaceProjection] = {}
    budgets = calculate_pending_copy_budgets(config, plan, device_scans)
    stale_paths = stale_music_files + stale_playlists

    # Resolve reclaimable sizes from the cached device scan instead of stat()ing each stale file.
    size_lookup: dict[str, int] = {}
    for drive, scan in device_scans.items():
        for rel, (size, _) in scan.files.items():
            size_lookup[os.path.normcase(os.path.join(drive, rel))] = size

    for drive in (config.internal_drive, config.sd_drive):
        budget = budgets.get(drive, PendingCopyBudget())
        projection = DriveSpaceProjection(
            files_to_copy=budget.files,
            bytes_to_copy=budget.bytes_needed,
        )

        reclaimable_bytes = 0
        for stale_path in stale_paths:
            if not is_path_within_root(stale_path, drive):
                continue
            cached = size_lookup.get(os.path.normcase(stale_path))
            if cached is not None:
                reclaimable_bytes += cached
                continue
            try:
                reclaimable_bytes += os.path.getsize(stale_path)
            except OSError:
                continue
        projection.reclaimable_bytes = reclaimable_bytes

        try:
            projection.free_bytes = get_free_bytes(drive)
            projection.available_after_cleanup = projection.free_bytes + reclaimable_bytes
            # Mirror the hard guard: require the copy plus the safety reserve to fit.
            required = projection.bytes_to_copy
            if required > 0:
                required += COPY_SPACE_RESERVE_BYTES
            if required > projection.available_after_cleanup:
                projection.shortfall_bytes = required - projection.available_after_cleanup
        except OSError as exc:
            projection.error = str(exc)

        projections[drive] = projection

    return projections


def get_drive_music_files(drive_root: str) -> dict[str, list[str]]:
    files_by_artist: dict[str, list[str]] = {}
    if not os.path.isdir(drive_root):
        return files_by_artist

    for artist_folder in list_artist_folders(drive_root):
        artist_root = os.path.join(drive_root, artist_folder)
        rel_files: list[str] = []
        for root, _, files in os.walk(artist_root):
            for file_name in files:
                full_path = os.path.join(root, file_name)
                rel_files.append(os.path.normpath(os.path.relpath(full_path, drive_root)))
        files_by_artist[artist_folder] = rel_files
    return files_by_artist


def get_drive_audio_paths(drive_root: str) -> set[str]:
    audio_paths: set[str] = set()
    for rel_files in get_drive_music_files(drive_root).values():
        for rel_path in rel_files:
            if Path(rel_path).suffix.lower() in AUDIO_EXT:
                audio_paths.add(os.path.normpath(rel_path))
    return audio_paths


def get_drive_playlists(drive_root: str) -> set[str]:
    if not os.path.isdir(drive_root):
        return set()
    return {
        file_name
        for file_name in os.listdir(drive_root)
        if os.path.isfile(os.path.join(drive_root, file_name))
        and file_name.lower().endswith(PLAYLIST_EXT)
    }


def count_albums_to_add(config: AppConfig, plan: LibraryPlan) -> int:
    """Count artist folders assigned to a drive that do not yet exist on that drive."""
    return sum(
        1
        for folder, drive in plan.assignments.items()
        if not os.path.isdir(os.path.join(drive, folder))
    )


def count_albums_to_remove(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
) -> int:
    """Count artist folders present on either drive that the plan will delete."""
    stale: set[str] = set()
    for drive in (config.internal_drive, config.sd_drive):
        scan = device_scans.get(drive)
        if scan is None:
            continue
        for folder in scan.artist_folders:
            if folder in plan.source_artists and folder not in plan.assignments:
                continue
            if folder not in plan.source_artists or plan.assignments.get(folder) != drive:
                stale.add(f"{drive}:{folder}")
    return len(stale)


def build_scan_report(config: AppConfig, status: Callable[[str], None] | None = None) -> ScanReport:
    def note(message: str) -> None:
        if status:
            status(message)

    result = SyncResult()
    cat = refresh_catalog(config, status)
    coverage = cat.coverage(config.internal_drive, config.sd_drive)
    device_scans = _guard_device_scans(config, cat, {
        config.internal_drive: device_scan_from_catalog(
            cat, catalog_mod.KIND_INTERNAL, config.internal_drive),
        config.sd_drive: device_scan_from_catalog(
            cat, catalog_mod.KIND_SD, config.sd_drive),
    }, result)

    note("Planning from catalog...")
    plan = build_library_plan(config, result, device_scans=device_scans, catalog=cat)
    cat.close()
    if plan is None:
        return ScanReport(
            plan=None,
            result=result,
            duplicate_tracks=[],
            stale_music_files=[],
            stale_playlists=[],
            device_scans=device_scans,
            catalog_coverage=coverage,
        )

    note("Computing changes...")
    dest_manifest = manifest_mod.Manifest.load(config.dest_drive)
    other_manifest = manifest_mod.Manifest.load(config.other_drive)
    duplicate_groups = build_duplicate_groups(
        config, plan, device_scans, dest_manifest, other_manifest)
    duplicate_tracks = [
        path for group in duplicate_groups if group.kind == "cross-drive" for path in group.paths
    ]
    stale_music_files = iter_stale_music_files(config, plan, device_scans)
    stale_playlists = iter_stale_playlists(config, plan, device_scans)
    albums_to_add = count_albums_to_add(config, plan)
    albums_to_remove = count_albums_to_remove(config, plan, device_scans)
    album_diff = compute_album_diff(config, plan, device_scans)
    space_projection = build_space_projection(config, plan, stale_music_files, stale_playlists, device_scans)

    return ScanReport(
        plan=plan,
        result=result,
        duplicate_tracks=duplicate_tracks,
        stale_music_files=stale_music_files,
        stale_playlists=stale_playlists,
        albums_to_add=albums_to_add,
        albums_to_remove=albums_to_remove,
        album_diff=album_diff,
        space_projection=space_projection,
        device_scans=device_scans,
        source_total_bytes=sum(plan.folder_sizes.values()),
        duplicate_groups=duplicate_groups,
        catalog_coverage=coverage,
    )


def write_playlist(path: str, lines: list[str], result: SyncResult, dry_run: bool,
                   counted: bool = True) -> None:
    if counted:
        result.playlists_written += 1
        result.log(f"Writing playlist: {path}")
    if dry_run:
        return

    parent = storage_mod.parent(path)
    if parent:
        storage_mod.makedirs(parent)
    storage_mod.write_text(path, "".join(line.replace("\\", "/") + "\n" for line in lines))


def _player_volume_prefix(dest_drive: str, config: AppConfig) -> str:
    if os.path.normcase(dest_drive) == os.path.normcase(config.sd_drive):
        return "/contents_ext"
    return "/contents"


def _cinder_dir(music_path: str) -> str:
    return os.path.join(walkman_mod.volume_root(music_path), CINDER_PLAYLISTS_DIR)


def _cinder_playlist_lines(name: str, tracks: list[str], dest_drive: str, config: AppConfig) -> list[str]:
    """PC-planned list for Cinder: paths as the player sees them, no #SYNC: / #CINDER-EDITED."""
    stem = Path(name).stem
    prefix = _player_volume_prefix(dest_drive, config) + "/MUSIC/"
    return ["#EXTM3U", f"#PLAYLIST:{stem}"] + [
        prefix + track.replace("\\", "/") for track in tracks
    ]


def pull_cinder_playlists(config: AppConfig, result: SyncResult, dry_run: bool) -> None:
    """Copy #CINDER-EDITED lists off the dest volume into playlist_dir/from-walkman/."""
    if not config.is_cinder:
        return
    roots = []
    for drive in (config.dest_drive, config.internal_drive, config.sd_drive):
        root = _cinder_dir(drive)
        if root not in roots and os.path.isdir(root):
            roots.append(root)
    target_dir = os.path.join(config.playlist_dir, "from-walkman")
    for folder in roots:
        try:
            names = os.listdir(folder)
        except OSError:
            continue
        for name in names:
            if not name.lower().endswith(PLAYLIST_EXT):
                continue
            path = os.path.join(folder, name)
            try:
                text = Path(path).read_text(encoding="utf-8-sig", errors="replace")
            except OSError:
                continue
            if "#CINDER-EDITED:" not in text:
                continue
            cleaned = "\n".join(
                line for line in text.splitlines() if not line.startswith("#CINDER-EDITED:")
            ) + "\n"
            dest = os.path.join(target_dir, name)
            result.log(f"Pulling edited playlist {name} -> {dest}")
            if dry_run:
                continue
            os.makedirs(target_dir, exist_ok=True)
            Path(dest).write_text(cleaned, encoding="utf-8", newline="\n")


def process_playlist(
    playlist_name: str,
    plan: LibraryPlan,
    config: AppConfig,
    result: SyncResult,
    dry_run: bool,
    progress: ProgressTracker | None = None,
) -> None:
    track_lines = plan.playlist_entries.get(playlist_name, [])
    dest_drive = plan.playlist_drive.get(playlist_name)

    if dest_drive and track_lines:
        write_playlist(
            storage_mod.join(dest_drive, playlist_name),
            track_lines,
            result,
            dry_run,
        )
        if config.is_cinder:
            cinder_dir = _cinder_dir(dest_drive)
            write_playlist(
                storage_mod.join(
                    cinder_dir,
                    playlist_name if playlist_name.lower().endswith(".m3u8")
                    else playlist_name + ".m3u8",
                ),
                _cinder_playlist_lines(playlist_name, track_lines, dest_drive, config),
                result,
                dry_run,
                counted=False,
            )
    else:
        result.skipped_entries += 1
        result.log(f"Skipping playlist with no assigned tracks: {playlist_name}")

    if progress:
        progress.step(f"Processing playlist {playlist_name}")


def sync_library(
    config: AppConfig,
    dry_run: bool = False,
    progress_callback: ProgressCallback | None = None,
    progress_ui: object | None = None,
    device_scans: dict[str, DeviceScan] | None = None,
    plan: LibraryPlan | None = None,
) -> SyncResult:
    result = SyncResult()
    cat: catalog_mod.Catalog | None = None
    if device_scans is None or plan is None:
        cat = refresh_catalog(config)
        if device_scans is None:
            device_scans = {
                config.internal_drive: device_scan_from_catalog(
                    cat, catalog_mod.KIND_INTERNAL, config.internal_drive),
                config.sd_drive: device_scan_from_catalog(
                    cat, catalog_mod.KIND_SD, config.sd_drive),
            }
        device_scans = _guard_device_scans(config, cat, device_scans, result)
        if not require_live_dest(config, cat, result):
            cat.close()
            return result
        if plan is None:
            plan = build_library_plan(
                config, result, device_scans=device_scans, catalog=cat)
        if plan is None:
            if cat is not None:
                cat.close()
            return result
    elif not dry_run:
        cat = catalog_mod.Catalog()
        device_scans = _guard_device_scans(config, cat, device_scans or {}, result)
        if not require_live_dest(config, cat, result):
            cat.close()
            return result

    dest_manifest = manifest_mod.Manifest.load(config.dest_drive)
    other_manifest = manifest_mod.Manifest.load(config.other_drive)
    key_cache = contentkey_mod.KeyCache()
    copy_jobs = build_copy_jobs(
        config, plan, device_scans, dest_manifest, other_manifest, key_cache)
    copy_budgets = budgets_from_jobs(config, copy_jobs)
    total_copy_bytes = sum(job.transfer_bytes for job in copy_jobs)

    result.log("Scanning library...")
    stale_music_files = iter_stale_music_files(config, plan, device_scans)
    stale_playlists = iter_stale_playlists(config, plan, device_scans)
    other_label = drive_label(config.other_drive, config)
    dest_label = drive_label(config.dest_drive, config)
    other_stale = [
        path for path in stale_music_files
        if is_path_within_root(path, config.other_drive)
    ]
    if other_stale:
        result.log(f"{other_label} → {dest_label}: {len(other_stale)} file(s) will be removed from the unused volume.")
    progress = ProgressTracker(
        len(copy_jobs) + len(plan.playlist_names) + len(stale_music_files) + len(stale_playlists),
        progress_callback,
        ui=progress_ui,
    )

    pull_cinder_playlists(config, result, dry_run)

    dest = config.dest_drive
    adopt_paths = {
        os.path.normcase(job.adopt_from) for job in copy_jobs if job.adopt_from
    }
    dest_stale = [
        path for path in stale_music_files
        if is_path_within_root(path, dest) and os.path.normcase(path) not in adopt_paths
    ]
    leftover_stale = [
        path for path in stale_music_files
        if os.path.normcase(path) not in {os.path.normcase(p) for p in dest_stale}
    ]

    dest_kind = dest_kind_for(config, dest)
    result.log("Reconciling dest volume...")
    for file_path in dest_stale:
        rel = storage_mod.relative(file_path, dest)
        dest_manifest.forget(rel)
        if cat is not None and not dry_run:
            cat.forget_dest_file(dest_kind, dest, rel)
        remove_file(file_path, result, dry_run, playlist=False)
        progress.step(f"Removing {os.path.basename(file_path)}")

    for playlist_path in stale_playlists:
        remove_file(playlist_path, result, dry_run, playlist=True)
        progress.step(f"Removing {os.path.basename(playlist_path)}")

    if not dry_run:
        prune_after_removals(config, plan, dest_stale)
        ensure_copy_space_available(config, plan, result, copy_budgets)
        try:
            dest_manifest.save(config.dest_drive)
        except OSError:
            pass

    result.log("Copying music...")
    progress.begin_copy_phase(total_copy_bytes, dest_label)
    _execute_copies(
        copy_jobs, result, dry_run, progress,
        dest_manifest=dest_manifest,
        dest_label=dest_label,
        workers=config.copy_workers,
        catalog=None if dry_run else cat,
        dest_kind=dest_kind,
    )
    progress.end_copy_phase()

    if leftover_stale:
        result.log(f"{other_label} → {dest_label}: removing leftover files from the unused volume.")
        for file_path in leftover_stale:
            if is_path_within_root(file_path, dest):
                rel = storage_mod.relative(file_path, dest)
                dest_manifest.forget(rel)
                if cat is not None and not dry_run:
                    cat.forget_dest_file(dest_kind, dest, rel)
            elif cat is not None and not dry_run:
                other = config.other_drive
                if is_path_within_root(file_path, other):
                    rel = storage_mod.relative(file_path, other)
                    cat.forget_dest_file(dest_kind_for(config, other), other, rel)
            remove_file(file_path, result, dry_run, playlist=False)
            progress.step(f"Removing {os.path.basename(file_path)}")
        if not dry_run:
            prune_after_removals(config, plan, leftover_stale)
            try:
                dest_manifest.save(config.dest_drive)
            except OSError:
                pass

    result.log("Processing playlists...")
    for file_name in sorted(plan.playlist_names, key=str.lower):
        process_playlist(file_name, plan, config, result, dry_run, progress)

    result.log("Sync complete." if not dry_run else "Preview complete.")
    if cat is not None:
        cat.close()
    return result


def remove_file(path: str, result: SyncResult, dry_run: bool, playlist: bool) -> None:
    if playlist:
        result.playlists_deleted += 1
        result.log(f"Removing playlist: {path}")
    else:
        result.files_deleted += 1
        result.log(f"Removing file: {path}")

    if not dry_run and storage_mod.exists(path):
        storage_mod.remove(path)


def prune_empty_dirs(root_path: str) -> None:
    if not os.path.isdir(root_path):
        return

    for current_root, dirs, files in os.walk(root_path, topdown=False):
        if dirs or files:
            continue
        if os.path.normcase(current_root) != os.path.normcase(root_path):
            try:
                os.rmdir(current_root)
            except OSError:
                pass


def prune_empty_root_artist_dirs(drive_root: str, plan: LibraryPlan) -> None:
    if not os.path.isdir(drive_root):
        return

    for artist_folder in list_artist_folders(drive_root):
        artist_root = os.path.join(drive_root, artist_folder)
        if artist_folder in plan.source_artists and plan.assignments.get(artist_folder) == drive_root:
            continue
        try:
            if not os.listdir(artist_root):
                os.rmdir(artist_root)
        except OSError:
            pass


def prune_after_removals(
    config: AppConfig,
    plan: LibraryPlan,
    stale_music_files: list[str],
) -> None:
    """Drop directories emptied by stale-file removal.

    Only the artist folders that actually lost a file are walked, instead of every
    folder in the library on both drives — pruning stays cheap over USB even though
    most syncs remove nothing. Whole stale artist roots are then cleared per drive.
    """
    drives = (config.internal_drive, config.sd_drive)
    touched: set[str] = set()
    for stale_path in stale_music_files:
        normalized = os.path.normcase(stale_path)
        for drive in drives:
            if is_path_within_root(stale_path, drive):
                rel = storage_mod.relative(stale_path, drive)
                parts = Path(rel).parts
                if parts:
                    touched.add(storage_mod.join(drive, parts[0]))
                break

    for artist_root in touched:
        prune_empty_dirs(artist_root)
    for drive in drives:
        prune_empty_root_artist_dirs(drive, plan)


def iter_stale_music_files(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
) -> list[str]:
    stale_files: list[str] = []
    dest = config.dest_drive
    dest_desired = {os.path.normcase(rel) for rel in plan.desired_files_by_drive.get(dest, set())}
    for drive in (config.internal_drive, config.sd_drive):
        scan = device_scans.get(drive)
        if scan is None:
            continue
        is_dest = os.path.normcase(drive) == os.path.normcase(dest)
        for rel_path in scan.partials:
            stale_files.append(storage_mod.join(drive, rel_path))
        for artist_folder, rel_files in scan.artist_files.items():
            assigned_drive = plan.assignments.get(artist_folder)
            for rel_path in rel_files:
                if _is_preserved_name(os.path.basename(rel_path)):
                    continue
                if is_dest:
                    if assigned_drive is None and artist_folder in plan.source_artists:
                        continue  # skipped-because-full, keep on dest
                    if os.path.normcase(rel_path) in dest_desired:
                        continue
                    stale_files.append(storage_mod.join(drive, rel_path))
                    continue
                # Unused volume: everything is stale (single dest).
                stale_files.append(storage_mod.join(drive, rel_path))
    return stale_files


def iter_stale_playlists(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
) -> list[str]:
    stale_playlists: list[str] = []
    dest = config.dest_drive
    for drive in (config.internal_drive, config.sd_drive):
        scan = device_scans.get(drive)
        if scan is None:
            continue
        expected = plan.expected_playlists_by_drive.get(drive, set())
        is_dest = os.path.normcase(drive) == os.path.normcase(dest)
        for playlist_name in scan.playlists:
            if playlist_name.lower() in MANAGED_PLAYLISTS:
                continue
            if _is_preserved_name(playlist_name):
                continue
            if is_dest and playlist_name in expected:
                continue
            stale_playlists.append(storage_mod.join(drive, playlist_name))
    return stale_playlists


def album_folder(rel: str) -> str:
    """Album directory for a *relative* path (`Artist - Album/...`)."""
    parts = Path(rel).parts
    return parts[0] if parts else ""


def same_album_collisions(rels: list[str]) -> list[list[str]]:
    """Unique rels that share a key, grouped by album; only folders with 2+ files."""
    unique = sorted({os.path.normcase(r): r for r in rels}.values())
    by_album: dict[str, list[str]] = defaultdict(list)
    for rel in unique:
        by_album[album_folder(rel)].append(rel)
    return [paths for paths in by_album.values() if len(paths) > 1]


def build_duplicate_groups(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
    dest_manifest: manifest_mod.Manifest,
    other_manifest: manifest_mod.Manifest,
) -> list[DuplicateGroup]:
    dest = device_scans.get(config.dest_drive) or DeviceScan(root=config.dest_drive)
    other = device_scans.get(config.other_drive) or DeviceScan(root=config.other_drive)
    groups: list[DuplicateGroup] = []

    dest_audio = {os.path.normcase(p): p for p in dest.audio_paths}
    other_audio = {os.path.normcase(p): p for p in other.audio_paths}
    for folded, rel in sorted(dest_audio.items()):
        if folded in other_audio:
            groups.append(DuplicateGroup(
                kind="cross-drive",
                key=rel,
                paths=[
                    storage_mod.join(config.dest_drive, dest.original_rel(rel) or rel),
                    storage_mod.join(config.other_drive, other.original_rel(rel) or other_audio[folded]),
                ],
                auto_fix=True,
            ))

    dest_by_key: dict[str, list[str]] = defaultdict(list)
    for rel, record in dest_manifest.records.items():
        if record.content_key:
            dest_by_key[record.content_key].append(rel)
    for folded, key in dest.content_keys.items():
        if key:
            dest_by_key[key].append(dest.original_rel(folded) or folded)
    for key, rels in dest_by_key.items():
        for album_rels in same_album_collisions(rels):
            groups.append(DuplicateGroup(
                kind="content-key",
                key=key,
                paths=[storage_mod.join(config.dest_drive, rel) for rel in album_rels],
                auto_fix=True,
            ))

    try:
        from likesync.keys import track_key
        from likesync.tags import _from_filename
    except Exception:
        return groups

    by_likes: dict[str, list[str]] = defaultdict(list)
    for folder, entries in plan.source_files.items():
        for entry in entries:
            if Path(entry.rel_path).suffix.lower() not in AUDIO_EXT:
                continue
            artist, title = entry.artist, entry.title
            if not artist or not title:
                names = _from_filename(entry.rel_path)
                artist, title = names.artist, names.title
            if not artist or not title:
                continue
            by_likes[track_key(artist, title)].append(entry.rel_path)
    for key, rels in by_likes.items():
        for album_rels in same_album_collisions(rels):
            groups.append(DuplicateGroup(
                kind="likes-key",
                key=key,
                paths=album_rels,
                auto_fix=False,
            ))

    for rel in dest.audio_paths:
        folder = Path(rel).parts[0] if Path(rel).parts else ""
        if folder in plan.source_artists:
            continue
        tags = _from_filename(rel)
        artist, title = tags.artist, tags.title
        if artist and title:
            groups.append(DuplicateGroup(
                kind="device-only",
                key=track_key(artist, title),
                paths=[storage_mod.join(config.dest_drive, rel)],
                auto_fix=False,
            ))
    return groups


def cleanup_library(
    config: AppConfig,
    dry_run: bool = True,
    progress_callback: ProgressCallback | None = None,
    device_scans: dict[str, DeviceScan] | None = None,
    plan: LibraryPlan | None = None,
) -> SyncResult:
    result = SyncResult()
    cat: catalog_mod.Catalog | None = None
    if device_scans is None or plan is None:
        cat = refresh_catalog(config)
        if device_scans is None:
            device_scans = {
                config.internal_drive: device_scan_from_catalog(
                    cat, catalog_mod.KIND_INTERNAL, config.internal_drive),
                config.sd_drive: device_scan_from_catalog(
                    cat, catalog_mod.KIND_SD, config.sd_drive),
            }
        device_scans = _guard_device_scans(config, cat, device_scans, result)
        if not require_live_dest(config, cat, result):
            cat.close()
            return result
        if plan is None:
            plan = build_library_plan(
                config, result, device_scans=device_scans, catalog=cat)
        if plan is None:
            if cat is not None:
                cat.close()
            return result
    elif not dry_run:
        cat = catalog_mod.Catalog()
        device_scans = _guard_device_scans(config, cat, device_scans or {}, result)
        if not require_live_dest(config, cat, result):
            cat.close()
            return result

    stale_music_files = iter_stale_music_files(config, plan, device_scans)
    stale_playlists = iter_stale_playlists(config, plan, device_scans)
    progress = ProgressTracker(
        max(len(stale_music_files) + len(stale_playlists), 1),
        progress_callback,
    )

    for file_path in stale_music_files:
        if cat is not None and not dry_run:
            for drive in (config.internal_drive, config.sd_drive):
                if is_path_within_root(file_path, drive):
                    rel = storage_mod.relative(file_path, drive)
                    cat.forget_dest_file(dest_kind_for(config, drive), drive, rel)
                    break
        remove_file(file_path, result, dry_run, playlist=False)
        progress.step(f"Cleaning {os.path.basename(file_path)}")

    for playlist_path in stale_playlists:
        remove_file(playlist_path, result, dry_run, playlist=True)
        progress.step(f"Cleaning {os.path.basename(playlist_path)}")

    if not dry_run:
        prune_after_removals(config, plan, stale_music_files)

    result.log("Cleanup complete." if not dry_run else "Cleanup preview complete.")
    if cat is not None:
        cat.close()
    return result


def _submit_with_retries(
    submit_batch: list[ScrobbleEntry],
    config: AppConfig,
    result: SyncResult,
    retries: int,
    retry_delay: float,
) -> tuple[int, list[tuple[ScrobbleEntry, str]]]:
    """Submit a batch, then re-submit Last.fm-rejected entries up to *retries* times.

    Non-listened entries are filtered out before this is called, so every rejection here
    is a genuine Last.fm ignoredMessage. Returns the total accepted count and the entries
    still rejected after the final attempt (for the caller to keep in the log).
    """
    accepted_total, _, rejected = submit_scrobble_batch(submit_batch, config)

    attempt = 0
    while rejected and attempt < retries:
        attempt += 1
        if retry_delay > 0:
            time.sleep(retry_delay)
        retry_entries = [entry for entry, _ in rejected]
        result.log(f"Retry {attempt}/{retries}: re-submitting {len(retry_entries)} rejected scrobble(s).")
        accepted, _, rejected = submit_scrobble_batch(retry_entries, config)
        accepted_total += accepted

    return accepted_total, rejected


def upload_scrobbles(
    config: AppConfig,
    dry_run: bool = True,
    progress_callback: ProgressCallback | None = None,
    retries: int = 0,
    retry_delay: float = 2.0,
) -> SyncResult:
    result = SyncResult()
    scrobble_files = list_scrobble_files(config, result)
    total_entries = sum(len(scrobble_file.entries) for scrobble_file in scrobble_files)
    progress = ProgressTracker(max(total_entries, 1), progress_callback)

    if not scrobble_files:
        result.log("No scrobble log files found on the configured Walkman drives.")
        progress.step("No scrobble logs found")
        result.log("Scrobble upload complete." if not dry_run else "Scrobble upload preview complete.")
        return result

    if total_entries == 0:
        result.log("Scrobble log files were found, but they do not contain any pending plays.")
        progress.step("No pending scrobbles")
        result.log("Scrobble upload complete." if not dry_run else "Scrobble upload preview complete.")
        return result

    if not dry_run:
        missing = [
            name
            for name, value in (
                ("Last.fm API key", config.lastfm_api_key),
                ("Last.fm API secret", config.lastfm_api_secret),
                ("Last.fm session key", config.lastfm_session_key),
            )
            if not value.strip()
        ]
        if missing:
            result.log("Missing Last.fm credentials: " + ", ".join(missing))
            return result

    for scrobble_file in scrobble_files:
        entry_count = len(scrobble_file.entries)
        if entry_count == 0:
            result.log(f"No pending plays in {scrobble_file.path}")
            continue

        result.log(
            f"{scrobble_file.path} ({scrobble_file.device_label}) contains {entry_count} pending scrobble(s)."
        )
        if dry_run:
            for entry in scrobble_file.entries:
                if entry.source_flag.upper() == "L":
                    progress.step(f"Previewing {entry.track[:28]}")
                else:
                    progress.step(f"Ignoring {entry.track[:28]}")
            continue

        processed_count = 0
        stop_processing = False
        kept_entries: list[ScrobbleEntry] = []
        for batch in iter_scrobble_batches(scrobble_file.entries):
            submit_batch = [e for e in batch if e.source_flag.upper() == "L"]
            skipped_in_batch = len(batch) - len(submit_batch)

            try:
                if submit_batch:
                    accepted, rejected = _submit_with_retries(
                        submit_batch, config, result, retries, retry_delay
                    )
                else:
                    accepted, rejected = 0, []
            except (OSError, ET.ParseError, RuntimeError, error.URLError) as exc:
                result.log(f"Last.fm upload failed for {scrobble_file.path}: {exc}")
                stop_processing = True
                break

            result.scrobbles_uploaded += accepted
            result.scrobbles_ignored += skipped_in_batch
            result.scrobbles_rejected += len(rejected)
            processed_count += len(batch)
            # Rejected entries survive (in original order) so a later run can try again
            # instead of losing the play; accepted and non-listened entries are dropped.
            kept_entries.extend(entry for entry, _ in rejected)
            for entry, reason in rejected:
                result.log(f"Rejected: {entry.artist} - {entry.track}: {reason}")
            result.log(
                f"Uploaded batch from {scrobble_file.path}: accepted {accepted}, "
                f"non-listened {skipped_in_batch}, rejected {len(rejected)}."
            )
            for entry in batch:
                if entry.source_flag.upper() == "L":
                    progress.step(f"Uploading {entry.track[:28]}")
                else:
                    progress.step(f"Skipping {entry.track[:28]}")

        remaining_entries = kept_entries + scrobble_file.entries[processed_count:]
        if processed_count > 0:
            rewrite_scrobble_file(scrobble_file.path, scrobble_file.header_lines, remaining_entries)
            if not remaining_entries:
                result.scrobble_logs_cleared += 1
                result.log(f"Cleared uploaded scrobbles from {scrobble_file.path}")
            else:
                details: list[str] = []
                if kept_entries:
                    details.append(f"{len(kept_entries)} rejected")
                unprocessed = len(remaining_entries) - len(kept_entries)
                if unprocessed:
                    details.append(f"{unprocessed} unprocessed")
                suffix = f" ({', '.join(details)})" if details else ""
                result.log(
                    f"Kept {len(remaining_entries)} scrobble(s) in {scrobble_file.path}{suffix}"
                )

        if stop_processing:
            break

    result.log("Scrobble upload complete." if not dry_run else "Scrobble upload preview complete.")
    return result


def clear_screen() -> None:
    os.system("cls" if os.name == "nt" else "clear")


def pause() -> None:
    input("\nPress Enter to continue...")


def prompt_retry_count(default: int = 3) -> int:
    """Ask how many times to re-submit Last.fm-rejected scrobbles (0 disables retries)."""
    raw = input(
        f"Retry Last.fm-rejected scrobbles? Retries [{default}, 0 to disable]: "
    ).strip()
    if not raw:
        return default
    try:
        return max(int(raw), 0)
    except ValueError:
        return default


def prompt_text(label: str, current_value: str, *, secret: bool = False) -> str:
    print(label)
    print(f"Current: {mask_secret(current_value) if secret else current_value}")
    new_value = input("New value (blank to keep current): ").strip()
    return new_value or current_value


def prompt_lastfm_login(config: AppConfig) -> bool:
    clear_screen()
    print("Last.fm Sign-In")
    print("==============")

    if not config.lastfm_api_key.strip() or not config.lastfm_api_secret.strip():
        print("Set your Last.fm API key and API secret first.")
        pause()
        return False

    username = input(
        f"Last.fm username [{config.lastfm_username or 'not set'}]: "
    ).strip() or config.lastfm_username
    if not username:
        print("A Last.fm username is required.")
        pause()
        return False

    password = getpass.getpass("Last.fm password: ").strip()
    if not password:
        print("A Last.fm password is required.")
        pause()
        return False

    try:
        authenticated_username, session_key = fetch_lastfm_session(config, username, password)
    except (OSError, ET.ParseError, RuntimeError, error.URLError) as exc:
        print(f"Last.fm authentication failed: {exc}")
        pause()
        return False

    config.lastfm_username = authenticated_username
    config.lastfm_session_key = session_key

    # Persist credentials to .env so they survive restarts.
    try:
        save_env_value("LASTFM_USERNAME", authenticated_username)
        save_env_value("LASTFM_SESSION_KEY", session_key)
        print(f"Authenticated as {authenticated_username}. Credentials saved to .env")
    except OSError as exc:
        print(f"Authenticated as {authenticated_username}, but could not save to .env: {exc}")

    pause()
    return True


def prompt_path(label: str, current_value: str) -> str:
    return prompt_text(label, current_value, secret=False)


def album_matches_query(album_name: str, query: str) -> bool:
    if not query:
        return True
    normalized_query = canonical_folder_key(query)
    normalized_album = canonical_folder_key(album_name)
    return normalized_query in normalized_album


def list_selectable_album_folders(source_dir: str) -> tuple[list[str], list[str]]:
    if not os.path.isdir(source_dir):
        return [], [f"Source directory does not exist: {source_dir}"]
    probe_result = SyncResult()
    folders = resolve_source_folders(source_dir, probe_result)
    return folders, probe_result.logs


def parse_album_selection_indexes(raw_value: str, max_index: int) -> tuple[list[int], str | None]:
    indexes: list[int] = []
    seen: set[int] = set()
    tokens = raw_value.replace(",", " ").split()
    if not tokens:
        return [], "Enter album numbers, a command, or 'done'."

    for token in tokens:
        if "-" in token:
            left, right = token.split("-", 1)
            if not left.isdigit() or not right.isdigit():
                return [], f"Invalid range: {token}"
            start = int(left)
            end = int(right)
            if start > end:
                start, end = end, start
            if start < 1 or end > max_index:
                return [], f"Selection out of range: {token}"
            for index in range(start, end + 1):
                if index not in seen:
                    seen.add(index)
                    indexes.append(index)
            continue

        if not token.isdigit():
            return [], f"Invalid selection: {token}"
        index = int(token)
        if index < 1 or index > max_index:
            return [], f"Selection out of range: {token}"
        if index not in seen:
            seen.add(index)
            indexes.append(index)

    return indexes, None


def prompt_album_exclusions(current_value: str, source_dir: str) -> str:
    current_entries = parse_album_exclusion_entries(current_value)
    available_albums, discovery_logs = list_selectable_album_folders(source_dir)
    if not available_albums:
        print("Select excluded albums")
        print("======================")
        print("No albums are available to select.")
        for line in discovery_logs[:5]:
            print(line)
        pause()
        return current_value

    available_lookup = {
        canonical_folder_key(album_name): album_name
        for album_name in available_albums
    }
    selected_albums = {
        available_lookup[canonical_folder_key(entry)]
        for entry in current_entries
        if canonical_folder_key(entry) in available_lookup
    }
    missing_entries = sorted(
        [
            entry for entry in current_entries
            if canonical_folder_key(entry) not in available_lookup
        ],
        key=str.lower,
    )
    search_query = ""
    page = 0
    status_message = ""

    while True:
        filtered_albums = [
            album_name for album_name in available_albums
            if album_matches_query(album_name, search_query)
        ]
        total_matches = len(filtered_albums)
        total_pages = max((total_matches - 1) // ALBUM_SELECTION_PAGE_SIZE + 1, 1)
        page = min(page, total_pages - 1)
        page_start = page * ALBUM_SELECTION_PAGE_SIZE
        page_end = page_start + ALBUM_SELECTION_PAGE_SIZE
        visible_albums = filtered_albums[page_start:page_end]

        clear_screen()
        print("Select excluded albums")
        print("======================")
        print(f"Source: {source_dir}")
        print(f"Selected: {len(selected_albums)} of {len(available_albums)} album(s)")
        print(f"Search: {search_query or '(all albums)'}")
        if discovery_logs:
            print(f"Library notes: {len(discovery_logs)} duplicate-name issue(s) were auto-resolved.")
        if missing_entries:
            print(
                "Saved exclusions not found in source: "
                f"{format_album_exclusion_summary(missing_entries, limit=6)}"
            )
        print(
            f"Showing {page_start + 1 if total_matches else 0}-"
            f"{page_start + len(visible_albums)} of {total_matches} match(es)"
            f" | page {page + 1}/{total_pages}"
        )
        print()

        if visible_albums:
            for offset, album_name in enumerate(visible_albums, start=1):
                marker = "x" if album_name in selected_albums else " "
                print(f"{offset:>2}. [{marker}] {album_name}")
        else:
            print("No albums match the current search.")

        print()
        print("Commands: numbers to toggle, 'search <text>' or '/text' to search, '/' to clear search,")
        print("'all' to select matches on this page, 'none' to clear matches on this page,")
        print("'clear' to remove every selection, 'next'/'prev' to change page,")
        print("'done' to save, 'cancel' to keep the current exclusions.")
        if status_message:
            print(f"\n{status_message}")

        command = input("\nSelection: ").strip()
        lowered = command.casefold()

        if lowered in {"done", "save"}:
            return ", ".join(sorted(selected_albums, key=str.lower))
        if lowered in {"cancel", "back", "quit"}:
            return current_value
        if lowered == "":
            status_message = "Enter album numbers, a command, or 'done'."
            continue
        if lowered == "/":
            search_query = ""
            page = 0
            status_message = "Search cleared."
            continue
        if command.startswith("/"):
            search_query = command[1:].strip()
            page = 0
            status_message = (
                f"Showing matches for '{search_query}'."
                if search_query else
                "Search cleared."
            )
            continue
        if lowered.startswith("search "):
            search_query = command[7:].strip()
            page = 0
            status_message = (
                f"Showing matches for '{search_query}'."
                if search_query else
                "Search cleared."
            )
            continue
        if lowered in {"next", "n"}:
            if page + 1 < total_pages:
                page += 1
                status_message = ""
            else:
                status_message = "You are already on the last page."
            continue
        if lowered in {"prev", "p", "previous"}:
            if page > 0:
                page -= 1
                status_message = ""
            else:
                status_message = "You are already on the first page."
            continue
        if lowered == "all":
            if not visible_albums:
                status_message = "There are no visible albums to select."
                continue
            selected_albums.update(visible_albums)
            status_message = f"Selected {len(visible_albums)} album(s) on this page."
            continue
        if lowered == "none":
            if not visible_albums:
                status_message = "There are no visible albums to clear."
                continue
            cleared = sum(1 for album_name in visible_albums if album_name in selected_albums)
            selected_albums.difference_update(visible_albums)
            status_message = f"Cleared {cleared} album(s) on this page."
            continue
        if lowered == "clear":
            if not selected_albums:
                status_message = "No albums are currently selected."
                continue
            selected_albums.clear()
            status_message = "Cleared every excluded album."
            continue

        indexes, error_message = parse_album_selection_indexes(command, len(visible_albums))
        if error_message:
            status_message = error_message
            continue

        toggled = 0
        for index in indexes:
            album_name = visible_albums[index - 1]
            if album_name in selected_albums:
                selected_albums.remove(album_name)
            else:
                selected_albums.add(album_name)
            toggled += 1
        status_message = f"Toggled {toggled} album(s)."


def prompt_float(label: str, current_value: float, *, allow_zero: bool = False) -> float:
    print(label)
    print(f"Current: {current_value}")
    new_value = input("New value (blank to keep current): ").strip()
    if not new_value:
        return current_value

    try:
        value = float(new_value)
    except ValueError:
        print("Please enter a number.")
        pause()
        return current_value

    if value < 0 or (value == 0 and not allow_zero):
        print("Value must be greater than zero." if not allow_zero else "Value cannot be negative.")
        pause()
        return current_value

    return value


def autodetect_storage_paths(config: AppConfig) -> None:
    config.auto_detect_drives = True
    config.refresh_drives()
    _persist_config(config)


def _persist_config(config: AppConfig) -> None:
    try:
        config.persist()
    except OSError:
        pass


def format_eta(seconds: float | None) -> str:
    if seconds is None:
        return "--:--"

    total_seconds = max(int(round(seconds)), 0)
    hours, remainder = divmod(total_seconds, 3600)
    minutes, secs = divmod(remainder, 60)
    if hours:
        return f"{hours:d}:{minutes:02d}:{secs:02d}"
    return f"{minutes:02d}:{secs:02d}"


def render_progress(current: int, total: int, message: str, eta_seconds: float | None = None) -> None:
    width = 32
    ratio = min(max(current / max(total, 1), 0), 1)
    filled = int(width * ratio)
    bar = "#" * filled + "-" * (width - filled)
    eta_text = format_eta(eta_seconds)
    pct = int(ratio * 100)
    # Copy phase reports byte totals; classic is one line: dest, filename, percent.
    if total >= 1024 * 1024:
        safe_message = safe_console_text(message[:44])
        line = (
            f"\r{safe_message:44} {pct:3d}% "
            f"{human_size(current)}/{human_size(total)} ETA {eta_text:>8}"
        )
    else:
        safe_message = safe_console_text(message[:30])
        line = f"\r[{bar}] {current}/{total} {safe_message:30} ETA {eta_text:>8}"
    sys.stdout.write(line)
    sys.stdout.flush()
    if current >= total:
        sys.stdout.write("\n")
        sys.stdout.flush()


def safe_console_text(text: str) -> str:
    encoding = sys.stdout.encoding or "utf-8"
    return text.encode(encoding, errors="replace").decode(encoding, errors="replace")


def safe_print(text: str = "") -> None:
    print(safe_console_text(text))


def run_with_progress(title: str, operation: Callable[[ProgressCallback], SyncResult]) -> SyncResult:
    clear_screen()
    print(title)
    print("=" * len(title))
    result = operation(render_progress)
    if result.logs and not result.logs[-1].endswith("complete."):
        print()
    return result


def drive_label(drive: str, config: AppConfig) -> str:
    if drive and os.path.normcase(drive) == os.path.normcase(config.internal_drive):
        return "Internal"
    if drive and os.path.normcase(drive) == os.path.normcase(config.sd_drive):
        return "SD Card"
    return drive or ""


def catalog_index_line(config: AppConfig, report: ScanReport | None) -> str:
    """Dashboard line: library / Walkman catalog age and key coverage."""
    coverage = getattr(report, "catalog_coverage", None) or {}
    src = coverage.get(catalog_mod.KIND_SOURCE)
    dest_kind = catalog_mod.KIND_INTERNAL if config.dest_is_internal() else catalog_mod.KIND_SD
    dest = coverage.get(dest_kind)
    lib_age = catalog_mod.format_age(src.scanned_at if src else 0)
    wm_age = catalog_mod.format_age(dest.scanned_at if dest else 0)
    lib_cov = f"{src.key_count}/{src.file_count}" if src else "0/0"
    wm_cov = f"{dest.key_count}/{dest.file_count}" if dest else "0/0"
    plugged = "connected" if config.dest_is_connected() else "unplugged"
    return (
        f"Library index {lib_age} ({lib_cov} keys) · "
        f"Walkman index {wm_age} ({wm_cov}) {plugged}"
    )


def overflow_warnings(config: AppConfig, report: ScanReport) -> list[tuple[str, DriveSpaceProjection]]:
    """Drives the current plan would overflow, surfaced as a pre-sync warning."""
    warnings: list[tuple[str, DriveSpaceProjection]] = []
    info = report.space_projection.get(config.dest_drive)
    if info is not None and info.shortfall_bytes > 0:
        warnings.append((drive_label(config.dest_drive, config), info))
    return warnings


def sample_folder_files(source_dir: str, folder_name: str, limit: int = 10) -> list[str]:
    """Return up to *limit* relative file paths from a source artist folder."""
    folder_root = os.path.join(source_dir, folder_name)
    if not os.path.isdir(folder_root):
        return []
    samples: list[str] = []
    for root, _, files in os.walk(folder_root):
        for fname in sorted(files):
            rel = os.path.relpath(os.path.join(root, fname), source_dir)
            samples.append(os.path.normpath(rel))
            if len(samples) >= limit:
                return samples
    return samples


def compute_album_diff(
    config: AppConfig,
    plan: LibraryPlan,
    device_scans: dict[str, DeviceScan],
) -> AlbumDiff:
    """
    Classify every album folder as an addition, deletion, move, or unchanged.

    * **Addition**  – in the plan but not present on *any* device right now.
    * **Deletion**  – present on a device but absent from the plan entirely.
    * **Move**      – present on one device, plan assigns it to a different one.
    * **Unchanged** – already on the device the plan assigns it to.

    Moves are net-neutral: they don't inflate either side of the ledger.
    """
    # Current state: folder name → set of drives it exists on
    current_drives: dict[str, set[str]] = {}
    for drive in (config.internal_drive, config.sd_drive):
        scan = device_scans.get(drive)
        if scan is None:
            continue
        for folder in scan.artist_folders:
            current_drives.setdefault(folder, set()).add(drive)

    planned = plan.assignments  # folder → destination drive

    additions: list[tuple[str, int, str]] = []
    deletions: list[tuple[str, int, str]] = []
    moves: list[tuple[str, int, str, str]] = []
    unchanged = 0

    for folder in sorted(set(current_drives) | set(planned), key=str.lower):
        current = current_drives.get(folder, set())
        planned_drive = planned.get(folder)

        # Resolve size – prefer the plan's pre-computed value, then the cached device scan.
        size = plan.folder_sizes.get(folder, 0)
        if size == 0 and current:
            for d in sorted(current):
                scan = device_scans.get(d)
                if scan is None:
                    continue
                size = sum(scan.files[rel][0] for rel in scan.artist_files.get(folder, []) if rel in scan.files)
                if size:
                    break

        if planned_drive and not current:
            # Brand-new album not on any device.
            additions.append((folder, size, planned_drive))
        elif not planned_drive and current:
            if folder in plan.source_artists:
                # Source album was skipped because it does not currently fit. Sync keeps
                # existing device files for skipped albums, so this is not a deletion.
                unchanged += 1
                continue
            # Album on a device but gone from source library.
            primary = sorted(current)[0]
            deletions.append((folder, size, primary))
        elif planned_drive and current:
            if planned_drive in current:
                # Stays right where it is.
                unchanged += 1
            else:
                # Shuffled from one device to the other.
                from_drive = sorted(current)[0]
                moves.append((folder, size, from_drive, planned_drive))

    return AlbumDiff(
        additions=additions,
        deletions=deletions,
        moves=moves,
        unchanged_count=unchanged,
    )


def view_album_changes(config: AppConfig, report: ScanReport) -> None:
    """GitHub-style diff view: additions (green +), deletions (red -), moves (yellow ~)."""
    clear_screen()
    title = "Album Changes"
    safe_print(_bold(title))
    safe_print("=" * len(title))

    if report.plan is None:
        print("No scan data available. Run a scan first (option 1).")
        pause()
        return

    diff = report.album_diff
    if diff is None:
        diff = compute_album_diff(config, report.plan, report.device_scans)

    net_cnt = diff.net_count
    net_sz = diff.net_size

    # ── Net-change header ─────────────────────────────────────────
    if net_cnt == 0 and net_sz == 0 and not diff.moves:
        safe_print("\n  No album changes. Library is in sync.")
    else:
        cnt_sign = "+" if net_cnt >= 0 else ""
        sz_sign = "+" if net_sz >= 0 else "-"
        sz_display = human_size(abs(net_sz))
        safe_print(
            f"\n  Net change: {_bold(cnt_sign + str(net_cnt))} album(s), "
            f"{_bold(sz_sign + sz_display)}"
        )

    # ── Additions ─────────────────────────────────────────────────
    if diff.additions:
        header = f"+++ Additions ({len(diff.additions)} album(s), {human_size(diff.additions_size)})"
        safe_print(f"\n  {_green(header)}")
        for folder, size, dest in diff.additions:
            label = drive_label(dest, config)
            safe_print(f"  {_green('+')} {folder:<42} {human_size(size):>9}  -> {label}")

    # ── Deletions ─────────────────────────────────────────────────
    if diff.deletions:
        header = f"--- Deletions ({len(diff.deletions)} album(s), {human_size(diff.deletions_size)})"
        safe_print(f"\n  {_red(header)}")
        for folder, size, src in diff.deletions:
            label = drive_label(src, config)
            safe_print(f"  {_red('-')} {folder:<42} {human_size(size):>9}  <- {label}")

    # ── Moves (net-neutral) ───────────────────────────────────────
    if diff.moves:
        header = f"~~~ Moves ({len(diff.moves)} album(s), {human_size(diff.moves_size)} net-neutral)"
        safe_print(f"\n  {_yellow(header)}")
        for folder, size, from_d, to_d in diff.moves:
            from_lbl = drive_label(from_d, config)
            to_lbl = drive_label(to_d, config)
            safe_print(f"  {_yellow('~')} {folder:<42} {from_lbl} -> {to_lbl}")

    # ── Unchanged count ──────────────────────────────────────────
    if diff.unchanged_count:
        safe_print(f"\n  {_dim(str(diff.unchanged_count) + ' album(s) unchanged on their current device(s).')}")

    pause()


def view_scrobble_plan(config: AppConfig) -> None:
    clear_screen()
    print("Scrobble Preview Plan")
    print("=" * 21)

    scrobble_files = list_scrobble_files(config)
    if not scrobble_files:
        print("No pending scrobbles found on any device.")
        pause()
        return

    total_to_submit = 0
    total_to_skip = 0

    for scrobble_file in scrobble_files:
        print(f"\n{scrobble_file.device_label.upper()} LOG: {scrobble_file.path.name}")
        print("-" * 88)
        print(f" {'STATUS':10} | {'TIMESTAMP':16} | {'ARTIST':26} | {'TRACK'}")
        print("-" * 88)

        file_l = 0
        file_s = 0

        for entry in scrobble_file.entries:
            is_listened = entry.source_flag.upper() == "L"
            if is_listened:
                file_l += 1
                status = "UPLOAD"
            else:
                file_s += 1
                status = "SKIP"

            # Only show first 50 to avoid massive scroll, but show summary
            if (file_l + file_s) <= 50:
                try:
                    dt = datetime.datetime.fromtimestamp(entry.timestamp).strftime("%Y-%m-%d %H:%M")
                except Exception:
                    dt = str(entry.timestamp)

                artist = (entry.artist[:24] + "..") if len(entry.artist) > 26 else entry.artist
                track = (entry.track[:34] + "..") if len(entry.track) > 36 else entry.track
                print(f" [{status:8}] | {dt:16} | {artist:26} | {track}")

        if len(scrobble_file.entries) > 50:
            print(f" ... and {len(scrobble_file.entries) - 50} more entries")

        print(f" Sub-total: {file_l} to upload, {file_s} to skip")
        total_to_submit += file_l
        total_to_skip += file_s

    print("\n" + "=" * 88)
    print(" FINAL SUMMARY")
    print(f" Total tracks to submit: {total_to_submit}")
    print(f" Total tracks to skip  : {total_to_skip}")
    print(f" Total entries in logs : {total_to_submit + total_to_skip}")
    print("=" * 88)
    pause()


def _drive_used_size(report: ScanReport, drive: str) -> int:
    scan = report.device_scans.get(drive)
    return scan.total_size if scan is not None else get_dir_size(drive)


def render_dashboard(config: AppConfig, report: ScanReport) -> None:
    source_bytes = report.source_total_bytes or get_dir_size(config.source_dir)
    source_size = human_size(source_bytes)
    internal_size = human_size(_drive_used_size(report, config.internal_drive))
    sd_size = human_size(_drive_used_size(report, config.sd_drive))
    excluded_summary = format_album_exclusion_summary(config.excluded_album_entries)

    # Parse scrobble logs once, derive all three counts from the same pass.
    scrobble_files = list_scrobble_files(config)
    scrobble_log_count = len(scrobble_files)
    pending_scrobbles = sum(
        1 for sf in scrobble_files for e in sf.entries if e.source_flag.upper() == "L"
    )
    skipped_scrobbles = sum(
        1 for sf in scrobble_files for e in sf.entries if e.source_flag.upper() != "L"
    )

    bar = _cyan("=" * 72)
    print(bar)
    print(_bold(_cyan(" Sony Sync Dashboard")))
    print(bar)
    print(f" Source library : {config.source_dir}")
    print(f" Playlist source: {config.playlist_dir}")
    dest_is_internal = os.path.normcase(config.dest_drive) == os.path.normcase(config.internal_drive)
    internal_mark = " dest" if dest_is_internal else " unused"
    sd_mark = " dest" if not dest_is_internal else " unused"
    internal_cap = human_size(config.internal_max_bytes) if config.internal_max_bytes else "live"
    sd_cap = human_size(config.sd_max_bytes) if config.sd_max_bytes else "live"
    print(f" Dest drive     : {drive_label(config.dest_drive, config)} ({config.dest})")
    print(f" Internal music : {config.internal_drive} ({internal_size} used / {internal_cap}){internal_mark}")
    print(f" SD card music  : {config.sd_drive} ({sd_size} used / {sd_cap}){sd_mark}")
    unused_used = _drive_used_size(report, config.other_drive)
    if unused_used:
        print(_yellow(
            f" {drive_label(config.other_drive, config)} leftover will be cleared "
            f"({human_size(unused_used)})"
        ))
    print(f" Source size    : {source_size}")
    print(f" Index          : {catalog_index_line(config, report)}")
    print(f" Excluded albums: {excluded_summary}")
    print(
        f" Internal budget: {human_size(config.internal_music_budget_bytes)}"
        f" after {config.internal_safety_buffer_gb:.1f} GB buffer"
    )
    pending_text = (
        _yellow(f"{pending_scrobbles} pending play(s)") if pending_scrobbles else "0 pending play(s)"
    )
    print(
        f" Last.fm        : {config.lastfm_username or '(not signed in)'} | "
        f"{pending_text} ({skipped_scrobbles} skipped)"
    )
    print(f" Scrobble logs  : {scrobble_log_count} file(s)")
    print(_cyan("-" * 72))

    if report.plan is None:
        print(_red(" Scan status    : configuration error"))
        for line in report.result.logs[-3:]:
            print(f"  {line}")
        print(_cyan("-" * 72))
        return

    playlists_internal = sum(1 for drive in report.plan.playlist_drive.values() if drive == config.internal_drive)
    playlists_sd = sum(1 for drive in report.plan.playlist_drive.values() if drive == config.sd_drive)
    playlists_unplaced = len(report.plan.playlist_names) - len(report.plan.playlist_drive)

    # ── Net album change line (single source of truth for add/remove) ──
    if report.album_diff is not None:
        d = report.album_diff
        nc = d.net_count
        ns = d.net_size
        cnt_str = f"+{nc}" if nc >= 0 else str(nc)
        sz_str = f"+{human_size(abs(ns))}" if ns >= 0 else f"-{human_size(abs(ns))}"
        move_note = f", {len(d.moves)} moved" if d.moves else ""
        net_line = f" Net album change: {cnt_str} album(s), {sz_str}{move_note}"
        print(_green(net_line) if (nc or ns) else _dim(net_line))

    print(_bold(" Scan snapshot"))
    print(f"  Albums to internal : {report.result.artists_internal}")
    print(f"  Albums to SD card  : {report.result.artists_sd}")
    print(f"  Albums excluded    : {report.result.artists_excluded}")
    print(f"  Albums skipped     : {report.result.artists_skipped}")
    print(f"  Albums to add      : {_green(str(report.albums_to_add)) if report.albums_to_add else '0'}")
    print(f"  Albums to remove   : {_red(str(report.albums_to_remove)) if report.albums_to_remove else '0'}")
    print(f"  Playlists internal : {playlists_internal}")
    print(f"  Playlists SD card  : {playlists_sd}")
    print(f"  Playlists unplaced : {playlists_unplaced}")
    print(f"  Duplicate songs    : {len(report.duplicate_tracks)}")
    print(f"  Files to remove    : {len(report.stale_music_files)}")
    print(f"  Playlists to remove: {len(report.stale_playlists)}")
    print(f"  Playlist path misses: {report.result.skipped_entries}")
    if report.space_projection:
        render_space_projection(config, report.space_projection)
    print(_cyan("-" * 72))
    print(_bold(" Library") + "                  " + _bold("Maintenance"))
    print(f" {_cyan('1.')} Refresh scan           {_cyan(' 9.')} Preview cleanup")
    print(f" {_cyan('2.')} View album plan        {_cyan('10.')} Run cleanup")
    print(f" {_cyan('3.')} View playlist plan     {_cyan('11.')} Preview scrobbles")
    print(f" {_cyan('4.')} View album changes     {_cyan('12.')} Upload scrobbles")
    print(f" {_cyan('5.')} View duplicate inbox   {_cyan('13.')} Edit sync settings")
    print(f" {_cyan('6.')} View removals          {_cyan('14.')} Edit Last.fm settings")
    print(f" {_cyan('7.')} Preview sync summary   {_cyan('15.')} Index library")
    print(f" {_cyan('8.')} Run sync               {_cyan('16.')} Index Walkman")
    print(f"                                   {_cyan('17.')} Quit")


def render_result(title: str, result: SyncResult) -> None:
    clear_screen()
    safe_print(title)
    safe_print("=" * len(title))
    safe_print(f"Files copied       : {result.files_copied}")
    safe_print(f"Playlists written  : {result.playlists_written}")
    safe_print(f"Files deleted      : {result.files_deleted}")
    safe_print(f"Playlists deleted  : {result.playlists_deleted}")
    safe_print(f"Scrobbles uploaded : {result.scrobbles_uploaded}")
    safe_print(f"Scrobbles ignored  : {result.scrobbles_ignored}")
    safe_print(f"Scrobbles rejected : {result.scrobbles_rejected}")
    safe_print(f"Logs cleared       : {result.scrobble_logs_cleared}")
    safe_print(f"Folders to internal: {result.artists_internal}")
    safe_print(f"Folders to SD card : {result.artists_sd}")
    safe_print(f"Folders excluded   : {result.artists_excluded}")
    safe_print(f"Folders skipped    : {result.artists_skipped}")
    safe_print(f"Skipped entries    : {result.skipped_entries}")
    safe_print("\nActivity log")
    safe_print("-" * 64)
    if result.logs:
        for line in result.logs:
            safe_print(line)
    else:
        safe_print("No activity recorded.")


def render_space_projection(config: AppConfig, projection: dict[str, DriveSpaceProjection]) -> None:
    print(" Copy space check")
    for drive in (config.internal_drive, config.sd_drive):
        info = projection.get(drive)
        label = drive_label(drive, config)
        if info is None:
            print(f"  {label:<12}: unavailable")
            continue
        if info.error:
            print(f"  {label:<12}: unavailable ({info.error})")
            continue

        available = info.available_after_cleanup if info.available_after_cleanup is not None else 0
        print(
            f"  {label:<12}: need {human_size(info.bytes_to_copy)} for {info.files_to_copy} file(s), "
            f"free now {human_size(info.free_bytes or 0)}, "
            f"free after cleanup {human_size(available)}"
        )
        if info.reclaimable_bytes:
            print(_green(f"               reclaiming about {human_size(info.reclaimable_bytes)} from planned removals"))
        if info.shortfall_bytes:
            print(_red(f"               WARNING short by {human_size(info.shortfall_bytes)}"))


def view_album_plan(config: AppConfig, report: ScanReport) -> None:
    clear_screen()
    print(_bold(_cyan("Album Plan")))
    print(_cyan("=" * 10))
    if report.plan is None:
        print("No scan data available.")
        pause()
        return

    numbered: list[str] = []  # global index (1-based) -> folder
    for drive in (config.internal_drive, config.sd_drive):
        folders = sorted(
            [folder for folder, assigned in report.plan.assignments.items() if assigned == drive],
            key=str.lower,
        )
        total_size = sum(report.plan.folder_sizes[folder] for folder in folders)
        print(f"\n{_bold(drive_label(drive, config))}: {len(folders)} album(s), {human_size(total_size)}")
        for folder in folders[:40]:
            numbered.append(folder)
            playlist_count = len(report.plan.folder_playlists.get(folder, set()))
            print(f"  {_cyan(f'{len(numbered):>3}.')} {folder} [{human_size(report.plan.folder_sizes[folder])}] ({playlist_count} playlist(s))")
        if len(folders) > 40:
            print(f"       ... and {len(folders) - 40} more (not numbered)")

    if report.result.artists_skipped:
        print(f"\nSkipped albums: {report.result.artists_skipped}")

    selection = input("\nEnter an album number to drill in, or press Enter to go back: ").strip()
    if not selection:
        return
    if not selection.isdigit() or not (1 <= int(selection) <= len(numbered)):
        print("Invalid album number.")
        pause()
        return
    folder_name = numbered[int(selection) - 1]

    clear_screen()
    print(_bold(folder_name))
    print("=" * len(folder_name))
    assigned_drive = report.plan.assignments.get(folder_name)
    if assigned_drive is None:
        print("This folder is not currently assigned to a device.")
        pause()
        return

    print(f"Planned storage : {drive_label(assigned_drive, config)}")
    print(f"Folder size     : {human_size(report.plan.folder_sizes.get(folder_name, 0))}")
    playlists = sorted(report.plan.folder_playlists.get(folder_name, set()), key=str.lower)
    print(f"Playlist links  : {len(playlists)}")
    for playlist_name in playlists[:15]:
        print(f"  {playlist_name}")
    if len(playlists) > 15:
        print(f"  ... and {len(playlists) - 15} more")
    print("\nSample files")
    for rel_file in sample_folder_files(config.source_dir, folder_name):
        print(f"  {rel_file}")
    pause()


def view_playlist_plan(config: AppConfig, report: ScanReport) -> None:
    clear_screen()
    print("Playlist Plan")
    print("=" * 13)
    if report.plan is None:
        print("No scan data available.")
        pause()
        return

    for playlist_name in sorted(report.plan.playlist_names, key=str.lower):
        drive = report.plan.playlist_drive.get(playlist_name)
        tracks = report.plan.playlist_entries.get(playlist_name, [])
        folders = sorted({Path(track).parts[0] for track in tracks if Path(track).parts}, key=str.lower)
        label = drive_label(drive, config) if drive else "Unplaced"
        print(f"{playlist_name}")
        print(f"  Device : {label}")
        print(f"  Tracks : {len(tracks)}")
        print(f"  Albums : {len(folders)}")
    pause()


def view_duplicate_inbox(config: AppConfig, report: ScanReport) -> None:
    clear_screen()
    print("Duplicate Inbox")
    print("=" * 15)
    groups = report.duplicate_groups or []
    if not groups:
        print("No duplicate groups.")
        pause()
        return

    print(f"Found {len(groups)} group(s). Auto-fix is path/content-key identity only.\n")
    shown = 0
    for group in groups:
        mark = "auto-fix" if group.auto_fix else "inbox"
        print(f"  [{mark}] {group.kind}  {group.key}")
        for path in group.paths:
            print(f"      {path}")
        shown += 1
        if shown >= 40:
            break
    if len(groups) > 40:
        print(f"\n... and {len(groups) - 40} more")
    pause()


def view_removals(report: ScanReport) -> None:
    clear_screen()
    print("Pending Removals")
    print("=" * 16)
    print(f"Music files : {len(report.stale_music_files)}")
    for path in report.stale_music_files[:80]:
        print(f"  {path}")
    if len(report.stale_music_files) > 80:
        print(f"  ... and {len(report.stale_music_files) - 80} more")
    print(f"\nPlaylists   : {len(report.stale_playlists)}")
    for path in report.stale_playlists[:20]:
        print(f"  {path}")
    if len(report.stale_playlists) > 20:
        print(f"  ... and {len(report.stale_playlists) - 20} more")
    pause()


def render_preview_summary(config: AppConfig, report: ScanReport) -> None:
    clear_screen()
    print("Sync Preview Summary")
    print("=" * 20)
    if report.plan is None:
        print("No scan data available.")
        pause()
        return

    dest_label = drive_label(config.dest_drive, config)
    other_label = drive_label(config.other_drive, config)
    print(f"Destination        : {dest_label} ({config.dest})")
    print(f"Device             : {config.device_kind}")
    print(f"Albums to dest     : {report.result.artists_internal + report.result.artists_sd}")
    print(f"Albums excluded    : {report.result.artists_excluded}")
    print(f"Albums skipped     : {report.result.artists_skipped}")
    print(f"Albums to add      : {report.albums_to_add}")
    print(f"Albums to remove   : {report.albums_to_remove}")
    other_stale = [
        path for path in report.stale_music_files
        if is_path_within_root(path, config.other_drive)
    ]
    if other_stale:
        print(_yellow(
            f"{other_label} → {dest_label}: {len(other_stale)} file(s) will be removed "
            "from the unused volume."
        ))
    print(f"Duplicate groups   : {len(report.duplicate_groups)}")
    print(f"Files to remove    : {len(report.stale_music_files)}")
    print(f"Playlists to remove: {len(report.stale_playlists)}")
    print(f"Playlist path misses: {report.result.skipped_entries}")
    if report.space_projection:
        print()
        render_space_projection(config, report.space_projection)

    # ── Album diff box ────────────────────────────────────────────
    if report.album_diff is not None:
        d = report.album_diff
        nc = d.net_count
        ns = d.net_size
        cnt_str = f"+{nc}" if nc >= 0 else str(nc)
        sz_str = f"+{human_size(abs(ns))}" if ns >= 0 else f"-{human_size(abs(ns))}"
        print(f"\nNet album change: {cnt_str} album(s), {sz_str}")
        if d.additions:
            print(f"  {_green('+')} {len(d.additions)} addition(s)  {human_size(d.additions_size)}")
        if d.deletions:
            print(f"  {_red('-')} {len(d.deletions)} deletion(s)  {human_size(d.deletions_size)}")
        if d.moves:
            print(f"  {_yellow('~')} {len(d.moves)} move(s)       {human_size(d.moves_size)} (net-neutral)")
        if d.unchanged_count:
            print(f"    {d.unchanged_count} unchanged")

    print("\nRecent planner notes")
    for line in report.result.logs[:20]:
        print(f"  {line}")
    pause()


def edit_sync_settings(config: AppConfig) -> None:
    while True:
        clear_screen()
        print("Sync Settings")
        print("=" * 13)
        print("1. Auto-detect Walkman paths (label WALKMAN + the other removable volume)")
        print("2. Edit source library path")
        print("3. Edit playlist source path")
        print("4. Edit internal music path")
        print("5. Edit SD card music path")
        print("6. Edit internal storage limit")
        print("7. Edit internal safety buffer")
        print("8. Edit SD storage limit")
        print("9. Destination drive (internal / sd / auto)")
        print("10. Device profile (cinder / android-walkman)")
        print("11. Copy workers (1 = default)")
        print("12. Select excluded albums")
        print("13. Index library")
        print("14. Index Walkman")
        print("15. Back")
        choice = input("\nSelect an option: ").strip()

        if choice == "1":
            autodetect_storage_paths(config)
            print("\nDetected storage paths:")
            print(f"  Internal: {config.internal_drive}")
            print(f"  SD card : {config.sd_drive}")
            pause()
            continue
        if choice == "2":
            clear_screen()
            config.source_dir = prompt_path("Change source library path", config.source_dir)
            _persist_config(config)
            continue
        if choice == "3":
            clear_screen()
            config.playlist_dir = prompt_path("Change playlist source path", config.playlist_dir)
            _persist_config(config)
            continue
        if choice == "4":
            clear_screen()
            config.internal_drive = prompt_path("Change internal music path", config.internal_drive)
            config.auto_detect_drives = False
            _persist_config(config)
            continue
        if choice == "5":
            clear_screen()
            config.sd_drive = prompt_path("Change SD card music path", config.sd_drive)
            config.auto_detect_drives = False
            _persist_config(config)
            continue
        if choice == "6":
            clear_screen()
            config.internal_max_gb = prompt_float(
                "Change internal storage limit (GB, 0 = live)", config.internal_max_gb, allow_zero=True)
            _persist_config(config)
            continue
        if choice == "7":
            clear_screen()
            config.internal_safety_buffer_gb = prompt_float(
                "Change internal safety buffer (GB)",
                config.internal_safety_buffer_gb,
            )
            _persist_config(config)
            continue
        if choice == "8":
            clear_screen()
            config.sd_max_gb = prompt_float(
                "Change SD storage limit (GB, 0 = live)", config.sd_max_gb, allow_zero=True)
            _persist_config(config)
            continue
        if choice == "9":
            clear_screen()
            print(f"Current dest: {config.dest}  (internal | sd | auto)")
            value = input("New dest (blank to keep): ").strip().lower()
            if value in ("internal", "sd", "auto"):
                config.dest = value
                _persist_config(config)
            pause()
            continue
        if choice == "10":
            clear_screen()
            print(f"Current device: {config.device_kind}  (cinder | android-walkman)")
            value = input("New profile (blank to keep): ").strip().lower().replace(" ", "-")
            if value in ("android", "zx500", "a100"):
                value = walkman_mod.KIND_ANDROID
            if value in walkman_mod.DEVICE_KINDS:
                config.device_kind = value
                _persist_config(config)
            pause()
            continue
        if choice == "11":
            clear_screen()
            print(f"Current copy workers: {config.copy_workers}  (1 = default, dest LUN only)")
            raw = input("New value (blank to keep): ").strip()
            if raw:
                try:
                    config.copy_workers = max(int(raw), 1)
                    _persist_config(config)
                except ValueError:
                    print("Please enter a whole number.")
            pause()
            continue
        if choice == "12":
            clear_screen()
            updated_value = prompt_album_exclusions(config.excluded_albums, config.source_dir)
            if updated_value == config.excluded_albums:
                continue
            config.excluded_albums = updated_value
            _persist_config(config)
            print("Saved excluded albums.")
            pause()
            continue
        if choice == "13":
            result = index_library(config, status=lambda message: safe_print(_dim(f"  {message}")))
            render_result("Library index", result)
            pause()
            continue
        if choice == "14":
            result = index_walkman(config, status=lambda message: safe_print(_dim(f"  {message}")))
            render_result("Walkman index", result)
            pause()
            continue
        if choice == "15":
            return
        print("Invalid selection.")
        pause()


def edit_lastfm_settings(config: AppConfig) -> None:
    while True:
        clear_screen()
        print("Last.fm Settings")
        print("=" * 15)
        print(f"User    : {config.lastfm_username or '(not signed in)'}")
        print(f"API key : {mask_secret(config.lastfm_api_key)}")
        print(f"Secret  : {mask_secret(config.lastfm_api_secret)}")
        print(f"Session : {'ready' if config.lastfm_session_key.strip() else '(not set)'}")
        print("\n1. Edit Last.fm API key")
        print("2. Edit Last.fm API secret")
        print("3. Sign in to Last.fm")
        print("4. Back")
        choice = input("\nSelect an option: ").strip()

        if choice == "1":
            clear_screen()
            config.lastfm_api_key = prompt_text("Change Last.fm API key", config.lastfm_api_key, secret=True)
            continue
        if choice == "2":
            clear_screen()
            config.lastfm_api_secret = prompt_text("Change Last.fm API secret", config.lastfm_api_secret, secret=True)
            continue
        if choice == "3":
            prompt_lastfm_login(config)
            continue
        if choice == "4":
            return
        print("Invalid selection.")
        pause()


def scan_library_report(config: AppConfig) -> ScanReport:
    """Build a scan report while showing live phase feedback (scans block the UI)."""
    clear_screen()
    safe_print(_bold(_cyan("Scanning library")))
    safe_print("")
    return build_scan_report(config, status=lambda message: safe_print(_dim(f"  {message}")))


def run_tui() -> None:
    config = AppConfig()
    report = scan_library_report(config)

    while True:
        clear_screen()
        render_dashboard(config, report)

        choice = input("\nSelect an option: ").strip()

        if choice == "1":
            report = scan_library_report(config)
            continue

        if choice == "2":
            view_album_plan(config, report)
            continue

        if choice == "3":
            view_playlist_plan(config, report)
            continue

        if choice == "4":                                       # ← NEW
            view_album_changes(config, report)
            continue

        if choice == "5":                                       # ← was 4
            view_duplicate_inbox(config, report)
            continue

        if choice == "6":                                       # ← was 5
            view_removals(report)
            continue

        if choice == "7":                                       # ← was 6
            render_preview_summary(config, report)
            continue

        if choice == "8":                                       # ← was 7
            preview = run_with_progress(
                "Sync Preview",
                lambda progress: sync_library(
                    config, dry_run=True, progress_callback=progress,
                    device_scans=report.device_scans, plan=report.plan,
                ),
            )
            render_result("Sync Preview", preview)

            warnings = overflow_warnings(config, report)
            if warnings:
                safe_print("")
                for label, info in warnings:
                    safe_print(_red(
                        f"WARNING: {label} would overflow — short by {human_size(info.shortfall_bytes)} "
                        f"(need {human_size(info.bytes_to_copy)}, only "
                        f"{human_size(info.available_after_cleanup or 0)} free after cleanup)."
                    ))
                safe_print(_red("Running anyway may fail partway and leave the device partially written."))

            confirm = input("\nRun the real sync now? [y/N]: ").strip().lower()
            if confirm != "y":
                report = scan_library_report(config)
                continue

            try:
                result = run_with_progress(
                    "Sync Running",
                    lambda progress: sync_library(
                        config, dry_run=False, progress_callback=progress,
                        device_scans=report.device_scans, plan=report.plan,
                    ),
                )
            except OSError as exc:
                clear_screen()
                safe_print(_red("Sync stopped before finishing: not enough free space."))
                safe_print(str(exc))
                report = scan_library_report(config)
                pause()
                continue
            render_result("Sync Complete", result)
            report = scan_library_report(config)
            pause()
            continue

        if choice == "9":                                       # ← was 8
            result = run_with_progress(
                "Cleanup Preview",
                lambda progress: cleanup_library(
                    config, dry_run=True, progress_callback=progress,
                    device_scans=report.device_scans, plan=report.plan,
                ),
            )
            render_result("Cleanup Preview", result)
            pause()
            continue

        if choice == "10":                                      # ← was 9
            preview = run_with_progress(
                "Cleanup Preview",
                lambda progress: cleanup_library(
                    config, dry_run=True, progress_callback=progress,
                    device_scans=report.device_scans, plan=report.plan,
                ),
            )
            render_result("Cleanup Preview", preview)
            confirm = input("\nDelete the stale files shown above? [y/N]: ").strip().lower()
            if confirm != "y":
                report = scan_library_report(config)
                continue

            result = run_with_progress(
                "Cleanup Running",
                lambda progress: cleanup_library(
                    config, dry_run=False, progress_callback=progress,
                    device_scans=report.device_scans, plan=report.plan,
                ),
            )
            render_result("Cleanup Complete", result)
            report = scan_library_report(config)
            pause()
            continue

        if choice == "11":                                      # ← was 10
            view_scrobble_plan(config)
            continue

        if choice == "12":                                      # ← was 11
            view_scrobble_plan(config)
            confirm = input("\nUpload these plays to Last.fm and clear them from the device? [y/N]: ").strip().lower()
            if confirm != "y":
                continue

            retries = prompt_retry_count()
            result = run_with_progress(
                "Scrobble Upload Running",
                lambda progress: upload_scrobbles(
                    config, dry_run=False, progress_callback=progress, retries=retries
                ),
            )
            render_result("Scrobble Upload Complete", result)
            pause()
            continue

        if choice == "13":                                      # ← was 12
            edit_sync_settings(config)
            report = scan_library_report(config)
            continue

        if choice == "14":                                      # ← was 13
            edit_lastfm_settings(config)
            report = scan_library_report(config)
            continue

        if choice == "15":
            result = index_library(config, status=lambda message: safe_print(_dim(f"  {message}")))
            render_result("Library index", result)
            report = scan_library_report(config)
            pause()
            continue

        if choice == "16":
            result = index_walkman(config, status=lambda message: safe_print(_dim(f"  {message}")))
            render_result("Walkman index", result)
            report = scan_library_report(config)
            pause()
            continue

        if choice == "17":
            break

        print("Invalid selection.")
        pause()


def _main() -> int:
    """Full-screen front-end by default; `--classic` keeps the original scrolling menu."""
    walkman_mod.ensure_repo_cwd()
    if "--bench-copy" in sys.argv[1:]:
        args = [arg for arg in sys.argv[1:] if arg != "--bench-copy"]
        dest = args[0] if args else AppConfig().dest_drive
        print(copyutil.bench_copy(dest))
        return 0
    if "--classic" in sys.argv[1:]:
        import tui
        tui.prepare_console()
        run_tui()
        return 0
    try:
        import app
    except Exception as exc:                      # never leave the user without a UI
        print(f"full-screen UI unavailable ({exc}); falling back to the classic menu.\n")
        import tui
        tui.prepare_console()
        run_tui()
        return 0
    return app.main()


if __name__ == "__main__":
    import tui
    raise SystemExit(tui.run_with_hold(_main))
