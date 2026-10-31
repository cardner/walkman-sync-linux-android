//! Atomic dest write: CopyFileEx on Windows, large-buffer copy elsewhere.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const CHUNK: usize = 8 * 1024 * 1024;

pub fn partial_path(dst: &Path) -> std::path::PathBuf {
    let name = dst
        .file_name()
        .map(|n| format!(".{}.partial", n.to_string_lossy()))
        .unwrap_or_else(|| ".partial".into());
    dst.with_file_name(name)
}

pub fn copy_atomic(src: &Path, dst: &Path) -> io::Result<u64> {
    copy_atomic_progress(src, dst, &mut |_| {})
}

pub fn copy_atomic_progress(
    src: &Path,
    dst: &Path,
    progress: &mut dyn FnMut(u64),
) -> io::Result<u64> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = partial_path(dst);
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    let result = (|| {
        let copied = copy_to(src, &tmp, progress)?;
        fs::rename(&tmp, dst)?;
        Ok(copied)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn copy_to(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64)) -> io::Result<u64> {
    #[cfg(windows)]
    {
        if copyfile_ex(src, dst, progress)? {
            return Ok(fs::metadata(dst)?.len());
        }
    }
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    copyfileobj(src, dst, progress)
}

fn copyfileobj(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64)) -> io::Result<u64> {
    let mut incoming = File::open(src)?;
    let mut outgoing = File::create(dst)?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = incoming.read(&mut buf)?;
        if n == 0 {
            break;
        }
        outgoing.write_all(&buf[..n])?;
        done += n as u64;
        progress(done);
    }
    let _ = fs::set_permissions(dst, fs::metadata(src)?.permissions());
    Ok(done)
}

#[cfg(windows)]
struct CopyProgress<'a> {
    cb: &'a mut dyn FnMut(u64),
    last: Instant,
    last_n: u64,
}

#[cfg(windows)]
unsafe extern "system" fn on_copy_progress(
    _total_file_size: i64,
    total_bytes_transferred: i64,
    _stream_size: i64,
    _stream_bytes_transferred: i64,
    _stream_number: u32,
    _callback_reason: u32,
    _source: windows_sys::Win32::Foundation::HANDLE,
    _destination: windows_sys::Win32::Foundation::HANDLE,
    data: *const core::ffi::c_void,
) -> u32 {
    const PROGRESS_CONTINUE: u32 = 0;
    if data.is_null() {
        return PROGRESS_CONTINUE;
    }
    let state = &mut *(data as *mut CopyProgress<'_>);
    let n = total_bytes_transferred as u64;
    if n <= state.last_n {
        return PROGRESS_CONTINUE;
    }
    let now = Instant::now();
    if n.saturating_sub(state.last_n) >= 1024 * 1024
        || now.duration_since(state.last) >= Duration::from_millis(100)
    {
        state.last = now;
        state.last_n = n;
        (state.cb)(n);
    }
    PROGRESS_CONTINUE
}

#[cfg(windows)]
fn copyfile_ex(src: &Path, dst: &Path, progress: &mut dyn FnMut(u64)) -> io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::CopyFileExW;
    let src_w: Vec<u16> = src.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut state = CopyProgress {
        cb: progress,
        last: Instant::now(),
        last_n: 0,
    };
    let ok = unsafe {
        CopyFileExW(
            src_w.as_ptr(),
            dst_w.as_ptr(),
            Some(on_copy_progress),
            &mut state as *mut CopyProgress as *const core::ffi::c_void,
            std::ptr::null_mut(),
            0,
        )
    };
    Ok(ok != 0)
}
