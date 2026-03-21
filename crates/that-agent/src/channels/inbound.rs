use tokio::sync::mpsc;

use crate::channels::channel::InboundMessage;

/// Routes inbound messages from external channels to agent sessions.
///
/// `InboundRouter` is a thin adapter over the mpsc receiver that the
/// `ChannelRouter` produces. The actual session matching and creation logic
/// lives in the caller (e.g. `that-core` or `that-agent`) where the
/// `SessionManager` is available.
///
/// ## Usage
///
/// ```ignore
/// let (router, inbound_rx) = ChannelRouter::new(channels, primary_idx);
/// let router = Arc::new(router);
/// router.start_listeners().await?;
///
/// let inbound_router = InboundRouter::new(inbound_rx);
/// inbound_router.run(|msg| async move {
///     // match msg.channel_id + msg.sender_id → session
///     // feed msg.text into execute_agent_run_channel(...)
/// }).await;
/// ```
pub struct InboundRouter {
    rx: mpsc::UnboundedReceiver<InboundMessage>,
}

impl InboundRouter {
    /// Create a new inbound router from the channel's message receiver.
    pub fn new(rx: mpsc::UnboundedReceiver<InboundMessage>) -> Self {
        Self { rx }
    }

    /// Run the router, dispatching each inbound message to the provided handler.
    ///
    /// The handler receives the full [`InboundMessage`] and is responsible for:
    /// 1. Looking up or creating an agent session for `(channel_id, sender_id)`.
    /// 2. Feeding `msg.text` as the next task into `execute_agent_run_channel()`.
    ///
    /// Runs until the channel closes (i.e. all senders are dropped).
    pub async fn run<F, Fut>(mut self, handler: F)
    where
        F: Fn(InboundMessage) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        while let Some(msg) = self.rx.recv().await {
            handler(msg).await;
        }
    }

    /// Run the router, spawning a Tokio task for each inbound message.
    ///
    /// Unlike `run()`, this does not serialize message handling — each message
    /// is dispatched concurrently. Use when message ordering per-session is
    /// managed by the handler itself (e.g. a per-session queue).
    pub async fn run_concurrent<F, Fut>(mut self, handler: F)
    where
        F: Fn(InboundMessage) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        use std::sync::Arc;
        let handler = Arc::new(handler);
        while let Some(msg) = self.rx.recv().await {
            let h = Arc::clone(&handler);
            tokio::spawn(async move { h(msg).await });
        }
    }
}
