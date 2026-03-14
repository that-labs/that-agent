use std::sync::Arc;
use tracing::{info, warn};

use crate::{acl::RefCommand, state::AppState};

/// Fire-and-forget post-receive hooks: webhook notification + optional auto-merge.
pub fn on_push(state: Arc<AppState>, repo: String, agent: Option<String>, refs: Vec<RefCommand>) {
    tokio::spawn(async move {
        for r in &refs {
            let branch = r.refname.strip_prefix("refs/heads/").unwrap_or(&r.refname);

            // Webhook notification
            if let Some(url) = &state.webhook_url {
                let payload = serde_json::json!({
                    "event": "push",
                    "repo": repo,
                    "branch": branch,
                    "agent": agent.as_deref().unwrap_or("orchestrator"),
                    "old_commit": r.old,
                    "commit": r.new,
                });
                let client = reqwest::Client::new();
                match client.post(url).json(&payload).timeout(std::time::Duration::from_secs(5)).send().await {
                    Ok(resp) => info!("webhook -> {} {}", url, resp.status()),
                    Err(e) => warn!("webhook failed: {e}"),
                }
            }

            // Auto-merge: if enabled and this is a task branch, try fast-forward into main
            if state.auto_merge && branch.starts_with("task/") {
                auto_merge(&state, &repo, branch).await;
            }
        }
    });
}

async fn auto_merge(state: &AppState, repo: &str, branch: &str) {
    let repo_path = match state.repo_path(repo) {
        Some(p) if p.exists() => p,
        _ => return,
    };

    // Check if branch can be merged cleanly using merge-tree (git 2.38+)
    let merge_tree = tokio::process::Command::new("git")
        .arg("-C").arg(&repo_path)
        .args(["merge-tree", "--write-tree", "main", branch])
        .output()
        .await;

    match merge_tree {
        Ok(out) if out.status.success() => {
            // Clean merge possible — perform it
            let merge = tokio::process::Command::new("git")
                .arg("-C").arg(&repo_path)
                .args(["merge", branch, "--no-ff", "-m"])
                .arg(format!("Auto-merge {branch}"))
                .output()
                .await;
            match merge {
                Ok(o) if o.status.success() => info!("auto-merged {branch} into main in {repo}"),
                Ok(o) => warn!("auto-merge exec failed: {}", String::from_utf8_lossy(&o.stderr)),
                Err(e) => warn!("auto-merge spawn: {e}"),
            }
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // merge-tree output: first line is tree OID, remaining lines are conflicting paths
            let conflicting_files: Vec<&str> = stdout.lines().skip(1).filter(|l| !l.is_empty()).collect();
            info!("auto-merge skipped (conflict) for {branch} in {repo}: {conflicting_files:?}");
            if let Some(url) = &state.webhook_url {
                let payload = serde_json::json!({
                    "event": "merge_conflict",
                    "repo": repo,
                    "branch": branch,
                    "conflicting_files": conflicting_files,
                });
                let _ = reqwest::Client::new()
                    .post(url).json(&payload)
                    .timeout(std::time::Duration::from_secs(5))
                    .send().await;
            }
        }
        Err(e) => warn!("merge-tree: {e}"),
    }
}
