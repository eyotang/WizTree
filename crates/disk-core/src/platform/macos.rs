//! Darwin bulk metadata with a per-directory POSIX fallback.
//! No Spotlight dependency, raw APFS parsing, or file-content reads.
use super::{macos_bulk as bulk, unix};
use crate::*;
use std::{
    ffi::OsString,
    os::{fd::AsRawFd, unix::ffi::OsStringExt},
};
pub(super) struct Reader {
    buffer: Vec<u64>,
}
impl Reader {
    pub(super) fn new() -> Self {
        Self {
            buffer: vec![0; 64 * 1024 / 8],
        }
    }
    pub(super) fn enumerate(
        &mut self,
        path: &Path,
        cancel: &AtomicBool,
        metrics: &DirectoryIo,
        mut emit: impl FnMut(io::Result<Entry>),
    ) -> io::Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let file = unix::open_directory(path, metrics)?;
        let mut attrs = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: bulk::COMMON,
            volattr: 0,
            dirattr: 0,
            fileattr: bulk::FILE,
            forkattr: 0,
        };
        let mut emitted = false;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let started = Instant::now();
            // SAFETY: fd and attrlist are live, buffer is aligned and writable.
            // No PACK_INVAL_ATTRS: decoder follows the actual returned bitmap.
            let count = unsafe {
                libc::getattrlistbulk(
                    file.as_raw_fd(),
                    (&mut attrs as *mut libc::attrlist).cast(),
                    self.buffer.as_mut_ptr().cast(),
                    self.buffer.len() * 8,
                    0,
                )
            };
            metrics.queries.fetch_add(1, Ordering::Relaxed);
            metrics
                .query_ns
                .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if !emitted
                    && error
                        .raw_os_error()
                        .is_some_and(|e| [libc::ENOTSUP, libc::ENOSYS, libc::EINVAL].contains(&e))
                {
                    drop(file); // Never mix bulk and readdir offsets on the same fd.
                    metrics.portable_fallbacks.fetch_add(1, Ordering::Relaxed);
                    return unix::enumerate(path, cancel, metrics, emit);
                }
                return Err(error);
            }
            if count == 0 {
                return Ok(());
            }
            emitted = true;
            // SAFETY: backing buffer remains initialized and alive during parsing.
            let bytes = unsafe {
                std::slice::from_raw_parts(self.buffer.as_ptr().cast::<u8>(), self.buffer.len() * 8)
            };
            bulk::parse_batch(bytes, count as usize, |result| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => {
                        emit(Err(error));
                        return;
                    }
                };
                let is_dir = entry.object_type == 2; // Darwin VDIR; VREG is 1.
                if !is_dir && entry.object_type != 1 {
                    return;
                }
                if !is_dir && (entry.size.is_none() || entry.allocated.is_none()) {
                    metrics.special_files.fetch_add(1, Ordering::Relaxed);
                    match unix::stat_entry(file.as_raw_fd(), path, &entry.name, metrics) {
                        Ok(Some(entry)) => emit(Ok(entry)),
                        Ok(None) => {}
                        Err(e) => emit(Err(e)),
                    }
                    return;
                }
                let name = OsString::from_vec(entry.name);
                emit(Ok(Entry {
                    path: if is_dir {
                        path.join(&name)
                    } else {
                        PathBuf::new()
                    },
                    name,
                    is_dir,
                    size: if is_dir { 0 } else { entry.size.unwrap() },
                    allocated: if is_dir { 0 } else { entry.allocated.unwrap() },
                    modified: entry.modified,
                }));
            })?;
        }
    }
}
