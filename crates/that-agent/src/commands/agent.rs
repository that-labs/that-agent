use crate::cli::{self, PluginCommands};

/// Lightweight pre-flight checks before an agent run. Warns but does not bail.
fn preflight_checks(agent: &crate::config::AgentDef, sandbox: bool) {
    if dirs::home_dir().is_none() {
        tracing::warn!("$HOME is not set — agent state may be written to the current directory");
    }

    let mode = std::env::var("THAT_SANDBOX_MODE")
        .unwrap_or_else(|_| "docker".to_string())
        .to_ascii_lowercase();

    if sandbox
        && mode == "docker"
        && std::process::Command::new("docker")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_err()
    {
        tracing::warn!(
            "Docker CLI not found — sandbox mode requires Docker. Run with --no-sandbox or install Docker."
        );
    }

    let has_key = matches!(
        agent.provider.as_str(),
        "anthropic"
            if std::env::var("ANTHROPIC_API_KEY").is_ok()
                || std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok()
    ) || matches!(
        agent.provider.as_str(),
        "openai" if std::env::var("OPENAI_API_KEY").is_ok()
    ) || matches!(
        agent.provider.as_str(),
        "openrouter" if std::env::var("OPENROUTER_API_KEY").is_ok()
    );
    if !has_key {
        tracing::warn!(
            provider = %agent.provider,
            "No API key found for provider — set the appropriate environment variable"
        );
    }
}

fn required_agent_name_or_exit(
    cli: &cli::Cli,
    ws: &crate::config::WorkspaceConfig,
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

fn inject_agent_profile_env(agent_name: &str) -> anyhow::Result<usize> {
    let mut count = 0;
    // Load non-secret exports from .bashrc
    let path = super::secrets::agent_bashrc_path(agent_name)?;
    if path.exists() {
        let content = std::fs::read_to_string(path)?;
        let exports = super::secrets::load_exports_from_bashrc(&content);
        count += exports.len();
        for (k, v) in exports {
            std::env::set_var(k, v);
        }
    }
    // Load encrypted secrets (migrates from bashrc on first call)
    let secrets = super::secrets::load_agent_secrets(agent_name)?;
    count += secrets.len();
    for (k, v) in secrets {
        std::env::set_var(k, v);
    }
    Ok(count)
}

/// Handle agent orchestration commands (run, agent, status).
pub async fn handle_agent_orchestration_command(cli: &cli::Cli) -> anyhow::Result<()> {
    let mut ws = crate::config::WorkspaceConfig::load(cli.workspace.as_deref())?;

    if let Some(workspace) = &cli.workspace {
        ws.workspace = Some(workspace.clone());
    }

    let use_sandbox = !cli.no_sandbox;

    // Commands that don't require an agent
    match &cli.command {
        cli::Commands::Agent { command } => {
            match command {
                cli::AgentCommands::List => {
                    crate::orchestration::handle_agent_command(
                        &ws,
                        crate::control::cli::AgentCommands::List,
                    )?;
                }
                cli::AgentCommands::Show { name } => {
                    crate::orchestration::handle_agent_command(
                        &ws,
                        crate::control::cli::AgentCommands::Show { name: name.clone() },
                    )?;
                }
                cli::AgentCommands::Delete { name } => {
                    crate::orchestration::handle_agent_command(
                        &ws,
                        crate::control::cli::AgentCommands::Delete { name: name.clone() },
                    )?;
                }
                cli::AgentCommands::Init {
                    name,
                    api_key,
                    prompt,
                    force,
                    shared_workspace,
                } => {
                    let mut defaults = crate::config::AgentDef::default();
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

                    // If --api-key was passed, inject it into the correct env var so
                    // api_key_for_provider (called later in execution) picks it up.
                    if let Some(value) =
                        api_key.as_ref().map(|v| v.trim()).filter(|v| !v.is_empty())
                    {
                        let env_key = match defaults.provider.as_str() {
                            "anthropic" => {
                                if crate::auth::is_anthropic_oauth_token(value) {
                                    "CLAUDE_CODE_OAUTH_TOKEN"
                                } else {
                                    "ANTHROPIC_API_KEY"
                                }
                            }
                            "openai" => "OPENAI_API_KEY",
                            "openrouter" => "OPENROUTER_API_KEY",
                            other => anyhow::bail!(
                                "Unsupported provider '{other}'. Use 'anthropic', 'openai', or 'openrouter'."
                            ),
                        };
                        std::env::set_var(env_key, value);
                    } else {
                        // Validate a key is available before proceeding.
                        crate::orchestration::execution::api_key_for_provider(&defaults.provider)?;
                    }

                    crate::orchestration::init_workspace(
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
                        let (identity_md, soul_md) = crate::orchestration::generate_soul_md(
                            &defaults.provider,
                            &defaults.model,
                            &generation_prompt,
                        )
                        .await?;

                        crate::workspace::save_identity_local(name, &identity_md)?;
                        crate::workspace::save_soul_local(name, &soul_md)?;

                        if let Some(path) = crate::workspace::identity_md_path_local(name) {
                            println!("Generated identity file at {}", path.display());
                        }
                        if let Some(path) = crate::workspace::soul_md_path_local(name) {
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
                    crate::orchestration::show_status(&ws, &agent, use_sandbox)?;
                }
                cli::AgentCommands::Skill { command } => {
                    required_agent_name_or_exit(
                        cli,
                        &ws,
                        "that --agent <name> agent skill <subcommand>",
                    );
                    super::skill::handle_skill_command(cli, command)
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
        cli::Commands::Init { .. } => anyhow::bail!(
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

    preflight_checks(&agent, !cli.no_sandbox);

    match &cli.command {
        cli::Commands::Run { command } => match command {
            cli::RunCommands::Query {
                task,
                task_file,
                session,
                remote,
                token,
                timeout,
                parent,
                role,
                inherit_workspace,
            } => {
                // Resolve task: --task-file takes priority over positional arg
                let resolved_task = if let Some(path) = task_file {
                    std::fs::read_to_string(path)
                        .map_err(|e| anyhow::anyhow!("failed to read task file: {e}"))?
                } else if let Some(t) = task {
                    t.clone()
                } else {
                    anyhow::bail!("either a task argument or --task-file is required");
                };

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
                    crate::orchestration::run_remote_query(
                        url,
                        resolved_task.clone(),
                        session.as_deref(),
                        token.as_deref(),
                        timeout.unwrap_or(300),
                    )
                    .await?;
                } else {
                    crate::orchestration::run_task(
                        &ws,
                        &agent,
                        &resolved_task,
                        session.as_deref(),
                        use_sandbox,
                        cli.debug,
                    )
                    .await?;
                }
            }
            cli::RunCommands::Chat { session } => {
                #[cfg(feature = "tui")]
                if !cli.no_tui && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    crate::orchestration::run_chat_tui(
                        &ws,
                        &agent,
                        session.as_deref(),
                        use_sandbox,
                        cli.debug,
                    )
                    .await?;
                    return Ok(());
                }
                crate::orchestration::run_chat(
                    &ws,
                    &agent,
                    session.as_deref(),
                    use_sandbox,
                    cli.debug,
                )
                .await?;
            }
            cli::RunCommands::Listen {
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

                let registry = crate::channels::ChannelFactoryRegistry::with_builtin_adapters();
                let (router, inbound_rx) = registry.build_router(
                    &agent.channels,
                    crate::channels::ChannelBuildMode::Headless,
                    None,
                )?;
                crate::orchestration::run_listen(&ws, &agent, use_sandbox, router, inbound_rx)
                    .await?;
            }
        },
        cli::Commands::Status => {
            crate::orchestration::show_status(&ws, &agent, use_sandbox)?;
        }
        cli::Commands::Plugin { command } => {
            handle_plugin_command(&agent, command)?;
        }
        _ => unreachable!(),
    }

    Ok(())
}

pub fn handle_plugin_command(
    agent: &crate::config::AgentDef,
    command: &PluginCommands,
) -> anyhow::Result<()> {
    match command {
        PluginCommands::List => {
            let registry = crate::plugins::PluginRegistry::load(&agent.name);
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
            let registry = crate::plugins::PluginRegistry::load(&agent.name);
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
            crate::plugins::set_plugin_enabled(&agent.name, id, true)?;
            println!("Enabled plugin '{id}' for agent '{}'.", agent.name);
        }
        PluginCommands::Disable { id } => {
            crate::plugins::set_plugin_enabled(&agent.name, id, false)?;
            println!("Disabled plugin '{id}' for agent '{}'.", agent.name);
        }
        PluginCommands::Create { id, force } => {
            let dir = crate::plugins::create_plugin_scaffold(&agent.name, id, *force)?;
            println!("Created plugin scaffold at {}", dir.display());
        }
    }
    Ok(())
}
