//! Query state independent of rendering: all tree sorting/filtering runs off the UI thread.
use disk_core::{AnalysisIndex, Snapshot};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tree,
    Files,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Size,
    Allocated,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Percent,
    Size,
    Allocated,
    Items,
    Files,
    Folders,
    Modified,
    Path,
}
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub id: usize,
    pub depth: usize,
}
pub struct Query {
    pub version: u64,
    pub snapshot: Arc<Snapshot>,
    pub index: Arc<AnalysisIndex>,
    pub root: usize,
    pub mode: Mode,
    pub expanded: HashSet<usize>,
    pub query: String,
    pub extension: Option<String>,
    pub minimum: u64,
    pub sort: SortKey,
    pub descending: bool,
    pub limit: usize,
    pub match_paths: bool,
    pub include_folders: bool,
    pub exclude: String,
    pub duplicates: bool,
}
pub fn execute(request: &Query, generation: &AtomicU64) -> Option<Vec<Row>> {
    let s = &request.snapshot;
    let idx = &request.index;
    let compare = |a: &usize, b: &usize| {
        let a_node = &s.nodes[*a];
        let b_node = &s.nodes[*b];
        let order = match request.sort {
            SortKey::Name => a_node.name.cmp(&b_node.name),
            SortKey::Size | SortKey::Percent => a_node.size.cmp(&b_node.size),
            SortKey::Allocated => a_node.allocated.cmp(&b_node.allocated),
            SortKey::Items => (idx.nodes[*a].files + idx.nodes[*a].dirs)
                .cmp(&(idx.nodes[*b].files + idx.nodes[*b].dirs)),
            SortKey::Files => idx.nodes[*a].files.cmp(&idx.nodes[*b].files),
            SortKey::Folders => idx.nodes[*a].dirs.cmp(&idx.nodes[*b].dirs),
            SortKey::Modified => a_node.modified.cmp(&b_node.modified),
            SortKey::Path => s.path(*a).cmp(&s.path(*b)),
        };
        (if request.descending {
            order.reverse()
        } else {
            order
        })
        .then(a.cmp(b))
    };
    let filtering = !request.query.is_empty() || request.extension.is_some() || request.minimum > 0;
    if request.mode == Mode::Files || filtering || request.duplicates || !request.exclude.is_empty()
    {
        let pattern = request.query.to_lowercase();
        let excluded = request.exclude.to_lowercase();
        let mut duplicates = std::collections::HashMap::new();
        if request.duplicates {
            for node in &s.nodes {
                if !node.is_dir {
                    *duplicates
                        .entry((node.name.clone(), node.size))
                        .or_insert(0usize) += 1;
                }
            }
        }
        let mut ids = Vec::new();
        for (id, node) in s.nodes.iter().enumerate() {
            if id % 2048 == 0 && generation.load(Ordering::Relaxed) != request.version {
                return None;
            }
            if (node.is_dir && !request.include_folders) || node.size < request.minimum {
                continue;
            }
            if request.duplicates
                && (node.is_dir
                    || duplicates
                        .get(&(node.name.clone(), node.size))
                        .copied()
                        .unwrap_or(0)
                        < 2)
            {
                continue;
            }
            if let Some(ext) = &request.extension
                && (node.is_dir || extension(&node.name) != *ext)
            {
                continue;
            }
            let text = if request.match_paths {
                s.path(id).to_string_lossy().to_lowercase()
            } else {
                node.name.to_string_lossy().to_lowercase()
            };
            if !pattern.is_empty() && !matches(&text, &pattern) {
                continue;
            }
            if !excluded.is_empty() && matches(&text, &excluded) {
                continue;
            }
            ids.push(id);
        }
        ids.sort_unstable_by(compare);
        if request.limit > 0 {
            ids.truncate(request.limit);
        }
        return (generation.load(Ordering::Relaxed) == request.version)
            .then(|| ids.into_iter().map(|id| Row { id, depth: 0 }).collect());
    }
    let mut out = vec![];
    let mut stack = vec![(request.root, 0usize)];
    while let Some((id, depth)) = stack.pop() {
        if out.len() % 2048 == 0 && generation.load(Ordering::Relaxed) != request.version {
            return None;
        }
        out.push(Row { id, depth });
        if request.expanded.contains(&id) {
            let mut children = s.nodes[id].children.clone();
            children.sort_unstable_by(compare);
            stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)));
        }
    }
    (generation.load(Ordering::Relaxed) == request.version).then_some(out)
}
pub fn extension(name: &std::ffi::OsStr) -> String {
    std::path::Path::new(name)
        .extension()
        .map_or(String::new(), |s| s.to_string_lossy().to_lowercase())
}
pub fn color(extension: &str) -> u32 {
    let colors = [
        0x6389d8, 0x57a89c, 0xbc8f56, 0x9471bb, 0xcb788c, 0x729d65, 0x638caa, 0xc28b54, 0x8c83ba,
        0x599fba, 0xba789d, 0x8c9e57,
    ];
    let hash = extension
        .bytes()
        .fold(2166136261u32, |n, b| (n ^ b as u32).wrapping_mul(16777619));
    colors[hash as usize % colors.len()]
}
// Case-insensitive names are prepared by caller. '*' and '?' mirror familiar file masks.
fn matches(value: &str, pattern: &str) -> bool {
    if !pattern.contains(['*', '?']) {
        return value.contains(pattern);
    }
    let v: Vec<_> = value.chars().collect();
    let p: Vec<_> = pattern.chars().collect();
    let (mut i, mut j, mut star, mut back) = (0, 0, None, 0);
    while i < v.len() {
        if j < p.len() && (p[j] == '?' || p[j] == v[i]) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some(j);
            j += 1;
            back = i;
        } else if let Some(at) = star {
            back += 1;
            i = back;
            j = at + 1;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn familiar_name_masks() {
        assert!(matches("report.csv", "*.csv"));
        assert!(matches("汉字.txt", "??.txt"));
        assert!(!matches("x.csv.zip", "*.csv"));
        assert!(matches("video.mov", "video"));
        assert!(matches("aabbcc", "a*b*c*"));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use disk_core::{Progress, ScanOptions, scan};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::AtomicBool,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "wiztree-query-{}-{nonce}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, size: usize, byte: u8) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, vec![byte; size]).unwrap();
        }
        fn query(&self) -> Query {
            let snapshot = Arc::new(
                scan(
                    &self.0,
                    &ScanOptions { workers: 4 },
                    &AtomicBool::new(false),
                    &Progress::default(),
                )
                .unwrap(),
            );
            let index = Arc::new(AnalysisIndex::new(&snapshot));
            Query {
                version: 7,
                snapshot,
                index,
                root: 0,
                mode: Mode::Files,
                expanded: HashSet::new(),
                query: String::new(),
                extension: None,
                minimum: 0,
                sort: SortKey::Name,
                descending: false,
                limit: 0,
                match_paths: false,
                include_folders: false,
                exclude: String::new(),
                duplicates: false,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn rows(query: &Query) -> Vec<Row> {
        execute(query, &AtomicU64::new(query.version))
            .expect("current generation should return rows")
    }
    fn node(query: &Query, relative: &str) -> usize {
        let wanted = query.snapshot.root.join(relative);
        query
            .snapshot
            .nodes
            .iter()
            .enumerate()
            .find_map(|(id, _)| (query.snapshot.path(id) == wanted).then_some(id))
            .expect("fixture node missing")
    }
    fn relative_paths(query: &Query, result: &[Row]) -> HashSet<PathBuf> {
        result
            .iter()
            .map(|row| {
                query
                    .snapshot
                    .path(row.id)
                    .strip_prefix(&query.snapshot.root)
                    .unwrap()
                    .to_path_buf()
            })
            .collect()
    }
    #[test]
    fn expanded_tree_preserves_depth_and_allocated_sibling_order_after_collapse() {
        let fixture = Fixture::new();
        fixture.write("big/sub/deep.bin", 16384, 1);
        fixture.write("big/high.bin", 8192, 2);
        fixture.write("big/low.bin", 4096, 3);
        fixture.write("small/one.bin", 1024, 4);
        fixture.write("root.bin", 2048, 5);
        let mut query = fixture.query();
        query.mode = Mode::Tree;
        query.sort = SortKey::Allocated;
        query.descending = true;
        let big = node(&query, "big");
        let sub = node(&query, "big/sub");
        let deep = node(&query, "big/sub/deep.bin");
        query.expanded = HashSet::from([0, big, sub]);
        let expanded = rows(&query);
        assert_eq!((expanded[0].id, expanded[0].depth), (0, 0));
        assert!(expanded.iter().any(|row| row.id == deep && row.depth == 3));
        let mut ancestors = Vec::new();
        let mut previous_allocated = std::collections::HashMap::new();
        for row in &expanded {
            ancestors.truncate(row.depth);
            if row.depth > 0 {
                assert_eq!(
                    query.snapshot.nodes[row.id].parent,
                    Some(ancestors[row.depth - 1])
                );
                let parent = query.snapshot.nodes[row.id].parent.unwrap();
                let allocation = query.snapshot.nodes[row.id].allocated;
                if let Some(previous) = previous_allocated.insert(parent, allocation) {
                    assert!(
                        previous >= allocation,
                        "siblings must be sorted without flattening the hierarchy"
                    );
                }
            }
            ancestors.push(row.id);
        }
        let big_position = expanded.iter().position(|row| row.id == big).unwrap();
        assert_eq!(expanded[big_position + 1].id, sub);
        query.expanded.remove(&big);
        let collapsed = rows(&query);
        assert!(collapsed.iter().any(|row| row.id == big && row.depth == 1));
        assert!(!collapsed.iter().any(|row| row.id == sub || row.id == deep));
        assert_eq!(collapsed.len(), query.snapshot.nodes[0].children.len() + 1);
        query.expanded.insert(big);
        assert_eq!(
            rows(&query)
                .iter()
                .map(|row| (row.id, row.depth))
                .collect::<Vec<_>>(),
            expanded
                .iter()
                .map(|row| (row.id, row.depth))
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn file_masks_extension_threshold_and_exclusion_cover_nested_paths() {
        let fixture = Fixture::new();
        for (name, size) in [
            ("root.csv", 100),
            ("folder/nested.csv", 200),
            ("folder/ignore.csv", 500),
            ("folder/small.csv", 5),
            ("folder/not.csv.zip", 500),
            ("folder/nested.txt", 500),
            ("other/nested.CSV", 300),
        ] {
            fixture.write(name, size, 8);
        }
        let mut query = fixture.query();
        query.query = "*.csv".into();
        query.extension = Some("csv".into());
        query.minimum = 100;
        query.exclude = "ignore".into();
        query.match_paths = true;
        let expected = ["root.csv", "folder/nested.csv", "other/nested.CSV"]
            .map(PathBuf::from)
            .into_iter()
            .collect();
        assert_eq!(relative_paths(&query, &rows(&query)), expected);
        assert!(rows(&query).iter().all(|row| row.depth == 0));
        // A full path on macOS can start with /var/folders; address the
        // fixture's exact directory component rather than that host prefix.
        query.query = format!("{}*", query.snapshot.root.to_string_lossy());
        assert_eq!(relative_paths(&query, &rows(&query)), expected);
        let separator = std::path::MAIN_SEPARATOR;
        query.query = format!("*{separator}folder{separator}nested.csv");
        assert_eq!(
            relative_paths(&query, &rows(&query)),
            HashSet::from([PathBuf::from("folder/nested.csv")])
        );
        query.query = format!("{separator}folder{separator}");
        query.match_paths = false;
        assert!(
            rows(&query).is_empty(),
            "name-only matching must not match a containing folder"
        );
        query.match_paths = true;
        assert_eq!(
            relative_paths(&query, &rows(&query)),
            HashSet::from([PathBuf::from("folder/nested.csv")])
        );
        query.query = "*.csv".into();
        query.exclude = format!("*{separator}folder{separator}*");
        assert_eq!(
            relative_paths(&query, &rows(&query)),
            ["root.csv", "other/nested.CSV"]
                .map(PathBuf::from)
                .into_iter()
                .collect()
        );
    }
    #[test]
    fn include_folders_adds_real_directories_and_root_to_file_results() {
        let fixture = Fixture::new();
        fixture.write("folder/deep/file.bin", 7, 1);
        fs::create_dir_all(fixture.0.join("empty")).unwrap();
        let mut query = fixture.query();
        let files = rows(&query);
        assert_eq!(files.len(), 1);
        assert!(files.iter().all(|row| !query.snapshot.nodes[row.id].is_dir));
        query.include_folders = true;
        let all = rows(&query);
        assert_eq!(all.len() as u64, query.snapshot.files + query.snapshot.dirs);
        assert!(all.iter().any(|row| row.id == 0));
        assert!(
            all.iter()
                .any(|row| query.snapshot.path(row.id).ends_with(Path::new("empty")))
        );
        assert!(all.iter().all(|row| row.depth == 0));
    }
    #[test]
    fn duplicates_are_candidates_by_exact_name_and_size_without_content_hashing() {
        let fixture = Fixture::new();
        fixture.write("a/dup.bin", 64, 1);
        fixture.write("b/dup.bin", 64, 2);
        fixture.write("c/dup.bin", 128, 1);
        fixture.write("d/same-content.bin", 64, 1);
        assert_ne!(
            fs::read(fixture.0.join("a/dup.bin")).unwrap(),
            fs::read(fixture.0.join("b/dup.bin")).unwrap()
        );
        let mut query = fixture.query();
        query.duplicates = true;
        query.include_folders = true;
        assert_eq!(
            relative_paths(&query, &rows(&query)),
            ["a/dup.bin", "b/dup.bin"]
                .map(PathBuf::from)
                .into_iter()
                .collect()
        );
        assert!(
            rows(&query)
                .iter()
                .all(|row| !query.snapshot.nodes[row.id].is_dir)
        );
    }
    #[test]
    fn all_limit_returns_more_than_one_thousand_and_capped_results_keep_sort_order() {
        let fixture = Fixture::new();
        for index in 0..1007 {
            fixture.write(&format!("entry-{index:04}.bin"), 0, 0);
        }
        let mut query = fixture.query();
        query.limit = 0;
        let all = rows(&query);
        assert_eq!(all.len(), 1007);
        query.limit = 1000;
        let capped = rows(&query);
        assert_eq!(capped.len(), 1000);
        assert_eq!(
            capped.iter().map(|row| row.id).collect::<Vec<_>>(),
            all.iter().take(1000).map(|row| row.id).collect::<Vec<_>>()
        );
    }
    #[test]
    fn stale_generations_never_return_tree_file_or_duplicate_results() {
        let fixture = Fixture::new();
        fixture.write("a/same.bin", 8, 1);
        fixture.write("b/same.bin", 8, 2);
        let mut query = fixture.query();
        let generation = AtomicU64::new(query.version + 1);
        assert!(execute(&query, &generation).is_none());
        query.duplicates = true;
        assert!(execute(&query, &generation).is_none());
        query.duplicates = false;
        query.mode = Mode::Tree;
        query.expanded.insert(0);
        assert!(execute(&query, &generation).is_none());
        generation.store(query.version, Ordering::Relaxed);
        assert!(execute(&query, &generation).is_some());
    }
}
