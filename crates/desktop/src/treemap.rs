//! Hierarchical, pixel-aware squarified treemap. Public coordinates are normalized.
use disk_core::Snapshot;
use std::{cmp::Ordering, collections::BinaryHeap};

#[derive(Clone, Copy, Debug)]
pub struct Tile {
    pub id: Option<usize>,
    pub bytes: u64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}
impl Rect {
    fn area(self) -> f64 {
        self.w * self.h
    }
}
#[derive(Clone, Copy)]
struct Item {
    id: Option<usize>,
    bytes: u64,
    weight: f64,
}
#[derive(Clone, Copy)]
struct Cell {
    item: Item,
    rect: Rect,
}
impl Cell {
    fn tile(self, width: f64, height: f64) -> Tile {
        Tile {
            id: self.item.id,
            bytes: self.item.bytes,
            x: (self.rect.x / width) as f32,
            y: (self.rect.y / height) as f32,
            w: (self.rect.w / width) as f32,
            h: (self.rect.h / height) as f32,
        }
    }
}

/// Compatibility API: a flat squarified treemap for a square viewport.
#[allow(dead_code)] // Flat-layout compatibility API; production uses the hierarchical path.
pub fn layout(values: &[(Option<usize>, u64)]) -> Vec<Tile> {
    let items = values
        .iter()
        .filter(|(_, bytes)| *bytes > 0)
        .map(|&(id, bytes)| Item {
            id,
            bytes,
            weight: bytes as f64,
        })
        .collect();
    squarify(
        items,
        Rect {
            x: 0.,
            y: 0.,
            w: 1.,
            h: 1.,
        },
    )
    .into_iter()
    .map(|cell| cell.tile(1., 1.))
    .collect()
}

#[derive(Clone, Copy)]
struct Pending {
    depth: usize,
    slot: usize,
    area: f64,
}
impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot && self.area == other.area
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        self.area
            .total_cmp(&other.area)
            .then_with(|| other.slot.cmp(&self.slot))
    }
}

/// Expand the largest visible directories first, down to actual file IDs.
///
/// Tiny children and children that cannot fit the global tile budget are retained
/// in one aggregate cell bearing their parent directory's original ID. Its bytes
/// represent only the unexpanded remainder. No positive-size sibling is dropped.
/// The 16 square-pixel threshold and hard 6000-cell ceiling bound drawing cost.
/// Both aspect-ratio decisions and culling use the real viewport dimensions.
/// Directory aggregate metrics may saturate u64; child sums use u128 before
/// converting to geometric weights, avoiding overflow even for extreme values.
pub fn hierarchical_layout(
    snapshot: &Snapshot,
    root: usize,
    metric_allocated: bool,
    width: f32,
    height: f32,
    max_tiles: usize,
) -> Vec<Tile> {
    hierarchical_layout_at_depth(
        snapshot,
        root,
        metric_allocated,
        width,
        height,
        max_tiles,
        usize::MAX,
    )
}
/// Stop expanding after `depth` levels while preserving aggregate cell area.
pub fn hierarchical_layout_at_depth(
    snapshot: &Snapshot,
    root: usize,
    metric_allocated: bool,
    width: f32,
    height: f32,
    max_tiles: usize,
    depth: usize,
) -> Vec<Tile> {
    let budget = max_tiles.min(6000);
    if budget == 0 || !width.is_finite() || !height.is_finite() || width <= 0. || height <= 0. {
        return Vec::new();
    }
    let Some(node) = snapshot.nodes.get(root) else {
        return Vec::new();
    };
    let metric = |id: usize| {
        let node = &snapshot.nodes[id];
        if metric_allocated {
            node.allocated
        } else {
            node.size
        }
    };
    let bytes = metric(root);
    if bytes == 0 {
        return Vec::new();
    }
    let (width, height) = (f64::from(width), f64::from(height));
    let rect = Rect {
        x: 0.,
        y: 0.,
        w: width,
        h: height,
    };
    let mut cells = vec![Cell {
        item: Item {
            id: Some(root),
            bytes,
            weight: bytes as f64,
        },
        rect,
    }];
    let mut pending = BinaryHeap::new();
    if node.is_dir {
        pending.push(Pending {
            depth: 0,
            slot: 0,
            area: rect.area(),
        });
    }
    // Iteration also handles very deep, single-child directory chains without
    // consuming the call stack. Snapshot's scanner guarantees an acyclic tree.
    while let Some(Pending {
        slot, depth: level, ..
    }) = pending.pop()
    {
        if level >= depth {
            continue;
        }
        let cell = cells[slot];
        let id = cell.item.id.unwrap();
        let node = &snapshot.nodes[id];
        if !node.is_dir || cell.rect.w < 2. || cell.rect.h < 2. {
            continue;
        }
        let mut children: Vec<_> = node
            .children
            .iter()
            .copied()
            .filter(|&child| child < snapshot.nodes.len() && metric(child) > 0)
            .map(|child| (child, metric(child)))
            .collect();
        if children.is_empty() {
            continue;
        }
        let child_sum: u128 = children.iter().map(|(_, bytes)| u128::from(*bytes)).sum();
        // Retain any directory-owned bytes too (e.g. incomplete snapshots).
        let total = child_sum.max(u128::from(metric(id)));
        let own = total - child_sum;
        let available = budget - cells.len() + 1;
        let child_count = children.len();
        // A directory with millions of entries needs only a linear selection
        // pass, followed by sorting at most the remaining 6000 visible cells.
        if child_count > available {
            children.select_nth_unstable_by(available, |a, b| {
                b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0))
            });
            children.truncate(available);
        }
        children.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let eligible = children
            .iter()
            .take_while(|(_, bytes)| cell.rect.area() * (*bytes as f64 / total as f64) >= 16.)
            .count();
        let mut visible = eligible.min(available);
        if visible == available && (visible < child_count || own > 0) {
            visible = visible.saturating_sub(1);
        }
        if visible == 0 {
            continue;
        }
        let selected_sum: u128 = children[..visible]
            .iter()
            .map(|(_, bytes)| u128::from(*bytes))
            .sum();
        let mut items: Vec<_> = children[..visible]
            .iter()
            .map(|&(child, bytes)| Item {
                id: Some(child),
                bytes,
                weight: bytes as f64,
            })
            .collect();
        let remainder = total - selected_sum;
        if remainder > 0 {
            items.push(Item {
                id: Some(id),
                bytes: remainder.min(u128::from(u64::MAX)) as u64,
                weight: remainder as f64,
            });
        }
        let expanded = squarify(items, cell.rect);
        for (index, child_cell) in expanded.into_iter().enumerate() {
            let child_slot = if index == 0 {
                cells[slot] = child_cell;
                slot
            } else {
                cells.push(child_cell);
                cells.len() - 1
            };
            let child = child_cell.item.id.unwrap();
            // A remainder cell is terminal for this viewport, but still clickable
            // as its original directory ID, allowing a new focused layout.
            if child != id && snapshot.nodes[child].is_dir {
                pending.push(Pending {
                    depth: level + 1,
                    slot: child_slot,
                    area: child_cell.rect.area(),
                });
            }
        }
    }
    cells
        .into_iter()
        .map(|cell| cell.tile(width, height))
        .collect()
}

fn worst(row: &[f64], side: f64) -> f64 {
    if row.is_empty() || side <= 0. {
        return f64::INFINITY;
    }
    let sum: f64 = row.iter().sum();
    let min = row.iter().copied().fold(f64::INFINITY, f64::min);
    let max = row.iter().copied().fold(0., f64::max);
    let squared = sum * sum;
    (side * side * max / squared).max(squared / (side * side * min))
}

fn squarify(mut items: Vec<Item>, mut rect: Rect) -> Vec<Cell> {
    items.retain(|item| item.weight.is_finite() && item.weight > 0.);
    if items.is_empty() || rect.area() <= 0. {
        return Vec::new();
    }
    items.sort_unstable_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.id.cmp(&b.id)));
    let total: f64 = items.iter().map(|item| item.weight).sum();
    let scale = rect.area() / total;
    let areas: Vec<_> = items.iter().map(|item| item.weight * scale).collect();
    let mut out = Vec::with_capacity(items.len());
    let mut start = 0;
    while start < items.len() {
        let side = rect.w.min(rect.h);
        let mut end = start + 1;
        while end < items.len()
            && worst(&areas[start..=end], side) <= worst(&areas[start..end], side)
        {
            end += 1;
        }
        let sum: f64 = areas[start..end].iter().sum();
        if rect.w >= rect.h {
            let strip = if end == items.len() {
                rect.w
            } else {
                (sum / rect.h).min(rect.w)
            };
            let mut y = rect.y;
            for index in start..end {
                let h = if index + 1 == end {
                    (rect.y + rect.h - y).max(0.)
                } else {
                    (areas[index] / strip).min(rect.y + rect.h - y).max(0.)
                };
                out.push(Cell {
                    item: items[index],
                    rect: Rect {
                        x: rect.x,
                        y,
                        w: strip,
                        h,
                    },
                });
                y += h;
            }
            rect.x += strip;
            rect.w = (rect.w - strip).max(0.);
        } else {
            let strip = if end == items.len() {
                rect.h
            } else {
                (sum / rect.w).min(rect.h)
            };
            let mut x = rect.x;
            for index in start..end {
                let w = if index + 1 == end {
                    (rect.x + rect.w - x).max(0.)
                } else {
                    (areas[index] / strip).min(rect.x + rect.w - x).max(0.)
                };
                out.push(Cell {
                    item: items[index],
                    rect: Rect {
                        x,
                        y: rect.y,
                        w,
                        h: strip,
                    },
                });
                x += w;
            }
            rect.y += strip;
            rect.h = (rect.h - strip).max(0.);
        }
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_core::{Node, ScanMethod};
    use std::{path::PathBuf, time::Duration};

    fn node(size: u64, children: Vec<usize>) -> Node {
        Node {
            parent: None,
            name: "test".into(),
            size,
            allocated: size,
            is_dir: !children.is_empty(),
            modified: None,
            children,
        }
    }
    fn snapshot(nodes: Vec<Node>) -> Snapshot {
        Snapshot {
            root: PathBuf::from("test"),
            nodes,
            elapsed: Duration::ZERO,
            files: 0,
            dirs: 0,
            errors: 0,
            cancelled: false,
            method: ScanMethod::ParallelWalk,
            fallback_reason: None,
            issues: Vec::new(),
        }
    }
    fn assert_covers(tiles: &[Tile]) {
        let area: f64 = tiles.iter().map(|t| f64::from(t.w) * f64::from(t.h)).sum();
        assert!((area - 1.).abs() < 0.00002, "area={area}");
        for tile in tiles {
            assert!(
                tile.x.is_finite()
                    && tile.y.is_finite()
                    && tile.w.is_finite()
                    && tile.h.is_finite()
            );
            assert!(tile.x >= 0. && tile.y >= 0. && tile.w > 0. && tile.h > 0.);
            assert!(tile.x + tile.w <= 1.00001 && tile.y + tile.h <= 1.00001);
        }
        // Geometric overlap would invalidate coverage even if total area is one.
        if tiles.len() < 200 {
            for (index, a) in tiles.iter().enumerate() {
                for b in &tiles[index + 1..] {
                    let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                    let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                    assert!(w <= 0.00001 || h <= 0.00001);
                }
            }
        }
    }
    #[test]
    fn areas_preserve_sizes_and_stay_in_bounds() {
        let tiles = layout(&[(Some(0), 60), (Some(1), 25), (Some(2), 15), (Some(3), 0)]);
        assert_eq!(tiles.len(), 3);
        for t in &tiles {
            assert!((t.w * t.h - t.bytes as f32 / 100.).abs() < 0.00001);
        }
        assert_covers(&tiles);
    }
    #[test]
    fn depth_control_preserves_area_and_exposes_folder_before_files() {
        let tree = snapshot(vec![
            node(30, vec![1, 4]),
            node(20, vec![2, 3]),
            node(8, vec![]),
            node(12, vec![]),
            node(10, vec![]),
        ]);
        let shallow = hierarchical_layout_at_depth(&tree, 0, false, 1000., 500., 6000, 1);
        assert_covers(&shallow);
        assert_eq!(shallow.iter().map(|tile| tile.bytes).sum::<u64>(), 30);
        assert!(shallow.iter().any(|tile| tile.id == Some(1)));
        assert!(!shallow.iter().any(|tile| tile.id == Some(2)));
        let deep = hierarchical_layout_at_depth(&tree, 0, false, 1000., 500., 6000, 2);
        assert_covers(&deep);
        assert_eq!(deep.iter().map(|tile| tile.bytes).sum::<u64>(), 30);
        assert!(deep.iter().any(|tile| tile.id == Some(2)));
        assert!(deep.iter().any(|tile| tile.id == Some(3)));
        let root = hierarchical_layout_at_depth(&tree, 0, false, 1000., 500., 6000, 0);
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].id, Some(0));
    }
    #[test]
    fn nested_root_displays_descendant_files() {
        let tree = snapshot(vec![
            node(100, vec![1]),
            node(100, vec![2, 3]),
            node(60, vec![]),
            node(40, vec![]),
        ]);
        let tiles = hierarchical_layout(&tree, 0, false, 1200., 600., 6000);
        assert_eq!(tiles.len(), 2);
        assert!(tiles.iter().all(|tile| matches!(tile.id, Some(2 | 3))));
        assert_covers(&tiles);
        assert_eq!(
            hierarchical_layout(&tree, 1, false, 1200., 600., 6000).len(),
            2
        );
    }
    #[test]
    fn budget_and_pixel_culling_preserve_all_remaining_bytes() {
        let mut nodes = vec![node(10_000, (1..=10_000).collect())];
        nodes.extend((0..10_000).map(|_| node(1, vec![])));
        let tree = snapshot(nodes);
        for (width, budget) in [(10., 6000), (2000., 17), (2000., 1), (2000., usize::MAX)] {
            let tiles = hierarchical_layout(&tree, 0, false, width, 1000., budget);
            assert!(tiles.len() <= budget.min(6000));
            assert_eq!(tiles.iter().map(|tile| tile.bytes).sum::<u64>(), 10_000);
            assert!(tiles.iter().any(|tile| tile.id == Some(0)));
            assert_covers(&tiles);
        }
    }
    #[test]
    fn descendants_are_not_truncated_to_top_64() {
        let mut nodes = vec![node(128, vec![1]), node(128, (2..130).collect())];
        nodes.extend((0..128).map(|_| node(1, vec![])));
        let tiles = hierarchical_layout(&snapshot(nodes), 0, false, 1600., 800., 6000);
        assert_eq!(tiles.len(), 128);
        assert!(tiles.iter().all(|tile| tile.id.unwrap() >= 2));
        assert_covers(&tiles);
    }
    #[test]
    fn deep_chains_and_directory_owned_remainders_are_preserved() {
        let depth = 10_000;
        let mut nodes: Vec<_> = (0..depth).map(|id| node(10, vec![id + 1])).collect();
        nodes.push(node(10, vec![]));
        let tiles = hierarchical_layout(&snapshot(nodes), 0, false, 100., 100., 1);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].id, Some(depth));
        assert_covers(&tiles);
        let tree = snapshot(vec![
            node(100, vec![1, 2]),
            node(60, vec![]),
            node(20, vec![]),
        ]);
        let tiles = hierarchical_layout(&tree, 0, false, 1000., 1000., 10);
        assert_eq!(tiles.iter().map(|tile| tile.bytes).sum::<u64>(), 100);
        assert_eq!(
            tiles.iter().find(|tile| tile.id == Some(0)).unwrap().bytes,
            20
        );
        assert_covers(&tiles);
    }
    #[test]
    fn allocated_metric_and_real_aspect_ratio() {
        let mut tree = snapshot(vec![
            node(100, vec![1, 2]),
            node(50, vec![]),
            node(50, vec![]),
        ]);
        tree.nodes[0].allocated = 200;
        tree.nodes[1].allocated = 150;
        tree.nodes[2].allocated = 50;
        let tiles = hierarchical_layout(&tree, 0, true, 1600., 200., 10);
        let first = tiles.iter().find(|tile| tile.id == Some(1)).unwrap();
        assert!((first.w * first.h - 0.75).abs() < 0.00001);
        assert_covers(&tiles);
        let equal = hierarchical_layout(&tree, 0, false, 1600., 200., 10);
        assert!(
            equal
                .iter()
                .all(|tile| tile.h > 0.99 && (tile.w - 0.5).abs() < 0.00001)
        );
    }
    #[test]
    fn empty_and_extreme_values() {
        assert!(layout(&[]).is_empty());
        assert_covers(&layout(&[(None, u64::MAX), (Some(1), u64::MAX)]));
        let tree = snapshot(vec![
            node(u64::MAX, vec![1, 2, 3]),
            node(u64::MAX, vec![]),
            node(u64::MAX, vec![]),
            node(0, vec![]),
        ]);
        let tiles = hierarchical_layout(&tree, 0, false, 1200., 600., usize::MAX);
        assert_eq!(tiles.len(), 2);
        assert_covers(&tiles);
        assert!(hierarchical_layout(&tree, usize::MAX, false, 1200., 600., 20).is_empty());
        assert!(hierarchical_layout(&tree, 0, false, f32::NAN, 600., 20).is_empty());
        assert!(hierarchical_layout(&tree, 0, false, 1200., 0., 20).is_empty());
        assert!(hierarchical_layout(&tree, 0, false, 1200., 600., 0).is_empty());
        assert!(
            hierarchical_layout(&snapshot(vec![node(0, vec![])]), 0, false, 10., 10., 10)
                .is_empty()
        );
    }
}
