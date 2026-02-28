//! Agent lifecycle management — spawn, list, and query peer agents.
//!
//! Agents registered here persist across sessions via a file-backed registry
//! at `~/.that-agent/cluster/agents.json`. The spawning agent writes a
//! `config.toml` for the child and starts the binary in the background.

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
        that_channels::atomic_write_json(&self.path, entries)
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

// ── Spawn ─────────────────────────────────────────────────────────────────────

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
    cmd.arg("--agent").arg(name).arg("run");
    if let Some(port) = gateway_port {
        cmd.env("THAT_GATEWAY_ADDR", format!("127.0.0.1:{port}"));
    }
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

/// Derive the cluster directory from the memory DB path.
///
/// `memory_db_path` has the form `~/.that-agent/agents/<name>/memory.db`.
/// Walking up 3 levels gives `~/.that-agent/`; we append `cluster/`.
pub fn cluster_dir_from_db(memory_db_path: &Path) -> Option<PathBuf> {
    memory_db_path.ancestors().nth(3).map(|p| p.join("cluster"))
}
