use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::process::Command;

use crate::state::AppState;

#[derive(Serialize)]
pub(crate) struct RepoInfo {
    name: String,
}

#[derive(Serialize)]
pub(crate) struct BranchInfo {
    name: String,
    ahead: u32,
    behind: u32,
    last_commit: String,
}

#[derive(Serialize)]
pub(crate) struct Activity {
    repo: String,
    branches: Vec<BranchInfo>,
}

/// GET /api/repos — list all repos
pub async fn list_repos(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<RepoInfo>>, (StatusCode, String)> {
    let mut repos = Vec::new();
    let mut entries = tokio::fs::read_dir(&state.repo_root)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("readdir: {e}")))?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".git") {
            repos.push(RepoInfo { name });
        }
    }
    Ok(Json(repos))
}

/// POST /api/repos/{repo} — create a bare repo
pub async fn create_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    state.ensure_repo(&repo).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let name = if repo.ends_with(".git") { repo } else { format!("{repo}.git") };
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "name": name, "created": true }))))
}

/// DELETE /api/repos/{repo} — remove a repo
pub async fn delete_repo(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let path = state.repo_path(&repo).ok_or((StatusCode::BAD_REQUEST, "invalid repo name".into()))?;
    if !path.exists() {
        return Err((StatusCode::NOT_FOUND, "repo not found".into()));
    }
    tokio::fs::remove_dir_all(&path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")))?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/repos/{repo}/activity — branches with ahead/behind counts
pub async fn repo_activity(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<Activity>, (StatusCode, String)> {
    let path = state.repo_path(&repo).ok_or((StatusCode::BAD_REQUEST, "invalid repo name".into()))?;
    if !path.exists() {
        return Err((StatusCode::NOT_FOUND, "repo not found".into()));
    }

    let branch_out = Command::new("git")
        .arg("-C").arg(&path)
        .args(["branch", "--format=%(refname:short)"])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git branch: {e}")))?;

    let branch_list = String::from_utf8_lossy(&branch_out.stdout);
    let mut branches = Vec::new();

    for name in branch_list.lines().filter(|l| !l.is_empty()) {
        let (ahead, behind) = if name != "main" {
            ahead_behind(&path, name).await
        } else {
            (0, 0)
        };

        let last_commit = Command::new("git")
            .arg("-C").arg(&path)
            .args(["log", "-1", "--format=%H %ci %s", name])
            .output()
            .await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();

        branches.push(BranchInfo { name: name.to_string(), ahead, behind, last_commit });
    }

    let repo_name = repo.trim_end_matches(".git").to_string();
    Ok(Json(Activity { repo: repo_name, branches }))
}

/// GET /api/repos/{repo}/diff/{*branch} — unified diff of branch vs main
pub async fn branch_diff(
    State(state): State<Arc<AppState>>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let path = state.repo_path(&repo).ok_or((StatusCode::BAD_REQUEST, "invalid repo name".into()))?;
    if !path.exists() {
        return Err((StatusCode::NOT_FOUND, "repo not found".into()));
    }

    let output = Command::new("git")
        .arg("-C").arg(&path)
        .args(["diff", &format!("main...{branch}")])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git diff: {e}")))?;

    if !output.status.success() {
        return Err((StatusCode::BAD_REQUEST, String::from_utf8_lossy(&output.stderr).into()));
    }

    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        output.stdout,
    ))
}

#[derive(Serialize)]
pub(crate) struct ConflictInfo {
    repo: String,
    base: String,
    branch: String,
    conflicting_files: Vec<String>,
    diff_branch: String,
    diff_main: String,
}

/// GET /api/repos/{repo}/conflicts/{*branch} — conflict analysis against main
pub async fn branch_conflicts(
    State(state): State<Arc<AppState>>,
    Path((repo, branch)): Path<(String, String)>,
) -> Result<Json<ConflictInfo>, (StatusCode, String)> {
    let path = state.repo_path(&repo).ok_or((StatusCode::BAD_REQUEST, "invalid repo name".into()))?;
    if !path.exists() {
        return Err((StatusCode::NOT_FOUND, "repo not found".into()));
    }

    // merge-tree --write-tree outputs conflicting file info on failure (git 2.38+)
    let mt = Command::new("git")
        .arg("-C").arg(&path)
        .args(["merge-tree", "--write-tree", "--name-only", "main", &branch])
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("merge-tree: {e}")))?;

    let conflicting_files = if mt.status.success() {
        vec![] // no conflicts
    } else {
        // merge-tree stderr/stdout lists conflicting paths (one per line after the tree hash line)
        String::from_utf8_lossy(&mt.stdout)
            .lines()
            .skip(1) // first line is the (partial) tree OID
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect()
    };

    // What the branch changed vs merge-base
    let diff_branch = Command::new("git")
        .arg("-C").arg(&path)
        .args(["diff", &format!("main...{branch}")])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    // What main changed since the branch diverged
    let diff_main = Command::new("git")
        .arg("-C").arg(&path)
        .args(["diff", &format!("{branch}...main")])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    let repo_name = repo.trim_end_matches(".git").to_string();
    Ok(Json(ConflictInfo {
        repo: repo_name,
        base: "main".into(),
        branch,
        conflicting_files,
        diff_branch,
        diff_main,
    }))
}

async fn ahead_behind(repo_path: &std::path::Path, branch: &str) -> (u32, u32) {
    let out = Command::new("git")
        .arg("-C").arg(repo_path)
        .args(["rev-list", "--left-right", "--count", &format!("{branch}...main")])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let parts: Vec<&str> = s.trim().split('\t').collect();
            if parts.len() == 2 {
                let ahead = parts[0].parse().unwrap_or(0);
                let behind = parts[1].parse().unwrap_or(0);
                return (ahead, behind);
            }
            (0, 0)
        }
        _ => (0, 0),
    }
}
