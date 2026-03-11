//! Repository tree mapping for that-tools.
//!
//! Generates compact, token-efficient directory trees with .gitignore awareness.
//! The `--ranked` flag (Phase 2: PageRank) will order files by architectural importance.

use crate::output::{self, BudgetedOutput, CompactionStrategy};
use serde::{Deserialize, Serialize};
#[cfg(feature = "code-analysis")]
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum TreeError {
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single entry in the tree output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<usize>,
    /// PageRank importance score (populated when --ranked is used).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<f64>,
}

/// Result of a tree command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeResult {
    pub root: String,
    pub entries: Vec<TreeEntry>,
    pub total_files: usize,
    pub total_dirs: usize,
}

/// Generate a repository tree with .gitignore awareness.
///
/// Returns a compact tree representation optimized for agent consumption.
/// Files are sorted alphabetically within each directory level.
/// When `ranked` is true, entries are annotated with PageRank scores
/// and sorted by importance (highest first).
pub fn code_tree(
    root: &Path,
    max_depth: Option<usize>,
    max_tokens: Option<usize>,
    compact: bool,
    ranked: bool,
) -> Result<BudgetedOutput, TreeError> {
    if !root.exists() {
        return Err(TreeError::NotFound(root.to_string_lossy().to_string()));
    }

    let depth = max_depth.unwrap_or(4);
    let inv = crate::tools::code::inventory::collect_inventory(root, Some(depth))?;
    #[allow(unused_mut)]
    let mut entries: Vec<TreeEntry> = inv
        .entries
        .iter()
        .map(|entry| TreeEntry {
            path: entry.relative_path.clone(),
            entry_type: if entry.is_dir {
                "dir".to_string()
            } else {
                "file".to_string()
            },
            depth: entry.depth,
            symbols: None,
            rank: None,
        })
        .collect();
    let total_files = inv.total_files;
    let total_dirs = inv.total_dirs;

    // Annotate with PageRank scores and sort when ranked
    if ranked {
        #[cfg(feature = "code-analysis")]
        {
            // Find the actual project root (directory containing .that-tools/) instead of
            // looking at the tree root which may be a subdirectory.
            let project_root =
                crate::index::find_tools_root(root).unwrap_or_else(|| root.to_path_buf());
            let db_path = crate::index::index_db_path(&project_root);
            if db_path.exists() {
                if let Ok(index) = crate::index::SymbolIndex::open(&db_path) {
                    // Try to get existing scores; compute if missing
                    let scores = match index.get_pagerank_scores() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("failed to read PageRank scores: {}", e);
                            HashMap::new()
                        }
                    };
                    let scores = if scores.is_empty() {
                        match crate::index::pagerank::compute_pagerank(&index) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("failed to compute PageRank: {}", e);
                                HashMap::new()
                            }
                        }
                    } else {
                        scores
                    };

                    for entry in &mut entries {
                        if entry.entry_type == "file" {
                            // Scores use project-root-relative paths, but tree entries
                            // use tree-root-relative paths. Convert to project-root-relative.
                            let abs_path = root.join(&entry.path);
                            let project_rel = abs_path
                                .strip_prefix(&project_root)
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| entry.path.clone());
                            entry.rank = scores.get(&project_rel).copied();
                        }
                    }

                    // Sort files by rank within each depth group, keeping dirs in place.
                    // This preserves the hierarchical tree structure.
                    let mut i = 0;
                    while i < entries.len() {
                        // Find contiguous runs of files at the same depth
                        if entries[i].entry_type == "file" {
                            let depth = entries[i].depth;
                            let start = i;
                            while i < entries.len()
                                && entries[i].entry_type == "file"
                                && entries[i].depth == depth
                            {
                                i += 1;
                            }
                            // Sort this slice of files by rank (highest first)
                            entries[start..i].sort_by(|a, b| {
                                let rank_a = a.rank.unwrap_or(0.0);
                                let rank_b = b.rank.unwrap_or(0.0);
                                rank_b
                                    .partial_cmp(&rank_a)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        } // cfg(code-analysis)
    }

    if compact {
        // Compact mode: build ASCII tree, wrap in JSON envelope for consistency
        let tree_string = build_compact_tree(&entries, root);
        let content_budget = max_tokens.map(|b| (b as f64 * 0.8) as usize).unwrap_or(200);
        let budgeted = output::apply_budget_to_text(
            &tree_string,
            content_budget,
            CompactionStrategy::HeadTail,
        );

        #[derive(serde::Serialize)]
        struct CompactTreeResult {
            tree: String,
            total_files: usize,
            total_dirs: usize,
            truncated: bool,
        }
        let result = CompactTreeResult {
            tree: budgeted.content,
            total_files,
            total_dirs,
            truncated: budgeted.truncated,
        };
        return Ok(output::emit_json(&result, max_tokens));
    }

    let result = TreeResult {
        root: root.to_string_lossy().to_string(),
        entries,
        total_files,
        total_dirs,
    };

    // Budget applied to FINAL JSON output — always valid JSON
    Ok(output::emit_json(&result, max_tokens))
}

/// Build a compact ASCII tree representation.
fn build_compact_tree(entries: &[TreeEntry], root: &Path) -> String {
    let mut lines = Vec::new();
    let root_name = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy();
    lines.push(format!("{}/", root_name));

    for entry in entries {
        let indent = "  ".repeat(entry.depth);
        let suffix = if entry.entry_type == "dir" { "/" } else { "" };
        let name = Path::new(&entry.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        lines.push(format!("{}{}{}", indent, name, suffix));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src").join("utils")).unwrap();
        fs::create_dir(tmp.path().join("tests")).unwrap();
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\n",
        )
        .unwrap();
        fs::write(tmp.path().join("src").join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(tmp.path().join("src").join("lib.rs"), "pub mod utils;\n").unwrap();
        fs::write(
            tmp.path().join("src").join("utils").join("helpers.rs"),
            "pub fn helper() {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("tests").join("test_main.rs"),
            "#[test] fn test() {}\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn test_tree_basic() {
        let tmp = setup_project();
        let result = code_tree(tmp.path(), None, None, false, false).unwrap();
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("lib.rs"));
        assert!(result.content.contains("Cargo.toml"));
        assert!(result.content.contains("tests"));
    }

    #[test]
    fn test_tree_compact_format() {
        let tmp = setup_project();
        let result = code_tree(tmp.path(), None, Some(500), true, false).unwrap();
        assert!(result.content.contains("src/"));
        assert!(result.content.contains("main.rs"));
    }

    #[test]
    fn test_tree_max_depth() {
        let tmp = setup_project();
        let result = code_tree(tmp.path(), Some(1), None, false, false).unwrap();
        // At depth 1, should see top-level entries only
        assert!(!result.content.contains("helpers.rs"));
    }

    #[test]
    fn test_tree_respects_gitignore() {
        let tmp = setup_project();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target").join("debug"), "binary data").unwrap();

        let result = code_tree(tmp.path(), None, None, false, false).unwrap();
        assert!(!result.content.contains("\"target\""));
    }

    #[test]
    fn test_tree_not_found() {
        let result = code_tree(Path::new("/nonexistent"), None, None, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_tree_token_budget() {
        let tmp = TempDir::new().unwrap();
        // Create many files
        for i in 0..100 {
            fs::write(tmp.path().join(format!("file_{:03}.rs", i)), "content").unwrap();
        }

        let result = code_tree(tmp.path(), None, Some(30), false, false).unwrap();
        assert!(result.truncated);
    }

    #[test]
    fn test_tree_counts() {
        let tmp = setup_project();
        let result = code_tree(tmp.path(), None, None, false, false).unwrap();
        let parsed: TreeResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.total_files > 0);
        assert!(parsed.total_dirs > 0);
    }

    #[test]
    fn test_tree_entry_serialization() {
        let entry = TreeEntry {
            path: "src/main.rs".to_string(),
            entry_type: "file".to_string(),
            depth: 2,
            symbols: None,
            rank: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("src/main.rs"));
        assert!(!json.contains("symbols")); // None should be skipped
        assert!(!json.contains("rank")); // None should be skipped
    }

    #[test]
    fn test_tree_entry_with_symbols() {
        let entry = TreeEntry {
            path: "src/main.rs".to_string(),
            entry_type: "file".to_string(),
            depth: 2,
            symbols: Some(5),
            rank: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"symbols\":5"));
    }

    #[test]
    fn test_tree_entry_with_rank() {
        let entry = TreeEntry {
            path: "src/main.rs".to_string(),
            entry_type: "file".to_string(),
            depth: 1,
            symbols: None,
            rank: Some(0.75),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"rank\":0.75"));
    }
}
