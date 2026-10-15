// Opt-in application integration check; fixture scans only, no deletions.
impl DiskApp {
    fn assert_scan_results_empty(&self, cx: &App) {
        assert!(self.data.is_none());
        assert!(self.rows.is_empty() && self.tiles.is_empty());
        assert!(self.pending_query.is_none() && self.pending_map.is_none());
        assert!(!self.querying && self.sync_path.is_none());
        assert!(self.marked.is_empty() && self.mark_plan.roots.is_empty());
        assert!(!self.history.can_back() && !self.history.can_forward());
        assert_eq!((self.scope, self.map_root, self.selected), (0, 0, 0));
        assert!(self.pending_scroll.is_none() && !self.locate_pending);
        let table = self.table.read(cx);
        assert!(table.delegate().snapshot.is_none());
        assert!(table.delegate().index.is_none() && table.delegate().rows.is_empty());
        assert!(table.selected_row().is_none());
        let types = self.types.read(cx);
        assert!(types.delegate().index.is_none() && types.delegate().order.is_empty());
        assert!(types.selected_row().is_none());
    }
    fn schedule_scan_reset_smoke(window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |app, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(80)).await;
                if app.update_in(cx, |app, _, _| app.data.is_some() && app.job.is_none() && !app.querying && !app.tiles.is_empty()).unwrap() { break; }
            }
            let (new_path, real_scan_rx, held_tx) = app.update_in(cx, |app, window, cx| {
                let snapshot = app.data.as_ref().expect("initial fixture scan").snapshot.clone();
                let alpha = snapshot.nodes.iter().position(|node| node.name == "alpha").unwrap();
                let beta = snapshot.nodes.iter().position(|node| node.name == "beta").unwrap();
                app.toggle_mark(beta, cx);
                app.navigate_to(alpha, window, cx);
                assert!(app.history.can_back());
                assert!(!app.types.read(cx).delegate().order.is_empty());
                let stale_query_version = app.query_version.load(Ordering::Relaxed);
                let stale_map_version = app.map_version;
                let old_rows = app.rows.clone();
                let old_tiles = app.tiles.clone();
                let new_path = snapshot.path(beta);
                app.sync_path = Some(snapshot.root.clone());
                // Exercise the same completion handler as the real folder picker.
                app.finish_folder_choice(Ok(Some(vec![new_path.clone()])), window, cx);
                assert!(app.job.is_some());
                app.assert_scan_results_empty(cx);
                assert!(app.query_version.load(Ordering::Relaxed) > stale_query_version);
                assert!(app.map_version > stale_map_version);
                assert_eq!(app.path.read(cx).value().as_str(), pretty_path(&new_path));
                // Hold only delivery of the real result, for an observable empty frame.
                let (held_tx, held_rx) = mpsc::channel();
                let real_scan_rx = std::mem::replace(&mut app.job.as_mut().unwrap().rx, held_rx);
                // Simulate late replies from the previous snapshot via the production poll.
                let (query_tx, query_rx) = mpsc::channel();
                let (map_tx, map_rx) = mpsc::channel();
                query_tx.send(QueryResult { version: stale_query_version, rows: old_rows }).unwrap();
                map_tx.send(MapResult { version: stale_map_version, tiles: old_tiles }).unwrap();
                let original_query_rx = std::mem::replace(&mut app.query_rx, query_rx);
                let original_map_rx = std::mem::replace(&mut app.map_rx, map_rx);
                app.poll(cx);
                app.assert_scan_results_empty(cx);
                app.query_rx = original_query_rx;
                app.map_rx = original_map_rx;
                eprintln!("[scan-reset-smoke] folder selection cleared all results; stale replies rejected");
                (new_path, real_scan_rx, held_tx)
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(350)).await;
            interaction_smoke_stage("scan-empty");
            cx.background_executor().timer(Duration::from_millis(1700)).await;
            app.update_in(cx, |app, _, cx| {
                app.assert_scan_results_empty(cx);
                app.job.as_mut().unwrap().rx = real_scan_rx;
            }).unwrap();
            drop(held_tx);
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(80)).await;
                if app.update_in(cx, |app, _, _| app.data.is_some() && app.job.is_none() && !app.querying && !app.tiles.is_empty()).unwrap() { break; }
            }
            app.update_in(cx, |app, _, cx| {
                let snapshot = &app.data.as_ref().expect("new folder result missing").snapshot;
                assert_eq!(snapshot.root, std::fs::canonicalize(&new_path).unwrap());
                assert_eq!(snapshot.files, 1);
                assert!(!app.rows.is_empty() && !app.tiles.is_empty());
                assert!(!app.types.read(cx).delegate().order.is_empty());
                app.start_scan(cx); // Same-folder rescan must clear too.
                app.assert_scan_results_empty(cx);
            }).unwrap();
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(80)).await;
                if app.update_in(cx, |app, _, _| app.data.is_some() && app.job.is_none()).unwrap() { break; }
            }
            app.update_in(cx, |app, window, cx| {
                assert!(app.data.is_some());
                let missing = new_path.join("missing-scan-reset-target");
                assert!(!missing.exists());
                app.set_path(missing, window, cx);
                app.assert_scan_results_empty(cx);
            }).unwrap();
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(80)).await;
                if app.update_in(cx, |app, _, _| app.job.is_none()).unwrap() { break; }
            }
            app.update_in(cx, |app, _, cx| {
                app.assert_scan_results_empty(cx);
                assert!(app.status.starts_with("扫描失败："), "{}", app.status);
                eprintln!("[scan-reset-smoke] passed: new folder, rescan, stale query/map rejection, failed scan remains empty");
            }).unwrap();
            interaction_smoke_stage("scan-failed");
            cx.background_executor().timer(Duration::from_millis(1300)).await;
            app.update_in(cx, |_, window, _| window.remove_window()).unwrap();
        }).detach();
    }
}
