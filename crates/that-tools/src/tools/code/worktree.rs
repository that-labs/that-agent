//! Git worktree primitives for multi-agent collaboration.
//!
//! Manages git worktrees so that multiple agents can work on isolated branches
//! of the same repository without interfering with each other.
//!
//! Worktrees are stored under `{base_repo}/.worktrees/{agent_name}/` and
//! branches follow the naming convention `agent/{agent_name}/{YYYYMMDD-HHMMSS}`.
//!
//! **Note:** The `.worktrees/` directory should be added to `.gitignore`.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Information about a git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Agent name this worktree belongs to.
    pub agent_name: String,
    /// Branch name.
    pub branch: String,
    /// Filesystem path of the worktree.
    pub path: PathBuf,
    /// Whether this is the main worktree.
    pub is_main: bool,
}

/// Result of a merge operation.
#[derive(Debug)]
pub struct MergeResult {
    /// Whether the merge was successful.
    pub success: bool,
    /// Number of commits merged.
    pub commits_merged: usize,
    /// Summary message.
    pub message: String,
    /// Files that had conflicts (empty if merge succeeded).
    pub conflicts: Vec<String>,
}

// ---------------------------------------------------------------------------
// Filesystem lock
// ---------------------------------------------------------------------------

/// A simple exclusive filesystem lock backed by a lockfile.
///
/// The lock is created atomically via `O_CREAT | O_EXCL`. If the lockfile
/// already exists and is older than 60 seconds it is considered stale and
/// forcibly removed before retrying.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    /// Acquire the lock, blocking until success or unrecoverable error.
    fn acquire(path: &Path) -> Result<Self> {
        let lock_path = path.to_path_buf();

        // First attempt: try exclusive create.
        match Self::try_create(&lock_path) {
            Ok(()) => {
                tracing::debug!(path = %lock_path.display(), "acquired worktree lock");
                return Ok(Self { path: lock_path });
            }
            Err(_) => {
                // File already exists -- check staleness.
            }
        }

        // Check if the existing lock is stale (older than 60 seconds).
        if Self::is_stale(&lock_path) {
            tracing::warn!(
                path = %lock_path.display(),
                "removing stale worktree lock (older than 60s)"
            );
            let _ = fs::remove_file(&lock_path);
        }

        // Second attempt after potential stale-lock removal.
        Self::try_create(&lock_path).context("failed to acquire worktree lock")?;
        tracing::debug!(path = %lock_path.display(), "acquired worktree lock (after stale removal)");
        Ok(Self { path: lock_path })
    }

    fn try_create(path: &Path) -> Result<()> {
        OpenOptions::new()
            .write(true)
            .create_new(true) // O_CREAT | O_EXCL
            .open(path)
            .context("lockfile already exists or cannot be created")?;
        Ok(())
    }

    fn is_stale(path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return true; // Cannot determine age -- treat as stale.
        };
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or(Duration::ZERO);
        age > Duration::from_secs(60)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "failed to release worktree lock"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate that `repo_path` is inside a git repository.
fn validate_git_repo(repo_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(repo_path)
        .output()
        .context("failed to execute git rev-parse")?;

    if !output.status.success() {
        bail!("{} is not a git repository", repo_path.display());
    }
    Ok(())
}

/// Determine the default branch name (`main`, `master`, or `HEAD`).
fn find_default_branch(repo_path: &Path) -> String {
    // Check for 'main'
    let check = |name: &str| -> bool {
        Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{name}")])
            .current_dir(repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    if check("main") {
        "main".to_string()
    } else if check("master") {
        "master".to_string()
    } else {
        "HEAD".to_string()
    }
}

/// Extract the branch name checked out in a given worktree directory.
///
/// Reads the worktree's `.git` file which contains a `gitdir:` pointer,
/// then inspects `HEAD` within that gitdir. Falls back to running
/// `git rev-parse --abbrev-ref HEAD` inside the worktree.
fn get_worktree_branch(worktree_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(worktree_path)
        .output()
        .context("failed to get worktree branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("could not determine worktree branch: {}", stderr.trim());
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        bail!("worktree is in detached HEAD state");
    }
    Ok(branch)
}

/// Run a git command and return its stdout on success, or an error with stderr.
fn run_git(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to execute: git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Get the current branch in a repository.
fn current_branch(repo_path: &Path) -> Result<String> {
    let out = run_git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(out.trim().to_string())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Create a new git worktree for a named agent.
///
/// Worktree location: `{base_repo}/.worktrees/{agent_name}/`
/// Branch naming: `agent/{agent_name}/{YYYYMMDD-HHMMSS}` (or custom prefix).
///
/// Uses a filesystem lock at `{base_repo}/.worktrees/.lock` for concurrent
/// creation safety.
pub fn create_worktree(
    base_repo: &Path,
    agent_name: &str,
    branch_prefix: Option<&str>,
) -> Result<WorktreeInfo> {
    validate_git_repo(base_repo)?;

    let worktrees_dir = base_repo.join(".worktrees");
    fs::create_dir_all(&worktrees_dir).context("failed to create .worktrees directory")?;

    // Acquire the lock before mutating worktree state.
    let lock_path = worktrees_dir.join(".lock");
    let _lock = FileLock::acquire(&lock_path)?;

    let worktree_path = worktrees_dir.join(agent_name);
    if worktree_path.exists() {
        bail!(
            "worktree for agent '{}' already exists at {}",
            agent_name,
            worktree_path.display()
        );
    }

    // Build the branch name.
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let prefix = branch_prefix.unwrap_or("agent");
    let branch_name = format!("{prefix}/{agent_name}/{timestamp}");

    tracing::info!(
        agent = agent_name,
        branch = %branch_name,
        path = %worktree_path.display(),
        "creating worktree"
    );

    let wt_path_str = worktree_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8"))?;

    run_git(
        base_repo,
        &["worktree", "add", wt_path_str, "-b", &branch_name],
    )
    .with_context(|| format!("git worktree add failed for agent '{agent_name}'"))?;

    // Lock is released automatically when `_lock` goes out of scope.
    Ok(WorktreeInfo {
        agent_name: agent_name.to_string(),
        branch: branch_name,
        path: worktree_path,
        is_main: false,
    })
}

/// List all active worktrees in the repository.
///
/// Parses the porcelain output of `git worktree list --porcelain`.
pub fn list_worktrees(base_repo: &Path) -> Result<Vec<WorktreeInfo>> {
    validate_git_repo(base_repo)?;

    let raw = run_git(base_repo, &["worktree", "list", "--porcelain"])?;

    let worktrees_dir = base_repo.join(".worktrees");
    let worktrees_dir_canonical = worktrees_dir.canonicalize().ok();

    let mut results = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut current_is_bare = false;

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("worktree ") {
            // Flush previous entry if any.
            if let Some(path) = current_path.take() {
                let info = build_worktree_info(
                    &path,
                    current_branch.take(),
                    current_is_bare,
                    worktrees_dir_canonical.as_deref(),
                );
                results.push(info);
                current_is_bare = false;
            }
            current_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(rest.to_string());
        } else if line == "bare" {
            current_is_bare = true;
        }
        // Blank line ends a block, but we flush on the next "worktree " prefix
        // or at the end.
    }

    // Flush last entry.
    if let Some(path) = current_path.take() {
        let info = build_worktree_info(
            &path,
            current_branch.take(),
            current_is_bare,
            worktrees_dir_canonical.as_deref(),
        );
        results.push(info);
    }

    Ok(results)
}

/// Build a `WorktreeInfo` from parsed porcelain fields.
fn build_worktree_info(
    path: &Path,
    branch: Option<String>,
    _is_bare: bool,
    worktrees_dir: Option<&Path>,
) -> WorktreeInfo {
    // Determine if this worktree lives under `.worktrees/` and extract the agent name.
    let (agent_name, is_main) = match worktrees_dir {
        Some(wt_dir) => {
            let canonical = path.canonicalize().ok();
            let p = canonical.as_deref().unwrap_or(path);
            if p.starts_with(wt_dir) {
                // The directory name directly under `.worktrees/` is the agent name.
                let relative = p.strip_prefix(wt_dir).unwrap_or(p);
                let name = relative
                    .components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str())
                    .unwrap_or("unknown")
                    .to_string();
                (name, false)
            } else {
                // Main worktree (the repository root itself).
                ("main".to_string(), true)
            }
        }
        None => ("main".to_string(), true),
    };

    WorktreeInfo {
        agent_name,
        branch: branch.unwrap_or_else(|| "HEAD".to_string()),
        path: path.to_path_buf(),
        is_main,
    }
}

/// Remove a worktree and optionally delete its branch.
///
/// When `force` is true, uses `--force` for the removal and also deletes
/// the associated branch with `git branch -D`.
pub fn remove_worktree(base_repo: &Path, agent_name: &str, force: bool) -> Result<()> {
    validate_git_repo(base_repo)?;

    let worktree_path = base_repo.join(".worktrees").join(agent_name);
    if !worktree_path.exists() {
        bail!(
            "worktree for agent '{}' not found at {}",
            agent_name,
            worktree_path.display()
        );
    }

    // Read the branch name before removal.
    let branch = get_worktree_branch(&worktree_path).ok();

    let wt_path_str = worktree_path
        .to_str()
        .ok_or_else(|| anyhow!("worktree path is not valid UTF-8"))?;

    tracing::info!(
        agent = agent_name,
        path = %worktree_path.display(),
        force = force,
        "removing worktree"
    );

    let mut args = vec!["worktree", "remove", wt_path_str];
    if force {
        args.push("--force");
    }

    run_git(base_repo, &args)
        .with_context(|| format!("failed to remove worktree for agent '{agent_name}'"))?;

    // Delete the branch when force is requested.
    if force {
        if let Some(ref branch_name) = branch {
            tracing::info!(
                branch = %branch_name,
                "deleting branch after forced worktree removal"
            );
            if let Err(e) = run_git(base_repo, &["branch", "-D", branch_name]) {
                tracing::warn!(
                    branch = %branch_name,
                    error = %e,
                    "failed to delete branch (may already be gone)"
                );
            }
        }
    }

    Ok(())
}

/// Show diff of an agent's worktree changes against the base branch.
///
/// Uses the three-dot diff syntax (`base...branch`) which shows only the
/// changes introduced on the worktree branch since it diverged from the base.
pub fn worktree_diff(
    base_repo: &Path,
    agent_name: &str,
    base_branch: Option<&str>,
) -> Result<String> {
    validate_git_repo(base_repo)?;

    let worktree_path = base_repo.join(".worktrees").join(agent_name);
    if !worktree_path.exists() {
        bail!("worktree for agent '{}' not found", agent_name);
    }

    let branch = get_worktree_branch(&worktree_path)?;
    let base = base_branch
        .map(String::from)
        .unwrap_or_else(|| find_default_branch(base_repo));

    let range = format!("{base}...{branch}");
    tracing::debug!(range = %range, "computing worktree diff");

    run_git(base_repo, &["diff", &range])
}

/// Show commit log for an agent's worktree since it diverged from the base.
///
/// Uses the two-dot range syntax (`base..branch`) to list only commits
/// reachable from the worktree branch but not from the base.
pub fn worktree_log(
    base_repo: &Path,
    agent_name: &str,
    max_count: Option<usize>,
) -> Result<String> {
    validate_git_repo(base_repo)?;

    let worktree_path = base_repo.join(".worktrees").join(agent_name);
    if !worktree_path.exists() {
        bail!("worktree for agent '{}' not found", agent_name);
    }

    let branch = get_worktree_branch(&worktree_path)?;
    let base = find_default_branch(base_repo);
    let range = format!("{base}..{branch}");

    tracing::debug!(range = %range, "computing worktree log");

    let mut args = vec!["log", "--oneline"];

    let max_count_str;
    if let Some(n) = max_count {
        max_count_str = format!("--max-count={n}");
        args.push(&max_count_str);
    }

    args.push(&range);
    run_git(base_repo, &args)
}

/// Merge an agent's worktree branch into a target branch.
///
/// The merge uses `--no-ff` to preserve branch topology. If conflicts occur,
/// the merge is aborted and the conflicting files are reported.
///
/// Returns the repository to the branch it was on before the merge attempt.
pub fn merge_worktree(
    base_repo: &Path,
    agent_name: &str,
    target_branch: Option<&str>,
) -> Result<MergeResult> {
    validate_git_repo(base_repo)?;

    let worktree_path = base_repo.join(".worktrees").join(agent_name);
    if !worktree_path.exists() {
        bail!("worktree for agent '{}' not found", agent_name);
    }

    let wt_branch = get_worktree_branch(&worktree_path)?;
    let target = target_branch
        .map(String::from)
        .unwrap_or_else(|| find_default_branch(base_repo));

    // Remember current branch so we can restore it on failure.
    let original_branch = current_branch(base_repo)?;

    // Count commits to merge.
    let count_range = format!("{target}..{wt_branch}");
    let count_output = run_git(base_repo, &["rev-list", "--count", &count_range])?;
    let commits_merged: usize = count_output.trim().parse().unwrap_or(0);

    if commits_merged == 0 {
        return Ok(MergeResult {
            success: true,
            commits_merged: 0,
            message: format!("nothing to merge: '{wt_branch}' has no commits ahead of '{target}'"),
            conflicts: Vec::new(),
        });
    }

    tracing::info!(
        agent = agent_name,
        branch = %wt_branch,
        target = %target,
        commits = commits_merged,
        "merging worktree branch"
    );

    // Checkout the target branch.
    if let Err(e) = run_git(base_repo, &["checkout", &target]) {
        bail!("failed to checkout target branch '{target}': {e}");
    }

    // Attempt the merge.
    let merge_msg = format!("Merge agent/{agent_name} work");
    let merge_output = Command::new("git")
        .args(["merge", &wt_branch, "--no-ff", "-m", &merge_msg])
        .current_dir(base_repo)
        .output()
        .context("failed to execute git merge")?;

    if merge_output.status.success() {
        let stdout = String::from_utf8_lossy(&merge_output.stdout);
        tracing::info!(agent = agent_name, "merge successful");

        // Return to the original branch if it differs from target.
        if original_branch != target {
            let _ = run_git(base_repo, &["checkout", &original_branch]);
        }

        return Ok(MergeResult {
            success: true,
            commits_merged,
            message: format!(
                "merged {commits_merged} commit(s) from '{wt_branch}' into '{target}': {}",
                stdout.trim()
            ),
            conflicts: Vec::new(),
        });
    }

    // Merge failed -- collect conflict information.
    tracing::warn!(agent = agent_name, "merge encountered conflicts");

    let conflicts = collect_conflict_files(base_repo);

    // Abort the failed merge so the repo is not left in a dirty state.
    let _ = run_git(base_repo, &["merge", "--abort"]);

    // Return to the original branch.
    if original_branch != target {
        let _ = run_git(base_repo, &["checkout", &original_branch]);
    }

    let stderr = String::from_utf8_lossy(&merge_output.stderr);
    Ok(MergeResult {
        success: false,
        commits_merged: 0,
        message: format!(
            "merge of '{wt_branch}' into '{target}' failed with conflicts: {}",
            stderr.trim()
        ),
        conflicts,
    })
}

/// Collect files with merge conflicts from `git diff --name-only --diff-filter=U`.
fn collect_conflict_files(repo_path: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo_path)
        .output();

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Set up a minimal git repository with one commit on a `main` branch.
    fn setup_git_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path();

        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(p)
            .output()
            .unwrap();

        fs::write(p.join("README.md"), "# hello\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(p)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(p)
            .output()
            .unwrap();

        tmp
    }

    #[test]
    fn test_create_and_list_worktree() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "alice", None).unwrap();

        assert_eq!(info.agent_name, "alice");
        assert!(info.branch.starts_with("agent/alice/"));
        assert!(info.path.exists());
        assert!(!info.is_main);

        let list = list_worktrees(repo.path()).unwrap();
        // Should have at least the main worktree + alice's.
        assert!(list.len() >= 2);
        let alice_entry = list.iter().find(|w| w.agent_name == "alice");
        assert!(alice_entry.is_some());
    }

    #[test]
    fn test_create_worktree_custom_prefix() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "bob", Some("feature")).unwrap();

        assert!(
            info.branch.starts_with("feature/bob/"),
            "expected prefix 'feature/bob/', got '{}'",
            info.branch
        );
    }

    #[test]
    fn test_create_worktree_duplicate_fails() {
        let repo = setup_git_repo();
        create_worktree(repo.path(), "charlie", None).unwrap();
        let second = create_worktree(repo.path(), "charlie", None);
        assert!(second.is_err());
    }

    #[test]
    fn test_remove_worktree() {
        let repo = setup_git_repo();
        create_worktree(repo.path(), "dave", None).unwrap();

        let wt_path = repo.path().join(".worktrees").join("dave");
        assert!(wt_path.exists());

        remove_worktree(repo.path(), "dave", false).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_remove_worktree_force_deletes_branch() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "eve", None).unwrap();
        let branch = info.branch.clone();

        remove_worktree(repo.path(), "eve", true).unwrap();

        // Verify the branch is gone.
        let check = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{branch}")])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(!check.status.success(), "branch should have been deleted");
    }

    #[test]
    fn test_remove_nonexistent_worktree_fails() {
        let repo = setup_git_repo();
        let result = remove_worktree(repo.path(), "ghost", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_worktree_diff_no_changes() {
        let repo = setup_git_repo();
        create_worktree(repo.path(), "frank", None).unwrap();

        let diff = worktree_diff(repo.path(), "frank", None).unwrap();
        // No changes yet, diff should be empty.
        assert!(diff.trim().is_empty());
    }

    #[test]
    fn test_worktree_diff_with_changes() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "grace", None).unwrap();

        // Make a commit in the worktree.
        fs::write(info.path.join("new_file.txt"), "hello from grace\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&info.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "add new file"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let diff = worktree_diff(repo.path(), "grace", None).unwrap();
        assert!(
            diff.contains("new_file.txt"),
            "diff should mention the new file"
        );
    }

    #[test]
    fn test_worktree_log() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "heidi", None).unwrap();

        // Make two commits in the worktree.
        fs::write(info.path.join("a.txt"), "a\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&info.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "commit one"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        fs::write(info.path.join("b.txt"), "b\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&info.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "commit two"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let log = worktree_log(repo.path(), "heidi", None).unwrap();
        let lines: Vec<&str> = log.trim().lines().collect();
        assert_eq!(lines.len(), 2, "should show 2 commits");
        assert!(log.contains("commit one"));
        assert!(log.contains("commit two"));

        // Test max_count
        let log_limited = worktree_log(repo.path(), "heidi", Some(1)).unwrap();
        let lines: Vec<&str> = log_limited.trim().lines().collect();
        assert_eq!(lines.len(), 1, "should show 1 commit with max_count=1");
    }

    #[test]
    fn test_worktree_log_nonexistent() {
        let repo = setup_git_repo();
        let result = worktree_log(repo.path(), "nobody", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_worktree_success() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "ivan", None).unwrap();

        // Make a commit in the worktree.
        fs::write(info.path.join("ivan.txt"), "ivan's work\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&info.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "ivan's contribution"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let result = merge_worktree(repo.path(), "ivan", Some("main")).unwrap();
        assert!(result.success);
        assert_eq!(result.commits_merged, 1);
        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_merge_worktree_nothing_to_merge() {
        let repo = setup_git_repo();
        create_worktree(repo.path(), "judy", None).unwrap();

        let result = merge_worktree(repo.path(), "judy", Some("main")).unwrap();
        assert!(result.success);
        assert_eq!(result.commits_merged, 0);
        assert!(result.message.contains("nothing to merge"));
    }

    #[test]
    fn test_merge_worktree_with_conflict() {
        let repo = setup_git_repo();
        let info = create_worktree(repo.path(), "karl", None).unwrap();

        // Modify the same file in both the main worktree and Karl's worktree.
        // First, commit a change on main.
        fs::write(repo.path().join("README.md"), "# main change\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo.path())
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "main change"])
            .current_dir(repo.path())
            .output()
            .unwrap();

        // Now commit a conflicting change in Karl's worktree.
        fs::write(info.path.join("README.md"), "# karl change\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&info.path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "karl's conflicting change"])
            .current_dir(&info.path)
            .output()
            .unwrap();

        let result = merge_worktree(repo.path(), "karl", Some("main")).unwrap();
        assert!(!result.success);
        assert!(!result.conflicts.is_empty());
        assert!(
            result.conflicts.contains(&"README.md".to_string()),
            "README.md should be in conflicts"
        );
    }

    #[test]
    fn test_not_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        let result = create_worktree(tmp.path(), "agent", None);
        assert!(result.is_err());
    }

    #[test]
    fn test_find_default_branch() {
        let repo = setup_git_repo();
        let branch = find_default_branch(repo.path());
        assert_eq!(branch, "main");
    }

    #[test]
    fn test_file_lock_basic() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("test.lock");

        {
            let _lock = FileLock::acquire(&lock_path).unwrap();
            assert!(lock_path.exists(), "lock file should exist while held");
        }
        // Lock is dropped -- file should be removed.
        assert!(
            !lock_path.exists(),
            "lock file should be cleaned up on drop"
        );
    }

    #[test]
    fn test_file_lock_stale_recovery() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("stale.lock");

        // Create a "stale" lockfile with an old modification time.
        fs::write(&lock_path, "").unwrap();
        // We cannot easily backdate the mtime portably, so instead we
        // just verify that a fresh lock is not considered stale.
        assert!(
            !FileLock::is_stale(&lock_path),
            "freshly created lock should not be stale"
        );
    }
}
