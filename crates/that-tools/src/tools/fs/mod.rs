//! File system tools for Anvil.
//!
//! Token-aware file operations that minimize context waste.
//! `anvil fs ls` returns minimal JSON arrays instead of verbose ls output.
//! `anvil fs cat` returns budget-limited file content.

use crate::output::{self, BudgetedOutput, CompactionStrategy};
use crate::tools::path_guard;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FsError {
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),
    #[error("already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single entry in a directory listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Result of an `anvil fs ls` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LsResult {
    pub entries: Vec<FsEntry>,
    pub total: usize,
}

/// Result of an `anvil fs cat` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatResult {
    pub path: String,
    pub content: String,
    pub lines: usize,
    pub tokens: usize,
    pub truncated: bool,
}

/// Lists directory contents with .gitignore awareness and token-minimal output.
///
/// Uses the `ignore` crate to respect .gitignore rules automatically.
/// Output is a compact JSON object with entries array.
pub fn ls(
    path: &Path,
    max_depth: Option<usize>,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, FsError> {
    if !path.exists() {
        return Err(FsError::NotFound(path.to_path_buf()));
    }

    // Guard: reject paths that escape the workspace
    let path = &path_guard::guard(path)?;

    let depth = max_depth.unwrap_or(2);
    let mut entries = Vec::new();

    let walker = WalkBuilder::new(path)
        .max_depth(Some(depth))
        .hidden(true)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build();

    for entry in walker.flatten() {
        if entry.path() == path {
            continue;
        }

        let metadata = entry.metadata().ok();
        let entry_type = if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            "dir"
        } else {
            "file"
        };

        let relative = entry.path().strip_prefix(path).unwrap_or(entry.path());

        entries.push(FsEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: relative.to_string_lossy().to_string(),
            entry_type: entry_type.to_string(),
            size: metadata.as_ref().map(|m| m.len()),
        });
    }

    let total = entries.len();
    let result = LsResult { entries, total };

    // Budget is applied to the FINAL JSON output — always valid JSON
    Ok(output::emit_json(&result, max_tokens))
}

/// Reads file content with token budget enforcement.
///
/// The content field is budgeted first (text compaction), then the full
/// result struct is serialized to JSON. Budget applies to the final payload.
pub fn cat(path: &Path, max_tokens: Option<usize>) -> Result<BudgetedOutput, FsError> {
    if !path.exists() {
        return Err(FsError::NotFound(path.to_path_buf()));
    }

    // Guard: reject paths that escape the workspace
    let path = &path_guard::guard(path)?;

    let content = std::fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            FsError::PermissionDenied(path.to_path_buf())
        } else {
            FsError::Io(e)
        }
    })?;

    let lines = content.lines().count();

    // Budget the content text first (reserve ~40% of budget for JSON envelope)
    let content_budget = max_tokens.map(|b| (b as f64 * 0.7) as usize).unwrap_or(512);
    let budgeted_content =
        output::apply_budget_to_text(&content, content_budget, CompactionStrategy::HeadTail);

    let result = CatResult {
        path: path.to_string_lossy().to_string(),
        content: budgeted_content.content,
        lines,
        tokens: budgeted_content.tokens,
        truncated: budgeted_content.truncated,
    };

    // Final budget on the complete JSON output
    Ok(output::emit_json(&result, max_tokens))
}

/// Result of an `anvil fs write` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResult {
    pub path: String,
    pub bytes_written: u64,
    pub created: bool,
    pub backup: Option<String>,
    pub dry_run: bool,
}

/// Result of an `anvil fs mkdir` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MkdirResult {
    pub path: String,
    pub created: bool,
}

/// Result of an `anvil fs rm` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RmResult {
    pub path: String,
    pub removed: bool,
    pub was_dir: bool,
    pub dry_run: bool,
}

/// Write content to a file. Reads content from the provided string.
///
/// If `backup` is true and the file exists, creates a `.bak` copy first.
/// If `dry_run` is true, reports what would happen without writing.
pub fn write(
    path: &Path,
    content: &str,
    dry_run: bool,
    backup: bool,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, FsError> {
    // Guard: reject traversal (file may not exist yet, so use component check)
    path_guard::reject_traversal(path)?;
    if path.exists() {
        let _ = path_guard::guard(path)?;
    } else if let Some(root) = path_guard::workspace_root() {
        if let Some(parent) = path.parent() {
            if parent.exists() {
                let _ = path_guard::safe_path(&root, parent)?;
            }
        }
    }

    let created = !path.exists();
    let mut backup_path = None;

    if !dry_run {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Backup existing file if requested
        if backup && path.exists() {
            let bak = path.with_extension(format!(
                "{}.bak",
                path.extension().unwrap_or_default().to_string_lossy()
            ));
            std::fs::copy(path, &bak)?;
            backup_path = Some(bak.to_string_lossy().to_string());
        }

        std::fs::write(path, content)?;
    }

    let result = WriteResult {
        path: path.to_string_lossy().to_string(),
        bytes_written: content.len() as u64,
        created,
        backup: backup_path,
        dry_run,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Create a directory, optionally creating parent directories.
pub fn mkdir(
    path: &Path,
    parents: bool,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, FsError> {
    // Guard: reject traversal (dir doesn't exist yet)
    path_guard::reject_traversal(path)?;
    if let Some(root) = path_guard::workspace_root() {
        if let Some(parent) = path.parent() {
            if parent.exists() {
                let _ = path_guard::safe_path(&root, parent)?;
            }
        }
    }

    if path.exists() {
        return Err(FsError::AlreadyExists(path.to_path_buf()));
    }

    if parents {
        std::fs::create_dir_all(path)?;
    } else {
        std::fs::create_dir(path)?;
    }

    let result = MkdirResult {
        path: path.to_string_lossy().to_string(),
        created: true,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Remove a file or directory.
///
/// For directories, `recursive` must be true or the directory must be empty.
/// If `dry_run` is true, reports what would happen without removing.
pub fn rm(
    path: &Path,
    recursive: bool,
    dry_run: bool,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, FsError> {
    if !path.exists() {
        return Err(FsError::NotFound(path.to_path_buf()));
    }

    // Guard: reject paths that escape the workspace
    let path = &path_guard::guard(path)?;

    let was_dir = path.is_dir();

    if !dry_run {
        if was_dir {
            if recursive {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_dir(path).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::Other
                        || e.to_string().contains("not empty")
                        || e.to_string().contains("Directory not empty")
                    {
                        FsError::DirectoryNotEmpty(path.to_path_buf())
                    } else {
                        FsError::Io(e)
                    }
                })?;
            }
        } else {
            std::fs::remove_file(path)?;
        }
    }

    let result = RmResult {
        path: path.to_string_lossy().to_string(),
        removed: !dry_run,
        was_dir,
        dry_run,
    };

    Ok(output::emit_json(&result, max_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_dir() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub mod utils;\n").unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("utils.rs"),
            "pub fn helper() {}\n",
        )
        .unwrap();
        tmp
    }

    #[test]
    fn test_ls_basic() {
        let tmp = setup_test_dir();
        let result = ls(tmp.path(), None, None).unwrap();
        assert!(!result.truncated);
        // Output must be valid JSON
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.contains("main.rs"));
        assert!(result.content.contains("lib.rs"));
    }

    #[test]
    fn test_ls_always_valid_json() {
        let tmp = TempDir::new().unwrap();
        for i in 0..100 {
            fs::write(tmp.path().join(format!("file_{:03}.rs", i)), "content").unwrap();
        }
        // Even with tiny budget, must be valid JSON
        let result = ls(tmp.path(), None, Some(20)).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "ls output must always be valid JSON: {}",
            result.content
        );
    }

    #[test]
    fn test_ls_respects_gitignore() {
        let tmp = setup_test_dir();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target").join("debug"), "binary").unwrap();
        fs::write(tmp.path().join("build.log"), "log content").unwrap();

        let result = ls(tmp.path(), None, None).unwrap();
        assert!(!result.content.contains("\"target\""));
        assert!(!result.content.contains("build.log"));
    }

    #[test]
    fn test_ls_max_depth() {
        let tmp = setup_test_dir();
        fs::create_dir_all(tmp.path().join("a").join("b").join("c")).unwrap();
        fs::write(tmp.path().join("a").join("b").join("c").join("deep.rs"), "").unwrap();

        let result = ls(tmp.path(), Some(1), None).unwrap();
        assert!(!result.content.contains("deep.rs"));
    }

    #[test]
    fn test_ls_not_found() {
        let result = ls(Path::new("/nonexistent/path"), None, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FsError::NotFound(_)));
    }

    #[test]
    fn test_cat_basic() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.rs");
        fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let result = cat(&file_path, None).unwrap();
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.contains("fn main()"));
    }

    #[test]
    fn test_cat_always_valid_json() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("big.rs");
        let content: String = (0..500)
            .map(|i| format!("// Line {}: some code here\n", i))
            .collect();
        fs::write(&file_path, &content).unwrap();

        // Even with tiny budget, must be valid JSON
        let result = cat(&file_path, Some(20)).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "cat output must always be valid JSON: {}",
            result.content
        );
    }

    #[test]
    fn test_cat_not_found() {
        let result = cat(Path::new("/nonexistent/file.rs"), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_fs_entry_serialization() {
        let entry = FsEntry {
            name: "main.rs".to_string(),
            path: "src/main.rs".to_string(),
            entry_type: "file".to_string(),
            size: Some(1024),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("main.rs"));
        assert!(json.contains("\"type\":\"file\""));
        assert!(json.contains("1024"));
    }

    #[test]
    fn test_fs_entry_size_omitted_when_none() {
        let entry = FsEntry {
            name: "src".to_string(),
            path: "src".to_string(),
            entry_type: "dir".to_string(),
            size: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("size"));
    }

    // --- write tests ---

    #[test]
    fn test_write_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("new_file.txt");
        let result = write(&file_path, "hello world", false, false, None).unwrap();
        let parsed: WriteResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.created);
        assert_eq!(parsed.bytes_written, 11);
        assert!(!parsed.dry_run);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello world");
    }

    #[test]
    fn test_write_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("existing.txt");
        fs::write(&file_path, "old content").unwrap();
        let result = write(&file_path, "new content", false, false, None).unwrap();
        let parsed: WriteResult = serde_json::from_str(&result.content).unwrap();
        assert!(!parsed.created);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "new content");
    }

    #[test]
    fn test_write_backup() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("backup_test.txt");
        fs::write(&file_path, "original").unwrap();
        let result = write(&file_path, "updated", false, true, None).unwrap();
        let parsed: WriteResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.backup.is_some());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "updated");
    }

    #[test]
    fn test_write_dry_run() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("dry_run.txt");
        let result = write(&file_path, "content", true, false, None).unwrap();
        let parsed: WriteResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.dry_run);
        assert!(!file_path.exists());
    }

    #[test]
    fn test_write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("a").join("b").join("c.txt");
        let result = write(&file_path, "deep", false, false, None).unwrap();
        let parsed: WriteResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.created);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "deep");
    }

    // --- mkdir tests ---

    #[test]
    fn test_mkdir_basic() {
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path().join("new_dir");
        let result = mkdir(&dir_path, false, None).unwrap();
        let parsed: MkdirResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.created);
        assert!(dir_path.is_dir());
    }

    #[test]
    fn test_mkdir_parents() {
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path().join("a").join("b").join("c");
        let result = mkdir(&dir_path, true, None).unwrap();
        let parsed: MkdirResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.created);
        assert!(dir_path.is_dir());
    }

    #[test]
    fn test_mkdir_already_exists() {
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path().join("existing");
        fs::create_dir(&dir_path).unwrap();
        let result = mkdir(&dir_path, false, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FsError::AlreadyExists(_)));
    }

    // --- rm tests ---

    #[test]
    fn test_rm_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("to_remove.txt");
        fs::write(&file_path, "content").unwrap();
        let result = rm(&file_path, false, false, None).unwrap();
        let parsed: RmResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.removed);
        assert!(!parsed.was_dir);
        assert!(!file_path.exists());
    }

    #[test]
    fn test_rm_dir_recursive() {
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path().join("to_remove_dir");
        fs::create_dir(&dir_path).unwrap();
        fs::write(dir_path.join("file.txt"), "content").unwrap();
        let result = rm(&dir_path, true, false, None).unwrap();
        let parsed: RmResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.removed);
        assert!(parsed.was_dir);
        assert!(!dir_path.exists());
    }

    #[test]
    fn test_rm_dry_run() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("keep_me.txt");
        fs::write(&file_path, "content").unwrap();
        let result = rm(&file_path, false, true, None).unwrap();
        let parsed: RmResult = serde_json::from_str(&result.content).unwrap();
        assert!(!parsed.removed);
        assert!(parsed.dry_run);
        assert!(file_path.exists());
    }

    #[test]
    fn test_rm_not_found() {
        let result = rm(Path::new("/nonexistent/file"), false, false, None);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FsError::NotFound(_)));
    }
}
