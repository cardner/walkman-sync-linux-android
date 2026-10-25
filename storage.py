"""Dest I/O: local filesystem, adb://, or mtp://.

The catalog, copy path, and likes writer call these helpers instead of os.path so an
Android Walkman can be a card in a reader (filesystem), USB debugging (ADB), or Explorer's
WALKMAN portable device (MTP). Source library paths stay ordinary Windows paths.
"""
from __future__ import annotations

import os
import re
import shutil
import subprocess
import tempfile
from collections.abc import Callable

ADB_SCHEME = "adb://"
MTP_SCHEME = "mtp://"
ProgressFn = Callable[[int, int, int], None]


def is_remote(path: str) -> bool:
    return bool(path) and path.startswith((ADB_SCHEME, MTP_SCHEME))


def scheme(path: str) -> str:
    if path.startswith(ADB_SCHEME):
        return "adb"
    if path.startswith(MTP_SCHEME):
        return "mtp"
    return "fs"


def norm(path: str) -> str:
    if is_remote(path):
        return path.replace("\\", "/").rstrip("/")
    return os.path.normcase(os.path.normpath(path))


def join(base: str, *parts: str) -> str:
    extra = [part.replace("\\", "/").strip("/") for part in parts if part]
    if is_remote(base):
        if not extra:
            return base.rstrip("/")
        return base.replace("\\", "/").rstrip("/") + "/" + "/".join(extra)
    return os.path.join(base, *parts) if extra else base


def relative(path: str, root: str) -> str:
    if is_remote(root) or is_remote(path):
        folded = norm(path)
        base = norm(root)
        if folded == base:
            return ""
        if folded.startswith(base + "/"):
            return folded[len(base) + 1:].replace("/", os.sep)
        return os.path.basename(path.replace("\\", "/"))
    return os.path.normpath(os.path.relpath(path, root))


def parent(path: str) -> str:
    if is_remote(path):
        folded = path.replace("\\", "/").rstrip("/")
        head, sep, tail = folded.rpartition("/")
        if not sep:
            return path
        # Keep scheme://id even if we are at the storage root.
        if head.endswith(":/") or head.endswith("://") or head.count("/") <= 2:
            return folded
        return head
    return os.path.dirname(path)


def isdir(path: str) -> bool:
    if not path:
        return False
    kind = scheme(path)
    if kind == "adb":
        return _Adb(path).isdir()
    if kind == "mtp":
        return _Mtp(path).isdir()
    return os.path.isdir(path)


def exists(path: str) -> bool:
    if not path:
        return False
    kind = scheme(path)
    if kind == "adb":
        return _Adb(path).exists()
    if kind == "mtp":
        return _Mtp(path).exists()
    return os.path.exists(path)


def serial(path: str) -> str:
    if not path:
        return ""
    kind = scheme(path)
    if kind == "adb":
        return _Adb(path).serial()
    if kind == "mtp":
        return _Mtp(path).serial()
    import walkman as walkman_mod
    return walkman_mod.get_volume_serial(path)


def label(path: str) -> str:
    if not path:
        return ""
    kind = scheme(path)
    if kind == "adb":
        return "adb"
    if kind == "mtp":
        return "WALKMAN"
    import walkman as walkman_mod
    return walkman_mod.volume_label_for(path)


def disk_usage(path: str) -> tuple[int, int] | None:
    if not path:
        return None
    kind = scheme(path)
    if kind == "adb":
        return _Adb(path).disk_usage()
    if kind == "mtp":
        return _Mtp(path).disk_usage()
    try:
        usage = shutil.disk_usage(path)
        return usage.total, usage.free
    except OSError:
        return None


def walk_files(root: str, *, dest: bool = False) -> list[tuple[str, int, float]]:
    kind = scheme(root)
    if kind == "adb":
        return _Adb(root).walk_files(dest=dest)
    if kind == "mtp":
        return _Mtp(root).walk_files(dest=dest)
    return _walk_fs(root, dest=dest)


def _walk_fs(root: str, *, dest: bool = False) -> list[tuple[str, int, float]]:
    entries: list[tuple[str, int, float]] = []
    if not os.path.isdir(root):
        return entries
    skip = {
        ".sony-sync-manifest.tsv", "liked songs.m3u8", "liked songs.m3u",
        ".scrobbler.log", "scrobble.logs", "cinder_playlists", "cinder_palettes",
        "listen_sources.conf", "listen_sync_state.json",
    }
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            with os.scandir(current) as iterator:
                for entry in iterator:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            if dest and current == root and (
                                entry.name.lower().startswith(("cinder_", "listen_"))
                                or entry.name.lower() in skip
                            ):
                                continue
                            stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            name = entry.name.lower()
                            if dest and (
                                name.startswith(("cinder_", "listen_"))
                                or name in skip
                                or (current == root and name == ".sony-sync-manifest.tsv")
                            ):
                                continue
                            stat_result = entry.stat()
                            rel = os.path.normpath(os.path.relpath(entry.path, root))
                            entries.append((rel, stat_result.st_size, stat_result.st_mtime))
                    except OSError:
                        continue
        except OSError:
            continue
    return entries


def makedirs(path: str) -> None:
    kind = scheme(path)
    if kind == "adb":
        _Adb(path).makedirs()
        return
    if kind == "mtp":
        _Mtp(path).makedirs()
        return
    os.makedirs(path, exist_ok=True)


def remove(path: str) -> None:
    kind = scheme(path)
    if kind == "adb":
        _Adb(path).remove()
        return
    if kind == "mtp":
        _Mtp(path).remove()
        return
    os.remove(path)


def replace(src: str, dst: str) -> None:
    if scheme(src) == "fs" and scheme(dst) == "fs":
        os.replace(src, dst)
        return
    copy_file(src, dst)
    try:
        remove(src)
    except OSError:
        pass


def read_text(path: str) -> str:
    kind = scheme(path)
    if kind == "adb":
        return _Adb(path).read_text()
    if kind == "mtp":
        return _Mtp(path).read_text()
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def write_text(path: str, body: str) -> None:
    kind = scheme(path)
    if kind == "fs":
        parent_dir = os.path.dirname(path)
        if parent_dir:
            os.makedirs(parent_dir, exist_ok=True)
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(body)
        os.replace(tmp, path)
        return
    if kind == "adb":
        _Adb(path).write_text(body)
        return
    _Mtp(path).write_text(body)


def copy_file(src: str, dst: str, on_progress: ProgressFn | None = None) -> int:
    """Copy a host file *src* onto dest *dst* (host, adb, or mtp)."""
    kind = scheme(dst)
    if kind == "adb":
        return _Adb(dst).push(src, on_progress)
    if kind == "mtp":
        return _Mtp(dst).push(src, on_progress)
    import copyutil
    return copyutil.copy_atomic(src, dst, on_progress)


def local_file(path: str) -> str | None:
    """Host path for a dest file, or None when the dest is remote (do not hash USB/MTP)."""
    return None if is_remote(path) else path


def within(path: str, root: str) -> bool:
    if not path or not root:
        return False
    if is_remote(root) or is_remote(path):
        folded = norm(path)
        base = norm(root)
        return folded == base or folded.startswith(base + "/")
    try:
        from pathlib import Path
        return Path(path).resolve(strict=False).is_relative_to(Path(root).resolve(strict=False))
    except OSError:
        return False


def detect_android_mounts() -> tuple[str, str]:
    """`(internal_music, sd_music)` as adb:// or mtp:// URIs. Empty when nothing is there."""
    adb = _detect_adb()
    if adb[0] or adb[1]:
        return adb
    return _detect_mtp()


# ── ADB ──────────────────────────────────────────────────────────


def _adb_bin() -> str:
    return os.environ.get("SONY_SYNC_ADB", "adb")


class _Adb:
    def __init__(self, uri: str) -> None:
        rest = uri[len(ADB_SCHEME):]
        serial, sep, devpath = rest.partition("/")
        self.device = serial
        self.devpath = ("/" + devpath) if sep else "/"
        self.uri = uri.rstrip("/")

    def _run(self, args: list[str], *, timeout: int = 60, check: bool = False) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [_adb_bin(), "-s", self.device, *args],
            capture_output=True, text=True, timeout=timeout, check=check,
        )

    def _shell(self, command: str, timeout: int = 60) -> str:
        result = self._run(["shell", command], timeout=timeout)
        return (result.stdout or "").replace("\r\n", "\n")

    def exists(self) -> bool:
        out = self._shell(f'if [ -e "{self.devpath}" ]; then echo 1; else echo 0; fi').strip()
        return out.startswith("1")

    def isdir(self) -> bool:
        out = self._shell(f'if [ -d "{self.devpath}" ]; then echo 1; else echo 0; fi').strip()
        return out.startswith("1")

    def serial(self) -> str:
        return f"adb-{self.device}"

    def disk_usage(self) -> tuple[int, int] | None:
        out = self._shell(f'df -P "{self.devpath}" 2>/dev/null | tail -n 1')
        parts = out.split()
        if len(parts) >= 4:
            try:
                # df -P: filesystem, 1k-blocks, used, available, ...
                total = int(parts[1]) * 1024
                free = int(parts[3]) * 1024
                return total, free
            except ValueError:
                return None
        return None

    def makedirs(self) -> None:
        self._shell(f'mkdir -p "{self.devpath}"')

    def remove(self) -> None:
        self._shell(f'rm -f "{self.devpath}"')

    def walk_files(self, *, dest: bool = False) -> list[tuple[str, int, float]]:
        out = self._shell(
            f'toybox find "{self.devpath}" -type f -exec toybox stat -c "%s %Y %n" {{}} + 2>/dev/null'
        )
        entries: list[tuple[str, int, float]] = []
        prefix = self.devpath.rstrip("/") + "/"
        skip_names = {
            ".sony-sync-manifest.tsv", "liked songs.m3u8", "liked songs.m3u",
            ".scrobbler.log", "scrobble.logs",
        }
        for line in out.splitlines():
            parts = line.strip().split(" ", 2)
            if len(parts) < 3:
                continue
            try:
                size = int(parts[0])
                mtime = float(parts[1])
            except ValueError:
                continue
            abs_path = parts[2]
            if not abs_path.startswith(self.devpath):
                continue
            rel = abs_path[len(prefix):] if abs_path.startswith(prefix) else os.path.basename(abs_path)
            rel = rel.replace("/", os.sep)
            name = os.path.basename(rel).lower()
            if dest and (name.startswith("cinder_") or name.startswith("listen_") or name in skip_names):
                continue
            entries.append((rel, size, mtime))
        return entries

    def push(self, src: str, on_progress: ProgressFn | None = None) -> int:
        parent_dir = parent(self.uri)
        if parent_dir != self.uri:
            _Adb(parent_dir).makedirs()
        result = self._run(["push", src, self.devpath], timeout=3600)
        if result.returncode != 0:
            raise OSError(result.stderr.strip() or f"adb push failed: {src} -> {self.devpath}")
        size = os.path.getsize(src)
        if on_progress:
            on_progress(size, size, size)
        return size

    def read_text(self) -> str:
        out = self._run(["exec-out", "cat", self.devpath], timeout=120)
        if out.returncode != 0:
            raise OSError(out.stderr.strip() or f"adb cat failed: {self.devpath}")
        return out.stdout

    def write_text(self, body: str) -> None:
        parent_dir = parent(self.uri)
        if parent_dir != self.uri:
            _Adb(parent_dir).makedirs()
        encoded = body.encode("utf-8")
        result = subprocess.run(
            [_adb_bin(), "-s", self.device, "exec-in", "sh", "-c", f'cat > "{self.devpath}"'],
            input=encoded, capture_output=True, timeout=120,
        )
        if result.returncode != 0:
            # exec-in is not on every adb; fall back to a temp push.
            tmp = tempfile.NamedTemporaryFile("wb", delete=False, suffix=".txt")
            try:
                tmp.write(encoded)
                tmp.close()
                self.push(tmp.name)
            finally:
                try:
                    os.remove(tmp.name)
                except OSError:
                    pass


def _detect_adb() -> tuple[str, str]:
    try:
        listing = subprocess.run(
            [_adb_bin(), "devices"], capture_output=True, text=True, timeout=8,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "", ""
    if listing.returncode != 0:
        return "", ""
    serials = []
    for line in listing.stdout.splitlines()[1:]:
        parts = line.split()
        if len(parts) >= 2 and parts[1] == "device":
            serials.append(parts[0])
    chosen = ""
    for serial in serials:
        model = _adb_shell(serial, "getprop ro.product.model").strip()
        if _looks_like_walkman_model(model) or len(serials) == 1:
            chosen = serial
            if _looks_like_walkman_model(model):
                break
    if not chosen:
        return "", ""
    internal = ""
    for candidate in ("/sdcard/Music", "/storage/emulated/0/Music"):
        if _adb_shell(chosen, f'if [ -d "{candidate}" ]; then echo 1; fi').strip() == "1":
            internal = f"{ADB_SCHEME}{chosen}{candidate}"
            break
        _adb_shell(chosen, f'mkdir -p "{candidate}"')
        internal = f"{ADB_SCHEME}{chosen}{candidate}"
        break
    sd = ""
    storage_list = _adb_shell(chosen, "ls /storage 2>/dev/null")
    for name in storage_list.split():
        if name in {"emulated", "self", "sdcard0", "sdcard1"}:
            continue
        if not re.fullmatch(r"[0-9A-F]{4}-[0-9A-F]{4}", name, re.I) and "sd" not in name.lower():
            continue
        music = f"/storage/{name}/Music"
        if _adb_shell(chosen, f'if [ -d "{music}" ] || [ -d "/storage/{name}" ]; then echo 1; fi').strip() == "1":
            _adb_shell(chosen, f'mkdir -p "{music}"')
            sd = f"{ADB_SCHEME}{chosen}{music}"
            break
    return internal, sd


def _adb_shell(serial: str, command: str) -> str:
    try:
        result = subprocess.run(
            [_adb_bin(), "-s", serial, "shell", command],
            capture_output=True, text=True, timeout=8,
        )
    except (OSError, subprocess.TimeoutExpired):
        return ""
    return (result.stdout or "").replace("\r\n", "\n")


def _looks_like_walkman_model(model: str) -> bool:
    upper = model.upper()
    return upper.startswith("NW-") or "WALKMAN" in upper or "ZX" in upper


# ── MTP (Windows Shell.Application) ──────────────────────────────


class _Mtp:
    def __init__(self, uri: str) -> None:
        rest = uri[len(MTP_SCHEME):]
        device, sep, storage_path = rest.partition("/")
        self.device = device
        self.rel = storage_path.replace("\\", "/") if sep else ""
        self.uri = uri.rstrip("/")

    def serial(self) -> str:
        return f"mtp-{self.device}"

    def isdir(self) -> bool:
        return _ps_bool(_ps_shell_probe(self.device, self.rel))

    def exists(self) -> bool:
        return self.isdir() or _ps_bool(_ps_shell_probe(self.device, self.rel, as_file=True))

    def disk_usage(self) -> tuple[int, int] | None:
        return None

    def makedirs(self) -> None:
        _ps_run(_ps_mkdir(self.device, self.rel))

    def remove(self) -> None:
        _ps_run(_ps_remove(self.device, self.rel))

    def walk_files(self, *, dest: bool = False) -> list[tuple[str, int, float]]:
        raw = _ps_run(_ps_list(self.device, self.rel))
        entries: list[tuple[str, int, float]] = []
        skip_names = {
            ".sony-sync-manifest.tsv", "liked songs.m3u8", "liked songs.m3u",
            ".scrobbler.log", "scrobble.logs",
        }
        for line in raw.splitlines():
            parts = line.split("\t")
            if len(parts) < 3:
                continue
            rel, size_s, mtime_s = parts[0], parts[1], parts[2]
            try:
                size = int(size_s)
                mtime = float(mtime_s)
            except ValueError:
                continue
            name = os.path.basename(rel.replace("/", os.sep)).lower()
            if dest and (name.startswith("cinder_") or name.startswith("listen_") or name in skip_names):
                continue
            entries.append((rel.replace("/", os.sep), size, mtime))
        return entries

    def push(self, src: str, on_progress: ProgressFn | None = None) -> int:
        folder = parent(self.uri)
        name = self.rel.rsplit("/", 1)[-1]
        _Mtp(folder).makedirs()
        staged = src
        tmp_dir = None
        if os.path.basename(src) != name:
            tmp_dir = tempfile.mkdtemp()
            staged = os.path.join(tmp_dir, name)
            shutil.copy2(src, staged)
        try:
            _ps_run(_ps_copy(self.device, _Mtp(folder).rel, staged, name))
        finally:
            if tmp_dir:
                shutil.rmtree(tmp_dir, ignore_errors=True)
        size = os.path.getsize(src)
        if on_progress:
            on_progress(size, size, size)
        return size

    def read_text(self) -> str:
        name = self.rel.rsplit("/", 1)[-1]
        tmp_dir = tempfile.mkdtemp()
        try:
            _ps_run(_ps_pull(self.device, self.rel, tmp_dir))
            path = os.path.join(tmp_dir, name)
            with open(path, encoding="utf-8") as handle:
                return handle.read()
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)

    def write_text(self, body: str) -> None:
        name = self.rel.rsplit("/", 1)[-1] or "file.txt"
        tmp_dir = tempfile.mkdtemp()
        tmp_path = os.path.join(tmp_dir, name)
        try:
            with open(tmp_path, "w", encoding="utf-8", newline="\n") as handle:
                handle.write(body)
            self.push(tmp_path)
        finally:
            shutil.rmtree(tmp_dir, ignore_errors=True)


def _ps_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _ps_run(script: str, timeout: int = 120) -> str:
    try:
        result = subprocess.run(
            ["powershell", "-NoProfile", "-Command", script],
            capture_output=True, text=True, timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired):
        return ""
    if result.returncode != 0:
        raise OSError((result.stderr or result.stdout or "powershell MTP failed").strip())
    return result.stdout or ""


def _ps_bool(script: str) -> bool:
    try:
        out = _ps_run(script).strip()
    except OSError:
        return False
    return out.startswith("1")


def _ps_shell_probe(device: str, rel: str, as_file: bool = False) -> str:
    return (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); if (-not $ns) { '0'; exit }; "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "if (-not $dev) { '0'; exit }; "
        "$folder = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  if (-not $hit) { '0'; exit }; $folder = $hit.GetFolder; "
        "  if (-not $folder) { if ($hit) { '1'; exit } else { '0'; exit } } "
        "}; '1'"
    )


def _ps_list(device: str, rel: str) -> str:
    return (
        "$ErrorActionPreference = 'Stop'; "
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "if (-not $dev) { exit 1 }; "
        "$root = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $root.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  if (-not $hit) { exit 1 }; $root = $hit.GetFolder "
        "}; "
        "function Walk($folder, $prefix) { "
        "  foreach ($item in $folder.Items()) { "
        "    $rel = if ($prefix) { $prefix + '/' + $item.Name } else { $item.Name }; "
        "    if ($item.IsFolder) { Walk $item.GetFolder $rel } "
        "    else { $size = $item.Size; $mtime = 0; "
        "      try { $mtime = [DateTimeOffset]$item.ModifyDate.ToUniversalTime(); "
        "            $mtime = $mtime.ToUnixTimeSeconds() } catch {}; "
        "      Write-Output ($rel + \"`t\" + $size + \"`t\" + $mtime) } "
        "  } "
        "}; Walk $root ''"
    )


def _ps_mkdir(device: str, rel: str) -> str:
    return (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "$folder = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  if (-not $hit) { $folder.NewFolder($part) | Out-Null; "
        "    foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } } }; "
        "  $folder = $hit.GetFolder "
        "}"
    )


def _ps_remove(device: str, rel: str) -> str:
    folder_rel, _, name = rel.replace("\\", "/").rpartition("/")
    return (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "$folder = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in folder_rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  $folder = $hit.GetFolder "
        "}; "
        f"foreach ($c in $folder.Items()) {{ if ($c.Name -eq {_ps_quote(name)}) {{ $c.InvokeVerb('delete') }} }}"
    )


def _ps_copy(device: str, folder_rel: str, src: str, name: str) -> str:
    return (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "$folder = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in folder_rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  $folder = $hit.GetFolder "
        "}; "
        f"$folder.CopyHere({_ps_quote(src)}, 16); "
        "Start-Sleep -Milliseconds 400"
    )


def _ps_pull(device: str, rel: str, dest: str) -> str:
    folder_rel, _, name = rel.replace("\\", "/").rpartition("/")
    return (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); "
        f"$dev = $null; foreach ($i in $ns.Items()) {{ if ($i.Name -eq {_ps_quote(device)}) {{ $dev = $i; break }} }}; "
        "$folder = $dev.GetFolder; "
        f"$parts = @({', '.join(_ps_quote(p) for p in folder_rel.split('/') if p)}); "
        "foreach ($part in $parts) { "
        "  $hit = $null; foreach ($c in $folder.Items()) { if ($c.Name -eq $part) { $hit = $c; break } }; "
        "  $folder = $hit.GetFolder "
        "}; "
        f"$item = $null; foreach ($c in $folder.Items()) {{ if ($c.Name -eq {_ps_quote(name)}) {{ $item = $c; break }} }}; "
        f"$dest = $shell.NameSpace({_ps_quote(str(os.path.dirname(dest)))}); "
        "$dest.CopyHere($item, 16); Start-Sleep -Milliseconds 400"
    )


def _detect_mtp() -> tuple[str, str]:
    if os.name != "nt":
        return "", ""
    script = (
        "$shell = New-Object -ComObject Shell.Application; "
        "$ns = $shell.NameSpace(0x11); if (-not $ns) { exit }; "
        "foreach ($i in $ns.Items()) { "
        "  $n = $i.Name; if ($n -match 'WALKMAN|NW-') { "
        "    $storages = @(); foreach ($s in $i.GetFolder.Items()) { $storages += $s.Name }; "
        "    Write-Output ($n + \"`t\" + ($storages -join '`t')) "
        "  } "
        "}"
    )
    try:
        raw = _ps_run(script, timeout=15)
    except OSError:
        return "", ""
    internal = ""
    sd = ""
    for line in raw.splitlines():
        parts = line.split("\t")
        if not parts:
            continue
        device = parts[0].strip()
        names = [p.strip() for p in parts[1:] if p.strip()]
        for name in names:
            music = f"{MTP_SCHEME}{device}/{name}/Music"
            lower = name.lower()
            if "internal" in lower or "shared" in lower or lower == "phone":
                internal = music
            else:
                sd = music
        if not internal and names:
            internal = f"{MTP_SCHEME}{device}/{names[0]}/Music"
        if internal:
            break
    return internal, sd
