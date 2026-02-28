use crate::cli::{self, OutputFormatArg, ToolsCommands};
use crate::ToolContext;
use that_tools::output;

fn resolved_tools_agent_name(cli: &cli::Cli) -> String {
    if let Some(agent) = &cli.agent {
        return agent.clone();
    }
    that_core::config::WorkspaceConfig::load(cli.workspace.as_deref())
        .map(|ws| ws.default_agent)
        .unwrap_or_else(|_| "default".to_string())
}

fn apply_agent_memory_db(config: &mut that_tools::config::ThatToolsConfig, agent_name: &str) {
    config.memory.db_path = that_core::config::AgentDef::agent_memory_db_path(agent_name)
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
            that_tools::cli::InitProfile::Safe => "safe",
            that_tools::cli::InitProfile::Agent => "agent",
            that_tools::cli::InitProfile::Unrestricted => "unrestricted",
        };
        match that_tools::config::init::init(profile_name, force) {
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
    let mut tools_config = match that_tools::config::load_config(cwd.as_deref()) {
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
    if let Err(err) = that_tools::tools::memory::ensure_initialized(&tools_config.memory) {
        tracing::warn!(
            agent = %agent_name,
            path = %tools_config.memory.db_path,
            error = %err,
            "Failed to initialize agent memory database for CLI tool command"
        );
    }

    let explicit_max_tokens = cli.max_tokens;
    let resolved_format = cli
        .format
        .clone()
        .unwrap_or(match &tools_config.core.default_format {
            that_tools::config::OutputFormat::Json => OutputFormatArg::Json,
            that_tools::config::OutputFormat::Compact => OutputFormatArg::Compact,
            that_tools::config::OutputFormat::Markdown => OutputFormatArg::Markdown,
            that_tools::config::OutputFormat::Raw => OutputFormatArg::Raw,
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
                let schema_str = that_tools::config::export_schema();
                let schema_value: serde_json::Value =
                    serde_json::from_str(&schema_str).unwrap_or(serde_json::Value::Null);
                let result = output::emit_json(&schema_value, explicit_max_tokens);
                println!("{}", ctx.format_output(&result));
                Ok(())
            }
            ToolsCommands::ConfigInit { .. } => Ok(()),
        },
        cli::Commands::ConfigSchema => {
            let schema_str = that_tools::config::export_schema();
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
    command: &that_tools::cli::SessionCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_path = if ctx.config.session.sessions_path.is_empty() {
        that_tools::tools::session::default_sessions_path()
    } else {
        std::path::PathBuf::from(&ctx.config.session.sessions_path)
    };
    let soft_threshold = ctx.config.session.soft_threshold_tokens;

    match command {
        that_tools::cli::SessionCommands::Init { session_id } => {
            let record =
                that_tools::tools::session::init_session(session_id.clone(), &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        that_tools::cli::SessionCommands::Stats { session_id } => {
            let stats =
                that_tools::tools::session::get_stats(session_id, &sessions_path, soft_threshold)?;
            let budgeted = output::emit_json(&stats, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        that_tools::cli::SessionCommands::AddTokens { session_id, tokens } => {
            let record =
                that_tools::tools::session::add_tokens(session_id, *tokens, &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        that_tools::cli::SessionCommands::ResetContext { session_id, to } => {
            let record =
                that_tools::tools::session::reset_context(session_id, *to, &sessions_path)?;
            let budgeted = output::emit_json(&record, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}

pub fn handle_tools_skills_command(
    ctx: &ToolContext,
    command: &that_tools::cli::SkillsCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        that_tools::cli::SkillsCommands::List => {
            let result = that_tools::tools::skills::list();
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        that_tools::cli::SkillsCommands::Read { skill } => {
            match that_tools::tools::skills::read(skill) {
                Some(result) => {
                    let budgeted = output::emit_json(&result, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                    Ok(())
                }
                None => Err(format!("unknown skill: '{}'", skill).into()),
            }
        }
        that_tools::cli::SkillsCommands::Install { skill, path, force } => {
            let result =
                that_tools::tools::skills::install(skill.as_deref(), path.as_deref(), *force)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
