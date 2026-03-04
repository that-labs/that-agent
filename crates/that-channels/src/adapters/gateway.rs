use anyhow::Result;
use async_trait::async_trait;

use super::validate_callback_url;
use crate::channel::{Channel, ChannelCapabilities, ChannelEvent, MessageHandle, OutboundTarget};
use crate::registry::ChannelEntry;

/// Outbound-only channel adapter that POSTs the final agent response to a
/// registered gateway callback URL.
pub struct GatewayChannelAdapter {
    entry: ChannelEntry,
    client: reqwest::Client,
}

impl GatewayChannelAdapter {
    pub fn new(entry: ChannelEntry) -> Self {
        Self {
            entry,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Channel for GatewayChannelAdapter {
    fn id(&self) -> &str {
        &self.entry.id
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            inbound: false,
            ask_human: false,
            ..ChannelCapabilities::default()
        }
    }

    async fn send_event(
        &self,
        event: &ChannelEvent,
        _target: Option<&OutboundTarget>,
    ) -> Result<MessageHandle> {
        // SSRF protection: validate callback URL before every outbound POST.
        validate_callback_url(&self.entry.callback_url).map_err(|e| anyhow::anyhow!("{e}"))?;

        match event {
            ChannelEvent::Done {
                text,
                input_tokens,
                output_tokens,
                ..
            } => {
                let body = serde_json::json!({
                    "event": "done",
                    "text": text,
                    "tokens": input_tokens + output_tokens,
                });
                let _ = self
                    .client
                    .post(&self.entry.callback_url)
                    .json(&body)
                    .send()
                    .await;
            }
            ChannelEvent::Error(msg) => {
                let body = serde_json::json!({ "event": "error", "text": msg });
                let _ = self
                    .client
                    .post(&self.entry.callback_url)
                    .json(&body)
                    .send()
                    .await;
            }
            _ => {} // drop streaming tokens
        }
        Ok(MessageHandle::default())
    }
}
