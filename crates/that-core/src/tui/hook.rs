use crate::agent_loop::hook::{HookAction, LoopHook};
use tokio::sync::{mpsc, oneshot};

use super::TuiEvent;

#[derive(Clone)]
pub struct TuiHook {
    tx: mpsc::UnboundedSender<TuiEvent>,
}

impl TuiHook {
    pub fn new(tx: mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl LoopHook for TuiHook {
    async fn on_text_delta(&self, delta: &str) {
        let _ = self.tx.send(TuiEvent::Token(delta.to_string()));
    }

    async fn on_reasoning_delta(&self, delta: &str) {
        let _ = self.tx.send(TuiEvent::ThinkingDelta(delta.to_string()));
    }

    async fn on_tool_call(&self, name: &str, call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let message = serde_json::from_str::<serde_json::Value>(args_json)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(String::from))
                .unwrap_or_else(|| "Agent is asking for input:".into());

            let (response_tx, response_rx) = oneshot::channel();
            let _ = self.tx.send(TuiEvent::HumanAsk {
                message,
                response_tx,
            });

            let result_json = match response_rx.await {
                Ok(response) => {
                    let approved = {
                        let lower = response.to_lowercase();
                        lower != "no" && lower != "n" && lower != "deny"
                    };
                    serde_json::json!({
                        "response": response,
                        "approved": approved,
                        "method": "tui_hook",
                        "elapsed_ms": 0
                    })
                    .to_string()
                }
                Err(_) => serde_json::json!({
                    "response": "User quit",
                    "approved": false,
                    "method": "tui_hook",
                    "elapsed_ms": 0
                })
                .to_string(),
            };
            HookAction::Skip { result_json }
        } else {
            let _ = self.tx.send(TuiEvent::ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                args: args_json.to_string(),
            });
            HookAction::Continue
        }
    }

    async fn on_tool_result(&self, name: &str, call_id: &str, result_json: &str) {
        let _ = self.tx.send(TuiEvent::ToolResult {
            call_id: call_id.to_string(),
            name: name.to_string(),
            result: result_json.to_string(),
        });
    }

    async fn on_steering_picked_up(&self) {
        let _ = self.tx.send(TuiEvent::SteeringPickedUp);
    }
}
