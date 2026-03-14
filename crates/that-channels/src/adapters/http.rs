use std::collections::HashMap;
use std::convert::Infallible;
#[cfg(feature = "pairing")]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::prelude::*;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Request as AxumRequest, State};
use axum::http::{header, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use dashmap::DashMap;
#[cfg(feature = "pairing")]
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::adapters::gateway_routes::{execute_shell_handler, DynamicRouteRegistry, RouteHandler};
use crate::channel::{
    Channel, ChannelCapabilities, ChannelEvent, InboundAttachment, InboundMessage, MessageHandle,
    OutboundTarget,
};
use crate::config::AdapterConfig;

/// Sentinel `sender_id` for zero-cost sub-agent notifications.
/// Must match the check in the orchestrator's inbound router.
pub const NOTIFY_SENDER_ID: &str = "__notify__";

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

#[derive(Debug, Deserialize)]
struct NotifyRequest {
    message: String,
    agent: Option<String>,
}

/// A single base64-encoded attachment in an inbound webhook request.
#[derive(Debug, Deserialize)]
struct InboundAttachmentPayload {
    /// Base64-encoded file bytes.
    data: String,
    /// MIME type (e.g. "image/png", "audio/ogg").
    mime_type: String,
}

/// POST /v1/inbound request body — external systems push messages to the agent.
#[derive(Debug, Deserialize)]
struct InboundRequest {
    message: String,
    sender_id: String,
    channel_id: Option<String>,
    callback_url: Option<String>,
    #[serde(default)]
    attachments: Vec<InboundAttachmentPayload>,
}

// ─── Pairing ─────────────────────────────────────────────────────────────────

#[cfg(feature = "pairing")]
struct PairingState {
    code: String,
    token: String,
    used: bool,
}

#[cfg(feature = "pairing")]
#[derive(Debug, Deserialize)]
struct PairRequest {
    code: String,
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
    /// Bearer token for auth enforcement. `None` means no auth (open gateway).
    auth_token: RwLock<Option<String>>,
    /// Pairing state for first-time token acquisition (None when pre-configured).
    #[cfg(feature = "pairing")]
    pairing: Mutex<Option<PairingState>>,
    /// Request timeout in seconds.
    request_timeout_secs: u64,
    /// Path to the dynamic route registry file, if configured.
    route_registry_path: Option<PathBuf>,
    /// File path for persisting the bearer token across restarts.
    #[cfg(feature = "pairing")]
    token_file: Option<PathBuf>,
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
/// | `/v1/inbound` | POST | Webhook endpoint for external messages with attachments |
/// | `/v1/schema` | GET | Returns JSON schema for the `/v1/inbound` endpoint |
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
        #[cfg(feature = "pairing")]
        let (resolved_token, pairing, token_file) = {
            let token_file = dirs::home_dir().map(|h| {
                let dir = h.join(".that-agent");
                dir.join(format!("gateway_token_{id}"))
            });

            if let Some(tok) = auth_token {
                (Some(tok), None, token_file)
            } else if let Some(tok) = token_file.as_deref().and_then(load_persisted_token) {
                info!("Gateway restored paired token from disk");
                (Some(tok), None, token_file)
            } else {
                let code = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000u32));
                let token = Uuid::new_v4().to_string();
                eprintln!(
                    "⚡ Gateway pairing code: {code}  →  POST /pair {{\"code\":\"{code}\"}} to get your bearer token"
                );
                (
                    None,
                    Some(PairingState {
                        code,
                        token,
                        used: false,
                    }),
                    token_file,
                )
            }
        };

        #[cfg(not(feature = "pairing"))]
        let resolved_token = auth_token;

        let state = Arc::new(HttpState {
            active_requests: DashMap::new(),
            pending_asks: Mutex::new(HashMap::new()),
            inbound_tx: Mutex::new(None),
            shutdown_tx: Mutex::new(None),
            adapter_id: id.to_string(),
            auth_token: RwLock::new(resolved_token),
            #[cfg(feature = "pairing")]
            pairing: Mutex::new(pairing),
            request_timeout_secs,
            route_registry_path: None,
            #[cfg(feature = "pairing")]
            token_file,
        });

        Self {
            id: id.to_string(),
            bind_addr: bind_addr.to_string(),
            request_timeout_secs,
            state,
        }
    }

    /// Attach a dynamic route registry to this adapter.
    ///
    /// When set, unmatched requests fall back to the registry before returning 404.
    /// Must be called before the adapter starts (no other Arc clones exist yet).
    pub fn with_route_registry(mut self, registry: &DynamicRouteRegistry) -> Self {
        Arc::get_mut(&mut self.state)
            .expect("with_route_registry called after Arc was shared")
            .route_registry_path = Some(registry.path.clone());
        self
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
            inbound_images: true,
            inbound_audio: true,
            rich_messages: false,
            reactions: false,
            native_api: false,
            deferred_start: false,
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
        // Health (and pair when enabled) are NOT wrapped with auth middleware.
        let public_routes = Router::new().route("/health", get(handle_health));
        #[cfg(feature = "pairing")]
        let public_routes = public_routes.route("/pair", post(handle_pair));

        // Authenticated routes — auth middleware always applied.
        let api_routes = Router::new()
            .route("/v1/chat", post(handle_chat))
            .route("/v1/chat/stream", post(handle_chat_stream))
            .route("/v1/chat/respond", post(handle_chat_respond))
            .route("/v1/inbound", post(handle_inbound))
            .route("/v1/notify", post(handle_notify))
            .route("/v1/schema", get(handle_schema))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                auth_middleware,
            ));

        let app = public_routes
            .merge(api_routes)
            .fallback(dynamic_route_handler)
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
        let request_id = target.and_then(|t| t.request_id.as_deref()).unwrap_or("");

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
            .and_then(|t| t.request_id.as_deref())
            .ok_or_else(|| anyhow::anyhow!("HTTP ask_human: no request_id in target"))?;

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

// ─── Token Persistence ───────────────────────────────────────────────────────

#[cfg(feature = "pairing")]
fn load_persisted_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(feature = "pairing")]
fn persist_token(path: &Path, token: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, token) {
        warn!("Failed to persist gateway token to {}: {e}", path.display());
    }
}

// ─── Auth Middleware ──────────────────────────────────────────────────────────

async fn auth_middleware(
    State(state): State<Arc<HttpState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let guard = state.auth_token.read().await;
    let expected_token = match guard.as_deref() {
        Some(t) => t.to_owned(),
        None => {
            // No token configured.
            #[cfg(feature = "pairing")]
            {
                // Pairing enabled — reject until paired.
                return (
                    StatusCode::UNAUTHORIZED,
                    "Gateway not yet paired. POST /pair with your pairing code first.",
                )
                    .into_response();
            }
            #[cfg(not(feature = "pairing"))]
            {
                // No pairing, no token — open gateway (cluster-internal).
                drop(guard);
                return next.run(req).await;
            }
        }
    };
    drop(guard);

    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match auth_header {
        Some(value) if value.starts_with("Bearer ") => {
            let token = &value[7..];
            if token == expected_token {
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

/// POST /pair — exchange a pairing code for a bearer token.
#[cfg(feature = "pairing")]
async fn handle_pair(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<PairRequest>,
) -> Response {
    let mut pairing = state.pairing.lock().await;
    let ps = match pairing.as_mut() {
        Some(ps) => ps,
        None => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "already paired" })),
            )
                .into_response();
        }
    };

    if ps.used {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "already paired" })),
        )
            .into_response();
    }

    if body.code != ps.code {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "invalid code" })),
        )
            .into_response();
    }

    ps.used = true;
    let token = ps.token.clone();

    // Activate the token so the auth middleware starts accepting it.
    let mut guard = state.auth_token.write().await;
    *guard = Some(token.clone());
    drop(guard);

    // Persist the token so it survives restarts.
    if let Some(ref path) = state.token_file {
        persist_token(path, &token);
    }

    info!("Gateway paired successfully");

    (StatusCode::OK, Json(serde_json::json!({ "token": token }))).into_response()
}

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

/// POST /v1/inbound — external webhook endpoint for pushing messages with optional attachments.
///
/// Accepts a JSON body with `message`, `sender_id`, optional `channel_id`,
/// optional `callback_url`, and an optional array of base64-encoded `attachments`.
/// Returns 202 Accepted immediately after queuing the message.
async fn handle_inbound(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<InboundRequest>,
) -> Response {
    // Validate callback_url for SSRF before accepting the request.
    if let Some(ref url) = body.callback_url {
        if let Err(e) = super::validate_callback_url(url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    }

    let tx = match acquire_inbound_tx(&state).await {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };

    // Decode base64 attachments into InboundAttachment variants.
    let mut attachments = Vec::with_capacity(body.attachments.len());
    for (i, att) in body.attachments.iter().enumerate() {
        let data = match BASE64_STANDARD.decode(&att.data) {
            Ok(d) => d,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": format!("Invalid base64 in attachment {i}: {e}")
                    })),
                )
                    .into_response();
            }
        };
        let attachment = if att.mime_type.starts_with("audio/") {
            InboundAttachment::Audio {
                data,
                mime_type: att.mime_type.clone(),
                duration_secs: None,
            }
        } else if att.mime_type.starts_with("image/") {
            InboundAttachment::Image {
                data,
                mime_type: att.mime_type.clone(),
            }
        } else {
            InboundAttachment::Document {
                data,
                mime_type: att.mime_type.clone(),
                filename: None,
            }
        };
        attachments.push(attachment);
    }

    let channel_id = body
        .channel_id
        .as_deref()
        .unwrap_or(&state.adapter_id)
        .to_string();

    let msg = InboundMessage {
        channel_id,
        sender_id: body.sender_id.clone(),
        text: body.message.clone(),
        message_id: None,
        conversation_id: None,
        session_hint: None,
        attachments,
        callback_url: body.callback_url.clone(),
        deferred: true,
        metadata: None,
    };

    if tx.send(msg).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Inbound channel closed" })),
        )
            .into_response();
    }

    info!(
        adapter = %state.adapter_id,
        sender = %body.sender_id,
        attachments = body.attachments.len(),
        "Dispatched inbound webhook message"
    );

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "accepted" })),
    )
        .into_response()
}

/// GET /v1/schema — returns the JSON schema for the /v1/inbound endpoint.
async fn handle_schema() -> impl IntoResponse {
    Json(serde_json::json!({
        "endpoint": "/v1/inbound",
        "method": "POST",
        "content_type": "application/json",
        "body": {
            "message": { "type": "string", "required": true, "description": "The message text" },
            "sender_id": { "type": "string", "required": true, "description": "Sender identifier" },
            "channel_id": { "type": "string", "required": false, "description": "Override channel ID (defaults to adapter ID)" },
            "callback_url": { "type": "string", "required": false, "description": "URL for async response delivery" },
            "attachments": {
                "type": "array",
                "required": false,
                "description": "Base64-encoded file attachments",
                "items": {
                    "data": { "type": "string", "description": "Base64-encoded file bytes" },
                    "mime_type": { "type": "string", "description": "MIME type (e.g. image/png, audio/ogg)" }
                }
            }
        }
    }))
}

/// POST /v1/notify — zero-cost event sink for sub-agent status reports.
///
/// Queues a notification with `sender_id = "__notify__"`. No LLM turn fires;
/// the orchestrator batches these into the next heartbeat tick.
async fn handle_notify(
    State(state): State<Arc<HttpState>>,
    Json(body): Json<NotifyRequest>,
) -> Response {
    let tx = match acquire_inbound_tx(&state).await {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };

    let sender = body.agent.as_deref().unwrap_or("unknown");
    let msg = InboundMessage {
        channel_id: state.adapter_id.clone(),
        sender_id: NOTIFY_SENDER_ID.to_string(),
        text: format!("[{}] {}", sender, body.message),
        message_id: None,
        conversation_id: None,
        session_hint: None,
        attachments: vec![],
        callback_url: None,
        deferred: false,
        metadata: None,
    };

    if tx.send(msg).is_err() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Inbound channel closed" })),
        )
            .into_response();
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "queued" })),
    )
        .into_response()
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Clone the inbound sender, or return a 503 Response if the listener is not registered.
async fn acquire_inbound_tx(
    state: &HttpState,
) -> Result<mpsc::UnboundedSender<InboundMessage>, Response> {
    state
        .inbound_tx
        .lock()
        .await
        .as_ref()
        .map(|tx| tx.clone())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "Inbound listener not yet registered" })),
            )
                .into_response()
        })
}

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
        attachments: vec![],
        callback_url: None,
        deferred: false,
        metadata: None,
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

/// Fallback handler for dynamically registered routes.
///
/// Looks up (method, path) in the `DynamicRouteRegistry` and executes the handler.
/// Falls back to 404 when no matching route is found or no registry is configured.
async fn dynamic_route_handler(State(state): State<Arc<HttpState>>, req: AxumRequest) -> Response {
    let registry_path = state.route_registry_path.as_ref().cloned().or_else(|| {
        std::env::var("THAT_GATEWAY_ROUTES_PATH")
            .ok()
            .map(std::path::PathBuf::from)
    });

    let Some(path) = registry_path else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "not found" })),
        )
            .into_response();
    };

    let registry = DynamicRouteRegistry::new(path);
    let method = req.method().as_str().to_uppercase();
    let uri_path = req.uri().path().to_string();

    let route = match registry.lookup(&method, &uri_path) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "not found" })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match route.handler {
        RouteHandler::Static { body } => (StatusCode::OK, Json(body)).into_response(),
        RouteHandler::Shell {
            command,
            timeout_secs,
        } => {
            // Read request body to pass as REQUEST_BODY env var.
            let body_bytes = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .ok()
                .and_then(|b| String::from_utf8(b.to_vec()).ok())
                .filter(|s| !s.is_empty());

            let (status_code, body_str) =
                execute_shell_handler(&command, timeout_secs, body_bytes).await;

            let status =
                StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            (
                status,
                [(header::CONTENT_TYPE, "application/json")],
                body_str,
            )
                .into_response()
        }
    }
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
                Some(SseEvent::default().event("ask_human").data(payload))
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
        | ChannelEvent::Retrying { .. }
        | ChannelEvent::Reset => None,
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
