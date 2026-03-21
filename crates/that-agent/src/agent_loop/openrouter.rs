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
use base64::Engine as _;
use tokio::sync::mpsc;

use super::anthropic::TurnEvent;
use super::types::{Message, ToolCall, ToolDef, Usage};

const OPENROUTER_DISABLED_TOOLS: &[&str] = &[
    "list_skills",
    "provider_admin",
    "agent_run",
    "agent_task",
    "agent_admin",
    "workspace_admin",
];

/// Execute one streaming turn against the OpenRouter Chat Completions API.
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

    let routed_tools = filter_openrouter_tools(tools);
    if routed_tools.len() != tools.len() {
        tracing::warn!(
            provider = "openrouter",
            model = model,
            original_tools = tools.len(),
            routed_tools = routed_tools.len(),
            "OpenRouter request filtered tool surface back to v0.2.2-compatible set"
        );
    }

    let client = super::llm_http_client();
    let mut used_prompt_caching = prompt_caching;
    let mut body = build_request(
        system,
        messages,
        &routed_tools,
        model,
        max_tokens,
        used_prompt_caching,
    );
    let mut response = send_request(client, api_key, &body).await?;

    if !response.status().is_success() {
        let status = response.status();
        let error_body = response.text().await.unwrap_or_default();
        if used_prompt_caching && should_retry_without_cache(status, &error_body) {
            tracing::warn!(
                provider = "openrouter",
                model = model,
                "OpenRouter could not route prompt-cached request; retrying without cache_control markers"
            );
            used_prompt_caching = false;
            body = build_request(system, messages, &routed_tools, model, max_tokens, false);
            response = send_request(client, api_key, &body).await?;
        } else {
            return Err(anyhow::anyhow!(format_http_error(
                status,
                &error_body,
                model,
                !routed_tools.is_empty(),
                used_prompt_caching,
            )));
        }
    }

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(format_http_error(
            status,
            &body,
            model,
            !routed_tools.is_empty(),
            used_prompt_caching,
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut usage = Usage::default();

    // In-progress tool call slots, keyed by index within a single response.
    // Each slot: (call_id, name, accumulated_args).
    let mut tool_slots: std::collections::HashMap<u64, (String, String, String)> =
        std::collections::HashMap::new();

    let idle_timeout = tokio::time::Duration::from_secs(super::STREAM_IDLE_TIMEOUT_SECS);

    loop {
        let chunk = match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(chunk)) => chunk?,
            Ok(None) => break, // Stream ended cleanly.
            Err(_elapsed) => {
                return Err(anyhow::anyhow!(
                    "OpenRouter SSE stream timed out: no data for {}s",
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

fn filter_openrouter_tools(tools: &[ToolDef]) -> Vec<ToolDef> {
    tools
        .iter()
        .filter(|tool| !OPENROUTER_DISABLED_TOOLS.contains(&tool.name.as_str()))
        .cloned()
        .collect()
}

async fn send_request(
    client: &reqwest::Client,
    api_key: &str,
    body: &str,
) -> Result<reqwest::Response> {
    Ok(client
        .post("https://openrouter.ai/api/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .header("X-Title", "that-agent")
        .body(body.to_string())
        .send()
        .await?)
}

fn should_retry_without_cache(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::NOT_FOUND
        && body.contains("No endpoints found that can handle the requested parameters")
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
    });

    if !tools.is_empty() {
        // Only enforce require_parameters when we actually send tools,
        // so OpenRouter only routes to providers that support tool calling.
        body["provider"] = serde_json::json!({ "require_parameters": true });
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
///
/// When `prompt_caching` is true, a `cache_control` breakpoint is added to
/// the last user/tool message so the conversation prefix is cached across turns.
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
            Message::User { content, images } => {
                let msg_content = if images.is_empty() {
                    serde_json::json!(content)
                } else {
                    let mut parts: Vec<serde_json::Value> = images
                        .iter()
                        .map(|(data, mime)| {
                            let b64 = base64::prelude::BASE64_STANDARD.encode(data);
                            serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime};base64,{b64}") }
                            })
                        })
                        .collect();
                    parts.push(serde_json::json!({ "type": "text", "text": content }));
                    serde_json::json!(parts)
                };
                out.push(serde_json::json!({
                    "role": "user",
                    "content": msg_content,
                }));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut assistant_msg = serde_json::json!({
                    "role": "assistant",
                    // Some routed providers reject assistant tool-call messages when
                    // `content` is omitted entirely, even if tool_calls is present.
                    "content": content,
                });
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
                call_id,
                content,
                images,
                ..
            } => {
                out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": content,
                }));
                // OpenRouter tool role is text-only; inject a user message
                // with image_url blocks so the model can see images.
                if !images.is_empty() {
                    let mut parts: Vec<serde_json::Value> = images
                        .iter()
                        .map(|(data, mime)| {
                            let b64 = base64::prelude::BASE64_STANDARD.encode(data);
                            serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": format!("data:{mime};base64,{b64}") }
                            })
                        })
                        .collect();
                    parts.push(serde_json::json!({
                        "type": "text",
                        "text": "[Visual content from image_read tool]"
                    }));
                    out.push(serde_json::json!({ "role": "user", "content": parts }));
                }
            }
        }
    }

    // Add cache breakpoint to the last user/tool message so the conversation
    // prefix is cached. Skip on turn 1 (only system message + 1 user message).
    if prompt_caching && out.len() > 2 {
        if let Some(last_user_idx) = out
            .iter()
            .rposition(|m| matches!(m["role"].as_str(), Some("user") | Some("tool")))
        {
            let msg = &mut out[last_user_idx];
            // For tool messages, content is a plain string — convert to content array.
            // For user messages, content may be string or array.
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

    out
}

fn format_http_error(
    status: reqwest::StatusCode,
    body: &str,
    model: &str,
    has_tools: bool,
    used_prompt_caching: bool,
) -> String {
    let mut message = format!("OpenRouter API error {status}: {body}");
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return message;
    };

    let provider_error = value
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let provider_name = value
        .pointer("/error/metadata/provider_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let raw = value
        .pointer("/error/metadata/raw")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if provider_error.eq_ignore_ascii_case("Provider returned error") {
        let provider_segment = if provider_name.is_empty() {
            "the routed provider".to_string()
        } else {
            format!("provider '{provider_name}'")
        };
        let tool_segment = if has_tools {
            " during a tool-call turn"
        } else {
            ""
        };
        message = format!(
            "OpenRouter API error {status}: {provider_segment} failed for model '{model}'{tool_segment}. \
             Try a different provider/model selection. In channel mode, use /models to switch. \
             Raw provider response: {}",
            if raw.is_empty() { body } else { raw }
        );
    }

    if should_retry_without_cache(status, body) {
        let mut hint = format!(
            "OpenRouter API error {status}: no routed provider can satisfy model '{model}' with the current request parameters."
        );
        if has_tools {
            hint.push_str(
                " This run sends tool schemas, so the routed provider must support tool calling.",
            );
        }
        if used_prompt_caching {
            hint.push_str(" Prompt-caching markers were enabled for this request.");
        }
        hint.push_str(
            " Try a different model, or retry without prompt caching / with fewer routed features.",
        );
        return hint;
    }

    message
}

#[cfg(test)]
mod tests {
    use super::{
        filter_openrouter_tools, format_http_error, messages_to_chat_completions,
        should_retry_without_cache,
    };
    use crate::agent_loop::{Message, ToolCall, ToolDef};

    #[test]
    fn provider_error_adds_model_switch_hint() {
        let err = format_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Provider returned error","metadata":{"raw":"ERROR","provider_name":"Stealth"}}}"#,
            "openai/gpt-5.2-codex",
            true,
            true,
        );

        assert!(err.contains("provider 'Stealth' failed"));
        assert!(err.contains("use /models to switch"));
    }

    #[test]
    fn no_endpoints_error_adds_routing_hint() {
        let err = format_http_error(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"error":{"message":"No endpoints found that can handle the requested parameters.","code":404}}"#,
            "mistralai/mistral-small-2603",
            true,
            false,
        );

        assert!(err.contains("no routed provider can satisfy"));
        assert!(err.contains("tool calling"));
    }

    #[test]
    fn no_endpoints_error_is_cache_retryable() {
        assert!(should_retry_without_cache(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"error":{"message":"No endpoints found that can handle the requested parameters.","code":404}}"#,
        ));
    }

    #[test]
    fn assistant_tool_call_messages_always_include_content() {
        let messages = vec![Message::Assistant {
            content: String::new(),
            tool_calls: vec![ToolCall {
                call_id: "call_1".into(),
                name: "shell_exec".into(),
                args_json: r#"{"cmd":"pwd"}"#.into(),
            }],
        }];

        let wire = messages_to_chat_completions("system", &messages, false);
        assert_eq!(wire[1]["role"], "assistant");
        assert_eq!(wire[1]["content"], "");
        assert!(wire[1]["tool_calls"].is_array());
    }

    #[test]
    fn openrouter_filters_post_v022_tools() {
        let tools = vec![
            ToolDef {
                name: "shell_exec".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
            ToolDef {
                name: "agent_run".into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
        ];

        let filtered = filter_openrouter_tools(&tools);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "shell_exec");
    }
}
