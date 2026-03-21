use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::sandbox::docker::DockerSandboxClient;
use crate::sandbox::kubernetes::KubernetesSandboxClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    Docker,
    Kubernetes,
}

impl SandboxMode {
    pub fn from_env() -> Self {
        match std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_else(|_| "docker".to_string())
            .to_ascii_lowercase()
            .as_str()
        {
            "k8s" | "kubernetes" => Self::Kubernetes,
            _ => Self::Docker,
        }
    }
}

#[derive(Debug, Clone)]
pub enum BackendClient {
    Docker(DockerSandboxClient),
    Kubernetes(KubernetesSandboxClient),
}

impl BackendClient {
    pub async fn connect(agent_name: &str, workspace: &Path) -> Result<Self> {
        match SandboxMode::from_env() {
            SandboxMode::Docker => {
                let client = DockerSandboxClient::connect_sync(agent_name, workspace)?;
                Ok(Self::Docker(client))
            }
            SandboxMode::Kubernetes => {
                let client = KubernetesSandboxClient::connect(agent_name).await?;
                Ok(Self::Kubernetes(client))
            }
        }
    }

    pub fn mode(&self) -> SandboxMode {
        match self {
            Self::Docker(_) => SandboxMode::Docker,
            Self::Kubernetes(_) => SandboxMode::Kubernetes,
        }
    }
}
