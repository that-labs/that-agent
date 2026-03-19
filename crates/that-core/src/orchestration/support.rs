use anyhow::{Context, Result};

use crate::agent_loop::{self, Message};
use crate::config::AgentDef;
use crate::sandbox::SandboxClient;
use crate::session::SessionManager;
use crate::skills;
#[cfg(feature = "tui")]
use crate::tui;
use crate::workspace;

use super::execution::api_key_for_provider;

/// Resolve this agent's public gateway URL from environment variables.
///
/// Resolution order:
/// 1. `THAT_GATEWAY_URL` — explicit override (used by callers that already know the URL)
/// 2. K8s cluster-local service name when `THAT_SANDBOX_MODE` is `k8s`/`kubernetes`
/// 3. `http://localhost:<port>` derived from `THAT_GATEWAY_ADDR`
pub fn resolve_gateway_url() -> String {
    let addr = std::env::var("THAT_GATEWAY_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    std::env::var("THAT_GATEWAY_URL").unwrap_or_else(|_| {
        let port = addr.rsplit(':').next().unwrap_or("8080");
        let mode = std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if mode == "k8s" || mode == "kubernetes" {
            let ns = std::env::var("POD_NAMESPACE")
                .or_else(|_| std::env::var("THAT_SANDBOX_K8S_NAMESPACE"))
                .unwrap_or_else(|_| "default".to_string());
            format!("http://that-agent.{ns}.svc.cluster.local:{port}")
        } else {
            format!("http://localhost:{port}")
        }
    })
}

/// Load all workspace files for the current mode (sandbox or local).
///
/// Also updates the global Status.md cache so the system reminder stays in sync.
pub fn load_workspace_files(agent: &AgentDef, sandbox: bool) -> workspace::WorkspaceFiles {
    let ws = if sandbox {
        let container = SandboxClient::container_name(agent);
        workspace::load_all_sandbox(&container, &agent.name)
    } else {
        workspace::load_all_local(&agent.name)
    };
    super::config::set_agent_status(ws.status.clone());
    ws
}

/// Extract the agent's compaction instructions from `Agents.md`.
///
/// If the agent has written a `## Compaction` section, its content is used
/// as the summarization system prompt — letting the agent shape what gets
/// preserved across session boundaries.
pub fn extract_compaction_instructions(agent_name: &str, sandbox: bool) -> Option<String> {
    let agents_md = if sandbox {
        // In sandbox mode, Agents.md lives inside the container — not easily
        // readable here. Fall back to default instructions.
        return None;
    } else {
        let path = dirs::home_dir()?
            .join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("Agents.md");
        std::fs::read_to_string(path).ok()?
    };
    // Find the ## Compaction heading and extract everything until the next ## heading.
    let start = agents_md.find("## Compaction")?;
    let body_start = agents_md[start..].find('\n').map(|i| start + i + 1)?;
    let end = agents_md[body_start..]
        .find("\n## ")
        .map(|i| body_start + i)
        .unwrap_or(agents_md.len());
    let section = agents_md[body_start..end].trim();
    if section.is_empty() {
        None
    } else {
        Some(section.to_string())
    }
}

/// Build a concise LLM-generated summary of the conversation history.
///
/// Uses the agent's `## Compaction` section from `Agents.md` as the
/// summarization system prompt. If no section exists, falls back to a
/// simple turn-count string — the agent must write the prompt to get
/// meaningful summaries.
pub async fn build_compact_summary(
    provider: &str,
    model: &str,
    agent_name: &str,
    sandbox: bool,
    history: &[Message],
) -> String {
    let Some(system) = extract_compaction_instructions(agent_name, sandbox) else {
        return fallback_summary(history);
    };

    // Build a transcript for the LLM to summarize.
    let mut transcript = String::new();
    for msg in history {
        match msg {
            Message::User { content, .. } => {
                transcript.push_str("User: ");
                transcript.push_str(content);
                transcript.push('\n');
            }
            Message::Assistant { content, .. } => {
                if !content.is_empty() {
                    transcript.push_str("Assistant: ");
                    transcript.push_str(content);
                    transcript.push('\n');
                }
            }
            Message::Tool { name, content, .. } => {
                // Include tool results briefly for context.
                let preview: String = content.chars().take(200).collect();
                transcript.push_str(&format!("[Tool {name}: {preview}]\n"));
            }
        }
    }

    // Truncate to avoid blowing up the summarization prompt.
    let truncated: String = transcript.chars().take(12_000).collect();
    let prompt = format!("Summarize this conversation:\n\n{truncated}");

    match api_key_for_provider(provider) {
        Ok(api_key) => {
            match agent_loop::complete_once(provider, model, &api_key, &system, &prompt, 500).await
            {
                Ok(summary) if !summary.trim().is_empty() => summary.trim().to_string(),
                Ok(_) | Err(_) => fallback_summary(history),
            }
        }
        Err(_) => fallback_summary(history),
    }
}

pub fn fallback_summary(history: &[Message]) -> String {
    let user_turns = history
        .iter()
        .filter(|m| matches!(m, Message::User { .. }))
        .count();
    format!("Session with {user_turns} user turn(s)")
}

/// Compact session memory by calling that_tools directly (no subprocess).
///
/// Memory always lives on the host regardless of sandbox mode — no docker exec needed.
pub async fn compact_session(
    _container: Option<&str>,
    session_id: &str,
    summary: &str,
) -> Result<String> {
    use that_tools::tools::dispatch::{execute_tool, ToolRequest};

    let mut config = that_tools::config::load_config(None).unwrap_or_default();
    // Override policy so compaction is never blocked by a Prompt fallback.
    config.policy.default = that_tools::config::PolicyLevel::Allow;

    let request = ToolRequest::MemCompact {
        summary: summary.to_string(),
        session_id: Some(session_id.to_string()),
    };

    let resp = tokio::task::spawn_blocking(move || execute_tool(&config, &request, None))
        .await
        .context("Failed to run mem compact")?;

    if resp.success {
        Ok(resp
            .output
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("Session compacted.")
            .to_string())
    } else {
        let err = resp
            .output
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("compact failed");
        Err(anyhow::anyhow!("{}", err))
    }
}

/// Show current status and configuration.
pub fn show_status(
    ws: &crate::config::WorkspaceConfig,
    agent: &AgentDef,
    sandbox: bool,
) -> Result<()> {
    println!("that-agent v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!(
        "Workspace:      {}",
        ws.workspace
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(current dir)".into())
    );
    println!("Default agent:  {}", ws.default_agent);
    println!("Provider:       {}", agent.provider);
    println!("Model:          {}", agent.model);
    println!("Max turns:      {}", agent.max_turns);
    println!(
        "Sandbox:        {}",
        if sandbox { "enabled" } else { "disabled" }
    );
    println!("Tool:           bash (native)");

    let state_dir = ws.resolve_state_dir()?;
    println!();
    println!("State dir:  {}", state_dir.display());

    if let Ok(session_mgr) = SessionManager::new(&state_dir) {
        if let Ok(sessions) = session_mgr.list_sessions() {
            println!("Sessions:   {}", sessions.len());
        }
    }
    let plugins = that_plugins::PluginRegistry::load(&agent.name);
    println!("Plugins:    {}", plugins.enabled_plugins().count());

    // Show available agents
    if let Ok(agents) = ws.list_agents() {
        println!();
        println!("Agents:");
        for name in &agents {
            let marker = if name == &ws.default_agent { " *" } else { "" };
            println!("  {name}{marker}");
        }
    }

    Ok(())
}

/// Build command palette entries from built-ins, enabled plugin commands, and discovered skills.
#[cfg(feature = "tui")]
pub fn build_palette_commands(
    skills: &[skills::SkillMeta],
    plugin_commands: &[that_plugins::ResolvedPluginCommand],
) -> Vec<tui::CommandEntry> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut commands = vec![
        tui::CommandEntry {
            name: "/model".into(),
            description: "select or change model".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/resume".into(),
            description: "resume a session".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/usage".into(),
            description: "show usage stats".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/skills".into(),
            description: "browse and manage skills".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/help".into(),
            description: "show help".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/compact".into(),
            description: "compact and save session to memory".into(),
            is_skill: false,
        },
        tui::CommandEntry {
            name: "/stop".into(),
            description: "stop the active run".into(),
            is_skill: false,
        },
    ];
    for command in &commands {
        seen.insert(command.name.clone());
    }

    for plugin_cmd in plugin_commands {
        let name = format!("/{}", plugin_cmd.command);
        if !seen.insert(name.clone()) {
            continue;
        }
        commands.push(tui::CommandEntry {
            name,
            description: plugin_cmd.description.clone(),
            is_skill: false,
        });
    }

    for skill in skills {
        let name = format!("/{}", skill.name);
        if !seen.insert(name.clone()) {
            continue;
        }
        commands.push(tui::CommandEntry {
            name,
            description: skill.description.clone(),
            is_skill: true,
        });
    }

    commands
}
