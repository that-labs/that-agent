//! that — Unified agent orchestration and tool execution.
//!
//! Single binary that merges tool commands with that-agent orchestration.
//! When invoked as `that-tools` (via symlink), only tool commands are exposed.
//! When invoked as `that`, the full unified CLI is available.

mod cli;
mod commands;

use clap::Parser;

use cli::{Cli, Commands, OutputFormatArg};
use that_tools::config::PolicyLevel;
use that_tools::output;

/// Execution context for tool invocations.
/// Carries resolved config, format, and budget settings.
pub struct ToolContext {
    config: that_tools::ThatToolsConfig,
    max_tokens: Option<usize>,
    format: OutputFormatArg,
}

impl ToolContext {
    /// Check if a tool action is allowed by policy.
    fn check_policy(&self, tool_name: &str) -> Result<(), String> {
        let level = self.policy_for(tool_name);
        match level {
            PolicyLevel::Allow => Ok(()),
            PolicyLevel::Prompt => {
                if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    match that_tools::tools::human::prompt::confirm_terminal(&format!(
                        "Tool '{}' requires approval. Allow?",
                        tool_name
                    )) {
                        Ok(true) => Ok(()),
                        Ok(false) => {
                            Err(format!("policy denied: user denied tool '{}'", tool_name))
                        }
                        Err(e) => Err(format!(
                            "policy denied: could not prompt for tool '{}': {}",
                            tool_name, e
                        )),
                    }
                } else {
                    Err(format!(
                        "policy denied: tool '{}' requires approval but no interactive terminal is available",
                        tool_name
                    ))
                }
            }
            PolicyLevel::Deny => Err(format!(
                "policy denied: tool '{}' is not allowed by current policy",
                tool_name
            )),
        }
    }

    fn policy_for(&self, tool_name: &str) -> &PolicyLevel {
        match tool_name {
            "code_read" => &self.config.policy.tools.code_read,
            "code_edit" => &self.config.policy.tools.code_edit,
            "fs_read" => &self.config.policy.tools.fs_read,
            "fs_write" => &self.config.policy.tools.fs_write,
            "fs_delete" => &self.config.policy.tools.fs_delete,
            "shell_exec" => &self.config.policy.tools.shell_exec,
            "search" => &self.config.policy.tools.search,
            "memory" => &self.config.policy.tools.memory,
            "git_commit" => &self.config.policy.tools.git_commit,
            "git_push" => &self.config.policy.tools.git_push,
            "mem_compact" => &self.config.policy.tools.mem_compact,
            _ => &self.config.policy.default,
        }
    }

    /// Format output according to --format flag.
    fn format_output(&self, budgeted: &output::BudgetedOutput) -> String {
        match self.format {
            OutputFormatArg::Json => {
                let envelope = output::OutputEnvelope::from_budgeted(budgeted);
                serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| budgeted.content.clone())
            }
            OutputFormatArg::Compact => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => {
                        let compacted = output::compact_json_value(&v);
                        serde_json::to_string(&compacted)
                            .unwrap_or_else(|_| budgeted.content.clone())
                    }
                    Err(_) => budgeted.content.clone(),
                }
            }
            OutputFormatArg::Raw => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => output::render_raw(&v),
                    Err(_) => budgeted.content.clone(),
                }
            }
            OutputFormatArg::Markdown => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => output::render_markdown(&v),
                    Err(_) => budgeted.content.clone(),
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Initialize tracing.
    // Set PHOENIX_TRACING=true to forward structured traces to Phoenix.
    let filter = match (cli.verbose, cli.quiet, cli.debug) {
        (0, true, _) => "error",
        (0, false, _) => "info",
        (1, _, _) => "info",
        (2, _, _) => "debug",
        _ => "trace",
    };

    that_core::observability::init_tracing(filter);

    // Route command to the appropriate handler
    match &cli.command {
        // ── Tool commands ──
        Commands::Code { .. }
        | Commands::Fs { .. }
        | Commands::Mem { .. }
        | Commands::Search { .. }
        | Commands::Exec { .. }
        | Commands::Human { .. }
        | Commands::Daemon { .. }
        | Commands::ConfigSchema
        | Commands::ConfigInit { .. }
        | Commands::Tools { .. } => {
            commands::handle_tools_command(&cli)?;
        }

        // ── Session commands (merged: transcript + token tracking) ──
        Commands::Session { command } => {
            commands::handle_session_command(&cli, command)?;
        }

        // ── Skill commands (merged) ──
        Commands::Skill { command } => {
            commands::handle_skill_command(&cli, command)?;
        }

        Commands::Secrets { command } => {
            commands::handle_secrets_command(&cli, command)?;
        }

        // ── Agent orchestration commands ──
        Commands::Run { .. }
        | Commands::Agent { .. }
        | Commands::Status
        | Commands::Plugin { .. }
        | Commands::Init { .. } => {
            commands::handle_agent_orchestration_command(&cli).await?;
        }
    }

    that_core::observability::shutdown_tracing();
    Ok(())
}
