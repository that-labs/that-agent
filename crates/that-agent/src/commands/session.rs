use crate::cli::{self, OutputFormatArg, SessionCommands};
use crate::commands::ToolContext;
use crate::tools::output;

/// Handle merged session commands.
pub fn handle_session_command(
    cli: &cli::Cli,
    command: &SessionCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        // Transcript-based session commands (from that-agent)
        SessionCommands::List | SessionCommands::Show { .. } | SessionCommands::New => {
            let ws = crate::config::WorkspaceConfig::load(cli.workspace.as_deref())
                .map_err(|e| e.to_string())?;
            let ta_cmd = match command {
                SessionCommands::List => crate::control::cli::SessionCommands::List,
                SessionCommands::Show { id } => {
                    crate::control::cli::SessionCommands::Show { id: id.clone() }
                }
                SessionCommands::New => crate::control::cli::SessionCommands::New,
                _ => unreachable!(),
            };
            crate::orchestration::handle_session_command(&ws, ta_cmd).map_err(|e| e.to_string())?;
            Ok(())
        }
        // Token-tracking session commands (from that-tools)
        SessionCommands::Init { .. }
        | SessionCommands::Stats { .. }
        | SessionCommands::AddTokens { .. }
        | SessionCommands::ResetContext { .. } => {
            let cwd = std::env::current_dir().ok();
            let tools_config = crate::tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;

            let sessions_path = if tools_config.session.sessions_path.is_empty() {
                crate::tools::impls::session::default_sessions_path()
            } else {
                std::path::PathBuf::from(&tools_config.session.sessions_path)
            };
            let soft_threshold = tools_config.session.soft_threshold_tokens;
            let max_tokens = cli
                .max_tokens
                .or(Some(tools_config.output.default_max_tokens));

            let resolved_format =
                cli.format
                    .clone()
                    .unwrap_or(match &tools_config.core.default_format {
                        crate::tools::config::OutputFormat::Json => OutputFormatArg::Json,
                        crate::tools::config::OutputFormat::Compact => OutputFormatArg::Compact,
                        crate::tools::config::OutputFormat::Markdown => OutputFormatArg::Markdown,
                        crate::tools::config::OutputFormat::Raw => OutputFormatArg::Raw,
                    });

            let ctx = ToolContext {
                max_tokens,
                format: resolved_format,
                config: tools_config,
            };

            match command {
                SessionCommands::Init { session_id } => {
                    let record = crate::tools::impls::session::init_session(
                        session_id.clone(),
                        &sessions_path,
                    )?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::Stats { session_id } => {
                    let stats = crate::tools::impls::session::get_stats(
                        session_id,
                        &sessions_path,
                        soft_threshold,
                    )?;
                    let budgeted = output::emit_json(&stats, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::AddTokens { session_id, tokens } => {
                    let record = crate::tools::impls::session::add_tokens(
                        session_id,
                        *tokens,
                        &sessions_path,
                    )?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::ResetContext { session_id, to } => {
                    let record = crate::tools::impls::session::reset_context(
                        session_id,
                        *to,
                        &sessions_path,
                    )?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                _ => unreachable!(),
            }
            Ok(())
        }
    }
}
