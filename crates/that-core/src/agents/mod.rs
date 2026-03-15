//! Agent lifecycle management — spawn, list, and query peer agents.
//!
//! In Kubernetes mode, agents are K8s Deployments (persistent) or Jobs (ephemeral).
//! Agents registered here persist across sessions via a file-backed registry
//! at `~/.that-agent/cluster/agents.json`. The spawning agent writes a
//! `config.toml` for the child and starts the binary in the background.
//!
//! In Kubernetes mode (`THAT_SANDBOX_MODE=kubernetes`), agents are created as
//! K8s Deployments (persistent) or Jobs (ephemeral) instead of local processes.
//! The file-backed registry is replaced by K8s labels as source of truth.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── Registry ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub name: String,
    pub role: Option<String>,
    pub parent: Option<String>,
    pub pid: u32,
    pub gateway_url: Option<String>,
    pub started_at: String,
}

/// File-backed registry of known peer agents in the cluster.
///
/// Persisted at `<home>/.that-agent/cluster/agents.json`.
/// Entries are stale when the PID is dead — callers should check `is_alive`.
pub struct AgentRegistry {
    path: PathBuf,
}

impl AgentRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Upsert an agent entry (keyed by name).
    pub fn register(&self, entry: AgentEntry) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.name != entry.name);
        entries.push(entry);
        self.save(&entries)
    }

    /// Remove a named agent from the registry.
    pub fn unregister(&self, name: &str) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.name != name);
        self.save(&entries)
    }

    /// Return all registered agents.
    pub fn list(&self) -> Result<Vec<AgentEntry>> {
        self.load()
    }

    fn load(&self) -> Result<Vec<AgentEntry>> {
        match std::fs::read_to_string(&self.path) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, entries: &[AgentEntry]) -> Result<()> {
        that_channels::atomic_write_json(&self.path, entries)
    }

    /// Return `true` if the OS process with `pid` is still running.
    pub fn is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ── Spawn (local) ────────────────────────────────────────────────────────────

/// Spawn a named sub-agent in the background and register it.
///
/// Writes `~/.that-agent/agents/<name>/config.toml`, starts the agent binary
/// (resolved via `current_exe`), and records the PID in the registry.
///
/// Returns the registry entry for the new agent.
pub async fn spawn_agent(
    name: &str,
    role: Option<&str>,
    parent: Option<&str>,
    gateway_port: Option<u16>,
    model: Option<&str>,
    agent_registry: &AgentRegistry,
) -> Result<AgentEntry> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home dir"))?;
    let agent_dir = home.join(".that-agent").join("agents").join(name);
    std::fs::create_dir_all(&agent_dir)?;

    // Write minimal config.toml.
    let config_toml = build_config_toml(role, parent, model, gateway_port);
    std::fs::write(agent_dir.join("config.toml"), &config_toml)?;

    // Start the agent binary.
    let binary = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("--agent").arg(name).arg("run");
    if let Some(port) = gateway_port {
        cmd.env("THAT_GATEWAY_ADDR", format!("127.0.0.1:{port}"));
    }
    // Pass parent's gateway URL so the child can post notifications back.
    cmd.env(
        "THAT_PARENT_GATEWAY_URL",
        crate::orchestration::support::resolve_gateway_url(),
    );
    if let Ok(tok) = std::env::var("THAT_GATEWAY_TOKEN") {
        cmd.env("THAT_PARENT_GATEWAY_TOKEN", tok);
    }
    // Detach: let the child outlive the parent.
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    let child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get child PID"))?;

    let gateway_url = gateway_port.map(|p| format!("http://localhost:{p}"));
    let entry = AgentEntry {
        name: name.to_string(),
        role: role.map(str::to_string),
        parent: parent.map(str::to_string),
        pid,
        gateway_url,
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    agent_registry.register(entry.clone())?;
    Ok(entry)
}

/// Query an agent's `/v1/chat` endpoint synchronously.
///
/// `gateway_url` must be the base URL (e.g. `http://localhost:8081`).
/// Returns the agent's response text.
pub async fn query_agent(gateway_url: &str, message: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gateway_url}/v1/chat"))
        .json(&serde_json::json!({ "message": message, "sender_id": "parent" }))
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    Ok(body["text"].as_str().unwrap_or_default().to_string())
}

/// SSE event received from a streaming agent query.
#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    ToolCall { name: String, args: String },
    ToolResult { name: String, result: String },
    Done { text: String },
    Error { message: String },
}

/// Query an agent's `/v1/chat/stream` SSE endpoint, sending events to `event_tx`.
///
/// Only relays `tool_call`, `tool_result`, and `done` events. Returns the final text.
pub async fn query_agent_stream(
    gateway_url: &str,
    agent_name: &str,
    message: &str,
    timeout_secs: u64,
    event_tx: tokio::sync::mpsc::UnboundedSender<AgentStreamEvent>,
) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gateway_url}/v1/chat/stream"))
        .json(&serde_json::json!({ "message": message, "sender_id": "parent" }))
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await?
        .error_for_status()?;

    let mut final_text = String::new();
    let mut got_terminal = false;
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut current_event_type = String::new();

    use futures::StreamExt;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // Parse SSE frames: "event: <type>\ndata: <json>\n\n"
        while let Some(boundary) = buf.find("\n\n") {
            let frame = buf[..boundary].to_string();
            buf = buf[boundary + 2..].to_string();

            for line in frame.lines() {
                if let Some(ev) = line.strip_prefix("event: ") {
                    current_event_type = ev.trim().to_string();
                } else if let Some(data) = line.strip_prefix("data: ") {
                    let parsed: serde_json::Value =
                        serde_json::from_str(data.trim()).unwrap_or_default();
                    match current_event_type.as_str() {
                        "tool_call" => {
                            let name = format!(
                                "{}/{}",
                                agent_name,
                                parsed["name"].as_str().unwrap_or("unknown")
                            );
                            let args = parsed["args"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::ToolCall { name, args });
                        }
                        "tool_result" => {
                            let name = format!(
                                "{}/{}",
                                agent_name,
                                parsed["name"].as_str().unwrap_or("unknown")
                            );
                            let result = parsed["result"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::ToolResult { name, result });
                        }
                        "done" => {
                            got_terminal = true;
                            final_text = parsed["text"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::Done {
                                text: final_text.clone(),
                            });
                        }
                        "error" => {
                            let err_msg = parsed["error"]
                                .as_str()
                                .unwrap_or("unknown error")
                                .to_string();
                            let _ = event_tx.send(AgentStreamEvent::Error {
                                message: err_msg.clone(),
                            });
                            return Err(anyhow::anyhow!(
                                "sub-agent '{agent_name}' error: {err_msg}"
                            ));
                        }
                        _ => {} // skip stream_token, etc.
                    }
                    current_event_type.clear();
                }
            }
        }
    }
    // Stream ended without a terminal event — sub-agent was likely aborted.
    if !got_terminal {
        let msg = format!("sub-agent '{agent_name}' stream ended without completion (likely aborted or hit tool limit)");
        let _ = event_tx.send(AgentStreamEvent::Error {
            message: msg.clone(),
        });
        return Err(anyhow::anyhow!(msg));
    }
    Ok(final_text)
}

/// Fire-and-forget: post a message to a sub-agent's `/v1/inbound` with a callback URL.
///
/// Returns immediately after the POST succeeds. The sub-agent will process
/// asynchronously and POST its result to `callback_url` when done.
pub async fn query_agent_async(
    gateway_url: &str,
    parent_name: &str,
    parent_gateway_url: &str,
    message: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let callback = format!("{parent_gateway_url}/v1/notify");
    client
        .post(format!("{gateway_url}/v1/inbound"))
        .json(&serde_json::json!({
            "message": message,
            "sender_id": parent_name,
            "callback_url": callback,
        }))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

// ── K8s mode detection ───────────────────────────────────────────────────────

/// Returns `true` when running in K8s sandbox mode.
pub fn is_k8s_mode() -> bool {
    matches!(
        std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    )
}

/// Resolve the container image for child agents.
///
/// 1. `THAT_AGENT_IMAGE` env var (explicit override)
/// 2. Own pod image via `kubectl get pod $HOSTNAME`
/// 3. Fallback default
pub async fn resolve_agent_image() -> String {
    if let Ok(img) = std::env::var("THAT_AGENT_IMAGE") {
        if !img.trim().is_empty() {
            return img.trim().to_string();
        }
    }
    if let Ok(hostname) = std::env::var("HOSTNAME") {
        let output = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "pod",
                &hostname,
                "-o",
                "jsonpath={.spec.containers[0].image}",
            ])
            .output()
            .await;
        if let Ok(o) = output {
            if o.status.success() {
                let img = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !img.is_empty() {
                    return img;
                }
            }
        }
    }
    "ghcr.io/that-labs/that-agent:latest".to_string()
}

/// Get the parent deployment's UID for ownerReferences.
async fn parent_deploy_uid() -> Result<String> {
    let deploy_name =
        std::env::var("THAT_K8S_DEPLOYMENT_NAME").unwrap_or_else(|_| "that-agent".to_string());
    let output = tokio::process::Command::new("kubectl")
        .args([
            "get",
            "deployment",
            &deploy_name,
            "-o",
            "jsonpath={.metadata.uid}",
        ])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "failed to get parent deploy UID");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Get the K8s namespace from env (POD_NAMESPACE) or default.
fn k8s_namespace() -> String {
    std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

fn sanitize_name(input: &str) -> String {
    that_sandbox::kubernetes::sanitize_k8s_name(input)
}

// ── K8s Spawn — Persistent Agent ─────────────────────────────────────────────

/// Spawn a persistent agent as a K8s Deployment + Service.
///
/// Creates: ServiceAccount, RoleBinding, ConfigMap, Deployment, Service.
/// All resources are labeled for scoped management.
pub async fn spawn_persistent_agent_k8s(
    name: &str,
    role: Option<&str>,
    parent: &str,
    model: Option<&str>,
    env_overrides: Option<&std::collections::HashMap<String, String>>,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");
    let image = resolve_agent_image().await;
    let deploy_uid = parent_deploy_uid().await.unwrap_or_default();
    let parent_deploy =
        std::env::var("THAT_K8S_DEPLOYMENT_NAME").unwrap_or_else(|_| "that-agent".to_string());
    let parent_gw = crate::orchestration::support::resolve_gateway_url();
    let gw_token = std::env::var("THAT_GATEWAY_TOKEN").unwrap_or_default();

    let provider = std::env::var("THAT_AGENT_PROVIDER").unwrap_or_default();
    let model_str = model
        .map(crate::model_catalog::normalize_model)
        .or_else(|| std::env::var("THAT_AGENT_MODEL").ok())
        .unwrap_or_default();

    let cpu_limit = std::env::var("THAT_AGENT_CHILD_CPU_LIMIT").unwrap_or_else(|_| "1".into());
    let mem_limit = std::env::var("THAT_AGENT_CHILD_MEMORY_LIMIT").unwrap_or_else(|_| "2Gi".into());

    let role_str = role.unwrap_or("");
    let labels = k8s_labels(name, parent, "persistent", role_str);
    let owner_refs = k8s_owner_refs(&parent_deploy, &deploy_uid);

    let mut config_data = serde_json::json!({
        "THAT_AGENT_NAME": name,
        "THAT_AGENT_PARENT": parent,
        "THAT_AGENT_ROLE": role_str,
        "THAT_SANDBOX_MODE": "kubernetes",
        "THAT_TRUSTED_LOCAL_SANDBOX": "1",
        "THAT_AGENT_PROVIDER": provider,
        "THAT_AGENT_MODEL": model_str,
        "THAT_PARENT_GATEWAY_URL": parent_gw,
        "THAT_PARENT_GATEWAY_TOKEN": gw_token,
        "THAT_SANDBOX_K8S_NAMESPACE": ns,
        // Blank out channel tokens so children don't steal the parent's channels.
        // Children communicate via their HTTP gateway only.
        "TELEGRAM_BOT_TOKEN": "",
        "DISCORD_BOT_TOKEN": "",
        "SLACK_BOT_TOKEN": "",
        "SLACK_APP_TOKEN": "",
    });
    if let Ok(auth) = std::env::var("CLAUDE_CODE_AUTH") {
        config_data["CLAUDE_CODE_AUTH"] = serde_json::json!(auth);
    }

    // Apply caller-provided env overrides (e.g. a dedicated TELEGRAM_BOT_TOKEN).
    if let Some(overrides) = env_overrides {
        for (k, v) in overrides {
            config_data[k] = serde_json::json!(v);
        }
    }

    let resources = serde_json::json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                }
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "RoleBinding",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "roleRef": {
                    "apiGroup": "rbac.authorization.k8s.io",
                    "kind": "Role",
                    "name": "that-agent-child-readonly"
                },
                "subjects": [{
                    "kind": "ServiceAccount",
                    "name": sa_name,
                    "namespace": ns,
                }]
            },
            {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": format!("{sa_name}-config"),
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "data": config_data,
            },
            {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "spec": {
                    "replicas": 1,
                    "selector": { "matchLabels": { "that-agent/name": sanitize_label_value(name) } },
                    "template": {
                        "metadata": { "labels": labels },
                        "spec": {
                            "serviceAccountName": sa_name,
                            "automountServiceAccountToken": true,
                            "containers": [{
                                "name": "agent",
                                "image": image,
                                "command": ["that", "--agent", name, "run", "listen", "--no-sandbox"],
                                "envFrom": [
                                    { "secretRef": { "name": "that-agent-secrets", "optional": true } },
                                    { "configMapRef": { "name": format!("{sa_name}-config") } },
                                ],
                                "ports": [{ "containerPort": 8080, "name": "gateway" }],
                                "readinessProbe": {
                                    "exec": { "command": ["/bin/sh", "-c", "test -f /tmp/that-agent-ready"] },
                                    "initialDelaySeconds": 5,
                                    "periodSeconds": 5,
                                },
                                "livenessProbe": {
                                    "exec": { "command": ["/bin/sh", "-c",
                                        "test -f /tmp/that-agent-alive && [ $(( $(date +%s) - $(stat -c %Y /tmp/that-agent-alive) )) -lt 30 ]"] },
                                    "initialDelaySeconds": 30,
                                    "periodSeconds": 10,
                                },
                                "resources": {
                                    "requests": { "cpu": "200m", "memory": "256Mi" },
                                    "limits": { "cpu": cpu_limit, "memory": mem_limit },
                                },
                                "volumeMounts": [{ "name": "agent-home", "mountPath": "/home/agent/.that-agent" }],
                            }],
                            "volumes": [{ "name": "agent-home", "emptyDir": {} }],
                        }
                    }
                }
            },
            {
                "apiVersion": "v1",
                "kind": "Service",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "spec": {
                    "selector": { "that-agent/name": sanitize_label_value(name) },
                    "ports": [{ "name": "gateway", "port": 8080, "targetPort": 8080 }],
                }
            }
        ]
    });

    kubectl_apply_json(&resources).await?;

    let gateway_url = format!("http://{sa_name}.{ns}.svc.cluster.local:8080");
    Ok(serde_json::json!({
        "name": name,
        "type": "persistent",
        "gateway_url": gateway_url,
    }))
}

// ── K8s Spawn — Ephemeral Agent (agent_run) ──────────────────────────────────

/// Run an ephemeral task agent as a K8s Job. Blocks until completion.
pub async fn run_ephemeral_agent_k8s(
    name: &str,
    role: Option<&str>,
    task: &str,
    parent: &str,
    model: Option<&str>,
    workspace: bool,
    timeout_secs: u64,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");
    let image = resolve_agent_image().await;
    let deploy_uid = parent_deploy_uid().await.unwrap_or_default();
    let parent_deploy =
        std::env::var("THAT_K8S_DEPLOYMENT_NAME").unwrap_or_else(|_| "that-agent".to_string());
    let parent_gw = crate::orchestration::support::resolve_gateway_url();
    let gw_token = std::env::var("THAT_GATEWAY_TOKEN").unwrap_or_default();
    let git_svc = git_server_url(&ns);
    let proxy_svc = cache_proxy_url(&ns);

    let provider = std::env::var("THAT_AGENT_PROVIDER").unwrap_or_default();
    let model_str = model
        .map(crate::model_catalog::normalize_model)
        .or_else(|| std::env::var("THAT_AGENT_MODEL").ok())
        .unwrap_or_default();

    let cpu_limit = std::env::var("THAT_AGENT_CHILD_CPU_LIMIT").unwrap_or_else(|_| "1".into());
    let mem_limit = std::env::var("THAT_AGENT_CHILD_MEMORY_LIMIT").unwrap_or_else(|_| "2Gi".into());

    let role_str = role.unwrap_or("");

    if workspace {
        // Verify the workspace repo exists on the git server (workspace_share must have been called).
        let check = reqwest::Client::new()
            .get(format!("{git_svc}/api/repos/workspace/activity"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        match check {
            Ok(resp) if resp.status().is_success() => {}
            _ => {
                anyhow::bail!(
                    "workspace=true but no workspace repo found on the git server. \
                     Call workspace_share(path) first to push your repo."
                );
            }
        }
    }

    let labels = k8s_labels(name, parent, "ephemeral", role_str);
    let owner_refs = k8s_owner_refs(&parent_deploy, &deploy_uid);

    let mut config_data = serde_json::json!({
        "THAT_AGENT_NAME": name,
        "THAT_AGENT_PARENT": parent,
        "THAT_AGENT_ROLE": role_str,
        "THAT_SANDBOX_MODE": "kubernetes",
        "THAT_TRUSTED_LOCAL_SANDBOX": "1",
        "THAT_AGENT_PROVIDER": provider,
        "THAT_AGENT_MODEL": model_str,
        "THAT_PARENT_GATEWAY_URL": parent_gw,
        "THAT_PARENT_GATEWAY_TOKEN": gw_token,
        "HTTP_PROXY": proxy_svc,
        "HTTPS_PROXY": proxy_svc,
        "NO_PROXY": "*.svc.cluster.local,10.0.0.0/8,api.anthropic.com,api.openai.com,openrouter.ai",
        "THAT_SANDBOX_K8S_NAMESPACE": ns,
    });

    // Forward auth/credential env vars so children share the parent's rate limits.
    // API keys are already in that-agent-secrets (mounted via envFrom).
    // CLAUDE_CODE_AUTH is an OAuth token that may only exist on the parent Deployment.
    for key in ["CLAUDE_CODE_AUTH"] {
        if let Ok(val) = std::env::var(key) {
            config_data[key] = serde_json::json!(val);
        }
    }
    if workspace {
        config_data["GIT_REPO_URL"] = serde_json::json!(format!("{git_svc}/workspace.git"));
        config_data["GIT_BRANCH"] = serde_json::json!(format!("task/{safe_name}"));
    }

    // Build K8s resources as JSON (no YAML indentation issues)
    let resources = serde_json::json!({
        "apiVersion": "v1",
        "kind": "List",
        "items": [
            {
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                }
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "RoleBinding",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "roleRef": {
                    "apiGroup": "rbac.authorization.k8s.io",
                    "kind": "Role",
                    "name": "that-agent-child-readonly"
                },
                "subjects": [{
                    "kind": "ServiceAccount",
                    "name": sa_name,
                    "namespace": ns,
                }]
            },
            {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": format!("{sa_name}-config"),
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "data": config_data,
            },
            {
                "apiVersion": "v1",
                "kind": "ConfigMap",
                "metadata": {
                    "name": format!("{sa_name}-task"),
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "data": {
                    "task.txt": task,
                },
            },
            {
                "apiVersion": "batch/v1",
                "kind": "Job",
                "metadata": {
                    "name": sa_name,
                    "namespace": ns,
                    "labels": labels,
                    "ownerReferences": owner_refs,
                },
                "spec": {
                    "backoffLimit": 0,
                    "ttlSecondsAfterFinished": 300,
                    "template": {
                        "metadata": { "labels": labels },
                        "spec": {
                            "serviceAccountName": sa_name,
                            "automountServiceAccountToken": true,
                            "restartPolicy": "Never",
                            "containers": [{
                                "name": "agent",
                                "image": image,
                                "command": ["/bin/bash", "-c",
                                    "exec that --agent \"$THAT_AGENT_NAME\" run query --parent \"$THAT_AGENT_PARENT\" --no-sandbox --task-file /etc/agent-task/task.txt"],
                                "envFrom": [
                                    { "secretRef": { "name": "that-agent-secrets", "optional": true } },
                                    { "configMapRef": { "name": format!("{sa_name}-config") } },
                                ],
                                "resources": {
                                    "requests": { "cpu": "200m", "memory": "256Mi" },
                                    "limits": { "cpu": cpu_limit, "memory": mem_limit },
                                },
                                "volumeMounts": [
                                    { "name": "workspace", "mountPath": "/workspace" },
                                    { "name": "task", "mountPath": "/etc/agent-task", "readOnly": true },
                                ],
                            }],
                            "volumes": [
                                { "name": "workspace", "emptyDir": {} },
                                { "name": "task", "configMap": { "name": format!("{sa_name}-task") } },
                            ],
                        }
                    }
                }
            }
        ]
    });

    kubectl_apply_json(&resources).await?;

    // Watch Job until completion or timeout.
    // Every 30s, tail the child's logs so the parent sees progress.
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let job_name = sa_name.clone();
    let mut last_log_check = std::time::Instant::now() - Duration::from_secs(30);
    let mut last_log_line = String::new();

    loop {
        if start.elapsed() > timeout {
            // Grab final logs before cleanup for the error message
            let final_logs = tail_job_logs(&job_name, &ns, 20).await;
            let _ = kubectl_delete_by_label(name, &ns).await;
            anyhow::bail!("agent_run timed out after {timeout_secs}s. Last output:\n{final_logs}");
        }

        let output = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "job",
                &job_name,
                "-n",
                &ns,
                "-o",
                "jsonpath={.status.conditions[0].type} {.status.conditions[0].message}",
            ])
            .output()
            .await?;

        let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if status_str.starts_with("Complete") {
            break;
        }
        if status_str.starts_with("Failed") {
            let logs = tail_job_logs(&job_name, &ns, 30).await;
            anyhow::bail!("agent job failed: {status_str}\nLast output:\n{logs}");
        }

        // Check active count as fallback
        let active_out = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "job",
                &job_name,
                "-n",
                &ns,
                "-o",
                "jsonpath={.status.succeeded} {.status.failed}",
            ])
            .output()
            .await?;
        let active_str = String::from_utf8_lossy(&active_out.stdout)
            .trim()
            .to_string();
        let parts: Vec<&str> = active_str.split_whitespace().collect();
        if parts.first().map(|s| *s == "1").unwrap_or(false) {
            break; // succeeded
        }
        if parts
            .get(1)
            .map(|s| *s != "0" && !s.is_empty())
            .unwrap_or(false)
        {
            let logs = tail_job_logs(&job_name, &ns, 30).await;
            anyhow::bail!("agent job failed\nLast output:\n{logs}");
        }

        // Periodic progress: tail last log line every 30s, extract turn info
        if last_log_check.elapsed() >= Duration::from_secs(30) {
            last_log_check = std::time::Instant::now();
            let latest = tail_job_logs(&job_name, &ns, 3).await;
            if !latest.is_empty() && latest != last_log_line {
                last_log_line = latest.clone();
                let elapsed = start.elapsed().as_secs();
                // Extract turn info like "turn 15/75" from log lines
                let turn_info = latest
                    .lines()
                    .rev()
                    .find_map(|l| {
                        l.find("turn=").and_then(|i| {
                            let rest = &l[i..];
                            let turn = rest.strip_prefix("turn=")?.split_whitespace().next()?;
                            let max = rest.find("max_turns=").and_then(|j| {
                                rest[j..]
                                    .strip_prefix("max_turns=")?
                                    .split_whitespace()
                                    .next()
                            })?;
                            Some(format!("turn {turn}/{max}"))
                        })
                    })
                    .unwrap_or_else(|| "working...".to_string());
                let msg = format!("[{name}] {turn_info} ({elapsed}s)");
                tracing::info!(agent = %name, "{msg}");
                // Post to parent's own gateway so it shows on the channel
                let gw = crate::orchestration::support::resolve_gateway_url();
                let _ = reqwest::Client::new()
                    .post(format!("{gw}/v1/notify"))
                    .json(&serde_json::json!({
                        "message": msg,
                        "agent": name,
                    }))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await;
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Collect logs
    let output_text = tail_job_logs(&job_name, &ns, 200).await;

    let elapsed = start.elapsed().as_secs();

    // Note: workspace branch collection is handled by workspace_collect(path, worker),
    // not auto-fetched here. The orchestrator calls it explicitly after agent_run returns.

    Ok(serde_json::json!({
        "name": name,
        "status": "succeeded",
        "output": output_text,
        "elapsed_secs": elapsed,
    }))
}

// ── K8s Agent List ───────────────────────────────────────────────────────────

/// List all managed agents in K8s namespace (Deployments + Jobs).
pub async fn list_agents_k8s() -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let output = tokio::process::Command::new("kubectl")
        .args([
            "get",
            "deployments,jobs",
            "-n",
            &ns,
            "-l",
            "that-agent/managed=true",
            "-o",
            "json",
        ])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "kubectl get failed");

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let items = parsed["items"].as_array().cloned().unwrap_or_default();

    let agents: Vec<serde_json::Value> = items
        .iter()
        .filter_map(|item| {
            let labels = item["metadata"]["labels"].as_object()?;
            let name = labels.get("that-agent/name")?.as_str()?;
            let parent = labels
                .get("that-agent/parent")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let role = labels
                .get("that-agent/role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let agent_type = labels
                .get("that-agent/type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let kind = item["kind"].as_str().unwrap_or("");
            let (alive, gateway_url, status) = if kind == "Deployment" {
                let ready = item["status"]["readyReplicas"].as_u64().unwrap_or(0) >= 1;
                let safe = sanitize_name(name);
                let gw = format!("http://that-agent-{safe}.{ns}.svc.cluster.local:8080");
                (ready, Some(gw), if ready { "running" } else { "pending" })
            } else {
                // Job
                let succeeded = item["status"]["succeeded"].as_u64().unwrap_or(0) >= 1;
                let failed = item["status"]["failed"].as_u64().unwrap_or(0) >= 1;
                let status = if succeeded {
                    "succeeded"
                } else if failed {
                    "failed"
                } else {
                    "active"
                };
                (false, None, status)
            };

            Some(serde_json::json!({
                "name": name,
                "parent": parent,
                "role": role,
                "type": agent_type,
                "kind": kind,
                "alive": alive,
                "status": status,
                "gateway_url": gateway_url,
            }))
        })
        .collect();

    Ok(serde_json::json!({ "agents": agents }))
}

// ── K8s Agent Query ──────────────────────────────────────────────────────────

/// Query a persistent agent by resolving its K8s Service DNS.
/// If the target is the parent agent, use THAT_PARENT_GATEWAY_URL directly.
/// Otherwise construct a cross-namespace DNS name using the parent's namespace
/// (siblings are deployed in the same namespace as the parent).
pub async fn query_agent_k8s(
    name: &str,
    message: &str,
    timeout_secs: u64,
) -> Result<serde_json::Value> {
    let safe_name = sanitize_name(name);
    let parent_name = std::env::var("THAT_AGENT_PARENT").unwrap_or_default();

    let gateway_url = if !parent_name.is_empty() && sanitize_name(&parent_name) == safe_name {
        // Querying our parent — use the known gateway URL
        std::env::var("THAT_PARENT_GATEWAY_URL").unwrap_or_else(|_| {
            let ns = k8s_namespace();
            format!("http://that-agent-{safe_name}.{ns}.svc.cluster.local:8080")
        })
    } else {
        // Querying a sibling or other agent — use the parent's namespace (where agents live)
        let ns = std::env::var("THAT_SANDBOX_K8S_NAMESPACE").unwrap_or_else(|_| k8s_namespace());
        format!("http://that-agent-{safe_name}.{ns}.svc.cluster.local:8080")
    };

    // Retry with backoff for DNS propagation
    let mut last_err = None;
    for delay in [2, 4, 8] {
        match query_agent(&gateway_url, message, timeout_secs).await {
            Ok(resp) => {
                return Ok(serde_json::json!({
                    "agent": name,
                    "response": resp,
                }))
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("query failed")))
}

// ── K8s Unregister ───────────────────────────────────────────────────────────

/// Remove a child agent and all its K8s resources via label-scoped delete.
pub async fn unregister_agent_k8s(name: &str) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    kubectl_delete_by_label(name, &ns).await?;
    Ok(serde_json::json!({ "name": name, "status": "unregistered" }))
}

/// Stop a child agent — delete its Job/Deployment and all associated resources.
/// Equivalent to unregister but returns "stopped" status for semantic clarity.
pub async fn agent_stop_k8s(name: &str) -> Result<serde_json::Value> {
    unregister_agent_k8s(name).await.map(|mut v| {
        v["status"] = serde_json::json!("stopped");
        v
    })
}

/// Get detailed status of a child agent — Job/Deployment state, pod phase, start time.
pub async fn agent_status_k8s(name: &str) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");

    // Try Job first (ephemeral), then Deployment (persistent)
    let job_out = tokio::process::Command::new("kubectl")
        .args([
            "get", "job", &sa_name, "-n", &ns, "-o",
            "jsonpath={.status.conditions[0].type},{.status.conditions[0].status},{.status.startTime},{.status.succeeded},{.status.failed},{.status.active}",
        ])
        .output()
        .await?;

    if job_out.status.success() {
        let raw = String::from_utf8_lossy(&job_out.stdout);
        let parts: Vec<&str> = raw.split(',').collect();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Job",
            "condition": parts.first().unwrap_or(&""),
            "condition_status": parts.get(1).unwrap_or(&""),
            "start_time": parts.get(2).unwrap_or(&""),
            "succeeded": parts.get(3).unwrap_or(&"0"),
            "failed": parts.get(4).unwrap_or(&"0"),
            "active": parts.get(5).unwrap_or(&"0"),
        }));
    }

    // Try Deployment
    let deploy_out = tokio::process::Command::new("kubectl")
        .args([
            "get", "deployment", &sa_name, "-n", &ns, "-o",
            "jsonpath={.status.readyReplicas},{.status.replicas},{.status.updatedReplicas},{.metadata.creationTimestamp}",
        ])
        .output()
        .await?;

    if deploy_out.status.success() {
        let raw = String::from_utf8_lossy(&deploy_out.stdout);
        let parts: Vec<&str> = raw.split(',').collect();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Deployment",
            "ready": parts.first().unwrap_or(&"0"),
            "replicas": parts.get(1).unwrap_or(&"0"),
            "updated": parts.get(2).unwrap_or(&"0"),
            "created": parts.get(3).unwrap_or(&""),
        }));
    }

    anyhow::bail!("agent '{name}' not found as Job or Deployment in {ns}")
}

/// Get recent logs from a child agent's pod.
pub async fn agent_logs_k8s(name: &str, tail: u32) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");

    let output = tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("job/{sa_name}"),
            "-n",
            &ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await?;

    if output.status.success() {
        let logs = String::from_utf8_lossy(&output.stdout).to_string();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Job",
            "lines": tail,
            "logs": logs,
        }));
    }

    // Fallback: try deployment pods
    let output = tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("deployment/{sa_name}"),
            "-n",
            &ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await?;

    if output.status.success() {
        let logs = String::from_utf8_lossy(&output.stdout).to_string();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Deployment",
            "lines": tail,
            "logs": logs,
        }));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("cannot get logs for agent '{name}': {stderr}")
}

/// Delete all ephemeral child Jobs for the current agent.
/// Used by /stop to clean up running workers when the parent run is cancelled.
pub async fn cleanup_ephemeral_children() -> Result<()> {
    let ns = k8s_namespace();
    let parent_name = std::env::var("THAT_AGENT_NAME").unwrap_or_else(|_| "default".into());
    let safe_parent = sanitize_name(&parent_name);
    tracing::info!("cleaning up ephemeral children of {safe_parent} in {ns}");
    let output = tokio::process::Command::new("kubectl")
        .args([
            "delete",
            "job,configmap,serviceaccount,rolebinding",
            "-l",
            &format!("that-agent/parent={safe_parent},that-agent/type=ephemeral"),
            "-n",
            &ns,
            "--ignore-not-found",
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("cleanup_ephemeral_children: {stderr}");
    }
    Ok(())
}

// ── Workspace sharing ────────────────────────────────────────────────────────

/// Push a local git repo to the in-cluster git server for child access.
pub async fn workspace_share(path: &str, repo_name: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);

    // Validate it's a git repo
    let check = tokio::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--git-dir"])
        .output()
        .await?;
    anyhow::ensure!(
        check.status.success(),
        "path is not a git repository: {path}"
    );

    let name = repo_name.unwrap_or_else(|| {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
    });

    let repo_url = format!("{git_svc}/{name}.git");

    // Push current state to git-server (auto-inits bare repo on first access)
    let push = tokio::process::Command::new("git")
        .args(["-C", path, "push", &repo_url, "HEAD:main", "--force"])
        .output()
        .await?;
    let push_stderr = String::from_utf8_lossy(&push.stderr);
    anyhow::ensure!(
        push.status.success(),
        "git push to git-server failed: {push_stderr}"
    );

    // Register webhook so the git server notifies us on worker pushes
    let parent_gw = crate::orchestration::support::resolve_gateway_url();
    let notify_url = format!("{parent_gw}/v1/notify");
    let _ = reqwest::Client::new()
        .post(format!("{git_svc}/api/repos/{name}/webhook"))
        .json(&serde_json::json!({ "url": notify_url }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    Ok(serde_json::json!({
        "name": name,
        "clone_url": repo_url,
        "webhook": notify_url,
    }))
}

/// Merge or review a worker's code changes back into local workspace.
pub async fn workspace_collect(
    path: &str,
    worker: &str,
    strategy: &str,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let safe_worker = sanitize_name(worker);
    let branch = format!("task/{safe_worker}");
    let repo_url = format!("{git_svc}/workspace.git");

    // Fetch the worker's branch
    let fetch = tokio::process::Command::new("git")
        .args(["-C", path, "fetch", &repo_url, &branch])
        .output()
        .await?;
    anyhow::ensure!(fetch.status.success(), "git fetch failed");

    if strategy == "review" {
        let diff = tokio::process::Command::new("git")
            .args(["-C", path, "diff", "HEAD...FETCH_HEAD"])
            .output()
            .await?;
        let diff_text = String::from_utf8_lossy(&diff.stdout).to_string();
        return Ok(serde_json::json!({
            "strategy": "review",
            "diff": diff_text,
        }));
    }

    // Merge
    let merge = tokio::process::Command::new("git")
        .args([
            "-C",
            path,
            "merge",
            "FETCH_HEAD",
            "--no-ff",
            "-m",
            &format!("Merge worker {worker} results"),
        ])
        .output()
        .await?;

    if merge.status.success() {
        // Count merged commits
        let log = tokio::process::Command::new("git")
            .args(["-C", path, "log", "--oneline", "HEAD...FETCH_HEAD"])
            .output()
            .await?;
        let commit_count = String::from_utf8_lossy(&log.stdout).lines().count();

        // Clean up task branch
        let _ = tokio::process::Command::new("git")
            .args(["-C", path, "push", &repo_url, "--delete", &branch])
            .output()
            .await;

        Ok(serde_json::json!({
            "strategy": "merge",
            "merged": true,
            "commits": commit_count,
            "conflicts": [],
        }))
    } else {
        let stderr = String::from_utf8_lossy(&merge.stderr).to_string();
        // Abort the failed merge so the working tree is clean
        let _ = tokio::process::Command::new("git")
            .args(["-C", path, "merge", "--abort"])
            .status()
            .await;
        // Try to fetch conflict details from the git server REST API
        let conflicts = async {
            reqwest::Client::new()
                .get(format!("{git_svc}/api/repos/workspace/conflicts/{branch}"))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .ok()?
                .json::<serde_json::Value>()
                .await
                .ok()
        }
        .await;
        let conflicting_files = conflicts
            .as_ref()
            .and_then(|c| c.get("conflicting_files"))
            .cloned()
            .unwrap_or(serde_json::json!([]));
        Ok(serde_json::json!({
            "strategy": "merge",
            "merged": false,
            "error": stderr,
            "conflicting_files": conflicting_files,
            "hint": "Use agent_query to ask the worker to rebase against main and resolve conflicts in the listed files",
        }))
    }
}

// ── Git Server REST Wrappers ─────────────────────────────────────────────────

/// Query the git server for branch activity on a repo (branches, ahead/behind, last commit).
pub async fn workspace_activity(repo: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let url = format!("{git_svc}/api/repos/{repo_name}/activity");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

/// Get a unified diff of a worker's branch vs main, without cloning.
pub async fn workspace_branch_diff(branch: &str, repo: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let url = format!("{git_svc}/api/repos/{repo_name}/diff/{branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(serde_json::json!({ "branch": branch, "diff": body }))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

/// Analyze merge conflicts between a worker's branch and main.
pub async fn workspace_conflicts(branch: &str, repo: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let url = format!("{git_svc}/api/repos/{repo_name}/conflicts/{branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn build_config_toml(
    role: Option<&str>,
    parent: Option<&str>,
    model: Option<&str>,
    gateway_port: Option<u16>,
) -> String {
    let mut out = String::new();
    if let Some(m) = model {
        out.push_str(&format!("model = \"{m}\"\n"));
    }
    if let Some(r) = role {
        out.push_str(&format!("role = \"{r}\"\n"));
    }
    if let Some(p) = parent {
        out.push_str(&format!("parent = \"{p}\"\n"));
    }
    if let Some(port) = gateway_port {
        out.push_str(&format!(
            "\n[[channels.adapters]]\ntype = \"http\"\nbind_addr = \"127.0.0.1:{port}\"\n"
        ));
    }
    out
}

/// Resolve the git-server Service URL (independent pod).
fn git_server_url(ns: &str) -> String {
    std::env::var("THAT_GIT_SERVER_URL")
        .unwrap_or_else(|_| format!("http://that-agent-git-server.{ns}.svc.cluster.local:9418"))
}

/// Resolve the cache-proxy Service URL (independent pod).
fn cache_proxy_url(ns: &str) -> String {
    std::env::var("THAT_CACHE_PROXY_URL")
        .unwrap_or_else(|_| format!("http://that-agent-cache-proxy.{ns}.svc.cluster.local:3128"))
}

/// Derive the cluster directory from the memory DB path.
///
/// `memory_db_path` has the form `~/.that-agent/agents/<name>/memory.db`.
/// Walking up 3 levels gives `~/.that-agent/`; we append `cluster/`.
pub fn cluster_dir_from_db(memory_db_path: &Path) -> Option<PathBuf> {
    memory_db_path.ancestors().nth(3).map(|p| p.join("cluster"))
}

/// Build K8s labels as a JSON object.
fn k8s_labels(name: &str, parent: &str, agent_type: &str, role: &str) -> serde_json::Value {
    serde_json::json!({
        "that-agent/managed": "true",
        "that-agent/name": sanitize_label_value(name),
        "that-agent/parent": sanitize_label_value(parent),
        "that-agent/type": agent_type,
        "that-agent/role": sanitize_label_value(role),
    })
}

/// Sanitize a string for use as a K8s label value (max 63 chars, [a-zA-Z0-9._-]).
fn sanitize_label_value(s: &str) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .take(63)
        .collect();
    sanitized.trim_matches('-').to_string()
}

/// Build ownerReferences array for K8s resources.
fn k8s_owner_refs(deploy_name: &str, uid: &str) -> serde_json::Value {
    if uid.is_empty() {
        return serde_json::json!([]);
    }
    serde_json::json!([{
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "name": deploy_name,
        "uid": uid,
        "controller": true,
        "blockOwnerDeletion": false,
    }])
}

// Keep YAML helpers for persistent agent (will migrate later)
/// Apply a JSON K8s resource list via `kubectl apply -f -`.
async fn kubectl_apply_json(resource: &serde_json::Value) -> Result<()> {
    let json_str = serde_json::to_string(resource)?;
    let mut child = tokio::process::Command::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(json_str.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl apply failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Tail the last N log lines from a K8s Job pod.
async fn tail_job_logs(job_name: &str, ns: &str, tail: u32) -> String {
    tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("job/{job_name}"),
            "-n",
            ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Delete all resources for a named child agent by label selector.
async fn kubectl_delete_by_label(name: &str, ns: &str) -> Result<()> {
    let output = tokio::process::Command::new("kubectl")
        .args([
            "delete",
            "deployment,service,job,serviceaccount,rolebinding,configmap",
            "-l",
            &format!("that-agent/name={name}"),
            "-n",
            ns,
            "--ignore-not-found",
        ])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "kubectl delete failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
