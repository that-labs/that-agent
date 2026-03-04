use std::sync::Arc;

use anyhow::Result;
use tracing::warn;

use crate::agent_loop::{self, LoopConfig, Message, ToolContext};
use crate::config::AgentDef;
use crate::hooks::{
    channel_notify_tool_def, channel_send_file_tool_def, channel_send_message_tool_def,
    channel_send_raw_tool_def, channel_settings_tool_def, ChannelHook,
};
use crate::tools::all_tool_defs;

use super::config::*;
use super::hooks::{AgentHook, EvalHook};

fn agent_state_dir(agent: &AgentDef) -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".that-agent").join("agents").join(&agent.name))
}

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
        other => Err(anyhow::anyhow!(
            "Unsupported provider: {other}. Use 'anthropic', 'openai', or 'openrouter'."
        )),
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
            },
            images: vec![],
            steering: None,
        };
        let hook = AgentHook { debug };
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
            },
            images: vec![],
            steering: None,
        };
        let hook = EvalHook::new();
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

    let gateway_url = super::support::resolve_gateway_url();
    let channel_info = format!(
        "## Active Channels\n\n\
         You are communicating through the following channels: {ids}\n\
         Active route for this response: {active}\n\
         Primary channel (used for interactive human_ask): {primary}\n\
         Channel env vars available at runtime:\n\
         - `THAT_CHANNEL_IDS={ids}`\n\
         - `THAT_CHANNEL_PRIMARY={primary}`\n\
         - `THAT_CONFIG_PATH={config_path}` — this agent's channel and adapter configuration file\n\n\
         ## Gateway\n\n\
         Your HTTP gateway is always listening at `{gateway_url}`.\n\
         - `POST {gateway_url}/v1/inbound` — async fire-and-forget (returns 202). Triggers a background agent run. \
         Use this from plugins and services. Response delivered via `callback_url` or `channel_notify`.\n\
         - `POST {gateway_url}/v1/chat` — synchronous (blocks until done). Only for one-shot queries that need inline response. \
         Never use from plugins — it blocks the caller and makes tool calls visible on the user's channel.\n\
         - `POST {gateway_url}/v1/notify` — zero-cost queue (returns 202). No LLM turn, batched into next heartbeat.\n\
         - `GET {gateway_url}/v1/schema` — introspection endpoint for bridge plugins at startup.\n\
         Use `channel_register` to hot-register a bridge and `channel_list` to see active bridges.\n\
         When building or deploying a bridge plugin, give it this gateway URL and configure it to use `/v1/inbound` (not `/v1/chat`).",
        ids = router.channel_ids().await,
        active = active_channel,
        primary = primary_id,
        config_path = config_path,
        gateway_url = gateway_url,
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
            },
            images: images.clone(),
            steering: None,
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
