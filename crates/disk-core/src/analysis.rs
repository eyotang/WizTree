//! Linear descendant summaries and a reusable file-extension index.
use crate::Snapshot;
use std::{collections::HashMap, path::Path};
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct NodeSummary {
    /// Descendant files, excluding this node itself.
    pub files: u64,
    /// Descendant directories, excluding this node itself.
    pub dirs: u64,
}
#[derive(Clone, Debug)]
pub struct ExtensionStat {
    /// Lowercase without a leading dot; empty means no extension.
    pub extension: String,
    pub files: u64,
    pub size: u64,
    pub allocated: u64,
    /// Arena IDs; full paths are reconstructed on demand via Snapshot::path.
    pub file_ids: Vec<usize>,
}
#[derive(Clone, Debug)]
pub struct AnalysisIndex {
    pub nodes: Vec<NodeSummary>,
    /// All extensions in descending logical-size order.
    pub extensions: Vec<ExtensionStat>,
    extension_lookup: HashMap<String, usize>,
}
impl AnalysisIndex {
    /// Descendant aggregation is O(n); extension grouping is expected O(n).
    /// Sorting the e extension groups costs O(e log e).
    pub fn new(snapshot: &Snapshot) -> Self {
        let mut nodes = vec![NodeSummary::default(); snapshot.nodes.len()];
        let mut groups: HashMap<String, ExtensionStat> = HashMap::new();
        for (id, node) in snapshot.nodes.iter().enumerate() {
            if node.is_dir {
                continue;
            }
            let extension = Path::new(&node.name)
                .extension()
                .map_or_else(String::new, |value| value.to_string_lossy().to_lowercase());
            let group = groups
                .entry(extension.clone())
                .or_insert_with(|| ExtensionStat {
                    extension,
                    files: 0,
                    size: 0,
                    allocated: 0,
                    file_ids: Vec::new(),
                });
            group.files += 1;
            group.size = group.size.saturating_add(node.size);
            group.allocated = group.allocated.saturating_add(node.allocated);
            group.file_ids.push(id);
        }
        for id in (0..snapshot.nodes.len()).rev() {
            if let Some(parent) = snapshot.nodes[id].parent {
                let summary = nodes[id];
                nodes[parent].files = nodes[parent]
                    .files
                    .saturating_add(summary.files + u64::from(!snapshot.nodes[id].is_dir));
                nodes[parent].dirs = nodes[parent]
                    .dirs
                    .saturating_add(summary.dirs + u64::from(snapshot.nodes[id].is_dir));
            }
        }
        let mut extensions: Vec<_> = groups.into_values().collect();
        extensions.sort_unstable_by(|a, b| {
            b.size
                .cmp(&a.size)
                .then_with(|| a.extension.cmp(&b.extension))
        });
        let extension_lookup = extensions
            .iter()
            .enumerate()
            .map(|(id, stat)| (stat.extension.clone(), id))
            .collect();
        Self {
            nodes,
            extensions,
            extension_lookup,
        }
    }
    pub fn extension(&self, extension: &str) -> Option<&ExtensionStat> {
        self.extension_lookup
            .get(&extension.trim_start_matches('.').to_lowercase())
            .map(|id| &self.extensions[*id])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Progress, ScanOptions, scan};
    use std::{
        fs,
        sync::atomic::{AtomicBool, AtomicU64, Ordering},
        time::{Duration, SystemTime},
    };
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            static ID: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "disk-analysis-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn nested_counts_all_file_extensions_and_real_modified_times() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.0.join("nested/deep")).unwrap();
        fs::create_dir_all(fixture.0.join("empty")).unwrap();
        for (name, size) in [
            ("one.RS", 3),
            ("nested/two.rs", 7),
            ("nested/deep/archive.TAR.GZ", 11),
            ("README", 13),
            (".gitignore", 17),
        ] {
            fs::write(fixture.0.join(name), vec![0u8; size]).unwrap();
        }
        let expected_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_650_000_000);
        fs::File::options()
            .write(true)
            .open(fixture.0.join("one.RS"))
            .unwrap()
            .set_modified(expected_time)
            .unwrap();
        let snapshot = scan(
            &fixture.0,
            &ScanOptions { workers: 4 },
            &AtomicBool::new(false),
            &Progress::default(),
        )
        .unwrap();
        let index = AnalysisIndex::new(&snapshot);
        assert_eq!(index.nodes[0], NodeSummary { files: 5, dirs: 3 });
        let nested = snapshot
            .nodes
            .iter()
            .position(|n| n.name == "nested")
            .unwrap();
        assert_eq!(index.nodes[nested], NodeSummary { files: 2, dirs: 1 });
        let empty = snapshot
            .nodes
            .iter()
            .position(|n| n.name == "empty")
            .unwrap();
        assert_eq!(index.nodes[empty], NodeSummary::default());
        let rs = index.extension(".Rs").unwrap();
        assert_eq!((rs.files, rs.size), (2, 10));
        assert_eq!(rs.file_ids.len(), 2);
        assert!(rs.file_ids.iter().all(|id| !snapshot.nodes[*id].is_dir));
        assert_eq!(index.extension("").unwrap().files, 2);
        assert_eq!(index.extension("gz").unwrap().size, 11);
        assert!(index.extension("missing").is_none());
        assert_eq!(
            index.extensions.iter().map(|e| e.files).sum::<u64>(),
            snapshot.files
        );
        assert_eq!(
            index.extensions.iter().map(|e| e.size).sum::<u64>(),
            snapshot.nodes[0].size
        );
        assert_eq!(
            index.extensions.iter().map(|e| e.allocated).sum::<u64>(),
            snapshot.nodes[0].allocated
        );
        let one = snapshot
            .nodes
            .iter()
            .position(|n| n.name == "one.RS")
            .unwrap();
        assert_eq!(snapshot.nodes[one].modified, Some(expected_time));
        assert_eq!(index.nodes[one], NodeSummary::default());
        let directory_time = snapshot.nodes[nested].modified.unwrap();
        let actual_directory_time = fs::metadata(fixture.0.join("nested"))
            .unwrap()
            .modified()
            .unwrap();
        assert!(
            directory_time
                .duration_since(actual_directory_time)
                .or_else(|_| actual_directory_time.duration_since(directory_time))
                .unwrap()
                < Duration::from_secs(2)
        );
    }
}
