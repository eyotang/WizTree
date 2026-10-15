impl DiskApp {
    fn current_location(&self, cx: &App) -> ViewLocation {
        ViewLocation {
            scope: self.scope,
            map_root: self.map_root,
            selected: self.selected,
            expanded: self.expanded.clone(),
            mode: self.mode,
            query: self.search.read(cx).value().to_string(),
            exclude: self.exclude.read(cx).value().to_string(),
            minimum: self.minimum.read(cx).value().to_string(),
            extension: self.extension.clone(),
            duplicates: self.duplicates,
            scroll: self
                .table
                .read(cx)
                .vertical_scroll_handle
                .0
                .borrow()
                .base_handle
                .offset(),
        }
    }
    fn apply_location(
        &mut self,
        location: ViewLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scope = location.scope;
        self.map_root = location.map_root;
        self.selected = location.selected;
        self.expanded = location.expanded;
        self.mode = location.mode;
        self.extension = location.extension;
        self.duplicates = location.duplicates;
        for (input, value) in [
            (&self.search, location.query),
            (&self.exclude, location.exclude),
            (&self.minimum, location.minimum),
        ] {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.pending_scroll = Some(location.scroll);
        self.locate_pending = false;
        self.request_query(cx);
        self.request_map(cx);
    }
    fn navigate_to(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .data
            .as_ref()
            .and_then(|data| data.snapshot.nodes.get(id))
            .is_none_or(|node| !node.is_dir)
        {
            return;
        }
        if id == self.scope {
            self.select(id, cx);
            self.map_root = id;
            self.request_map(cx);
            return;
        }
        let current = self.current_location(cx);
        let mut next = current.clone();
        next.scope = id;
        next.map_root = id;
        next.selected = id;
        next.expanded.insert(id);
        next.mode = Mode::Tree;
        next.query.clear();
        next.exclude.clear();
        next.minimum.clear();
        next.extension = None;
        next.duplicates = false;
        next.scroll = point(px(0.), px(0.));
        self.history.push(current, next.clone());
        self.apply_location(next, window, cx);
    }
    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(location) = self.history.back(self.current_location(cx)) {
            self.apply_location(location, window, cx);
        }
    }
    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(location) = self.history.forward(self.current_location(cx)) {
            self.apply_location(location, window, cx);
        }
    }
    fn edit_location(&mut self, _: &EditLocation, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup_busy || self.job.is_some() {
            return;
        }
        self.path_editing = true;
        self.path.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }
    fn select_map_node(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.map_focus, cx);
        self.select(id, cx);
        self.sidebar_tab = SidebarTab::Details;
        self.show_types = true;
        // Reveal in the current tree without changing scope, filters or map root.
        if self.mode == Mode::Tree && !self.is_filtered(cx) {
            if let Some(data) = &self.data {
                let mut current = id;
                while let Some(parent) = data.snapshot.nodes[current].parent {
                    self.expanded.insert(parent);
                    current = parent;
                }
            }
            self.locate_pending = true;
            self.request_query(cx);
        }
    }
    pub fn toggle_mark(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.cleanup_busy || self.job.is_some() || id == 0 {
            return;
        }
        let Some(data) = &self.data else {
            return;
        };
        if id >= data.snapshot.nodes.len() {
            return;
        }
        if !self.marked.remove(&id) {
            self.marked.insert(id);
        }
        self.mark_plan = Arc::new(interaction::mark_plan(&data.snapshot, &self.marked));
        self.update_table(cx);
    }
    fn marking_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if (event.keystroke.key == "space" || event.keystroke.key == " ")
            && !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.platform
            && !event.keystroke.modifiers.alt
        {
            cx.stop_propagation();
            if !event.is_held {
                self.toggle_mark(self.selected, cx);
            }
        }
    }
    fn mark_selected(&mut self, _: &ToggleMark, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_mark(self.selected, cx);
    }
    fn review_marks(&mut self, _: &ReviewMarks, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_tab = SidebarTab::Review;
        self.show_types = true;
        cx.notify();
    }
    fn confirm_trash(&mut self, ids: Vec<usize>, window: &mut Window, cx: &mut Context<Self>) {
        if self.cleanup_busy || self.job.is_some() {
            return;
        }
        let Some(data) = &self.data else {
            return;
        };
        let snapshot = data.snapshot.clone();
        let plan = interaction::mark_plan(&snapshot, &ids.into_iter().collect());
        if plan.roots.is_empty() {
            self.status = "扫描根目录不能移到回收站，请选择具体文件或子目录".into();
            cx.notify();
            return;
        }
        let paths = plan
            .roots
            .iter()
            .map(|&id| pretty_path(&snapshot.path(id)))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = format!(
            "所选项目占用合计 {}，实际释放空间可能受硬链接等影响。\n\n{}",
            bytes(plan.allocated),
            paths
        );
        let response = window.prompt(
            PromptLevel::Warning,
            &format!("将 {} 个项目移到系统回收站？", plan.roots.len()),
            Some(&detail),
            &[
                PromptButton::Cancel("取消".into()),
                PromptButton::Ok("移到回收站".into()),
            ],
            cx,
        );
        self.cleanup_busy = true;
        cx.spawn_in(window, async move |app, cx| {
            let confirmed = matches!(response.await, Ok(1));
            let _ = app.update_in(cx, |app, _, cx| {
                if !confirmed {
                    app.cleanup_busy = false;
                    cx.notify();
                    return;
                }
                if !app
                    .data
                    .as_ref()
                    .is_some_and(|data| Arc::ptr_eq(&data.snapshot, &snapshot))
                {
                    app.cleanup_busy = false;
                    app.status = "扫描结果已更新，请重新选择项目".into();
                    cx.notify();
                    return;
                }
                let (tx, rx) = mpsc::channel();
                app.cleanup_rx = Some(rx);
                app.status = "正在移到系统回收站…".into();
                std::thread::spawn(move || {
                    let mut report = CleanupReport {
                        snapshot: snapshot.clone(),
                        succeeded: vec![],
                        failed: vec![],
                    };
                    for id in plan.roots {
                        let result = interaction::trash_target(&snapshot, id).and_then(|path| {
                            trash::delete(path)
                                .map_err(|error| std::io::Error::other(error.to_string()))
                        });
                        match result {
                            Ok(()) => report.succeeded.push(id),
                            Err(error) => report.failed.push((id, error.to_string())),
                        }
                    }
                    let _ = tx.send(report);
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn poll_cleanup(&mut self, cx: &mut Context<Self>) {
        let result = self.cleanup_rx.as_ref().map(|rx| rx.try_recv());
        match result {
            Some(Ok(report)) => {
                self.cleanup_busy = false;
                self.cleanup_rx = None;
                if self
                    .data
                    .as_ref()
                    .is_some_and(|data| Arc::ptr_eq(&data.snapshot, &report.snapshot))
                {
                    let done = report.succeeded.iter().copied().collect();
                    self.marked
                        .retain(|&id| !interaction::covered(&report.snapshot, &done, id));
                    self.mark_plan =
                        Arc::new(interaction::mark_plan(&report.snapshot, &self.marked));
                    self.update_table(cx);
                }
                self.status = format!(
                    "已移到回收站 {} 项，失败 {} 项",
                    report.succeeded.len(),
                    report.failed.len()
                );
                if let Some((id, error)) = report.failed.first() {
                    self.status.push_str(&format!(
                        " · {}：{} · 请重新扫描",
                        pretty_path(&report.snapshot.path(*id)),
                        error
                    ));
                } else if !report.succeeded.is_empty() {
                    self.start_scan(cx);
                }
                cx.notify();
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.cleanup_busy = false;
                self.cleanup_rx = None;
                self.status = "回收站操作线程已停止，请重新扫描".into();
                cx.notify();
            }
            _ => {}
        }
    }
}
