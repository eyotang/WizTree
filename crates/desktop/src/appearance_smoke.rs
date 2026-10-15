// Render the actual native GPUI scene, not a separate mockup.
impl DiskApp {
    fn schedule_appearance_smoke(window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |app, cx| {
            if std::env::args().any(|argument| argument == "--readme-preview") {
                assert!(
                    std::env::var_os("WIZTREE_STATE_DIR").is_some(),
                    "demo capture requires isolated preferences"
                );
                app.update_in(cx, |app, window, cx| {
                    let snapshot = Arc::new(readme_demo_snapshot());
                    let index = Arc::new(AnalysisIndex::new(&snapshot));
                    // Never use the host's real drive names, capacities or file paths.
                    let (_, receiver) = mpsc::channel();
                    app.volumes_rx = receiver;
                    app.volumes = vec![VolumeInfo {
                        root: snapshot.root.clone(),
                        label: "Demo".into(),
                        total: 512 * 1024 * 1024 * 1024,
                        free: 192 * 1024 * 1024 * 1024,
                        filesystem: None,
                    }];
                    let media = snapshot
                        .nodes
                        .iter()
                        .position(|node| node.name == "Media")
                        .unwrap();
                    app.install(
                        Data {
                            source: None,
                            snapshot,
                            index,
                        },
                        cx,
                    );
                    app.expanded.insert(media);
                    app.path
                        .update(cx, |input, cx| input.set_value("Demo Disk", window, cx));
                    app.status = "演示数据 · 不对应实际磁盘文件".into();
                    app.request_query(cx);
                })
                .unwrap();
            }
            for _ in 0..200 {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if app
                    .update_in(cx, |app, _, _| {
                        app.data.is_some()
                            && app.job.is_none()
                            && !app.querying
                            && !app.tiles.is_empty()
                    })
                    .unwrap()
                {
                    break;
                }
            }
            for (name, dark, expanded, tab) in [
                ("light-overview", false, false, SidebarTab::Details),
                ("dark-overview", true, false, SidebarTab::Details),
                ("light-filters", false, true, SidebarTab::Types),
                ("dark-filters", true, true, SidebarTab::Review),
                ("narrow-overview", false, false, SidebarTab::Details),
            ] {
                app.update_in(cx, |app, window, cx| {
                    assert!(
                        app.data.is_some(),
                        "fixture must complete before visual capture"
                    );
                    window.resize(if name == "narrow-overview" {
                        size(px(1060.), px(760.))
                    } else {
                        size(px(1380.), px(920.))
                    });
                    app.dark = dark;
                    apply_theme(dark, window, cx);
                    app.show_filter_options = expanded;
                    app.match_paths = expanded;
                    app.include_folders = expanded;
                    app.sidebar_tab = tab;
                    app.mode = if expanded { Mode::Files } else { Mode::Tree };
                    if app.marked.is_empty()
                        && let Some(id) = app.rows.iter().find(|r| r.id != 0).map(|r| r.id)
                    {
                        app.select(id, cx);
                        if expanded {
                            app.toggle_mark(id, cx);
                        }
                    }
                    app.request_query(cx);
                    cx.notify();
                })
                .unwrap();
                cx.background_executor()
                    .timer(Duration::from_millis(850))
                    .await;
                app.update_in(cx, |_, window, _| {
                    let directory = PathBuf::from(
                        std::env::var_os("WIZTREE_SMOKE_OUTPUT").expect("visual output directory"),
                    );
                    let image = window.render_to_image().expect("native scene readback");
                    let file =
                        std::fs::File::create(directory.join(format!("{name}.png"))).unwrap();
                    let mut encoder = png::Encoder::new(
                        std::io::BufWriter::new(file),
                        image.width(),
                        image.height(),
                    );
                    encoder.set_color(png::ColorType::Rgba);
                    encoder.set_depth(png::BitDepth::Eight);
                    encoder
                        .write_header()
                        .unwrap()
                        .write_image_data(image.as_raw())
                        .unwrap();
                    eprintln!("[appearance-smoke] captured {name}");
                })
                .unwrap();
            }
            app.update_in(cx, |_, window, _| window.remove_window())
                .unwrap();
        })
        .detach();
    }
}

/// Fixed, in-memory example content. No directories are enumerated to create it.
fn readme_demo_snapshot() -> Snapshot {
    const GIB: u64 = 1024 * 1024 * 1024;
    let groups: &[(&str, &[(&str, u64)])] = &[
        (
            "Projects",
            &[
                ("Source.zip", 3 * GIB),
                ("Assets.zip", 11 * GIB),
                ("Build.bin", 8 * GIB),
            ],
        ),
        (
            "Media",
            &[
                ("Showreel.mp4", 24 * GIB),
                ("Photos.png", 12 * GIB),
                ("Illustrations.svg", 2 * GIB),
            ],
        ),
        (
            "Documents",
            &[
                ("Design.pdf", 3 * GIB),
                ("Notes.md", GIB / 8),
                ("Archive.zip", 6 * GIB),
            ],
        ),
        (
            "Downloads",
            &[
                ("Release.zip", 6 * GIB),
                ("Samples.mp4", 4 * GIB),
                ("Dataset.bin", 2 * GIB),
            ],
        ),
        (
            "Caches",
            &[("BuildCache.bin", 5 * GIB), ("Thumbnails.bin", 2 * GIB)],
        ),
    ];
    let node = |name: &str, parent, is_dir, size| disk_core::Node {
        parent,
        name: name.into(),
        size,
        allocated: size,
        is_dir,
        modified: None,
        children: Vec::new(),
    };
    let mut nodes = vec![node("Demo Disk", None, true, 0)];
    let mut files = 0;
    for (directory, entries) in groups {
        let id = nodes.len();
        nodes.push(node(directory, Some(0), true, 0));
        nodes[0].children.push(id);
        for (name, bytes) in *entries {
            let child = nodes.len();
            nodes.push(node(name, Some(id), false, *bytes));
            nodes[id].children.push(child);
            nodes[id].size += bytes;
            nodes[id].allocated += bytes;
            files += 1;
        }
        nodes[0].size += nodes[id].size;
        nodes[0].allocated += nodes[id].allocated;
    }
    Snapshot {
        root: PathBuf::from("Demo Disk"),
        nodes,
        elapsed: Duration::ZERO,
        files,
        dirs: groups.len() as u64 + 1,
        errors: 0,
        cancelled: false,
        method: disk_core::ScanMethod::ParallelWalk,
        fallback_reason: None,
        issues: vec![],
    }
}
