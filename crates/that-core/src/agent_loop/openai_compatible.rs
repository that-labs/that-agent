use anyhow::Result;
use base64::Engine as _;
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::provider_registry::ProviderEntry;

use super::anthropic::TurnEvent;
use super::types::{Message, ToolCall, ToolDef, Usage};

#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_turn(
    provider: &ProviderEntry,
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    tx: mpsc::Sender<TurnEvent>,
) -> Result<Usage> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = build_request(system, messages, tools, model, max_tokens);
    let response = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(format!(
            "Provider '{}' API error {}: {}",
            provider.id, status, body
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut usage = Usage::default();
    let mut tool_slots: std::collections::HashMap<u64, (String, String, String)> =
        std::collections::HashMap::new();
    let idle_timeout = tokio::time::Duration::from_secs(super::STREAM_IDLE_TIMEOUT_SECS);

    loop {
        let chunk = match tokio::time::timeout(idle_timeout, stream.next()).await {
            Ok(Some(chunk)) => chunk?,
            Ok(None) => break,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Provider '{}' SSE stream timed out: no data for {}s",
                    provider.id,
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
                flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;
                return Ok(usage);
            }
            let Ok(val) = serde_json::from_str::<serde_json::Value>(data) else {
                tracing::debug!(provider = %provider.id, "SSE unparseable: {data}");
                continue;
            };
            if let Some(err) = val.get("error") {
                let msg = err["message"]
                    .as_str()
                    .or_else(|| err.as_str())
                    .unwrap_or("unknown error");
                let message = format!("Provider '{}' stream error: {msg}", provider.id);
                let _ = tx
                    .send(TurnEvent::Error(anyhow::anyhow!("{message}")))
                    .await;
                return Err(anyhow::anyhow!("{message}"));
            }
            if let Some(u) = val.get("usage").and_then(|u| u.as_object()) {
                usage.input_tokens =
                    u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                usage.output_tokens = u
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
            }
            let Some(choice) = val.get("choices").and_then(|c| c.get(0)) else {
                continue;
            };
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            if let Some(delta) = delta {
                if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        full_text.push_str(content);
                        let _ = tx.send(TurnEvent::TextDelta(content.to_string())).await;
                    }
                }
                if let Some(tc_arr) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tc_arr {
                        let idx = tc["index"].as_u64().unwrap_or(0);
                        if let Some(call_id) = tc.get("id").and_then(|v| v.as_str()) {
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
                            tool_slots.insert(idx, (call_id.to_string(), name, initial_args));
                        } else if let Some(slot) = tool_slots.get_mut(&idx) {
                            if let Some(args_delta) =
                                tc.pointer("/function/arguments").and_then(|v| v.as_str())
                            {
                                slot.2.push_str(args_delta);
                            }
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
            if choice.get("finish_reason").is_some_and(|f| !f.is_null()) {
                flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;
            }
        }
    }

    flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;
    Ok(usage)
}

fn build_request(
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    model: &str,
    max_tokens: u32,
) -> String {
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": messages_to_chat_completions(system, messages),
    });
    if !tools.is_empty() {
        body["parallel_tool_calls"] = serde_json::json!(false);
        body["tools"] =
            serde_json::Value::Array(tools.iter().map(tool_to_chat_completions).collect());
    }
    body.to_string()
}

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

fn messages_to_chat_completions(system: &str, messages: &[Message]) -> Vec<serde_json::Value> {
    let mut out = vec![serde_json::json!({
        "role": "system",
        "content": system,
    })];
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
                    "content": content,
                });
                if !tool_calls.is_empty() {
                    assistant_msg["tool_calls"] = serde_json::Value::Array(
                        tool_calls
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
                            .collect(),
                    );
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
    out
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
