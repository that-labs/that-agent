use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::sync::Arc;
use tokio::process::Command;
use tracing::warn;

use crate::{acl, hooks, state::AppState};

#[derive(serde::Deserialize)]
pub struct ServiceQuery {
    service: String,
}

/// GET /{repo}/info/refs?service=git-upload-pack|git-receive-pack
pub async fn info_refs(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(q): Query<ServiceQuery>,
) -> Result<Response, (StatusCode, String)> {
    let svc = &q.service;
    if svc != "git-upload-pack" && svc != "git-receive-pack" {
        return Err((StatusCode::BAD_REQUEST, "invalid service".into()));
    }

    let repo_path = state.ensure_repo(&repo).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let output = Command::new("git")
        .arg(svc.strip_prefix("git-").unwrap())
        .args(["--stateless-rpc", "--advertise-refs"])
        .arg(&repo_path)
        .output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git: {e}")))?;

    if !output.status.success() {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, String::from_utf8_lossy(&output.stderr).into()));
    }

    // Build pkt-line response: service announcement + flush + refs
    let mut body = Vec::new();
    let svc_line = format!("# service={svc}\n");
    body.extend(pkt_line(svc_line.as_bytes()));
    body.extend(b"0000"); // flush
    body.extend(&output.stdout);

    Ok(Response::builder()
        .header("Content-Type", format!("application/x-{svc}-advertisement"))
        .header("Cache-Control", "no-cache")
        .body(Body::from(body))
        .unwrap())
}

/// POST /{repo}/git-upload-pack — fetch/clone
pub async fn upload_pack(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let repo_path = state.ensure_repo(&repo).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    run_service("upload-pack", &repo_path, &body).await
}

/// POST /{repo}/git-receive-pack — push
pub async fn receive_pack(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, (StatusCode, String)> {
    let agent = headers
        .get("X-Agent-Name")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // ACL: parse ref commands from the request body, enforce branch policy
    let refs = acl::parse_ref_commands(&body);
    if let Some(agent_name) = &agent {
        if let Err(msg) = acl::check(&refs, agent_name) {
            return Err((StatusCode::FORBIDDEN, msg));
        }
    }

    let repo_path = state.ensure_repo(&repo).await.map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let response = run_service("receive-pack", &repo_path, &body).await?;

    // Post-receive: record push, fire webhook, optionally auto-merge
    let repo_clean = repo.trim_end_matches(".git").to_string();
    for r in &refs {
        state.record_push(&repo_clean, &r.refname);
    }
    hooks::on_push(Arc::clone(&state), repo_clean, agent, refs);

    Ok(response)
}

/// Pipe request body to `git {service} --stateless-rpc {repo}`, stream stdout back.
async fn run_service(
    service: &str,
    repo_path: &std::path::Path,
    input: &[u8],
) -> Result<Response, (StatusCode, String)> {
    let mut child = Command::new("git")
        .arg(service)
        .args(["--stateless-rpc"])
        .arg(repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn git: {e}")))?;

    // Write request body to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(e) = stdin.write_all(input).await {
            warn!("stdin write: {e}");
        }
        drop(stdin);
    }

    // Buffer stdout (V1 — workspace repos are small; swap to streaming if needed)
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("git wait: {e}")))?;

    Ok(Response::builder()
        .header("Content-Type", format!("application/x-git-{service}-result"))
        .header("Cache-Control", "no-cache")
        .body(Body::from(output.stdout))
        .unwrap())
}

/// Encode a single pkt-line: 4-hex-digit length prefix + data.
fn pkt_line(data: &[u8]) -> Vec<u8> {
    let len = data.len() + 4;
    let mut out = format!("{len:04x}").into_bytes();
    out.extend_from_slice(data);
    out
}
