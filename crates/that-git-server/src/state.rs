use chrono::{DateTime, Utc};
use std::{collections::HashMap, path::PathBuf, sync::Mutex};

pub struct AppState {
    pub repo_root: PathBuf,
    pub webhook_url: Option<String>,
    pub expiry_hours: u64,
    pub auto_merge: bool,
    /// repo_name -> (branch -> last_push_time). Seeded from filesystem mtime on startup,
    /// updated in-memory on each push. Expiry uses fs mtime as fallback after restart.
    pub last_push: Mutex<HashMap<String, HashMap<String, DateTime<Utc>>>>,
}

impl AppState {
    pub fn new(
        repo_root: PathBuf,
        webhook_url: Option<String>,
        expiry_hours: u64,
        auto_merge: bool,
    ) -> Self {
        Self { repo_root, webhook_url, expiry_hours, auto_merge, last_push: Mutex::new(HashMap::new()) }
    }

    /// Resolve repo path, ensuring it's under repo_root. Returns None on traversal attempt.
    pub fn repo_path(&self, repo: &str) -> Option<PathBuf> {
        let clean = repo.trim_matches('/').replace("..", "");
        if clean.is_empty() || clean.contains('/') {
            return None;
        }
        let name = if clean.ends_with(".git") { clean } else { format!("{clean}.git") };
        Some(self.repo_root.join(name))
    }

    /// Ensure a bare repo exists at the given path. Auto-inits on first access.
    pub async fn ensure_repo(&self, repo: &str) -> Result<PathBuf, String> {
        let path = self.repo_path(repo).ok_or("invalid repo name")?;
        if !path.exists() {
            let out = tokio::process::Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&path)
                .output()
                .await
                .map_err(|e| format!("git init: {e}"))?;
            if !out.status.success() {
                return Err(format!("git init failed: {}", String::from_utf8_lossy(&out.stderr)));
            }
            let _ = tokio::process::Command::new("git")
                .arg("-C").arg(&path)
                .args(["config", "http.receivepack", "true"])
                .status()
                .await;
        }
        Ok(path)
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
