#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
extern crate gpui_kit as gpui;
mod app;
mod image_export;
mod interaction;
mod query;
mod storage;
mod tables;
mod treemap;
use gpui_kit::*;
fn main() {
    // Resolve hardware concurrency before platform initialization/thread affinity
    // changes, so UI and CLI use the same default I/O worker budget.
    let mut scan_options = disk_core::ScanOptions::default();
    if let Some(workers) = std::env::args()
        .find_map(|arg| {
            arg.strip_prefix("--workers=")
                .and_then(|v| v.parse::<usize>().ok())
        })
        .filter(|v| (1..=64).contains(v))
    {
        scan_options.workers = workers;
    }
    let pool_started = std::time::Instant::now();
    if let Err(error) = disk_core::prepare_scan_threads(&scan_options) {
        eprintln!("无法初始化扫描线程：{error}");
    }
    if std::env::var_os("WIZTREE_DIAGNOSTICS").is_some() {
        eprintln!(
            "[startup] scan_pool_ms={:.3}",
            pool_started.elapsed().as_secs_f64() * 1000.
        );
    }
    let initial = std::env::args_os()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with("--"))
        .map(std::path::PathBuf::from);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            cx.bind_keys([
                KeyBinding::new("alt-left", app::GoBack, Some("DiskApp")),
                KeyBinding::new("alt-right", app::GoForward, Some("DiskApp")),
                KeyBinding::new("alt-up", app::GoUp, Some("DiskApp")),
                KeyBinding::new("backspace", app::GoUp, Some("DataTable")),
                KeyBinding::new("backspace", app::GoUp, Some("DiskMap")),
                KeyBinding::new("secondary-shift-m", app::ReviewMarks, Some("DiskApp")),
                KeyBinding::new("secondary-l", app::EditLocation, Some("DiskApp")),
                KeyBinding::new("enter", app::OpenSelected, Some("DiskApp")),
                KeyBinding::new("up", app::PreviousRow, Some("DiskApp")),
                KeyBinding::new("down", app::NextRow, Some("DiskApp")),
                KeyBinding::new("secondary-f", app::Find, Some("DiskApp")),
                KeyBinding::new("secondary-o", app::ChooseFolder, Some("DiskApp")),
                KeyBinding::new("secondary-r", app::Rescan, Some("DiskApp")),
                KeyBinding::new("right", app::ExpandSelected, Some("DataTable")),
                KeyBinding::new("left", app::CollapseSelected, Some("DataTable")),
                KeyBinding::new("secondary-c", app::CopySelected, Some("DiskApp")),
                KeyBinding::new("secondary-e", app::RevealSelected, Some("DiskApp")),
                KeyBinding::new("delete", app::TrashSelected, Some("DataTable")),
                KeyBinding::new("escape", app::CancelScan, Some("DiskApp")),
            ]);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1380.), px(920.)),
                    cx,
                ))),
                window_min_size: Some(size(px(1060.), px(720.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("WizTree · 快速空间分析".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| app::DiskApp::new(initial.clone(), scan_options.clone(), window, cx))
            })
            .expect("无法创建桌面窗口");
            #[cfg(debug_assertions)]
            eprintln!("[startup] GPUI window created");
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
