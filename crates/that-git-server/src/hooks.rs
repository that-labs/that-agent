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
                match client
                    .post(url)
                    .json(&payload)
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await
                {
                    Ok(resp) => info!("webhook -> {} {}", url, resp.status()),
                    Err(e) => warn!("webhook failed: {e}"),
                }
            }

            // Auto-merge: if enabled and this is a task branch, try merge into main
            if state.auto_merge && branch.starts_with("task/") {
                auto_merge(&state, &repo, branch).await;
            }
        }
    });
}

/// Auto-merge a task branch into main in a bare repo.
///
/// Uses `git merge-tree --write-tree` to compute the merged tree OID without a working tree,
/// then `git commit-tree` + `git update-ref` to create the merge commit and advance main.
async fn auto_merge(state: &AppState, repo: &str, branch: &str) {
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
                    if let Some(url) = &state.webhook_url {
                        let payload = serde_json::json!({
                            "event": "auto_merge",
                            "repo": repo,
                            "branch": branch,
                            "commit": commit_oid,
                        });
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
            if let Some(url) = &state.webhook_url {
                let payload = serde_json::json!({
                    "event": "merge_conflict",
                    "repo": repo,
                    "branch": branch,
                    "conflicting_files": conflicting_files,
                });
                let _ = reqwest::Client::new()
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
