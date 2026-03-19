use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct KubernetesSandboxClient {
    pub namespace: String,
    pub registry: String,
}

impl KubernetesSandboxClient {
    pub fn default_namespace(agent_name: &str) -> String {
        format!("that-agent-{}", sanitize_k8s_name(agent_name))
    }

    pub fn from_env(agent_name: &str) -> Self {
        let namespace = std::env::var("THAT_SANDBOX_K8S_NAMESPACE")
            .unwrap_or_else(|_| Self::default_namespace(agent_name));
        let registry = std::env::var("THAT_SANDBOX_K8S_REGISTRY")
            .unwrap_or_else(|_| "registry.local:5000".to_string());
        Self {
            namespace,
            registry,
        }
    }

    pub async fn connect(agent_name: &str) -> Result<Self> {
        let client = Self::from_env(agent_name);
        ensure_kubectl_available()?;
        Ok(client)
    }

    pub fn build_and_push_image(
        &self,
        image: &str,
        context_dir: &Path,
        dockerfile: &Path,
    ) -> Result<()> {
        run_cmd(
            "docker",
            &[
                "build",
                "-f",
                &dockerfile.display().to_string(),
                "-t",
                image,
                &context_dir.display().to_string(),
            ],
        )?;
        run_cmd("docker", &["push", image])?;
        Ok(())
    }

    pub fn apply_kustomize(&self, kustomize_dir: &Path) -> Result<()> {
        run_cmd(
            "kubectl",
            &[
                "-n",
                &self.namespace,
                "apply",
                "-k",
                &kustomize_dir.display().to_string(),
            ],
        )
    }

    pub fn rollout_status(&self, kind: &str, name: &str, timeout_secs: u64) -> Result<()> {
        run_cmd(
            "kubectl",
            &[
                "-n",
                &self.namespace,
                "rollout",
                "status",
                &format!("{kind}/{name}"),
                "--timeout",
                &format!("{timeout_secs}s"),
            ],
        )
    }

    pub fn list_managed_resources(
        &self,
        agent_name: &str,
        plugin_id: Option<&str>,
    ) -> Result<String> {
        let mut selector = format!("that.agent={agent_name},that.managed=true");
        if let Some(plugin) = plugin_id {
            selector.push_str(&format!(",that.plugin={plugin}"));
        }

        let output = std::process::Command::new("kubectl")
            .args([
                "-n",
                &self.namespace,
                "get",
                "all",
                "-l",
                &selector,
                "-o",
                "name",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("Failed to run kubectl get all")?;

        if !output.status.success() {
            anyhow::bail!(
                "kubectl get all failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn ensure_kubectl_available() -> Result<()> {
    run_cmd("kubectl", &["version", "--client"])
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .with_context(|| format!("Failed to run {cmd}"))?;

    if !status.success() {
        anyhow::bail!("{} {} failed", cmd, args.join(" "));
    }
    Ok(())
}

pub fn sanitize_k8s_name(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}
