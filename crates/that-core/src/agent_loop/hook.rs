//! `LoopHook` — callback trait for the agentic loop.
//!
//! Implementations intercept text deltas, tool calls, and tool results.
//! The hook can short-circuit a tool call by returning `HookAction::Skip`
//! with a synthetic result JSON string.

/// What the loop should do after `on_tool_call` returns.
#[derive(Debug, Clone)]
pub enum HookAction {
    /// Let the loop execute the tool normally.
    Continue,
    /// Skip tool execution and inject this JSON string as the result instead.
    Skip { result_json: String },
}

/// Callback interface invoked at each observable event in the agentic loop.
#[async_trait::async_trait]
pub trait LoopHook: Send + Sync {
    /// Called on each text token delta streaming from the model.
    async fn on_text_delta(&self, delta: &str);

    /// Called on each reasoning/thinking token delta (Anthropic extended thinking).
    async fn on_reasoning_delta(&self, delta: &str);

    /// Called when the model requests a tool call, before the tool executes.
    ///
    /// Return `HookAction::Skip { result_json }` to intercept and inject a
    /// synthetic result (e.g. `human_ask` routed via a channel).
    /// Return `HookAction::Continue` to let the loop execute the tool.
    async fn on_tool_call(&self, name: &str, call_id: &str, args_json: &str) -> HookAction;

    /// Called after a tool has returned its result.
    async fn on_tool_result(&self, name: &str, call_id: &str, result_json: &str);

    /// Called when the loop drains queued steering hints (default: no-op).
    async fn on_steering_picked_up(&self) {}
}

/// A no-op hook used as a default / in tests.
pub struct NoopHook;

#[async_trait::async_trait]
impl LoopHook for NoopHook {
    async fn on_text_delta(&self, _delta: &str) {}
    async fn on_reasoning_delta(&self, _delta: &str) {}
    async fn on_tool_call(&self, _name: &str, _call_id: &str, _args_json: &str) -> HookAction {
        HookAction::Continue
    }
    async fn on_tool_result(&self, _name: &str, _call_id: &str, _result_json: &str) {}
}
