// Application-internal integration checks. Never confirms filesystem mutations.
impl DiskApp {
    fn schedule_interaction_smoke(window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |app,cx| {
            for _ in 0..100 {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if app.update_in(cx,|app,_,_| app.data.is_some() && app.job.is_none() && !app.querying && !app.tiles.is_empty()).unwrap() { break; }
            }
            let (snapshot,alpha,beta,leaf)=app.update_in(cx,|app,window,cx| {
                let data=app.data.as_ref().expect("fixture scan not complete");
                let snapshot=data.snapshot.clone();
                let find=|name:&str| snapshot.nodes.iter().position(|node|node.name==name).unwrap();
                let (alpha,beta,leaf)=(find("alpha"),find("beta"),find("a.txt"));
                app.select_map_node(alpha,window,cx);
                assert_eq!((app.scope,app.map_root,app.selected),(0,0,alpha));
                let key=KeyDownEvent {keystroke:Keystroke { key:"space".into(),key_char:Some(" ".into()),..Default::default()},is_held:false,prefer_character_input:false};
                app.marking_key(&key,window,cx);
                app.marking_key(&KeyDownEvent{is_held:true,..key},window,cx);
                assert!(app.marked.contains(&alpha));
                app.toggle_mark(leaf,cx); app.toggle_mark(beta,cx);
                assert_eq!(app.marked.len(),3); assert_eq!(app.mark_plan.roots.len(),2);
                assert!(interaction::covered(&snapshot,&app.marked,leaf));
                (snapshot,alpha,beta,leaf)
            }).unwrap();
            // Dispatch through the actual rendered keyboard routing, outside
            // an entity lease, rather than calling only the application handler.
            app.update_in(cx,|app,window,cx| {
                app.toggle_mark(alpha,cx);
                window.focus(&app.map_focus,cx);
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let event=KeyDownEvent {keystroke:Keystroke {key:"space".into(),key_char:Some(" ".into()),..Default::default()},is_held:false,prefer_character_input:false};
            cx.update(|window,cx| { window.dispatch_event(PlatformInput::KeyDown(event.clone()),cx); }).unwrap();
            cx.update(|window,cx| { window.dispatch_event(PlatformInput::KeyDown(KeyDownEvent {is_held:true,..event.clone()}),cx); }).unwrap();
            app.update_in(cx,|app,window,cx| {
                assert!(app.marked.contains(&alpha),"map Space routing failed");
                assert_eq!(app.marked.len(),3,"held Space toggled again");
                app.search.update(cx,|input,cx|input.focus(window,cx));
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(100)).await;
            cx.update(|window,cx| { window.dispatch_event(PlatformInput::KeyDown(event),cx); }).unwrap();
            app.update_in(cx,|app,window,cx| {
                assert_eq!(app.marked.len(),3,"input Space triggered file marking");
                app.clear_filter(window,cx); window.focus(&app.map_focus,cx);
                eprintln!("[interaction-smoke] real map/input keyboard routing passed");
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            interaction_smoke_stage("details");
            cx.background_executor().timer(Duration::from_millis(1700)).await;
            app.update_in(cx,|app,window,cx| {
                app.table.read(cx).vertical_scroll_handle.0.borrow().base_handle.set_offset(point(px(0.),px(-280.)));
                app.navigate_to(alpha,window,cx);
                assert_eq!(app.scope,alpha); assert!(app.marked.contains(&beta));
                assert!(app.job.is_none()); assert!(Arc::ptr_eq(&snapshot,&app.data.as_ref().unwrap().snapshot));
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            app.update_in(cx,|app,window,cx| {
                app.select(leaf,cx); app.go_back(&GoBack,window,cx);
                assert_eq!((app.scope,app.selected),(0,alpha));
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(600)).await;
            app.update_in(cx,|app,window,cx| {
                let scroll=app.current_location(cx).scroll;
                assert!((f32::from(scroll.y)+280.).abs()<30.,"scroll not restored: {scroll:?}");
                app.go_forward(&GoForward,window,cx);
                assert_eq!((app.scope,app.selected),(alpha,leaf));
                app.navigate_to(beta,window,cx); assert!(!app.history.can_forward());
                app.go_back(&GoBack,window,cx); app.go_back(&GoBack,window,cx);
                assert_eq!(app.scope,0); assert!(app.job.is_none());
                assert_eq!(app.mark_plan.roots.len(),2);
                app.review_marks(&ReviewMarks,window,cx);
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            interaction_smoke_stage("review");
            cx.background_executor().timer(Duration::from_millis(1700)).await;
            app.update_in(cx,|app,_,cx| {
                app.map_only=true; app.map_depth=1; app.sidebar_tab=SidebarTab::Types;
                app.request_map(cx);
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            interaction_smoke_stage("map");
            cx.background_executor().timer(Duration::from_millis(1700)).await;
            app.update_in(cx,|app,window,cx| {
                assert_eq!(app.map_root,0); assert!(app.tiles.iter().any(|tile|tile.id==Some(alpha)));
                app.map_only=false; app.sidebar_tab=SidebarTab::Review;
                app.confirm_trash(app.mark_plan.roots.clone(),window,cx);
                assert!(app.cleanup_busy);
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            interaction_smoke_stage("confirm");
            cx.background_executor().timer(Duration::from_millis(1700)).await;
            app.update_in(cx,|app,window,cx| {
                assert!(app.cleanup_rx.is_none(),"mutation ran without confirmation");
                assert!(snapshot.path(leaf).exists()); assert!(snapshot.path(beta).exists());
                assert!(window.has_active_dialog(cx),"cleanup confirmation is not owned by the window");
            }).unwrap();
            let escape=KeyDownEvent {keystroke:Keystroke {key:"escape".into(),..Default::default()},is_held:false,prefer_character_input:false};
            cx.update(|window,cx| { window.dispatch_event(PlatformInput::KeyDown(escape),cx); }).unwrap();
            cx.background_executor().timer(Duration::from_millis(150)).await;
            app.update_in(cx,|app,window,cx| {
                assert!(!app.cleanup_busy && app.cleanup_rx.is_none(),"cancel did not release cleanup state");
                assert!(!window.has_active_dialog(cx));
                app.confirm_trash(app.mark_plan.roots.clone(),window,cx);
                assert!(app.cleanup_busy && window.has_active_dialog(cx));
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(150)).await;
            app.update_in(cx,|app,window,_| {
                assert!(app.cleanup_rx.is_none(),"mutation ran without confirmation");
                assert!(snapshot.path(leaf).exists()); assert!(snapshot.path(beta).exists());
                eprintln!("[interaction-smoke] passed: selection, held-space, marks, history, scroll, depth, review, cancel-and-reopen, close-with-confirmation; no filesystem mutations");
                window.remove_window();
            }).unwrap();
        }).detach();
    }
}
#[cfg(feature = "interaction-smoke")]
fn interaction_smoke_stage(stage: &str) {
    eprintln!("[interaction-smoke] stage={stage}");
    if let Some(directory) = std::env::var_os("WIZTREE_SMOKE_OUTPUT") {
        std::fs::write(PathBuf::from(directory).join("stage"), stage).unwrap();
    }
}
