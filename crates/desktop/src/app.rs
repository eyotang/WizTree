use crate::{
    appearance::{chrome, quiet_button, toggle_button},
    interaction::{self, History, MarkPlan},
    query::{self, Metric, Mode, Row, SortKey},
    storage,
    tables::{Explorer, Extensions, bytes, number},
    treemap::{self, Tile},
};
use disk_core::{AnalysisIndex, Progress, ScanOptions, ScanStrategy, Snapshot, VolumeInfo};
use gpui_kit::component::{
    button::*,
    checkbox::Checkbox,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    resizable::{h_resizable, resizable_panel, v_resizable},
    tab::{Tab, TabBar},
    table::{DataTable, TableEvent, TableState},
    *,
};
use gpui_kit::{prelude::FluentBuilder, *};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
gpui_kit::actions!(
    disk,
    [
        GoBack,
        GoForward,
        GoUp,
        ToggleMark,
        ReviewMarks,
        EditLocation,
        OpenSelected,
        PreviousRow,
        NextRow,
        Find,
        ChooseFolder,
        Rescan,
        CancelScan,
        ExpandSelected,
        CollapseSelected,
        CopySelected,
        RevealSelected,
        TrashSelected
    ]
);
#[derive(Clone, Copy)]
pub enum ItemAction {
    Open,
    Reveal,
    Zoom,
    Copy,
    Export,
    Trash,
    Mark,
    Locate,
}
struct Data {
    source: Option<PathBuf>,
    snapshot: Arc<Snapshot>,
    index: Arc<AnalysisIndex>,
}
struct ScanJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<Progress>,
    rx: mpsc::Receiver<Result<Data, String>>,
    started: Instant,
    diagnostic_tick: u64,
}
struct QueryResult {
    version: u64,
    rows: Arc<Vec<Row>>,
}
struct MapRequest {
    version: u64,
    snapshot: Arc<Snapshot>,
    root: usize,
    allocated: bool,
    width: f32,
    height: f32,
    depth: usize,
}
struct MapResult {
    version: u64,
    tiles: Vec<Tile>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarTab {
    Details,
    Types,
    Review,
}
#[derive(Clone)]
struct ViewLocation {
    scope: usize,
    map_root: usize,
    selected: usize,
    expanded: HashSet<usize>,
    mode: Mode,
    query: String,
    exclude: String,
    minimum: String,
    extension: Option<String>,
    duplicates: bool,
    scroll: Point<Pixels>,
}
struct CleanupReport {
    snapshot: Arc<Snapshot>,
    succeeded: Vec<usize>,
    failed: Vec<(usize, String)>,
}
pub struct DiskApp {
    focus: FocusHandle,
    map_focus: FocusHandle,
    sidebar_tab: SidebarTab,
    map_only: bool,
    map_depth: usize,
    motion_enabled: bool,
    path_editing: bool,
    history: History<ViewLocation>,
    pending_scroll: Option<Point<Pixels>>,
    marked: HashSet<usize>,
    mark_plan: Arc<MarkPlan>,
    cleanup_busy: bool,
    cleanup_rx: Option<mpsc::Receiver<CleanupReport>>,
    path: Entity<InputState>,
    search: Entity<InputState>,
    minimum: Entity<InputState>,
    exclude: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    table: Entity<TableState<Explorer>>,
    types: Entity<TableState<Extensions>>,
    data: Option<Data>,
    expanded: HashSet<usize>,
    selected: usize,
    scope: usize,
    map_root: usize,
    mode: Mode,
    metric: Metric,
    extension: Option<String>,
    sort_key: SortKey,
    descending: bool,
    limit: usize,
    match_paths: bool,
    include_folders: bool,
    duplicates: bool,
    rows: Arc<Vec<Row>>,
    tiles: Vec<Tile>,
    job: Option<ScanJob>,
    folder_picker_open: bool,
    folder_picker_task: Option<Task<()>>,
    #[cfg(windows)]
    elevation_rx: Option<mpsc::Receiver<Result<(), String>>>,
    scan_strategy: ScanStrategy,
    scan_options: ScanOptions,
    query_tx: mpsc::SyncSender<query::Query>,
    query_rx: mpsc::Receiver<QueryResult>,
    query_version: Arc<AtomicU64>,
    pending_query: Option<(Instant, query::Query)>,
    querying: bool,
    map_tx: mpsc::SyncSender<MapRequest>,
    map_rx: mpsc::Receiver<MapResult>,
    map_version: u64,
    pending_map: Option<MapRequest>,
    map_size: (f32, f32),
    operations: mpsc::Receiver<String>,
    operation_tx: mpsc::Sender<String>,
    sync_path: Option<PathBuf>,
    locate_pending: bool,
    load_rx: Option<mpsc::Receiver<Result<Data, String>>>,
    volumes: Vec<VolumeInfo>,
    volumes_rx: mpsc::Receiver<std::io::Result<Vec<VolumeInfo>>>,
    preferences: storage::Preferences,
    dark: bool,
    show_filter_options: bool,
    show_types: bool,
    show_map: bool,
    status: String,
}
impl DiskApp {
    pub fn new(
        initial: Option<PathBuf>,
        scan_options: ScanOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let arguments: Vec<String> = std::env::args_os()
            .filter_map(|argument| argument.into_string().ok())
            .collect();
        let initial_filter = arguments
            .iter()
            .find_map(|a| a.strip_prefix("--filter="))
            .unwrap_or("");
        let initial_mode = if arguments.iter().any(|a| a == "--files") {
            Mode::Files
        } else {
            Mode::Tree
        };
        let initial_limit = if arguments.iter().any(|a| a == "--all") {
            0
        } else {
            1000
        };
        let preferences = storage::load_preferences();
        let dark = preferences.dark.unwrap_or_else(|| cx.theme().is_dark());
        apply_theme(dark, window, cx);
        let path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("选择磁盘或输入文件夹路径")
                .default_value(
                    initial
                        .as_ref()
                        .map_or(String::new(), |p| p.display().to_string()),
                )
        });
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("文件名或通配符，例如 *.zip")
                .default_value(initial_filter)
        });
        let minimum = cx.new(|cx| InputState::new(window, cx).placeholder("例如 100 MB"));
        let exclude = cx.new(|cx| InputState::new(window, cx).placeholder("例如 *.tmp"));
        let owner = cx.weak_entity();
        let table = cx.new(|inner| {
            TableState::new(Explorer::new(owner.clone()), window, inner)
                .col_resizable(true)
                .col_movable(true)
                .col_selectable(false)
                .loop_selection(false)
        });
        let types = cx.new(|inner| {
            TableState::new(Extensions::new(owner.clone()), window, inner)
                .col_resizable(true)
                .col_movable(false)
                .col_selectable(false)
        });
        let mut subscriptions = Vec::new();
        for input in [&search, &minimum, &exclude] {
            subscriptions.push(cx.subscribe_in(input, window, |app, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    if app.is_filtered(cx) {
                        app.mode = Mode::Files;
                    }
                    app.request_query(cx);
                }
            }));
        }
        subscriptions.push(cx.subscribe_in(&path, window, |app, _, event, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                app.path_editing = false;
                app.start_scan(cx);
            }
        }));
        subscriptions.push(
            cx.subscribe_in(
                &table,
                window,
                |app, state, event, window, cx| match event {
                    TableEvent::SelectRow(row) => {
                        let id = state.read(cx).delegate().rows.get(*row).map(|r| r.id);
                        if let Some(id) = id {
                            app.select(id, cx);
                        }
                    }
                    TableEvent::DoubleClickedRow(row) => {
                        let id = state.read(cx).delegate().rows.get(*row).map(|r| r.id);
                        if let Some(id) = id {
                            if app
                                .data
                                .as_ref()
                                .is_some_and(|d| d.snapshot.nodes[id].is_dir)
                            {
                                app.toggle(id, cx);
                            } else {
                                app.item_action(ItemAction::Open, id, window, cx);
                            }
                        }
                    }
                    TableEvent::ColumnWidthsChanged(widths) => {
                        let widths = widths.clone();
                        state.update(cx, |table, _| {
                            for (c, w) in table.delegate_mut().columns.iter_mut().zip(widths) {
                                c.width = w;
                            }
                        });
                    }
                    _ => {}
                },
            ),
        );
        subscriptions.push(cx.subscribe_in(&types, window, |app, state, event, _, cx| {
            if let TableEvent::SelectRow(row) = event {
                let delegate = state.read(cx).delegate();
                if let Some(index) = &delegate.index
                    && let Some(&id) = delegate.order.get(*row)
                {
                    app.extension = Some(index.extensions[id].extension.clone());
                    app.mode = Mode::Files;
                    app.request_query(cx);
                }
            }
        }));
        let (query_tx, requests) = mpsc::sync_channel::<query::Query>(1);
        let (result_tx, query_rx) = mpsc::channel();
        let query_version = Arc::new(AtomicU64::new(0));
        let generation = query_version.clone();
        std::thread::spawn(move || {
            while let Ok(mut q) = requests.recv() {
                while let Ok(next) = requests.try_recv() {
                    q = next;
                }
                if let Some(rows) = query::execute(&q, &generation) {
                    let _ = result_tx.send(QueryResult {
                        version: q.version,
                        rows: Arc::new(rows),
                    });
                }
            }
        });
        let (map_tx, map_requests) = mpsc::sync_channel::<MapRequest>(1);
        let (map_results, map_rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(mut q) = map_requests.recv() {
                while let Ok(next) = map_requests.try_recv() {
                    q = next;
                }
                let tiles = treemap::hierarchical_layout_at_depth(
                    &q.snapshot,
                    q.root,
                    q.allocated,
                    q.width,
                    q.height,
                    6000,
                    q.depth,
                );
                let _ = map_results.send(MapResult {
                    version: q.version,
                    tiles,
                });
            }
        });
        let (operation_tx, operations) = mpsc::channel();
        let (vol_tx, volumes_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = vol_tx.send(disk_core::list_volumes());
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.spawn(async move |app, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(80))
                    .await;
                if app.update(cx, |app, cx| app.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let mut app = Self {
            focus,
            map_focus: cx.focus_handle(),
            sidebar_tab: SidebarTab::Details,
            map_only: false,
            map_depth: 3,
            motion_enabled: !cx.reduce_motion(),
            path_editing: false,
            history: History::default(),
            pending_scroll: None,
            marked: HashSet::new(),
            mark_plan: Arc::new(MarkPlan::default()),
            cleanup_busy: false,
            cleanup_rx: None,
            path,
            search,
            minimum,
            exclude,
            _subscriptions: subscriptions,
            table,
            types,
            data: None,
            expanded: HashSet::from([0]),
            selected: 0,
            scope: 0,
            map_root: 0,
            mode: initial_mode,
            metric: Metric::Allocated,
            extension: None,
            sort_key: SortKey::Allocated,
            descending: true,
            limit: initial_limit,
            match_paths: false,
            include_folders: false,
            duplicates: false,
            rows: Arc::new(vec![]),
            tiles: vec![],
            job: None,
            folder_picker_open: false,
            folder_picker_task: None,
            scan_options,
            #[cfg(windows)]
            elevation_rx: None,
            scan_strategy: match arguments.iter().find_map(|a| a.strip_prefix("--method=")) {
                Some("mft") => ScanStrategy::NtfsMft,
                Some("directory") => ScanStrategy::Directory,
                None if cfg!(windows) && preferences.prefer_mft => ScanStrategy::NtfsMft,
                _ => ScanStrategy::Auto,
            },
            query_tx,
            query_rx,
            query_version,
            pending_query: None,
            querying: false,
            map_tx,
            map_rx,
            map_version: 0,
            pending_map: None,
            map_size: (1200., 340.),
            operations,
            operation_tx,
            sync_path: None,
            locate_pending: false,
            load_rx: None,
            volumes: vec![],
            volumes_rx,
            preferences,
            dark,
            show_filter_options: false,
            show_types: true,
            show_map: true,
            status: "选择磁盘或文件夹，查看目录树、文件类型和空间分布".into(),
        };
        #[cfg(feature = "dialog-smoke")]
        if arguments
            .iter()
            .any(|argument| argument == "--smoke-folder-dialog")
        {
            Self::schedule_folder_dialog_smoke(window, cx);
        }
        #[cfg(feature = "interaction-smoke")]
        if arguments
            .iter()
            .any(|argument| argument == "--smoke-interactions")
        {
            Self::schedule_interaction_smoke(window, cx);
        }
        #[cfg(feature = "interaction-smoke")]
        if arguments
            .iter()
            .any(|argument| argument == "--smoke-scan-reset")
        {
            Self::schedule_scan_reset_smoke(window, cx);
        }
        #[cfg(feature = "appearance-smoke")]
        if arguments.iter().any(|a| a == "--smoke-appearance") {
            Self::schedule_appearance_smoke(window, cx);
        }
        if let Some(path) = arguments.iter().find_map(|a| a.strip_prefix("--load=")) {
            app.load_file(PathBuf::from(path), cx);
        } else if initial.is_some() {
            app.start_scan(cx);
        }
        app
    }
    fn elevation_pending(&self) -> bool {
        #[cfg(windows)]
        {
            self.elevation_rx.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
    fn start_scan(&mut self, cx: &mut Context<Self>) {
        if self.job.is_some() || self.cleanup_busy || self.elevation_pending() {
            return;
        }
        let path = PathBuf::from(self.path.read(cx).value().as_str());
        if path.as_os_str().is_empty() {
            self.status = "请先选择分析目标".into();
            cx.notify();
            return;
        }
        #[cfg(windows)]
        if self.scan_strategy == ScanStrategy::NtfsMft {
            match crate::elevation::is_elevated() {
                Ok(false) => {
                    let (tx, rx) = mpsc::channel();
                    let workers = self.scan_options.workers;
                    self.elevation_rx = Some(rx);
                    self.status = "正在准备管理员扫描，将在新窗口扫描所选文件夹…".into();
                    std::thread::spawn(move || {
                        let result = std::fs::canonicalize(&path)
                            .and_then(|path| {
                                crate::elevation::is_local_ntfs_directory(&path).and_then(
                                    |supported| {
                                        if supported {
                                            crate::elevation::launch_mft_scan(&path, workers)
                                        } else {
                                            Err(std::io::Error::new(
                                                std::io::ErrorKind::Unsupported,
                                                "MFT 仅适用于本地 NTFS 文件夹，请选择目录扫描",
                                            ))
                                        }
                                    },
                                )
                            })
                            .map_err(|error| {
                                if error.raw_os_error() == Some(1223) {
                                    "已取消管理员授权，原有结果已保留".to_string()
                                } else {
                                    format!("无法启动管理员扫描：{error}")
                                }
                            });
                        let _ = tx.send(result);
                    });
                    cx.notify();
                    return;
                }
                Ok(true) => {}
                Err(error) => {
                    self.status = format!("无法检查管理员权限：{error}");
                    cx.notify();
                    return;
                }
            }
        }
        self.clear_scan_results(cx);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Progress::default());
        let (worker_cancel, worker_progress) = (cancel.clone(), progress.clone());
        let (tx, rx) = mpsc::channel();
        let strategy = self.scan_strategy;
        let options = self.scan_options.clone();
        std::thread::spawn(move || {
            if std::env::var_os("WIZTREE_DIAGNOSTICS").is_some() {
                eprintln!(
                    "[scan-start] workers={} strategy={strategy:?}",
                    options.workers
                );
            }
            let result = disk_core::scan_with_strategy(
                &path,
                &options,
                &worker_cancel,
                &worker_progress,
                strategy,
            )
            .map(|s| {
                let indexing_started = Instant::now();
                let index = Arc::new(AnalysisIndex::new(&s));
                let index_elapsed = indexing_started.elapsed();
                if let Err(error) = storage::save_scan_diagnostics(
                    &s, options.workers, worker_progress.snapshot(), index_elapsed, strategy,
                ) {
                    eprintln!("[diagnostics] {error}");
                }
                if std::env::var_os("WIZTREE_DIAGNOSTICS").is_some() {
                    eprintln!(
                        "[scan] method={} files={} dirs={} errors={} io_ms={:.3} index_ms={:.3}",
                        s.method,
                        s.files,
                        s.dirs,
                        s.errors,
                        s.elapsed.as_secs_f64() * 1000.,
                        index_elapsed.as_secs_f64() * 1000.
                    );
                    let io = worker_progress.snapshot().directory_io;
                    eprintln!("[directory-io] opens={} absolute_opens={} queries={} open_worker_ms={:.3} query_worker_ms={:.3} special_files={} special_worker_ms={:.3} portable_fallbacks={}",
                        io.opens, io.absolute_opens, io.queries, io.open_ns as f64 / 1e6,
                        io.query_ns as f64 / 1e6, io.special_files, io.special_ns as f64 / 1e6,
                        io.portable_fallbacks);
                }
                Data {
                    source: None,
                    snapshot: Arc::new(s),
                    index,
                }
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        self.job = Some(ScanJob {
            cancel,
            progress,
            rx,
            started: Instant::now(),
            diagnostic_tick: 0,
        });
        self.status = "正在读取文件元数据…".into();
        cx.notify();
    }
    /// A new scan owns the result area immediately. Invalidate work before
    /// dropping its snapshot so late background replies cannot restore old rows.
    #[cfg(windows)]
    fn poll_elevation(&mut self, cx: &mut Context<Self>) {
        let result = self
            .elevation_rx
            .as_ref()
            .and_then(|rx| match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("管理员启动任务已停止，原有结果已保留".into()))
                }
            });
        if let Some(result) = result {
            self.elevation_rx = None;
            match result {
                Ok(()) => {
                    self.clear_scan_results(cx);
                    self.status = "已在管理员窗口开始 MFT 扫描；此窗口仍保留，可关闭".into();
                }
                Err(error) => self.status = error,
            }
            cx.notify();
        }
    }
    fn clear_scan_results(&mut self, cx: &mut Context<Self>) {
        self.query_version.fetch_add(1, Ordering::Relaxed);
        self.map_version += 1;
        self.pending_query = None;
        self.pending_map = None;
        self.querying = false;
        self.load_rx = None;
        self.sync_path = None;
        self.data = None;
        self.rows = Arc::new(vec![]);
        self.tiles.clear();
        self.selected = 0;
        self.scope = 0;
        self.map_root = 0;
        self.expanded.clear();
        self.extension = None;
        self.marked.clear();
        self.mark_plan = Arc::new(MarkPlan::default());
        self.history = History::default();
        self.pending_scroll = None;
        self.locate_pending = false;
        self.path_editing = false;
        self.update_table(cx);
        self.table.update(cx, |table, cx| {
            table.clear_selection(cx);
            table
                .vertical_scroll_handle
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(0.)));
        });
        self.types.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.index = None;
            delegate.order.clear();
            table.clear_selection(cx);
            table.refresh(cx);
            table
                .vertical_scroll_handle
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), px(0.)));
        });
    }
    fn poll(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        self.poll_elevation(cx);
        self.poll_cleanup(cx);
        if let Some(job) = &mut self.job {
            if std::env::var_os("WIZTREE_DIAGNOSTICS").is_some() {
                let tick = job.started.elapsed().as_secs() / 2 + 1;
                if tick != job.diagnostic_tick {
                    job.diagnostic_tick = tick;
                    let p = job.progress.snapshot();
                    let io = p.directory_io;
                    eprintln!(
                        "[scan-progress] elapsed_ms={:.3} files={} dirs={} opens={} queries={} enumerate_ms={:.3} publish_wait_ms={:.3} publish_ms={:.3} allocate_ms={:.3} job_drop_ms={:.3} worker_start_ms={:.3}",
                        job.started.elapsed().as_secs_f64() * 1000.,
                        p.files,
                        p.dirs,
                        io.opens,
                        io.queries,
                        io.enumerate_ns as f64 / 1e6,
                        io.publish_wait_ns as f64 / 1e6,
                        io.publish_ns as f64 / 1e6,
                        io.entry_allocate_ns as f64 / 1e6,
                        io.job_drop_ns as f64 / 1e6,
                        io.worker_start_delay_ns as f64 / 1e6
                    );
                }
            }
            match job.rx.try_recv() {
                Ok(result) => {
                    self.job = None;
                    match result {
                        Ok(data) => self.install(data, cx),
                        Err(e) => {
                            self.status = format!("扫描失败：{e}");
                            cx.notify();
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.status = "扫描线程已停止".into();
                    cx.notify();
                }
                Err(mpsc::TryRecvError::Empty) => cx.notify(),
            }
        }
        if let Some(rx) = &self.load_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.load_rx = None;
                    match result {
                        Ok(data) => self.install(data, cx),
                        Err(e) => self.status = format!("载入失败：{e}"),
                    }
                    cx.notify();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.load_rx = None;
                }
                _ => {}
            }
        }
        if self
            .pending_query
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() >= Duration::from_millis(100))
        {
            let (_, q) = self.pending_query.take().unwrap();
            if let Err(mpsc::TrySendError::Full(q)) = self.query_tx.try_send(q) {
                self.pending_query = Some((Instant::now(), q));
            }
        }
        while let Ok(result) = self.query_rx.try_recv() {
            if result.version == self.query_version.load(Ordering::Relaxed) {
                self.rows = result.rows;
                self.querying = false;
                self.update_table(cx);
                if let Some(offset) = self.pending_scroll.take() {
                    let row = ((-f32::from(offset.y) / 28.).max(0.) as usize)
                        .min(self.rows.len().saturating_sub(1));
                    let handle = &self.table.read(cx).vertical_scroll_handle;
                    handle.scroll_to_item_strict(row, ScrollStrategy::Top);
                } else if self.locate_pending {
                    self.locate_pending = false;
                    if let Some(row) = self.rows.iter().position(|r| r.id == self.selected) {
                        self.table
                            .update(cx, |table, cx| table.scroll_to_row(row, cx));
                    }
                }
            }
        }
        if let Some(q) = self.pending_map.take()
            && let Err(mpsc::TrySendError::Full(q)) = self.map_tx.try_send(q)
        {
            self.pending_map = Some(q);
        }
        while let Ok(result) = self.map_rx.try_recv() {
            if result.version == self.map_version {
                self.tiles = result.tiles;
                cx.notify();
            }
        }
        while let Ok(message) = self.operations.try_recv() {
            self.status = message;
            cx.notify();
        }
        if let Ok(result) = self.volumes_rx.try_recv() {
            match result {
                Ok(v) => self.volumes = v,
                Err(e) => self.status = format!("磁盘列表读取失败：{e}"),
            }
            cx.notify();
        }
    }
    fn install(&mut self, data: Data, cx: &mut Context<Self>) {
        let s = &data.snapshot;
        self.status = format!(
            "{} · {:.2} 秒 · {} 文件 · {} 文件夹 · {} 错误 · {}",
            if data.source.is_some() {
                "已载入离线结果"
            } else if s.cancelled {
                "已停止，结果不完整"
            } else {
                "扫描完成"
            },
            s.elapsed.as_secs_f64(),
            number(s.files),
            number(data.index.nodes[0].dirs),
            s.errors,
            if data.source.is_some() {
                "快照 / CSV"
            } else if s.method == disk_core::ScanMethod::NtfsMft {
                "MFT"
            } else {
                "目录扫描"
            }
        );
        if let Some(reason) = &s.fallback_reason {
            self.status.push_str(&format!(" · {reason}"));
        }
        self.sync_path = Some(s.root.clone());
        self.preferences.recent.retain(|p| p != &s.root);
        self.preferences.recent.insert(0, s.root.clone());
        self.preferences.recent.truncate(8);
        let prefs = self.preferences.clone();
        std::thread::spawn(move || {
            let _ = storage::save_preferences(&prefs);
        });
        let index = data.index.clone();
        self.types.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.order = (0..index.extensions.len()).collect();
            delegate
                .order
                .sort_unstable_by_key(|&i| std::cmp::Reverse(index.extensions[i].allocated));
            delegate.index = Some(index);
            table.refresh(cx);
            cx.notify();
        });
        self.data = Some(data);
        self.scope = 0;
        self.map_root = 0;
        self.selected = 0;
        self.marked.clear();
        self.mark_plan = Arc::new(MarkPlan::default());
        self.pending_scroll = None;
        self.locate_pending = false;
        self.path_editing = false;
        self.expanded = HashSet::from([0]);
        if std::env::args().any(|a| a == "--expand=2") {
            self.expanded.extend(
                self.data.as_ref().unwrap().snapshot.nodes[0]
                    .children
                    .iter()
                    .copied(),
            );
        }
        self.extension = None;
        self.history.reset(self.current_location(cx));
        self.rows = Arc::new(vec![]);
        self.tiles.clear();
        self.update_table(cx);
        self.request_query(cx);
        self.request_map(cx);
    }
    fn update_table(&mut self, cx: &mut Context<Self>) {
        let data = self.data.as_ref();
        let rows = self.rows.clone();
        let expanded = self.expanded.clone();
        let mode = if self.is_filtered(cx) {
            Mode::Files
        } else {
            self.mode
        };
        let selected = self.selected;
        let scope = self.scope;
        let marked = self.marked.clone();
        self.table.update(cx, |table, cx| {
            let delegate = table.delegate_mut();
            delegate.set_mode(mode);
            delegate.snapshot = data.map(|d| d.snapshot.clone());
            delegate.index = data.map(|d| d.index.clone());
            delegate.rows = rows;
            delegate.expanded = expanded;
            delegate.selected = selected;
            delegate.scope = scope;
            delegate.marked = marked;
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }
    fn is_filtered(&self, cx: &App) -> bool {
        !self.search.read(cx).value().is_empty()
            || self.extension.is_some()
            || !self.minimum.read(cx).value().is_empty()
            || !self.exclude.read(cx).value().is_empty()
            || self.duplicates
    }
    fn request_query(&mut self, cx: &mut Context<Self>) {
        let Some(data) = &self.data else {
            return;
        };
        let version = self.query_version.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending_query = Some((
            Instant::now(),
            query::Query {
                version,
                snapshot: data.snapshot.clone(),
                index: data.index.clone(),
                root: self.scope,
                mode: self.mode,
                expanded: self.expanded.clone(),
                query: self.search.read(cx).value().to_string(),
                extension: self.extension.clone(),
                minimum: parse_size(self.minimum.read(cx).value().as_str()),
                sort: self.sort_key,
                descending: self.descending,
                limit: self.limit,
                match_paths: self.match_paths,
                include_folders: self.include_folders,
                exclude: self.exclude.read(cx).value().to_string(),
                duplicates: self.duplicates,
            },
        ));
        self.querying = true;
        cx.notify();
    }
    fn request_map(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = &self.data {
            self.map_version += 1;
            self.pending_map = Some(MapRequest {
                version: self.map_version,
                snapshot: data.snapshot.clone(),
                root: self.map_root,
                allocated: self.metric == Metric::Allocated,
                width: self.map_size.0,
                height: self.map_size.1,
                depth: self.map_depth,
            });
            cx.notify();
        }
    }
    pub fn sort(&mut self, key: SortKey, descending: bool, cx: &mut Context<Self>) {
        self.sort_key = key;
        self.descending = descending;
        self.request_query(cx);
    }
    pub fn toggle(&mut self, id: usize, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        self.request_query(cx);
    }
    fn select(&mut self, id: usize, cx: &mut Context<Self>) {
        self.selected = id;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().selected = id;
            cx.notify();
        });
        cx.notify();
    }
    fn locate(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        self.locate_pending = true;
        if let Some(data) = &self.data {
            let mut node = id;
            while let Some(parent) = data.snapshot.nodes[node].parent {
                self.expanded.insert(parent);
                node = parent;
            }
        }
        self.mode = Mode::Tree;
        self.extension = None;
        self.search.update(cx, |s, cx| s.set_value("", window, cx));
        self.minimum.update(cx, |s, cx| s.set_value("", window, cx));
        self.exclude.update(cx, |s, cx| s.set_value("", window, cx));
        self.duplicates = false;
        self.request_query(cx);
    }
}
impl Drop for DiskApp {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }
}
include!("app_actions.rs");
include!("app_view.rs");

#[cfg(feature = "dialog-smoke")]
include!("dialog_smoke.rs");

include!("interaction_actions.rs");
include!("inspector.rs");

#[cfg(feature = "interaction-smoke")]
include!("interaction_smoke.rs");

#[cfg(feature = "interaction-smoke")]
include!("scan_reset_smoke.rs");

#[cfg(feature = "appearance-smoke")]
include!("appearance_smoke.rs");
