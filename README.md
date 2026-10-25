# Sony sync

Two things live here, and they are deliberately separate:

| | what it does | run it |
|---|---|---|
| **music sync** (`sync.py`) | plans and copies the library onto the Walkman's two volumes, writes the playlists, removes what no longer belongs, uploads `.scrobbler.log` to Last.fm | `python sync.py` or the **Walkman Sync** shortcut |
| **likes sync** (`likesync/`) | keeps *liked* in step across the Walkman, Last.fm and MusicBee, and writes a **Liked Songs** playlist onto the device | `python -m likesync sync` |

`app.py` is the full-screen front-end both of them share; `tui.py` is the terminal toolkit it is
built from. Everything is standard library only — no pip install, no venv, any Python 3.10+.

```
python3 launch.py --install-shortcut  # Windows: Desktop + Start Menu
                                      # macOS: Desktop .command + ~/Applications .app
python3 launch.py                     # own window if double-clicked; in-place in a terminal
python3 sync.py                       # full-screen UI in the current terminal
python3 sync.py --classic             # the original scrolling menu, unchanged
python3 -m likesync status            # likes only, no UI, read-only
python3 -m unittest discover -s tests
```

Python 3.10+ on PATH (no venv, no pip). After `--install-shortcut`:

* **Windows** — pin **Walkman Sync** from the Start Menu or the desktop. The shortcut uses
  `assets/walkman-sync.ico`. Double-click `launch.pyw` for the same dedicated window without a
  bootstrap console flash. Windows Terminal is used when it is installed; otherwise a plain console.
* **macOS** — double-click **Walkman Sync** on the Desktop (Terminal.app), or drag
  `~/Applications/Walkman Sync.app` to the Dock. Both pick up `assets/walkman-sync.png`. Cinder
  mounts as `/Volumes/WALKMAN`; an Android card in a USB reader is `/Volumes/<label>/Music`. A
  Windows UNC library path (`\\server\share`) is tried as `/Volumes/<share>` when the UNC is missing.

`python3 sync.py` is unchanged for running inside a terminal you already have. MusicBee likes stay
Windows-only.

---

## Android Walkmans (ZX500 / A100, WM1AM2, A300 / ZX700)

Stock W. Music on these players is a different USB world from a Cinder A50. Settings ▸ **Device**
is `cinder` (default) or `android-walkman`.

On `android-walkman`:

* music and relative `.m3u8` playlists go in **`Music/`** (Sony's default folder — do not rename it);
* DSD/AIFF (`.dsf` / `.dff` / `.aiff`) copy with the rest of the library;
* `Liked Songs.m3u8` is written; Cinder like-import files and `cinder_playlists/` are not;
* `.scrobbler.log` upload is skipped (the player has WiFi; stock W. Music does not write that log);
* a last-known drive letter is **not** swept if its volume serial no longer matches the catalog.

How to connect:

1. **microSD in a USB reader** — the usual ZX505 path (16 GB internal). Set destination to **sd**.
   After the copy, on the player: W. Music ▸ Update Database. On a Mac the card appears under
   `/Volumes/<label>/Music`.
2. **ADB** (USB debugging) — if the unit allows it, re-detect mounts; dest URIs look like
   `adb://SERIAL/sdcard/Music`. Works on Windows and macOS when `adb` is on PATH. Override the
   binary with `SONY_SYNC_ADB`.
3. **MTP File Transfer** — Windows only (Explorer's WALKMAN portable device), used when ADB is
   locked. Slower, and skip-if-present is size-only. On a Mac with USB debugging locked, use the
   card reader instead.

Hearts / bookmarks inside W. Music live in Sony's private database and are not written by this
tool. Last.fm and MusicBee likes still land as the **Liked Songs** playlist.

---

## Several Walkmans, one library

The source library and album exclusions are shared. Each plugged player is its own profile,
keyed by volume serial: mounts, dest (internal vs SD), cinder vs android-walkman, and disk
budgets. Plug the Walkman you mean — Sony-sync matches it automatically. That unit's dest catalog
and likes snapshot stay separate, so syncing a ZX505 cannot sweep an A50.

On-device files (`cinder_*`, `Liked Songs.m3u8`, `listen_sources.conf`, `.scrobbler.log`) were
already per disk. Rename the active player in Settings. A first-seen serial toasts
`new Walkman: …`.

---

## The likes sync

### Why it needs three-way state

The Walkman has no WiFi, so it can never call Last.fm itself. Cinder therefore writes a file and a
PC tool carries it — the same shape scrobbling already has:

```
device  /contents/cinder_liked.conf        object ids   (the real store, meaningless off-device)
        /contents/cinder_loved.tsv         artist ⇥ title   ← Cinder exports on every change
        /contents/cinder_liked_import.tsv  artist ⇥ title   → likesync writes, Cinder consumes
        /contents/MUSIC/Liked Songs.m3u8   the playable list on the device
last.fm user.getLovedTracks / track.love / track.unlove
musicbee an exported playlist (and optionally file-tag ratings)
```

A three-way set comparison cannot tell "just loved over here" from "just unliked over there", so
`likesync` keeps a snapshot of each source (`.likesync/state.json`) and diffs each source against
**its own** previous snapshot. The rules that fall out of that:

* **first run is additive** — the union of all three, nothing is ever removed;
* **a source that is not available contributes nothing** — an unplugged Walkman or a Last.fm
  outage is not evidence that anything was unliked;
* **while an import is pending** (Cinder has not merged it yet) the device is additive-only — its
  export still shows the pre-push list, and treating that as removals would push the difference
  back out as unloves;
* **a conflict keeps the like** by default (`conflict: liked`), because losing a hand-curated like
  to an ambiguity is worse than keeping one you meant to drop. `conflict: latest` is the opposite.

### Matching

Everything is keyed on `artist + title`, normalised: case, curly punctuation, `feat.` credits and
re-issue suffixes (`- Remastered 2009`, `(2021 Mix)`, `(Deluxe Edition)`) fold away, while
anything that marks a *different recording* — live, remix, acoustic, demo — stays distinct. When
the exact key misses, the primary artist is tried, then the album-folder artist, which is what
matches Last.fm's `Little Simz feat. Cleo Sol` to the file tagged `Cleo Sol` on a Little Simz
album. On the reference library that resolves **129 of 131** liked tracks; the two that miss are
genuinely not in the library.

The same normalisation exists on the device in `player/cinder-ffi/src/likes.rs` — the two sides
have to agree or nothing lines up.

### Setting it up

1. **Last.fm** — credentials are read from the Walkman first (`listen_sources.conf` at the
   volume root, the same file Cinder / listen_pull uses: `lastfm_user`, `lastfm_api_key`,
   `lastfm_api_secret`, `lastfm_session_key`). Any field that is empty there falls back to
   `.env` (`LASTFM_API_KEY` / `LASTFM_API_SECRET` / `LASTFM_USERNAME` / `LASTFM_SESSION_KEY`).
   Sign in with `python -m likesync login` or the Last.fm screen; that writes `.env`. AudioMuse
   `audiomuse_url` / `audiomuse_token` follow the same device-then-PC rule (`AUDIOMUSE_URL` /
   `AUDIOMUSE_TOKEN` in `.env`). Sony-sync pulls AudioMuse mixes and scores; Navidrome playlists
   stay listen_pull's.
2. **MusicBee** — make a playlist of what you love (an auto-playlist on `Love` or `Rating ≥ 4`
   works), then MusicBee ▸ right-click the playlist ▸ *auto-export as m3u* into the exported
   playlists folder. `likesync` finds anything named `Loved` / `Liked` / `Favourites` on its own,
   or set `musicbee_loved_playlist` explicitly. If MusicBee's own Last.fm love is switched on,
   this step is optional — the loves arrive through Last.fm anyway.
   To read stars from the files instead, set `musicbee_rating_threshold` to e.g. `4`.
3. **The device** — nothing to configure. The `Liked Songs.m3u8` playlist works on any Cinder
   build; the hearts on the device only update on a build that has the liked-import (see below).

Settings live in `likesync.json`; `python -m likesync config key=value` edits them.

### What it writes

| file | when |
|---|---|
| `<device>\MUSIC\Liked Songs.m3u8` | always, per drive, listing only that drive's tracks (a playlist that names a file on the other volume is a dead row) |
| `<device>\cinder_liked_import.tsv` | when `device_import` is on (internal volume only — `cinder_liked.conf` lives at `/contents`) |
| `<playlists>\Liked Songs.m3u8` | when `musicbee_export_playlist` is on |
| Last.fm | `track.love` / `track.unlove`, one call per change, throttled to Last.fm's rate limit |

`sync.py` knows to leave `Liked Songs.m3u8` alone at both ends: it is excluded from the source
playlist scan (otherwise its whole-library span would collapse every album onto one drive) and
from the stale-playlist sweep on the device.

### On the device

Cinder's import (`likes.rs`) replaces the liked set with the resolved rows and renames the file to
`.tsv.done`. It refuses to act on a file with no `# artist` header, or on one whose rows resolve
to nothing — in both cases the file is left in place for the next boot. An empty file *with* the
header is honoured: that means "everything was unliked".

---

## AudioMuse mixes and Channels scores

Sony-sync owns the AudioMuse pull. Navidrome playlists stay with listen_pull. Same
`listen_sources.conf` keys (`audiomuse_url` / `audiomuse_token`), then `.env`
(`AUDIOMUSE_URL` / `AUDIOMUSE_TOKEN`).

The menu item **AudioMuse: mixes + scores** calls `GET /api/playlists` and `GET /api/sync` and
writes Cinder's next-boot imports at the internal volume root (`/contents`), not under `MUSIC/`:

| file | what Cinder does |
|---|---|
| `cinder_playlist_import.tsv` | AudioMuse mixes (`am:` ids). Existing Navidrome rows in that file are kept. |
| `cinder_audiomuse_import.tsv` | Channels scores (energy mapped onto 0–1 arousal). Copied to `cinder_audiomuse.tsv`. |

It does not fetch OpenSubsonic playlists, and it does not write `.m3u8` into `MUSIC/` — Cinder
resolves artist/title/album on boot. Android Walkmans skip this pull (no Cinder import). If
AudioMuse is down, previous import files stay put so Navidrome rows are not wiped.

After the pull, unplug and let Cinder boot so the mixes land in `cinder_playlists/` and the scores
reach Channels.

---

## The UI

```
 WALKMAN SYNC                              walkman connected   last.fm Superwils0
 ┌─ Storage ──────────────────┐ ┌─ This sync ────────────────┐
 │ Internal ███████░░░ 41/54G │ │ Net  +6 albums  +1.2 GB    │
 ...
 › 1  Sync music to the Walkman
   2  Album changes (what moves)
   7  Likes: device ⇄ Last.fm ⇄ MusicBee
 ↑↓ move  ⏎ choose  r rescan  / filter  q quit
```

* one write per frame into the alternate screen buffer — no flicker, and your scrollback survives;
* single keypresses, arrow keys, `/` to filter any list, `esc` to go back;
* scans run on a worker thread with a live phase/ETA readout instead of freezing;
* panels stack and the menu shrinks on a small terminal — the menu is never pushed off-screen;
* CJK and accented titles are measured in display cells, so columns line up.

Every screen is a pure `render_*(…) -> list[str]`, which is why `tests/test_tui.py` can render all
of them headlessly against the real planner.

## Tests

```
python -m unittest discover -s tests      # merge rules, tags, keys, TUI, catalog, android dest
```

The merge-rule tests are the ones that matter: they cover first run, removal propagation, an
absent source, a pending device import, and both conflict policies.
