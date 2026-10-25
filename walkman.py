"""Walkman volume detection, live disk budgets, and persisted sync settings.

Shared by `sync.py` (music paths under MUSIC/) and `likesync` (drive roots, where Cinder's
files live). One heuristic, one settings file.
"""
from __future__ import annotations

import json
import os
import shutil
import sys
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent
SETTINGS_PATH = REPO_ROOT / "sync.json"


def ensure_repo_cwd() -> Path:
    """Shortcuts can start us in System32; settings and .env live next to this file."""
    os.chdir(REPO_ROOT)
    return REPO_ROOT

DRIVE_REMOVABLE = 2
DRIVE_FIXED = 3
HEADROOM_BYTES = 512 * 1024 * 1024
COPY_SPACE_RESERVE_BYTES = 64 * 1024 * 1024
WALKMAN_MARKERS = (
    ".scrobbler.log",
    "cinder_loved.tsv",
    "cinder_liked.conf",
    "cinder_liked_import.tsv",
    "listen_sources.conf",
    "OMGAUDIO",
)
LISTEN_SOURCES_NAME = "listen_sources.conf"
# listen_sources.conf (device) → os.environ (what likes/scrobbles/AudioMuse already read)
LISTEN_CREDENTIAL_KEYS = {
    "lastfm_user": "LASTFM_USERNAME",
    "lastfm_api_key": "LASTFM_API_KEY",
    "lastfm_api_secret": "LASTFM_API_SECRET",
    "lastfm_session_key": "LASTFM_SESSION_KEY",
    "audiomuse_url": "AUDIOMUSE_URL",
    "audiomuse_token": "AUDIOMUSE_TOKEN",
}
SD_LABELS = {"32 GB", "SD", "SD CARD", "SD_CARD"}

DEST_INTERNAL = "internal"
DEST_SD = "sd"
DEST_AUTO = "auto"
KIND_CINDER = "cinder"
KIND_ANDROID = "android-walkman"
DEVICE_KINDS = (KIND_CINDER, KIND_ANDROID)
PENDING_DEVICE_ID = "pending"

# Shared by music sync, the catalog, and likesync. Android Walkmans also play DSD/AIFF.
AUDIO_EXT = (
    ".flac", ".wav", ".mp3", ".m4a", ".aac", ".alac",
    ".dsf", ".dff", ".aiff", ".aif",
)


def iter_windows_drives() -> list[str]:
    if os.name != "nt":
        return []
    import ctypes
    drives: list[str] = []
    mask = ctypes.windll.kernel32.GetLogicalDrives()  # type: ignore[attr-defined]
    for index in range(26):
        if mask & (1 << index):
            drives.append(f"{chr(65 + index)}:\\")
    return drives


def _volume_information(drive_root: str) -> tuple[str, int]:
    """`(label, serial)` for a Windows volume. Empty / 0 when unavailable."""
    if os.name != "nt" or not drive_root:
        return "", 0
    import ctypes
    from ctypes import wintypes
    volume_name = ctypes.create_unicode_buffer(261)
    fs_name = ctypes.create_unicode_buffer(261)
    serial = wintypes.DWORD()
    max_comp = wintypes.DWORD()
    flags = wintypes.DWORD()
    ok = ctypes.windll.kernel32.GetVolumeInformationW(  # type: ignore[attr-defined]
        ctypes.c_wchar_p(drive_root),
        volume_name,
        len(volume_name),
        ctypes.byref(serial),
        ctypes.byref(max_comp),
        ctypes.byref(flags),
        fs_name,
        len(fs_name),
    )
    if not ok:
        return "", 0
    return volume_name.value.strip(), int(serial.value)


def get_volume_label(drive_root: str) -> str:
    if not drive_root or drive_root.startswith(("adb://", "mtp://")):
        return ""
    label, _ = _volume_information(drive_root)
    if label:
        return label
    if os.name != "nt":
        return Path(drive_root_of(drive_root)).name
    return ""


def get_volume_serial(path: str) -> str:
    """Stable volume id: hex serial, VolumeUUID, or a path hash."""
    if not path:
        return ""
    if path.startswith("adb://"):
        return f"adb-{path[6:].split('/', 1)[0]}"
    if path.startswith("mtp://"):
        return f"mtp-{path[6:].split('/', 1)[0]}"
    root = drive_root_of(path)
    _, serial = _volume_information(root)
    if serial:
        return f"{serial:08x}"
    uuid = _macos_volume_uuid(root)
    if uuid:
        return uuid
    return source_serial(path)


def source_serial(path: str) -> str:
    """Stable id for a library root (UNC path has no volume serial)."""
    import hashlib
    folded = os.path.normcase(os.path.abspath(path) if path else "")
    return hashlib.sha1(folded.encode("utf-8", errors="replace")).hexdigest()[:16]


_macos_info_cache: dict[str, dict] = {}


def _macos_disk_info(root: str) -> dict:
    if sys.platform != "darwin" or not root:
        return {}
    cached = _macos_info_cache.get(root)
    if cached is not None:
        return cached
    import plistlib
    import subprocess
    info: dict = {}
    try:
        result = subprocess.run(
            ["diskutil", "info", "-plist", root],
            capture_output=True, timeout=5, check=False,
        )
        if result.returncode == 0 and result.stdout:
            payload = plistlib.loads(result.stdout)
            if isinstance(payload, dict):
                info = payload
    except Exception:
        info = {}
    _macos_info_cache[root] = info
    return info


def _macos_volume_uuid(root: str) -> str:
    info = _macos_disk_info(root)
    uuid = info.get("VolumeUUID") or info.get("DiskUUID") or ""
    return str(uuid) if uuid else ""


def resolve_library_path(path: str) -> str:
    """Keep *path* when it exists. On POSIX, a missing UNC share may be `/Volumes/<share>`."""
    if not path:
        return path
    if os.path.isdir(path) or os.path.isfile(path):
        return path
    if os.name == "nt":
        return path
    folded = path.replace("/", "\\")
    if not folded.startswith("\\\\"):
        return path
    parts = [piece for piece in folded.split("\\") if piece]
    if len(parts) < 2:
        return path
    server, share, *rest = parts
    for candidate in (
        os.path.join("/Volumes", share, *rest),
        os.path.join("/Volumes", share),
        os.path.join("/Volumes", server, share, *rest),
    ):
        if os.path.isdir(candidate):
            return candidate
    return path


def get_drive_type(drive_root: str) -> int:
    if os.name == "nt":
        import ctypes
        return int(ctypes.windll.kernel32.GetDriveTypeW(ctypes.c_wchar_p(drive_root)))  # type: ignore[attr-defined]
    if sys.platform == "darwin" and drive_root:
        info = _macos_disk_info(drive_root)
        if not info:
            return DRIVE_REMOVABLE
        if info.get("Ejectable") or info.get("Removable") or info.get("RemovableMedia"):
            return DRIVE_REMOVABLE
        bus = str(info.get("BusProtocol") or info.get("Protocol") or "").upper()
        if "USB" in bus:
            return DRIVE_REMOVABLE
        return DRIVE_FIXED
    return 0


def disk_usage(path: str) -> tuple[int, int] | None:
    """(total, free) bytes for the volume holding *path*, or None."""
    if path and path.startswith(("adb://", "mtp://")):
        import storage as storage_mod
        return storage_mod.disk_usage(path)
    for candidate in (path, str(Path(path).anchor or path)):
        if not candidate:
            continue
        try:
            usage = shutil.disk_usage(candidate)
            return usage.total, usage.free
        except OSError:
            continue
    return None


def music_folder_names(*, android: bool = False) -> tuple[str, ...]:
    """Android W. Music indexes `Music`; Cinder and older MSC players use `MUSIC`."""
    return ("Music", "MUSIC") if android else ("MUSIC", "Music")


def resolve_music_path(drive_root: str, *, android: bool = False) -> str:
    names = music_folder_names(android=android)
    if drive_root.startswith(("adb://", "mtp://")):
        import storage as storage_mod
        folded = drive_root.replace("\\", "/").rstrip("/")
        if folded.rsplit("/", 1)[-1].lower() == "music":
            return drive_root.rstrip("/\\")
        for folder_name in names:
            candidate = storage_mod.join(drive_root, folder_name)
            if storage_mod.isdir(candidate):
                return candidate
        return storage_mod.join(drive_root, names[0])
    for folder_name in names:
        candidate = os.path.join(drive_root, folder_name)
        if os.path.isdir(candidate):
            return candidate
    return os.path.join(drive_root, names[0])


def drive_root_of(path: str) -> str:
    """Mount root: `D:\\` on Windows, `/Volumes/WALKMAN` on macOS.

    POSIX `Path.anchor` is `/`, which would collapse every USB volume into one serial.
    Remote dest URIs are returned unchanged.
    """
    if not path:
        return path
    if path.startswith(("adb://", "mtp://")):
        return path
    if os.name == "nt":
        anchor = Path(path).anchor
        return str(anchor) if anchor else path
    try:
        current = Path(path)
        if current.is_file():
            current = current.parent
        elif not current.exists():
            current = current.parent if current.parent != current else current
        current = current.resolve() if current.exists() else current
        current_dev = os.stat(current).st_dev
        while True:
            parent = current.parent
            if parent == current:
                break
            try:
                if os.stat(parent).st_dev != current_dev:
                    break
            except OSError:
                break
            current = parent
        return str(current)
    except OSError:
        return path


def system_drive_root() -> str:
    windir = os.environ.get("SystemDrive") or os.environ.get("WINDIR", "C:")
    if len(windir) == 2 and windir.endswith(":"):
        return windir + "\\"
    anchor = Path(windir).anchor
    return str(anchor) if anchor else r"C:\\"


def has_music_folder(drive_root: str) -> bool:
    return os.path.isdir(os.path.join(drive_root, "MUSIC")) or os.path.isdir(
        os.path.join(drive_root, "Music")
    )


def has_walkman_marker(drive_root: str) -> bool:
    music = resolve_music_path(drive_root)
    for name in WALKMAN_MARKERS:
        if os.path.exists(os.path.join(drive_root, name)) or os.path.exists(os.path.join(music, name)):
            return True
    return False


def volume_label_for(path: str) -> str:
    """Label of the volume that holds *path*, or empty."""
    if not path:
        return ""
    return get_volume_label(drive_root_of(path))


def path_is_dir(path: str) -> bool:
    """True when *path* exists as a directory on the local disk or a remote dest URI."""
    if not path:
        return False
    if path.startswith(("adb://", "mtp://")):
        import storage as storage_mod
        return storage_mod.isdir(path)
    return os.path.isdir(path)


def describe_mount(music_path: str) -> str:
    """Short status for Settings: `D:\\Music  WALKMAN  connected`."""
    if not music_path:
        return "not found"
    label = volume_label_for(music_path) if not music_path.startswith(("adb://", "mtp://")) else ""
    state = "connected" if path_is_dir(music_path) else "unplugged"
    extra = f"  {label}" if label else ""
    return f"{music_path}{extra}  {state}"


def infer_device_kind(internal_root: str = "", sd_root: str = "") -> str:
    """Cinder if a live volume has Sony/Cinder markers; Android if only a Music card is present."""
    for root in (internal_root, sd_root):
        if not root or root.startswith(("adb://", "mtp://")):
            continue
        if has_walkman_marker(root):
            return KIND_CINDER
        if get_volume_label(root).strip().upper() == "WALKMAN":
            return KIND_CINDER
    if any(root.startswith(("adb://", "mtp://")) for root in (internal_root, sd_root) if root):
        return KIND_ANDROID
    for root in (internal_root, sd_root):
        if root and has_music_folder(root) and not has_walkman_marker(root):
            return KIND_ANDROID
    return KIND_CINDER


def parse_listen_sources(text: str) -> dict[str, str]:
    """Non-empty credential fields from a `listen_sources.conf` body."""
    values: dict[str, str] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        value = value.strip().strip('"').strip("'")
        if key in LISTEN_CREDENTIAL_KEYS and value:
            values[key] = value
    return values


def listen_sources_paths(*roots: str) -> list[str]:
    """Candidate `listen_sources.conf` paths: override, then each volume root (and MUSIC/)."""
    import storage as storage_mod
    paths: list[str] = []
    seen: set[str] = set()

    def add(path: str) -> None:
        if not path:
            return
        folded = storage_mod.norm(path)
        if folded in seen:
            return
        seen.add(folded)
        paths.append(path)

    add(os.environ.get("CINDER_LISTEN_CONFIG", "").strip())
    for root in roots:
        if not root:
            continue
        add(storage_mod.join(root, LISTEN_SOURCES_NAME))
        parent = volume_root(root)
        if parent and parent != root:
            add(storage_mod.join(parent, LISTEN_SOURCES_NAME))
    return paths


def read_listen_sources(*roots: str) -> dict[str, str]:
    """First parseable `listen_sources.conf` on the device (internal, then SD)."""
    import storage as storage_mod
    for path in listen_sources_paths(*roots):
        try:
            if not storage_mod.exists(path):
                continue
            parsed = parse_listen_sources(storage_mod.read_text(path))
        except OSError:
            continue
        if parsed:
            return parsed
    return {}


def apply_listen_credentials(*roots: str) -> dict[str, str]:
    """Copy non-empty device credentials into the environment. Empty fields stay on the PC `.env`.

    Returns the env keys that came from the Walkman.
    """
    parsed = read_listen_sources(*roots)
    applied: dict[str, str] = {}
    for src_key, env_key in LISTEN_CREDENTIAL_KEYS.items():
        value = parsed.get(src_key, "").strip()
        if not value:
            continue
        os.environ[env_key] = value
        applied[env_key] = value
    if any(key.startswith("LASTFM_") for key in applied):
        os.environ["WALKMAN_SYNC_LASTFM_FROM"] = "device"
    if any(key.startswith("AUDIOMUSE_") for key in applied):
        os.environ["WALKMAN_SYNC_AUDIOMUSE_FROM"] = "device"
    return applied


def volume_root(music_path: str) -> str:
    """Directory that holds MUSIC/ and Cinder's `/contents` files.

    On a real Walkman that is the drive root (`D:\\`). In tests it is the parent of the
    fake Music folder, so we do not write `cinder_playlists` onto `C:\\`. Remote dest
    URIs (`adb://`, `mtp://`) stay as URIs; a trailing `/Music` is stripped.
    """
    if music_path.startswith(("adb://", "mtp://")):
        import storage as storage_mod
        folded = music_path.replace("\\", "/").rstrip("/")
        if folded.rsplit("/", 1)[-1].lower() == "music":
            return storage_mod.parent(folded)
        return folded
    parent = os.path.dirname(os.path.abspath(music_path))
    return parent if parent else drive_root_of(music_path)


@dataclass
class VolumeCandidate:
    root: str
    label: str
    total: int
    free: int
    drive_type: int


def _collect_candidates() -> list[VolumeCandidate]:
    if os.name == "nt":
        return _collect_windows_candidates()
    if sys.platform == "darwin":
        return _collect_macos_candidates()
    return []


def _collect_windows_candidates() -> list[VolumeCandidate]:
    found: list[VolumeCandidate] = []
    system = os.path.normcase(system_drive_root())
    for drive in iter_windows_drives():
        if os.path.normcase(drive) == system:
            continue
        usage = disk_usage(drive)
        if usage is None:
            continue
        total, free = usage
        drive_type = get_drive_type(drive)
        if drive_type in (1, 4, 5, 6):  # no-root, remote, cdrom, ramdisk
            continue
        found.append(VolumeCandidate(
            root=drive,
            label=get_volume_label(drive),
            total=total,
            free=free,
            drive_type=drive_type,
        ))
    return found


def _collect_macos_candidates() -> list[VolumeCandidate]:
    volumes = Path("/Volumes")
    if not volumes.is_dir():
        return []
    try:
        boot_dev = os.stat("/").st_dev
    except OSError:
        boot_dev = -1
    found: list[VolumeCandidate] = []
    try:
        entries = list(volumes.iterdir())
    except OSError:
        return []
    for entry in entries:
        try:
            target = entry.resolve() if entry.is_symlink() else entry
            if not target.is_dir():
                continue
            if os.stat(target).st_dev == boot_dev:
                continue
            root = str(entry)
            usage = disk_usage(root)
            if usage is None:
                continue
            total, free = usage
            found.append(VolumeCandidate(
                root=root,
                label=entry.name,
                total=total,
                free=free,
                drive_type=get_drive_type(root),
            ))
        except OSError:
            continue
    return found


def _is_walkman(candidate: VolumeCandidate) -> bool:
    if candidate.label.strip().upper() == "WALKMAN":
        return True
    return has_walkman_marker(candidate.root)


def _detect_walkman_roots_hardware() -> tuple[str, str]:
    candidates = _collect_candidates()
    walkman = next((c for c in candidates if _is_walkman(c)), None)
    internal = walkman.root if walkman else ""

    others = [
        c for c in candidates
        if not internal or os.path.normcase(c.root) != os.path.normcase(internal)
    ]
    card = None
    if internal:
        removable = [c for c in others if c.drive_type == DRIVE_REMOVABLE]
        if removable:
            card = max(removable, key=lambda c: c.total)
    if card is None:
        labelled = [c for c in others if c.label.strip().upper() in SD_LABELS]
        if labelled:
            card = max(labelled, key=lambda c: c.total)
    if card is None:
        with_music = [
            c for c in others
            if c.drive_type == DRIVE_REMOVABLE and has_music_folder(c.root)
        ]
        if with_music:
            card = max(with_music, key=lambda c: c.total)
    sd = card.root if card else ""
    return internal, sd


def detect_walkman_roots(*, use_env: bool = True) -> tuple[str, str]:
    """`(internal_root, sd_root)` — drive roots, not Music folders.

    Internal is the volume labelled WALKMAN (or one that holds Cinder/scrobble files).
    SD is the other removable volume that appears with it. A live Walkman wins over
    stale env / saved letters. Env `LIKESYNC_INTERNAL` / `LIKESYNC_SD` is only used
    when nothing is plugged in.
    """
    internal, sd = _detect_walkman_roots_hardware()
    if internal or sd:
        return internal, sd
    if not use_env:
        return "", ""
    return os.environ.get("LIKESYNC_INTERNAL", ""), os.environ.get("LIKESYNC_SD", "")


def detect_walkman_paths(*, use_env: bool = True, android: bool = False) -> tuple[str, str]:
    """Music-folder paths (`D:\\Music`, `H:\\Music`). Empty when nothing is plugged in."""
    internal_root, sd_root = detect_walkman_roots(use_env=False)
    internal = resolve_music_path(internal_root, android=android) if internal_root else ""
    sd = resolve_music_path(sd_root, android=android) if sd_root else ""
    if internal or sd:
        return internal, sd
    if not use_env:
        return "", ""
    return os.environ.get("SYNC_INTERNAL_DRIVE", ""), os.environ.get("SYNC_SD_DRIVE", "")


def live_budget_bytes(path: str, held_bytes: int = 0, headroom: int = HEADROOM_BYTES) -> int:
    """(free + music already held) − headroom. Zero if the volume cannot be read."""
    usage = disk_usage(path)
    if usage is None:
        return 0
    _, free = usage
    return max((free + held_bytes) - headroom, 0)


def live_total_bytes(path: str) -> int:
    usage = disk_usage(path)
    return usage[0] if usage else 0


def live_free_bytes(path: str) -> int:
    usage = disk_usage(path)
    if usage is None:
        raise OSError(f"Could not determine free space for destination: {path}")
    return usage[1]


def pick_dest(dest: str, internal_drive: str, sd_drive: str,
              internal_held: int = 0, sd_held: int = 0) -> str:
    """Resolve `internal` | `sd` | `auto` to a music path.

    Auto (the unset default) is the volume with more room: live budget when held
    bytes are known, otherwise the larger volume total so a filled dest is not
    abandoned for an empty card.
    """
    if dest == DEST_SD:
        return sd_drive
    if dest == DEST_INTERNAL:
        return internal_drive
    if internal_held or sd_held:
        internal_budget = live_budget_bytes(internal_drive, internal_held)
        sd_budget = live_budget_bytes(sd_drive, sd_held)
        if sd_budget > internal_budget:
            return sd_drive
        if internal_budget > sd_budget:
            return internal_drive
    sd_total = live_total_bytes(sd_drive)
    internal_total = live_total_bytes(internal_drive)
    if sd_total > internal_total:
        return sd_drive
    return internal_drive


@dataclass
class DeviceProfile:
    """One Walkman: mounts, dest, kind, and disk budgets. Source library is shared."""
    name: str = ""
    internal_serial: str = ""
    sd_serial: str = ""
    internal_drive: str = ""
    sd_drive: str = ""
    dest: str = DEST_AUTO
    device_kind: str = KIND_CINDER
    internal_max_gb: float = 0.0
    sd_max_gb: float = 0.0

    @property
    def primary_serial(self) -> str:
        return self.internal_serial or self.sd_serial

    def matches(self, internal_serial: str = "", sd_serial: str = "") -> bool:
        live = {serial for serial in (internal_serial, sd_serial) if serial}
        if not live:
            return False
        known = {serial for serial in (self.internal_serial, self.sd_serial) if serial}
        return bool(live & known)

    @classmethod
    def from_dict(cls, data: dict) -> "DeviceProfile":
        known = {item.name for item in fields(cls)}
        parsed = {key: value for key, value in data.items() if key in known}
        profile = cls(**parsed)
        if profile.dest not in (DEST_INTERNAL, DEST_SD, DEST_AUTO):
            profile.dest = DEST_AUTO
        if profile.device_kind not in DEVICE_KINDS:
            profile.device_kind = KIND_CINDER
        try:
            profile.internal_max_gb = float(profile.internal_max_gb or 0)
            profile.sd_max_gb = float(profile.sd_max_gb or 0)
        except (TypeError, ValueError):
            profile.internal_max_gb = 0.0
            profile.sd_max_gb = 0.0
        return profile


def default_device_name(internal_drive: str, sd_drive: str, existing: list[str]) -> str:
    label = ""
    for path in (internal_drive, sd_drive):
        if not path or path.startswith(("adb://", "mtp://")):
            continue
        label = get_volume_label(drive_root_of(path))
        if label:
            break
    base = (label or "Walkman").strip() or "Walkman"
    taken = {name.casefold() for name in existing if name}
    if base.casefold() not in taken:
        return base
    index = 2
    while f"{base} {index}".casefold() in taken:
        index += 1
    return f"{base} {index}"


def live_serials(internal_drive: str, sd_drive: str) -> tuple[str, str]:
    return (
        get_volume_serial(internal_drive) if internal_drive else "",
        get_volume_serial(sd_drive) if sd_drive else "",
    )


def bind_live_device(
    settings: "SyncSettings",
    internal_drive: str,
    sd_drive: str,
    *,
    inferred_kind: str = "",
) -> tuple[bool, bool]:
    """Match or create a profile for the plugged volumes.

    Returns `(created, changed)` — created is a first-seen serial (toast); changed
    means active mounts/id/kind-on-create moved.
    """
    if not internal_drive and not sd_drive:
        return False, False
    internal_serial, sd_serial = live_serials(internal_drive, sd_drive)
    primary = internal_serial or sd_serial
    if not primary:
        return False, False

    matches = [key for key, profile in settings.devices.items()
               if profile.matches(internal_serial, sd_serial)]
    created = False
    if matches:
        if settings.active_device in matches:
            key = settings.active_device
        elif internal_serial:
            key = next(
                (item for item in matches
                 if settings.devices[item].internal_serial == internal_serial),
                matches[0],
            )
        else:
            key = matches[0]
    elif PENDING_DEVICE_ID in settings.devices:
        key = primary
        settings.devices[key] = settings.devices.pop(PENDING_DEVICE_ID)
    else:
        key = primary
        created = True
        settings.devices[key] = DeviceProfile(
            name=default_device_name(
                internal_drive, sd_drive,
                [profile.name for profile in settings.devices.values()],
            ),
            device_kind=inferred_kind if inferred_kind in DEVICE_KINDS else KIND_CINDER,
        )

    profile = settings.devices[key]
    before = (settings.active_device, profile.internal_drive, profile.sd_drive,
              profile.internal_serial, profile.sd_serial)
    if internal_drive:
        profile.internal_drive = internal_drive
        if internal_serial:
            profile.internal_serial = internal_serial
    else:
        profile.internal_drive = ""
    if sd_drive:
        profile.sd_drive = sd_drive
        if sd_serial:
            profile.sd_serial = sd_serial
    else:
        profile.sd_drive = ""
    if created and inferred_kind in DEVICE_KINDS:
        profile.device_kind = inferred_kind
    if not profile.name:
        profile.name = default_device_name(
            internal_drive, sd_drive,
            [item.name for item_key, item in settings.devices.items() if item_key != key],
        )
    settings.active_device = key
    after = (settings.active_device, profile.internal_drive, profile.sd_drive,
             profile.internal_serial, profile.sd_serial)
    return created, created or before != after


@dataclass
class SyncSettings:
    source_dir: str = r"\\WillineauVault\Music"
    playlist_dir: str = r"\\WillineauVault\Music"
    excluded_albums: str = ""
    copy_workers: int = 1
    auto_detect_drives: bool = True
    active_device: str = ""
    devices: dict[str, DeviceProfile] = field(default_factory=dict)

    def active_profile(self) -> DeviceProfile | None:
        return self.devices.get(self.active_device)

    def ensure_active_profile(self) -> DeviceProfile:
        profile = self.active_profile()
        if profile is not None:
            return profile
        key = self.active_device or PENDING_DEVICE_ID
        self.devices[key] = DeviceProfile(name="Walkman")
        self.active_device = key
        return self.devices[key]

    def other_device_names(self) -> list[str]:
        return [profile.name or key for key, profile in self.devices.items()
                if key != self.active_device]

    @property
    def internal_drive(self) -> str:
        profile = self.active_profile()
        return profile.internal_drive if profile else ""

    @property
    def sd_drive(self) -> str:
        profile = self.active_profile()
        return profile.sd_drive if profile else ""

    @property
    def dest(self) -> str:
        profile = self.active_profile()
        return profile.dest if profile else DEST_AUTO

    @property
    def device_kind(self) -> str:
        profile = self.active_profile()
        return profile.device_kind if profile else KIND_CINDER

    @property
    def internal_max_gb(self) -> float:
        profile = self.active_profile()
        return profile.internal_max_gb if profile else 0.0

    @property
    def sd_max_gb(self) -> float:
        profile = self.active_profile()
        return profile.sd_max_gb if profile else 0.0

    def _migrate_flat(self, data: dict) -> None:
        internal = str(data.get("internal_drive") or "")
        sd = str(data.get("sd_drive") or "")
        internal_serial, sd_serial = live_serials(internal, sd)
        key = internal_serial or sd_serial or PENDING_DEVICE_ID
        self.devices[key] = DeviceProfile.from_dict({
            "name": default_device_name(internal, sd, []),
            "internal_serial": internal_serial,
            "sd_serial": sd_serial,
            "internal_drive": internal,
            "sd_drive": sd,
            "dest": data.get("dest") or DEST_AUTO,
            "device_kind": data.get("device_kind") or KIND_CINDER,
            "internal_max_gb": data.get("internal_max_gb") or 0,
            "sd_max_gb": data.get("sd_max_gb") or 0,
        })
        self.active_device = key

    def _normalize(self) -> None:
        try:
            self.copy_workers = max(int(self.copy_workers), 1)
        except (TypeError, ValueError):
            self.copy_workers = 1
        if self.active_device and self.active_device not in self.devices and self.devices:
            self.active_device = next(iter(self.devices))
        if not self.active_device and self.devices:
            self.active_device = next(iter(self.devices))

    @classmethod
    def load(cls, path: Path = SETTINGS_PATH) -> "SyncSettings":
        settings = cls()
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError, TypeError):
            data = {}
        if not isinstance(data, dict):
            data = {}
        for key in ("source_dir", "playlist_dir", "excluded_albums",
                    "copy_workers", "auto_detect_drives", "active_device"):
            if key in data:
                setattr(settings, key, data[key])
        devices_raw = data.get("devices")
        if isinstance(devices_raw, dict) and devices_raw:
            for key, value in devices_raw.items():
                if isinstance(value, dict):
                    settings.devices[str(key)] = DeviceProfile.from_dict(value)
        else:
            settings._migrate_flat(data)
        settings._normalize()
        return settings

    def save(self, path: Path = SETTINGS_PATH) -> None:
        payload = {
            "source_dir": self.source_dir,
            "playlist_dir": self.playlist_dir,
            "excluded_albums": self.excluded_albums,
            "copy_workers": self.copy_workers,
            "auto_detect_drives": self.auto_detect_drives,
            "active_device": self.active_device,
            "devices": {key: asdict(profile) for key, profile in self.devices.items()},
        }
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
