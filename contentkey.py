"""Tag-independent audio identity for skip-if-present.

A retag changes the file's size and mtime but not the PCM. FLAC already stores an MD5 of the
decoded samples in STREAMINFO; an MP3 is hashed from the first MPEG frame to (optional) ID3v1.
The key is cached on the PC by (path, size, mtime) so a rescan does not re-read every header.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path

CACHE_PATH = Path(__file__).resolve().parent / ".sony-sync" / "contentkeys.json"


def flac_header_key(path: str) -> str | None:
    """STREAMINFO-only identity. Safe to run on USB; never reads audio frames."""
    try:
        with open(path, "rb") as handle:
            if handle.read(4) != b"fLaC":
                return None
            handle.seek(0)
            return _flac_key(handle, allow_raw=False)
    except OSError:
        return None


def content_key(path: str) -> str | None:
    """Audio identity, or None for a container this module does not handle."""
    try:
        with open(path, "rb") as handle:
            magic = handle.read(4)
            handle.seek(0)
            if magic == b"fLaC":
                return _flac_key(handle)
            if magic[:3] == b"ID3" or magic[:2] == b"\xff\xfb" or magic[:2] == b"\xff\xfa":
                return _mp3_key(handle)
    except OSError:
        return None
    return None


def _flac_key(handle, allow_raw: bool = True) -> str | None:
    if handle.read(4) != b"fLaC":
        return None
    audio_offset = 4
    md5 = b""
    samples = 0
    while True:
        header = handle.read(4)
        if len(header) < 4:
            return None
        last = header[0] & 0x80
        block_type = header[0] & 0x7F
        length = int.from_bytes(header[1:4], "big")
        body = handle.read(length)
        audio_offset += 4 + length
        if block_type == 0 and len(body) >= 34:
            bits = int.from_bytes(body[10:18], "big")
            samples = bits & ((1 << 36) - 1)
            md5 = body[18:34]
        if last:
            break
    if md5 and any(md5):
        return f"flac-{md5.hex()}-{samples}"
    if not allow_raw:
        return None
    remaining = _remaining(handle, audio_offset)
    digest = _hash_from(handle, audio_offset, remaining)
    return f"flacraw-{digest}-{remaining}"


def _mp3_key(handle) -> str | None:
    start = _id3v2_len(handle)
    handle.seek(0, os.SEEK_END)
    total = handle.tell()
    end = total
    if total >= start + 128:
        handle.seek(total - 128)
        if handle.read(3) == b"TAG":
            end -= 128
    length = max(end - start, 0)
    if length <= 0:
        return None
    handle.seek(start)
    head = handle.read(2)
    if len(head) < 2 or not (head[0] == 0xFF and head[1] & 0xE0 == 0xE0):
        # ID3-only or not an MP3 we recognise.
        if start == 0:
            return None
        handle.seek(start)
        digest = _hash_from(handle, start, length)
        return f"mp3-{digest}-{length}"
    handle.seek(start)
    digest = _hash_from(handle, start, length)
    return f"mp3-{digest}-{length}"


def _id3v2_len(handle) -> int:
    handle.seek(0)
    head = handle.read(10)
    if len(head) < 10 or head[:3] != b"ID3":
        return 0
    size = (
        ((head[6] & 0x7F) << 21)
        | ((head[7] & 0x7F) << 14)
        | ((head[8] & 0x7F) << 7)
        | (head[9] & 0x7F)
    )
    return 10 + size


def _remaining(handle, offset: int) -> int:
    handle.seek(0, os.SEEK_END)
    return max(handle.tell() - offset, 0)


def _hash_from(handle, offset: int, length: int) -> str:
    handle.seek(offset)
    hasher = hashlib.blake2s(digest_size=8)
    left = length
    while left > 0:
        chunk = handle.read(min(1 << 16, left))
        if not chunk:
            break
        hasher.update(chunk)
        left -= len(chunk)
    return hasher.hexdigest()


class KeyCache:
    """(path, size, mtime) → content key. Saved whole; a torn write is just a slower next scan."""

    def __init__(self, path: Path = CACHE_PATH) -> None:
        self.path = path
        self._rows: dict[str, dict] = {}
        self._dirty = False
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(payload, dict):
                self._rows = payload
        except (OSError, ValueError):
            self._rows = {}

    def get(self, path: str, size: int, mtime: float) -> str | None:
        row = self._rows.get(os.path.normcase(path))
        if not row:
            return None
        if int(row.get("size", -1)) != size:
            return None
        if abs(float(row.get("mtime", 0)) - mtime) > 2:
            return None
        key = row.get("key")
        return key if isinstance(key, str) and key else None

    def put(self, path: str, size: int, mtime: float, key: str) -> None:
        self._rows[os.path.normcase(path)] = {"size": size, "mtime": mtime, "key": key}
        self._dirty = True

    def resolve(self, path: str, size: int, mtime: float) -> str | None:
        cached = self.get(path, size, mtime)
        if cached is not None:
            return cached
        key = content_key(path)
        if key:
            self.put(path, size, mtime, key)
        return key

    def save(self) -> None:
        if not self._dirty:
            return
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._rows), encoding="utf-8")
        os.replace(tmp, self.path)
        self._dirty = False
