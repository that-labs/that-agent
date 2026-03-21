//! Shared codebase inventory collection for code tools.
//!
//! Builds a deterministic, `.gitignore`-aware view of files/directories once,
//! so multiple tools can reuse the same traversal behavior.

use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// A single discovered path in the repository inventory.
#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub abs_path: PathBuf,
    pub relative_path: String,
    pub depth: usize,
    pub is_dir: bool,
}

/// Shared inventory result for a traversal pass.
#[derive(Debug, Clone)]
pub struct CodeInventory {
    pub entries: Vec<InventoryEntry>,
    pub total_files: usize,
    pub total_dirs: usize,
}

/// Build an inventory of files and directories under `root`.
pub fn collect_inventory(root: &Path, max_depth: Option<usize>) -> std::io::Result<CodeInventory> {
    let mut walker = WalkBuilder::new(root);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_name(|a, b| a.cmp(b));

    if let Some(depth) = max_depth {
        walker.max_depth(Some(depth));
    }

    let mut entries = Vec::new();
    let mut total_files = 0usize;
    let mut total_dirs = 0usize;

    for entry in walker.build().flatten() {
        let path = entry.path();
        if path == root {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.is_empty() {
            continue;
        }

        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir {
            total_dirs += 1;
        } else {
            total_files += 1;
        }

        entries.push(InventoryEntry {
            abs_path: path.to_path_buf(),
            depth: relative.split('/').count(),
            relative_path: relative,
            is_dir,
        });
    }

    Ok(CodeInventory {
        entries,
        total_files,
        total_dirs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_collect_inventory_counts_and_depth() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src").join("nested")).unwrap();
        fs::write(tmp.path().join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(
            tmp.path().join("src").join("nested").join("util.rs"),
            "pub fn util() {}\n",
        )
        .unwrap();

        let inv = collect_inventory(tmp.path(), None).unwrap();
        assert!(inv.total_files >= 2);
        assert!(inv.total_dirs >= 2);
        assert!(inv
            .entries
            .iter()
            .any(|e| e.relative_path == "src/main.rs" && e.depth == 2));
        assert!(inv
            .entries
            .iter()
            .any(|e| e.relative_path == "src/nested/util.rs" && e.depth == 3));
    }

    #[test]
    fn test_collect_inventory_respects_gitignore() {
        let tmp = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join(".gitignore"), "*.log\n").unwrap();
        fs::write(tmp.path().join("debug.log"), "should be ignored\n").unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();

        let inv = collect_inventory(tmp.path(), None).unwrap();
        assert!(inv.entries.iter().any(|e| e.relative_path == "main.rs"));
        assert!(!inv.entries.iter().any(|e| e.relative_path == "debug.log"));
    }
}
