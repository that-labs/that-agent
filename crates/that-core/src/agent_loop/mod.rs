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
mod openai_compatible;
mod openrouter;

pub use hook::{HookAction, LoopHook, NoopHook};
pub use types::{Message, ToolCall, ToolDef, Usage};

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use that_tools::ThatToolsConfig;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, info_span, warn, Instrument};

use crate::tools::typed::dispatch as dispatch_tool;

/// Max seconds to wait for the next SSE/WS chunk before treating the stream as stalled.
/// Applies to all providers (Anthropic, OpenRouter, OpenAI HTTP).
/// 90s is generous but needed — extended thinking on large contexts can pause before
/// the first delta. Anthropic sends keepalive comments during thinking, but transient
/// API issues may produce genuine 90s silences that trigger retries.
pub(super) const STREAM_IDLE_TIMEOUT_SECS: u64 = 90;

/// Shared HTTP client with connect timeout only. No total-request timeout — SSE
/// streams can run for minutes. Idle connections expire after 30s to avoid reusing
/// stale connections after a mid-stream failure.
pub(super) fn llm_http_client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(4)
            .pool_idle_timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Max chars kept for tool args / result previews recorded on spans.
const TRACE_PREVIEW_CHARS: usize = 400;
/// Max chars kept for richer LLM input/output payload previews.
const TRACE_LLM_IO_CHARS: usize = 4_000;
/// Max chars logged for tool call args/results in app logs (compact single-line).
const TOOL_LOG_PREVIEW_CHARS: usize = 120;
/// Prefix injected into steering hint messages so the LLM can identify them.
pub const STEERING_HINT_PREFIX: &str = "[hint]:";

/// Shared type for the optional mid-run steering queue.
pub type SteeringQueue = Arc<Mutex<Vec<String>>>;

/// Captured loop state when a run fails after some turns already completed.
///
/// `messages` contains the fully checkpointed conversation history up to the
/// last successfully completed tool/result step, so callers can retry without
/// replaying side effects from earlier turns.
pub struct InterruptedRun {
    pub error: anyhow::Error,
    pub messages: Vec<Message>,
    pub usage: Usage,
}

/// Hard ceiling on tool result chars allowed into conversation context.
/// ~8K tokens at ~4 chars/token. Generous for structured data, fatal for base64.
const MAX_TOOL_RESULT_CHARS: usize = 32_000;
/// Minimum consecutive base64-alphabet chars to flag a blob.
const BASE64_BLOB_MIN_LEN: usize = 1_000;
/// Fraction of context window at which we warn the agent to compact.
const CONTEXT_WARN_FRACTION: f64 = 0.70;
/// Fraction of context window at which compaction is mandatory.
const CONTEXT_CRITICAL_FRACTION: f64 = 0.85;

/// Tools classified as exploration — consecutive turns using only these trigger anti-loop.
const EXPLORATION_TOOLS: &[&str] = &[
    "shell_exec",
    "fs_ls",
    "fs_cat",
    "code_grep",
    "code_search",
    "code_read",
];
/// Soft warning threshold — inject a nudge after this many exploration-only turns.
const EXPLORATION_SOFT_LIMIT: u32 = 8;
/// Hard limit — force the agent to stop exploring and report.
const EXPLORATION_HARD_LIMIT: u32 = 12;

// ─── Configuration ────────────────────────────────────────────────────────────

/// Context needed to execute tool calls — policies and sandbox routing.
#[derive(Clone)]
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
    /// Agent name for structured run logging.
    pub agent_name: String,
}

impl ToolContext {
    /// Return the agent name for use as sender_id in inter-agent communication.
    pub fn sender_name(&self) -> &str {
        if self.agent_name.is_empty() {
            "parent"
        } else {
            &self.agent_name
        }
    }
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
    /// Optional steering queue: mid-run hints from the human, drained each turn.
    pub steering: Option<SteeringQueue>,
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Run the multi-turn agentic loop for one task.
///
/// Returns `(final_text, aggregated_usage)` on success.
/// Errors on unsupported provider, API failures, or max turns exceeded.
pub async fn run(config: &LoopConfig, task: &str, hook: &dyn LoopHook) -> Result<(String, Usage)> {
    let mut messages = config.history.clone();
    messages.push(Message::User {
        content: task.into(),
        images: config.images.clone(),
    });
    run_from_checkpoint(config, messages, hook)
        .await
        .map_err(|interrupted| interrupted.error)
}

/// Run the loop and preserve completed conversation state on failure.
pub async fn run_with_checkpoint(
    config: &LoopConfig,
    task: &str,
    hook: &dyn LoopHook,
) -> std::result::Result<(String, Usage), InterruptedRun> {
    let mut messages = config.history.clone();
    messages.push(Message::User {
        content: task.into(),
        images: config.images.clone(),
    });
    run_from_checkpoint(config, messages, hook).await
}

/// Resume the loop from an exact checkpointed message chain.
pub async fn resume_with_checkpoint(
    config: &LoopConfig,
    messages: Vec<Message>,
    hook: &dyn LoopHook,
) -> std::result::Result<(String, Usage), InterruptedRun> {
    run_from_checkpoint(config, messages, hook).await
}

async fn run_from_checkpoint(
    config: &LoopConfig,
    mut messages: Vec<Message>,
    hook: &dyn LoopHook,
) -> std::result::Result<(String, Usage), InterruptedRun> {
    // Log the user request to the structured run log.
    if let Some(ref sd) = config.tool_ctx.state_dir {
        let task = messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .unwrap_or("");
        crate::audit::log_run_event(sd, &config.tool_ctx.agent_name, "Input", "agent_loop", task);
    }

    let mut total_usage = Usage::default();
    let mut pending_edit_verification: HashMap<String, String> = HashMap::new();
    let openai_session: Option<Arc<Mutex<openai::OpenAiWsState>>> =
        if config.provider == "openai" && config.openai_websocket {
            Some(openai::new_ws_session())
        } else {
            None
        };

    // Turn-budget thresholds — loop-invariant.
    let budget_half = config.max_turns / 2;
    let budget_eighty = config.max_turns * 4 / 5;

    // Context window thresholds — derived from model's actual window size.
    let ctx_window = crate::model_catalog::context_window(&config.model);
    let context_warn_tokens = (ctx_window as f64 * CONTEXT_WARN_FRACTION) as u32;
    let context_critical_tokens = (ctx_window as f64 * CONTEXT_CRITICAL_FRACTION) as u32;

    // When resuming from a network retry checkpoint, count assistant messages
    // AFTER the last user message to continue from where we left off.
    // History messages (before the last user message) don't count as consumed turns.
    let turns_consumed = {
        let last_user = messages
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(0);
        messages[last_user..]
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count() as u32
    };

    // Anti-loop: track consecutive exploration-only turns.
    let mut exploration_streak: u32 = 0;
    let mut seen_calls: HashSet<u64> = HashSet::new();

    for turn in turns_consumed..config.max_turns {
        // Drain steering hints queued by the human between turns.
        if let Some(ref queue) = config.steering {
            let hints: Vec<String> = {
                let mut q = queue.lock().await;
                q.drain(..).collect()
            };
            if !hints.is_empty() {
                let merged = hints.join("\n");
                messages.push(Message::user(format!("{STEERING_HINT_PREFIX} {merged}")));
                hook.on_steering_picked_up().await;
                debug!("Injected {} steering hint(s)", hints.len());
            }
        }

        // ── Turn-budget reminders ────────────────────────────────────────────
        // Inject a lightweight system reminder at 50% and 80% of max_turns so the
        // agent has a live signal of how many turns remain — the baked-in preamble
        // instruction gets buried under tool history in long runs.
        let current = turn + 1;
        if config.max_turns >= 10 && (current == budget_half || current == budget_eighty) {
            let remaining = config.max_turns - current;
            messages.push(Message::user(format!(
                "<system-reminder>\nturn_budget: {current}/{} — {remaining} turns remaining. \
                 Prioritize completing the current objective. If you have open tasks, \
                 focus on finishing them before starting new work.\n</system-reminder>",
                config.max_turns
            )));
        }

        info!(
            turn = current,
            max_turns = config.max_turns,
            provider = %config.provider,
            model = %config.model,
            ">>> turn {current}/{}", config.max_turns
        );
        let turn_input_preview = llm_input_preview(config, &messages, current);

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
            turn = current,
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
        let (text, tool_calls, usage) =
            match run_turn(config, &messages, hook, openai_session.clone())
                .instrument(turn_span.clone())
                .await
            {
                Ok(ok) => ok,
                Err(error) => {
                    return Err(InterruptedRun {
                        error,
                        messages,
                        usage: total_usage,
                    });
                }
            };

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
        if usage.cache_read_tokens > 0 || usage.cache_write_tokens > 0 {
            info!(
                turn = current,
                tool_calls = tool_calls.len(),
                in_tok = usage.input_tokens,
                out_tok = usage.output_tokens,
                cache_read = usage.cache_read_tokens,
                cache_write = usage.cache_write_tokens,
                "<<< turn {current}/{} calls={} tok={}/{} cache=r:{}/w:{}",
                config.max_turns,
                tool_calls.len(),
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
            );
        } else {
            info!(
                turn = current,
                tool_calls = tool_calls.len(),
                in_tok = usage.input_tokens,
                out_tok = usage.output_tokens,
                "<<< turn {current}/{} calls={} tok={}/{}",
                config.max_turns,
                tool_calls.len(),
                usage.input_tokens,
                usage.output_tokens
            );
        }

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
            if let Some(ref sd) = config.tool_ctx.state_dir {
                crate::audit::log_run_event(
                    sd,
                    &config.tool_ctx.agent_name,
                    "Output",
                    "agent_loop",
                    &text,
                );
            }
            return Ok((text, total_usage));
        }

        messages.push(Message::Assistant {
            content: text,
            tool_calls: tool_calls.clone(),
        });

        // Execute tool calls — parallel-safe tools run concurrently, others sequentially.
        // Results are collected into a vec indexed by original position to preserve order.
        type ToolResult = (String, Vec<(Vec<u8>, String)>);
        let mut tool_results: Vec<Option<ToolResult>> = vec![None; tool_calls.len()];
        let mut terminal_output: Option<String> = None;

        // Partition: identify which tools can run in parallel.
        let parallel_indices: Vec<usize> = tool_calls
            .iter()
            .enumerate()
            .filter(|(_, tc)| is_parallel_safe(&tc.name))
            .map(|(i, _)| i)
            .collect();
        let sequential_indices: Vec<usize> = tool_calls
            .iter()
            .enumerate()
            .filter(|(_, tc)| !is_parallel_safe(&tc.name))
            .map(|(i, _)| i)
            .collect();

        // Run parallel-safe tools concurrently.
        if parallel_indices.len() > 1 {
            // Pre-log all parallel tool calls (audit + tracing) before dispatching.
            for &i in &parallel_indices {
                let tc = &tool_calls[i];
                let logged_args = compact_oneliner(&tc.args_json, TOOL_LOG_PREVIEW_CHARS);
                info!(tool = %tc.name, call_id = %tc.call_id, " → {}: {logged_args}", tc.name);
                if let Some(ref sd) = config.tool_ctx.state_dir {
                    crate::audit::log_event(
                        sd,
                        "tool_call",
                        &format!("{}: {}", tc.name, tc.args_json),
                    );
                }
            }
            let parallel_futures: Vec<_> = parallel_indices
                .iter()
                .map(|&i| {
                    let tc = &tool_calls[i];
                    let tool_ctx = config.tool_ctx.clone();
                    let name = tc.name.clone();
                    let args = tc.args_json.clone();
                    async move {
                        let dr = dispatch_tool(&name, &args, &tool_ctx).await;
                        (i, dr.text, dr.images)
                    }
                })
                .collect();
            let results = futures::future::join_all(parallel_futures).await;
            for (i, text, images) in results {
                tool_results[i] = Some((text, images));
            }
        } else {
            // Only 0 or 1 parallel tool — run in the sequential path below.
        }

        // Run sequential tools (and any single parallel tool not yet executed).
        let remaining_parallel: Vec<usize> = parallel_indices
            .iter()
            .filter(|&&i| tool_results[i].is_none())
            .copied()
            .collect();
        for &i in sequential_indices.iter().chain(remaining_parallel.iter()) {
            let tc = &tool_calls[i];
            let args_preview = truncate_chars(&tc.args_json, TRACE_PREVIEW_CHARS);
            let logged_args = compact_oneliner(&tc.args_json, TOOL_LOG_PREVIEW_CHARS);
            info!(
                tool = %tc.name,
                call_id = %tc.call_id,
                " → {}: {logged_args}", tc.name
            );

            if let Some(ref sd) = config.tool_ctx.state_dir {
                crate::audit::log_event(sd, "tool_call", &format!("{}: {}", tc.name, tc.args_json));
                crate::audit::log_run_event(
                    sd,
                    &config.tool_ctx.agent_name,
                    "ToolCall",
                    "agent_loop",
                    &format!("{} {}", tc.name, tc.args_json),
                );
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

            let mut tool_images: Vec<(Vec<u8>, String)> = Vec::new();
            let result = if terminal_output.is_some() {
                r#"{"skipped":true,"reason":"run already finalized via answer"}"#.to_string()
            } else {
                match action {
                    HookAction::Skip { result_json } => {
                        tool_span.in_scope(|| {
                            tracing::Span::current().record("tool.skipped", true);
                        });
                        result_json
                    }
                    HookAction::Finish {
                        result_json,
                        output_text,
                    } => {
                        tool_span.in_scope(|| {
                            tracing::Span::current().record("tool.skipped", true);
                        });
                        terminal_output = Some(output_text);
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
                                    let dr =
                                        dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                                            .instrument(tool_span.clone())
                                            .await;
                                    tool_images = dr.images;
                                    if !is_tool_error_result(&dr.text) {
                                        pending_edit_verification
                                            .insert(edit_path, tc.call_id.clone());
                                    }
                                    dr.text
                                }
                            } else {
                                let dr = dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                                    .instrument(tool_span.clone())
                                    .await;
                                tool_images = dr.images;
                                dr.text
                            }
                        } else {
                            let dr = dispatch_tool(&tc.name, &tc.args_json, &config.tool_ctx)
                                .instrument(tool_span.clone())
                                .await;
                            tool_images = dr.images;
                            if tc.name == "code_read" && !is_tool_error_result(&dr.text) {
                                if let Some(read_path) = tool_arg_path(&tc.args_json) {
                                    pending_edit_verification.remove(&read_path);
                                }
                            }
                            dr.text
                        }
                    }
                }
            };

            let result_preview = truncate_chars(&result, TRACE_PREVIEW_CHARS);
            tool_span.record("output.value", result_preview.as_str());

            tool_results[i] = Some((result, tool_images));
        }

        // Post-process all results in original order: audit, logging, hook, message push.
        for (i, tc) in tool_calls.iter().enumerate() {
            let (result, tool_images) = tool_results[i]
                .take()
                .unwrap_or_else(|| (r#"{"error":"tool not executed"}"#.to_string(), vec![]));

            let is_error = is_tool_error_result(&result);
            if is_error {
                if let Some(ref sd) = config.tool_ctx.state_dir {
                    crate::audit::log_error(sd, &tc.name, &result, &tc.args_json);
                }
            }
            let result_chars = result.chars().count();
            if let Some(ref sd) = config.tool_ctx.state_dir {
                let status = if is_error { "error" } else { "ok" };
                crate::audit::log_run_event(
                    sd,
                    &config.tool_ctx.agent_name,
                    "ToolResult",
                    "agent_loop",
                    &format!(
                        "{} ({}, {} chars) {}",
                        tc.name, status, result_chars, &result
                    ),
                );
            }

            // Log for parallel tools that didn't log during execution
            if parallel_indices.len() > 1 && parallel_indices.contains(&i) {
                // Already logged during parallel execution
            } else {
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
            }

            hook.on_tool_result(&tc.name, &tc.call_id, &result).await;
            let content = sanitize_tool_result(&tc.name, &result);
            messages.push(Message::Tool {
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                content,
                images: tool_images,
            });
        }

        if let Some(output) = terminal_output {
            return Ok((output, total_usage));
        }

        // If context is getting large, warn the agent before its next turn so it
        // calls mem_compact proactively — preventing response truncation mid-tool-call.
        let pct = if ctx_window > 0 {
            (usage.input_tokens as f64 / ctx_window as f64 * 100.0) as u32
        } else {
            0
        };
        if usage.input_tokens > context_critical_tokens {
            warn!(
                input_tokens = usage.input_tokens,
                context_window = ctx_window,
                usage_pct = pct,
                "Context {pct}% full — injecting mandatory mem_compact"
            );
            messages.push(Message::user(format!(
                "<system-reminder>\ncontext_pressure: critical ({pct}% of {ctx_window} tokens)\n\
                 Your context window is {pct}% full. You MUST call mem_compact NOW as \
                 your very first action this turn — no other tool calls before it. Failure \
                 to compact will cause response truncation and lost work.\n</system-reminder>"
            )));
        } else if usage.input_tokens > context_warn_tokens {
            warn!(
                input_tokens = usage.input_tokens,
                context_window = ctx_window,
                usage_pct = pct,
                "Context {pct}% full — injecting mem_compact reminder"
            );
            messages.push(Message::user(format!(
                "<system-reminder>\ncontext_pressure: high ({pct}% of {ctx_window} tokens)\n\
                 Your context window is {pct}% full. Call mem_compact NOW as your first \
                 action this turn to preserve the session before the window fills and \
                 responses get truncated.\n</system-reminder>"
            )));
        }

        // ── Anti-loop: exploration streak detection ────────────────────────
        if !tool_calls.is_empty()
            && tool_calls
                .iter()
                .all(|tc| EXPLORATION_TOOLS.contains(&tc.name.as_str()))
        {
            exploration_streak += 1;
            // Repeated-call booster: hash (name, args) and add +1 for duplicates.
            for tc in &tool_calls {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                tc.name.hash(&mut hasher);
                tc.args_json.hash(&mut hasher);
                let h = hasher.finish();
                if !seen_calls.insert(h) {
                    exploration_streak += 1;
                }
            }
        } else {
            exploration_streak = 0;
            seen_calls.clear();
        }

        if exploration_streak >= EXPLORATION_HARD_LIMIT {
            warn!(
                streak = exploration_streak,
                "Anti-loop hard limit reached — forcing input_required"
            );
            messages.push(Message::user(
                "STOP exploring. You have exceeded the exploration limit. \
                 Report input_required with a specific question about what context is missing."
                    .to_string(),
            ));
        } else if exploration_streak >= EXPLORATION_SOFT_LIMIT {
            warn!(
                streak = exploration_streak,
                "Anti-loop soft warning — injecting nudge"
            );
            messages.push(Message::user(format!(
                "You have been exploring for {exploration_streak} turns without progress. \
                 Check if the information you need was provided in the task message or \
                 scratchpad. If you cannot find what you need, report input_required \
                 with a specific question about what is missing."
            )));
        }

        debug!(turn = current, "Loop turn complete, continuing");
    }

    Err(InterruptedRun {
        error: anyhow::anyhow!("max turns ({}) reached", config.max_turns),
        messages,
        usage: total_usage,
    })
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
        other => {
            let entry = crate::provider_registry::find_registered_provider(other)
                .ok_or_else(|| anyhow::anyhow!("Unsupported provider: {other}"))?;
            match entry.transport.as_str() {
                "openai_chat" => {
                    let tools: Vec<ToolDef> = vec![];
                    tokio::spawn({
                        let api_key = api_key.to_string();
                        let model = model.to_string();
                        let system = system.to_string();
                        let messages = messages.clone();
                        async move {
                            let _ = openai_compatible::stream_turn(
                                &entry, &api_key, &model, &system, &messages, &tools, max_tokens,
                                tx,
                            )
                            .await;
                        }
                    });
                }
                transport => {
                    return Err(anyhow::anyhow!(
                        "Unsupported provider transport '{transport}' for {other}"
                    ));
                }
            }
        }
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
                other => {
                    let entry = crate::provider_registry::find_registered_provider(other)
                        .ok_or_else(|| anyhow::anyhow!("Unsupported provider: {other}"))?;
                    match entry.transport.as_str() {
                        "openai_chat" => {
                            openai_compatible::stream_turn(
                                &entry, &api_key, &model, &system, &messages, &tools, max_tokens,
                                tx,
                            )
                            .await
                        }
                        transport => Err(anyhow::anyhow!(
                            "Unsupported provider transport '{transport}' for {other}"
                        )),
                    }
                }
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
                ..
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

// ─── Tool result sanitization ────────────────────────────────────────────────

/// Sanitize a tool result before it enters conversation history.
///
/// Strips base64 blobs (useless to the LLM). When a result exceeds the char
/// ceiling, returns a structured error directing the agent to re-call with
/// narrower parameters (offsets, line ranges, limits) instead of silently
/// feeding it truncated data that degrades reasoning.
fn sanitize_tool_result(name: &str, result: &str) -> String {
    // Fast path: small result with no base64 — return as-is.
    if result.len() <= MAX_TOOL_RESULT_CHARS && !likely_contains_base64(result) {
        return result.to_string();
    }

    // Try JSON-aware base64 stripping first.
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(result) {
        let stripped = strip_base64_blobs(&mut value);
        let cleaned = value.to_string();
        if stripped > 0 {
            tracing::warn!(
                tool = %name,
                blobs_stripped = stripped,
                original_chars = result.len(),
                cleaned_chars = cleaned.len(),
                "stripped base64 blobs from tool result"
            );
        }
        if cleaned.len() <= MAX_TOOL_RESULT_CHARS {
            return cleaned;
        }
        // Too large — return error with guidance instead of truncated garbage.
        tracing::warn!(tool = %name, original_chars = result.len(), "tool result exceeded size limit");
        return overflow_error(name, result.len());
    }

    // Non-JSON and too large — same: error with guidance.
    if result.len() > MAX_TOOL_RESULT_CHARS {
        tracing::warn!(tool = %name, original_chars = result.len(), "tool result exceeded size limit");
        return overflow_error(name, result.len());
    }

    result.to_string()
}

/// Build a structured error telling the agent to retry with narrower parameters.
fn overflow_error(tool_name: &str, original_chars: usize) -> String {
    let guidance = match tool_name {
        "code_read" => "Re-call with `line` and `end_line` to read a specific range.",
        "code_grep" => "Reduce `limit`, add `include`/`exclude` globs, or narrow the `path`.",
        "shell_exec" => "Pipe output through `head`, `tail`, or `grep` to limit result size.",
        "fs_cat" => "Use `code_read` with `line`/`end_line` instead for large files.",
        "fs_ls" => "Reduce `max_depth` or target a more specific subdirectory.",
        "code_tree" => "Reduce `depth` or target a more specific subdirectory.",
        "mem_recall" => "Reduce `limit` or use a more specific `query`.",
        "code_summary" => "Target a specific subdirectory instead of a broad path.",
        _ => "Re-call with more specific parameters to reduce output size.",
    };
    serde_json::json!({
        "error": "result_too_large",
        "original_chars": original_chars,
        "max_chars": MAX_TOOL_RESULT_CHARS,
        "action_required": guidance,
    })
    .to_string()
}

/// Quick byte scan: does the string contain a run of 1000+ base64-alphabet chars?
fn likely_contains_base64(s: &str) -> bool {
    let mut run = 0usize;
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' {
            run += 1;
            if run >= BASE64_BLOB_MIN_LEN {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Walk a JSON value and replace base64 blob strings with a size placeholder.
fn strip_base64_blobs(value: &mut serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) if is_base64_blob(s) => {
            let len = s.len();
            *value = serde_json::Value::String(format!("[base64 data stripped, {len} chars]"));
            1
        }
        serde_json::Value::Array(arr) => arr.iter_mut().map(strip_base64_blobs).sum(),
        serde_json::Value::Object(map) => map.values_mut().map(strip_base64_blobs).sum(),
        _ => 0,
    }
}

/// Check if a string is a base64 blob: long run, 95%+ base64-alphabet chars.
fn is_base64_blob(s: &str) -> bool {
    if s.len() < BASE64_BLOB_MIN_LEN {
        return false;
    }
    let b64_count = s
        .bytes()
        .filter(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/' || *b == b'=')
        .count();
    b64_count * 100 / s.len() >= 95
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

/// Tools that can safely run concurrently — they don't mutate local filesystem state
/// or depend on ordering with other tool calls in the same turn.
fn is_parallel_safe(name: &str) -> bool {
    matches!(
        name,
        "agent_run"
            | "agent_query"
            | "http_request"
            | "workspace_activity"
            | "workspace_diff"
            | "workspace_conflicts"
            | "agent_list"
            | "list_skills"
            | "read_skill"
    )
}

fn is_tool_error_result(result_json: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(result_json) {
        Ok(v) => crate::hooks::is_error_value(&v),
        Err(_) => true,
    }
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
