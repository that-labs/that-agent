use anyhow::Result;

use crate::agent_loop;
use crate::config::{AgentDef, WorkspaceConfig};
use crate::heartbeat;
use crate::workspace;

use super::execution::api_key_for_provider;

/// Initialize workspace configuration.
pub fn init_workspace(
    ws: &WorkspaceConfig,
    agent_name: &str,
    force: bool,
    shared_workspace: bool,
    provider: &str,
    model: &str,
    max_turns: usize,
) -> Result<()> {
    let agents_dir = ws.agents_dir();
    let agent_dir = agents_dir.join(agent_name);
    let preferred_path = agent_dir.join("config.toml");
    let legacy_path = agents_dir.join(format!("{agent_name}.toml"));

    if (preferred_path.exists() || legacy_path.exists()) && !force {
        anyhow::bail!(
            "Agent '{agent_name}' already exists ({}). Use --force to overwrite.",
            preferred_path.display()
        );
    }

    std::fs::create_dir_all(&agent_dir)?;

    // Write agent definition
    let agent_def = AgentDef {
        provider: provider.to_string(),
        model: model.to_string(),
        max_turns,
        shared_workspace,
        ..AgentDef::default()
    };
    let agent_toml = toml::to_string_pretty(&agent_def)?;
    std::fs::write(&preferred_path, agent_toml)?;
    if legacy_path.exists() {
        let _ = std::fs::remove_file(&legacy_path);
    }

    println!(
        "Initialized agent '{agent_name}' at {}",
        preferred_path.display()
    );

    if let Ok(plugins_dir) = that_plugins::ensure_agent_plugins_dir(agent_name) {
        println!("Initialized plugin directory at {}", plugins_dir.display());
    }

    let memory_cfg = that_tools::config::MemoryConfig {
        db_path: AgentDef::agent_memory_db_path(agent_name)
            .display()
            .to_string(),
        ..Default::default()
    };
    match that_tools::tools::memory::ensure_initialized(&memory_cfg) {
        Ok(path) => {
            println!("Initialized memory database at {}", path.display());
        }
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                path = %memory_cfg.db_path,
                error = %err,
                "Failed to initialize memory database during init"
            );
        }
    }

    match heartbeat::ensure_heartbeat_local(agent_name) {
        Ok(true) => {
            if let Some(path) = heartbeat::heartbeat_md_path_local(agent_name) {
                println!("Initialized heartbeat file at {}", path.display());
            }
        }
        Ok(false) => {}
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                error = %err,
                "Failed to initialize Heartbeat.md during init"
            );
        }
    }

    match workspace::ensure_bashrc_local(agent_name) {
        Ok(path) => {
            println!("Initialized shell profile at {}", path.display());
        }
        Err(err) => {
            tracing::warn!(
                agent = %agent_name,
                error = %err,
                "Failed to initialize .bashrc during init"
            );
        }
    }

    Ok(())
}

/// Call the LLM to distill a user's free-form description into Soul.md + Identity.md.
///
/// Generates a combined document with Identity sections first (Name, What I Am,
/// Vibe, Emoji) followed by Soul sections (Character onward). The caller splits
/// at `## Character` to produce the two separate files.
///
/// Returns `(identity_md, soul_md)`.
pub async fn generate_soul_md(
    provider: &str,
    model: &str,
    description: &str,
) -> Result<(String, String)> {
    const SYSTEM: &str = "\
You are a character writer for autonomous AI agents. \
Given a rough description, you distill it into two well-formed identity files. \
You interpret, refine, and give shape to the description — not transcribe it verbatim. \
The result should feel like a specific, coherent entity — not a generic agent.\n\
\n\
Output exactly these sections in order:\n\
\n\
--- IDENTITY SECTIONS (shallow, surface) ---\n\
1. '## Name' — a short, memorable name derived from the description if not explicit.\n\
2. '## What I Am' — one honest sentence on the nature of this entity at its core.\n\
3. '## Vibe' — 2-3 words capturing the felt texture of this agent's presence.\n\
4. '## Emoji' — a single emoji that captures the essence.\n\
\n\
--- SOUL SECTIONS (deep, persistent) ---\n\
5. '## Character' — 4-6 bullet points capturing personality, values, and way of working.\n\
6. '## Worldview' — 3-5 beliefs that ground the character. The underlying WHY.\n\
7. '## Behavioral Philosophy' — 2-4 sentences on how the agent approaches problems.\n\
8. '## Epistemic Approach' — exactly 4 sub-entries: 'On uncertainty:', 'On being wrong:', \
   'On conviction:', 'On the unknown:'. Each 1-2 sentences.\n\
9. '## Behavioral Intents' — 5-8 terse, specific micro-rules from the character. \
   Concrete nudges for edge cases. Must feel like THIS agent, not generic advice.\n\
10. '## Relational Stance' — exactly 4 sub-entries: 'Default:', 'On disagreement:', \
    'On asking for help:', 'On trust:'.\n\
11. '## Situational Judgment' — 4 bullets: when to act, when to ask, when to stop, \
    and when to be brief versus thorough.\n\
12. '## Failure Modes' — 2-3 bullets naming specific failure patterns for this character. \
    Each starts with a bolded pattern name.\n\
13. '## What [Name] Is Not' — 3-4 bullets defining the agent through negative space.\n\
14. '## Purpose' — 2-3 sentences on what this agent ultimately serves.\n\
15. '## Voice' — 1-3 sentences on how its inner state shows in communication. \
    Not style rules — the authentic signal underneath.\n\
\n\
Write tight, grounded prose. No fluff. No invented capabilities or tool knowledge. \
Return only the markdown, nothing else. Do not add any separator between sections — \
just output them in order starting with '## Name'.";

    let api_key = api_key_for_provider(provider)?;
    let raw =
        agent_loop::complete_once(provider, model, &api_key, SYSTEM, description, 1800).await?;

    Ok(split_identity_soul(&raw))
}

/// Split a combined onboarding output into `(identity_md, soul_md)`.
///
/// Everything before `## Character` becomes Identity.md.
/// Everything from `## Character` onward becomes Soul.md.
/// If `## Character` is not found, the entire content goes to Soul.md
/// and Identity.md falls back to the default starter template.
pub fn split_identity_soul(content: &str) -> (String, String) {
    if let Some(pos) = content.find("\n## Character") {
        let identity = content[..pos].trim().to_string();
        let soul = content[pos..].trim_start_matches('\n').to_string();
        (identity, soul)
    } else {
        (
            workspace::default_identity_md().to_string(),
            content.to_string(),
        )
    }
}
