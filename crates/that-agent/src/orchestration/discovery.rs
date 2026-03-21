use crate::config::AgentDef;
use crate::skills;

use super::config::trusted_local_sandbox_enabled;

/// Convert a skill name (e.g. `json-formatter`) to a valid bot command name (`json_formatter`).
/// Telegram requires lowercase alphanumeric + underscore, max 32 chars.
pub fn skill_to_command(name: &str) -> String {
    let cmd: String = name
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect::<String>()
        .to_lowercase();
    cmd.chars().take(32).collect()
}

/// Find a skill whose normalized command name matches `cmd`.
pub fn find_skill_by_command<'a>(
    cmd: &str,
    skills: &'a [skills::SkillMeta],
) -> Option<&'a skills::SkillMeta> {
    skills.iter().find(|s| skill_to_command(&s.name) == cmd)
}

pub fn read_skill_content(skill: &skills::SkillMeta) -> Option<String> {
    std::fs::read_to_string(&skill.path).ok()
}

pub fn find_plugin_command<'a>(
    cmd: &str,
    commands: &'a [crate::plugins::ResolvedPluginCommand],
) -> Option<&'a crate::plugins::ResolvedPluginCommand> {
    commands.iter().find(|c| c.command == cmd)
}

pub fn render_plugin_command_task(
    command: &crate::plugins::ResolvedPluginCommand,
    args: &str,
) -> String {
    let trimmed = args.trim();
    let task = command
        .task_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match task {
        Some(template) if template.contains("{{args}}") => template.replace("{{args}}", trimmed),
        Some(template) if trimmed.is_empty() => template.to_string(),
        Some(template) => format!("{template}\n\nUser args: {trimmed}"),
        None => trimmed.to_string(),
    }
}

pub fn activation_matches_message(
    activation: &crate::plugins::ResolvedPluginActivation,
    text: &str,
    slash_command: Option<&str>,
) -> bool {
    if activation.event != "message_in" {
        return false;
    }
    let mut matched = false;
    if let Some(cmd) = activation.command.as_deref() {
        matched = slash_command == Some(cmd);
        if !matched {
            return false;
        }
    }
    if let Some(contains) = activation.contains.as_deref() {
        let contains_match = text
            .to_ascii_lowercase()
            .contains(&contains.to_ascii_lowercase());
        matched = matched || contains_match;
        if !contains_match {
            return false;
        }
    }
    if let Some(trigger) = activation.trigger.as_deref() {
        let trigger_match = text
            .to_ascii_lowercase()
            .contains(&trigger.to_ascii_lowercase());
        matched = matched || trigger_match;
        if !trigger_match {
            return false;
        }
    }
    matched
}

pub fn render_activation_task(
    activation: &crate::plugins::ResolvedPluginActivation,
    message: &str,
    slash_args: Option<&str>,
) -> String {
    let args = slash_args.unwrap_or("").trim();
    let template = activation
        .task_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(tpl) = template {
        return tpl
            .replace("{{message}}", message.trim())
            .replace("{{args}}", args);
    }
    if let Some(desc) = activation.description.as_deref().map(str::trim) {
        if !desc.is_empty() {
            return format!(
                "Activation '{}' ({}) triggered.\n\n{}\n\nMessage: {}",
                activation.name,
                activation.plugin_id,
                desc,
                message.trim()
            );
        }
    }
    format!(
        "Activation '{}' from plugin '{}' triggered by message: {}",
        activation.name,
        activation.plugin_id,
        message.trim()
    )
}

pub fn append_plugin_heartbeat_tasks(
    task: &mut String,
    plugin_tasks: &[crate::plugins::PluginHeartbeatTask],
) {
    if plugin_tasks.is_empty() {
        return;
    }
    task.push_str(
        "\n\nPlugin heartbeat items (routines/activations):\n\n\
         For each item, complete the work and include what changed.\n\n",
    );
    for item in plugin_tasks {
        task.push_str(&format!(
            "## [{}] {}::{} (priority: {}, schedule: {})\n{}\n\n",
            item.source,
            item.plugin_id,
            item.name,
            item.priority,
            item.schedule,
            item.body.trim()
        ));
    }
}

/// Parse a `/command [args]` out of an inbound message.
/// Returns `(command, args)` if the text starts with `/`; `None` otherwise.
/// Strips bot @mentions (e.g. `/start@mybot args` → `("start", "args")`).
pub fn parse_slash_command(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }
    let without_slash = &text[1..];
    // Split on first space to separate command from args.
    let (raw_cmd, args) = match without_slash.find(' ') {
        Some(pos) => (&without_slash[..pos], without_slash[pos + 1..].trim()),
        None => (without_slash, ""),
    };
    // Strip @botname suffix from command.
    let cmd = raw_cmd.split('@').next().unwrap_or(raw_cmd).to_lowercase();
    Some((cmd, args.to_string()))
}

/// Build the /help reply text listing all registered bot commands.
pub fn build_help_text(commands: &[crate::channels::BotCommand]) -> String {
    let mut out = String::from("Available commands:\n");
    for c in commands {
        out.push_str(&format!("/{} — {}\n", c.command, c.description));
    }
    out
}

/// Build the full bot command list from built-ins + discovered skills.
pub fn build_bot_commands_list(
    skills: &[skills::SkillMeta],
    plugin_commands: &[crate::plugins::ResolvedPluginCommand],
) -> Vec<crate::channels::BotCommand> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cmds = vec![
        crate::channels::BotCommand {
            command: "help".into(),
            description: "List available commands".into(),
        },
        crate::channels::BotCommand {
            command: "clear".into(),
            description: "Clear conversation history".into(),
        },
        crate::channels::BotCommand {
            command: "compact".into(),
            description: "Keep only the most recent exchanges".into(),
        },
        crate::channels::BotCommand {
            command: "stop".into(),
            description: "Stop the active agent run".into(),
        },
        crate::channels::BotCommand {
            command: "models".into(),
            description: "Choose the provider and model for this conversation".into(),
        },
    ];
    for built_in in &cmds {
        seen.insert(built_in.command.clone());
    }

    for plugin_cmd in plugin_commands {
        if seen.insert(plugin_cmd.command.clone()) {
            cmds.push(crate::channels::BotCommand {
                command: plugin_cmd.command.clone(),
                description: plugin_cmd.description.chars().take(256).collect(),
            });
        }
    }

    for skill in skills {
        let cmd = skill_to_command(&skill.name);
        if !seen.insert(cmd.clone()) {
            continue;
        }
        let desc: String = skill.description.chars().take(256).collect();
        cmds.push(crate::channels::BotCommand {
            command: cmd,
            description: desc,
        });
    }
    cmds
}

/// Compute a fingerprint over all effective skill/plugin assets for an agent.
/// Used to detect when skills/plugins are added, removed, enabled, disabled, or modified.
pub fn skills_fingerprint(agent: &AgentDef) -> u64 {
    let plugin_registry = crate::plugins::PluginRegistry::load(&agent.name);
    skills_fingerprint_with_registry(agent, &plugin_registry)
}

/// Compute a fingerprint using a pre-loaded plugin registry.
pub fn skills_fingerprint_with_registry(
    agent: &AgentDef,
    plugin_registry: &crate::plugins::PluginRegistry,
) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    plugin_registry.fingerprint.hash(&mut hasher);
    for err in &plugin_registry.load_errors {
        err.hash(&mut hasher);
    }
    let skill_roots = skill_roots_for_agent(agent, plugin_registry);
    let mut files: Vec<(String, u128)> = skill_roots
        .iter()
        .flat_map(|dir| {
            std::fs::read_dir(dir)
                .ok()
                .into_iter()
                .flat_map(|entries| entries.flatten())
                .filter_map(|e| {
                    let skill_file = e.path().join("SKILL.md");
                    if skill_file.exists() {
                        let mtime_ns = std::fs::metadata(&skill_file)
                            .and_then(|m| m.modified())
                            .and_then(|t| {
                                t.duration_since(std::time::UNIX_EPOCH)
                                    .map_err(std::io::Error::other)
                            })
                            .map(|d| d.as_nanos())
                            .unwrap_or(0);
                        Some((skill_file.to_string_lossy().into_owned(), mtime_ns))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, mtime_ns) in files {
        name.hash(&mut hasher);
        mtime_ns.hash(&mut hasher);
    }
    hasher.finish()
}

pub fn skill_roots_for_agent(
    agent: &AgentDef,
    plugin_registry: &crate::plugins::PluginRegistry,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = skills::skills_dir_local(&agent.name) {
        roots.push(local);
    }
    roots.extend(plugin_registry.enabled_skill_dirs());
    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let key = root.to_string_lossy().to_string();
        if seen.insert(key) {
            deduped.push(root);
        }
    }
    deduped
}

pub fn resolved_skill_roots(agent: &AgentDef) -> Vec<std::path::PathBuf> {
    let plugin_registry = crate::plugins::PluginRegistry::load(&agent.name);
    resolved_skill_roots_with_registry(agent, &plugin_registry)
}

pub fn resolved_skill_roots_with_registry(
    agent: &AgentDef,
    registry: &crate::plugins::PluginRegistry,
) -> Vec<std::path::PathBuf> {
    let mut roots = skill_roots_for_agent(agent, registry);
    if roots.is_empty() {
        roots.push(std::path::PathBuf::from(".that-agent/skills"));
    }
    roots
}

pub fn discover_plugin_commands(agent: &AgentDef) -> Vec<crate::plugins::ResolvedPluginCommand> {
    crate::plugins::PluginRegistry::load(&agent.name).enabled_commands()
}

pub fn discover_plugin_activations(
    agent: &AgentDef,
) -> Vec<crate::plugins::ResolvedPluginActivation> {
    crate::plugins::PluginRegistry::load(&agent.name).enabled_activations()
}

pub fn format_plugin_preamble(agent: &AgentDef, sandbox: bool) -> String {
    let registry = crate::plugins::PluginRegistry::load(&agent.name);
    format_plugin_preamble_with_registry(agent, sandbox, &registry)
}

pub fn format_plugin_preamble_with_registry(
    agent: &AgentDef,
    sandbox: bool,
    registry: &crate::plugins::PluginRegistry,
) -> String {
    format_plugin_preamble_full(agent, sandbox, registry, None)
}

pub fn format_plugin_preamble_full(
    agent: &AgentDef,
    sandbox: bool,
    registry: &crate::plugins::PluginRegistry,
    cluster: Option<&crate::plugins::cluster::ClusterRegistry>,
) -> String {
    let summaries = registry.summaries();
    if summaries.is_empty() {
        return String::new();
    }

    let plugins_path = if sandbox {
        format!("/home/agent/.that-agent/agents/{}/plugins", agent.name)
    } else {
        format!("~/.that-agent/agents/{}/plugins", agent.name)
    };
    let runtime_note = if sandbox {
        match crate::sandbox::backend::SandboxMode::from_env() {
            crate::sandbox::backend::SandboxMode::Docker => {
                let socket = crate::sandbox::docker::docker_socket_status();
                if socket.enabled {
                    format!(
                        "Sandbox runtime mode: `docker` with host Docker socket mounted at `{}`. \
                         Prefer Dockerfile + `docker build/run` or `docker compose` for deploy/run requests.",
                        socket.path.display()
                    )
                } else {
                    format!(
                        "Sandbox runtime mode: `docker` without host Docker socket at `{}`. \
                         Plugin deploy flows can still run in-container, but sibling host-container orchestration is unavailable.",
                        socket.path.display()
                    )
                }
            }
            crate::sandbox::backend::SandboxMode::Kubernetes => {
                let k8s =
                    crate::sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
                let image_backend = std::env::var("THAT_IMAGE_BUILD_BACKEND")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "Sandbox runtime mode: `kubernetes` (namespace `{}`, registry `{}`). \
                     Active image builder: `{}` from `<system-reminder>`. \
                     Follow backend strictly: use BuildKit when backend is `buildkit`; \
                     do not request Docker socket unless backend is explicitly `docker`. \
                     Prefer manifest + kubectl apply workflows with rollout checks.",
                    k8s.namespace, k8s.registry, image_backend
                )
            }
        }
    } else if trusted_local_sandbox_enabled() {
        match crate::sandbox::backend::SandboxMode::from_env() {
            crate::sandbox::backend::SandboxMode::Docker => {
                let socket = crate::sandbox::docker::docker_socket_status();
                if socket.enabled {
                    format!(
                        "Trusted local runtime mode: `docker` with host Docker socket at `{}`. \
                         Docker deploy flows are available.",
                        socket.path.display()
                    )
                } else {
                    format!(
                        "Trusted local runtime mode: `docker` without host Docker socket at `{}`. \
                         Avoid Docker daemon workflows unless socket access becomes available.",
                        socket.path.display()
                    )
                }
            }
            crate::sandbox::backend::SandboxMode::Kubernetes => {
                let k8s =
                    crate::sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
                let image_backend = std::env::var("THAT_IMAGE_BUILD_BACKEND")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "Trusted local runtime mode: `kubernetes` (namespace `{}`, registry `{}`). \
                     Active image builder: `{}` from `<system-reminder>`. \
                     Follow backend strictly: use BuildKit when backend is `buildkit`; \
                     do not request Docker socket unless backend is explicitly `docker`. \
                     Prefer manifest + kubectl apply workflows with rollout checks.",
                    k8s.namespace, k8s.registry, image_backend
                )
            }
        }
    } else {
        "Runtime backends: `docker` (default; can run/deploy via Docker socket) and `kubernetes`."
            .to_string()
    };

    let mut out = String::new();
    out.push_str("## Plugins\n\n");
    out.push_str(&format!(
        "Plugin directory: `{plugins_path}`  \n\
         Plugins are agent-scoped and must keep assets inside their own plugin directory.  \n\
         Standard plugin subdirectories: `skills/`, `scripts/`, `deploy/`, `state/`, `artifacts/`.  \n\
         {runtime_note}  \n\
         Plugins can add commands, skills, routines, activations, and emoji packs.  \n\
         Changes are hot-reloaded automatically.\n\n"
    ));
    // Pre-load cluster plugin statuses if available.
    let cluster_plugins = cluster.and_then(|c| c.list().ok()).unwrap_or_default();

    for plugin in registry.enabled_plugins() {
        let desc = plugin
            .manifest
            .description
            .as_deref()
            .unwrap_or("No description");
        let skill_dir = plugin.dir.join(plugin.manifest.skills_subdir());
        let plugin_skills = skills::discover_skills_local(&skill_dir);
        let skill_line = if plugin_skills.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = plugin_skills
                .iter()
                .map(|s| format!("`{}` ({})", s.name, s.description))
                .collect();
            format!("  Skills: {}\n", names.join(", "))
        };
        let status_line = cluster_plugins
            .iter()
            .find(|cp| cp.id == plugin.manifest.id)
            .and_then(|cp| cp.deploy_status.as_deref())
            .map(|s| format!(" | deploy: **{s}**"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- **{}** (`{}` v{}): {}.{status_line} Commands: {}, Routines: {}, Activations: {}, Emojis: {}\n{skill_line}",
            plugin.manifest.display_name(),
            plugin.manifest.id,
            plugin.manifest.version,
            desc,
            plugin.manifest.commands.len(),
            plugin.manifest.routines.len(),
            plugin.manifest.activations.len(),
            plugin.manifest.emojis.len(),
        ));
    }
    let emojis = registry.enabled_emojis();
    if !emojis.is_empty() {
        out.push_str("### Emoji Catalog\n\n");
        for emoji in emojis {
            out.push_str(&format!(
                "- `{}.{}` => {}\n",
                emoji.plugin_id, emoji.name, emoji.value
            ));
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Compute a fingerprint from a file's mtime. Returns 0 if the file is missing or unreadable.
pub fn file_mtime_hash(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Discover skills for the current mode (sandbox or local).
pub fn discover_skills(agent: &AgentDef, _sandbox: bool) -> Vec<skills::SkillMeta> {
    let plugin_registry = crate::plugins::PluginRegistry::load(&agent.name);
    discover_skills_with_registry(agent, &plugin_registry)
}

/// Discover skills using a pre-loaded plugin registry, avoiding redundant I/O.
pub fn discover_skills_with_registry(
    agent: &AgentDef,
    registry: &crate::plugins::PluginRegistry,
) -> Vec<skills::SkillMeta> {
    for err in &registry.load_errors {
        tracing::warn!(agent = %agent.name, error = %err, "Plugin load warning");
    }
    let roots = skill_roots_for_agent(agent, registry);
    let mut skills_found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        for skill in skills::discover_skills_local(&root) {
            if seen.insert(skill.name.clone()) {
                skills_found.push(skill);
            }
        }
    }
    skills_found.sort_by(|a, b| a.name.cmp(&b.name));
    skills_found
}
