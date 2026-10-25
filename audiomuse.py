"""AudioMuse mixes + Channels scores — the Cinder import contract listen_pull uses.

Sony-sync owns AudioMuse. Navidrome playlists stay listen_pull's. This module fetches
`GET /api/playlists` and `GET /api/sync`, then writes:

* `cinder_playlist_import.tsv` at the internal volume root (AudioMuse half replaced,
  Navidrome rows kept)
* `cinder_audiomuse_import.tsv` (full score dump; Cinder copies it to `cinder_audiomuse.tsv`)

Stdlib only. Credentials come from `listen_sources.conf` then `.env`.
"""
from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from urllib import error, parse, request

import storage as storage_mod
import walkman as walkman_mod

PLAYLIST_IMPORT = "cinder_playlist_import.tsv"
SCORE_IMPORT = "cinder_audiomuse_import.tsv"
SCORE_STORE = "cinder_audiomuse.tsv"
PLAYLIST_HEADER = "# navidrome playlists — sony-sync\n"
SCORE_HEADER = "# audiomuse scores — sony-sync\n"
AUDIOMUSE_ID_PREFIX = "am:"
MAX_TRACKS = 4096
ENERGY_MIN = 0.01
ENERGY_MAX = 0.15
MOOD_FLOOR = 0.05
PAGE_LIMIT = 500
HTTP_TIMEOUT = 60
USER_AGENT = "sony-sync/audiomuse (+walkman)"


class AudioMuseError(RuntimeError):
    pass


@dataclass
class Track:
    artist: str
    title: str
    album: str = ""


@dataclass
class Playlist:
    id: str
    kind: str
    name: str
    tracks: list[Track] = field(default_factory=list)


@dataclass
class Score:
    id: str
    fp: str
    artist: str
    title: str
    album: str
    valence: float
    arousal: float
    bpm: float
    dance: float
    moods: str


@dataclass
class PullResult:
    skipped: str = ""
    error: str = ""
    mixes: list[Playlist] = field(default_factory=list)
    scores: list[Score] = field(default_factory=list)
    navidrome_kept: int = 0
    playlist_body: str = ""
    score_body: str = ""
    playlist_path: str = ""
    score_path: str = ""
    written: bool = False
    logs: list[str] = field(default_factory=list)

    def log(self, message: str) -> None:
        self.logs.append(message)


def normalize_base(url: str) -> str:
    text = (url or "").strip().rstrip("/")
    if not text:
        return ""
    if text.startswith(("http://", "https://")):
        return text
    return "http://" + text


def clean(value: str) -> str:
    text = "".join(ch for ch in (value or "") if ch not in "\t\n\r")
    return " ".join(text.split())


def classify(name: str) -> tuple[str, str]:
    text = clean(name)
    if text == "Sonic Fingerprint by AudioMuse-AI":
        return "fingerprint", text
    for suffix, kind in (
        ("_automatic", "automatic"),
        ("_instant", "instant"),
        ("_radio", "radio"),
    ):
        if text.endswith(suffix):
            rest = text[: -len(suffix)].strip()
            if rest:
                return kind, rest
    return "playlist", text or "Playlist"


def audiomuse_id(name: str) -> str:
    return AUDIOMUSE_ID_PREFIX + clean(name)


def is_audiomuse_id(list_id: str) -> bool:
    return list_id.startswith(AUDIOMUSE_ID_PREFIX)


def _as_str(value) -> str:
    return value if isinstance(value, str) else ""


def _json_str(obj: dict, keys: tuple[str, ...]) -> str:
    for key in keys:
        value = obj.get(key)
        if isinstance(value, str):
            return value
    return ""


def playlists_from_json(body: str) -> list[Playlist]:
    value = json.loads(body)
    if not isinstance(value, dict):
        raise AudioMuseError("audiomuse playlists: expected an object")
    if value and all(not isinstance(entry, list) for entry in value.values()):
        raise AudioMuseError("audiomuse playlists: response had no playlist arrays")
    out: list[Playlist] = []
    seen: dict[str, int] = {}
    for name, tracks in value.items():
        if not isinstance(tracks, list):
            continue
        kind, display = classify(name)
        list_id = audiomuse_id(name)
        if list_id == AUDIOMUSE_ID_PREFIX:
            continue
        count = seen.get(list_id, 0) + 1
        seen[list_id] = count
        if count > 1:
            list_id = f"{list_id}#{count}"
        parsed: list[Track] = []
        for item in tracks:
            if not isinstance(item, dict):
                continue
            track = _playlist_track(item)
            if track is not None:
                parsed.append(track)
            if len(parsed) >= MAX_TRACKS:
                break
        out.append(Playlist(id=list_id, kind=kind, name=display, tracks=parsed))
    return out


def _playlist_track(item: dict) -> Track | None:
    artist = clean(_json_str(item, ("author", "artist", "albumartist")))
    title = clean(_json_str(item, ("title", "name")))
    if not artist or not title:
        return None
    return Track(artist=artist, title=title, album=clean(_json_str(item, ("album",))))


def _as_float(value) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def _parse_pairs(raw: str) -> dict[str, float]:
    out: dict[str, float] = {}
    for part in (raw or "").split(","):
        if ":" not in part:
            continue
        name, score = part.split(":", 1)
        name = name.strip().casefold()
        if not name:
            continue
        try:
            out[name] = float(score.strip())
        except ValueError:
            continue
    return out


def mood_labels(raw: str) -> str:
    labels: list[str] = []
    for part in (raw or "").split(","):
        if ":" not in part:
            continue
        name, score = part.split(":", 1)
        name = name.strip().lower()
        try:
            n = float(score.strip())
        except ValueError:
            continue
        if n >= MOOD_FLOOR and name:
            labels.append(name)
    return ",".join(labels)


def map_track(track: dict) -> Score | None:
    artist = clean(_as_str(track.get("artist")))
    title = clean(_as_str(track.get("title")))
    if not artist or not title:
        return None
    tempo = _as_float(track.get("tempo"))
    energy = _as_float(track.get("energy"))
    if tempo is None or energy is None:
        return None
    features = _parse_pairs(_as_str(track.get("other_features")))
    happy = features.get("happy", 0.0)
    sad = features.get("sad", 0.0)
    dance = features.get("danceable", 0.0)
    arousal = (energy - ENERGY_MIN) / (ENERGY_MAX - ENERGY_MIN)
    valence = (happy - sad + 1.0) / 2.0
    return Score(
        id=_as_str(track.get("id")),
        fp=_as_str(track.get("fp")),
        artist=artist,
        title=title,
        album=clean(_as_str(track.get("album"))),
        valence=min(max(valence, 0.0), 1.0),
        arousal=min(max(arousal, 0.0), 1.0),
        bpm=tempo,
        dance=min(max(dance, 0.0), 1.0),
        moods=mood_labels(_as_str(track.get("mood_vector"))),
    )


def scores_from_json(body: str) -> tuple[list[Score], bool]:
    value = json.loads(body)
    raw_tracks = value.get("tracks") if isinstance(value, dict) else None
    tracks = raw_tracks if isinstance(raw_tracks, list) else []
    rows: list[Score] = []
    for item in tracks:
        if not isinstance(item, dict):
            continue
        score = map_track(item)
        if score is not None:
            rows.append(score)
    has_more = bool(value.get("has_more")) if isinstance(value, dict) else False
    return rows, has_more


def payload_query(page: int) -> str:
    return parse.urlencode({
        "include_embeddings": "false",
        "page": page,
        "limit": PAGE_LIMIT,
    })


def render_playlists(lists: list[Playlist]) -> str:
    lines = [
        PLAYLIST_HEADER.rstrip("\n"),
        "# playlist\tid\tkind\tname",
        "# track\tid\tartist\ttitle\talbum",
    ]
    for item in lists:
        lines.append(f"playlist\t{clean(item.id)}\t{item.kind}\t{clean(item.name)}")
        for track in item.tracks[:MAX_TRACKS]:
            lines.append(
                f"track\t{clean(item.id)}\t{track.artist}\t{track.title}\t{track.album}"
            )
    return "\n".join(lines) + "\n"


def parse_playlists(body: str) -> list[Playlist]:
    lists: list[Playlist] = []
    by_id: dict[str, int] = {}
    for raw in (body or "").splitlines():
        line = raw.rstrip("\r")
        if not line or line.startswith("#"):
            continue
        cols = line.split("\t")
        kind_row = cols[0] if cols else ""
        if kind_row == "playlist":
            list_id = clean(cols[1] if len(cols) > 1 else "")
            kind = cols[2] if len(cols) > 2 else "playlist"
            name = clean(cols[3] if len(cols) > 3 else "")
            if not list_id or list_id in by_id:
                continue
            by_id[list_id] = len(lists)
            lists.append(Playlist(id=list_id, kind=kind or "playlist", name=name))
        elif kind_row == "track":
            list_id = clean(cols[1] if len(cols) > 1 else "")
            artist = clean(cols[2] if len(cols) > 2 else "")
            title = clean(cols[3] if len(cols) > 3 else "")
            album = clean(cols[4] if len(cols) > 4 else "")
            if not artist or not title:
                continue
            index = by_id.get(list_id)
            if index is not None:
                lists[index].tracks.append(Track(artist=artist, title=title, album=album))
    return lists


def split_rendered(body: str) -> tuple[list[Playlist], list[Playlist]]:
    navidrome: list[Playlist] = []
    audiomuse: list[Playlist] = []
    for item in parse_playlists(body):
        if is_audiomuse_id(item.id):
            audiomuse.append(item)
        else:
            navidrome.append(item)
    return navidrome, audiomuse


def without_names_covered_by(navidrome: list[Playlist], audiomuse: list[Playlist]) -> list[Playlist]:
    covered = {item.name.casefold() for item in audiomuse}
    if not covered:
        return list(navidrome)
    return [item for item in navidrome if item.name.casefold() not in covered]


def merge_playlist_import(previous: str, incoming: list[Playlist]) -> list[Playlist]:
    navidrome, _old_am = split_rendered(previous)
    navidrome = without_names_covered_by(navidrome, incoming)
    return navidrome + list(incoming)


def render_scores(rows: list[Score]) -> str:
    lines = [
        SCORE_HEADER.rstrip("\n"),
        "# artist\ttitle\talbum\tvalence\tarousal\tbpm\tdance\tmoods",
    ]
    for row in rows:
        lines.append(
            f"{row.artist}\t{row.title}\t{row.album}\t"
            f"{row.valence:.4f}\t{row.arousal:.4f}\t{row.bpm:.4f}\t"
            f"{row.dance:.4f}\t{row.moods}"
        )
    return "\n".join(lines) + "\n"


def http_get(url: str, token: str = "") -> str:
    headers = {"User-Agent": USER_AGENT}
    if token.strip():
        headers["Authorization"] = f"Bearer {token.strip()}"
    req = request.Request(url, headers=headers)
    try:
        with request.urlopen(req, timeout=HTTP_TIMEOUT) as response:
            return response.read().decode("utf-8")
    except error.HTTPError as exc:
        raise AudioMuseError(f"HTTP {exc.code} from AudioMuse") from exc
    except (error.URLError, OSError, TimeoutError) as exc:
        raise AudioMuseError(f"AudioMuse request failed: {exc}") from exc


def fetch_playlists(base: str, token: str = "") -> list[Playlist]:
    url = f"{normalize_base(base)}/api/playlists"
    try:
        return playlists_from_json(http_get(url, token))
    except json.JSONDecodeError as exc:
        raise AudioMuseError(f"audiomuse playlists: invalid JSON ({exc})") from exc


def fetch_scores(base: str, token: str = "",
                 progress=None) -> list[Score]:
    """Full catalogue dump. A partial TSV would replace Cinder's on-device store."""
    root = normalize_base(base)
    rows: list[Score] = []
    page = 1
    while page <= 10_000:
        if progress:
            progress(f"AudioMuse scores page {page}")
        try:
            batch, more = scores_from_json(http_get(f"{root}/api/sync?{payload_query(page)}", token))
        except json.JSONDecodeError as exc:
            raise AudioMuseError(f"audiomuse scores: invalid JSON ({exc})") from exc
        rows.extend(batch)
        if not more:
            return rows
        page += 1
    raise AudioMuseError("audiomuse sync did not end")


def configured(config) -> bool:
    return bool((getattr(config, "audiomuse_url", "") or os.environ.get("AUDIOMUSE_URL", "")).strip())


def origin_label() -> str:
    return "Walkman" if os.environ.get("WALKMAN_SYNC_AUDIOMUSE_FROM") == "device" else ".env"


def compact_count(n: int) -> str:
    if n >= 1000:
        return f"{n / 1000:.1f}k"
    return str(n)


def _count_score_rows(body: str) -> int:
    return sum(1 for line in (body or "").splitlines() if line.strip() and not line.startswith("#"))


def existing_counts(config) -> tuple[int | None, int | None]:
    """Mix / score counts already on the volume, if the import files are readable."""
    root = import_root(config)
    if not root:
        return None, None
    mixes: int | None = None
    scores: int | None = None
    playlist_path = storage_mod.join(root, PLAYLIST_IMPORT)
    try:
        if storage_mod.exists(playlist_path):
            _nd, audiomuse_lists = split_rendered(storage_mod.read_text(playlist_path))
            mixes = len(audiomuse_lists)
    except OSError:
        pass
    for name in (SCORE_IMPORT, SCORE_STORE):
        path = storage_mod.join(root, name)
        try:
            if storage_mod.exists(path):
                scores = _count_score_rows(storage_mod.read_text(path))
                break
        except OSError:
            continue
    return mixes, scores


def status_rows(config) -> list[str]:
    if getattr(config, "is_android", False):
        return ["skipped on android-walkman"]
    if not configured(config):
        return ["not configured"]
    origin = origin_label()
    mixes, scores = existing_counts(config)
    if mixes is None and scores is None:
        return [f"configured  {origin}", "no import on the Walkman yet"]
    mix_text = f"{mixes} mixes" if mixes is not None else "mixes —"
    score_text = f"{compact_count(scores)} scores" if scores is not None else "scores —"
    return [f"{mix_text}  {score_text}  {origin}"]


def preview_rows(result: PullResult) -> list[str]:
    if result.skipped:
        return [result.skipped]
    if result.error:
        return [result.error, "", "Previous import files were left in place."]
    track_total = sum(len(item.tracks) for item in result.mixes)
    rows = [
        f"{len(result.mixes)} mix(es), {track_total} track(s)",
        f"{len(result.scores)} score(s)",
        f"{result.navidrome_kept} Navidrome list(s) kept",
        "",
    ]
    for item in result.mixes:
        rows.append(f"  {item.kind:<12} {item.name}  ({len(item.tracks)})")
    if result.written:
        rows += ["", f"wrote {result.playlist_path}", f"wrote {result.score_path}"]
    return rows


def import_root(config) -> str:
    drive = getattr(config, "internal_drive", "") or ""
    if not drive:
        return ""
    return walkman_mod.volume_root(drive)


def prepare(config, progress=None) -> PullResult:
    """Fetch mixes and scores. Does not write. On HTTP failure, previous imports stay put."""
    result = PullResult()
    if getattr(config, "is_android", False):
        result.skipped = "android-walkman — Cinder imports are not used"
        result.log(result.skipped)
        return result
    url = (getattr(config, "audiomuse_url", "") or os.environ.get("AUDIOMUSE_URL", "")).strip()
    token = (getattr(config, "audiomuse_token", "") or os.environ.get("AUDIOMUSE_TOKEN", "")).strip()
    if not url:
        result.skipped = "AudioMuse URL is empty (listen_sources.conf / .env)"
        result.log(result.skipped)
        return result
    root = import_root(config)
    if not root:
        result.skipped = "Walkman internal volume is not connected"
        result.log(result.skipped)
        return result
    result.playlist_path = storage_mod.join(root, PLAYLIST_IMPORT)
    result.score_path = storage_mod.join(root, SCORE_IMPORT)
    try:
        if progress:
            progress("Fetching AudioMuse playlists")
        result.mixes = fetch_playlists(url, token)
        if progress:
            progress("Fetching AudioMuse scores")
        result.scores = fetch_scores(url, token, progress=progress)
    except AudioMuseError as exc:
        result.error = str(exc)
        result.log(result.error)
        return result
    previous = ""
    try:
        if storage_mod.exists(result.playlist_path):
            previous = storage_mod.read_text(result.playlist_path)
    except OSError:
        previous = ""
    combined = merge_playlist_import(previous, result.mixes)
    result.navidrome_kept = sum(1 for item in combined if not is_audiomuse_id(item.id))
    result.playlist_body = render_playlists(combined)
    result.score_body = render_scores(result.scores)
    result.log(
        f"{len(result.mixes)} mix(es), {sum(len(item.tracks) for item in result.mixes)} track(s); "
        f"{result.navidrome_kept} Navidrome list(s) kept"
    )
    result.log(f"{len(result.scores)} score(s)")
    return result


def commit(preview: PullResult, *, dry_run: bool = False) -> PullResult:
    """Write the bodies from `prepare`. No-op when skipped, failed, or dry-run."""
    if preview.skipped or preview.error:
        return preview
    if dry_run:
        preview.log("dry run — imports not written")
        preview.written = False
        return preview
    try:
        storage_mod.write_text(preview.playlist_path, preview.playlist_body)
        storage_mod.write_text(preview.score_path, preview.score_body)
    except OSError as exc:
        preview.error = f"could not write import files: {exc}"
        preview.log(preview.error)
        preview.written = False
        return preview
    preview.written = True
    preview.log(f"wrote {preview.playlist_path}")
    preview.log(f"wrote {preview.score_path}")
    return preview


def pull(config, *, dry_run: bool = False, progress=None) -> PullResult:
    preview = prepare(config, progress=progress)
    return commit(preview, dry_run=dry_run)
