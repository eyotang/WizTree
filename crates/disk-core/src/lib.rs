//! Read-only, bounded-concurrency filesystem scanner.
//!
//! Nodes are held in an arena; child indices never contain references into that
//! arena. Directory traversal never follows symbolic links or Windows reparse
//! points. The scanner does not modify the scanned filesystem.
pub mod analysis;
mod platform;
pub use platform::directory_backend;
#[cfg(any(windows, test))]
mod ntfs;
mod volumes;
#[cfg(windows)]
mod windows_directory;
pub use analysis::{AnalysisIndex, ExtensionStat, NodeSummary};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, Metadata},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
pub use volumes::{VolumeInfo, list_volumes};

type ScanPool = Arc<rayon::ThreadPool>;
struct CachedPool {
    workers: usize,
    pool: ScanPool,
}
fn scan_pool(workers: usize) -> io::Result<ScanPool> {
    static CACHE: OnceLock<Mutex<Option<CachedPool>>> = OnceLock::new();
    let workers = workers.clamp(1, 64);
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| io::Error::other("Scan worker pool lock poisoned"))?;
    if let Some(cached) = cache.as_ref().filter(|cached| cached.workers == workers) {
        return Ok(cached.pool.clone());
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("disk-scan-{i}"))
        .build()
        .map_err(io::Error::other)?;
    // Ensure every OS thread has entered its event loop before returning.
    // Desktop calls this before graphics initialization to avoid paying graphics
    // DLL thread attach/detach costs during each individual scan.
    pool.broadcast(|_| ());
    let pool = Arc::new(pool);
    *cache = Some(CachedPool {
        workers,
        pool: pool.clone(),
    });
    Ok(pool)
}
/// Prepare and retain the bounded worker pool before GUI platform initialization.
/// CLI callers can simply scan; it initializes the same pool lazily.
pub fn prepare_scan_threads(options: &ScanOptions) -> io::Result<()> {
    scan_pool(options.workers).map(|_| ())
}

/// Automatic chooses raw MFT for whole volumes and batched directory I/O for
/// subfolders. Explicit MFT also accepts an NTFS subfolder; failure falls back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanStrategy {
    #[default]
    Auto,
    Directory,
    NtfsMft,
}
#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub workers: usize,
}
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            workers: thread::available_parallelism()
                .map_or(4, usize::from)
                .clamp(2, 16),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanMethod {
    ParallelWalk,
    NtfsMft,
}
impl std::fmt::Display for ScanMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ParallelWalk => "Parallel directory walk",
            Self::NtfsMft => "NTFS raw MFT",
        })
    }
}
#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Option<usize>,
    pub name: OsString,
    /// Logical bytes, aggregated over descendants for a directory.
    pub size: u64,
    /// Allocated bytes, aggregated over descendants for a directory.
    pub allocated: u64,
    pub is_dir: bool,
    /// Last data modification time reported by the filesystem.
    pub modified: Option<SystemTime>,
    pub children: Vec<usize>,
}
#[derive(Clone, Debug)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}
#[derive(Debug)]
pub struct Snapshot {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    pub elapsed: Duration,
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub cancelled: bool,
    pub method: ScanMethod,
    /// Why raw NTFS scanning fell back, if it was applicable but unavailable.
    pub fallback_reason: Option<String>,
    /// A bounded sample of errors. `errors` is the complete error count.
    pub issues: Vec<ScanIssue>,
}
impl Snapshot {
    pub fn path(&self, mut id: usize) -> PathBuf {
        let mut parts = Vec::new();
        while let Some(parent) = self.nodes[id].parent {
            parts.push(&self.nodes[id].name);
            id = parent;
        }
        let mut path = self.root.clone();
        for part in parts.into_iter().rev() {
            path.push(part);
        }
        path
    }
    /// Descending size order with deterministic case-insensitive name tie breaks.
    pub fn largest_children(&self, id: usize) -> Vec<usize> {
        let mut children = self.nodes[id].children.clone();
        children.sort_unstable_by(|a, b| {
            self.nodes[*b].size.cmp(&self.nodes[*a].size).then_with(|| {
                self.nodes[*a]
                    .name
                    .to_string_lossy()
                    .to_lowercase()
                    .cmp(&self.nodes[*b].name.to_string_lossy().to_lowercase())
            })
        });
        children
    }
    pub fn write_csv(&self, mut output: impl Write) -> io::Result<()> {
        writeln!(output, "path,type,size,allocated")?;
        for (id, node) in self.nodes.iter().enumerate() {
            let escaped = self.path(id).to_string_lossy().replace('"', "\"\"");
            writeln!(
                output,
                "\"{}\",{},{},{}",
                escaped,
                if node.is_dir { "directory" } else { "file" },
                node.size,
                node.allocated
            )?;
        }
        output.flush()
    }
}
#[derive(Default, Debug)]
pub struct Progress {
    pub directory_io: DirectoryIo,
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    /// Volume records read while preparing an MFT subtree, not target file count.
    pub mft_records: AtomicU64,
    pub mft_bytes: AtomicU64,
    pub mft_extents: AtomicU64,
    pub mft_cached: AtomicBool,
}
/// Summed worker I/O time (may exceed wall time because operations overlap).
#[derive(Default, Debug)]
pub struct DirectoryIo {
    pub worker_start_delay_ns: AtomicU64,
    pub queue_wait_ns: AtomicU64,
    pub entry_allocate_ns: AtomicU64,
    pub enumerate_ns: AtomicU64,
    pub publish_wait_ns: AtomicU64,
    pub publish_ns: AtomicU64,
    pub job_drop_ns: AtomicU64,
    pub aggregate_ns: AtomicU64,
    pub traversal_ns: AtomicU64,
    pub pool_ns: AtomicU64,
    pub setup_ns: AtomicU64,
    pub canonicalize_ns: AtomicU64,
    pub opens: AtomicU64,
    pub open_ns: AtomicU64,
    pub queries: AtomicU64,
    pub query_ns: AtomicU64,
    pub special_files: AtomicU64,
    pub special_ns: AtomicU64,
    pub absolute_opens: AtomicU64,
    pub portable_fallbacks: AtomicU64,
}
#[derive(Clone, Copy, Default, Debug)]
pub struct DirectoryIoSnapshot {
    pub worker_start_delay_ns: u64,
    pub queue_wait_ns: u64,
    pub entry_allocate_ns: u64,
    pub enumerate_ns: u64,
    pub publish_wait_ns: u64,
    pub publish_ns: u64,
    pub job_drop_ns: u64,
    pub aggregate_ns: u64,
    pub traversal_ns: u64,
    pub pool_ns: u64,
    pub setup_ns: u64,
    pub canonicalize_ns: u64,
    pub opens: u64,
    pub open_ns: u64,
    pub queries: u64,
    pub query_ns: u64,
    pub special_files: u64,
    pub special_ns: u64,
    pub absolute_opens: u64,
    pub portable_fallbacks: u64,
}
impl DirectoryIo {
    fn snapshot(&self) -> DirectoryIoSnapshot {
        DirectoryIoSnapshot {
            worker_start_delay_ns: self.worker_start_delay_ns.load(Ordering::Relaxed),
            queue_wait_ns: self.queue_wait_ns.load(Ordering::Relaxed),
            entry_allocate_ns: self.entry_allocate_ns.load(Ordering::Relaxed),
            enumerate_ns: self.enumerate_ns.load(Ordering::Relaxed),
            publish_wait_ns: self.publish_wait_ns.load(Ordering::Relaxed),
            publish_ns: self.publish_ns.load(Ordering::Relaxed),
            job_drop_ns: self.job_drop_ns.load(Ordering::Relaxed),
            aggregate_ns: self.aggregate_ns.load(Ordering::Relaxed),
            traversal_ns: self.traversal_ns.load(Ordering::Relaxed),
            pool_ns: self.pool_ns.load(Ordering::Relaxed),
            setup_ns: self.setup_ns.load(Ordering::Relaxed),
            canonicalize_ns: self.canonicalize_ns.load(Ordering::Relaxed),
            opens: self.opens.load(Ordering::Relaxed),
            open_ns: self.open_ns.load(Ordering::Relaxed),
            queries: self.queries.load(Ordering::Relaxed),
            query_ns: self.query_ns.load(Ordering::Relaxed),
            special_files: self.special_files.load(Ordering::Relaxed),
            special_ns: self.special_ns.load(Ordering::Relaxed),
            absolute_opens: self.absolute_opens.load(Ordering::Relaxed),
            portable_fallbacks: self.portable_fallbacks.load(Ordering::Relaxed),
        }
    }
    fn reset(&self) {
        for value in [
            &self.worker_start_delay_ns,
            &self.queue_wait_ns,
            &self.entry_allocate_ns,
            &self.enumerate_ns,
            &self.publish_wait_ns,
            &self.publish_ns,
            &self.job_drop_ns,
            &self.aggregate_ns,
            &self.traversal_ns,
            &self.pool_ns,
            &self.setup_ns,
            &self.canonicalize_ns,
            &self.opens,
            &self.open_ns,
            &self.queries,
            &self.query_ns,
            &self.special_files,
            &self.special_ns,
            &self.absolute_opens,
            &self.portable_fallbacks,
        ] {
            value.store(0, Ordering::Relaxed);
        }
    }
}
#[derive(Clone, Copy, Default, Debug)]
pub struct ProgressSnapshot {
    pub directory_io: DirectoryIoSnapshot,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub mft_records: u64,
    pub mft_bytes: u64,
    pub mft_extents: u64,
    pub mft_cached: bool,
}
impl Progress {
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            directory_io: self.directory_io.snapshot(),
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            mft_records: self.mft_records.load(Ordering::Relaxed),
            mft_bytes: self.mft_bytes.load(Ordering::Relaxed),
            mft_extents: self.mft_extents.load(Ordering::Relaxed),
            mft_cached: self.mft_cached.load(Ordering::Relaxed),
        }
    }
    fn reset(&self) {
        self.directory_io.reset();
        self.files.store(0, Ordering::Relaxed);
        self.dirs.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        self.mft_records.store(0, Ordering::Relaxed);
        self.mft_bytes.store(0, Ordering::Relaxed);
        self.mft_extents.store(0, Ordering::Relaxed);
        self.mft_cached.store(false, Ordering::Relaxed);
    }
}
struct Job {
    #[cfg(windows)]
    directory_parent: Option<std::sync::Arc<fs::File>>,
    path: PathBuf,
    parent: usize,
}
struct State {
    jobs: VecDeque<Job>,
    pending: usize,
    nodes: Vec<Node>,
    issues: Vec<ScanIssue>,
}
struct Entry {
    #[cfg(windows)]
    directory_parent: Option<std::sync::Arc<fs::File>>,
    name: OsString,
    path: PathBuf,
    size: u64,
    allocated: u64,
    is_dir: bool,
    modified: Option<SystemTime>,
}
const MAX_ISSUES: usize = 128;

/// Scans a root using at most 64 workers. A worker pool shares a directory queue;
/// filesystem I/O is performed outside the arena lock. Errors below the root are
/// counted and scanning continues. Cancellation returns a coherent partial tree.
pub fn scan(
    root: &Path,
    options: &ScanOptions,
    cancel: &AtomicBool,
    progress: &Progress,
) -> io::Result<Snapshot> {
    scan_with_strategy(root, options, cancel, progress, ScanStrategy::Auto)
}
/// Select the scan transport without changing size, cancellation or fallback semantics.
pub fn scan_with_strategy(
    root: &Path,
    options: &ScanOptions,
    cancel: &AtomicBool,
    progress: &Progress,
    strategy: ScanStrategy,
) -> io::Result<Snapshot> {
    let start = Instant::now();
    progress.reset();
    let root = fs::canonicalize(root)?;
    let canonicalize_ns = start.elapsed().as_nanos() as u64;
    let setup_started = Instant::now();
    #[cfg(not(windows))]
    let fallback_reason = (strategy == ScanStrategy::NtfsMft)
        .then(|| "NTFS MFT transport requires Windows; using directory scan".to_owned());
    #[cfg(windows)]
    let fallback_reason = if strategy == ScanStrategy::NtfsMft
        || (strategy == ScanStrategy::Auto && ntfs::is_volume_root(&root))
    {
        match ntfs::try_scan(&root, cancel, progress, start) {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => Some(format!(
                "MFT unavailable, using batched directory scan: {error}"
            )),
        }
    } else {
        None
    };
    progress.reset();
    progress
        .directory_io
        .canonicalize_ns
        .store(canonicalize_ns, Ordering::Relaxed);
    let metadata = fs::symlink_metadata(&root)?;
    let root_dir = metadata.is_dir();
    let root_size = if root_dir { 0 } else { metadata.len() };
    let root_allocated = if root_dir {
        0
    } else {
        allocated_size(&root, &metadata)?
    };
    let node = Node {
        parent: None,
        name: root.file_name().unwrap_or(root.as_os_str()).to_os_string(),
        size: root_size,
        allocated: root_allocated,
        is_dir: root_dir,
        modified: metadata.modified().ok(),
        children: Vec::new(),
    };
    let initial_jobs = if root_dir {
        VecDeque::from([Job {
            #[cfg(windows)]
            directory_parent: None,
            path: root.clone(),
            parent: 0,
        }])
    } else {
        VecDeque::new()
    };
    let shared = (
        Mutex::new(State {
            jobs: initial_jobs,
            pending: usize::from(root_dir),
            nodes: vec![node],
            issues: Vec::new(),
        }),
        Condvar::new(),
    );
    if root_dir {
        progress.dirs.store(1, Ordering::Relaxed);
    } else {
        progress.files.store(1, Ordering::Relaxed);
        progress.bytes.store(root_size, Ordering::Relaxed);
    }
    progress
        .directory_io
        .setup_ns
        .store(setup_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if root_dir {
        let pool_started = Instant::now();
        let pool = scan_pool(options.workers)?;
        progress
            .directory_io
            .pool_ns
            .store(pool_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let traversal_started = Instant::now();
        pool.scope(|scope| {
            for _ in 0..options.workers.clamp(1, 64) {
                let shared = &shared;
                scope.spawn(move |_| worker(shared, cancel, progress, traversal_started));
            }
        });
        progress.directory_io.traversal_ns.store(
            traversal_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
    }
    let aggregation_started = Instant::now();
    let mut state = shared.0.into_inner().unwrap();
    // Parents always precede descendants, irrespective of worker completion order.
    // A reverse arena pass therefore aggregates the entire tree in linear time.
    for id in (1..state.nodes.len()).rev() {
        let size = state.nodes[id].size;
        let allocated = state.nodes[id].allocated;
        let parent = state.nodes[id].parent.unwrap();
        state.nodes[parent].size = state.nodes[parent].size.saturating_add(size);
        state.nodes[parent].allocated = state.nodes[parent].allocated.saturating_add(allocated);
    }
    progress.directory_io.aggregate_ns.store(
        aggregation_started.elapsed().as_nanos() as u64,
        Ordering::Relaxed,
    );
    let totals = progress.snapshot();
    Ok(Snapshot {
        root,
        nodes: state.nodes,
        elapsed: start.elapsed(),
        files: totals.files,
        dirs: totals.dirs,
        errors: totals.errors,
        cancelled: cancel.load(Ordering::Relaxed),
        method: ScanMethod::ParallelWalk,
        fallback_reason,
        issues: state.issues,
    })
}
fn worker(
    shared: &(Mutex<State>, Condvar),
    cancel: &AtomicBool,
    progress: &Progress,
    dispatched: Instant,
) {
    let metrics = &progress.directory_io;
    metrics
        .worker_start_delay_ns
        .fetch_add(dispatched.elapsed().as_nanos() as u64, Ordering::Relaxed);
    let mut reader = platform::Reader::new();
    loop {
        let queue_started = Instant::now();
        let job = {
            let mut state = shared.0.lock().unwrap();
            loop {
                if cancel.load(Ordering::Relaxed) || state.pending == 0 {
                    shared.1.notify_all();
                    return;
                }
                if let Some(job) = state.jobs.pop_front() {
                    break job;
                }
                state = shared.1.wait(state).unwrap();
            }
        };
        metrics
            .queue_wait_ns
            .fetch_add(queue_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        let allocation_started = Instant::now();
        let mut entries = Vec::with_capacity(1024);
        metrics.entry_allocate_ns.fetch_add(
            allocation_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        let mut issues = Vec::new();
        let mut error_count = 0u64;
        let enumeration_started = Instant::now();
        let result =
            enumerate_directory(&job, cancel, progress, &mut reader, |entry| match entry {
                Ok(entry) => {
                    entries.push(entry);
                    if entries.len() >= 1024 {
                        commit_entries(shared, job.parent, &mut entries, progress);
                    }
                }
                Err(error) => {
                    error_count += 1;
                    if issues.len() < MAX_ISSUES {
                        issues.push(ScanIssue {
                            path: job.path.clone(),
                            message: error.to_string(),
                        });
                    }
                }
            });
        metrics.enumerate_ns.fetch_add(
            enumeration_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        if let Err(error) = result {
            error_count += 1;
            if issues.len() < MAX_ISSUES {
                issues.push(ScanIssue {
                    path: job.path.clone(),
                    message: error.to_string(),
                });
            }
        }
        commit_entries(shared, job.parent, &mut entries, progress);
        let publish_started = Instant::now();
        let mut state = shared.0.lock().unwrap();
        metrics.publish_wait_ns.fetch_add(
            publish_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        let publish_started = Instant::now();
        progress.errors.fetch_add(error_count, Ordering::Relaxed);
        let remaining = MAX_ISSUES.saturating_sub(state.issues.len());
        state.issues.extend(issues.into_iter().take(remaining));
        state.pending -= 1;
        shared.1.notify_all();
        drop(state);
        metrics.publish_ns.fetch_add(
            publish_started.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );
        let drop_started = Instant::now();
        drop(job);
        metrics
            .job_drop_ns
            .fetch_add(drop_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
fn enumerate_directory(
    job: &Job,
    cancel: &AtomicBool,
    progress: &Progress,
    reader: &mut platform::Reader,
    emit: impl FnMut(io::Result<Entry>),
) -> io::Result<()> {
    reader.enumerate(job, cancel, progress, emit)
}
#[cfg(any(windows, test, not(any(target_os = "linux", target_os = "macos"))))]
fn enumerate_portable(
    path: &Path,
    cancel: &AtomicBool,
    mut emit: impl FnMut(io::Result<Entry>),
) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let result = entry.and_then(|entry| {
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                return Ok(None);
            }
            let metadata = entry.metadata()?;
            if is_reparse_point(&metadata) {
                return Ok(None);
            }
            let is_dir = metadata.is_dir();
            if !is_dir && !metadata.is_file() {
                return Ok(None);
            }
            let size = if is_dir { 0 } else { metadata.len() };
            let full_path = entry.path();
            let allocated = if is_dir {
                0
            } else {
                match allocated_size(&full_path, &metadata) {
                    Ok(value) => value,
                    Err(error) => {
                        emit(Err(io::Error::new(
                            error.kind(),
                            format!(
                                "Allocated size unavailable: {}: {error}",
                                full_path.display()
                            ),
                        )));
                        0
                    }
                }
            };
            Ok(Some(Entry {
                #[cfg(windows)]
                directory_parent: None,
                name: entry.file_name(),
                path: if is_dir { full_path } else { PathBuf::new() },
                size,
                allocated,
                is_dir,
                modified: metadata.modified().ok(),
            }))
        });
        match result {
            Ok(Some(entry)) => emit(Ok(entry)),
            Ok(None) => {}
            Err(error) => emit(Err(error)),
        }
    }
    Ok(())
}
fn commit_entries(
    shared: &(Mutex<State>, Condvar),
    parent: usize,
    entries: &mut Vec<Entry>,
    progress: &Progress,
) {
    if entries.is_empty() {
        return;
    }
    let metrics = &progress.directory_io;
    let publish_started = Instant::now();
    let mut state = shared.0.lock().unwrap();
    metrics.publish_wait_ns.fetch_add(
        publish_started.elapsed().as_nanos() as u64,
        Ordering::Relaxed,
    );
    let publish_started = Instant::now();
    let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
    for entry in entries.drain(..) {
        let id = state.nodes.len();
        state.nodes.push(Node {
            parent: Some(parent),
            name: entry.name,
            size: entry.size,
            allocated: entry.allocated,
            is_dir: entry.is_dir,
            modified: entry.modified,
            children: Vec::new(),
        });
        state.nodes[parent].children.push(id);
        if entry.is_dir {
            dirs += 1;
            state.jobs.push_back(Job {
                #[cfg(windows)]
                directory_parent: entry.directory_parent,
                path: entry.path,
                parent: id,
            });
            state.pending += 1;
        } else {
            files += 1;
            bytes = bytes.saturating_add(entry.size);
        }
    }
    progress.files.fetch_add(files, Ordering::Relaxed);
    progress.dirs.fetch_add(dirs, Ordering::Relaxed);
    progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    shared.1.notify_all();
    drop(state);
    metrics.publish_ns.fetch_add(
        publish_started.elapsed().as_nanos() as u64,
        Ordering::Relaxed,
    );
}
#[cfg(unix)]
fn allocated_size(_: &Path, metadata: &Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.blocks().saturating_mul(512))
}
#[cfg(windows)]
fn allocated_size(path: &Path, metadata: &Metadata) -> io::Result<u64> {
    use std::os::windows::fs::MetadataExt;
    windows_allocated_size(path, metadata.file_attributes())
}
#[cfg(windows)]
fn windows_allocated_size(path: &Path, attributes: u32) -> io::Result<u64> {
    use std::{
        ffi::c_void,
        fs::OpenOptions,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
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
    #[derive(Default)]
    struct StandardInfo {
        allocation: i64,
        end_of_file: i64,
        links: u32,
        delete_pending: u8,
        directory: u8,
    }
    #[repr(C)]
    #[derive(Default)]
    struct CompressionInfo {
        compressed_size: i64,
        format: u16,
        unit_shift: u8,
        chunk_shift: u8,
        cluster_shift: u8,
        reserved: [u8; 3],
    }
    // FILE_READ_ATTRIBUTES only; no file contents are read. OPEN_REPARSE_POINT
    // protects against accidentally opening a replaced symlink target.
    let file = OpenOptions::new()
        .access_mode(0x80)
        .share_mode(7)
        .custom_flags(0x0020_0000)
        .open(path)?;
    let allocation = if attributes & 0xa00 != 0 {
        let mut info = CompressionInfo::default();
        // SAFETY: the output struct is C-compatible and lives through this
        // synchronous call; file owns a valid handle.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                8,
                (&mut info as *mut CompressionInfo).cast(),
                std::mem::size_of::<CompressionInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        info.compressed_size
    } else {
        let mut info = StandardInfo::default();
        // SAFETY: same ownership and layout guarantees as above.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                1,
                (&mut info as *mut StandardInfo).cast(),
                std::mem::size_of::<StandardInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        info.allocation
    };
    u64::try_from(allocation)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Negative allocated size"))
}
#[cfg(not(any(unix, windows)))]
fn allocated_size(_: &Path, metadata: &Metadata) -> io::Result<u64> {
    Ok(metadata.len())
}
#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(all(
    not(windows),
    any(test, not(any(target_os = "linux", target_os = "macos")))
))]
fn is_reparse_point(_: &Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "disk-core-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn nested_tree_totals_paths_and_parallel_consistency() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("a/b")).unwrap();
        fs::create_dir_all(fixture.0.join("empty")).unwrap();
        fs::write(fixture.0.join("one"), [0u8; 7]).unwrap();
        fs::write(fixture.0.join("a/two"), [0u8; 11]).unwrap();
        fs::write(fixture.0.join("a/b/three"), [0u8; 13]).unwrap();
        for workers in [1, 4, 64] {
            let snapshot = scan(
                &fixture.0,
                &ScanOptions { workers },
                &AtomicBool::new(false),
                &Progress::default(),
            )
            .unwrap();
            assert_eq!((snapshot.files, snapshot.dirs, snapshot.errors), (3, 4, 0));
            assert_eq!(snapshot.nodes[0].size, 31);
            assert_eq!(snapshot.nodes.len(), 7);
            for (id, node) in snapshot.nodes.iter().enumerate().skip(1) {
                assert!(node.parent.unwrap() < id);
                assert!(snapshot.path(id).exists());
            }
            let a = snapshot.nodes.iter().position(|n| n.name == "a").unwrap();
            assert_eq!(snapshot.nodes[a].size, 24);
        }
    }
    #[test]
    fn cancelled_scan_returns_partial_valid_tree() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("ignored"), [0u8; 3]).unwrap();
        let result = scan(
            &fixture.0,
            &ScanOptions::default(),
            &AtomicBool::new(true),
            &Progress::default(),
        )
        .unwrap();
        assert!(result.cancelled);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.files, 0);
    }
    #[test]
    fn reused_pool_isolates_simultaneous_scans_cancellation_and_progress() {
        let left = Fixture::new();
        let right = Fixture::new();
        for (root, value) in [(&left.0, 7u8), (&right.0, 13u8)] {
            for i in 0..80 {
                let directory = root.join(format!("d{i}"));
                fs::create_dir(&directory).unwrap();
                fs::write(directory.join("file"), vec![value; usize::from(value)]).unwrap();
            }
        }
        let options = ScanOptions { workers: 4 };
        prepare_scan_threads(&options).unwrap();
        thread::scope(|scope| {
            let a = scope.spawn(|| {
                scan(
                    &left.0,
                    &options,
                    &AtomicBool::new(false),
                    &Progress::default(),
                )
                .unwrap()
            });
            let b = scope.spawn(|| {
                scan(
                    &right.0,
                    &options,
                    &AtomicBool::new(false),
                    &Progress::default(),
                )
                .unwrap()
            });
            let cancelled = scope.spawn(|| {
                scan(
                    &left.0,
                    &options,
                    &AtomicBool::new(true),
                    &Progress::default(),
                )
                .unwrap()
            });
            for (snapshot, bytes) in [(a.join().unwrap(), 80 * 7), (b.join().unwrap(), 80 * 13)] {
                assert_eq!(
                    (snapshot.files, snapshot.dirs, snapshot.errors),
                    (80, 81, 0)
                );
                assert_eq!(snapshot.nodes[0].size, bytes);
                assert!(!snapshot.cancelled);
            }
            assert!(cancelled.join().unwrap().cancelled);
        });
        let progress = Progress::default();
        for _ in 0..2 {
            let snapshot = scan(&left.0, &options, &AtomicBool::new(false), &progress).unwrap();
            assert_eq!(progress.snapshot().files, 80);
            assert_eq!(snapshot.nodes[0].size, 560);
            #[cfg(windows)]
            assert_eq!(progress.snapshot().directory_io.opens, 81);
        }
    }
    #[test]
    fn missing_root_is_an_error() {
        let fixture = Fixture::new();
        assert!(
            scan(
                &fixture.0.join("missing"),
                &ScanOptions::default(),
                &AtomicBool::new(false),
                &Progress::default()
            )
            .is_err()
        );
    }
    #[test]
    fn csv_escapes_paths_and_file_root_is_supported() {
        let fixture = Fixture::new();
        let file = fixture.0.join("comma,name");
        fs::write(&file, [1u8; 5]).unwrap();
        let snapshot = scan(
            &file,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.dirs, 0);
        assert_eq!(snapshot.nodes[0].size, 5);
        let mut csv = Vec::new();
        snapshot.write_csv(&mut csv).unwrap();
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.contains("comma,name\",file,5,"));
    }
    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_never_traversed() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("folder")).unwrap();
        fs::write(fixture.0.join("folder/file"), [0u8; 9]).unwrap();
        std::os::unix::fs::symlink(&fixture.0, fixture.0.join("folder/loop")).unwrap();
        let snapshot = scan(
            &fixture.0,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(snapshot.files, 1);
        assert_eq!(snapshot.dirs, 2);
        assert_eq!(snapshot.nodes[0].size, 9);
    }
    #[test]
    fn large_single_directory_publishes_every_batch() {
        let fixture = Fixture::new();
        for index in 0..2051 {
            fs::write(fixture.0.join(format!("item-{index}")), []).unwrap();
        }
        let progress = Progress::default();
        let snapshot = scan(
            &fixture.0,
            &ScanOptions { workers: 4 },
            &AtomicBool::new(false),
            &progress,
        )
        .unwrap();
        assert_eq!(snapshot.files, 2051);
        assert_eq!(snapshot.nodes.len(), 2052);
        assert_eq!(progress.snapshot().files, 2051);
    }
    #[cfg(windows)]
    #[test]
    fn sparse_file_uses_physical_allocation_instead_of_logical_length() {
        use std::{ffi::c_void, fs::OpenOptions, os::windows::io::AsRawHandle};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn DeviceIoControl(
                handle: *mut c_void,
                code: u32,
                input: *const c_void,
                input_len: u32,
                output: *mut c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut c_void,
            ) -> i32;
        }
        let fixture = Fixture::new();
        let path = fixture.0.join("sparse");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut returned = 0u32;
        // SAFETY: owned test file, no input/output data, synchronous call. Only
        // this newly created temporary file is modified.
        let result = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x900c4,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(
            result,
            0,
            "FSCTL_SET_SPARSE failed: {}",
            io::Error::last_os_error()
        );
        file.set_len(32 * 1024 * 1024).unwrap();
        drop(file);
        let snapshot = scan(
            &path,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(snapshot.nodes[0].size, 32 * 1024 * 1024);
        assert_eq!(snapshot.nodes[0].allocated, 0);
        assert_eq!(snapshot.errors, 0);
    }
}

#[cfg(all(test, windows))]
mod windows_metadata_regression {
    use super::*;
    #[test]
    fn real_multi_batch_directory_matches_per_file_queries_including_modified_times() {
        let root =
            std::env::temp_dir().join(format!("wiztree-batch-parity-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        for i in 0..600 {
            fs::write(
                root.join(format!("{i:04}-{}.txt", "长".repeat(50))),
                vec![42u8; i % 900 + 1],
            )
            .unwrap();
        }
        fs::create_dir(root.join("空目录 🎉")).unwrap();
        fs::write(root.join("零字节"), []).unwrap();
        let collect = |native: bool| {
            let mut rows = std::collections::HashMap::new();
            let mut emit = |entry: io::Result<Entry>| {
                let e = entry.unwrap();
                assert!(
                    rows.insert(e.name, (e.is_dir, e.size, e.allocated, e.modified))
                        .is_none()
                );
            };
            if native {
                windows_directory::Reader::new()
                    .enumerate(
                        &root,
                        None,
                        &AtomicBool::new(false),
                        &DirectoryIo::default(),
                        &mut emit,
                    )
                    .unwrap();
            } else {
                enumerate_portable(&root, &AtomicBool::new(false), &mut emit).unwrap();
            }
            rows
        };
        let native = collect(true);
        let portable = collect(false);
        assert_eq!(native.len(), 602);
        assert_eq!(native, portable);
    }
}
