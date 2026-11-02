# Walkman Sync

One `walkman-sync` binary (Windows `.exe`, macOS) — library copy, likes, scrobbles, and AudioMuse for Sony Walkmans. No Python runtime.

```
cargo build --release
target/release/walkman-sync --install-shortcut   # Windows: Desktop + Start Menu
                                                 # macOS: Desktop .command + ~/Applications .app
target/release/walkman-sync                      # dashboard in the current terminal
cargo test
```

After `--install-shortcut`:

* **Windows** — pin **Walkman Sync** from the Start Menu or the desktop. The shortcut uses `walkman-sync.ico` next to the exe (extracted from the binary on install). Double-click opens a console. Windows Terminal is used when `wt.exe` is on PATH; otherwise a plain console.
* **macOS** — double-click **Walkman Sync** on the Desktop (Terminal.app), or drag `~/Applications/Walkman Sync.app` to the Dock. Both pick up `walkman-sync.png`. Cinder mounts as `/Volumes/WALKMAN`; an Android card in a USB reader is `/Volumes/<label>/Music`. A Windows UNC library path (`\\server\share`) is tried as `/Volumes/<share>` when the UNC is missing.

Re-running `--install-shortcut` overwrites the old Python `.lnk` / `.command` / `.app` at the same names, so existing pins keep working.

Settings live in `sync.json` (nested `devices` map keyed by volume serial). The file catalog is `.sony-sync/catalog.sqlite`. Likes state is `.likesync/state.json`. Same schema as the old Python app, so a library is not re-hashed from scratch.

MusicBee likes stay Windows-only. Helper scripts `audit_scrobbles.py`, `paste_lastfm.py`, and `tracklist_scrobbler.py` are still Python and are not part of the app.

---

## Scan architecture

First index is slow if every MP3 is hashed end-to-end. The dashboard is slow if every launch walks the Walkman over USB. The binary:

* **MP3 identity** `mp3h-`: ID3 skip + first MPEG frame + Xing/Info/VBRI (or audio length). Existing `mp3-` rows stay until size/mtime change. Dest MP3 keys come from the copy manifest only. Dest FLAC: STREAMINFO header only.
* **Enumerate vs enrich:** walk `rel/size/mtime` with no audio open; open headers only for new/changed source files (and dest FLAC missing a key).
* **Dashboard:** walk the **source** library (**audio only**). Nested album playlists, cover art, AppleDouble `._*`, and recycle/`@eaDir` trees are not cataloged. Playlists come from the playlist folder rewrite onto MUSIC root. **Skip dest walk** iff the plugged serial already has a catalog (`volume_for_root(kind, live_path).file_count > 0`). First plug of Walkman B always walks. Card vs `adb://` / `mtp://` stay different serials.
* **Walk dest** on Index Walkman, Sync, preview, Cleanup, `r` after `i`/`w`. Dest catalog is audio-only. Sync/Cleanup also list leftover nested sidecars (cover art, `.cue`/`.nfo`, nested `.m3u8`, `.partial`) for deletion. Root playlists, `cinder_*`, likes, and scrobble logs stay. After `i`/`w`, the report rebuilds from sqlite.
* **Parallel enrich:** rayon on a local source disk; serial on UNC/ADB and on dest USB.

No AcoustID. No dest MP3 hash over USB.

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
   `adb://SERIAL/sdcard/Music`. Walkman Sync fetches Google platform-tools into
   `%LOCALAPPDATA%\Walkman Sync\platform-tools` (Windows) or
   `~/Library/Application Support/Walkman Sync/platform-tools` (macOS) on first use, so `adb`
   does not need to be on PATH. A `platform-tools` folder next to the exe or in the app
   directory is used if present. Override with `SONY_SYNC_ADB`; set `SONY_SYNC_NO_ADB_FETCH=1`
   to skip the download. USB debugging must still be enabled on the player, and this computer
   authorized. Google's Android SDK license applies to the fetched tools.
3. **MTP File Transfer** — Windows only (Explorer's WALKMAN portable device), used when ADB is
   locked or `adb devices` sees nothing. Slower, and skip-if-present is size-only. On a Mac
   with USB debugging locked, use the card reader instead.

Hearts / bookmarks inside W. Music live in Sony's private database and are not written by this
tool. Last.fm and MusicBee likes still land as the **Liked Songs** playlist.

---

## Stock Walkmans (NW-WM1A first)

Settings ▸ **Device** `stock` (alias `wm1a`) is a non-Cinder mass-storage player. The NW-WM1A is the one this path is written for. NW-A55, ZX300, and WM1Z use the same code later; they do not have their own checks yet.

A volume with `DevIcon.fil` or `DevLogo.fil` and no Cinder files is `stock`. Cinder files still win. `.scrobbler.log` does not select a kind. A card that only has `MUSIC` does not downgrade a saved `stock` or `cinder` profile.

On `stock`:

* music goes in `MUSIC/`;
* library playlists and AudioMuse mixes are relative `.m3u` files in that folder (`Mix.m3u8` is written as `Mix.m3u`). `cinder_playlists/` is not written;
* likes round-trip through `MUSIC/Liked Songs.m3u`. That is a playlist, not a bookmark heart. A missing file does not unlike anything. A file that still matches the last write does not unlike anything either;
* AudioMuse scores are not written. The WM1A has no Channels screen;
* plays upload from `.scrobbler.log` at the internal volume root. Stock firmware does not create that file. The on-device recorder is a separate package and is not installed by Walkman Sync. Plug in the player itself; a card in a reader does not carry the log.

After a copy, unplug and let the player rebuild its library.

---

## Several Walkmans, one library

The source library and album exclusions are shared. Each plugged player is its own profile,
keyed by volume serial: mounts, dest (internal vs SD), cinder vs android-walkman, and disk
budgets. Plug the Walkman you mean — Walkman Sync matches it automatically. That unit's dest catalog
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
likes keep a snapshot of each source (`.likesync/state.json`) and diffs each source against
**its own** previous snapshot. The device snapshot is keyed `device:<serial>`. The rules that fall out of that:

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
anything that marks a *different recording* — live, remix, acoustic, demo — stays distinct.

### Setting it up

1. **Last.fm** — credentials are read from the Walkman first (`listen_sources.conf` at the
   volume root: `lastfm_user`, `lastfm_api_key`, `lastfm_api_secret`, `lastfm_session_key`). Any
   field that is empty there falls back to `.env`. Sign in from the Last.fm screen. AudioMuse
   `audiomuse_url` / `audiomuse_token` follow the same device-then-PC rule.
2. **MusicBee** (Windows) — export a Loved/Liked playlist as `.m3u` into the playlists folder.
3. **The device** — nothing to configure. The `Liked Songs.m3u8` playlist works on any Cinder
   build; the hearts on the device only update on a build that has the liked-import.

### What it writes

| file | when |
|---|---|
| `<device>\MUSIC\Liked Songs.m3u8` | always, per dest drive, listing only that drive's tracks |
| `<device>\cinder_liked_import.tsv` | when `device_import` is on (internal volume only) |
| `<playlists>\Liked Songs.m3u8` | when `musicbee_export_playlist` is on |
| Last.fm | `track.love` / `track.unlove`, one call per change, throttled to Last.fm's rate limit |

### On the device

Cinder's import (`likes.rs`) replaces the liked set with the resolved rows and renames the file to
`.tsv.done`. It refuses to act on a file with no `# artist` header, or on one whose rows resolve
to nothing — in both cases the file is left in place for the next boot. An empty file *with* the
header is honoured: that means "everything was unliked".

---

## AudioMuse mixes and Channels scores

Walkman Sync owns the AudioMuse pull. Navidrome playlists stay with listen_pull. Same
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
 │ Internal 41G dest          │ │ +12 add  ~3 modify  -4 remove │
 ...
 › 1  Sync music to the Walkman
   2  Plan (added / modified / removed)
   r  Refresh plan
   5  Likes: device ⇄ Last.fm ⇄ MusicBee
 ↑↓ move  ⏎ choose  r refresh plan  q quit
```

Hotkeys: `1` sync, `2` plan, `r` refresh plan (walks dest), `5` likes, `i`/`w` index, `s` settings, `l` Last.fm, `q` quit.

### Main Menu

<img width="1113" height="625" alt="image" src="https://github.com/user-attachments/assets/e2e680d5-c4fb-4025-8115-660ae9c05965" />


### Transfer in progress 

<img width="1111" height="622" alt="image" src="https://github.com/user-attachments/assets/b66b0d37-4ef8-468d-9bef-222853344d28" />

### Settings

<img width="1110" height="624" alt="image" src="https://github.com/user-attachments/assets/52fae09d-e5e7-444d-9a30-51e38e211ebf" />



## Tests

```
cargo test
```

Covered without live hardware: two serials dest-skip vs must-walk, `mp3h-` stability across an ID3-only change, enumerate-only vs enrich, nested `sync.json` migration, `--install-shortcut` IconLocation / macOS `.command` paths (temp dirs), and audio-only walks that skip cover art / nested playlists / AppleDouble while marking leftover dest sidecars stale.
