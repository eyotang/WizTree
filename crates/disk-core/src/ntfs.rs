//! NTFS 3.x decoding and read-only Windows volume transport.
//! Every binary field is bounds checked; unsupported layouts reject the fast
//! path so the caller can fall back to the ordinary directory walker.
use crate::{Node, Progress, ScanMethod, Snapshot};
#[cfg(windows)]
use std::sync::atomic::AtomicBool;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    io,
    path::Path,
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u16_at(data: &[u8], offset: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(
        data.get(offset..offset + 2)
            .ok_or_else(|| invalid("Truncated NTFS u16"))?
            .try_into()
            .unwrap(),
    ))
}
fn u32_at(data: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .ok_or_else(|| invalid("Truncated NTFS u32"))?
            .try_into()
            .unwrap(),
    ))
}
fn u64_at(data: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(|| invalid("Truncated NTFS u64"))?
            .try_into()
            .unwrap(),
    ))
}
const REFERENCE_MASK: u64 = 0x0000_ffff_ffff_ffff;
#[derive(Clone, Debug, PartialEq)]
struct Run {
    vcn: u64,
    lcn: Option<u64>,
    clusters: u64,
}
/// Checked RETRIEVAL_POINTERS_BUFFER. Callers request extent boundaries, so a
/// rounded-down/reallocated response rejects the snapshot rather than overlaps.
fn decode_retrieval(data: &[u8], requested: u64) -> io::Result<Vec<Run>> {
    let count = u32_at(data, 0)? as usize;
    let start = u64_at(data, 8)?;
    if start != requested || count == 0 || count > data.len().saturating_sub(16) / 16 {
        return Err(invalid("Invalid MFT retrieval pointer header"));
    }
    let mut runs = Vec::with_capacity(count);
    let mut current = start;
    for i in 0..count {
        let next = u64_at(data, 16 + i * 16)?;
        let lcn = u64_at(data, 24 + i * 16)? as i64;
        if next <= current || next > i64::MAX as u64 || lcn < -1 {
            return Err(invalid("Invalid MFT retrieval extent"));
        }
        runs.push(Run {
            vcn: current,
            lcn: if lcn == -1 { None } else { Some(lcn as u64) },
            clusters: next - current,
        });
        current = next;
    }
    Ok(runs)
}
fn decode_runs(data: &[u8], mut vcn: u64) -> io::Result<Vec<Run>> {
    let mut runs = Vec::new();
    let mut offset = 0usize;
    let mut lcn = 0i64;
    loop {
        let header = *data
            .get(offset)
            .ok_or_else(|| invalid("Unterminated NTFS runlist"))?;
        offset += 1;
        if header == 0 {
            return Ok(runs);
        }
        let count = (header & 15) as usize;
        let delta = (header >> 4) as usize;
        if count == 0 || count > 8 || delta > 8 {
            return Err(invalid("Invalid NTFS run width"));
        }
        let length_bytes = data
            .get(offset..offset + count)
            .ok_or_else(|| invalid("Truncated NTFS run length"))?;
        offset += count;
        let mut raw = [0u8; 8];
        raw[..count].copy_from_slice(length_bytes);
        let clusters = u64::from_le_bytes(raw);
        if clusters == 0 {
            return Err(invalid("Zero length NTFS run"));
        }
        let physical = if delta == 0 {
            None
        } else {
            let bytes = data
                .get(offset..offset + delta)
                .ok_or_else(|| invalid("Truncated NTFS run delta"))?;
            offset += delta;
            let mut raw = if bytes[delta - 1] & 128 != 0 {
                [255u8; 8]
            } else {
                [0u8; 8]
            };
            raw[..delta].copy_from_slice(bytes);
            lcn = lcn
                .checked_add(i64::from_le_bytes(raw))
                .ok_or_else(|| invalid("NTFS LCN overflow"))?;
            Some(u64::try_from(lcn).map_err(|_| invalid("Negative NTFS LCN"))?)
        };
        runs.push(Run {
            vcn,
            lcn: physical,
            clusters,
        });
        vcn = vcn
            .checked_add(clusters)
            .ok_or_else(|| invalid("NTFS VCN overflow"))?;
    }
}
fn fixup(record: &mut [u8], sector: usize) -> io::Result<()> {
    if record.get(..4) != Some(b"FILE") {
        return Err(invalid("Invalid MFT FILE signature"));
    }
    if sector < 256 || !record.len().is_multiple_of(sector) {
        return Err(invalid("Invalid MFT sector geometry"));
    }
    let offset = u16_at(record, 4)? as usize;
    let count = u16_at(record, 6)? as usize;
    if count != record.len() / sector + 1 {
        return Err(invalid("Invalid MFT update sequence count"));
    }
    let usa = record
        .get(offset..offset + count * 2)
        .ok_or_else(|| invalid("Truncated MFT update sequence"))?
        .to_vec();
    for i in 1..count {
        let end = i * sector - 2;
        if record[end..end + 2] != usa[..2] {
            return Err(invalid("Torn MFT sector detected"));
        }
        record[end..end + 2].copy_from_slice(&usa[i * 2..i * 2 + 2]);
    }
    Ok(())
}
fn attributes(record: &[u8]) -> io::Result<Vec<&[u8]>> {
    let used = u32_at(record, 24)? as usize;
    let data = record
        .get(..used)
        .ok_or_else(|| invalid("MFT used length exceeds record"))?;
    let mut offset = u16_at(record, 20)? as usize;
    let mut result = Vec::new();
    if offset < 42 {
        return Err(invalid("MFT attributes overlap header"));
    }
    loop {
        let kind = u32_at(data, offset)?;
        if kind == u32::MAX {
            return Ok(result);
        }
        let length = u32_at(data, offset + 4)? as usize;
        if length < 24 || !length.is_multiple_of(8) {
            return Err(invalid("Invalid NTFS attribute length"));
        }
        let attribute = data
            .get(offset..offset + length)
            .ok_or_else(|| invalid("Truncated NTFS attribute"))?;
        result.push(attribute);
        offset += length;
    }
}
fn resident_value(attribute: &[u8]) -> io::Result<&[u8]> {
    if attribute[8] != 0 {
        return Err(invalid("Nonresident value unsupported here"));
    }
    let size = u32_at(attribute, 16)? as usize;
    let offset = u16_at(attribute, 20)? as usize;
    attribute
        .get(offset..offset + size)
        .ok_or_else(|| invalid("Truncated NTFS resident value"))
}
/// NTFS timestamps are unsigned 100ns ticks since 1601-01-01 UTC.
/// Source: https://learn.microsoft.com/en-us/windows/win32/api/minwinbase/ns-minwinbase-filetime
pub(crate) fn ntfs_time(ticks: u64) -> Option<SystemTime> {
    const EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    if ticks == 0 {
        return None;
    }
    let delta = ticks.abs_diff(EPOCH_TICKS);
    let duration = Duration::new(delta / 10_000_000, ((delta % 10_000_000) * 100) as u32);
    if ticks >= EPOCH_TICKS {
        UNIX_EPOCH.checked_add(duration)
    } else {
        UNIX_EPOCH.checked_sub(duration)
    }
}
#[derive(Debug)]
struct Record {
    id: u64,
    base: u64,
    parent: u64,
    name: Option<OsString>,
    aliases: Vec<(u64, OsString)>,
    size: Option<(u64, u64)>,
    is_dir: bool,
    reparse: bool,
    modified: Option<SystemTime>,
}
fn parse_record(record: &[u8], id: u64) -> io::Result<Option<Record>> {
    let flags = u16_at(record, 22)?;
    if flags & 1 == 0 {
        return Ok(None);
    }
    let mut result = Record {
        id: id | ((u16_at(record, 16)? as u64) << 48),
        base: u64_at(record, 32)?,
        parent: 0,
        name: None,
        aliases: Vec::new(),
        size: None,
        is_dir: flags & 2 != 0,
        reparse: false,
        modified: None,
    };
    let mut name_rank = 0u8;
    let mut filename_modified = None;
    for attribute in attributes(record)? {
        match u32_at(attribute, 0)? {
            0x10 => {
                let value = resident_value(attribute)?;
                result.reparse |= u32_at(value, 32)? & 0x400 != 0;
                // ntfs3 ATTR_STD_INFO.m_time is at 0x08, distinct from c_time
                // (metadata change) at 0x10: https://github.com/torvalds/linux/blob/master/fs/ntfs3/ntfs.h
                result.modified = ntfs_time(u64_at(value, 8)?);
            }
            0x30 => {
                let value = resident_value(attribute)?;
                let length = *value
                    .get(64)
                    .ok_or_else(|| invalid("Truncated FILE_NAME"))?
                    as usize;
                let namespace = *value
                    .get(65)
                    .ok_or_else(|| invalid("Missing FILE_NAME namespace"))?;
                let rank = match namespace {
                    1 | 3 => 3,
                    0 => 2,
                    2 => 0,
                    _ => 0,
                };
                if rank > 0 {
                    if length == 0 {
                        return Err(invalid("Empty NTFS filename"));
                    }
                    let bytes = value
                        .get(66..66 + length * 2)
                        .ok_or_else(|| invalid("Truncated UTF16 filename"))?;
                    let utf16: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]]))
                        .collect();
                    #[cfg(windows)]
                    let name = {
                        use std::os::windows::ffi::OsStringExt;
                        OsString::from_wide(&utf16)
                    };
                    #[cfg(not(windows))]
                    let name = OsString::from(String::from_utf16_lossy(&utf16));
                    let parent = u64_at(value, 0)?;
                    if rank > name_rank {
                        if let Some(old) = result.name.take()
                            && (old != name || result.parent != parent)
                        {
                            result.aliases.push((result.parent, old));
                        }
                        result.aliases.retain(|(p, n)| *p != parent || *n != name);
                        result.name = Some(name);
                        result.parent = parent;
                        name_rank = rank;
                        filename_modified = ntfs_time(u64_at(value, 16)?);
                    } else if result
                        .name
                        .as_ref()
                        .is_none_or(|n| *n != name || result.parent != parent)
                        && !result
                            .aliases
                            .iter()
                            .any(|(p, n)| *p == parent && *n == name)
                    {
                        result.aliases.push((parent, name));
                    }
                }
            }
            0x80 if attribute[9] == 0 => {
                if attribute[8] == 0 {
                    let logical = resident_value(attribute)?.len() as u64;
                    // Match NTFS FILE_STANDARD_INFO/FILE_ID_BOTH_DIR_INFO: the
                    // resident value occupies an eight-byte-aligned record area.
                    result.size = Some((logical, (logical + 7) & !7));
                } else if u64_at(attribute, 16)? == 0 {
                    let logical = u64_at(attribute, 48)?;
                    let allocated = if u16_at(attribute, 12)? & 0x80ff != 0 {
                        u64_at(attribute, 64)?
                    } else {
                        u64_at(attribute, 40)?
                    };
                    result.size = Some((logical, allocated));
                }
            }
            0xc0 => result.reparse = true,
            _ => {}
        }
    }
    result.modified = result.modified.or(filename_modified);
    Ok(Some(result))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        fs::{File, OpenOptions},
        io::{Read, Seek, SeekFrom},
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };
    /// All raw volume buffers satisfy noncached sector alignment. Allocation
    /// owns exactly one initialized byte region and never escapes this thread.
    struct AlignedBuffer {
        pointer: std::ptr::NonNull<u8>,
        layout: std::alloc::Layout,
    }
    impl AlignedBuffer {
        fn new(length: usize, alignment: usize) -> io::Result<Self> {
            let layout = std::alloc::Layout::from_size_align(length, alignment.max(65536))
                .map_err(|_| invalid("Invalid raw I/O buffer layout"))?;
            // SAFETY: layout is nonzero and valid; zero initialization makes
            // the entire region readable before the first device operation.
            let pointer = std::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        "Raw I/O buffer allocation failed",
                    )
                })?;
            Ok(Self { pointer, layout })
        }
    }
    impl std::ops::Deref for AlignedBuffer {
        type Target = [u8];
        fn deref(&self) -> &[u8] {
            // SAFETY: pointer owns layout.size initialized bytes for self's lifetime.
            unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.layout.size()) }
        }
    }
    impl std::ops::DerefMut for AlignedBuffer {
        fn deref_mut(&mut self) -> &mut [u8] {
            // SAFETY: unique &mut self ensures exclusive access to the owned region.
            unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.layout.size()) }
        }
    }
    impl Drop for AlignedBuffer {
        fn drop(&mut self) {
            // SAFETY: exact allocator pointer/layout pair, freed once by its owner.
            unsafe { std::alloc::dealloc(self.pointer.as_ptr(), self.layout) }
        }
    }
    const FSCTL_GET_NTFS_VOLUME_DATA: u32 = 0x90064;
    const FSCTL_GET_NTFS_FILE_RECORD: u32 = 0x90068;
    const FSCTL_GET_RETRIEVAL_POINTERS: u32 = 0x90073;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(file: *mut c_void, information: *mut u32) -> i32;
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }
    fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let mut returned = 0u32;
        // SAFETY: handle is owned by file; byte slices remain live, and the call
        // is synchronous (null OVERLAPPED) with checked 32-bit buffer lengths.
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                input.as_ptr().cast(),
                input
                    .len()
                    .try_into()
                    .map_err(|_| invalid("IOCTL input too long"))?,
                output.as_mut_ptr().cast(),
                output
                    .len()
                    .try_into()
                    .map_err(|_| invalid("IOCTL output too long"))?,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(returned as usize)
        }
    }
    fn retrieval_runs(drive: u8, cancel: &AtomicBool) -> io::Result<Vec<Run>> {
        let path = format!(r"{}:\$MFT", drive as char);
        let mft = OpenOptions::new()
            .access_mode(0x80)
            .share_mode(7)
            .open(path)?;
        let mut output = vec![0u64; 65536 / 8];
        let mut requested = 0u64;
        let mut runs = Vec::new();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "MFT extent query cancelled",
                ));
            }
            let mut returned = 0u32;
            // SAFETY: valid owned file, aligned live input/output allocations,
            // synchronous call. ERROR_MORE_DATA also supplies complete extents.
            let ok = unsafe {
                DeviceIoControl(
                    mft.as_raw_handle(),
                    FSCTL_GET_RETRIEVAL_POINTERS,
                    (&requested as *const u64).cast(),
                    8,
                    output.as_mut_ptr().cast(),
                    (output.len() * 8) as u32,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            };
            let more = if ok == 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(38) if !runs.is_empty() => break,
                    Some(234) => true,
                    _ => return Err(error),
                }
            } else {
                false
            };
            if returned as usize > output.len() * 8 {
                return Err(invalid("MFT extent output overflow"));
            }
            // SAFETY: returned bounds are checked and all output words initialized.
            let data = unsafe {
                std::slice::from_raw_parts(output.as_ptr().cast::<u8>(), returned as usize)
            };
            let batch = decode_retrieval(data, requested)?;
            let last = batch.last().unwrap();
            requested = last
                .vcn
                .checked_add(last.clusters)
                .ok_or_else(|| invalid("MFT VCN overflow"))?;
            runs.extend(batch);
            if runs.len() > 1_000_000 {
                return Err(invalid("Too many MFT extents"));
            }
            if !more {
                break;
            }
        }
        Ok(runs)
    }
    fn read_at(file: &mut File, offset: u64, data: &mut [u8]) -> io::Result<()> {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(data)
    }
    fn mft_mapping(
        record: &[u8],
        runs: &mut Vec<Run>,
        extensions: &mut Vec<u64>,
    ) -> io::Result<()> {
        for attribute in attributes(record)? {
            match u32_at(attribute, 0)? {
                0x80 if attribute[9] == 0 && attribute[8] == 1 => {
                    let offset = u16_at(attribute, 32)? as usize;
                    runs.extend(decode_runs(
                        attribute
                            .get(offset..)
                            .ok_or_else(|| invalid("Invalid MFT run offset"))?,
                        u64_at(attribute, 16)?,
                    )?);
                }
                0x20 => {
                    let value = resident_value(attribute)?;
                    let mut offset = 0;
                    while offset < value.len() {
                        let kind = u32_at(value, offset)?;
                        let length = u16_at(value, offset + 4)? as usize;
                        if length < 26 || offset + length > value.len() {
                            return Err(invalid("Invalid MFT attribute list entry"));
                        }
                        if kind == 0x80 && value[offset + 6] == 0 {
                            let id = u64_at(value, offset + 16)?;
                            if id & REFERENCE_MASK != 0 {
                                extensions.push(id);
                            }
                        }
                        offset += length;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    pub(super) fn scan_volume(
        root: &Path,
        cancel: &AtomicBool,
        progress: &Progress,
        start: Instant,
    ) -> io::Result<Snapshot> {
        let root_text = root.as_os_str().to_string_lossy();
        let drive = root_text.strip_prefix(r"\\?\").unwrap_or(&root_text);
        let bytes = drive.as_bytes();
        if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || &bytes[1..3] != b":\\" {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "MFT fast path requires a local drive path",
            ));
        }
        // Resolve the exact directory reference including its sequence number.
        // Keep the handle alive through assembly so the directory cannot be reused.
        let target = OpenOptions::new()
            .access_mode(0x80)
            .share_mode(7)
            .custom_flags(0x0220_0000)
            .open(root)?;
        let mut identity = [0u32; 13]; // BY_HANDLE_FILE_INFORMATION, 52 bytes.
        // SAFETY: owned handle, correctly aligned and sized synchronous output.
        if unsafe { GetFileInformationByHandle(target.as_raw_handle(), identity.as_mut_ptr()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if identity[0] & 0x10 == 0 || identity[0] & 0x400 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "MFT target must be an ordinary directory",
            ));
        }
        let target_reference = ((identity[11] as u64) << 32) | identity[12] as u64;
        let volume_path = format!(r"\\.\{}:", bytes[0] as char);
        // Access is read-only. Lack of elevated raw-volume permission causes a
        // clean fallback; the application never launches an elevation prompt.
        let mut volume = OpenOptions::new()
            .read(true)
            .share_mode(7)
            .custom_flags(0x2000_0000)
            .open(volume_path)?;
        let mut geometry = [0u8; 128];
        let returned = ioctl(&volume, FSCTL_GET_NTFS_VOLUME_DATA, &[], &mut geometry)?;
        if returned < 104 || u16_at(&geometry, 100)? != 3 || u16_at(&geometry, 102)? > 1 {
            return Err(invalid("Unsupported NTFS version"));
        }
        let sector = u32_at(&geometry, 40)? as usize;
        let cluster = u32_at(&geometry, 44)? as u64;
        let record_size = u32_at(&geometry, 48)? as usize;
        let valid = u64_at(&geometry, 56)?;
        if !(256..=65536).contains(&sector)
            || !sector.is_power_of_two()
            || cluster < sector as u64
            || !cluster.is_power_of_two()
            || !(512..=65536).contains(&record_size)
            || !record_size.is_multiple_of(sector)
            || valid % record_size as u64 != 0
        {
            return Err(invalid("Invalid NTFS geometry"));
        }
        // Retrieval pointers cover nonresident ATTRIBUTE_LIST layouts without
        // decoding that stream. Keep bootstrap mapping as a checked fallback.
        let mut runs = match retrieval_runs(bytes[0], cancel) {
            Ok(runs) => runs,
            Err(extent_error) => {
                let fallback = (|| -> io::Result<Vec<Run>> {
                    let mft_offset = u64_at(&geometry, 64)?
                        .checked_mul(cluster)
                        .ok_or_else(|| invalid("MFT start overflow"))?;
                    let mut bootstrap = AlignedBuffer::new(record_size, sector)?;
                    read_at(&mut volume, mft_offset, &mut bootstrap)?;
                    fixup(&mut bootstrap, sector)?;
                    let mft_reference = (u16_at(&bootstrap, 16)? as u64) << 48;
                    let mut runs = Vec::new();
                    let mut extensions = Vec::new();
                    mft_mapping(&bootstrap, &mut runs, &mut extensions)?;
                    let mut visited = HashSet::new();
                    while let Some(id) = extensions.pop() {
                        if !visited.insert(id) {
                            continue;
                        }
                        if visited.len() > 1024 {
                            return Err(invalid("Too many MFT mapping extensions"));
                        }
                        let mut output = vec![0u8; record_size + 16];
                        let returned = ioctl(
                            &volume,
                            FSCTL_GET_NTFS_FILE_RECORD,
                            &(id & REFERENCE_MASK).to_le_bytes(),
                            &mut output,
                        )?;
                        if returned < 12 + record_size
                            || u64_at(&output, 0)? & REFERENCE_MASK != id & REFERENCE_MASK
                            || u32_at(&output, 8)? as usize != record_size
                        {
                            return Err(invalid("MFT mapping extension mismatch"));
                        }
                        let record = &mut output[12..12 + record_size];
                        fixup(record, sector)?;
                        if u16_at(record, 16)? as u64 != id >> 48
                            || u64_at(record, 32)? != mft_reference
                        {
                            return Err(invalid("Stale MFT mapping extension reference"));
                        }
                        mft_mapping(record, &mut runs, &mut extensions)?;
                    }
                    Ok(runs)
                })();
                fallback.map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("MFT extents: {extent_error}; bootstrap mapping: {error}"),
                    )
                })?
            }
        };
        runs.sort_by_key(|run| run.vcn);
        let mut expected = 0u64;
        for run in &runs {
            if run.vcn != expected || run.lcn.is_none() {
                return Err(invalid("Incomplete or sparse MFT mapping"));
            }
            expected = expected
                .checked_add(run.clusters)
                .ok_or_else(|| invalid("MFT length overflow"))?;
        }
        if expected
            .checked_mul(cluster)
            .ok_or_else(|| invalid("MFT allocation overflow"))?
            < valid
        {
            return Err(invalid("Incomplete MFT run coverage"));
        }
        // Some Windows/filesystem configurations permit cached MFT stream
        // reads under backup semantics. Prefer them; otherwise keep raw extents.
        let mut cached_mft = OpenOptions::new()
            .access_mode(1)
            .share_mode(7)
            .custom_flags(0x0a00_0000)
            .open(format!(r"{}:\$MFT", bytes[0] as char))
            .ok();
        progress.mft_bytes.store(valid, Ordering::Relaxed);
        progress
            .mft_extents
            .store(runs.len() as u64, Ordering::Relaxed);
        progress
            .mft_cached
            .store(cached_mft.is_some(), Ordering::Relaxed);
        let mut records = Vec::new();
        let mut scanned = 0u64;
        let batch = (4 * 1024 * 1024 / record_size) * record_size;
        let mut buffer = AlignedBuffer::new(batch, sector)?;
        for run in runs {
            let run_start = run
                .vcn
                .checked_mul(cluster)
                .ok_or_else(|| invalid("MFT VCN byte overflow"))?;
            if run_start >= valid {
                break;
            }
            let run_bytes = run
                .clusters
                .checked_mul(cluster)
                .ok_or_else(|| invalid("MFT run overflow"))?
                .min(valid - run_start);
            if !run_bytes.is_multiple_of(record_size as u64) {
                return Err(invalid("MFT record split across run boundary"));
            }
            let disk_start = run
                .lcn
                .unwrap()
                .checked_mul(cluster)
                .ok_or_else(|| invalid("MFT LCN byte overflow"))?;
            let mut offset = 0u64;
            while offset < run_bytes {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let length = (run_bytes - offset).min(batch as u64) as usize;
                let cached_ok = if let Some(file) = &mut cached_mft {
                    read_at(file, run_start + offset, &mut buffer[..length]).is_ok()
                } else {
                    false
                };
                if !cached_ok {
                    cached_mft = None;
                    progress.mft_cached.store(false, Ordering::Relaxed);
                    read_at(
                        &mut volume,
                        disk_start
                            .checked_add(offset)
                            .ok_or_else(|| invalid("MFT read offset overflow"))?,
                        &mut buffer[..length],
                    )?;
                }
                for (index, record) in buffer[..length].chunks_exact_mut(record_size).enumerate() {
                    let id = (run_start + offset) / record_size as u64 + index as u64;
                    // Unused records may contain arbitrary stale or zero bytes.
                    if record.get(..4) != Some(b"FILE") {
                        continue;
                    }
                    if u16_at(record, 22)? & 1 == 0 {
                        continue;
                    }
                    fixup(record, sector)?;
                    if let Some(record) = parse_record(record, id)? {
                        records.push(record);
                    }
                }
                offset += length as u64;
                scanned += length as u64;
                progress
                    .mft_records
                    .store(scanned / record_size as u64, Ordering::Relaxed);
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
        }
        if scanned < valid && !cancel.load(Ordering::Relaxed) {
            return Err(invalid("MFT stream ended early"));
        }
        assemble_subtree(
            root,
            records,
            cancel.load(Ordering::Relaxed),
            start,
            progress,
            Some(target_reference),
        )
    }
    #[cfg(test)]
    mod buffer_tests {
        use super::*;
        #[test]
        fn raw_buffers_obey_sector_alignment() {
            for sector in [512, 4096, 65536] {
                let mut buffer = AlignedBuffer::new(4 * 1024 * 1024, sector).unwrap();
                assert_eq!(buffer.as_ptr() as usize % 65536, 0);
                assert_eq!(buffer.len(), 4 * 1024 * 1024);
                buffer[0] = 17;
                assert_eq!(buffer[0], 17);
            }
        }
    }
}

fn assemble_subtree(
    root: &Path,
    mut records: Vec<Record>,
    cancelled: bool,
    start: Instant,
    progress: &Progress,
    wanted_reference: Option<u64>,
) -> io::Result<Snapshot> {
    let by_id: HashMap<u64, usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.base == 0)
        .map(|(i, r)| (r.id, i))
        .collect();
    // Attributes can spill into extension records. Merge their unnamed stream
    // and filename into the base record before constructing the parent graph.
    for index in 0..records.len() {
        let base = records[index].base;
        if base == 0 {
            continue;
        }
        if let Some(&destination) = by_id.get(&base) {
            if records[index].size.is_some() {
                records[destination].size = records[index].size;
            }
            records[destination].reparse |= records[index].reparse;
            if records[destination].modified.is_none() {
                records[destination].modified = records[index].modified;
            }
            let mut aliases = std::mem::take(&mut records[index].aliases);
            if let Some(name) = records[index].name.take().filter(|name| !name.is_empty()) {
                let parent = records[index].parent;
                if records[destination].name.is_none() {
                    records[destination].name = Some(name);
                    records[destination].parent = parent;
                } else if records[destination]
                    .name
                    .as_ref()
                    .is_none_or(|n| *n != name || records[destination].parent != parent)
                {
                    aliases.push((parent, name));
                }
            }
            for alias in aliases {
                if records[destination]
                    .name
                    .as_ref()
                    .is_none_or(|name| *name != alias.1 || records[destination].parent != alias.0)
                    && !records[destination].aliases.contains(&alias)
                {
                    records[destination].aliases.push(alias);
                }
            }
        }
    }
    if !cancelled && wanted_reference.is_none_or(|reference| reference & REFERENCE_MASK == 5) {
        for record in &records {
            if record.base != 0 && !by_id.contains_key(&record.base) {
                return Err(invalid("Missing or stale MFT base record reference"));
            }
            if record.base == 0
                && record.id & REFERENCE_MASK >= 16
                && !record.reparse
                && !by_id.contains_key(&record.parent)
            {
                return Err(invalid("Missing or stale MFT parent reference"));
            }
        }
    }
    // Unrelated orphans on an active volume must not invalidate a requested
    // subtree. A stale reference to a node *inside* that subtree must reject it.
    let mut stale_parents: HashSet<u64> = records
        .iter()
        .filter(|r| r.base == 0 && !r.reparse && !by_id.contains_key(&r.parent))
        .map(|r| r.parent & REFERENCE_MASK)
        .collect();
    let stale_bases: HashSet<u64> = records
        .iter()
        .filter(|r| r.base != 0 && !by_id.contains_key(&r.base))
        .map(|r| r.base & REFERENCE_MASK)
        .collect();
    let mut children: HashMap<u64, Vec<(usize, Option<usize>)>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if record.base == 0
            && record.id & REFERENCE_MASK >= 16
            && !record.reparse
            && record.name.is_some()
        {
            children
                .entry(record.parent)
                .or_default()
                .push((index, None));
            if !record.is_dir {
                for (alias, (parent, _)) in record.aliases.iter().enumerate() {
                    if !by_id.contains_key(parent) {
                        stale_parents.insert(*parent & REFERENCE_MASK);
                    }
                    children
                        .entry(*parent)
                        .or_default()
                        .push((index, Some(alias)));
                }
            }
        }
    }
    let root_reference = wanted_reference.or_else(|| {
        records
            .iter()
            .find(|r| r.id & REFERENCE_MASK == 5 && r.base == 0)
            .map(|r| r.id)
    });
    let root_reference = match root_reference {
        Some(reference) => reference,
        None if cancelled => 5,
        None => return Err(invalid("MFT root record missing")),
    };
    let root_record = by_id.get(&root_reference).map(|&id| &records[id]);
    if !cancelled && root_record.is_none() {
        return Err(invalid("MFT target directory missing or sequence changed"));
    }
    if root_record.is_some_and(|r| !r.is_dir || r.reparse) {
        return Err(invalid("MFT target is not an ordinary directory"));
    }
    let mut nodes = vec![Node {
        parent: None,
        name: root.as_os_str().to_os_string(),
        size: 0,
        allocated: 0,
        is_dir: true,
        modified: root_record.and_then(|r| r.modified),
        children: Vec::new(),
    }];
    let mut queue = VecDeque::from([(root_reference, 0usize)]);
    let mut seen = HashSet::from([root_reference]);
    let (mut files, mut dirs) = (0u64, 1u64);
    while let Some((parent_id, parent_index)) = queue.pop_front() {
        if !cancelled
            && (stale_parents.contains(&(parent_id & REFERENCE_MASK))
                || stale_bases.contains(&(parent_id & REFERENCE_MASK)))
        {
            return Err(invalid("Stale reference inside requested NTFS subtree"));
        }
        for (index, alias) in children.remove(&parent_id).unwrap_or_default() {
            let record = &mut records[index];
            if !cancelled && stale_bases.contains(&(record.id & REFERENCE_MASK)) {
                return Err(invalid(
                    "Stale data extension inside requested NTFS subtree",
                ));
            }
            if record.is_dir && !seen.insert(record.id) {
                return Err(invalid("Cycle in NTFS filename graph"));
            }
            let (size, allocated) = if record.is_dir {
                (0, 0)
            } else {
                match record.size {
                    Some(size) => size,
                    None if cancelled => (0, 0),
                    None => return Err(invalid("Unnamed file stream missing from MFT")),
                }
            };
            let id = nodes.len();
            nodes.push(Node {
                parent: Some(parent_index),
                name: if let Some(alias) = alias {
                    std::mem::take(&mut record.aliases[alias].1)
                } else {
                    record.name.take().unwrap()
                },
                size,
                allocated,
                is_dir: record.is_dir,
                modified: record.modified,
                children: Vec::new(),
            });
            nodes[parent_index].children.push(id);
            if record.is_dir {
                dirs += 1;
                queue.push_back((record.id, id));
            } else {
                files += 1;
            }
        }
    }
    for id in (1..nodes.len()).rev() {
        let parent = nodes[id].parent.unwrap();
        nodes[parent].size = nodes[parent].size.saturating_add(nodes[id].size);
        nodes[parent].allocated = nodes[parent].allocated.saturating_add(nodes[id].allocated);
    }
    progress.files.store(files, Ordering::Relaxed);
    progress.dirs.store(dirs, Ordering::Relaxed);
    progress.bytes.store(nodes[0].size, Ordering::Relaxed);
    Ok(Snapshot {
        root: root.to_path_buf(),
        nodes,
        elapsed: start.elapsed(),
        files,
        dirs,
        errors: 0,
        cancelled,
        method: ScanMethod::NtfsMft,
        fallback_reason: None,
        issues: Vec::new(),
    })
}
#[cfg(windows)]
pub(crate) fn is_volume_root(root: &Path) -> bool {
    let text = root.as_os_str().to_string_lossy();
    let drive = text.strip_prefix(r"\\?\").unwrap_or(&text).as_bytes();
    drive.len() == 3 && drive[0].is_ascii_alphabetic() && &drive[1..] == b":\\"
}
#[cfg(test)]
fn assemble(
    root: &Path,
    records: Vec<Record>,
    cancelled: bool,
    start: Instant,
    progress: &Progress,
) -> io::Result<Snapshot> {
    assemble_subtree(root, records, cancelled, start, progress, None)
}
#[cfg(windows)]
pub(crate) fn try_scan(
    root: &Path,
    cancel: &AtomicBool,
    progress: &Progress,
    start: Instant,
) -> io::Result<Snapshot> {
    windows::scan_volume(root, cancel, progress, start)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runlist_supports_negative_deltas_and_sparse_extents() {
        let runs = decode_runs(&[0x11, 3, 10, 0x11, 2, 0xfe, 0x01, 4, 0], 0).unwrap();
        assert_eq!(
            runs,
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(10),
                    clusters: 3
                },
                Run {
                    vcn: 3,
                    lcn: Some(8),
                    clusters: 2
                },
                Run {
                    vcn: 5,
                    lcn: None,
                    clusters: 4
                }
            ]
        );
    }
    #[test]
    fn runlist_rejects_invalid_width_and_overflow() {
        assert!(decode_runs(&[0x90, 0], 0).is_err());
        assert!(decode_runs(&[0x11, 1, 0xff, 0], 0).is_err());
        assert!(decode_runs(&[0x11, 1, 1, 0], u64::MAX).is_err());
        assert!(decode_runs(&[0x11, 1], 0).is_err());
    }
    #[test]
    fn fixup_detects_torn_sector() {
        let mut record = vec![0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&48u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[48..54].copy_from_slice(&[9, 9, 1, 2, 3, 4]);
        record[510..512].copy_from_slice(&[9, 9]);
        record[1022..1024].copy_from_slice(&[9, 9]);
        let mut torn = record.clone();
        torn[510] = 8;
        assert!(fixup(&mut torn, 512).is_err());
        fixup(&mut record, 512).unwrap();
        assert_eq!(&record[510..512], &[1, 2]);
        assert_eq!(&record[1022..1024], &[3, 4]);
    }
    #[test]
    fn every_truncated_record_rejects_without_panicking() {
        let mut record = vec![0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[20..22].copy_from_slice(&48u16.to_le_bytes());
        record[22..24].copy_from_slice(&1u16.to_le_bytes());
        record[24..28].copy_from_slice(&56u32.to_le_bytes());
        record[48..52].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_record(&record, 16).is_ok());
        for len in 0..56 {
            assert!(parse_record(&record[..len], 16).is_err());
        }
    }
    #[test]
    fn graph_aggregates_and_merges_extension_stream() {
        let make = |id, base, parent, name: &str, size, is_dir| Record {
            id,
            base,
            parent,
            name: Some(name.into()),
            aliases: Vec::new(),
            size,
            is_dir,
            reparse: false,
            modified: None,
        };
        let records = vec![
            make(5, 0, 5, "root", None, true),
            make(16, 0, 5, "folder", None, true),
            make(17, 0, 16, "file", None, false),
            make(18, 17, 0, "", Some((31, 4096)), false),
            make(19, 0, 5, "junction", None, true),
            make(20, 0, 19, "hidden", Some((999, 4096)), false),
        ];
        let mut records = records;
        records[4].reparse = true;
        let snapshot = assemble(
            Path::new("C:\\"),
            records,
            false,
            Instant::now(),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!((snapshot.files, snapshot.dirs), (1, 2));
        assert_eq!(
            (snapshot.nodes[0].size, snapshot.nodes[0].allocated),
            (31, 4096)
        );
    }
    #[test]
    fn missing_file_size_rejects_full_snapshot() {
        let records = vec![Record {
            id: 16,
            base: 0,
            parent: 5,
            name: Some("unknown".into()),
            aliases: Vec::new(),
            size: None,
            is_dir: false,
            reparse: false,
            modified: None,
        }];
        assert!(
            assemble(
                Path::new("C:\\"),
                records,
                false,
                Instant::now(),
                &Progress::default()
            )
            .is_err()
        );
    }
    fn file_record_with_data(nonresident: bool, sparse: bool) -> Vec<u8> {
        let mut record = vec![0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[20..22].copy_from_slice(&48u16.to_le_bytes());
        record[22..24].copy_from_slice(&1u16.to_le_bytes());
        let mut offset = 48usize;
        let name = "sample.bin".encode_utf16().collect::<Vec<_>>();
        let value_size = 66 + name.len() * 2;
        let length = (24 + value_size + 7) & !7;
        let attribute = &mut record[offset..offset + length];
        attribute[..4].copy_from_slice(&0x30u32.to_le_bytes());
        attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        attribute[16..20].copy_from_slice(&(value_size as u32).to_le_bytes());
        attribute[20..22].copy_from_slice(&24u16.to_le_bytes());
        attribute[24..32].copy_from_slice(&5u64.to_le_bytes());
        attribute[24 + 64] = name.len() as u8;
        attribute[24 + 65] = 1;
        for (i, c) in name.into_iter().enumerate() {
            attribute[24 + 66 + i * 2..24 + 68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        offset += length;
        let length = if nonresident { 80 } else { 32 };
        let attribute = &mut record[offset..offset + length];
        attribute[..4].copy_from_slice(&0x80u32.to_le_bytes());
        attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        if nonresident {
            attribute[8] = 1;
            attribute[40..48].copy_from_slice(&8192u64.to_le_bytes());
            attribute[48..56].copy_from_slice(&5000u64.to_le_bytes());
            if sparse {
                attribute[12..14].copy_from_slice(&0x8000u16.to_le_bytes());
                attribute[64..72].copy_from_slice(&4096u64.to_le_bytes());
            }
        } else {
            attribute[16..20].copy_from_slice(&3u32.to_le_bytes());
            attribute[20..22].copy_from_slice(&24u16.to_le_bytes());
            attribute[24..27].copy_from_slice(b"abc");
        }
        offset += length;
        record[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        record[24..28].copy_from_slice(&((offset + 8) as u32).to_le_bytes());
        record
    }
    #[test]
    fn filename_and_data_attributes_report_real_sizes() {
        for (nonresident, sparse, expected) in [
            (false, false, (3, 8)),
            (true, false, (5000, 8192)),
            (true, true, (5000, 4096)),
        ] {
            let record = parse_record(&file_record_with_data(nonresident, sparse), 16)
                .unwrap()
                .unwrap();
            assert_eq!(record.parent, 5);
            assert_eq!(record.name, Some("sample.bin".into()));
            assert_eq!(record.size, Some(expected));
        }
    }
    #[test]
    fn resident_stream_alignment_and_truncation_match_windows_metadata() {
        for length in [0u32, 1, 3, 7, 8] {
            let mut bytes = file_record_with_data(false, false);
            let mut offset = u16_at(&bytes, 20).unwrap() as usize;
            while u32_at(&bytes, offset).unwrap() != 0x80 {
                offset += u32_at(&bytes, offset + 4).unwrap() as usize;
            }
            bytes[offset + 16..offset + 20].copy_from_slice(&length.to_le_bytes());
            let parsed = parse_record(&bytes, 16).unwrap().unwrap();
            assert_eq!(
                parsed.size,
                Some((length as u64, if length == 0 { 0 } else { 8 }))
            );
            bytes[offset + 16..offset + 20].copy_from_slice(&116u32.to_le_bytes());
            assert!(parse_record(&bytes, 16).is_err());
        }
    }
    #[test]
    fn stale_parent_and_base_sequences_are_rejected() {
        let root = 5 | (1u64 << 48);
        let folder = 16 | (2u64 << 48);
        let make = |id, base, parent, name: &str, is_dir| Record {
            id,
            base,
            parent,
            name: Some(name.into()),
            aliases: Vec::new(),
            size: Some((7, 4096)),
            is_dir,
            reparse: false,
            modified: None,
        };
        let records = vec![
            make(root, 0, root, "root", true),
            make(folder, 0, root, "folder", true),
            make(17 | (3u64 << 48), 0, 16 | (1u64 << 48), "file", false),
        ];
        assert!(
            assemble(
                Path::new("C:\\"),
                records,
                false,
                Instant::now(),
                &Progress::default()
            )
            .is_err()
        );
        let records = vec![
            make(root, 0, root, "root", true),
            make(folder, 0, root, "folder", true),
            make(17 | (3u64 << 48), 16 | (1u64 << 48), 0, "", false),
        ];
        assert!(
            assemble(
                Path::new("C:\\"),
                records,
                false,
                Instant::now(),
                &Progress::default()
            )
            .is_err()
        );
        let records = vec![
            make(root, 0, root, "root", true),
            make(folder, 0, root, "folder", true),
            make(17 | (3u64 << 48), 0, folder, "file", false),
        ];
        let result = assemble(
            Path::new("C:\\"),
            records,
            false,
            Instant::now(),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(result.files, 1);
        assert_eq!(result.nodes[0].size, 7);
    }
    #[test]
    fn parser_preserves_reference_sequence_bits() {
        let mut record = file_record_with_data(true, false);
        record[16..18].copy_from_slice(&9u16.to_le_bytes());
        let parent = 5 | (3u64 << 48);
        record[72..80].copy_from_slice(&parent.to_le_bytes());
        let base = 22 | (7u64 << 48);
        record[32..40].copy_from_slice(&base.to_le_bytes());
        let parsed = parse_record(&record, 16).unwrap().unwrap();
        assert_eq!(parsed.id, 16 | (9u64 << 48));
        assert_eq!(parsed.parent, parent);
        assert_eq!(parsed.base, base);
    }
    #[test]
    fn filetime_conversion_preserves_epoch_fraction_and_pre_unix_times() {
        let epoch = 116_444_736_000_000_000u64;
        assert_eq!(ntfs_time(0), None);
        assert_eq!(ntfs_time(epoch), Some(UNIX_EPOCH));
        assert_eq!(
            ntfs_time(epoch + 12_345_678),
            Some(UNIX_EPOCH + Duration::new(1, 234_567_800))
        );
        assert_eq!(
            ntfs_time(epoch - 10_000_000),
            Some(UNIX_EPOCH - Duration::from_secs(1))
        );
    }
    #[test]
    fn standard_information_modification_time_overrides_stale_filename_copy() {
        let mut record = file_record_with_data(true, false);
        let used = u32_at(&record, 24).unwrap() as usize;
        record.copy_within(48..used, 120);
        record[48..120].fill(0);
        let attribute = &mut record[48..120];
        attribute[..4].copy_from_slice(&0x10u32.to_le_bytes());
        attribute[4..8].copy_from_slice(&72u32.to_le_bytes());
        attribute[16..20].copy_from_slice(&48u32.to_le_bytes());
        attribute[20..22].copy_from_slice(&24u16.to_le_bytes());
        let epoch = 116_444_736_000_000_000u64;
        attribute[32..40].copy_from_slice(&(epoch + 420_000_000).to_le_bytes());
        record[160..168].copy_from_slice(&(epoch + 110_000_000).to_le_bytes());
        record[24..28].copy_from_slice(&((used + 72) as u32).to_le_bytes());
        let parsed = parse_record(&record, 16).unwrap().unwrap();
        assert_eq!(parsed.modified, Some(UNIX_EPOCH + Duration::from_secs(42)));
        let mut filename_only = file_record_with_data(true, false);
        filename_only[88..96].copy_from_slice(&(epoch + 110_000_000).to_le_bytes());
        assert_eq!(
            parse_record(&filename_only, 16).unwrap().unwrap().modified,
            Some(UNIX_EPOCH + Duration::from_secs(11))
        );
    }
}

#[cfg(test)]
mod subtree_tests {
    use super::*;
    fn record(
        id: u64,
        parent: u64,
        name: &str,
        directory: bool,
        size: Option<(u64, u64)>,
    ) -> Record {
        Record {
            id,
            base: 0,
            parent,
            name: Some(name.into()),
            aliases: Vec::new(),
            size,
            is_dir: directory,
            reparse: false,
            modified: None,
        }
    }
    #[test]
    fn subtree_uses_full_reference_excludes_siblings_reparse_and_unrelated_orphans() {
        let target = (2u64 << 48) | 16;
        let mut link = record(22, target, "link", true, None);
        link.reparse = true;
        let mut extension = record(21, 0, "", false, Some((31, 4096)));
        extension.base = 20;
        let records = vec![
            record(5, 5, "root", true, None),
            record(target, 5, "chosen", true, None),
            record(17, 5, "sibling", true, None),
            record(18, 17, "outside.bin", false, Some((999, 4096))),
            record(19, target, "nested", true, None),
            record(20, 19, "file.bin", false, None),
            extension,
            link,
            record(23, 22, "hidden.bin", false, Some((555, 4096))),
            record(24, 999, "unrelated orphan", false, Some((8, 4096))),
        ];
        let snapshot = assemble_subtree(
            Path::new(r"D:\chosen"),
            records,
            false,
            Instant::now(),
            &Progress::default(),
            Some(target),
        )
        .unwrap();
        assert_eq!((snapshot.files, snapshot.dirs), (1, 2));
        assert_eq!(
            (snapshot.nodes[0].size, snapshot.nodes[0].allocated),
            (31, 4096)
        );
        assert_eq!(
            snapshot.path(2),
            Path::new(r"D:\chosen").join("nested").join("file.bin")
        );
    }
    #[test]
    fn subtree_rejects_reused_target_and_stale_descendant_references() {
        let target = (2u64 << 48) | 16;
        let root = || record(target, 5, "chosen", true, None);
        assert!(
            assemble_subtree(
                Path::new("root"),
                vec![root()],
                false,
                Instant::now(),
                &Progress::default(),
                Some((3u64 << 48) | 16)
            )
            .is_err()
        );
        let stale = record(20, (3u64 << 48) | 16, "file", false, Some((1, 1)));
        assert!(
            assemble_subtree(
                Path::new("root"),
                vec![root(), stale],
                false,
                Instant::now(),
                &Progress::default(),
                Some(target)
            )
            .is_err()
        );
        let mut extension = record(21, 0, "", false, Some((9, 4096)));
        extension.base = (3u64 << 48) | 20;
        assert!(
            assemble_subtree(
                Path::new("root"),
                vec![
                    root(),
                    record(20, target, "file", false, Some((1, 1))),
                    extension
                ],
                false,
                Instant::now(),
                &Progress::default(),
                Some(target)
            )
            .is_err()
        );
    }
    #[test]
    fn missing_or_file_target_rejects_complete_scan_but_cancel_remains_partial() {
        assert!(
            assemble_subtree(
                Path::new("root"),
                vec![],
                false,
                Instant::now(),
                &Progress::default(),
                Some(16)
            )
            .is_err()
        );
        assert!(
            assemble_subtree(
                Path::new("root"),
                vec![record(16, 5, "file", false, Some((1, 1)))],
                false,
                Instant::now(),
                &Progress::default(),
                Some(16)
            )
            .is_err()
        );
        let snapshot = assemble_subtree(
            Path::new("root"),
            vec![],
            true,
            Instant::now(),
            &Progress::default(),
            Some(16),
        )
        .unwrap();
        assert!(snapshot.cancelled);
        assert_eq!(snapshot.files, 0);
        assert_eq!(snapshot.nodes.len(), 1);
    }
    #[cfg(windows)]
    #[test]
    fn automatic_mft_only_selects_actual_drive_roots() {
        for path in [r"D:\", r"\\?\D:\"] {
            assert!(is_volume_root(Path::new(path)));
        }
        for path in [
            r"D:\chosen",
            r"\\?\D:\chosen",
            r"\\server\share\",
            r"D:",
            "relative",
        ] {
            assert!(!is_volume_root(Path::new(path)));
        }
    }
}

#[cfg(test)]
mod retrieval_tests {
    use super::*;
    fn data(start: u64, pairs: &[(u64, i64)]) -> Vec<u8> {
        let mut bytes = vec![0; 16 + pairs.len() * 16];
        bytes[..4].copy_from_slice(&(pairs.len() as u32).to_le_bytes());
        bytes[8..16].copy_from_slice(&start.to_le_bytes());
        for (i, (next, lcn)) in pairs.iter().enumerate() {
            bytes[16 + i * 16..24 + i * 16].copy_from_slice(&next.to_le_bytes());
            bytes[24 + i * 16..32 + i * 16].copy_from_slice(&lcn.to_le_bytes());
        }
        bytes
    }
    #[test]
    fn fragments_and_continuation_keep_absolute_vcn_and_lcn() {
        assert_eq!(
            decode_retrieval(&data(0, &[(3, 100), (8, 20)]), 0).unwrap(),
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(100),
                    clusters: 3
                },
                Run {
                    vcn: 3,
                    lcn: Some(20),
                    clusters: 5
                }
            ]
        );
        assert_eq!(
            decode_retrieval(&data(8, &[(10, 200)]), 8).unwrap()[0],
            Run {
                vcn: 8,
                lcn: Some(200),
                clusters: 2
            }
        );
        assert_eq!(
            decode_retrieval(&data(0, &[(3, -1)]), 0).unwrap()[0].lcn,
            None
        );
    }
    #[test]
    fn malformed_truncated_overlapping_and_negative_extents_reject() {
        let valid = data(0, &[(3, 100)]);
        for len in 0..valid.len() {
            assert!(decode_retrieval(&valid[..len], 0).is_err());
        }
        for invalid_data in [
            data(2, &[(3, 100)]),
            data(0, &[(0, 100)]),
            data(0, &[(3, -2)]),
            data(0, &[(3, 100), (2, 200)]),
            data(0, &[(u64::MAX, 100)]),
            data(0, &[]),
        ] {
            assert!(decode_retrieval(&invalid_data, 0).is_err());
        }
    }
}

#[cfg(test)]
mod hardlink_tests {
    use super::*;
    fn record(id: u64, parent: u64, name: &str, dir: bool) -> Record {
        Record {
            id,
            base: 0,
            parent,
            name: Some(name.into()),
            aliases: Vec::new(),
            size: if dir { None } else { Some((11, 4096)) },
            is_dir: dir,
            reparse: false,
            modified: None,
        }
    }
    #[test]
    fn hardlinks_are_directory_entries_within_and_across_subtrees() {
        let make = || {
            let mut file = record(20, 16, "first.bin", false);
            file.aliases = vec![(16, "second.bin".into()), (17, "third.bin".into())];
            vec![
                record(5, 5, "root", true),
                record(16, 5, "chosen", true),
                record(17, 5, "outside", true),
                file,
            ]
        };
        let snapshot = assemble_subtree(
            Path::new("chosen"),
            make(),
            false,
            Instant::now(),
            &Progress::default(),
            Some(16),
        )
        .unwrap();
        assert_eq!(snapshot.files, 2);
        assert_eq!(snapshot.nodes[0].size, 22);
        assert_eq!(snapshot.nodes[0].allocated, 8192);
        let names: HashSet<_> = snapshot
            .nodes
            .iter()
            .skip(1)
            .map(|n| n.name.clone())
            .collect();
        assert!(names.contains(&OsString::from("first.bin")));
        assert!(names.contains(&OsString::from("second.bin")));
        let outside = assemble_subtree(
            Path::new("outside"),
            make(),
            false,
            Instant::now(),
            &Progress::default(),
            Some(17),
        )
        .unwrap();
        assert_eq!(outside.files, 1);
        assert_eq!(outside.nodes[1].name, "third.bin");
    }
}
