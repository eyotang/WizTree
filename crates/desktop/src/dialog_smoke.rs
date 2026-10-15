// Explicit application-internal native-dialog integration check.
// Enabled only with `--features dialog-smoke -- --smoke-folder-dialog`.
// No external input injection or changes to any user files.
impl DiskApp {
    fn schedule_folder_dialog_smoke(window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |app, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            for round in 1..=2 {
                app.update_in(cx, |app, window, cx| {
                    app.choose(window, cx);
                    assert!(app.folder_picker_open);
                    // Re-entering the handler must not open another picker.
                    app.choose(window, cx);
                    eprintln!("[dialog-smoke] requested round={round}");
                })
                .unwrap();
                // Keep GPUI's normal 80ms poll running while Windows owns the
                // native modal loop. The previous synchronous handler aborted here.
                cx.background_executor()
                    .timer(Duration::from_millis(if round == 1 { 12000 } else { 800 }))
                    .await;
                app.update_in(cx, |app, window, cx| {
                    assert!(
                        app.folder_picker_open,
                        "native dialog finished unexpectedly"
                    );
                    app.folder_picker_task.take(); // Cancel the native request via its receiver.
                    let previous_path = app.path.read(cx).value().to_string();
                    app.finish_folder_choice(Ok(None), window, cx);
                    assert_eq!(app.path.read(cx).value().as_str(), previous_path);
                    assert!(app.job.is_none());
                    assert!(!app.folder_picker_open);
                    eprintln!("[dialog-smoke] cancelled round={round}");
                })
                .unwrap();
                cx.background_executor()
                    .timer(Duration::from_millis(600))
                    .await;
            }
            if let Some(path) = std::env::var_os("WIZTREE_SMOKE_FOLDER").map(PathBuf::from) {
                app.update_in(cx, |app, window, cx| {
                    // Exercise the exact production selection-completion handler.
                    // The selected path is injected, not clicked in the OS dialog.
                    app.finish_folder_choice(Ok(Some(vec![path.clone()])), window, cx);
                    assert!(app.job.is_some());
                })
                .unwrap();
                for _ in 0..50 {
                    cx.background_executor()
                        .timer(Duration::from_millis(80))
                        .await;
                    if app.update_in(cx, |app, _, _| app.job.is_none()).unwrap() {
                        break;
                    }
                }
                app.update_in(cx, |app, window, cx| {
                    assert!(app.job.is_none());
                    let snapshot = &app
                        .data
                        .as_ref()
                        .expect("selected folder not installed")
                        .snapshot;
                    assert_eq!(snapshot.root, std::fs::canonicalize(path).unwrap());
                    assert_eq!(
                        (snapshot.files, snapshot.errors, snapshot.nodes[0].size),
                        (1, 0, 3)
                    );
                    eprintln!("[dialog-smoke] selection-completion scan passed");
                    let previous_path = app.path.read(cx).value().to_string();
                    app.finish_folder_choice(Err("injected dialog error".into()), window, cx);
                    assert_eq!(app.path.read(cx).value().as_str(), previous_path);
                    assert!(app.status.contains("injected dialog error"));
                    assert!(!app.folder_picker_open);
                })
                .unwrap();
            }
            app.update_in(cx, |app, window, cx| {
                app.choose(window, cx);
                assert!(app.folder_picker_open);
                eprintln!("[dialog-smoke] closing owner with active request");
            })
            .unwrap();
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            app.update_in(cx, |_, window, _| {
                eprintln!("[dialog-smoke] passed; closing test window");
                window.remove_window();
            })
            .unwrap();
        })
        .detach();
    }
}
