"""The Walkman side of the likes sync.

Cinder keeps two files. `cinder_liked.conf` is the real store — one MediaStore object id per line
— and it is useless off-device, because those ids are rebuilt whenever the database is. Beside it
Cinder writes `cinder_loved.tsv` (`artist \t title`), which exists precisely so a PC-side tool can
read the list. That export is what this module reads.

Writing goes the other way through `cinder_liked_import.tsv`: the same two columns, dropped at the
volume root, which Cinder resolves against its own library on the next start and *replaces*
`cinder_liked.conf`. A build that predates the import simply ignores the file — so a device that
has not been reflashed still gets the playlist, just not the hearts.
"""
from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from .config import DEVICE_IMPORT_TSV, DEVICE_LOVED_TSV, LIKED_PLAYLIST
from .keys import track_key

IMPORT_HEADER = (
    "# artist\ttitle — liked list pushed from the PC (likesync).\n"
    "# Cinder replaces cinder_liked.conf on the next start and renames this file .done.\n"
)
FOREIGN_IMPORT_MARKERS = ("listen_pull", "flint")
PLAYLIST_HEADER = "#EXTM3U\n"


@dataclass
class DeviceVolume:
    root: str                  # the volume root: D:\  == /contents on the device
    label: str = ""

    @property
    def music_dir(self) -> str:
        try:
            from storage import isdir, join
            from walkman import KIND_ANDROID, SyncSettings, music_folder_names
            android = SyncSettings.load().device_kind == KIND_ANDROID
            names = music_folder_names(android=android)
        except Exception:
            isdir = os.path.isdir
            join = os.path.join
            names = ("MUSIC", "Music")
        leaf = self.root.replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]
        if leaf.lower() == "music":
            return self.root
        for name in names:
            candidate = join(self.root, name)
            if isdir(candidate):
                return candidate
        return join(self.root, names[0])

    @property
    def loved_path(self) -> str:
        return _join(self.root, DEVICE_LOVED_TSV)

    @property
    def import_path(self) -> str:
        return _join(self.root, DEVICE_IMPORT_TSV)

    @property
    def playlist_path(self) -> str:
        return _join(self.music_dir, LIKED_PLAYLIST)

    @property
    def present(self) -> bool:
        if not self.root:
            return False
        try:
            from storage import isdir
            return isdir(self.root)
        except Exception:
            return os.path.isdir(self.root)

    @property
    def import_pending(self) -> bool:
        """True while an import written by a previous run has not been consumed by the device."""
        return _exists(self.import_path)

    @property
    def import_is_foreign(self) -> bool:
        """True when another tool (listen_pull, Flint) left an import we must not overwrite."""
        if not _exists(self.import_path):
            return False
        try:
            head = _read(self.import_path)[:800].lower()
        except OSError:
            return False
        if "likesync" in head:
            return False
        return any(mark in head for mark in FOREIGN_IMPORT_MARKERS)


def volumes(internal_root: str, sd_root: str) -> list[DeviceVolume]:
    found = []
    for root, label in ((internal_root, "internal"), (sd_root, "sd")):
        if root:
            found.append(DeviceVolume(root=root, label=label))
    return found


# ── read ─────────────────────────────────────────────────────────

def read_loved(volume: DeviceVolume) -> dict[str, dict]:
    """Parse `cinder_loved.tsv` → {track_key: {"artist","title"}}.

    Also accepts the file under the music folder, because that is the other place a user might
    reasonably drop it, and a missing file is an empty list rather than an error: a device with
    no likes yet and a device that was never connected must not look different here — the caller
    decides that from `present`.
    """
    result: dict[str, dict] = {}
    for candidate in (volume.loved_path, _join(volume.music_dir, DEVICE_LOVED_TSV)):
        if not _exists(candidate):
            continue
        try:
            body = _read(candidate)
        except OSError:
            continue
        for line in body.splitlines():
            if not line.strip() or line.startswith("#"):
                continue
            parts = line.rstrip("\r\n").split("\t")
            if len(parts) < 2:
                continue
            artist, title = parts[0].strip(), parts[1].strip()
            if artist and title:
                result[track_key(artist, title)] = {"artist": artist, "title": title}
        break
    return result


def read_all(volumes_: list[DeviceVolume]) -> dict[str, dict]:
    merged: dict[str, dict] = {}
    for volume in volumes_:
        if volume.present:
            merged.update(read_loved(volume))
    return merged


# ── write ────────────────────────────────────────────────────────

def write_import(volume: DeviceVolume, tracks: list[dict], dry_run: bool = False) -> int:
    """Write the full liked list for Cinder to merge. Full list, not a delta: the device is the
    one participant that can be wiped and reindexed, so it must be able to rebuild from one file.
    """
    if dry_run:
        return len(tracks)
    body = IMPORT_HEADER + "".join(
        f"{track['artist']}\t{track['title']}\n" for track in tracks
    )
    _atomic_write(volume.import_path, body)
    return len(tracks)


def write_playlist(volume: DeviceVolume, rel_paths: list[str], dry_run: bool = False) -> int:
    """`Liked Songs.m3u8` in the dest drive's music folder.

    Paths are relative to that folder with forward slashes, which is what `sync.py` writes for
    every other playlist and what Sony's indexer accepts. Music sync now keeps the whole library
    on one volume, so this list is dest-only.
    """
    if dry_run:
        return len(rel_paths)
    body = PLAYLIST_HEADER + "".join(path.replace("\\", "/") + "\n" for path in rel_paths)
    _atomic_write(volume.playlist_path, body)
    return len(rel_paths)


def remove_playlist(volume: DeviceVolume, dry_run: bool = False) -> bool:
    if not _exists(volume.playlist_path):
        return False
    if not dry_run:
        try:
            _remove(volume.playlist_path)
        except OSError:
            return False
    return True


def _join(base: str, *parts: str) -> str:
    try:
        from storage import join
        return join(base, *parts)
    except Exception:
        return os.path.join(base, *parts)


def _exists(path: str) -> bool:
    try:
        from storage import exists
        return exists(path)
    except Exception:
        return os.path.isfile(path)


def _read(path: str) -> str:
    try:
        from storage import is_remote, read_text
        if is_remote(path):
            return read_text(path)
    except OSError:
        raise
    except Exception:
        pass
    return Path(path).read_text(encoding="utf-8-sig", errors="replace")


def _remove(path: str) -> None:
    try:
        from storage import remove
        remove(path)
        return
    except OSError:
        raise
    except Exception:
        os.remove(path)


def _atomic_write(path: str, body: str) -> None:
    """Temp file + replace. The device volume is FAT/exFAT on removable flash that gets yanked."""
    try:
        from storage import write_text
        write_text(path, body)
        return
    except OSError:
        raise
    except Exception:
        pass
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(body)
    os.replace(tmp, path)
