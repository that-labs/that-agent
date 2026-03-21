use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A unique session identifier.
pub type SessionId = String;

/// A unique run identifier within a session.
pub type RunId = String;

/// Entry in the JSONL session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub timestamp: DateTime<Utc>,
    pub run_id: RunId,
    #[serde(flatten)]
    pub event: TranscriptEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TranscriptEvent {
    #[serde(rename = "run_start")]
    RunStart { task: String },

    #[serde(rename = "user_message")]
    UserMessage { content: String },

    #[serde(rename = "assistant_message")]
    AssistantMessage { content: String },

    #[serde(rename = "tool_call")]
    ToolCall {
        tool: String,
        arguments: serde_json::Value,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        tool: String,
        result: String,
        is_error: bool,
    },

    #[serde(rename = "run_end")]
    RunEnd {
        status: RunStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },

    #[serde(rename = "compaction")]
    Compaction { summary: String },

    #[serde(rename = "restart")]
    Restart {
        interrupted_run_id: Option<String>,
        /// The task/prompt that was running when the process was interrupted.
        #[serde(skip_serializing_if = "Option::is_none")]
        last_task: Option<String>,
        /// The last tool the agent called before the interruption.
        #[serde(skip_serializing_if = "Option::is_none")]
        last_tool: Option<String>,
        /// Recent tool call names from the interrupted run (most recent last).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        recent_tools: Vec<String>,
    },

    #[serde(rename = "usage")]
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
        tool_calls: u64,
        model: String,
        provider: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Success,
    Error,
    Cancelled,
    MaxTurns,
}

/// Summary of a session for display in the resume picker.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: SessionId,
    pub preview: String,
    pub timestamp: String,
    pub entry_count: usize,
}

/// Aggregated usage data across sessions.
#[derive(Debug, Clone, Default)]
pub struct AggregatedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub tool_calls: u64,
    pub estimated_cost: f64,
    pub session_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelPreferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl ChannelPreferences {
    pub fn is_default(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }
}

/// Manages sessions and their JSONL transcripts.
pub struct SessionManager {
    state_dir: PathBuf,
}

impl SessionManager {
    pub fn new(state_dir: &Path) -> Result<Self> {
        let sessions_dir = state_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
        })
    }

    /// Create a new session with a date-based ID, returning its ID.
    ///
    /// Format: YYYYMMDD-HHMMSS-XXXX where XXXX is 4 hex chars derived from a UUID.
    pub fn create_session(&self) -> Result<SessionId> {
        let now = Utc::now();
        let date_part = now.format("%Y%m%d-%H%M%S").to_string();
        let uuid_bytes = Uuid::new_v4();
        let hex_suffix = format!(
            "{:02x}{:02x}",
            uuid_bytes.as_bytes()[0],
            uuid_bytes.as_bytes()[1]
        );
        let id = format!("{date_part}-{hex_suffix}");
        let path = self.session_path(&id);
        // Create the empty transcript file
        std::fs::write(&path, "")?;
        tracing::info!(session_id = %id, "Created new session");
        Ok(id)
    }

    /// List all session IDs sorted by modification time (newest first).
    pub fn list_sessions(&self) -> Result<Vec<SessionId>> {
        let dir = self.state_dir.join("sessions");
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut sessions: Vec<(SessionId, std::time::SystemTime)> = vec![];
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".jsonl") {
                let id = name.trim_end_matches(".jsonl").to_string();
                let modified = entry.metadata()?.modified()?;
                sessions.push((id, modified));
            }
        }
        sessions.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(sessions.into_iter().map(|(id, _)| id).collect())
    }

    /// Append an entry to a session transcript.
    pub fn append(&self, session_id: &str, entry: &TranscriptEntry) -> Result<()> {
        let path = self.session_path(session_id);
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("Failed to open session transcript")?
            .write_all(line.as_bytes())
            .context("Failed to write transcript entry")?;
        Ok(())
    }

    /// Read all entries from a session transcript.
    pub fn read_transcript(&self, session_id: &str) -> Result<Vec<TranscriptEntry>> {
        let path = self.session_path(session_id);
        if !path.exists() {
            anyhow::bail!("Session not found: {session_id}");
        }
        let content = std::fs::read_to_string(&path)?;
        let entries: Vec<TranscriptEntry> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()
            .context("Failed to parse transcript")?;
        Ok(entries)
    }

    /// Get the most recent session ID, or create one if none exists.
    pub fn get_or_create_session(&self) -> Result<SessionId> {
        let sessions = self.list_sessions()?;
        if let Some(id) = sessions.first() {
            Ok(id.clone())
        } else {
            self.create_session()
        }
    }

    /// Return summaries of recent sessions for the resume picker.
    ///
    /// Reads the first user message from each session for a preview.
    pub fn session_summaries(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let sessions = self.list_sessions()?;
        let mut summaries = Vec::new();

        for id in sessions.into_iter().take(limit) {
            let path = self.session_path(&id);
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Count entries and find first user message for preview
            let mut entry_count = 0usize;
            let mut preview = String::new();
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                entry_count += 1;
                if preview.is_empty() {
                    if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
                        if let TranscriptEvent::UserMessage { content } = &entry.event {
                            preview = if content.len() > 80 {
                                format!("{}...", &content[..content.floor_char_boundary(80)])
                            } else {
                                content.clone()
                            };
                        }
                    }
                }
            }

            if preview.is_empty() {
                preview = "(empty session)".into();
            }

            // Extract timestamp from ID format
            let timestamp = parse_session_timestamp(&id);

            summaries.push(SessionSummary {
                id,
                preview,
                timestamp,
                entry_count,
            });
        }

        Ok(summaries)
    }

    /// Find a session whose ID starts with the given prefix.
    pub fn find_session_by_prefix(&self, prefix: &str) -> Result<Option<SessionId>> {
        let sessions = self.list_sessions()?;
        let matches: Vec<_> = sessions
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.into_iter().next().unwrap())),
            _ => {
                // Return the most recent match (list is already sorted newest-first)
                Ok(Some(matches.into_iter().next().unwrap()))
            }
        }
    }

    /// Aggregate usage data from sessions modified after `since`.
    pub fn aggregate_usage(&self, since: DateTime<Utc>) -> Result<AggregatedUsage> {
        let dir = self.state_dir.join("sessions");
        if !dir.exists() {
            return Ok(AggregatedUsage::default());
        }

        let since_system: std::time::SystemTime = since.into();
        let mut agg = AggregatedUsage::default();

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".jsonl") {
                continue;
            }
            let modified = entry.metadata()?.modified()?;
            if modified < since_system {
                continue;
            }

            // Read usage events from this session
            let content = match std::fs::read_to_string(entry.path()) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut session_has_usage = false;
            for line in content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(te) = serde_json::from_str::<TranscriptEntry>(line) {
                    if let TranscriptEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        tool_calls,
                        model,
                        provider,
                        ..
                    } = &te.event
                    {
                        agg.input_tokens += input_tokens;
                        agg.output_tokens += output_tokens;
                        agg.cached_input_tokens += cached_input_tokens;
                        agg.tool_calls += tool_calls;
                        agg.estimated_cost +=
                            estimate_cost(*input_tokens, *output_tokens, provider, model);
                        session_has_usage = true;
                    }
                }
            }
            if session_has_usage {
                agg.session_count += 1;
            }
        }

        Ok(agg)
    }

    /// Load the persistent sender-key → session-ID index for channel-based runs.
    ///
    /// Returns an empty map if the index does not exist yet (first run).
    pub fn load_channel_sessions(&self) -> std::collections::HashMap<String, SessionId> {
        let path = self.state_dir.join("channel_sessions.json");
        if !path.exists() {
            return std::collections::HashMap::new();
        }
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// If the given session has an interrupted run (RunStart without RunEnd),
    /// append a `Restart` marker and return the interrupted run ID.
    pub fn mark_restart_if_interrupted(&self, session_id: &str) -> Option<String> {
        let entries = self.read_transcript(session_id).ok()?;
        let interrupted = find_interrupted_run(&entries)?;
        let _ = self.append(
            session_id,
            &TranscriptEntry {
                timestamp: chrono::Utc::now(),
                run_id: new_run_id(),
                event: TranscriptEvent::Restart {
                    interrupted_run_id: Some(interrupted.run_id.clone()),
                    last_task: interrupted.last_task,
                    last_tool: interrupted.last_tool,
                    recent_tools: interrupted.recent_tools,
                },
            },
        );
        Some(interrupted.run_id)
    }

    /// Persist a sender-key → session-ID mapping so context can be restored after a restart.
    pub fn save_channel_session(&self, sender_key: &str, session_id: &str) {
        let path = self.state_dir.join("channel_sessions.json");
        let mut map = self.load_channel_sessions();
        map.insert(sender_key.to_string(), session_id.to_string());
        let _ = crate::channels::atomic_write_json(&path, &map);
    }

    /// Load persistent per-sender channel preferences such as model overrides.
    pub fn load_channel_preferences(
        &self,
    ) -> std::collections::HashMap<String, ChannelPreferences> {
        let path = self.state_dir.join("channel_preferences.json");
        if !path.exists() {
            return std::collections::HashMap::new();
        }
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist per-sender channel preferences.
    pub fn save_channel_preferences(&self, sender_key: &str, prefs: &ChannelPreferences) {
        let path = self.state_dir.join("channel_preferences.json");
        let mut map = self.load_channel_preferences();
        if prefs.is_default() {
            map.remove(sender_key);
        } else {
            map.insert(sender_key.to_string(), prefs.clone());
        }
        let _ = crate::channels::atomic_write_json(&path, &map);
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.state_dir
            .join("sessions")
            .join(format!("{session_id}.jsonl"))
    }
}

/// Rebuild LLM conversation history from transcript entries.
///
/// Extracts UserMessage and AssistantMessage events into Messages
/// suitable for continuing a conversation.
pub fn rebuild_history(entries: &[TranscriptEntry]) -> Vec<crate::agent_loop::Message> {
    let mut history = Vec::new();
    for entry in entries {
        match &entry.event {
            TranscriptEvent::UserMessage { content } => {
                history.push(crate::agent_loop::Message::user(content));
            }
            TranscriptEvent::AssistantMessage { content } => {
                history.push(crate::agent_loop::Message::assistant(content));
            }
            TranscriptEvent::Restart {
                last_task,
                last_tool,
                recent_tools,
                ..
            } => {
                let ctx = InterruptedRun {
                    run_id: String::new(),
                    last_task: last_task.clone(),
                    last_tool: last_tool.clone(),
                    recent_tools: recent_tools.clone(),
                };
                append_restart_marker(&mut history, &ctx);
            }
            _ => {}
        }
    }
    // Catch not-yet-persisted interruptions (no Restart event written yet).
    if let Some(ctx) = find_interrupted_run(entries) {
        append_restart_marker(&mut history, &ctx);
    }
    history
}

/// Rebuild conversation history from a transcript, respecting compaction.
///
/// - If a `Compaction` event exists, its summary is injected as a context anchor and only
///   messages **after** the last compaction are replayed.
/// - `max_pairs` caps the number of user/assistant exchanges restored. Pass `usize::MAX`
///   to restore everything since the last compaction (caller should guard the no-compaction
///   case with a reasonable fallback).
pub fn rebuild_history_recent(
    entries: &[TranscriptEntry],
    max_pairs: usize,
) -> Vec<crate::agent_loop::Message> {
    // Locate the last compaction marker.
    let compaction_pos = entries
        .iter()
        .rposition(|e| matches!(e.event, TranscriptEvent::Compaction { .. }));

    let (compaction_summary, tail) = if let Some(pos) = compaction_pos {
        let summary = match &entries[pos].event {
            TranscriptEvent::Compaction { summary } => Some(summary.clone()),
            _ => None,
        };
        (summary, &entries[pos + 1..])
    } else {
        (None, entries)
    };

    // Collect complete user→assistant pairs from the tail, and track restart events.
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut pending_user: Option<String> = None;
    let mut has_restart_event = false;
    for entry in tail {
        match &entry.event {
            TranscriptEvent::UserMessage { content } => {
                pending_user = Some(content.clone());
            }
            TranscriptEvent::AssistantMessage { content } => {
                if let Some(user) = pending_user.take() {
                    pairs.push((user, content.clone()));
                }
            }
            TranscriptEvent::Restart {
                last_task,
                last_tool,
                recent_tools,
                ..
            } => {
                // Persisted restart marker — inject as a synthetic pair.
                let ctx = InterruptedRun {
                    run_id: String::new(),
                    last_task: last_task.clone(),
                    last_tool: last_tool.clone(),
                    recent_tools: recent_tools.clone(),
                };
                let (user_msg, asst_msg) = restart_marker_pair(&ctx);
                pairs.push((user_msg, asst_msg));
                has_restart_event = true;
            }
            _ => {}
        }
    }

    // Keep only the most recent `max_pairs` exchanges.
    let skip = pairs.len().saturating_sub(max_pairs);
    let recent = &pairs[skip..];

    let mut history: Vec<crate::agent_loop::Message> = Vec::new();

    // Prepend the compaction summary as a context anchor.
    if let Some(summary) = compaction_summary {
        history.push(crate::agent_loop::Message::user(format!(
            "[Conversation context summary from an earlier session: {summary}]"
        )));
        history.push(crate::agent_loop::Message::assistant(
            "Understood, I have the context from our previous conversation.".to_string(),
        ));
    }

    for (user, assistant) in recent {
        history.push(crate::agent_loop::Message::user(user));
        history.push(crate::agent_loop::Message::assistant(assistant));
    }

    // Detect not-yet-persisted interrupted runs (no Restart event written yet).
    if !has_restart_event {
        if let Some(ctx) = find_interrupted_run(tail) {
            append_restart_marker(&mut history, &ctx);
        }
    }

    history
}

/// Build the restart awareness message pair (user, assistant) from an interrupted run context.
fn restart_marker_pair(ctx: &InterruptedRun) -> (String, String) {
    let mut msg = String::from(
        "[System: the previous run was interrupted by a process restart. \
         Any in-progress work may be incomplete.",
    );
    if let Some(task) = &ctx.last_task {
        let preview: String = task.chars().take(200).collect();
        msg.push_str(&format!(" You were working on: \"{preview}\""));
        if task.chars().count() > 200 {
            msg.push_str("...");
        }
        msg.push('.');
    }
    if !ctx.recent_tools.is_empty() {
        // Show up to the last 15 tool calls so the agent sees what it was doing.
        let chain: Vec<&str> = ctx
            .recent_tools
            .iter()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|s| s.as_str())
            .collect();
        msg.push_str(&format!(" Recent tool chain: {}", chain.join(" → ")));
        msg.push('.');
    } else if let Some(tool) = &ctx.last_tool {
        msg.push_str(&format!(" The last tool you called was: {tool}."));
    }
    msg.push_str(" Assess the situation before continuing.]");
    (
        msg,
        "Understood — I was interrupted. I'll check the current state before proceeding."
            .to_string(),
    )
}

/// Append a restart awareness message pair to the history so the LLM knows
/// it was interrupted and what it was doing at the time.
fn append_restart_marker(history: &mut Vec<crate::agent_loop::Message>, ctx: &InterruptedRun) {
    let (user_msg, asst_msg) = restart_marker_pair(ctx);
    history.push(crate::agent_loop::Message::user(user_msg));
    history.push(crate::agent_loop::Message::assistant(asst_msg));
}

/// Context about an interrupted run.
struct InterruptedRun {
    run_id: String,
    last_task: Option<String>,
    last_tool: Option<String>,
    /// Recent tool call names from the interrupted run (most recent last).
    recent_tools: Vec<String>,
}

/// Returns context about the last interrupted run, if any.
fn find_interrupted_run(entries: &[TranscriptEntry]) -> Option<InterruptedRun> {
    let mut current: Option<InterruptedRun> = None;
    let mut open = 0i32;
    for entry in entries {
        match &entry.event {
            TranscriptEvent::RunStart { task } => {
                current = Some(InterruptedRun {
                    run_id: entry.run_id.clone(),
                    last_task: Some(task.clone()),
                    last_tool: None,
                    recent_tools: Vec::new(),
                });
                open += 1;
            }
            TranscriptEvent::RunEnd { .. } | TranscriptEvent::Restart { .. } => {
                open = (open - 1).max(0);
                if open == 0 {
                    current = None;
                }
            }
            TranscriptEvent::ToolCall { tool, .. } if open > 0 => {
                if let Some(ref mut c) = current {
                    c.last_tool = Some(tool.clone());
                    c.recent_tools.push(tool.clone());
                }
            }
            _ => {}
        }
    }
    if open > 0 {
        current
    } else {
        None
    }
}

/// Parse a session ID into a human-readable timestamp string.
///
/// Handles both date-based (YYYYMMDD-HHMMSS-XXXX) and legacy UUID formats.
fn parse_session_timestamp(id: &str) -> String {
    // Date-based format: 20260216-143052-a7f3
    if id.len() >= 15 && id.chars().take(8).all(|c| c.is_ascii_digit()) {
        if let (Ok(date), Ok(time)) = (
            chrono::NaiveDate::parse_from_str(&id[..8], "%Y%m%d"),
            chrono::NaiveTime::parse_from_str(&id[9..15], "%H%M%S"),
        ) {
            let dt = chrono::NaiveDateTime::new(date, time);
            return dt.format("%Y-%m-%d %H:%M").to_string();
        }
    }
    // Legacy UUID format — use file metadata would be ideal but we don't have the path here,
    // so just return the ID truncated
    if id.len() > 20 {
        format!("{}...", &id[..8])
    } else {
        id.to_string()
    }
}

/// Estimate cost for a single usage event.
fn estimate_cost(input_tokens: u64, output_tokens: u64, provider: &str, model: &str) -> f64 {
    let _ = provider;
    let (input_rate, output_rate) = match model {
        m if m.starts_with("claude-opus-4") => (5.0, 25.0),
        m if m.starts_with("claude-sonnet-4") => (3.0, 15.0),
        m if m.starts_with("claude-haiku-4") => (1.0, 5.0),
        m if m.starts_with("gpt-5.2") => (1.75, 14.0),
        m if m.starts_with("gpt-5.1") => (0.25, 2.0),
        m if m.starts_with("gpt-4o") => (2.50, 10.0),
        m if m.starts_with("gpt-4.1") => (2.0, 8.0),
        m if m.starts_with("o4-mini") => (1.10, 4.40),
        m if m.starts_with("o3") => (0.40, 1.60),
        _ => (3.0, 15.0),
    };
    let input_cost = (input_tokens as f64) * input_rate / 1_000_000.0;
    let output_cost = (output_tokens as f64) * output_rate / 1_000_000.0;
    input_cost + output_cost
}

use std::io::Write;

/// Generate a new run ID.
pub fn new_run_id() -> RunId {
    Uuid::new_v4().to_string()
}
