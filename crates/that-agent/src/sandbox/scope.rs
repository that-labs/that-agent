use std::path::PathBuf;

use anyhow::{Context, Result};

pub const THAT_AGENT_HOME_DIR: &str = ".that-agent";
pub const AGENTS_DIR: &str = "agents";
pub const PLUGINS_DIR: &str = "plugins";
pub const SKILLS_DIR: &str = "skills";
pub const STATE_DIR: &str = "state";
pub const ARTIFACTS_DIR: &str = "artifacts";
pub const DEPLOY_DIR: &str = "deploy";
pub const SCRIPTS_DIR: &str = "scripts";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTarget {
    AgentRoot,
    AgentSkills,
    PluginsRoot,
    PluginRoot { plugin_id: String },
    PluginSkills { plugin_id: String },
    PluginState { plugin_id: String },
    PluginArtifacts { plugin_id: String },
    PluginDeploy { plugin_id: String },
    PluginScripts { plugin_id: String },
}

pub fn normalize_plugin_id(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

pub fn normalize_skill_dir_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

pub fn agent_root(agent_name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to resolve home directory")?;
    Ok(home
        .join(THAT_AGENT_HOME_DIR)
        .join(AGENTS_DIR)
        .join(agent_name))
}

pub fn plugins_root(agent_name: &str) -> Result<PathBuf> {
    Ok(agent_root(agent_name)?.join(PLUGINS_DIR))
}

pub fn agent_skills_root(agent_name: &str) -> Result<PathBuf> {
    Ok(agent_root(agent_name)?.join(SKILLS_DIR))
}

pub fn plugin_root(agent_name: &str, plugin_id: &str) -> Result<PathBuf> {
    let normalized = normalize_plugin_id(plugin_id);
    if normalized.is_empty() {
        anyhow::bail!("Plugin id must contain at least one alphanumeric character");
    }
    Ok(plugins_root(agent_name)?.join(normalized))
}

pub fn resolve_scope_path(agent_name: &str, target: &ScopeTarget) -> Result<PathBuf> {
    let root = agent_root(agent_name)?;
    match target {
        ScopeTarget::AgentRoot => Ok(root),
        ScopeTarget::AgentSkills => Ok(root.join(SKILLS_DIR)),
        ScopeTarget::PluginsRoot => Ok(root.join(PLUGINS_DIR)),
        ScopeTarget::PluginRoot { plugin_id } => plugin_root(agent_name, plugin_id),
        ScopeTarget::PluginSkills { plugin_id } => {
            Ok(plugin_root(agent_name, plugin_id)?.join(SKILLS_DIR))
        }
        ScopeTarget::PluginState { plugin_id } => {
            Ok(plugin_root(agent_name, plugin_id)?.join(STATE_DIR))
        }
        ScopeTarget::PluginArtifacts { plugin_id } => {
            Ok(plugin_root(agent_name, plugin_id)?.join(ARTIFACTS_DIR))
        }
        ScopeTarget::PluginDeploy { plugin_id } => {
            Ok(plugin_root(agent_name, plugin_id)?.join(DEPLOY_DIR))
        }
        ScopeTarget::PluginScripts { plugin_id } => {
            Ok(plugin_root(agent_name, plugin_id)?.join(SCRIPTS_DIR))
        }
    }
}

pub fn ensure_scope_path(agent_name: &str, target: &ScopeTarget) -> Result<PathBuf> {
    let path = resolve_scope_path(agent_name, target)?;
    std::fs::create_dir_all(&path)
        .with_context(|| format!("Failed to create scope directory {}", path.display()))?;
    Ok(path)
}
