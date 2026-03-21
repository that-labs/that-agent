//! that — Unified agent orchestration and tool execution.

use clap::Parser;

use that_agent::cli::{Cli, Commands};
use that_agent::commands;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    let filter = match (cli.verbose, cli.quiet, cli.debug) {
        (0, true, _) => "error",
        (0, false, _) => "info",
        (1, _, _) => "info",
        (2, _, _) => "debug",
        _ => "trace",
    };

    that_agent::observability::init_tracing(filter);

    match &cli.command {
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
        Commands::Session { command } => {
            commands::handle_session_command(&cli, command)?;
        }
        Commands::Skill { command } => {
            commands::handle_skill_command(&cli, command)?;
        }
        Commands::Secrets { command } => {
            commands::handle_secrets_command(&cli, command)?;
        }
        Commands::Eval { command } => {
            commands::handle_eval_command(&cli, command).await?;
        }
        Commands::Run { .. }
        | Commands::Agent { .. }
        | Commands::Status
        | Commands::Plugin { .. }
        | Commands::Init { .. } => {
            commands::handle_agent_orchestration_command(&cli).await?;
        }
    }

    that_agent::observability::shutdown_tracing();
    Ok(())
}
