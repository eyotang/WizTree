//! Versioned, validated snapshots and atomic local preferences.
use chrono::{DateTime, SecondsFormat, Utc};
use disk_core::{Node, ScanIssue, ScanMethod, Snapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
const VERSION: u32 = 1;
const MAX_DOCUMENT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PREFERENCES_BYTES: u64 = 1024 * 1024;
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn unicode<'a>(value: &'a OsStr, field: &str) -> io::Result<&'a str> {
    let value = value.to_str().ok_or_else(|| {
        invalid(format!(
            "{field} 包含非 Unicode 字符，无法无损写入 JSON/CSV"
        ))
    })?;
    if value.contains('\0') {
        return Err(invalid(format!("{field} 包含 NUL 字符")));
    }
    Ok(value)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDuration {
    seconds: u64,
    nanos: u32,
}
impl StoredDuration {
    fn from_duration(value: Duration) -> Self {
        Self {
            seconds: value.as_secs(),
            nanos: value.subsec_nanos(),
        }
    }
    fn duration(&self) -> io::Result<Duration> {
        if self.nanos >= 1_000_000_000 {
            return Err(invalid("无效的纳秒字段"));
        }
        Ok(Duration::new(self.seconds, self.nanos))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTime {
    before_epoch: bool,
    seconds: u64,
    nanos: u32,
}
impl StoredTime {
    fn from_time(time: SystemTime) -> Self {
        let (before_epoch, duration) = match time.duration_since(UNIX_EPOCH) {
            Ok(value) => (false, value),
            Err(error) => (true, error.duration()),
        };
        Self {
            before_epoch,
            seconds: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        }
    }
    fn time(&self) -> io::Result<SystemTime> {
        let duration = StoredDuration {
            seconds: self.seconds,
            nanos: self.nanos,
        }
        .duration()?;
        if self.before_epoch {
            UNIX_EPOCH.checked_sub(duration)
        } else {
            UNIX_EPOCH.checked_add(duration)
        }
        .ok_or_else(|| invalid("修改时间超出平台支持范围"))
    }
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredMethod {
    ParallelWalk,
    NtfsMft,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredNode {
    parent: Option<usize>,
    name: String,
    is_dir: bool,
    size: u64,
    allocated: u64,
    modified: Option<StoredTime>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIssue {
    path: String,
    message: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    root: String,
    nodes: Vec<StoredNode>,
    files: u64,
    dirs: u64,
    errors: u64,
    cancelled: bool,
    elapsed: StoredDuration,
    method: StoredMethod,
    fallback_reason: Option<String>,
    issues: Vec<StoredIssue>,
}
fn document(snapshot: &Snapshot) -> io::Result<Document> {
    let root = unicode(snapshot.root.as_os_str(), "根路径")?.to_owned();
    let nodes = snapshot
        .nodes
        .iter()
        .map(|node| {
            Ok(StoredNode {
                parent: node.parent,
                name: unicode(&node.name, "文件名")?.to_owned(),
                is_dir: node.is_dir,
                size: node.size,
                allocated: node.allocated,
                modified: node.modified.map(StoredTime::from_time),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let issues = snapshot
        .issues
        .iter()
        .map(|issue| {
            Ok(StoredIssue {
                path: unicode(issue.path.as_os_str(), "错误路径")?.to_owned(),
                message: issue.message.clone(),
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(Document {
        version: VERSION,
        root,
        nodes,
        files: snapshot.files,
        dirs: snapshot.dirs,
        errors: snapshot.errors,
        cancelled: snapshot.cancelled,
        elapsed: StoredDuration::from_duration(snapshot.elapsed),
        method: match snapshot.method {
            ScanMethod::ParallelWalk => StoredMethod::ParallelWalk,
            ScanMethod::NtfsMft => StoredMethod::NtfsMft,
        },
        fallback_reason: snapshot.fallback_reason.clone(),
        issues,
    })
}
/// Writes a v1 .wtree.json document using a sibling temporary file. The old
/// destination is retained if serialization, flushing or replacement fails.
pub fn save(snapshot: &Snapshot, path: &Path) -> io::Result<()> {
    let document = document(snapshot)?;
    // Validate before creating a temporary file, without duplicating the arena.
    validate_header(&document)?;
    atomic_write(path, |file| {
        let mut writer = LimitedWriter {
            inner: BufWriter::new(file),
            written: 0,
            limit: MAX_DOCUMENT_BYTES,
        };
        serde_json::to_writer(&mut writer, &document).map_err(io::Error::from)?;
        writer.flush()
    })
}
fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    if size > limit {
        return Err(invalid(format!(
            "文件超过读取上限（{} MiB）",
            limit / (1024 * 1024)
        )));
    }
    let mut bytes = Vec::with_capacity((size as usize).min(16 * 1024 * 1024));
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("读取期间文件增长并超过大小上限"));
    }
    Ok(bytes)
}
/// Reads at most 512 MiB, rejects unknown versions, validates the arena and
/// recomputes all directory totals and global file/directory counts.
pub fn load(path: &Path) -> io::Result<Snapshot> {
    let bytes = read_limited(path, MAX_DOCUMENT_BYTES)?;
    let document: Document = serde_json::from_slice(&bytes).map_err(io::Error::from)?;
    restore(document)
}
fn validate_header(document: &Document) -> io::Result<()> {
    if document.version != VERSION {
        return Err(invalid(format!(
            "不支持的扫描文档版本：{}",
            document.version
        )));
    }
    let root = Path::new(&document.root);
    if document.root.contains('\0')
        || !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(invalid("扫描根路径必须是没有父目录跳转的绝对路径"));
    }
    if document.nodes.is_empty() {
        return Err(invalid("扫描文档缺少根节点"));
    }
    if document.nodes[0].parent.is_some() {
        return Err(invalid("节点 0 必须是唯一根节点"));
    }
    if document.issues.len() > 128 || document.issues.len() as u64 > document.errors {
        return Err(invalid("错误样本与错误总数不一致"));
    }
    document.elapsed.duration()?;
    Ok(())
}
fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.contains('\0') || name.contains('/') {
        return Err(invalid("文件名为空或包含 NUL"));
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(invalid("非根节点名称必须是单个文件名，不能包含目录跳转"));
    }
    #[cfg(windows)]
    if name.contains(['/', '\\', ':']) {
        return Err(invalid("Windows 文件名不能包含分隔符或数据流冒号"));
    }
    Ok(())
}
fn restore(document: Document) -> io::Result<Snapshot> {
    validate_header(&document)?;
    let mut nodes: Vec<Node> = Vec::with_capacity(document.nodes.len());
    let mut claimed_directories = Vec::new();
    let (mut files, mut dirs) = (0u64, 0u64);
    for (id, stored) in document.nodes.into_iter().enumerate() {
        if id > 0 {
            validate_name(&stored.name)?;
            let parent = stored
                .parent
                .ok_or_else(|| invalid("只有节点 0 可以没有父节点"))?;
            if parent >= id || !nodes[parent].is_dir {
                return Err(invalid("父节点必须在当前节点之前，且必须为目录"));
            }
            nodes[parent].children.push(id);
        } else if stored.name.contains('\0') {
            return Err(invalid("根节点名称包含 NUL"));
        }
        let (size, allocated) = if stored.is_dir {
            dirs += 1;
            claimed_directories.push((id, stored.size, stored.allocated));
            (0, 0)
        } else {
            files += 1;
            (stored.size, stored.allocated)
        };
        nodes.push(Node {
            parent: stored.parent,
            name: stored.name.into(),
            is_dir: stored.is_dir,
            size,
            allocated,
            modified: stored.modified.as_ref().map(StoredTime::time).transpose()?,
            children: Vec::new(),
        });
    }
    for id in (1..nodes.len()).rev() {
        let parent = nodes[id].parent.unwrap();
        nodes[parent].size = nodes[parent]
            .size
            .checked_add(nodes[id].size)
            .ok_or_else(|| invalid("逻辑大小汇总溢出"))?;
        nodes[parent].allocated = nodes[parent]
            .allocated
            .checked_add(nodes[id].allocated)
            .ok_or_else(|| invalid("分配空间汇总溢出"))?;
    }
    for (id, size, allocated) in claimed_directories {
        if nodes[id].size != size || nodes[id].allocated != allocated {
            return Err(invalid("目录大小与文件汇总不一致，文档可能被修改"));
        }
    }
    if files != document.files || dirs != document.dirs {
        return Err(invalid("文件/目录总数与节点数据不一致"));
    }
    let issues = document
        .issues
        .into_iter()
        .map(|issue| {
            if issue.path.contains('\0') {
                Err(invalid("错误路径包含 NUL"))
            } else {
                Ok(ScanIssue {
                    path: issue.path.into(),
                    message: issue.message,
                })
            }
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(Snapshot {
        root: document.root.into(),
        nodes,
        elapsed: document.elapsed.duration()?,
        files,
        dirs,
        errors: document.errors,
        cancelled: document.cancelled,
        method: match document.method {
            StoredMethod::ParallelWalk => ScanMethod::ParallelWalk,
            StoredMethod::NtfsMft => ScanMethod::NtfsMft,
        },
        fallback_reason: document.fallback_reason,
        issues,
    })
}
fn csv_date(time: Option<SystemTime>) -> io::Result<String> {
    let Some(time) = time else {
        return Ok(String::new());
    };
    let (seconds, nanos) = match time.duration_since(UNIX_EPOCH) {
        Ok(value) => (
            i64::try_from(value.as_secs()).map_err(|_| invalid("日期超出 CSV 支持范围"))?,
            value.subsec_nanos(),
        ),
        Err(error) => {
            let duration = error.duration();
            let seconds =
                i64::try_from(duration.as_secs()).map_err(|_| invalid("日期超出 CSV 支持范围"))?;
            if duration.subsec_nanos() == 0 {
                (-seconds, 0)
            } else {
                (
                    -seconds.checked_add(1).ok_or_else(|| invalid("日期溢出"))?,
                    1_000_000_000 - duration.subsec_nanos(),
                )
            }
        }
    };
    DateTime::<Utc>::from_timestamp(seconds, nanos)
        .map(|date| date.to_rfc3339_opts(SecondsFormat::AutoSi, true))
        .ok_or_else(|| invalid("日期超出 CSV 支持范围"))
}
fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}
/// Exports all nodes, or the selected arena IDs in supplied order. Dates are
/// ISO 8601 UTC; missing dates are empty. Invalid IDs fail before replacing data.
pub fn export_csv(snapshot: &Snapshot, ids: Option<&[usize]>, path: &Path) -> io::Result<()> {
    if let Some(ids) = ids
        && ids.iter().any(|id| *id >= snapshot.nodes.len())
    {
        return Err(invalid("CSV 选择包含无效节点编号"));
    }
    atomic_write(path, |file| {
        let mut writer = BufWriter::new(file);
        writeln!(writer, "path,type,logical,allocated,modified_utc")?;
        let mut row = |id: usize| -> io::Result<()> {
            let node = &snapshot.nodes[id];
            let full_path = snapshot.path(id);
            let full_path = unicode(full_path.as_os_str(), "文件路径")?;
            writeln!(
                writer,
                "{},{},{},{},{}",
                csv_field(full_path),
                if node.is_dir { "directory" } else { "file" },
                node.size,
                node.allocated,
                csv_field(&csv_date(node.modified)?)
            )
        };
        if let Some(ids) = ids {
            for &id in ids {
                row(id)?;
            }
        } else {
            for id in 0..snapshot.nodes.len() {
                row(id)?;
            }
        }
        writer.flush()
    })
}
struct LimitedWriter<W> {
    inner: W,
    written: u64,
    limit: u64,
}
impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.written) {
            return Err(invalid("文档超过写入大小上限"));
        }
        let count = self.inner.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
fn atomic_write(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "保存路径没有文件名"))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = None;
    for _ in 0..16 {
        let mut name = file_name.to_os_string();
        name.push(format!(
            ".tmp.{}.{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let candidate = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let (temporary, mut file) = temporary
        .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "无法分配临时保存文件"))?;
    let result = write(&mut file).and_then(|_| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = replace(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}
#[cfg(windows)]
fn replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if source[..source.len() - 1].contains(&0) || target[..target.len() - 1].contains(&0) {
        return Err(invalid("保存路径包含 NUL"));
    }
    // SAFETY: both strings are NUL terminated and remain valid for this
    // synchronous same-directory replacement. No cross-volume copy is enabled.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    pub recent: Vec<PathBuf>,
    pub dark: Option<bool>,
    pub prefer_mft: bool,
}
fn preferences_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("WIZTREE_STATE_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path).join("preferences.json"));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("AppData/Roaming"))
    });
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    base.map(|base| base.join("WizTreeModern/preferences.json"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法确定偏好设置目录"))
}
fn read_preferences(path: &Path) -> io::Result<Preferences> {
    let bytes = read_limited(path, MAX_PREFERENCES_BYTES)?;
    let mut preferences: Preferences = serde_json::from_slice(&bytes).map_err(io::Error::from)?;
    let mut seen = HashSet::new();
    preferences.recent.retain(|path| {
        path.is_absolute()
            && unicode(path.as_os_str(), "最近路径").is_ok()
            && seen.insert(path.clone())
    });
    preferences.recent.truncate(20);
    Ok(preferences)
}
/// A bounded, local last-scan report for diagnosing a slow first scan.
/// This stores totals only, never the directory tree, and is not a scan cache.
pub fn save_scan_diagnostics(
    snapshot: &Snapshot,
    workers: usize,
    progress: disk_core::ProgressSnapshot,
    index_elapsed: Duration,
    strategy: disk_core::ScanStrategy,
) -> io::Result<()> {
    let path = preferences_path()?.with_file_name("last-scan.json");
    let io = progress.directory_io;
    let mut report = serde_json::json!({
        "version": 2,
        "build": "2026-10-15-full-directory-info-uac",
        "backend": if snapshot.method == ScanMethod::NtfsMft { "windows-ntfs-mft" } else { disk_core::directory_backend() },
        "finished_utc": Utc::now().to_rfc3339(),
        "method": snapshot.method.to_string(),
        "workers": workers,
        "files": snapshot.files, "directories": snapshot.dirs,
        "errors": snapshot.errors, "cancelled": snapshot.cancelled,
        "logical_bytes": snapshot.nodes.first().map_or(0, |node| node.size),
        "allocated_bytes": snapshot.nodes.first().map_or(0, |node| node.allocated),
        "scan_ms": snapshot.elapsed.as_secs_f64() * 1000.,
        "index_ms": index_elapsed.as_secs_f64() * 1000.,
        "canonicalize_ms": io.canonicalize_ns as f64 / 1e6,
        "setup_ms": io.setup_ns as f64 / 1e6,
        "pool_ms": io.pool_ns as f64 / 1e6,
        "traversal_ms": io.traversal_ns as f64 / 1e6,
        "aggregate_ms": io.aggregate_ns as f64 / 1e6,
        "worker_start_delay_worker_ms": io.worker_start_delay_ns as f64 / 1e6,
        "queue_wait_worker_ms": io.queue_wait_ns as f64 / 1e6,
        "entry_allocate_worker_ms": io.entry_allocate_ns as f64 / 1e6,
        "enumerate_worker_ms": io.enumerate_ns as f64 / 1e6,
        "publish_wait_worker_ms": io.publish_wait_ns as f64 / 1e6,
        "publish_worker_ms": io.publish_ns as f64 / 1e6,
        "job_drop_worker_ms": io.job_drop_ns as f64 / 1e6,
        "directory_opens": io.opens, "absolute_opens": io.absolute_opens,
        "query_calls": io.queries,
        "open_worker_ms": io.open_ns as f64 / 1e6,
        "query_worker_ms": io.query_ns as f64 / 1e6,
        "special_files": io.special_files,
        "special_worker_ms": io.special_ns as f64 / 1e6,
        "portable_fallbacks": io.portable_fallbacks,
        "mft_bytes": progress.mft_bytes,
        "mft_extents": progress.mft_extents,
        "mft_cached": progress.mft_cached,
        "fallback_reason": snapshot.fallback_reason,
        "time_note": "Worker times are summed across concurrent threads; cache state was not measured.",
    });
    report["app_version"] = env!("CARGO_PKG_VERSION").into();
    report["scan_root"] = serde_json::to_value(&snapshot.root).map_err(io::Error::from)?;
    report["requested_strategy"] = format!("{strategy:?}").into();
    #[cfg(windows)]
    {
        report["process_elevated"] =
            serde_json::to_value(crate::elevation::is_elevated().ok()).map_err(io::Error::from)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(&path, |file| {
        serde_json::to_writer_pretty(file, &report).map_err(io::Error::from)
    })
}
pub fn load_preferences() -> Preferences {
    preferences_path()
        .and_then(|path| read_preferences(&path))
        .unwrap_or_default()
}
fn write_preferences(preferences: &Preferences, path: &Path) -> io::Result<()> {
    for path in &preferences.recent {
        unicode(path.as_os_str(), "最近路径")?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(path, |file| {
        let mut writer = LimitedWriter {
            inner: BufWriter::new(file),
            written: 0,
            limit: MAX_PREFERENCES_BYTES,
        };
        serde_json::to_writer(&mut writer, preferences).map_err(io::Error::from)?;
        writer.flush()
    })
}
pub fn save_preferences(preferences: &Preferences) -> io::Result<()> {
    write_preferences(preferences, &preferences_path()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::{Progress, ScanOptions, scan};
    use std::sync::atomic::AtomicBool;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "wiztree-storage-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn snapshot(&self) -> Snapshot {
            fs::create_dir_all(self.0.join("folder")).unwrap();
            fs::write(self.0.join("folder/comma,name.txt"), b"hello").unwrap();
            let time = UNIX_EPOCH + Duration::new(1_650_000_000, 123_456_700);
            File::options()
                .write(true)
                .open(self.0.join("folder/comma,name.txt"))
                .unwrap()
                .set_modified(time)
                .unwrap();
            scan(
                &self.0,
                &ScanOptions { workers: 2 },
                &AtomicBool::new(false),
                &Progress::default(),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn snapshot_roundtrip_rebuilds_arena_and_preserves_times() {
        let fixture = Fixture::new();
        let original = fixture.snapshot();
        let path = fixture.0.join("sample.wtree.json");
        save(&original, &path).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!(restored.root, original.root);
        assert_eq!(restored.files, original.files);
        assert_eq!(restored.dirs, original.dirs);
        assert_eq!(restored.elapsed, original.elapsed);
        assert_eq!(restored.method, original.method);
        for (a, b) in restored.nodes.iter().zip(&original.nodes) {
            assert_eq!(
                (
                    &a.name,
                    a.parent,
                    a.is_dir,
                    a.size,
                    a.allocated,
                    a.modified,
                    &a.children
                ),
                (
                    &b.name,
                    b.parent,
                    b.is_dir,
                    b.size,
                    b.allocated,
                    b.modified,
                    &b.children
                )
            );
        }
    }
    #[test]
    fn malformed_graphs_versions_and_forged_totals_are_rejected() {
        let fixture = Fixture::new();
        let snapshot = fixture.snapshot();
        let baseline = serde_json::to_value(document(&snapshot).unwrap()).unwrap();
        for mutate in [0, 1, 2, 3, 4, 5, 6] {
            let mut value = baseline.clone();
            match mutate {
                0 => value["version"] = 2.into(),
                1 => value["nodes"][1]["parent"] = serde_json::Value::Null,
                2 => value["nodes"][1]["parent"] = 1.into(),
                3 => value["nodes"][0]["size"] = 999.into(),
                4 => value["files"] = 999.into(),
                5 => value["nodes"][1]["name"] = "..".into(),
                6 => value["nodes"][0]["parent"] = 0.into(),
                _ => unreachable!(),
            };
            let doc: Document = serde_json::from_value(value).unwrap();
            assert!(restore(doc).is_err(), "mutation {mutate} accepted");
        }
    }
    #[test]
    fn failed_atomic_write_keeps_existing_document_and_removes_temporary() {
        let fixture = Fixture::new();
        let path = fixture.0.join("saved");
        fs::write(&path, b"original").unwrap();
        assert!(
            atomic_write(&path, |file| {
                file.write_all(b"partial")?;
                Err(io::Error::other("injected failure"))
            })
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        atomic_write(&path, |file| file.write_all(b"replacement")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }
    #[test]
    fn csv_subset_quotes_paths_dates_and_rejects_invalid_ids_without_overwrite() {
        let fixture = Fixture::new();
        let snapshot = fixture.snapshot();
        let id = snapshot.nodes.iter().position(|node| !node.is_dir).unwrap();
        let path = fixture.0.join("export.csv");
        export_csv(&snapshot, Some(&[id]), &path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("comma,name.txt\",file,5,"));
        assert!(text.contains("2022-04-15T05:20:00.123456700Z"));
        assert!(export_csv(&snapshot, Some(&[usize::MAX]), &path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert_eq!(csv_field("a,\"b\"\nc"), "\"a,\"\"b\"\"\nc\"");
    }
    #[test]
    fn modified_times_before_unix_epoch_are_lossless() {
        let original = UNIX_EPOCH - Duration::new(3, 123_456_700);
        assert_eq!(StoredTime::from_time(original).time().unwrap(), original);
        assert_eq!(
            csv_date(Some(original)).unwrap(),
            "1969-12-31T23:59:56.876543300Z"
        );
    }
    #[test]
    fn read_limit_is_enforced_before_json_parsing() {
        let fixture = Fixture::new();
        let path = fixture.0.join("too-big");
        fs::write(&path, [0u8; 33]).unwrap();
        assert!(read_limited(&path, 32).is_err());
        assert_eq!(read_limited(&path, 33).unwrap().len(), 33);
    }
    #[test]
    fn preferences_roundtrip_deduplicates_recent_paths() {
        let fixture = Fixture::new();
        let path = fixture.0.join("state/preferences.json");
        let preferences = Preferences {
            prefer_mft: true,
            recent: vec![fixture.0.clone(), fixture.0.clone()],
            dark: Some(true),
        };
        write_preferences(&preferences, &path).unwrap();
        let restored = read_preferences(&path).unwrap();
        assert_eq!(restored.recent, vec![fixture.0.clone()]);
        assert_eq!(restored.dark, Some(true));
        assert!(restored.prefer_mft);
        fs::write(&path, br#"{"recent":[],"dark":false}"#).unwrap();
        assert!(!read_preferences(&path).unwrap().prefer_mft);
    }
    #[cfg(windows)]
    #[test]
    fn non_unicode_names_are_explicitly_rejected_without_overwriting() {
        use std::os::windows::ffi::OsStringExt;
        let fixture = Fixture::new();
        let mut snapshot = fixture.snapshot();
        snapshot.nodes[1].name = std::ffi::OsString::from_wide(&[0xd800]);
        let path = fixture.0.join("keep.wtree.json");
        fs::write(&path, b"keep").unwrap();
        let error = save(&snapshot, &path).unwrap_err();
        assert!(error.to_string().contains("非 Unicode"));
        assert_eq!(fs::read(path).unwrap(), b"keep");
    }
    #[cfg(unix)]
    #[test]
    fn non_unicode_names_are_explicitly_rejected_without_overwriting() {
        use std::os::unix::ffi::OsStringExt;
        let fixture = Fixture::new();
        let mut snapshot = fixture.snapshot();
        snapshot.nodes[1].name = std::ffi::OsString::from_vec(vec![255]);
        let path = fixture.0.join("keep.wtree.json");
        fs::write(&path, b"keep").unwrap();
        assert!(save(&snapshot, &path).is_err());
        assert_eq!(fs::read(path).unwrap(), b"keep");
    }
    #[test]
    fn file_root_and_scan_status_roundtrip_without_rescanning() {
        let fixture = Fixture::new();
        let source = fixture.0.join("single.bin");
        fs::write(&source, b"abc").unwrap();
        let mut snapshot = scan(
            &source,
            &ScanOptions::default(),
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        snapshot.cancelled = true;
        snapshot.errors = 1;
        snapshot.fallback_reason = Some("access denied".into());
        snapshot.issues = vec![ScanIssue {
            path: source.clone(),
            message: "sample failure".into(),
        }];
        snapshot.nodes[0].modified = None;
        let path = fixture.0.join("single.wtree.json");
        save(&snapshot, &path).unwrap();
        fs::remove_file(&source).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!((restored.files, restored.dirs, restored.errors), (1, 0, 1));
        assert!(restored.cancelled);
        assert_eq!(restored.nodes[0].size, 3);
        assert!(restored.nodes[0].children.is_empty());
        assert_eq!(restored.nodes[0].modified, None);
        assert_eq!(restored.fallback_reason, snapshot.fallback_reason);
        assert_eq!(restored.issues[0].message, "sample failure");
    }
    #[test]
    fn file_as_parent_and_size_overflow_are_rejected() {
        let fixture = Fixture::new();
        let snapshot = fixture.snapshot();
        let baseline = serde_json::to_value(document(&snapshot).unwrap()).unwrap();
        let mut file_parent = baseline.clone();
        file_parent["nodes"][1]["is_dir"] = false.into();
        let document: Document = serde_json::from_value(file_parent).unwrap();
        assert!(restore(document).is_err());
        let mut overflow = baseline;
        overflow["nodes"][2]["size"] = u64::MAX.into();
        let mut extra = overflow["nodes"][2].clone();
        extra["size"] = 1.into();
        extra["name"] = "extra".into();
        overflow["nodes"].as_array_mut().unwrap().push(extra);
        let document: Document = serde_json::from_value(overflow).unwrap();
        assert!(restore(document).is_err());
    }
}

/// Imports this application's CSV and WizTree's UTF-8 CSV export. Directory
/// aggregate columns are deliberately ignored; all totals are rebuilt from
/// file rows. CSV is an interchange format, so the imported method is the
/// portable-walk category with a descriptive source note.
pub fn load_csv(path: &Path) -> io::Result<Snapshot> {
    let bytes = read_limited(path, MAX_DOCUMENT_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("CSV 必须使用 UTF-8 编码；不支持的编码请先转换"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (offset, columns) = csv_header(text)?;
    let mut cursor = offset;
    let _header = csv_record(text.as_bytes(), &mut cursor)?;
    let mut entries = Vec::new();
    while let Some(row) = csv_record(text.as_bytes(), &mut cursor)? {
        if row.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let field = |index: usize| {
            row.get(index)
                .map(String::as_str)
                .ok_or_else(|| invalid("CSV 数据行缺少必要列"))
        };
        let raw_path = field(columns.path)?;
        let entry_path = csv_absolute_path(raw_path)?;
        let tail_directory = raw_path.ends_with('/') || (cfg!(windows) && raw_path.ends_with('\\'));
        let attribute_directory = columns
            .attributes
            .and_then(|index| row.get(index))
            .map(|value| csv_directory_attribute(value))
            .transpose()?
            .unwrap_or(false);
        let is_dir = match columns
            .kind
            .and_then(|index| row.get(index))
            .map(|value| value.trim().to_lowercase())
        {
            Some(value)
                if ["directory", "dir", "folder", "目录", "文件夹"].contains(&value.as_str()) =>
            {
                true
            }
            Some(value) if ["file", "文件"].contains(&value.as_str()) => {
                if tail_directory || attribute_directory {
                    return Err(invalid("CSV 文件类型与路径/目录属性冲突"));
                }
                false
            }
            Some(value) if !value.is_empty() => {
                return Err(invalid(format!("不支持的 CSV 文件类型：{value}")));
            }
            _ => tail_directory || attribute_directory,
        };
        // Require both columns to exist even for directories, but never add
        // exported directory aggregates to the leaf data a second time.
        let raw_size = field(columns.size)?;
        let raw_allocated = field(columns.allocated)?;
        let (size, allocated) = if is_dir {
            (0, 0)
        } else {
            (csv_bytes(raw_size)?, csv_bytes(raw_allocated)?)
        };
        let modified = parse_csv_time(
            columns
                .modified
                .and_then(|index| row.get(index))
                .map_or("", String::as_str),
        )?;
        entries.push(CsvEntry {
            path: entry_path,
            is_dir,
            size,
            allocated,
            modified,
        });
    }
    if entries.is_empty() {
        return Err(invalid("CSV 没有文件或目录数据"));
    }
    csv_snapshot(entries)
}
struct CsvColumns {
    path: usize,
    size: usize,
    allocated: usize,
    modified: Option<usize>,
    attributes: Option<usize>,
    kind: Option<usize>,
}
fn csv_column_name(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|value| !matches!(value, ' ' | '_' | '-'))
        .collect()
}
fn csv_header(text: &str) -> io::Result<(usize, CsvColumns)> {
    let mut offset = 0;
    // WizTree preambles are plain physical lines, not part of the data table.
    // Find a recognized header before parsing the quoted data records.
    for line in text.split_inclusive('\n').take(32) {
        let mut cursor = 0;
        if let Ok(Some(row)) = csv_record(line.as_bytes(), &mut cursor) {
            let names: Vec<_> = row.iter().map(|value| csv_column_name(value)).collect();
            let find = |aliases: &[&str]| {
                names
                    .iter()
                    .position(|value| aliases.contains(&value.as_str()))
            };
            if let (Some(path), Some(size), Some(allocated)) = (
                find(&[
                    "path",
                    "filename",
                    "filepath",
                    "文件名",
                    "文件名称",
                    "文件路径",
                ]),
                find(&["size", "logical", "logicalsize", "大小", "逻辑大小"]),
                find(&["allocated", "allocatedsize", "分配", "分配大小", "占用空间"]),
            ) {
                return Ok((
                    offset,
                    CsvColumns {
                        path,
                        size,
                        allocated,
                        modified: find(&[
                            "modified",
                            "modifiedutc",
                            "lastmodified",
                            "修改时间",
                            "修改日期",
                            "最近修改时间",
                        ]),
                        attributes: find(&["attributes", "属性"]),
                        kind: find(&["type", "类型", "类别"]),
                    },
                ));
            }
        }
        offset += line.len();
    }
    Err(invalid(
        "无法识别 CSV 表头：需要路径/File Name/文件名称、大小和分配列",
    ))
}
/// RFC 4180-style state machine. Delimiters and line endings inside quoted
/// fields remain literal; quotes escape as a doubled pair. Malformed quoting
/// is rejected rather than silently turning a path into extra columns.
fn csv_record(input: &[u8], cursor: &mut usize) -> io::Result<Option<Vec<String>>> {
    if *cursor >= input.len() {
        return Ok(None);
    }
    let mut fields = Vec::new();
    let mut field = Vec::new();
    // 0 = field start, 1 = unquoted, 2 = quoted, 3 = closing quote.
    let mut state = 0u8;
    while *cursor < input.len() {
        let byte = input[*cursor];
        *cursor += 1;
        if state == 2 {
            if byte == b'"' {
                if input.get(*cursor) == Some(&b'"') {
                    field.push(b'"');
                    *cursor += 1;
                } else {
                    state = 3;
                }
            } else {
                field.push(byte);
            }
            continue;
        }
        if byte == b',' {
            fields.push(
                String::from_utf8(std::mem::take(&mut field))
                    .map_err(|_| invalid("CSV 字段不是 UTF-8"))?,
            );
            state = 0;
        } else if byte == b'\r' || byte == b'\n' {
            if byte == b'\r' && input.get(*cursor) == Some(&b'\n') {
                *cursor += 1;
            }
            fields.push(String::from_utf8(field).map_err(|_| invalid("CSV 字段不是 UTF-8"))?);
            return Ok(Some(fields));
        } else if byte == b'"' && state == 0 {
            state = 2;
        } else if byte == b'"' || state == 3 {
            return Err(invalid("CSV 引号格式错误"));
        } else {
            field.push(byte);
            state = 1;
        }
    }
    if state == 2 {
        return Err(invalid("CSV 引号字段未结束"));
    }
    fields.push(String::from_utf8(field).map_err(|_| invalid("CSV 字段不是 UTF-8"))?);
    Ok(Some(fields))
}
fn csv_bytes(value: &str) -> io::Result<u64> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(format!("CSV 字节数必须是非负整数：{value}")));
    }
    value.parse().map_err(|_| invalid("CSV 字节数溢出"))
}
fn csv_directory_attribute(value: &str) -> io::Result<bool> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(false);
    }
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .map(|flags| flags & 0x10 != 0)
            .map_err(|_| invalid("CSV 十六进制属性无效"));
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u64>()
            .map(|flags| flags & 0x10 != 0)
            .map_err(|_| invalid("CSV 数字属性溢出"));
    }
    let flags = value.to_ascii_uppercase();
    if flags == "DIRECTORY" {
        return Ok(true);
    }
    if !flags.chars().all(|flag| "RHSADNTOCPIUVE- ".contains(flag)) {
        return Err(invalid(format!("不支持的 CSV 属性表示：{value}")));
    }
    Ok(flags.contains('D'))
}
fn csv_absolute_path(value: &str) -> io::Result<PathBuf> {
    if value.is_empty() || value.contains('\0') {
        return Err(invalid("CSV 路径为空或包含 NUL"));
    }
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(invalid(format!(
            "CSV 路径必须是本平台的绝对路径，且不含父目录跳转：{value}"
        )));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::CurDir) {
            normalized.push(component.as_os_str());
        }
    }
    Ok(normalized)
}
fn csv_path_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.as_os_str().to_string_lossy().to_lowercase();
        if let Some(unc) = text.strip_prefix(r"\\?\unc\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else if let Some(drive) = text.strip_prefix(r"\\?\") {
            PathBuf::from(drive)
        } else {
            PathBuf::from(text)
        }
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}
fn parse_csv_time(value: &str) -> io::Result<Option<SystemTime>> {
    use chrono::TimeZone;
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let date = if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        date.with_timezone(&Utc)
    } else {
        let naive = [
            "%Y/%m/%d %H:%M:%S%.f",
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S%.f",
        ]
        .into_iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(value, format).ok())
        .ok_or_else(|| invalid(format!("无法识别 CSV 修改时间：{value}")))?;
        chrono::Local
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| invalid("CSV 本地修改时间处于夏令时歧义或不存在的时间区间"))?
            .with_timezone(&Utc)
    };
    let seconds = date.timestamp();
    let nanos = date.timestamp_subsec_nanos();
    let time = if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds as u64, nanos))
    } else {
        let seconds = seconds.unsigned_abs();
        let duration = if nanos == 0 {
            Duration::from_secs(seconds)
        } else {
            Duration::new(seconds - 1, 1_000_000_000 - nanos)
        };
        UNIX_EPOCH.checked_sub(duration)
    };
    time.map(Some)
        .ok_or_else(|| invalid("CSV 修改时间超出平台范围"))
}
struct CsvEntry {
    path: PathBuf,
    is_dir: bool,
    size: u64,
    allocated: u64,
    modified: Option<SystemTime>,
}
fn csv_snapshot(entries: Vec<CsvEntry>) -> io::Result<Snapshot> {
    use std::collections::HashMap;
    let mut root = entries[0].path.clone();
    for entry in &entries {
        let key = csv_path_key(&entry.path);
        while !key.starts_with(csv_path_key(&root)) {
            if !root.pop() {
                return Err(invalid("CSV 含不同卷或不同共享根的路径，请分别导入"));
            }
        }
    }
    let root_components = root.components().count();
    let mut nodes = vec![Node {
        parent: None,
        name: root.file_name().unwrap_or(root.as_os_str()).to_os_string(),
        size: 0,
        allocated: 0,
        is_dir: true,
        modified: None,
        children: Vec::new(),
    }];
    let mut ids = HashMap::from([(csv_path_key(&root), 0usize)]);
    let mut explicit = HashSet::new();
    for entry in entries {
        let mut current = root.clone();
        let mut parent = 0usize;
        let parts: Vec<_> = entry.path.components().skip(root_components).collect();
        if parts.is_empty() {
            if !explicit.insert(0) {
                return Err(invalid("CSV 中存在重复路径记录"));
            }
            nodes[0].is_dir = entry.is_dir;
            nodes[0].size = entry.size;
            nodes[0].allocated = entry.allocated;
            nodes[0].modified = entry.modified;
            continue;
        }
        for (position, component) in parts.iter().enumerate() {
            if !nodes[parent].is_dir {
                return Err(invalid("CSV 将普通文件用作父目录"));
            }
            let Component::Normal(name) = component else {
                return Err(invalid("CSV 路径层级不合法"));
            };
            validate_name(
                name.to_str()
                    .ok_or_else(|| invalid("CSV 文件名不是 Unicode"))?,
            )?;
            current.push(name);
            let key = csv_path_key(&current);
            let last = position + 1 == parts.len();
            let id = if let Some(&id) = ids.get(&key) {
                id
            } else {
                let id = nodes.len();
                nodes.push(Node {
                    parent: Some(parent),
                    name: (*name).to_os_string(),
                    size: 0,
                    allocated: 0,
                    is_dir: if last { entry.is_dir } else { true },
                    modified: None,
                    children: Vec::new(),
                });
                nodes[parent].children.push(id);
                ids.insert(key, id);
                id
            };
            if last {
                if !explicit.insert(id) {
                    return Err(invalid("CSV 中存在重复路径记录"));
                }
                if nodes[id].is_dir != entry.is_dir {
                    return Err(invalid("CSV 路径同时被声明为文件和目录"));
                }
                nodes[id].size = entry.size;
                nodes[id].allocated = entry.allocated;
                nodes[id].modified = entry.modified;
            }
            parent = id;
        }
    }
    if !nodes[0].is_dir && nodes.len() > 1 {
        return Err(invalid("CSV 根文件不能包含子节点"));
    }
    let (mut files, mut dirs) = (0u64, 0u64);
    for node in &nodes {
        if node.is_dir {
            dirs += 1;
        } else {
            files += 1;
        }
    }
    for id in (1..nodes.len()).rev() {
        let parent = nodes[id].parent.unwrap();
        nodes[parent].size = nodes[parent]
            .size
            .checked_add(nodes[id].size)
            .ok_or_else(|| invalid("CSV 逻辑大小汇总溢出"))?;
        nodes[parent].allocated = nodes[parent]
            .allocated
            .checked_add(nodes[id].allocated)
            .ok_or_else(|| invalid("CSV 分配大小汇总溢出"))?;
    }
    Ok(Snapshot {
        root,
        nodes,
        elapsed: Duration::ZERO,
        files,
        dirs,
        errors: 0,
        cancelled: false,
        method: ScanMethod::ParallelWalk,
        fallback_reason: Some("已导入 CSV；扫描耗时和原始扫描方法未保存在 CSV 中".into()),
        issues: Vec::new(),
    })
}

#[cfg(test)]
mod csv_import_tests {
    use super::*;
    use disk_core::{Progress, ScanOptions, scan};
    use std::sync::atomic::AtomicBool;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "wiztree-csv-{}-{nonce}-{}",
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
    fn exported_csv_roundtrip_preserves_tree_unicode_and_modified_times() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("资料/空目录")).unwrap();
        fs::write(fixture.0.join("资料/报告,最终.csv"), b"hello").unwrap();
        fs::write(fixture.0.join("zero.txt"), []).unwrap();
        let snapshot = scan(
            &fixture.0,
            &ScanOptions { workers: 2 },
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        let path = fixture.0.join("saved.csv");
        export_csv(&snapshot, None, &path).unwrap();
        let imported = load_csv(&path).unwrap();
        assert_eq!(imported.root, snapshot.root);
        assert_eq!(
            (imported.files, imported.dirs),
            (snapshot.files, snapshot.dirs)
        );
        for (id, node) in snapshot.nodes.iter().enumerate() {
            let imported_id = imported
                .nodes
                .iter()
                .enumerate()
                .find_map(|(other, _)| (imported.path(other) == snapshot.path(id)).then_some(other))
                .unwrap();
            let other = &imported.nodes[imported_id];
            assert_eq!(
                (other.size, other.allocated, other.is_dir, other.modified),
                (node.size, node.allocated, node.is_dir, node.modified)
            );
        }
    }
    #[test]
    fn csv_parser_preserves_quotes_crlf_embedded_newline_and_rejects_bad_quotes() {
        let mut cursor = 0;
        let input = b"\"a,\"\"b\"\"\",\"line1\r\nline2\",\r\nlast,row";
        assert_eq!(
            csv_record(input, &mut cursor).unwrap().unwrap(),
            ["a,\"b\"", "line1\r\nline2", ""]
        );
        assert_eq!(
            csv_record(input, &mut cursor).unwrap().unwrap(),
            ["last", "row"]
        );
        assert!(csv_record(input, &mut cursor).unwrap().is_none());
        for input in [
            b"\"unterminated".as_slice(),
            b"x\"bad\",y",
            b"\"closed\"bad,y",
        ] {
            assert!(csv_record(input, &mut 0).is_err());
        }
    }
    #[test]
    fn original_wiztree_preamble_chinese_fields_and_directory_aggregates() {
        let fixture = Fixture::new();
        let root = fixture.0.join("export-root");
        let folder = root.join("目录");
        let file = folder.join("有,引号.txt");
        let text = format!(
            "\u{feff}生成由 WizTree 4.33\r\n捐赠信息\r\n第三行前言\r\n文件名称,大小,分配,修改时间,属性,文件,文件夹,DRIVECAPACITY\r\n{},9999,9999,2026/10/14 16:27:53,0,1,1,12345\r\n{},9999,9999,,D,1,0\r\n{},13,4096,,8224,0,0\r\n",
            csv_field(&format!("{}{}", root.display(), std::path::MAIN_SEPARATOR)),
            csv_field(&folder.display().to_string()),
            csv_field(&file.display().to_string())
        );
        let path = fixture.0.join("original.csv");
        fs::write(&path, text).unwrap();
        let snapshot = load_csv(&path).unwrap();
        assert_eq!(snapshot.root, root);
        assert_eq!((snapshot.files, snapshot.dirs), (1, 2));
        assert_eq!(
            (snapshot.nodes[0].size, snapshot.nodes[0].allocated),
            (13, 4096)
        );
        assert!(snapshot.nodes[0].modified.is_some());
        assert!(
            snapshot
                .nodes
                .iter()
                .find(|n| !n.is_dir)
                .unwrap()
                .modified
                .is_none()
        );
    }
    #[test]
    fn missing_directory_rows_are_rebuilt_from_common_ancestor() {
        let fixture = Fixture::new();
        let root = fixture.0.join("root");
        let path = fixture.0.join("files.csv");
        let text = format!(
            "File Name,Size,Allocated,Modified,Attributes\n{},4,4096,,32\n{},7,4096,,32\n",
            csv_field(&root.join("one/nested/a.txt").display().to_string()),
            csv_field(&root.join("two/b.txt").display().to_string())
        );
        fs::write(&path, text).unwrap();
        let snapshot = load_csv(&path).unwrap();
        assert_eq!(snapshot.root, root);
        assert_eq!((snapshot.files, snapshot.dirs), (2, 4));
        assert_eq!(snapshot.nodes[0].size, 11);
        for (id, node) in snapshot.nodes.iter().enumerate().skip(1) {
            assert!(node.parent.unwrap() < id);
            assert!(snapshot.nodes[node.parent.unwrap()].is_dir);
        }
    }
    #[test]
    fn unsafe_paths_duplicates_bad_bytes_and_unknown_headers_are_rejected() {
        let fixture = Fixture::new();
        let path = fixture.0.join("bad.csv");
        let absolute = fixture.0.join("file").display().to_string();
        for text in [
            "wrong,header\nx,y\n".to_owned(),
            "path,type,size,allocated,modified\nrelative,file,1,1,\n".to_owned(),
            format!(
                "path,type,size,allocated,modified\n{},file,1,1,\n{},file,1,1,\n",
                csv_field(&absolute),
                csv_field(&absolute)
            ),
            format!(
                "path,type,size,allocated,modified\n{},file,nope,1,\n",
                csv_field(&absolute)
            ),
        ] {
            fs::write(&path, text).unwrap();
            assert!(load_csv(&path).is_err());
        }
    }
    #[test]
    fn directory_attributes_are_explicit_and_never_inferred_from_arbitrary_words() {
        assert!(csv_directory_attribute("D").unwrap());
        assert!(csv_directory_attribute("0x2010").unwrap());
        assert!(!csv_directory_attribute("8224").unwrap());
        assert!(csv_directory_attribute("ReadOnly").is_err());
        assert!(csv_directory_attribute("0xinvalid").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn windows_verbatim_and_case_variants_share_a_common_root() {
        let entries = vec![
            CsvEntry {
                path: PathBuf::from(r"\\?\C:\Fixture\"),
                is_dir: true,
                size: 0,
                allocated: 0,
                modified: None,
            },
            CsvEntry {
                path: PathBuf::from(r"c:\fixture\Nested\file.txt"),
                is_dir: false,
                size: 7,
                allocated: 4096,
                modified: None,
            },
        ];
        let snapshot = csv_snapshot(entries).unwrap();
        assert_eq!((snapshot.files, snapshot.dirs), (1, 2));
        assert_eq!(snapshot.nodes[0].size, 7);
        let entries = vec![
            CsvEntry {
                path: PathBuf::from(r"C:\one.txt"),
                is_dir: false,
                size: 1,
                allocated: 4096,
                modified: None,
            },
            CsvEntry {
                path: PathBuf::from(r"D:\two.txt"),
                is_dir: false,
                size: 1,
                allocated: 4096,
                modified: None,
            },
        ];
        assert!(csv_snapshot(entries).is_err());
    }
}
