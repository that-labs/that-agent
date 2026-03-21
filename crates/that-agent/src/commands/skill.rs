use crate::cli::{self, SkillCommands};
use crate::tools::output;

/// Handle merged skill commands.
pub fn handle_skill_command(
    cli: &cli::Cli,
    command: &SkillCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        // Install comes from that-tools
        SkillCommands::Install { skill, path, force } => {
            let cwd = std::env::current_dir().ok();
            let tools_config = crate::tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;
            let max_tokens = cli
                .max_tokens
                .or(Some(tools_config.output.default_max_tokens));

            let installed =
                crate::tools::impls::skills::install(skill.as_deref(), path.as_deref(), *force)?;
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
            let tools_config = crate::tools::config::load_config(cwd.as_deref())
                .map_err(|e| format!("config load failed: {}", e))?;
            let max_tokens = cli
                .max_tokens
                .or(Some(tools_config.output.default_max_tokens));

            match crate::tools::impls::skills::read(skill) {
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
                let path = crate::plugins::create_plugin_skill_scaffold(
                    agent_name, plugin_id, name, *force,
                )?;
                println!(
                    "Created plugin-scoped skill '{}' for plugin '{}' at {}",
                    name,
                    plugin_id,
                    path.display()
                );
            } else {
                let path = crate::skills::create_skill_scaffold_local(agent_name, name, *force)?;
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
            let ow_skills = crate::tools::impls::skills::list();
            let budgeted = output::emit_json(&ow_skills, None);
            println!("{}", budgeted.content);
            Ok(())
        }
        // Show from that-agent side
        SkillCommands::Show { name } => {
            if let Some(agent_name) = &cli.agent {
                let cmd = crate::control::cli::SkillCommands::Show { name: name.clone() };
                let ws = crate::config::WorkspaceConfig::load(cli.workspace.as_deref())
                    .map_err(|e| e.to_string())?;
                let agent = ws.load_agent(agent_name).map_err(|e| e.to_string())?;
                crate::orchestration::handle_skill_command(&agent, !cli.no_sandbox, cmd)
                    .map_err(|e| e.to_string())?;
            } else {
                // Fall back to that-tools skill read
                match crate::tools::impls::skills::read(name) {
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
