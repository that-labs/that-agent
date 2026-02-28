//! Thin owned agentic loop.
//!
//! Provides a multi-turn LLM loop that:
//! - Streams text + reasoning tokens to a `LoopHook`
//! - Dispatches tool calls via `crate::tools::typed::dispatch`
//! - Supports Anthropic (with prompt caching + extended thinking), OpenAI, and OpenRouter
//! - Retries on transient network errors with exponential backoff
//! - Returns the final assistant text and aggregated `Usage`
//!
//! # Tracing
//!
//! Every LLM API request emits an `llm_turn` span with standard Gen AI semantic
//! convention attributes (`gen_ai.system`, `gen_ai.request.model`,
//! `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, cache counters).
//!
//! Every tool dispatch emits a `tool_call` child span with `gen_ai.tool.name`,
//! `gen_ai.tool.call.id`, a truncated args preview, and a truncated result preview.
//! Skipped calls (e.g. `human_ask` intercepted by `EvalHook`) are marked
//! `tool.skipped = true` so they're still visible in the trace.

pub mod hook;
pub mod types;

mod anthropic;
mod openai;
mod openrouter;

pub use hook::{HookAction, LoopHook, NoopHook};
pub use types::{Message, ToolCall, ToolDef, Usage};

use anyhow::Result;
use std::{collections::HashMap, sync::Arc};
use that_tools::ThatToolsConfig;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, info_span, Instrument};

use crate::tools::typed::dispatch as dispatch_tool;

/// Max chars kept for tool args / result previews recorded on spans.
const TRACE_PREVIEW_CHARS: usize = 400;
/// Max chars kept for richer LLM input/output payload previews.
const TRACE_LLM_IO_CHARS: usize = 4_000;
/// Max chars logged for tool call args/results in app logs (compact single-line).
const TOOL_LOG_PREVIEW_CHARS: usize = 120;

// ─── Configuration ────────────────────────────────────────────────────────────

/// Context needed to execute tool calls — policies and sandbox routing.
pub struct ToolContext {
    pub config: ThatToolsConfig,
    pub container: Option<String>,
    pub skill_roots: Vec<std::path::PathBuf>,
    pub cluster_registry: Option<std::sync::Arc<that_plugins::cluster::ClusterRegistry>>,
    pub channel_registry: Option<std::sync::Arc<that_channels::registry::DynamicChannelRegistry>>,
    pub router: Option<std::sync::Arc<that_channels::ChannelRouter>>,
    pub route_registry: Option<std::sync::Arc<that_channels::DynamicRouteRegistry>>,
    /// Agent state directory for audit logging. When `None`, audit is silently skipped.
    pub state_dir: Option<std::path::PathBuf>,
}

/// All parameters for a single `run()` invocation.
pub struct LoopConfig {
    pub provider: String,
    pub model: String,
    /// API key for the provider (read from env by the caller).
    pub api_key: String,
    /// Fully assembled system prompt / preamble.
    pub system: String,
    pub max_tokens: u32,
    pub max_turns: u32,
    /// Tool schemas sent to the LLM.
    pub tools: Vec<ToolDef>,
    /// Pre-existing conversation history injected before `task`.
    pub history: Vec<Message>,
    /// Enable prompt caching (Anthropic and OpenRouter; ignored for OpenAI).
    pub prompt_caching: bool,
    /// OpenAI transport mode: true = WebSocket (default), false = HTTP streaming.
    pub openai_websocket: bool,
    /// Print tool call / result debug info to stderr.
    pub debug: bool,
    /// Tool execution context.
    pub tool_ctx: ToolContext,
    /// Images to attach to the current turn's user message (data, mime_type).
    pub images: Vec<(Vec<u8>, String)>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Run the multi-turn agentic loop for one task.
///
/// Returns `(final_text, aggregated_usage)` on success.
/// Errors on unsupported provider, API failures, or max turns exceeded.
pub async fn run(config: &LoopConfig, task: &str, hook: &dyn LoopHook) -> Result<(String, Usage)> {
    let mut messages: Vec<Message> = config.history.clone();
    messages.push(Message::User {
        content: task.into(),
        images: config.images.clone(),
    });
    let mut total_usage = Usage::default();
    let mut pending_edit_verification: HashMap<String, String> = HashMap::new();
    let openai_session: Option<Arc<Mutex<openai::OpenAiWsState>>> =
        if config.provider == "openai" && config.openai_websocket {
            Some(openai::new_ws_session())
        } else {
            None
        };

    for turn in 0..config.max_turns {
        info!(
            turn = turn + 1,
            max_turns = config.max_turns,
            provider = %config.provider,
            model = %config.model,
            ">>> turn {}/{}", turn + 1, config.max_turns
        );
        let turn_input_preview = llm_input_preview(config, &messages, turn + 1);

        // ── LLM request span ─────────────────────────────────────────────────
        // Emit both GenAI semantic-convention attributes and OpenInference
        // attributes so Phoenix can classify spans and compute costs.
        let turn_span = info_span!(
            "llm_turn",
            otel.name = format!("llm:{}", config.provider),
            openinference.span.kind = "LLM",
            gen_ai.system = %config.provider,
            gen_ai.provider.name = %config.provider,
            gen_ai.request.model = %config.model,
            gen_ai.operation.name = "chat",
            turn = turn + 1,
            gen_ai.prompt = %turn_input_preview,
            gen_ai.completion = tracing::field::Empty,
            llm.provider = %config.provider,
            llm.model_name = %config.model,
            input.value = %turn_input_preview,
            input.mime_type = "application/json",
            output.value = tracing::field::Empty,
            output.mime_type = "application/json",
            // Token usage — filled after the streaming turn completes.
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            llm.token_count.prompt = tracing::field::Empty,
            llm.token_count.completion = tracing::field::Empty,
            llm.token_count.total = tracing::field::Empty,
            gen_ai.cache_read_input_tokens = tracing::field::Empty,
            gen_ai.cache_write_input_tokens = tracing::field::Empty,
            tool_calls = tracing::field::Empty,
        );

        // Keep a clone so we can record post-await fields on the same span.
        let (text, tool_calls, usage) = run_turn(config, &messages, hook, openai_session.clone())
            .instrument(turn_span.clone())
            .await?;

        let llm_output_preview = llm_output_preview(&text, &tool_calls);
        let completion = if text.is_empty() && !tool_calls.is_empty() {
            llm_output_preview.as_str()
        } else {
            text.as_str()
        };
        turn_span.record("gen_ai.completion", completion);
        turn_span.record("output.value", llm_output_preview.as_str());
        turn_span.record("gen_ai.usage.input_tokens", usage.input_tokens);
        turn_span.record("gen_ai.usage.output_tokens", usage.output_tokens);
        turn_span.record("llm.token_count.prompt", usage.input_tokens);
        turn_span.record("llm.token_count.completion", usage.output_tokens);
        turn_span.record(
            "llm.token_count.total",
            usage.input_tokens + usage.output_tokens,
        );
        if usage.cache_read_tokens > 0 {
            turn_span.record("gen_ai.cache_read_input_tokens", usage.cache_read_tokens);
        }
        if usage.cache_write_tokens > 0 {
            turn_span.record("gen_ai.cache_write_input_tokens", usage.cache_write_tokens);
        }
        turn_span.record("tool_calls", tool_calls.len() as u64);

        total_usage = total_usage.add(&usage);
        info!(
            turn = turn + 1,
            tool_calls = tool_calls.len(),
            in_tok = usage.input_tokens,
            out_tok = usage.output_tokens,
            "<<< turn {}/{} calls={} tok={}/{}",
            turn + 1,
            config.max_turns,
            tool_calls.len(),
            usage.input_tokens,
            usage.output_tokens
        );

        if tool_calls.is_empty() {
            if !pending_edit_verification.is_empty() {
                let mut pending_files: Vec<String> =
                    pending_edit_verification.keys().cloned().collect();
                pending_files.sort();
                messages.push(Message::assistant(text.clone()));
                messages.push(Message::user(format!(
                    "<system-reminder>\n\
                     edit_verification_required: true\n\
                     requirement: Before finalizing, call code_read on each edited file to verify actual on-disk content.\n\
                     pending_files: {}\n\
                     </system-reminder>",
                    pending_files.join(", ")
                )));
                continue;
            }
            return Ok((text, total_usage));
        }

        messages.push(Message::Assistant {
            content: text,
            tool_calls: tool_calls.clone(),
        });

        // Execute each tool call — each gets its own child span.
        for tc in &tool_calls {
            let args_preview = truncate_chars(&tc.args_json, TRACE_PREVIEW_CHARS);
            let logged_args = compact_oneliner(&tc.args_json, TOOL_LOG_PREVIEW_CHARS);
            info!(
                tool = %tc.name,
                call_id = %tc.call_id,
                " → {}: {logged_args}", tc.name
            );

            if let Some(ref sd) = config.tool_ctx.state_dir {
                let args_short: String = tc.args_json.chars().take(120).collect();
                crate::audit::log_event(sd, "tool_call", &format!("{}: {args_short}", tc.name));
            }

            let tool_span = info_span!(
                "tool_call",
                otel.name = %format!("tool:{}", tc.name),
                openinference.span.kind = "TOOL",
                gen_ai.tool.name = %tc.name,
                gen_ai.tool.call.id = %tc.call_id,
                gen_ai.operation.name = "execute_tool",
                tool.name = %tc.name,
                input.value = %args_preview,
                input.mime_type = "application/json",
                tool.skipped = false,
                tool.guard_reason = tracing::field::Empty,
                output.value = tracing::field::Empty,
                output.mime_type = "application/json",
            );

            let action = hook
                .on_tool_call(&tc.name, &tc.call_id, &tc.args_json)
                .await;

            let result = match action {
                HookAction::Skip { result_json } => {
                    // Brief synchronous entry so the span appears in the trace.
                    tool_span.in_scope(|| {
                        tracing::Span::current().record("tool.skipped", true);
                    });
                    result_json
                }
                HookAction::Continue => {
                    if tc.name == "code_edit" {
                        if let Some(edit_path) = tool_arg_path(&tc.args_json) {
                            if let Some(previous_call_id) =
                                pending_edit_verification.get(&edit_path).cloned()
                            {
                                tool_span.in_scope(|| {
                                    tracing::Span::current().record(
                                        "tool.guard_reason",
                                        "edit_requires_read_verification",
                                    );
                                });
                                edit_verification_guard_result(&edit_path, &previous_call_id)
                            } else {
                                let dispatched =
                                    dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                                        .instrument(tool_span.clone())
                                        .await;
                                if !is_tool_error_result(&dispatched) {
                                    pending_edit_verification.insert(edit_path, tc.call_id.clone());
                                }
                                dispatched
                            }
                        } else {
                            dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                                .instrument(tool_span.clone())
                                .await
                        }
                    } else {
                        let dispatched = dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                            .instrument(tool_span.clone())
                            .await;
                        if tc.name == "code_read" && !is_tool_error_result(&dispatched) {
                            if let Some(read_path) = tool_arg_path(&tc.args_json) {
                                pending_edit_verification.remove(&read_path);
                            }
                        }
                        dispatched
                    }
                }
            };

            let result_preview = truncate_chars(&result, TRACE_PREVIEW_CHARS);
            tool_span.record("output.value", result_preview.as_str());
            let is_error = is_tool_error_result(&result);
            let result_chars = result.chars().count();
            let status_str = if is_error {
                let snippet = compact_oneliner(&result, 60);
                format!("ERR  {snippet}")
            } else {
                "ok".to_string()
            };
            info!(
                tool = %tc.name,
                call_id = %tc.call_id,
                is_error,
                result_chars,
                " ← {}: {status_str}  ({result_chars} chars)", tc.name
            );

            hook.on_tool_result(&tc.name, &tc.call_id, &result).await;
            messages.push(Message::Tool {
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                content: result,
            });
        }

        debug!(turn = turn + 1, "Loop turn complete, continuing");
    }

    Err(anyhow::anyhow!("max turns ({}) reached", config.max_turns))
}

/// Convenience helper for a single non-tool-use LLM call (no loop, no hooks).
///
/// Used for one-shot tasks like `generate_soul_md` and the LLM judge.
pub async fn complete_once(
    provider: &str,
    model: &str,
    api_key: &str,
    system: &str,
    user_prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let input_preview = trace_json_preview(serde_json::json!({
        "provider": provider,
        "model": model,
        "operation": "chat",
        "system": system,
        "messages": [{"role": "user", "content": user_prompt}],
    }));

    let span = info_span!(
        "llm_once",
        otel.name = format!("llm:{provider}"),
        openinference.span.kind = "LLM",
        gen_ai.system = %provider,
        gen_ai.provider.name = %provider,
        gen_ai.request.model = %model,
        gen_ai.operation.name = "chat",
        gen_ai.prompt = %input_preview,
        gen_ai.completion = tracing::field::Empty,
        llm.provider = %provider,
        llm.model_name = %model,
        input.value = %input_preview,
        input.mime_type = "application/json",
        output.value = tracing::field::Empty,
        output.mime_type = "application/json",
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        llm.token_count.prompt = tracing::field::Empty,
        llm.token_count.completion = tracing::field::Empty,
        llm.token_count.total = tracing::field::Empty,
    );

    complete_once_inner(provider, model, api_key, system, user_prompt, max_tokens)
        .instrument(span)
        .await
}

async fn complete_once_inner(
    provider: &str,
    model: &str,
    api_key: &str,
    system: &str,
    user_prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<anthropic::TurnEvent>(256);
    let messages = vec![Message::user(user_prompt)];

    match provider {
        "anthropic" => {
            let tools: Vec<ToolDef> = vec![];
            tokio::spawn({
                let api_key = api_key.to_string();
                let model = model.to_string();
                let system = system.to_string();
                let messages = messages.clone();
                async move {
                    let _ = anthropic::stream_turn(
                        &api_key, &model, &system, &messages, &tools, max_tokens, false, tx,
                    )
                    .await;
                }
            });
        }
        "openai" => {
            let openai_websocket = openai_websocket_enabled();
            let tools: Vec<ToolDef> = vec![];
            tokio::spawn({
                let api_key = api_key.to_string();
                let model = model.to_string();
                let system = system.to_string();
                let messages = messages.clone();
                async move {
                    let _ = openai::stream_turn(
                        &api_key,
                        &model,
                        &system,
                        &messages,
                        &tools,
                        max_tokens,
                        tx,
                        openai_websocket,
                        None,
                    )
                    .await;
                }
            });
        }
        "openrouter" => {
            let tools: Vec<ToolDef> = vec![];
            tokio::spawn({
                let api_key = api_key.to_string();
                let model = model.to_string();
                let system = system.to_string();
                let messages = messages.clone();
                async move {
                    let _ = openrouter::stream_turn(
                        &api_key, &model, &system, &messages, &tools, max_tokens, true, tx,
                    )
                    .await;
                }
            });
        }
        other => return Err(anyhow::anyhow!("Unsupported provider: {other}")),
    }

    let mut text = String::new();
    let mut usage = Usage::default();
    while let Some(event) = rx.recv().await {
        match event {
            anthropic::TurnEvent::TextDelta(d) => text.push_str(&d),
            anthropic::TurnEvent::TurnEnd {
                full_text,
                usage: u,
            } => {
                if text.is_empty() {
                    text = full_text;
                }
                usage = u;
                break;
            }
            anthropic::TurnEvent::Error(e) => return Err(e),
            _ => {}
        }
    }

    let span = tracing::Span::current();
    let output_preview = trace_json_preview(serde_json::json!({
        "text": text,
    }));
    span.record("gen_ai.completion", text.as_str());
    span.record("output.value", output_preview.as_str());
    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
    span.record("llm.token_count.prompt", usage.input_tokens);
    span.record("llm.token_count.completion", usage.output_tokens);
    span.record(
        "llm.token_count.total",
        usage.input_tokens + usage.output_tokens,
    );

    Ok(text)
}

// ─── Internal ─────────────────────────────────────────────────────────────────

/// Run one streaming provider turn, collect text + tool calls, drive hooks.
async fn run_turn(
    config: &LoopConfig,
    messages: &[Message],
    hook: &dyn LoopHook,
    openai_session: Option<Arc<Mutex<openai::OpenAiWsState>>>,
) -> Result<(String, Vec<ToolCall>, Usage)> {
    let (tx, mut rx) = mpsc::channel::<anthropic::TurnEvent>(512);
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = Usage::default();

    // Spawn the provider streaming task.
    let provider_task = {
        let api_key = config.api_key.clone();
        let model = config.model.clone();
        let system = config.system.clone();
        let messages = messages.to_vec();
        let tools = config.tools.clone();
        let max_tokens = config.max_tokens;
        let caching = config.prompt_caching;
        let provider = config.provider.clone();
        let openai_websocket = config.openai_websocket;
        let openai_session = openai_session.clone();

        tokio::spawn(async move {
            match provider.as_str() {
                "anthropic" => {
                    anthropic::stream_turn(
                        &api_key, &model, &system, &messages, &tools, max_tokens, caching, tx,
                    )
                    .await
                }
                "openai" => {
                    openai::stream_turn(
                        &api_key,
                        &model,
                        &system,
                        &messages,
                        &tools,
                        max_tokens,
                        tx,
                        openai_websocket,
                        openai_session,
                    )
                    .await
                }
                "openrouter" => {
                    openrouter::stream_turn(
                        &api_key, &model, &system, &messages, &tools, max_tokens, caching, tx,
                    )
                    .await
                }
                other => Err(anyhow::anyhow!("Unsupported provider: {other}")),
            }
        })
    };

    // Process events as they arrive — drives hook callbacks live.
    let mut in_thinking = false;
    while let Some(event) = rx.recv().await {
        match event {
            anthropic::TurnEvent::TextDelta(d) => {
                if in_thinking {
                    in_thinking = false;
                    eprintln!();
                }
                text.push_str(&d);
                hook.on_text_delta(&d).await;
            }
            anthropic::TurnEvent::ReasoningDelta(d) => {
                if !in_thinking {
                    eprintln!("\x1b[2m[thinking]\x1b[0m");
                    in_thinking = true;
                }
                if !d.is_empty() {
                    eprint!("\x1b[2m{d}\x1b[0m");
                }
                hook.on_reasoning_delta(&d).await;
            }
            anthropic::TurnEvent::ToolCallComplete(tc) => {
                if config.debug {
                    eprintln!("\x1b[36m[tool call] {} {}\x1b[0m", tc.name, tc.args_json);
                }
                tool_calls.push(tc);
            }
            anthropic::TurnEvent::TurnEnd {
                full_text,
                usage: u,
            } => {
                if text.is_empty() {
                    text = full_text;
                }
                usage = u;
            }
            anthropic::TurnEvent::Error(e) => {
                return Err(e);
            }
        }
    }

    // Wait for the provider task to finish and surface any errors.
    provider_task.await??;

    Ok((text, tool_calls, usage))
}

fn llm_input_preview(config: &LoopConfig, messages: &[Message], turn_number: u32) -> String {
    trace_json_preview(serde_json::json!({
        "provider": config.provider,
        "model": config.model,
        "operation": "chat",
        "turn": turn_number,
        "system": config.system,
        "messages": messages_to_trace(messages),
    }))
}

fn openai_websocket_enabled() -> bool {
    std::env::var("THAT_OPENAI_WEBSOCKET")
        .ok()
        .map(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(true)
}

fn llm_output_preview(text: &str, tool_calls: &[ToolCall]) -> String {
    trace_json_preview(serde_json::json!({
        "text": text,
        "tool_calls": tool_calls_to_trace(tool_calls),
    }))
}

fn messages_to_trace(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::User { content, .. } => out.push(serde_json::json!({
                "role": "user",
                "content": content,
            })),
            Message::Assistant {
                content,
                tool_calls,
            } => out.push(serde_json::json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls_to_trace(tool_calls),
            })),
            Message::Tool {
                call_id,
                name,
                content,
            } => out.push(serde_json::json!({
                "role": "tool",
                "call_id": call_id,
                "name": name,
                "content": content,
            })),
        }
    }
    out
}

fn tool_calls_to_trace(tool_calls: &[ToolCall]) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        let args = serde_json::from_str::<serde_json::Value>(&tc.args_json)
            .unwrap_or_else(|_| serde_json::Value::String(tc.args_json.clone()));
        out.push(serde_json::json!({
            "call_id": tc.call_id,
            "name": tc.name,
            "args": args,
        }));
    }
    out
}

fn trace_json_preview(value: serde_json::Value) -> String {
    truncate_chars(&value.to_string(), TRACE_LLM_IO_CHARS)
}

/// Produce a compact single-line preview: strips control characters (replacing
/// them with a space), collapses consecutive whitespace, and truncates to
/// `max_chars` visible characters with a `…` suffix when cut.
fn compact_oneliner(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut prev_space = false;
    for ch in s.chars() {
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

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
    }
    out
}

fn tool_arg_path(args_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let raw = value.get("path")?.as_str()?.trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

fn is_tool_error_result(result_json: &str) -> bool {
    let value: serde_json::Value = match serde_json::from_str(result_json) {
        Ok(v) => v,
        Err(_) => return true,
    };
    if value.get("error").is_some() {
        return true;
    }
    if value.get("timed_out").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return true;
    }
    if let Some(code) = value.get("exit_code").and_then(|v| v.as_i64()) {
        return code != 0;
    }
    false
}

fn edit_verification_guard_result(path: &str, previous_call_id: &str) -> String {
    serde_json::json!({
        "error": format!(
            "code_edit blocked: file '{}' was already edited by {}. Run code_read on this file first to verify current content, then apply the next edit.",
            path, previous_call_id
        )
    })
    .to_string()
}
