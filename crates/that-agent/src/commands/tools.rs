use crate::cli::{self, OutputFormatArg, ToolsCommands};
use crate::commands::ToolContext;
use crate::tools::output;

fn resolved_tools_agent_name(cli: &cli::Cli) -> String {
    if let Some(agent) = &cli.agent {
        return agent.clone();
    }
    crate::config::WorkspaceConfig::load(cli.workspace.as_deref())
        .map(|ws| ws.default_agent)
        .unwrap_or_else(|_| "default".to_string())
}

fn apply_agent_memory_db(config: &mut crate::tools::config::ThatToolsConfig, agent_name: &str) {
    config.memory.db_path = crate::config::AgentDef::agent_memory_db_path(agent_name)
        .display()
        .to_string();
}

/// Handle all tool commands (code, fs, mem, search, exec, human, daemon, config).
pub fn handle_tools_command(cli: &cli::Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Handle config-init before loading config (it creates the config file)
    let init_profile_and_force = match &cli.command {
        cli::Commands::ConfigInit { profile, force } => Some((profile, *force)),
        cli::Commands::Tools {
            command: ToolsCommands::ConfigInit { profile, force },
        } => Some((profile, *force)),
        _ => None,
    };
    if let Some((profile, force)) = init_profile_and_force {
        let profile_name = match profile {
            crate::tools::cli::InitProfile::Safe => "safe",
            crate::tools::cli::InitProfile::Agent => "agent",
            crate::tools::cli::InitProfile::Unrestricted => "unrestricted",
        };
        match crate::tools::config::init::init(profile_name, force) {
            Ok(result) => {
                let json = serde_json::to_string_pretty(&result).unwrap_or_default();
                println!("{}", json);
                return Ok(());
            }
            Err(e) => {
                eprintln!("{}", serde_json::json!({"error": e.to_string()}));
                std::process::exit(1);
            }
        }
    }

    // Load tools config
    let cwd = std::env::current_dir().ok();
    let mut tools_config = match crate::tools::config::load_config(cwd.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "{}",
                serde_json::json!({"error": format!("config load failed: {}", e)})
            );
            std::process::exit(1);
        }
    };
    let agent_name = resolved_tools_agent_name(cli);
    apply_agent_memory_db(&mut tools_config, &agent_name);

    let explicit_max_tokens = cli.max_tokens;
    let resolved_format = cli
        .format
        .clone()
        .unwrap_or(match &tools_config.core.default_format {
            crate::tools::config::OutputFormat::Json => OutputFormatArg::Json,
            crate::tools::config::OutputFormat::Compact => OutputFormatArg::Compact,
            crate::tools::config::OutputFormat::Markdown => OutputFormatArg::Markdown,
            crate::tools::config::OutputFormat::Raw => OutputFormatArg::Raw,
        });

    let ctx = ToolContext {
        max_tokens: cli
            .max_tokens
            .or(Some(tools_config.output.default_max_tokens)),
        format: resolved_format,
        config: tools_config,
    };

    match &cli.command {
        cli::Commands::Fs { command } => super::fs::handle_fs(&ctx, command),
        cli::Commands::Code { command } => super::code::handle_code(&ctx, command),
        cli::Commands::Mem { command } => super::memory::handle_mem(&ctx, command),
        cli::Commands::Human { command } => super::human::handle_human(&ctx, command),
        cli::Commands::Search { command } => super::search::handle_search(&ctx, command),
        cli::Commands::Exec { command } => super::exec::handle_exec(&ctx, command),
        cli::Commands::Daemon { command } => super::daemon::handle_daemon(&ctx, command),
        cli::Commands::Tools { command } => match command {
            ToolsCommands::Fs { command } => super::fs::handle_fs(&ctx, command),
            ToolsCommands::Code { command } => super::code::handle_code(&ctx, command),
            ToolsCommands::Mem { command } => super::memory::handle_mem(&ctx, command),
            ToolsCommands::Human { command } => super::human::handle_human(&ctx, command),
            ToolsCommands::Search { command } => super::search::handle_search(&ctx, command),
            ToolsCommands::Exec { command } => super::exec::handle_exec(&ctx, command),
            ToolsCommands::Daemon { command } => super::daemon::handle_daemon(&ctx, command),
            ToolsCommands::Session { command } => handle_tools_session_command(&ctx, command),
            ToolsCommands::Skills { command } => handle_tools_skills_command(&ctx, command),
            ToolsCommands::ConfigSchema => {
                let schema_str = crate::tools::config::export_schema();
                let schema_value: serde_json::Value =
                    serde_json::from_str(&schema_str).unwrap_or(serde_json::Value::Null);
                let result = output::emit_json(&schema_value, explicit_max_tokens);
                println!("{}", ctx.format_output(&result));
                Ok(())
            }
            ToolsCommands::ConfigInit { .. } => Ok(()),
        },
        cli::Commands::ConfigSchema => {
            let schema_str = crate::tools::config::export_schema();
            let schema_value: serde_json::Value =
                serde_json::from_str(&schema_str).unwrap_or(serde_json::Value::Null);
            let result = output::emit_json(&schema_value, explicit_max_tokens);
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn handle_tools_session_command(
    ctx: &ToolContext,
    command: &crate::tools::cli::SessionCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_path = if ctx.config.session.sessions_path.is_empty() {
        crate::tools::impls::session::default_sessions_path()
    } else {
        std::path::PathBuf::from(&ctx.config.session.sessions_path)
    };
    let soft_threshold = ctx.config.session.soft_threshold_tokens;

    match command {
        crate::tools::cli::SessionCommands::Init { session_id } => {
            let record =
                crate::tools::impls::session::init_session(session_id.clone(), &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        crate::tools::cli::SessionCommands::Stats { session_id } => {
            let stats = crate::tools::impls::session::get_stats(
                session_id,
                &sessions_path,
                soft_threshold,
            )?;
            let budgeted = output::emit_json(&stats, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        crate::tools::cli::SessionCommands::AddTokens { session_id, tokens } => {
            let record =
                crate::tools::impls::session::add_tokens(session_id, *tokens, &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        crate::tools::cli::SessionCommands::ResetContext { session_id, to } => {
            let record =
                crate::tools::impls::session::reset_context(session_id, *to, &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}

pub fn handle_tools_skills_command(
    ctx: &ToolContext,
    command: &crate::tools::cli::SkillsCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        crate::tools::cli::SkillsCommands::List => {
            let result = crate::tools::impls::skills::list();
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        crate::tools::cli::SkillsCommands::Read { skill } => {
            match crate::tools::impls::skills::read(skill) {
                Some(result) => {
                    let budgeted = output::emit_json(&result, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                    Ok(())
                }
                None => Err(format!("unknown skill: '{}'", skill).into()),
            }
        }
        crate::tools::cli::SkillsCommands::Install { skill, path, force } => {
            let result =
                crate::tools::impls::skills::install(skill.as_deref(), path.as_deref(), *force)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
