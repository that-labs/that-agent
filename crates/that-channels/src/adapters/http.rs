use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use base64::prelude::*;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::channel::{
    Channel, ChannelCapabilities, ChannelEvent, InboundMessage, MessageHandle, OutboundTarget,
};
use crate::config::AdapterConfig;

// ─── Request / Response Types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
    conversation_id: Option<String>,
    sender_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    text: String,
    conversation_id: String,
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct RespondRequest {
    request_id: String,
    response: String,
}

// ─── Shared State ────────────────────────────────────────────────────────────

struct HttpState {
    /// Maps request_id -> sender for routing outbound events to the correct HTTP response.
    active_requests: DashMap<String, mpsc::UnboundedSender<ChannelEvent>>,
    /// Maps request_id -> oneshot sender for ask_human responses.
    pending_asks: Mutex<HashMap<String, oneshot::Sender<String>>>,
    /// Stored inbound_tx for pushing InboundMessages from HTTP handlers.
    inbound_tx: Mutex<Option<mpsc::UnboundedSender<InboundMessage>>>,
    /// Shutdown signal sender.
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// The adapter ID used as channel_id in InboundMessages.
    adapter_id: String,
    /// Optional bearer token for auth enforcement.
    auth_token: Option<String>,
    /// Request timeout in seconds.
    request_timeout_secs: u64,
}

// ─── HTTP Gateway Adapter ────────────────────────────────────────────────────

/// HTTP Gateway channel adapter.
///
/// Exposes the agent as an HTTP API with both synchronous and streaming (SSE)
/// endpoints. Incoming HTTP requests are translated to `InboundMessage`s that
/// flow through the standard channel router, and outbound `ChannelEvent`s are
/// routed back to the originating HTTP response via per-request mpsc channels.
///
/// ## Endpoints
///
/// | Route | Method | Purpose |
/// |-------|--------|---------|
/// | `/health` | GET | Health check (exempt from auth) |
/// | `/v1/chat` | POST | Synchronous chat — blocks until `Done` or `Error` |
/// | `/v1/chat/stream` | POST | SSE streaming — sends events as they arrive |
/// | `/v1/chat/respond` | POST | Reply to an `ask_human` prompt |
///
/// ## Auth
///
/// When `auth_token` is set, all routes except `/health` require an
/// `Authorization: Bearer <token>` header. Requests without a valid token
/// receive a 401 Unauthorized response.
pub struct HttpAdapter {
    id: String,
    bind_addr: String,
    request_timeout_secs: u64,
    state: Arc<HttpState>,
}

impl HttpAdapter {
    /// Create a new HTTP gateway adapter.
    ///
    /// - `id`: unique adapter identifier (e.g. "api", "http")
    /// - `bind_addr`: socket address to listen on (e.g. "0.0.0.0:8080")
    /// - `auth_token`: optional bearer token for request authentication
    /// - `request_timeout_secs`: maximum time to wait for agent completion per request
    pub fn new(
        id: &str,
        bind_addr: &str,
        auth_token: Option<String>,
        request_timeout_secs: u64,
    ) -> Self {
        let state = Arc::new(HttpState {
            active_requests: DashMap::new(),
            pending_asks: Mutex::new(HashMap::new()),
            inbound_tx: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
            adapter_id: id.to_string(),
            auth_token: auth_token.clone(),
            request_timeout_secs,
        });

        Self {
            id: id.to_string(),
            bind_addr: bind_addr.to_string(),
            request_timeout_secs,
            state,
        }
    }
}

#[async_trait]
impl Channel for HttpAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            inbound: true,
            ask_human: true,
            typing_indicator: false,
            command_menu: false,
            max_message_len: usize::MAX,
            message_edit: false,
            attachments: true,
        }
    }

    fn format_instructions(&self) -> Option<String> {
        Some("You are responding via HTTP API. Use standard Markdown formatting.".to_string())
    }

    async fn on_start(&self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        {
            let mut guard = self.state.shutdown_tx.lock().await;
            *guard = Some(shutdown_tx);
        }

        let state = Arc::clone(&self.state);
        let bind_addr = self.bind_addr.clone();
        let adapter_id = self.id.clone();

        // Build the router.
        // Health endpoint is NOT wrapped with auth middleware.
        let health_route = Router::new().route("/health", get(handle_health));

        // Authenticated routes.
        let api_routes = Router::new()
            .route("/v1/chat", post(handle_chat))
            .route("/v1/chat/stream", post(handle_chat_stream))
            .route("/v1/chat/respond", post(handle_chat_respond));

        let api_routes = if state.auth_token.is_some() {
            api_routes.layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                auth_middleware,
            ))
        } else {
            api_routes
        };

        let app = health_route
            .merge(api_routes)
            .with_state(Arc::clone(&state));

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("Failed to bind HTTP adapter on {bind_addr}"))?;

        info!(channel = %adapter_id, addr = %bind_addr, "HTTP gateway listening");

        let adapter_id_for_error = adapter_id.clone();
        tokio::spawn(async move {
            let server = axum::serve(listener, app);
            let graceful = server.with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!(channel = %adapter_id, "HTTP gateway shutdown signal received");
            });
            if let Err(e) = graceful.await {
                error!(channel = %adapter_id_for_error, "HTTP gateway server error: {e:#}");
            }
        });

        Ok(())
    }

    async fn on_stop(&self) {
        let shutdown_tx = {
            let mut guard = self.state.shutdown_tx.lock().await;
            guard.take()
        };
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(());
            info!(channel = %self.id, "HTTP gateway shutdown initiated");
            // Give inflight requests a moment to drain.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn on_config_update(&self, _cfg: &AdapterConfig) {
        // No live-mutable configuration for the HTTP adapter.
    }

    async fn send_event(
        &self,
        event: &ChannelEvent,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        let request_id = target.and_then(|t| t.thread_id.as_deref()).unwrap_or("");

        if request_id.is_empty() {
            // No target request — cannot route the event.
            return Ok(MessageHandle::default());
        }

        // Forward the event to the per-request sink.
        if let Some(sink) = self.state.active_requests.get(request_id) {
            let _ = sink.send(event.clone());
        }

        // Terminal events: clean up the active request entry.
        match event {
            ChannelEvent::Done { .. } | ChannelEvent::Error(_) => {
                self.state.active_requests.remove(request_id);
            }
            // Attachments are not routable via a per-request sink (no binary SSE),
            // so we skip the sink lookup and let the event fan-out to any subscriber.
            ChannelEvent::Attachment { .. } => {}
            _ => {}
        }

        Ok(MessageHandle::default())
    }

    async fn ask_human(
        &self,
        message: &str,
        timeout: Option<u64>,
        target: Option<&OutboundTarget>,
    ) -> Result<String> {
        let request_id = target
            .and_then(|t| t.thread_id.as_deref())
            .ok_or_else(|| anyhow::anyhow!("HTTP ask_human: no request_id in target.thread_id"))?;

        let timeout_secs = timeout.unwrap_or(self.request_timeout_secs);

        // Create a oneshot channel for the response.
        let (resp_tx, resp_rx) = oneshot::channel::<String>();
        {
            let mut pending = self.state.pending_asks.lock().await;
            pending.insert(request_id.to_string(), resp_tx);
        }

        // Send an ask_human SSE event to the request's event sink so the client
        // knows it needs to POST to /v1/chat/respond.
        if let Some(sink) = self.state.active_requests.get(request_id) {
            let ask_event_json = serde_json::json!({
                "request_id": request_id,
                "message": message,
            });
            // We send a special ChannelEvent::Notify that the SSE handler will
            // recognize as an ask_human event. To keep the Channel trait clean,
            // we use the Notify variant with a magic prefix.
            let _ = sink.send(ChannelEvent::Notify(format!(
                "__ask_human__:{}",
                ask_event_json
            )));
        }

        // Wait for the human to respond via /v1/chat/respond.
        match tokio::time::timeout(Duration::from_secs(timeout_secs), resp_rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                let mut pending = self.state.pending_asks.lock().await;
                pending.remove(request_id);
                Err(anyhow::anyhow!(
                    "HTTP ask_human: response channel closed for request {request_id}"
                ))
            }
            Err(_) => {
                let mut pending = self.state.pending_asks.lock().await;
                pending.remove(request_id);
                Err(anyhow::anyhow!(
                    "HTTP ask_human timed out after {timeout_secs}s for request {request_id}"
                ))
            }
        }
    }

    async fn start_listener(&self, tx: mpsc::UnboundedSender<InboundMessage>) -> Result<()> {
        let mut guard = self.state.inbound_tx.lock().await;
        *guard = Some(tx);
        info!(channel = %self.id, "HTTP inbound listener registered");
        Ok(())
    }
}

// ─── Auth Middleware ──────────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<HttpState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let expected_token = match &state.auth_token {
        Some(t) => t,
        None => return next.run(req).await,
    };

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(value) if value.starts_with("Bearer ") => {
            let token = &value[7..];
            if token == expected_token.as_str() {
                next.run(req).await
            } else {
                (StatusCode::UNAUTHORIZED, "Invalid bearer token").into_response()
            }
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            "Missing or invalid Authorization header",
        )
            .into_response(),
    }
}

// ─── Route Handlers ──────────────────────────────────────────────────────────

/// GET /health — always returns 200 OK.
async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

/// POST /v1/chat — synchronous chat endpoint.
///
/// Sends an InboundMessage through the channel system, then blocks until a
/// `Done` or `Error` event is received on the per-request event channel.
async fn handle_chat(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<ChatRequest>,
) -> Response {
    let request_id = Uuid::new_v4().to_string();
    let sender_id = body
        .sender_id
        .clone()
        .unwrap_or_else(|| "api-client".to_string());

    // Create a per-request event channel.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<ChannelEvent>();
    state.active_requests.insert(request_id.clone(), event_tx);

    // Push inbound message to the agent.
    let pushed = push_inbound(
        &state,
        &request_id,
        &body.message,
        &sender_id,
        body.conversation_id.as_deref(),
    )
    .await;

    if let Err(e) = pushed {
        state.active_requests.remove(&request_id);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    // Wait for the terminal event.
    let timeout_duration = Duration::from_secs(state.request_timeout_secs);
    let result = tokio::time::timeout(timeout_duration, async {
        while let Some(event) = event_rx.recv().await {
            match event {
                ChannelEvent::Done {
                    text,
                    input_tokens,
                    output_tokens,
                    ..
                } => {
                    return Ok(ChatResponse {
                        text,
                        conversation_id: request_id.clone(),
                        input_tokens,
                        output_tokens,
                    });
                }
                ChannelEvent::Error(err) => {
                    return Err(err);
                }
                // Ignore intermediate events in sync mode.
                _ => continue,
            }
        }
        Err("Event channel closed without terminal event".to_string())
    })
    .await;

    // Ensure cleanup regardless of outcome.
    state.active_requests.remove(&request_id);

    match result {
        Ok(Ok(resp)) => (StatusCode::OK, Json(serde_json::json!(resp))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
        Err(_) => {
            warn!(request_id = %request_id, "HTTP chat request timed out");
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({ "error": "Request timed out" })),
            )
                .into_response()
        }
    }
}

/// POST /v1/chat/stream — SSE streaming chat endpoint.
///
/// Sends an InboundMessage, then streams `ChannelEvent`s as SSE events until
/// a terminal (`Done` or `Error`) event is received.
async fn handle_chat_stream(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<ChatRequest>,
) -> Response {
    let request_id = Uuid::new_v4().to_string();
    let sender_id = body
        .sender_id
        .clone()
        .unwrap_or_else(|| "api-client".to_string());

    // Create a per-request event channel.
    let (event_tx, event_rx) = mpsc::unbounded_channel::<ChannelEvent>();
    state.active_requests.insert(request_id.clone(), event_tx);

    // Push inbound message to the agent.
    let pushed = push_inbound(
        &state,
        &request_id,
        &body.message,
        &sender_id,
        body.conversation_id.as_deref(),
    )
    .await;

    if let Err(e) = pushed {
        state.active_requests.remove(&request_id);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let req_id_for_cleanup = request_id.clone();
    let state_for_cleanup = Arc::clone(&state);
    let timeout_secs = state.request_timeout_secs;

    // Convert the receiver into an SSE stream.
    let stream = UnboundedReceiverStream::new(event_rx);

    // Wrap in a timeout stream that terminates if no events arrive for too long.
    let stream = tokio_stream::StreamExt::timeout(stream, Duration::from_secs(timeout_secs));

    let sse_stream = stream.filter_map(move |item| {
        let req_id = req_id_for_cleanup.clone();
        let state_ref = Arc::clone(&state_for_cleanup);

        match item {
            Ok(event) => {
                let sse_event = channel_event_to_sse(&req_id, &event);
                let is_terminal =
                    matches!(event, ChannelEvent::Done { .. } | ChannelEvent::Error(_));
                if is_terminal {
                    state_ref.active_requests.remove(&req_id);
                }
                sse_event
            }
            Err(_elapsed) => {
                // Timeout — clean up and send error event.
                state_ref.active_requests.remove(&req_id);
                Some(
                    SseEvent::default()
                        .event("error")
                        .data(serde_json::json!({ "error": "Stream timed out" }).to_string()),
                )
            }
        }
    });

    Sse::new(sse_stream.map(Ok::<_, Infallible>))
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// POST /v1/chat/respond — submit a response to a pending `ask_human` prompt.
async fn handle_chat_respond(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<RespondRequest>,
) -> Response {
    let mut pending = state.pending_asks.lock().await;
    if let Some(tx) = pending.remove(&body.request_id) {
        match tx.send(body.response) {
            Ok(()) => (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "delivered" })),
            )
                .into_response(),
            Err(_) => (
                StatusCode::GONE,
                Json(serde_json::json!({ "error": "Request is no longer waiting for a response" })),
            )
                .into_response(),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("No pending ask_human for request_id '{}'", body.request_id)
            })),
        )
            .into_response()
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Push an InboundMessage to the agent via the stored inbound_tx.
async fn push_inbound(
    state: &HttpState,
    request_id: &str,
    message: &str,
    sender_id: &str,
    conversation_id: Option<&str>,
) -> Result<()> {
    let inbound_tx = state.inbound_tx.lock().await;
    let tx = inbound_tx
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("HTTP adapter: inbound listener not yet registered"))?;

    let msg = InboundMessage {
        channel_id: state.adapter_id.clone(),
        sender_id: sender_id.to_string(),
        text: message.to_string(),
        message_id: None,
        conversation_id: conversation_id.map(|s| s.to_string()),
        session_hint: Some(request_id.to_string()),
    };

    tx.send(msg)
        .map_err(|_| anyhow::anyhow!("HTTP adapter: inbound channel closed"))?;

    info!(
        adapter = %state.adapter_id,
        request_id = %request_id,
        sender = %sender_id,
        "Dispatched inbound HTTP message to agent"
    );

    Ok(())
}

/// Convert a `ChannelEvent` to an SSE event.
///
/// Returns `None` for event types that have no SSE representation (e.g.
/// `TypingIndicator`, `ThinkingDelta`, `Retrying`).
fn channel_event_to_sse(_request_id: &str, event: &ChannelEvent) -> Option<SseEvent> {
    match event {
        ChannelEvent::StreamToken(token) => Some(
            SseEvent::default()
                .event("stream_token")
                .data(serde_json::json!({ "token": token }).to_string()),
        ),
        ChannelEvent::ToolCall {
            call_id,
            name,
            args,
        } => Some(
            SseEvent::default().event("tool_call").data(
                serde_json::json!({
                    "call_id": call_id,
                    "name": name,
                    "args": args,
                })
                .to_string(),
            ),
        ),
        ChannelEvent::ToolResult {
            call_id,
            name,
            result,
        } => Some(
            SseEvent::default().event("tool_result").data(
                serde_json::json!({
                    "call_id": call_id,
                    "name": name,
                    "result": result,
                })
                .to_string(),
            ),
        ),
        ChannelEvent::Done {
            text,
            input_tokens,
            output_tokens,
            ..
        } => Some(
            SseEvent::default().event("done").data(
                serde_json::json!({
                    "text": text,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                })
                .to_string(),
            ),
        ),
        ChannelEvent::Error(err) => Some(
            SseEvent::default()
                .event("error")
                .data(serde_json::json!({ "error": err }).to_string()),
        ),
        ChannelEvent::Notify(msg) => {
            // Special encoding for ask_human events forwarded through Notify.
            if let Some(payload) = msg.strip_prefix("__ask_human__:") {
                Some(
                    SseEvent::default()
                        .event("ask_human")
                        .data(payload.to_string()),
                )
            } else {
                Some(
                    SseEvent::default()
                        .event("notify")
                        .data(serde_json::json!({ "message": msg }).to_string()),
                )
            }
        }
        ChannelEvent::Attachment {
            filename,
            data,
            caption,
            mime_type,
        } => Some(
            SseEvent::default().event("attachment").data(
                serde_json::json!({
                    "filename": filename,
                    "mime_type": mime_type,
                    "caption": caption,
                    "size_bytes": data.len(),
                    "data_base64": BASE64_STANDARD.encode(data.as_ref()),
                })
                .to_string(),
            ),
        ),
        // Events with no SSE representation.
        ChannelEvent::TypingIndicator
        | ChannelEvent::ThinkingDelta(_)
        | ChannelEvent::Retrying { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_event_to_sse_maps_stream_token() {
        let event = ChannelEvent::StreamToken("hello".into());
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_maps_done() {
        let event = ChannelEvent::Done {
            text: "result".into(),
            input_tokens: 10,
            output_tokens: 20,
            cached_input_tokens: 5,
            cache_write_tokens: 0,
        };
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_maps_error() {
        let event = ChannelEvent::Error("something broke".into());
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_skips_typing_indicator() {
        let event = ChannelEvent::TypingIndicator;
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_none());
    }

    #[test]
    fn channel_event_to_sse_maps_ask_human_notify() {
        let payload = r#"{"request_id":"r1","message":"confirm?"}"#;
        let event = ChannelEvent::Notify(format!("__ask_human__:{payload}"));
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_maps_regular_notify() {
        let event = ChannelEvent::Notify("progress update".into());
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_maps_tool_call() {
        let event = ChannelEvent::ToolCall {
            call_id: "c1".into(),
            name: "shell_exec".into(),
            args: "ls -la".into(),
        };
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn channel_event_to_sse_maps_tool_result() {
        let event = ChannelEvent::ToolResult {
            call_id: "c1".into(),
            name: "shell_exec".into(),
            result: "file1\nfile2".into(),
        };
        let sse = channel_event_to_sse("req-1", &event);
        assert!(sse.is_some());
    }

    #[test]
    fn http_adapter_capabilities() {
        let adapter = HttpAdapter::new("test-api", "127.0.0.1:0", None, 60);
        let caps = adapter.capabilities();
        assert!(caps.inbound);
        assert!(caps.ask_human);
        assert!(!caps.typing_indicator);
        assert!(!caps.command_menu);
        assert_eq!(caps.max_message_len, usize::MAX);
        assert!(!caps.message_edit);
    }

    #[test]
    fn http_adapter_format_instructions() {
        let adapter = HttpAdapter::new("test-api", "127.0.0.1:0", None, 60);
        let instructions = adapter.format_instructions();
        assert!(instructions.is_some());
        assert!(instructions.unwrap().contains("Markdown"));
    }

    #[test]
    fn http_adapter_id() {
        let adapter = HttpAdapter::new("my-api", "127.0.0.1:0", None, 60);
        assert_eq!(adapter.id(), "my-api");
    }
}
