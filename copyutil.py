"""Copy primitives: atomic dest write, CopyFileEx when available, large-buffer fallback.

Default is one writer onto one USB LUN. A progress callback receives (bytes_this_chunk,
bytes_so_far, bytes_total) so the UI can move a per-file bar without waiting for close().
The portable path pipelines an SMB read against a USB write through a two-chunk queue.
"""
from __future__ import annotations

import ctypes
import os
import queue
import shutil
import subprocess
import sys
import threading
import time
from collections.abc import Callable
from pathlib import Path

CHUNK_BYTES = 8 * 1024 * 1024
COPY_FILE_FAIL_IF_EXISTS = 0x00000001
COPY_FILE_NO_BUFFERING = 0x00001000
PROGRESS_CONTINUE = 0
PROGRESS_CANCEL = 1

ProgressFn = Callable[[int, int, int], None]


def partial_path(dst: str) -> str:
    directory, name = os.path.split(dst)
    return os.path.join(directory, f".{name}.partial")


def copy_atomic(src: str, dst: str, on_progress: ProgressFn | None = None) -> int:
    """Copy `src` to `dst` through a sibling `.name.partial`, then replace.

    Tries CopyFileExW on Windows (progress callback, unbuffered USB). Falls back to a
    pipelined large-buffer copyfileobj. Cleans the partial up on failure.
    """
    tmp = partial_path(dst)
    try:
        if os.path.exists(tmp):
            os.remove(tmp)
        copied = _copy_to(src, tmp, on_progress)
        os.replace(tmp, dst)
        return copied
    except Exception:
        try:
            if os.path.exists(tmp):
                os.remove(tmp)
        except OSError:
            pass
        raise


def _copy_to(src: str, dst: str, on_progress: ProgressFn | None) -> int:
    if os.name == "nt" and _copyfile_ex(src, dst, on_progress):
        return os.path.getsize(dst)
    try:
        if os.path.exists(dst):
            os.remove(dst)
    except OSError:
        pass
    return _copyfileobj(src, dst, on_progress, pipelined=True)


def _copyfileobj(src: str, dst: str, on_progress: ProgressFn | None,
                 pipelined: bool = True) -> int:
    if pipelined:
        return _copyfileobj_pipelined(src, dst, on_progress)
    total = os.path.getsize(src)
    done = 0
    with open(src, "rb") as incoming, open(dst, "wb") as outgoing:
        while True:
            chunk = incoming.read(CHUNK_BYTES)
            if not chunk:
                break
            outgoing.write(chunk)
            done += len(chunk)
            if on_progress:
                on_progress(len(chunk), done, total)
    _copystat(src, dst)
    return done


def _copyfileobj_pipelined(src: str, dst: str, on_progress: ProgressFn | None) -> int:
    """One reader fills a 2-chunk queue while the writer drains it (SMB RTT vs flash)."""
    total = os.path.getsize(src)
    chunks: queue.Queue[bytes | None] = queue.Queue(maxsize=2)
    errors: list[BaseException] = []

    def reader() -> None:
        try:
            with open(src, "rb") as incoming:
                while True:
                    chunk = incoming.read(CHUNK_BYTES)
                    if not chunk:
                        chunks.put(None)
                        return
                    chunks.put(chunk)
        except BaseException as exc:
            errors.append(exc)
            try:
                chunks.put(None)
            except Exception:
                pass

    thread = threading.Thread(target=reader, daemon=True)
    thread.start()
    done = 0
    try:
        with open(dst, "wb") as outgoing:
            while True:
                chunk = chunks.get()
                if chunk is None:
                    break
                outgoing.write(chunk)
                done += len(chunk)
                if on_progress:
                    on_progress(len(chunk), done, total)
    finally:
        thread.join(timeout=30)
    if errors:
        raise errors[0]
    _copystat(src, dst)
    return done


def _copystat(src: str, dst: str) -> None:
    try:
        shutil.copystat(src, dst, follow_symlinks=True)
    except OSError:
        pass


def _copyfile_ex(src: str, dst: str, on_progress: ProgressFn | None) -> bool:
    for flags in (COPY_FILE_NO_BUFFERING, 0):
        try:
            if os.path.exists(dst):
                os.remove(dst)
        except OSError:
            pass
        if _copyfile_ex_flags(src, dst, on_progress, flags):
            return True
    return False


def _copyfile_ex_flags(src: str, dst: str, on_progress: ProgressFn | None, flags: int) -> bool:
    try:
        kernel32 = ctypes.windll.kernel32  # type: ignore[attr-defined]
    except AttributeError:
        return False

    total_holder = [0]
    done_holder = [0]
    cancelled = [False]

    @ctypes.WINFUNCTYPE(
        ctypes.c_uint,
        ctypes.c_longlong, ctypes.c_longlong, ctypes.c_longlong, ctypes.c_longlong,
        ctypes.c_uint, ctypes.c_uint, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
    )
    def routine(total, transferred, _stream_size, _stream_bytes, _stream, _reason,
                _src, _dst, _data):
        total_holder[0] = int(total)
        chunk = int(transferred) - done_holder[0]
        done_holder[0] = int(transferred)
        if on_progress and chunk >= 0:
            try:
                on_progress(chunk, done_holder[0], total_holder[0] or int(total))
            except Exception:
                cancelled[0] = True
                return PROGRESS_CANCEL
        return PROGRESS_CONTINUE

    kernel32.CopyFileExW.argtypes = [
        ctypes.c_wchar_p, ctypes.c_wchar_p, ctypes.c_void_p,
        ctypes.c_void_p, ctypes.c_void_p, ctypes.c_uint,
    ]
    kernel32.CopyFileExW.restype = ctypes.c_int
    ok = kernel32.CopyFileExW(src, dst, routine, None, None, flags)
    if cancelled[0] or not ok:
        return False
    return True


def _robocopy_file(src: str, dst: str) -> int:
    src_dir, src_name = os.path.split(src)
    dst_dir = os.path.dirname(dst)
    Path(dst_dir).mkdir(parents=True, exist_ok=True)
    completed = subprocess.run(
        ["robocopy", src_dir, dst_dir, src_name, "/J", "/NJH", "/NJS", "/NP"],
        capture_output=True,
        text=True,
    )
    # robocopy uses 0–7 as success-ish; 8+ is failure.
    if completed.returncode >= 8:
        raise OSError(f"robocopy failed ({completed.returncode}): {completed.stderr}")
    dest = os.path.join(dst_dir, src_name)
    if os.path.normcase(dest) != os.path.normcase(dst) and os.path.exists(dest):
        os.replace(dest, dst)
    return os.path.getsize(dst)


def bench_copy(dest_dir: str, file_bytes: int = 8 * 1024 * 1024, rounds: int = 2) -> str:
    """Write a throwaway file with each primitive and report MB/s. Does not use the library."""
    Path(dest_dir).mkdir(parents=True, exist_ok=True)
    src = os.path.join(dest_dir, ".sony-sync-bench-src.bin")
    lines = [f"bench-copy → {dest_dir}  ({file_bytes} bytes × {rounds})"]
    payload = os.urandom(min(file_bytes, 1 << 20))
    with open(src, "wb") as handle:
        written = 0
        while written < file_bytes:
            chunk = payload if written + len(payload) <= file_bytes else payload[: file_bytes - written]
            handle.write(chunk)
            written += len(chunk)

    def timed(name: str, fn) -> None:
        times: list[float] = []
        for index in range(rounds):
            dst = os.path.join(dest_dir, f".sony-sync-bench-{index}.bin")
            try:
                if os.path.exists(dst):
                    os.remove(dst)
                start = time.perf_counter()
                fn(dst)
                times.append(time.perf_counter() - start)
            except Exception as exc:
                lines.append(f"  {name}: failed ({exc})")
                return
            finally:
                try:
                    os.remove(dst)
                except OSError:
                    pass
        if not times:
            lines.append(f"  {name}: failed")
            return
        best = min(times)
        mb_s = (file_bytes / (1024 * 1024)) / best if best > 0 else 0
        lines.append(f"  {name}: {mb_s:.1f} MB/s  ({best:.2f}s best of {rounds})")

    try:
        timed("copyfileobj sequential", lambda d: _copyfileobj(src, d, None, pipelined=False))
        timed("copyfileobj pipelined", lambda d: _copyfileobj(src, d, None, pipelined=True))
        timed("CopyFileExW", lambda d: _copy_to(src, d, None) if os.name == "nt" else (_copyfileobj(src, d, None)))
        timed("shutil.copy2", lambda d: shutil.copy2(src, d) or os.path.getsize(d))
        if os.name == "nt":
            try:
                timed("robocopy /J", lambda d: _robocopy_file(src, d))
            except OSError as exc:
                lines.append(f"  robocopy /J: skipped ({exc})")

        # 1 vs 2 writers onto the same dest folder (same LUN). Extra writers often lose on flash.
        def parallel_copy(workers: int) -> None:
            from concurrent.futures import ThreadPoolExecutor
            paths = [os.path.join(dest_dir, f".sony-sync-bench-w{i}.bin") for i in range(workers)]
            for path in paths:
                if os.path.exists(path):
                    os.remove(path)
            try:
                with ThreadPoolExecutor(max_workers=workers) as pool:
                    list(pool.map(lambda d: _copyfileobj(src, d, None, pipelined=True), paths))
            finally:
                for path in paths:
                    try:
                        os.remove(path)
                    except OSError:
                        pass

        for workers in (1, 2):
            times: list[float] = []
            for _ in range(rounds):
                start = time.perf_counter()
                parallel_copy(workers)
                times.append(time.perf_counter() - start)
            best = min(times) if times else 0
            total = file_bytes * workers
            mb_s = (total / (1024 * 1024)) / best if best > 0 else 0
            lines.append(f"  {workers} writer(s) pipelined: {mb_s:.1f} MB/s  ({best:.2f}s best of {rounds})")
    finally:
        try:
            os.remove(src)
        except OSError:
            pass
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    dest = args[0] if args else os.getcwd()
    print(bench_copy(dest))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
