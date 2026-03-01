//! OpenAI Responses API client.
//!
//! Supports two transport modes:
//!   - WebSocket mode (default): lower-latency continuation across turns using
//!     a persistent socket and `previous_response_id` chaining.
//!   - Legacy HTTP streaming mode: `POST /v1/responses` with SSE parsing.
//!
//! Both modes emit `TurnEvent`s over an mpsc channel and use the Responses API
//! item format for message/tool history.

use anyhow::Result;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, Message as WsMessage},
    MaybeTlsStream, WebSocketStream,
};

use super::anthropic::TurnEvent;
use super::types::{Message, ToolCall, ToolDef, Usage};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Default)]
pub(super) struct OpenAiWsState {
    ws: Option<WsStream>,
    previous_response_id: Option<String>,
    last_messages_len: usize,
}

pub(super) fn new_ws_session() -> Arc<Mutex<OpenAiWsState>> {
    Arc::new(Mutex::new(OpenAiWsState::default()))
}

/// Execute one streaming turn against the OpenAI Responses API.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_turn(
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    tx: mpsc::Sender<TurnEvent>,
    websocket_mode: bool,
    session: Option<Arc<Mutex<OpenAiWsState>>>,
) -> Result<Usage> {
    if websocket_mode {
        if let Some(shared) = session {
            stream_turn_ws_persistent(
                api_key, model, system, messages, tools, max_tokens, tx, &shared,
            )
            .await
        } else {
            stream_turn_ws_ephemeral(api_key, model, system, messages, tools, max_tokens, tx).await
        }
    } else {
        stream_turn_http(api_key, model, system, messages, tools, max_tokens, tx).await
    }
}

async fn stream_turn_ws_ephemeral(
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    tx: mpsc::Sender<TurnEvent>,
) -> Result<Usage> {
    let mut ws = connect_ws(api_key).await?;
    let create_payload = build_ws_request(system, messages, tools, model, max_tokens, None);
    ws.send(WsMessage::Text(create_payload.to_string())).await?;

    let turn = read_ws_turn(&mut ws, &tx).await;
    let _ = ws.close(None).await;

    match turn {
        Ok(outcome) => Ok(outcome.usage),
        Err(TurnFailure::Api { detail, .. }) => {
            let message = format!("OpenAI Responses API error: {detail}");
            let _ = tx
                .send(TurnEvent::Error(anyhow::anyhow!("{message}")))
                .await;
            Err(anyhow::anyhow!("{message}"))
        }
        Err(TurnFailure::Transport(err)) => {
            let _ = tx.send(TurnEvent::Error(anyhow::anyhow!("{err:#}"))).await;
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_turn_ws_persistent(
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    tx: mpsc::Sender<TurnEvent>,
    session: &Arc<Mutex<OpenAiWsState>>,
) -> Result<Usage> {
    let mut retried_without_previous = false;

    loop {
        let mut state = session.lock().await;
        if state.ws.is_none() {
            state.ws = Some(connect_ws(api_key).await?);
        }

        let using_previous = state.previous_response_id.is_some();
        let start_idx = if using_previous {
            state.last_messages_len.min(messages.len())
        } else {
            0
        };

        let create_payload = build_ws_request(
            system,
            &messages[start_idx..],
            tools,
            model,
            max_tokens,
            state.previous_response_id.as_deref(),
        );

        let turn = {
            let ws = state.ws.as_mut().expect("websocket state initialized");
            if let Err(e) = ws.send(WsMessage::Text(create_payload.to_string())).await {
                Err(TurnFailure::Transport(e.into()))
            } else {
                read_ws_turn(ws, &tx).await
            }
        };

        match turn {
            Ok(outcome) => {
                if outcome.completed {
                    state.previous_response_id = outcome.response_id;
                    state.last_messages_len = messages.len();
                } else {
                    state.previous_response_id = None;
                    state.last_messages_len = 0;
                    state.ws = None;
                }
                return Ok(outcome.usage);
            }
            Err(TurnFailure::Api { code, detail }) => {
                let recoverable = matches!(
                    code.as_deref(),
                    Some("previous_response_not_found")
                        | Some("websocket_connection_limit_reached")
                );

                if recoverable && !retried_without_previous {
                    state.previous_response_id = None;
                    state.last_messages_len = 0;
                    if matches!(code.as_deref(), Some("websocket_connection_limit_reached")) {
                        state.ws = None;
                    }
                    retried_without_previous = true;
                    continue;
                } else {
                    let message = format!("OpenAI Responses API error: {detail}");
                    let _ = tx
                        .send(TurnEvent::Error(anyhow::anyhow!("{message}")))
                        .await;
                    return Err(anyhow::anyhow!("{message}"));
                }
            }
            Err(TurnFailure::Transport(err)) => {
                state.ws = None;
                state.previous_response_id = None;
                state.last_messages_len = 0;
                let _ = tx.send(TurnEvent::Error(anyhow::anyhow!("{err:#}"))).await;
                return Err(err);
            }
        }
    }
}

async fn stream_turn_http(
    api_key: &str,
    model: &str,
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    max_tokens: u32,
    tx: mpsc::Sender<TurnEvent>,
) -> Result<Usage> {
    let body = build_http_request(system, messages, tools, model, max_tokens);

    let client = reqwest::Client::new();
    let response = client
        .post("https://api.openai.com/v1/responses")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("OpenAI API error {status}: {body}"));
    }

    let mut stream = response.bytes_stream();
    let mut buf = String::new();
    let mut full_text = String::new();
    let mut usage = Usage::default();

    // In-progress tool call slots, keyed by output_index.
    // Each slot: (call_id, name, accumulated_args).
    let mut tool_slots: std::collections::HashMap<u64, (String, String, String)> =
        std::collections::HashMap::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete lines from the buffer.
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf = buf[nl + 1..].to_string();

            // Responses API SSE lines look like:
            //   event: response.output_text.delta
            //   data: {"type":"response.output_text.delta","delta":"hello"}
            // We skip `event:` lines and blank lines; only parse `data:` lines.
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };

            match handle_response_event(data, &tx, &mut full_text, &mut usage, &mut tool_slots)
                .await?
            {
                EventControl::Continue => {}
                EventControl::Completed { .. } => return Ok(usage),
                EventControl::ApiError { detail, .. } => {
                    let message = format!("OpenAI Responses API error: {detail}");
                    let _ = tx
                        .send(TurnEvent::Error(anyhow::anyhow!("{message}")))
                        .await;
                    return Err(anyhow::anyhow!("{message}"));
                }
            }
        }
    }

    // Stream ended without `response.completed` — flush any remaining tool slots
    // and send TurnEnd so the caller is not left waiting.
    flush_pending_turn(&tx, &full_text, &usage, &mut tool_slots).await;

    Ok(usage)
}

struct TurnOutcome {
    usage: Usage,
    completed: bool,
    response_id: Option<String>,
}

enum TurnFailure {
    Api {
        code: Option<String>,
        detail: String,
    },
    Transport(anyhow::Error),
}

enum EventControl {
    Continue,
    Completed {
        response_id: Option<String>,
    },
    ApiError {
        code: Option<String>,
        detail: String,
    },
}

async fn read_ws_turn(
    ws: &mut WsStream,
    tx: &mpsc::Sender<TurnEvent>,
) -> std::result::Result<TurnOutcome, TurnFailure> {
    let mut full_text = String::new();
    let mut usage = Usage::default();

    // In-progress tool call slots, keyed by output_index.
    // Each slot: (call_id, name, accumulated_args).
    let mut tool_slots: std::collections::HashMap<u64, (String, String, String)> =
        std::collections::HashMap::new();

    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| TurnFailure::Transport(e.into()))?;
        match msg {
            WsMessage::Text(text) => {
                match handle_response_event(&text, tx, &mut full_text, &mut usage, &mut tool_slots)
                    .await
                    .map_err(TurnFailure::Transport)?
                {
                    EventControl::Continue => {}
                    EventControl::Completed { response_id } => {
                        return Ok(TurnOutcome {
                            usage,
                            completed: true,
                            response_id,
                        });
                    }
                    EventControl::ApiError { code, detail } => {
                        return Err(TurnFailure::Api { code, detail });
                    }
                }
            }
            WsMessage::Binary(bytes) => {
                if let Ok(text) = std::str::from_utf8(&bytes) {
                    match handle_response_event(
                        text,
                        tx,
                        &mut full_text,
                        &mut usage,
                        &mut tool_slots,
                    )
                    .await
                    .map_err(TurnFailure::Transport)?
                    {
                        EventControl::Continue => {}
                        EventControl::Completed { response_id } => {
                            return Ok(TurnOutcome {
                                usage,
                                completed: true,
                                response_id,
                            });
                        }
                        EventControl::ApiError { code, detail } => {
                            return Err(TurnFailure::Api { code, detail });
                        }
                    }
                }
            }
            WsMessage::Ping(payload) => {
                ws.send(WsMessage::Pong(payload))
                    .await
                    .map_err(|e| TurnFailure::Transport(e.into()))?;
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }

    // Stream ended without `response.completed` — flush any remaining tool slots
    // and send TurnEnd so the caller is not left waiting.
    flush_pending_turn(tx, &full_text, &usage, &mut tool_slots).await;

    Ok(TurnOutcome {
        usage,
        completed: false,
        response_id: None,
    })
}

async fn handle_response_event(
    raw: &str,
    tx: &mpsc::Sender<TurnEvent>,
    full_text: &mut String,
    usage: &mut Usage,
    tool_slots: &mut std::collections::HashMap<u64, (String, String, String)>,
) -> Result<EventControl> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Ok(EventControl::Continue);
    };
    let Some(event_type) = val["type"].as_str() else {
        return Ok(EventControl::Continue);
    };

    match event_type {
        // ── Text output ───────────────────────────────────────────────────────
        "response.output_text.delta" => {
            if let Some(delta) = val["delta"].as_str() {
                if !delta.is_empty() {
                    full_text.push_str(delta);
                    let _ = tx.send(TurnEvent::TextDelta(delta.to_string())).await;
                }
            }
        }

        // ── Tool call start ───────────────────────────────────────────────────
        "response.output_item.added" => {
            if let Some(item) = val["item"].as_object() {
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let idx = val["output_index"].as_u64().unwrap_or(0);
                    let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    tool_slots.insert(idx, (call_id, name, String::new()));
                }
            }
        }

        // ── Tool call argument streaming ──────────────────────────────────────
        "response.function_call_arguments.delta" => {
            let idx = val["output_index"].as_u64().unwrap_or(0);
            if let Some(slot) = tool_slots.get_mut(&idx) {
                if let Some(delta) = val["delta"].as_str() {
                    slot.2.push_str(delta);
                }
            }
        }

        // ── Tool call complete ────────────────────────────────────────────────
        "response.output_item.done" => {
            if let Some(item) = val["item"].as_object() {
                if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                    let idx = val["output_index"].as_u64().unwrap_or(0);
                    if let Some((call_id, name, args)) = tool_slots.remove(&idx) {
                        // Prefer the final arguments from the done event (authoritative).
                        let final_args = item["arguments"]
                            .as_str()
                            .map(|s| s.to_string())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(args);
                        let _ = tx
                            .send(TurnEvent::ToolCallComplete(ToolCall {
                                call_id,
                                name,
                                args_json: final_args,
                            }))
                            .await;
                    }
                }
            }
        }

        // ── Response complete ─────────────────────────────────────────────────
        "response.completed" => {
            if let Some(u) = val.pointer("/response/usage").and_then(|u| u.as_object()) {
                usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0) as u32;
                usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as u32;
            }
            let response_id = val
                .pointer("/response/id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);

            let _ = tx
                .send(TurnEvent::TurnEnd {
                    full_text: full_text.clone(),
                    usage: usage.clone(),
                })
                .await;

            return Ok(EventControl::Completed { response_id });
        }

        // ── API-level error ───────────────────────────────────────────────────
        "error" => {
            let code = val
                .pointer("/error/code")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let msg = val
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| val["message"].as_str())
                .unwrap_or("unknown error");
            let detail = if let Some(c) = &code {
                format!("{c}: {msg}")
            } else {
                msg.to_string()
            };
            return Ok(EventControl::ApiError { code, detail });
        }

        _ => {} // ignore all other event types
    }

    Ok(EventControl::Continue)
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

async fn connect_ws(api_key: &str) -> Result<WsStream> {
    let mut req = "wss://api.openai.com/v1/responses".into_client_request()?;
    req.headers_mut()
        .insert("Authorization", format!("Bearer {api_key}").parse()?);
    req.headers_mut()
        .insert("Content-Type", "application/json".parse()?);

    let (ws, _) = tokio_tungstenite::connect_async(req).await?;
    Ok(ws)
}

// ─── Request builders ─────────────────────────────────────────────────────────

fn build_ws_request(
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    model: &str,
    max_tokens: u32,
    previous_response_id: Option<&str>,
) -> serde_json::Value {
    let input = messages_to_responses_input(messages);
    let tools_json: Vec<serde_json::Value> = tools.iter().map(tool_to_responses).collect();

    let mut body = serde_json::json!({
        "type": "response.create",
        "model": model,
        "max_output_tokens": max_tokens,
        // System prompt moves to the top-level `instructions` field.
        "instructions": system,
        // We manage our own history — no server-side state needed.
        "store": false,
        "input": input,
    });

    if let Some(prev_id) = previous_response_id {
        body["previous_response_id"] = serde_json::Value::String(prev_id.to_string());
    }

    if !tools_json.is_empty() {
        body["tools"] = serde_json::Value::Array(tools_json);
    }

    body
}

fn build_http_request(
    system: &str,
    messages: &[Message],
    tools: &[ToolDef],
    model: &str,
    max_tokens: u32,
) -> String {
    let input = messages_to_responses_input(messages);
    let tools_json: Vec<serde_json::Value> = tools.iter().map(tool_to_responses).collect();

    let mut body = serde_json::json!({
        "model": model,
        "max_output_tokens": max_tokens,
        "stream": true,
        // System prompt moves to the top-level `instructions` field.
        "instructions": system,
        // We manage our own history — no server-side state needed.
        "store": false,
        "input": input,
    });

    if !tools_json.is_empty() {
        body["tools"] = serde_json::Value::Array(tools_json);
    }

    body.to_string()
}

/// Convert a tool definition to the Responses API internally-tagged format.
///
/// Unlike Chat Completions (externally-tagged under `"function": {...}`), the
/// Responses API puts `name`, `description`, and `parameters` at the top level.
/// `strict` defaults to `true` in the Responses API; we disable it because our
/// schemas have optional fields and no `additionalProperties: false` constraint.
fn tool_to_responses(t: &ToolDef) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "name": t.name,
        "description": t.description,
        "parameters": t.parameters,
        "strict": false,
    })
}

/// Convert our internal `Message` history to Responses API input items.
///
/// The Responses API separates concerns more cleanly than Chat Completions:
///   - `Message::User`      → `{"role":"user","content":"…"}`
///   - `Message::Assistant` → optional text item THEN one `function_call` item
///     per tool call (separate items, not embedded)
///   - `Message::Tool`      → `{"type":"function_call_output","call_id":"…","output":"…"}`
pub fn messages_to_responses_input(messages: &[Message]) -> Vec<serde_json::Value> {
    let mut items: Vec<serde_json::Value> = Vec::new();

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
                items.push(serde_json::json!({ "role": "user", "content": msg_content }));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                // Emit assistant text content first (if any).
                if !content.is_empty() {
                    items.push(serde_json::json!({ "role": "assistant", "content": content }));
                }
                // Each tool call becomes its own function_call item.
                for tc in tool_calls {
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": tc.call_id,
                        "name": tc.name,
                        "arguments": tc.args_json,
                    }));
                }
            }
            Message::Tool {
                call_id, content, ..
            } => {
                items.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": content,
                }));
            }
        }
    }

    items
}
