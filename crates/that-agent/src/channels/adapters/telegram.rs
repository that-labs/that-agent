use std::collections::HashMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, info, warn};

use crate::channels::channel::{
    BotCommand, Channel, ChannelCapabilities, ChannelEvent, InboundAttachment, InboundMessage,
    MessageHandle, OutboundTarget,
};
use crate::channels::config::AdapterConfig;

/// Maximum number of completed tools to show in the status message.
const TOOL_STATUS_MAX_HISTORY: usize = 8;

/// Minimum seconds between edits of the live cluster-status message.
const NOTIFY_EDIT_THROTTLE_SECS: u64 = 5;

/// Minimum milliseconds between `sendMessageDraft` calls per stream (throttle).
const DRAFT_THROTTLE_MS: u128 = 300;

/// Tracks a single editable tool-status message per chat.
///
/// Instead of sending one Telegram message per tool call, all tool calls
/// within a single agent run are consolidated into one message that is
/// edited in-place as new tools execute.
struct ToolStatusTracker {
    message_id: i64,
    /// Completed tool names, chronological order (oldest first).
    completed: Vec<String>,
    /// Currently executing tool line (e.g. "shell_exec: cmd=ls -la").
    current_line: Option<String>,
}

impl ToolStatusTracker {
    fn render(&self) -> String {
        let mut lines = Vec::new();
        if let Some(current) = &self.current_line {
            lines.push(format!("⚙️ {current}"));
        }
        // Most recent completed first (reverse chronological).
        let recent: Vec<_> = self
            .completed
            .iter()
            .rev()
            .take(TOOL_STATUS_MAX_HISTORY)
            .collect();
        for name in &recent {
            lines.push(format!("  · {name}"));
        }
        let hidden = self.completed.len().saturating_sub(TOOL_STATUS_MAX_HISTORY);
        if hidden > 0 {
            lines.push(format!("  + {hidden} more"));
        }
        lines.join("\n")
    }
}

/// Live cluster-status message: one editable Telegram message per chat that
/// shows the latest status line for every agent that has sent a notification.
/// Edited in-place (throttled) instead of sending a new message each time.
struct NotifyStatusTracker {
    message_id: i64,
    /// Latest status text per agent/sender name, insertion-ordered via Vec.
    agents: Vec<(String, String)>,
    last_edit: Option<std::time::Instant>,
}

impl NotifyStatusTracker {
    fn new() -> Self {
        Self {
            message_id: 0,
            agents: Vec::new(),
            last_edit: None,
        }
    }

    fn upsert(&mut self, key: String, value: String) {
        if let Some(entry) = self.agents.iter_mut().find(|(k, _)| k == &key) {
            entry.1 = value;
        } else {
            self.agents.push((key, value));
        }
    }

    fn ready_to_edit(&self) -> bool {
        self.last_edit
            .map(|t| t.elapsed().as_secs() >= NOTIFY_EDIT_THROTTLE_SECS)
            .unwrap_or(true)
    }

    fn render(&self) -> String {
        let mut out = String::from("🤖 Cluster status:");
        for (name, status) in &self.agents {
            out.push_str(&format!("\n• {name}: {status}"));
        }
        out
    }
}

/// Internal mutable state shared between the inbound listener and `ask_human`.
struct TelegramState {
    /// Per-run/per-target stream buffers; keyed by `stream_key(...)`.
    token_buffers: HashMap<String, String>,
    /// Timestamp of the last `sendMessageDraft` call per stream key (for throttling).
    draft_last_sent: HashMap<String, std::time::Instant>,
    /// Last processed update ID for long-polling offset tracking.
    update_offset: i64,
    /// Pending ask_human replies keyed by chat/sender (see `ask_key`).
    pending_asks: HashMap<String, oneshot::Sender<String>>,
    /// Live-mutable set of extra chat IDs the bot accepts (groups, channels, DMs).
    /// The primary `default_chat_id` is always accepted regardless of this list.
    /// Updated at runtime via `on_config_update` without restarting.
    allowed_chats: Vec<String>,
    /// Live-mutable allowlist of user IDs. Empty = accept all users.
    /// Updated at runtime via `on_config_update` without restarting.
    allowed_senders: Vec<String>,
    /// Per-chat tool status tracker. One editable message per chat shows
    /// the current and recent tool calls in a compact stacked format.
    tool_status: HashMap<String, ToolStatusTracker>,
    /// Per-chat live cluster-status tracker. All agent notifications are
    /// consolidated into one editable message instead of flooding the chat.
    notify_status: HashMap<String, NotifyStatusTracker>,
}

/// Telegram Bot API channel adapter.
///
/// ## Outbound
/// - Streaming tokens are buffered internally and sent as a single message on `Done`.
/// - Long responses are automatically chunked at paragraph/line/word boundaries to
///   stay within Telegram's 4096-character limit.
/// - Agent responses are sent with MarkdownV2 formatting with a plain-text fallback.
/// - `TypingIndicator` events trigger `sendChatAction` so the user sees the bot typing.
///
/// ## Inbound
/// - Uses Telegram's `getUpdates` long-polling API with exponential backoff.
/// - A single polling loop handles both `start_listener` messages and `ask_human` replies.
/// - Messages from senders not in `allowed_senders` are rejected with a notice.
/// - `allowed_senders` is live-mutable: the agent can update the TOML config and the
///   new list takes effect within 5 seconds via `on_config_update` — no restart needed.
///
/// ## Configuration
/// Set `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` env vars, or provide them
/// directly in `AdapterConfig`. Use `allowed_senders` to restrict which user IDs
/// can send messages to the bot.
pub struct TelegramAdapter {
    id: String,
    token: String,
    default_chat_id: String,
    http: reqwest::Client,
    state: Mutex<TelegramState>,
}

impl TelegramAdapter {
    /// Create a new Telegram adapter.
    pub fn new(
        id: impl Into<String>,
        token: impl Into<String>,
        chat_id: impl Into<String>,
        allowed_chats: Vec<String>,
        allowed_senders: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            token: token.into(),
            default_chat_id: chat_id.into(),
            http: reqwest::Client::new(),
            state: Mutex::new(TelegramState {
                token_buffers: HashMap::new(),
                draft_last_sent: HashMap::new(),
                update_offset: 0,
                pending_asks: HashMap::new(),
                allowed_chats,
                allowed_senders,
                tool_status: HashMap::new(),
                notify_status: HashMap::new(),
            }),
        }
    }

    fn target_chat_id<'a>(&'a self, target: Option<&'a OutboundTarget>) -> &'a str {
        target
            .and_then(|t| t.recipient_id.as_deref())
            .filter(|v| !v.is_empty())
            .unwrap_or(&self.default_chat_id)
    }

    /// Build the base API URL for this bot.
    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    /// Buffer key for streamed tokens. Prefers a stable session ID when available.
    fn stream_key(&self, target: Option<&OutboundTarget>) -> String {
        if let Some(session_id) = target
            .and_then(|t| t.session_id.as_deref())
            .filter(|s| !s.is_empty())
        {
            return format!("session:{session_id}");
        }
        if let Some(chat_id) = target
            .and_then(|t| t.recipient_id.as_deref())
            .filter(|s| !s.is_empty())
        {
            if let Some(sender_id) = target
                .and_then(|t| t.sender_id.as_deref())
                .filter(|s| !s.is_empty())
            {
                return format!("chat:{chat_id}:sender:{sender_id}");
            }
            return format!("chat:{chat_id}");
        }
        format!("chat:{}", self.default_chat_id)
    }

    /// Send a plain-text message (no Markdown parsing) to the configured chat.
    ///
    /// Automatically chunks long messages to stay within the 4096-char limit.
    async fn send_plain(&self, chat_id: &str, text: &str) -> Result<()> {
        for chunk in chunk_message(text, TELEGRAM_MAX_CHARS) {
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
            });
            let resp = self
                .http
                .post(self.api_url("sendMessage"))
                .json(&body)
                .send()
                .await
                .context("Telegram sendMessage (plain) request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Telegram sendMessage plain failed ({status}): {body}");
            }
        }
        Ok(())
    }

    /// Send a MarkdownV2-formatted message, falling back to plain text on parse errors.
    ///
    /// Automatically chunks long messages to stay within the 4096-char limit.
    async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        for chunk in chunk_message(text, TELEGRAM_MAX_CHARS) {
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": "MarkdownV2",
            });
            let resp = self
                .http
                .post(self.api_url("sendMessage"))
                .json(&body)
                .send()
                .await
                .context("Telegram sendMessage request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body_text = resp.text().await.unwrap_or_default();
                if is_markdown_parse_error(status, &body_text) {
                    debug!(channel = %self.id, "MarkdownV2 parse failed ({status}): {body_text} — retrying as plain text");
                    // Fall back to plain text if MarkdownV2 parse fails.
                    let plain = markdown_v2_to_plain(chunk);
                    self.send_plain(chat_id, &plain).await?;
                } else {
                    anyhow::bail!("Telegram sendMessage failed ({status}): {body_text}");
                }
            }
        }
        Ok(())
    }

    /// Show "typing…" indicator in the chat.
    ///
    /// Telegram's typing action expires after ~5 seconds, so callers should
    /// refresh it periodically for long-running agent turns.
    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": "typing",
        });
        let resp = self
            .http
            .post(self.api_url("sendChatAction"))
            .json(&body)
            .send()
            .await
            .context("Telegram sendChatAction request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            debug!(channel = %self.id, "sendChatAction failed ({status}) — ignoring");
        }
        Ok(())
    }

    /// Add an emoji reaction to a message using `setMessageReaction` (Bot API 7.0+).
    async fn set_reaction(&self, chat_id: &str, message_id: i64, emoji: &str) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{"type": "emoji", "emoji": emoji}],
        });
        let resp = self
            .http
            .post(self.api_url("setMessageReaction"))
            .json(&body)
            .send()
            .await
            .context("Telegram setMessageReaction request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            warn!(channel = %self.id, %emoji, "setMessageReaction failed ({status})");
        }
        Ok(())
    }

    /// Send a plain-text message and return the Telegram `message_id` from the response.
    ///
    /// Unlike [`Self::send_plain`] this does not chunk the message — intended for
    /// short, single-line messages such as tool-call status lines.
    async fn send_plain_get_id(&self, chat_id: &str, text: &str) -> Result<Option<i64>> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });
        let resp = self
            .http
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .context("Telegram sendMessage request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram sendMessage JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            let desc = resp["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram sendMessage failed: {desc}");
        }
        Ok(resp["result"]["message_id"].as_i64())
    }

    /// Stream a partial draft message to a DM chat via `sendMessageDraft` (Bot API 9.3+).
    ///
    /// Fire-and-forget: errors are non-fatal — the final `sendMessage` always commits the result.
    /// Only valid for private chats (positive chat ID); groups silently ignore this.
    async fn send_message_draft(&self, chat_id: &str, text: &str) -> Result<()> {
        let body = serde_json::json!({ "chat_id": chat_id, "text": text });
        let resp = self
            .http
            .post(self.api_url("sendMessageDraft"))
            .json(&body)
            .send()
            .await
            .context("sendMessageDraft request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("sendMessageDraft failed ({status}): {body}");
        }
        Ok(())
    }

    /// Edit an existing Telegram message in-place using `editMessageText`.
    async fn edit_message_text(&self, chat_id: &str, message_id: i64, text: &str) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        let resp = self
            .http
            .post(self.api_url("editMessageText"))
            .json(&body)
            .send()
            .await
            .context("Telegram editMessageText request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram editMessageText JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            let desc = resp["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram editMessageText failed: {desc}");
        }
        Ok(())
    }

    /// Send a file as a Telegram document using `sendDocument` (multipart form upload).
    ///
    /// Falls back to a plain-text caption-only message when the bytes are empty.
    async fn send_document(
        &self,
        chat_id: &str,
        filename: &str,
        data: &[u8],
        caption: Option<&str>,
    ) -> Result<()> {
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename.to_string())
            .mime_str(mime_type_from_filename(filename).unwrap_or("application/octet-stream"))?;
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);
        if let Some(cap) = caption.filter(|s| !s.is_empty()) {
            form = form.text("caption", cap.to_string());
        }

        let resp = self
            .http
            .post(self.api_url("sendDocument"))
            .multipart(form)
            .send()
            .await
            .context("Telegram sendDocument request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram sendDocument JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            let desc = resp["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram sendDocument failed: {desc}");
        }
        Ok(())
    }

    /// Delete a Telegram message by chat_id and message_id.
    async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });
        let resp = self
            .http
            .post(self.api_url("deleteMessage"))
            .json(&body)
            .send()
            .await
            .context("Telegram deleteMessage request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram deleteMessage JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            let desc = resp["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram deleteMessage failed: {desc}");
        }
        Ok(())
    }

    /// Poll for new updates once, returning the raw JSON result array.
    async fn poll_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<serde_json::Value>> {
        let url = self.api_url("getUpdates");
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", timeout_secs.to_string()),
                (
                    "allowed_updates",
                    r#"["message","callback_query"]"#.to_string(),
                ),
            ])
            .timeout(std::time::Duration::from_secs(timeout_secs + 5))
            .send()
            .await
            .context("Telegram getUpdates request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram getUpdates JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            anyhow::bail!(
                "Telegram getUpdates failed: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            );
        }
        Ok(resp["result"].as_array().cloned().unwrap_or_default())
    }

    /// Download a file from Telegram by its `file_id`.
    ///
    /// Uses `getFile` to resolve the file path, then fetches the bytes from the
    /// Telegram file API. Returns the raw bytes on success.
    async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(self.api_url("getFile"))
            .query(&[("file_id", file_id)])
            .send()
            .await
            .context("Telegram getFile request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram getFile JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            anyhow::bail!(
                "Telegram getFile failed: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            );
        }

        let file_path = resp["result"]["file_path"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Telegram getFile: missing file_path"))?;

        let url = format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.token, file_path
        );
        let bytes = self
            .http
            .get(&url)
            .send()
            .await
            .context("Telegram file download failed")?
            .bytes()
            .await
            .context("Telegram file download body read failed")?;

        Ok(bytes.to_vec())
    }
}

/// Telegram's per-message character limit.
const TELEGRAM_MAX_CHARS: usize = 4096;

/// Exponential backoff parameters for the polling loop.
const BACKOFF_INITIAL_SECS: u64 = 1;
const BACKOFF_MAX_SECS: u64 = 60;

#[async_trait]
impl Channel for TelegramAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            inbound: true,
            ask_human: true,
            typing_indicator: true,
            command_menu: true,
            max_message_len: TELEGRAM_MAX_CHARS,
            message_edit: true,
            attachments: true,
            inbound_images: true,
            inbound_audio: true,
            rich_messages: true,
            reactions: true,
            native_api: true,
            deferred_start: true,
        }
    }

    async fn on_config_update(&self, cfg: &AdapterConfig) {
        let mut state = self.state.lock().await;
        if state.allowed_chats != cfg.allowed_chats {
            info!(
                channel = %self.id,
                old = ?state.allowed_chats,
                new = ?cfg.allowed_chats,
                "Hot-reloaded allowed_chats"
            );
            state.allowed_chats = cfg.allowed_chats.clone();
        }
        if state.allowed_senders != cfg.allowed_senders {
            info!(
                channel = %self.id,
                old = ?state.allowed_senders,
                new = ?cfg.allowed_senders,
                "Hot-reloaded allowed_senders"
            );
            state.allowed_senders = cfg.allowed_senders.clone();
        }
    }

    fn format_instructions(&self) -> Option<String> {
        Some(
            "## Telegram Formatting\n\n\
             You are communicating via Telegram. Use MarkdownV2 formatting:\n\
             - Bold: `**text**` → `*text*`\n\
             - Italic: `_text_`\n\
             - Code: `` `code` `` or ` ```block``` `\n\
             - Escape special chars: `.`, `!`, `(`, `)`, `-`, `_`, `*`, `[`, `]`, `{`, `}`, `#`, `+`, `=`, `|`, `~`, `>`, `<`\n\
             Long responses are automatically split into multiple messages — no need to manually shorten them.\n\n\
             ## Rich Messages (Telegram)\n\n\
             Use `channel_send_message` to send messages with interactive UI elements:\n\
             - Inline keyboards: rows of buttons with callback data, rendered below the message\n\
             - Reply keyboards: custom keyboard layouts replacing the default keyboard\n\
             - Remove keyboard: dismiss a previously shown reply keyboard\n\
             Parse modes: `MarkdownV2`, `HTML`, `Plain`.\n\n\
             ## Native API (Telegram)\n\n\
             Use `channel_send_raw` to call any Telegram Bot API method directly when the \
             rich message model doesn't cover your use case. Pass the method name and a JSON \
             payload — the response JSON is returned verbatim."
                .to_string(),
        )
    }

    /// Validate the bot token by calling `getMe`, then ensure no webhook is registered
    /// that would compete with long-polling.
    async fn on_start(&self) -> Result<()> {
        let resp = self
            .http
            .get(self.api_url("getMe"))
            .send()
            .await
            .context("Telegram on_start: getMe request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram on_start: getMe response parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            anyhow::bail!(
                "Telegram bot token validation failed: {}",
                resp["description"].as_str().unwrap_or("unknown error")
            );
        }

        let bot_name = resp["result"]["username"].as_str().unwrap_or("unknown");
        info!(channel = %self.id, bot = %bot_name, "Telegram bot validated");

        // Check for an active webhook — if one exists it will consume all updates
        // and getUpdates long-polling will never receive any messages.
        let wh = self
            .http
            .get(self.api_url("getWebhookInfo"))
            .send()
            .await
            .context("Telegram on_start: getWebhookInfo request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram on_start: getWebhookInfo response parse failed")?;

        let webhook_url = wh["result"]["url"].as_str().unwrap_or("").to_string();
        if !webhook_url.is_empty() {
            warn!(
                channel = %self.id,
                url = %webhook_url,
                "Active webhook detected — deleting it so long-polling can receive messages"
            );
            let del = self
                .http
                .post(self.api_url("deleteWebhook"))
                .json(&serde_json::json!({"drop_pending_updates": false}))
                .send()
                .await
                .context("Telegram on_start: deleteWebhook request failed")?
                .json::<serde_json::Value>()
                .await
                .context("Telegram on_start: deleteWebhook response parse failed")?;

            if del["ok"].as_bool() != Some(true) {
                anyhow::bail!(
                    "Telegram deleteWebhook failed: {}",
                    del["description"].as_str().unwrap_or("unknown error")
                );
            }
            info!(channel = %self.id, "Webhook deleted — long-polling is now active");
        }

        Ok(())
    }

    async fn send_event(
        &self,
        event: &ChannelEvent,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        let chat_id = self.target_chat_id(target).to_string();
        let stream_key = self.stream_key(target);
        let handle = match event {
            ChannelEvent::StreamToken(token) => {
                // Buffer tokens — send as one message on Done.
                // Also push live draft updates via sendMessageDraft for DM chats (Bot API 9.3+).
                let is_dm = chat_id.parse::<i64>().map(|id| id > 0).unwrap_or(false);
                let mut state = self.state.lock().await;
                // Push token first — entry borrow ends at the semicolon.
                state
                    .token_buffers
                    .entry(stream_key.clone())
                    .or_default()
                    .push_str(token);
                let draft_text = if is_dm {
                    let now = std::time::Instant::now();
                    let due = state
                        .draft_last_sent
                        .get(&stream_key)
                        .map(|t| now.duration_since(*t).as_millis() >= DRAFT_THROTTLE_MS)
                        .unwrap_or(true);
                    if due {
                        state.draft_last_sent.insert(stream_key.clone(), now);
                        state.token_buffers.get(&stream_key).cloned()
                    } else {
                        None
                    }
                } else {
                    None
                };
                drop(state);
                if let Some(text) = draft_text {
                    if let Err(e) = self.send_message_draft(&chat_id, &text).await {
                        debug!(channel = %self.id, "sendMessageDraft failed (non-fatal): {e:#}");
                    }
                }
                MessageHandle::default()
            }
            ChannelEvent::TypingIndicator => {
                self.send_typing(&chat_id).await?;
                MessageHandle::default()
            }
            ChannelEvent::ThinkingDelta(_) => {
                // Thinking deltas are not forwarded to Telegram.
                MessageHandle::default()
            }
            ChannelEvent::Reset => {
                // New run starting — discard any stale state from an aborted previous run.
                let mut state = self.state.lock().await;
                state.token_buffers.remove(&stream_key);
                state.draft_last_sent.remove(&stream_key);
                state.tool_status.remove(&chat_id);
                MessageHandle::default()
            }
            ChannelEvent::ToolCall { name, args, .. } => {
                // Discard buffered text — it's intermediate reasoning, not user-facing output.
                {
                    let mut state = self.state.lock().await;
                    state.token_buffers.remove(&stream_key);
                    state.draft_last_sent.remove(&stream_key);
                }
                // Consolidate all tool calls into a single editable status message.
                // Each new ToolCall edits the message to show the current tool
                // prominently and completed tools stacked below.
                let line = format!("{name}: {args}");
                let (text, existing_msg_id) = {
                    let mut state = self.state.lock().await;
                    let tracker = state.tool_status.entry(chat_id.clone()).or_insert_with(|| {
                        ToolStatusTracker {
                            message_id: 0,
                            completed: Vec::new(),
                            current_line: None,
                        }
                    });
                    // Move previous current tool to completed list.
                    if let Some(prev) = tracker.current_line.take() {
                        let name_only = prev.split(':').next().unwrap_or(&prev).trim().to_string();
                        tracker.completed.push(name_only);
                    }
                    tracker.current_line = Some(line);
                    (tracker.render(), tracker.message_id)
                };

                if existing_msg_id == 0 {
                    // First tool call in this chat — send a new message.
                    let msg_id = self.send_plain_get_id(&chat_id, &text).await?;
                    if let Some(mid) = msg_id {
                        let mut state = self.state.lock().await;
                        if let Some(t) = state.tool_status.get_mut(&chat_id) {
                            t.message_id = mid;
                        }
                    }
                } else {
                    // Edit the existing status message.
                    if let Err(e) = self
                        .edit_message_text(&chat_id, existing_msg_id, &text)
                        .await
                    {
                        warn!(channel = %self.id, "Tool status edit failed: {e:#}");
                    }
                }
                MessageHandle::default()
            }
            ChannelEvent::ToolResult { name, .. } => {
                // Move current tool to completed. No edit here — the next
                // ToolCall (or Done) will update the display.
                let mut state = self.state.lock().await;
                if let Some(tracker) = state.tool_status.get_mut(&chat_id) {
                    if tracker.current_line.is_some() {
                        tracker.current_line = None;
                        tracker.completed.push(name.clone());
                    }
                }
                MessageHandle::default()
            }
            ChannelEvent::Done { text, .. } => {
                // Delete the tool status message — the agent's response replaces it.
                let status_msg_id = {
                    let mut state = self.state.lock().await;
                    state
                        .tool_status
                        .remove(&chat_id)
                        .filter(|t| t.message_id > 0)
                        .map(|t| t.message_id)
                };
                if let Some(msg_id) = status_msg_id {
                    if let Err(e) = self.delete_message(&chat_id, msg_id).await {
                        debug!(channel = %self.id, "Tool status delete failed: {e:#}");
                    }
                }

                let mut state = self.state.lock().await;
                let buffered = state.token_buffers.remove(&stream_key);
                drop(state);

                // If the Done event carries explicit text (from the `answer` tool),
                // use it — it's the agent's complete final response. The streaming
                // buffer may only contain a short preamble the agent emitted before
                // calling answer.
                let message = if !text.is_empty() {
                    text.clone()
                } else {
                    buffered.filter(|b| !b.is_empty()).unwrap_or_default()
                };
                if message.is_empty() {
                    // Nothing to send.
                } else if target.map(|t| t.status_update).unwrap_or(false) {
                    // Heartbeat response — merge into the live cluster status message
                    // as the top "parent" summary line instead of a new message.
                    let (rendered, existing_id) = {
                        let mut state = self.state.lock().await;
                        let tracker = state
                            .notify_status
                            .entry(chat_id.clone())
                            .or_insert_with(NotifyStatusTracker::new);
                        tracker.upsert("parent".to_string(), message.clone());
                        tracker.last_edit = Some(std::time::Instant::now());
                        (tracker.render(), tracker.message_id)
                    };
                    if existing_id == 0 {
                        if let Ok(Some(mid)) = self.send_plain_get_id(&chat_id, &rendered).await {
                            let mut state = self.state.lock().await;
                            if let Some(t) = state.notify_status.get_mut(&chat_id) {
                                t.message_id = mid;
                            }
                        }
                    } else if let Err(e) = self
                        .edit_message_text(&chat_id, existing_id, &rendered)
                        .await
                    {
                        debug!(channel = %self.id, "Heartbeat status edit failed: {e:#}");
                        self.state.lock().await.notify_status.remove(&chat_id);
                    }
                } else {
                    self.send_text(&chat_id, &message).await?;
                }
                MessageHandle::default()
            }
            ChannelEvent::Error(err) => {
                // Delete the tool status message on error too.
                let status_msg_id = {
                    let mut state = self.state.lock().await;
                    state.token_buffers.remove(&stream_key);
                    state
                        .tool_status
                        .remove(&chat_id)
                        .filter(|t| t.message_id > 0)
                        .map(|t| t.message_id)
                };
                if let Some(msg_id) = status_msg_id {
                    if let Err(e) = self.delete_message(&chat_id, msg_id).await {
                        debug!(channel = %self.id, "Tool status delete failed: {e:#}");
                    }
                }
                self.send_plain(&chat_id, &format!("❌ Error: {err}"))
                    .await?;
                MessageHandle::default()
            }
            ChannelEvent::Retrying {
                attempt,
                max_attempts,
                delay_secs,
            } => {
                self.send_plain(
                    &chat_id,
                    &format!(
                        "⏳ Network error — retrying ({attempt}/{max_attempts}) in {delay_secs}s…"
                    ),
                )
                .await?;
                MessageHandle::default()
            }
            ChannelEvent::Notify(msg) => {
                // Extract "[agent-name] rest" prefix if present.
                let (agent_key, status_text) = if msg.starts_with('[') {
                    if let Some(close) = msg.find(']') {
                        let key = msg[1..close].to_string();
                        let text = msg[close + 1..].trim().to_string();
                        (key, text)
                    } else {
                        ("system".to_string(), msg.clone())
                    }
                } else {
                    ("system".to_string(), msg.clone())
                };

                let (rendered, existing_id, ready) = {
                    let mut state = self.state.lock().await;
                    let tracker = state
                        .notify_status
                        .entry(chat_id.clone())
                        .or_insert_with(NotifyStatusTracker::new);
                    tracker.upsert(agent_key, status_text);
                    let ready = tracker.ready_to_edit();
                    if ready {
                        tracker.last_edit = Some(std::time::Instant::now());
                    }
                    (tracker.render(), tracker.message_id, ready)
                };

                if !ready {
                    // Throttled — state updated, no API call.
                } else if existing_id == 0 {
                    if let Ok(Some(mid)) = self.send_plain_get_id(&chat_id, &rendered).await {
                        let mut state = self.state.lock().await;
                        if let Some(t) = state.notify_status.get_mut(&chat_id) {
                            t.message_id = mid;
                        }
                    }
                } else if let Err(e) = self
                    .edit_message_text(&chat_id, existing_id, &rendered)
                    .await
                {
                    debug!(channel = %self.id, "Notify status edit failed: {e:#}");
                    // Message was deleted or too old — reset so next notify starts fresh.
                    self.state.lock().await.notify_status.remove(&chat_id);
                }
                MessageHandle::default()
            }
            ChannelEvent::Attachment {
                filename,
                data,
                caption,
                ..
            } => {
                self.send_document(&chat_id, filename, data, caption.as_deref())
                    .await?;
                MessageHandle::default()
            }
        };
        Ok(handle)
    }

    async fn update_message(
        &self,
        handle: &MessageHandle,
        content: &str,
        _target: Option<&OutboundTarget>,
    ) -> Result<()> {
        let chat_id = handle
            .conversation_id
            .as_deref()
            .unwrap_or(&self.default_chat_id);
        let message_id: i64 = match handle.message_id.as_deref().and_then(|s| s.parse().ok()) {
            Some(id) => id,
            None => return Ok(()),
        };
        if let Err(e) = self.edit_message_text(chat_id, message_id, content).await {
            warn!(channel = %self.id, message_id, "update_message failed: {e:#}");
        }
        Ok(())
    }

    async fn ask_human(
        &self,
        message: &str,
        timeout: Option<u64>,
        target: Option<&OutboundTarget>,
    ) -> Result<String> {
        let timeout_secs = timeout.unwrap_or(300);
        let chat_id = self.target_chat_id(target).to_string();
        let sender_id = target
            .and_then(|t| t.sender_id.as_deref())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let ask_key = ask_key(&chat_id, sender_id.as_deref());

        // Register a oneshot channel — the listener loop will fulfill it.
        let (ask_tx, ask_rx) = oneshot::channel::<String>();
        {
            let mut state = self.state.lock().await;
            if state.pending_asks.contains_key(&ask_key) {
                anyhow::bail!(
                    "Telegram ask_human already pending for chat {}{}",
                    chat_id,
                    sender_id
                        .as_deref()
                        .map(|s| format!(", sender {}", s))
                        .unwrap_or_default()
                );
            }
            state.pending_asks.insert(ask_key.clone(), ask_tx);
        }

        if let Err(e) = self.send_text(&chat_id, message).await {
            let mut state = self.state.lock().await;
            state.pending_asks.remove(&ask_key);
            return Err(e);
        }

        // Wait for the listener loop to deliver the user's reply.
        match tokio::time::timeout(tokio::time::Duration::from_secs(timeout_secs), ask_rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => {
                let mut state = self.state.lock().await;
                state.pending_asks.remove(&ask_key);
                Err(anyhow::anyhow!(
                    "Telegram ask_human: response channel closed"
                ))
            }
            Err(_) => {
                // Clean up stale oneshot.
                let mut state = self.state.lock().await;
                state.pending_asks.remove(&ask_key);
                Err(anyhow::anyhow!(
                    "Telegram ask_human timed out after {timeout_secs}s"
                ))
            }
        }
    }

    async fn react(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let effective_chat = if chat_id.is_empty() {
            &self.default_chat_id
        } else {
            chat_id
        };
        let mid: i64 = message_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid Telegram message_id: {message_id}"))?;
        self.set_reaction(effective_chat, mid, emoji).await
    }

    async fn send_message(
        &self,
        msg: crate::channels::message::OutboundMessage,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        let chat_id = self.target_chat_id(target).to_string();
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": msg.text,
        });
        match msg.parse_mode {
            Some(crate::channels::message::ParseMode::MarkdownV2) => {
                body["parse_mode"] = "MarkdownV2".into();
            }
            Some(crate::channels::message::ParseMode::HTML) => {
                body["parse_mode"] = "HTML".into();
            }
            Some(crate::channels::message::ParseMode::Plain) | None => {}
        }
        if let Some(reply_id) = msg
            .reply_to_message_id
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
        {
            body["reply_to_message_id"] = reply_id.into();
        }
        if let Some(markup) = &msg.reply_markup {
            body["reply_markup"] = match markup {
                crate::channels::message::ReplyMarkup::InlineKeyboard(rows) => {
                    let keyboard: Vec<Vec<serde_json::Value>> = rows
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|btn| {
                                    serde_json::json!({
                                        "text": btn.text,
                                        "callback_data": btn.callback_data,
                                    })
                                })
                                .collect()
                        })
                        .collect();
                    serde_json::json!({ "inline_keyboard": keyboard })
                }
                crate::channels::message::ReplyMarkup::ReplyKeyboard {
                    keyboard,
                    resize,
                    one_time,
                } => {
                    let kb: Vec<Vec<serde_json::Value>> = keyboard
                        .iter()
                        .map(|row| {
                            row.iter()
                                .map(|btn| serde_json::json!({ "text": btn.text }))
                                .collect()
                        })
                        .collect();
                    serde_json::json!({
                        "keyboard": kb,
                        "resize_keyboard": resize,
                        "one_time_keyboard": one_time,
                    })
                }
                crate::channels::message::ReplyMarkup::RemoveKeyboard => {
                    serde_json::json!({ "remove_keyboard": true })
                }
                crate::channels::message::ReplyMarkup::ForceReply {
                    input_field_placeholder,
                } => {
                    let mut v = serde_json::json!({
                        "force_reply": true,
                        "selective": true,
                    });
                    if let Some(placeholder) = input_field_placeholder {
                        v["input_field_placeholder"] = placeholder.clone().into();
                    }
                    v
                }
            };
        }

        let resp = self
            .http
            .post(self.api_url("sendMessage"))
            .json(&body)
            .send()
            .await
            .context("Telegram sendMessage (rich) request failed")?
            .json::<serde_json::Value>()
            .await
            .context("Telegram sendMessage (rich) JSON parse failed")?;

        if resp["ok"].as_bool() != Some(true) {
            let desc = resp["description"].as_str().unwrap_or("unknown error");
            anyhow::bail!("Telegram sendMessage (rich) failed: {desc}");
        }
        let message_id = resp["result"]["message_id"]
            .as_i64()
            .map(|id| id.to_string());
        Ok(MessageHandle {
            message_id,
            channel_id: Some(self.id.clone()),
            conversation_id: Some(chat_id),
        })
    }

    async fn send_raw(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let resp = self
            .http
            .post(self.api_url(method))
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("Telegram {method} request failed"))?
            .json::<serde_json::Value>()
            .await
            .with_context(|| format!("Telegram {method} JSON parse failed"))?;
        Ok(resp)
    }

    async fn register_commands(&self, commands: &[BotCommand]) -> Result<()> {
        let tg_commands: Vec<serde_json::Value> = commands
            .iter()
            .map(|c| {
                // Telegram descriptions have a 256-char limit.
                let desc: String = c.description.chars().take(256).collect();
                serde_json::json!({"command": c.command, "description": desc})
            })
            .collect();

        let body = serde_json::json!({"commands": tg_commands});
        let resp = self
            .http
            .post(self.api_url("setMyCommands"))
            .json(&body)
            .send()
            .await
            .context("Telegram setMyCommands request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Telegram setMyCommands failed ({status}): {body}");
        } else {
            info!(channel = %self.id, count = commands.len(), "Registered slash commands with Telegram");
        }
        Ok(())
    }

    async fn start_listener(&self, tx: mpsc::UnboundedSender<InboundMessage>) -> Result<()> {
        let mut offset = {
            let state = self.state.lock().await;
            state.update_offset
        };

        info!(channel = %self.id, "Telegram long-poll listener started (offset={offset})");

        let mut backoff_secs = BACKOFF_INITIAL_SECS;
        let mut consecutive_errors: u32 = 0;

        loop {
            debug!(channel = %self.id, offset, "Polling getUpdates…");
            let updates = match self.poll_updates(offset, 30).await {
                Ok(u) => {
                    // Reset backoff on any successful poll.
                    if consecutive_errors > 0 {
                        info!(
                            channel = %self.id,
                            "Telegram polling recovered after {consecutive_errors} consecutive errors"
                        );
                    }
                    consecutive_errors = 0;
                    backoff_secs = BACKOFF_INITIAL_SECS;
                    u
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        channel = %self.id,
                        consecutive = consecutive_errors,
                        "Telegram poll error: {e:#}"
                    );
                    if consecutive_errors >= 3 {
                        warn!(
                            channel = %self.id,
                            "Telegram polling degraded — check connectivity and bot token"
                        );
                    }
                    tokio::time::sleep(tokio::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(BACKOFF_MAX_SECS);
                    continue;
                }
            };

            if updates.is_empty() {
                debug!(channel = %self.id, "Poll returned no updates");
            } else {
                debug!(channel = %self.id, count = updates.len(), "Poll returned updates");
            }

            for update in &updates {
                let update_id = update["update_id"].as_i64().unwrap_or(0);
                offset = update_id + 1;

                // Handle callback_query updates from inline keyboard buttons.
                if let Some(cb) = update.get("callback_query") {
                    let cb_id = cb["id"].as_str().unwrap_or_default();
                    let cb_data = cb["data"].as_str().unwrap_or_default().to_string();
                    let cb_chat_id = cb["message"]["chat"]["id"]
                        .as_i64()
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let cb_sender_id = cb["from"]["id"]
                        .as_i64()
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let cb_message_id = cb["message"]["message_id"]
                        .as_i64()
                        .map(|id| id.to_string());

                    // Auto-answer to dismiss the client spinner.
                    let _ = self
                        .http
                        .post(self.api_url("answerCallbackQuery"))
                        .json(&serde_json::json!({ "callback_query_id": cb_id }))
                        .send()
                        .await;

                    if !cb_data.is_empty() {
                        let msg = InboundMessage {
                            channel_id: self.id.clone(),
                            sender_id: cb_sender_id,
                            text: cb_data.clone(),
                            message_id: cb_message_id,
                            conversation_id: Some(cb_chat_id),
                            session_hint: None,
                            attachments: vec![],
                            callback_url: None,
                            deferred: false,
                            metadata: Some(serde_json::json!({
                                "type": "callback_query",
                                "callback_query_id": cb_id,
                                "data": cb_data,
                            })),
                        };
                        if tx.send(msg).is_err() {
                            info!(channel = %self.id, "Inbound receiver dropped — stopping listener");
                            return Ok(());
                        }
                    }
                    continue;
                }

                let chat_id = update["message"]["chat"]["id"]
                    .as_i64()
                    .map(|id| id.to_string())
                    .unwrap_or_default();

                // Log every arriving update so operators can diagnose filtering issues.
                info!(
                    channel = %self.id,
                    update_id,
                    chat_id,
                    update_type = if update["message"].is_object() { "message" } else { "other" },
                    "Update received from Telegram"
                );

                // Accept messages from the primary chat_id and any extra allowed_chats.
                let chat_accepted = {
                    let s = self.state.lock().await;
                    chat_id == self.default_chat_id || s.allowed_chats.contains(&chat_id)
                };
                if !chat_accepted {
                    warn!(
                        channel = %self.id,
                        update_id,
                        chat_id,
                        expected = %self.default_chat_id,
                        "Dropping update: chat_id not in allowed list — check chat_id in agent config"
                    );
                    continue;
                }

                // Extract text: prefer caption (for photos/voice), fall back to text.
                let raw_text = update["message"]["caption"]
                    .as_str()
                    .or_else(|| update["message"]["text"].as_str())
                    .unwrap_or("");

                // If this is a reply, prepend the quoted message as context so the
                // agent knows what the user is referring to.
                let text = if let Some(quoted) = update["message"]["reply_to_message"]["text"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                {
                    let sender = update["message"]["reply_to_message"]["from"]["first_name"]
                        .as_str()
                        .unwrap_or("Someone");
                    format!("[Replying to {sender}: \"{quoted}\"]\n\n{raw_text}")
                } else {
                    raw_text.to_string()
                };

                // Build attachments from photo, voice, and audio fields.
                let mut attachments = Vec::new();

                // Photo: array of PhotoSize, take the last (largest) entry.
                if let Some(photos) = update["message"]["photo"].as_array() {
                    if let Some(largest) = photos.last() {
                        if let Some(file_id) = largest["file_id"].as_str() {
                            match self.download_file(file_id).await {
                                Ok(data) => {
                                    attachments.push(InboundAttachment::Image {
                                        data,
                                        mime_type: "image/jpeg".to_string(),
                                    });
                                }
                                Err(e) => {
                                    warn!(channel = %self.id, "Failed to download photo: {e:#}");
                                }
                            }
                        }
                    }
                }

                // Voice message.
                if let Some(file_id) = update["message"]["voice"]["file_id"].as_str() {
                    let duration = update["message"]["voice"]["duration"]
                        .as_u64()
                        .map(|d| d as u32);
                    match self.download_file(file_id).await {
                        Ok(data) => {
                            attachments.push(InboundAttachment::Audio {
                                data,
                                mime_type: "audio/ogg".to_string(),
                                duration_secs: duration,
                            });
                        }
                        Err(e) => {
                            warn!(channel = %self.id, "Failed to download voice: {e:#}");
                        }
                    }
                }

                // Audio file (music, podcasts, etc.).
                if let Some(file_id) = update["message"]["audio"]["file_id"].as_str() {
                    let duration = update["message"]["audio"]["duration"]
                        .as_u64()
                        .map(|d| d as u32);
                    let mime = update["message"]["audio"]["mime_type"]
                        .as_str()
                        .unwrap_or("audio/mpeg")
                        .to_string();
                    match self.download_file(file_id).await {
                        Ok(data) => {
                            attachments.push(InboundAttachment::Audio {
                                data,
                                mime_type: mime,
                                duration_secs: duration,
                            });
                        }
                        Err(e) => {
                            warn!(channel = %self.id, "Failed to download audio: {e:#}");
                        }
                    }
                }

                // Document (zip, pdf, txt, etc.).
                if let Some(file_id) = update["message"]["document"]["file_id"].as_str() {
                    let mime = update["message"]["document"]["mime_type"]
                        .as_str()
                        .unwrap_or("application/octet-stream")
                        .to_string();
                    let filename = update["message"]["document"]["file_name"]
                        .as_str()
                        .map(String::from);
                    match self.download_file(file_id).await {
                        Ok(data) => {
                            attachments.push(InboundAttachment::Document {
                                data,
                                mime_type: mime,
                                filename,
                            });
                        }
                        Err(e) => {
                            warn!(channel = %self.id, "Failed to download document: {e:#}");
                        }
                    }
                }

                // Skip if both text and attachments are empty.
                if text.is_empty() && attachments.is_empty() {
                    debug!(
                        channel = %self.id,
                        update_id,
                        chat_id,
                        "Dropping update: no text and no supported attachments"
                    );
                    continue;
                }

                let sender_id = update["message"]["from"]["id"]
                    .as_i64()
                    .map(|id| id.to_string())
                    .unwrap_or_default();

                // Allow-list check: if a whitelist is configured, reject unknown senders.
                // Read from state so updates via on_config_update take effect immediately.
                let allowed = {
                    let s = self.state.lock().await;
                    s.allowed_senders.clone()
                };
                if !allowed.is_empty() && !allowed.contains(&sender_id) {
                    info!(channel = %self.id, update_id, sender = %sender_id, "Rejecting message from unlisted sender");
                    let _ = self
                        .send_plain(&chat_id, "You are not authorized to use this bot.")
                        .await;
                    continue;
                }

                info!(
                    channel = %self.id,
                    update_id,
                    sender = %sender_id,
                    attachments = attachments.len(),
                    "Received message: {text:?}"
                );

                // Check if ask_human is waiting for a reply (text-only).
                let mut state = self.state.lock().await;
                state.update_offset = offset;
                let exact_key = ask_key(&chat_id, Some(&sender_id));
                let wildcard_key = ask_key(&chat_id, None);
                if let Some(ask_tx) = state
                    .pending_asks
                    .remove(&exact_key)
                    .or_else(|| state.pending_asks.remove(&wildcard_key))
                {
                    info!(channel = %self.id, sender = %sender_id, "Routing message to pending ask_human");
                    let _ = ask_tx.send(text);
                    continue;
                }
                drop(state);

                // Otherwise route to the inbound channel.
                let message_id = update["message"]["message_id"]
                    .as_i64()
                    .map(|id| id.to_string());
                info!(channel = %self.id, sender = %sender_id, message_id = ?message_id, "Dispatching inbound message to agent");
                let msg = InboundMessage {
                    channel_id: self.id.clone(),
                    sender_id,
                    text,
                    message_id,
                    conversation_id: Some(chat_id),
                    session_hint: None,
                    attachments,
                    callback_url: None,
                    deferred: false,
                    metadata: None,
                };
                if tx.send(msg).is_err() {
                    // Receiver dropped — stop listening.
                    info!(channel = %self.id, "Inbound receiver dropped — stopping listener");
                    return Ok(());
                }
            }
        }
    }
}

/// Split a message into chunks that fit within `max_chars`, breaking at natural
/// boundaries (paragraph → line → word → hard cut) to keep messages readable.
fn chunk_message(text: &str, max_chars: usize) -> Vec<&str> {
    if text.chars().count() <= max_chars {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining);
            break;
        }

        // Find the byte offset of the `max_chars`-th character boundary.
        let byte_limit = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let candidate = &remaining[..byte_limit];

        // Prefer split at paragraph (double newline), then line, then space.
        let split_byte = candidate
            .rfind("\n\n")
            .map(|i| i + 2)
            .or_else(|| candidate.rfind('\n').map(|i| i + 1))
            .or_else(|| candidate.rfind(' ').map(|i| i + 1))
            .unwrap_or(byte_limit);

        // Ensure split_byte lands on a valid UTF-8 char boundary.
        let split_byte = advance_to_char_boundary(remaining, split_byte);

        let chunk = remaining[..split_byte].trim_end();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        remaining = remaining[split_byte..]
            .trim_start_matches('\n')
            .trim_start();
    }

    chunks
}

/// Advance `pos` forward to the nearest valid UTF-8 char boundary in `s`.
fn advance_to_char_boundary(s: &str, mut pos: usize) -> usize {
    while pos < s.len() && !s.is_char_boundary(pos) {
        pos += 1;
    }
    pos.min(s.len())
}

/// Key used to match ask_human prompts with inbound replies.
fn ask_key(chat_id: &str, sender_id: Option<&str>) -> String {
    match sender_id {
        Some(sender) if !sender.is_empty() => format!("chat:{chat_id}:sender:{sender}"),
        _ => format!("chat:{chat_id}:sender:*"),
    }
}

/// Guess a MIME type from a filename extension.
///
/// Returns a static MIME type string or `None` for unknown extensions. Used
/// to set the `Content-Type` when uploading documents to Telegram.
fn mime_type_from_filename(filename: &str) -> Option<&'static str> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "csv" => "text/csv",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        _ => return None,
    })
}

fn is_markdown_parse_error(status: reqwest::StatusCode, body: &str) -> bool {
    if status != reqwest::StatusCode::BAD_REQUEST {
        return false;
    }
    let body_lower = body.to_ascii_lowercase();
    body_lower.contains("parse entities")
        || body_lower.contains("can't parse entities")
        || body_lower.contains("can't find end")
}

fn markdown_v2_to_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                // Remove markdown escapes before punctuation/special chars so plain fallback
                // stays readable if MarkdownV2 parsing fails.
                if next.is_ascii_punctuation() || next == '\\' {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_message_preserves_unicode_and_limits_size() {
        let input = "hello😀world".repeat(300);
        let chunks = chunk_message(&input, 128);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 128));
        assert_eq!(chunks.concat(), input);
    }

    #[test]
    fn ask_key_distinguishes_sender() {
        assert_eq!(ask_key("123", Some("1")), "chat:123:sender:1");
        assert_eq!(ask_key("123", None), "chat:123:sender:*");
    }

    #[test]
    fn markdown_v2_to_plain_unescapes_visible_escapes() {
        let input = r"Here\'s \(CoinGecko id: `worldcoin\-wld`\)\:";
        let got = markdown_v2_to_plain(input);
        assert_eq!(got, "Here's (CoinGecko id: `worldcoin-wld`):");
    }
}
