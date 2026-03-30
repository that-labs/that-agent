use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::info;

use crate::config::{AgentDef, WorkspaceConfig};
use crate::default_skills;
use crate::sandbox::SandboxClient;

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
    let name1 = agent.name.clone();
    tokio::task::spawn_blocking(move || default_skills::install_default_skills(&name1)).await?;

    if sandbox {
        let mode = crate::sandbox::backend::SandboxMode::from_env();
        if mode == crate::sandbox::backend::SandboxMode::Kubernetes {
            // Already running inside a Kubernetes pod — the pod is the sandbox.
            // Tools route via THAT_SANDBOX_MODE; no Docker container needed.
            info!(agent = %agent.name, "Kubernetes sandbox mode — pod is the sandbox, skipping container setup");
            Ok(None)
        } else {
            info!(agent = %agent.name, "Preparing Docker sandbox container");
            let sc = SandboxClient::connect(agent, workspace).await?;
            Ok(Some(sc.container_name))
        }
    } else {
        Ok(None)
    }
}
