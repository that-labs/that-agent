use std::sync::Arc;
use tracing::{info, warn};

use crate::{acl::RefCommand, state::AppState};

fn branch_task_id(branch: &str) -> Option<&str> {
    let mut parts = branch.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("task"), Some(_worker), Some(task_id), None) if !task_id.trim().is_empty() => {
            Some(task_id)
        }
        _ => None,
    }
}

/// Fire-and-forget post-receive hooks: webhook notification + optional auto-merge.
pub fn on_push(state: Arc<AppState>, repo: String, agent: Option<String>, refs: Vec<RefCommand>) {
    tokio::spawn(async move {
        let agent_str = agent.as_deref().unwrap_or("orchestrator");
        let client = reqwest::Client::new(); // one client per push, reused for all refs + auto-merge
        for r in &refs {
            let branch = r.refname.strip_prefix("refs/heads/").unwrap_or(&r.refname);

            info!(
                repo = %repo,
                branch = %branch,
                agent = %agent_str,
                commit = %r.new,
                "push received"
            );

            // Webhook notification (per-repo URL, then global fallback)
            // Payload must include "message" field for /v1/notify compatibility.
            if let Some(url) = state.webhook_url_for(&repo) {
                let mut payload = serde_json::json!({
                    "message": format!("git push: {agent_str} pushed {branch} to {repo} ({})", &r.new[..8.min(r.new.len())]),
                    "agent": format!("git-server/{agent_str}"),
                    "event": "push",
                    "repo": repo,
                    "branch": branch,
                    "commit": r.new,
                });
                if let Some(task_id) = branch_task_id(branch) {
                    payload["task_id"] = serde_json::json!(task_id);
                }
                match client
                    .post(&url)
                    .json(&payload)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await
                {
                    Ok(resp) => {
                        info!(repo = %repo, url = %url, status = %resp.status(), "webhook delivered")
                    }
                    Err(e) => warn!(repo = %repo, url = %url, "webhook failed: {e}"),
                }
            }

            // Auto-merge: if enabled and this is a task branch, try merge into main
            if state.auto_merge && branch.starts_with("task/") {
                auto_merge(&state, &client, &repo, branch).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::branch_task_id;

    #[test]
    fn branch_task_id_extracts_only_task_scoped_suffix() {
        assert_eq!(branch_task_id("task/worker/task-123"), Some("task-123"));
        assert_eq!(branch_task_id("task/worker"), None);
        assert_eq!(branch_task_id("main"), None);
        assert_eq!(branch_task_id("task/worker/task-123/extra"), None);
    }
}

/// Auto-merge a task branch into main in a bare repo.
///
/// Uses `git merge-tree --write-tree` to compute the merged tree OID without a working tree,
/// then `git commit-tree` + `git update-ref` to create the merge commit and advance main.
async fn auto_merge(state: &AppState, client: &reqwest::Client, repo: &str, branch: &str) {
    let repo_path = match state.repo_path(repo) {
        Some(p) if p.exists() => p,
        _ => return,
    };

    let cmd = |args: &[&str]| {
        let mut c = tokio::process::Command::new("git");
        c.arg("-C").arg(&repo_path).args(args);
        c
    };

    // Step 1: merge-tree --write-tree produces the merged tree OID (works on bare repos)
    let merge_tree = cmd(&["merge-tree", "--write-tree", "main", branch])
        .output()
        .await;

    match merge_tree {
        Ok(out) if out.status.success() => {
            let tree_oid = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if tree_oid.is_empty() {
                warn!("auto-merge: empty tree OID from merge-tree");
                return;
            }

            // Step 2: resolve parent commits (main and branch tips)
            let main_oid = match cmd(&["rev-parse", "refs/heads/main"]).output().await {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => {
                    warn!("auto-merge: cannot resolve main");
                    return;
                }
            };
            let branch_oid = match cmd(&["rev-parse", &format!("refs/heads/{branch}")])
                .output()
                .await
            {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                _ => {
                    warn!("auto-merge: cannot resolve {branch}");
                    return;
                }
            };

            // Step 3: commit-tree with two parents (merge commit)
            let msg = format!("Auto-merge {branch}");
            let commit = cmd(&[
                "commit-tree",
                &tree_oid,
                "-p",
                &main_oid,
                "-p",
                &branch_oid,
                "-m",
                &msg,
            ])
            .output()
            .await;

            let commit_oid = match commit {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).trim().to_string()
                }
                Ok(o) => {
                    warn!(
                        "auto-merge commit-tree failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                    return;
                }
                Err(e) => {
                    warn!("auto-merge commit-tree: {e}");
                    return;
                }
            };

            // Step 4: advance main to the new merge commit
            let update = cmd(&["update-ref", "refs/heads/main", &commit_oid, &main_oid])
                .output()
                .await;

            match update {
                Ok(o) if o.status.success() => {
                    info!("auto-merged {branch} into main in {repo} ({commit_oid})");
                    // Notify success via webhook
                    if let Some(url) = state.webhook_url_for(repo) {
                        let mut payload = serde_json::json!({
                            "message": format!("auto-merged {branch} into main in {repo}"),
                            "agent": "git-server",
                            "event": "auto_merge",
                            "repo": repo,
                            "branch": branch,
                            "commit": commit_oid,
                        });
                        if let Some(task_id) = branch_task_id(branch) {
                            payload["task_id"] = serde_json::json!(task_id);
                        }
                        let _ = reqwest::Client::new()
                            .post(url)
                            .json(&payload)
                            .timeout(std::time::Duration::from_secs(5))
                            .send()
                            .await;
                    }
                }
                Ok(o) => warn!(
                    "auto-merge update-ref failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => warn!("auto-merge update-ref: {e}"),
            }
        }
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let conflicting_files: Vec<&str> =
                stdout.lines().skip(1).filter(|l| !l.is_empty()).collect();
            info!("auto-merge skipped (conflict) for {branch} in {repo}: {conflicting_files:?}");
            if let Some(url) = state.webhook_url_for(repo) {
                let mut payload = serde_json::json!({
                    "message": format!("merge conflict: {branch} in {repo} — files: {conflicting_files:?}"),
                    "agent": "git-server",
                    "event": "merge_conflict",
                    "repo": repo,
                    "branch": branch,
                    "conflicting_files": conflicting_files,
                });
                if let Some(task_id) = branch_task_id(branch) {
                    payload["task_id"] = serde_json::json!(task_id);
                }
                let _ = client
                    .post(url)
                    .json(&payload)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await;
            }
        }
        Err(e) => warn!("merge-tree: {e}"),
    }
}
