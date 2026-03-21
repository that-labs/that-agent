//! TOML schema types for eval scenarios.
//!
//! A scenario file describes a sequence of steps the runner will execute against
//! the agent, plus a rubric the LLM judge uses to score the run.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

// ── Top-level scenario ───────────────────────────────────────────────────────

/// A complete evaluation scenario loaded from a TOML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Human-readable scenario name.
    pub name: String,

    /// What this scenario measures.
    #[serde(default)]
    pub description: String,

    /// Agent name to run against (default: "default").
    #[serde(default = "default_agent_name")]
    pub agent_name: String,

    /// LLM provider override (e.g. "anthropic").
    #[serde(default)]
    pub provider: Option<String>,

    /// Model override (e.g. "claude-sonnet-4-6").
    #[serde(default)]
    pub model: Option<String>,

    /// LLM judge provider override (e.g. "openai").
    #[serde(default)]
    pub judge_provider: Option<String>,

    /// LLM judge model override.
    #[serde(default)]
    pub judge_model: Option<String>,

    /// Maximum multi-turn iterations per prompt step (default: 10).
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,

    /// Run inside a Docker sandbox (default: false).
    #[serde(default)]
    pub sandbox: bool,

    /// Optional tags for filtering with `run-all --tags`.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Maximum wall-clock seconds per prompt step (default: 120).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Ordered list of steps the runner executes.
    #[serde(default)]
    pub steps: Vec<Step>,

    /// Scoring rubric passed to the LLM judge.
    #[serde(default)]
    pub rubric: Rubric,
}

fn default_agent_name() -> String {
    "default".into()
}
fn default_max_turns() -> usize {
    10
}
fn default_timeout_secs() -> u64 {
    120
}

impl Scenario {
    /// Load a scenario from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read scenario {}: {e}", path.display()))?;
        let scenario: Scenario = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Cannot parse scenario {}: {e}", path.display()))?;
        Ok(scenario)
    }
}

// ── Steps ────────────────────────────────────────────────────────────────────

/// A single step in the scenario.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// Send a user message to the agent (optionally sharing history via session key).
    Prompt(PromptStep),

    /// Clear in-memory history for a session label (JSONL transcript is kept).
    ResetSession(ResetSessionStep),

    /// Write a SKILL.md to the agent's skills directory.
    CreateSkill(CreateSkillStep),

    /// Run a shell command (setup / teardown).
    RunCommand(RunCommandStep),

    /// Write a file to disk.
    CreateFile(CreateFileStep),

    /// Run assertions; failures are recorded but do not abort the run.
    Assert(AssertStep),
}

/// Send a prompt to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptStep {
    /// Session label — steps sharing a label share conversation history.
    pub session: String,
    /// The user message to send.
    pub content: String,
}

/// Clear in-memory history for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetSessionStep {
    pub session: String,
}

/// Write a skill to the agent's skills directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSkillStep {
    /// Skill directory name (used as sub-directory under `skills/`).
    pub name: String,
    /// Full SKILL.md content including YAML frontmatter.
    pub content: String,
}

/// Execute a shell command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCommandStep {
    pub command: String,
}

/// Write a file to a specific path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFileStep {
    pub path: String,
    pub content: String,
}

/// Run one or more assertions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertStep {
    #[serde(default)]
    pub assertions: Vec<Assertion>,
}

// ── Assertions ───────────────────────────────────────────────────────────────

/// An individual assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    /// Assert that a file exists on disk.
    FileExists { path: String },

    /// Assert that a shell command exits 0.
    CommandSucceeds { command: String },

    /// Assert that a file's contents contain a substring.
    FileContains { path: String, contains: String },

    /// Assert that a specific tool call appears in eval hook transcript.
    ToolCallSeen {
        tool: String,
        #[serde(default = "default_min_count")]
        min_count: usize,
    },
}

// ── Rubric ───────────────────────────────────────────────────────────────────

/// The scoring rubric given to the LLM judge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rubric {
    #[serde(default)]
    pub criteria: Vec<Criterion>,
}

/// A single rubric criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Criterion {
    pub name: String,
    pub description: String,
    /// Relative weight (0–100). Weights need not sum to 100 — the judge uses them
    /// as relative importance signals.
    #[serde(default = "default_weight")]
    pub weight: u32,
}

fn default_weight() -> u32 {
    25
}

fn default_min_count() -> usize {
    1
}
