use crate::{
    app::{DiskApp, ItemAction, pretty_path},
    query::{self, Mode, Row, SortKey},
};
use disk_core::{AnalysisIndex, Snapshot};
use gpui_kit::component::{
    button::*,
    menu::{PopupMenu, PopupMenuItem},
    table::*,
    *,
};
use gpui_kit::{prelude::FluentBuilder, *};
use std::{collections::HashSet, sync::Arc, time::SystemTime};
pub struct Explorer {
    pub owner: WeakEntity<DiskApp>,
    pub snapshot: Option<Arc<Snapshot>>,
    pub index: Option<Arc<AnalysisIndex>>,
    pub rows: Arc<Vec<Row>>,
    pub expanded: HashSet<usize>,
    pub marked: HashSet<usize>,
    pub columns: Vec<Column>,
    pub mode: Mode,
    pub selected: usize,
    pub scope: usize,
}
impl Explorer {
    pub fn new(owner: WeakEntity<DiskApp>) -> Self {
        Self {
            owner,
            snapshot: None,
            index: None,
            rows: Arc::new(vec![]),
            expanded: HashSet::new(),
            marked: HashSet::new(),
            columns: columns(Mode::Tree),
            mode: Mode::Tree,
            selected: 0,
            scope: 0,
        }
    }
    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode != mode {
            self.mode = mode;
            self.columns = columns(mode);
        }
    }
}
fn columns(mode: Mode) -> Vec<Column> {
    let mut out = vec![
        Column::new(
            "name",
            if mode == Mode::Tree {
                "文件夹 / 文件"
            } else {
                "文件名"
            },
        )
        .width(280.)
        .min_width(180.)
        .sortable()
        .fixed_left(),
        Column::new("percent", "占比")
            .width(88.)
            .sortable()
            .text_right(),
        Column::new("size", "大小")
            .width(100.)
            .sortable()
            .text_right(),
        Column::new("allocated", "实际占用")
            .width(100.)
            .sort(ColumnSort::Descending)
            .text_right(),
    ];
    if mode == Mode::Tree {
        out.extend([
            Column::new("items", "项目")
                .width(65.)
                .sortable()
                .text_right(),
            Column::new("files", "文件")
                .width(65.)
                .sortable()
                .text_right(),
            Column::new("folders", "文件夹")
                .width(65.)
                .sortable()
                .text_right(),
        ]);
    }
    out.push(Column::new("modified", "修改时间").width(145.).sortable());
    if mode == Mode::Files {
        out.push(Column::new("path", "完整路径").width(480.).sortable());
    }
    out
}
impl TableDelegate for Explorer {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }
    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }
    fn column(&self, col: usize, _: &App) -> Column {
        self.columns
            .get(col)
            .cloned()
            .unwrap_or_else(|| Column::new("", ""))
    }
    fn move_column(
        &mut self,
        from: usize,
        to: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) {
        if from >= self.columns.len() || to >= self.columns.len() {
            return;
        }
        let c = self.columns.remove(from);
        self.columns.insert(to, c);
    }
    fn perform_sort(
        &mut self,
        col: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(column) = self.columns.get(col) else {
            return;
        };
        let key = match column.key.as_ref() {
            "name" => SortKey::Name,
            "percent" => SortKey::Percent,
            "size" => SortKey::Size,
            "allocated" => SortKey::Allocated,
            "items" => SortKey::Items,
            "files" => SortKey::Files,
            "folders" => SortKey::Folders,
            "modified" => SortKey::Modified,
            _ => SortKey::Path,
        };
        let _ = self.owner.update(cx, |app, cx| {
            app.sort(key, !matches!(sort, ColumnSort::Ascending), cx)
        });
    }
    fn render_tr(
        &mut self,
        row: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let Some(entry) = self.rows.get(row) else {
            return div().id(("stale-row", row));
        };
        let id = entry.id;
        div()
            .id(("row", id))
            .when(
                self.snapshot
                    .as_ref()
                    .is_some_and(|s| crate::interaction::covered(s, &self.marked, id)),
                |d| d.bg(cx.theme().warning.opacity(0.12)),
            )
            .when(self.selected == id, |d| d.bg(cx.theme().selection))
    }
    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Virtual scrolling may request a preceding-frame row while an
        // asynchronous query replaces or shrinks the backing collection.
        let (Some(entry), Some(s), Some(index), Some(column)) = (
            self.rows.get(row).copied(),
            self.snapshot.as_ref(),
            self.index.as_ref(),
            self.columns.get(col),
        ) else {
            return div().into_any_element();
        };
        let (Some(node), Some(summary)) = (s.nodes.get(entry.id), index.nodes.get(entry.id)) else {
            return div().into_any_element();
        };
        let key = column.key.as_ref();
        if key == "name" {
            let id = entry.id;
            let owner = self.owner.clone();
            let expanded = self.expanded.contains(&id);
            return div()
                .flex()
                .items_center()
                .gap_2()
                .h_full()
                .min_w_0()
                .pl(px(entry.depth as f32 * 16.))
                .child({
                    let mark_owner = self.owner.clone();
                    let exact = self.marked.contains(&id);
                    let inherited = !exact && crate::interaction::covered(s, &self.marked, id);
                    Button::new(("mark-row", id))
                        .small()
                        .ghost()
                        .compact()
                        .w(px(20.))
                        .h(px(20.))
                        .label(if exact || inherited { "✓" } else { "○" })
                        .disabled(id == 0 || inherited)
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = mark_owner.update(cx, |app, cx| app.toggle_mark(id, cx));
                        })
                })
                .child(if self.mode == Mode::Tree && node.is_dir {
                    div()
                        .id(("expand", id))
                        .size(px(18.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|d| d.bg(cx.theme().muted))
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall(),
                        )
                        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = owner.update(cx, |app, cx| app.toggle(id, cx));
                        })
                        .into_any_element()
                } else {
                    div().w(px(18.)).flex_shrink_0().into_any_element()
                })
                .child(
                    Icon::new(if node.is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .small()
                    .text_color(if node.is_dir {
                        cx.theme().primary
                    } else {
                        rgb(query::color(&query::extension(&node.name))).into()
                    }),
                )
                .child(div().min_w_0().truncate().child(if entry.id == 0 {
                    pretty_path(&s.root)
                } else {
                    node.name.to_string_lossy().into_owned()
                }))
                .into_any_element();
        }
        if key == "percent" {
            let denominator = if self.mode == Mode::Tree {
                node.parent.unwrap_or(self.scope)
            } else {
                0
            };
            let total = s.nodes[denominator].allocated;
            let ratio = if total > 0 {
                node.allocated as f64 / total as f64
            } else {
                0.
            };
            return div()
                .flex()
                .items_center()
                .justify_end()
                .gap_2()
                .child(
                    div()
                        .w(px(38.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(cx.theme().muted)
                        .child(
                            div()
                                .h_full()
                                .w(relative(ratio.clamp(0., 1.) as f32))
                                .rounded_full()
                                .bg(cx.theme().primary),
                        ),
                )
                .child(format!("{:.1}%", ratio * 100.))
                .into_any_element();
        }
        let text = match key {
            "size" => bytes(node.size),
            "allocated" => bytes(node.allocated),
            "items" => number(summary.files + summary.dirs),
            "files" => number(summary.files),
            "folders" => number(summary.dirs),
            "modified" => date(node.modified),
            "path" => pretty_path(&s.path(entry.id)),
            _ => String::new(),
        };
        div().truncate().child(text).into_any_element()
    }
    fn context_menu(
        &mut self,
        row: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(entry) = self.rows.get(row) else {
            return menu;
        };
        let id = entry.id;
        let mut menu = menu;
        for (label, action) in [
            ("打开", ItemAction::Open),
            ("在文件管理器中显示", ItemAction::Reveal),
            ("聚焦此目录", ItemAction::Zoom),
            ("标记 / 取消标记", ItemAction::Mark),
            ("复制完整路径", ItemAction::Copy),
            ("导出此项 CSV", ItemAction::Export),
            ("移到回收站…", ItemAction::Trash),
        ] {
            let owner = self.owner.clone();
            menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = owner.update(cx, |app, cx| app.item_action(action, id, window, cx));
            }));
        }
        menu
    }
}
pub struct Extensions {
    pub index: Option<Arc<AnalysisIndex>>,
    pub order: Vec<usize>,
    pub columns: Vec<Column>,
}
impl Extensions {
    pub fn new(_owner: WeakEntity<DiskApp>) -> Self {
        Self {
            index: None,
            order: vec![],
            columns: vec![
                Column::new("extension", "扩展名").width(100.).sortable(),
                Column::new("percent", "占比")
                    .width(68.)
                    .sortable()
                    .text_right(),
                Column::new("size", "大小")
                    .width(84.)
                    .sortable()
                    .text_right(),
                Column::new("allocated", "实际占用")
                    .width(84.)
                    .sort(ColumnSort::Descending)
                    .text_right(),
                Column::new("files", "文件数")
                    .width(65.)
                    .sortable()
                    .text_right(),
            ],
        }
    }
}
impl TableDelegate for Extensions {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }
    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }
    fn column(&self, col: usize, _: &App) -> Column {
        self.columns
            .get(col)
            .cloned()
            .unwrap_or_else(|| Column::new("", ""))
    }
    fn perform_sort(
        &mut self,
        col: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if let Some(index) = &self.index {
            let Some(column) = self.columns.get(col) else {
                return;
            };
            let key = column.key.as_ref();
            self.order.sort_unstable_by(|&a, &b| {
                let a = &index.extensions[a];
                let b = &index.extensions[b];
                let cmp = match key {
                    "extension" => a.extension.cmp(&b.extension),
                    "files" => a.files.cmp(&b.files),
                    "allocated" => a.allocated.cmp(&b.allocated),
                    _ => a.size.cmp(&b.size),
                };
                if matches!(sort, ColumnSort::Ascending) {
                    cmp
                } else {
                    cmp.reverse()
                }
            });
            cx.notify();
        }
    }
    fn render_td(
        &mut self,
        row: usize,
        col: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (Some(index), Some(&id), Some(column)) = (
            self.index.as_ref(),
            self.order.get(row),
            self.columns.get(col),
        ) else {
            return div().into_any_element();
        };
        let Some(stat) = index.extensions.get(id) else {
            return div().into_any_element();
        };
        let key = column.key.as_ref();
        if key == "extension" {
            return div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .size(px(9.))
                        .rounded_sm()
                        .bg(rgb(query::color(&stat.extension))),
                )
                .child(if stat.extension.is_empty() {
                    "(无扩展名)".into()
                } else {
                    format!(".{}", stat.extension)
                })
                .into_any_element();
        }
        let text = match key {
            "size" => bytes(stat.size),
            "allocated" => bytes(stat.allocated),
            "files" => number(stat.files),
            _ => {
                let total = index.extensions.iter().map(|s| s.size).sum::<u64>();
                format!(
                    "{:.1}%",
                    if total > 0 {
                        stat.size as f64 / total as f64 * 100.
                    } else {
                        0.
                    }
                )
            }
        };
        div().child(text).into_any_element()
    }
}
pub fn bytes(n: u64) -> String {
    let u = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024. && i < u.len() - 1 {
        v /= 1024.;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.2} {}", u[i])
    }
}
pub fn number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
pub fn date(time: Option<SystemTime>) -> String {
    time.map_or("—".into(), |t| {
        let date: chrono::DateTime<chrono::Local> = t.into();
        date.format("%Y-%m-%d %H:%M").to_string()
    })
}
