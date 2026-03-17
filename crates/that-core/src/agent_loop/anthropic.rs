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

    let is_oauth = is_oauth_token(api_key);

    let client = super::llm_http_client();
    let mut req = client
        .post("https://api.anthropic.com/v1/messages")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");

    if is_oauth {
        req = req
            .header("Authorization", format!("Bearer {api_key}"))
            .header(
                "anthropic-beta",
                "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14,oauth-2025-04-20",
            );
    } else {
        req = req.header("x-api-key", api_key).header(
            "anthropic-beta",
            "prompt-caching-2024-07-31,interleaved-thinking-2025-05-14",
        );
    }

    let response = req.body(body.clone()).send().await?;

    let status = response.status();
    if !status.is_success() {
        let resp_body = response.text().await.unwrap_or_default();
        if status.as_u16() == 400 {
            // Log the request body for debugging malformed requests.
            let req_preview: String = body.chars().take(2000).collect();
            tracing::error!(
                status = %status,
                response = %resp_body,
                request_preview = %req_preview,
                "Anthropic 400 — request body preview logged for debugging"
            );
        }
        return Err(anyhow::anyhow!("Anthropic API error {status}: {resp_body}"));
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

    let idle_timeout = tokio::time::Duration::from_secs(super::STREAM_IDLE_TIMEOUT_SECS);

    loop {
        let chunk = match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(chunk)) => chunk?,
            Ok(None) => break, // Stream ended cleanly.
            Err(_elapsed) => {
                return Err(anyhow::anyhow!(
                    "Anthropic SSE stream timed out: no data for {}s",
                    super::STREAM_IDLE_TIMEOUT_SECS
                ));
            }
        };
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

    let messages_json = messages_to_anthropic(messages, prompt_caching);

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": system_block,
        "messages": messages_json,
        "tools": tools_json,
    });

    // Claude 4+ models support extended thinking. Enable it with a budget
    // of ~60% of max_tokens, leaving room for the visible response.
    // Models: claude-opus-4*, claude-sonnet-4*, claude-haiku-4*, etc.
    if model.starts_with("claude-") && !model.contains("-3") {
        let budget = (max_tokens as u64 * 3 / 5).max(1024);
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    body.to_string()
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
///
/// When `prompt_caching` is true, a `cache_control` breakpoint is added to
/// the **last user-role message** (either a plain user turn or a tool-result
/// group). This lets Anthropic cache the entire conversation prefix up to
/// that point, so each new turn only re-parses the latest messages instead
/// of the full growing history.
pub fn messages_to_anthropic(messages: &[Message], prompt_caching: bool) -> serde_json::Value {
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
                    let input: serde_json::Value = serde_json::from_str(&tc.args_json)
                        .unwrap_or_else(|_| serde_json::json!({}));
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
                        call_id,
                        content,
                        images,
                        ..
                    } = &messages[i]
                    {
                        if images.is_empty() {
                            tool_results.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": content,
                            }));
                        } else {
                            // Build array content with image block(s) + text
                            let mut blocks: Vec<serde_json::Value> = images
                                .iter()
                                .map(|(data, mime)| {
                                    let b64 =
                                        base64::prelude::BASE64_STANDARD.encode(data);
                                    serde_json::json!({
                                        "type": "image",
                                        "source": { "type": "base64", "media_type": mime, "data": b64 }
                                    })
                                })
                                .collect();
                            blocks.push(serde_json::json!({ "type": "text", "text": content }));
                            tool_results.push(serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": call_id,
                                "content": blocks,
                            }));
                        }
                        i += 1;
                    } else {
                        break;
                    }
                }
                out.push(serde_json::json!({ "role": "user", "content": tool_results }));
            }
        }
    }

    // Merge consecutive same-role messages. Steering hints, anti-loop nudges,
    // and budget reminders can create user→user sequences after tool results.
    // Anthropic's API rejects consecutive messages with the same role.
    let mut merged: Vec<serde_json::Value> = Vec::with_capacity(out.len());
    for msg in out {
        let dominated = merged
            .last()
            .map(|prev| prev["role"] == msg["role"])
            .unwrap_or(false);
        if dominated {
            // Append content blocks from msg into the previous message.
            let prev = merged.last_mut().unwrap();
            let new_blocks = if msg["content"].is_string() {
                vec![serde_json::json!({ "type": "text", "text": msg["content"] })]
            } else if let Some(arr) = msg["content"].as_array() {
                arr.clone()
            } else {
                vec![]
            };
            // Ensure prev content is in array form.
            if prev["content"].is_string() {
                let text = prev["content"].as_str().unwrap_or("").to_string();
                prev["content"] = serde_json::json!([{ "type": "text", "text": text }]);
            }
            if let Some(arr) = prev["content"].as_array_mut() {
                arr.extend(new_blocks);
            }
        } else {
            merged.push(msg);
        }
    }
    let mut out = merged;

    // Add cache breakpoint to the last user-role message so the entire
    // conversation prefix is cached across turns. We skip adding it when
    // the last message is also the only message (turn 1) — in that case
    // the system + tools breakpoints already cover the cacheable prefix.
    if prompt_caching && out.len() > 1 {
        if let Some(last_user_idx) = out.iter().rposition(|m| m["role"].as_str() == Some("user")) {
            let msg = &mut out[last_user_idx];
            // Content is either a string (plain user message) or an array
            // (tool_result blocks or image + text blocks). For cache_control
            // we need the array form so we can tag the last block.
            if msg["content"].is_string() {
                let text = msg["content"].as_str().unwrap_or("").to_string();
                msg["content"] = serde_json::json!([{
                    "type": "text",
                    "text": text,
                    "cache_control": { "type": "ephemeral" }
                }]);
            } else if let Some(arr) = msg["content"].as_array_mut() {
                if let Some(last_block) = arr.last_mut() {
                    last_block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                }
            }
        }
    }

    serde_json::Value::Array(out)
}

/// Returns `true` when the credential is an Anthropic OAuth token
/// (prefix `sk-ant-oat`) rather than a regular API key.
fn is_oauth_token(token: &str) -> bool {
    token.starts_with("sk-ant-oat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_followed_by_user_message_alternate_roles() {
        // Anti-loop and steering hints inject User messages after tool results.
        // Anthropic requires strict user/assistant alternation — verify no
        // consecutive same-role messages are produced.
        let messages = vec![
            Message::user("do the task"),
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id: "call_1".into(),
                    name: "shell_exec".into(),
                    args_json: r#"{"cmd":"ls"}"#.into(),
                }],
            },
            Message::Tool {
                call_id: "call_1".into(),
                name: "shell_exec".into(),
                content: "file.txt".into(),
                images: vec![],
            },
            // Injected by anti-loop or steering — user message after tool result
            Message::user("You have been exploring too long. Stop."),
        ];

        let wire = messages_to_anthropic(&messages, false);
        let arr = wire.as_array().unwrap();
        let mut prev_role = "";
        for (i, msg) in arr.iter().enumerate() {
            let role = msg["role"].as_str().unwrap();
            assert_ne!(
                role, prev_role,
                "consecutive '{role}' roles at index {i}: {msg}"
            );
            prev_role = role;
        }
    }

    #[test]
    fn cache_control_added_to_last_user_message() {
        let messages = vec![
            Message::user("first"),
            Message::assistant("ok"),
            Message::user("second"),
        ];
        let wire = messages_to_anthropic(&messages, true);
        let arr = wire.as_array().unwrap();
        let last_user = arr.iter().rposition(|m| m["role"] == "user").unwrap();
        let content = &arr[last_user]["content"];
        assert!(content.is_array());
        let last_block = content.as_array().unwrap().last().unwrap();
        assert_eq!(
            last_block["cache_control"]["type"].as_str(),
            Some("ephemeral")
        );
    }

    #[test]
    fn turn1_with_restart_history_produces_valid_request() {
        // Simulate restart history + new "yo" message — this is the exact
        // sequence that causes 400 errors in production.
        let messages = vec![
            // Restart history from rebuild_history_recent
            Message::user("[System: the previous run was interrupted. You were working on: \"deploy finx\". Recent tool chain: fs_cat → shell_exec → code_edit. Assess the situation before continuing.]"),
            Message::assistant("Understood — I was interrupted. I'll check the current state before proceeding."),
            // New inbound message
            Message::user("yo\n\n<system-reminder>\nchannel info here\n</system-reminder>"),
        ];

        let wire = messages_to_anthropic(&messages, true);
        let arr = wire.as_array().unwrap();

        // Must have exactly 3 messages with alternating roles
        assert_eq!(arr.len(), 3, "expected 3 messages, got: {arr:?}");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[2]["role"], "user");

        // Content must not be null or empty array
        for (i, msg) in arr.iter().enumerate() {
            let content = &msg["content"];
            assert!(!content.is_null(), "message {i} has null content: {msg}");
            if let Some(arr) = content.as_array() {
                assert!(
                    !arr.is_empty(),
                    "message {i} has empty content array: {msg}"
                );
            }
        }
    }

    #[test]
    fn single_user_message_produces_valid_wire_format() {
        // Simplest case: just "yo" with no history
        let messages = vec![Message::user("yo")];
        let wire = messages_to_anthropic(&messages, true);
        let arr = wire.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "user");
        // Turn 1 with single message: cache_control should NOT be added
        // (out.len() == 1, so the cache condition `out.len() > 1` is false)
        let content = &arr[0]["content"];
        assert!(content.is_string() || content.is_array());
    }

    #[test]
    fn build_request_produces_valid_json() {
        let messages = vec![Message::user("hello")];
        let tools = vec![ToolDef {
            name: "shell_exec".into(),
            description: "Run a command".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }),
        }];
        let body = build_request(
            "system prompt",
            &messages,
            &tools,
            "claude-opus-4-6",
            4096,
            true,
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("build_request should produce valid JSON");

        assert_eq!(parsed["model"], "claude-opus-4-6");
        assert_eq!(parsed["stream"], true);
        assert!(parsed["system"].is_array());
        assert!(parsed["messages"].is_array());
        assert!(parsed["tools"].is_array());
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 1);
        // Opus models must have thinking enabled
        assert_eq!(parsed["thinking"]["type"], "enabled");
        assert!(parsed["thinking"]["budget_tokens"].as_u64().unwrap() >= 1024);
    }

    #[test]
    fn build_request_sonnet_has_thinking() {
        let messages = vec![Message::user("hello")];
        let body = build_request("system", &messages, &[], "claude-sonnet-4-6", 4096, false);
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["thinking"]["type"], "enabled");
    }

    #[test]
    fn build_request_claude3_has_no_thinking() {
        let messages = vec![Message::user("hello")];
        let body = build_request(
            "system",
            &messages,
            &[],
            "claude-3-5-sonnet-20241022",
            4096,
            false,
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed["thinking"].is_null());
    }
}
