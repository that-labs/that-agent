//! Agent lifecycle management — spawn, list, and query peer agents.
//!
//! In Kubernetes mode, agents are K8s Deployments (persistent) or Jobs (ephemeral).
//! Agents registered here persist across sessions via a file-backed registry
//! at `~/.that-agent/cluster/agents.json`. The spawning agent writes a
//! `config.toml` for the child and starts the binary in the background.
//!
//! In Kubernetes mode (`THAT_SANDBOX_MODE=kubernetes`), agents are created as
//! K8s Deployments (persistent) or Jobs (ephemeral) instead of local processes.
//! The file-backed registry is replaced by K8s labels as source of truth.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── Registry ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub name: String,
    pub role: Option<String>,
    pub parent: Option<String>,
    pub pid: u32,
    pub gateway_url: Option<String>,
    pub started_at: String,
}

/// File-backed registry of known peer agents in the cluster.
///
/// Persisted at `<home>/.that-agent/cluster/agents.json`.
/// Entries are stale when the PID is dead — callers should check `is_alive`.
pub struct AgentRegistry {
    path: PathBuf,
}

impl AgentRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Upsert an agent entry (keyed by name).
    pub fn register(&self, entry: AgentEntry) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.name != entry.name);
        entries.push(entry);
        self.save(&entries)
    }

    /// Remove a named agent from the registry.
    pub fn unregister(&self, name: &str) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.name != name);
        self.save(&entries)
    }

    /// Return all registered agents.
    pub fn list(&self) -> Result<Vec<AgentEntry>> {
        self.load()
    }

    fn load(&self) -> Result<Vec<AgentEntry>> {
        match std::fs::read_to_string(&self.path) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, entries: &[AgentEntry]) -> Result<()> {
        crate::channels::atomic_write_json(&self.path, entries)
    }

    /// Return `true` if the OS process with `pid` is still running.
    pub fn is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

// ── Agent Task Registry ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

impl std::fmt::Display for AgentTaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submitted => write!(f, "submitted"),
            Self::Working => write!(f, "working"),
            Self::InputRequired => write!(f, "input_required"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Canceled => write!(f, "canceled"),
        }
    }
}

impl std::str::FromStr for AgentTaskState {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        serde_json::from_value(serde_json::Value::String(s.to_string()))
            .map_err(|_| format!("unknown task state: {s}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub from: String,
    pub text: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScratchpadEntry {
    pub from: String,
    pub note: String,
    pub timestamp: String,
    #[serde(default)]
    pub kind: String,
}

/// Max scratchpad entries per task.
const MAX_SCRATCHPAD_ENTRIES: usize = 50;
/// Max stable header entries per task.
const MAX_SCRATCHPAD_HEADER_ENTRIES: usize = 12;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    pub id: String,
    pub agent: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub participants: Vec<String>,
    pub state: AgentTaskState,
    pub created_at: String,
    pub updated_at: String,
    pub result: Option<String>,
    pub messages: Vec<TaskMessage>,
    #[serde(default)]
    pub scratchpad_header: Vec<ScratchpadEntry>,
    #[serde(default)]
    pub scratchpad: Vec<ScratchpadEntry>,
    #[serde(default)]
    pub scratchpad_revision: u64,
}

/// Append-only journal-backed registry of agent tasks at `<cluster_dir>/agent_tasks.json`.
#[derive(Debug)]
pub struct AgentTaskRegistry {
    path: PathBuf,
}

/// Max terminal (completed/failed) tasks kept in the registry before pruning.
const MAX_TERMINAL_TASKS: usize = 100;
/// Max messages per task to prevent context bloat.
const MAX_MESSAGES_PER_TASK: usize = 30;

impl AgentTaskRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Derive registry from a memory DB path (convenience).
    pub fn from_db_path(db_path: &Path) -> Option<Self> {
        cluster_dir_from_db(db_path).map(|d| Self::new(d.join("agent_tasks.json")))
    }

    pub fn create(&self, agent: &str, message: &str, owner: &str) -> Result<AgentTask> {
        let now = chrono::Utc::now().to_rfc3339();
        let task = AgentTask {
            id: uuid::Uuid::new_v4().to_string(),
            agent: agent.to_string(),
            owner: owner.to_string(),
            participants: vec![owner.to_string(), agent.to_string()],
            state: AgentTaskState::Submitted,
            created_at: now.clone(),
            updated_at: now.clone(),
            result: None,
            messages: vec![TaskMessage {
                from: owner.to_string(),
                text: message.to_string(),
                timestamp: now,
            }],
            scratchpad_header: Vec::new(),
            scratchpad: Vec::new(),
            scratchpad_revision: 0,
        };
        self.record_event(crate::channels::TaskJournalEvent::Created {
            task: serde_json::to_value(&task)?,
        })?;
        Ok(task)
    }

    pub fn get(&self, id: &str) -> Result<Option<AgentTask>> {
        Ok(self.load()?.into_iter().find(|t| t.id == id))
    }

    pub fn list_active(&self) -> Result<Vec<AgentTask>> {
        Ok(self
            .load()?
            .into_iter()
            .filter(|t| {
                !matches!(
                    t.state,
                    AgentTaskState::Completed | AgentTaskState::Failed | AgentTaskState::Canceled
                )
            })
            .collect())
    }

    pub fn update_state(
        &self,
        id: &str,
        state: AgentTaskState,
        from: Option<&str>,
        message: Option<&str>,
    ) -> Result<()> {
        self.record_event(crate::channels::TaskJournalEvent::StateUpdated {
            id: id.to_string(),
            state: state.to_string(),
            from: from.map(|value| value.to_string()),
            message: message.map(|value| value.to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn append_message(&self, id: &str, from: &str, text: &str) -> Result<()> {
        self.record_event(crate::channels::TaskJournalEvent::MessageAppended {
            id: id.to_string(),
            from: from.to_string(),
            text: text.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn scratchpad_append(&self, id: &str, from: &str, note: &str) -> Result<()> {
        self.scratchpad_append_kind(id, from, note, None)
    }

    pub fn scratchpad_append_kind(
        &self,
        id: &str,
        from: &str,
        note: &str,
        kind: Option<&str>,
    ) -> Result<()> {
        self.record_event(crate::channels::TaskJournalEvent::ScratchpadAppended {
            id: id.to_string(),
            from: from.to_string(),
            note: note.to_string(),
            kind: kind.unwrap_or_default().to_string(),
            section: "activity".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn scratchpad_set_header(
        &self,
        id: &str,
        from: &str,
        kind: &str,
        note: &str,
    ) -> Result<()> {
        if self.get(id)?.is_some_and(|task| {
            task.scratchpad_header
                .iter()
                .any(|entry| entry.kind == kind && entry.from == from && entry.note == note)
        }) {
            return Ok(());
        }
        self.record_event(crate::channels::TaskJournalEvent::ScratchpadAppended {
            id: id.to_string(),
            from: from.to_string(),
            note: note.to_string(),
            kind: kind.to_string(),
            section: "header".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    pub fn add_participant(&self, id: &str, participant: &str) -> Result<()> {
        self.record_event(crate::channels::TaskJournalEvent::ParticipantAdded {
            id: id.to_string(),
            participant: participant.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    fn cap_messages(task: &mut AgentTask) {
        if task.messages.len() > MAX_MESSAGES_PER_TASK {
            task.messages = task
                .messages
                .split_off(task.messages.len() - MAX_MESSAGES_PER_TASK);
        }
    }

    /// Remove oldest terminal tasks when the list exceeds the cap.
    fn prune_terminal(tasks: &mut Vec<AgentTask>) {
        let terminal: usize = tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    AgentTaskState::Completed | AgentTaskState::Failed | AgentTaskState::Canceled
                )
            })
            .count();
        if terminal > MAX_TERMINAL_TASKS {
            let to_remove = terminal - MAX_TERMINAL_TASKS;
            let mut removed = 0;
            tasks.retain(|t| {
                if removed >= to_remove {
                    return true;
                }
                if matches!(
                    t.state,
                    AgentTaskState::Completed | AgentTaskState::Failed | AgentTaskState::Canceled
                ) {
                    removed += 1;
                    return false;
                }
                true
            });
        }
    }

    fn record_event(&self, event: crate::channels::TaskJournalEvent) -> Result<()> {
        crate::channels::with_path_lock(&self.path, || {
            crate::channels::seed_task_journal_from_snapshot(&self.path)?;
            let mut tasks = self.load_unlocked()?;
            Self::apply_event(&mut tasks, event.clone())?;
            crate::channels::append_task_journal_event(&self.path, &event)?;
            Self::prune_terminal(&mut tasks);
            self.save_unlocked(&tasks)?;
            Ok(())
        })
    }

    fn load(&self) -> Result<Vec<AgentTask>> {
        self.load_unlocked()
    }

    fn load_unlocked(&self) -> Result<Vec<AgentTask>> {
        let journal = crate::channels::read_task_journal_events(&self.path)?;
        if !journal.is_empty() {
            let mut tasks = Vec::new();
            for event in journal {
                Self::apply_event(&mut tasks, event)?;
            }
            for task in &mut tasks {
                Self::normalize_task(task);
            }
            Self::prune_terminal(&mut tasks);
            return Ok(tasks);
        }
        match std::fs::read_to_string(&self.path) {
            Ok(data) => {
                let mut tasks: Vec<AgentTask> = serde_json::from_str(&data)?;
                for task in &mut tasks {
                    Self::normalize_task(task);
                }
                Ok(tasks)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save_unlocked(&self, tasks: &[AgentTask]) -> Result<()> {
        crate::channels::atomic_write_json(&self.path, tasks)
    }

    fn apply_event(
        tasks: &mut Vec<AgentTask>,
        event: crate::channels::TaskJournalEvent,
    ) -> Result<()> {
        match event {
            crate::channels::TaskJournalEvent::Snapshot { tasks: snapshot } => {
                *tasks = serde_json::from_value(snapshot)?;
                for task in tasks.iter_mut() {
                    Self::normalize_task(task);
                }
            }
            crate::channels::TaskJournalEvent::Created { task } => {
                let mut task: AgentTask = serde_json::from_value(task)?;
                Self::normalize_task(&mut task);
                tasks.retain(|existing| existing.id != task.id);
                tasks.push(task);
            }
            crate::channels::TaskJournalEvent::StateUpdated {
                id,
                state,
                from,
                message,
                timestamp,
            } => {
                if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                    task.state = state.parse().map_err(anyhow::Error::msg)?;
                    task.updated_at = timestamp.clone();
                    if let Some(msg) = message {
                        let actor = from.unwrap_or_else(|| task.agent.clone());
                        Self::add_participant_name(task, &actor);
                        if matches!(
                            task.state,
                            AgentTaskState::Completed
                                | AgentTaskState::Failed
                                | AgentTaskState::Canceled
                        ) {
                            task.result = Some(msg.clone());
                        }
                        task.messages.push(TaskMessage {
                            from: actor,
                            text: msg,
                            timestamp,
                        });
                        Self::cap_messages(task);
                    }
                }
            }
            crate::channels::TaskJournalEvent::MessageAppended {
                id,
                from,
                text,
                timestamp,
            } => {
                if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                    task.updated_at = timestamp.clone();
                    Self::add_participant_name(task, &from);
                    task.messages.push(TaskMessage {
                        from,
                        text,
                        timestamp,
                    });
                    Self::cap_messages(task);
                }
            }
            crate::channels::TaskJournalEvent::ScratchpadAppended {
                id,
                from,
                note,
                kind,
                section,
                timestamp,
            } => {
                if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                    Self::add_participant_name(task, &from);
                    let entry = ScratchpadEntry {
                        from,
                        note,
                        timestamp: timestamp.clone(),
                        kind,
                    };
                    task.updated_at = timestamp;
                    task.scratchpad_revision += 1;
                    if section == "header" {
                        task.scratchpad_header
                            .retain(|existing| existing.kind != entry.kind);
                        task.scratchpad_header.push(entry);
                        if task.scratchpad_header.len() > MAX_SCRATCHPAD_HEADER_ENTRIES {
                            task.scratchpad_header = task.scratchpad_header.split_off(
                                task.scratchpad_header.len() - MAX_SCRATCHPAD_HEADER_ENTRIES,
                            );
                        }
                    } else {
                        task.scratchpad.push(entry);
                        if task.scratchpad.len() > MAX_SCRATCHPAD_ENTRIES {
                            task.scratchpad = task
                                .scratchpad
                                .split_off(task.scratchpad.len() - MAX_SCRATCHPAD_ENTRIES);
                        }
                    }
                }
            }
            crate::channels::TaskJournalEvent::ParticipantAdded {
                id,
                participant,
                timestamp,
            } => {
                if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                    task.updated_at = timestamp;
                    Self::add_participant_name(task, &participant);
                }
            }
        }
        Ok(())
    }

    fn normalize_task(task: &mut AgentTask) {
        if task.owner.trim().is_empty() {
            task.owner = task
                .messages
                .first()
                .map(|msg| msg.from.clone())
                .filter(|from| !from.trim().is_empty())
                .unwrap_or_else(|| "parent".to_string());
        }
        let owner = task.owner.clone();
        let agent = task.agent.clone();
        Self::add_participant_name(task, &owner);
        Self::add_participant_name(task, &agent);
        if task.scratchpad_revision == 0 {
            task.scratchpad_revision =
                (task.scratchpad_header.len() + task.scratchpad.len()) as u64;
        }
    }

    fn add_participant_name(task: &mut AgentTask, participant: &str) {
        let participant = participant.trim();
        if participant.is_empty() {
            return;
        }
        if !task.participants.iter().any(|p| p == participant) {
            task.participants.push(participant.to_string());
        }
    }
}

/// Resolve cluster dir and agent's gateway URL in one step.
///
/// Returns `(cluster_dir, gateway_url)`. Used by tool dispatch to avoid
/// repeating the 6-line resolve→list→find→extract pattern.
pub fn resolve_agent_gateway(db_path: &Path, agent_name: &str) -> Result<(PathBuf, String)> {
    let cluster_dir =
        cluster_dir_from_db(db_path).ok_or_else(|| anyhow::anyhow!("Cannot derive cluster dir"))?;
    let reg = AgentRegistry::new(cluster_dir.join("agents.json"));
    let entries = reg.list()?;
    let entry = entries
        .iter()
        .find(|e| e.name == agent_name)
        .ok_or_else(|| anyhow::anyhow!("agent '{}' not found in registry", agent_name))?;
    let gw = entry
        .gateway_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("agent '{}' has no gateway URL", agent_name))?
        .to_string();
    Ok((cluster_dir, gw))
}

/// Post a message to a sub-agent's `/v1/inbound` endpoint.
pub async fn post_to_agent_inbound(
    gateway_url: &str,
    message: &str,
    sender_id: &str,
    callback_url: Option<&str>,
) -> Result<()> {
    let mut body = serde_json::json!({
        "message": message,
        "sender_id": sender_id,
    });
    if let Some(cb) = callback_url {
        body["callback_url"] = serde_json::Value::String(cb.to_string());
    }
    let client = reqwest::Client::new();
    client
        .post(format!("{gateway_url}/v1/inbound"))
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

// ── Spawn (local) ────────────────────────────────────────────────────────────

/// Spawn a named sub-agent in the background and register it.
///
/// Writes `~/.that-agent/agents/<name>/config.toml`, starts the agent binary
/// (resolved via `current_exe`), and records the PID in the registry.
///
/// Returns the registry entry for the new agent.
pub async fn spawn_agent(
    name: &str,
    role: Option<&str>,
    parent: Option<&str>,
    gateway_port: Option<u16>,
    model: Option<&str>,
    agent_registry: &AgentRegistry,
) -> Result<AgentEntry> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home dir"))?;
    let agent_dir = home.join(".that-agent").join("agents").join(name);
    std::fs::create_dir_all(&agent_dir)?;

    // Write minimal config.toml.
    let config_toml = build_config_toml(role, parent, model, gateway_port);
    std::fs::write(agent_dir.join("config.toml"), &config_toml)?;

    // Start the agent binary.
    let binary = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(&binary);
    cmd.arg("--agent").arg(name).arg("run").arg("listen");
    if let Some(port) = gateway_port {
        cmd.env("THAT_GATEWAY_ADDR", format!("127.0.0.1:{port}"));
    }
    // Pass parent's gateway URL so the child can post notifications back.
    cmd.env(
        "THAT_PARENT_GATEWAY_URL",
        crate::orchestration::support::resolve_gateway_url(),
    );
    if let Ok(tok) = std::env::var("THAT_GATEWAY_TOKEN") {
        cmd.env("THAT_PARENT_GATEWAY_TOKEN", tok);
    }
    // Propagate hierarchy depth (root=0, persistent child=1, etc.)
    cmd.env(
        "THAT_AGENT_DEPTH",
        (crate::orchestration::config::parse_env_u8("THAT_AGENT_DEPTH", 0) + 1).to_string(),
    );
    // Detach: let the child outlive the parent.
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null());
    let child = cmd.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get child PID"))?;

    let gateway_url = gateway_port.map(|p| format!("http://localhost:{p}"));
    let entry = AgentEntry {
        name: name.to_string(),
        role: role.map(str::to_string),
        parent: parent.map(str::to_string),
        pid,
        gateway_url,
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    agent_registry.register(entry.clone())?;
    Ok(entry)
}

/// Query an agent's `/v1/chat` endpoint synchronously.
///
/// `gateway_url` must be the base URL (e.g. `http://localhost:8081`).
/// Returns the agent's response text.
pub async fn query_agent(gateway_url: &str, message: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gateway_url}/v1/chat"))
        .json(&serde_json::json!({ "message": message, "sender_id": "parent" }))
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    Ok(body["text"].as_str().unwrap_or_default().to_string())
}

/// SSE event received from a streaming agent query.
#[derive(Debug, Clone)]
pub enum AgentStreamEvent {
    ToolCall { name: String, args: String },
    ToolResult { name: String, result: String },
    Done { text: String },
    Error { message: String },
}

/// Query an agent's `/v1/chat/stream` SSE endpoint, sending events to `event_tx`.
///
/// Only relays `tool_call`, `tool_result`, and `done` events. Returns the final text.
pub async fn query_agent_stream(
    gateway_url: &str,
    agent_name: &str,
    message: &str,
    timeout_secs: u64,
    event_tx: tokio::sync::mpsc::UnboundedSender<AgentStreamEvent>,
) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{gateway_url}/v1/chat/stream"))
        .json(&serde_json::json!({ "message": message, "sender_id": "parent" }))
        .timeout(Duration::from_secs(timeout_secs))
        .send()
        .await?
        .error_for_status()?;

    let mut final_text = String::new();
    let mut got_terminal = false;
    let mut bytes = resp.bytes_stream();
    let mut buf = String::new();
    let mut current_event_type = String::new();

    use futures::StreamExt;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));

        // Parse SSE frames: "event: <type>\ndata: <json>\n\n"
        while let Some(boundary) = buf.find("\n\n") {
            let frame = buf[..boundary].to_string();
            buf = buf[boundary + 2..].to_string();

            for line in frame.lines() {
                if let Some(ev) = line.strip_prefix("event: ") {
                    current_event_type = ev.trim().to_string();
                } else if let Some(data) = line.strip_prefix("data: ") {
                    let parsed: serde_json::Value =
                        serde_json::from_str(data.trim()).unwrap_or_default();
                    match current_event_type.as_str() {
                        "tool_call" => {
                            let name = format!(
                                "{}/{}",
                                agent_name,
                                parsed["name"].as_str().unwrap_or("unknown")
                            );
                            let args = parsed["args"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::ToolCall { name, args });
                        }
                        "tool_result" => {
                            let name = format!(
                                "{}/{}",
                                agent_name,
                                parsed["name"].as_str().unwrap_or("unknown")
                            );
                            let result = parsed["result"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::ToolResult { name, result });
                        }
                        "done" => {
                            got_terminal = true;
                            final_text = parsed["text"].as_str().unwrap_or_default().to_string();
                            let _ = event_tx.send(AgentStreamEvent::Done {
                                text: final_text.clone(),
                            });
                        }
                        "error" => {
                            let err_msg = parsed["error"]
                                .as_str()
                                .unwrap_or("unknown error")
                                .to_string();
                            let _ = event_tx.send(AgentStreamEvent::Error {
                                message: err_msg.clone(),
                            });
                            return Err(anyhow::anyhow!(
                                "sub-agent '{agent_name}' error: {err_msg}"
                            ));
                        }
                        _ => {} // skip stream_token, etc.
                    }
                    current_event_type.clear();
                }
            }
        }
    }
    // Stream ended without a terminal event — sub-agent was likely aborted.
    if !got_terminal {
        let msg = format!("sub-agent '{agent_name}' stream ended without completion (likely aborted or hit tool limit)");
        let _ = event_tx.send(AgentStreamEvent::Error {
            message: msg.clone(),
        });
        return Err(anyhow::anyhow!(msg));
    }
    Ok(final_text)
}

/// Fire-and-forget: post a message to a sub-agent's `/v1/inbound` with a callback URL.
///
/// Returns immediately after the POST succeeds. The sub-agent will process
/// asynchronously and POST its result to `callback_url` when done.
pub async fn query_agent_async(
    gateway_url: &str,
    parent_name: &str,
    parent_gateway_url: &str,
    message: &str,
) -> Result<()> {
    let client = reqwest::Client::new();
    let callback = format!("{parent_gateway_url}/v1/notify");
    client
        .post(format!("{gateway_url}/v1/inbound"))
        .json(&serde_json::json!({
            "message": message,
            "sender_id": parent_name,
            "callback_url": callback,
        }))
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

// ── K8s mode detection ───────────────────────────────────────────────────────

/// Returns `true` when running in K8s sandbox mode.
pub fn is_k8s_mode() -> bool {
    matches!(
        std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    )
}

/// Resolve the container image for child agents.
/// Get the K8s namespace from env (POD_NAMESPACE) or default.
fn k8s_namespace() -> String {
    std::env::var("POD_NAMESPACE").unwrap_or_else(|_| "default".to_string())
}

fn sanitize_name(input: &str) -> String {
    crate::sandbox::kubernetes::sanitize_k8s_name(input)
}

// ── Helm chart reference ─────────────────────────────────────────────────────

/// Default OCI chart reference for child agent deployments.
/// Override with THAT_HELM_CHART env var for local charts or private registries.
const HELM_CHART_OCI_DEFAULT: &str = "oci://ghcr.io/that-labs/helm/that-agent";

fn helm_chart_ref() -> String {
    std::env::var("THAT_HELM_CHART").unwrap_or_else(|_| HELM_CHART_OCI_DEFAULT.to_string())
}

/// Run `helm upgrade --install` with the given --set args. Returns stdout.
async fn helm_install(release: &str, ns: &str, sets: &[String]) -> Result<String> {
    helm_run(release, ns, sets, true).await
}

/// Run `helm upgrade --install` without --wait (for async ephemeral Jobs).
async fn helm_install_nowait(release: &str, ns: &str, sets: &[String]) -> Result<String> {
    helm_run(release, ns, sets, false).await
}

async fn helm_run(release: &str, ns: &str, sets: &[String], wait: bool) -> Result<String> {
    let mut args = vec![
        "upgrade".to_string(),
        "--install".to_string(),
        release.to_string(),
        helm_chart_ref(),
        "--namespace".to_string(),
        ns.to_string(),
    ];
    // Pin chart version to match the running agent's version
    if let Ok(ver) = std::env::var("THAT_HELM_CHART_VERSION") {
        if !ver.is_empty() {
            args.push("--version".to_string());
            args.push(ver);
        }
    }
    if wait {
        args.extend([
            "--wait".to_string(),
            "--timeout".to_string(),
            "120s".to_string(),
        ]);
    }
    for s in sets {
        args.push("--set".to_string());
        args.push(s.clone());
    }
    let output = tokio::process::Command::new("helm")
        .args(&args)
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "helm install failed for '{}': {}",
        release,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `helm uninstall` for a release.
async fn helm_uninstall(release: &str, ns: &str) -> Result<()> {
    let output = tokio::process::Command::new("helm")
        .args(["uninstall", release, "--namespace", ns])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "helm uninstall failed for '{}': {}",
        release,
        String::from_utf8_lossy(&output.stderr)
    );
    // Clean up identity ConfigMap (created by parent, not managed by Helm)
    let _ = tokio::process::Command::new("kubectl")
        .args([
            "delete",
            "configmap",
            &format!("{release}-identity"),
            "--namespace",
            ns,
            "--ignore-not-found",
        ])
        .output()
        .await;
    Ok(())
}

/// Build common --set args for child agent Helm installs.
fn child_helm_sets(
    name: &str,
    role_type: &str,
    agent_role: &str,
    parent: &str,
    model: &str,
    identity_cm: Option<&str>,
) -> Vec<String> {
    let parent_gw = crate::orchestration::support::resolve_gateway_url();
    let gw_token = std::env::var("THAT_GATEWAY_TOKEN").unwrap_or_default();
    let provider = std::env::var("THAT_AGENT_PROVIDER").unwrap_or_default();
    let cpu_limit = std::env::var("THAT_AGENT_CHILD_CPU_LIMIT").unwrap_or_else(|_| "1".into());
    let mem_limit = std::env::var("THAT_AGENT_CHILD_MEMORY_LIMIT").unwrap_or_else(|_| "2Gi".into());

    let mut sets = vec![
        format!("agent.name={name}"),
        format!("agent.role={role_type}"),
        format!("agent.agentRole={}", agent_role.replace(',', "\\,")),
        format!("agent.parent={parent}"),
        format!("agent.parentGatewayUrl={parent_gw}"),
        format!("agent.parentGatewayToken={gw_token}"),
        format!("agent.provider={provider}"),
        format!("agent.model={model}"),
        format!("agent.resources.limits.cpu={cpu_limit}"),
        format!("agent.resources.limits.memory={mem_limit}"),
        "agent.resources.requests.cpu=200m".to_string(),
        "agent.resources.requests.memory=256Mi".to_string(),
        "secrets.existingSecret=that-agent-secrets".to_string(),
        "accessLevel=namespace-admin".to_string(),
        "gitServer.enabled=false".to_string(),
        "buildkit.enabled=false".to_string(),
        "cacheProxy.enabled=false".to_string(),
        "pdb.enabled=false".to_string(),
        "agent.storage.size=1Gi".to_string(),
    ];

    // Identity ConfigMap: mount config.toml + identity files into child
    if let Some(cm) = identity_cm {
        sets.push(format!("agent.identityConfigMap={cm}"));
    } else if !agent_role.is_empty() {
        // Fallback: pass role as bootstrap prompt when no identity ConfigMap
        sets.push(format!(
            "agent.bootstrapPrompt={}",
            agent_role.replace(',', "\\,")
        ));
    }

    // Forward the image so children use the same version as parent
    if let Ok(image) = std::env::var("THAT_AGENT_IMAGE") {
        if let Some((repo, tag)) = image.rsplit_once(':') {
            sets.push(format!("agent.image.repository={repo}"));
            sets.push(format!("agent.image.tag={tag}"));
        }
    }

    sets
}

// ── K8s Spawn — Persistent Agent ─────────────────────────────────────────────

/// Spawn a persistent agent via Helm (Deployment + Service).
///
/// Uses the same Helm chart as the root agent with `agent.role=child`.
pub async fn spawn_persistent_agent_k8s(
    name: &str,
    role: Option<&str>,
    parent: &str,
    model: Option<&str>,
    _env_overrides: Option<&std::collections::HashMap<String, String>>,
    db_path: &std::path::Path,
    identity_configmap: Option<&str>,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let release_name = format!("that-agent-{safe_name}");

    let model_str = model
        .map(crate::model_catalog::normalize_model)
        .or_else(|| std::env::var("THAT_AGENT_MODEL").ok())
        .unwrap_or_default();

    let sets = child_helm_sets(
        name,
        "child",
        role.unwrap_or(""),
        parent,
        &model_str,
        identity_configmap,
    );
    helm_install(&release_name, &ns, &sets).await?;

    let gateway_url = format!("http://{release_name}.{ns}.svc.cluster.local:8080");

    // Register in the local agent registry so agent_task/agent_query can resolve it.
    if let Some(cluster_dir) = cluster_dir_from_db(db_path) {
        let reg = AgentRegistry::new(cluster_dir.join("agents.json"));
        let _ = reg.register(AgentEntry {
            name: name.to_string(),
            role: role.map(str::to_string),
            parent: Some(parent.to_string()),
            pid: 0,
            gateway_url: Some(gateway_url.clone()),
            started_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    Ok(serde_json::json!({
        "name": name,
        "type": "persistent",
        "gateway_url": gateway_url,
    }))
}

// ── K8s Spawn — Ephemeral Agent (agent_run) ──────────────────────────────────

/// Run an ephemeral task agent as a Helm release (K8s Job). Async non-blocking.
///
/// The parent monitors the Job via kubectl and reports progress.
#[allow(clippy::too_many_arguments)]
pub async fn run_ephemeral_agent_k8s(
    name: &str,
    role: Option<&str>,
    task: &str,
    parent: &str,
    model: Option<&str>,
    workspace: bool,
    timeout_secs: u64,
    _bootstrap: Option<&crate::workspace::GoldBootstrap>,
    identity_configmap: Option<&str>,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let release_name = format!("that-agent-{safe_name}");
    let git_svc = git_server_url(&ns);

    let model_str = model
        .map(crate::model_catalog::normalize_model)
        .or_else(|| std::env::var("THAT_AGENT_MODEL").ok())
        .unwrap_or_default();

    if workspace {
        let check = reqwest::Client::new()
            .get(format!("{git_svc}/api/repos/workspace/activity"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        match check {
            Ok(resp) if resp.status().is_success() => {}
            _ => {
                anyhow::bail!(
                    "workspace=true but no workspace repo found on the git server. \
                     Call workspace_share(path) first to push your repo."
                );
            }
        }
    }

    let mut sets = child_helm_sets(
        name,
        "ephemeral",
        role.unwrap_or(""),
        parent,
        &model_str,
        identity_configmap,
    );

    // Ephemeral-specific settings
    // Escape commas/special chars in task text by using a file-based approach
    // For now, truncate task to avoid shell escaping issues with --set
    let safe_task = task
        .chars()
        .filter(|c| !c.is_control())
        .take(4000)
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace(',', "\\,");
    sets.push(format!("agent.job.task={safe_task}"));
    sets.push("agent.job.ttlSeconds=300".to_string());
    sets.push("networkPolicy.enabled=false".to_string());

    if workspace {
        sets.push("agent.job.workspace=true".to_string());
        sets.push(format!("agent.job.gitRepoUrl={git_svc}/workspace.git"));
        sets.push(format!("agent.job.gitBranch=task/{safe_name}"));
    }

    // Don't wait — ephemeral jobs are async, parent monitors
    helm_install_nowait(&release_name, &ns, &sets).await?;

    // Monitor the Job until completion or timeout
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let job_name = release_name.clone();
    let mut last_log_check = std::time::Instant::now() - Duration::from_secs(30);
    let mut last_log_line = String::new();

    loop {
        if start.elapsed() > timeout {
            let final_logs = tail_job_logs(&job_name, &ns, 20).await;
            let _ = helm_uninstall(&release_name, &ns).await;
            anyhow::bail!("agent_run timed out after {timeout_secs}s. Last output:\n{final_logs}");
        }

        let output = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "job",
                &job_name,
                "-n",
                &ns,
                "-o",
                "jsonpath={.status.conditions[0].type} {.status.conditions[0].message}",
            ])
            .output()
            .await?;

        let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if status_str.starts_with("Complete") {
            break;
        }
        if status_str.starts_with("Failed") {
            let logs = tail_job_logs(&job_name, &ns, 30).await;
            anyhow::bail!("agent job failed: {status_str}\nLast output:\n{logs}");
        }

        let active_out = tokio::process::Command::new("kubectl")
            .args([
                "get",
                "job",
                &job_name,
                "-n",
                &ns,
                "-o",
                "jsonpath={.status.succeeded} {.status.failed}",
            ])
            .output()
            .await?;
        let active_str = String::from_utf8_lossy(&active_out.stdout)
            .trim()
            .to_string();
        let parts: Vec<&str> = active_str.split_whitespace().collect();
        if parts.first().map(|s| *s == "1").unwrap_or(false) {
            break;
        }
        if parts
            .get(1)
            .map(|s| *s != "0" && !s.is_empty())
            .unwrap_or(false)
        {
            let logs = tail_job_logs(&job_name, &ns, 30).await;
            anyhow::bail!("agent job failed\nLast output:\n{logs}");
        }

        if last_log_check.elapsed() >= Duration::from_secs(30) {
            last_log_check = std::time::Instant::now();
            let latest = tail_job_logs(&job_name, &ns, 3).await;
            if !latest.is_empty() && latest != last_log_line {
                last_log_line = latest.clone();
                let elapsed = start.elapsed().as_secs();
                let turn_info = latest
                    .lines()
                    .rev()
                    .find_map(|l| {
                        l.find("turn=").and_then(|i| {
                            let rest = &l[i..];
                            let turn = rest.strip_prefix("turn=")?.split_whitespace().next()?;
                            let max = rest.find("max_turns=").and_then(|j| {
                                rest[j..]
                                    .strip_prefix("max_turns=")?
                                    .split_whitespace()
                                    .next()
                            })?;
                            Some(format!("turn {turn}/{max}"))
                        })
                    })
                    .unwrap_or_else(|| "working...".to_string());
                let msg = format!("[{name}] {turn_info} ({elapsed}s)");
                tracing::info!(agent = %name, "{msg}");
                let gw = crate::orchestration::support::resolve_gateway_url();
                let _ = reqwest::Client::new()
                    .post(format!("{gw}/v1/notify"))
                    .json(&serde_json::json!({
                        "message": msg,
                        "agent": name,
                    }))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await;
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    let output_text = tail_job_logs(&job_name, &ns, 200).await;
    let elapsed = start.elapsed().as_secs();

    Ok(serde_json::json!({
        "name": name,
        "status": "succeeded",
        "output": output_text,
        "elapsed_secs": elapsed,
    }))
}

// ── K8s Agent List ───────────────────────────────────────────────────────────

/// List all managed agents in K8s namespace (Deployments + Jobs).
pub async fn list_agents_k8s() -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let output = tokio::process::Command::new("kubectl")
        .args([
            "get",
            "deployments,jobs",
            "-n",
            &ns,
            "-l",
            "that-agent/managed=true",
            "-o",
            "json",
        ])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "kubectl get failed");

    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let items = parsed["items"].as_array().cloned().unwrap_or_default();

    let agents: Vec<serde_json::Value> = items
        .iter()
        .filter_map(|item| {
            let labels = item["metadata"]["labels"].as_object()?;
            let name = labels.get("that-agent/name")?.as_str()?;
            let parent = labels
                .get("that-agent/parent")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let role = labels
                .get("that-agent/role")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let agent_type = labels
                .get("that-agent/type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let kind = item["kind"].as_str().unwrap_or("");
            let (alive, gateway_url, status) = if kind == "Deployment" {
                let ready = item["status"]["readyReplicas"].as_u64().unwrap_or(0) >= 1;
                let safe = sanitize_name(name);
                let gw = format!("http://that-agent-{safe}.{ns}.svc.cluster.local:8080");
                (ready, Some(gw), if ready { "running" } else { "pending" })
            } else {
                // Job
                let succeeded = item["status"]["succeeded"].as_u64().unwrap_or(0) >= 1;
                let failed = item["status"]["failed"].as_u64().unwrap_or(0) >= 1;
                let status = if succeeded {
                    "succeeded"
                } else if failed {
                    "failed"
                } else {
                    "active"
                };
                (false, None, status)
            };

            Some(serde_json::json!({
                "name": name,
                "parent": parent,
                "role": role,
                "type": agent_type,
                "kind": kind,
                "alive": alive,
                "status": status,
                "gateway_url": gateway_url,
            }))
        })
        .collect();

    Ok(serde_json::json!({ "agents": agents }))
}

// ── K8s Agent Query ──────────────────────────────────────────────────────────

/// Query a persistent agent by resolving its K8s Service DNS.
/// If the target is the parent agent, use THAT_PARENT_GATEWAY_URL directly.
/// Otherwise construct a cross-namespace DNS name using the parent's namespace
/// (siblings are deployed in the same namespace as the parent).
pub async fn query_agent_k8s(
    name: &str,
    message: &str,
    timeout_secs: u64,
) -> Result<serde_json::Value> {
    let safe_name = sanitize_name(name);
    let parent_name = std::env::var("THAT_AGENT_PARENT").unwrap_or_default();

    let gateway_url = if !parent_name.is_empty() && sanitize_name(&parent_name) == safe_name {
        // Querying our parent — use the known gateway URL
        std::env::var("THAT_PARENT_GATEWAY_URL").unwrap_or_else(|_| {
            let ns = k8s_namespace();
            format!("http://that-agent-{safe_name}.{ns}.svc.cluster.local:8080")
        })
    } else {
        // Querying a sibling or other agent — use the parent's namespace (where agents live)
        let ns = std::env::var("THAT_SANDBOX_K8S_NAMESPACE").unwrap_or_else(|_| k8s_namespace());
        format!("http://that-agent-{safe_name}.{ns}.svc.cluster.local:8080")
    };

    // Retry with backoff for DNS propagation
    let mut last_err = None;
    for delay in [2, 4, 8] {
        match query_agent(&gateway_url, message, timeout_secs).await {
            Ok(resp) => {
                return Ok(serde_json::json!({
                    "agent": name,
                    "response": resp,
                }))
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("query failed")))
}

// ── K8s Unregister ───────────────────────────────────────────────────────────

/// Remove a child agent by uninstalling its Helm release.
pub async fn unregister_agent_k8s(name: &str) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let release_name = format!("that-agent-{safe_name}");
    helm_uninstall(&release_name, &ns).await?;
    Ok(serde_json::json!({ "name": name, "status": "unregistered" }))
}

/// Stop a child agent — delete its Job/Deployment and all associated resources.
/// Equivalent to unregister but returns "stopped" status for semantic clarity.
pub async fn agent_stop_k8s(name: &str) -> Result<serde_json::Value> {
    unregister_agent_k8s(name).await.map(|mut v| {
        v["status"] = serde_json::json!("stopped");
        v
    })
}

/// Get detailed status of a child agent — Job/Deployment state, pod phase, start time.
pub async fn agent_status_k8s(name: &str) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");

    // Try Job first (ephemeral), then Deployment (persistent)
    let job_out = tokio::process::Command::new("kubectl")
        .args([
            "get", "job", &sa_name, "-n", &ns, "-o",
            "jsonpath={.status.conditions[0].type},{.status.conditions[0].status},{.status.startTime},{.status.succeeded},{.status.failed},{.status.active}",
        ])
        .output()
        .await?;

    if job_out.status.success() {
        let raw = String::from_utf8_lossy(&job_out.stdout);
        let parts: Vec<&str> = raw.split(',').collect();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Job",
            "condition": parts.first().unwrap_or(&""),
            "condition_status": parts.get(1).unwrap_or(&""),
            "start_time": parts.get(2).unwrap_or(&""),
            "succeeded": parts.get(3).unwrap_or(&"0"),
            "failed": parts.get(4).unwrap_or(&"0"),
            "active": parts.get(5).unwrap_or(&"0"),
        }));
    }

    // Try Deployment
    let deploy_out = tokio::process::Command::new("kubectl")
        .args([
            "get", "deployment", &sa_name, "-n", &ns, "-o",
            "jsonpath={.status.readyReplicas},{.status.replicas},{.status.updatedReplicas},{.metadata.creationTimestamp}",
        ])
        .output()
        .await?;

    if deploy_out.status.success() {
        let raw = String::from_utf8_lossy(&deploy_out.stdout);
        let parts: Vec<&str> = raw.split(',').collect();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Deployment",
            "ready": parts.first().unwrap_or(&"0"),
            "replicas": parts.get(1).unwrap_or(&"0"),
            "updated": parts.get(2).unwrap_or(&"0"),
            "created": parts.get(3).unwrap_or(&""),
        }));
    }

    anyhow::bail!("agent '{name}' not found as Job or Deployment in {ns}")
}

/// Get recent logs from a child agent's pod.
pub async fn agent_logs_k8s(name: &str, tail: u32) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let safe_name = sanitize_name(name);
    let sa_name = format!("that-agent-{safe_name}");

    let output = tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("job/{sa_name}"),
            "-n",
            &ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await?;

    if output.status.success() {
        let logs = String::from_utf8_lossy(&output.stdout).to_string();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Job",
            "lines": tail,
            "logs": logs,
        }));
    }

    // Fallback: try deployment pods
    let output = tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("deployment/{sa_name}"),
            "-n",
            &ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await?;

    if output.status.success() {
        let logs = String::from_utf8_lossy(&output.stdout).to_string();
        return Ok(serde_json::json!({
            "name": name,
            "kind": "Deployment",
            "lines": tail,
            "logs": logs,
        }));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("cannot get logs for agent '{name}': {stderr}")
}

/// Delete all ephemeral child Jobs for the current agent.
/// Used by /stop to clean up running workers when the parent run is cancelled.
pub async fn cleanup_ephemeral_children() -> Result<()> {
    let ns = k8s_namespace();
    let parent_name = std::env::var("THAT_AGENT_NAME").unwrap_or_else(|_| "default".into());
    let safe_parent = sanitize_name(&parent_name);
    tracing::info!("cleaning up ephemeral children of {safe_parent} in {ns}");
    let output = tokio::process::Command::new("kubectl")
        .args([
            "delete",
            "job,configmap,serviceaccount,rolebinding",
            "-l",
            &format!("that-agent/parent={safe_parent},that-agent/type=ephemeral"),
            "-n",
            &ns,
            "--ignore-not-found",
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("cleanup_ephemeral_children: {stderr}");
    }
    Ok(())
}

// ── Workspace sharing ────────────────────────────────────────────────────────

/// Push a local git repo to the in-cluster git server for child access.
pub async fn workspace_share(path: &str, repo_name: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);

    // Validate it's a git repo
    let check = tokio::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--git-dir"])
        .output()
        .await?;
    anyhow::ensure!(
        check.status.success(),
        "path is not a git repository: {path}"
    );

    let name = repo_name.unwrap_or_else(|| {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace")
    });

    let repo_url = format!("{git_svc}/{name}.git");

    // Push current state to git-server (auto-inits bare repo on first access)
    let push = tokio::process::Command::new("git")
        .args(["-C", path, "push", &repo_url, "HEAD:main", "--force"])
        .output()
        .await?;
    let push_stderr = String::from_utf8_lossy(&push.stderr);
    anyhow::ensure!(
        push.status.success(),
        "git push to git-server failed: {push_stderr}"
    );

    // Register webhook so the git server notifies us on worker pushes
    let parent_gw = crate::orchestration::support::resolve_gateway_url();
    let notify_url = format!("{parent_gw}/v1/notify");
    let _ = reqwest::Client::new()
        .post(format!("{git_svc}/api/repos/{name}/webhook"))
        .json(&serde_json::json!({ "url": notify_url }))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    Ok(serde_json::json!({
        "name": name,
        "clone_url": repo_url,
        "webhook": notify_url,
    }))
}

/// Merge or review a worker's code changes back into local workspace.
pub async fn workspace_collect(
    path: &str,
    worker: Option<&str>,
    branch: Option<&str>,
    task_id: Option<&str>,
    strategy: &str,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_url = format!("{git_svc}/workspace.git");
    let branch = resolve_workspace_branch(worker, branch, task_id, Some("workspace")).await?;
    let worker_label = worker
        .map(str::to_string)
        .or_else(|| task_branch_worker(&branch).map(str::to_string))
        .unwrap_or_else(|| branch.clone());

    // Fetch the worker's branch
    let fetch = tokio::process::Command::new("git")
        .args(["-C", path, "fetch", &repo_url, &branch])
        .output()
        .await?;
    anyhow::ensure!(fetch.status.success(), "git fetch failed");

    if strategy == "review" {
        let diff = tokio::process::Command::new("git")
            .args(["-C", path, "diff", "HEAD...FETCH_HEAD"])
            .output()
            .await?;
        let diff_text = String::from_utf8_lossy(&diff.stdout).to_string();
        return Ok(serde_json::json!({
            "strategy": "review",
            "diff": diff_text,
        }));
    }

    // Merge
    let merge = tokio::process::Command::new("git")
        .args([
            "-C",
            path,
            "merge",
            "FETCH_HEAD",
            "--no-ff",
            "-m",
            &format!("Merge worker {worker_label} results"),
        ])
        .output()
        .await?;

    if merge.status.success() {
        // Count merged commits
        let log = tokio::process::Command::new("git")
            .args(["-C", path, "log", "--oneline", "HEAD...FETCH_HEAD"])
            .output()
            .await?;
        let commit_count = String::from_utf8_lossy(&log.stdout).lines().count();

        // Clean up task branch
        let _ = tokio::process::Command::new("git")
            .args(["-C", path, "push", &repo_url, "--delete", &branch])
            .output()
            .await;

        Ok(serde_json::json!({
            "strategy": "merge",
            "merged": true,
            "commits": commit_count,
            "conflicts": [],
        }))
    } else {
        let stderr = String::from_utf8_lossy(&merge.stderr).to_string();
        // Abort the failed merge so the working tree is clean
        let _ = tokio::process::Command::new("git")
            .args(["-C", path, "merge", "--abort"])
            .status()
            .await;
        // Try to fetch conflict details from the git server REST API
        let conflicts = async {
            reqwest::Client::new()
                .get(format!("{git_svc}/api/repos/workspace/conflicts/{branch}"))
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .ok()?
                .json::<serde_json::Value>()
                .await
                .ok()
        }
        .await;
        let conflicting_files = conflicts
            .as_ref()
            .and_then(|c| c.get("conflicting_files"))
            .cloned()
            .unwrap_or(serde_json::json!([]));
        Ok(serde_json::json!({
            "strategy": "merge",
            "merged": false,
            "error": stderr,
            "conflicting_files": conflicting_files,
            "hint": "Use agent_query to ask the worker to rebase against main and resolve conflicts in the listed files",
        }))
    }
}

// ── Git Server REST Wrappers ─────────────────────────────────────────────────

/// Query the git server for branch activity on a repo (branches, ahead/behind, last commit).
pub async fn workspace_activity(repo: Option<&str>) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let url = format!("{git_svc}/api/repos/{repo_name}/activity");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

#[derive(serde::Deserialize)]
struct WorkspaceActivityBranch {
    name: String,
}

#[derive(serde::Deserialize)]
struct WorkspaceActivityResponse {
    branches: Vec<WorkspaceActivityBranch>,
}

pub async fn resolve_workspace_worker_branch(worker: &str, repo: Option<&str>) -> Result<String> {
    resolve_workspace_branch(Some(worker), None, None, repo).await
}

fn task_branch_worker(branch: &str) -> Option<&str> {
    let mut parts = branch.split('/');
    match (parts.next(), parts.next()) {
        (Some("task"), Some(worker)) if !worker.trim().is_empty() => Some(worker),
        _ => None,
    }
}

fn task_branch_task_id(branch: &str) -> Option<&str> {
    let mut parts = branch.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("task"), Some(_worker), Some(task_id), None) if !task_id.trim().is_empty() => {
            Some(task_id)
        }
        _ => None,
    }
}

fn select_workspace_branch(
    branches: &[String],
    worker: Option<&str>,
    branch: Option<&str>,
    task_id: Option<&str>,
) -> Result<String> {
    if let Some(branch) = branch
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .map(str::to_string)
    {
        return Ok(branch);
    }
    if let Some(task_id) = task_id.map(str::trim).filter(|task_id| !task_id.is_empty()) {
        let expected_worker = worker.map(sanitize_name);
        let matches: Vec<_> = branches
            .iter()
            .filter(|branch| {
                task_branch_task_id(branch.as_str()) == Some(task_id)
                    && expected_worker
                        .as_deref()
                        .map(|worker| task_branch_worker(branch.as_str()) == Some(worker))
                        .unwrap_or(true)
            })
            .cloned()
            .collect();
        return match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(anyhow::anyhow!(
                "no workspace branch found for task_id '{task_id}'"
            )),
            _ => Err(anyhow::anyhow!(
                "multiple workspace branches found for task_id '{task_id}'; specify branch explicitly"
            )),
        };
    }
    let worker = worker
        .map(str::trim)
        .filter(|worker| !worker.is_empty())
        .ok_or_else(|| anyhow::anyhow!("worker, branch, or task_id is required"))?;
    let safe_worker = sanitize_name(worker);
    let exact = format!("task/{safe_worker}");
    let prefix = format!("{exact}/");
    if branches.iter().any(|branch| branch == &exact) {
        return Ok(exact);
    }
    let mut matches = branches
        .iter()
        .filter(|branch| branch.starts_with(&prefix))
        .cloned()
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(anyhow::anyhow!("no workspace branch found for worker '{worker}'")),
        _ => Err(anyhow::anyhow!(
            "multiple workspace branches found for worker '{worker}'; inspect workspace activity and choose a branch explicitly"
        )),
    }
}

pub async fn resolve_workspace_branch(
    worker: Option<&str>,
    branch: Option<&str>,
    task_id: Option<&str>,
    repo: Option<&str>,
) -> Result<String> {
    let activity = workspace_activity(repo).await?;
    let parsed: WorkspaceActivityResponse = serde_json::from_value(activity)?;
    let branches = parsed
        .branches
        .into_iter()
        .map(|branch| branch.name)
        .collect::<Vec<_>>();
    select_workspace_branch(&branches, worker, branch, task_id)
}

/// Get a unified diff of a workspace branch vs main, without cloning.
pub async fn workspace_branch_diff(
    worker: Option<&str>,
    branch: Option<&str>,
    task_id: Option<&str>,
    repo: Option<&str>,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let branch = resolve_workspace_branch(worker, branch, task_id, Some(repo_name)).await?;
    let url = format!("{git_svc}/api/repos/{repo_name}/diff/{branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        Ok(serde_json::json!({ "branch": branch, "diff": body }))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

/// Analyze merge conflicts between a worker's branch and main.
pub async fn workspace_conflicts(
    worker: Option<&str>,
    branch: Option<&str>,
    task_id: Option<&str>,
    repo: Option<&str>,
) -> Result<serde_json::Value> {
    let ns = k8s_namespace();
    let git_svc = git_server_url(&ns);
    let repo_name = repo.unwrap_or("workspace");
    let branch = resolve_workspace_branch(worker, branch, task_id, Some(repo_name)).await?;
    let url = format!("{git_svc}/api/repos/{repo_name}/conflicts/{branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("git server unreachable: {e}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("bad json: {e}"))
    } else {
        anyhow::bail!("git server {status}: {body}")
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn build_config_toml(
    role: Option<&str>,
    parent: Option<&str>,
    model: Option<&str>,
    gateway_port: Option<u16>,
) -> String {
    let mut out = String::new();
    if let Some(m) = model {
        out.push_str(&format!("model = \"{m}\"\n"));
    }
    if let Some(r) = role {
        out.push_str(&format!("role = \"{r}\"\n"));
    }
    if let Some(p) = parent {
        out.push_str(&format!("parent = \"{p}\"\n"));
    }
    if let Some(port) = gateway_port {
        out.push_str(&format!(
            "\n[[channels.adapters]]\ntype = \"http\"\nbind_addr = \"127.0.0.1:{port}\"\n"
        ));
    }
    out
}

/// Resolve the git-server Service URL (independent pod).
fn git_server_url(ns: &str) -> String {
    std::env::var("THAT_GIT_SERVER_URL")
        .unwrap_or_else(|_| format!("http://that-agent-git-server.{ns}.svc.cluster.local:9418"))
}

/// Derive the cluster directory from the memory DB path.
///
/// `memory_db_path` has the form `~/.that-agent/agents/<name>/memory.db`.
/// Walking up 3 levels gives `~/.that-agent/`; we append `cluster/`.
pub fn cluster_dir_from_db(memory_db_path: &Path) -> Option<PathBuf> {
    memory_db_path.ancestors().nth(3).map(|p| p.join("cluster"))
}

/// Tail the last N log lines from a K8s Job pod.
async fn tail_job_logs(job_name: &str, ns: &str, tail: u32) -> String {
    tokio::process::Command::new("kubectl")
        .args([
            "logs",
            &format!("job/{job_name}"),
            "-n",
            ns,
            &format!("--tail={tail}"),
        ])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> AgentTaskRegistry {
        let dir =
            std::env::temp_dir().join(format!("that-agent-task-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        AgentTaskRegistry::new(dir.join("agent_tasks.json"))
    }

    #[test]
    fn scratchpad_header_replaces_by_kind_without_duplicate_revision() {
        let reg = temp_registry();
        let task = reg.create("worker", "finish the task", "parent").unwrap();

        reg.scratchpad_set_header(&task.id, "parent", "goal", "Overall shared goal:\nA")
            .unwrap();
        reg.scratchpad_set_header(&task.id, "parent", "goal", "Overall shared goal:\nA")
            .unwrap();
        reg.scratchpad_set_header(&task.id, "parent", "goal", "Overall shared goal:\nB")
            .unwrap();

        let task = reg.get(&task.id).unwrap().unwrap();
        assert_eq!(task.scratchpad_header.len(), 1);
        assert_eq!(task.scratchpad_header[0].kind, "goal");
        assert_eq!(task.scratchpad_header[0].note, "Overall shared goal:\nB");
        assert_eq!(task.scratchpad_revision, 2);
    }

    #[test]
    fn scratchpad_activity_tracks_kind_and_participants() {
        let reg = temp_registry();
        let task = reg.create("worker", "finish the task", "parent").unwrap();

        reg.scratchpad_append_kind(
            &task.id,
            "peer",
            "Need feedback on the diff",
            Some("review"),
        )
        .unwrap();

        let task = reg.get(&task.id).unwrap().unwrap();
        assert_eq!(task.scratchpad.len(), 1);
        assert_eq!(task.scratchpad[0].kind, "review");
        assert!(task.participants.iter().any(|p| p == "peer"));
        assert_eq!(task.scratchpad_revision, 1);
    }

    #[test]
    fn normalize_legacy_task_backfills_owner_participants_and_revision() {
        let reg = temp_registry();
        let legacy = serde_json::json!([{
            "id": "task-1",
            "agent": "worker",
            "state": "submitted",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "result": null,
            "messages": [{
                "from": "dispatcher",
                "text": "legacy task",
                "timestamp": "2026-01-01T00:00:00Z"
            }],
            "scratchpad": [{
                "from": "worker",
                "note": "old note",
                "timestamp": "2026-01-01T00:00:01Z"
            }]
        }]);
        std::fs::write(reg.path.clone(), serde_json::to_vec(&legacy).unwrap()).unwrap();

        let task = reg.get("task-1").unwrap().unwrap();
        assert_eq!(task.owner, "dispatcher");
        assert!(task.participants.iter().any(|p| p == "dispatcher"));
        assert!(task.participants.iter().any(|p| p == "worker"));
        assert_eq!(task.scratchpad_revision, 1);
        assert_eq!(task.scratchpad[0].kind, "");
    }

    #[test]
    fn journal_replays_task_state_without_snapshot_file() {
        let reg = temp_registry();
        let task = reg.create("worker", "finish the task", "parent").unwrap();
        reg.scratchpad_set_header(&task.id, "parent", "goal", "Overall shared goal:\nShip")
            .unwrap();
        reg.scratchpad_append_kind(&task.id, "worker", "Pushed commit abc123", Some("commit"))
            .unwrap();
        std::fs::remove_file(&reg.path).unwrap();

        let task = reg.get(&task.id).unwrap().unwrap();
        assert_eq!(task.scratchpad_header.len(), 1);
        assert_eq!(task.scratchpad_header[0].kind, "goal");
        assert_eq!(task.scratchpad.len(), 1);
        assert_eq!(task.scratchpad[0].kind, "commit");
        assert_eq!(task.scratchpad_revision, 2);
    }

    #[test]
    fn select_workspace_branch_prefers_explicit_branch() {
        let branches = vec!["task/dev/task-1".to_string()];
        assert_eq!(
            select_workspace_branch(&branches, Some("dev"), Some("task/custom"), Some("task-1"))
                .unwrap(),
            "task/custom"
        );
    }

    #[test]
    fn select_workspace_branch_uses_task_id_deterministically() {
        let branches = vec!["task/dev/task-1".to_string(), "task/dev/task-2".to_string()];
        assert_eq!(
            select_workspace_branch(&branches, None, None, Some("task-2")).unwrap(),
            "task/dev/task-2"
        );
    }

    #[test]
    fn select_workspace_branch_rejects_ambiguous_task_id() {
        let branches = vec![
            "task/dev/task-1".to_string(),
            "task/reviewer/task-1".to_string(),
        ];
        assert!(select_workspace_branch(&branches, None, None, Some("task-1")).is_err());
    }

    #[test]
    fn select_workspace_branch_uses_unique_worker_subbranch_fallback() {
        let branches = vec!["task/dev/task-1".to_string()];
        assert_eq!(
            select_workspace_branch(&branches, Some("dev"), None, None).unwrap(),
            "task/dev/task-1"
        );
    }

    #[test]
    fn concurrent_scratchpad_appends_do_not_drop_entries() {
        let reg = temp_registry();
        let path = reg.path.clone();
        let task = reg.create("worker", "finish the task", "parent").unwrap();

        std::thread::scope(|scope| {
            for worker_id in 0..8 {
                let path = path.clone();
                let task_id = task.id.clone();
                scope.spawn(move || {
                    let reg = AgentTaskRegistry::new(path);
                    for note_id in 0..5 {
                        reg.scratchpad_append_kind(
                            &task_id,
                            &format!("worker-{worker_id}"),
                            &format!("note-{note_id}"),
                            Some("progress"),
                        )
                        .unwrap();
                    }
                });
            }
        });

        let task = reg.get(&task.id).unwrap().unwrap();
        assert_eq!(task.scratchpad.len(), 40);
        assert_eq!(task.scratchpad_revision, 40);
    }
}
