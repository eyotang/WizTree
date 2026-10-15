use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::{null, null_mut};

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ELEVATION: u32 = 20;
const SEE_MASK_NOCLOSEPROCESS: u32 = 0x0040;
const SEE_MASK_NOASYNC: u32 = 0x0100;
const SEE_MASK_FLAG_NO_UI: u32 = 0x0400;
const SW_SHOWNORMAL: i32 = 1;
const COINIT_APARTMENTTHREADED: u32 = 0x2;
const COINIT_DISABLE_OLE1DDE: u32 = 0x4;
const RPC_E_CHANGED_MODE: i32 = 0x80010106u32 as i32;

// SHELLEXECUTEINFOW's anonymous union contains two HANDLE alternatives, so its
// ABI is represented by one pointer-sized slot. All other fields match shellapi.h.
#[repr(C)]
struct ShellExecuteInfo {
    cb_size: u32,
    mask: u32,
    hwnd: *mut c_void,
    verb: *const u16,
    file: *const u16,
    parameters: *const u16,
    directory: *const u16,
    show: i32,
    instance: *mut c_void,
    id_list: *mut c_void,
    class: *const u16,
    class_key: *mut c_void,
    hot_key: u32,
    icon_or_monitor: *mut c_void,
    process: *mut c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetVolumePathNameW(file: *const u16, volume: *mut u16, length: u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetVolumeInformationW(
        root: *const u16,
        volume_name: *mut u16,
        volume_name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        filesystem_name: *mut u16,
        filesystem_name_size: u32,
    ) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        information: *mut c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn ShellExecuteExW(info: *mut ShellExecuteInfo) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, flags: u32) -> i32;
    fn CoUninitialize();
}

/// Inspect the current process token without requesting additional privileges.
pub fn is_elevated() -> io::Result<bool> {
    let mut token = null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle; token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned_handle(token)?;
    let mut elevation = 0u32;
    let mut written = 0u32;
    // TOKEN_ELEVATION is one DWORD; both output buffers have the declared size.
    let success = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TOKEN_ELEVATION,
            (&mut elevation as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
            &mut written,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    if written != std::mem::size_of::<u32>() as u32 {
        return Err(io::Error::other(
            "Windows returned an invalid elevation token",
        ));
    }
    Ok(elevation != 0)
}

/// Check the resolved directory's actual volume before offering an MFT scan.
/// Junctions and symbolic links are resolved only for this check; the caller's
/// selected path is never modified. Network volumes and non-directories return
/// false, while filesystem/API failures remain visible to the caller.
pub fn is_local_ntfs_directory(path: &Path) -> io::Result<bool> {
    // Reject NUL explicitly before any filesystem or Win32 call.
    wide_z(path.as_os_str())?;
    if !std::fs::metadata(path)?.is_dir() {
        return Ok(false);
    }
    let resolved = std::fs::canonicalize(path)?;
    let resolved = wide_z(resolved.as_os_str())?;
    let mut root = vec![0u16; 32_768];
    // SAFETY: resolved is terminated; root has its declared writable length.
    if unsafe { GetVolumePathNameW(resolved.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Fixed, removable and RAM disks are local. Exclude mapped network drives,
    // unavailable roots, unknown media and optical volumes before requesting UAC.
    if !matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3 | 6) {
        return Ok(false);
    }
    let mut filesystem = [0u16; 64];
    // SAFETY: root is terminated by GetVolumePathNameW. Optional outputs are
    // null, and filesystem has the declared writable number of UTF-16 units.
    if unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let len = filesystem
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(filesystem.len());
    Ok(filesystem[..len].iter().copied().eq("NTFS".encode_utf16()))
}

/// Open a separate elevated copy of this executable and scan `path` using MFT.
///
/// Call this on a background thread: ShellExecuteExW waits for the UAC decision.
/// A declined UAC prompt is returned unchanged as Win32 ERROR_CANCELLED (1223).
/// Success means Windows launched the process, not that its scan has completed.
/// The original window and the new process remain alive independently.
pub fn launch_mft_scan(path: &Path, workers: usize) -> io::Result<()> {
    // Validate before initializing COM or showing a UAC prompt.
    let parameters = scan_parameters(path, workers)?;
    let executable = wide_z(std::env::current_exe()?.as_os_str())?;
    let verb = wide_z(OsStr::new("runas"))?;
    let _com = ComApartment::initialize()?;
    // SAFETY: all fields may be initialized to null/zero; mandatory fields follow.
    let mut info: ShellExecuteInfo = unsafe { std::mem::zeroed() };
    info.cb_size = std::mem::size_of::<ShellExecuteInfo>() as u32;
    // NOASYNC is required because this worker has no message loop. Requesting a
    // process handle lets us distinguish a new process from an existing handler.
    // NO_UI suppresses duplicate shell errors, while UAC consent still appears.
    info.mask = SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS | SEE_MASK_FLAG_NO_UI;
    info.verb = verb.as_ptr();
    info.file = executable.as_ptr();
    info.parameters = parameters.as_ptr();
    info.directory = null();
    info.show = SW_SHOWNORMAL;
    // SAFETY: all pointers refer to owned, NUL-terminated UTF-16 buffers that stay
    // alive through this synchronous call; no command interpreter is involved.
    let success = unsafe { ShellExecuteExW(&mut info) } != 0;
    let error = (!success).then(io::Error::last_os_error);
    finish_launch(success, error, info.process)?;
    Ok(())
}

fn finish_launch(success: bool, error: Option<io::Error>, process: *mut c_void) -> io::Result<()> {
    // Also release a handle if a shell implementation returns one on failure.
    let handle = if process.is_null() || process as isize == -1 {
        None
    } else {
        Some(owned_handle(process)?)
    };
    if !success {
        return Err(error.unwrap_or_else(|| io::Error::other("Windows could not launch WizTree")));
    }
    if handle.is_none() {
        return Err(io::Error::other(
            "Windows did not return a launched process handle",
        ));
    }
    // OwnedHandle closes only this process's reference; it never terminates the
    // child, and we deliberately do not wait for its UI or its scan to finish.
    Ok(())
}

fn owned_handle(handle: *mut c_void) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle as isize == -1 {
        return Err(io::Error::other("Windows returned an invalid handle"));
    }
    // SAFETY: callers pass a newly returned owned Win32 handle exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

struct ComApartment {
    initialized_here: bool,
}

impl ComApartment {
    fn initialize() -> io::Result<Self> {
        // SAFETY: reserved must be null; this call affects only the caller thread.
        let result = unsafe {
            CoInitializeEx(
                null_mut(),
                COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE,
            )
        };
        if result >= 0 {
            Ok(Self {
                initialized_here: true,
            })
        } else if result == RPC_E_CHANGED_MODE {
            // A caller may already own an MTA apartment. COM is initialized and
            // ShellExecuteExW supports that model; do not uninitialize its state.
            Ok(Self {
                initialized_here: false,
            })
        } else {
            Err(io::Error::other(format!(
                "Could not initialize Windows COM: 0x{:08X}",
                result as u32
            )))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized_here {
            // SAFETY: balances this thread's successful CoInitializeEx call,
            // including S_FALSE when COM was already initialized in the STA.
            unsafe { CoUninitialize() };
        }
    }
}

fn wide_z(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows paths cannot contain NUL",
        ));
    }
    value.push(0);
    Ok(value)
}

fn scan_parameters(path: &Path, workers: usize) -> io::Result<Vec<u16>> {
    if !(1..=64).contains(&workers) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Scan workers must be between 1 and 64",
        ));
    }
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A scan path is required",
        ));
    }
    let path = wide_z(path.as_os_str())?;
    let mut parameters: Vec<u16> = format!("--method=mft --workers={workers} ")
        .encode_utf16()
        .collect();
    quote_argument(&mut parameters, &path[..path.len() - 1]);
    parameters.push(0);
    Ok(parameters)
}

// Follow the Windows argv rules: double a run of backslashes before a quote or
// the closing quote, and escape a literal quote. Preserve every UTF-16 code unit,
// including non-BMP characters and unpaired surrogates in native Windows paths.
fn quote_argument(output: &mut Vec<u16>, argument: &[u16]) {
    const SLASH: u16 = b'\\' as u16;
    const QUOTE: u16 = b'"' as u16;
    output.push(QUOTE);
    let mut slashes = 0;
    for &unit in argument {
        if unit == SLASH {
            slashes += 1;
            continue;
        }
        if unit == QUOTE {
            output.extend(std::iter::repeat_n(SLASH, slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(SLASH, slashes));
        }
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(SLASH, slashes * 2));
    output.push(QUOTE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn CommandLineToArgvW(command: *const u16, count: *mut i32) -> *mut *mut u16;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    fn parse_parameters(parameters: &[u16]) -> Vec<Vec<u16>> {
        // Prefix a synthetic argv[0], whose parsing rules differ from arguments.
        let mut command: Vec<u16> = "wiztree.exe ".encode_utf16().collect();
        command.extend_from_slice(parameters);
        let mut count = 0;
        let argv = unsafe { CommandLineToArgvW(command.as_ptr(), &mut count) };
        assert!(!argv.is_null());
        let mut parsed = Vec::new();
        for index in 1..count as usize {
            let value = unsafe { *argv.add(index) };
            let mut len = 0;
            while unsafe { *value.add(len) } != 0 {
                len += 1;
            }
            parsed.push(unsafe { std::slice::from_raw_parts(value, len) }.to_vec());
        }
        unsafe { LocalFree(argv.cast()) };
        parsed
    }

    #[test]
    fn native_windows_parser_round_trips_scan_paths() {
        for path in [
            r"D:\source",
            r"D:\space and 中文\📁\",
            r"D:\",
            r"\\server\share\space folder\",
            r"\\?\D:\long folder\",
            "D:\\literal\"quote\\\\\" x & whoami ; %PATH% $()",
        ] {
            let parameters = scan_parameters(Path::new(path), 8).unwrap();
            assert_eq!(parameters.last(), Some(&0));
            let parsed = parse_parameters(&parameters);
            assert_eq!(parsed.len(), 3);
            assert_eq!(parsed[0], "--method=mft".encode_utf16().collect::<Vec<_>>());
            assert_eq!(parsed[1], "--workers=8".encode_utf16().collect::<Vec<_>>());
            assert_eq!(
                parsed[2],
                OsStr::new(path).encode_wide().collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn quote_and_backslash_combinations_round_trip() {
        // Exhaust the short sequences most likely to break Windows quoting,
        // using the real OS parser instead of a second copy of our algorithm.
        let alphabet = [b'\\' as u16, b'"' as u16, b' ' as u16, b'a' as u16];
        for len in 0..=6u32 {
            for mut bits in 0..4usize.pow(len) {
                let mut argument = Vec::new();
                for _ in 0..len {
                    argument.push(alphabet[bits % 4]);
                    bits /= 4;
                }
                let mut quoted = Vec::new();
                quote_argument(&mut quoted, &argument);
                quoted.push(0);
                assert_eq!(parse_parameters(&quoted), vec![argument]);
            }
        }
    }

    #[test]
    fn native_utf16_path_is_not_converted_lossily() {
        let native = vec![
            b'D' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
            b' ' as u16,
            b'\\' as u16,
        ];
        let path = OsString::from_wide(&native);
        let parsed = parse_parameters(&scan_parameters(Path::new(&path), 64).unwrap());
        assert_eq!(parsed[2], native);
    }

    #[test]
    fn invalid_inputs_fail_without_requesting_elevation() {
        for workers in [0, 65, usize::MAX] {
            assert_eq!(
                scan_parameters(Path::new(r"D:\source"), workers)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        for path in [
            OsString::new(),
            OsString::from_wide(&[b'D' as u16, 0, b'x' as u16]),
        ] {
            assert_eq!(
                scan_parameters(Path::new(&path), 1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn uac_cancellation_code_is_preserved() {
        let error =
            finish_launch(false, Some(io::Error::from_raw_os_error(1223)), null_mut()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(1223));
    }

    #[test]
    fn missing_process_is_not_reported_as_success() {
        assert!(finish_launch(true, None, null_mut()).is_err());
        assert!(owned_handle(null_mut()).is_err());
        assert!(owned_handle(-1isize as *mut c_void).is_err());
    }

    #[test]
    fn ffi_structure_matches_windows_abi() {
        assert_eq!(
            std::mem::size_of::<ShellExecuteInfo>(),
            if cfg!(target_pointer_width = "64") {
                112
            } else {
                60
            }
        );
    }

    #[test]
    fn a_regular_file_is_not_an_mft_directory() {
        assert!(!is_local_ntfs_directory(&std::env::current_exe().unwrap()).unwrap());
    }

    #[test]
    fn invalid_directory_inputs_preserve_errors() {
        let nul = OsString::from_wide(&[b'D' as u16, 0, b'x' as u16]);
        assert_eq!(
            is_local_ntfs_directory(Path::new(&nul)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let missing =
            std::env::temp_dir().join(format!("wiztree-missing-elevation-{}", std::process::id()));
        assert!(is_local_ntfs_directory(&missing).is_err());
    }

    #[test]
    fn current_process_elevation_can_be_queried_without_uac() {
        is_elevated().expect("query current process token");
    }
}
