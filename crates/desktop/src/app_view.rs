// Kept in the app module so all view callbacks share the application's actions.
impl DiskApp {
    fn view_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let scanning = self.job.is_some() || self.cleanup_busy;
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(54.))
            .flex_shrink_0()
            .px_3()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(self.menu_button(cx).into_any_element())
            .child(self.volumes_button(cx).into_any_element())
            .child(self.method_button(cx).into_any_element())
            .child(div().flex_1().min_w_0().child(if self.path_editing {
                Input::new(&self.path)
                    .small()
                    .disabled(scanning)
                    .into_any_element()
            } else {
                Button::new("edit-scan-location")
                    .small()
                    .ghost()
                    .w_full()
                    .disabled(scanning)
                    .label(if self.path.read(cx).value().is_empty() {
                        "选择磁盘或输入路径".to_string()
                    } else {
                        pretty_path(Path::new(self.path.read(cx).value().as_str()))
                    })
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.edit_location(&EditLocation, window, cx)
                    }))
                    .into_any_element()
            }))
            .child(
                Button::new("choose-folder")
                    .small()
                    .compact()
                    .label("文件夹…")
                    .disabled(scanning || self.folder_picker_open)
                    .on_click(cx.listener(|app, _, window, cx| app.choose(window, cx))),
            )
            .child(
                Button::new("scan-stop")
                    .small()
                    .primary()
                    .label(if scanning { "停止" } else { "扫描" })
                    .on_click(cx.listener(|app, _, window, cx| {
                        if app.job.is_some() {
                            app.cancel(&CancelScan, window, cx);
                        } else {
                            app.start_scan(cx);
                        }
                    })),
            )
            .child(
                Button::new("appearance")
                    .small()
                    .ghost()
                    .compact()
                    .label(if self.dark { "浅色" } else { "深色" })
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.dark = !app.dark;
                        apply_theme(app.dark, window, cx);
                        app.preferences.dark = Some(app.dark);
                        let preferences = app.preferences.clone();
                        std::thread::spawn(move || {
                            let _ = storage::save_preferences(&preferences);
                        });
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn breadcrumbs(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut bar = div()
            .flex()
            .items_center()
            .gap_1()
            .min_w_0()
            .overflow_hidden();
        if let Some(data) = &self.data {
            let mut ids = vec![self.scope];
            let mut id = self.scope;
            while let Some(parent) = data.snapshot.nodes[id].parent {
                ids.push(parent);
                id = parent;
            }
            ids.reverse();
            // Keep deep paths usable without letting their chrome consume the table.
            let start = ids.len().saturating_sub(5);
            if start > 0 {
                bar = bar.child(div().text_color(cx.theme().muted_foreground).child("…"));
            }
            for &id in &ids[start..] {
                let label = if id == 0 {
                    pretty_path(&data.snapshot.root)
                } else {
                    data.snapshot.nodes[id].name.to_string_lossy().into_owned()
                };
                bar = bar.child(
                    Button::new(("breadcrumb", id))
                        .small()
                        .ghost()
                        .compact()
                        .label(label)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.navigate_to(id, window, cx);
                        })),
                );
                if id != self.scope {
                    bar = bar.child(
                        Icon::new(IconName::ChevronRight)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    );
                }
            }
        } else {
            bar = bar.child(div().text_color(cx.theme().muted_foreground).child(
                if self.job.is_some() {
                    "正在扫描所选文件夹…"
                } else {
                    "选择一个磁盘或文件夹开始分析"
                },
            ));
        }
        bar.into_any_element()
    }

    fn capacity(&self, cx: &mut Context<Self>) -> AnyElement {
        let target = self
            .data
            .as_ref()
            .map(|data| data.snapshot.root.clone())
            .unwrap_or_else(|| PathBuf::from(self.path.read(cx).value().as_str()));
        let key = |path: &Path| {
            let value = path.to_string_lossy().replace('\\', "/");
            let value = value.strip_prefix("//?/").unwrap_or(&value).to_string();
            if cfg!(windows) {
                value.to_lowercase()
            } else {
                value
            }
        };
        let target = key(&target);
        let volume = self
            .volumes
            .iter()
            .filter(|volume| {
                let root = key(&volume.root);
                target == root
                    || target.starts_with(&if root.ends_with('/') {
                        root
                    } else {
                        format!("{root}/")
                    })
            })
            .max_by_key(|volume| volume.root.as_os_str().len());
        let mut bar = div()
            .flex()
            .items_center()
            .gap_4()
            .h(px(34.))
            .flex_shrink_0()
            .px_4()
            .text_xs()
            .bg(cx.theme().muted)
            .border_b_1()
            .border_color(cx.theme().border);
        if let Some(volume) = volume {
            let used = volume.total.saturating_sub(volume.free);
            let ratio = if volume.total == 0 {
                0.
            } else {
                used as f64 / volume.total as f64
            };
            bar = bar
                .child(div().font_weight(FontWeight::MEDIUM).child(format!(
                    "{} {}",
                    volume.root.display(),
                    volume.label
                )))
                .child(div().child(format!("容量 {}", bytes(volume.total))))
                .child(div().child(format!("已用 {}", bytes(used))))
                .child(div().child(format!("可用 {}", bytes(volume.free))))
                .child(
                    div()
                        .w(px(108.))
                        .h(px(5.))
                        .rounded_full()
                        .bg(cx.theme().border)
                        .child(
                            div()
                                .h_full()
                                .w(relative(ratio.clamp(0., 1.) as f32))
                                .rounded_full()
                                .bg(cx.theme().primary),
                        ),
                )
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(volume.filesystem.clone().unwrap_or_default()),
                );
        } else {
            bar = bar.child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("磁盘容量将在选定本地磁盘后显示"),
            );
        }
        bar = bar.child(div().flex_1());
        if let Some(data) = &self.data {
            bar = bar.child(div().child(format!(
                "{} {} · {} 个文件",
                "已分析",
                bytes(data.snapshot.nodes[0].allocated),
                number(data.snapshot.files)
            )));
        }
        bar.into_any_element()
    }

    fn view_navigation(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(43.))
            .flex_shrink_0()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("tree-mode")
                    .small()
                    .ghost()
                    .selected(self.mode == Mode::Tree && !self.is_filtered(cx))
                    .label("目录树")
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.clear_filter(window, cx);
                        app.mode = Mode::Tree;
                        app.extension = None;
                        app.request_query(cx);
                    })),
            )
            .child(
                Button::new("files-mode")
                    .small()
                    .ghost()
                    .selected(self.mode == Mode::Files || self.is_filtered(cx))
                    .label("文件视图")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.mode = Mode::Files;
                        app.extension = None;
                        app.request_query(cx);
                    })),
            )
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(cx.theme().border))
            .child(
                Button::new("history-back")
                    .small()
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronLeft)
                    .disabled(!self.history.can_back())
                    .on_click(cx.listener(|app, _, window, cx| app.go_back(&GoBack, window, cx))),
            )
            .child(
                Button::new("history-forward")
                    .small()
                    .ghost()
                    .compact()
                    .icon(IconName::ChevronRight)
                    .disabled(!self.history.can_forward())
                    .on_click(
                        cx.listener(|app, _, window, cx| app.go_forward(&GoForward, window, cx)),
                    ),
            )
            .child(
                Button::new("parent-folder")
                    .small()
                    .ghost()
                    .compact()
                    .icon(IconName::ArrowUp)
                    .disabled(self.scope == 0)
                    .on_click(cx.listener(|app, _, window, cx| app.go_up(&GoUp, window, cx))),
            )
            .child(div().flex_1().min_w_0().child(self.breadcrumbs(cx)))
            .child(
                Button::new("show-types")
                    .small()
                    .ghost()
                    .compact()
                    .selected(self.show_types)
                    .label("侧栏")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.show_types = !app.show_types;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("show-map")
                    .small()
                    .ghost()
                    .compact()
                    .selected(self.show_map)
                    .label("空间图")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.show_map = !app.show_map;
                        if app.show_map {
                            app.request_map(cx);
                        }
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn view_filters(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut options = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(34.))
            .px_3()
            .text_xs();
        options = options
            .child(
                Button::new("match-paths")
                    .small()
                    .ghost()
                    .compact()
                    .selected(self.match_paths)
                    .label("匹配完整路径")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.match_paths = !app.match_paths;
                        app.request_query(cx);
                    })),
            )
            .child(
                Button::new("include-folders")
                    .small()
                    .ghost()
                    .compact()
                    .selected(self.include_folders)
                    .label("包括文件夹")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.include_folders = !app.include_folders;
                        app.request_query(cx);
                    })),
            )
            .child(
                Button::new("duplicate-names")
                    .small()
                    .ghost()
                    .compact()
                    .selected(self.duplicates)
                    .label("同名同大小候选")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.duplicates = !app.duplicates;
                        app.request_query(cx);
                    })),
            )
            .child(div().w(px(1.)).h(px(14.)).mx_1().bg(cx.theme().border))
            .child(
                Button::new("result-limit")
                    .small()
                    .ghost()
                    .compact()
                    .label(if self.limit == 0 {
                        "结果：全部".to_string()
                    } else {
                        format!("结果：{}", number(self.limit as u64))
                    })
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.limit = match app.limit {
                            1000 => 10_000,
                            10_000 => 100_000,
                            100_000 => 0,
                            _ => 1000,
                        };
                        app.request_query(cx);
                    })),
            )
            .child(div().flex_1());
        if let Some(extension) = &self.extension {
            options = options.child(div().text_color(cx.theme().primary).child(
                if extension.is_empty() {
                    "筛选：无扩展名".to_string()
                } else {
                    format!("筛选：.{extension}")
                },
            ));
        }
        options = options.child(
            Button::new("clear-filters")
                .small()
                .ghost()
                .compact()
                .label("清除筛选")
                .disabled(!self.is_filtered(cx))
                .on_click(cx.listener(|app, _, window, cx| app.clear_filter(window, cx))),
        );
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(42.))
                    .px_3()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("筛选"),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .small()
                                .prefix(Icon::new(IconName::Search).small())
                                .cleanable(true),
                        ),
                    )
                    .child(
                        div()
                            .w(px(220.))
                            .child(Input::new(&self.exclude).small().cleanable(true)),
                    )
                    .child(
                        div()
                            .w(px(180.))
                            .child(Input::new(&self.minimum).small().cleanable(true)),
                    ),
            )
            .child(options)
            .into_any_element()
    }

    fn view_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let explorer = div()
            .on_key_down(cx.listener(Self::marking_key))
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                DataTable::new(&self.table)
                    .with_size(px(28.))
                    .header_row_height(px(30.))
                    .stripe(true)
                    .bordered(false)
                    .scrollbar_visible(true, true),
            );
        let map = self.treemap(cx).into_any_element();
        let main = if self.map_only {
            map
        } else {
            v_resizable("tables-and-map")
                .child(
                    resizable_panel()
                        .size(px(400.))
                        .size_range(px(140.)..px(2400.))
                        .child(explorer),
                )
                .child(
                    resizable_panel()
                        .size(px(300.))
                        .size_range(px(100.)..px(1800.))
                        .visible(self.show_map)
                        .child(map),
                )
                .into_any_element()
        };
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(
                h_resizable("workspace-and-inspector")
                    .child(
                        resizable_panel()
                            .size_range(px(440.)..px(4000.))
                            .child(main),
                    )
                    .child(
                        resizable_panel()
                            .size(px(340.))
                            .size_range(px(290.)..px(600.))
                            .visible(self.show_types)
                            .child(self.inspector(cx)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for DiskApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(path) = self.sync_path.take() {
            self.path.update(cx, |input, cx| {
                input.set_value(pretty_path(&path), window, cx)
            });
        }
        let toolbar = self.view_toolbar(cx);
        let capacity = self.capacity(cx);
        let navigation = self.view_navigation(cx);
        let filters = self.view_filters(cx);
        let workspace = self.view_workspace(cx);
        let status = if let Some(job) = &self.job {
            let progress = job.progress.snapshot();
            if progress.mft_records > 0 {
                format!(
                    "正在读取 NTFS 元数据 · {} 条记录 · {:.1} 秒 · Esc 停止",
                    number(progress.mft_records),
                    job.started.elapsed().as_secs_f64()
                )
            } else {
                format!(
                    "正在扫描 · {} 文件 · {} 文件夹 · {:.1} 秒 · Esc 停止",
                    number(progress.files),
                    number(progress.dirs),
                    job.started.elapsed().as_secs_f64()
                )
            }
        } else if self.querying {
            "正在筛选与排序…".to_string()
        } else {
            self.status.clone()
        };
        let selected_path = self
            .data
            .as_ref()
            .and_then(|data| {
                data.snapshot
                    .nodes
                    .get(self.selected)
                    .map(|_| pretty_path(&data.snapshot.path(self.selected)))
            })
            .unwrap_or_default();
        div()
            .id("disk-app")
            .key_context("DiskApp")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_sm()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::mark_selected))
            .on_action(cx.listener(Self::review_marks))
            .on_action(cx.listener(Self::edit_location))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::previous))
            .on_action(cx.listener(Self::next))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::choose_action))
            .on_action(cx.listener(Self::rescan))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::reveal))
            .on_action(cx.listener(Self::trash))
            .child(toolbar)
            .child(capacity)
            .child(navigation)
            .child(filters)
            .child(workspace)
            .child(
                div()
                    .h(px(27.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_4()
                    .text_xs()
                    .bg(cx.theme().muted)
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().min_w_0().truncate().child(status))
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} 项", number(self.rows.len() as u64))),
                    )
                    .child(
                        div()
                            .max_w(relative(0.45))
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(selected_path),
                    ),
            )
    }
}
