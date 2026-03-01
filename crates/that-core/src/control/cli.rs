use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "that-agent",
    about = "Anvil-first autonomous agent powered by Rig.rs",
    version
)]
pub struct Cli {
    /// Workspace directory (overrides config)
    #[arg(long, global = true)]
    pub workspace: Option<PathBuf>,

    /// LLM provider: anthropic, openai
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// LLM model identifier
    #[arg(long, global = true)]
    pub model: Option<String>,

    /// Maximum multi-turn iterations per run
    #[arg(long, global = true)]
    pub max_turns: Option<usize>,

    /// Skip the Docker sandbox and run bash directly on the host
    #[arg(long, global = true)]
    pub no_sandbox: bool,

    /// Agent name (loads from `agents/<name>.toml`)
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// Show tool calls and results in real time
    #[arg(long, global = true)]
    pub debug: bool,

    /// Disable TUI mode (use plain stdin/stdout for chat)
    #[arg(long, global = true)]
    pub no_tui: bool,

    /// Verbosity (-v info, -vv debug, -vvv trace)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the agent with a task
    Run {
        /// The task to execute
        task: String,

        /// Session ID (creates new if omitted)
        #[arg(long)]
        session: Option<String>,
    },

    /// Interactive chat mode
    Chat {
        /// Session ID (creates new if omitted)
        #[arg(long)]
        session: Option<String>,
    },

    /// Session management
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    /// Agent management
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Initialize workspace configuration
    Init {
        /// Agent name (defaults to "default")
        name: Option<String>,

        /// Overwrite existing config
        #[arg(long)]
        force: bool,

        /// Share the global workspace instead of creating an isolated one
        #[arg(long)]
        shared_workspace: bool,
    },

    /// Show agent status and configuration
    Status,

    /// Skill management
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
}

#[derive(Clone, Subcommand)]
pub enum SessionCommands {
    /// List all sessions
    List,
    /// Show a session transcript
    Show {
        /// Session ID
        id: String,
    },
    /// Create a new session
    New,
}

#[derive(Clone, Subcommand)]
pub enum SkillCommands {
    /// List available skills
    List,
    /// Show a skill's full content
    Show {
        /// Skill name
        name: String,
    },
}

#[derive(Clone, Subcommand)]
pub enum AgentCommands {
    /// List available agents
    List,
    /// Show an agent's configuration
    Show {
        /// Agent name
        name: String,
    },
    /// Delete an agent and its isolated workspace
    Delete {
        /// Agent name
        name: String,
    },
}
