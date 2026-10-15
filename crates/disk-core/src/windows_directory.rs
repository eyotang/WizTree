//! Batched Windows directory metadata: one handle per directory, not per file.
//! FILE_FULL_DIR_INFO layout follows the Windows SDK (FileName offset 68).
//! File IDs and DOS short names are not used by the snapshot, so avoid querying them.
use super::*;
use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    os::windows::{
        ffi::OsStringExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
};
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetFileInformationByHandleEx(
        handle: *mut c_void,
        class: i32,
        information: *mut c_void,
        size: u32,
    ) -> i32;
}
#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut c_void,
    name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_qos: *mut c_void,
}
#[repr(C)]
#[derive(Default)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtOpenFile(
        handle: *mut *mut c_void,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatusBlock,
        share: u32,
        options: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
fn open_directory(path: &Path, parent: Option<&File>) -> io::Result<File> {
    if let Some(parent) = parent {
        use std::os::windows::ffi::OsStrExt;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Missing directory component"))?;
        let name: Vec<u16> = name.encode_wide().collect();
        let length: u16 = (name.len() * 2)
            .try_into()
            .map_err(|_| invalid("Directory name too long"))?;
        let string = UnicodeString {
            length,
            maximum_length: length,
            buffer: name.as_ptr(),
        };
        let attributes = ObjectAttributes {
            length: std::mem::size_of::<ObjectAttributes>() as u32,
            root: parent.as_raw_handle(),
            name: &string,
            attributes: 0, // Exact name from the parent enumeration, preserving case-sensitive folders.
            security_descriptor: std::ptr::null_mut(),
            security_qos: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        let mut status = IoStatusBlock::default();
        // SAFETY: all counted UTF-16 and ABI structures live for this synchronous
        // call. Parent is borrowed; a successful handle is exclusively adopted.
        // FILE_LIST_DIRECTORY|SYNCHRONIZE; share read/write/delete;
        // DIRECTORY_FILE|SYNCHRONOUS_IO_NONALERT|OPEN_FOR_BACKUP_INTENT|OPEN_REPARSE_POINT.
        let result = unsafe {
            NtOpenFile(
                &mut handle,
                0x0010_0001,
                &attributes,
                &mut status,
                7,
                0x0020_4021,
            )
        };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(result) } as i32,
            ));
        }
        // SAFETY: NT success transfers exactly one owned file handle.
        return Ok(unsafe { File::from_raw_handle(handle) });
    }
    OpenOptions::new()
        .access_mode(1)
        .share_mode(7)
        .custom_flags(0x0220_0000)
        .open(path)
}
pub(super) struct Reader {
    words: Vec<u64>,
    relative: bool,
}
impl Reader {
    pub(super) fn new() -> Self {
        Self {
            words: vec![0; 64 * 1024 / 8],
            relative: std::env::var_os("WIZTREE_ABSOLUTE_DIRECTORY_OPEN").is_none(),
        }
    }
}
const HEADER: usize = 68;
const DIRECTORY_RESTART: i32 = 15; // FileFullDirectoryRestartInfo
const DIRECTORY_CONTINUE: i32 = 14; // FileFullDirectoryInfo
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn read_u32(data: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(at..at + 4)
            .ok_or_else(|| invalid("Truncated directory metadata"))?
            .try_into()
            .unwrap(),
    ))
}
fn read_u64(data: &[u8], at: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(at..at + 8)
            .ok_or_else(|| invalid("Truncated directory metadata"))?
            .try_into()
            .unwrap(),
    ))
}
/// Parse the checked variable-length chain before exposing each owned entry.
fn parse_batch(
    data: &[u8],
    cancel: &AtomicBool,
    mut emit: impl FnMut(OsString, u32, u64, u64, Option<SystemTime>),
) -> io::Result<()> {
    let mut offset = 0usize;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let entry = data
            .get(offset..)
            .ok_or_else(|| invalid("Invalid directory offset"))?;
        if entry.len() < HEADER {
            return Err(invalid("Truncated directory header"));
        }
        let next = read_u32(entry, 0)? as usize;
        let length = read_u32(entry, 60)? as usize;
        if !length.is_multiple_of(2) || length == 0 || length > 65534 {
            return Err(invalid("Invalid directory filename length"));
        }
        let end = HEADER
            .checked_add(length)
            .ok_or_else(|| invalid("Directory filename overflow"))?;
        if next != 0 && (!next.is_multiple_of(8) || next < end || next >= entry.len()) {
            return Err(invalid("Invalid next directory record"));
        }
        let name = entry
            .get(HEADER..end)
            .ok_or_else(|| invalid("Truncated directory filename"))?;
        let utf16: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        if utf16.iter().any(|&v| matches!(v, 0 | 47 | 92)) {
            return Err(invalid("Invalid directory name component"));
        }
        let attributes = read_u32(entry, 56)?;
        if utf16.as_slice() != [46] && utf16.as_slice() != [46, 46] && attributes & 0x440 == 0 {
            let logical = read_u64(entry, 40)?;
            let allocated = read_u64(entry, 48)?;
            if logical > i64::MAX as u64 || allocated > i64::MAX as u64 {
                return Err(invalid("Negative directory size"));
            }
            emit(
                OsString::from_wide(&utf16),
                attributes,
                logical,
                allocated,
                crate::ntfs::ntfs_time(read_u64(entry, 24)?),
            );
        }
        if next == 0 {
            return Ok(());
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| invalid("Directory offset overflow"))?;
    }
}
impl Reader {
    pub(super) fn enumerate(
        &mut self,
        path: &Path,
        parent: Option<&File>,
        cancel: &AtomicBool,
        metrics: &DirectoryIo,
        mut emit: impl FnMut(io::Result<Entry>),
    ) -> io::Result<()> {
        let parent = if self.relative { parent } else { None };
        let begin = Instant::now();
        let directory = open_directory(path, parent);
        metrics.opens.fetch_add(1, Ordering::Relaxed);
        metrics
            .open_ns
            .fetch_add(begin.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if parent.is_none() {
            metrics.absolute_opens.fetch_add(1, Ordering::Relaxed);
        }
        let directory = std::sync::Arc::new(directory?);
        let words = &mut self.words;
        let mut class = DIRECTORY_RESTART;
        let mut delivered = false;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            // SAFETY: valid owned directory handle and aligned writable allocation;
            // the synchronous API cannot retain a pointer to this buffer.
            let begin = Instant::now();
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    directory.as_raw_handle(),
                    class,
                    words.as_mut_ptr().cast(),
                    (words.len() * 8) as u32,
                )
            };
            metrics.queries.fetch_add(1, Ordering::Relaxed);
            metrics
                .query_ns
                .fetch_add(begin.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if ok == 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(18) => return Ok(()), // ERROR_NO_MORE_FILES
                    Some(1 | 50 | 87 | 124) if !delivered => {
                        return Err(io::Error::new(io::ErrorKind::Unsupported, error));
                    }
                    Some(234) if words.len() < 1024 * 1024 / 8 => {
                        words.resize(words.len() * 2, 0);
                        continue;
                    }
                    _ => return Err(error),
                }
            }
            class = DIRECTORY_CONTINUE;
            delivered = true;
            // SAFETY: all allocated words are initialized; the checked chain parser
            // only consumes the records returned by the successful kernel call.
            let bytes =
                unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), words.len() * 8) };
            parse_batch(
                bytes,
                cancel,
                |name, attributes, logical, allocation, modified| {
                    let is_dir = attributes & 0x10 != 0;
                    // Compression/sparse accounting still uses FileCompressionInfo so
                    // physical allocation remains consistent with the original walker.
                    let allocated = if !is_dir && attributes & 0xa00 != 0 {
                        let file_path = path.join(&name);
                        let begin = Instant::now();
                        let result = super::windows_allocated_size(&file_path, attributes);
                        metrics.special_files.fetch_add(1, Ordering::Relaxed);
                        metrics
                            .special_ns
                            .fetch_add(begin.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        match result {
                            Ok(value) => value,
                            Err(error) => {
                                emit(Err(io::Error::new(
                                    error.kind(),
                                    format!("{}: {error}", file_path.display()),
                                )));
                                0
                            }
                        }
                    } else if is_dir {
                        0
                    } else {
                        allocation
                    };
                    let entry_path = if is_dir {
                        path.join(&name)
                    } else {
                        PathBuf::new()
                    };
                    emit(Ok(Entry {
                        directory_parent: is_dir.then(|| directory.clone()),
                        name,
                        path: entry_path,
                        size: if is_dir { 0 } else { logical },
                        allocated,
                        is_dir,
                        modified,
                    }));
                },
            )?;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_open_uses_held_parent_after_rename_and_closes_every_handle() {
        use std::os::windows::io::AsRawHandle;
        let root = std::env::temp_dir().join(format!("wiztree-relative-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let old = root.join("parent");
        fs::create_dir(&old).unwrap();
        fs::create_dir(old.join("目录 🎉")).unwrap();
        fs::write(old.join("目录 🎉").join("file"), b"abc").unwrap();
        let parent = open_directory(&old, None).unwrap();
        let moved = root.join("moved");
        fs::rename(&old, &moved).unwrap();
        let child = open_directory(&old.join("目录 🎉"), Some(&parent)).unwrap();
        let mut words = vec![0u64; 8192];
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(
                    child.as_raw_handle(),
                    DIRECTORY_RESTART,
                    words.as_mut_ptr().cast(),
                    65536,
                )
            },
            0
        );
        let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr().cast::<u8>(), 65536) };
        let mut names = Vec::new();
        parse_batch(bytes, &AtomicBool::new(false), |name, _, size, _, _| {
            names.push((name, size));
        })
        .unwrap();
        assert_eq!(names, vec![(OsString::from("file"), 3)]);
        assert!(open_directory(&old.join("missing"), Some(&parent)).is_err());
        drop(child);
        drop(parent);
        fs::remove_dir_all(&moved).unwrap();
    }
    fn record(name: &[u16], attributes: u32, logical: u64, allocated: u64) -> Vec<u8> {
        let mut data = vec![0; HEADER + name.len() * 2];
        data[40..48].copy_from_slice(&logical.to_le_bytes());
        data[48..56].copy_from_slice(&allocated.to_le_bytes());
        data[56..60].copy_from_slice(&attributes.to_le_bytes());
        data[60..64].copy_from_slice(&(name.len() as u32 * 2).to_le_bytes());
        for (i, v) in name.iter().enumerate() {
            data[HEADER + i * 2..HEADER + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        data
    }
    #[test]
    fn batched_chain_retains_unicode_and_sizes_and_skips_reparse_and_dot() {
        let names = [".", "..", "中文 space.bin", "link", "folder"];
        let mut data = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let attrs = if i == 3 {
                0x400
            } else if i == 4 {
                0x10
            } else {
                0
            };
            let mut entry = record(&name.encode_utf16().collect::<Vec<_>>(), attrs, 123, 4096);
            if i + 1 < names.len() {
                entry.resize((entry.len() + 7) & !7, 0);
                let n = entry.len() as u32;
                entry[..4].copy_from_slice(&n.to_le_bytes());
            }
            data.extend(entry);
        }
        let mut rows = Vec::new();
        parse_batch(&data, &AtomicBool::new(false), |n, a, s, b, _| {
            rows.push((n, a, s, b))
        })
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "中文 space.bin");
        assert_eq!(rows[0].2, 123);
        assert_eq!(rows[0].3, 4096);
        assert_eq!(rows[1].1, 0x10);
    }
    #[test]
    fn malformed_chains_names_and_signed_sizes_are_rejected() {
        let valid = record(&[97], 0, 1, 4096);
        for length in 0..valid.len() {
            assert!(
                parse_batch(
                    &valid[..length],
                    &AtomicBool::new(false),
                    |_, _, _, _, _| {}
                )
                .is_err()
            );
        }
        for (at, value) in [(0, 8u32), (60, 3), (60, 65536)] {
            let mut v = valid.clone();
            v[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert!(parse_batch(&v, &AtomicBool::new(false), |_, _, _, _, _| {}).is_err());
        }
        let v = record(&[97], 0, u64::MAX, 1);
        assert!(parse_batch(&v, &AtomicBool::new(false), |_, _, _, _, _| {}).is_err());
        for name in [&[0u16][..], &[47], &[92]] {
            assert!(
                parse_batch(
                    &record(name, 0, 1, 1),
                    &AtomicBool::new(false),
                    |_, _, _, _, _| {}
                )
                .is_err()
            );
        }
    }
    #[test]
    fn cancelled_batch_emits_nothing() {
        let mut count = 0;
        parse_batch(
            &record(&[97], 0, 1, 1),
            &AtomicBool::new(true),
            |_, _, _, _, _| count += 1,
        )
        .unwrap();
        assert_eq!(count, 0);
    }
}
