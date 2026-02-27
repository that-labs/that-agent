//! OpenRouter Chat Completions API streaming client.
//!
//! Sends a single assistant turn request to the OpenRouter API and emits
//! `TurnEvent`s to a channel. Uses the standard Chat Completions SSE format
//! with tool call support — the universally-supported format on OpenRouter.
//!
//! Note: OpenRouter also offers a beta Responses API, but Chat Completions is
//! the stable path that works reliably across all models and multi-turn
//! conversations with tool calls.

use anyhow::Result;
use tokio::sync::mpsc;

use super::anthropic::TurnEvent;
use super::types::{Message, ToolCall, ToolDef, Usage};

/// Execute one streaming turn against the OpenRouter Chat Completions API.
///
/// Sends `TurnEvent`s to `tx` until the stream is exhausted.
/// Returns the `Usage` reported by the server.
pub(super) async fn stream_turn(
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    prompt_caching: bool,
    tx: mpsc::Sender<TurnEvent>,
) -> Result<Usage> {
    use futures::StreamExt;

    let body = build_request(system, messages, tools, model, max_tokens, prompt_caching);

    let client = reqwest::Client::new();
    let response = client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("OpenRouter API error {status}: {body}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut usage = Usage::default();

    // In-progress tool call slots, keyed by index within a single response.
    // Each slot: (call_id, name, accumulated_args).
    let mut tool_slots: std::collections::HashMap<u64, (String, String, String)> =
        std::collections::HashMap::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        loop {
            let Some(nl) = buf.find('\n') else { break };
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                // Flush any remaining tool slots and send TurnEnd.
                flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;
                return Ok(usage);
            }

            let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
                tracing::debug!(provider = "openrouter", "SSE unparseable: {data}");
                continue;
            };

            // Check for error responses embedded in the stream.
            if let Some(err) = val.get("error") {
                let msg = err["message"]
                    .as_str()
                    .or_else(|| err.as_str())
                    .unwrap_or("unknown error");
                let message = format!("OpenRouter API stream error: {msg}");
                let _ = tx
                    .send(TurnEvent::Error(anyhow::anyhow!("{message}")))
                    .await;
                return Err(anyhow::anyhow!("{message}"));
            }

            // Extract usage from the final chunk (enabled via stream_options.include_usage).
            if let Some(u) = val.get("usage").and_then(|u| u.as_object()) {
                usage.input_tokens =
                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                usage.output_tokens = u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                // Parse cached token details (OpenRouter prompt caching).
                if let Some(details) = u.get("prompt_tokens_details").and_then(|d| d.as_object()) {
                    if let Some(cached) = details.get("cached_tokens").and_then(|v| v.as_u64()) {
                        usage.cache_read_tokens = cached as u32;
                    }
                    if let Some(written) =
                        details.get("cache_write_tokens").and_then(|v| v.as_u64())
                    {
                        usage.cache_write_tokens = written as u32;
                    }
                }
                // Log raw usage for cache diagnostics — helps verify OpenRouter
                // includes prompt_tokens_details in streaming responses.
                let usage_json = serde_json::Value::Object(u.clone());
                tracing::debug!(
                    provider = "openrouter",
                    model = model,
                    "OpenRouter usage: {usage_json}"
                );
            }

            // Process choices[0].
            let Some(choice) = val.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };

            // Some models/providers send `delta` (standard streaming), others may
            // send a complete `message` object. Handle both.
            let delta = choice.get("delta").or_else(|| choice.get("message"));

            if let Some(delta) = delta {
                // Text content.
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        full_text.push_str(content);
                        let _ = tx.send(TurnEvent::TextDelta(content.to_string())).await;
                    }
                }

                // Tool calls — streamed as incremental deltas with an index field.
                if let Some(tc_arr) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tc_arr {
                        let idx = tc["index"].as_u64().unwrap_or(0);

                        // A delta with `id` (string) marks the start of a new tool call.
                        // A delta with `id: null` or missing `id` is a continuation.
                        let is_new = tc.get("id").and_then(|v| v.as_str()).is_some();

                        if is_new {
                            let call_id = tc["id"].as_str().unwrap_or("").to_string();
                            let name = tc
                                .pointer("/function/name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let initial_args = tc
                                .pointer("/function/arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            tool_slots.insert(idx, (call_id, name, initial_args));
                        } else if let Some(slot) = tool_slots.get_mut(&idx) {
                            // Accumulate argument fragments.
                            if let Some(args_delta) =
                                tc.pointer("/function/arguments").and_then(|v| v.as_str())
                            {
                                slot.2.push_str(args_delta);
                            }
                            // Some models send the name in a continuation delta.
                            if let Some(name) =
                                tc.pointer("/function/name").and_then(|v| v.as_str())
                            {
                                if !name.is_empty() && slot.1.is_empty() {
                                    slot.1 = name.to_string();
                                }
                            }
                        }
                    }
                }
            }

            // Any finish_reason flushes accumulated tool slots and emits TurnEnd.
            // Different models/providers use different values: "tool_calls" (standard),
            // "function_call" (legacy), "stop" (some send this even with tool calls).
            if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
                // Flush all tool slots as ToolCallComplete events.
                for (_, (call_id, name, args)) in std::mem::take(&mut tool_slots) {
                    if !name.is_empty() {
                        let _ = tx
                            .send(TurnEvent::ToolCallComplete(ToolCall {
                                call_id,
                                name,
                                args_json: args,
                            }))
                            .await;
                    }
                }
                let _ = tx
                    .send(TurnEvent::TurnEnd {
                        full_text: full_text.clone(),
                        usage: usage.clone(),
                    })
                    .await;
            }
        }
    }

    // Stream ended without [DONE] — flush and send TurnEnd.
    flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;

    Ok(usage)
}

async fn flush_pending_turn(
    tx: &mpsc::Sender<TurnEvent>,
    full_text: &str,
    usage: &Usage,
    tool_slots: &mut std::collections::HashMap<u64, (String, String, String)>,
) {
    for (_, (call_id, name, args)) in std::mem::take(tool_slots) {
        if !name.is_empty() {
            let _ = tx
                .send(TurnEvent::ToolCallComplete(ToolCall {
                    call_id,
                    name,
                    args_json: args,
                }))
                .await;
        }
    }
    let _ = tx
        .send(TurnEvent::TurnEnd {
            full_text: full_text.to_string(),
            usage: usage.clone(),
        })
        .await;
}

/// Build the Chat Completions request JSON body for OpenRouter.
fn build_request(
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    model: &str,
    max_tokens: u32,
    prompt_caching: bool,
) -> String {
    let chat_messages = messages_to_chat_completions(system, messages, prompt_caching);

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": chat_messages,
        // Force OpenRouter to only route to providers that support all
        // parameters we send (especially `tools`). Without this, cheaper
        // providers that don't support tool calling may be selected.
        "provider": {
            "require_parameters": true,
        },
    });

    if !tools.is_empty() {
        let mut tools_json: Vec<serde_json::Value> =
            tools.iter().map(tool_to_chat_completions).collect();
        // Add cache_control to the last tool to cache the whole tool block.
        if prompt_caching {
            if let Some(last) = tools_json.last_mut() {
                last["cache_control"] = serde_json::json!({ "type": "ephemeral" });
            }
        }
        body["tools"] = serde_json::Value::Array(tools_json);
    }

    body.to_string()
}

/// Convert a tool definition to the Chat Completions format (function wrapper).
fn tool_to_chat_completions(t: &ToolDef) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

/// Convert our `Message` list to Chat Completions wire format.
///
/// Prepends a system message, then maps each internal message type to the
/// corresponding Chat Completions role.
fn messages_to_chat_completions(
    system: &str,
    messages: &[Message],
    prompt_caching: bool,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(messages.len() + 1);

    // System message first — use content array with cache_control when caching is enabled.
    if prompt_caching {
        out.push(serde_json::json!({
            "role": "system",
            "content": [{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" }
            }],
        }));
    } else {
        out.push(serde_json::json!({
            "role": "system",
            "content": system,
        }));
    }

    for msg in messages {
        match msg {
            Message::User { content } => {
                out.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut assistant_msg = serde_json::json!({
                    "role": "assistant",
                });
                if !content.is_empty() {
                    assistant_msg["content"] = serde_json::Value::String(content.clone());
                }
                if !tool_calls.is_empty() {
                    let tc_json: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.call_id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.args_json,
                                }
                            })
                        })
                        .collect();
                    assistant_msg["tool_calls"] = serde_json::Value::Array(tc_json);
                }
                out.push(assistant_msg);
            }
            Message::Tool {
                call_id, content, ..
            } => {
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
            }
        }
    }

    out
}
