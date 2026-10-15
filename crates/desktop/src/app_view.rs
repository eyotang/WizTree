// Kept in the app module so all view callbacks share the application's actions.
impl DiskApp {
    fn view_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let scanning = self.job.is_some() || self.cleanup_busy;
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(60.))
            .flex_shrink_0()
            .px_4()
            .bg(chrome(cx))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mr_4()
                    .child(img(crate::branding::logo()).size(px(32.)))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("WizTree"),
                    ),
            )
            .child(self.volumes_button(cx).into_any_element())
            .child(self.method_button(cx).into_any_element())
            .child(div().flex_1().min_w_0().child(if self.path_editing {
                Input::new(&self.path)
                    .small()
                    .disabled(scanning)
                    .into_any_element()
            } else {
                quiet_button("edit-scan-location", cx)
                    .icon(IconName::FolderOpen)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border.opacity(0.6))
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
                quiet_button("choose-folder", cx)
                    .icon(IconName::Folder)
                    .label("文件夹…")
                    .disabled(scanning || self.folder_picker_open)
                    .on_click(cx.listener(|app, _, window, cx| app.choose(window, cx))),
            )
            .child(
                Button::new("scan-stop")
                    .small()
                    .primary()
                    .rounded(px(8.))
                    .w(px(80.))
                    .h(px(30.))
                    .icon(if scanning {
                        IconName::Square
                    } else {
                        IconName::Play
                    })
                    .label(if scanning { "停止" } else { "扫描" })
                    .on_click(cx.listener(|app, _, window, cx| {
                        if app.job.is_some() {
                            app.cancel(&CancelScan, window, cx);
                        } else {
                            app.start_scan(cx);
                        }
                    })),
            )
            .child(self.menu_button(cx).into_any_element())
            .child(
                quiet_button("appearance", cx)
                    .rounded(px(999.))
                    .icon(if self.dark {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    })
                    .tooltip(if self.dark {
                        "切换为浅色外观"
                    } else {
                        "切换为深色外观"
                    })
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
                    data.snapshot
                        .root
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| pretty_path(&data.snapshot.root))
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
        let snapshot = self.data.as_ref().map(|data| &data.snapshot);
        let pending = self.job.as_ref().map(|job| job.progress.snapshot());
        let occupied = snapshot
            .map(|s| bytes(s.nodes[0].allocated))
            .unwrap_or_else(|| "—".into());
        let files = snapshot
            .map(|s| number(s.files))
            .or_else(|| pending.as_ref().map(|p| number(p.files)))
            .unwrap_or_else(|| "—".into());
        let folders = snapshot
            .map(|s| number(s.dirs.saturating_sub(1)))
            .or_else(|| pending.as_ref().map(|p| number(p.dirs)))
            .unwrap_or_else(|| "—".into());
        let free = volume.map(|v| bytes(v.free)).unwrap_or_else(|| "—".into());
        let free_label = volume
            .map(|v| format!("磁盘可用 · 容量 {}", bytes(v.total)))
            .unwrap_or_else(|| "磁盘可用".into());
        let teal = rgb(if self.dark { 0x69c7b7 } else { 0x187f72 }).into();
        let violet = rgb(if self.dark { 0xb5a5e4 } else { 0x7865ab }).into();
        crate::appearance::panel(cx)
            .mx_4()
            .mb_3()
            .px_5()
            .py_4()
            .flex()
            .items_center()
            .gap_5()
            .flex_shrink_0()
            .child(crate::appearance::metric(
                IconName::ChartPie,
                "已分析占用",
                occupied,
                cx.theme().primary,
                cx,
            ))
            .child(crate::appearance::metric(
                IconName::FileText,
                "文件",
                files,
                cx.theme().primary,
                cx,
            ))
            .child(crate::appearance::metric(
                IconName::Folder,
                "文件夹",
                folders,
                teal,
                cx,
            ))
            .child(crate::appearance::metric(
                IconName::HardDrive,
                &free_label,
                free,
                violet,
                cx,
            ))
            .into_any_element()
    }

    fn view_navigation(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(48.))
            .flex_shrink_0()
            .px_4()
            .child(
                TabBar::new("view-mode")
                    .segmented()
                    .selected_index(usize::from(
                        self.mode == Mode::Files || self.is_filtered(cx),
                    ))
                    .child(Tab::new().label("目录树"))
                    .child(Tab::new().label("文件视图"))
                    .on_click(cx.listener(|app, index, window, cx| {
                        if *index == 0 {
                            app.clear_filter(window, cx);
                            app.mode = Mode::Tree;
                        } else {
                            app.mode = Mode::Files;
                        }
                        app.extension = None;
                        app.request_query(cx);
                    })),
            )
            .child(div().w(px(1.)).h(px(18.)).mx_1().bg(cx.theme().border))
            .child(
                quiet_button("history-back", cx)
                    .compact()
                    .icon(IconName::ChevronLeft)
                    .disabled(!self.history.can_back())
                    .on_click(cx.listener(|app, _, window, cx| app.go_back(&GoBack, window, cx))),
            )
            .child(
                quiet_button("history-forward", cx)
                    .compact()
                    .icon(IconName::ChevronRight)
                    .disabled(!self.history.can_forward())
                    .on_click(
                        cx.listener(|app, _, window, cx| app.go_forward(&GoForward, window, cx)),
                    ),
            )
            .child(
                quiet_button("parent-folder", cx)
                    .compact()
                    .icon(IconName::ArrowUp)
                    .disabled(self.scope == 0)
                    .on_click(cx.listener(|app, _, window, cx| app.go_up(&GoUp, window, cx))),
            )
            .child(div().flex_1().min_w_0().child(self.breadcrumbs(cx)))
            .child(
                toggle_button("show-types", self.show_types, cx)
                    .rounded(px(999.))
                    .w(px(30.))
                    .h(px(30.))
                    .icon(IconName::PanelRight)
                    .accessibility_label("侧栏")
                    .tooltip("显示或隐藏侧栏")
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.show_types = !app.show_types;
                        cx.notify();
                    })),
            )
            .child(
                toggle_button("show-map", self.show_map, cx)
                    .rounded(px(999.))
                    .w(px(30.))
                    .h(px(30.))
                    .icon(IconName::LayoutDashboard)
                    .accessibility_label("空间图")
                    .tooltip("显示或隐藏空间图")
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
            .h(px(38.))
            .px_4()
            .gap_4()
            .text_xs();
        options = options
            .child(
                Checkbox::new("match-paths")
                    .small()
                    .checked(self.match_paths)
                    .label("匹配完整路径")
                    .on_click(cx.listener(|app, checked, _, cx| {
                        app.match_paths = *checked;
                        app.request_query(cx);
                    })),
            )
            .child(
                Checkbox::new("include-folders")
                    .small()
                    .checked(self.include_folders)
                    .label("包括文件夹")
                    .on_click(cx.listener(|app, checked, _, cx| {
                        app.include_folders = *checked;
                        app.request_query(cx);
                    })),
            )
            .child(
                Checkbox::new("duplicate-names")
                    .small()
                    .checked(self.duplicates)
                    .label("同名同大小候选")
                    .on_click(cx.listener(|app, checked, _, cx| {
                        app.duplicates = *checked;
                        app.request_query(cx);
                    })),
            )
            .child(div().w(px(1.)).h(px(14.)).mx_1().bg(cx.theme().border))
            .child(
                quiet_button("result-limit", cx)
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
            quiet_button("clear-filters", cx)
                .compact()
                .label("清除筛选")
                .disabled(!self.is_filtered(cx) && !self.match_paths && !self.include_folders)
                .on_click(cx.listener(|app, _, window, cx| app.clear_filter(window, cx))),
        );
        let active_options = usize::from(self.match_paths)
            + usize::from(self.include_folders)
            + usize::from(self.duplicates)
            + usize::from(!self.exclude.read(cx).value().is_empty())
            + usize::from(!self.minimum.read(cx).value().is_empty());
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border.opacity(0.6))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(48.))
                    .px_4()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .small()
                                .appearance(false)
                                .prefix(Icon::new(IconName::Search).small())
                                .cleanable(true),
                        ),
                    )
                    .child(
                        toggle_button(
                            "filter-options",
                            self.show_filter_options || active_options > 0,
                            cx,
                        )
                        .icon(IconName::Settings2)
                        .label(if active_options == 0 {
                            "筛选条件".to_string()
                        } else {
                            format!("筛选条件 · {active_options}")
                        })
                        .toggled(self.show_filter_options)
                        .on_click(cx.listener(|app, _, _, cx| {
                            app.show_filter_options = !app.show_filter_options;
                            cx.notify();
                        })),
                    ),
            )
            .when(self.show_filter_options, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .px_4()
                        .py_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("排除"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.exclude).small().cleanable(true)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("最小大小"),
                        )
                        .child(
                            div()
                                .w(px(160.))
                                .child(Input::new(&self.minimum).small().cleanable(true)),
                        ),
                )
                .child(options)
            })
            .into_any_element()
    }

    fn view_workspace(&self, cx: &mut Context<Self>) -> AnyElement {
        let explorer = crate::appearance::panel(cx)
            .size_full()
            .flex()
            .flex_col()
            .child(self.view_navigation(cx))
            .child(self.view_filters(cx))
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .on_key_down(cx.listener(Self::marking_key))
                    .child(
                        DataTable::new(&self.table)
                            .with_size(px(30.))
                            .header_row_height(px(32.))
                            .stripe(false)
                            .bordered(false)
                            .scrollbar_visible(true, true),
                    ),
            );
        let map = crate::appearance::panel(cx)
            .size_full()
            .child(self.treemap(cx))
            .into_any_element();
        let main = if self.map_only {
            map
        } else {
            v_resizable("tables-and-map")
                .child(
                    resizable_panel()
                        .size(px(430.))
                        .size_range(
                            px(if self.show_filter_options { 300. } else { 210. })..px(2400.),
                        )
                        .child(explorer),
                )
                .child(
                    resizable_panel()
                        .size(px(300.))
                        .size_range(px(100.)..px(1800.))
                        .visible(self.show_map)
                        .child(div().pt_3().size_full().child(map)),
                )
                .into_any_element()
        };
        div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .px_4()
            .pb_3()
            .child(
                h_resizable("workspace-and-inspector")
                    .child(
                        resizable_panel().size_range(px(440.)..px(4000.)).child(
                            div()
                                .pr(px(if self.show_types { 12. } else { 0. }))
                                .size_full()
                                .child(main),
                        ),
                    )
                    .child(
                        resizable_panel()
                            .size(px(340.))
                            .size_range(px(290.)..px(600.))
                            .visible(self.show_types)
                            .child(
                                crate::appearance::panel(cx)
                                    .size_full()
                                    .child(self.inspector(cx)),
                            ),
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
            .bg(chrome(cx))
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
                    .bg(chrome(cx))
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
