//! Shared POSIX metadata conversion and fallback, not a filesystem-wide index.
use crate::*;
use std::{
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd, IntoRawFd, RawFd},
        unix::{ffi::OsStringExt, fs::OpenOptionsExt},
    },
};

pub(super) fn open_directory(path: &Path, metrics: &DirectoryIo) -> io::Result<File> {
    let started = Instant::now();
    let result = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path);
    metrics.opens.fetch_add(1, Ordering::Relaxed);
    metrics.absolute_opens.fetch_add(1, Ordering::Relaxed);
    metrics
        .open_ns
        .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    result
}

pub(super) fn modified(seconds: i64, nanos: i64) -> Option<SystemTime> {
    if !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    let epoch = SystemTime::UNIX_EPOCH;
    let base = if seconds >= 0 {
        epoch.checked_add(Duration::from_secs(seconds as u64))
    } else {
        epoch.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    }?;
    base.checked_add(Duration::from_nanos(nanos as u64))
}

pub(super) fn stat_entry(
    fd: RawFd,
    path: &Path,
    name: &[u8],
    metrics: &DirectoryIo,
) -> io::Result<Option<Entry>> {
    let name_c = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "NUL in directory name"))?;
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let started = Instant::now();
    // SAFETY: file owns a live directory fd, name is terminated, and libc defines
    // the target-specific stat layout. AT_SYMLINK_NOFOLLOW never follows a link.
    let result = unsafe {
        libc::fstatat(
            fd,
            name_c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    metrics.queries.fetch_add(1, Ordering::Relaxed);
    metrics
        .query_ns
        .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat initialized the output on success.
    let stat = unsafe { stat.assume_init() };
    let kind = stat.st_mode & libc::S_IFMT;
    let is_dir = kind == libc::S_IFDIR;
    if !is_dir && kind != libc::S_IFREG {
        return Ok(None);
    }
    let size = u64::try_from(stat.st_size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Negative file size"))?;
    let blocks = u64::try_from(stat.st_blocks)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Negative allocated blocks"))?;
    let name = OsString::from_vec(name.to_vec());
    Ok(Some(Entry {
        path: if is_dir {
            path.join(&name)
        } else {
            PathBuf::new()
        },
        name,
        size: if is_dir { 0 } else { size },
        allocated: if is_dir {
            0
        } else {
            blocks.saturating_mul(512)
        },
        is_dir,
        modified: modified(stat.st_mtime, stat.st_mtime_nsec),
    }))
}

struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the only stream reference.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn errno() -> *mut libc::c_int {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::__errno_location() }
    }
    #[cfg(target_os = "macos")]
    {
        unsafe { libc::__error() }
    }
}

/// readdir buffers getdents on Linux; every stat is relative to the same held fd.
/// Only active workers own descriptors, rather than keeping one per queued job.
pub(super) fn enumerate(
    path: &Path,
    cancel: &AtomicBool,
    metrics: &DirectoryIo,
    mut emit: impl FnMut(io::Result<Entry>),
) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let file = open_directory(path, metrics)?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is live; fdopendir takes ownership only on success.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _ = file.into_raw_fd();
    let stream = DirectoryStream(stream);
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // SAFETY: errno is thread-local; this stream is accessed by one worker.
        unsafe {
            *errno() = 0;
        }
        let started = Instant::now();
        let entry = unsafe { libc::readdir(stream.0) };
        metrics.queries.fetch_add(1, Ordering::Relaxed);
        metrics
            .query_ns
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if entry.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(error)
            };
        }
        // SAFETY: readdir returns a terminated d_name valid until the next call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        // d_type is only a skip hint; DT_UNKNOWN still goes through fstatat.
        let kind = unsafe { (*entry).d_type };
        if kind != libc::DT_UNKNOWN && kind != libc::DT_REG && kind != libc::DT_DIR {
            continue;
        }
        match stat_entry(fd, path, name, metrics) {
            Ok(Some(entry)) => emit(Ok(entry)),
            Ok(None) => {}
            Err(error) => emit(Err(error)),
        }
    }
    Ok(())
}
