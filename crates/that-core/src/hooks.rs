//! Channel hook — routes agent loop events to active [`ChannelRouter`] channels.
//!
//! `ChannelHook` lives here (not in `that-channels`) to avoid a circular
//! dependency: `that-core` depends on `that-channels`, so `that-channels`
//! cannot implement `crate::agent_loop::LoopHook`.

use std::sync::atomic::{AtomicBool, Ordering};
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
        return redact_secrets(
            &args_json
                .chars()
                .filter(|c| !c.is_control())
                .take(80)
                .collect::<String>(),
        );
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
            return redact_secrets(&format!("{key}={truncated}{ellipsis}"));
        }
    }
    if let Some(obj) = v.as_object() {
        for (key, val) in obj {
            if let Some(s) = val.as_str() {
                let truncated: String = s.chars().filter(|c| !c.is_control()).take(80).collect();
                return redact_secrets(&format!("{key}={truncated}"));
            }
        }
    }
    redact_secrets(&v.to_string().chars().take(80).collect::<String>())
}

/// Redact values that look like secrets/tokens from channel-visible previews.
///
/// Detects common token prefixes (ghp_, github_pat_, sk-, xoxb-, etc.) and
/// env-var assignment patterns (TOKEN=..., KEY=..., SECRET=...) and replaces
/// the secret portion with `***`.
fn redact_secrets(s: &str) -> String {
    let mut out = s.to_string();
    // Token prefixes: show the prefix + 4 chars, redact the rest.
    for prefix in &[
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "whsec_",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "pk_test_",
        "rk_live_",
        "rk_test_",
    ] {
        while let Some(start) = out.find(prefix) {
            let keep = start + prefix.len() + 4;
            let end = out[start..]
                .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '-' && c != '.')
                .map(|i| start + i)
                .unwrap_or(out.len());
            if end > keep {
                out.replace_range(keep..end, "***");
            } else {
                break;
            }
        }
    }
    // PEM private keys: -----BEGIN <type> PRIVATE KEY-----
    // Replace everything after the BEGIN header up to the END marker (or end of string).
    {
        let mut search_from = 0;
        while let Some(rel) = out[search_from..].find("-----BEGIN ") {
            let begin = search_from + rel;
            let after_begin = begin + "-----BEGIN ".len();
            if let Some(header_end) = out[after_begin..].find("-----") {
                let content_start = after_begin + header_end + 5;
                // Find the matching END marker or redact to end of string.
                let content_end = out[content_start..]
                    .find("-----END ")
                    .map(|i| {
                        let end_start = content_start + i;
                        out[end_start..]
                            .find("-----\n")
                            .or_else(|| out[end_start..].find("-----"))
                            .map(|j| end_start + j + 5)
                            .unwrap_or(out.len())
                    })
                    .unwrap_or(out.len());
                out.replace_range(content_start..content_end, "***");
                search_from = content_start + 3;
            } else {
                break;
            }
        }
    }

    // ENV_VAR=value patterns: redact the value after = for keys ending in
    // TOKEN, KEY, SECRET, PASSWORD, CREDENTIALS.
    for suffix in &["TOKEN", "KEY", "SECRET", "PASSWORD", "CREDENTIALS"] {
        // Scan for UPPER_CASE_SUFFIX= or SUFFIX="
        let needle = format!("{suffix}=");
        let mut search_from = 0;
        while let Some(eq_pos) = out[search_from..].find(&needle) {
            let abs_eq = search_from + eq_pos;
            // Verify the char before suffix looks like an env var name
            let var_start = out[..abs_eq]
                .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .map(|i| i + 1)
                .unwrap_or(0);
            let var_name = &out[var_start..abs_eq + suffix.len()];
            if !var_name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                search_from = abs_eq + needle.len();
                continue;
            }
            let val_start = abs_eq + needle.len();
            let (val_begin, quote) = if out[val_start..].starts_with(['"', '\'']) {
                (val_start + 1, true)
            } else {
                (val_start, false)
            };
            let val_end = if quote {
                out[val_begin..]
                    .find(['"', '\''])
                    .map(|i| val_begin + i)
                    .unwrap_or(out.len())
            } else {
                out[val_begin..]
                    .find(|c: char| c.is_whitespace() || c == ';' || c == '&')
                    .map(|i| val_begin + i)
                    .unwrap_or(out.len())
            };
            let keep = (val_begin + 4).min(val_end);
            if val_end > keep {
                out.replace_range(keep..val_end, "***");
            }
            search_from = keep + 3;
        }
    }
    out
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn github_pat() {
        // When inside GH_TOKEN="...", the env-var pattern fires — secret is still redacted.
        let s = r#"command=GH_TOKEN="github_pat_73Az8vcYUDOm9qEGKQrHr1efBBBBBBBBBBBBBBBB" gh auth"#;
        let r = redact_secrets(s);
        assert!(!r.contains("BBBBB"), "secret leaked: {r}");
        assert!(r.contains("***"), "no redaction: {r}");
    }

    #[test]
    fn github_pat_bare() {
        // Bare token without env-var wrapper.
        let r = redact_secrets("token github_pat_73Az8vcYUDOm9qEGKQrHr1efBBBBB rest");
        assert!(r.contains("github_pat_73Az***"), "got: {r}");
        assert!(!r.contains("BBBBB"));
    }

    #[test]
    fn sk_key() {
        let r = redact_secrets("sk-proj-abcdef1234567890longkey");
        assert!(r.contains("sk-proj***"), "got: {r}");
        assert!(!r.contains("1234567890"));
    }

    #[test]
    fn env_var_token() {
        let r = redact_secrets("MY_API_TOKEN=supersecretvalue123 next");
        assert!(r.contains("MY_API_TOKEN=supe***"), "got: {r}");
        assert!(!r.contains("secretvalue"));
    }

    #[test]
    fn no_false_positive() {
        let s = "path=/workspace/src/main.rs";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn pem_private_key() {
        let s = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA0Z3...\n-----END RSA PRIVATE KEY-----";
        let r = redact_secrets(s);
        assert!(
            r.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "header stripped: {r}"
        );
        assert!(!r.contains("MIIEpA"), "key material leaked: {r}");
        assert!(r.contains("***"), "no redaction: {r}");
    }

    #[test]
    fn pem_ec_key_inline() {
        let s =
            r#"echo "-----BEGIN EC PRIVATE KEY-----\nMHQCAQ...stuff-----END EC PRIVATE KEY-----""#;
        let r = redact_secrets(s);
        assert!(!r.contains("MHQCAQ"), "key material leaked: {r}");
    }
}

/// Check whether a pre-parsed JSON value represents a tool error.
pub fn is_error_value(value: &serde_json::Value) -> bool {
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

/// Return the `channel_send_message` tool schema.
///
/// Include this in the tool list when the agent has channel access. Sends a
/// structured message with optional rich UI (inline keyboards, reply markups).
/// [`ChannelHook`] intercepts calls and routes them to the router.
pub fn channel_send_message_tool_def() -> ToolDef {
    ToolDef {
        name: "channel_send_message".into(),
        description: "Send a rich message with optional interactive UI elements (inline \
            keyboards, reply keyboards) to a specific channel. Use this for messages that \
            need buttons, custom keyboards, or explicit parse mode control. For plain text \
            notifications, prefer channel_notify instead."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "Target channel ID (e.g. 'telegram')."
                },
                "text": {
                    "type": "string",
                    "description": "Message text content."
                },
                "parse_mode": {
                    "type": "string",
                    "enum": ["MarkdownV2", "HTML", "Plain"],
                    "description": "Optional text parsing mode. Omit for adapter default."
                },
                "reply_markup": {
                    "type": "object",
                    "description": "Optional interactive markup. Use {\"InlineKeyboard\": [[{\"text\":\"Label\",\"callback_data\":\"value\"}]]} for inline buttons, {\"ReplyKeyboard\":{\"keyboard\":[[{\"text\":\"Option\"}]],\"resize\":true,\"one_time\":true}} for custom keyboards, or {\"RemoveKeyboard\":null} to dismiss."
                },
                "reply_to_message_id": {
                    "type": "string",
                    "description": "Optional message ID to reply to (stringified platform ID)."
                }
            },
            "required": ["channel_id", "text"]
        }),
    }
}

/// Return the `channel_send_raw` tool schema.
///
/// Escape hatch for calling platform-native APIs directly. [`ChannelHook`]
/// intercepts calls and routes them to the router.
pub fn channel_send_raw_tool_def() -> ToolDef {
    ToolDef {
        name: "channel_send_raw".into(),
        description: "Call a platform-native API method directly on a channel adapter. \
            Use this as an escape hatch when the rich message model doesn't cover your \
            use case. The method name and JSON payload are forwarded verbatim to the \
            platform API, and the raw response JSON is returned."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": {
                    "type": "string",
                    "description": "Target channel ID (e.g. 'telegram')."
                },
                "method": {
                    "type": "string",
                    "description": "Platform API method name (e.g. 'sendPhoto', 'sendPoll')."
                },
                "payload": {
                    "type": "object",
                    "description": "JSON payload to send to the API method."
                }
            },
            "required": ["channel_id", "method", "payload"]
        }),
    }
}

/// Return the `answer` tool schema with dynamic formatting instructions.
///
/// The description includes the active channel's formatting guidance so the
/// agent knows how to compose its final message. [`ChannelHook`] intercepts
/// calls, sends a [`ChannelEvent::Done`], and returns a skip — dispatch() is
/// never reached.
pub fn channel_answer_tool_def(format_hint: &str) -> ToolDef {
    let mut desc = String::from(
        "Deliver your final answer to the human. This must be the last tool you call. \
         Compose it as a message to a person: lead with the outcome, not the mechanics.",
    );
    if !format_hint.is_empty() {
        desc.push(' ');
        desc.push_str(format_hint);
    }
    ToolDef {
        name: "answer".into(),
        description: desc,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The final answer message to deliver to the human."
                }
            },
            "required": ["message"]
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

/// Return the `channel_settings` tool schema.
pub fn channel_settings_tool_def() -> ToolDef {
    ToolDef {
        name: "channel_settings".into(),
        description: "Adjust channel display preferences for the current conversation. \
            Use work_visibility to control whether tool calls are shown to the user. \
            Set to false to hide internal work (cleaner output), true to show it again."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "work_visibility": {
                    "type": "boolean",
                    "description": "Whether to display tool calls and results on the channel."
                }
            },
            "required": ["work_visibility"]
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
    /// Per-sender preference: whether to surface tool calls/results on the channel.
    /// Toggled via the `channel_settings` tool. Defaults to true.
    show_work: Arc<AtomicBool>,
}

impl ChannelHook {
    pub fn new(
        router: Arc<ChannelRouter>,
        log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
        show_work: Arc<AtomicBool>,
    ) -> Self {
        Self {
            router,
            channel_id: None,
            target: None,
            log_tx,
            suppress_streaming: false,
            show_work,
        }
    }

    /// Create a hook scoped to one channel and optional target metadata.
    pub fn scoped(
        router: Arc<ChannelRouter>,
        channel_id: impl Into<String>,
        target: Option<OutboundTarget>,
        log_tx: Option<mpsc::UnboundedSender<ToolLogEvent>>,
        show_work: Arc<AtomicBool>,
    ) -> Self {
        Self {
            router,
            channel_id: Some(channel_id.into()),
            target,
            log_tx,
            suppress_streaming: false,
            show_work,
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
            show_work: Arc::new(AtomicBool::new(true)),
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
                // Background/heartbeat runs (suppress_streaming = true, no channel_id) must
                // not register a pending ask — doing so would create a wildcard that silently
                // consumes subsequent user messages, breaking any ongoing user conversation.
                if self.suppress_streaming {
                    let result_json = serde_json::json!({
                        "response": "human_ask is unavailable in background runs",
                        "approved": false,
                        "method": "channel_hook",
                        "elapsed_ms": 0,
                    })
                    .to_string();
                    return HookAction::Skip { result_json };
                }

                let parsed = serde_json::from_str::<serde_json::Value>(args_json).ok();
                let message = parsed
                    .as_ref()
                    .and_then(|v| v.get("message")?.as_str().map(String::from))
                    .unwrap_or_else(|| "The agent is asking for input.".into());
                let timeout = parsed.as_ref().and_then(|v| v.get("timeout")?.as_u64());

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
            "answer" => {
                let message = serde_json::from_str::<serde_json::Value>(args_json)
                    .ok()
                    .and_then(|v| v.get("message")?.as_str().map(String::from))
                    .unwrap_or_default();

                if !message.is_empty() {
                    let event = ChannelEvent::Done {
                        text: message.clone(),
                        input_tokens: 0,
                        output_tokens: 0,
                        cached_input_tokens: 0,
                        cache_write_tokens: 0,
                    };
                    if let Some(cid) = self.channel_id.as_deref() {
                        let _ = self.router.send_to(cid, &event, self.target.as_ref()).await;
                    } else {
                        self.router.broadcast(&event).await;
                    }
                }
                HookAction::Finish {
                    result_json: r#"{"delivered":true}"#.to_string(),
                    output_text: message,
                }
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
            "channel_send_message" => {
                let args = serde_json::from_str::<serde_json::Value>(args_json).unwrap_or_default();
                let channel_id = args
                    .get("channel_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                if channel_id.is_empty() || text.is_empty() {
                    return HookAction::Skip {
                        result_json: r#"{"error":"channel_id and text are required"}"#.to_string(),
                    };
                }

                let parse_mode = args
                    .get("parse_mode")
                    .and_then(|v| v.as_str())
                    .and_then(|s| match s {
                        "MarkdownV2" => Some(that_channels::message::ParseMode::MarkdownV2),
                        "HTML" => Some(that_channels::message::ParseMode::HTML),
                        "Plain" => Some(that_channels::message::ParseMode::Plain),
                        _ => None,
                    });

                let reply_markup: Option<that_channels::message::ReplyMarkup> = args
                    .get("reply_markup")
                    .and_then(|v| serde_json::from_value(v.clone()).ok());

                let reply_to_message_id = args
                    .get("reply_to_message_id")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));

                let msg = that_channels::message::OutboundMessage {
                    text,
                    parse_mode,
                    reply_markup,
                    reply_to_message_id,
                };

                let result_json = match self
                    .router
                    .send_message(channel_id, msg, self.target.as_ref())
                    .await
                {
                    Ok(handle) => serde_json::json!({
                        "sent": true,
                        "message_id": handle.message_id,
                        "channel_id": handle.channel_id,
                    })
                    .to_string(),
                    Err(e) => serde_json::json!({
                        "error": format!("{e:#}")
                    })
                    .to_string(),
                };
                HookAction::Skip { result_json }
            }
            "channel_send_raw" => {
                let args = serde_json::from_str::<serde_json::Value>(args_json).unwrap_or_default();
                let channel_id = args
                    .get("channel_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let payload = args
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));

                if channel_id.is_empty() || method.is_empty() {
                    return HookAction::Skip {
                        result_json: r#"{"error":"channel_id and method are required"}"#
                            .to_string(),
                    };
                }

                let result_json = match self.router.send_raw(channel_id, method, payload).await {
                    Ok(resp) => serde_json::json!({
                        "ok": true,
                        "response": resp,
                    })
                    .to_string(),
                    Err(e) => serde_json::json!({
                        "error": format!("{e:#}")
                    })
                    .to_string(),
                };
                HookAction::Skip { result_json }
            }
            "channel_settings" => {
                let args = serde_json::from_str::<JsonValue>(args_json).unwrap_or_default();
                if let Some(b) = args.get("work_visibility").and_then(|v| v.as_bool()) {
                    self.show_work.store(b, Ordering::Relaxed);
                }
                HookAction::Skip {
                    result_json: serde_json::json!({ "ok": true }).to_string(),
                }
            }
            "agent_query" => {
                let parsed = serde_json::from_str::<JsonValue>(args_json).unwrap_or_default();
                let stream = parsed.get("stream").and_then(|v| v.as_bool()) == Some(true);
                if !stream {
                    return HookAction::Continue;
                }

                // Streaming agent_query is relayed through the channel without blocking
                // on tool-event rendering. The underlying query itself remains synchronous.
                let agent_name = parsed["name"].as_str().unwrap_or("unknown").to_string();
                let message = parsed["message"].as_str().unwrap_or("").to_string();
                let timeout = parsed["timeout_secs"].as_u64().unwrap_or(120);

                let cluster_dir = dirs::home_dir()
                    .unwrap_or_default()
                    .join(".that-agent")
                    .join("cluster");
                let reg = crate::agents::AgentRegistry::new(cluster_dir.join("agents.json"));
                let gw = reg.list().ok().and_then(|entries| {
                    entries
                        .iter()
                        .find(|e| e.name == agent_name)
                        .and_then(|e| e.gateway_url.clone())
                });
                let Some(gateway_url) = gw else {
                    return HookAction::Skip {
                        result_json: serde_json::json!({
                            "error": format!("agent '{}' not found or has no gateway", agent_name)
                        })
                        .to_string(),
                    };
                };

                let router = Arc::clone(&self.router);
                let channel_id = self.channel_id.clone();
                let target = self.target.clone();
                let show_work = self.show_work.load(Ordering::Relaxed) && !self.suppress_streaming;

                // Spawn the streaming relay so work visibility continues while the
                // synchronous query is in-flight.
                let agent_name_ret = agent_name.clone();
                tokio::spawn(async move {
                    let (event_tx, mut event_rx) =
                        tokio::sync::mpsc::unbounded_channel::<crate::agents::AgentStreamEvent>();

                    let relay_router = Arc::clone(&router);
                    let relay_cid = channel_id.clone();
                    let relay_target = target.clone();
                    let relay = tokio::spawn(async move {
                        while let Some(event) = event_rx.recv().await {
                            if !show_work {
                                continue;
                            }
                            let ch_event = match &event {
                                crate::agents::AgentStreamEvent::ToolCall { name, args } => {
                                    ChannelEvent::ToolCall {
                                        call_id: String::new(),
                                        name: name.clone(),
                                        args: args.clone(),
                                    }
                                }
                                crate::agents::AgentStreamEvent::ToolResult { name, result } => {
                                    ChannelEvent::ToolResult {
                                        call_id: String::new(),
                                        name: name.clone(),
                                        result: result.clone(),
                                    }
                                }
                                crate::agents::AgentStreamEvent::Done { .. } => continue,
                                crate::agents::AgentStreamEvent::Error { message } => {
                                    ChannelEvent::Notify(format!("⚠ Sub-agent error: {message}"))
                                }
                            };
                            if let Some(cid) = relay_cid.as_deref() {
                                let _ = relay_router
                                    .send_to(cid, &ch_event, relay_target.as_ref())
                                    .await;
                            } else {
                                relay_router.broadcast(&ch_event).await;
                            }
                        }
                    });

                    let result = crate::agents::query_agent_stream(
                        &gateway_url,
                        &agent_name,
                        &message,
                        timeout,
                        event_tx,
                    )
                    .await;
                    let _ = relay.await;

                    // Deliver result as notification to channel + heartbeat queue.
                    let notification = match &result {
                        Ok(text) => {
                            let preview: String = text.chars().take(500).collect();
                            format!("[agent_query/{agent_name}] completed: {preview}")
                        }
                        Err(e) => format!("[agent_query/{agent_name}] failed: {e:#}"),
                    };
                    if let Some(cid) = channel_id.as_deref() {
                        let _ = router
                            .send_to(cid, &ChannelEvent::Notify(notification), target.as_ref())
                            .await;
                    } else {
                        router.notify_all(&notification).await;
                    }
                });

                // Return immediately — query runs in background.
                HookAction::Skip {
                    result_json: serde_json::json!({
                        "agent": agent_name_ret,
                        "status": "dispatched",
                        "note": "Query running in background. Result will arrive as a notification."
                    })
                    .to_string(),
                }
            }
            _ => {
                // Surface all tool invocations to the active channel when not
                // in a silent/background run and work visibility is enabled.
                if !self.suppress_streaming && self.show_work.load(Ordering::Relaxed) {
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
        // Parse once for both error detection and logging.
        let parsed = serde_json::from_str::<serde_json::Value>(result_json).ok();
        let is_error = parsed.as_ref().map(is_error_value).unwrap_or(false);
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
        if !self.suppress_streaming && self.show_work.load(Ordering::Relaxed) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::hook::LoopHook;

    #[tokio::test]
    async fn answer_tool_finishes_the_run() {
        let (router, _rx) = that_channels::ChannelRouter::new(vec![], 0);
        let router = Arc::new(router);
        let hook = ChannelHook::silent(router, None);
        let action = hook
            .on_tool_call("answer", "call-1", r#"{"message":"done"}"#)
            .await;
        match action {
            HookAction::Finish {
                result_json,
                output_text,
            } => {
                assert_eq!(result_json, r#"{"delivered":true}"#);
                assert_eq!(output_text, "done");
            }
            other => panic!("expected terminal answer hook action, got {other:?}"),
        }
    }
}
