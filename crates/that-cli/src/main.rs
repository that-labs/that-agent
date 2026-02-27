//! that — Unified agent orchestration and tool execution.
//!
//! Single binary that merges tool commands with that-agent orchestration.
//! When invoked as `that-tools` (via symlink), only tool commands are exposed.
//! When invoked as `that`, the full unified CLI is available.

mod cli;

use clap::Parser;

use cli::{
    Cli, Commands, OutputFormatArg, PluginCommands, RunCommands, SecretsCommands, SessionCommands,
    SkillCommands, ToolsCommands,
};
use that_tools::config::PolicyLevel;
use that_tools::output;

/// Execution context for tool invocations.
/// Carries resolved config, format, and budget settings.
struct ToolContext {
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

fn resolved_tools_agent_name(cli: &Cli) -> String {
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
            handle_tools_command(&cli)?;
        }

        // ── Session commands (merged: transcript + token tracking) ──
        Commands::Session { command } => {
            handle_session_command(&cli, command)?;
        }

        // ── Skill commands (merged) ──
        Commands::Skill { command } => {
            handle_skill_command(&cli, command)?;
        }

        Commands::Secrets { command } => {
            handle_secrets_command(&cli, command)?;
        }

        // ── Agent orchestration commands ──
        Commands::Run { .. }
        | Commands::Agent { .. }
        | Commands::Status
        | Commands::Plugin { .. }
        | Commands::Init { .. } => {
            handle_agent_orchestration_command(&cli).await?;
        }
    }

    that_core::observability::shutdown_tracing();
    Ok(())
}

/// Handle all tool commands (code, fs, mem, search, exec, human, daemon, config).
fn handle_tools_command(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Handle config-init before loading config (it creates the config file)
    let init_profile_and_force = match &cli.command {
        Commands::ConfigInit { profile, force } => Some((profile, *force)),
        Commands::Tools {
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
        Commands::Fs { command } => handle_fs(&ctx, command),
        Commands::Code { command } => handle_code(&ctx, command),
        Commands::Mem { command } => handle_mem(&ctx, command),
        Commands::Human { command } => handle_human(&ctx, command),
        Commands::Search { command } => handle_search(&ctx, command),
        Commands::Exec { command } => handle_exec(&ctx, command),
        Commands::Daemon { command } => handle_daemon(&ctx, command),
        Commands::Tools { command } => match command {
            ToolsCommands::Fs { command } => handle_fs(&ctx, command),
            ToolsCommands::Code { command } => handle_code(&ctx, command),
            ToolsCommands::Mem { command } => handle_mem(&ctx, command),
            ToolsCommands::Human { command } => handle_human(&ctx, command),
            ToolsCommands::Search { command } => handle_search(&ctx, command),
            ToolsCommands::Exec { command } => handle_exec(&ctx, command),
            ToolsCommands::Daemon { command } => handle_daemon(&ctx, command),
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
        Commands::ConfigSchema => {
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

fn handle_tools_session_command(
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

fn handle_tools_skills_command(
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

/// Handle merged session commands.
fn handle_session_command(
    cli: &Cli,
    command: &SessionCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        // Transcript-based session commands (from that-agent)
        SessionCommands::List | SessionCommands::Show { .. } | SessionCommands::New => {
            let ws = that_core::config::WorkspaceConfig::load(cli.workspace.as_deref())
                .map_err(|e| e.to_string())?;
            let ta_cmd = match command {
                SessionCommands::List => that_core::control::cli::SessionCommands::List,
                SessionCommands::Show { id } => {
                    that_core::control::cli::SessionCommands::Show { id: id.clone() }
                }
                SessionCommands::New => that_core::control::cli::SessionCommands::New,
                _ => unreachable!(),
            };
            that_core::orchestration::handle_session_command(&ws, ta_cmd)
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        // Token-tracking session commands (from that-tools)
        SessionCommands::Init { .. }
        | SessionCommands::Stats { .. }
        | SessionCommands::AddTokens { .. }
        | SessionCommands::ResetContext { .. } => {
            let cwd = std::env::current_dir().ok();
            let tools_config = that_tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;

            let sessions_path = if tools_config.session.sessions_path.is_empty() {
                that_tools::tools::session::default_sessions_path()
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
                        that_tools::config::OutputFormat::Json => OutputFormatArg::Json,
                        that_tools::config::OutputFormat::Compact => OutputFormatArg::Compact,
                        that_tools::config::OutputFormat::Markdown => OutputFormatArg::Markdown,
                        that_tools::config::OutputFormat::Raw => OutputFormatArg::Raw,
                    });

            let ctx = ToolContext {
                max_tokens,
                format: resolved_format,
                config: tools_config,
            };

            match command {
                SessionCommands::Init { session_id } => {
                    let record = that_tools::tools::session::init_session(
                        session_id.clone(),
                        &sessions_path,
                    )?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::Stats { session_id } => {
                    let stats = that_tools::tools::session::get_stats(
                        session_id,
                        &sessions_path,
                        soft_threshold,
                    )?;
                    let budgeted = output::emit_json(&stats, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::AddTokens { session_id, tokens } => {
                    let record = that_tools::tools::session::add_tokens(
                        session_id,
                        *tokens,
                        &sessions_path,
                    )?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                SessionCommands::ResetContext { session_id, to } => {
                    let record =
                        that_tools::tools::session::reset_context(session_id, *to, &sessions_path)?;
                    let budgeted = output::emit_json(&record, ctx.max_tokens);
                    println!("{}", ctx.format_output(&budgeted));
                }
                _ => unreachable!(),
            }
            Ok(())
        }
    }
}

/// Handle merged skill commands.
fn handle_skill_command(
    cli: &Cli,
    command: &SkillCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        // Install comes from that-tools
        SkillCommands::Install { skill, path, force } => {
            let cwd = std::env::current_dir().ok();
            let tools_config = that_tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;
            let max_tokens = cli
                .max_tokens
                .or(Some(tools_config.output.default_max_tokens));

            let installed =
                that_tools::tools::skills::install(skill.as_deref(), path.as_deref(), *force)?;
            let budgeted = output::emit_json(&installed, max_tokens);
            println!(
                "{}",
                serde_json::to_string_pretty(&budgeted.content).unwrap_or(budgeted.content)
            );
            Ok(())
        }
        // Read comes from that-tools
        SkillCommands::Read { skill } => {
            let cwd = std::env::current_dir().ok();
            let tools_config = that_tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;
            let max_tokens = cli
                .max_tokens
                .or(Some(tools_config.output.default_max_tokens));

            match that_tools::tools::skills::read(skill) {
                Some(s) => {
                    let budgeted = output::emit_json(&s, max_tokens);
                    println!("{}", budgeted.content);
                    Ok(())
                }
                None => Err(format!("unknown skill: '{}'", skill).into()),
            }
        }
        SkillCommands::Create {
            name,
            plugin,
            force,
        } => {
            let agent_name = cli.agent.as_deref().ok_or_else(|| {
                "skill create requires --agent to resolve target scope".to_string()
            })?;
            if let Some(plugin_id) = plugin {
                let path = that_plugins::create_plugin_skill_scaffold(
                    agent_name, plugin_id, name, *force,
                )?;
                println!(
                    "Created plugin-scoped skill '{}' for plugin '{}' at {}",
                    name,
                    plugin_id,
                    path.display()
                );
            } else {
                let path =
                    that_core::skills::create_skill_scaffold_local(agent_name, name, *force)?;
                println!(
                    "Created agent-scoped skill '{}' for agent '{}' at {}",
                    name,
                    agent_name,
                    path.display()
                );
            }
            Ok(())
        }
        // List merges both sources
        SkillCommands::List => {
            // List that-tools skills
            let ow_skills = that_tools::tools::skills::list();
            let budgeted = output::emit_json(&ow_skills, None);
            println!("{}", budgeted.content);
            Ok(())
        }
        // Show from that-agent side
        SkillCommands::Show { name } => {
            if let Some(agent_name) = &cli.agent {
                let cmd = that_core::control::cli::SkillCommands::Show { name: name.clone() };
                let ws = that_core::config::WorkspaceConfig::load(cli.workspace.as_deref())
                    .map_err(|e| e.to_string())?;
                let agent = ws.load_agent(agent_name).map_err(|e| e.to_string())?;
                that_core::orchestration::handle_skill_command(&agent, !cli.no_sandbox, cmd)
                    .map_err(|e| e.to_string())?;
            } else {
                // Fall back to that-tools skill read
                match that_tools::tools::skills::read(name) {
                    Some(s) => {
                        let budgeted = output::emit_json(&s, None);
                        println!("{}", budgeted.content);
                    }
                    None => return Err(format!("unknown skill: '{}'", name).into()),
                }
            }
            Ok(())
        }
    }
}

fn api_key_env_var_for_provider(provider: &str) -> anyhow::Result<&'static str> {
    match provider {
        "openai" => Ok("OPENAI_API_KEY"),
        "anthropic" => Ok("ANTHROPIC_API_KEY"),
        "openrouter" => Ok("OPENROUTER_API_KEY"),
        other => anyhow::bail!(
            "Unsupported provider '{other}'. Use 'anthropic', 'openai', or 'openrouter'."
        ),
    }
}

const SECRETS_BLOCK_START: &str = "# >>> that-managed-secrets >>>";
const SECRETS_BLOCK_END: &str = "# <<< that-managed-secrets <<<";

fn validate_secret_key(key: &str) -> anyhow::Result<()> {
    if key.is_empty() {
        anyhow::bail!("Secret key cannot be empty");
    }
    let mut chars = key.chars();
    let first = chars.next().unwrap_or('_');
    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("Invalid secret key '{key}': must start with [A-Za-z_]");
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("Invalid secret key '{key}': use only [A-Za-z0-9_]");
    }
    Ok(())
}

fn quote_bash_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn unquote_bash_single(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("'\"'\"'", "'")
    } else {
        value.to_string()
    }
}

fn parse_export_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("export ")?;
    let (key, raw_value) = rest.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), unquote_bash_single(raw_value.trim())))
}

fn agent_bashrc_path(agent_name: &str) -> anyhow::Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Failed to resolve home directory"))?;
    Ok(home
        .join(".that-agent")
        .join("agents")
        .join(agent_name)
        .join(".bashrc"))
}

fn load_secrets_from_bashrc(content: &str) -> std::collections::BTreeMap<String, String> {
    let mut in_block = false;
    let mut secrets = std::collections::BTreeMap::new();
    for line in content.lines() {
        if line.trim() == SECRETS_BLOCK_START {
            in_block = true;
            continue;
        }
        if line.trim() == SECRETS_BLOCK_END {
            break;
        }
        if in_block {
            if let Some((k, v)) = parse_export_line(line) {
                secrets.insert(k, v);
            }
        }
    }
    secrets
}

fn load_exports_from_bashrc(content: &str) -> std::collections::BTreeMap<String, String> {
    let mut exports = std::collections::BTreeMap::new();
    for line in content.lines() {
        if let Some((k, v)) = parse_export_line(line) {
            exports.insert(k, v);
        }
    }
    exports
}

fn remove_secrets_block(content: &str) -> String {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == SECRETS_BLOCK_START {
            in_block = true;
            continue;
        }
        if trimmed == SECRETS_BLOCK_END {
            in_block = false;
            continue;
        }
        if !in_block {
            out.push(line);
        }
    }
    out.join("\n")
}

fn render_bashrc_with_secrets(
    base_content: &str,
    secrets: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut rendered = remove_secrets_block(base_content);
    if !rendered.trim().is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if !rendered.trim().is_empty() {
        rendered.push('\n');
    }
    rendered.push_str(SECRETS_BLOCK_START);
    rendered.push('\n');
    for (k, v) in secrets {
        rendered.push_str("export ");
        rendered.push_str(k);
        rendered.push('=');
        rendered.push_str(&quote_bash_single(v));
        rendered.push('\n');
    }
    rendered.push_str(SECRETS_BLOCK_END);
    rendered.push('\n');
    rendered
}

fn inject_agent_profile_env(agent_name: &str) -> anyhow::Result<usize> {
    let path = agent_bashrc_path(agent_name)?;
    if !path.exists() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(path)?;
    let exports = load_exports_from_bashrc(&content);
    let count = exports.len();
    for (k, v) in exports {
        std::env::set_var(k, v);
    }
    Ok(count)
}

fn handle_secrets_command(cli: &Cli, command: &SecretsCommands) -> anyhow::Result<()> {
    let ws = that_core::config::WorkspaceConfig::load(cli.workspace.as_deref())?;
    let agent_name =
        required_agent_name_or_exit(cli, &ws, "that --agent <name> secrets <add|delete> ...");
    let bashrc_path = agent_bashrc_path(&agent_name)?;

    if let Some(parent) = bashrc_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = if bashrc_path.exists() {
        std::fs::read_to_string(&bashrc_path)?
    } else {
        String::new()
    };
    let mut secrets = load_secrets_from_bashrc(&existing);

    match command {
        SecretsCommands::Add { key, value } => {
            validate_secret_key(key)?;
            secrets.insert(key.clone(), value.clone());
            let rendered = render_bashrc_with_secrets(&existing, &secrets);
            std::fs::write(&bashrc_path, rendered)?;
            println!(
                "Added secret '{}' for agent '{}' in {}",
                key,
                agent_name,
                bashrc_path.display()
            );
        }
        SecretsCommands::Delete { key } => {
            validate_secret_key(key)?;
            let existed = secrets.remove(key).is_some();
            let rendered = render_bashrc_with_secrets(&existing, &secrets);
            std::fs::write(&bashrc_path, rendered)?;
            if existed {
                println!(
                    "Deleted secret '{}' for agent '{}' in {}",
                    key,
                    agent_name,
                    bashrc_path.display()
                );
            } else {
                println!(
                    "Secret '{}' was not set for agent '{}' (profile updated at {})",
                    key,
                    agent_name,
                    bashrc_path.display()
                );
            }
        }
    }

    Ok(())
}

fn required_agent_name_or_exit(
    cli: &Cli,
    ws: &that_core::config::WorkspaceConfig,
    usage_hint: &str,
) -> String {
    match &cli.agent {
        Some(name) => name.clone(),
        None => {
            let agents = ws.list_agents().unwrap_or_default();
            eprintln!("Error: --agent is required. Specify which agent to use.\n");
            if agents.is_empty() {
                eprintln!(
                    "No agents found. Run 'that agent init <name> --api-key <KEY>' to create one."
                );
            } else {
                eprintln!("Available agents:");
                for name in &agents {
                    eprintln!("  {name}");
                }
                eprintln!("\nUsage: {usage_hint}");
            }
            std::process::exit(1);
        }
    }
}

/// Handle agent orchestration commands (run, agent, status).
async fn handle_agent_orchestration_command(cli: &Cli) -> anyhow::Result<()> {
    let mut ws = that_core::config::WorkspaceConfig::load(cli.workspace.as_deref())?;

    if let Some(workspace) = &cli.workspace {
        ws.workspace = Some(workspace.clone());
    }

    let use_sandbox = !cli.no_sandbox;

    // Commands that don't require an agent
    match &cli.command {
        Commands::Agent { command } => {
            match command {
                cli::AgentCommands::List => {
                    that_core::orchestration::handle_agent_command(
                        &ws,
                        that_core::control::cli::AgentCommands::List,
                    )?;
                }
                cli::AgentCommands::Show { name } => {
                    that_core::orchestration::handle_agent_command(
                        &ws,
                        that_core::control::cli::AgentCommands::Show { name: name.clone() },
                    )?;
                }
                cli::AgentCommands::Delete { name } => {
                    that_core::orchestration::handle_agent_command(
                        &ws,
                        that_core::control::cli::AgentCommands::Delete { name: name.clone() },
                    )?;
                }
                cli::AgentCommands::Init {
                    name,
                    api_key,
                    prompt,
                    force,
                    shared_workspace,
                } => {
                    let mut defaults = that_core::config::AgentDef::default();
                    // Apply env var defaults first (THAT_AGENT_PROVIDER, THAT_AGENT_MODEL,
                    // THAT_AGENT_MAX_TURNS) so k8s ConfigMap values are picked up when
                    // no config file exists yet. CLI flags override env vars below.
                    defaults.apply_env_overrides(None);
                    if let Some(provider) = &cli.provider {
                        defaults.provider = provider.clone();
                    }
                    if let Some(model) = &cli.model {
                        defaults.model = model.clone();
                    }
                    if let Some(max_turns) = cli.max_turns {
                        defaults.max_turns = max_turns;
                    }

                    let env_key = api_key_env_var_for_provider(&defaults.provider)?;
                    let resolved_api_key = if let Some(value) =
                        api_key.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty())
                    {
                        value.to_string()
                    } else {
                        std::env::var(env_key).map_err(|_| {
                            anyhow::anyhow!(
                                "Missing API key for provider '{}'. Pass --api-key or set {} in environment/.env.",
                                defaults.provider,
                                env_key
                            )
                        })?
                    };
                    std::env::set_var(env_key, resolved_api_key);

                    that_core::orchestration::init_workspace(
                        &ws,
                        name,
                        *force,
                        *shared_workspace,
                        &defaults.provider,
                        &defaults.model,
                        defaults.max_turns,
                    )?;

                    if let Some(description) = prompt {
                        let generation_prompt = format!(
                            "The agent name is '{name}'. Use this exact value in the `## Name` section.\n\n{description}"
                        );
                        let (identity_md, soul_md) = that_core::orchestration::generate_soul_md(
                            &defaults.provider,
                            &defaults.model,
                            &generation_prompt,
                        )
                        .await?;

                        that_core::workspace::save_identity_local(name, &identity_md)?;
                        that_core::workspace::save_soul_local(name, &soul_md)?;

                        if let Some(path) = that_core::workspace::identity_md_path_local(name) {
                            println!("Generated identity file at {}", path.display());
                        }
                        if let Some(path) = that_core::workspace::soul_md_path_local(name) {
                            println!("Generated soul file at {}", path.display());
                        }
                    }
                }
                cli::AgentCommands::Status => {
                    let agent_name =
                        required_agent_name_or_exit(cli, &ws, "that --agent <name> agent status");
                    let mut agent = ws.load_agent(&agent_name)?;
                    if let Some(provider) = &cli.provider {
                        agent.provider = provider.clone();
                    }
                    if let Some(model) = &cli.model {
                        agent.model = model.clone();
                    }
                    if let Some(max_turns) = cli.max_turns {
                        agent.max_turns = max_turns;
                    }
                    that_core::orchestration::show_status(&ws, &agent, use_sandbox)?;
                }
                cli::AgentCommands::Skill { command } => {
                    required_agent_name_or_exit(
                        cli,
                        &ws,
                        "that --agent <name> agent skill <subcommand>",
                    );
                    handle_skill_command(cli, command)
                        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                }
                cli::AgentCommands::Plugin { command } => {
                    let agent_name = required_agent_name_or_exit(
                        cli,
                        &ws,
                        "that --agent <name> agent plugin <subcommand>",
                    );
                    let mut agent = ws.load_agent(&agent_name)?;
                    if let Some(provider) = &cli.provider {
                        agent.provider = provider.clone();
                    }
                    if let Some(model) = &cli.model {
                        agent.model = model.clone();
                    }
                    if let Some(max_turns) = cli.max_turns {
                        agent.max_turns = max_turns;
                    }
                    handle_plugin_command(&agent, command)?;
                }
            }
            return Ok(());
        }
        Commands::Init { .. } => anyhow::bail!(
            "'that init' is deprecated. Use 'that agent init <name> --api-key <KEY>' instead."
        ),
        _ => {}
    }

    // Commands below require --agent
    let agent_name = required_agent_name_or_exit(cli, &ws, "that --agent <name> <command>");

    let mut agent = ws.load_agent(&agent_name)?;

    // Apply CLI overrides (highest precedence — explicit user intent at invocation).
    // Env var overrides for model/provider/max_turns are now handled inside
    // AgentDef::apply_env_overrides() as *fallback defaults* — they only apply
    // when config.toml doesn't explicitly set the field. This lets agents change
    // their own model via config.toml edits without the configmap overriding them.
    if let Some(provider) = &cli.provider {
        agent.provider = provider.clone();
    }
    if let Some(model) = &cli.model {
        agent.model = model.clone();
    }
    if let Some(max_turns) = cli.max_turns {
        agent.max_turns = max_turns;
    }

    if let Err(err) = inject_agent_profile_env(&agent_name) {
        tracing::warn!(
            agent = %agent_name,
            error = %err,
            "Failed to load agent profile exports from .bashrc"
        );
    }

    match &cli.command {
        Commands::Run { command } => match command {
            RunCommands::Query {
                task,
                session,
                remote,
                token,
                timeout,
                parent,
                role,
                inherit_workspace,
            } => {
                // Apply hierarchy flags from CLI args
                if let Some(p) = parent {
                    agent.parent = Some(p.clone());
                }
                if let Some(r) = role {
                    agent.role = Some(r.clone());
                }
                if *inherit_workspace {
                    agent.inherit_workspace = true;
                }

                if let Some(url) = remote {
                    that_core::orchestration::run_remote_query(
                        &url,
                        task.clone(),
                        session.as_deref(),
                        token.as_deref(),
                        timeout.unwrap_or(300),
                    )
                    .await?;
                } else {
                    that_core::orchestration::run_task(
                        &ws,
                        &agent,
                        task,
                        session.as_deref(),
                        use_sandbox,
                        cli.debug,
                    )
                    .await?;
                }
            }
            RunCommands::Chat { session } => {
                if cli.no_tui || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    that_core::orchestration::run_chat(
                        &ws,
                        &agent,
                        session.as_deref(),
                        use_sandbox,
                        cli.debug,
                    )
                    .await?;
                } else {
                    that_core::orchestration::run_chat_tui(
                        &ws,
                        &agent,
                        session.as_deref(),
                        use_sandbox,
                        cli.debug,
                    )
                    .await?;
                }
            }
            RunCommands::Listen {
                session: _,
                parent,
                role,
                inherit_workspace,
            } => {
                // Apply hierarchy flags from CLI args
                if let Some(p) = parent {
                    agent.parent = Some(p.clone());
                }
                if let Some(r) = role {
                    agent.role = Some(r.clone());
                }
                if *inherit_workspace {
                    agent.inherit_workspace = true;
                }

                let registry =
                    that_core::that_channels::ChannelFactoryRegistry::with_builtin_adapters();
                let (router, inbound_rx) = registry.build_router(
                    &agent.channels,
                    that_core::that_channels::ChannelBuildMode::Headless,
                )?;
                that_core::orchestration::run_listen(&ws, &agent, use_sandbox, router, inbound_rx)
                    .await?;
            }
        },
        Commands::Status => {
            that_core::orchestration::show_status(&ws, &agent, use_sandbox)?;
        }
        Commands::Plugin { command } => {
            handle_plugin_command(&agent, command)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn handle_plugin_command(
    agent: &that_core::config::AgentDef,
    command: &PluginCommands,
) -> anyhow::Result<()> {
    match command {
        PluginCommands::List => {
            let registry = that_plugins::PluginRegistry::load(&agent.name);
            if registry.plugins.is_empty() {
                println!("No plugins installed for agent '{}'.", agent.name);
                return Ok(());
            }
            println!("Plugins for agent '{}':", agent.name);
            for plugin in &registry.plugins {
                let state = if plugin.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                println!(
                    "  {} ({}) - {}",
                    plugin.manifest.id, plugin.manifest.version, state
                );
            }
            if !registry.load_errors.is_empty() {
                println!("\nLoad warnings:");
                for err in &registry.load_errors {
                    println!("  - {err}");
                }
            }
        }
        PluginCommands::Show { id } => {
            let registry = that_plugins::PluginRegistry::load(&agent.name);
            if let Some(plugin) = registry.get(id) {
                let state = if plugin.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let manifest = toml::to_string_pretty(&plugin.manifest)?;
                println!("# Plugin: {} ({state})", plugin.manifest.id);
                println!("Path: {}", plugin.dir.display());
                println!("{manifest}");
            } else {
                anyhow::bail!("Plugin '{id}' not found for agent '{}'", agent.name);
            }
        }
        PluginCommands::Enable { id } => {
            that_plugins::set_plugin_enabled(&agent.name, id, true)?;
            println!("Enabled plugin '{id}' for agent '{}'.", agent.name);
        }
        PluginCommands::Disable { id } => {
            that_plugins::set_plugin_enabled(&agent.name, id, false)?;
            println!("Disabled plugin '{id}' for agent '{}'.", agent.name);
        }
        PluginCommands::Create { id, force } => {
            let dir = that_plugins::create_plugin_scaffold(&agent.name, id, *force)?;
            println!("Created plugin scaffold at {}", dir.display());
        }
    }
    Ok(())
}

// ── that-tools tool command handlers ──
// These are the tool command handlers.

fn handle_fs(
    ctx: &ToolContext,
    command: &that_tools::cli::FsCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::FsCommands;
    match command {
        FsCommands::Ls { path, max_depth } => {
            ctx.check_policy("fs_read").map_err(|e| e.to_string())?;
            let depth = max_depth.or(Some(ctx.config.output.fs_ls_max_depth));
            let result = that_tools::tools::fs::ls(path, depth, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Cat { path } => {
            ctx.check_policy("fs_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::cat(path, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Write {
            path,
            content,
            dry_run,
            backup,
        } => {
            ctx.check_policy("fs_write").map_err(|e| e.to_string())?;
            let content = match content {
                Some(s) => s.replace("\\n", "\n").replace("\\t", "\t"),
                None => std::io::read_to_string(std::io::stdin())?,
            };
            let result =
                that_tools::tools::fs::write(path, &content, *dry_run, *backup, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Mkdir { path, parents } => {
            ctx.check_policy("fs_write").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::mkdir(path, *parents, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Rm {
            path,
            recursive,
            dry_run,
        } => {
            ctx.check_policy("fs_delete").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::rm(path, *recursive, *dry_run, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}

fn handle_code(
    ctx: &ToolContext,
    command: &that_tools::cli::CodeCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::CodeCommands;
    match command {
        CodeCommands::Read {
            path,
            context,
            symbols,
            line,
            end_line,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let ctx_lines = context.or(Some(ctx.config.output.code_read_context_lines));
            let result = that_tools::tools::code::code_read(
                path,
                ctx_lines,
                *symbols,
                ctx.max_tokens,
                *line,
                *end_line,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Grep {
            pattern,
            path,
            context,
            limit,
            ignore_case,
            regex,
            include,
            exclude,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            // Auto-correct swapped arguments
            let (pattern, path) = if !path.exists() && std::path::Path::new(pattern).exists() {
                (
                    path.to_string_lossy().into_owned(),
                    std::path::PathBuf::from(pattern),
                )
            } else {
                (pattern.clone(), path.clone())
            };
            let result = that_tools::tools::code::code_grep_filtered_with_options(
                &path,
                &pattern,
                *context,
                ctx.max_tokens,
                *limit,
                *ignore_case,
                *regex,
                include,
                exclude,
                that_tools::tools::code::GrepRuntimeOptions {
                    workers: Some(ctx.config.code.grep_workers),
                    mmap_min_bytes: Some(ctx.config.code.mmap_min_bytes),
                },
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Tree {
            path,
            depth,
            compact,
            ranked,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let tree_depth = depth.or(Some(ctx.config.output.code_tree_max_depth));
            let result = that_tools::tools::code::code_tree(
                path,
                tree_depth,
                ctx.max_tokens,
                *compact,
                *ranked,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::Symbols {
            path,
            kind,
            name,
            references,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            handle_symbols(ctx, path, kind.as_deref(), name.as_deref(), *references)
        }
        CodeCommands::Index { path, status } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            handle_index(ctx, path, *status)
        }
        CodeCommands::Edit {
            path,
            patch,
            search,
            replace,
            target_fn,
            new_body,
            whole_file,
            all,
            dry_run,
        } => {
            ctx.check_policy("code_edit").map_err(|e| e.to_string())?;
            handle_edit(
                ctx,
                path,
                *patch,
                search.clone(),
                replace.clone(),
                target_fn.clone(),
                new_body.clone(),
                *whole_file,
                *all,
                *dry_run,
            )
        }
        CodeCommands::Summary { path } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::code::summary::code_summary(path, ctx.max_tokens)
                .map_err(|e| e.to_string())?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        CodeCommands::AstGrep {
            pattern,
            path,
            language,
        } => {
            ctx.check_policy("code_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::code::astgrep::structural_search(
                path,
                pattern,
                language.as_deref(),
                ctx.max_tokens,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}

fn handle_symbols(
    ctx: &ToolContext,
    path: &std::path::Path,
    kind_filter: Option<&str>,
    name_filter: Option<&str>,
    references: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::tools::code::parse;

    let symbols = if path.is_file() {
        let parsed = parse::parse_file(path)?;
        parsed.symbols
    } else {
        let mut all_symbols = Vec::new();
        let walker = ignore::WalkBuilder::new(path)
            .hidden(true)
            .git_ignore(true)
            .build();

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }
            if parse::Language::from_path(entry.path()).is_some() {
                if let Ok(parsed) = parse::parse_file(entry.path()) {
                    for mut sym in parsed.symbols {
                        let rel = entry
                            .path()
                            .strip_prefix(path)
                            .unwrap_or(entry.path())
                            .to_string_lossy();
                        sym.name = format!("{}:{}", rel, sym.name);
                        all_symbols.push(sym);
                    }
                }
            }
        }
        all_symbols
    };

    let filtered: Vec<_> = symbols
        .into_iter()
        .filter(|s| {
            if let Some(k) = kind_filter {
                let kind_str = format!("{:?}", s.kind).to_lowercase();
                if !kind_str.contains(&k.to_lowercase()) {
                    return false;
                }
            }
            if let Some(n) = name_filter {
                if !s.name.to_lowercase().contains(&n.to_lowercase()) {
                    return false;
                }
            }
            true
        })
        .collect();

    if references {
        let start = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let root = that_tools::index::find_tools_root(start).unwrap_or_else(|| start.to_path_buf());
        let db_path = that_tools::index::index_db_path(&root);
        if !db_path.exists() && ctx.config.code.auto_index {
            if let Ok(idx) = that_tools::index::SymbolIndex::open(&db_path) {
                let _ = idx.build(&root);
            }
        }
        if db_path.exists() {
            if let Ok(idx) = that_tools::index::SymbolIndex::open(&db_path) {
                #[derive(serde::Serialize)]
                struct SymbolWithRefs {
                    name: String,
                    kind: String,
                    line_start: usize,
                    line_end: usize,
                    references: Vec<that_tools::index::IndexedReference>,
                }
                let enriched: Vec<SymbolWithRefs> = filtered
                    .iter()
                    .map(|s| {
                        let bare_name = s.name.rsplit(':').next().unwrap_or(&s.name);
                        let refs = idx.query_references(bare_name).unwrap_or_default();
                        SymbolWithRefs {
                            name: s.name.clone(),
                            kind: format!("{:?}", s.kind).to_lowercase(),
                            line_start: s.line_start,
                            line_end: s.line_end,
                            references: refs,
                        }
                    })
                    .collect();
                let result = output::emit_json(&enriched, ctx.max_tokens);
                println!("{}", ctx.format_output(&result));
                return Ok(());
            }
        }
    }

    let result = output::emit_json(&filtered, ctx.max_tokens);
    println!("{}", ctx.format_output(&result));
    Ok(())
}

fn handle_index(
    ctx: &ToolContext,
    path: &std::path::Path,
    status: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let root = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let db_path = that_tools::index::index_db_path(root);

    if status {
        if !db_path.exists() {
            let status = that_tools::index::IndexStatus {
                path: db_path.to_string_lossy().to_string(),
                total_files: 0,
                total_symbols: 0,
                total_refs: 0,
                stale_files: 0,
                schema_version: "none".to_string(),
            };
            let result = output::emit_json(&status, ctx.max_tokens);
            println!("{}", ctx.format_output(&result));
            return Ok(());
        }
        let idx = that_tools::index::SymbolIndex::open(&db_path)?;
        let status = idx.status(root)?;
        let result = output::emit_json(&status, ctx.max_tokens);
        println!("{}", ctx.format_output(&result));
    } else {
        let idx = that_tools::index::SymbolIndex::open(&db_path)?;
        let build_result = idx.build(root)?;
        let result = output::emit_json(&build_result, ctx.max_tokens);
        println!("{}", ctx.format_output(&result));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_edit(
    ctx: &ToolContext,
    path: &std::path::Path,
    patch: bool,
    search: Option<String>,
    replace: Option<String>,
    target_fn: Option<String>,
    new_body: Option<String>,
    whole_file: bool,
    all: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::tools::code::edit;
    use that_tools::tools::code::git;

    let format = if patch {
        edit::EditFormat::UnifiedDiff
    } else if let (Some(s), Some(r)) = (&search, &replace) {
        edit::EditFormat::SearchReplace {
            search: s.replace("\\n", "\n").replace("\\t", "\t"),
            replace: r.replace("\\n", "\n").replace("\\t", "\t"),
            all,
        }
    } else if let (Some(name), Some(body)) = (&target_fn, &new_body) {
        edit::EditFormat::AstNode {
            symbol_name: name.clone(),
            new_body: body.replace("\\n", "\n").replace("\\t", "\t"),
        }
    } else if whole_file {
        edit::EditFormat::WholeFile
    } else {
        return Err(
            "specify one of: --patch, --search/--replace, --fn/--new-body, --whole-file".into(),
        );
    };

    let checkpoint = if ctx.config.code.git_safety && !dry_run {
        match git::create_checkpoint(path, ctx.config.code.git_safety_branch) {
            Ok(cp) => Some(cp),
            Err(e) => {
                tracing::debug!("git checkpoint skipped: {}", e);
                None
            }
        }
    } else {
        None
    };

    let result = edit::code_edit(path, &format, dry_run, ctx.max_tokens);

    match result {
        Ok(output) => {
            // Edit succeeded — stash is intentionally left (no drop API), safety branch already
            // created. The user can clean it up with `git stash drop` / `git branch -D`.
            println!("{}", ctx.format_output(&output));
            Ok(())
        }
        Err(e) => {
            // Edit failed — restore the pre-edit state
            if let Some(cp) = &checkpoint {
                if let Err(re) = git::restore_checkpoint(cp) {
                    tracing::warn!("checkpoint restore failed after edit error: {}", re);
                }
            }
            Err(e.to_string().into())
        }
    }
}

fn handle_mem(
    ctx: &ToolContext,
    command: &that_tools::cli::MemCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::MemCommands;
    ctx.check_policy("memory").map_err(|e| e.to_string())?;
    match command {
        MemCommands::Add {
            content,
            tags,
            source,
            session_id,
        } => {
            let result = that_tools::tools::memory::add(
                content,
                tags,
                source.as_deref(),
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Recall {
            query,
            limit,
            session_id,
        } => {
            let result = that_tools::tools::memory::recall(
                query,
                *limit,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Search {
            query,
            tags,
            limit,
            session_id,
        } => {
            let tag_filter = if tags.is_empty() {
                None
            } else {
                Some(tags.as_slice())
            };
            let result = that_tools::tools::memory::search(
                query,
                tag_filter,
                *limit,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Compact {
            summary,
            session_id,
        } => {
            ctx.check_policy("mem_compact").map_err(|e| e.to_string())?;
            let mut result = that_tools::tools::memory::compact(
                summary,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            if let Some(ref sid) = session_id {
                let sessions_path = if ctx.config.session.sessions_path.is_empty() {
                    that_tools::tools::session::default_sessions_path()
                } else {
                    std::path::PathBuf::from(&ctx.config.session.sessions_path)
                };
                if that_tools::tools::session::reset_context(sid, 0, &sessions_path).is_ok() {
                    result.context_tokens_reset = true;
                }
                if that_tools::tools::session::increment_compaction(sid, &sessions_path).is_ok() {
                    if let Ok(stats) = that_tools::tools::session::get_stats(
                        sid,
                        &sessions_path,
                        ctx.config.session.soft_threshold_tokens,
                    ) {
                        result.compaction_count = Some(stats.compaction_count);
                    }
                }
            }
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Unpin { id } => {
            let result = that_tools::tools::memory::unpin(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Remove { id } => {
            let result = that_tools::tools::memory::remove(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Prune {
            before_days,
            min_access,
        } => {
            let deleted =
                that_tools::tools::memory::prune(*before_days, *min_access, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"pruned": deleted}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Stats => {
            let stats = that_tools::tools::memory::stats(&ctx.config.memory)?;
            let budgeted = output::emit_json(&stats, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Export => {
            let memories = that_tools::tools::memory::export_memories(&ctx.config.memory)?;
            let budgeted = output::emit_json(&memories, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Import => {
            let stdin = std::io::read_to_string(std::io::stdin())?;
            let memories: Vec<that_tools::tools::memory::MemoryEntry> =
                serde_json::from_str(&stdin)?;
            let imported =
                that_tools::tools::memory::import_memories(&memories, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"imported": imported}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}

fn handle_human(
    ctx: &ToolContext,
    command: &that_tools::cli::HumanCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::HumanCommands;
    match command {
        HumanCommands::Ask { message, timeout } => {
            let result = that_tools::tools::human::ask(message, *timeout)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Approve { id, response } => {
            let result = that_tools::tools::human::approve(id, response.as_deref())?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Confirm { id } => {
            let result = that_tools::tools::human::confirm(id)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Pending => {
            let result = that_tools::tools::human::pending()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}

fn handle_search(
    ctx: &ToolContext,
    command: &that_tools::cli::SearchCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::SearchCommands;
    ctx.check_policy("search").map_err(|e| e.to_string())?;
    match command {
        SearchCommands::Query {
            query,
            engine,
            limit,
            no_cache,
        } => {
            let result = that_tools::tools::search::search(
                query,
                engine.as_deref(),
                *limit,
                *no_cache,
                &ctx.config.search,
                ctx.max_tokens,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        SearchCommands::Fetch { urls, mode } => {
            let result = that_tools::tools::search::fetch(urls, mode, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}

fn handle_exec(
    ctx: &ToolContext,
    command: &that_tools::cli::ExecCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::ExecCommands;
    ctx.check_policy("shell_exec").map_err(|e| e.to_string())?;
    match command {
        ExecCommands::Run {
            command,
            cwd,
            timeout,
            signal,
            stream,
        } => {
            let signal_mode = match signal {
                that_tools::cli::SignalModeArg::Graceful => {
                    that_tools::tools::exec::SignalMode::Graceful
                }
                that_tools::cli::SignalModeArg::Immediate => {
                    that_tools::tools::exec::SignalMode::Immediate
                }
            };
            let result = that_tools::tools::exec::exec_with_options(
                command,
                cwd.as_deref().map(|p| p.to_str().unwrap_or(".")),
                *timeout,
                ctx.max_tokens,
                signal_mode,
                *stream,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}

fn handle_daemon(
    ctx: &ToolContext,
    command: &that_tools::cli::DaemonCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::DaemonCommands;
    match command {
        DaemonCommands::Start => {
            let result = that_tools::daemon::start()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Stop => {
            let result = that_tools::daemon::stop()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Status => {
            let result = that_tools::daemon::status();
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
