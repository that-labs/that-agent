use chrono::{DateTime, Utc};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

pub struct AppState {
    pub repo_root: PathBuf,
    pub default_webhook_url: Option<String>,
    pub expiry_hours: u64,
    pub auto_merge: bool,
    /// repo_name -> (branch -> last_push_time). Seeded from filesystem mtime on startup,
    /// updated in-memory on each push. Expiry uses fs mtime as fallback after restart.
    pub last_push: Mutex<HashMap<String, HashMap<String, DateTime<Utc>>>>,
    /// Per-repo webhook URLs. Agents register their gateway URL when sharing a workspace.
    pub repo_webhooks: Mutex<HashMap<String, String>>,
}

impl AppState {
    pub fn new(
        repo_root: PathBuf,
        webhook_url: Option<String>,
        expiry_hours: u64,
        auto_merge: bool,
    ) -> Self {
        Self {
            repo_root,
            default_webhook_url: webhook_url.filter(|u| !u.trim().is_empty()),
            expiry_hours,
            auto_merge,
            last_push: Mutex::new(HashMap::new()),
            repo_webhooks: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve repo path, ensuring it's under repo_root. Returns None on traversal attempt.
    pub fn repo_path(&self, repo: &str) -> Option<PathBuf> {
        let clean = repo.trim_matches('/').replace("..", "");
        if clean.is_empty() || clean.contains('/') {
            return None;
        }
        let name = if clean.ends_with(".git") {
            clean
        } else {
            format!("{clean}.git")
        };
        Some(self.repo_root.join(name))
    }

    /// Ensure a bare repo exists at the given path. Auto-inits on first access.
    pub async fn ensure_repo(&self, repo: &str) -> Result<PathBuf, String> {
        let path = self.repo_path(repo).ok_or("invalid repo name")?;
        if !path.exists() {
            let out = tokio::process::Command::new("git")
                .args(["init", "--bare", "--quiet", "--initial-branch=main"])
                .arg(&path)
                .output()
                .await
                .map_err(|e| format!("git init: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "git init failed: {}",
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            let _ = tokio::process::Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["config", "http.receivepack", "true"])
                .status()
                .await;
        }
        Ok(path)
    }

    /// Resolve the webhook URL for a repo: per-repo override, then global default.
    pub fn webhook_url_for(&self, repo: &str) -> Option<String> {
        if let Ok(map) = self.repo_webhooks.lock() {
            if let Some(url) = map.get(repo) {
                return Some(url.clone());
            }
        }
        self.default_webhook_url.clone()
    }

    /// Register a webhook URL for a specific repo.
    pub fn register_webhook(&self, repo: &str, url: &str) {
        if let Ok(mut map) = self.repo_webhooks.lock() {
            map.insert(repo.to_string(), url.to_string());
        }
    }

    /// Record a push timestamp for a repo/branch.
    pub fn record_push(&self, repo: &str, branch: &str) {
        if let Ok(mut map) = self.last_push.lock() {
            map.entry(repo.to_string())
                .or_default()
                .insert(branch.to_string(), Utc::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new(PathBuf::from("/repos"), None, 24, false)
    }

    #[test]
    fn repo_path_appends_git_suffix() {
        let s = state();
        assert_eq!(
            s.repo_path("workspace").unwrap(),
            PathBuf::from("/repos/workspace.git")
        );
    }

    #[test]
    fn repo_path_keeps_existing_git_suffix() {
        let s = state();
        assert_eq!(
            s.repo_path("workspace.git").unwrap(),
            PathBuf::from("/repos/workspace.git")
        );
    }

    #[test]
    fn repo_path_rejects_traversal() {
        let s = state();
        assert!(s.repo_path("../etc/passwd").is_none());
        assert!(s.repo_path("..").is_none());
        assert!(s.repo_path("foo/../bar").is_none());
    }

    #[test]
    fn repo_path_rejects_slashes() {
        let s = state();
        assert!(s.repo_path("a/b").is_none());
    }

    #[test]
    fn repo_path_trims_leading_slashes() {
        let s = state();
        // Leading slashes are trimmed, resulting in a valid name
        assert_eq!(
            s.repo_path("/absolute").unwrap(),
            PathBuf::from("/repos/absolute.git")
        );
    }

    #[test]
    fn repo_path_rejects_empty() {
        let s = state();
        assert!(s.repo_path("").is_none());
        assert!(s.repo_path("/").is_none());
        assert!(s.repo_path("///").is_none());
    }

    #[test]
    fn record_push_stores_timestamps() {
        let s = state();
        s.record_push("workspace", "refs/heads/task/w1");
        s.record_push("workspace", "refs/heads/task/w2");
        let map = s.last_push.lock().unwrap();
        assert_eq!(map["workspace"].len(), 2);
        assert!(map["workspace"].contains_key("refs/heads/task/w1"));
    }
}
