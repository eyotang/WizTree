//! Compile-time selection of native directory metadata transports.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "macos", test))]
mod macos_bulk;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;

use super::*;

/// The selected transport family; individual unsupported filesystems may fall back.
pub fn directory_backend() -> &'static str {
    #[cfg(windows)]
    {
        "windows-batched-directory"
    }
    #[cfg(target_os = "macos")]
    {
        "macos-getattrlistbulk"
    }
    #[cfg(target_os = "linux")]
    {
        "linux-fd-relative-stat"
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        "portable-directory"
    }
}

pub(super) struct Reader {
    #[cfg(windows)]
    native: windows_directory::Reader,
    #[cfg(target_os = "linux")]
    native: linux::Reader,
    #[cfg(target_os = "macos")]
    native: macos::Reader,
}
impl Reader {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(windows)]
            native: windows_directory::Reader::new(),
            #[cfg(target_os = "linux")]
            native: linux::Reader::new(),
            #[cfg(target_os = "macos")]
            native: macos::Reader::new(),
        }
    }
    pub(super) fn enumerate(
        &mut self,
        job: &Job,
        cancel: &AtomicBool,
        progress: &Progress,
        mut emit: impl FnMut(io::Result<Entry>),
    ) -> io::Result<()> {
        #[cfg(windows)]
        {
            match self.native.enumerate(
                &job.path,
                job.directory_parent.as_deref(),
                cancel,
                &progress.directory_io,
                &mut emit,
            ) {
                Err(error) if error.kind() == io::ErrorKind::Unsupported => {}
                result => return result,
            }
            progress
                .directory_io
                .portable_fallbacks
                .fetch_add(1, Ordering::Relaxed);
            enumerate_portable(&job.path, cancel, emit)
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.native
                .enumerate(&job.path, cancel, &progress.directory_io, &mut emit)
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            let _ = progress;
            enumerate_portable(&job.path, cancel, emit)
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
