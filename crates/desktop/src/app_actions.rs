impl DiskApp {
    fn choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.folder_picker_open
            || self.job.is_some()
            || self.cleanup_busy
            || self.elevation_pending()
        {
            return;
        }
        // A synchronous native modal loop can re-enter GPUI while this entity
        // is borrowed by the button callback. The platform picker returns a
        // receiver immediately and keeps the modal loop outside this entity borrow.
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择分析文件夹".into()),
        });
        self.folder_picker_open = true;
        self.folder_picker_task = Some(cx.spawn_in(window, async move |app, cx| {
            let result = match selection.await {
                Ok(result) => result.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            // The window/entity may have been closed while the picker was open.
            let _ = app.update_in(cx, |app, window, cx| {
                app.finish_folder_choice(result, window, cx);
            });
        }));
        cx.notify();
    }
    fn finish_folder_choice(
        &mut self,
        result: Result<Option<Vec<PathBuf>>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.folder_picker_open = false;
        match result {
            Ok(Some(paths)) => {
                if let Some(path) = paths.into_iter().next() {
                    self.set_path(path, window, cx);
                }
            }
            Ok(None) => {}
            Err(error) => self.status = format!("无法选择文件夹：{error}"),
        }
        cx.notify();
    }
    fn set_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.path
            .update(cx, |s, cx| s.set_value(pretty_path(&path), window, cx));
        self.start_scan(cx);
    }
    pub fn item_action(
        &mut self,
        action: ItemAction,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = &self.data else {
            return;
        };
        let path = data.snapshot.path(id);
        match action {
            ItemAction::Copy => {
                cx.write_to_clipboard(ClipboardItem::new_string(pretty_path(&path)));
                self.status = "已复制完整路径".into();
            }
            ItemAction::Reveal => cx.reveal_path(&path),
            ItemAction::Open => {
                if data.snapshot.nodes[id].is_dir {
                    self.item_action(ItemAction::Zoom, id, window, cx);
                } else {
                    let tx = self.operation_tx.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = open_path(&path) {
                            let _ = tx.send(format!("打开失败：{e}"));
                        }
                    });
                }
            }
            ItemAction::Zoom => {
                let root = if data.snapshot.nodes[id].is_dir {
                    id
                } else {
                    data.snapshot.nodes[id].parent.unwrap_or(0)
                };
                self.navigate_to(root, window, cx);
            }
            ItemAction::Mark => self.toggle_mark(id, cx),
            ItemAction::Locate => self.locate(id, window, cx),
            ItemAction::Export => {
                let mut ids = vec![];
                let mut stack = vec![id];
                while let Some(id) = stack.pop() {
                    ids.push(id);
                    stack.extend(data.snapshot.nodes[id].children.iter().copied());
                }
                self.export(Some(ids), cx);
            }
            ItemAction::Trash => self.confirm_trash(vec![id], window, cx),
        }
        cx.notify();
    }
    fn export(&mut self, ids: Option<Vec<usize>>, cx: &mut Context<Self>) {
        if let Some(data) = &self.data
            && let Some(path) = rfd::FileDialog::new()
                .set_file_name("空间分析.csv")
                .add_filter("CSV", &["csv"])
                .save_file()
        {
            let s = data.snapshot.clone();
            let tx = self.operation_tx.clone();
            std::thread::spawn(move || {
                let r = storage::export_csv(&s, ids.as_deref(), &path);
                let _ = tx.send(match r {
                    Ok(()) => format!("CSV 已保存：{}", path.display()),
                    Err(e) => format!("导出失败：{e}"),
                });
            });
            cx.notify();
        }
    }
    fn export_image(&mut self, _cx: &mut Context<Self>) {
        if let Some(data) = &self.data
            && let Some(path) = rfd::FileDialog::new()
                .set_file_name("空间分布.png")
                .add_filter("PNG 图片", &["png"])
                .save_file()
        {
            let snapshot = data.snapshot.clone();
            let root = self.map_root;
            let allocated = self.metric == Metric::Allocated;
            let tx = self.operation_tx.clone();
            std::thread::spawn(move || {
                let result =
                    crate::image_export::save_png(&snapshot, root, allocated, &path, 1600, 900);
                let _ = tx.send(match result {
                    Ok(()) => format!("空间图已保存：{}", path.display()),
                    Err(e) => format!("图片导出失败：{e}"),
                });
            });
        }
    }
    fn save_snapshot(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = &self.data
            && let Some(path) = rfd::FileDialog::new()
                .set_file_name("扫描快照.wtree.json")
                .add_filter("扫描快照", &["json"])
                .save_file()
        {
            let s = data.snapshot.clone();
            let tx = self.operation_tx.clone();
            std::thread::spawn(move || {
                let r = storage::save(&s, &path);
                let _ = tx.send(match r {
                    Ok(()) => format!("快照已保存：{}", path.display()),
                    Err(e) => format!("保存失败：{e}"),
                });
            });
            cx.notify();
        }
    }
    fn load_snapshot(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("扫描快照或 WizTree CSV", &["json", "csv"])
            .pick_file()
        {
            self.load_file(path, cx);
        }
    }
    fn load_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = mpsc::channel();
        self.load_rx = Some(rx);
        self.status = "正在载入快照…".into();
        std::thread::spawn(move || {
            let loaded = if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"))
            {
                storage::load_csv(&path)
            } else {
                storage::load(&path)
            };
            let result = loaded
                .map(|s| {
                    let index = Arc::new(AnalysisIndex::new(&s));
                    Data {
                        source: Some(path.clone()),
                        snapshot: Arc::new(s),
                        index,
                    }
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });
        cx.notify();
    }
    fn clear_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.search, &self.minimum, &self.exclude] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.extension = None;
        self.duplicates = false;
        self.match_paths = false;
        self.include_folders = false;
        self.request_query(cx);
    }
    fn go_up(&mut self, _: &GoUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self
            .data
            .as_ref()
            .and_then(|data| data.snapshot.nodes[self.scope].parent)
        {
            self.navigate_to(parent, window, cx);
        } else if self.is_filtered(cx) {
            self.clear_filter(window, cx);
        }
    }
    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.item_action(ItemAction::Open, self.selected, window, cx);
    }
    fn previous(&mut self, _: &PreviousRow, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }
    fn next(&mut self, _: &NextRow, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        let row = self
            .rows
            .iter()
            .position(|r| r.id == self.selected)
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.rows.len() - 1);
        self.table.update(cx, |t, cx| t.set_selected_row(row, cx));
    }
    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        // Search belongs to the table surface, so reveal it before moving focus.
        self.map_only = false;
        self.search.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }
    fn choose_action(&mut self, _: &ChooseFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.choose(window, cx);
    }
    fn rescan(&mut self, _: &Rescan, _: &mut Window, cx: &mut Context<Self>) {
        self.start_scan(cx);
    }
    fn cancel(&mut self, _: &CancelScan, _: &mut Window, cx: &mut Context<Self>) {
        if self.path_editing {
            self.path_editing = false;
            cx.notify();
            return;
        }
        if self.sidebar_tab == SidebarTab::Review {
            self.sidebar_tab = SidebarTab::Details;
            cx.notify();
            return;
        }
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
            self.status = "正在停止…".into();
            cx.notify();
        }
    }
    fn expand(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.expanded.insert(self.selected);
        self.request_query(cx);
    }
    fn collapse(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        if !self.expanded.remove(&self.selected)
            && let Some(id) = self
                .data
                .as_ref()
                .and_then(|d| d.snapshot.nodes[self.selected].parent)
        {
            self.select(id, cx);
        }
        self.request_query(cx);
    }
    fn copy(&mut self, _: &CopySelected, w: &mut Window, cx: &mut Context<Self>) {
        self.item_action(ItemAction::Copy, self.selected, w, cx);
    }
    fn reveal(&mut self, _: &RevealSelected, w: &mut Window, cx: &mut Context<Self>) {
        self.item_action(ItemAction::Reveal, self.selected, w, cx);
    }
    fn trash(&mut self, _: &TrashSelected, w: &mut Window, cx: &mut Context<Self>) {
        self.item_action(ItemAction::Trash, self.selected, w, cx);
    }
    fn method_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let strategy = self.scan_strategy;
        quiet_button("scan-method", cx)
            .disabled(self.job.is_some() || self.cleanup_busy || self.elevation_pending())
            .label(match strategy {
                ScanStrategy::Auto => "自动",
                ScanStrategy::Directory => "目录",
                ScanStrategy::NtfsMft => "MFT",
            })
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for &(label, method) in scan_methods() {
                    let owner = owner.clone();
                    menu = menu.item(
                        PopupMenuItem::new(if method == strategy {
                            format!("✓ {label}")
                        } else {
                            label.to_owned()
                        })
                        .on_click(move |_, _, cx| {
                            let _ = owner.update(cx, |app, cx| {
                                app.scan_strategy = method;
                                #[cfg(windows)]
                                {
                                    app.preferences.prefer_mft = method == ScanStrategy::NtfsMft;
                                    let preferences = app.preferences.clone();
                                    std::thread::spawn(move || { let _ = storage::save_preferences(&preferences); });
                                    if method == ScanStrategy::NtfsMft {
                                        app.status = "已选择 MFT：支持整盘和文件夹，扫描时按需请求管理员权限".into();
                                    }
                                }
                                cx.notify();
                            });
                        }),
                    );
                }
                menu
            })
    }
    fn menu_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let ready = self.data.is_some();
        quiet_button("file-menu", cx)
            .disabled(self.elevation_pending())
            .label("文件")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (label, action) in [
                    ("载入快照 / WizTree CSV…", 0),
                    ("保存扫描快照…", 1),
                    ("导出全部 CSV…", 2),
                    ("导出当前列表 CSV…", 3),
                    ("导出当前空间图 PNG…", 4),
                ] {
                    let owner = owner.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .disabled(action != 0 && !ready)
                            .on_click(move |_, _, cx| {
                                let _ = owner.update(cx, |app, cx| match action {
                                    0 => app.load_snapshot(cx),
                                    1 => app.save_snapshot(cx),
                                    2 => app.export(None, cx),
                                    3 => app
                                        .export(Some(app.rows.iter().map(|r| r.id).collect()), cx),
                                    _ => app.export_image(cx),
                                });
                            }),
                    );
                }
                menu
            })
    }
    fn volumes_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let roots: Vec<_> = self
            .volumes
            .iter()
            .map(|v| {
                (
                    v.root.clone(),
                    format!("{}  {} · 可用 {}", v.root.display(), v.label, bytes(v.free)),
                )
            })
            .collect();
        let recent = self.preferences.recent.clone();
        quiet_button("volumes", cx)
            .icon(IconName::HardDrive)
            .label("磁盘")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (path, label) in &roots {
                    let target = path.clone();
                    let owner = owner.clone();
                    menu = menu.item(PopupMenuItem::new(label.clone()).on_click(
                        move |_, window, cx| {
                            let _ = owner
                                .update(cx, |app, cx| app.set_path(target.clone(), window, cx));
                        },
                    ));
                }
                menu = menu.separator();
                for path in &recent {
                    let target = path.clone();
                    let owner = owner.clone();
                    menu = menu.item(PopupMenuItem::new(pretty_path(path)).on_click(
                        move |_, window, cx| {
                            let _ = owner
                                .update(cx, |app, cx| app.set_path(target.clone(), window, cx));
                        },
                    ));
                }
                menu
            })
    }
    fn treemap(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.weak_entity();
        let mut map = div()
            .id("treemap")
            .key_context("DiskMap")
            .on_key_down(cx.listener(Self::marking_key))
            .track_focus(&self.map_focus)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().muted);
        map = map.child(
            canvas(
                move |bounds, _, cx| {
                    let _ = owner.update(cx, |app, cx| {
                        let dimensions =
                            (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        if (app.map_size.0 - dimensions.0).abs() > 4.
                            || (app.map_size.1 - dimensions.1).abs() > 4.
                        {
                            app.map_size = dimensions;
                            app.request_map(cx);
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
        if let Some(data) = &self.data {
            for tile in self.tiles.iter() {
                let Some(id) = tile.id else {
                    continue;
                };
                let node = &data.snapshot.nodes[id];
                let ext = query::extension(&node.name);
                let color = if node.is_dir {
                    0x8496aa
                } else {
                    query::color(&ext)
                };
                let name = node.name.to_string_lossy().into_owned();
                let dimmed = self
                    .extension
                    .as_ref()
                    .is_some_and(|filter| *filter != ext && !node.is_dir);
                let selected = id == self.selected;
                let marked = interaction::covered(&data.snapshot, &self.marked, id);
                let folder_color = [0x6389b1, 0x8b79ac, 0x5c9b87, 0x5e9ba8, 0xaa8a62, 0xa8788d][node
                    .name
                    .to_string_lossy()
                    .bytes()
                    .fold(0usize, |hash, byte| {
                        hash.wrapping_mul(31).wrapping_add(byte as usize)
                    })
                    % 6];
                let color = map_tint(
                    if marked {
                        0xd38740
                    } else if node.is_dir {
                        folder_color
                    } else {
                        color
                    },
                    self.dark,
                );
                let tip = format!(
                    "{}\n大小 {} · 实际占用 {}",
                    pretty_path(&data.snapshot.path(id)),
                    bytes(node.size),
                    bytes(node.allocated)
                );
                map = map.child(
                    div()
                        .id(("tile", id))
                        .absolute()
                        .left(relative(tile.x))
                        .top(relative(tile.y))
                        .w(relative(tile.w))
                        .h(relative(tile.h))
                        .border(if selected || marked { px(2.) } else { px(1.) })
                        .border_color(if selected {
                            cx.theme().primary
                        } else if marked {
                            cx.theme().warning
                        } else {
                            cx.theme().background
                        })
                        .bg(rgb(color))
                        .rounded(px(4.))
                        .overflow_hidden()
                        .when(dimmed, |d| d.opacity(0.22))
                        .cursor_pointer()
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                .build(window, cx)
                        })
                        .when(
                            tile.w * self.map_size.0 > 85. && tile.h * self.map_size.1 > 30.,
                            |d| {
                                d.p_2()
                                    .text_xs()
                                    .text_color(rgb(if self.dark { 0xf5f6fa } else { 0x23384d }))
                                    .child(div().truncate().child(name))
                                    .child(div().truncate().child(if marked {
                                        format!("{} · 已标记", bytes(tile.bytes))
                                    } else {
                                        bytes(tile.bytes)
                                    }))
                            },
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |app, event: &MouseDownEvent, window, cx| {
                                if event.click_count == 2 {
                                    app.item_action(ItemAction::Zoom, id, window, cx);
                                } else {
                                    app.select_map_node(id, window, cx);
                                }
                            }),
                        ),
                );
            }
        }
        if self.tiles.is_empty() {
            map = map.child(
                div()
                    .absolute()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.data.is_some() {
                        "该目录没有可显示的占用数据"
                    } else {
                        "扫描后在这里查看空间分布"
                    }),
            );
        }
        let map = if self.motion_enabled {
            map.with_animation(
                ("map-enter", self.map_root),
                Animation::new(Duration::from_millis(140)),
                |map, delta| map.opacity(0.88 + 0.12 * delta),
            )
            .into_any_element()
        } else {
            map.into_any_element()
        };
        let label = self.data.as_ref().map_or("空间分布".into(), |d| {
            pretty_path(&d.snapshot.path(self.map_root))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .child(
                div()
                    .h(px(38.))
                    .px_3()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border.opacity(0.6))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .child(
                                Icon::new(IconName::LayoutDashboard)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_sm()
                                    .child("空间分布"),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                Button::new("map-parent")
                                    .ghost()
                                    .small()
                                    .icon(IconName::ArrowUp)
                                    .disabled(self.scope == 0)
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.go_up(&GoUp, window, cx)
                                    })),
                            )
                            .child(
                                quiet_button("map-reset", cx)
                                    .icon(IconName::RotateCw)
                                    .tooltip("重置空间图")
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.map_root = app.scope;
                                        app.request_map(cx);
                                    })),
                            )
                            .child(
                                Button::new("depth-less")
                                    .small()
                                    .ghost()
                                    .compact()
                                    .label("−")
                                    .disabled(self.map_depth == 1)
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.map_depth = app.map_depth.saturating_sub(1).max(1);
                                        app.request_map(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .flex_shrink_0()
                                    .child(format!("{} 层", self.map_depth)),
                            )
                            .child(
                                Button::new("depth-more")
                                    .small()
                                    .ghost()
                                    .compact()
                                    .label("+")
                                    .disabled(self.map_depth == 8)
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.map_depth = (app.map_depth + 1).min(8);
                                        app.request_map(cx);
                                    })),
                            )
                            .child(
                                toggle_button("map-focus-mode", self.map_only, cx)
                                    .rounded(px(999.))
                                    .icon(if self.map_only {
                                        IconName::Minimize
                                    } else {
                                        IconName::Maximize
                                    })
                                    .tooltip(if self.map_only {
                                        "恢复树表"
                                    } else {
                                        "放大空间图"
                                    })
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.map_only = !app.map_only;
                                        app.show_map = true;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                quiet_button("map-options", cx)
                                    .icon(IconName::Ellipsis)
                                    .tooltip("空间图设置")
                                    .dropdown_menu({
                                        let owner = cx.weak_entity();
                                        let motion = self.motion_enabled;
                                        move |menu, _, _| {
                                            let owner = owner.clone();
                                            menu.item(
                                                PopupMenuItem::new("界面动效")
                                                    .checked(motion)
                                                    .on_click(move |_, _, cx| {
                                                        let _ = owner.update(cx, |app, cx| {
                                                            app.motion_enabled =
                                                                !app.motion_enabled;
                                                            cx.set_reduce_motion(
                                                                !app.motion_enabled,
                                                            );
                                                            cx.notify();
                                                        });
                                                    }),
                                            )
                                        }
                                    }),
                            )
                            .child(
                                TabBar::new("map-metric")
                                    .segmented()
                                    .small()
                                    .selected_index(usize::from(self.metric == Metric::Size))
                                    .child(Tab::new().label("实际占用"))
                                    .child(Tab::new().label("文件大小"))
                                    .on_click(cx.listener(|app, index, _, cx| {
                                        app.metric = if *index == 0 {
                                            Metric::Allocated
                                        } else {
                                            Metric::Size
                                        };
                                        app.request_map(cx);
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(map))
    }
}
fn parse_size(text: &str) -> u64 {
    let clean = text.trim().to_ascii_lowercase().replace(' ', "");
    let split = clean
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(clean.len());
    let n = clean[..split].parse::<f64>().unwrap_or(0.);
    let scale = match &clean[split..] {
        "k" | "kb" | "kib" => 1024f64,
        "m" | "mb" | "mib" => 1024f64.powi(2),
        "g" | "gb" | "gib" => 1024f64.powi(3),
        "t" | "tb" | "tib" => 1024f64.powi(4),
        _ => 1.,
    };
    (n * scale) as u64
}
fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                hwnd: isize,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show: i32,
            ) -> isize;
        }
        let op: Vec<_> = "open".encode_utf16().chain(Some(0)).collect();
        let file: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                0,
                op.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if result <= 32 {
            return Err(std::io::Error::other(format!("系统打开失败 ({result})")));
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("--")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
}

pub fn pretty_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    }
}
fn apply_theme(dark: bool, window: &mut Window, cx: &mut App) {
    Theme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        Some(window),
        cx,
    );
    Theme::update(cx, |theme| {
        let background = rgb(if dark { 0x1c1e22 } else { 0xffffff }).into();
        let foreground = rgb(if dark { 0xebedf0 } else { 0x25282e }).into();
        let muted = rgb(if dark { 0x2a2d33 } else { 0xf0f2f5 }).into();
        let muted_foreground = rgb(if dark { 0xa6adb9 } else { 0x626b79 }).into();
        let border = rgb(if dark { 0x363a42 } else { 0xe5e8ed }).into();
        theme.radius = px(7.);
        theme.radius_lg = px(12.);
        theme.colors.background = background;
        theme.colors.foreground = foreground;
        theme.colors.muted = muted;
        theme.colors.muted_foreground = muted_foreground;
        theme.colors.border = border;
        theme.colors.input = border;
        theme.colors.primary = rgb(if dark { 0x70b5ff } else { 0x0065d6 }).into();
        theme.colors.secondary = muted;
        theme.colors.secondary_foreground = foreground;
        theme.colors.secondary_hover = rgb(if dark { 0x333740 } else { 0xe9ecf1 }).into();
        theme.colors.secondary_active = rgb(if dark { 0x3a3f49 } else { 0xe1e5ec }).into();
        theme.colors.button = background;
        theme.colors.button_foreground = foreground;
        theme.colors.button_hover = theme.colors.secondary_hover;
        theme.colors.button_active = theme.colors.secondary_active;
        theme.colors.tab_bar_segmented = rgb(if dark { 0x111316 } else { 0xe8ebef }).into();
        theme.colors.tab_foreground = muted_foreground;
        theme.colors.tab_active_foreground = foreground;
        theme.colors.tab_bar = background;
        theme.colors.tab_active = background;
        theme.colors.selection = rgb(if dark { 0x243b56 } else { 0xeaf3ff }).into();
        theme.colors.table = background;
        theme.colors.table_head = rgb(if dark { 0x24272d } else { 0xf7f8fa }).into();
        theme.colors.table_head_foreground = muted_foreground;
        theme.colors.table_even = background;
        theme.colors.table_hover = rgb(if dark { 0x262b32 } else { 0xf5f8fc }).into();
        theme.colors.table_active = theme.colors.selection;
        theme.colors.table_active_border = border.opacity(0.);
        theme.colors.table_row_border = border.opacity(0.);
        // Critically damped: rapidly changing tabs retarget the existing spring.
        theme.motion.spring_move = theme.motion.spring_move.with_damping(1.0);
        theme.colors.primary_hover = rgb(if dark { 0x409cff } else { 0x0062cc }).into();
        theme.colors.primary_active = rgb(0x0056b3).into();
        theme.colors.primary_foreground = rgb(if dark { 0x10243c } else { 0xffffff }).into();
        theme.colors.ring = theme.colors.primary;
        theme.colors.button_primary = theme.colors.primary;
        theme.colors.button_primary_hover = theme.colors.primary_hover;
        theme.colors.button_primary_active = theme.colors.primary_active;
        theme.colors.button_primary_foreground = theme.colors.primary_foreground;
    });
}

fn map_tint(color: u32, dark: bool) -> u32 {
    let mix = |shift: u32| {
        let channel = (color >> shift) & 255;
        if dark {
            channel * 3 / 4
        } else {
            (channel * 3 + 255 * 2) / 5
        }
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// Only advertise transports provided by this platform.
fn scan_methods() -> &'static [(&'static str, ScanStrategy)] {
    #[cfg(windows)]
    {
        &[
            ("自动：文件夹批量读取，整卷尝试 MFT", ScanStrategy::Auto),
            ("目录：批量读取，无需管理员权限", ScanStrategy::Directory),
            (
                "MFT（实验）：整盘 / 文件夹，按需请求管理员权限",
                ScanStrategy::NtfsMft,
            ),
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            ("自动：macOS 批量读取文件属性", ScanStrategy::Auto),
            (
                "目录：原生批量读取，不支持时自动兼容",
                ScanStrategy::Directory,
            ),
        ]
    }
    #[cfg(target_os = "linux")]
    {
        &[
            ("自动：Linux 目录句柄相对查询", ScanStrategy::Auto),
            (
                "目录：原生元数据读取，无需管理员权限",
                ScanStrategy::Directory,
            ),
        ]
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        &[
            ("自动", ScanStrategy::Auto),
            ("目录", ScanStrategy::Directory),
        ]
    }
}
