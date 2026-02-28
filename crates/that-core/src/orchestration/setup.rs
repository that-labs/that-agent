use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use crate::config::{AgentDef, WorkspaceConfig};
use crate::default_skills;
use crate::sandbox::SandboxClient;
use crate::skills;

/// Resolve the effective workspace directory for an agent.
///
/// Priority:
/// 1. `inherit_workspace` — use the parent agent's workspace directory
/// 2. `shared_workspace` — use the global workspace (current behavior)
/// 3. Default — isolated per-agent workspace
pub fn resolve_agent_workspace(ws: &WorkspaceConfig, agent: &AgentDef) -> Result<PathBuf> {
    if agent.inherit_workspace {
        // Inherit the parent's workspace directory
        let dir = if let Some(parent_name) = &agent.parent {
            AgentDef::agent_workspace_dir(parent_name)
        } else {
            // No parent specified — fall back to own workspace
            AgentDef::agent_workspace_dir(&agent.name)
        };
        std::fs::create_dir_all(&dir).with_context(|| {
            format!("Failed to create inherited workspace at {}", dir.display())
        })?;
        Ok(dir)
    } else if agent.shared_workspace {
        // Use the global workspace (current behavior)
        let ws_path = ws.workspace.clone().unwrap_or_else(|| PathBuf::from("."));
        Ok(ws_path)
    } else {
        // Isolated per-agent workspace
        let dir = AgentDef::agent_workspace_dir(&agent.name);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create agent workspace at {}", dir.display()))?;
        Ok(dir)
    }
}

/// Ensure the sandbox container is running and return the container name.
/// In local mode, returns None.
pub async fn prepare_container(
    agent: &AgentDef,
    workspace: &Path,
    sandbox: bool,
) -> Result<Option<String>> {
    // Always install skills on the host — ReadSkillTool reads from the host regardless of mode.
    default_skills::install_default_skills(&agent.name);
    install_that_tools_skills_local(&agent.name);

    if sandbox {
        info!(agent = %agent.name, "Preparing Docker sandbox container");
        let sc = SandboxClient::connect(agent, workspace).await?;
        Ok(Some(sc.container_name))
    } else {
        Ok(None)
    }
}

/// Install that-tools skills in the agent skills directory.
///
/// This runs in-process (no shelling out to `that`) so it is deterministic even
/// when PATH contains an older binary. A legacy skill directory is removed
/// during install migration.
pub fn install_that_tools_skills_local(agent_name: &str) {
    let Some(skills_dir) = skills::skills_dir_local(agent_name) else {
        return;
    };

    fn legacy_skill_dir_name() -> String {
        ['o', 'w', 'a', 'n', 'a', 'i'].iter().collect()
    }

    let legacy_dir = skills_dir.join(legacy_skill_dir_name());
    if legacy_dir.exists() {
        if let Err(err) = std::fs::remove_dir_all(&legacy_dir) {
            tracing::warn!(
                agent = %agent_name,
                path = %legacy_dir.display(),
                error = %err,
                "Failed to remove legacy skill directory"
            );
        } else {
            tracing::info!(
                agent = %agent_name,
                path = %legacy_dir.display(),
                "Removed legacy skill directory"
            );
        }
    }

    match that_tools::tools::skills::install(None, Some(&skills_dir), true) {
        Ok(_) => info!(agent = %agent_name, "Installed that-tools skills locally"),
        Err(err) => tracing::warn!(
            agent = %agent_name,
            error = %err,
            "Failed to install that-tools skills locally"
        ),
    }
}
