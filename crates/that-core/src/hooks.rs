//! Channel hook — routes agent loop events to active [`ChannelRouter`] channels.
//!
//! `ChannelHook` lives here (not in `that-channels`) to avoid a circular
//! dependency: `that-core` depends on `that-channels`, so `that-channels`
//! cannot implement `crate::agent_loop::LoopHook`.

use std::sync::Arc;

use serde_json::Value as JsonValue;
use that_channels::channel::{ChannelEvent, OutboundTarget};
use that_channels::hook::ToolLogEvent;
use that_channels::router::ChannelRouter;
use tokio::sync::mpsc;
use tracing::debug;

use crate::agent_loop::hook::{HookAction, LoopHook};
use crate::agent_loop::types::ToolDef;

/// Extract a compact single-line preview of the most meaningful tool argument.
///
/// Tries tool-specific primary keys first (e.g. `cmd` for shell_exec, `path`
/// for file tools), then falls back to the first string-valued key, then to
/// truncated raw JSON. Output is stripped of control characters.
fn compact_args_preview(tool: &str, args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<JsonValue>(args_json) else {
        return args_json
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect();
    };
    let primary_keys: &[&str] = match tool {
        "shell_exec" => &["cmd", "command"],
        "code_edit" | "code_read" | "fs_write" | "fs_cat" | "fs_ls" | "fs_rm" | "fs_mkdir" => {
            &["path"]
        }
        "web_search" | "search" => &["query"],
        "fetch" => &["url"],
        "git_commit" => &["message"],
        _ => &["path", "cmd", "query", "url", "message"],
    };
    for key in primary_keys {
        if let Some(val) = v.get(key).and_then(|v| v.as_str()) {
            let truncated: String = val.chars().filter(|c| !c.is_control()).take(80).collect();
            let ellipsis = if val.chars().count() > 80 { "…" } else { "" };
            return format!("{key}={truncated}{ellipsis}");
        }
    }
    if let Some(obj) = v.as_object() {
        for (key, val) in obj {
            if let Some(s) = val.as_str() {
                let truncated: String = s.chars().filter(|c| !c.is_control()).take(80).collect();
                return format!("{key}={truncated}");
            }
        }
    }
    v.to_string().chars().take(80).collect()
}

fn tool_result_is_error(result_json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(result_json) else {
        return false;
    };
    if value.get("error").is_some() {
        return true;
    }
    if value.get("timed_out").and_then(|v| v.as_bool()) == Some(true) {
        return true;
    }
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return true;
    }
    if let Some(code) = value.get("exit_code").and_then(|v| v.as_i64()) {
        return code != 0;
    }
    false
}

/// Return the `channel_send_file` tool schema.
///
/// Include this in the tool list alongside [`channel_notify_tool_def`] when a
/// [`ChannelRouter`] is active. [`ChannelHook`] intercepts calls and reads the
/// file from disk before routing an [`that_channels::channel::ChannelEvent::Attachment`]
/// to the router — `dispatch()` is never reached.
pub fn channel_send_file_tool_def() -> ToolDef {
    ToolDef {
        name: "channel_send_file".into(),
        description: "Send a file from the local filesystem as an attachment to the human \
            operator via the active channel. Supported by channels that have native file \
            delivery (e.g. Telegram). Other channels receive a plain-text notification \
            with the filename and size instead. Use this to share generated reports, \
            exports, images, or any other file output mid-task without waiting for a reply."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or workspace-relative path to the file to send."
                },
                "caption": {
                    "type": "string",
                    "description": "Optional short description shown alongside the file."
                },
                "channel": {
                    "type": "string",
                    "description": "Optional channel ID to target. Omit to broadcast to all channels."
                }
            },
            "required": ["path"]
        }),
    }
}

/// Return the `channel_notify` tool schema.
///
/// Include this in the tool list when the agent is running in channel mode
/// (i.e. when a `ChannelRouter` is available). The [`ChannelHook`] intercepts
/// calls to this tool and routes them to the router — dispatch() is never reached.
pub fn channel_notify_tool_def() -> ToolDef {
    ToolDef {
        name: "channel_notify".into(),
        description: "Send a proactive notification to the human operator without waiting \
            for a response. Use this to report intermediate progress, discoveries, or \
            status updates during long-running tasks. Do not use this as a substitute \
            for human_ask when you actually need the human to respond."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The notification message to send. Keep it concise \
                        and informative. Format it according to the active channel's \
                        formatting conventions."
                }
            },
            "required": ["message"]
        }),
    }
}

/// A [`LoopHook`] that routes agent events to the active [`ChannelRouter`].
///
/// When `channel_id` is set, events are delivered only to that channel with
/// optional `target` metadata. Otherwise, events are broadcast to all channels.
///
/// When `suppress_streaming` is true, text deltas are not routed to any channel.
/// Use this for background/internal runs (e.g. heartbeat) where only deliberate
/// `channel_notify` tool calls should produce outbound messages.
#[derive(Clone)]
pub struct ChannelHook {
    router: Arc<ChannelRouter>,
    channel_id: Option<String>,
    target: Option<OutboundTarget>,
    /// Optional sink for tool call/result events for session transcript logging.
    log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
    suppress_streaming: bool,
}

impl ChannelHook {
    pub fn new(
        router: Arc<ChannelRouter>,
        log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
    ) -> Self {
        Self {
            router,
            channel_id: None,
            target: None,
            log_tx,
            suppress_streaming: false,
        }
    }

    /// Create a hook scoped to one channel and optional target metadata.
    pub fn scoped(
        router: Arc<ChannelRouter>,
        channel_id: impl Into<String>,
        target: Option<OutboundTarget>,
        log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
    ) -> Self {
        Self {
            router,
            channel_id: Some(channel_id.into()),
            target,
            log_tx,
            suppress_streaming: false,
        }
    }

    /// Create a hook that suppresses all streaming output to channels.
    ///
    /// Used for internal/background runs (heartbeat, scheduled tasks) where the
    /// agent should only communicate via the `channel_notify` tool.
    pub fn silent(
        router: Arc<ChannelRouter>,
        log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
    ) -> Self {
        Self {
            router,
            channel_id: None,
            target: None,
            log_tx,
            suppress_streaming: true,
        }
    }

    /// Returns true if streaming output is suppressed for this hook.
    pub fn is_silent(&self) -> bool {
        self.suppress_streaming
    }
}

#[async_trait::async_trait]
impl LoopHook for ChannelHook {
    async fn on_text_delta(&self, delta: &str) {
        if self.suppress_streaming {
            return;
        }
        let event = ChannelEvent::StreamToken(delta.to_string());
        if let Some(cid) = self.channel_id.as_deref() {
            let _ = self.router.send_to(cid, &event, self.target.as_ref()).await;
        } else {
            self.router.broadcast(&event).await;
        }
    }

    async fn on_reasoning_delta(&self, _delta: &str) {
        // Reasoning tokens are not routed to channels.
    }

    async fn on_tool_call(&self, name: &str, call_id: &str, args_json: &str) -> HookAction {
        debug!(channel = ?self.channel_id, tool = %name, " → {name}");

        // Log to session log sink (unfiltered).
        if let Some(tx) = &self.log_tx {
            let _ = tx.send(ToolLogEvent::Call {
                name: name.to_string(),
                args: args_json.to_string(),
            });
        }

        match name {
            "human_ask" => {
                let message = serde_json::from_str::<serde_json::Value>(args_json)
                    .ok()
                    .and_then(|v| v.get("message")?.as_str().map(String::from))
                    .unwrap_or_else(|| "The agent is asking for input.".into());

                let timeout = serde_json::from_str::<serde_json::Value>(args_json)
                    .ok()
                    .and_then(|v| v.get("timeout")?.as_u64());

                let result_json = match self
                    .router
                    .ask_human_primary(
                        &message,
                        timeout,
                        self.channel_id.as_deref(),
                        self.target.as_ref(),
                    )
                    .await
                {
                    Ok(response) => {
                        let approved = {
                            let lower = response.to_lowercase();
                            lower != "no" && lower != "n" && lower != "deny"
                        };
                        serde_json::json!({
                            "response": response,
                            "approved": approved,
                            "method": "channel_hook",
                            "elapsed_ms": 0,
                        })
                        .to_string()
                    }
                    Err(e) => serde_json::json!({
                        "response": format!("Error: {e:#}"),
                        "approved": false,
                        "method": "channel_hook",
                        "elapsed_ms": 0,
                    })
                    .to_string(),
                };
                HookAction::Skip { result_json }
            }
            "channel_notify" => {
                let message = serde_json::from_str::<serde_json::Value>(args_json)
                    .ok()
                    .and_then(|v| v.get("message")?.as_str().map(String::from))
                    .unwrap_or_default();

                if !message.is_empty() {
                    if let Some(cid) = self.channel_id.as_deref() {
                        self.router
                            .notify_channel(cid, &message, self.target.as_ref())
                            .await;
                    } else {
                        self.router.notify_all(&message).await;
                    }
                }
                HookAction::Skip {
                    result_json: r#"{"sent":true}"#.to_string(),
                }
            }
            "channel_send_file" => {
                let args = serde_json::from_str::<serde_json::Value>(args_json).unwrap_or_default();
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let caption = args
                    .get("caption")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let target_channel = args
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let result_json = if path.is_empty() {
                    r#"{"error":"path is required"}"#.to_string()
                } else {
                    match tokio::fs::read(path).await {
                        Ok(bytes) => {
                            let filename = std::path::Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("attachment")
                                .to_string();
                            let mime_type = mime_type_hint(&filename);
                            let size = bytes.len();
                            let event = that_channels::channel::ChannelEvent::Attachment {
                                filename: filename.clone(),
                                data: std::sync::Arc::new(bytes),
                                caption,
                                mime_type,
                            };
                            let cid = target_channel.as_deref().or(self.channel_id.as_deref());
                            if let Some(cid) = cid {
                                let _ =
                                    self.router.send_to(cid, &event, self.target.as_ref()).await;
                            } else {
                                self.router.broadcast(&event).await;
                            }
                            serde_json::json!({
                                "sent": true,
                                "filename": filename,
                                "size_bytes": size,
                            })
                            .to_string()
                        }
                        Err(e) => serde_json::json!({
                            "error": format!("Failed to read file '{path}': {e}")
                        })
                        .to_string(),
                    }
                };
                HookAction::Skip { result_json }
            }
            _ => {
                // Surface all tool invocations to the active channel when not
                // in a silent/background run, so users can follow along live.
                if !self.suppress_streaming {
                    let preview = compact_args_preview(name, args_json);
                    let event = ChannelEvent::ToolCall {
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        args: preview,
                    };
                    if let Some(cid) = self.channel_id.as_deref() {
                        let _ = self.router.send_to(cid, &event, self.target.as_ref()).await;
                    } else {
                        self.router.broadcast(&event).await;
                    }
                }
                HookAction::Continue
            }
        }
    }

    async fn on_tool_result(&self, name: &str, call_id: &str, result_json: &str) {
        let result: String = result_json.chars().take(2000).collect();
        let is_error = tool_result_is_error(result_json);
        debug!(tool = %name, is_error, result_chars = result_json.chars().count(), " ← {name}: {}", if is_error { "ERR" } else { "ok" });

        if let Some(tx) = &self.log_tx {
            let _ = tx.send(ToolLogEvent::Result {
                name: name.to_string(),
                result: result.clone(),
                is_error,
            });
        }

        // Dispatch to channels so adapters that support message editing (e.g. Telegram)
        // can update the in-flight ToolCall indicator with a completion summary.
        if !self.suppress_streaming {
            let event = ChannelEvent::ToolResult {
                call_id: call_id.to_string(),
                name: name.to_string(),
                result,
            };
            if let Some(cid) = self.channel_id.as_deref() {
                let _ = self.router.send_to(cid, &event, self.target.as_ref()).await;
            } else {
                self.router.broadcast(&event).await;
            }
        }
    }
}

/// Guess a MIME type string from a filename extension, for use in the Attachment event.
fn mime_type_hint(filename: &str) -> Option<String> {
    let ext = filename.rsplit('.').next()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "pdf" => "application/pdf",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "mp4" => "video/mp4",
            "mp3" => "audio/mpeg",
            "ogg" => "audio/ogg",
            "csv" => "text/csv",
            "txt" | "log" => "text/plain",
            "md" => "text/markdown",
            "json" => "application/json",
            "zip" => "application/zip",
            "tar" => "application/x-tar",
            "gz" => "application/gzip",
            _ => return None,
        }
        .to_string(),
    )
}
