//! Checked decoder for the requested Darwin getattrlistbulk attribute subset.
//! Kept independent of libc so malformed native buffers are tested on every OS.
use std::{
    io,
    time::{Duration, SystemTime},
};
pub(super) const NAME: u32 = 0x0000_0001;
pub(super) const OBJTYPE: u32 = 0x0000_0008;
pub(super) const MODTIME: u32 = 0x0000_0400;
pub(super) const ERROR: u32 = 0x2000_0000;
pub(super) const RETURNED: u32 = 0x8000_0000;
pub(super) const ALLOCSIZE: u32 = 0x0000_0004;
pub(super) const DATALENGTH: u32 = 0x0000_0200;
pub(super) const COMMON: u32 = NAME | OBJTYPE | MODTIME | ERROR | RETURNED;
pub(super) const FILE: u32 = ALLOCSIZE | DATALENGTH;
#[derive(Debug)]
pub(super) struct BulkEntry {
    pub name: Vec<u8>,
    pub object_type: u32,
    pub size: Option<u64>,
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid Darwin bulk attribute buffer",
    )
}
fn bytes<const N: usize>(record: &[u8], offset: usize) -> io::Result<[u8; N]> {
    record
        .get(offset..offset.checked_add(N).ok_or_else(invalid)?)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(invalid)
}
fn u32_at(record: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_ne_bytes(bytes(record, offset)?))
}
fn nonnegative(record: &[u8], offset: usize) -> io::Result<u64> {
    u64::try_from(i64::from_ne_bytes(bytes(record, offset)?)).map_err(|_| invalid())
}
fn decode(record: &[u8]) -> io::Result<BulkEntry> {
    let common = u32_at(record, 4)?;
    let file = u32_at(record, 16)?;
    if common & !COMMON != 0
        || file & !FILE != 0
        || u32_at(record, 8)? != 0
        || u32_at(record, 12)? != 0
        || u32_at(record, 20)? != 0
    {
        return Err(invalid());
    }
    let mut cursor = 24;
    // RETURNED_ATTRS and ERROR are special: they precede the regular bitmap order.
    if common & ERROR != 0 {
        let error = u32_at(record, cursor)?;
        cursor += 4;
        if error != 0 {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(error).map_err(|_| invalid())?,
            ));
        }
    }
    if common & (NAME | OBJTYPE) != NAME | OBJTYPE {
        return Err(invalid());
    }
    let name_ref = cursor;
    let offset = i32::from_ne_bytes(bytes(record, cursor)?);
    cursor += 4;
    let length = u32_at(record, cursor)? as usize;
    cursor += 4;
    let object_type = u32_at(record, cursor)?;
    cursor += 4;
    let modified = if common & MODTIME != 0 {
        let seconds = i64::from_ne_bytes(bytes(record, cursor)?);
        let nanos = i64::from_ne_bytes(bytes(record, cursor + 8)?);
        cursor += 16;
        if !(0..1_000_000_000).contains(&nanos) {
            return Err(invalid());
        }
        let base = if seconds >= 0 {
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
        } else {
            SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
        };
        base.and_then(|base| base.checked_add(Duration::from_nanos(nanos as u64)))
    } else {
        None
    };
    // File attributes are packed after all common attributes, in bitmap order.
    let allocated = if file & ALLOCSIZE != 0 {
        let value = nonnegative(record, cursor)?;
        cursor += 8;
        Some(value)
    } else {
        None
    };
    let size = if file & DATALENGTH != 0 {
        let value = nonnegative(record, cursor)?;
        cursor += 8;
        Some(value)
    } else {
        None
    };
    let start = name_ref
        .checked_add_signed(offset as isize)
        .ok_or_else(invalid)?;
    let end = start.checked_add(length).ok_or_else(invalid)?;
    if start < cursor || length < 2 {
        return Err(invalid());
    }
    let name = record.get(start..end).ok_or_else(invalid)?;
    if name.last() != Some(&0) || name[..name.len() - 1].iter().any(|b| *b == 0 || *b == b'/') {
        return Err(invalid());
    }
    let name = &name[..name.len() - 1];
    if name == b"." || name == b".." {
        return Err(invalid());
    }
    Ok(BulkEntry {
        name: name.to_vec(),
        object_type,
        size,
        allocated,
        modified,
    })
}
pub(super) fn parse_batch(
    buffer: &[u8],
    count: usize,
    mut emit: impl FnMut(io::Result<BulkEntry>),
) -> io::Result<()> {
    if count > buffer.len() / 24 {
        return Err(invalid());
    }
    let mut offset = 0usize;
    for _ in 0..count {
        let length = u32_at(buffer, offset)? as usize;
        if length < 24 || !length.is_multiple_of(8) {
            return Err(invalid());
        }
        let end = offset.checked_add(length).ok_or_else(invalid)?;
        let record = buffer.get(offset..end).ok_or_else(invalid)?;
        emit(decode(record));
        offset = end;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(name: &[u8], kind: u32, file: bool) -> Vec<u8> {
        let mut data = vec![0u8; if file { 72 } else { 56 }];
        data[4..8].copy_from_slice(&COMMON.to_ne_bytes());
        if file {
            data[16..20].copy_from_slice(&FILE.to_ne_bytes());
        }
        let name_offset = data.len();
        data[28..32].copy_from_slice(&((name_offset - 28) as i32).to_ne_bytes());
        data[32..36].copy_from_slice(&((name.len() + 1) as u32).to_ne_bytes());
        data[36..40].copy_from_slice(&kind.to_ne_bytes());
        data[40..48].copy_from_slice(&(-1i64).to_ne_bytes());
        data[48..56].copy_from_slice(&500_000_000i64.to_ne_bytes());
        if file {
            data[56..64].copy_from_slice(&4096i64.to_ne_bytes());
            data[64..72].copy_from_slice(&123i64.to_ne_bytes());
        }
        data.extend(name);
        data.push(0);
        while !data.len().is_multiple_of(8) {
            data.push(0);
        }
        let length = data.len() as u32;
        data[..4].copy_from_slice(&length.to_ne_bytes());
        data
    }
    #[test]
    fn files_directories_unicode_times_and_batches() {
        let mut batch = record("中文.txt".as_bytes(), 1, true);
        batch.extend(record(b"folder", 2, false));
        let mut entries = vec![];
        parse_batch(&batch, 2, |e| entries.push(e.unwrap())).unwrap();
        assert_eq!(entries[0].name, "中文.txt".as_bytes());
        assert_eq!(
            (
                entries[0].object_type,
                entries[0].size,
                entries[0].allocated
            ),
            (1, Some(123), Some(4096))
        );
        assert_eq!(
            entries[0].modified,
            SystemTime::UNIX_EPOCH.checked_sub(Duration::from_millis(500))
        );
        assert_eq!(
            (
                entries[1].object_type,
                entries[1].size,
                entries[1].allocated
            ),
            (2, None, None)
        );
    }
    #[test]
    fn truncation_bad_references_negative_sizes_and_entry_errors() {
        let good = record(b"file", 1, true);
        for end in 0..good.len() {
            assert!(parse_batch(&good[..end], 1, |_| {}).is_err());
        }
        for (offset, value) in [(28, -100i32), (28, i32::MAX), (32, i32::MAX)] {
            let mut bad = good.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
            parse_batch(&bad, 1, |e| assert!(e.is_err())).unwrap();
        }
        let mut bad = good.clone();
        bad[56..64].copy_from_slice(&(-1i64).to_ne_bytes());
        parse_batch(&bad, 1, |e| assert!(e.is_err())).unwrap();
        let mut error = good;
        error[24..28].copy_from_slice(&13u32.to_ne_bytes());
        parse_batch(&error, 1, |e| {
            assert_eq!(e.unwrap_err().raw_os_error(), Some(13))
        })
        .unwrap();
    }
    #[test]
    fn invalid_components_and_impossible_counts_reject() {
        for name in [b"..".as_slice(), b"a/b", b"a\0b"] {
            parse_batch(&record(name, 1, true), 1, |e| assert!(e.is_err())).unwrap();
        }
        assert!(parse_batch(&[], usize::MAX, |_| {}).is_err());
    }
}
