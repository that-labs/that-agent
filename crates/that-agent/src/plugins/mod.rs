//! that-plugins — Agent-scoped plugin registry with commands, activations, and routines.

pub mod cluster;
pub mod deploy;

use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::sandbox::scope::{self, ScopeTarget};
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, Timelike, Weekday};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};

pub const PLUGIN_MANIFEST_FILE: &str = "plugin.toml";
pub const PLUGIN_STATE_FILE: &str = ".plugin-state.toml";
pub const PLUGIN_RUNTIME_FILE: &str = ".plugin-runtime.toml";
const DEFAULT_PLUGIN_SKILLS_DIR: &str = "skills";
const DEFAULT_PLUGIN_DEPLOY_DIR: &str = "deploy";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub envvars: Vec<String>,
    #[serde(default)]
    pub skills_dir: Option<String>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub emojis: Vec<PluginEmoji>,
    #[serde(default)]
    pub routines: Vec<PluginRoutine>,
    #[serde(default)]
    pub activations: Vec<PluginActivation>,
    #[serde(default)]
    pub runtime: Option<PluginRuntime>,
    #[serde(default)]
    pub deploy: Option<PluginDeploy>,
}

impl PluginManifest {
    pub fn validate(mut self, fallback_id: &str) -> Result<Self> {
        let normalized_id = normalize_plugin_id(if self.id.trim().is_empty() {
            fallback_id
        } else {
            self.id.as_str()
        });
        if normalized_id.is_empty() {
            anyhow::bail!("Plugin id is empty after normalization");
        }
        self.id = normalized_id;
        if self.version.trim().is_empty() {
            anyhow::bail!("Plugin '{}' has an empty version", self.id);
        }
        Ok(self)
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    pub fn skills_subdir(&self) -> &str {
        self.skills_dir
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_PLUGIN_SKILLS_DIR)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRuntime {
    #[serde(default = "default_runtime_kind")]
    pub kind: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub command: Vec<String>,
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self {
            kind: default_runtime_kind(),
            context: Some(".".to_string()),
            dockerfile: Some("Dockerfile".to_string()),
            command: vec!["/bin/sh".to_string(), "scripts/run.sh".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDeploy {
    #[serde(default = "default_deploy_target")]
    pub target: String,
    #[serde(default = "default_deploy_kind")]
    pub kind: String,
    #[serde(default)]
    pub compose_file: Option<String>,
    #[serde(default)]
    pub kustomize_dir: Option<String>,
    #[serde(default)]
    pub replicas: Option<u32>,
}

impl Default for PluginDeploy {
    fn default() -> Self {
        Self {
            target: default_deploy_target(),
            kind: default_deploy_kind(),
            compose_file: Some(format!("{DEFAULT_PLUGIN_DEPLOY_DIR}/docker-compose.yml")),
            kustomize_dir: Some(format!("{DEFAULT_PLUGIN_DEPLOY_DIR}/k8s")),
            replicas: Some(1),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub command: String,
    pub description: String,
    #[serde(default)]
    pub task_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEmoji {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRoutine {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub task_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginActivation {
    pub name: String,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub task_template: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginCommand {
    pub plugin_id: String,
    pub command: String,
    pub description: String,
    pub task_template: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginEmoji {
    pub plugin_id: String,
    pub name: String,
    pub value: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPluginActivation {
    pub plugin_id: String,
    pub name: String,
    pub event: String,
    pub command: Option<String>,
    pub contains: Option<String>,
    pub trigger: Option<String>,
    pub priority: String,
    pub task_template: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginHeartbeatTask {
    pub source: String,
    pub plugin_id: String,
    pub name: String,
    pub priority: String,
    pub schedule: String,
    pub body: String,
    pub route: Option<PluginTaskRoute>,
}

/// Optional route metadata for heartbeat tasks sourced from inbound events.
///
/// When present, heartbeat processing can scope notifications to the original
/// channel/chat/sender instead of broadcasting globally.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PluginTaskRoute {
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub capabilities: Vec<String>,
    pub command_count: usize,
    pub routine_count: usize,
    pub activation_count: usize,
    pub emoji_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginFsEntry {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginReadSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub enabled: bool,
    pub path: String,
    pub command_count: usize,
    pub routine_count: usize,
    pub activation_count: usize,
    pub emoji_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginReadList {
    pub agent_name: String,
    pub plugins: Vec<PluginReadSummary>,
    pub load_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginReadDetail {
    pub agent_name: String,
    pub id: String,
    pub enabled: bool,
    pub plugin_dir: String,
    pub manifest_path: String,
    pub manifest: PluginManifest,
    pub required_envvars: Vec<String>,
    pub missing_envvars: Vec<String>,
    pub top_level_entries: Vec<PluginFsEntry>,
    pub load_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginValidationItem {
    pub plugin_id: String,
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub required_envvars: Vec<String>,
    pub missing_envvars: Vec<String>,
    pub manifest_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginValidationReport {
    pub agent_name: String,
    pub valid: bool,
    pub items: Vec<PluginValidationItem>,
    pub load_errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PluginRegistry {
    pub agent_name: String,
    pub root_dir: PathBuf,
    pub plugins: Vec<InstalledPlugin>,
    pub fingerprint: u64,
    pub load_errors: Vec<String>,
}

impl PluginRegistry {
    pub fn load(agent_name: &str) -> Self {
        let root_dir = agent_plugins_dir(agent_name).unwrap_or_else(|| {
            PathBuf::from(".that-agent/agents")
                .join(agent_name)
                .join("plugins")
        });

        let state_path = root_dir.join(PLUGIN_STATE_FILE);
        let state = read_state_file(&state_path);

        let mut plugins = Vec::new();
        let mut errors = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&root_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(dir_name) = path.file_name().map(|s| s.to_string_lossy().to_string())
                else {
                    continue;
                };

                let manifest_path = path.join(PLUGIN_MANIFEST_FILE);
                if !manifest_path.exists() {
                    continue;
                }

                let manifest_text = match std::fs::read_to_string(&manifest_path) {
                    Ok(text) => text,
                    Err(err) => {
                        errors.push(format!(
                            "Failed to read plugin manifest {}: {err}",
                            manifest_path.display()
                        ));
                        continue;
                    }
                };

                let manifest =
                    match toml::from_str::<PluginManifest>(&manifest_text).and_then(|m| {
                        m.validate(&dir_name)
                            .map_err(|e| toml::de::Error::custom(e.to_string()))
                    }) {
                        Ok(manifest) => manifest,
                        Err(err) => {
                            errors.push(format!(
                                "Failed to parse plugin manifest {}: {err}",
                                manifest_path.display()
                            ));
                            continue;
                        }
                    };

                let enabled = state
                    .plugins
                    .get(&manifest.id)
                    .map(|s| s.enabled)
                    .unwrap_or(manifest.enabled_by_default);

                plugins.push(InstalledPlugin {
                    manifest,
                    dir: path,
                    enabled,
                });
            }
        }

        plugins.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));

        let fingerprint = compute_registry_fingerprint(&root_dir, &state_path, &plugins);

        Self {
            agent_name: agent_name.to_string(),
            root_dir,
            plugins,
            fingerprint,
            load_errors: errors,
        }
    }

    pub fn enabled_plugins(&self) -> impl Iterator<Item = &InstalledPlugin> {
        self.plugins.iter().filter(|p| p.enabled)
    }

    pub fn enabled_skill_dirs(&self) -> Vec<PathBuf> {
        self.enabled_plugins()
            .map(|plugin| plugin.dir.join(plugin.manifest.skills_subdir()))
            .filter(|dir| dir.is_dir())
            .collect()
    }

    pub fn enabled_commands(&self) -> Vec<ResolvedPluginCommand> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for plugin in self.enabled_plugins() {
            for cmd in &plugin.manifest.commands {
                let normalized = normalize_command_name(&cmd.command);
                if normalized.is_empty() || !seen.insert(normalized.clone()) {
                    continue;
                }
                out.push(ResolvedPluginCommand {
                    plugin_id: plugin.manifest.id.clone(),
                    command: normalized,
                    description: truncate_text(&cmd.description, 256),
                    task_template: cmd.task_template.clone(),
                });
            }
        }
        out
    }

    pub fn enabled_emojis(&self) -> Vec<ResolvedPluginEmoji> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for plugin in self.enabled_plugins() {
            for emoji in &plugin.manifest.emojis {
                let normalized_name = normalize_plugin_id(&emoji.name);
                if normalized_name.is_empty() || !seen.insert(normalized_name.clone()) {
                    continue;
                }
                out.push(ResolvedPluginEmoji {
                    plugin_id: plugin.manifest.id.clone(),
                    name: normalized_name,
                    value: emoji.value.clone(),
                    description: emoji.description.clone(),
                });
            }
        }
        out
    }

    pub fn enabled_activations(&self) -> Vec<ResolvedPluginActivation> {
        let mut out = Vec::new();
        for plugin in self.enabled_plugins() {
            for activation in &plugin.manifest.activations {
                out.push(ResolvedPluginActivation {
                    plugin_id: plugin.manifest.id.clone(),
                    name: activation.name.clone(),
                    event: activation
                        .event
                        .as_deref()
                        .map(normalize_event_name)
                        .unwrap_or_else(|| "message_in".to_string()),
                    command: activation
                        .command
                        .as_deref()
                        .map(normalize_command_name)
                        .filter(|c| !c.is_empty()),
                    contains: activation
                        .contains
                        .as_deref()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(ToOwned::to_owned),
                    trigger: activation
                        .trigger
                        .as_deref()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(ToOwned::to_owned),
                    priority: normalize_priority(
                        activation.priority.as_deref().unwrap_or("high"),
                        "high",
                    ),
                    task_template: activation.task_template.clone(),
                    description: activation.description.clone(),
                });
            }
        }
        out
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.enabled_plugins()
            .map(|p| PluginSummary {
                id: p.manifest.id.clone(),
                name: p.manifest.display_name().to_string(),
                version: p.manifest.version.clone(),
                description: p.manifest.description.clone(),
                capabilities: p.manifest.capabilities.clone(),
                command_count: p.manifest.commands.len(),
                routine_count: p.manifest.routines.len(),
                activation_count: p.manifest.activations.len(),
                emoji_count: p.manifest.emojis.len(),
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&InstalledPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == id)
    }
}

pub fn agent_plugins_dir(agent_name: &str) -> Option<PathBuf> {
    scope::plugins_root(agent_name).ok()
}

pub fn ensure_agent_plugins_dir(agent_name: &str) -> Result<PathBuf> {
    let path = agent_plugins_dir(agent_name)
        .ok_or_else(|| anyhow::anyhow!("Could not resolve home directory for plugin path"))?;
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn set_plugin_enabled(agent_name: &str, plugin_id: &str, enabled: bool) -> Result<()> {
    let root = ensure_agent_plugins_dir(agent_name)?;
    let state_path = root.join(PLUGIN_STATE_FILE);
    let mut state = read_state_file(&state_path);
    state
        .plugins
        .entry(normalize_plugin_id(plugin_id))
        .or_default()
        .enabled = enabled;
    let text = toml::to_string_pretty(&state)?;
    std::fs::write(state_path, text)?;
    Ok(())
}

pub fn create_plugin_scaffold(agent_name: &str, plugin_id: &str, force: bool) -> Result<PathBuf> {
    let normalized_id = normalize_plugin_id(plugin_id);
    if normalized_id.is_empty() {
        anyhow::bail!("Plugin id must contain at least one alphanumeric character");
    }

    let plugin_dir = scope::resolve_scope_path(
        agent_name,
        &ScopeTarget::PluginRoot {
            plugin_id: normalized_id.clone(),
        },
    )?;
    if plugin_dir.exists() {
        if !force {
            anyhow::bail!(
                "Plugin '{}' already exists at {} (use --force to overwrite)",
                normalized_id,
                plugin_dir.display()
            );
        }
    } else {
        std::fs::create_dir_all(&plugin_dir)?;
    }

    let skills_dir = scope::ensure_scope_path(
        agent_name,
        &ScopeTarget::PluginSkills {
            plugin_id: normalized_id.clone(),
        },
    )?;
    let scripts_dir = scope::ensure_scope_path(
        agent_name,
        &ScopeTarget::PluginScripts {
            plugin_id: normalized_id.clone(),
        },
    )?;
    let deploy_dir = scope::ensure_scope_path(
        agent_name,
        &ScopeTarget::PluginDeploy {
            plugin_id: normalized_id.clone(),
        },
    )?;
    let _state_dir = scope::ensure_scope_path(
        agent_name,
        &ScopeTarget::PluginState {
            plugin_id: normalized_id.clone(),
        },
    )?;
    let _artifacts_dir = scope::ensure_scope_path(
        agent_name,
        &ScopeTarget::PluginArtifacts {
            plugin_id: normalized_id.clone(),
        },
    )?;

    let manifest = PluginManifest {
        id: normalized_id.clone(),
        version: "0.1.0".to_string(),
        name: Some(normalized_id.clone()),
        description: Some("Describe what this plugin adds".to_string()),
        enabled_by_default: true,
        capabilities: vec![],
        envvars: vec![],
        skills_dir: Some(DEFAULT_PLUGIN_SKILLS_DIR.to_string()),
        commands: vec![],
        emojis: vec![],
        routines: vec![],
        activations: vec![],
        runtime: Some(PluginRuntime::default()),
        deploy: Some(PluginDeploy::default()),
    };

    let manifest_path = plugin_dir.join(PLUGIN_MANIFEST_FILE);
    let manifest_toml = toml::to_string_pretty(&manifest)?;
    std::fs::write(&manifest_path, manifest_toml)?;

    let sample_skill = skills_dir.join("example");
    std::fs::create_dir_all(&sample_skill)?;
    let sample_skill_path = sample_skill.join("SKILL.md");
    if force || !sample_skill_path.exists() {
        std::fs::write(sample_skill_path, default_skill_template("example"))?;
    }

    let run_script_path = scripts_dir.join("run.sh");
    if force || !run_script_path.exists() {
        std::fs::write(
            &run_script_path,
            "#!/bin/sh\nset -eu\n\nARGS=\"${*:-}\"\necho \"plugin=$THAT_PLUGIN_ID agent=$THAT_AGENT_NAME args=$ARGS\"\n",
        )?;
    }

    let dockerfile_path = plugin_dir.join("Dockerfile");
    if force || !dockerfile_path.exists() {
        std::fs::write(
            &dockerfile_path,
            "FROM alpine:3.20\nWORKDIR /app\nCOPY . /app\nRUN chmod +x scripts/run.sh\nENTRYPOINT [\"/bin/sh\", \"/app/scripts/run.sh\"]\n",
        )?;
    }

    let compose_path = deploy_dir.join("docker-compose.yml");
    if force || !compose_path.exists() {
        std::fs::write(
            &compose_path,
            format!(
                "services:\n  {normalized_id}:\n    image: registry.local:5000/{normalized_id}:latest\n    container_name: {normalized_id}\n    restart: unless-stopped\n    command: [\"/bin/sh\", \"/app/scripts/run.sh\"]\n"
            ),
        )?;
    }

    let k8s_dir = deploy_dir.join("k8s");
    std::fs::create_dir_all(&k8s_dir)?;
    let kustomize_path = k8s_dir.join("kustomization.yaml");
    if force || !kustomize_path.exists() {
        std::fs::write(
            &kustomize_path,
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources: []\n",
        )?;
    }

    Ok(plugin_dir)
}

pub fn create_plugin_skill_scaffold(
    agent_name: &str,
    plugin_id: &str,
    skill_name: &str,
    force: bool,
) -> Result<PathBuf> {
    let plugin_id = normalize_plugin_id(plugin_id);
    if plugin_id.is_empty() {
        anyhow::bail!("Plugin id must contain at least one alphanumeric character");
    }
    let plugin_dir = scope::resolve_scope_path(
        agent_name,
        &ScopeTarget::PluginRoot {
            plugin_id: plugin_id.clone(),
        },
    )?;
    if !plugin_dir.exists() {
        anyhow::bail!(
            "Plugin '{}' does not exist for agent '{}' (create it first)",
            plugin_id,
            agent_name
        );
    }

    let manifest_path = plugin_dir.join(PLUGIN_MANIFEST_FILE);
    let manifest_text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read plugin manifest {}", manifest_path.display()))?;
    let manifest = toml::from_str::<PluginManifest>(&manifest_text)
        .and_then(|m| {
            m.validate(&plugin_id)
                .map_err(|e| toml::de::Error::custom(e.to_string()))
        })
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse plugin manifest {}: {e}",
                manifest_path.display()
            )
        })?;

    let skill_dir_name = scope::normalize_skill_dir_name(skill_name);
    if skill_dir_name.is_empty() {
        anyhow::bail!("Skill name must contain at least one alphanumeric character");
    }

    let skills_root = plugin_dir.join(manifest.skills_subdir());
    std::fs::create_dir_all(&skills_root)?;
    let skill_dir = skills_root.join(&skill_dir_name);
    if skill_dir.exists() && !force {
        anyhow::bail!(
            "Skill '{}' already exists at {} (use --force to overwrite)",
            skill_dir_name,
            skill_dir.display()
        );
    }
    std::fs::create_dir_all(&skill_dir)?;
    let skill_path = skill_dir.join("SKILL.md");
    if force || !skill_path.exists() {
        std::fs::write(&skill_path, default_skill_template(&skill_dir_name))?;
    }
    Ok(skill_path)
}

pub fn enqueue_activation_task(
    agent_name: &str,
    plugin_id: &str,
    activation_name: &str,
    priority: &str,
    task: &str,
    route: Option<PluginTaskRoute>,
) -> Result<()> {
    if task.trim().is_empty() {
        return Ok(());
    }
    let runtime_path = ensure_agent_plugins_dir(agent_name)?.join(PLUGIN_RUNTIME_FILE);
    let mut runtime = read_runtime_file(&runtime_path);
    runtime.activation_queue.push(QueuedActivation {
        plugin_id: normalize_plugin_id(plugin_id),
        activation_name: activation_name.trim().to_string(),
        priority: normalize_priority(priority, "high"),
        task: task.trim().to_string(),
        created_at: Local::now().to_rfc3339(),
        route,
    });
    write_runtime_file(&runtime_path, &runtime)
}

pub fn collect_due_heartbeat_tasks(
    agent_name: &str,
    registry: &PluginRegistry,
) -> Result<Vec<PluginHeartbeatTask>> {
    let runtime_path = ensure_agent_plugins_dir(agent_name)?.join(PLUGIN_RUNTIME_FILE);
    let mut runtime = read_runtime_file(&runtime_path);
    let now = Local::now();
    let mut tasks = Vec::new();

    for plugin in registry.enabled_plugins() {
        for routine in &plugin.manifest.routines {
            let schedule =
                normalize_schedule(routine.schedule.as_deref().unwrap_or("daily"), "daily");
            if schedule == "invalid" {
                continue;
            }
            let priority =
                normalize_priority(routine.priority.as_deref().unwrap_or("normal"), "normal");
            let key = format!("{}::{}", plugin.manifest.id, routine.name);
            let last_run = runtime
                .routine_last_run
                .get(&key)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Local));
            if !is_routine_due(&schedule, last_run, now) {
                continue;
            }
            let body = routine
                .task_template
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(ToOwned::to_owned)
                .or_else(|| routine.description.clone())
                .unwrap_or_else(|| format!("Run plugin routine '{}'", routine.name));
            tasks.push(PluginHeartbeatTask {
                source: "routine".to_string(),
                plugin_id: plugin.manifest.id.clone(),
                name: routine.name.clone(),
                priority,
                schedule: schedule.clone(),
                body,
                route: None,
            });
            runtime.routine_last_run.insert(key, now.to_rfc3339());
        }
    }

    for activation in runtime.activation_queue.drain(..) {
        tasks.push(PluginHeartbeatTask {
            source: "activation".to_string(),
            plugin_id: activation.plugin_id,
            name: activation.activation_name,
            priority: normalize_priority(&activation.priority, "high"),
            schedule: "once".to_string(),
            body: activation.task,
            route: activation.route,
        });
    }

    write_runtime_file(&runtime_path, &runtime)?;
    Ok(tasks)
}

pub fn read_plugin_snapshot(
    agent_name: &str,
    plugin_id: Option<&str>,
    include_files: bool,
) -> Result<serde_json::Value> {
    let registry = PluginRegistry::load(agent_name);
    if let Some(id) = plugin_id {
        let normalized = normalize_plugin_id(id);
        let plugin = registry.get(&normalized).ok_or_else(|| {
            anyhow::anyhow!(
                "Plugin '{}' not found for agent '{}'",
                normalized,
                agent_name
            )
        })?;
        let (required_envvars, missing_envvars) = env_requirements(&plugin.manifest.envvars);
        let top_level_entries = if include_files {
            list_top_level_entries(&plugin.dir)
        } else {
            Vec::new()
        };
        let detail = PluginReadDetail {
            agent_name: agent_name.to_string(),
            id: plugin.manifest.id.clone(),
            enabled: plugin.enabled,
            plugin_dir: plugin.dir.display().to_string(),
            manifest_path: plugin.dir.join(PLUGIN_MANIFEST_FILE).display().to_string(),
            manifest: plugin.manifest.clone(),
            required_envvars,
            missing_envvars,
            top_level_entries,
            load_errors: registry.load_errors,
        };
        return Ok(serde_json::to_value(detail)?);
    }

    let plugins = registry
        .plugins
        .iter()
        .map(|plugin| PluginReadSummary {
            id: plugin.manifest.id.clone(),
            name: plugin.manifest.display_name().to_string(),
            version: plugin.manifest.version.clone(),
            enabled: plugin.enabled,
            path: plugin.dir.display().to_string(),
            command_count: plugin.manifest.commands.len(),
            routine_count: plugin.manifest.routines.len(),
            activation_count: plugin.manifest.activations.len(),
            emoji_count: plugin.manifest.emojis.len(),
        })
        .collect::<Vec<_>>();
    let list = PluginReadList {
        agent_name: agent_name.to_string(),
        plugins,
        load_errors: registry.load_errors,
    };
    Ok(serde_json::to_value(list)?)
}

pub fn validate_plugin_snapshot(
    agent_name: &str,
    plugin_id: Option<&str>,
) -> Result<PluginValidationReport> {
    let registry = PluginRegistry::load(agent_name);
    let mut items = Vec::new();

    if let Some(id) = plugin_id {
        let normalized = normalize_plugin_id(id);
        let plugin = registry.get(&normalized).ok_or_else(|| {
            anyhow::anyhow!(
                "Plugin '{}' not found for agent '{}'",
                normalized,
                agent_name
            )
        })?;
        items.push(validate_plugin_entry(plugin));
    } else {
        for plugin in &registry.plugins {
            items.push(validate_plugin_entry(plugin));
        }
    }

    let valid = items.iter().all(|item| item.valid) && registry.load_errors.is_empty();
    Ok(PluginValidationReport {
        agent_name: agent_name.to_string(),
        valid,
        items,
        load_errors: registry.load_errors,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PluginStateFile {
    #[serde(default)]
    plugins: BTreeMap<String, PluginState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginState {
    #[serde(default = "default_true")]
    enabled: bool,
}

impl Default for PluginState {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PluginRuntimeFile {
    #[serde(default)]
    routine_last_run: BTreeMap<String, String>,
    #[serde(default)]
    activation_queue: Vec<QueuedActivation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedActivation {
    plugin_id: String,
    activation_name: String,
    priority: String,
    task: String,
    created_at: String,
    #[serde(default)]
    route: Option<PluginTaskRoute>,
}

fn read_state_file(path: &Path) -> PluginStateFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<PluginStateFile>(&text).ok())
        .unwrap_or_default()
}

fn list_top_level_entries(plugin_dir: &Path) -> Vec<PluginFsEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(plugin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let kind = if path.is_dir() { "dir" } else { "file" };
            out.push(PluginFsEntry {
                name,
                kind: kind.to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn validate_plugin_entry(plugin: &InstalledPlugin) -> PluginValidationItem {
    let manifest = &plugin.manifest;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_commands = HashSet::new();

    for cmd in &manifest.commands {
        let normalized = normalize_command_name(&cmd.command);
        if normalized.is_empty() {
            errors.push(format!(
                "Command '{}' normalizes to empty; use lowercase letters/digits/underscore.",
                cmd.command
            ));
            continue;
        }
        if !seen_commands.insert(normalized.clone()) {
            errors.push(format!(
                "Duplicate command after normalization: '{}'",
                normalized
            ));
        }
        if cmd.description.trim().is_empty() {
            errors.push(format!(
                "Command '{}' has an empty description.",
                cmd.command
            ));
        }
    }

    for activation in &manifest.activations {
        if let Some(cmd) = activation.command.as_deref() {
            let normalized = normalize_command_name(cmd);
            if normalized.is_empty() {
                errors.push(format!(
                    "Activation '{}' references invalid command '{}'.",
                    activation.name, cmd
                ));
            }
        }
    }

    for routine in &manifest.routines {
        let raw_schedule = routine.schedule.as_deref().unwrap_or("daily");
        if normalize_schedule(raw_schedule, "daily") == "invalid" {
            errors.push(format!(
                "Routine '{}' has invalid schedule '{}'. Use once|minutely|hourly|daily|weekly or cron: <expr>.",
                routine.name, raw_schedule
            ));
        }
    }

    let skills_dir = plugin.dir.join(manifest.skills_subdir());
    if !skills_dir.is_dir() {
        warnings.push(format!(
            "Skills directory '{}' is missing.",
            skills_dir.display()
        ));
    }

    if let Some(runtime) = manifest.runtime.as_ref() {
        if runtime.kind.trim().is_empty() {
            errors.push("runtime.kind is empty.".to_string());
        }
        if runtime.command.is_empty() {
            warnings.push("runtime.command is empty.".to_string());
        }
        if let Some(dockerfile) = runtime.dockerfile.as_deref() {
            let dockerfile_path = plugin.dir.join(dockerfile);
            if !dockerfile_path.exists() {
                warnings.push(format!(
                    "runtime.dockerfile '{}' does not exist.",
                    dockerfile_path.display()
                ));
            }
        }
    }

    let (required_envvars, missing_envvars) = env_requirements(&manifest.envvars);
    if !missing_envvars.is_empty() {
        warnings.push(format!(
            "Missing required environment variables: {}",
            missing_envvars.join(", ")
        ));
    }

    let manifest_path = plugin.dir.join(PLUGIN_MANIFEST_FILE).display().to_string();
    let valid = errors.is_empty();
    PluginValidationItem {
        plugin_id: manifest.id.clone(),
        valid,
        errors,
        warnings,
        required_envvars,
        missing_envvars,
        manifest_path,
    }
}

fn env_requirements(specs: &[String]) -> (Vec<String>, Vec<String>) {
    let mut required = Vec::new();
    for spec in specs {
        let var = extract_env_var_name(spec);
        if !var.is_empty() && !required.iter().any(|v| v == &var) {
            required.push(var);
        }
    }
    let missing = required
        .iter()
        .filter(|var| std::env::var(var.as_str()).is_err())
        .cloned()
        .collect();
    (required, missing)
}

fn extract_env_var_name(spec: &str) -> String {
    let spec = spec.trim();
    if let Some(start) = spec.find("${") {
        let after = &spec[start + 2..];
        if let Some(end) = after.find('}') {
            return after[..end].trim().to_string();
        }
    }
    spec.to_string()
}

fn read_runtime_file(path: &Path) -> PluginRuntimeFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<PluginRuntimeFile>(&text).ok())
        .unwrap_or_default()
}

fn write_runtime_file(path: &Path, runtime: &PluginRuntimeFile) -> Result<()> {
    let text = toml::to_string_pretty(runtime)?;
    std::fs::write(path, text)?;
    Ok(())
}

fn compute_registry_fingerprint(
    root_dir: &Path,
    state_path: &Path,
    plugins: &[InstalledPlugin],
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root_dir.display().to_string().hash(&mut hasher);

    hash_file_mtime(state_path, &mut hasher);

    for plugin in plugins {
        plugin.manifest.id.hash(&mut hasher);
        plugin.manifest.version.hash(&mut hasher);
        plugin.enabled.hash(&mut hasher);

        let manifest_path = plugin.dir.join(PLUGIN_MANIFEST_FILE);
        hash_file_mtime(&manifest_path, &mut hasher);

        let skills_dir = plugin.dir.join(plugin.manifest.skills_subdir());
        hash_skill_files(&skills_dir, &mut hasher);
    }

    hasher.finish()
}

fn hash_skill_files(dir: &Path, hasher: &mut std::collections::hash_map::DefaultHasher) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();

    for path in paths {
        if path.is_dir() {
            hash_skill_files(&path, hasher);
            continue;
        }
        if !path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
            continue;
        }
        path.display().to_string().hash(hasher);
        hash_file_mtime(&path, hasher);
    }
}

fn hash_file_mtime(path: &Path, hasher: &mut std::collections::hash_map::DefaultHasher) {
    let mtime_ns = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    path.display().to_string().hash(hasher);
    mtime_ns.hash(hasher);
}

fn normalize_plugin_id(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

pub fn normalize_command_name(value: &str) -> String {
    value
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect()
}

fn normalize_event_name(value: &str) -> String {
    let normalized: String = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if normalized.is_empty() {
        "message_in".to_string()
    } else {
        normalized
    }
}

fn normalize_schedule(value: &str, fallback: &str) -> String {
    let raw = value.trim();
    if raw.is_empty() {
        return fallback.to_string();
    }
    let normalized = raw.to_ascii_lowercase();
    match normalized.as_str() {
        "once" | "minutely" | "hourly" | "daily" | "weekly" => normalized,
        _ => {
            if let Some(expr) = raw.strip_prefix("cron:") {
                let expr = expr.trim();
                if parse_cron_expression(expr).is_ok() {
                    return format!("cron:{expr}");
                }
            } else if parse_cron_expression(raw).is_ok() {
                return format!("cron:{raw}");
            }
            "invalid".to_string()
        }
    }
}

fn normalize_priority(value: &str, fallback: &str) -> String {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "urgent" | "high" | "normal" | "low" => normalized,
        _ => fallback.to_string(),
    }
}

fn is_routine_due(schedule: &str, last_run: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    match schedule {
        "once" => last_run.is_none(),
        "minutely" => match last_run {
            None => true,
            Some(last) => now - last >= chrono::Duration::minutes(1),
        },
        "hourly" => match last_run {
            None => true,
            Some(last) => now - last > chrono::Duration::hours(1),
        },
        "daily" => match last_run {
            None => true,
            Some(last) => last.date_naive() < now.date_naive(),
        },
        "weekly" => match last_run {
            None => true,
            Some(last) => now - last > chrono::Duration::weeks(1),
        },
        _ if schedule.starts_with("cron:") => {
            let expr = schedule.trim_start_matches("cron:").trim();
            is_cron_due(expr, last_run, now)
        }
        _ => false,
    }
}

#[derive(Debug, Clone)]
struct CronExpression {
    minute: Vec<bool>,
    hour: Vec<bool>,
    day_of_month: Vec<bool>,
    month: Vec<bool>,
    day_of_week: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn is_cron_due(expr: &str, last_run: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    if last_run.is_none() {
        return true;
    }
    let Ok(parsed) = parse_cron_expression(expr) else {
        return false;
    };
    let slot = now
        .with_second(0)
        .and_then(|dt| dt.with_nanosecond(0))
        .unwrap_or(now);
    let Some(last) = last_run else {
        return true;
    };
    if last >= slot {
        return false;
    }
    cron_matches(&parsed, slot)
}

fn parse_cron_expression(expr: &str) -> Result<CronExpression, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err("cron expression must have exactly 5 fields: min hour dom mon dow".to_string());
    }
    let minute = parse_cron_field(fields[0], 0, 59, parse_u32_plain)?;
    let hour = parse_cron_field(fields[1], 0, 23, parse_u32_plain)?;
    let day_of_month = parse_cron_field(fields[2], 1, 31, parse_u32_plain)?;
    let month = parse_cron_field(fields[3], 1, 12, parse_month_value)?;
    let day_of_week = parse_cron_field(fields[4], 0, 7, parse_day_of_week_value)?;
    Ok(CronExpression {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
        dom_any: fields[2].trim() == "*",
        dow_any: fields[4].trim() == "*",
    })
}

fn cron_matches(expr: &CronExpression, when: DateTime<Local>) -> bool {
    let minute = when.minute() as usize;
    let hour = when.hour() as usize;
    let dom = when.day() as usize;
    let month = when.month() as usize;
    let dow = weekday_to_num(when.weekday()) as usize;
    if !expr.minute[minute] || !expr.hour[hour] || !expr.month[month] {
        return false;
    }
    let dom_match = expr.day_of_month[dom];
    let dow_match = expr.day_of_week[dow] || (dow == 0 && expr.day_of_week[7]);
    if expr.dom_any && expr.dow_any {
        true
    } else if expr.dom_any {
        dow_match
    } else if expr.dow_any {
        dom_match
    } else {
        dom_match || dow_match
    }
}

fn parse_cron_field(
    spec: &str,
    min: u32,
    max: u32,
    parse_value: fn(&str) -> Option<u32>,
) -> Result<Vec<bool>, String> {
    let mut flags = vec![false; (max + 1) as usize];
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty cron field segment".to_string());
        }
        let (base, step) = if let Some((base, step_str)) = part.split_once('/') {
            let step = step_str
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid cron step '{step_str}'"))?;
            if step == 0 {
                return Err("cron step cannot be zero".to_string());
            }
            (base.trim(), step)
        } else {
            (part, 1)
        };

        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((start_str, end_str)) = base.split_once('-') {
            let start = parse_value(start_str.trim())
                .ok_or_else(|| format!("invalid cron value '{start_str}'"))?;
            let end = parse_value(end_str.trim())
                .ok_or_else(|| format!("invalid cron value '{end_str}'"))?;
            if start > end {
                return Err(format!("invalid cron range '{base}'"));
            }
            (start, end)
        } else {
            let value = parse_value(base).ok_or_else(|| format!("invalid cron value '{base}'"))?;
            (value, value)
        };

        if start < min || end > max {
            return Err(format!(
                "cron value out of range '{part}' (expected {min}-{max})"
            ));
        }

        let mut value = start;
        while value <= end {
            flags[value as usize] = true;
            match value.checked_add(step) {
                Some(next) if next > value => value = next,
                _ => break,
            }
        }
    }

    if !flags[min as usize..=max as usize].iter().any(|v| *v) {
        return Err("cron field resolved to empty set".to_string());
    }
    Ok(flags)
}

fn parse_u32_plain(value: &str) -> Option<u32> {
    value.parse::<u32>().ok()
}

fn parse_month_value(value: &str) -> Option<u32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        other => other.parse::<u32>().ok(),
    }
}

fn parse_day_of_week_value(value: &str) -> Option<u32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sun" => Some(0),
        "mon" => Some(1),
        "tue" => Some(2),
        "wed" => Some(3),
        "thu" => Some(4),
        "fri" => Some(5),
        "sat" => Some(6),
        other => other.parse::<u32>().ok(),
    }
}

fn weekday_to_num(day: Weekday) -> u32 {
    match day {
        Weekday::Sun => 0,
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn default_true() -> bool {
    true
}

fn default_runtime_kind() -> String {
    "docker".to_string()
}

fn default_deploy_kind() -> String {
    "service".to_string()
}

fn default_deploy_target() -> String {
    "docker".to_string()
}

fn default_skill_template(skill_name: &str) -> String {
    format!(
        "---\nname: {skill_name}\ndescription: {skill_name} skill\nmetadata:\n  bootstrap: false\n  always: false\n---\n\nReplace this skill with plugin-specific instructions.\n"
    )
}
