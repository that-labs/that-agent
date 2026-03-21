use std::io::{self, BufRead, Write};
use std::sync::Arc;

use crate::agent_loop::hook::{HookAction, LoopHook};

/// Hook for interactive streaming mode — prints tokens live, handles `human_ask` via stdin.
pub struct AgentHook {
    pub debug: bool,
}

#[async_trait::async_trait]
impl LoopHook for AgentHook {
    async fn on_text_delta(&self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
    }

    async fn on_reasoning_delta(&self, delta: &str) {
        if !delta.is_empty() {
            eprint!("\x1b[2m{delta}\x1b[0m");
            let _ = io::stderr().flush();
        }
    }

    async fn on_tool_call(&self, name: &str, _call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let message = serde_json::from_str::<serde_json::Value>(args_json)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(String::from))
                .unwrap_or_else(|| "Agent is asking for input:".into());
            eprint!("\n\x1b[1;33m[human_ask]\x1b[0m {message}\n> ");
            let _ = io::stdout().flush();
            let mut response = String::new();
            let _ = io::stdin().lock().read_line(&mut response);
            let response = response.trim().to_string();
            let approved = {
                let lower = response.to_lowercase();
                lower != "no" && lower != "n" && lower != "deny"
            };
            let result = serde_json::json!({
                "response": response,
                "approved": approved,
                "method": "hook",
                "elapsed_ms": 0
            });
            return HookAction::Skip {
                result_json: result.to_string(),
            };
        }
        if self.debug {
            eprintln!("\x1b[36m[tool call] {name} {args_json}\x1b[0m");
        }
        HookAction::Continue
    }

    async fn on_tool_result(&self, name: &str, _call_id: &str, result_json: &str) {
        if self.debug {
            let truncated: String = result_json.chars().take(500).collect();
            let suffix = if result_json.chars().count() > 500 {
                "..."
            } else {
                ""
            };
            eprintln!("\x1b[33m[tool result] {name}: {truncated}{suffix}\x1b[0m");
        }
    }
}

/// Hook for eval mode — auto-denies `human_ask` and collects tool events for judge transcripts.
pub struct EvalHook {
    tool_events: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Default for EvalHook {
    fn default() -> Self {
        Self::new()
    }
}

impl EvalHook {
    pub fn new() -> Self {
        Self {
            tool_events: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// Drain and return collected tool event strings.
    pub fn take_events(&self) -> Vec<String> {
        self.tool_events.lock().unwrap().drain(..).collect()
    }
}

#[async_trait::async_trait]
impl LoopHook for EvalHook {
    async fn on_text_delta(&self, delta: &str) {
        print!("{delta}");
        let _ = io::stdout().flush();
    }

    async fn on_reasoning_delta(&self, _delta: &str) {}

    async fn on_tool_call(&self, name: &str, _call_id: &str, args_json: &str) -> HookAction {
        if name == "human_ask" {
            let result = serde_json::json!({
                "response": "eval-mode: no human available",
                "approved": false,
                "method": "eval_hook"
            });
            return HookAction::Skip {
                result_json: result.to_string(),
            };
        }
        let args: String = args_json.chars().take(300).collect();
        let suffix = if args_json.chars().count() > 300 {
            "…"
        } else {
            ""
        };
        self.tool_events
            .lock()
            .unwrap()
            .push(format!("CALL {name} {args}{suffix}"));
        HookAction::Continue
    }

    async fn on_tool_result(&self, name: &str, _call_id: &str, result_json: &str) {
        let truncated: String = result_json.chars().take(400).collect();
        let suffix = if result_json.chars().count() > 400 {
            "…"
        } else {
            ""
        };
        self.tool_events
            .lock()
            .unwrap()
            .push(format!("RESULT {name} {truncated}{suffix}"));
    }
}
