//! Unified CLI definition for the `that` binary.
//!
//! Merges tool commands and that-agent orchestration commands
//! into a single flat namespace.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// that — Unified agent orchestration and tool execution.
///
/// When invoked as `that-tools` (via symlink), only tool commands are available.
/// When invoked as `that`, the full unified CLI is available.
#[derive(Parser, Debug)]
#[command(
    name = "that",
    version,
    about = "Unified agent orchestration and tool execution.",
    long_about = "that unifies agent orchestration (run, chat, sessions) with \
    structural code comprehension, federated search, persistent memory, \
    human-in-the-loop governance, and token-budget enforcement."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    // ── Global flags (unified from both projects) ──
    /// Output format (overrides config default).
    #[arg(long, global = true)]
    pub format: Option<OutputFormatArg>,

    /// Maximum tokens for output (overrides config).
    #[arg(long, global = true)]
    pub max_tokens: Option<usize>,

    /// Agent name (loads from `agents/<name>.toml`).
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// LLM provider: anthropic, openai, openrouter.
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// LLM model identifier.
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Maximum multi-turn iterations per run.
    #[arg(long, global = true)]
    pub max_turns: Option<usize>,

    /// Workspace directory (overrides config).
    #[arg(long, global = true)]
    pub workspace: Option<PathBuf>,

    /// Skip the Docker sandbox and run locally.
    #[arg(long, global = true)]
    pub no_sandbox: bool,

    /// Disable TUI mode (use plain stdin/stdout for chat).
    #[arg(long, global = true)]
    pub no_tui: bool,

    /// Show tool calls and results in real time.
    #[arg(long, global = true)]
    pub debug: bool,

    /// Verbosity level (-v info, -vv debug, -vvv trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all non-essential output.
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormatArg {
    Json,
    Compact,
    Markdown,
    Raw,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    // ── Agent Orchestration (from that-agent) ──
    /// Runtime execution commands (query, chat, listen).
    Run {
        #[command(subcommand)]
        command: RunCommands,
    },

    /// Agent management.
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Evaluation harness for agent scenarios.
    Eval {
        #[command(subcommand)]
        command: EvalCommands,
    },

    /// Tool layer commands grouped under a single namespace.
    Tools {
        #[command(subcommand)]
        command: ToolsCommands,
    },

    /// Manage agent-scoped shell secrets exported in the agent bash profile.
    Secrets {
        #[command(subcommand)]
        command: SecretsCommands,
    },

    /// Show agent status and configuration.
    #[command(hide = true)]
    Status,

    // ── Code Intelligence (from that-tools) ──
    /// Code analysis with AST-aware structural comprehension.
    #[command(hide = true)]
    Code {
        #[command(subcommand)]
        command: crate::tools::cli::CodeCommands,
    },

    /// File system operations with token-minimal output.
    #[command(hide = true)]
    Fs {
        #[command(subcommand)]
        command: crate::tools::cli::FsCommands,
    },

    // ── Memory (from that-tools) ──
    /// Persistent memory operations.
    #[command(hide = true)]
    Mem {
        #[command(subcommand)]
        command: crate::tools::cli::MemCommands,
    },

    // ── Sessions (merged) ──
    /// Session management (transcripts + token tracking).
    #[command(hide = true)]
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    // ── Skills (merged) ──
    /// Skill management.
    #[command(hide = true)]
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },

    /// Plugin management (agent-scoped extensions).
    #[command(hide = true)]
    Plugin {
        #[command(subcommand)]
        command: PluginCommands,
    },

    // ── Other Tools (from that-tools) ──
    /// Federated web search and URL fetching.
    #[command(hide = true)]
    Search {
        #[command(subcommand)]
        command: crate::tools::cli::SearchCommands,
    },

    /// Execute a shell command with policy governance.
    #[command(hide = true)]
    Exec {
        #[command(subcommand)]
        command: crate::tools::cli::ExecCommands,
    },

    /// Human-in-the-loop interaction.
    #[command(hide = true)]
    Human {
        #[command(subcommand)]
        command: crate::tools::cli::HumanCommands,
    },

    /// Daemon mode (long-lived process).
    #[command(hide = true)]
    Daemon {
        #[command(subcommand)]
        command: crate::tools::cli::DaemonCommands,
    },

    /// Initialize workspace configuration.
    #[command(hide = true)]
    Init {
        /// Agent name (defaults to "default").
        name: Option<String>,
        /// Overwrite existing config.
        #[arg(long)]
        force: bool,
        /// Share the global workspace instead of creating an isolated one.
        #[arg(long)]
        shared_workspace: bool,
    },

    /// Initialize a project with a default that-tools configuration profile.
    #[command(hide = true)]
    ConfigInit {
        /// Configuration profile to use.
        #[arg(long, default_value = "safe")]
        profile: crate::tools::cli::InitProfile,
        /// Overwrite existing configuration file.
        #[arg(long)]
        force: bool,
    },

    /// Export the that-tools configuration JSON Schema.
    #[command(hide = true)]
    ConfigSchema,
}

/// Tool commands grouped under `that tools`.
#[derive(Subcommand, Debug)]
pub enum ToolsCommands {
    /// Code analysis with AST-aware structural comprehension.
    Code {
        #[command(subcommand)]
        command: crate::tools::cli::CodeCommands,
    },
    /// File system operations with token-minimal output.
    Fs {
        #[command(subcommand)]
        command: crate::tools::cli::FsCommands,
    },
    /// Persistent memory operations.
    Mem {
        #[command(subcommand)]
        command: crate::tools::cli::MemCommands,
    },
    /// Session tracking: token accumulation and compaction events.
    Session {
        #[command(subcommand)]
        command: crate::tools::cli::SessionCommands,
    },
    /// Federated web search and URL fetching.
    Search {
        #[command(subcommand)]
        command: crate::tools::cli::SearchCommands,
    },
    /// Execute a shell command with policy governance.
    Exec {
        #[command(subcommand)]
        command: crate::tools::cli::ExecCommands,
    },
    /// Human-in-the-loop interaction.
    Human {
        #[command(subcommand)]
        command: crate::tools::cli::HumanCommands,
    },
    /// Daemon mode (long-lived process).
    Daemon {
        #[command(subcommand)]
        command: crate::tools::cli::DaemonCommands,
    },
    /// View and install that-tools skills.
    Skills {
        #[command(subcommand)]
        command: crate::tools::cli::SkillsCommands,
    },
    /// Initialize a project with a default that-tools configuration profile.
    ConfigInit {
        /// Configuration profile to use.
        #[arg(long, default_value = "safe")]
        profile: crate::tools::cli::InitProfile,
        /// Overwrite existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Export the that-tools configuration JSON Schema.
    ConfigSchema,
}

/// Runtime commands grouped under `that run`.
#[derive(Subcommand, Debug, Clone)]
pub enum RunCommands {
    /// Execute a one-shot task with the agent.
    Query {
        /// The task to execute (ignored if --task-file is set).
        task: Option<String>,
        /// Read the task from a file instead of a CLI argument.
        /// Avoids shell escaping issues with large/complex task descriptions.
        #[arg(long)]
        task_file: Option<String>,
        /// Session ID (creates new if omitted).
        #[arg(long)]
        session: Option<String>,
        /// Send the query to a remote agent's HTTP gateway instead of running locally.
        #[arg(long)]
        remote: Option<String>,
        /// Bearer token for authenticating with the remote gateway.
        #[arg(long)]
        token: Option<String>,
        /// Request timeout in seconds (default: 300).
        #[arg(long, default_value = "300")]
        timeout: Option<u64>,
        /// Parent agent name (for tracking agent hierarchy).
        #[arg(long, env = "THAT_AGENT_PARENT")]
        parent: Option<String>,
        /// Role assigned to this agent by its parent.
        #[arg(long, env = "THAT_AGENT_ROLE")]
        role: Option<String>,
        /// Inherit workspace from parent instead of using an isolated directory.
        #[arg(long, env = "THAT_AGENT_INHERIT_WORKSPACE")]
        inherit_workspace: bool,
    },
    /// Interactive chat mode.
    Chat {
        /// Session ID (creates new if omitted).
        #[arg(long)]
        session: Option<String>,
    },
    /// Listen for inbound messages from configured channels (Telegram, Discord, WhatsApp).
    ///
    /// Starts all enabled channel adapters and routes incoming messages to the agent.
    /// Each unique sender gets a persistent session. Use `--no-sandbox` to run locally.
    Listen {
        /// Session ID to resume (creates new per-sender sessions if omitted).
        #[arg(long)]
        session: Option<String>,
        /// Parent agent name (for tracking agent hierarchy).
        #[arg(long, env = "THAT_AGENT_PARENT")]
        parent: Option<String>,
        /// Role assigned to this agent by its parent.
        #[arg(long, env = "THAT_AGENT_ROLE")]
        role: Option<String>,
        /// Inherit workspace from parent instead of using an isolated directory.
        #[arg(long, env = "THAT_AGENT_INHERIT_WORKSPACE")]
        inherit_workspace: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum SecretsCommands {
    /// Add or update a secret export for the selected agent (`--agent <name>`).
    #[command(alias = "ADD")]
    Add {
        /// Secret key (environment variable name).
        key: String,
        /// Secret value.
        #[arg(long)]
        value: String,
    },
    /// Delete a secret export for the selected agent (`--agent <name>`).
    #[command(alias = "DELETE")]
    Delete {
        /// Secret key (environment variable name).
        key: String,
    },
}

/// Merged session commands from both projects.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionCommands {
    /// List all sessions (transcripts).
    List,
    /// Show a session transcript.
    Show {
        /// Session ID.
        id: String,
    },
    /// Create a new session.
    New,
    /// Initialize or retrieve a token-tracking session record.
    Init {
        /// Session identifier. A new UUID is generated if omitted.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Show accumulated token usage and compaction count for a session.
    Stats {
        /// Session identifier.
        #[arg(long)]
        session_id: String,
    },
    /// Add tokens to a session's accumulated context count.
    AddTokens {
        /// Session identifier.
        #[arg(long)]
        session_id: String,
        /// Number of tokens to add.
        #[arg(long)]
        tokens: usize,
    },
    /// Reset the context token counter for a session.
    ResetContext {
        /// Session identifier.
        #[arg(long)]
        session_id: String,
        /// Token count to reset to (default: 0).
        #[arg(long, default_value = "0")]
        to: usize,
    },
}

/// Merged skill commands.
#[derive(Subcommand, Debug, Clone)]
pub enum SkillCommands {
    /// List available skills.
    List,
    /// Show a skill's full content.
    Show {
        /// Skill name.
        name: String,
    },
    /// Install skills as SKILL.md files for agent auto-discovery.
    Install {
        /// Specific skill to install (omit to install all).
        skill: Option<String>,
        /// Destination directory for skill folders.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite existing SKILL.md files.
        #[arg(long)]
        force: bool,
    },
    /// Read a skill's documentation (alias for show).
    Read {
        /// Skill name.
        skill: String,
    },
    /// Create a skill scaffold under the agent scope or a specific plugin scope.
    Create {
        /// Skill name.
        name: String,
        /// Plugin id to create this skill under plugin scope.
        #[arg(long)]
        plugin: Option<String>,
        /// Overwrite scaffold file when skill already exists.
        #[arg(long)]
        force: bool,
    },
}

/// Plugin management commands.
#[derive(Subcommand, Debug, Clone)]
pub enum PluginCommands {
    /// List installed plugins for the active agent.
    List,
    /// Show a plugin manifest and runtime state.
    Show {
        /// Plugin id.
        id: String,
    },
    /// Enable a plugin.
    Enable {
        /// Plugin id.
        id: String,
    },
    /// Disable a plugin.
    Disable {
        /// Plugin id.
        id: String,
    },
    /// Create a new plugin scaffold under the agent plugin directory.
    Create {
        /// Plugin id.
        id: String,
        /// Overwrite scaffold files when the plugin already exists.
        #[arg(long)]
        force: bool,
    },
}

/// Agent management commands.
#[derive(Subcommand, Debug, Clone)]
pub enum AgentCommands {
    /// Initialize a new agent.
    Init {
        /// Agent name (for example: Moshe).
        name: String,
        /// Provider API key used during initialization.
        /// If omitted, uses provider key from environment/.env.
        #[arg(long)]
        api_key: Option<String>,
        /// Identity prompt used to generate Identity.md and Soul.md.
        #[arg(long)]
        prompt: Option<String>,
        /// Overwrite existing config.
        #[arg(long)]
        force: bool,
        /// Share the global workspace instead of creating an isolated one.
        #[arg(long)]
        shared_workspace: bool,
    },
    /// List available agents.
    List,
    /// Show an agent's configuration.
    Show {
        /// Agent name.
        name: String,
    },
    /// Delete an agent and its isolated workspace.
    Delete {
        /// Agent name.
        name: String,
    },
    /// Show status for the selected agent (`--agent <name>`).
    Status,
    /// Skill management for the selected agent (`--agent <name>`).
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Plugin management for the selected agent (`--agent <name>`).
    Plugin {
        #[command(subcommand)]
        command: PluginCommands,
    },
}

/// Eval commands grouped under `that eval`.
#[derive(Subcommand, Debug, Clone)]
pub enum EvalCommands {
    /// Run a single scenario file.
    Run {
        /// Path to the scenario TOML file.
        scenario: PathBuf,
        /// Skip the LLM judge step.
        #[arg(long)]
        no_judge: bool,
        /// Fail if any step errors.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        fail_on_step_error: bool,
        /// Minimum assertion pass percentage.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
        min_assertion_pass_pct: u8,
        /// Minimum judge score (0-100).
        #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
        min_judge_score: Option<u32>,
    },
    /// Run all scenarios in a directory.
    RunAll {
        /// Directory containing scenario TOML files.
        #[arg(default_value = "evals/scenarios")]
        dir: PathBuf,
        /// Filter by tags (comma-separated).
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Skip the LLM judge step.
        #[arg(long)]
        no_judge: bool,
        /// Fail if any step errors.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        fail_on_step_error: bool,
        /// Minimum assertion pass percentage.
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
        min_assertion_pass_pct: u8,
        /// Minimum judge score (0-100).
        #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
        min_judge_score: Option<u32>,
    },
    /// Display a saved eval report.
    Report {
        /// Run ID.
        run_id: String,
        /// Output format.
        #[arg(long, default_value = "markdown")]
        format: crate::eval::cli::ReportFormat,
    },
    /// List past eval runs.
    List,
    /// List available scenarios in a directory.
    ListScenarios {
        /// Directory to scan.
        #[arg(default_value = "evals/scenarios")]
        dir: PathBuf,
    },
}
