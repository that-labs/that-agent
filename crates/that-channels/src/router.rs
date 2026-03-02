use std::sync::Arc;

use anyhow::Result;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

use crate::channel::{
    BotCommand, ChannelEvent, ChannelRef, InboundMessage, MessageHandle, OutboundTarget,
};
use crate::config::AdapterConfig;
use crate::message::OutboundMessage;

/// Routes agent events to multiple channels simultaneously (fan-out).
///
/// Outbound events are broadcast to all enabled channels concurrently.
/// Human-ask interactions are routed through the first channel that declares
/// `capabilities().ask_human == true`, with a "waiting for input" notification
/// broadcast to all other channels.
///
/// ## Construction
///
/// `ChannelRouter::new()` returns both the router and an `mpsc::UnboundedReceiver`
/// for inbound messages from all channel listeners. Wrap the router in `Arc`
/// before use, and pass the receiver to `InboundRouter::new()`.
///
/// ```ignore
/// let (router, inbound_rx) = ChannelRouter::new(channels, primary_idx);
/// let router = Arc::new(router);
/// router.initialize().await;
/// router.start_listeners().await?;
/// ```
pub struct ChannelRouter {
    channels: RwLock<Vec<ChannelRef>>,
    /// Preferred primary channel index for `human_ask` interactions.
    /// If that channel lacks `ask_human` capability, the router falls back
    /// to the first capable channel.
    primary_idx: usize,
    /// Sender for inbound messages collected from all channel listeners.
    inbound_tx: mpsc::UnboundedSender<InboundMessage>,
}

impl ChannelRouter {
    /// Create a new router with the given channels.
    ///
    /// Returns the router and the inbound message receiver. Pass `primary_idx`
    /// as the index of the preferred channel to use for `human_ask` interactions.
    pub fn new(
        channels: Vec<ChannelRef>,
        primary_idx: usize,
    ) -> (Self, mpsc::UnboundedReceiver<InboundMessage>) {
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        (
            Self {
                channels: RwLock::new(channels),
                primary_idx,
                inbound_tx,
            },
            inbound_rx,
        )
    }

    /// Number of active channels.
    pub async fn channel_count(&self) -> usize {
        self.channels.read().await.len()
    }

    /// Comma-separated IDs of all active channels.
    pub async fn channel_ids(&self) -> String {
        self.channels
            .read()
            .await
            .iter()
            .map(|c| c.id().to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// ID of the effective primary channel (the first ask_human-capable channel,
    /// defaulting to the first channel if none declare the capability).
    pub async fn primary_id(&self) -> String {
        let guard = self.channels.read().await;
        find_ask_human_in(&guard, self.primary_idx, None)
            .map(|c| c.id().to_string())
            .unwrap_or_else(|| {
                guard
                    .first()
                    .map(|c| c.id().to_string())
                    .unwrap_or_else(|| "none".into())
            })
    }

    /// Validate each channel's configuration and establish connections.
    ///
    /// Calls `on_start()` on every channel concurrently. Failures are logged as
    /// warnings but do not abort startup — a misconfigured adapter should not
    /// prevent other channels from working.
    pub async fn initialize(&self) {
        let channels: Vec<_> = self.channels.read().await.clone();
        let futs: Vec<_> = channels
            .iter()
            .map(|ch| {
                let ch = Arc::clone(ch);
                async move {
                    match ch.on_start().await {
                        Ok(()) => info!(channel = %ch.id(), "Channel initialized"),
                        Err(e) => warn!(channel = %ch.id(), "Channel startup failed: {e:#}"),
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await;
    }

    /// Broadcast a `ChannelEvent` to all channels concurrently.
    ///
    /// Errors from individual channels are logged but do not stop delivery to others.
    /// Message handles returned by adapters are discarded — use [`Self::send_to`]
    /// when you need the handle.
    pub async fn broadcast(&self, event: &ChannelEvent) {
        let channels: Vec<_> = self.channels.read().await.clone();
        let futs: Vec<_> = channels
            .iter()
            .map(|ch| {
                let ch = Arc::clone(ch);
                let event = event.clone();
                async move {
                    if let Err(e) = ch.send_event(&event, None).await {
                        error!(channel = %ch.id(), "Failed to send event: {e:#}");
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await;
    }

    /// Send an event to a single channel with optional routing metadata.
    ///
    /// Returns the [`MessageHandle`] from the adapter, which can be used with
    /// [`Self::update_message`] to edit the sent message in-place.
    pub async fn send_to(
        &self,
        channel_id: &str,
        event: &ChannelEvent,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        let guard = self.channels.read().await;
        if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
            return ch.send_event(event, target).await;
        }
        Ok(MessageHandle::default())
    }

    /// Broadcast a notification to all channels concurrently.
    ///
    /// Used by the `channel_notify` tool for mid-task progress updates.
    pub async fn notify_all(&self, message: &str) {
        let channels: Vec<_> = self.channels.read().await.clone();
        let futs: Vec<_> = channels
            .iter()
            .map(|ch| {
                let ch = Arc::clone(ch);
                let event = ChannelEvent::Notify(message.to_string());
                async move {
                    if let Err(e) = ch.send_event(&event, None).await {
                        error!(channel = %ch.id(), "Failed to send notification: {e:#}");
                    }
                }
            })
            .collect();
        futures::future::join_all(futs).await;
    }

    /// Send a notification to one channel with optional routing metadata.
    pub async fn notify_channel(
        &self,
        channel_id: &str,
        message: &str,
        target: Option<&OutboundTarget>,
    ) {
        let guard = self.channels.read().await;
        if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
            let event = ChannelEvent::Notify(message.to_string());
            if let Err(e) = ch.send_event(&event, target).await {
                error!(channel = %ch.id(), "Failed to send notification: {e:#}");
            }
        }
    }

    /// Ask a question via the primary ask_human-capable channel and return the response.
    ///
    /// Selects the first channel where `capabilities().ask_human` is true,
    /// falling back to the configured `primary_idx` if none declare the capability.
    /// A "waiting for input" notification is sent to all non-primary channels.
    pub async fn ask_human_primary(
        &self,
        message: &str,
        timeout: Option<u64>,
        preferred_channel_id: Option<&str>,
        target: Option<&OutboundTarget>,
    ) -> Result<String> {
        let guard = self.channels.read().await;
        let primary = find_ask_human_in(&guard, self.primary_idx, preferred_channel_id)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No channel supports ask_human. Configure a Telegram or TUI channel \
                 as the primary channel (channels.primary) to handle human_ask interactions."
                )
            })?;

        // Notify other channels that we are waiting for input on the primary.
        for ch in guard.iter() {
            if ch.id() != primary.id() {
                let notif = format!(
                    "Waiting for user input on `{}` channel: {}",
                    primary.id(),
                    message
                );
                let event = ChannelEvent::Notify(notif);
                if let Err(e) = ch.send_event(&event, None).await {
                    warn!(channel = %ch.id(), "Failed to broadcast pending human-ask: {e:#}");
                }
            }
        }
        drop(guard);

        primary.ask_human(message, timeout, target).await
    }

    /// Combined formatting instructions from all active channels.
    ///
    /// Each channel's instructions are joined and appended to the agent's
    /// system preamble so the agent knows how to format messages per channel.
    pub async fn combined_format_instructions(&self) -> String {
        let guard = self.channels.read().await;
        let mut out = String::new();
        for ch in guard.iter() {
            if let Some(instructions) = ch.format_instructions() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&instructions);
            }
        }
        out
    }

    /// Formatting instructions for one specific channel id.
    ///
    /// Returns an empty string when the channel has no formatting instructions
    /// or when the channel id is unknown.
    pub async fn format_instructions_for(&self, channel_id: &str) -> String {
        let guard = self.channels.read().await;
        guard
            .iter()
            .find(|c| c.id() == channel_id)
            .and_then(|c| c.format_instructions())
            .unwrap_or_default()
    }

    /// Start inbound listeners for all channels that declare `capabilities().inbound`.
    ///
    /// Each listener runs in its own Tokio task, feeding received messages into
    /// a shared inbound channel. Call once at startup, after `initialize()`.
    pub async fn start_listeners(&self) -> Result<()> {
        let guard = self.channels.read().await;
        for ch in guard.iter() {
            if !ch.capabilities().inbound {
                continue; // skip adapters that don't support inbound
            }
            let tx = self.inbound_tx.clone();
            let ch = Arc::clone(ch);
            tokio::spawn(async move {
                if let Err(e) = ch.start_listener(tx).await {
                    error!(channel = %ch.id(), "Inbound listener error: {e:#}");
                }
            });
        }
        Ok(())
    }

    /// Register slash commands with all channels that declare `capabilities().command_menu`.
    ///
    /// Called once after skill discovery. Channels without command menu support
    /// are silently skipped.
    pub async fn register_commands(&self, commands: &[BotCommand]) {
        let guard = self.channels.read().await;
        for ch in guard.iter() {
            if !ch.capabilities().command_menu {
                continue;
            }
            if let Err(e) = ch.register_commands(commands).await {
                warn!(channel = %ch.id(), "Failed to register commands: {e:#}");
            }
        }
    }

    /// Push a live configuration update to all matching channel adapters.
    ///
    /// Called by the hot-reload task whenever the agent's TOML config changes.
    /// Each adapter config is matched to a channel by ID; the channel's
    /// `on_config_update` is called so it can apply runtime-mutable changes
    /// (e.g. `allowed_senders`) without a full restart.
    pub async fn apply_config_updates(&self, adapter_configs: &[AdapterConfig]) {
        let guard = self.channels.read().await;
        for cfg in adapter_configs {
            let id = cfg
                .id
                .as_deref()
                .unwrap_or_else(|| cfg.adapter_type.as_str());
            if let Some(ch) = guard.iter().find(|c| c.id() == id) {
                ch.on_config_update(cfg).await;
            }
        }
    }

    /// Add an emoji reaction to a specific message on a specific channel.
    ///
    /// Finds the channel by ID and calls its `react()` method. If the channel
    /// is not found or does not support reactions, the call is silently ignored.
    pub async fn react_to_message(
        &self,
        channel_id: &str,
        chat_id: &str,
        message_id: i64,
        emoji: &str,
    ) {
        let guard = self.channels.read().await;
        if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
            if let Err(e) = ch.react(chat_id, message_id, emoji).await {
                warn!(channel = %channel_id, "react_to_message failed: {e:#}");
            }
        }
    }

    /// Edit a previously sent message using its handle.
    ///
    /// Routes to the adapter identified by `handle.channel_id`. Returns `Ok(())`
    /// if the channel is not found or the handle has no `message_id`.
    pub async fn update_message(&self, handle: &MessageHandle, content: &str) -> Result<()> {
        if let Some(channel_id) = handle.channel_id.as_deref() {
            let guard = self.channels.read().await;
            if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
                return ch.update_message(handle, content, None).await;
            }
        }
        Ok(())
    }

    /// Send a structured rich message to a specific channel.
    ///
    /// Routes the [`OutboundMessage`] to the adapter identified by `channel_id`.
    /// Returns an error if the channel is not found or does not support rich messages.
    pub async fn send_message(
        &self,
        channel_id: &str,
        msg: OutboundMessage,
        target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        let guard = self.channels.read().await;
        if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
            return ch.send_message(msg, target).await;
        }
        Err(anyhow::anyhow!("channel '{channel_id}' not found"))
    }

    /// Raw platform API passthrough to a specific channel.
    ///
    /// Routes the method + payload to the adapter identified by `channel_id`.
    /// Returns an error if the channel is not found or does not support native API.
    pub async fn send_raw(
        &self,
        channel_id: &str,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let guard = self.channels.read().await;
        if let Some(ch) = guard.iter().find(|c| c.id() == channel_id) {
            return ch.send_raw(method, payload).await;
        }
        Err(anyhow::anyhow!("channel '{channel_id}' not found"))
    }

    /// Runtime environment variable pairs to expose to the agent.
    ///
    /// These inform the agent about which channels are active:
    /// - `THAT_CHANNEL_IDS` — comma-separated list of active channel IDs
    /// - `THAT_CHANNEL_PRIMARY` — ID of the primary (human-ask) channel
    pub async fn env_vars(&self) -> Vec<(String, String)> {
        vec![
            ("THAT_CHANNEL_IDS".into(), self.channel_ids().await),
            ("THAT_CHANNEL_PRIMARY".into(), self.primary_id().await),
        ]
    }

    /// Dynamically add a channel at runtime.
    ///
    /// Calls `on_start()`, spawns an inbound listener if capable, then appends
    /// to the channel list.
    pub async fn add_channel(&self, ch: ChannelRef) {
        ch.on_start().await.ok();
        if ch.capabilities().inbound {
            let tx = self.inbound_tx.clone();
            let ch2 = Arc::clone(&ch);
            tokio::spawn(async move {
                if let Err(e) = ch2.start_listener(tx).await {
                    error!(channel = %ch2.id(), "listener error: {e:#}");
                }
            });
        }
        self.channels.write().await.push(ch);
    }

    /// Dynamically remove a channel at runtime by id.
    ///
    /// Calls `on_stop()` on the removed channel for graceful cleanup.
    pub async fn remove_channel(&self, id: &str) {
        let mut guard = self.channels.write().await;
        if let Some(pos) = guard.iter().position(|c| c.id() == id) {
            let ch = guard.remove(pos);
            drop(guard);
            ch.on_stop().await;
        }
    }
}

/// Shared helper: find the first ask_human-capable channel in a slice.
fn find_ask_human_in<'a>(
    channels: &'a [ChannelRef],
    primary_idx: usize,
    preferred_channel_id: Option<&str>,
) -> Option<&'a ChannelRef> {
    // Try an explicit preferred channel first (used for scoped inbound routing).
    if let Some(preferred) = preferred_channel_id {
        if let Some(ch) = channels.iter().find(|ch| ch.id() == preferred) {
            if ch.capabilities().ask_human {
                return Some(ch);
            }
        }
    }
    // Check the configured primary first.
    if let Some(ch) = channels.get(primary_idx) {
        if ch.capabilities().ask_human {
            return Some(ch);
        }
    }
    // Fall back to the first capable channel.
    channels.iter().find(|ch| ch.capabilities().ask_human)
}
