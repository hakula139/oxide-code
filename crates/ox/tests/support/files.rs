//! Scratch artifacts shared by the CLI and live test harnesses.

use std::path::{Path, PathBuf};

/// Every path under `root`, sorted.
pub(crate) fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("the scratch directory can be listed") {
            let path = entry.expect("the scratch entry can be read").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}
