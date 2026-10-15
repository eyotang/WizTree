// Render the actual native GPUI scene, not a separate mockup.
impl DiskApp {
    fn schedule_appearance_smoke(window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |app, cx| {
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
