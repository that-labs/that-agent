use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::channels::channel::OutboundTarget;
use crate::channels::router::ChannelRouter;

/// Error type for the `channel_notify` tool.
#[derive(Debug, thiserror::Error, Serialize)]
#[error("{0}")]
pub struct ChannelToolError(pub String);

impl From<anyhow::Error> for ChannelToolError {
    fn from(e: anyhow::Error) -> Self {
        Self(format!("{e:#}"))
    }
}

/// Arguments for the `channel_notify` tool.
#[derive(Debug, Deserialize)]
pub struct ChannelNotifyArgs {
    /// The message to send to the human operator.
    pub message: String,
}

/// Output of the `channel_notify` tool.
#[derive(Debug, Serialize)]
pub struct ChannelNotifyOutput {
    pub sent: bool,
}

/// A built-in agent tool for sending proactive mid-task notifications.
///
/// Broadcasts a message to all active channels via the `ChannelRouter`.
/// Useful for long-running tasks where the agent wants to inform the human
/// of progress without pausing to wait for a response (use `human_ask` for that).
///
/// Always registered alongside `human_ask` in the agent's tool set.
#[derive(Clone)]
pub struct ChannelNotifyTool {
    router: Arc<ChannelRouter>,
    channel_id: Option<String>,
    target: Option<OutboundTarget>,
}

impl ChannelNotifyTool {
    pub fn new(router: Arc<ChannelRouter>) -> Self {
        Self {
            router,
            channel_id: None,
            target: None,
        }
    }

    /// Create a scoped notify tool that sends to one channel/target.
    pub fn scoped(
        router: Arc<ChannelRouter>,
        channel_id: impl Into<String>,
        target: Option<OutboundTarget>,
    ) -> Self {
        Self {
            router,
            channel_id: Some(channel_id.into()),
            target,
        }
    }
}

impl ChannelNotifyTool {
    /// Execute the channel_notify logic directly (called by `that-core::hooks::ChannelHook`
    /// via `HookAction::Skip` — never dispatched through `typed::dispatch`).
    pub async fn call(
        &self,
        args: ChannelNotifyArgs,
    ) -> Result<ChannelNotifyOutput, ChannelToolError> {
        if let Some(channel_id) = self.channel_id.as_deref() {
            self.router
                .notify_channel(channel_id, &args.message, self.target.as_ref())
                .await;
        } else {
            self.router.notify_all(&args.message).await;
        }
        Ok(ChannelNotifyOutput { sent: true })
    }
}
