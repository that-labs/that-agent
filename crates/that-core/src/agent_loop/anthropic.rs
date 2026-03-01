//! Anthropic Messages API streaming client.
//!
//! Sends a single assistant turn request and emits `TurnEvent`s to a channel.
//! Handles prompt caching, extended thinking, tool definitions, and SSE parsing.

use anyhow::Result;
use base64::Engine as _;
use tokio::sync::mpsc;

use super::types::{Message, ToolCall, ToolDef, Usage};

/// Internal events emitted for a single provider turn.
///
/// The runner collects these to reconstruct the assistant message + tool calls
/// and to forward hook callbacks.
#[derive(Debug)]
pub(super) enum TurnEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallComplete(ToolCall),
    TurnEnd { full_text: String, usage: Usage },
    Error(anyhow::Error),
}

/// Execute one streaming turn against the Anthropic Messages API.
///
/// Sends `TurnEvent`s to `tx` until the stream is exhausted.
/// Returns the `Usage` reported by the server.
#[allow(clippy::too_many_arguments)]
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

    // Build request body.
    let body = build_request(system, messages, tools, model, max_tokens, prompt_caching);

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header(
            "anthropic-beta",
            "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14",
        )
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Anthropic API error {status}: {body}"));
    }

    // Parse SSE stream.
    let mut stream = response.bytes_stream();
    let mut buf = String::new();

    // Per-block state.
    #[derive(Default)]
    struct BlockState {
        kind: BlockKind,
        call_id: String,
        name: String,
        args_buf: String,
        text_buf: String,
    }
    #[derive(Default, PartialEq)]
    enum BlockKind {
        #[default]
        None,
        Text,
        Thinking,
        ToolUse,
    }

    let mut blocks: std::collections::HashMap<usize, BlockState> = Default::default();
    let mut full_text = String::new();
    let mut usage = Usage::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                break;
            }

            let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };

            match val["type"].as_str().unwrap_or("") {
                "message_start" => {
                    // Extract initial input token counts (including cache).
                    if let Some(u) = val.pointer("/message/usage") {
                        usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0) as u32;
                        usage.cache_read_tokens =
                            u["cache_read_input_tokens"].as_u64().unwrap_or(0) as u32;
                        usage.cache_write_tokens =
                            u["cache_creation_input_tokens"].as_u64().unwrap_or(0) as u32;
                    }
                }

                "content_block_start" => {
                    let idx = val["index"].as_u64().unwrap_or(0) as usize;
                    let cb = &val["content_block"];
                    let mut state = BlockState::default();
                    match cb["type"].as_str().unwrap_or("") {
                        "text" => {
                            state.kind = BlockKind::Text;
                        }
                        "thinking" => {
                            state.kind = BlockKind::Thinking;
                        }
                        "tool_use" => {
                            state.kind = BlockKind::ToolUse;
                            state.call_id = cb["id"].as_str().unwrap_or("").to_string();
                            state.name = cb["name"].as_str().unwrap_or("").to_string();
                        }
                        _ => {}
                    }
                    blocks.insert(idx, state);
                }

                "content_block_delta" => {
                    let idx = val["index"].as_u64().unwrap_or(0) as usize;
                    let delta = &val["delta"];
                    let Some(state) = blocks.get_mut(&idx) else {
                        continue;
                    };

                    match delta["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let text = delta["text"].as_str().unwrap_or("").to_string();
                            state.text_buf.push_str(&text);
                            full_text.push_str(&text);
                            let _ = tx.send(TurnEvent::TextDelta(text)).await;
                        }
                        "thinking_delta" => {
                            let thinking = delta["thinking"].as_str().unwrap_or("").to_string();
                            let _ = tx.send(TurnEvent::ReasoningDelta(thinking)).await;
                        }
                        "input_json_delta" => {
                            let part = delta["partial_json"].as_str().unwrap_or("");
                            state.args_buf.push_str(part);
                        }
                        _ => {}
                    }
                }

                "content_block_stop" => {
                    let idx = val["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(state) = blocks.remove(&idx) {
                        if state.kind == BlockKind::ToolUse && !state.name.is_empty() {
                            let _ = tx
                                .send(TurnEvent::ToolCallComplete(ToolCall {
                                    call_id: state.call_id,
                                    name: state.name,
                                    args_json: state.args_buf,
                                }))
                                .await;
                        }
                    }
                }

                "message_delta" => {
                    if let Some(u) = val.get("usage") {
                        usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as u32;
                    }
                }

                "message_stop" => {
                    let _ = tx
                        .send(TurnEvent::TurnEnd {
                            full_text: full_text.clone(),
                            usage: usage.clone(),
                        })
                        .await;
                }

                _ => {}
            }
        }
    }

    Ok(usage)
}

/// Build the Anthropic request JSON body.
fn build_request(
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    model: &str,
    max_tokens: u32,
    prompt_caching: bool,
) -> String {
    let system_block = if prompt_caching {
        serde_json::json!([{
            "type": "text",
            "text": system,
            "cache_control": { "type": "ephemeral" }
        }])
    } else {
        serde_json::json!([{ "type": "text", "text": system }])
    };

    // Convert tools to Anthropic format (input_schema instead of parameters).
    let tools_json: serde_json::Value = if prompt_caching && !tools.is_empty() {
        let mut arr: Vec<serde_json::Value> = tools.iter().map(tool_to_anthropic).collect();
        // Add cache_control to the last tool to cache the whole tool block.
        if let Some(last) = arr.last_mut() {
            last["cache_control"] = serde_json::json!({ "type": "ephemeral" });
        }
        serde_json::Value::Array(arr)
    } else {
        serde_json::Value::Array(tools.iter().map(tool_to_anthropic).collect())
    };

    let messages_json = messages_to_anthropic(messages);

    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": system_block,
        "messages": messages_json,
        "tools": tools_json,
    })
    .to_string()
}

fn tool_to_anthropic(t: &ToolDef) -> serde_json::Value {
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.parameters,
    })
}

/// Convert our `Message` list to the Anthropic wire format.
///
/// Consecutive `Message::Tool` entries (tool results) are combined into a
/// single user turn with multiple `tool_result` content blocks, as required
/// by the Anthropic API.
pub fn messages_to_anthropic(messages: &[Message]) -> serde_json::Value {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::User { content, images } => {
                let msg_content = if images.is_empty() {
                    serde_json::json!(content)
                } else {
                    let mut blocks: Vec<serde_json::Value> = images
                        .iter()
                        .map(|(data, mime)| {
                            let b64 = base64::prelude::BASE64_STANDARD.encode(data);
                            serde_json::json!({
                                "type": "image",
                                "source": { "type": "base64", "media_type": mime, "data": b64 }
                            })
                        })
                        .collect();
                    blocks.push(serde_json::json!({ "type": "text", "text": content }));
                    serde_json::json!(blocks)
                };
                out.push(serde_json::json!({ "role": "user", "content": msg_content }));
                i += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut content_blocks: Vec<serde_json::Value> = Vec::new();
                if !content.is_empty() {
                    content_blocks.push(serde_json::json!({ "type": "text", "text": content }));
                }
                for tc in tool_calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.args_json).unwrap_or(serde_json::Value::Null);
                    content_blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.call_id,
                        "name": tc.name,
                        "input": input,
                    }));
                }
                out.push(serde_json::json!({ "role": "assistant", "content": content_blocks }));
                i += 1;
            }
            Message::Tool { .. } => {
                // Collect all consecutive Tool messages into one user turn.
                let mut tool_results: Vec<serde_json::Value> = Vec::new();
                while i < messages.len() {
                    if let Message::Tool {
                        call_id, content, ..
                    } = &messages[i]
                    {
                        tool_results.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": call_id,
                            "content": content,
                        }));
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(serde_json::json!({ "role": "user", "content": tool_results }));
            }
        }
    }
    serde_json::Value::Array(out)
}
