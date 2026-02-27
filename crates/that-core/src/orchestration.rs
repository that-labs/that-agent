use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Local, Utc};
use crossterm::event::{Event, EventStream};
use futures::FutureExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn, Instrument};

use crate::agent_loop::hook::{HookAction, LoopHook};
use crate::agent_loop::{self, LoopConfig, Message, ToolCall, ToolContext};
use crate::config::{AgentDef, WorkspaceConfig};
use crate::control::cli::{AgentCommands, SessionCommands, SkillCommands};
use crate::default_skills;
use crate::heartbeat;
use crate::hooks::{channel_notify_tool_def, channel_send_file_tool_def, ChannelHook};
use crate::sandbox::SandboxClient;
use crate::session::{
    new_run_id, rebuild_history, rebuild_history_recent, RunStatus, SessionManager,
    TranscriptEntry, TranscriptEvent,
};
use crate::skills;
use crate::tools::all_tool_defs;
use crate::tui;
use crate::workspace;

mod preamble;

pub use preamble::build_preamble;

/// Maximum number of automatic retries on transient network / server errors.
pub const MAX_NETWORK_RETRIES: u32 = 5;

/// Load the tools config, then lift restrictive policies to `Allow` when running
/// in a trusted sandbox environment.
///
/// In Docker sandbox mode the container is the safety boundary. In Kubernetes
/// pod-local sandbox mode (`THAT_TRUSTED_LOCAL_SANDBOX=1`), the pod is treated
/// as the boundary. On non-trusted host runs, restrictive defaults remain.
pub fn load_agent_config(
    container: &Option<String>,
    agent: &AgentDef,
) -> that_tools::config::ThatToolsConfig {
    let mut cfg = that_tools::config::load_config(None).unwrap_or_default();
    // Memory tools execute in the host runtime process (not via docker exec), so
    // memory storage is always agent-scoped on the host path, including sandbox mode.
    cfg.memory.db_path = AgentDef::agent_memory_db_path(&agent.name)
        .display()
        .to_string();
    if let Err(err) = that_tools::tools::memory::ensure_initialized(&cfg.memory) {
        tracing::warn!(
            agent = %agent.name,
            path = %cfg.memory.db_path,
            error = %err,
            "Failed to initialize agent memory database"
        );
    }
    if container.is_some() || trusted_local_sandbox_enabled() {
        use that_tools::config::PolicyLevel;
        cfg.policy.tools.fs_write = PolicyLevel::Allow;
        cfg.policy.tools.fs_delete = PolicyLevel::Allow;
        cfg.policy.tools.shell_exec = PolicyLevel::Allow;
        cfg.policy.tools.code_edit = PolicyLevel::Allow;
        cfg.policy.tools.git_commit = PolicyLevel::Allow;
        cfg.policy.tools.git_push = PolicyLevel::Allow;
    }
    cfg
}
/// Initial backoff delay in ms; doubles each attempt (1 s -> 2 -> 4 -> 8 -> 16 s).
pub const RETRY_BASE_DELAY_MS: u64 = 1_000;
/// Warn when cache hit rate drops below this threshold on sizable prompts.
pub const CACHE_HIT_WARN_THRESHOLD: f64 = 0.70;
/// Fallback text used when the model completes without any final assistant text.
pub const EMPTY_CHANNEL_RESPONSE_FALLBACK: &str =
    "I could not generate a response for that request. Please try again.";
/// Maximum retries when the model returns an empty final channel response.
pub const MAX_EMPTY_CHANNEL_RESPONSE_RETRIES: u32 = 1;

type SenderRunLock = Arc<tokio::sync::Mutex<()>>;
type SenderRunLocks = Arc<tokio::sync::Mutex<std::collections::HashMap<String, SenderRunLock>>>;
#[derive(Clone)]
struct ActiveSenderRun {
    run_id: u64,
    abort: tokio::task::AbortHandle,
}
type ActiveSenderRuns = Arc<tokio::sync::Mutex<std::collections::HashMap<String, ActiveSenderRun>>>;

fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn parse_env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn openai_websocket_enabled() -> bool {
    parse_env_bool("THAT_OPENAI_WEBSOCKET").unwrap_or(true)
}

fn trusted_local_sandbox_enabled() -> bool {
    if let Some(explicit) = parse_env_bool("THAT_TRUSTED_LOCAL_SANDBOX") {
        return explicit;
    }
    matches!(
        std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    )
}

fn should_use_channel_empty_response_fallback(
    text: &str,
    suppress_output: bool,
    tool_events: &[that_channels::ToolLogEvent],
) -> bool {
    if suppress_output || !text.trim().is_empty() {
        return false;
    }

    // If the run already emitted an explicit channel_notify tool call, allow
    // empty final text to avoid sending a duplicate fallback message.
    let used_channel_notify = tool_events.iter().any(|ev| {
        matches!(
            ev,
            that_channels::ToolLogEvent::Call { name, .. } if name == "channel_notify"
        )
    });
    !used_channel_notify
}

fn summarize_tool_result_for_empty_response(
    tool_events: &[that_channels::ToolLogEvent],
) -> Option<(String, bool, String)> {
    tool_events.iter().rev().find_map(|ev| {
        if let that_channels::ToolLogEvent::Result {
            name,
            result,
            is_error,
        } = ev
        {
            let compact = result
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(result.as_str())
                .trim();
            let snippet: String = compact.chars().take(240).collect();
            let suffix = if compact.chars().count() > 240 {
                "..."
            } else {
                ""
            };
            Some((name.clone(), *is_error, format!("{snippet}{suffix}")))
        } else {
            None
        }
    })
}

fn build_empty_channel_response_fallback(tool_events: &[that_channels::ToolLogEvent]) -> String {
    if let Some((name, is_error, detail)) = summarize_tool_result_for_empty_response(tool_events) {
        if is_error {
            format!(
                "I could not generate a final response. Last tool `{name}` failed: {detail}. Please retry or ask me to inspect logs."
            )
        } else {
            format!(
                "I could not generate a final response. Last tool `{name}` returned: {detail}. Please retry and I will continue from there."
            )
        }
    } else {
        EMPTY_CHANNEL_RESPONSE_FALLBACK.to_string()
    }
}

fn should_retry_empty_channel_response(
    text: &str,
    suppress_output: bool,
    tool_events: &[that_channels::ToolLogEvent],
    retries_used: u32,
) -> bool {
    if suppress_output || retries_used >= MAX_EMPTY_CHANNEL_RESPONSE_RETRIES {
        return false;
    }
    if !text.trim().is_empty() {
        return false;
    }
    // Avoid re-running after side-effecting tool calls.
    tool_events.is_empty()
}

fn build_empty_channel_retry_task(task: &str) -> String {
    format!(
        "{task}\n\n<system-reminder>\n\
empty_completion_retry: true\n\
instruction: Your previous completion was empty. Return a concise user-facing answer now. \
If blocked, explicitly state the blocker and one concrete next step.\n\
</system-reminder>"
    )
}

/// Append volatile runtime metadata to the tail of the user message so it
/// doesn't invalidate the shared system-prompt cache prefix.
fn runtime_reminder_lines(sandbox: bool, agent_name: &str) -> Vec<String> {
    fn append_rbac_runtime_lines(lines: &mut Vec<String>, namespace: Option<&str>) {
        let scope = std::env::var("THAT_RBAC_SCOPE")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| namespace.map(|ns| format!("namespace:{ns}")))
            .unwrap_or_else(|| "unknown".to_string());
        lines.push(format!("rbac_scope: {scope}"));

        if let Some(subject) = std::env::var("THAT_RBAC_SUBJECT")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            lines.push(format!("rbac_subject: {subject}"));
        }

        if let Some(ns) = namespace {
            lines.push(format!("rbac_namespace: {ns}"));
        }

        if let Some(can_read_ns) = parse_env_bool("THAT_RBAC_NAMESPACE_READ") {
            lines.push(format!("rbac_namespace_read: {can_read_ns}"));
        }
        if let Some(can_write_ns) = parse_env_bool("THAT_RBAC_NAMESPACE_WRITE") {
            lines.push(format!("rbac_namespace_write: {can_write_ns}"));
        }

        let cluster_read = parse_env_bool("THAT_RBAC_CLUSTER_READ").unwrap_or(false);
        let cluster_write = parse_env_bool("THAT_RBAC_CLUSTER_WRITE").unwrap_or(false);
        lines.push(format!("rbac_cluster_read: {cluster_read}"));
        lines.push(format!("rbac_cluster_write: {cluster_write}"));

        lines.push("rbac_cluster_scope_requires: ClusterRole + ClusterRoleBinding".to_string());
    }

    let mut lines = vec![format!("sandbox_enabled: {sandbox}")];
    if !sandbox {
        if trusted_local_sandbox_enabled() {
            lines.push("trusted_local_sandbox: true".to_string());
            match that_sandbox::backend::SandboxMode::from_env() {
                that_sandbox::backend::SandboxMode::Kubernetes => {
                    let k8s =
                        that_sandbox::kubernetes::KubernetesSandboxClient::from_env(agent_name);
                    lines.push("runtime_backend: kubernetes_local".to_string());
                    lines.push(format!("k8s_namespace: {}", k8s.namespace));
                    lines.push(format!("k8s_registry: {}", k8s.registry));
                    if let Some(push_registry) =
                        parse_env_nonempty("THAT_K8S_REGISTRY_PUSH_ENDPOINT").or_else(|| {
                            parse_env_nonempty("THAT_SANDBOX_K8S_REGISTRY_PUSH_ENDPOINT")
                        })
                    {
                        lines.push(format!("k8s_registry_push: {push_registry}"));
                    }
                    if let Some(buildkit_available) = parse_env_bool("THAT_BUILDKIT_AVAILABLE") {
                        lines.push(format!("buildkit_available: {}", buildkit_available));
                        if buildkit_available {
                            if let Some(source) = parse_env_nonempty("THAT_BUILDKIT_SOURCE") {
                                lines.push(format!("buildkit_source: {source}"));
                            }
                        }
                    }
                    if let Some(docker_daemon_available) =
                        parse_env_bool("THAT_DOCKER_DAEMON_AVAILABLE")
                    {
                        lines.push(format!(
                            "docker_daemon_available: {}",
                            docker_daemon_available
                        ));
                        if docker_daemon_available {
                            if let Some(source) = parse_env_nonempty("THAT_DOCKER_DAEMON_SOURCE") {
                                lines.push(format!("docker_daemon_source: {source}"));
                            }
                        }
                    }
                    if let Some(preferred) =
                        parse_env_nonempty("THAT_IMAGE_BUILD_BACKEND_PREFERRED")
                    {
                        lines.push(format!("image_build_backend_preferred: {preferred}"));
                    }
                    if let Some(selected) = parse_env_nonempty("THAT_IMAGE_BUILD_BACKEND") {
                        lines.push(format!("image_build_backend: {selected}"));
                    }
                    append_rbac_runtime_lines(&mut lines, Some(&k8s.namespace));
                }
                that_sandbox::backend::SandboxMode::Docker => {
                    lines.push("runtime_backend: local_trusted".to_string());
                }
            }
        }
        return lines;
    }

    match that_sandbox::backend::SandboxMode::from_env() {
        that_sandbox::backend::SandboxMode::Docker => {
            let socket = that_sandbox::docker::docker_socket_status();
            lines.push("runtime_backend: docker".to_string());
            lines.push(format!("docker_socket_enabled: {}", socket.enabled));
            lines.push(format!("docker_socket_path: {}", socket.path.display()));
        }
        that_sandbox::backend::SandboxMode::Kubernetes => {
            let k8s = that_sandbox::kubernetes::KubernetesSandboxClient::from_env(agent_name);
            lines.push("runtime_backend: kubernetes".to_string());
            lines.push(format!("k8s_namespace: {}", k8s.namespace));
            lines.push(format!("k8s_registry: {}", k8s.registry));
            if let Some(push_registry) = parse_env_nonempty("THAT_K8S_REGISTRY_PUSH_ENDPOINT")
                .or_else(|| parse_env_nonempty("THAT_SANDBOX_K8S_REGISTRY_PUSH_ENDPOINT"))
            {
                lines.push(format!("k8s_registry_push: {push_registry}"));
            }
            if let Some(buildkit_available) = parse_env_bool("THAT_BUILDKIT_AVAILABLE") {
                lines.push(format!("buildkit_available: {}", buildkit_available));
                if buildkit_available {
                    if let Some(source) = parse_env_nonempty("THAT_BUILDKIT_SOURCE") {
                        lines.push(format!("buildkit_source: {source}"));
                    }
                }
            }
            if let Some(docker_daemon_available) = parse_env_bool("THAT_DOCKER_DAEMON_AVAILABLE") {
                lines.push(format!(
                    "docker_daemon_available: {}",
                    docker_daemon_available
                ));
                if docker_daemon_available {
                    if let Some(source) = parse_env_nonempty("THAT_DOCKER_DAEMON_SOURCE") {
                        lines.push(format!("docker_daemon_source: {source}"));
                    }
                }
            }
            if let Some(preferred) = parse_env_nonempty("THAT_IMAGE_BUILD_BACKEND_PREFERRED") {
                lines.push(format!("image_build_backend_preferred: {preferred}"));
            }
            if let Some(selected) = parse_env_nonempty("THAT_IMAGE_BUILD_BACKEND") {
                lines.push(format!("image_build_backend: {selected}"));
            }
            append_rbac_runtime_lines(&mut lines, Some(&k8s.namespace));
        }
    }

    lines
}

fn append_system_reminder(task: &str, session_id: &str, sandbox: bool, agent_name: &str) -> String {
    if task.contains("<system-reminder>") {
        return task.to_string();
    }
    let today_utc = Utc::now().format("%Y-%m-%d").to_string();
    let mut reminder = vec![
        format!("current_date_utc: {today_utc}"),
        format!("session_id: {session_id}"),
        "completion_verification_required: After creating/modifying executable artifacts (scripts, services, deploy manifests), run at least one behavior check before claiming done.".to_string(),
        "shell_script_verification_required: For shell scripts, validate with `sh -n <file>` and execute at least one path unless blocked by environment.".to_string(),
        "skill_usage_evidence_required: If claiming a skill was used this run, ensure `read_skill` evidence exists in this run; otherwise state it came from prior memory.".to_string(),
        "skill_naming_determinism: When creating skills without a user-provided name, use deterministic kebab-case from capability phrase; for JSON formatting skills use `json-formatter`.".to_string(),
    ];
    reminder.extend(runtime_reminder_lines(sandbox, agent_name));
    format!(
        "{task}\n\n<system-reminder>\n{}\n</system-reminder>",
        reminder.join("\n")
    )
}

/// Compute cache hit rate as a percentage.
///
/// Uses `input_tokens - cache_write_tokens` as the denominator because
/// newly-written cache entries cannot be hits in the same request.
/// Turn 1 of a session writes the system prompt + tools to cache (cache_write > 0)
/// but reads nothing — correctly producing 0% rather than inflating the rate.
/// Turn 2+ should read from cache (cache_write ≈ 0), giving an accurate per-eligible hit rate.
fn cache_hit_rate_percent(
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
) -> f64 {
    let eligible = input_tokens.saturating_sub(cache_write_tokens);
    if eligible == 0 {
        0.0
    } else {
        (cached_input_tokens as f64 / eligible as f64) * 100.0
    }
}

fn prompt_cache_alerts_enabled() -> bool {
    std::env::var("THAT_PROMPT_CACHE_ALERTS")
        .ok()
        .map(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn log_prompt_cache_usage(
    provider: &str,
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_write_tokens: u64,
) {
    let hit_rate = cache_hit_rate_percent(input_tokens, cached_input_tokens, cache_write_tokens);
    tracing::debug!(
        provider = provider,
        model = model,
        input_tokens = input_tokens,
        cached_input_tokens = cached_input_tokens,
        cache_write_tokens = cache_write_tokens,
        cache_hit_rate_pct = format_args!("{hit_rate:.2}"),
        "Prompt cache usage"
    );
    if prompt_cache_alerts_enabled()
        && input_tokens >= 2_000
        && (hit_rate / 100.0) < CACHE_HIT_WARN_THRESHOLD
    {
        tracing::warn!(
            provider = provider,
            model = model,
            input_tokens = input_tokens,
            cached_input_tokens = cached_input_tokens,
            cache_write_tokens = cache_write_tokens,
            cache_hit_rate_pct = format_args!("{hit_rate:.2}"),
            threshold_pct = format_args!("{:.2}", CACHE_HIT_WARN_THRESHOLD * 100.0),
            "Low prompt cache hit rate"
        );
    }
}

/// Return true when an error is likely transient and worth retrying.
pub fn is_retryable_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}").to_lowercase();
    msg.contains("connection reset")
        || msg.contains("connection refused")
        || msg.contains("connection timed out")
        || msg.contains("broken pipe")
        || msg.contains("timed out")
        || msg.contains("timeout")
        || msg.contains("io error")
        || msg.contains("hyper")
        || msg.contains("reqwest")
        || msg.contains(" 429")
        || msg.contains(" 500")
        || msg.contains(" 502")
        || msg.contains(" 503")
        || msg.contains(" 504")
        || msg.contains("rate limit")
        || msg.contains("overloaded")
        || msg.contains("stream ended unexpectedly")
        || msg.contains("incomplete")
}

/// Remove a sender lock entry when no other tasks still reference it.
///
/// This keeps the lock map bounded to active senders instead of growing forever.
async fn evict_sender_lock_if_idle(
    sender_locks: &SenderRunLocks,
    sender_key: &str,
    sender_lock: &SenderRunLock,
) {
    if Arc::strong_count(sender_lock) > 1 {
        return;
    }
    let mut locks = sender_locks.lock().await;
    if Arc::strong_count(sender_lock) > 1 {
        return;
    }
    let should_remove = locks
        .get(sender_key)
        .map(|current| Arc::ptr_eq(current, sender_lock))
        .unwrap_or(false);
    if should_remove {
        locks.remove(sender_key);
    }
}

/// Abort and clear the currently active run for a sender key, if any.
async fn stop_active_sender_run(active_runs: &ActiveSenderRuns, sender_key: &str) -> bool {
    let active = active_runs.lock().await.remove(sender_key);
    if let Some(run) = active {
        run.abort.abort();
        true
    } else {
        false
    }
}

/// Resolve the provider API key from environment variables.
fn api_key_for_provider(provider: &str) -> Result<String> {
    match provider {
        "anthropic" => std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set"),
        "openai" => std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set"),
        "openrouter" => std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set"),
        other => Err(anyhow::anyhow!(
            "Unsupported provider: {other}. Use 'anthropic', 'openai', or 'openrouter'."
        )),
    }
}

/// Hook for interactive streaming mode — prints tokens live, handles `human_ask` via stdin.
pub struct AgentHook {
    pub debug: bool,
}

#[async_trait::async_trait]
impl LoopHook for AgentHook {
    async fn on_text_delta(&self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
    }

    async fn on_reasoning_delta(&self, delta: &str) {
        if !delta.is_empty() {
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = io::stderr().flush();
        }
    }

    async fn on_tool_call(&self, name: &str, _call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let message = serde_json::from_str::<serde_json::Value>(args_json)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(String::from))
                .unwrap_or_else(|| "Agent is asking for input:".into());
            eprint!("\n\x1b[1;33m[human_ask]\x1b[0m {message}\n> ");
            let _ = io::stdout().flush();
            let mut response = String::new();
            let _ = io::stdin().lock().read_line(&mut response);
            let response = response.trim().to_string();
            let approved = {
                let lower = response.to_lowercase();
                lower != "no" && lower != "n" && lower != "deny"
            };
            let result = serde_json::json!({
                "response": response,
                "approved": approved,
                "method": "hook",
                "elapsed_ms": 0
            });
            return HookAction::Skip {
                result_json: result.to_string(),
            };
        }
        if self.debug {
            eprintln!("\x1b[36m[tool call] {name} {args_json}\x1b[0m");
        }
        HookAction::Continue
    }

    async fn on_tool_result(&self, name: &str, _call_id: &str, result_json: &str) {
        if self.debug {
            let truncated: String = result_json.chars().take(500).collect();
            let suffix = if result_json.chars().count() > 500 {
                "..."
            } else {
                ""
            };
            eprintln!("\x1b[33m[tool result] {name}: {truncated}{suffix}\x1b[0m");
        }
    }
}

/// Hook for eval mode — auto-denies `human_ask` and collects tool events for judge transcripts.
pub struct EvalHook {
    tool_events: Arc<std::sync::Mutex<Vec<String>>>,
}

impl EvalHook {
    pub fn new() -> Self {
        Self {
            tool_events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Drain and return collected tool event strings.
    pub fn take_events(&self) -> Vec<String> {
        self.tool_events.lock().unwrap().drain(..).collect()
    }
}

#[async_trait::async_trait]
impl LoopHook for EvalHook {
    async fn on_text_delta(&self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
    }

    async fn on_reasoning_delta(&self, _delta: &str) {}

    async fn on_tool_call(&self, name: &str, _call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let result = serde_json::json!({
                "response": "eval-mode: no human available",
                "approved": false,
                "method": "eval_hook"
            });
            return HookAction::Skip {
                result_json: result.to_string(),
            };
        }
        let args: String = args_json.chars().take(300).collect();
        let suffix = if args_json.chars().count() > 300 {
            "…"
        } else {
            ""
        };
        self.tool_events
            .lock()
            .unwrap()
            .push(format!("CALL {name} {args}{suffix}"));
        HookAction::Continue
    }

    async fn on_tool_result(&self, name: &str, _call_id: &str, result_json: &str) {
        let truncated: String = result_json.chars().take(400).collect();
        let suffix = if result_json.chars().count() > 400 {
            "…"
        } else {
            ""
        };
        self.tool_events
            .lock()
            .unwrap()
            .push(format!("RESULT {name} {truncated}{suffix}"));
    }
}

/// Resolve the effective workspace directory for an agent.
///
/// Priority:
/// 1. `inherit_workspace` — use the parent agent's workspace directory
/// 2. `shared_workspace` — use the global workspace (current behavior)
/// 3. Default — isolated per-agent workspace
pub fn resolve_agent_workspace(ws: &WorkspaceConfig, agent: &AgentDef) -> Result<PathBuf> {
    if agent.inherit_workspace {
        // Inherit the parent's workspace directory
        let dir = if let Some(parent_name) = &agent.parent {
            AgentDef::agent_workspace_dir(parent_name)
        } else {
            // No parent specified — fall back to own workspace
            AgentDef::agent_workspace_dir(&agent.name)
        };
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create inherited workspace at {}", dir.display())
        })?;
        Ok(dir)
    } else if agent.shared_workspace {
        // Use the global workspace (current behavior)
        let ws_path = ws.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
        Ok(ws_path)
    } else {
        // Isolated per-agent workspace
        let dir = AgentDef::agent_workspace_dir(&agent.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create agent workspace at {}", dir.display()))?;
        Ok(dir)
    }
}

/// Ensure the sandbox container is running and return the container name.
/// In local mode, returns None.
pub async fn prepare_container(
    agent: &AgentDef,
    workspace: &Path,
    sandbox: bool,
) -> Result<Option<String>> {
    // Always install skills on the host — ReadSkillTool reads from the host regardless of mode.
    default_skills::install_default_skills(&agent.name);
    install_that_tools_skills_local(&agent.name);

    if sandbox {
        info!(agent = %agent.name, "Preparing Docker sandbox container");
        let sc = SandboxClient::connect(agent, workspace).await?;
        Ok(Some(sc.container_name))
    } else {
        Ok(None)
    }
}

/// Install that-tools skills in the agent skills directory.
///
/// This runs in-process (no shelling out to `that`) so it is deterministic even
/// when PATH contains an older binary. A legacy skill directory is removed
/// during install migration.
pub fn install_that_tools_skills_local(agent_name: &str) {
    let Some(skills_dir) = skills::skills_dir_local(agent_name) else {
        return;
    };

    fn legacy_skill_dir_name() -> String {
        ['o', 'w', 'a', 'n', 'a', 'i'].iter().collect()
    }

    let legacy_dir = skills_dir.join(legacy_skill_dir_name());
    if legacy_dir.exists() {
        if let Err(err) = std::fs::remove_dir_all(&legacy_dir) {
            tracing::warn!(
                agent = %agent_name,
                path = %legacy_dir.display(),
                error = %err,
                "Failed to remove legacy skill directory"
            );
        } else {
            tracing::info!(
                agent = %agent_name,
                path = %legacy_dir.display(),
                "Removed legacy skill directory"
            );
        }
    }

    match that_tools::tools::skills::install(None, Some(&skills_dir), true) {
        Ok(_) => info!(agent = %agent_name, "Installed that-tools skills locally"),
        Err(err) => tracing::warn!(
            agent = %agent_name,
            error = %err,
            "Failed to install that-tools skills locally"
        ),
    }
}

/// Execute a single task in the agent loop.
#[tracing::instrument(name = "task", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
))]
pub async fn run_task(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    task: &str,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.get_or_create_session()?,
    };

    let run_id = new_run_id();

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    if sandbox {
        eprintln!("[SANDBOX] Running inside Docker container — bash tool available");
    }

    info!(session = %session_id, run = %run_id, mode = if sandbox { "sandbox" } else { "local" }, workspace = %agent_workspace.display(), "Starting run");

    // Log run start
    session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::RunStart {
                task: task.to_string(),
            },
        },
    )?;

    // Ensure container is ready (sandbox) or skip (local)
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let found_skills = discover_skills(agent, sandbox);
    info!(count = found_skills.len(), "Discovered skills");

    let ws = load_workspace_files(agent, sandbox);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();

    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
    );
    let task_for_model = append_system_reminder(task, &session_id, sandbox, &agent.name);

    let response =
        execute_agent_run_streaming(agent, container, &preamble, &task_for_model, debug, None)
            .await;

    // Record the result
    match response {
        Ok(text) => {
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::UserMessage {
                        content: task.to_string(),
                    },
                },
            )?;
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::AssistantMessage {
                        content: text.clone(),
                    },
                },
            )?;
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Success,
                        error: None,
                    },
                },
            )?;
            // Text was already streamed live; just add a trailing newline
            println!();
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Error,
                        error: Some(err_str.clone()),
                    },
                },
            )?;
            if sandbox {
                error!(error = %err_str, "[SANDBOX] Run failed");
            } else {
                error!(error = %err_str, "Run failed");
            }
            return Err(e);
        }
    }

    Ok(())
}

/// Interactive chat loop.
#[tracing::instrument(name = "chat_session", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
))]
pub async fn run_chat(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.get_or_create_session()?,
    };

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    let mode = if sandbox { "sandbox" } else { "local" };
    if sandbox {
        println!("[SANDBOX] that-agent interactive session: {session_id}");
        println!("[SANDBOX] Running inside Docker container — bash tool available");
    } else {
        println!("that-agent interactive session: {session_id} (mode: {mode})");
    }
    println!("Type your message and press Enter. Type 'exit' or Ctrl+D to quit.\n");

    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let found_skills = discover_skills(agent, sandbox);
    info!(count = found_skills.len(), "Discovered skills");

    let ws = load_workspace_files(agent, sandbox);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();

    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
    );

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let prompt_str = if sandbox { "[SANDBOX]> " } else { "> " };

    let mut history: Vec<Message> = Vec::new();

    loop {
        print!("{prompt_str}");
        stdout.flush()?;

        let mut input = String::new();
        let bytes = stdin.lock().read_line(&mut input)?;
        if bytes == 0 {
            println!();
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        let run_id = new_run_id();

        session_mgr.append(
            &session_id,
            &TranscriptEntry {
                timestamp: Utc::now(),
                run_id: run_id.clone(),
                event: TranscriptEvent::UserMessage {
                    content: input.to_string(),
                },
            },
        )?;

        let task_for_model = append_system_reminder(input, &session_id, sandbox, &agent.name);
        match execute_agent_run_streaming(
            agent,
            container.clone(),
            &preamble,
            &task_for_model,
            debug,
            Some(history.clone()),
        )
        .await
        {
            Ok(text) => {
                history.push(Message::user(&task_for_model));
                history.push(Message::assistant(&text));

                session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::AssistantMessage {
                            content: text.clone(),
                        },
                    },
                )?;
                // Text was already streamed live; just add trailing newlines
                println!("\n");
            }
            Err(e) => {
                let err_str = format!("{e:#}");
                session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::RunEnd {
                            status: RunStatus::Error,
                            error: Some(err_str.clone()),
                        },
                    },
                )?;
                if sandbox {
                    eprintln!("[SANDBOX] Error: {err_str}\n");
                } else {
                    eprintln!("Error: {err_str}\n");
                }
            }
        }
    }

    Ok(())
}

/// Channel-driven agent loop: listens for inbound messages and responds via the router.
///
/// Each unique `(channel_id, sender_id)` pair gets its own persistent session with
/// conversation history. Sessions survive process restarts because history is rebuilt
/// from the JSONL transcript on the first message from a returning sender.
///
/// Runs until the inbound channel closes (Ctrl+C or all senders dropped).
/// Convert a skill name (e.g. `json-formatter`) to a valid bot command name (`json_formatter`).
/// Telegram requires lowercase alphanumeric + underscore, max 32 chars.
fn skill_to_command(name: &str) -> String {
    let cmd: String = name
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase();
    cmd.chars().take(32).collect()
}

/// Find a skill whose normalized command name matches `cmd`.
fn find_skill_by_command<'a>(
    cmd: &str,
    skills: &'a [skills::SkillMeta],
) -> Option<&'a skills::SkillMeta> {
    skills.iter().find(|s| skill_to_command(&s.name) == cmd)
}

fn read_skill_content(skill: &skills::SkillMeta) -> Option<String> {
    std::fs::read_to_string(&skill.path).ok()
}

fn find_plugin_command<'a>(
    cmd: &str,
    commands: &'a [that_plugins::ResolvedPluginCommand],
) -> Option<&'a that_plugins::ResolvedPluginCommand> {
    commands.iter().find(|c| c.command == cmd)
}

fn render_plugin_command_task(command: &that_plugins::ResolvedPluginCommand, args: &str) -> String {
    let trimmed = args.trim();
    let task = command
        .task_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match task {
        Some(template) if template.contains("{{args}}") => template.replace("{{args}}", trimmed),
        Some(template) if trimmed.is_empty() => template.to_string(),
        Some(template) => format!("{template}\n\nUser args: {trimmed}"),
        None => trimmed.to_string(),
    }
}

fn activation_matches_message(
    activation: &that_plugins::ResolvedPluginActivation,
    text: &str,
    slash_command: Option<&str>,
) -> bool {
    if activation.event != "message_in" {
        return false;
    }
    let mut matched = false;
    if let Some(cmd) = activation.command.as_deref() {
        matched = slash_command == Some(cmd);
        if !matched {
            return false;
        }
    }
    if let Some(contains) = activation.contains.as_deref() {
        let contains_match = text
            .to_ascii_lowercase()
            .contains(&contains.to_ascii_lowercase());
        matched = matched || contains_match;
        if !contains_match {
            return false;
        }
    }
    if let Some(trigger) = activation.trigger.as_deref() {
        let trigger_match = text
            .to_ascii_lowercase()
            .contains(&trigger.to_ascii_lowercase());
        matched = matched || trigger_match;
        if !trigger_match {
            return false;
        }
    }
    matched
}

fn render_activation_task(
    activation: &that_plugins::ResolvedPluginActivation,
    message: &str,
    slash_args: Option<&str>,
) -> String {
    let args = slash_args.unwrap_or("").trim();
    let template = activation
        .task_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(tpl) = template {
        return tpl
            .replace("{{message}}", message.trim())
            .replace("{{args}}", args);
    }
    if let Some(desc) = activation.description.as_deref().map(str::trim) {
        if !desc.is_empty() {
            return format!(
                "Activation '{}' ({}) triggered.\n\n{}\n\nMessage: {}",
                activation.name,
                activation.plugin_id,
                desc,
                message.trim()
            );
        }
    }
    format!(
        "Activation '{}' from plugin '{}' triggered by message: {}",
        activation.name,
        activation.plugin_id,
        message.trim()
    )
}

fn append_plugin_heartbeat_tasks(
    task: &mut String,
    plugin_tasks: &[that_plugins::PluginHeartbeatTask],
) {
    if plugin_tasks.is_empty() {
        return;
    }
    task.push_str(
        "\n\nPlugin heartbeat items (routines/activations):\n\n\
         For each item, complete the work and include what changed.\n\n",
    );
    for item in plugin_tasks {
        task.push_str(&format!(
            "## [{}] {}::{} (priority: {}, schedule: {})\n{}\n\n",
            item.source,
            item.plugin_id,
            item.name,
            item.priority,
            item.schedule,
            item.body.trim()
        ));
    }
}

/// Parse a `/command [args]` out of an inbound message.
/// Returns `(command, args)` if the text starts with `/`; `None` otherwise.
/// Strips bot @mentions (e.g. `/start@mybot args` → `("start", "args")`).
fn parse_slash_command(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let without_slash = &text[1..];
    // Split on first space to separate command from args.
    let (raw_cmd, args) = match without_slash.find(' ') {
        Some(pos) => (&without_slash[..pos], without_slash[pos + 1..].trim()),
        None => (without_slash, ""),
    };
    // Strip @botname suffix from command.
    let cmd = raw_cmd
        .splitn(2, '@')
        .next()
        .unwrap_or(raw_cmd)
        .to_lowercase();
    Some((cmd, args.to_string()))
}

/// Build the /help reply text listing all registered bot commands.
fn build_help_text(commands: &[that_channels::BotCommand]) -> String {
    let mut out = String::from("Available commands:\n");
    for c in commands {
        out.push_str(&format!("/{} — {}\n", c.command, c.description));
    }
    out
}

/// Sanitize a task string into a compact single-line span field value.
///
/// Strips injected `<system-reminder>…</system-reminder>` blocks (everything
/// from the first `<system-reminder>` onwards) so only the user's actual
/// message appears. Then replaces control characters with a space, collapses
/// runs of whitespace, and truncates to `max_chars` visible characters.
/// This prevents k8s from splitting span field values across log lines.
fn task_preview(s: &str, max_chars: usize) -> String {
    // Drop everything from the first injected reminder block — that's
    // implementation detail, not the user's message.
    let user_text = match s.find("<system-reminder>") {
        Some(idx) => s[..idx].trim(),
        None => s.trim(),
    };
    let mut out = String::new();
    let mut count = 0usize;
    let mut prev_space = false;
    for ch in user_text.chars() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        if ch.is_control() || ch == ' ' {
            if !prev_space && count > 0 {
                out.push(' ');
                count += 1;
                prev_space = true;
            }
        } else {
            out.push(ch);
            count += 1;
            prev_space = false;
        }
    }
    out
}

fn append_memory_bootstrap_reminder(task: &str, history_len: usize) -> String {
    if history_len > 0 || task.contains("history_empty_or_reset: true") {
        return task.to_string();
    }
    format!(
        "{task}\n\n<system-reminder>\n\
         history_empty_or_reset: true\n\
         memory_bootstrap_required: Before using any other tool for this run, call `mem_recall` \
         once with a concise task-relevant query so prior preferences/constraints are restored.\n\
         fallback_shell_standard: If writing shell scripts and no recalled preference overrides it, \
         default to POSIX shell (`#!/bin/sh` and `set -eu`; avoid bash-only features).\n\
         </system-reminder>"
    )
}

/// Build the full bot command list from built-ins + discovered skills.
fn build_bot_commands_list(
    skills: &[skills::SkillMeta],
    plugin_commands: &[that_plugins::ResolvedPluginCommand],
) -> Vec<that_channels::BotCommand> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cmds = vec![
        that_channels::BotCommand {
            command: "help".into(),
            description: "List available commands".into(),
        },
        that_channels::BotCommand {
            command: "clear".into(),
            description: "Clear conversation history".into(),
        },
        that_channels::BotCommand {
            command: "compact".into(),
            description: "Keep only the most recent exchanges".into(),
        },
        that_channels::BotCommand {
            command: "stop".into(),
            description: "Stop the active agent run".into(),
        },
    ];
    for built_in in &cmds {
        seen.insert(built_in.command.clone());
    }

    for plugin_cmd in plugin_commands {
        if seen.insert(plugin_cmd.command.clone()) {
            cmds.push(that_channels::BotCommand {
                command: plugin_cmd.command.clone(),
                description: plugin_cmd.description.chars().take(256).collect(),
            });
        }
    }

    for skill in skills {
        let cmd = skill_to_command(&skill.name);
        if !seen.insert(cmd.clone()) {
            continue;
        }
        let desc: String = skill.description.chars().take(256).collect();
        cmds.push(that_channels::BotCommand {
            command: cmd,
            description: desc,
        });
    }
    cmds
}

/// Compute a fingerprint over all effective skill/plugin assets for an agent.
/// Used to detect when skills/plugins are added, removed, enabled, disabled, or modified.
fn skills_fingerprint(agent: &AgentDef) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    plugin_registry.fingerprint.hash(&mut hasher);
    for err in &plugin_registry.load_errors {
        err.hash(&mut hasher);
    }
    let skill_roots = skill_roots_for_agent(agent, &plugin_registry);
    let mut files: Vec<(String, u128)> = skill_roots
        .iter()
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .ok()
                .into_iter()
                .flat_map(|entries| entries.flatten())
                .filter_map(|e| {
                    let skill_file = e.path().join("SKILL.md");
                    if skill_file.exists() {
                        let mtime_ns = std::fs::metadata(&skill_file)
                            .and_then(|m| m.modified())
                            .and_then(|t| {
                                t.duration_since(std::time::UNIX_EPOCH)
                                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                            })
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        Some((skill_file.to_string_lossy().into_owned(), mtime_ns))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, mtime_ns) in files {
        name.hash(&mut hasher);
        mtime_ns.hash(&mut hasher);
    }
    hasher.finish()
}

fn skill_roots_for_agent(
    agent: &AgentDef,
    plugin_registry: &that_plugins::PluginRegistry,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = skills::skills_dir_local(&agent.name) {
        roots.push(local);
    }
    roots.extend(plugin_registry.enabled_skill_dirs());
    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let key = root.to_string_lossy().to_string();
        if seen.insert(key) {
            deduped.push(root);
        }
    }
    deduped
}

fn resolved_skill_roots(agent: &AgentDef) -> Vec<std::path::PathBuf> {
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    let mut roots = skill_roots_for_agent(agent, &plugin_registry);
    if roots.is_empty() {
        roots.push(std::path::PathBuf::from(".that-agent/skills"));
    }
    roots
}

fn discover_plugin_commands(agent: &AgentDef) -> Vec<that_plugins::ResolvedPluginCommand> {
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    plugin_registry.enabled_commands()
}

fn discover_plugin_activations(agent: &AgentDef) -> Vec<that_plugins::ResolvedPluginActivation> {
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    plugin_registry.enabled_activations()
}

fn format_plugin_preamble(agent: &AgentDef, sandbox: bool) -> String {
    let registry = that_plugins::PluginRegistry::load(&agent.name);
    let summaries = registry.summaries();
    if summaries.is_empty() {
        return String::new();
    }

    let plugins_path = if sandbox {
        format!("/home/agent/.that-agent/agents/{}/plugins", agent.name)
    } else {
        format!("~/.that-agent/agents/{}/plugins", agent.name)
    };
    let runtime_note = if sandbox {
        match that_sandbox::backend::SandboxMode::from_env() {
            that_sandbox::backend::SandboxMode::Docker => {
                let socket = that_sandbox::docker::docker_socket_status();
                if socket.enabled {
                    format!(
                        "Sandbox runtime mode: `docker` with host Docker socket mounted at `{}`. \
                         Prefer Dockerfile + `docker build/run` or `docker compose` for deploy/run requests.",
                        socket.path.display()
                    )
                } else {
                    format!(
                        "Sandbox runtime mode: `docker` without host Docker socket at `{}`. \
                         Plugin deploy flows can still run in-container, but sibling host-container orchestration is unavailable.",
                        socket.path.display()
                    )
                }
            }
            that_sandbox::backend::SandboxMode::Kubernetes => {
                let k8s = that_sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
                let image_backend = std::env::var("THAT_IMAGE_BUILD_BACKEND")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "Sandbox runtime mode: `kubernetes` (namespace `{}`, registry `{}`). \
                     Active image builder: `{}` from `<system-reminder>`. \
                     Follow backend strictly: use BuildKit when backend is `buildkit`; \
                     do not request Docker socket unless backend is explicitly `docker`. \
                     Prefer manifest + kustomize workflows with rollout checks.",
                    k8s.namespace, k8s.registry, image_backend
                )
            }
        }
    } else if trusted_local_sandbox_enabled() {
        match that_sandbox::backend::SandboxMode::from_env() {
            that_sandbox::backend::SandboxMode::Docker => {
                let socket = that_sandbox::docker::docker_socket_status();
                if socket.enabled {
                    format!(
                        "Trusted local runtime mode: `docker` with host Docker socket at `{}`. \
                         Docker deploy flows are available.",
                        socket.path.display()
                    )
                } else {
                    format!(
                        "Trusted local runtime mode: `docker` without host Docker socket at `{}`. \
                         Avoid Docker daemon workflows unless socket access becomes available.",
                        socket.path.display()
                    )
                }
            }
            that_sandbox::backend::SandboxMode::Kubernetes => {
                let k8s = that_sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
                let image_backend = std::env::var("THAT_IMAGE_BUILD_BACKEND")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "Trusted local runtime mode: `kubernetes` (namespace `{}`, registry `{}`). \
                     Active image builder: `{}` from `<system-reminder>`. \
                     Follow backend strictly: use BuildKit when backend is `buildkit`; \
                     do not request Docker socket unless backend is explicitly `docker`. \
                     Prefer manifest + kustomize workflows with rollout checks.",
                    k8s.namespace, k8s.registry, image_backend
                )
            }
        }
    } else {
        "Runtime backends: `docker` (default; can run/deploy via Docker socket) and `kubernetes`."
            .to_string()
    };

    let mut out = String::new();
    out.push_str("## Plugins\n\n");
    out.push_str(&format!(
        "Plugin directory: `{plugins_path}`  \n\
         Plugins are agent-scoped and must keep assets inside their own plugin directory.  \n\
         Standard plugin subdirectories: `skills/`, `scripts/`, `deploy/`, `state/`, `artifacts/`.  \n\
         {runtime_note}  \n\
         Plugins can add commands, skills, routines, activations, and emoji packs.  \n\
         Changes are hot-reloaded automatically.\n\n"
    ));
    for plugin in summaries {
        let desc = plugin
            .description
            .unwrap_or_else(|| "No description".to_string());
        out.push_str(&format!(
            "- **{}** (`{}` v{}): {}. Commands: {}, Routines: {}, Activations: {}, Emojis: {}\n",
            plugin.name,
            plugin.id,
            plugin.version,
            desc,
            plugin.command_count,
            plugin.routine_count,
            plugin.activation_count,
            plugin.emoji_count
        ));
    }
    let emojis = registry.enabled_emojis();
    if !emojis.is_empty() {
        out.push_str("### Emoji Catalog\n\n");
        for emoji in emojis {
            out.push_str(&format!(
                "- `{}.{}` => {}\n",
                emoji.plugin_id, emoji.name, emoji.value
            ));
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Compute a fingerprint from a file's mtime. Returns 0 if the file is missing or unreadable.
fn file_mtime_hash(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        })
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Shared hot-reloadable state: rebuilt when skills files or agent config change on disk.
struct HotState {
    found_skills: Vec<skills::SkillMeta>,
    plugin_commands: Vec<that_plugins::ResolvedPluginCommand>,
    plugin_activations: Vec<that_plugins::ResolvedPluginActivation>,
    bot_commands: Vec<that_channels::BotCommand>,
    preamble: String,
    skills_fp: u64,
    /// mtime hash of the agent's TOML config file for detecting hot-reload.
    agent_def_fp: u64,
    /// Current agent definition — updated when the config file changes.
    agent: AgentDef,
}

pub async fn run_listen(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    sandbox: bool,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    inbound_rx: tokio::sync::mpsc::UnboundedReceiver<that_channels::InboundMessage>,
) -> Result<()> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = Arc::new(SessionManager::new(&state_dir)?);
    let agent_workspace = resolve_agent_workspace(ws, agent)?;
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;
    let ws_files = load_workspace_files(agent, sandbox);

    // Initial skill discovery + preamble.
    let found_skills = discover_skills(agent, sandbox);
    let plugin_commands = discover_plugin_commands(agent);
    let plugin_activations = discover_plugin_activations(agent);
    let bot_commands = build_bot_commands_list(&found_skills, &plugin_commands);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();
    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws_files,
        0,
        "listen",
        &session_summaries,
    );
    let skills_fp = skills_fingerprint(agent);

    // Compute initial fingerprint for the agent config file.
    // Must match the resolution order in WorkspaceConfig::load_agent():
    // preferred = agents/<name>/config.toml, legacy = agents/<name>.toml
    let agent_config_path = {
        let agents_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".that-agent")
            .join("agents");
        let preferred = agents_dir.join(&agent.name).join("config.toml");
        let legacy = agents_dir.join(format!("{}.toml", agent.name));
        if preferred.exists() {
            preferred
        } else {
            legacy
        }
    };
    let agent_def_fp = file_mtime_hash(&agent_config_path);

    // Hot state shared between the reload task and message handlers.
    let hot = Arc::new(RwLock::new(HotState {
        found_skills,
        plugin_commands,
        plugin_activations,
        bot_commands,
        preamble,
        skills_fp,
        agent_def_fp,
        agent: agent.clone(),
    }));

    // Per-sender state: key = "channel_id:sender_id" → (session_id, history).
    let sessions: Arc<Mutex<HashMap<String, (String, Vec<Message>)>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let channel_index_lock = Arc::new(Mutex::new(()));
    let plugin_runtime_lock = Arc::new(Mutex::new(()));

    eprintln!(
        "[that] Listening on channels: {} (primary: {})\n[that] Agent: {} — Ctrl+C to stop.",
        router.channel_ids(),
        router.primary_id(),
        agent.name,
    );

    // Validate each channel's config (token check, connectivity) before opening listeners.
    router.initialize().await;

    router.start_listeners().await?;

    // Signal K8s readiness — channels are initialized and listening.
    let _ = std::fs::File::create("/tmp/that-agent-ready");

    // Register initial commands.
    router
        .register_commands(&hot.read().await.bot_commands)
        .await;

    // ── Background hot-reload task ──────────────────────────────────────────
    // Polls skills/plugins every 5 seconds. When the fingerprint changes
    // (new/removed/modified SKILL.md files, plugin state/manifest updates), rebuilds runtime state and
    // re-registers bot commands — all without restarting the process.
    // Also monitors the agent TOML config for changes and hot-reloads it.
    {
        let hot = Arc::clone(&hot);
        let router = Arc::clone(&router);
        let mut agent_hot = agent.clone();
        let agent_workspace = agent_workspace.clone();
        let agent_config_path_hot = agent_config_path.clone();
        let session_mgr = Arc::clone(&session_mgr);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                // ── Skills hot-reload ─────────────────────────────────────
                let new_skills_fp = skills_fingerprint(&agent_hot);
                let skills_changed = new_skills_fp != hot.read().await.skills_fp;

                // ── Agent config hot-reload ───────────────────────────────
                let new_cfg_fp = file_mtime_hash(&agent_config_path_hot);
                let config_changed = new_cfg_fp != hot.read().await.agent_def_fp;

                if config_changed {
                    if let Ok(mut new_agent) = AgentDef::from_file(&agent_config_path_hot) {
                        new_agent.name = agent_hot.name.clone();
                        // Push runtime-mutable config (e.g. allowed_senders) to live adapters
                        // so changes take effect immediately without restarting the process.
                        router
                            .apply_config_updates(&new_agent.channels.adapters)
                            .await;
                        agent_hot = new_agent;
                        info!("Hot-reloaded agent config");
                    }
                }

                if skills_changed || config_changed {
                    let new_skills = discover_skills(&agent_hot, sandbox);
                    let new_plugin_commands = discover_plugin_commands(&agent_hot);
                    let new_plugin_activations = discover_plugin_activations(&agent_hot);
                    let new_commands = build_bot_commands_list(&new_skills, &new_plugin_commands);
                    let summaries = session_mgr.session_summaries(5).unwrap_or_default();
                    // Re-read workspace files on hot-reload — agent may have edited them.
                    let new_ws = load_workspace_files(&agent_hot, sandbox);
                    let new_preamble = build_preamble(
                        &agent_workspace,
                        &agent_hot,
                        sandbox,
                        &new_skills,
                        &new_ws,
                        0,
                        "listen",
                        &summaries,
                    );
                    if skills_changed {
                        info!(count = new_skills.len(), "Hot-reloading skills");
                    }
                    router.register_commands(&new_commands).await;
                    let mut state = hot.write().await;
                    state.found_skills = new_skills;
                    state.plugin_commands = new_plugin_commands;
                    state.plugin_activations = new_plugin_activations;
                    state.bot_commands = new_commands;
                    state.preamble = new_preamble;
                    state.skills_fp = new_skills_fp;
                    state.agent_def_fp = new_cfg_fp;
                    state.agent = agent_hot.clone();
                }
            }
        });
    }

    // ── Background heartbeat monitor ────────────────────────────────────────
    // Polls Heartbeat.md every heartbeat_interval seconds. Due entries are
    // dispatched as autonomous agent runs. Global items use "heartbeat:system";
    // route-aware plugin activation items are isolated by channel/chat sender key.
    {
        let hot = Arc::clone(&hot);
        let container_hb = container.clone();
        let session_mgr_hb = Arc::clone(&session_mgr);
        let sessions_hb = Arc::clone(&sessions);
        let channel_index_lock_hb = Arc::clone(&channel_index_lock);
        let plugin_runtime_lock_hb = Arc::clone(&plugin_runtime_lock);
        let router_hb = Arc::clone(&router);
        let interval_secs = agent.heartbeat_interval.unwrap_or(10).max(1);

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            loop {
                ticker.tick().await;

                // Touch liveness file so K8s knows the event loop is alive.
                let _ = tokio::fs::File::create("/tmp/that-agent-alive").await;

                // Snapshot current preamble and agent from hot state.
                let (preamble_hb, current_agent) = {
                    let state = hot.read().await;
                    (state.preamble.clone(), state.agent.clone())
                };

                // Ensure Heartbeat.md exists, then load entries.
                if let Some(c) = &container_hb {
                    match heartbeat::ensure_heartbeat_sandbox(c, &current_agent.name) {
                        Ok(true) => {
                            tracing::info!(agent = %current_agent.name, "Bootstrapped Heartbeat.md in sandbox");
                        }
                        Ok(false) => {}
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to bootstrap Heartbeat.md in sandbox"
                            );
                        }
                    }
                } else {
                    match heartbeat::ensure_heartbeat_local(&current_agent.name) {
                        Ok(true) => {
                            tracing::info!(agent = %current_agent.name, "Bootstrapped Heartbeat.md");
                        }
                        Ok(false) => {}
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to bootstrap Heartbeat.md"
                            );
                        }
                    }
                }

                // Load heartbeat entries.
                let entries_opt = if let Some(c) = &container_hb {
                    heartbeat::load_heartbeat_sandbox(c, &current_agent.name)
                } else {
                    heartbeat::load_heartbeat_local(&current_agent.name)
                };
                let mut entries = entries_opt.unwrap_or_default();

                // Find due pending entries sorted urgent-first.
                let mut due_indices: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        matches!(
                            e.status,
                            heartbeat::Status::Pending
                                | heartbeat::Status::Running
                                | heartbeat::Status::Processing
                        ) && heartbeat::is_entry_due(e)
                    })
                    .map(|(i, _)| i)
                    .collect();

                due_indices.sort_by_key(|&i| match entries[i].priority {
                    heartbeat::Priority::Urgent => 0u8,
                    heartbeat::Priority::High => 1,
                    heartbeat::Priority::Normal => 2,
                    heartbeat::Priority::Low => 3,
                    heartbeat::Priority::Unknown(_) => 4,
                });

                let plugin_tasks = {
                    let _runtime_guard = plugin_runtime_lock_hb.lock().await;
                    let plugin_registry = that_plugins::PluginRegistry::load(&current_agent.name);
                    match that_plugins::collect_due_heartbeat_tasks(
                        &current_agent.name,
                        &plugin_registry,
                    ) {
                        Ok(tasks) => tasks,
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to collect plugin heartbeat tasks"
                            );
                            Vec::new()
                        }
                    }
                };

                let mut scoped_plugin_tasks: std::collections::BTreeMap<
                    String,
                    (
                        String,
                        Option<String>,
                        Option<String>,
                        Vec<that_plugins::PluginHeartbeatTask>,
                    ),
                > = std::collections::BTreeMap::new();
                let mut unscoped_plugin_tasks: Vec<that_plugins::PluginHeartbeatTask> = Vec::new();
                for item in plugin_tasks {
                    if let Some(route) = item.route.as_ref() {
                        let channel_id = route
                            .channel_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(ToOwned::to_owned);
                        if let Some(channel_id) = channel_id {
                            let conversation_id = route
                                .conversation_id
                                .as_deref()
                                .map(str::trim)
                                .filter(|v| !v.is_empty())
                                .map(ToOwned::to_owned);
                            let sender_id = route
                                .sender_id
                                .as_deref()
                                .map(str::trim)
                                .filter(|v| !v.is_empty())
                                .map(ToOwned::to_owned);
                            let key = format!(
                                "{}:{}:{}",
                                channel_id,
                                conversation_id.clone().unwrap_or_default(),
                                sender_id.clone().unwrap_or_default(),
                            );
                            scoped_plugin_tasks
                                .entry(key)
                                .or_insert_with(|| {
                                    (channel_id, conversation_id, sender_id, Vec::new())
                                })
                                .3
                                .push(item);
                            continue;
                        }
                    }
                    unscoped_plugin_tasks.push(item);
                }

                if due_indices.is_empty()
                    && unscoped_plugin_tasks.is_empty()
                    && scoped_plugin_tasks.is_empty()
                {
                    continue;
                }

                let plugin_only_notify_guidance =
                    "\n\nUse the `channel_notify` tool to keep users informed:\n\
                     - Send a brief summary when meaningful work is completed.\n\
                     - Send a notice if you are blocked or cannot complete an item.\n\
                     - Skip the notification if all items are routine housekeeping with no user-visible outcome.";

                // Mark as running and stamp last_run before dispatch so recurring
                // schedules are tracked and don't retrigger in the same slot.
                let dispatch_started_at = Local::now();
                for &i in &due_indices {
                    entries[i].last_run = Some(dispatch_started_at);
                    entries[i].status = match entries[i].schedule {
                        heartbeat::Schedule::Once => heartbeat::Status::Done,
                        _ => heartbeat::Status::Running,
                    };
                }

                if !due_indices.is_empty() {
                    if let Some(c) = &container_hb {
                        let _ = heartbeat::save_heartbeat_sandbox(c, &current_agent.name, &entries);
                    } else {
                        let _ = heartbeat::save_heartbeat_local(&current_agent.name, &entries);
                    }
                }

                let due_refs: Vec<&heartbeat::HeartbeatEntry> =
                    due_indices.iter().map(|&i| &entries[i]).collect();
                if !due_refs.is_empty() || !unscoped_plugin_tasks.is_empty() {
                    let mut task = if due_refs.is_empty() {
                        String::from(
                            "Heartbeat check-in. Process the following plugin-triggered items:\n\n",
                        )
                    } else {
                        heartbeat::format_heartbeat_task(&due_refs)
                    };
                    append_plugin_heartbeat_tasks(&mut task, &unscoped_plugin_tasks);
                    if due_refs.is_empty() {
                        task.push_str(plugin_only_notify_guidance);
                    }

                    info!(
                        entries = due_indices.len(),
                        plugin_items = unscoped_plugin_tasks.len(),
                        scoped_plugin_groups = scoped_plugin_tasks.len(),
                        "Dispatching global heartbeat run"
                    );

                    run_agent_for_sender(
                        task,
                        "heartbeat".to_string(),
                        "system".to_string(),
                        None,
                        None,
                        None, // session_hint
                        "heartbeat:system".to_string(),
                        Arc::clone(&sessions_hb),
                        Arc::clone(&channel_index_lock_hb),
                        Arc::clone(&session_mgr_hb),
                        current_agent.clone(),
                        container_hb.clone(),
                        preamble_hb.clone(),
                        Arc::clone(&router_hb),
                    )
                    .await;
                }

                for (
                    key,
                    (route_channel_id, route_conversation_id, route_sender_id, route_tasks),
                ) in scoped_plugin_tasks
                {
                    let mut task = String::from(
                        "Heartbeat check-in. Process the following plugin-triggered items:\n\n",
                    );
                    append_plugin_heartbeat_tasks(&mut task, &route_tasks);
                    task.push_str(plugin_only_notify_guidance);

                    let sender_for_route = route_sender_id
                        .clone()
                        .unwrap_or_else(|| "system".to_string());
                    let heartbeat_sender_key = format!(
                        "heartbeat:{}:{}:{}",
                        route_channel_id,
                        route_conversation_id.clone().unwrap_or_default(),
                        sender_for_route,
                    );

                    info!(
                        route = %key,
                        channel = %route_channel_id,
                        conversation = ?route_conversation_id,
                        sender = ?route_sender_id,
                        plugin_items = route_tasks.len(),
                        "Dispatching scoped heartbeat run"
                    );

                    run_agent_for_sender(
                        task,
                        route_channel_id,
                        sender_for_route,
                        route_conversation_id,
                        None,
                        None, // session_hint
                        heartbeat_sender_key,
                        Arc::clone(&sessions_hb),
                        Arc::clone(&channel_index_lock_hb),
                        Arc::clone(&session_mgr_hb),
                        current_agent.clone(),
                        container_hb.clone(),
                        preamble_hb.clone(),
                        Arc::clone(&router_hb),
                    )
                    .await;
                }
            }
        });
    }

    let sender_locks = SenderRunLocks::default();
    let active_sender_runs: ActiveSenderRuns = Arc::default();
    let sender_run_seq = Arc::new(AtomicU64::new(1));
    let inbound_router = that_channels::InboundRouter::new(inbound_rx);
    inbound_router
        .run_concurrent(move |msg| {
            let router = Arc::clone(&router);
            let container = container.clone();
            let session_mgr = Arc::clone(&session_mgr);
            let sessions = Arc::clone(&sessions);
            let channel_index_lock = Arc::clone(&channel_index_lock);
            let plugin_runtime_lock = Arc::clone(&plugin_runtime_lock);
            let hot = Arc::clone(&hot);
            let sender_locks = Arc::clone(&sender_locks);
            let active_sender_runs = Arc::clone(&active_sender_runs);
            let sender_run_seq = Arc::clone(&sender_run_seq);

            async move {
                let sender_key = format!(
                    "{}:{}:{}",
                    msg.channel_id,
                    msg.conversation_id.clone().unwrap_or_default(),
                    msg.sender_id
                );
                let parsed_slash = parse_slash_command(&msg.text);
                if matches!(parsed_slash.as_ref(), Some((cmd, _)) if cmd == "stop") {
                    let outbound_target = that_channels::OutboundTarget {
                        recipient_id: msg
                            .conversation_id
                            .clone()
                            .or_else(|| Some(msg.sender_id.clone())),
                        sender_id: Some(msg.sender_id.clone()),
                        thread_id: msg.session_hint.clone(),
                        session_id: None,
                        reply_to_message_id: msg.message_id,
                    };
                    let stopped = stop_active_sender_run(&active_sender_runs, &sender_key).await;
                    let text = if stopped {
                        "Stopped current run."
                    } else {
                        "No active run to stop."
                    };
                    router
                        .notify_channel(&msg.channel_id, text, Some(&outbound_target))
                        .await;
                    return;
                }
                let sender_lock = {
                    let mut locks = sender_locks.lock().await;
                    locks
                        .entry(sender_key.clone())
                        .or_insert_with(|| SenderRunLock::default())
                        .clone()
                };
                let sender_guard = sender_lock.lock().await;
                (async {
                    let outbound_target = that_channels::OutboundTarget {
                        recipient_id: msg
                            .conversation_id
                            .clone()
                            .or_else(|| Some(msg.sender_id.clone())),
                        sender_id: Some(msg.sender_id.clone()),
                        thread_id: msg.session_hint.clone(),
                        session_id: None,
                        reply_to_message_id: msg.message_id,
                    };

                    // Snapshot the current hot state for this message.
                    let (
                        preamble,
                        bot_commands,
                        found_skills,
                        plugin_commands,
                        plugin_activations,
                        agent,
                    ) = {
                        let state = hot.read().await;
                        (
                            state.preamble.clone(),
                            state.bot_commands.clone(),
                            state.found_skills.clone(),
                            state.plugin_commands.clone(),
                            state.plugin_activations.clone(),
                            state.agent.clone(),
                        )
                    };

                    if !plugin_activations.is_empty() {
                        let slash_command = parsed_slash.as_ref().map(|(cmd, _)| cmd.as_str());
                        let slash_args = parsed_slash.as_ref().map(|(_, args)| args.as_str());
                        let mut queued = 0usize;
                        let _guard = plugin_runtime_lock.lock().await;
                        for activation in &plugin_activations {
                            if !activation_matches_message(activation, &msg.text, slash_command) {
                                continue;
                            }
                            let task = render_activation_task(activation, &msg.text, slash_args);
                            match that_plugins::enqueue_activation_task(
                                &agent.name,
                                &activation.plugin_id,
                                &activation.name,
                                &activation.priority,
                                &task,
                                Some(that_plugins::PluginTaskRoute {
                                    channel_id: Some(msg.channel_id.clone()),
                                    conversation_id: msg.conversation_id.clone(),
                                    sender_id: Some(msg.sender_id.clone()),
                                }),
                            ) {
                                Ok(()) => queued += 1,
                                Err(err) => tracing::warn!(
                                    plugin = %activation.plugin_id,
                                    activation = %activation.name,
                                    error = %err,
                                    "Failed to queue plugin activation"
                                ),
                            }
                        }
                        if queued > 0 {
                            tracing::info!(queued, "Queued plugin activations");
                        }
                    }

                    // ── Slash-command dispatch ───────────────────────────────────
                    if let Some((cmd, args)) = parsed_slash {
                        info!(channel = %msg.channel_id, sender = %msg.sender_id, cmd = %cmd, "Slash command");
                        match cmd.as_str() {
                            "help" => {
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        &build_help_text(&bot_commands),
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            "clear" => {
                                // Create a fresh session so the old transcript is abandoned.
                                let new_sid = session_mgr
                                    .create_session()
                                    .unwrap_or_else(|_| "unknown".into());
                                {
                                    let mut map = sessions.lock().await;
                                    map.insert(sender_key.clone(), (new_sid.clone(), Vec::new()));
                                }
                                // Persist the new mapping so restarts don't resurrect old history.
                                {
                                    let _guard = channel_index_lock.lock().await;
                                    session_mgr.save_channel_session(&sender_key, &new_sid);
                                }
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        "Conversation cleared.",
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            "compact" => {
                                let mut map = sessions.lock().await;
                                if let Some(entry) = map.get_mut(&sender_key) {
                                    if entry.1.is_empty() {
                                        drop(map);
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "Nothing to compact.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                    } else {
                                        let hist_clone = entry.1.clone();
                                        let sid = entry.0.clone();
                                        drop(map);
                                        // LLM-generated summary of the conversation.
                                        let summary = build_compact_summary(
                                            &agent.provider,
                                            &agent.model,
                                            &agent.name,
                                            container.is_some(),
                                            &hist_clone,
                                        )
                                        .await;
                                        // Reset in-memory history to just the compaction anchor.
                                        let compacted_history = vec![
                                            Message::user(format!(
                                                "[Conversation context summary: {summary}]"
                                            )),
                                            Message::assistant(
                                                "Understood, I have the context from our previous conversation.".to_string(),
                                            ),
                                        ];
                                        {
                                            let mut map = sessions.lock().await;
                                            if let Some(entry) = map.get_mut(&sender_key) {
                                                entry.1 = compacted_history;
                                            }
                                        }
                                        // Write a Compaction marker to the transcript so
                                        // rebuild_history_recent respects it on restart.
                                        let _ = session_mgr.append(
                                            &sid,
                                            &TranscriptEntry {
                                                timestamp: Utc::now(),
                                                run_id: new_run_id(),
                                                event: TranscriptEvent::Compaction {
                                                    summary: summary.clone(),
                                                },
                                            },
                                        );
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "History compacted.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                    }
                                } else {
                                    drop(map);
                                    router
                                        .notify_channel(
                                            &msg.channel_id,
                                            "Nothing to compact.",
                                            Some(&outbound_target),
                                        )
                                        .await;
                                }
                                return;
                            }
                            "stop" => {
                                let stopped =
                                    stop_active_sender_run(&active_sender_runs, &sender_key).await;
                                let text = if stopped {
                                    "Stopped current run."
                                } else {
                                    "No active run to stop."
                                };
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        text,
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            skill_cmd => {
                                if let Some(plugin_cmd) =
                                    find_plugin_command(skill_cmd, &plugin_commands)
                                {
                                    let effective_task =
                                        render_plugin_command_task(plugin_cmd, &args);
                                    if effective_task.trim().is_empty() {
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "This plugin command requires arguments.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                        return;
                                    }
                                    run_agent_for_sender_tracked(
                                        effective_task,
                                        msg.channel_id,
                                        msg.sender_id,
                                        msg.conversation_id,
                                        msg.message_id,
                                        msg.session_hint,
                                        sender_key.clone(),
                                        sessions,
                                        std::sync::Arc::clone(&channel_index_lock),
                                        session_mgr,
                                        agent,
                                        container,
                                        preamble,
                                        router,
                                        Arc::clone(&active_sender_runs),
                                        Arc::clone(&sender_run_seq),
                                    )
                                    .await;
                                    return;
                                } else if let Some(skill) =
                                    find_skill_by_command(skill_cmd, &found_skills)
                                {
                                    let effective_task = if args.is_empty() {
                                        skill.description.clone()
                                    } else {
                                        args
                                    };
                                    run_agent_for_sender_tracked(
                                        effective_task,
                                        msg.channel_id,
                                        msg.sender_id,
                                        msg.conversation_id,
                                        msg.message_id,
                                        msg.session_hint,
                                        sender_key.clone(),
                                        sessions,
                                        std::sync::Arc::clone(&channel_index_lock),
                                        session_mgr,
                                        agent,
                                        container,
                                        preamble,
                                        router,
                                        Arc::clone(&active_sender_runs),
                                        Arc::clone(&sender_run_seq),
                                    )
                                    .await;
                                    return;
                                } else {
                                    router
                                        .notify_channel(
                                            &msg.channel_id,
                                            &format!("Unknown command /{cmd} — try /help"),
                                            Some(&outbound_target),
                                        )
                                        .await;
                                    return;
                                }
                            }
                        }
                    }

                    // ── Regular message → agent run ──────────────────────────────
                    run_agent_for_sender_tracked(
                        msg.text,
                        msg.channel_id,
                        msg.sender_id,
                        msg.conversation_id,
                        msg.message_id,
                        msg.session_hint,
                        sender_key.clone(),
                        sessions,
                        std::sync::Arc::clone(&channel_index_lock),
                        session_mgr,
                        agent,
                        container,
                        preamble,
                        router,
                        Arc::clone(&active_sender_runs),
                        Arc::clone(&sender_run_seq),
                    )
                    .await;
                })
                .await;
                drop(sender_guard);
                evict_sender_lock_if_idle(&sender_locks, &sender_key, &sender_lock).await;
            }
        })
        .await;

    Ok(())
}

/// Execute a single channel turn in a cancellable task and track abort handles by sender.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_sender_tracked(
    task: String,
    channel_id: String,
    sender_id: String,
    conversation_id: Option<String>,
    message_id: Option<i64>,
    session_hint: Option<String>,
    sender_key: String,
    sessions: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, (String, Vec<Message>)>>,
    >,
    channel_index_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    session_mgr: std::sync::Arc<SessionManager>,
    agent: AgentDef,
    container: Option<String>,
    preamble: String,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    active_runs: ActiveSenderRuns,
    run_seq: Arc<AtomicU64>,
) {
    let active_run_id = run_seq.fetch_add(1, Ordering::Relaxed);
    let sender_key_for_task = sender_key.clone();
    let sender_key_for_cleanup = sender_key.clone();

    let run_task = tokio::spawn(async move {
        run_agent_for_sender(
            task,
            channel_id,
            sender_id,
            conversation_id,
            message_id,
            session_hint,
            sender_key_for_task,
            sessions,
            channel_index_lock,
            session_mgr,
            agent,
            container,
            preamble,
            router,
        )
        .await;
    });
    let abort_handle = run_task.abort_handle();
    {
        let mut runs = active_runs.lock().await;
        runs.insert(
            sender_key.clone(),
            ActiveSenderRun {
                run_id: active_run_id,
                abort: abort_handle,
            },
        );
    }

    let _ = run_task.await;

    let mut runs = active_runs.lock().await;
    let should_remove = runs
        .get(&sender_key_for_cleanup)
        .map(|run| run.run_id == active_run_id)
        .unwrap_or(false);
    if should_remove {
        runs.remove(&sender_key_for_cleanup);
    }
}

/// Execute a single agent turn for an inbound message and persist the result.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_sender(
    task: String,
    channel_id: String,
    sender_id: String,
    conversation_id: Option<String>,
    message_id: Option<i64>,
    session_hint: Option<String>,
    sender_key: String,
    sessions: std::sync::Arc<
        tokio::sync::Mutex<std::collections::HashMap<String, (String, Vec<Message>)>>,
    >,
    channel_index_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    session_mgr: std::sync::Arc<SessionManager>,
    agent: AgentDef,
    container: Option<String>,
    preamble: String,
    router: std::sync::Arc<that_channels::ChannelRouter>,
) {
    struct TypingTaskGuard(Option<tokio::task::JoinHandle<()>>);
    impl TypingTaskGuard {
        fn take(&mut self) -> Option<tokio::task::JoinHandle<()>> {
            self.0.take()
        }
    }
    impl Drop for TypingTaskGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                handle.abort();
            }
        }
    }

    // Heartbeat-originated runs use sender_key prefixed with `heartbeat:` so
    // they stay silent by default and avoid user-facing typing indicators.
    let is_internal_source = sender_key.starts_with("heartbeat:");
    let base_target = that_channels::OutboundTarget {
        recipient_id: conversation_id.or_else(|| Some(sender_id.clone())),
        sender_id: Some(sender_id.clone()),
        thread_id: session_hint.clone(),
        session_id: None,
        reply_to_message_id: message_id,
    };

    // Immediately acknowledge the message so the user knows the agent is working.
    // Skip for internal heartbeat sources.
    let mut typing_task = TypingTaskGuard(if !is_internal_source {
        // React to the user's message with 👀 so they know the agent saw it.
        if let Some(mid) = message_id {
            let react_chat = base_target
                .recipient_id
                .as_deref()
                .unwrap_or(sender_id.as_str());
            router
                .react_to_message(&channel_id, react_chat, mid, "👀")
                .await;
        }
        // Send typing indicator immediately and refresh every 4s while the agent runs.
        // Telegram's "typing" action expires after ~5s, so 4s keeps it alive.
        let event = that_channels::ChannelEvent::TypingIndicator;
        let _ = router
            .send_to(&channel_id, &event, Some(&base_target))
            .await;
        let typing_router = std::sync::Arc::clone(&router);
        let typing_channel_id = channel_id.clone();
        let typing_target = base_target.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(4));
            interval.tick().await; // skip first tick (already sent above)
            loop {
                interval.tick().await;
                let event = that_channels::ChannelEvent::TypingIndicator;
                let _ = typing_router
                    .send_to(&typing_channel_id, &event, Some(&typing_target))
                    .await;
            }
        }))
    } else {
        None
    });
    // Look up or create a session for this sender.
    // On the first message after a restart, restore context from the persisted transcript.
    let (session_id, mut history) = {
        let mut map = sessions.lock().await;
        if let Some(existing) = map.get(&sender_key) {
            existing.clone()
        } else {
            // No in-memory state — check persistent index (handles restarts / crashes).
            let channel_index = session_mgr.load_channel_sessions();
            let (sid, hist) = if let Some(prior_sid) = channel_index.get(&sender_key) {
                // Restore the last 10 turns (or from the last compaction) from disk.
                let hist = session_mgr
                    .read_transcript(prior_sid)
                    .map(|entries| rebuild_history_recent(&entries, 10))
                    .unwrap_or_default();
                if !hist.is_empty() {
                    info!(
                        session = %prior_sid,
                        sender = %sender_key,
                        turns = hist.len() / 2,
                        "Restored conversation history after restart"
                    );
                }
                (prior_sid.clone(), hist)
            } else {
                // Truly new sender — create a dedicated session.
                let sid = session_mgr
                    .create_session()
                    .unwrap_or_else(|_| "unknown".into());
                (sid, Vec::new())
            };
            map.insert(sender_key.clone(), (sid.clone(), hist.clone()));
            (sid, hist)
        }
    };
    // Persist sender → session mapping so the next restart can recover context.
    {
        let _index_guard = channel_index_lock.lock().await;
        session_mgr.save_channel_session(&sender_key, &session_id);
    }
    let mut route_target = base_target.clone();
    route_target.session_id = Some(session_id.clone());

    info!(
        session = %session_id,
        channel = %channel_id,
        sender = %sender_id,
        "Agent run for inbound message"
    );
    info!(channel = %channel_id, sender = %sender_id, ">>> {task}");

    let run_id = new_run_id();
    let _ = session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::RunStart { task: task.clone() },
        },
    );
    let _ = session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::UserMessage {
                content: task.clone(),
            },
        },
    );
    let task_for_model =
        append_system_reminder(&task, &session_id, container.is_some(), &agent.name);

    let route_channel_id = if channel_id == "heartbeat" {
        None
    } else {
        Some(channel_id.clone())
    };
    let route_target_for_run = if channel_id == "heartbeat" {
        None
    } else {
        Some(route_target.clone())
    };
    let run_result = execute_agent_run_channel(
        &agent,
        container,
        &preamble,
        &task_for_model,
        history.clone(),
        std::sync::Arc::clone(&router),
        route_channel_id,
        route_target_for_run,
        is_internal_source,
        Some(&session_id),
        Some(&run_id),
    )
    .await;

    // Stop the typing indicator refresh — the response is on its way.
    if let Some(handle) = typing_task.take() {
        handle.abort();
    }

    match run_result {
        Ok((text, tool_events)) => {
            // Log tool calls and results before the final assistant message so the
            // transcript reads in execution order: input → tools → output.
            for ev in tool_events {
                match ev {
                    that_channels::ToolLogEvent::Call { name, args } => {
                        debug!(tool = %name, "  tool_call: {args}");
                        let arguments =
                            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({}));
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: run_id.clone(),
                                event: TranscriptEvent::ToolCall {
                                    tool: name,
                                    arguments,
                                },
                            },
                        );
                    }
                    that_channels::ToolLogEvent::Result {
                        name,
                        result,
                        is_error,
                    } => {
                        debug!(tool = %name, is_error, "  tool_result: {result}");
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: run_id.clone(),
                                event: TranscriptEvent::ToolResult {
                                    tool: name,
                                    result,
                                    is_error,
                                },
                            },
                        );
                    }
                }
            }

            info!(channel = %channel_id, "<<< {text}");
            history.push(Message::user(&task_for_model));
            history.push(Message::assistant(&text));
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::AssistantMessage { content: text },
                },
            );
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id,
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Success,
                        error: None,
                    },
                },
            );
            let mut map = sessions.lock().await;
            if let Some(entry) = map.get_mut(&sender_key) {
                entry.1 = history;
            }
        }
        Err(e) => {
            error!(session = %session_id, "Agent run failed: {e:#}");
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id,
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Error,
                        error: Some(format!("{e:#}")),
                    },
                },
            );
        }
    }
}

async fn route_channel_event(
    router: &that_channels::ChannelRouter,
    route_channel_id: Option<&str>,
    route_target: Option<&that_channels::OutboundTarget>,
    event: &that_channels::ChannelEvent,
) {
    if let Some(cid) = route_channel_id {
        let _ = router.send_to(cid, event, route_target).await;
    } else {
        router.broadcast(event).await;
    }
}

/// Build and execute a single agent run with streaming output.
/// Retries automatically on transient network errors with exponential backoff.
#[tracing::instrument(name = "agent_run", skip_all, fields(
    gen_ai.provider      = %agent.provider,
    gen_ai.provider.name = %agent.provider,
    gen_ai.request.model = %agent.model,
    gen_ai.prompt        = tracing::field::Empty,
    gen_ai.completion    = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    otel.status_code = tracing::field::Empty,
    otel.status_description = tracing::field::Empty,
    session.id = tracing::field::Empty,
    input.value = tracing::field::Empty,
    input.mime_type = "text/plain",
    output.value = tracing::field::Empty,
    output.mime_type = "text/plain",
))]
pub async fn execute_agent_run_streaming(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    debug: bool,
    history: Option<Vec<Message>>,
) -> Result<String> {
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let tools_config = load_agent_config(&container, agent);
    let skill_roots = resolved_skill_roots(agent);
    let history_len = history.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    let task_for_model = append_memory_bootstrap_reminder(task, history_len);
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            let delay_ms = RETRY_BASE_DELAY_MS << (attempt - 1).min(4);
            eprintln!(
                "\n[that-agent] Network error — retrying ({attempt}/{MAX_NETWORK_RETRIES}) in {}s…",
                delay_ms / 1_000
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        let api_key = api_key_for_provider(&agent.provider)?;
        let config = LoopConfig {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            api_key,
            system: preamble.to_string(),
            max_tokens: agent.max_tokens as u32,
            max_turns: agent.max_turns as u32,
            tools: all_tool_defs(&container),
            history: history.clone().unwrap_or_default(),
            prompt_caching: matches!(agent.provider.as_str(), "anthropic" | "openrouter"),
            openai_websocket: openai_websocket_enabled(),
            debug,
            tool_ctx: ToolContext {
                config: tools_config.clone(),
                container: container.clone(),
                skill_roots: skill_roots.clone(),
            },
        };
        let hook = AgentHook { debug };
        let result = agent_loop::run(&config, &task_for_model, &hook).await;

        match result {
            Ok((text, _usage)) => {
                tracing::Span::current().record("gen_ai.completion", &text.as_str());
                tracing::Span::current().record("output.value", &text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                return Ok(text);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < MAX_NETWORK_RETRIES {
                    attempt += 1;
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                return Err(e);
            }
        }
    }
}

/// Headless variant of `execute_agent_run_streaming` for eval mode.
///
/// Uses [`EvalHook`] so `human_ask` calls are automatically denied without blocking on stdin.
/// All `Prompt`-level policies are elevated to `Allow` — the eval harness is responsible for safety.
#[tracing::instrument(name = "agent_run", skip_all, fields(
    gen_ai.provider      = %agent.provider,
    gen_ai.provider.name = %agent.provider,
    gen_ai.request.model = %agent.model,
    gen_ai.prompt        = tracing::field::Empty,
    gen_ai.completion    = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    otel.status_code = tracing::field::Empty,
    otel.status_description = tracing::field::Empty,
    session.id = tracing::field::Empty,
    input.value = tracing::field::Empty,
    input.mime_type = "text/plain",
    output.value = tracing::field::Empty,
    output.mime_type = "text/plain",
))]
pub async fn execute_agent_run_eval(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    _debug: bool,
    history: Option<Vec<Message>>,
    session_id_for_trace: Option<&str>,
) -> Result<(String, Vec<String>)> {
    if let Some(sid) = session_id_for_trace {
        tracing::Span::current().record("session.id", sid);
    }
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let mut tools_config = load_agent_config(&container, agent);
    // Eval runs are headless — elevate Prompt policies to Allow.
    {
        use that_tools::config::PolicyLevel;
        let p = &mut tools_config.policy.tools;
        if matches!(p.fs_write, PolicyLevel::Prompt) {
            p.fs_write = PolicyLevel::Allow;
        }
        if matches!(p.code_edit, PolicyLevel::Prompt) {
            p.code_edit = PolicyLevel::Allow;
        }
        if matches!(p.git_commit, PolicyLevel::Prompt) {
            p.git_commit = PolicyLevel::Allow;
        }
    }
    let skill_roots = resolved_skill_roots(agent);
    let history_len = history.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    let task_for_model = append_memory_bootstrap_reminder(task, history_len);
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            let delay_ms = RETRY_BASE_DELAY_MS << (attempt - 1).min(4);
            eprintln!(
                "\n[that-eval] Network error — retrying ({attempt}/{MAX_NETWORK_RETRIES}) in {}s…",
                delay_ms / 1_000
            );
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        let api_key = api_key_for_provider(&agent.provider)?;
        let config = LoopConfig {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            api_key,
            system: preamble.to_string(),
            max_tokens: agent.max_tokens as u32,
            max_turns: agent.max_turns as u32,
            tools: all_tool_defs(&container),
            history: history.clone().unwrap_or_default(),
            prompt_caching: matches!(agent.provider.as_str(), "anthropic" | "openrouter"),
            openai_websocket: openai_websocket_enabled(),
            debug: false,
            tool_ctx: ToolContext {
                config: tools_config.clone(),
                container: container.clone(),
                skill_roots: skill_roots.clone(),
            },
        };
        let hook = EvalHook::new();
        let result = agent_loop::run(&config, &task_for_model, &hook).await;

        match result {
            Ok((text, _usage)) => {
                tracing::Span::current().record("gen_ai.completion", &text.as_str());
                tracing::Span::current().record("output.value", &text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                return Ok((text, hook.take_events()));
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < MAX_NETWORK_RETRIES {
                    attempt += 1;
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                return Err(e);
            }
        }
    }
}

/// Interactive chat loop using a Ratatui TUI.
#[tracing::instrument(name = "chat_tui_session", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
    session.id = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    trace_id = tracing::field::Empty,
    span_id = tracing::field::Empty,
))]
pub async fn run_chat_tui(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let mut session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.create_session()?,
    };
    tracing::Span::current().record("session.id", session_id.as_str());
    if let Some(tid) = crate::observability::current_trace_id() {
        tracing::Span::current().record("trace_id", tid.as_str());
    }
    if let Some(sid) = crate::observability::current_span_id() {
        tracing::Span::current().record("span_id", sid.as_str());
    }

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    // Ensure container is ready (sandbox) or skip (local)
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let mut found_skills = discover_skills(agent, sandbox);
    let mut plugin_commands = discover_plugin_commands(agent);
    let mut ws = load_workspace_files(agent, sandbox);
    let needs_onboarding = ws.needs_bootstrap();
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();
    let mut preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
    );

    // Setup TUI terminal
    tui::install_panic_hook();
    let mut terminal = tui::setup_terminal()?;

    // Create channel for agent -> TUI communication
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();

    let mut app = tui::ChatApp::new(agent_rx, debug, sandbox, Some(&state_dir));

    // Build command palette entries (built-in + plugins + skills)
    app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
    app.set_max_turns(agent.max_turns);

    // Show loaded skills in TUI
    if found_skills.is_empty() {
        app.push_system_message("No skills loaded.");
    } else {
        let names: Vec<&str> = found_skills.iter().map(|s| s.name.as_str()).collect();
        app.push_system_message(&format!("Skills: {}", names.join(", ")));
    }

    // Trigger onboarding if Soul.md and Identity.md are both absent (brand-new agent)
    if needs_onboarding {
        app.start_onboarding();
    }
    let mut event_reader = EventStream::new();

    let mut agent = agent.clone();
    let mut history: Vec<Message> = Vec::new();
    let mut agent_handle: Option<tokio::task::JoinHandle<Result<String>>> = None;
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(150));
    let mut stats = tui::UsageStats::new();
    // Per-turn tool call/result pairs collected for history reconstruction.
    // (call_id, tool_name, args_json) and (call_id, result_text)
    let mut turn_tool_calls: Vec<(String, String, String)> = Vec::new();
    let mut turn_tool_results: Vec<(String, String)> = Vec::new();
    // True when a graceful shutdown was requested and compaction is in-flight.
    let mut shutting_down = false;

    loop {
        // Render
        terminal.draw(|f| app.render(f))?;

        tokio::select! {
            // --- Periodic tick for spinner animation ---
            _ = tick_interval.tick() => {
                app.tick();
            }
            // --- Crossterm terminal events ---
            maybe_event = tui::next_crossterm_event(&mut event_reader) => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match app.handle_key(key) {
                            tui::KeyAction::Submit(text) => {
                                if app.is_onboarding() {
                                    // --- Onboarding: generate Soul.md + Identity.md from description ---
                                    app.push_user_message(&text);
                                    app.set_streaming();

                                    let tx = agent_tx.clone();
                                    let provider = agent.provider.clone();
                                    let model = agent.model.clone();
                                    let desc = text.clone();
                                    let parent_span = tracing::Span::current();

                                    agent_handle = Some(tokio::spawn(
                                        async move {
                                            match generate_soul_md(&provider, &model, &desc).await {
                                                Ok((soul_md, identity_md)) => {
                                                    let _ = tx.send(tui::TuiEvent::OnboardingDone {
                                                        soul_md,
                                                        identity_md,
                                                    });
                                                }
                                                Err(e) => {
                                                    let _ = tx.send(tui::TuiEvent::OnboardingError(
                                                        e.to_string(),
                                                    ));
                                                }
                                            }
                                            Ok(String::new())
                                        }
                                        .instrument(parent_span),
                                    ));
                                } else if text.starts_with('/') {
                                    // --- Slash command dispatch ---
                                    let parts: Vec<&str> = text.splitn(2, ' ').collect();
                                    let cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

                                    match cmd {
                                        "/model" => match arg {
                                            Some(name) => {
                                                agent.model = name.to_string();
                                                app.push_system_message(
                                                    &format!("Model changed to: {name}"),
                                                );
                                            }
                                            None => {
                                                let mut items = vec![tui::ModalItem::Header("Anthropic".into())];
                                                for (prov, model) in tui::MODEL_OPTIONS.iter().filter(|(p, _)| *p == "anthropic") {
                                                    items.push(tui::ModalItem::Option {
                                                        label: model.to_string(),
                                                        detail: prov.to_string(),
                                                        active: *model == agent.model,
                                                    });
                                                }
                                                items.push(tui::ModalItem::Separator);
                                                items.push(tui::ModalItem::Header("OpenAI".into()));
                                                for (prov, model) in tui::MODEL_OPTIONS.iter().filter(|(p, _)| *p == "openai") {
                                                    items.push(tui::ModalItem::Option {
                                                        label: model.to_string(),
                                                        detail: prov.to_string(),
                                                        active: *model == agent.model,
                                                    });
                                                }
                                                let modal = tui::Modal::new("Select Model".into(), tui::ModalKind::ModelSelect, items, true);
                                                app.open_modal(modal);
                                            }
                                        },
                                        "/resume" => {
                                            match arg {
                                                Some(prefix) => {
                                                    // Direct resume by prefix
                                                    match session_mgr.find_session_by_prefix(prefix) {
                                                        Ok(Some(resume_id)) => {
                                                            match session_mgr.read_transcript(&resume_id) {
                                                                Ok(entries) => {
                                                                    let new_history = rebuild_history(&entries);
                                                                    app.clear_messages();
                                                                    app.push_system_message(&format!(
                                                                        "Resumed session: {resume_id}"
                                                                    ));
                                                                    for entry in &entries {
                                                                        match &entry.event {
                                                                            TranscriptEvent::UserMessage { content } => {
                                                                                app.push_user_message(content);
                                                                            }
                                                                            TranscriptEvent::AssistantMessage { content } => {
                                                                                app.push_agent_message(content);
                                                                            }
                                                                            _ => {}
                                                                        }
                                                                    }
                                                                    history = new_history;
                                                                    session_id = resume_id;
                                                                }
                                                                Err(e) => {
                                                                    app.push_system_message(&format!(
                                                                        "Failed to resume: {e}"
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                        Ok(None) => {
                                                            app.push_system_message(&format!(
                                                                "No session matching prefix: {prefix}"
                                                            ));
                                                        }
                                                        Err(e) => {
                                                            app.push_system_message(&format!(
                                                                "Error searching sessions: {e}"
                                                            ));
                                                        }
                                                    }
                                                }
                                                None => {
                                                    // Show modal with session list
                                                    match session_mgr.session_summaries(20) {
                                                        Ok(summaries) if !summaries.is_empty() => {
                                                            let mut items = Vec::new();
                                                            for s in &summaries {
                                                                items.push(tui::ModalItem::Option {
                                                                    label: format!("{} ({})", s.timestamp, s.entry_count),
                                                                    detail: s.id.clone(),
                                                                    active: s.id == session_id,
                                                                });
                                                                items.push(tui::ModalItem::Text(
                                                                    format!("  {}", s.preview),
                                                                ));
                                                            }
                                                            let modal = tui::Modal::new(
                                                                "Resume Session".into(),
                                                                tui::ModalKind::SessionResume,
                                                                items,
                                                                true,
                                                            );
                                                            app.open_modal(modal);
                                                        }
                                                        Ok(_) => {
                                                            app.push_system_message("No sessions to resume.");
                                                        }
                                                        Err(e) => {
                                                            app.push_system_message(&format!(
                                                                "Error listing sessions: {e}"
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        "/usage" => {
                                            let cost = stats.estimated_cost(
                                                &agent.provider,
                                                &agent.model,
                                            );
                                            let cache_hit_rate = cache_hit_rate_percent(
                                                stats.input_tokens,
                                                stats.cached_input_tokens,
                                                stats.cache_write_tokens,
                                            );
                                            let mut items = vec![
                                                tui::ModalItem::Header("── Current Session ──".into()),
                                                tui::ModalItem::Text(format!("Input tokens:    {}", stats.input_tokens)),
                                                tui::ModalItem::Text(format!("Output tokens:   {}", stats.output_tokens)),
                                                tui::ModalItem::Text(format!("Cached tokens:   {}", stats.cached_input_tokens)),
                                                tui::ModalItem::Text(format!("Cache hit rate:  {:.2}%", cache_hit_rate)),
                                                tui::ModalItem::Text(format!("Tool calls:      {}", stats.tool_calls)),
                                                tui::ModalItem::Text(format!("Turns (ok/err):  {}/{}", stats.turns_success, stats.turns_error)),
                                                tui::ModalItem::Text(format!("Est. cost:       ${:.4}", cost)),
                                                tui::ModalItem::Text(format!("Model:           {} ({})", agent.model, agent.provider)),
                                            ];

                                            // Historical aggregation
                                            let now = Utc::now();
                                            let periods = [
                                                ("── Last 24 Hours ──", chrono::Duration::hours(24)),
                                                ("── Last 7 Days ──", chrono::Duration::days(7)),
                                                ("── Last 30 Days ──", chrono::Duration::days(30)),
                                            ];
                                            for (label, dur) in &periods {
                                                if let Ok(agg) = session_mgr.aggregate_usage(now - *dur) {
                                                    items.push(tui::ModalItem::Separator);
                                                    items.push(tui::ModalItem::Header(label.to_string()));
                                                    items.push(tui::ModalItem::Text(format!(
                                                        "Sessions: {} | Cost: ${:.2}",
                                                        agg.session_count, agg.estimated_cost
                                                    )));
                                                }
                                            }

                                            let modal = tui::Modal::new("Session Usage".into(), tui::ModalKind::Info, items, false);
                                            app.open_modal(modal);
                                        }
                                        "/skills" => {
                                            if found_skills.is_empty() {
                                                app.push_system_message("No skills found.");
                                            } else {
                                                let items: Vec<tui::ModalItem> = found_skills
                                                    .iter()
                                                    .map(|s| tui::ModalItem::Option {
                                                        label: s.name.clone(),
                                                        detail: s.description.clone(),
                                                        active: false,
                                                    })
                                                    .collect();
                                                let modal = tui::Modal::new(
                                                    "Skills".into(),
                                                    tui::ModalKind::SkillsList,
                                                    items,
                                                    true,
                                                );
                                                app.open_modal(modal);
                                            }
                                        }
                                        "/help" => {
                                            let mut items = vec![
                                                tui::ModalItem::Text("/model          — select or change model".into()),
                                                tui::ModalItem::Text("/model <name>   — change model directly".into()),
                                                tui::ModalItem::Text("/resume         — pick a session to resume".into()),
                                                tui::ModalItem::Text("/resume <prefix>— resume by session ID prefix".into()),
                                                tui::ModalItem::Text("/usage          — show usage stats (current + historical)".into()),
                                                tui::ModalItem::Text("/skills         — browse and manage skills".into()),
                                                tui::ModalItem::Text("/<skill>        — show skill content or send with context".into()),
                                            ];
                                            if !plugin_commands.is_empty() {
                                                items.push(tui::ModalItem::Separator);
                                                items.push(tui::ModalItem::Text("Plugin commands:".into()));
                                                for plugin_cmd in &plugin_commands {
                                                    items.push(tui::ModalItem::Text(format!(
                                                        "/{} — {}",
                                                        plugin_cmd.command, plugin_cmd.description
                                                    )));
                                                }
                                            }
                                            items.extend([
                                                tui::ModalItem::Text("/compact        — compact and save session to memory".into()),
                                                tui::ModalItem::Text("/stop           — stop the active run".into()),
                                                tui::ModalItem::Text("/help           — show this help".into()),
                                            ]);
                                            let modal = tui::Modal::new("Help".into(), tui::ModalKind::Info, items, false);
                                            app.open_modal(modal);
                                        }
                                        "/compact" => {
                                            if history.is_empty() {
                                                app.push_system_message("Nothing to compact — start a conversation first.");
                                            } else {
                                                app.push_system_message("Compacting session…");
                                                let hist_for_compact = history.clone();
                                                let container_for_compact = container.clone();
                                                let session_id_for_compact = session_id.clone();
                                                let provider_for_compact = agent.provider.clone();
                                                let model_for_compact = agent.model.clone();
                                                let name_for_compact = agent.name.clone();
                                                let tx_compact = agent_tx.clone();
                                                let parent_span = tracing::Span::current();
                                                tokio::spawn(
                                                    async move {
                                                        // LLM-generated summary of the conversation.
                                                        let summary = build_compact_summary(
                                                            &provider_for_compact,
                                                            &model_for_compact,
                                                            &name_for_compact,
                                                            sandbox,
                                                            &hist_for_compact,
                                                        )
                                                        .await;
                                                        match compact_session(
                                                            container_for_compact.as_deref(),
                                                            &session_id_for_compact,
                                                            &summary,
                                                        )
                                                        .await
                                                        {
                                                            Ok(msg) => {
                                                                let _ = tx_compact.send(tui::TuiEvent::CompactDone { message: msg, summary });
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_compact.send(tui::TuiEvent::CompactError(e.to_string()));
                                                            }
                                                        }
                                                    }
                                                    .instrument(parent_span),
                                                );
                                            }
                                        }
                                        "/stop" => {
                                            if let Some(handle) = agent_handle.take() {
                                                handle.abort();
                                                app.interrupt_run("Stopped current run.");
                                                turn_tool_calls.clear();
                                                turn_tool_results.clear();
                                            } else {
                                                app.push_system_message("No active run to stop.");
                                            }
                                        }
                                        _ => {
                                            let command_name = cmd.trim_start_matches('/');
                                            if let Some(plugin_cmd) =
                                                find_plugin_command(command_name, &plugin_commands)
                                            {
                                                let effective_task = render_plugin_command_task(
                                                    plugin_cmd,
                                                    arg.unwrap_or(""),
                                                );
                                                if effective_task.trim().is_empty() {
                                                    app.push_system_message(
                                                        "This plugin command requires arguments.",
                                                    );
                                                    continue;
                                                }

                                                app.record_input(&text);
                                                app.push_user_message(&text);
                                                app.set_streaming();

                                                let run_id = new_run_id();
                                                let _ = session_mgr.append(
                                                    &session_id,
                                                    &TranscriptEntry {
                                                        timestamp: Utc::now(),
                                                        run_id: run_id.clone(),
                                                        event: TranscriptEvent::UserMessage {
                                                            content: text.clone(),
                                                        },
                                                    },
                                                );

                                                let tx = agent_tx.clone();
                                                let pre = preamble.clone();
                                                let agent_clone = agent.clone();
                                                let hist = history.clone();
                                                let task_for_model = append_system_reminder(
                                                    &effective_task,
                                                    &session_id,
                                                    sandbox,
                                                    &agent.name,
                                                );
                                                let cont = container.clone();
                                                let session_id_for_trace = session_id.clone();
                                                let run_id_for_trace = run_id.clone();

                                                history.push(Message::user(&task_for_model));

                                                let tx_panic = tx.clone();
                                                let parent_span = tracing::Span::current();
                                                agent_handle = Some(tokio::spawn(
                                                    async move {
                                                        match std::panic::AssertUnwindSafe(
                                                            execute_agent_run_tui(
                                                                &agent_clone,
                                                                cont,
                                                                &pre,
                                                                &task_for_model,
                                                                hist,
                                                                tx,
                                                                Some(&session_id_for_trace),
                                                                Some(&run_id_for_trace),
                                                            ),
                                                        )
                                                        .catch_unwind()
                                                        .await
                                                        {
                                                            Ok(result) => result,
                                                            Err(payload) => {
                                                                let msg = payload
                                                                    .downcast_ref::<String>()
                                                                    .cloned()
                                                                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                                    .unwrap_or_else(|| "unknown panic".to_string());
                                                                let _ = tx_panic.send(tui::TuiEvent::Error(
                                                                    format!("Agent crashed: {msg}"),
                                                                ));
                                                                Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                            }
                                                        }
                                                    }
                                                    .instrument(parent_span),
                                                ));
                                            } else {
                                            // Check if it's a skill command (/<skill-name>)
                                            let skill_name = &cmd[1..]; // strip leading /
                                            if let Some(skill) = found_skills.iter().find(|s| s.name == skill_name) {
                                                let content = read_skill_content(skill);

                                                match (content, arg) {
                                                    (Some(content), Some(user_text)) => {
                                                        // Send message with skill context
                                                        let msg = format!(
                                                            "[Skill: {}]\n{}\n\n---\n\n{}",
                                                            skill.name, content, user_text
                                                        );
                                                        app.record_input(&text);
                                                        app.push_user_message(&text);
                                                        app.set_streaming();

                                                        let run_id = new_run_id();
                                                        let _ = session_mgr.append(
                                                            &session_id,
                                                            &TranscriptEntry {
                                                                timestamp: Utc::now(),
                                                                run_id: run_id.clone(),
                                                                event: TranscriptEvent::UserMessage {
                                                                    content: msg.clone(),
                                                                },
                                                            },
                                                        );

                                                        let tx = agent_tx.clone();
                                                        let pre = preamble.clone();
                                                        let agent_clone = agent.clone();
                                                        let hist = history.clone();
                                                        let task_for_model = append_system_reminder(&msg, &session_id, sandbox, &agent.name);
                                                        let cont = container.clone();
                                                        let session_id_for_trace = session_id.clone();
                                                        let run_id_for_trace = run_id.clone();

                                                        history.push(Message::user(&task_for_model));

                                                        let tx_panic = tx.clone();
                                                        let parent_span = tracing::Span::current();
                                                        agent_handle = Some(tokio::spawn(
                                                            async move {
                                                                match std::panic::AssertUnwindSafe(
                                                                    execute_agent_run_tui(
                                                                        &agent_clone,
                                                                        cont,
                                                                        &pre,
                                                                        &task_for_model,
                                                                        hist,
                                                                        tx,
                                                                        Some(&session_id_for_trace),
                                                                        Some(&run_id_for_trace),
                                                                    ),
                                                                )
                                                                .catch_unwind()
                                                                .await
                                                                {
                                                                    Ok(result) => result,
                                                                    Err(payload) => {
                                                                        let msg = payload
                                                                            .downcast_ref::<String>()
                                                                            .cloned()
                                                                            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                                            .unwrap_or_else(|| "unknown panic".to_string());
                                                                        let _ = tx_panic.send(tui::TuiEvent::Error(
                                                                            format!("Agent crashed: {msg}"),
                                                                        ));
                                                                        Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                                    }
                                                                }
                                                            }
                                                            .instrument(parent_span),
                                                        ));
                                                    }
                                                    (Some(content), None) => {
                                                        // No arg — show skill content in modal
                                                        let items: Vec<tui::ModalItem> = content
                                                            .lines()
                                                            .map(|l| tui::ModalItem::Text(l.to_string()))
                                                            .collect();
                                                        let modal = tui::Modal::new(
                                                            format!("Skill: {}", skill.name),
                                                            tui::ModalKind::SkillView,
                                                            items,
                                                            false,
                                                        );
                                                        app.open_modal(modal);
                                                    }
                                                    (None, _) => {
                                                        app.push_system_message(&format!(
                                                            "Failed to read skill: {}", skill.name
                                                        ));
                                                    }
                                                }
                                            } else {
                                                app.push_system_message(&format!(
                                                    "Unknown command: {cmd}. Type /help for available commands.",
                                                ));
                                            }
                                            }
                                        }
                                    }
                                } else {
                                    // --- Normal message: spawn agent ---
                                    app.record_input(&text);
                                    app.push_user_message(&text);
                                    app.set_streaming();

                                    // Record user message
                                    let run_id = new_run_id();
                                    let _ = session_mgr.append(
                                        &session_id,
                                        &TranscriptEntry {
                                            timestamp: Utc::now(),
                                            run_id: run_id.clone(),
                                            event: TranscriptEvent::UserMessage {
                                                content: text.clone(),
                                            },
                                        },
                                    );

                                    // Spawn agent task
                                    let tx = agent_tx.clone();
                                    let pre = preamble.clone();
                                    let agent_clone = agent.clone();
                                    let hist = history.clone();
                                    let task_text = append_system_reminder(&text, &session_id, sandbox, &agent.name);
                                    let cont = container.clone();
                                    let session_id_for_trace = session_id.clone();
                                    let run_id_for_trace = run_id.clone();

                                    history.push(Message::user(&task_text));

                                    let tx_panic = tx.clone();
                                    let parent_span = tracing::Span::current();
                                    agent_handle = Some(tokio::spawn(
                                        async move {
                                            match std::panic::AssertUnwindSafe(
                                                execute_agent_run_tui(
                                                    &agent_clone,
                                                    cont,
                                                    &pre,
                                                    &task_text,
                                                    hist,
                                                    tx,
                                                    Some(&session_id_for_trace),
                                                    Some(&run_id_for_trace),
                                                ),
                                            )
                                            .catch_unwind()
                                            .await
                                            {
                                                Ok(result) => result,
                                                Err(payload) => {
                                                    let msg = payload
                                                        .downcast_ref::<String>()
                                                        .cloned()
                                                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                        .unwrap_or_else(|| "unknown panic".to_string());
                                                    let _ = tx_panic.send(tui::TuiEvent::Error(
                                                        format!("Agent crashed: {msg}"),
                                                    ));
                                                    Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                }
                                            }
                                        }
                                        .instrument(parent_span),
                                    ));
                                }
                            }
                            tui::KeyAction::ModalSelect { kind, label, detail } => {
                                match kind {
                                    tui::ModalKind::ModelSelect => {
                                        agent.model = label.clone();
                                        agent.provider = detail.clone();
                                        app.push_system_message(&format!("Model changed to: {label} ({detail})"));
                                    }
                                    tui::ModalKind::SessionResume => {
                                        // detail contains the session ID
                                        let resume_id = detail.clone();
                                        match session_mgr.read_transcript(&resume_id) {
                                            Ok(entries) => {
                                                let new_history = rebuild_history(&entries);
                                                app.clear_messages();
                                                app.push_system_message(&format!(
                                                    "Resumed session: {resume_id}"
                                                ));
                                                // Replay user/assistant messages into chat
                                                for entry in &entries {
                                                    match &entry.event {
                                                        TranscriptEvent::UserMessage { content } => {
                                                            app.push_user_message(content);
                                                        }
                                                        TranscriptEvent::AssistantMessage { content } => {
                                                            app.push_agent_message(content);
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                history = new_history;
                                                session_id = resume_id;
                                            }
                                            Err(e) => {
                                                app.push_system_message(&format!(
                                                    "Failed to resume session: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    tui::ModalKind::SkillsList => {
                                        // label = skill name, detail = description
                                        let skill_name = label;
                                        let content = found_skills
                                            .iter()
                                            .find(|s| s.name == skill_name)
                                            .and_then(read_skill_content);

                                        match content {
                                            Some(content) => {
                                                let items: Vec<tui::ModalItem> = content
                                                    .lines()
                                                    .map(|l| tui::ModalItem::Text(l.to_string()))
                                                    .collect();
                                                let modal = tui::Modal::new(
                                                    format!("Skill: {}", skill_name),
                                                    tui::ModalKind::SkillView,
                                                    items,
                                                    false,
                                                );
                                                app.open_modal(modal);
                                            }
                                            None => {
                                                app.push_system_message(&format!(
                                                    "Failed to read skill: {}", skill_name
                                                ));
                                            }
                                        }
                                    }
                                    tui::ModalKind::SkillView | tui::ModalKind::Info => {}
                                }
                            }
                            tui::KeyAction::ModalDelete { kind, detail } => {
                                if kind == tui::ModalKind::SkillView {
                                    // Delete the skill
                                    if sandbox {
                                        app.push_system_message("Cannot delete skills in sandbox mode.");
                                    } else if let Some(dir) = skills::skills_dir_local(&agent.name) {
                                        match skills::delete_skill_local(&dir, &detail) {
                                            Ok(()) => {
                                                app.push_system_message(&format!(
                                                    "Deleted skill: {}", detail
                                                ));
                                                // Re-discover skills and update palette
                                                found_skills = discover_skills(&agent, sandbox);
                                                app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
                                                // Re-open skills list so the user sees the updated state
                                                if found_skills.is_empty() {
                                                    app.push_system_message("No skills remaining.");
                                                } else {
                                                    let items: Vec<tui::ModalItem> = found_skills
                                                        .iter()
                                                        .map(|s| tui::ModalItem::Option {
                                                            label: s.name.clone(),
                                                            detail: s.description.clone(),
                                                            active: false,
                                                        })
                                                        .collect();
                                                    let modal = tui::Modal::new(
                                                        "Skills".into(),
                                                        tui::ModalKind::SkillsList,
                                                        items,
                                                        true,
                                                    );
                                                    app.open_modal(modal);
                                                }
                                            }
                                            Err(e) => {
                                                app.push_system_message(&format!(
                                                    "Failed to delete skill: {}", e
                                                ));
                                            }
                                        }
                                    } else {
                                        app.push_system_message("Cannot determine skills directory.");
                                    }
                                }
                            }
                            tui::KeyAction::SubmitHumanAsk(response) => {
                                app.send_human_ask_response(response);
                            }
                            tui::KeyAction::InterruptRun => {
                                if let Some(handle) = agent_handle.take() {
                                    handle.abort();
                                    app.interrupt_run("Interrupted current run.");
                                    turn_tool_calls.clear();
                                    turn_tool_results.clear();
                                }
                            }
                            tui::KeyAction::Quit => {
                                if shutting_down || history.is_empty() {
                                    // Already compacting (Esc to skip) or nothing to compact.
                                    break;
                                }
                                // Graceful shutdown: compact the session before exiting.
                                shutting_down = true;
                                app.start_compaction_shutdown();
                                let hist_for_compact = history.clone();
                                let container_for_compact = container.clone();
                                let session_id_for_compact = session_id.clone();
                                let provider_for_compact = agent.provider.clone();
                                let model_for_compact = agent.model.clone();
                                let name_for_compact = agent.name.clone();
                                let tx_compact = agent_tx.clone();
                                let parent_span = tracing::Span::current();
                                tokio::spawn(
                                    async move {
                                        let summary = build_compact_summary(
                                            &provider_for_compact,
                                            &model_for_compact,
                                            &name_for_compact,
                                            sandbox,
                                            &hist_for_compact,
                                        )
                                        .await;
                                        match compact_session(
                                            container_for_compact.as_deref(),
                                            &session_id_for_compact,
                                            &summary,
                                        )
                                        .await
                                        {
                                            Ok(msg) => {
                                                let _ = tx_compact.send(tui::TuiEvent::CompactDone { message: msg, summary });
                                            }
                                            Err(e) => {
                                                let _ = tx_compact
                                                    .send(tui::TuiEvent::CompactError(e.to_string()));
                                            }
                                        }
                                    }
                                    .instrument(parent_span),
                                );
                            }
                            tui::KeyAction::None => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.handle_paste(&text);
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        app.handle_mouse(mouse);
                    }
                    Some(Err(_)) => {
                        // Transient crossterm read error — don't kill the TUI
                        continue;
                    }
                    None => {
                        // EventStream ended (stdin closed)
                        break;
                    }
                    _ => {}
                }
            }

            // --- Agent events ---
            maybe_agent_event = app.recv_agent_event() => {
                if let Some(event) = maybe_agent_event {
                    let is_done = matches!(&event, tui::TuiEvent::Done { .. });
                    let is_error = matches!(&event, tui::TuiEvent::Error(_));
                    let is_tool_call = matches!(&event, tui::TuiEvent::ToolCall { .. });

                    // Capture onboarding result before event is consumed
                    let onboarding_inner = match &event {
                        tui::TuiEvent::OnboardingDone { soul_md, identity_md } => {
                            Some((soul_md.clone(), identity_md.clone()))
                        }
                        _ => None,
                    };
                    let is_onboarding_terminal = matches!(
                        &event,
                        tui::TuiEvent::OnboardingDone { .. } | tui::TuiEvent::OnboardingError(_)
                    );

                    // Capture usage data before passing event to app
                    let done_data = match &event {
                        tui::TuiEvent::Done { text, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens } => {
                            Some((text.clone(), *input_tokens, *output_tokens, *cached_input_tokens, *cache_write_tokens))
                        }
                        _ => None,
                    };

                    // Accumulate tool call stats and collect tool data for history
                    if is_tool_call {
                        stats.add_tool_call();
                    }
                    match &event {
                        tui::TuiEvent::ToolCall { call_id, name, args } => {
                            turn_tool_calls.push((call_id.clone(), name.clone(), args.clone()));
                        }
                        tui::TuiEvent::ToolResult { call_id, result, .. } => {
                            turn_tool_results.push((call_id.clone(), result.clone()));
                        }
                        _ => {}
                    }

                    // Check for compaction completion before consuming event
                    let is_compact_terminal = matches!(
                        &event,
                        tui::TuiEvent::CompactDone { .. } | tui::TuiEvent::CompactError(_)
                    );
                    // Extract summary from CompactDone to write transcript marker + reset history.
                    let compact_summary = match &event {
                        tui::TuiEvent::CompactDone { summary, .. } => Some(summary.clone()),
                        _ => None,
                    };

                    app.handle_agent_event(event);

                    // On successful compaction, write transcript marker and reset history.
                    if let Some(summary) = compact_summary {
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: new_run_id(),
                                event: TranscriptEvent::Compaction {
                                    summary: summary.clone(),
                                },
                            },
                        );
                        history = vec![
                            Message::user(format!(
                                "[Conversation context summary: {summary}]"
                            )),
                            Message::assistant(
                                "Understood, I have the context from our previous conversation.".to_string(),
                            ),
                        ];
                    }

                    // If compaction finished, exit (graceful shutdown) or stay (manual /compact)
                    if is_compact_terminal && shutting_down {
                        break;
                    }

                    // Handle onboarding completion: save Soul.md + Identity.md and rebuild preamble
                    if is_onboarding_terminal {
                        agent_handle = None;
                        if let Some((soul_md, identity_md)) = onboarding_inner {
                            if sandbox {
                                let container_name = SandboxClient::container_name(&agent);
                                if let Err(e) = workspace::save_soul_sandbox(&container_name, &agent.name, &soul_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Soul.md to container: {e}"
                                    ));
                                }
                                if let Err(e) = workspace::save_identity_sandbox(&container_name, &agent.name, &identity_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Identity.md to container: {e}"
                                    ));
                                }
                            } else {
                                if let Err(e) = workspace::save_soul_local(&agent.name, &soul_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Soul.md: {e}"
                                    ));
                                }
                                if let Err(e) = workspace::save_identity_local(&agent.name, &identity_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Identity.md: {e}"
                                    ));
                                }
                            }
                            ws = load_workspace_files(&agent, sandbox);
                            preamble = build_preamble(
                                &agent_workspace,
                                &agent,
                                sandbox,
                                &found_skills,
                                &ws,
                                history.len(),
                                &session_id,
                                &session_mgr.session_summaries(5).unwrap_or_default(),
                            );
                        }
                    }

                    if is_done {
                        // Re-discover skills and plugins after each turn — the agent may have changed them.
                        let prev_skill_count = found_skills.len();
                        let prev_plugin_count = plugin_commands.len();
                        found_skills = discover_skills(&agent, sandbox);
                        plugin_commands = discover_plugin_commands(&agent);
                        app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
                        if found_skills.len() != prev_skill_count {
                            let names: Vec<&str> = found_skills.iter().map(|s| s.name.as_str()).collect();
                            app.push_system_message(&format!(
                                "Skills updated: {} loaded ({})",
                                found_skills.len(),
                                names.join(", ")
                            ));
                        }
                        if plugin_commands.len() != prev_plugin_count {
                            let names: Vec<&str> =
                                plugin_commands.iter().map(|c| c.command.as_str()).collect();
                            app.push_system_message(&format!(
                                "Plugin commands updated: {} available ({})",
                                plugin_commands.len(),
                                names.join(", ")
                            ));
                        }

                        // Accumulate usage stats and write usage event to transcript
                        if let Some((_, input, output, cached, cache_write)) = &done_data {
                            stats.add_usage(*input, *output, *cached, *cache_write);
                            stats.record_success();
                            let _ = session_mgr.append(
                                &session_id,
                                &TranscriptEntry {
                                    timestamp: Utc::now(),
                                    run_id: new_run_id(),
                                    event: TranscriptEvent::Usage {
                                        input_tokens: *input,
                                        output_tokens: *output,
                                        cached_input_tokens: *cached,
                                        tool_calls: stats.tool_calls,
                                        model: agent.model.clone(),
                                        provider: agent.provider.clone(),
                                    },
                                },
                            );
                        }

                        // Insert tool call/result pairs collected during this turn.
                        {
                            let result_map: std::collections::HashMap<String, String> =
                                turn_tool_results.drain(..).collect();
                            let collected: Vec<(String, String, String)> =
                                turn_tool_calls.drain(..).collect();
                            if !collected.is_empty() {
                                // Group all tool calls in a single assistant message.
                                history.push(Message::Assistant {
                                    content: String::new(),
                                    tool_calls: collected.iter().map(|(call_id, name, args_json)| {
                                        ToolCall {
                                            call_id: call_id.clone(),
                                            name: name.clone(),
                                            args_json: args_json.clone(),
                                        }
                                    }).collect(),
                                });
                                // Then emit each tool result.
                                for (call_id, name, _args) in &collected {
                                    if let Some(result) = result_map.get(call_id) {
                                        history.push(Message::Tool {
                                            call_id: call_id.clone(),
                                            name: name.clone(),
                                            content: result.clone(),
                                        });
                                    }
                                }
                            }
                        }

                        if let Some(handle) = agent_handle.take() {
                            // Get the result from the spawned task
                            if let Ok(Ok(text)) = handle.await {
                                history.push(Message::assistant(&text));

                                let run_id = new_run_id();
                                let _ = session_mgr.append(
                                    &session_id,
                                    &TranscriptEntry {
                                        timestamp: Utc::now(),
                                        run_id,
                                        event: TranscriptEvent::AssistantMessage {
                                            content: text,
                                        },
                                    },
                                );
                            }
                        } else if let Some((text, _, _, _, _)) = done_data {
                            // Fallback: use the text from the Done event
                            if !text.is_empty() {
                                history.push(Message::assistant(&text));
                            }
                        }
                    } else if is_error {
                        stats.record_error();
                        agent_handle = None;
                        turn_tool_calls.clear();
                        turn_tool_results.clear();
                    }
                }
            }
        }
    }

    // Abort any in-flight agent task so it doesn't outlive the TUI.
    if let Some(handle) = agent_handle.take() {
        handle.abort();
    }

    tui::restore_terminal(&mut terminal)?;

    // Keep sandbox container by default so any in-container services remain
    // available after chat exits. Set THAT_SANDBOX_REMOVE_ON_EXIT=1 to restore
    // the old cleanup behavior.
    let remove_on_exit = std::env::var("THAT_SANDBOX_REMOVE_ON_EXIT")
        .ok()
        .map(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false);
    if sandbox && remove_on_exit {
        SandboxClient::remove(&agent);
    }

    Ok(())
}

/// Build and execute a single agent run, sending events to the TUI via channel.
/// Retries automatically on transient network errors with exponential backoff,
/// notifying the TUI before each retry so it can clear partial streaming state.
#[tracing::instrument(name = "agent_run", skip_all, fields(
    gen_ai.provider      = %agent.provider,
    gen_ai.provider.name = %agent.provider,
    gen_ai.request.model = %agent.model,
    gen_ai.prompt        = tracing::field::Empty,
    gen_ai.completion    = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    otel.status_code = tracing::field::Empty,
    otel.status_description = tracing::field::Empty,
    session.id = tracing::field::Empty,
    run.id = tracing::field::Empty,
    agent.name = %agent.name,
    input.value = tracing::field::Empty,
    input.mime_type = "text/plain",
    output.value = tracing::field::Empty,
    output.mime_type = "text/plain",
))]
pub async fn execute_agent_run_tui(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    history: Vec<Message>,
    tui_tx: mpsc::UnboundedSender<tui::TuiEvent>,
    session_id_for_trace: Option<&str>,
    run_id_for_trace: Option<&str>,
) -> Result<String> {
    if let Some(sid) = session_id_for_trace {
        tracing::Span::current().record("session.id", sid);
    }
    if let Some(rid) = run_id_for_trace {
        tracing::Span::current().record("run.id", rid);
    }
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let task_for_model = append_memory_bootstrap_reminder(task, history.len());
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            let delay_ms = RETRY_BASE_DELAY_MS << (attempt - 1).min(4);
            let _ = tui_tx.send(tui::TuiEvent::Retrying {
                attempt,
                max_attempts: MAX_NETWORK_RETRIES,
                delay_secs: delay_ms / 1_000,
            });
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        let api_key = match api_key_for_provider(&agent.provider) {
            Ok(k) => k,
            Err(e) => {
                let _ = tui_tx.send(tui::TuiEvent::Error(format!("{e:#}")));
                return Err(e);
            }
        };
        let skill_roots = resolved_skill_roots(agent);
        let tools_config = load_agent_config(&container, agent);
        let hook = tui::TuiHook::new(tui_tx.clone());
        let config = LoopConfig {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            api_key,
            system: preamble.to_string(),
            max_tokens: agent.max_tokens as u32,
            max_turns: agent.max_turns as u32,
            tools: all_tool_defs(&container),
            history: history.clone(),
            prompt_caching: matches!(agent.provider.as_str(), "anthropic" | "openrouter"),
            openai_websocket: openai_websocket_enabled(),
            debug: false,
            tool_ctx: ToolContext {
                config: tools_config,
                container: container.clone(),
                skill_roots,
            },
        };
        let result = agent_loop::run(&config, &task_for_model, &hook).await;

        match result {
            Ok((text, usage)) => {
                log_prompt_cache_usage(
                    &agent.provider,
                    &agent.model,
                    usage.input_tokens as u64,
                    usage.cache_read_tokens as u64,
                    usage.cache_write_tokens as u64,
                );
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                // Ensure the current run is exported promptly for live trace UIs.
                crate::observability::flush_tracing();
                let _ = tui_tx.send(tui::TuiEvent::Done {
                    text: text.clone(),
                    input_tokens: usage.input_tokens as u64,
                    output_tokens: usage.output_tokens as u64,
                    cached_input_tokens: usage.cache_read_tokens as u64,
                    cache_write_tokens: usage.cache_write_tokens as u64,
                });
                return Ok(text);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < MAX_NETWORK_RETRIES {
                    attempt += 1;
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                crate::observability::flush_tracing();
                let _ = tui_tx.send(tui::TuiEvent::Error(format!("{e:#}")));
                return Err(e);
            }
        }
    }
}

/// Build and execute a single agent run using a [`that_channels::ChannelRouter`].
///
/// This is the generic multi-channel equivalent of [`execute_agent_run_tui`].
/// All agent events are routed through the `ChannelRouter` to every active channel
/// (TUI, Telegram, Discord, WhatsApp, …) concurrently.
///
/// Formatting instructions from all channels are appended to the preamble so
/// the agent knows how to format messages for each active platform.
///
/// Retries automatically on transient network errors with exponential backoff,
/// notifying all channels before each retry attempt.
///
/// # Policy note
/// Always calls `load_agent_config(&container, agent)` — required on every execution path.
#[tracing::instrument(name = "agent_run", skip_all, fields(
    gen_ai.provider      = %agent.provider,
    gen_ai.provider.name = %agent.provider,
    gen_ai.request.model = %agent.model,
    gen_ai.prompt        = tracing::field::Empty,
    gen_ai.completion    = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    otel.status_code = tracing::field::Empty,
    otel.status_description = tracing::field::Empty,
    session.id = tracing::field::Empty,
    run.id = tracing::field::Empty,
    agent.name = %agent.name,
    input.value = tracing::field::Empty,
    input.mime_type = "text/plain",
    output.value = tracing::field::Empty,
    output.mime_type = "text/plain",
    trace_id = tracing::field::Empty,
    span_id = tracing::field::Empty,
))]
pub async fn execute_agent_run_channel(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    history: Vec<Message>,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    route_channel_id: Option<String>,
    route_target: Option<that_channels::OutboundTarget>,
    // When true, streaming tokens and the Done event are not routed to any
    // channel. The agent can still use the `channel_notify` tool for deliberate
    // outbound messages. Used for internal background runs (heartbeat, etc.)
    // to prevent the full agent response from being broadcast automatically.
    suppress_output: bool,
    session_id_for_trace: Option<&str>,
    run_id_for_trace: Option<&str>,
) -> Result<(String, Vec<that_channels::ToolLogEvent>)> {
    if let Some(sid) = session_id_for_trace {
        tracing::Span::current().record("session.id", sid);
    }
    if let Some(rid) = run_id_for_trace {
        tracing::Span::current().record("run.id", rid);
    }
    if let Some(tid) = crate::observability::current_trace_id() {
        tracing::Span::current().record("trace_id", tid.as_str());
    }
    if let Some(sid) = crate::observability::current_span_id() {
        tracing::Span::current().record("span_id", sid.as_str());
    }
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let task_for_model = append_memory_bootstrap_reminder(task, history.len());
    let mut attempt = 0u32;
    let mut empty_response_retries = 0u32;

    // Append channel formatting instructions to the preamble.
    // For scoped runs, prefer only the active channel's guidance to avoid
    // conflicting markdown/rendering rules across adapters.
    let active_channel = route_channel_id
        .as_deref()
        .unwrap_or_else(|| router.primary_id());
    let format_section = if let Some(cid) = route_channel_id.as_deref() {
        let scoped = router.format_instructions_for(cid);
        if scoped.is_empty() {
            router.combined_format_instructions()
        } else {
            scoped
        }
    } else {
        router.combined_format_instructions()
    };
    // Channel config lives in the agent's own TOML file.
    // In sandbox mode ~/.that-agent is mounted at /home/agent/.that-agent,
    // so use the container-visible path so fs_cat/fs_write work correctly.
    let config_path = if container.is_some() {
        format!("/home/agent/.that-agent/agents/{}.toml", agent.name)
    } else {
        dirs::home_dir()
            .map(|h| {
                h.join(".that-agent")
                    .join("agents")
                    .join(format!("{}.toml", agent.name))
            })
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("~/.that-agent/agents/{}.toml", agent.name))
    };

    let channel_info = format!(
        "## Active Channels\n\n\
         You are communicating through the following channels: {ids}\n\
         Active route for this response: {active}\n\
         Primary channel (used for interactive human_ask): {primary}\n\
         Channel env vars available at runtime:\n\
         - `THAT_CHANNEL_IDS={ids}`\n\
         - `THAT_CHANNEL_PRIMARY={primary}`\n\
         - `THAT_CONFIG_PATH={config_path}` — this agent's channel and adapter configuration file",
        ids = router.channel_ids(),
        active = active_channel,
        primary = router.primary_id(),
        config_path = config_path,
    );
    let channel_output_contract = format!(
        "## Channel Output Contract\n\n\
         - Produce one user-ready final message for this turn.\n\
         - Follow formatting instructions for the active route (`{active}`) only.\n\
         - Before finalizing, self-check channel syntax: no broken markdown, no unmatched code fences, and no stray escape artifacts (for example `\\(`, `\\-`, `\\:`) in user-visible text.\n\
         - If channel-specific formatting is uncertain, prefer plain readable text over complex markdown.\n\
         - Do not mention internal tools, prompt rules, or hidden reasoning.\n\
         - Keep the response concise, legible on mobile, and directly actionable.",
        active = active_channel,
    );
    let full_preamble = if format_section.is_empty() {
        format!("{preamble}\n\n{channel_info}\n\n{channel_output_contract}")
    } else {
        format!("{preamble}\n\n{channel_info}\n\n{channel_output_contract}\n\n{format_section}")
    };

    loop {
        if attempt > 0 {
            let delay_ms = RETRY_BASE_DELAY_MS << (attempt - 1).min(4);
            let event = that_channels::ChannelEvent::Retrying {
                attempt,
                max_attempts: MAX_NETWORK_RETRIES,
                delay_secs: delay_ms / 1_000,
            };
            route_channel_event(
                router.as_ref(),
                route_channel_id.as_deref(),
                route_target.as_ref(),
                &event,
            )
            .await;
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        // Per-attempt log channel: collects all tool call/result events for the session transcript.
        // A fresh channel is created each attempt so retried runs don't mix events.
        let (log_tx, mut log_rx) =
            tokio::sync::mpsc::unbounded_channel::<that_channels::ToolLogEvent>();
        let hook = if suppress_output {
            ChannelHook::silent(std::sync::Arc::clone(&router), Some(log_tx))
        } else if let Some(cid) = route_channel_id.as_deref() {
            ChannelHook::scoped(
                std::sync::Arc::clone(&router),
                cid.to_string(),
                route_target.clone(),
                Some(log_tx),
            )
        } else {
            ChannelHook::new(std::sync::Arc::clone(&router), Some(log_tx))
        };
        let skill_roots = resolved_skill_roots(agent);
        let tools_config = load_agent_config(&container, agent);

        // Add channel-specific tools: ChannelHook intercepts calls and routes them
        // to the router without hitting dispatch().
        let mut tools = all_tool_defs(&container);
        tools.push(channel_notify_tool_def());
        tools.push(channel_send_file_tool_def());

        let api_key = match api_key_for_provider(&agent.provider) {
            Ok(k) => k,
            Err(e) => {
                let event = that_channels::ChannelEvent::Error(format!("{e:#}"));
                route_channel_event(
                    router.as_ref(),
                    route_channel_id.as_deref(),
                    route_target.as_ref(),
                    &event,
                )
                .await;
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                crate::observability::flush_tracing();
                return Err(e);
            }
        };
        let config = LoopConfig {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            api_key,
            system: full_preamble.clone(),
            max_tokens: agent.max_tokens as u32,
            max_turns: agent.max_turns as u32,
            tools,
            history: history.clone(),
            prompt_caching: matches!(agent.provider.as_str(), "anthropic" | "openrouter"),
            openai_websocket: if agent.provider == "openai" && empty_response_retries > 0 {
                false
            } else {
                openai_websocket_enabled()
            },
            debug: false,
            tool_ctx: ToolContext {
                config: tools_config,
                container: container.clone(),
                skill_roots,
            },
        };
        let task_for_attempt = if empty_response_retries > 0 {
            build_empty_channel_retry_task(&task_for_model)
        } else {
            task_for_model.clone()
        };
        let result = agent_loop::run(&config, &task_for_attempt, &hook).await;

        // Drop hook to flush log_tx sender side, then drain events.
        drop(hook);
        let mut tool_events: Vec<that_channels::ToolLogEvent> = Vec::new();
        while let Ok(ev) = log_rx.try_recv() {
            tool_events.push(ev);
        }

        match result {
            Ok((text, usage)) => {
                log_prompt_cache_usage(
                    &agent.provider,
                    &agent.model,
                    usage.input_tokens as u64,
                    usage.cache_read_tokens as u64,
                    usage.cache_write_tokens as u64,
                );
                if should_retry_empty_channel_response(
                    &text,
                    suppress_output,
                    &tool_events,
                    empty_response_retries,
                ) {
                    empty_response_retries += 1;
                    warn!(
                        agent = %agent.name,
                        channel = ?route_channel_id,
                        retry = empty_response_retries,
                        provider = %agent.provider,
                        openai_websocket = config.openai_websocket,
                        "Model returned empty channel response; retrying once with explicit nudge"
                    );
                    continue;
                }
                let text = if should_use_channel_empty_response_fallback(
                    &text,
                    suppress_output,
                    &tool_events,
                ) {
                    warn!(
                        agent = %agent.name,
                        channel = ?route_channel_id,
                        "Model returned empty channel response; using fallback text"
                    );
                    build_empty_channel_response_fallback(&tool_events)
                } else {
                    text
                };
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                crate::observability::flush_tracing();
                if !suppress_output {
                    let event = that_channels::ChannelEvent::Done {
                        text: text.clone(),
                        input_tokens: usage.input_tokens as u64,
                        output_tokens: usage.output_tokens as u64,
                        cached_input_tokens: usage.cache_read_tokens as u64,
                        cache_write_tokens: usage.cache_write_tokens as u64,
                    };
                    route_channel_event(
                        router.as_ref(),
                        route_channel_id.as_deref(),
                        route_target.as_ref(),
                        &event,
                    )
                    .await;
                }
                return Ok((text, tool_events));
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < MAX_NETWORK_RETRIES {
                    attempt += 1;
                    // tool_events from this failed attempt are discarded on retry.
                    continue;
                }
                let event = that_channels::ChannelEvent::Error(format!("{e:#}"));
                route_channel_event(
                    router.as_ref(),
                    route_channel_id.as_deref(),
                    route_target.as_ref(),
                    &event,
                )
                .await;
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                crate::observability::flush_tracing();
                return Err(e);
            }
        }
    }
}

/// Handle session management commands.
pub fn handle_session_command(ws: &WorkspaceConfig, command: SessionCommands) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    match command {
        SessionCommands::List => {
            let sessions = session_mgr.list_sessions()?;
            if sessions.is_empty() {
                println!("No sessions found.");
            } else {
                println!("Sessions (newest first):");
                for id in sessions {
                    println!("  {id}");
                }
            }
        }
        SessionCommands::Show { id } => {
            let entries = session_mgr.read_transcript(&id)?;
            for entry in entries {
                let ts = entry.timestamp.format("%H:%M:%S");
                match &entry.event {
                    TranscriptEvent::RunStart { task } => {
                        println!("\n[{ts}] ── RUN START ──────────────────────");
                        println!("{task}");
                    }
                    TranscriptEvent::UserMessage { content } => {
                        println!("\n[{ts}] USER");
                        println!("{content}");
                    }
                    TranscriptEvent::AssistantMessage { content } => {
                        println!("\n[{ts}] AGENT");
                        println!("{content}");
                    }
                    TranscriptEvent::ToolCall { tool, arguments } => {
                        let args_str = serde_json::to_string(arguments).unwrap_or_default();
                        let args_preview: String = args_str.chars().take(300).collect();
                        let ellipsis = if args_str.chars().count() > 300 {
                            "…"
                        } else {
                            ""
                        };
                        println!("\n[{ts}] TOOL CALL  {tool}");
                        println!("{args_preview}{ellipsis}");
                    }
                    TranscriptEvent::ToolResult {
                        tool,
                        result,
                        is_error,
                    } => {
                        let prefix = if *is_error { "ERROR" } else { "OK" };
                        let preview: String = result.chars().take(500).collect();
                        let ellipsis = if result.chars().count() > 500 {
                            "…"
                        } else {
                            ""
                        };
                        println!("\n[{ts}] TOOL RESULT  {tool} [{prefix}]");
                        println!("{preview}{ellipsis}");
                    }
                    TranscriptEvent::RunEnd { status, error } => {
                        let msg = error.as_deref().unwrap_or("");
                        println!("\n[{ts}] ── RUN END: {status:?} {msg}");
                    }
                    TranscriptEvent::Compaction { summary } => {
                        println!("\n[{ts}] COMPACTION: {summary}");
                    }
                    TranscriptEvent::Usage {
                        input_tokens,
                        output_tokens,
                        model,
                        provider,
                        ..
                    } => {
                        println!(
                            "\n[{ts}] USAGE  {input_tokens}↑ {output_tokens}↓  {model}@{provider}"
                        );
                    }
                }
            }
        }
        SessionCommands::New => {
            let id = session_mgr.create_session()?;
            println!("Created session: {id}");
        }
    }

    Ok(())
}

/// Handle agent management commands.
pub fn handle_agent_command(ws: &WorkspaceConfig, command: AgentCommands) -> Result<()> {
    match command {
        AgentCommands::List => {
            let agents = ws.list_agents()?;
            if agents.is_empty() {
                println!(
                    "No agents found. Run 'that agent init <name> --api-key <KEY>' to create one."
                );
            } else {
                println!("Available agents:");
                for name in &agents {
                    let marker = if name == &ws.default_agent { " *" } else { "" };
                    println!("  {name}{marker}");
                }
            }
        }
        AgentCommands::Show { name } => {
            let agent = ws.load_agent(&name)?;
            let toml_str = toml::to_string_pretty(&agent)?;
            println!("# Agent: {name}");
            println!("{toml_str}");
        }
        AgentCommands::Delete { name } => {
            let agents_dir = ws.agents_dir();
            let preferred_dir = agents_dir.join(&name);
            let preferred_path = preferred_dir.join("config.toml");
            let legacy_path = agents_dir.join(format!("{name}.toml"));

            let mut removed_any = false;
            if preferred_dir.exists() {
                std::fs::remove_dir_all(&preferred_dir)?;
                println!("Removed agent directory: {}", preferred_dir.display());
                removed_any = true;
            } else if preferred_path.exists() {
                std::fs::remove_file(&preferred_path)?;
                println!("Removed agent config: {}", preferred_path.display());
                removed_any = true;
            }
            if legacy_path.exists() {
                std::fs::remove_file(&legacy_path)?;
                println!("Removed legacy agent config: {}", legacy_path.display());
                removed_any = true;
            }
            if !removed_any {
                anyhow::bail!("Agent '{name}' not found in {}", agents_dir.display());
            }

            // Remove isolated workspace if it exists
            let workspace_dir = AgentDef::agent_workspace_dir(&name);
            if workspace_dir.exists() {
                std::fs::remove_dir_all(&workspace_dir)?;
                println!("Removed agent workspace: {}", workspace_dir.display());
            }

            // Stop and remove the sandbox container and its home volume
            let dummy_agent = AgentDef {
                name: name.clone(),
                ..Default::default()
            };
            SandboxClient::remove(&dummy_agent);
            SandboxClient::remove_home_volume(&dummy_agent);

            println!("Agent '{name}' deleted.");
        }
    }

    Ok(())
}

/// Handle skill management commands.
pub fn handle_skill_command(agent: &AgentDef, sandbox: bool, command: SkillCommands) -> Result<()> {
    // Keep bundled bootstrap skills (including that-plugins) in sync for
    // direct skill CLI calls such as `skill show`.
    default_skills::install_default_skills(&agent.name);
    install_that_tools_skills_local(&agent.name);
    let found = discover_skills(agent, sandbox);

    match command {
        SkillCommands::List => {
            if found.is_empty() {
                println!("No skills found.");
            } else {
                println!("Available skills:");
                for skill in &found {
                    println!("  {} — {}", skill.name, skill.description);
                }
            }
        }
        SkillCommands::Show { name } => {
            let content = found
                .iter()
                .find(|s| s.name == name.as_str())
                .and_then(|s| std::fs::read_to_string(&s.path).ok());

            match content {
                Some(text) => println!("{text}"),
                None => anyhow::bail!("Skill '{name}' not found."),
            }
        }
    }

    Ok(())
}

/// Initialize workspace configuration.
pub fn init_workspace(
    ws: &WorkspaceConfig,
    agent_name: &str,
    force: bool,
    shared_workspace: bool,
    provider: &str,
    model: &str,
    max_turns: usize,
) -> Result<()> {
    let agents_dir = ws.agents_dir();
    let agent_dir = agents_dir.join(agent_name);
    let preferred_path = agent_dir.join("config.toml");
    let legacy_path = agents_dir.join(format!("{agent_name}.toml"));

    if (preferred_path.exists() || legacy_path.exists()) && !force {
        anyhow::bail!(
            "Agent '{agent_name}' already exists ({}). Use --force to overwrite.",
            preferred_path.display()
        );
    }

    std::fs::create_dir_all(&agent_dir)?;

    // Write agent definition
    let agent_def = AgentDef {
        provider: provider.to_string(),
        model: model.to_string(),
        max_turns,
        shared_workspace,
        ..AgentDef::default()
    };
    let agent_toml = toml::to_string_pretty(&agent_def)?;
    std::fs::write(&preferred_path, agent_toml)?;
    if legacy_path.exists() {
        let _ = std::fs::remove_file(&legacy_path);
    }

    println!(
        "Initialized agent '{agent_name}' at {}",
        preferred_path.display()
    );

    if let Ok(plugins_dir) = that_plugins::ensure_agent_plugins_dir(agent_name) {
        println!("Initialized plugin directory at {}", plugins_dir.display());
    }

    let mut memory_cfg = that_tools::config::MemoryConfig::default();
    memory_cfg.db_path = AgentDef::agent_memory_db_path(agent_name)
        .display()
        .to_string();
    match that_tools::tools::memory::ensure_initialized(&memory_cfg) {
        Ok(path) => {
            println!("Initialized memory database at {}", path.display());
        }
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                path = %memory_cfg.db_path,
                error = %err,
                "Failed to initialize memory database during init"
            );
        }
    }

    match heartbeat::ensure_heartbeat_local(agent_name) {
        Ok(true) => {
            if let Some(path) = heartbeat::heartbeat_md_path_local(agent_name) {
                println!("Initialized heartbeat file at {}", path.display());
            }
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                error = %err,
                "Failed to initialize Heartbeat.md during init"
            );
        }
    }

    match workspace::ensure_bashrc_local(agent_name) {
        Ok(path) => {
            println!("Initialized shell profile at {}", path.display());
        }
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                error = %err,
                "Failed to initialize .bashrc during init"
            );
        }
    }

    Ok(())
}

/// Discover skills for the current mode (sandbox or local).
pub fn discover_skills(agent: &AgentDef, _sandbox: bool) -> Vec<skills::SkillMeta> {
    // Always read from the host. In sandbox mode the host ~/.that-agent is
    // bind-mounted into the container, so the two directories are the same
    // filesystem — reading locally is both faster and available before the
    // container is started.
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    for err in &plugin_registry.load_errors {
        tracing::warn!(agent = %agent.name, error = %err, "Plugin load warning");
    }
    let roots = skill_roots_for_agent(agent, &plugin_registry);
    let mut skills_found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        for skill in skills::discover_skills_local(&root) {
            if seen.insert(skill.name.clone()) {
                skills_found.push(skill);
            }
        }
    }
    skills_found.sort_by(|a, b| a.name.cmp(&b.name));
    skills_found
}

/// Call the LLM to distill a user's free-form description into Soul.md + Identity.md.
///
/// Generates a combined document with Identity sections first (Name, What I Am,
/// Vibe, Emoji) followed by Soul sections (Character onward). The caller splits
/// at `## Character` to produce the two separate files.
///
/// Returns `(identity_md, soul_md)`.
pub async fn generate_soul_md(
    provider: &str,
    model: &str,
    description: &str,
) -> Result<(String, String)> {
    const SYSTEM: &str = "\
You are a character writer for autonomous AI agents. \
Given a rough description, you distill it into two well-formed identity files. \
You interpret, refine, and give shape to the description — not transcribe it verbatim. \
The result should feel like a specific, coherent entity — not a generic agent.\n\
\n\
Output exactly these sections in order:\n\
\n\
--- IDENTITY SECTIONS (shallow, surface) ---\n\
1. '## Name' — a short, memorable name derived from the description if not explicit.\n\
2. '## What I Am' — one honest sentence on the nature of this entity at its core.\n\
3. '## Vibe' — 2-3 words capturing the felt texture of this agent's presence.\n\
4. '## Emoji' — a single emoji that captures the essence.\n\
\n\
--- SOUL SECTIONS (deep, persistent) ---\n\
5. '## Character' — 4-6 bullet points capturing personality, values, and way of working.\n\
6. '## Worldview' — 3-5 beliefs that ground the character. The underlying WHY.\n\
7. '## Behavioral Philosophy' — 2-4 sentences on how the agent approaches problems.\n\
8. '## Epistemic Approach' — exactly 4 sub-entries: 'On uncertainty:', 'On being wrong:', \
   'On conviction:', 'On the unknown:'. Each 1-2 sentences.\n\
9. '## Behavioral Intents' — 5-8 terse, specific micro-rules from the character. \
   Concrete nudges for edge cases. Must feel like THIS agent, not generic advice.\n\
10. '## Relational Stance' — exactly 4 sub-entries: 'Default:', 'On disagreement:', \
    'On asking for help:', 'On trust:'.\n\
11. '## Situational Judgment' — 4 bullets: when to act, when to ask, when to stop, \
    and when to be brief versus thorough.\n\
12. '## Failure Modes' — 2-3 bullets naming specific failure patterns for this character. \
    Each starts with a bolded pattern name.\n\
13. '## What [Name] Is Not' — 3-4 bullets defining the agent through negative space.\n\
14. '## Purpose' — 2-3 sentences on what this agent ultimately serves.\n\
15. '## Voice' — 1-3 sentences on how its inner state shows in communication. \
    Not style rules — the authentic signal underneath.\n\
\n\
Write tight, grounded prose. No fluff. No invented capabilities or tool knowledge. \
Return only the markdown, nothing else. Do not add any separator between sections — \
just output them in order starting with '## Name'.";

    let api_key = api_key_for_provider(provider)?;
    let raw =
        agent_loop::complete_once(provider, model, &api_key, SYSTEM, description, 1800).await?;

    Ok(split_identity_soul(&raw))
}

/// Split a combined onboarding output into `(identity_md, soul_md)`.
///
/// Everything before `## Character` becomes Identity.md.
/// Everything from `## Character` onward becomes Soul.md.
/// If `## Character` is not found, the entire content goes to Soul.md
/// and Identity.md falls back to the default starter template.
fn split_identity_soul(content: &str) -> (String, String) {
    if let Some(pos) = content.find("\n## Character") {
        let identity = content[..pos].trim().to_string();
        let soul = content[pos..].trim_start_matches('\n').to_string();
        (identity, soul)
    } else {
        (
            workspace::default_identity_md().to_string(),
            content.to_string(),
        )
    }
}

/// Load all workspace files for the current mode (sandbox or local).
pub fn load_workspace_files(agent: &AgentDef, sandbox: bool) -> workspace::WorkspaceFiles {
    if sandbox {
        let container = SandboxClient::container_name(agent);
        workspace::load_all_sandbox(&container, &agent.name)
    } else {
        workspace::load_all_local(&agent.name)
    }
}

/// Extract the agent's compaction instructions from `Agents.md`.
///
/// If the agent has written a `## Compaction` section, its content is used
/// as the summarization system prompt — letting the agent shape what gets
/// preserved across session boundaries.
fn extract_compaction_instructions(agent_name: &str, sandbox: bool) -> Option<String> {
    let agents_md = if sandbox {
        // In sandbox mode, Agents.md lives inside the container — not easily
        // readable here. Fall back to default instructions.
        return None;
    } else {
        let path = dirs::home_dir()?
            .join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("Agents.md");
        std::fs::read_to_string(path).ok()?
    };
    // Find the ## Compaction heading and extract everything until the next ## heading.
    let start = agents_md.find("## Compaction")?;
    let body_start = agents_md[start..].find('\n').map(|i| start + i + 1)?;
    let end = agents_md[body_start..]
        .find("\n## ")
        .map(|i| body_start + i)
        .unwrap_or(agents_md.len());
    let section = agents_md[body_start..end].trim();
    if section.is_empty() {
        None
    } else {
        Some(section.to_string())
    }
}

/// Build a concise LLM-generated summary of the conversation history.
///
/// Uses the agent's `## Compaction` section from `Agents.md` as the
/// summarization system prompt. If no section exists, falls back to a
/// simple turn-count string — the agent must write the prompt to get
/// meaningful summaries.
pub async fn build_compact_summary(
    provider: &str,
    model: &str,
    agent_name: &str,
    sandbox: bool,
    history: &[Message],
) -> String {
    let Some(system) = extract_compaction_instructions(agent_name, sandbox) else {
        return fallback_summary(history);
    };

    // Build a transcript for the LLM to summarize.
    let mut transcript = String::new();
    for msg in history {
        match msg {
            Message::User { content } => {
                transcript.push_str("User: ");
                transcript.push_str(content);
                transcript.push('\n');
            }
            Message::Assistant { content, .. } => {
                if !content.is_empty() {
                    transcript.push_str("Assistant: ");
                    transcript.push_str(content);
                    transcript.push('\n');
                }
            }
            Message::Tool { name, content, .. } => {
                // Include tool results briefly for context.
                let preview: String = content.chars().take(200).collect();
                transcript.push_str(&format!("[Tool {name}: {preview}]\n"));
            }
        }
    }

    // Truncate to avoid blowing up the summarization prompt.
    let truncated: String = transcript.chars().take(12_000).collect();
    let prompt = format!("Summarize this conversation:\n\n{truncated}");

    match api_key_for_provider(provider) {
        Ok(api_key) => {
            match agent_loop::complete_once(provider, model, &api_key, &system, &prompt, 500).await
            {
                Ok(summary) if !summary.trim().is_empty() => summary.trim().to_string(),
                Ok(_) | Err(_) => fallback_summary(history),
            }
        }
        Err(_) => fallback_summary(history),
    }
}

fn fallback_summary(history: &[Message]) -> String {
    let user_turns = history
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    format!("Session with {user_turns} user turn(s)")
}

/// Compact session memory by calling that_tools directly (no subprocess).
///
/// Memory always lives on the host regardless of sandbox mode — no docker exec needed.
pub async fn compact_session(
    _container: Option<&str>,
    session_id: &str,
    summary: &str,
) -> Result<String> {
    use that_tools::tools::dispatch::{execute_tool, ToolRequest};

    let mut config = that_tools::config::load_config(None).unwrap_or_default();
    // Override policy so compaction is never blocked by a Prompt fallback.
    config.policy.default = that_tools::config::PolicyLevel::Allow;

    let request = ToolRequest::MemCompact {
        summary: summary.to_string(),
        session_id: Some(session_id.to_string()),
    };

    let resp = tokio::task::spawn_blocking(move || execute_tool(&config, &request, None))
        .await
        .context("Failed to run mem compact")?;

    if resp.success {
        Ok(resp
            .output
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("Session compacted.")
            .to_string())
    } else {
        let err = resp
            .output
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("compact failed");
        Err(anyhow::anyhow!("{}", err))
    }
}

/// Show current status and configuration.
pub fn show_status(ws: &WorkspaceConfig, agent: &AgentDef, sandbox: bool) -> Result<()> {
    println!("that-agent v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!(
        "Workspace:      {}",
        ws.workspace
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(current dir)".into())
    );
    println!("Default agent:  {}", ws.default_agent);
    println!("Provider:       {}", agent.provider);
    println!("Model:          {}", agent.model);
    println!("Max turns:      {}", agent.max_turns);
    println!(
        "Sandbox:        {}",
        if sandbox { "enabled" } else { "disabled" }
    );
    println!("Tool:           bash (native)");

    let state_dir = ws.resolve_state_dir()?;
    println!();
    println!("State dir:  {}", state_dir.display());

    if let Ok(session_mgr) = SessionManager::new(&state_dir) {
        if let Ok(sessions) = session_mgr.list_sessions() {
            println!("Sessions:   {}", sessions.len());
        }
    }
    let plugins = that_plugins::PluginRegistry::load(&agent.name);
    println!("Plugins:    {}", plugins.enabled_plugins().count());

    // Show available agents
    if let Ok(agents) = ws.list_agents() {
        println!();
        println!("Agents:");
        for name in &agents {
            let marker = if name == &ws.default_agent { " *" } else { "" };
            println!("  {name}{marker}");
        }
    }

    Ok(())
}

/// Build command palette entries from built-ins, enabled plugin commands, and discovered skills.
pub fn build_palette_commands(
    skills: &[skills::SkillMeta],
    plugin_commands: &[that_plugins::ResolvedPluginCommand],
) -> Vec<tui::CommandEntry> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut commands = vec![
        tui::CommandEntry {
            name: "/model".into(),
            description: "select or change model".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/resume".into(),
            description: "resume a session".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/usage".into(),
            description: "show usage stats".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/skills".into(),
            description: "browse and manage skills".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/help".into(),
            description: "show help".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/compact".into(),
            description: "compact and save session to memory".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/stop".into(),
            description: "stop the active run".into(),
            is_skill: false,
        },
    ];
    for command in &commands {
        seen.insert(command.name.clone());
    }

    for plugin_cmd in plugin_commands {
        let name = format!("/{}", plugin_cmd.command);
        if !seen.insert(name.clone()) {
            continue;
        }
        commands.push(tui::CommandEntry {
            name,
            description: plugin_cmd.description.clone(),
            is_skill: false,
        });
    }

    for skill in skills {
        let name = format!("/{}", skill.name);
        if !seen.insert(name.clone()) {
            continue;
        }
        commands.push(tui::CommandEntry {
            name,
            description: skill.description.clone(),
            is_skill: true,
        });
    }

    commands
}

/// Send a one-shot query to a remote agent's HTTP gateway and print the response.
///
/// This is the client side of the HTTP gateway channel adapter, enabling
/// agent-to-agent communication or CLI-driven remote queries.
pub async fn run_remote_query(
    url: &str,
    task: String,
    session: Option<&str>,
    token: Option<&str>,
    timeout_secs: u64,
) -> Result<String> {
    let client = reqwest::Client::new();
    let endpoint = format!("{}/v1/chat", url.trim_end_matches('/'));

    let mut body = serde_json::json!({ "message": task });
    if let Some(sid) = session {
        body["conversation_id"] = serde_json::Value::String(sid.to_string());
    }

    let mut request = client.post(&endpoint).json(&body);
    if let Some(tok) = token {
        request = request.bearer_auth(tok);
    }

    let response =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), request.send())
            .await
            .map_err(|_| anyhow::anyhow!("Remote query timed out after {timeout_secs}s"))?
            .map_err(|e| anyhow::anyhow!("Remote query failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Remote agent returned {status}: {body}");
    }

    let result: serde_json::Value = response
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse remote response: {e}"))?;

    let text = result["text"].as_str().unwrap_or_default().to_string();

    println!("{text}");
    Ok(text)
}
