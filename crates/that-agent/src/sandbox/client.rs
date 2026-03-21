use std::path::Path;

use anyhow::{Context, Result};

use crate::config::AgentDef;

/// Compatibility wrapper around the shared sandbox crate.
///
/// The current `that-core` runtime still expects a Docker container name for
/// tool routing, so this adapter keeps the same API while delegating all Docker
/// lifecycle logic to `that-sandbox`.
pub struct SandboxClient {
    pub container_name: String,
}

impl SandboxClient {
    pub fn container_name(agent: &AgentDef) -> String {
        crate::sandbox::docker::DockerSandboxClient::container_name(&agent.name)
    }

    pub fn home_volume_name(agent: &AgentDef) -> String {
        crate::sandbox::docker::DockerSandboxClient::home_volume_name(&agent.name)
    }

    pub async fn connect(agent: &AgentDef, workspace: &Path) -> Result<Self> {
        let mode = crate::sandbox::backend::SandboxMode::from_env();
        if mode != crate::sandbox::backend::SandboxMode::Docker {
            anyhow::bail!(
                "that-core runtime currently supports Docker sandbox mode only. \
                 Set THAT_SANDBOX_MODE=docker to run the agent loop."
            );
        }

        let agent_name = agent.name.clone();
        let workspace = workspace.to_path_buf();
        let inner = tokio::task::spawn_blocking(move || {
            crate::sandbox::docker::DockerSandboxClient::connect_sync(&agent_name, &workspace)
        })
        .await
        .context("sandbox connect task panicked")??;
        Ok(Self {
            container_name: inner.container_name,
        })
    }

    pub fn remove(agent: &AgentDef) {
        crate::sandbox::docker::DockerSandboxClient::remove(&agent.name);
    }

    pub fn remove_home_volume(agent: &AgentDef) {
        crate::sandbox::docker::DockerSandboxClient::remove_home_volume(&agent.name);
    }
}
