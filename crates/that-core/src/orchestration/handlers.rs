use anyhow::Result;

use crate::config::{AgentDef, WorkspaceConfig};
use crate::control::cli::{AgentCommands, SessionCommands, SkillCommands};
use crate::default_skills;
use crate::sandbox::SandboxClient;
use crate::session::{SessionManager, TranscriptEvent};

use super::discovery::discover_skills;
use super::setup::install_that_tools_skills_local;

/// Handle session management commands.
pub fn handle_session_command(ws: &WorkspaceConfig, command: SessionCommands) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    match command {
        SessionCommands::List => {
            let sessions = session_mgr.list_sessions()?;
            if sessions.is_empty() {
                println!("No sessions found.");
            } else {
                println!("Sessions (newest first):");
                for id in sessions {
                    println!("  {id}");
                }
            }
        }
        SessionCommands::Show { id } => {
            let entries = session_mgr.read_transcript(&id)?;
            for entry in entries {
                let ts = entry.timestamp.format("%H:%M:%S");
                match &entry.event {
                    TranscriptEvent::RunStart { task } => {
                        println!("\n[{ts}] ── RUN START ──────────────────────");
                        println!("{task}");
                    }
                    TranscriptEvent::UserMessage { content } => {
                        println!("\n[{ts}] USER");
                        println!("{content}");
                    }
                    TranscriptEvent::AssistantMessage { content } => {
                        println!("\n[{ts}] AGENT");
                        println!("{content}");
                    }
                    TranscriptEvent::ToolCall { tool, arguments } => {
                        let args_str = serde_json::to_string(arguments).unwrap_or_default();
                        let args_preview: String = args_str.chars().take(300).collect();
                        let ellipsis = if args_str.chars().count() > 300 {
                            "…"
                        } else {
                            ""
                        };
                        println!("\n[{ts}] TOOL CALL  {tool}");
                        println!("{args_preview}{ellipsis}");
                    }
                    TranscriptEvent::ToolResult {
                        tool,
                        result,
                        is_error,
                    } => {
                        let prefix = if *is_error { "ERROR" } else { "OK" };
                        let preview: String = result.chars().take(500).collect();
                        let ellipsis = if result.chars().count() > 500 {
                            "…"
                        } else {
                            ""
                        };
                        println!("\n[{ts}] TOOL RESULT  {tool} [{prefix}]");
                        println!("{preview}{ellipsis}");
                    }
                    TranscriptEvent::RunEnd { status, error } => {
                        let msg = error.as_deref().unwrap_or("");
                        println!("\n[{ts}] ── RUN END: {status:?} {msg}");
                    }
                    TranscriptEvent::Compaction { summary } => {
                        println!("\n[{ts}] COMPACTION: {summary}");
                    }
                    TranscriptEvent::Usage {
                        input_tokens,
                        output_tokens,
                        model,
                        provider,
                        ..
                    } => {
                        println!(
                            "\n[{ts}] USAGE  {input_tokens}↑ {output_tokens}↓  {model}@{provider}"
                        );
                    }
                    TranscriptEvent::Restart {
                        interrupted_run_id,
                        last_task,
                        last_tool,
                        ..
                    } => {
                        let run = interrupted_run_id.as_deref().unwrap_or("unknown");
                        println!("\n[{ts}] ── RESTART (interrupted run: {run}) ──");
                        if let Some(task) = last_task {
                            println!("  task: {task}");
                        }
                        if let Some(tool) = last_tool {
                            println!("  last tool: {tool}");
                        }
                    }
                }
            }
        }
        SessionCommands::New => {
            let id = session_mgr.create_session()?;
            println!("Created session: {id}");
        }
    }

    Ok(())
}

/// Handle agent management commands.
pub fn handle_agent_command(ws: &WorkspaceConfig, command: AgentCommands) -> Result<()> {
    match command {
        AgentCommands::List => {
            let agents = ws.list_agents()?;
            if agents.is_empty() {
                println!(
                    "No agents found. Run 'that agent init <name> --api-key <KEY>' to create one."
                );
            } else {
                println!("Available agents:");
                for name in &agents {
                    let marker = if name == &ws.default_agent { " *" } else { "" };
                    println!("  {name}{marker}");
                }
            }
        }
        AgentCommands::Show { name } => {
            let agent = ws.load_agent(&name)?;
            let toml_str = toml::to_string_pretty(&agent)?;
            println!("# Agent: {name}");
            println!("{toml_str}");
        }
        AgentCommands::Delete { name } => {
            let agents_dir = ws.agents_dir();
            let preferred_dir = agents_dir.join(&name);
            let preferred_path = preferred_dir.join("config.toml");
            let legacy_path = agents_dir.join(format!("{name}.toml"));

            let mut removed_any = false;
            if preferred_dir.exists() {
                std::fs::remove_dir_all(&preferred_dir)?;
                println!("Removed agent directory: {}", preferred_dir.display());
                removed_any = true;
            } else if preferred_path.exists() {
                std::fs::remove_file(&preferred_path)?;
                println!("Removed agent config: {}", preferred_path.display());
                removed_any = true;
            }
            if legacy_path.exists() {
                std::fs::remove_file(&legacy_path)?;
                println!("Removed legacy agent config: {}", legacy_path.display());
                removed_any = true;
            }
            if !removed_any {
                anyhow::bail!("Agent '{name}' not found in {}", agents_dir.display());
            }

            // Remove isolated workspace if it exists
            let workspace_dir = AgentDef::agent_workspace_dir(&name);
            if workspace_dir.exists() {
                std::fs::remove_dir_all(&workspace_dir)?;
                println!("Removed agent workspace: {}", workspace_dir.display());
            }

            // Stop and remove the sandbox container and its home volume
            let dummy_agent = AgentDef {
                name: name.clone(),
                ..Default::default()
            };
            SandboxClient::remove(&dummy_agent);
            SandboxClient::remove_home_volume(&dummy_agent);

            println!("Agent '{name}' deleted.");
        }
    }

    Ok(())
}

/// Handle skill management commands.
pub fn handle_skill_command(agent: &AgentDef, sandbox: bool, command: SkillCommands) -> Result<()> {
    // Keep bundled bootstrap skills (including that-plugins) in sync for
    // direct skill CLI calls such as `skill show`.
    default_skills::install_default_skills(&agent.name);
    install_that_tools_skills_local(&agent.name);
    let found = discover_skills(agent, sandbox);

    match command {
        SkillCommands::List => {
            if found.is_empty() {
                println!("No skills found.");
            } else {
                println!("Available skills:");
                for skill in &found {
                    println!("  {} — {}", skill.name, skill.description);
                }
            }
        }
        SkillCommands::Show { name } => {
            let content = found
                .iter()
                .find(|s| s.name == name.as_str())
                .and_then(|s| std::fs::read_to_string(&s.path).ok());

            match content {
                Some(text) => println!("{text}"),
                None => anyhow::bail!("Skill '{name}' not found."),
            }
        }
    }

    Ok(())
}
