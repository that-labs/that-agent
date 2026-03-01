use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use super::PluginManifest;
use crate::deploy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginPolicy {
    pub allow: Vec<String>,
}

impl Default for PluginPolicy {
    fn default() -> Self {
        Self {
            allow: vec!["*".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterPlugin {
    pub id: String,
    pub version: String,
    pub owner_agent: String,
    pub policy: PluginPolicy,
    pub manifest: PluginManifest,
    /// Cached deploy status from last reconciliation.
    #[serde(default)]
    pub deploy_status: Option<String>,
    /// Unix timestamp of last status check.
    #[serde(default)]
    pub status_checked_at: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClusterState {
    main_agent: Option<String>,
    plugins: Vec<ClusterPlugin>,
}

pub struct ClusterRegistry {
    path: PathBuf,
}

impl ClusterRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn install(&self, manifest: PluginManifest, owner: &str) -> Result<ClusterPlugin> {
        let mut state = self.load()?;
        if state.main_agent.is_none() {
            state.main_agent = Some(owner.to_string());
        }
        let plugin = ClusterPlugin {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            owner_agent: owner.to_string(),
            policy: PluginPolicy::default(),
            deploy_status: None,
            status_checked_at: None,
            manifest,
        };
        if let Some(existing) = state.plugins.iter_mut().find(|p| p.id == plugin.id) {
            *existing = plugin.clone();
        } else {
            state.plugins.push(plugin.clone());
        }
        self.save(&state)?;
        Ok(plugin)
    }

    pub fn uninstall(&self, id: &str, requestor: &str) -> Result<()> {
        let mut state = self.load()?;
        let is_main = state.main_agent.as_deref() == Some(requestor);
        let owner = state
            .plugins
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.owner_agent.clone());
        match owner {
            Some(ref o) if o == requestor || is_main => {}
            Some(_) => bail!("only the owner or main agent can uninstall plugin '{id}'"),
            None => bail!("plugin '{id}' not found"),
        }
        state.plugins.retain(|p| p.id != id);
        self.save(&state)
    }

    pub fn list(&self) -> Result<Vec<ClusterPlugin>> {
        Ok(self.load()?.plugins)
    }

    pub fn set_policy(&self, id: &str, allow: Vec<String>, requestor: &str) -> Result<()> {
        let mut state = self.load()?;
        let is_main = state.main_agent.as_deref() == Some(requestor);
        let plugin = state
            .plugins
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow::anyhow!("plugin '{id}' not found"))?;
        if plugin.owner_agent != requestor && !is_main {
            bail!("only the owner or main agent can change policy for '{id}'");
        }
        plugin.policy = PluginPolicy { allow };
        self.save(&state)
    }

    /// Look up a single plugin by ID.
    pub fn find(&self, id: &str) -> Result<Option<ClusterPlugin>> {
        Ok(self.load()?.plugins.into_iter().find(|p| p.id == id))
    }

    /// Reconcile deploy status for all plugins that declare a deploy target.
    /// Updates cached status and timestamp in the registry. Call periodically (e.g. every 60s).
    pub async fn reconcile_status(&self, plugin_dir: &Path) -> Result<()> {
        let mut state = self.load()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for plugin in &mut state.plugins {
            if let Some(dep) = &plugin.manifest.deploy {
                let backend = deploy::backend_for(dep, plugin_dir);
                let status = backend.status(&plugin.id).await;
                plugin.deploy_status = Some(match &status {
                    Ok(deploy::DeployStatus::Running) => "running".into(),
                    Ok(deploy::DeployStatus::Stopped) => "stopped".into(),
                    Ok(deploy::DeployStatus::Pending) => "pending".into(),
                    Ok(deploy::DeployStatus::Deploying) => "deploying".into(),
                    Ok(deploy::DeployStatus::Degraded) => "degraded".into(),
                    Ok(deploy::DeployStatus::Failed(m)) => format!("failed: {m}"),
                    Err(e) => format!("error: {e}"),
                });
                plugin.status_checked_at = Some(now);
            }
        }
        self.save(&state)
    }

    fn load(&self) -> Result<ClusterState> {
        if !self.path.exists() {
            return Ok(ClusterState::default());
        }
        let data = std::fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&data)?)
    }

    fn save(&self, state: &ClusterState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
