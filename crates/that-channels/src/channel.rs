use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::AdapterConfig;
use crate::message::OutboundMessage;

/// An attachment received alongside an inbound user message.
#[derive(Debug, Clone)]
pub enum InboundAttachment {
    Image {
        data: Vec<u8>,
        mime_type: String,
    },
    Audio {
        data: Vec<u8>,
        mime_type: String,
        duration_secs: Option<u32>,
    },
    Document {
        data: Vec<u8>,
        mime_type: String,
        filename: Option<String>,
    },
}

/// Events emitted by the agent during a run, broadcast to all active channels.
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// A streaming text token from the assistant.
    StreamToken(String),
    /// A reasoning/thinking delta (shown in debug-capable channels).
    ThinkingDelta(String),
    /// The agent is actively processing — channels that support typing indicators
    /// should show one. Sent once at the start of a run and refreshed every ~4s.
    TypingIndicator,
    /// The agent is invoking a tool.
    ToolCall {
        call_id: String,
        name: String,
        args: String,
    },
    /// A tool returned its result.
    ToolResult {
        call_id: String,
        name: String,
        result: String,
    },
    /// Clear per-run adapter state (buffered tokens, in-progress tool indicators).
    ///
    /// Fired at the start of every agent run attempt so adapters discard any stale
    /// state left behind by a previously aborted run targeting the same session.
    Reset,
    /// The agent turn completed successfully.
    Done {
        text: String,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        cache_write_tokens: u64,
    },
    /// The agent turn failed with an error.
    Error(String),
    /// A transient error occurred; the agent is retrying with backoff.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
    },
    /// A proactive mid-task notification from the agent (via channel_notify tool).
    Notify(String),
    /// A file attachment sent by the agent (via channel_send_file tool).
    ///
    /// Adapters that support attachments (`capabilities().attachments == true`) should
    /// deliver the file using their native file-sending API (e.g. Telegram `sendDocument`).
    /// Adapters that do not support attachments should fall back to a plain-text
    /// notification describing the file.
    Attachment {
        /// Suggested filename for the recipient (e.g. "report.csv").
        filename: String,
        /// Raw file bytes. Wrapped in `Arc` to avoid expensive clones during fan-out.
        data: std::sync::Arc<Vec<u8>>,
        /// Optional caption shown alongside the file.
        caption: Option<String>,
        /// Optional MIME type hint (e.g. "text/csv", "application/pdf").
        mime_type: Option<String>,
    },
}

/// A handle to a previously sent message, returned by [`Channel::send_event`].
///
/// Adapters that support message editing (e.g. Telegram) populate `message_id`
/// and `conversation_id` so callers can later call [`Channel::update_message`].
/// Adapters that do not support editing return [`MessageHandle::default()`].
#[derive(Debug, Clone, Default)]
pub struct MessageHandle {
    /// Platform-native message ID (stringified per-platform: Telegram i64, Discord snowflake, Slack timestamp, etc.).
    pub message_id: Option<String>,
    /// Channel identifier the handle belongs to (e.g. "telegram").
    pub channel_id: Option<String>,
    /// Conversation-level identifier (chat ID, thread ID, etc.).
    pub conversation_id: Option<String>,
}

/// Declared capabilities of a channel adapter.
///
/// Used by the channel router to make capability-aware routing decisions —
/// e.g. only route `human_ask` to channels that can respond, only start
/// inbound listeners for channels that support them, etc.
#[derive(Debug, Clone)]
pub struct ChannelCapabilities {
    /// Adapter can receive inbound messages from users.
    pub inbound: bool,
    /// Adapter supports blocking `ask_human` interactions (bidirectional).
    pub ask_human: bool,
    /// Adapter can show a "typing…" indicator while the agent is working.
    pub typing_indicator: bool,
    /// Adapter supports registering a platform-native slash command menu.
    pub command_menu: bool,
    /// Maximum characters per outbound message before chunking is needed.
    pub max_message_len: usize,
    /// Adapter supports editing previously sent messages via [`Channel::update_message`].
    pub message_edit: bool,
    /// Adapter can deliver file attachments natively (e.g. Telegram `sendDocument`).
    ///
    /// When `false`, the router will fall back to sending a plain-text notification
    /// describing the file instead of delivering the bytes.
    pub attachments: bool,
    /// Adapter can receive inbound image attachments.
    pub inbound_images: bool,
    /// Adapter can receive inbound audio attachments.
    pub inbound_audio: bool,
    /// Adapter supports structured [`OutboundMessage`] with rich UI elements
    /// (inline keyboards, reply markups, etc.) via [`Channel::send_message`].
    pub rich_messages: bool,
    /// Adapter supports adding emoji reactions to messages via [`Channel::react`].
    pub reactions: bool,
    /// Adapter supports raw platform API passthrough via [`Channel::send_raw`].
    pub native_api: bool,
    /// `on_start()` requires external network calls (DNS, TLS, API validation).
    ///
    /// Channels with this flag are initialized *after* the readiness probe
    /// fires, so slow external APIs don't block K8s startup.
    pub deferred_start: bool,
}

impl Default for ChannelCapabilities {
    fn default() -> Self {
        Self {
            inbound: false,
            ask_human: false,
            typing_indicator: false,
            command_menu: false,
            max_message_len: 4096,
            message_edit: false,
            attachments: false,
            inbound_images: false,
            inbound_audio: false,
            rich_messages: false,
            reactions: false,
            native_api: false,
            deferred_start: false,
        }
    }
}

/// A message received from an external channel (inbound).
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Identifier of the adapter that received this message (e.g. "telegram", "tui").
    pub channel_id: String,
    /// Identifier of the sender within the channel (user ID, username, etc.).
    pub sender_id: String,
    /// The message text content.
    pub text: String,
    /// Platform-native message ID (stringified), used for reactions and reply threading.
    /// `None` for channels that don't expose message IDs (e.g. TUI).
    pub message_id: Option<String>,
    /// Conversation-level destination identifier for replies.
    ///
    /// Examples:
    /// - Telegram: chat ID
    /// - Discord: channel/thread ID
    /// - WhatsApp: phone number or conversation ID
    pub conversation_id: Option<String>,
    /// Optional routing hint for mapping to an existing session.
    pub session_hint: Option<String>,
    /// Callback URL for async response delivery (set by inbound webhook callers).
    pub callback_url: Option<String>,
    /// When true, queued for next heartbeat tick instead of immediate agent run.
    pub deferred: bool,
    /// Attachments received alongside the message (images, audio, etc.).
    pub attachments: Vec<InboundAttachment>,
    /// Platform-specific metadata (e.g. callback query info from Telegram).
    pub metadata: Option<serde_json::Value>,
}

/// Optional outbound routing metadata for channel replies.
///
/// This enables scoped delivery to the original conversation/sender instead of
/// global broadcast.
#[derive(Debug, Clone, Default)]
pub struct OutboundTarget {
    /// Conversation destination (chat/channel/thread/phone).
    pub recipient_id: Option<String>,
    /// Sender identity for policy / adapter filtering.
    pub sender_id: Option<String>,
    /// Optional per-platform thread/sub-channel ID.
    pub thread_id: Option<String>,
    /// Session identifier for correlation.
    pub session_id: Option<String>,
    /// Reply-to message identifier when supported (stringified).
    pub reply_to_message_id: Option<String>,
    /// Correlation identifier for request/response pairing (e.g. HTTP request ID).
    /// Distinct from `thread_id` which represents actual platform threads.
    pub request_id: Option<String>,
}

/// Abstraction over a communication channel (TUI, Telegram, Discord, WhatsApp, …).
///
/// Each adapter is responsible for:
/// - Sending agent events to end users in a channel-appropriate format.
/// - Proactive mid-task notifications.
/// - Bidirectional human-ask interactions (for channels that support it).
/// - Inbound message listening (for channels that receive external input).
///
/// Formatting is adapter-specific: Telegram uses MarkdownV2, TUI uses ANSI/ratatui rendering, etc.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Unique identifier for this channel instance (e.g. "tui", "telegram").
    fn id(&self) -> &str;

    /// Declare the capabilities this adapter supports.
    ///
    /// The router uses these flags to skip features that aren't available —
    /// for example, it won't call `start_listener` if `inbound` is false.
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }

    /// Channel-specific formatting instructions injected into the agent's system preamble.
    ///
    /// Return `Some(instructions)` to teach the agent how to format responses for
    /// this channel. Return `None` if no special formatting is required.
    fn format_instructions(&self) -> Option<String> {
        None
    }

    /// Validate configuration and establish any persistent connections.
    ///
    /// Called once at startup by the channel router before listeners
    /// are started. Use this to verify API tokens, test connectivity, etc.
    /// Errors are logged as warnings but do not abort startup.
    async fn on_start(&self) -> Result<()> {
        Ok(())
    }

    /// Gracefully release resources held by this channel.
    ///
    /// Called when the channel router is shutting down. Default no-op.
    async fn on_stop(&self) {}

    /// Apply a live configuration update without restarting the adapter.
    ///
    /// Called by the hot-reload task whenever the agent's TOML config file
    /// changes on disk. Implementations should update any mutable runtime
    /// state that can change without a full restart (e.g. `allowed_senders`,
    /// `default_chat_id`). Default no-op for adapters that have no live-
    /// mutable configuration.
    async fn on_config_update(&self, _cfg: &AdapterConfig) {}

    /// Send an agent event to this channel.
    ///
    /// Adapters may buffer certain event types (e.g. streaming tokens) and send
    /// them as a single message on `Done`, depending on platform constraints.
    ///
    /// Returns a [`MessageHandle`] that callers can use with [`Self::update_message`]
    /// to edit the sent message in-place. Adapters that do not support editing
    /// return [`MessageHandle::default()`].
    async fn send_event(
        &self,
        event: &ChannelEvent,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle>;

    /// Edit a previously sent message in-place using its handle.
    ///
    /// Called by the router's `update_message` when an existing message should
    /// be replaced with new content (e.g. replacing a "⚙️ running…" tool-call
    /// indicator with a "⚙️ done ✓" summary on `ToolResult`).
    ///
    /// Default no-op — only adapters where `capabilities().message_edit` is
    /// `true` need to implement this.
    async fn update_message(
        &self,
        _handle: &MessageHandle,
        _content: &str,
        _target: Option<&OutboundTarget>,
    ) -> Result<()> {
        Ok(())
    }

    /// Ask the human a question and wait for a text response.
    ///
    /// Used by the `human_ask` tool via the primary channel. Non-primary channels
    /// receive a broadcast notification that input is pending.
    async fn ask_human(
        &self,
        _message: &str,
        _timeout: Option<u64>,
        _target: Option<&OutboundTarget>,
    ) -> Result<String> {
        anyhow::bail!("ask_human not supported by this channel")
    }

    /// Start an inbound listener, feeding received messages into `tx`.
    ///
    /// Called once at startup by `ChannelRouter::start_listeners()`. Only called
    /// for adapters where `capabilities().inbound` is true. Default no-op for
    /// outbound-only adapters.
    async fn start_listener(&self, _tx: mpsc::UnboundedSender<InboundMessage>) -> Result<()> {
        Ok(())
    }

    /// Add an emoji reaction to a specific inbound message.
    ///
    /// `chat_id` is the conversation the message came from (required for
    /// platforms like Telegram where reactions are scoped to a chat).
    /// Used to acknowledge received messages (e.g. 👀) without sending a
    /// separate chat message. Default no-op for channels that don't support
    /// native reactions.
    async fn react(&self, _chat_id: &str, _message_id: &str, _emoji: &str) -> Result<()> {
        Ok(())
    }

    /// Send a structured rich message to the channel.
    ///
    /// Adapters that declare `capabilities().rich_messages == true` translate
    /// the [`OutboundMessage`] into their native API representation (e.g.
    /// Telegram `sendMessage` with `reply_markup`).
    async fn send_message(
        &self,
        _msg: OutboundMessage,
        _target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        Err(anyhow::anyhow!("rich messages not supported"))
    }

    /// Raw platform API passthrough.
    ///
    /// Adapters that declare `capabilities().native_api == true` forward the
    /// method name and JSON payload directly to the underlying platform API
    /// and return the raw response.
    async fn send_raw(&self, _method: &str, _payload: Value) -> Result<Value> {
        Err(anyhow::anyhow!("native API not supported"))
    }

    /// Register slash commands with the platform's native command menu.
    ///
    /// Called once at startup after skills are discovered. Default no-op — only
    /// called for adapters where `capabilities().command_menu` is true.
    async fn register_commands(&self, _commands: &[BotCommand]) -> Result<()> {
        Ok(())
    }
}

/// A slash command to register with the channel platform's command menu.
#[derive(Debug, Clone)]
pub struct BotCommand {
    /// Command name without the leading slash.
    /// Must be lowercase, alphanumeric + underscores. Length constraints are platform-specific.
    pub command: String,
    /// Short human-readable description shown in the command picker UI.
    pub description: String,
}

/// A shared, type-erased reference to any channel implementation.
pub type ChannelRef = Arc<dyn Channel>;
