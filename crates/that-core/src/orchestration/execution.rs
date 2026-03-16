use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::agent_loop::{self, LoopConfig, Message, SteeringQueue, ToolContext};
use crate::config::AgentDef;
use crate::hooks::{
    channel_answer_tool_def, channel_notify_tool_def, channel_send_file_tool_def,
    channel_send_message_tool_def, channel_send_raw_tool_def, channel_settings_tool_def,
    ChannelHook,
};
use crate::tools::all_tool_defs;

use super::config::*;
use super::hooks::{AgentHook, EvalHook};

fn agent_state_dir(agent: &AgentDef) -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".that-agent").join("agents").join(&agent.name))
}

const FINALIZATION_SUMMARY_EVENT_LIMIT: usize = 8;
const FINALIZATION_SNIPPET_CHARS: usize = 160;

/// Resolve the provider API key from environment variables.
///
/// For Anthropic, checks `CLAUDE_CODE_OAUTH_TOKEN` first (OAuth flow),
/// then falls back to `ANTHROPIC_API_KEY`.
pub fn api_key_for_provider(provider: &str) -> Result<String> {
    match provider {
        "anthropic" => std::env::var("CLAUDE_CODE_OAUTH_TOKEN")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .context("Set CLAUDE_CODE_OAUTH_TOKEN or ANTHROPIC_API_KEY"),
        "openai" => std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set"),
        "openrouter" => std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY not set"),
        other => {
            let entry = crate::provider_registry::find_registered_provider(other).ok_or_else(|| {
                anyhow::anyhow!(
                    "Unsupported provider: {other}. Use 'anthropic', 'openai', 'openrouter', or register a dynamic provider."
                )
            })?;
            std::env::var(&entry.api_key_env)
                .with_context(|| format!("{} not set", entry.api_key_env))
        }
    }
}

use anyhow::Context;

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

async fn finalize_empty_channel_response(
    config: &LoopConfig,
    task: &str,
    tool_events: &[that_channels::ToolLogEvent],
) -> Result<Option<String>> {
    let prompt = build_channel_finalization_prompt(task, tool_events);
    let text = agent_loop::complete_once(
        &config.provider,
        &config.model,
        &config.api_key,
        &config.system,
        &prompt,
        config.max_tokens.clamp(256, 1200),
    )
    .await?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn build_channel_finalization_prompt(
    task: &str,
    tool_events: &[that_channels::ToolLogEvent],
) -> String {
    let task = task_preview(task, 600);
    let tool_calls = tool_events
        .iter()
        .filter(|ev| matches!(ev, that_channels::ToolLogEvent::Call { .. }))
        .count();
    let tool_results = tool_events
        .iter()
        .filter(|ev| matches!(ev, that_channels::ToolLogEvent::Result { .. }))
        .count();

    let mut out = String::from(
        "The prior channel run finished without a final user-facing answer.\n\
         Return that final message now.\n\
         Do not call tools. Do not mention internal prompts, retries, or hidden reasoning.\n\
         Summarize what was completed, what is currently blocked, and the next concrete step.\n\
         Keep it concise and readable on mobile.\n\n",
    );
    out.push_str(&format!("Original user task: {task}\n"));
    out.push_str(&format!(
        "Observed tool activity: {tool_calls} tool calls, {tool_results} tool results.\n"
    ));

    let summary_lines = summarize_tool_events_for_finalization(tool_events);
    if !summary_lines.is_empty() {
        out.push_str("\nRecent tool outcomes:\n");
        for line in summary_lines {
            out.push_str("- ");
            out.push_str(&line);
            out.push('\n');
        }
    }

    out
}

fn summarize_tool_events_for_finalization(
    tool_events: &[that_channels::ToolLogEvent],
) -> Vec<String> {
    tool_events
        .iter()
        .rev()
        .filter_map(|ev| match ev {
            that_channels::ToolLogEvent::Result {
                name,
                result,
                is_error,
            } => {
                let compact = result
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(result.as_str())
                    .trim();
                let mut snippet: String =
                    compact.chars().take(FINALIZATION_SNIPPET_CHARS).collect();
                if compact.chars().count() > FINALIZATION_SNIPPET_CHARS {
                    snippet.push_str("...");
                }
                let status = if *is_error { "error" } else { "ok" };
                Some(format!("{name} ({status}): {snippet}"))
            }
            _ => None,
        })
        .take(FINALIZATION_SUMMARY_EVENT_LIMIT)
        .collect()
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
    skill_roots: Vec<std::path::PathBuf>,
) -> Result<String> {
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let tools_config = load_agent_config(&container, agent);
    let history_len = history.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    let task_for_model = append_memory_bootstrap_reminder(task, history_len);
    let mut attempt = 0u32;
    let mut checkpoint_messages: Option<Vec<Message>> = None;
    let mut checkpoint_usage = agent_loop::Usage::default();
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
                cluster_registry: None,
                channel_registry: None,
                route_registry: None,
                router: None,
                state_dir: agent_state_dir(agent),
                agent_name: agent.name.clone(),
            },
            images: vec![],
            steering: None,
        };
        let hook = AgentHook { debug };
        let result = if let Some(messages) = checkpoint_messages.clone() {
            agent_loop::resume_with_checkpoint(&config, messages, &hook).await
        } else {
            agent_loop::run_with_checkpoint(&config, &task_for_model, &hook).await
        };

        match result {
            Ok((text, usage)) => {
                // Log cache usage from the last run only (not cumulative) — cumulative
                // totals produce meaningless hit-rate percentages.
                log_prompt_cache_usage(
                    &agent.provider,
                    &agent.model,
                    usage.input_tokens as u64,
                    usage.cache_read_tokens as u64,
                    usage.cache_write_tokens as u64,
                );
                let _usage = checkpoint_usage.add(&usage);
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                return Ok(text);
            }
            Err(interrupted) => {
                if is_retryable_error(&interrupted.error) && attempt < MAX_NETWORK_RETRIES {
                    if checkpoint_messages.is_some() {
                        attempt = 0;
                    }
                    attempt += 1;
                    checkpoint_usage = checkpoint_usage.add(&interrupted.usage);
                    checkpoint_messages = Some(interrupted.messages);
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current().record(
                    "otel.status_description",
                    format!("{:#}", interrupted.error).as_str(),
                );
                return Err(interrupted.error);
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
#[allow(clippy::too_many_arguments)]
pub async fn execute_agent_run_eval(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    _debug: bool,
    history: Option<Vec<Message>>,
    session_id_for_trace: Option<&str>,
    skill_roots: Vec<std::path::PathBuf>,
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
    let history_len = history.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    let task_for_model = append_memory_bootstrap_reminder(task, history_len);
    let mut attempt = 0u32;
    let mut checkpoint_messages: Option<Vec<Message>> = None;
    let mut checkpoint_usage = agent_loop::Usage::default();
    let mut checkpoint_events: Vec<String> = Vec::new();
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
                cluster_registry: None,
                channel_registry: None,
                route_registry: None,
                router: None,
                state_dir: agent_state_dir(agent),
                agent_name: agent.name.clone(),
            },
            images: vec![],
            steering: None,
        };
        let hook = EvalHook::new();
        let result = if let Some(messages) = checkpoint_messages.clone() {
            agent_loop::resume_with_checkpoint(&config, messages, &hook).await
        } else {
            agent_loop::run_with_checkpoint(&config, &task_for_model, &hook).await
        };

        match result {
            Ok((text, usage)) => {
                let mut events = checkpoint_events;
                events.extend(hook.take_events());
                log_prompt_cache_usage(
                    &agent.provider,
                    &agent.model,
                    usage.input_tokens as u64,
                    usage.cache_read_tokens as u64,
                    usage.cache_write_tokens as u64,
                );
                let _usage = checkpoint_usage.add(&usage);
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                return Ok((text, events));
            }
            Err(interrupted) => {
                if is_retryable_error(&interrupted.error) && attempt < MAX_NETWORK_RETRIES {
                    if checkpoint_messages.is_some() {
                        attempt = 0;
                    }
                    attempt += 1;
                    checkpoint_usage = checkpoint_usage.add(&interrupted.usage);
                    checkpoint_messages = Some(interrupted.messages);
                    checkpoint_events.extend(hook.take_events());
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current().record(
                    "otel.status_description",
                    format!("{:#}", interrupted.error).as_str(),
                );
                return Err(interrupted.error);
            }
        }
    }
}

/// Route a channel event to a specific channel or broadcast to all.
pub async fn route_channel_event(
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

fn resolved_agent_config_path(agent_name: &str, sandbox: bool) -> String {
    let use_preferred = dirs::home_dir()
        .map(|home| {
            let preferred = home
                .join(".that-agent")
                .join("agents")
                .join(agent_name)
                .join("config.toml");
            let legacy = home
                .join(".that-agent")
                .join("agents")
                .join(format!("{agent_name}.toml"));
            preferred.exists() || !legacy.exists()
        })
        .unwrap_or(true);
    if sandbox {
        if use_preferred {
            format!("/home/agent/.that-agent/agents/{agent_name}/config.toml")
        } else {
            format!("/home/agent/.that-agent/agents/{agent_name}.toml")
        }
    } else if let Some(home) = dirs::home_dir() {
        let path = if use_preferred {
            home.join(".that-agent")
                .join("agents")
                .join(agent_name)
                .join("config.toml")
        } else {
            home.join(".that-agent")
                .join("agents")
                .join(format!("{agent_name}.toml"))
        };
        path.to_string_lossy().into_owned()
    } else if use_preferred {
        format!("~/.that-agent/agents/{agent_name}/config.toml")
    } else {
        format!("~/.that-agent/agents/{agent_name}.toml")
    }
}

fn channel_config_env_lines(effective_config_path: Option<&str>, base_config_path: &str) -> String {
    if let Some(effective_config_path) = effective_config_path {
        format!(
            "- `THAT_CONFIG_PATH={effective_config_path}` — effective runtime config for this conversation (includes any /models override)\n\
             - `THAT_AGENT_CONFIG_PATH={base_config_path}` — base agent config on disk"
        )
    } else {
        format!(
            "- `THAT_CONFIG_PATH={base_config_path}` — this agent's channel and adapter configuration file"
        )
    }
}

/// Build and execute a single agent run using a [`that_channels::ChannelRouter`].
///
/// This is the generic multi-channel equivalent of [`super::tui_session::execute_agent_run_tui`].
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
#[allow(clippy::too_many_arguments)]
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
    show_work: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id_for_trace: Option<&str>,
    run_id_for_trace: Option<&str>,
    images: Vec<(Vec<u8>, String)>,
    cluster_registry: Option<std::sync::Arc<that_plugins::cluster::ClusterRegistry>>,
    channel_registry: Option<std::sync::Arc<that_channels::registry::DynamicChannelRegistry>>,
    route_registry: Option<Arc<that_channels::DynamicRouteRegistry>>,
    skill_roots: Vec<std::path::PathBuf>,
    steering: Option<SteeringQueue>,
    effective_config_path: Option<String>,
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
    let mut attempt = 0u32;
    let mut empty_response_retries = 0u32;

    // Append channel formatting instructions to the preamble.
    // For scoped runs, prefer only the active channel's guidance to avoid
    // conflicting markdown/rendering rules across adapters.
    let primary_id = router.primary_id().await;
    let active_channel = route_channel_id.as_deref().unwrap_or(&primary_id);
    let format_section = if let Some(cid) = route_channel_id.as_deref() {
        let scoped = router.format_instructions_for(cid).await;
        if scoped.is_empty() {
            router.combined_format_instructions().await
        } else {
            scoped
        }
    } else {
        router.combined_format_instructions().await
    };
    let base_config_path = resolved_agent_config_path(&agent.name, container.is_some());
    let config_env_lines =
        channel_config_env_lines(effective_config_path.as_deref(), &base_config_path);

    let gateway_url = super::support::resolve_gateway_url();
    let channel_info = format!(
        "## Active Channels\n\n\
         You are communicating through the following channels: {ids}\n\
         Active route for this response: {active}\n\
         Primary channel (used for interactive human_ask): {primary}\n\
         Channel env vars available at runtime:\n\
         - `THAT_CHANNEL_IDS={ids}`\n\
         - `THAT_CHANNEL_PRIMARY={primary}`\n\
         {config_env_lines}\n\n\
         ## Gateway\n\n\
         Your HTTP gateway is always listening at `{gateway_url}`.\n\
         - `POST {gateway_url}/v1/inbound` — async fire-and-forget (returns 202). Triggers a background agent run. \
         Use this from plugins and services. Response delivered via `callback_url` or `answer`.\n\
         - `POST {gateway_url}/v1/chat` — synchronous (blocks until done). Only for one-shot queries that need inline response. \
         Never use from plugins — it blocks the caller and makes tool calls visible on the user's channel.\n\
         - `POST {gateway_url}/v1/notify` — zero-cost queue (returns 202). No LLM turn, batched into next heartbeat.\n\
         - `GET {gateway_url}/v1/schema` — introspection endpoint for bridge plugins at startup.\n\
         Use `channel_register` to hot-register a bridge and `channel_list` to see active bridges.\n\
         Use `provider_register` to add an OpenAI-compatible inference provider at runtime. \
         Once its API key env var is configured, it will show up in `/models`.\n\
         When building or deploying a bridge plugin, give it this gateway URL and configure it to use `/v1/inbound` (not `/v1/chat`).",
        ids = router.channel_ids().await,
        active = active_channel,
        primary = primary_id,
        config_env_lines = config_env_lines,
        gateway_url = gateway_url,
    );
    let channel_output_contract = format!(
        "## Channel Output\n\n\
         Your final answer goes directly to the human on `{active}`.\n\
         Your Communication Style (from Agents.md) applies here — follow it.\n\n\
         - After completing your work, deliver your final answer by calling `answer`.\n\
         It must be the **last** tool you call. The message is delivered with proper \
         channel formatting.\n\
         - Use `channel_notify` only for mid-turn progress updates, not for the final answer.\n\
         - Do not rely on trailing text after your last tool call — it may not reach the channel.\n\
         - No file paths with line numbers, no checkmark lists, no verification dumps.\n\
         The human wants to know what happened and what is next, not see your work log.\n\
         - Self-check channel syntax before sending: no broken markdown, no unmatched fences, \
         no stray escape artifacts.\n\
         - When unsure about channel formatting, prefer plain readable text.\n\
         - Keep it concise and legible on mobile.",
        active = active_channel,
    );
    // Keep the system message = stable preamble only so the prompt cache always hits.
    // Volatile channel context (active route, gateway URL, format instructions) is injected
    // into the task message as a system-reminder instead.
    let channel_ctx = if format_section.is_empty() {
        format!("\n\n<system-reminder>\n{channel_info}\n\n{channel_output_contract}\n</system-reminder>")
    } else {
        format!("\n\n<system-reminder>\n{channel_info}\n\n{channel_output_contract}\n\n{format_section}\n</system-reminder>")
    };
    let task_for_model = format!(
        "{}{}",
        append_memory_bootstrap_reminder(task, history.len()),
        channel_ctx
    );
    let full_preamble = preamble.to_string();
    let mut checkpoint_messages: Option<Vec<Message>> = None;
    let mut checkpoint_usage = agent_loop::Usage::default();
    let mut checkpoint_tool_events: Vec<that_channels::ToolLogEvent> = Vec::new();

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

        // Clear any stale adapter state from a previously aborted or failed run.
        if !suppress_output {
            route_channel_event(
                router.as_ref(),
                route_channel_id.as_deref(),
                route_target.as_ref(),
                &that_channels::ChannelEvent::Reset,
            )
            .await;
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
                std::sync::Arc::clone(&show_work),
            )
        } else {
            ChannelHook::new(
                std::sync::Arc::clone(&router),
                Some(log_tx),
                std::sync::Arc::clone(&show_work),
            )
        };
        let tools_config = load_agent_config(&container, agent);

        // Add channel-specific tools: ChannelHook intercepts calls and routes them
        // to the router without hitting dispatch().
        let mut tools = all_tool_defs(&container);
        let answer_fmt = router.format_instructions_for(active_channel).await;
        tools.push(channel_answer_tool_def(&answer_fmt));
        tools.push(channel_notify_tool_def());
        tools.push(channel_send_file_tool_def());
        tools.push(channel_send_message_tool_def());
        tools.push(channel_send_raw_tool_def());
        tools.push(channel_settings_tool_def());

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
                skill_roots: skill_roots.clone(),
                cluster_registry: cluster_registry.clone(),
                channel_registry: channel_registry.clone(),
                route_registry: route_registry.clone(),
                router: Some(std::sync::Arc::clone(&router)),
                state_dir: agent_state_dir(agent),
                agent_name: agent.name.clone(),
            },
            images: images.clone(),
            steering: steering.clone(),
        };
        let task_for_attempt = if empty_response_retries > 0 {
            build_empty_channel_retry_task(&task_for_model)
        } else {
            task_for_model.clone()
        };
        let result = if let Some(messages) = checkpoint_messages.clone() {
            agent_loop::resume_with_checkpoint(&config, messages, &hook).await
        } else {
            agent_loop::run_with_checkpoint(&config, &task_for_attempt, &hook).await
        };

        // Drop hook to flush log_tx sender side, then drain events.
        drop(hook);
        let mut tool_events: Vec<that_channels::ToolLogEvent> = Vec::new();
        while let Ok(ev) = log_rx.try_recv() {
            tool_events.push(ev);
        }

        match result {
            Ok((text, usage)) => {
                if !checkpoint_tool_events.is_empty() {
                    let mut merged = std::mem::take(&mut checkpoint_tool_events);
                    merged.extend(tool_events);
                    tool_events = merged;
                }
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
                    match finalize_empty_channel_response(&config, &task_for_model, &tool_events)
                        .await
                    {
                        Ok(Some(finalized)) => {
                            warn!(
                                agent = %agent.name,
                                channel = ?route_channel_id,
                                "Model returned empty channel response; recovered with forced finalization pass"
                            );
                            finalized
                        }
                        Ok(None) => {
                            warn!(
                                agent = %agent.name,
                                channel = ?route_channel_id,
                                "Model returned empty channel response; finalization pass was empty, using fallback text"
                            );
                            build_empty_channel_response_fallback(&tool_events)
                        }
                        Err(err) => {
                            warn!(
                                agent = %agent.name,
                                channel = ?route_channel_id,
                                error = %err,
                                "Model returned empty channel response; finalization pass failed, using fallback text"
                            );
                            build_empty_channel_response_fallback(&tool_events)
                        }
                    }
                } else {
                    text
                };
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                crate::observability::flush_tracing();
                if !suppress_output
                    && !last_tool_was_answer(&tool_events)
                    && !last_tool_was_channel_notify(&tool_events)
                {
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
            Err(interrupted) => {
                if is_retryable_error(&interrupted.error) && attempt < MAX_NETWORK_RETRIES {
                    // Reset retry counter if the agent made progress since the last error
                    // (new tool events means turns completed successfully before this failure).
                    if !tool_events.is_empty() {
                        attempt = 0;
                    }
                    attempt += 1;
                    checkpoint_usage = checkpoint_usage.add(&interrupted.usage);
                    checkpoint_messages = Some(interrupted.messages);
                    checkpoint_tool_events.extend(tool_events);
                    continue;
                }
                if !checkpoint_tool_events.is_empty() {
                    checkpoint_tool_events.extend(tool_events);
                }
                let event = that_channels::ChannelEvent::Error(format!("{:#}", interrupted.error));
                route_channel_event(
                    router.as_ref(),
                    route_channel_id.as_deref(),
                    route_target.as_ref(),
                    &event,
                )
                .await;
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current().record(
                    "otel.status_description",
                    format!("{:#}", interrupted.error).as_str(),
                );
                crate::observability::flush_tracing();
                return Err(interrupted.error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_channel_finalization_prompt, channel_config_env_lines, resolved_agent_config_path,
    };

    #[test]
    fn finalization_prompt_uses_user_task_and_recent_tool_results() {
        let tool_events = vec![
            that_channels::ToolLogEvent::Call {
                name: "shell_exec".into(),
                args: r#"{"cmd":"kubectl get pods"}"#.into(),
            },
            that_channels::ToolLogEvent::Result {
                name: "shell_exec".into(),
                result: "pods ready".into(),
                is_error: false,
            },
        ];

        let prompt = build_channel_finalization_prompt(
            "Ship it\n\n<system-reminder>\ninternal: true\n</system-reminder>",
            &tool_events,
        );

        assert!(prompt.contains("Original user task: Ship it"));
        assert!(prompt.contains("shell_exec (ok): pods ready"));
        assert!(!prompt.contains("internal: true"));
    }

    #[test]
    fn config_env_lines_include_effective_and_base_paths_when_overridden() {
        let lines = channel_config_env_lines(
            Some("/home/agent/.that-agent/state/channel-configs/demo/telegram_123.toml"),
            "/home/agent/.that-agent/agents/demo/config.toml",
        );

        assert!(lines.contains(
            "THAT_CONFIG_PATH=/home/agent/.that-agent/state/channel-configs/demo/telegram_123.toml"
        ));
        assert!(lines
            .contains("THAT_AGENT_CONFIG_PATH=/home/agent/.that-agent/agents/demo/config.toml"));
    }

    #[test]
    fn sandbox_agent_config_path_prefers_config_toml_layout() {
        let path = resolved_agent_config_path("demo", true);
        assert_eq!(path, "/home/agent/.that-agent/agents/demo/config.toml");
    }
}
