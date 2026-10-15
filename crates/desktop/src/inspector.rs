impl DiskApp {
    fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .h(px(46.))
            .px_4()
            .flex()
            .items_center()
            .flex_shrink_0()
            .bg(cx.theme().background)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                TabBar::new("inspector-tabs")
                    .small()
                    .underline()
                    .selected_index(match self.sidebar_tab {
                        SidebarTab::Details => 0,
                        SidebarTab::Types => 1,
                        SidebarTab::Review => 2,
                    })
                    .child(Tab::new().label("详情"))
                    .child(Tab::new().label("文件类型"))
                    .child(
                        Tab::new().label("复核").suffix(
                            div()
                                .px_1p5()
                                .rounded_full()
                                .text_xs()
                                .bg(cx.theme().muted)
                                .text_color(cx.theme().muted_foreground)
                                .child(self.marked.len().to_string()),
                        ),
                    )
                    .on_click(cx.listener(|app, index, _, cx| {
                        app.sidebar_tab = match index {
                            0 => SidebarTab::Details,
                            1 => SidebarTab::Types,
                            _ => SidebarTab::Review,
                        };
                        cx.notify();
                    })),
            );
        let content = match self.sidebar_tab {
            SidebarTab::Details => self.selection_details(cx),
            SidebarTab::Types => div()
                .size_full()
                .flex()
                .flex_col()
                .min_h_0()
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("扫描全集的文件类型；点击类型筛选当前目录"),
                )
                .child(
                    div().flex_1().min_h_0().child(
                        DataTable::new(&self.types)
                            .with_size(px(28.))
                            .header_row_height(px(30.))
                            .stripe(false)
                            .bordered(false)
                            .scrollbar_visible(true, true),
                    ),
                )
                .into_any_element(),
            SidebarTab::Review => self.review_panel(cx),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .bg(cx.theme().background)
            .child(header)
            .child(div().flex_1().min_h_0().min_w_0().child(content))
            .into_any_element()
    }
    fn detail_line(&self, label: &str, value: String, cx: &App) -> AnyElement {
        div()
            .flex()
            .items_start()
            .justify_between()
            .gap_3()
            .py_1()
            .text_xs()
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(div().min_w_0().text_right().child(value))
            .into_any_element()
    }
    fn selection_details(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(data) = &self.data else {
            return div()
                .p_5()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(if self.job.is_some() {
                    "正在扫描，完成后可查看项目详情。"
                } else {
                    "选择磁盘或文件夹开始分析；点击树表或空间图中的项目，在这里查看详情。"
                })
                .into_any_element();
        };
        let id = self.selected;
        let Some(node) = data.snapshot.nodes.get(id) else {
            return div().into_any_element();
        };
        let summary = data.index.nodes[id];
        let path = pretty_path(&data.snapshot.path(id));
        let total = data.snapshot.nodes[self.scope].allocated;
        let ratio = if total == 0 {
            0.
        } else {
            node.allocated as f64 / total as f64
        };
        let exact = self.marked.contains(&id);
        let inherited = !exact && interaction::covered(&data.snapshot, &self.marked, id);
        let name = if id == 0 {
            pretty_path(&data.snapshot.root)
        } else {
            node.name.to_string_lossy().into_owned()
        };
        let date = node
            .modified
            .map(|time| {
                let date: chrono::DateTime<chrono::Local> = time.into();
                date.format("%Y-%m-%d %H:%M").to_string()
            })
            .unwrap_or_else(|| "—".into());
        let mut buttons = div().flex().flex_wrap().gap_2().mt_3();
        for (button, label, action) in [
            ("detail-open", "打开", ItemAction::Open),
            ("detail-reveal", "在文件管理器中显示", ItemAction::Reveal),
            ("detail-copy", "复制路径", ItemAction::Copy),
            ("detail-locate", "定位到树表", ItemAction::Locate),
        ] {
            buttons = buttons.child(quiet_button(button, cx).label(label).on_click(
                cx.listener(move |app, _, window, cx| app.item_action(action, id, window, cx)),
            ));
        }
        if node.is_dir {
            buttons = buttons.child(
                quiet_button("detail-browse", cx)
                    .icon(IconName::FolderOpen)
                    .label("进入目录")
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.navigate_to(id, window, cx)),
                    ),
            );
        }
        div()
            .id("selection-details")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mb_2()
                    .child(
                        Icon::new(if node.is_dir {
                            IconName::Folder
                        } else {
                            IconName::File
                        })
                        .small()
                        .text_color(cx.theme().primary),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_lg()
                            .child(name),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(path),
            )
            .child(
                div()
                    .mt_5()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("实际占用"),
            )
            .child(
                div()
                    .text_size(px(32.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(bytes(node.allocated)),
            )
            .child(
                div()
                    .h(px(6.))
                    .w_full()
                    .my_2()
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(cx.theme().primary)
                            .w(relative(ratio.clamp(0., 1.) as f32)),
                    ),
            )
            .child(self.detail_line("占当前目录", format!("{:.1}%", ratio * 100.), cx))
            .child(self.detail_line("逻辑大小", bytes(node.size), cx))
            .child(self.detail_line(
                "文件",
                number(if node.is_dir { summary.files } else { 1 }),
                cx,
            ))
            .child(self.detail_line("子文件夹", number(summary.dirs), cx))
            .child(self.detail_line("修改时间", date, cx))
            .child(buttons)
            .child(div().h(px(1.)).w_full().my_4().bg(cx.theme().border))
            .child(
                toggle_button("detail-mark", exact || inherited, cx)
                    .w_full()
                    .icon(if exact || inherited {
                        IconName::CircleCheck
                    } else {
                        IconName::Plus
                    })
                    .tooltip("Space 标记或取消标记，文件不会被修改")
                    .disabled(
                        id == 0
                            || inherited
                            || self.cleanup_busy
                            || self.job.is_some()
                            || self.elevation_pending(),
                    )
                    .label(if inherited {
                        "已随父目录标记"
                    } else if exact {
                        "已加入复核 · 点击取消"
                    } else {
                        "加入复核"
                    })
                    .on_click(cx.listener(move |app, _, _, cx| app.toggle_mark(id, cx))),
            )
            .child(
                Button::new("detail-review")
                    .small()
                    .ghost()
                    .w_full()
                    .mt_1()
                    .label(format!("查看 {} 个标记", self.marked.len()))
                    .disabled(self.marked.is_empty())
                    .on_click(
                        cx.listener(|app, _, window, cx| {
                            app.review_marks(&ReviewMarks, window, cx)
                        }),
                    ),
            )
            .child(
                div()
                    .mt_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("单击选中 · 双击空间图或 Enter 进入目录。标记不会修改磁盘文件。"),
            )
            .into_any_element()
    }
    fn review_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel = div()
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .p_4()
            .gap_3()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("清理前复核"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} 个标记 · {} 项操作（父子项目已合并）",
                        self.marked.len(),
                        self.mark_plan.roots.len()
                    )),
            )
            .child(
                div()
                    .text_size(px(28.))
                    .child(bytes(self.mark_plan.allocated)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("所选项目占用合计；实际释放空间可能受硬链接等影响。"),
            );
        if self.mark_plan.roots.is_empty() {
            return panel.child(div().mt_5().text_sm().text_color(cx.theme().muted_foreground)
                .child("在树表或空间图选中项目后按 Space，或点击详情中的“标记”。跨目录浏览会保留标记。"))
                .into_any_element();
        }
        let owner = cx.weak_entity();
        panel = panel
            .child(
                uniform_list(
                    "review-items",
                    self.mark_plan.roots.len(),
                    move |range, _, cx| {
                        owner
                            .update(cx, |app, cx| {
                                let Some(data) = &app.data else {
                                    return vec![];
                                };
                                range
                                    .filter_map(|row| {
                                        let &id = app.mark_plan.roots.get(row)?;
                                        let node = &data.snapshot.nodes[id];
                                        let name = node.name.to_string_lossy().into_owned();
                                        let path = pretty_path(&data.snapshot.path(id));
                                        Some(
                                            div()
                                                .id(("review-item", id))
                                                .h(px(72.))
                                                .w_full()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .border_b_1()
                                                .border_color(cx.theme().border)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .flex()
                                                        .flex_col()
                                                        .gap_1()
                                                        .child(
                                                            div().truncate().text_sm().child(name),
                                                        )
                                                        .child(
                                                            div()
                                                                .truncate()
                                                                .text_xs()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child(path),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .child(bytes(node.allocated)),
                                                        ),
                                                )
                                                .child(
                                                    Button::new(("unmark-review", id))
                                                        .small()
                                                        .ghost()
                                                        .label("移除")
                                                        .disabled(app.cleanup_busy)
                                                        .on_click(cx.listener(
                                                            move |app, _, _, cx| {
                                                                app.toggle_mark(id, cx)
                                                            },
                                                        )),
                                                ),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                    },
                )
                .flex_1()
                .min_h_0(),
            )
            .child(
                Button::new("review-clear")
                    .small()
                    .ghost()
                    .label("清空标记")
                    .disabled(self.cleanup_busy)
                    .on_click(cx.listener(|app, _, _, cx| {
                        app.marked.clear();
                        app.mark_plan = Arc::new(MarkPlan::default());
                        app.update_table(cx);
                    })),
            )
            .child(
                Button::new("review-trash")
                    .small()
                    .primary()
                    .w_full()
                    .label(if self.cleanup_busy {
                        "正在处理…"
                    } else {
                        "移到系统回收站…"
                    })
                    .disabled(self.cleanup_busy || self.job.is_some() || self.elevation_pending())
                    .on_click(cx.listener(|app, _, window, cx| {
                        app.confirm_trash(app.mark_plan.roots.clone(), window, cx)
                    })),
            );
        panel.into_any_element()
    }
}
