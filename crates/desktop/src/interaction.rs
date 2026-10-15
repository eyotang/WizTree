//! Navigation and cleanup selection semantics, independent of the UI runtime.
use disk_core::Snapshot;
use std::collections::HashSet;

pub struct History<T> {
    entries: Vec<T>,
    cursor: usize,
}
impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
        }
    }
}
impl<T: Clone> History<T> {
    pub fn reset(&mut self, value: T) {
        self.entries = vec![value];
        self.cursor = 0;
    }
    pub fn can_back(&self) -> bool {
        self.cursor > 0
    }
    pub fn can_forward(&self) -> bool {
        self.cursor + 1 < self.entries.len()
    }
    pub fn push(&mut self, current: T, next: T) {
        if self.entries.is_empty() {
            self.reset(current);
        } else {
            self.entries[self.cursor] = current;
        }
        self.entries.truncate(self.cursor + 1);
        self.entries.push(next);
        if self.entries.len() > 64 {
            self.entries.remove(0);
        }
        self.cursor = self.entries.len() - 1;
    }
    pub fn back(&mut self, current: T) -> Option<T> {
        if !self.can_back() {
            return None;
        }
        self.entries[self.cursor] = current;
        self.cursor -= 1;
        Some(self.entries[self.cursor].clone())
    }
    pub fn forward(&mut self, current: T) -> Option<T> {
        if !self.can_forward() {
            return None;
        }
        self.entries[self.cursor] = current;
        self.cursor += 1;
        Some(self.entries[self.cursor].clone())
    }
}
#[derive(Clone, Default, Debug)]
pub struct MarkPlan {
    pub roots: Vec<usize>,
    pub allocated: u64,
}
pub fn covered(snapshot: &Snapshot, marked: &HashSet<usize>, mut id: usize) -> bool {
    while id != 0 {
        let Some(node) = snapshot.nodes.get(id) else {
            return false;
        };
        if marked.contains(&id) {
            return true;
        }
        let Some(parent) = node.parent else {
            return false;
        };
        id = parent;
    }
    false
}
pub fn mark_plan(snapshot: &Snapshot, marked: &HashSet<usize>) -> MarkPlan {
    let mut plan = MarkPlan::default();
    for &id in marked {
        if id == 0 || id >= snapshot.nodes.len() {
            continue;
        }
        let parent = snapshot.nodes[id].parent.unwrap_or(0);
        if covered(snapshot, marked, parent) {
            continue;
        }
        plan.roots.push(id);
        plan.allocated = plan.allocated.saturating_add(snapshot.nodes[id].allocated);
    }
    plan.roots.sort_unstable_by(|a, b| {
        snapshot.nodes[*b]
            .allocated
            .cmp(&snapshot.nodes[*a].allocated)
            .then(a.cmp(b))
    });
    plan
}
/// Resolve a reviewed snapshot item without following replaced parent links.
pub fn trash_target(snapshot: &Snapshot, id: usize) -> std::io::Result<std::path::PathBuf> {
    let fail = || std::io::Error::other("项目路径或类型已改变，请重新扫描后操作");
    let node = snapshot
        .nodes
        .get(id)
        .filter(|_| id != 0)
        .ok_or_else(fail)?;
    let root = std::fs::canonicalize(&snapshot.root)?;
    if root != snapshot.root {
        return Err(fail());
    }
    let path = snapshot.path(id);
    let parent = std::fs::canonicalize(path.parent().ok_or_else(fail)?)?;
    if !parent.starts_with(&root) || parent != path.parent().unwrap() {
        return Err(fail());
    }
    let metadata = std::fs::symlink_metadata(&path)?;
    if metadata.is_symlink() || metadata.is_dir() != node.is_dir {
        return Err(fail());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(fail());
        }
    }
    Ok(path)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn back_forward_restore_changed_selection_scroll_and_new_branches() {
        let mut h = History::default();
        h.reset((0, 0, 0));
        h.push((0, 7, 80), (7, 7, 0));
        h.push((7, 9, 40), (9, 9, 0));
        assert_eq!(h.back((9, 10, 20)), Some((7, 9, 40)));
        assert_eq!(h.back((7, 8, 60)), Some((0, 7, 80)));
        assert_eq!(h.back((0, 7, 80)), None);
        assert_eq!(h.forward((0, 8, 100)), Some((7, 8, 60)));
        assert_eq!(h.forward((7, 9, 40)), Some((9, 10, 20)));
        h.back((9, 9, 0));
        h.push((7, 9, 40), (12, 12, 0));
        assert!(!h.can_forward());
        assert!(h.can_back());
        h.reset((20, 20, 0));
        assert!(!h.can_back());
    }
    #[test]
    fn navigation_retains_at_most_sixty_four_locations() {
        let mut h = History::default();
        h.reset(0);
        for i in 1..100 {
            h.push(i - 1, i);
        }
        let mut count = 0;
        while h.back(99).is_some() {
            count += 1;
        }
        assert_eq!(count, 63);
    }
    #[test]
    fn cleanup_rejects_root_missing_items_and_changed_types_without_deleting() {
        use disk_core::{Progress, ScanOptions, scan};
        use std::sync::atomic::AtomicBool;
        let root = std::env::temp_dir().join(format!(
            "wiztree-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::write(root.join("file"), b"abc").unwrap();
        let snapshot = scan(
            &root,
            &ScanOptions { workers: 1 },
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        let id = snapshot
            .nodes
            .iter()
            .position(|node| node.name == "file")
            .unwrap();
        assert!(trash_target(&snapshot, 0).is_err());
        assert!(trash_target(&snapshot, usize::MAX).is_err());
        assert_eq!(trash_target(&snapshot, id).unwrap(), snapshot.path(id));
        assert_eq!(std::fs::read(root.join("file")).unwrap(), b"abc");
        std::fs::remove_file(root.join("file")).unwrap();
        assert!(trash_target(&snapshot, id).is_err());
        std::fs::create_dir(root.join("file")).unwrap();
        assert!(trash_target(&snapshot, id).is_err());
    }
    #[test]
    fn marks_deduplicate_ancestors_ignore_root_and_stale_ids() {
        use disk_core::{Node, ScanMethod};
        let make = |parent, allocated, is_dir| Node {
            parent,
            name: "item".into(),
            size: allocated,
            allocated,
            is_dir,
            children: vec![],
            modified: None,
        };
        let snapshot = Snapshot {
            root: "test".into(),
            nodes: vec![
                make(None, 90, true),
                make(Some(0), 60, true),
                make(Some(1), 20, false),
                make(Some(1), 40, false),
                make(Some(0), 30, false),
            ],
            elapsed: Default::default(),
            files: 3,
            dirs: 2,
            errors: 0,
            cancelled: false,
            method: ScanMethod::ParallelWalk,
            fallback_reason: None,
            issues: vec![],
        };
        let marked = HashSet::from([0, 1, 2, 4, 999]);
        let plan = mark_plan(&snapshot, &marked);
        assert_eq!(plan.roots, vec![1, 4]);
        assert_eq!(plan.allocated, 90);
        assert!(covered(&snapshot, &marked, 3));
        assert!(!covered(&snapshot, &marked, 0));
        let plan = mark_plan(&snapshot, &HashSet::from([2, 3]));
        assert_eq!(plan.roots, vec![3, 2]);
        assert_eq!(plan.allocated, 60);
    }
}
