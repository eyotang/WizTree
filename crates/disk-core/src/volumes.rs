//! Live volume enumeration. This queries mount metadata and capacity only.
use std::{io, path::PathBuf};
#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub root: PathBuf,
    pub label: String,
    pub total: u64,
    /// Bytes available to the current user, accounting for filesystem quotas.
    pub free: u64,
    pub filesystem: Option<String>,
}
/// Lists mounted, ready filesystems with nonzero capacity. Inaccessible or
/// unready individual drives are skipped; enumeration failure is returned.
pub fn list_volumes() -> io::Result<Vec<VolumeInfo>> {
    platform::list_volumes()
}

#[cfg(windows)]
mod platform {
    use super::*;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDiskFreeSpaceExW(
            root: *const u16,
            available: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
        fn GetVolumeInformationW(
            root: *const u16,
            label: *mut u16,
            label_length: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            filesystem: *mut u16,
            filesystem_length: u32,
        ) -> i32;
    }
    fn decode(data: &[u16]) -> String {
        String::from_utf16_lossy(&data[..data.iter().position(|v| *v == 0).unwrap_or(data.len())])
    }
    pub(super) fn list_volumes() -> io::Result<Vec<VolumeInfo>> {
        // SAFETY: GetLogicalDrives has no pointer arguments or mutation side effects.
        let drives = unsafe { GetLogicalDrives() };
        if drives == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut volumes = Vec::new();
        for index in 0..26 {
            if drives & (1u32 << index) == 0 {
                continue;
            }
            let root = format!("{}:\\", char::from(b'A' + index as u8));
            let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            let (mut available, mut total, mut total_free) = (0u64, 0u64, 0u64);
            // SAFETY: root is NUL terminated; all out-pointers are valid and the
            // synchronous call reads capacity metadata only.
            if unsafe {
                GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free)
            } == 0
                || total == 0
            {
                continue;
            }
            let (mut label, mut filesystem) = ([0u16; 261], [0u16; 261]);
            let (mut serial, mut maximum, mut flags) = (0u32, 0u32, 0u32);
            // SAFETY: both buffers match the provided character counts and the
            // three scalar output pointers remain live through this call.
            let named = unsafe {
                GetVolumeInformationW(
                    wide.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    &mut serial,
                    &mut maximum,
                    &mut flags,
                    filesystem.as_mut_ptr(),
                    filesystem.len() as u32,
                )
            } != 0;
            let label = if named { decode(&label) } else { String::new() };
            let filesystem = if named {
                Some(decode(&filesystem))
            } else {
                None
            };
            volumes.push(VolumeInfo {
                root: root.into(),
                label,
                total,
                free: available,
                filesystem,
            });
        }
        Ok(volumes)
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{collections::HashSet, ffi::CString, os::unix::ffi::OsStrExt};
    // libc counter widths vary between 32/64-bit Unix targets.
    #[allow(clippy::unnecessary_cast)]
    pub(super) fn list_volumes() -> io::Result<Vec<VolumeInfo>> {
        let mounts = mounts()?;
        let mut seen = HashSet::new();
        let mut volumes = Vec::new();
        for (root, filesystem) in mounts {
            if !seen.insert(root.clone()) || !root.is_dir() {
                continue;
            }
            let Ok(name) = CString::new(root.as_os_str().as_bytes()) else {
                continue;
            };
            let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
            // SAFETY: platform libc provides the correct statvfs layout; name
            // is NUL terminated, output is initialized only on success.
            if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
                continue;
            }
            // SAFETY: the successful call initialized the complete structure.
            let stats = unsafe { stats.assume_init() };
            let block = if stats.f_frsize == 0 {
                stats.f_bsize as u64
            } else {
                stats.f_frsize as u64
            };
            let total = (stats.f_blocks as u64).saturating_mul(block);
            if total == 0 {
                continue;
            }
            let free = (stats.f_bavail as u64).saturating_mul(block);
            let label = root
                .file_name()
                .map(|v| v.to_string_lossy().into_owned())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| root.display().to_string());
            volumes.push(VolumeInfo {
                root,
                label,
                total,
                free,
                filesystem: Some(filesystem),
            });
        }
        volumes.sort_unstable_by(|a, b| a.root.cmp(&b.root));
        Ok(volumes)
    }
    #[cfg(target_os = "linux")]
    fn mounts() -> io::Result<Vec<(PathBuf, String)>> {
        Ok(super::parse_linux_mounts(&std::fs::read_to_string(
            "/proc/self/mountinfo",
        )?))
    }
    #[cfg(target_os = "macos")]
    fn mounts() -> io::Result<Vec<(PathBuf, String)>> {
        use std::{ffi::OsString, mem::MaybeUninit, os::unix::ffi::OsStringExt};
        // getfsstat writes into caller-owned memory rather than getmntinfo's
        // process-global static array, so concurrent callers cannot invalidate it.
        // SAFETY: null/zero requests only the mount count.
        let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let capacity = count as usize + 16;
        let mut buffer: Vec<MaybeUninit<libc::statfs>> = Vec::with_capacity(capacity);
        let byte_count = capacity
            .checked_mul(std::mem::size_of::<libc::statfs>())
            .and_then(|n| i32::try_from(n).ok())
            .ok_or_else(|| io::Error::other("Mount table too large"))?;
        // SAFETY: buffer has capacity for exactly byte_count bytes; getfsstat
        // initializes at most that many complete entries.
        let count =
            unsafe { libc::getfsstat(buffer.as_mut_ptr().cast(), byte_count, libc::MNT_NOWAIT) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = (count as usize).min(capacity);
        let mut result = Vec::with_capacity(count);
        for index in 0..count {
            // SAFETY: indices below returned count were initialized by getfsstat.
            let mount = unsafe { buffer.as_ptr().add(index).read().assume_init() };
            let path: Vec<u8> = mount
                .f_mntonname
                .iter()
                .take_while(|v| **v != 0)
                .map(|v| *v as u8)
                .collect();
            let filesystem = mount
                .f_fstypename
                .iter()
                .take_while(|v| **v != 0)
                .map(|v| *v as u8)
                .collect::<Vec<_>>();
            result.push((
                PathBuf::from(OsString::from_vec(path)),
                String::from_utf8_lossy(&filesystem).into_owned(),
            ));
        }
        Ok(result)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn mounts() -> io::Result<Vec<(PathBuf, String)>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Mount enumeration is implemented for Linux and macOS",
        ))
    }
}
#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;
    pub(super) fn list_volumes() -> io::Result<Vec<VolumeInfo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Volume enumeration unavailable",
        ))
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_mounts(source: &str) -> Vec<(PathBuf, String)> {
    fn unescape(value: &str) -> Vec<u8> {
        let input = value.as_bytes();
        let mut output = Vec::with_capacity(input.len());
        let mut index = 0;
        while index < input.len() {
            if input[index] == b'\\'
                && index + 3 < input.len()
                && input[index + 1] <= b'3'
                && input[index + 1..index + 4]
                    .iter()
                    .all(|c| (b'0'..=b'7').contains(c))
            {
                output.push(
                    (input[index + 1] - b'0') * 64
                        + (input[index + 2] - b'0') * 8
                        + input[index + 3]
                        - b'0',
                );
                index += 4;
            } else {
                output.push(input[index]);
                index += 1;
            }
        }
        output
    }
    let mut result = Vec::new();
    for line in source.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let Some(mount) = left.split_whitespace().nth(4) else {
            continue;
        };
        let Some(filesystem) = right.split_whitespace().next() else {
            continue;
        };
        let mount = unescape(mount);
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(mount))
        };
        #[cfg(not(unix))]
        let path = PathBuf::from(String::from_utf8_lossy(&mount).into_owned());
        result.push((path, filesystem.to_owned()));
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mountinfo_parser_handles_escaped_mounts_and_ignores_malformed_lines() {
        let mounts = parse_linux_mounts(
            "30 1 8:1 / / rw - ext4 /dev/sda1 rw\n31 1 8:2 / /media/My\\040Disk rw - ntfs3 /dev/sdb1 rw\nmalformed\n",
        );
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0], (PathBuf::from("/"), "ext4".into()));
        assert_eq!(mounts[1], (PathBuf::from("/media/My Disk"), "ntfs3".into()));
    }
    #[test]
    fn real_volume_enumeration_returns_sensible_capacity() {
        let volumes = list_volumes().unwrap();
        assert!(!volumes.is_empty());
        for volume in volumes {
            assert!(volume.root.is_absolute());
            assert!(volume.total > 0);
            assert!(volume.free <= volume.total);
            assert!(volume.root.exists());
        }
    }
}
