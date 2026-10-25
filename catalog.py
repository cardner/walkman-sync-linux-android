"""Persistent source + dest file catalog.

A scan walks the tree once and writes sqlite. Later scans only re-read files whose
path/size/mtime changed. Planning, skip-if-present, duplicates, and likesync read this
instead of opening every header again.
"""
from __future__ import annotations

import json
import os
import sqlite3
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import contentkey as contentkey_mod
import manifest as manifest_mod
import storage as storage_mod
import walkman as walkman_mod

REPO_ROOT = Path(__file__).resolve().parent
DEFAULT_PATH = REPO_ROOT / ".sony-sync" / "catalog.sqlite"
MTIME_TOLERANCE = 2.0
AUDIO_EXT = walkman_mod.AUDIO_EXT
PLAYLIST_EXT = (".m3u", ".m3u8")
KIND_SOURCE = "source"
KIND_INTERNAL = "internal"
KIND_SD = "sd"

ProgressFn = Callable[[str, int, int], None]


@dataclass
class RefreshStats:
    added: int = 0
    updated: int = 0
    removed: int = 0
    unchanged: int = 0
    keyed: int = 0

    @property
    def total_seen(self) -> int:
        return self.added + self.updated + self.unchanged


@dataclass
class VolumeInfo:
    id: int
    kind: str
    serial: str
    label: str
    last_root: str
    scanned_at: float
    file_count: int
    key_count: int


@dataclass
class FileRow:
    rel: str
    size: int
    mtime: float
    content_key: str = ""
    artist: str = ""
    title: str = ""
    album: str = ""
    track_no: int = 0
    suffix: str = ""


def _is_preserved(name: str) -> bool:
    lower = name.lower()
    if lower.startswith(("cinder_", "listen_")):
        return True
    if lower in {
        "liked songs.m3u8", "liked songs.m3u",
        "listen_sources.conf", "listen_sync_state.json",
        ".scrobbler.log", "scrobble.logs",
        manifest_mod.MANIFEST_NAME, "cinder_playlists", "cinder_palettes",
    }:
        return True
    return False


def _is_partial(name: str) -> bool:
    return name.startswith(".") and name.endswith(manifest_mod.PARTIAL_SUFFIX)


def resolve_path(path: Path | str | None = None) -> Path:
    if path is not None:
        return Path(path)
    env = os.environ.get("SONY_SYNC_CATALOG", "").strip()
    if env:
        return Path(env)
    return DEFAULT_PATH


def _walk(root: str, base: str, *, dest: bool = False) -> list[tuple[str, int, float]]:
    del base  # walk is always relative to the volume root
    return storage_mod.walk_files(root, dest=dest)


SCHEMA = """
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS volumes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    serial TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    last_root TEXT NOT NULL DEFAULT '',
    scanned_at REAL NOT NULL DEFAULT 0,
    UNIQUE (kind, serial)
);
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    volume_id INTEGER NOT NULL REFERENCES volumes(id) ON DELETE CASCADE,
    rel TEXT NOT NULL,
    rel_fold TEXT NOT NULL,
    size INTEGER NOT NULL,
    mtime REAL NOT NULL,
    content_key TEXT NOT NULL DEFAULT '',
    artist TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    album TEXT NOT NULL DEFAULT '',
    track_no INTEGER NOT NULL DEFAULT 0,
    suffix TEXT NOT NULL DEFAULT '',
    seen_at REAL NOT NULL DEFAULT 0,
    UNIQUE (volume_id, rel_fold)
);
CREATE INDEX IF NOT EXISTS files_key ON files(content_key) WHERE content_key != '';
CREATE INDEX IF NOT EXISTS files_tags ON files(artist, title) WHERE artist != '' AND title != '';
"""


class Catalog:
    def __init__(self, path: Path | str | None = None) -> None:
        self.path = resolve_path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._db = sqlite3.connect(self.path)
        self._db.row_factory = sqlite3.Row
        self._db.execute("PRAGMA foreign_keys = ON")
        self._db.executescript(SCHEMA)
        self._key_import = self._load_key_import()
        self._tag_import = self._load_tag_import()

    def close(self) -> None:
        self._db.close()

    def __enter__(self) -> "Catalog":
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    def _meta(self, key: str) -> str:
        row = self._db.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
        return row["value"] if row else ""

    def _set_meta(self, key: str, value: str) -> None:
        self._db.execute(
            "INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )

    def _should_import_json(self) -> bool:
        try:
            return self.path.resolve() == DEFAULT_PATH.resolve()
        except OSError:
            return False

    def _load_key_import(self) -> dict[str, dict]:
        if self._meta("imported_keys") == "1":
            return {}
        if not self._should_import_json():
            self._set_meta("imported_keys", "1")
            self._db.commit()
            return {}
        path = contentkey_mod.CACHE_PATH
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self._set_meta("imported_keys", "1")
            self._db.commit()
            return {}
        rows = payload if isinstance(payload, dict) else {}
        self._set_meta("imported_keys", "1")
        self._db.commit()
        return rows

    def _load_tag_import(self) -> dict[str, list]:
        if self._meta("imported_tags") == "1":
            return {}
        if not self._should_import_json():
            self._set_meta("imported_tags", "1")
            self._db.commit()
            return {}
        path = REPO_ROOT / ".likesync" / "tagcache.json"
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            self._set_meta("imported_tags", "1")
            self._db.commit()
            return {}
        entries = payload.get("entries", {}) if isinstance(payload, dict) else {}
        self._set_meta("imported_tags", "1")
        self._db.commit()
        return entries if isinstance(entries, dict) else {}

    def _imported_key(self, abs_path: str, size: int, mtime: float) -> str:
        row = self._key_import.get(os.path.normcase(abs_path))
        if not row:
            return ""
        try:
            if int(row.get("size", -1)) != size:
                return ""
            if abs(float(row.get("mtime", 0)) - mtime) > MTIME_TOLERANCE:
                return ""
            key = row.get("key") or ""
            return key if isinstance(key, str) else ""
        except (TypeError, ValueError):
            return ""

    def _imported_tags(self, abs_path: str, size: int, mtime: float) -> tuple[str, str, str, int]:
        row = self._tag_import.get(os.path.normcase(abs_path))
        if not isinstance(row, list) or len(row) < 7:
            return "", "", "", 0
        try:
            if int(row[0]) != size or abs(float(row[1]) - mtime) > MTIME_TOLERANCE:
                return "", "", "", 0
            return str(row[2] or ""), str(row[3] or ""), str(row[4] or ""), int(row[6] or 0)
        except (TypeError, ValueError):
            return "", "", "", 0

    def _ensure_volume(self, kind: str, root: str) -> int:
        if kind == KIND_SOURCE:
            serial = walkman_mod.source_serial(root)
            label = "library"
        else:
            serial = storage_mod.serial(root)
            label = storage_mod.label(root)
        row = self._db.execute(
            "SELECT id FROM volumes WHERE kind = ? AND serial = ?",
            (kind, serial),
        ).fetchone()
        if row:
            self._db.execute(
                "UPDATE volumes SET last_root = ?, label = ? WHERE id = ?",
                (root, label, row["id"]),
            )
            return int(row["id"])
        cur = self._db.execute(
            "INSERT INTO volumes(kind, serial, label, last_root) VALUES (?, ?, ?, ?)",
            (kind, serial, label, root),
        )
        return int(cur.lastrowid)

    def volume_for_kind(self, kind: str) -> VolumeInfo | None:
        row = self._db.execute(
            "SELECT v.*, "
            "(SELECT COUNT(*) FROM files WHERE volume_id = v.id) AS file_count, "
            "(SELECT COUNT(*) FROM files WHERE volume_id = v.id AND content_key != '') AS key_count "
            "FROM volumes v WHERE kind = ? ORDER BY scanned_at DESC LIMIT 1",
            (kind,),
        ).fetchone()
        if not row:
            return None
        return VolumeInfo(
            id=row["id"], kind=row["kind"], serial=row["serial"], label=row["label"],
            last_root=row["last_root"], scanned_at=row["scanned_at"],
            file_count=row["file_count"], key_count=row["key_count"],
        )

    def volume_for_root(self, kind: str, root: str) -> VolumeInfo | None:
        if kind == KIND_SOURCE:
            serial = walkman_mod.source_serial(root)
        else:
            serial = storage_mod.serial(root)
        row = self._db.execute(
            "SELECT v.*, "
            "(SELECT COUNT(*) FROM files WHERE volume_id = v.id) AS file_count, "
            "(SELECT COUNT(*) FROM files WHERE volume_id = v.id AND content_key != '') AS key_count "
            "FROM volumes v WHERE kind = ? AND serial = ?",
            (kind, serial),
        ).fetchone()
        if not row:
            return None
        return VolumeInfo(
            id=row["id"], kind=row["kind"], serial=row["serial"], label=row["label"],
            last_root=row["last_root"], scanned_at=row["scanned_at"],
            file_count=row["file_count"], key_count=row["key_count"],
        )

    def coverage(self, internal_root: str = "", sd_root: str = "") -> dict[str, VolumeInfo | None]:
        return {
            KIND_SOURCE: self.volume_for_kind(KIND_SOURCE),
            KIND_INTERNAL: self.volume_for_root(KIND_INTERNAL, internal_root) if internal_root else None,
            KIND_SD: self.volume_for_root(KIND_SD, sd_root) if sd_root else None,
        }

    def files_for(self, kind: str, root: str | None = None) -> list[FileRow]:
        info = self.volume_for_root(kind, root) if root else self.volume_for_kind(kind)
        if info is None:
            return []
        rows = self._db.execute(
            "SELECT rel, size, mtime, content_key, artist, title, album, track_no, suffix "
            "FROM files WHERE volume_id = ?",
            (info.id,),
        )
        return [
            FileRow(
                rel=r["rel"], size=r["size"], mtime=r["mtime"],
                content_key=r["content_key"], artist=r["artist"], title=r["title"],
                album=r["album"], track_no=r["track_no"], suffix=r["suffix"],
            )
            for r in rows
        ]

    def refresh_source(self, source_dir: str, progress: ProgressFn | None = None) -> RefreshStats:
        return self._refresh(KIND_SOURCE, source_dir, dest=False, progress=progress)

    def refresh_dest(self, music_path: str, kind: str, progress: ProgressFn | None = None) -> RefreshStats:
        if kind not in (KIND_INTERNAL, KIND_SD):
            raise ValueError(f"dest kind must be internal or sd, not {kind}")
        return self._refresh(kind, music_path, dest=True, progress=progress)

    def confirm_live(self, kind: str, path: str) -> tuple[bool, str]:
        """Whether *path* is a connected volume we can index for *kind*.

        Looks up the catalog row for this path's serial, not the most recently
        scanned Walkman of the same kind. An empty row (never indexed) is allowed
        so the first sync can land.
        """
        if not path:
            return False, "no destination set"
        if not storage_mod.isdir(path):
            return False, "destination is not connected"
        live = storage_mod.serial(path)
        info = self.volume_for_root(kind, path)
        if info is None or not info.serial:
            return True, ""
        if live != info.serial:
            return False, (
                f"{kind} path {path} is a different volume "
                f"(catalog {info.serial}, live {live}) — refusing to write"
            )
        return True, ""

    def _refresh(self, kind: str, root: str, *, dest: bool, progress: ProgressFn | None) -> RefreshStats:
        stats = RefreshStats()
        if not root or not storage_mod.isdir(root):
            return stats
        volume_id = self._ensure_volume(kind, root)
        stamp = time.time()
        live_by_fold: dict[str, tuple[str, int, float]] = {}
        for rel, size, mtime in _walk(root, root, dest=dest):
            folded = os.path.normcase(os.path.normpath(rel))
            live_by_fold.setdefault(folded, (rel, size, mtime))
        live = list(live_by_fold.values())
        remote = storage_mod.is_remote(root)
        existing = {
            row["rel_fold"]: row
            for row in self._db.execute(
                "SELECT id, rel_fold, size, mtime, content_key, artist, title, album, track_no "
                "FROM files WHERE volume_id = ?",
                (volume_id,),
            )
        }
        dest_manifest = manifest_mod.Manifest.load(root) if dest else None
        total = max(len(live), 1)
        for index, (rel, size, mtime) in enumerate(live, start=1):
            if progress and (index == 1 or index % 64 == 0 or index == len(live)):
                folder = Path(rel).parts[0] if Path(rel).parts else rel
                progress(folder, index, total)
            folded = os.path.normcase(os.path.normpath(rel))
            suffix = Path(rel).suffix.lower()
            prev = existing.pop(folded, None)
            unchanged = (
                prev is not None
                and int(prev["size"]) == size
                and (remote or abs(float(prev["mtime"]) - mtime) <= MTIME_TOLERANCE)
            )
            if unchanged:
                self._db.execute(
                    "UPDATE files SET seen_at = ?, rel = ? WHERE id = ?",
                    (stamp, rel, prev["id"]),
                )
                stats.unchanged += 1
                continue
            abs_path = storage_mod.join(root, rel)
            key = (prev["content_key"] if prev else "") or ""
            artist = prev["artist"] if prev else ""
            title = prev["title"] if prev else ""
            album = prev["album"] if prev else ""
            track_no = int(prev["track_no"]) if prev else 0
            if dest:
                if _is_partial(os.path.basename(rel)):
                    key = ""
                else:
                    record = dest_manifest.lookup(rel) if dest_manifest else None
                    if record and record.content_key:
                        key = record.content_key
                    elif suffix == ".flac" and not key and not remote:
                        local = storage_mod.local_file(abs_path)
                        if local:
                            key = contentkey_mod.flac_header_key(local) or ""
            else:
                if suffix in (".flac", ".mp3"):
                    key = self._imported_key(abs_path, size, mtime) or key
                    if not key:
                        key = contentkey_mod.content_key(abs_path) or ""
                if suffix in AUDIO_EXT:
                    imported = self._imported_tags(abs_path, size, mtime)
                    if imported[0] or imported[1]:
                        artist, title, album, track_no = imported
                    else:
                        artist, title, album, track_no = self._read_tags(abs_path, rel)
            if key:
                stats.keyed += 1
            if prev is None:
                self._db.execute(
                    "INSERT INTO files(volume_id, rel, rel_fold, size, mtime, content_key, "
                    "artist, title, album, track_no, suffix, seen_at) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) "
                    "ON CONFLICT(volume_id, rel_fold) DO UPDATE SET "
                    "rel = excluded.rel, size = excluded.size, mtime = excluded.mtime, "
                    "content_key = excluded.content_key, artist = excluded.artist, "
                    "title = excluded.title, album = excluded.album, "
                    "track_no = excluded.track_no, suffix = excluded.suffix, "
                    "seen_at = excluded.seen_at",
                    (volume_id, rel, folded, size, mtime, key or "",
                     artist or "", title or "", album or "", track_no, suffix, stamp),
                )
                stats.added += 1
            else:
                self._db.execute(
                    "UPDATE files SET rel = ?, size = ?, mtime = ?, content_key = ?, "
                    "artist = ?, title = ?, album = ?, track_no = ?, suffix = ?, seen_at = ? "
                    "WHERE id = ?",
                    (rel, size, mtime, key or "", artist or "", title or "", album or "",
                     track_no, suffix, stamp, prev["id"]),
                )
                stats.updated += 1
        leftover = list(existing.values())
        if leftover:
            self._db.executemany(
                "DELETE FROM files WHERE id = ?",
                [(row["id"],) for row in leftover],
            )
            stats.removed = len(leftover)
        self._db.execute("UPDATE volumes SET scanned_at = ? WHERE id = ?", (stamp, volume_id))
        self._db.commit()
        return stats

    def _read_tags(self, abs_path: str, rel: str) -> tuple[str, str, str, int]:
        try:
            from likesync.tags import read_tags, _from_filename
        except Exception:
            return "", "", "", 0
        try:
            tags = read_tags(abs_path)
        except Exception:
            tags = _from_filename(rel)
        artist = getattr(tags, "best_artist", None) or getattr(tags, "artist", "") or ""
        return artist, getattr(tags, "title", "") or "", getattr(tags, "album", "") or "", int(
            getattr(tags, "track_no", 0) or 0
        )

    def upsert_dest_file(
        self,
        kind: str,
        root: str,
        rel: str,
        size: int,
        mtime: float,
        content_key: str = "",
    ) -> None:
        volume_id = self._ensure_volume(kind, root)
        folded = os.path.normcase(rel)
        suffix = Path(rel).suffix.lower()
        self._db.execute(
            "INSERT INTO files(volume_id, rel, rel_fold, size, mtime, content_key, suffix, seen_at) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?) "
            "ON CONFLICT(volume_id, rel_fold) DO UPDATE SET "
            "rel = excluded.rel, size = excluded.size, mtime = excluded.mtime, "
            "content_key = CASE WHEN excluded.content_key != '' THEN excluded.content_key "
            "ELSE files.content_key END, suffix = excluded.suffix, seen_at = excluded.seen_at",
            (volume_id, rel, folded, size, mtime, content_key or "", suffix, time.time()),
        )
        self._db.commit()

    def forget_dest_file(self, kind: str, root: str, rel: str) -> None:
        info = self.volume_for_root(kind, root)
        if info is None:
            return
        self._db.execute(
            "DELETE FROM files WHERE volume_id = ? AND rel_fold = ?",
            (info.id, os.path.normcase(rel)),
        )
        self._db.commit()

    def key_for(self, kind: str, root: str, rel: str) -> str:
        info = self.volume_for_root(kind, root)
        if info is None:
            return ""
        row = self._db.execute(
            "SELECT content_key FROM files WHERE volume_id = ? AND rel_fold = ?",
            (info.id, os.path.normcase(rel)),
        ).fetchone()
        return row["content_key"] if row else ""

    def rels_for_key(self, kind: str, root: str, content_key: str) -> list[str]:
        if not content_key:
            return []
        info = self.volume_for_root(kind, root)
        if info is None:
            return []
        return [
            row["rel"]
            for row in self._db.execute(
                "SELECT rel FROM files WHERE volume_id = ? AND content_key = ?",
                (info.id, content_key),
            )
        ]


def format_age(scanned_at: float, now: float | None = None) -> str:
    if not scanned_at:
        return "never"
    elapsed = (now or time.time()) - scanned_at
    if elapsed < 60:
        return "just now"
    if elapsed < 3600:
        return f"{int(elapsed // 60)}m ago"
    if elapsed < 86400:
        return f"{int(elapsed // 3600)}h ago"
    return f"{int(elapsed // 86400)}d ago"
