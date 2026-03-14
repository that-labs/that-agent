use crate::config::AgentDef;

/// Maximum number of automatic retries on transient network / server errors.
pub const MAX_NETWORK_RETRIES: u32 = 5;

/// Initial backoff delay in ms; doubles each attempt (0.5 s -> 1 -> 2 -> 4 -> 8 s).
pub const RETRY_BASE_DELAY_MS: u64 = 500;
/// Warn when cache hit rate drops below this threshold on sizable prompts.
pub const CACHE_HIT_WARN_THRESHOLD: f64 = 0.70;
/// Fallback text used when the model completes without any final assistant text.
pub const EMPTY_CHANNEL_RESPONSE_FALLBACK: &str =
    "I could not generate a response for that request. Please try again.";
/// Maximum retries when the model returns an empty final channel response.
pub const MAX_EMPTY_CHANNEL_RESPONSE_RETRIES: u32 = 1;

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

pub fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn parse_env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn openai_websocket_enabled() -> bool {
    parse_env_bool("THAT_OPENAI_WEBSOCKET").unwrap_or(true)
}

pub fn trusted_local_sandbox_enabled() -> bool {
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

pub fn should_use_channel_empty_response_fallback(
    text: &str,
    suppress_output: bool,
    tool_events: &[that_channels::ToolLogEvent],
) -> bool {
    if suppress_output || !text.trim().is_empty() {
        return false;
    }

    // Mid-task notifications are not a substitute for the run's final user-facing
    // answer. Only suppress the fallback when a terminal channel delivery tool
    // already succeeded and produced its own outbound message, or when the agent
    // deliberately sent its final answer via channel_notify as its last action.
    !has_successful_terminal_channel_output(tool_events)
        && !last_tool_was_answer(tool_events)
        && !last_tool_was_channel_notify(tool_events)
}

pub fn summarize_tool_result_for_empty_response(
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

pub fn build_empty_channel_response_fallback(
    tool_events: &[that_channels::ToolLogEvent],
) -> String {
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

const TERMINAL_CHANNEL_TOOL_NAMES: &[&str] = &[
    "channel_send_message",
    "channel_send_file",
    "channel_send_raw",
];

fn has_successful_terminal_channel_output(tool_events: &[that_channels::ToolLogEvent]) -> bool {
    tool_events.iter().any(|ev| {
        matches!(
            ev,
            that_channels::ToolLogEvent::Result {
                name,
                is_error: false,
                ..
            } if TERMINAL_CHANNEL_TOOL_NAMES.contains(&name.as_str())
        )
    })
}

/// Returns true when the last tool result in the event log is a successful
/// `answer`. This signals the agent delivered its final answer via the
/// dedicated answer tool, so no duplicate Done event is needed.
pub fn last_tool_was_answer(tool_events: &[that_channels::ToolLogEvent]) -> bool {
    last_tool_result_name(tool_events) == Some(("answer", false))
}

/// Returns true when the last tool result in the event log is a successful
/// `channel_notify`. This signals the agent deliberately sent its final
/// answer via the notification tool, so no fallback response is needed.
pub fn last_tool_was_channel_notify(tool_events: &[that_channels::ToolLogEvent]) -> bool {
    last_tool_result_name(tool_events) == Some(("channel_notify", false))
}

fn last_tool_result_name(tool_events: &[that_channels::ToolLogEvent]) -> Option<(&str, bool)> {
    tool_events.iter().rev().find_map(|ev| {
        if let that_channels::ToolLogEvent::Result { name, is_error, .. } = ev {
            Some((name.as_str(), *is_error))
        } else {
            None
        }
    })
}

/// Memory tools are safe to re-invoke (idempotent intent); the model should
/// still generate a user-facing confirmation after calling them.
const MEMORY_TOOL_NAMES: &[&str] = &["mem_add", "mem_recall", "mem_compact"];

fn only_memory_tool_calls(tool_events: &[that_channels::ToolLogEvent]) -> bool {
    let calls: Vec<&str> = tool_events
        .iter()
        .filter_map(|ev| {
            if let that_channels::ToolLogEvent::Call { name, .. } = ev {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();
    !calls.is_empty() && calls.iter().all(|n| MEMORY_TOOL_NAMES.contains(n))
}

pub fn should_retry_empty_channel_response(
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
    // If the agent deliberately sent its final answer via answer or channel_notify, skip retry.
    if last_tool_was_answer(tool_events) || last_tool_was_channel_notify(tool_events) {
        return false;
    }
    // Avoid re-running after side-effecting tool calls — but memory tools are
    // safe to re-invoke, so allow one retry to produce the user-facing response.
    tool_events.is_empty() || only_memory_tool_calls(tool_events)
}

pub fn build_empty_channel_retry_task(task: &str) -> String {
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
pub fn runtime_reminder_lines(sandbox: bool, agent_name: &str) -> Vec<String> {
    fn runtime_home_dir() -> std::path::PathBuf {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

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
    let home_dir = runtime_home_dir();
    let persistent_home_dir = home_dir.join(".that-agent");
    lines.push(format!("home_dir: {}", home_dir.display()));
    lines.push(format!(
        "task_workspace_dir: {}",
        if sandbox { "/workspace" } else { "." }
    ));
    lines.push(format!(
        "persistent_home_dir: {}",
        persistent_home_dir.display()
    ));
    lines.push(format!(
        "agent_home_dir: {}",
        persistent_home_dir
            .join("agents")
            .join(agent_name)
            .display()
    ));
    lines.push(format!(
        "state_dir: {}",
        persistent_home_dir.join("state").display()
    ));
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

pub fn append_system_reminder(
    task: &str,
    session_id: &str,
    sandbox: bool,
    agent_name: &str,
) -> String {
    if task.contains("<system-reminder>") {
        return task.to_string();
    }
    let today_utc = chrono::Utc::now().format("%Y-%m-%d").to_string();
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
pub fn cache_hit_rate_percent(
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

pub fn prompt_cache_alerts_enabled() -> bool {
    std::env::var("THAT_PROMPT_CACHE_ALERTS")
        .ok()
        .map(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

pub fn log_prompt_cache_usage(
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

/// Sanitize a task string into a compact single-line span field value.
///
/// Strips injected `<system-reminder>…</system-reminder>` blocks (everything
/// from the first `<system-reminder>` onwards) so only the user's actual
/// message appears. Then replaces control characters with a space, collapses
/// runs of whitespace, and truncates to `max_chars` visible characters.
/// This prevents k8s from splitting span field values across log lines.
pub fn task_preview(s: &str, max_chars: usize) -> String {
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

pub fn append_memory_bootstrap_reminder(task: &str, history_len: usize) -> String {
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

#[cfg(test)]
mod tests {
    use super::{
        last_tool_was_answer, should_retry_empty_channel_response,
        should_use_channel_empty_response_fallback,
    };
    use that_channels::ToolLogEvent;

    #[test]
    fn empty_response_after_final_channel_notify_skips_fallback() {
        // channel_notify as the last tool call = agent's deliberate final answer.
        let tool_events = vec![
            ToolLogEvent::Call {
                name: "channel_notify".into(),
                args: r#"{"message":"Done! Everything deployed."}"#.into(),
            },
            ToolLogEvent::Result {
                name: "channel_notify".into(),
                result: r#"{"sent":true}"#.into(),
                is_error: false,
            },
        ];

        assert!(!should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn empty_response_after_mid_run_channel_notify_uses_fallback() {
        // channel_notify followed by more tools = progress update, not final answer.
        let tool_events = vec![
            ToolLogEvent::Result {
                name: "channel_notify".into(),
                result: r#"{"sent":true}"#.into(),
                is_error: false,
            },
            ToolLogEvent::Call {
                name: "shell_exec".into(),
                args: r#"{"cmd":"deploy"}"#.into(),
            },
            ToolLogEvent::Result {
                name: "shell_exec".into(),
                result: "deployed".into(),
                is_error: false,
            },
        ];

        assert!(should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn empty_response_after_failed_channel_notify_uses_fallback() {
        let tool_events = vec![ToolLogEvent::Result {
            name: "channel_notify".into(),
            result: r#"{"error":"send failed"}"#.into(),
            is_error: true,
        }];

        assert!(should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn empty_response_after_successful_terminal_channel_send_skips_fallback() {
        let tool_events = vec![ToolLogEvent::Result {
            name: "channel_send_message".into(),
            result: r#"{"sent":true}"#.into(),
            is_error: false,
        }];

        assert!(!should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn empty_response_after_failed_terminal_channel_send_uses_fallback() {
        let tool_events = vec![ToolLogEvent::Result {
            name: "channel_send_message".into(),
            result: r#"{"error":"boom"}"#.into(),
            is_error: true,
        }];

        assert!(should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn last_tool_was_answer_detects_successful_answer() {
        let tool_events = vec![
            ToolLogEvent::Call {
                name: "answer".into(),
                args: r#"{"message":"Here is your result."}"#.into(),
            },
            ToolLogEvent::Result {
                name: "answer".into(),
                result: r#"{"delivered":true}"#.into(),
                is_error: false,
            },
        ];
        assert!(last_tool_was_answer(&tool_events));
    }

    #[test]
    fn empty_response_after_answer_skips_fallback() {
        let tool_events = vec![
            ToolLogEvent::Call {
                name: "answer".into(),
                args: r#"{"message":"Done."}"#.into(),
            },
            ToolLogEvent::Result {
                name: "answer".into(),
                result: r#"{"delivered":true}"#.into(),
                is_error: false,
            },
        ];
        assert!(!should_use_channel_empty_response_fallback(
            "",
            false,
            &tool_events
        ));
    }

    #[test]
    fn retry_skipped_when_last_tool_was_answer() {
        let tool_events = vec![
            ToolLogEvent::Call {
                name: "answer".into(),
                args: r#"{"message":"All done."}"#.into(),
            },
            ToolLogEvent::Result {
                name: "answer".into(),
                result: r#"{"delivered":true}"#.into(),
                is_error: false,
            },
        ];
        assert!(!should_retry_empty_channel_response(
            "",
            false,
            &tool_events,
            0
        ));
    }

    #[test]
    fn retry_skipped_when_last_tool_was_channel_notify() {
        let tool_events = vec![
            ToolLogEvent::Call {
                name: "channel_notify".into(),
                args: r#"{"message":"All done."}"#.into(),
            },
            ToolLogEvent::Result {
                name: "channel_notify".into(),
                result: r#"{"sent":true}"#.into(),
                is_error: false,
            },
        ];

        assert!(!should_retry_empty_channel_response(
            "",
            false,
            &tool_events,
            0
        ));
    }
}
