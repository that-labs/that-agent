use tokio::sync::{mpsc, oneshot};

use super::TuiEvent;

/// A [`that_channels::Channel`] implementation backed by the TUI's mpsc event channel.
///
/// Wraps `mpsc::UnboundedSender<TuiEvent>` and maps [`that_channels::ChannelEvent`]
/// to the appropriate [`TuiEvent`] variants. Lives in `that-core` (not in
/// `that-channels`) to avoid a circular dependency between the two crates.
pub struct TuiChannel {
    id: String,
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl TuiChannel {
    /// Create a new TUI channel backed by the given sender.
    pub fn new(id: impl Into<String>, tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { id: id.into(), tx }
    }
}

#[async_trait::async_trait]
impl that_channels::Channel for TuiChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> that_channels::ChannelCapabilities {
        that_channels::ChannelCapabilities {
            ask_human: true,
            ..that_channels::ChannelCapabilities::default()
        }
    }

    async fn send_event(
        &self,
        event: &that_channels::ChannelEvent,
        _target: Option<&that_channels::OutboundTarget>,
    ) -> anyhow::Result<that_channels::MessageHandle> {
        use that_channels::ChannelEvent;
        let _ = match event {
            ChannelEvent::StreamToken(t) => self.tx.send(TuiEvent::Token(t.clone())),
            ChannelEvent::ThinkingDelta(t) => self.tx.send(TuiEvent::ThinkingDelta(t.clone())),
            ChannelEvent::ToolCall {
                call_id,
                name,
                args,
            } => self.tx.send(TuiEvent::ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                args: args.clone(),
            }),
            ChannelEvent::ToolResult {
                call_id,
                name,
                result,
            } => self.tx.send(TuiEvent::ToolResult {
                call_id: call_id.clone(),
                name: name.clone(),
                result: result.clone(),
            }),
            ChannelEvent::Done {
                text,
                input_tokens,
                output_tokens,
                cached_input_tokens,
                cache_write_tokens,
            } => self.tx.send(TuiEvent::Done {
                text: text.clone(),
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                cached_input_tokens: *cached_input_tokens,
                cache_write_tokens: *cache_write_tokens,
            }),
            ChannelEvent::Error(e) => self.tx.send(TuiEvent::Error(e.clone())),
            ChannelEvent::Retrying {
                attempt,
                max_attempts,
                delay_secs,
            } => self.tx.send(TuiEvent::Retrying {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_secs: *delay_secs,
            }),
            ChannelEvent::Notify(msg) => self.tx.send(TuiEvent::Token(format!("\n📢 {msg}\n"))),
            ChannelEvent::Attachment {
                filename,
                data,
                caption,
                ..
            } => {
                let size_kb = data.len() as f64 / 1024.0;
                let line = if let Some(cap) = caption.as_deref().filter(|s| !s.is_empty()) {
                    format!("\n📎 {filename} ({size_kb:.1} KB) — {cap}\n")
                } else {
                    format!("\n📎 {filename} ({size_kb:.1} KB)\n")
                };
                self.tx.send(TuiEvent::Token(line))
            }
            // TUI has its own visual rendering — typing indicators and run resets are not needed.
            ChannelEvent::TypingIndicator | ChannelEvent::Reset => {
                return Ok(that_channels::MessageHandle::default())
            }
        };
        Ok(that_channels::MessageHandle::default())
    }

    async fn ask_human(
        &self,
        message: &str,
        _timeout: Option<u64>,
        _target: Option<&that_channels::OutboundTarget>,
    ) -> anyhow::Result<String> {
        let (response_tx, response_rx) = oneshot::channel();
        let _ = self.tx.send(TuiEvent::HumanAsk {
            message: message.to_string(),
            response_tx,
        });
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("TUI ask_human: response channel closed"))
    }

    async fn start_listener(
        &self,
        _tx: tokio::sync::mpsc::UnboundedSender<that_channels::InboundMessage>,
    ) -> anyhow::Result<()> {
        // TUI handles input via its own crossterm event loop.
        // No external inbound listener is needed.
        Ok(())
    }
}
