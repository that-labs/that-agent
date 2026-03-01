use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use that_channels::config::ChannelConfig;

// ── Default value helpers ───────────────────────────────────────────

fn default_provider() -> String {
    "openai".into()
}
fn default_model() -> String {
    "gpt-5.2-codex".into()
}
fn default_max_turns() -> usize {
    75
}
fn default_max_tokens() -> u64 {
    16384
}
fn default_agent_name() -> String {
    "default".into()
}
fn default_heartbeat_interval() -> Option<u64> {
    Some(10)
}

// ── AgentDef — self-contained agent definition ──────────────────────

/// A self-contained agent definition loaded from its own TOML file.
/// Combines LLM settings and workspace configuration into one unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    /// Agent name (set programmatically, not from TOML).
    #[serde(skip)]
    pub name: String,

    /// LLM provider (e.g. "openai", "anthropic").
    #[serde(default = "default_provider")]
    pub provider: String,

    /// Model identifier.
    #[serde(default = "default_model")]
    pub model: String,

    /// Maximum multi-turn iterations per run.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,

    /// Maximum tokens for the LLM response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,

    /// System preamble injected into every run.
    #[serde(default)]
    pub preamble: Option<String>,

    /// When false (default), the agent gets its own isolated workspace.
    /// When true, it shares the global workspace from WorkspaceConfig.
    #[serde(default)]
    pub shared_workspace: bool,

    /// Per-agent channel configuration (TUI, Telegram, Discord, WhatsApp, …).
    ///
    /// Defined directly in the agent's config file (`~/.that-agent/agents/<name>/config.toml`),
    /// making it accessible from inside the sandbox container at
    /// `/home/agent/.that-agent/agents/<name>/config.toml`. The agent can read and edit
    /// this file using its file tools.
    #[serde(default)]
    pub channels: ChannelConfig,

    /// Heartbeat monitor poll interval in seconds (default: 10).
    ///
    /// The background heartbeat task wakes every this many seconds to check for
    /// due entries in Heartbeat.md. Set to a lower value for more responsive
    /// urgent-priority processing.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: Option<u64>,

    /// IANA timezone for wall-clock schedules (daily, cron). Defaults to OS local time.
    #[serde(default)]
    pub timezone: Option<String>,

    /// Name of the parent agent that spawned this agent (if any).
    #[serde(default)]
    pub parent: Option<String>,

    /// Role assigned by the orchestrating parent (e.g. "explorer", "developer", "reviewer").
    #[serde(default)]
    pub role: Option<String>,

    /// When true, inherit the parent agent's workspace directory instead of creating an isolated one.
    #[serde(default)]
    pub inherit_workspace: bool,
}

impl Default for AgentDef {
    fn default() -> Self {
        Self {
            name: default_agent_name(),
            provider: default_provider(),
            model: default_model(),
            max_turns: default_max_turns(),
            max_tokens: default_max_tokens(),
            preamble: None,
            shared_workspace: false,
            channels: ChannelConfig::default(),
            heartbeat_interval: default_heartbeat_interval(),
            timezone: None,
            parent: None,
            role: None,
            inherit_workspace: false,
        }
    }
}

impl AgentDef {
    /// Load an agent definition from a TOML file.
    ///
    /// Env var precedence:
    /// - **LLM fields** (model, provider, max_turns): env vars are *fallback defaults*
    ///   that only apply when the TOML file does not explicitly set the field.
    ///   This lets the agent change its own model via config.toml edits.
    /// - **Hierarchy fields** (parent, role, inherit_workspace): env vars *always override*
    ///   because they are set by the parent agent or deployment orchestrator.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read agent file {}: {e}", path.display()))?;
        // Parse once as a raw value to detect which keys were explicitly set in the file.
        let raw: toml::Value = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse agent file {}: {e}", path.display()))?;
        let mut agent: AgentDef = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Failed to parse agent file {}: {e}", path.display()))?;
        // Expand ${ENV_VAR} placeholders in channel adapter fields.
        agent.channels.resolve_env_vars();
        // Apply environment variable overrides with correct precedence.
        agent.apply_env_overrides(Some(&raw));
        Ok(agent)
    }

    /// Apply environment variable overrides with two-tier precedence.
    ///
    /// **Fallback defaults** (model, provider, max_turns, max_tokens): env vars apply
    /// only when the config file did not explicitly set the field. When `raw` is `None`
    /// (no config file exists), all env vars apply unconditionally.
    ///
    /// **Hard overrides** (parent, role, inherit_workspace): env vars always win because
    /// they are set by the parent agent or deployment orchestrator at spawn time.
    pub fn apply_env_overrides(&mut self, raw: Option<&toml::Value>) {
        let file_has = |key: &str| raw.and_then(|v| v.get(key)).is_some();

        // ── Fallback defaults: env vars fill in what the config file didn't set ──
        if !file_has("provider") {
            if let Ok(v) = std::env::var("THAT_AGENT_PROVIDER") {
                if !v.is_empty() {
                    self.provider = v;
                }
            }
        }
        if !file_has("model") {
            if let Ok(v) = std::env::var("THAT_AGENT_MODEL") {
                if !v.is_empty() {
                    self.model = v;
                }
            }
        }
        if !file_has("max_turns") {
            if let Ok(v) = std::env::var("THAT_AGENT_MAX_TURNS") {
                if let Ok(n) = v.parse() {
                    self.max_turns = n;
                }
            }
        }
        if !file_has("max_tokens") {
            if let Ok(v) = std::env::var("THAT_AGENT_MAX_TOKENS") {
                if let Ok(n) = v.parse() {
                    self.max_tokens = n;
                }
            }
        }

        // ── Hard overrides: env vars always win (set by parent/orchestrator) ──
        if let Ok(v) = std::env::var("THAT_AGENT_PARENT") {
            if !v.is_empty() {
                self.parent = Some(v);
            }
        }
        if let Ok(v) = std::env::var("THAT_AGENT_ROLE") {
            if !v.is_empty() {
                self.role = Some(v);
            }
        }
        if let Ok(v) = std::env::var("THAT_AGENT_INHERIT_WORKSPACE") {
            if matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ) {
                self.inherit_workspace = true;
            }
        }
    }

    /// Return the isolated workspace directory for a named agent.
    pub fn agent_workspace_dir(name: &str) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".that-agent")
            .join("workspaces")
            .join(name)
    }

    /// Return the persistent memory DB path for a named agent.
    pub fn agent_memory_db_path(name: &str) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".that-agent")
            .join("agents")
            .join(name)
            .join("memory.db")
    }
}

// ── WorkspaceConfig — shared infrastructure config ──────────────────

/// Workspace-level configuration for shared infrastructure settings.
/// Does NOT contain agent-specific fields — those live in AgentDef files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Workspace directory (the agent's only working directory).
    #[serde(default)]
    pub workspace: Option<PathBuf>,

    /// State directory for transcripts, memory, jobs.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,

    /// Name of the default agent (refers to a file in agents/ dir).
    #[serde(default = "default_agent_name")]
    pub default_agent: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            workspace: None,
            state_dir: None,
            default_agent: default_agent_name(),
        }
    }
}

/// Raw TOML shape for loading workspace config files.
/// Uses Option fields so we can detect which values were explicitly set.
#[derive(Debug, Deserialize)]
struct RawWorkspaceConfig {
    #[serde(default)]
    workspace: Option<PathBuf>,
    #[serde(default)]
    state_dir: Option<PathBuf>,
    #[serde(default)]
    default_agent: Option<String>,
}

impl WorkspaceConfig {
    /// Load workspace config with layered precedence:
    /// 1. Compiled defaults
    /// 2. Global config (~/.config/that-agent/config.toml) — workspace-level fields only
    /// 3. Workspace config (.that-agent/config.toml) — workspace-level fields only
    ///
    /// Note: channel configuration is per-agent and lives in the agent's own config file
    /// (`~/.that-agent/agents/<name>/config.toml`), not here.
    pub fn load(workspace: Option<&Path>) -> anyhow::Result<Self> {
        let mut config = WorkspaceConfig::default();

        // Global config
        if let Some(config_dir) = dirs::config_dir() {
            let global_path = config_dir.join("that-agent").join("config.toml");
            if global_path.exists() {
                let text = std::fs::read_to_string(&global_path)?;
                let raw: RawWorkspaceConfig = toml::from_str(&text)?;
                config.apply_raw(raw);
            }
        }

        // Workspace config
        let ws = workspace
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok());

        if let Some(ws) = &ws {
            let project_path = ws.join(".that-agent").join("config.toml");
            if project_path.exists() {
                let text = std::fs::read_to_string(&project_path)?;
                let raw: RawWorkspaceConfig = toml::from_str(&text)?;
                config.apply_raw(raw);
            }
        }

        // Set workspace if not explicitly configured
        if config.workspace.is_none() {
            config.workspace = ws;
        }

        Ok(config)
    }

    /// Apply non-None fields from a raw config on top of current values.
    fn apply_raw(&mut self, raw: RawWorkspaceConfig) {
        if let Some(v) = raw.workspace {
            self.workspace = Some(v);
        }
        if let Some(v) = raw.state_dir {
            self.state_dir = Some(v);
        }
        if let Some(v) = raw.default_agent {
            self.default_agent = v;
        }
    }

    /// Resolve the state directory, creating it if needed.
    pub fn resolve_state_dir(&self) -> anyhow::Result<PathBuf> {
        let dir = if let Some(d) = &self.state_dir {
            d.clone()
        } else {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".that-agent")
                .join("state")
        };
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Path to the agents directory: `~/.that-agent/agents/`.
    pub fn agents_dir(&self) -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".that-agent")
            .join("agents")
    }

    /// Load a named agent definition from the agents directory.
    ///
    /// Preferred location: `~/.that-agent/agents/<name>/config.toml` (inside the agent
    /// home directory so all agent files are co-located).
    /// Legacy fallback: `~/.that-agent/agents/<name>.toml` (the old sibling-file layout).
    pub fn load_agent(&self, name: &str) -> anyhow::Result<AgentDef> {
        let dir = self.agents_dir();
        let preferred = dir.join(name).join("config.toml");
        let legacy = dir.join(format!("{name}.toml"));

        let mut agent = if preferred.exists() {
            AgentDef::from_file(&preferred)?
        } else if legacy.exists() {
            AgentDef::from_file(&legacy)?
        } else {
            // No config file — use defaults (allows running without init).
            // Pass None so all env vars apply unconditionally as overrides.
            let mut a = AgentDef::default();
            a.apply_env_overrides(None);
            a
        };
        agent.name = name.to_string();
        Ok(agent)
    }

    /// List available agent names by scanning the agents directory.
    ///
    /// Recognises both layouts:
    /// - `<name>/config.toml` — preferred (config inside the agent home directory)
    /// - `<name>.toml`        — legacy (sibling TOML file)
    pub fn list_agents(&self) -> anyhow::Result<Vec<String>> {
        let dir = self.agents_dir();
        if !dir.exists() {
            return Ok(vec![default_agent_name()]);
        }
        let mut agents = std::collections::HashSet::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.ends_with(".toml") {
                // Legacy: <name>.toml sibling file.
                agents.insert(file_name.trim_end_matches(".toml").to_string());
            } else if entry.path().join("config.toml").exists() {
                // Preferred: <name>/config.toml inside the agent home directory.
                agents.insert(file_name);
            }
        }
        let mut result: Vec<String> = agents.into_iter().collect();
        result.sort();
        Ok(result)
    }
}
