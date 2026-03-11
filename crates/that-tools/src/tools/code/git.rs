//! Git safety operations for that-tools.
//!
//! Provides checkpoint/restore functionality to protect against edit failures.
//! Uses git CLI for all repository operations.

use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GitError {
    #[error("not a git repository")]
    NotARepo,
    #[error("git command failed: {0}")]
    CommandFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Run a git command in the given directory and return stdout on success.
fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git").args(args).current_dir(dir).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(GitError::CommandFailed(stderr))
    }
}

/// A snapshot of git state that can be restored on failure.
pub struct GitCheckpoint {
    pub repo_path: PathBuf,
    pub original_branch: String,
    pub stash_created: bool,
    pub safety_branch: Option<String>,
}

/// Create a checkpoint before editing.
///
/// 1. Discover the git repository
/// 2. If working tree is dirty, stash changes
/// 3. If `create_branch` is true, create a safety branch
pub fn create_checkpoint(file_path: &Path, create_branch: bool) -> Result<GitCheckpoint, GitError> {
    // Discover the git repository root
    let repo_path = PathBuf::from(
        git(file_path, &["rev-parse", "--show-toplevel"]).map_err(|_| GitError::NotARepo)?,
    );

    // Get current branch name
    let original_branch =
        git(&repo_path, &["branch", "--show-current"]).unwrap_or_else(|_| "HEAD".to_string());
    let original_branch = if original_branch.is_empty() {
        "HEAD".to_string()
    } else {
        original_branch
    };

    // Check if working tree is dirty
    let porcelain = git(&repo_path, &["status", "--porcelain"]).unwrap_or_default();
    let is_dirty = !porcelain.is_empty();
    let mut stash_created = false;

    if is_dirty {
        let output = Command::new("git")
            .args(["stash", "push", "-m", "that-tools: pre-edit safety stash"])
            .current_dir(&repo_path)
            .output()?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // "No local changes" means nothing was stashed
            stash_created = !stdout.contains("No local changes");
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("git stash failed: {}", stderr);
        }
    }

    // Optionally create a safety branch
    let safety_branch = if create_branch {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let branch_name = format!("that-tools/edit-{}", timestamp);
        match git(&repo_path, &["checkout", "-b", &branch_name]) {
            Ok(_) => Some(branch_name),
            Err(e) => {
                tracing::warn!("safety branch creation failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    Ok(GitCheckpoint {
        repo_path,
        original_branch,
        stash_created,
        safety_branch,
    })
}

/// Restore a checkpoint (pop stash, switch back to original branch).
pub fn restore_checkpoint(checkpoint: &GitCheckpoint) -> Result<(), GitError> {
    if checkpoint.stash_created {
        git(&checkpoint.repo_path, &["stash", "pop"])
            .map_err(|e| GitError::CommandFailed(format!("stash pop failed: {}", e)))?;
    }

    if let Some(ref branch) = checkpoint.safety_branch {
        git(
            &checkpoint.repo_path,
            &["checkout", &checkpoint.original_branch],
        )
        .map_err(|e| {
            GitError::CommandFailed(format!(
                "checkout back to {} failed: {}",
                checkpoint.original_branch, e
            ))
        })?;

        // Delete the safety branch
        let _ = git(&checkpoint.repo_path, &["branch", "-D", branch]);
    }

    Ok(())
}

/// Check if a path is inside a git repository.
#[allow(dead_code)]
pub fn is_git_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--show-toplevel"]).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        tmp
    }

    #[test]
    fn test_is_git_repo() {
        let tmp = setup_git_repo();
        assert!(is_git_repo(tmp.path()));
    }

    #[test]
    fn test_not_git_repo() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_git_repo(tmp.path()));
    }

    #[test]
    fn test_checkpoint_clean_repo() {
        let tmp = setup_git_repo();
        let cp = create_checkpoint(tmp.path(), false).unwrap();
        assert!(!cp.stash_created, "clean repo should not create stash");
    }

    #[test]
    fn test_checkpoint_dirty_repo() {
        let tmp = setup_git_repo();
        // Make dirty
        fs::write(tmp.path().join("main.rs"), "fn main() { /* dirty */ }\n").unwrap();

        let cp = create_checkpoint(tmp.path(), false).unwrap();
        assert!(cp.stash_created, "dirty repo should create stash");

        // Restore
        restore_checkpoint(&cp).unwrap();

        // Verify the dirty change is back
        let content = fs::read_to_string(tmp.path().join("main.rs")).unwrap();
        assert!(
            content.contains("dirty"),
            "stash pop should restore changes"
        );
    }

    #[test]
    fn test_checkpoint_not_a_repo() {
        let tmp = TempDir::new().unwrap();
        let result = create_checkpoint(tmp.path(), false);
        assert!(result.is_err());
    }

    #[test]
    fn test_checkpoint_with_safety_branch() {
        let tmp = setup_git_repo();
        let cp = create_checkpoint(tmp.path(), true).unwrap();
        assert!(cp.safety_branch.is_some(), "should create safety branch");
        assert!(cp
            .safety_branch
            .as_ref()
            .unwrap()
            .starts_with("that-tools/edit-"));

        // Restore should switch back to original branch
        restore_checkpoint(&cp).unwrap();
    }

    #[test]
    fn test_restore_no_stash() {
        let tmp = setup_git_repo();
        let cp = GitCheckpoint {
            repo_path: tmp.path().to_path_buf(),
            original_branch: "main".to_string(),
            stash_created: false,
            safety_branch: None,
        };
        // Should succeed without doing anything
        restore_checkpoint(&cp).unwrap();
    }
}
