use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::process::Command;

use super::{PluginDeploy, PluginManifest};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployStatus {
    Pending,
    Deploying,
    Running,
    Degraded,
    Stopped,
    Failed(String),
}

#[async_trait]
pub trait DeployBackend: Send + Sync {
    async fn deploy(&self, manifest: &PluginManifest) -> Result<()>;
    async fn undeploy(&self, id: &str) -> Result<()>;
    async fn status(&self, id: &str) -> Result<DeployStatus>;
}

pub struct DockerComposeBackend {
    pub plugin_dir: PathBuf,
}

pub struct KubernetesBackend {
    pub namespace: String,
    pub kustomize_dir: Option<String>,
}

/// Resolve the K8s namespace from env: `POD_NAMESPACE` → `THAT_SANDBOX_K8S_NAMESPACE` → `"default"`.
pub fn resolve_k8s_namespace() -> String {
    std::env::var("POD_NAMESPACE")
        .or_else(|_| std::env::var("THAT_SANDBOX_K8S_NAMESPACE"))
        .unwrap_or_else(|_| "default".into())
}

pub struct LocalBackend;

pub fn backend_for(deploy: &PluginDeploy, plugin_dir: &Path) -> Box<dyn DeployBackend> {
    match deploy.kind.as_str() {
        "docker-compose" => Box::new(DockerComposeBackend {
            plugin_dir: plugin_dir.to_path_buf(),
        }),
        "kubernetes" => Box::new(KubernetesBackend {
            namespace: resolve_k8s_namespace(),
            kustomize_dir: deploy.kustomize_dir.clone(),
        }),
        _ => Box::new(LocalBackend),
    }
}

#[async_trait]
impl DeployBackend for DockerComposeBackend {
    async fn deploy(&self, manifest: &PluginManifest) -> Result<()> {
        let file = compose_file(manifest, &self.plugin_dir);
        let out = Command::new("docker")
            .args(["compose", "-f"])
            .arg(&file)
            .args(["up", "-d"])
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "docker compose up failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    async fn undeploy(&self, id: &str) -> Result<()> {
        let file = self.plugin_dir.join(format!("{id}/docker-compose.yml"));
        let out = Command::new("docker")
            .args(["compose", "-f"])
            .arg(&file)
            .args(["down"])
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "docker compose down failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    async fn status(&self, id: &str) -> Result<DeployStatus> {
        let file = self.plugin_dir.join(format!("{id}/docker-compose.yml"));
        let out = Command::new("docker")
            .args(["compose", "-f"])
            .arg(&file)
            .args(["ps", "--format", "json"])
            .output()
            .await?;
        if !out.status.success() {
            return Ok(DeployStatus::Stopped);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.trim().is_empty() {
            return Ok(DeployStatus::Stopped);
        }
        if stdout.contains("\"running\"") {
            Ok(DeployStatus::Running)
        } else {
            Ok(DeployStatus::Degraded)
        }
    }
}

#[async_trait]
impl DeployBackend for KubernetesBackend {
    async fn deploy(&self, manifest: &PluginManifest) -> Result<()> {
        let dir = manifest
            .deploy
            .as_ref()
            .and_then(|d| d.kustomize_dir.as_deref())
            .unwrap_or(".");
        let out = Command::new("kubectl")
            .args(["apply", "-k", dir, "-n", &self.namespace])
            .output()
            .await?;
        if !out.status.success() {
            bail!(
                "kubectl apply failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    async fn undeploy(&self, id: &str) -> Result<()> {
        let out = if let Some(dir) = &self.kustomize_dir {
            Command::new("kubectl")
                .args(["delete", "-k", dir, "-n", &self.namespace])
                .output()
                .await?
        } else {
            Command::new("kubectl")
                .args([
                    "delete",
                    "all",
                    "-l",
                    &format!("app={id}"),
                    "-n",
                    &self.namespace,
                ])
                .output()
                .await?
        };
        if !out.status.success() {
            bail!(
                "kubectl delete failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    async fn status(&self, id: &str) -> Result<DeployStatus> {
        let out = Command::new("kubectl")
            .args([
                "get",
                "pods",
                "-n",
                &self.namespace,
                "-l",
                &format!("app={id}"),
                "-o",
                "json",
            ])
            .output()
            .await?;
        if !out.status.success() {
            return Ok(DeployStatus::Stopped);
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("\"Running\"") {
            Ok(DeployStatus::Running)
        } else if stdout.contains("\"Pending\"") {
            Ok(DeployStatus::Pending)
        } else {
            Ok(DeployStatus::Degraded)
        }
    }
}

#[async_trait]
impl DeployBackend for LocalBackend {
    async fn deploy(&self, _manifest: &PluginManifest) -> Result<()> {
        Ok(())
    }

    async fn undeploy(&self, id: &str) -> Result<()> {
        let _ = Command::new("pkill").args(["-f", id]).output().await;
        Ok(())
    }

    async fn status(&self, id: &str) -> Result<DeployStatus> {
        let out = Command::new("pgrep").args(["-f", id]).output().await?;
        if out.status.success() {
            Ok(DeployStatus::Running)
        } else {
            Ok(DeployStatus::Stopped)
        }
    }
}

fn compose_file(manifest: &PluginManifest, plugin_dir: &Path) -> PathBuf {
    manifest
        .deploy
        .as_ref()
        .and_then(|d| d.compose_file.as_deref())
        .map(|f| plugin_dir.join(f))
        .unwrap_or_else(|| plugin_dir.join(format!("{}/docker-compose.yml", manifest.id)))
}
