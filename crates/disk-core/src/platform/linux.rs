//! Linux-native buffered readdir + directory-fd-relative fstatat.
//! No privileged raw ext4/XFS/Btrfs device reads or mandatory locate database.
use crate::*;
pub(super) struct Reader;
impl Reader {
    pub(super) fn new() -> Self {
        Self
    }
    pub(super) fn enumerate(
        &mut self,
        path: &Path,
        cancel: &AtomicBool,
        metrics: &DirectoryIo,
        emit: impl FnMut(io::Result<Entry>),
    ) -> io::Result<()> {
        super::unix::enumerate(path, cancel, metrics, emit)
    }
}
