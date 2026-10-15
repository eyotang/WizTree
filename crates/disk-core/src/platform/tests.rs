use super::*;
use std::{
    collections::BTreeMap,
    ffi::CString,
    os::unix::{ffi::OsStrExt, fs::symlink},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "native-scan-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
type MetadataRows = BTreeMap<OsString, (bool, u64, u64, Option<SystemTime>)>;
fn entries(path: &Path) -> MetadataRows {
    let mut entries = BTreeMap::new();
    Reader::new()
        .enumerate(
            &Job {
                path: path.into(),
                parent: 0,
            },
            &AtomicBool::new(false),
            &Progress::default(),
            |entry| {
                let entry = entry.unwrap();
                assert!(
                    entries
                        .insert(
                            entry.name,
                            (entry.is_dir, entry.size, entry.allocated, entry.modified)
                        )
                        .is_none()
                );
            },
        )
        .unwrap();
    entries
}
#[test]
fn native_matches_metadata_sparse_hardlinks_and_skips_links_special_files() {
    use std::{fs::File, os::unix::fs::MetadataExt};
    let root = Fixture::new();
    fs::create_dir(root.0.join("nested")).unwrap();
    fs::write(root.0.join("中文 file"), b"abc").unwrap();
    fs::write(root.0.join("empty"), []).unwrap();
    let sparse = File::create(root.0.join("sparse")).unwrap();
    sparse.set_len(8 * 1024 * 1024).unwrap();
    drop(sparse);
    fs::hard_link(root.0.join("中文 file"), root.0.join("hardlink")).unwrap();
    symlink(&root.0, root.0.join("loop")).unwrap();
    let fifo = CString::new(root.0.join("pipe").as_os_str().as_bytes()).unwrap();
    // SAFETY: terminated path, creates only a FIFO in this owned fixture.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(root.0.join(OsString::from_vec(vec![b'a', 0xff])), b"x").unwrap();
    }
    let actual = entries(&root.0);
    assert!(!actual.contains_key(std::ffi::OsStr::new("loop")));
    assert!(!actual.contains_key(std::ffi::OsStr::new("pipe")));
    let mut expected = BTreeMap::new();
    for entry in fs::read_dir(&root.0).unwrap() {
        let entry = entry.unwrap();
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        if !meta.is_file() && !meta.is_dir() {
            continue;
        }
        expected.insert(
            entry.file_name(),
            (
                meta.is_dir(),
                if meta.is_dir() { 0 } else { meta.len() },
                if meta.is_dir() {
                    0
                } else {
                    meta.blocks() * 512
                },
                meta.modified().ok(),
            ),
        );
    }
    assert_eq!(actual, expected);
    let mut portable = BTreeMap::new();
    enumerate_portable(&root.0, &AtomicBool::new(false), |entry| {
        let entry = entry.unwrap();
        portable.insert(
            entry.name,
            (entry.is_dir, entry.size, entry.allocated, entry.modified),
        );
    })
    .unwrap();
    assert_eq!(actual, portable);
}
#[test]
fn fd_relative_stat_survives_parent_rename_and_never_follows_symlinks() {
    use std::os::fd::AsRawFd;
    let mut root = Fixture::new();
    fs::write(root.0.join("file"), b"abc").unwrap();
    symlink("file", root.0.join("link")).unwrap();
    let metrics = DirectoryIo::default();
    let file = unix::open_directory(&root.0, &metrics).unwrap();
    let renamed = root.0.with_extension("moved");
    fs::rename(&root.0, &renamed).unwrap();
    let original = std::mem::replace(&mut root.0, renamed);
    let entry = unix::stat_entry(file.as_raw_fd(), &original, b"file", &metrics)
        .unwrap()
        .unwrap();
    assert_eq!(entry.size, 3);
    assert!(
        unix::stat_entry(file.as_raw_fd(), &original, b"link", &metrics)
            .unwrap()
            .is_none()
    );
}
#[test]
fn cancellation_before_io_and_missing_directory_propagate() {
    let root = Fixture::new();
    Reader::new()
        .enumerate(
            &Job {
                path: root.0.clone(),
                parent: 0,
            },
            &AtomicBool::new(true),
            &Progress::default(),
            |_| panic!("cancelled scan emitted an entry"),
        )
        .unwrap();
    assert!(
        Reader::new()
            .enumerate(
                &Job {
                    path: root.0.join("missing"),
                    parent: 0
                },
                &AtomicBool::new(false),
                &Progress::default(),
                |_| {}
            )
            .is_err()
    );
}
