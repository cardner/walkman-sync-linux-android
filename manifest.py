"""Per-volume skip manifest: what we last wrote, and from which source.

A SensMe-tagged dest is a few kilobytes larger than its source, so size+mtime against the
source would recopy forever. The manifest records the source's size/mtime, the dest size we
wrote, and the audio content key. A file is up to date when the dest still matches that row,
or when the dest's stored content key still equals the source's.
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

MANIFEST_NAME = ".sony-sync-manifest.tsv"
PARTIAL_SUFFIX = ".partial"
MTIME_TOLERANCE_SECONDS = 2


@dataclass
class Record:
    source_size: int
    source_mtime: float
    copy_size: int
    content_key: str = ""


@dataclass
class Manifest:
    records: dict[str, Record] = field(default_factory=dict)  # rel_path (as written) -> row
    _by_case: dict[str, str] = field(default_factory=dict)
    _by_key: dict[str, str] = field(default_factory=dict)

    @classmethod
    def path_for(cls, volume_music: str) -> str:
        import storage as storage_mod
        return storage_mod.join(volume_music, MANIFEST_NAME) if volume_music else MANIFEST_NAME

    @classmethod
    def load(cls, volume_music: str) -> "Manifest":
        import storage as storage_mod
        manifest = cls()
        if not volume_music:
            return manifest
        path = cls.path_for(volume_music)
        try:
            text = storage_mod.read_text(path)
        except OSError:
            return manifest
        for line in text.splitlines()[1:]:
            parts = line.split("\t")
            if len(parts) < 4:
                continue
            rel, src_size, src_mtime, copy_size = parts[0], parts[1], parts[2], parts[3]
            key = parts[4] if len(parts) > 4 else ""
            try:
                record = Record(int(src_size), float(src_mtime), int(copy_size), key)
            except ValueError:
                continue
            manifest._insert(rel, record)
        return manifest

    def _insert(self, rel: str, record: Record) -> None:
        self.records[rel] = record
        self._by_case[os.path.normcase(rel)] = rel
        if record.content_key:
            self._by_key[record.content_key] = rel

    def lookup(self, rel: str) -> Record | None:
        original = self._by_case.get(os.path.normcase(rel), rel)
        return self.records.get(original)

    def rel_for_key(self, content_key: str) -> str | None:
        return self._by_key.get(content_key)

    def up_to_date(
        self,
        rel: str,
        source_size: int,
        source_mtime: float,
        dest_size: int | None,
        source_key: str | None = None,
    ) -> bool:
        if dest_size is None:
            return False
        record = self.lookup(rel)
        if record is not None:
            source_ok = (
                record.source_size == source_size
                and source_mtime <= record.source_mtime + MTIME_TOLERANCE_SECONDS
            )
            dest_ok = record.copy_size == dest_size
            if source_ok and dest_ok:
                return True
            if source_key and record.content_key and record.content_key == source_key and dest_ok:
                return True
        if source_key:
            other = self.rel_for_key(source_key)
            if other is not None:
                held = self.records[other]
                if held.copy_size == dest_size or os.path.normcase(other) == os.path.normcase(rel):
                    return held.copy_size == dest_size and bool(held.content_key == source_key)
        return False

    def remember(self, rel: str, source_size: int, source_mtime: float,
                 copy_size: int, content_key: str = "") -> None:
        previous = self.lookup(rel)
        if previous and previous.content_key and previous.content_key in self._by_key:
            if self._by_key[previous.content_key] == (
                self._by_case.get(os.path.normcase(rel), rel)
            ):
                del self._by_key[previous.content_key]
        original = self._by_case.get(os.path.normcase(rel), rel)
        if original != rel and original in self.records:
            del self.records[original]
        self._insert(rel, Record(source_size, source_mtime, copy_size, content_key))

    def forget(self, rel: str) -> None:
        original = self._by_case.pop(os.path.normcase(rel), None)
        if original is None:
            original = rel
        record = self.records.pop(original, None)
        if record and record.content_key and self._by_key.get(record.content_key) == original:
            del self._by_key[record.content_key]

    def save(self, volume_music: str) -> None:
        import storage as storage_mod
        if not volume_music:
            return
        path = self.path_for(volume_music)
        lines = ["# sony-sync manifest\tsource_size\tsource_mtime\tcopy_size\tcontent_key"]
        for rel, record in sorted(self.records.items(), key=lambda item: item[0].lower()):
            lines.append(
                f"{rel}\t{record.source_size}\t{record.source_mtime:.6f}\t"
                f"{record.copy_size}\t{record.content_key}"
            )
        storage_mod.write_text(path, "\n".join(lines) + "\n")
