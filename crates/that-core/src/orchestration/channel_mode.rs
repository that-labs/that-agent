use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, Utc};
use tracing::{debug, error, info, warn};

use crate::agent_loop::{Message, SteeringQueue};
use crate::config::{AgentDef, WorkspaceConfig};
use crate::heartbeat;
use crate::model_catalog::{available_providers, normalize_provider, suggested_models};
use crate::session::{
    new_run_id, rebuild_history_recent, ChannelPreferences, RunStatus, SessionManager,
    TranscriptEntry, TranscriptEvent,
};

use super::config::*;
use super::discovery::*;
use super::execution::execute_agent_run_channel;
use super::preamble::build_preamble;
use super::setup::{prepare_container, resolve_agent_workspace};
use super::support::{build_compact_summary, load_workspace_files};

use crate::skills;

/// Shared hot-reloadable state: rebuilt when skills files or agent config change on disk.
pub(super) struct HotState {
    pub found_skills: Vec<skills::SkillMeta>,
    pub plugin_commands: Vec<that_plugins::ResolvedPluginCommand>,
    pub plugin_activations: Vec<that_plugins::ResolvedPluginActivation>,
    pub bot_commands: Vec<that_channels::BotCommand>,
    pub preamble: String,
    pub skills_fp: u64,
    /// mtime hash of the agent's TOML config file for detecting hot-reload.
    pub agent_def_fp: u64,
    /// Current agent definition — updated when the config file changes.
    pub agent: AgentDef,
    /// Pre-resolved skill roots for tool dispatch.
    pub skill_roots: Vec<std::path::PathBuf>,
}

type ChannelSessions = Arc<
    tokio::sync::Mutex<std::collections::HashMap<String, (String, Vec<Message>, Arc<AtomicBool>)>>,
>;
type ChannelModelPrefs =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, ChannelPreferences>>>;
type PluginTaskEntry = (
    String,
    Option<String>,
    Option<String>,
    Vec<that_plugins::PluginHeartbeatTask>,
);

pub(super) type SenderRunLock = Arc<tokio::sync::Mutex<()>>;
pub(super) type SenderRunLocks =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, SenderRunLock>>>;
#[derive(Clone)]
pub(super) struct ActiveSenderRun {
    run_id: u64,
    abort: tokio::task::AbortHandle,
    steering: SteeringQueue,
}
pub(super) type ActiveSenderRuns =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, ActiveSenderRun>>>;

/// Remove a sender lock entry when no other tasks still reference it.
///
/// This keeps the lock map bounded to active senders instead of growing forever.
pub async fn evict_sender_lock_if_idle(
    sender_locks: &SenderRunLocks,
    sender_key: &str,
    sender_lock: &SenderRunLock,
) {
    if Arc::strong_count(sender_lock) > 1 {
        return;
    }
    let mut locks = sender_locks.lock().await;
    if Arc::strong_count(sender_lock) > 1 {
        return;
    }
    let should_remove = locks
        .get(sender_key)
        .map(|current| Arc::ptr_eq(current, sender_lock))
        .unwrap_or(false);
    if should_remove {
        locks.remove(sender_key);
    }
}

/// Abort and clear the currently active run for a sender key, if any.
/// Also cleans up any ephemeral K8s Jobs spawned by this agent.
pub(super) async fn stop_active_sender_run(
    active_runs: &ActiveSenderRuns,
    sender_key: &str,
) -> bool {
    let active = active_runs.lock().await.remove(sender_key);
    if let Some(run) = active {
        run.abort.abort();
        // Clean up ephemeral child Jobs in K8s (fire-and-forget)
        if crate::agents::is_k8s_mode() {
            tokio::spawn(async {
                if let Err(e) = crate::agents::cleanup_ephemeral_children().await {
                    tracing::warn!("failed to clean up child jobs on /stop: {e}");
                }
            });
        }
        true
    } else {
        false
    }
}

fn apply_channel_preferences(agent: &AgentDef, prefs: &ChannelPreferences) -> AgentDef {
    let mut effective = agent.clone();
    if let (Some(provider), Some(model)) = (prefs.provider.as_deref(), prefs.model.as_deref()) {
        effective.provider = provider.to_string();
        effective.model = model.to_string();
    }
    effective
}

fn active_provider<'a>(agent: &'a AgentDef, prefs: &'a ChannelPreferences) -> &'a str {
    prefs.provider.as_deref().unwrap_or(&agent.provider)
}

fn active_model<'a>(agent: &'a AgentDef, prefs: &'a ChannelPreferences) -> &'a str {
    prefs.model.as_deref().unwrap_or(&agent.model)
}

fn channel_model_status(agent: &AgentDef, prefs: &ChannelPreferences) -> String {
    let source = if prefs.is_default() {
        "agent default"
    } else {
        "channel override"
    };
    format!(
        "Current model for this conversation: {} / {} ({source}).",
        active_provider(agent, prefs),
        active_model(agent, prefs),
    )
}

fn channel_config_slug(sender_key: &str) -> String {
    let slug: String = sender_key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "default".into()
    } else {
        slug
    }
}

fn effective_channel_config_host_path(
    agent_name: &str,
    sender_key: &str,
) -> Option<std::path::PathBuf> {
    let file_name = format!("{}.toml", channel_config_slug(sender_key));
    dirs::home_dir().map(|home| {
        home.join(".that-agent")
            .join("state")
            .join("channel-configs")
            .join(agent_name)
            .join(file_name)
    })
}

fn effective_channel_config_visible_path(
    agent_name: &str,
    sender_key: &str,
    sandbox: bool,
) -> Option<String> {
    let file_name = format!("{}.toml", channel_config_slug(sender_key));
    if sandbox {
        Some(format!(
            "/home/agent/.that-agent/state/channel-configs/{agent_name}/{file_name}"
        ))
    } else {
        effective_channel_config_host_path(agent_name, sender_key)
            .map(|path| path.to_string_lossy().into_owned())
    }
}

fn persist_effective_channel_config(
    agent: &AgentDef,
    sender_key: &str,
    sandbox: bool,
) -> Option<String> {
    let path = effective_channel_config_host_path(&agent.name, sender_key)?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let text = toml::to_string_pretty(agent).ok()?;
    std::fs::write(&path, text).ok()?;
    effective_channel_config_visible_path(&agent.name, sender_key, sandbox)
}

fn remove_effective_channel_config(agent_name: &str, sender_key: &str) {
    if let Some(path) = effective_channel_config_host_path(agent_name, sender_key) {
        let _ = std::fs::remove_file(path);
    }
}

fn provider_menu_text(
    agent: &AgentDef,
    prefs: &ChannelPreferences,
    providers: &[String],
) -> String {
    let mut text = format!(
        "{}\n\nAvailable providers:\n",
        channel_model_status(agent, prefs)
    );
    for provider in providers {
        let marker = if provider == active_provider(agent, prefs) {
            "✓"
        } else {
            "•"
        };
        text.push_str(&format!("{marker} {provider}\n"));
    }
    text.push_str("\nUse /models <provider> to see suggested models, or /models reset to go back to the agent default.");
    text
}

fn model_menu_text(agent: &AgentDef, prefs: &ChannelPreferences, provider: &str) -> String {
    let mut text = format!(
        "{}\n\nSuggested models for {provider}:\n",
        channel_model_status(agent, prefs)
    );
    for model in suggested_models(provider) {
        let marker =
            if provider == active_provider(agent, prefs) && model == active_model(agent, prefs) {
                "✓"
            } else {
                "•"
            };
        text.push_str(&format!("{marker} {model}\n"));
    }
    text.push_str("\nUse /models <provider> <model> to set a custom model.");
    text
}

fn provider_menu_markup(
    agent: &AgentDef,
    prefs: &ChannelPreferences,
    providers: &[String],
) -> Option<that_channels::ReplyMarkup> {
    if providers.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for chunk in providers.chunks(2) {
        let row = chunk
            .iter()
            .map(|provider| that_channels::InlineButton {
                text: if provider == active_provider(agent, prefs) {
                    format!("✓ {provider}")
                } else {
                    provider.to_string()
                },
                callback_data: format!("/models {provider}"),
            })
            .collect::<Vec<_>>();
        rows.push(row);
    }
    if !prefs.is_default() {
        rows.push(vec![that_channels::InlineButton {
            text: "Reset to default".into(),
            callback_data: "/models reset".into(),
        }]);
    }
    Some(that_channels::ReplyMarkup::InlineKeyboard(rows))
}

fn model_menu_markup(
    agent: &AgentDef,
    prefs: &ChannelPreferences,
    provider: &str,
) -> Option<that_channels::ReplyMarkup> {
    let models = suggested_models(provider);
    if models.is_empty() {
        return None;
    }
    let mut rows = Vec::new();
    for chunk in models.chunks(2) {
        let row = chunk
            .iter()
            .map(|model| that_channels::InlineButton {
                text: if provider == active_provider(agent, prefs)
                    && model == active_model(agent, prefs)
                {
                    format!("✓ {model}")
                } else {
                    model.to_string()
                },
                callback_data: format!("/models {provider} {model}"),
            })
            .collect::<Vec<_>>();
        rows.push(row);
    }
    rows.push(vec![that_channels::InlineButton {
        text: "Back".into(),
        callback_data: "/models".into(),
    }]);
    Some(that_channels::ReplyMarkup::InlineKeyboard(rows))
}

async fn send_channel_menu(
    router: &Arc<that_channels::ChannelRouter>,
    channel_id: &str,
    target: &that_channels::OutboundTarget,
    text: String,
    reply_markup: Option<that_channels::ReplyMarkup>,
) {
    if let Some(reply_markup) = reply_markup {
        let msg = that_channels::OutboundMessage {
            text: text.clone(),
            parse_mode: Some(that_channels::ParseMode::Plain),
            reply_markup: Some(reply_markup),
            reply_to_message_id: target.reply_to_message_id.clone(),
        };
        if router
            .send_message(channel_id, msg, Some(target))
            .await
            .is_ok()
        {
            return;
        }
    }
    router.notify_channel(channel_id, &text, Some(target)).await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_models_command(
    router: &Arc<that_channels::ChannelRouter>,
    session_mgr: &Arc<SessionManager>,
    model_prefs: &ChannelModelPrefs,
    sender_key: &str,
    channel_id: &str,
    target: &that_channels::OutboundTarget,
    agent: &AgentDef,
    current_prefs: &ChannelPreferences,
    sandbox: bool,
    args: &str,
) {
    let providers = available_providers();
    if providers.is_empty() {
        router
            .notify_channel(
                channel_id,
                "No provider API keys are configured. Set an API key first, then try /models again.",
                Some(target),
            )
            .await;
        return;
    }

    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.as_slice() {
        [] => {
            send_channel_menu(
                router,
                channel_id,
                target,
                provider_menu_text(agent, current_prefs, &providers),
                provider_menu_markup(agent, current_prefs, &providers),
            )
            .await;
        }
        ["reset"] => {
            let prefs = ChannelPreferences::default();
            {
                let mut map = model_prefs.lock().await;
                map.remove(sender_key);
            }
            session_mgr.save_channel_preferences(sender_key, &prefs);
            remove_effective_channel_config(&agent.name, sender_key);
            let message = format!(
                "Model reset. This conversation now uses the agent default: {} / {}.",
                agent.provider, agent.model
            );
            router
                .notify_channel(channel_id, &message, Some(target))
                .await;
        }
        [provider] => {
            let Some(provider) = normalize_provider(provider) else {
                router
                    .notify_channel(
                        channel_id,
                        "Unknown provider. Use /models to pick from the configured providers.",
                        Some(target),
                    )
                    .await;
                return;
            };
            if !providers.iter().any(|candidate| candidate == &provider) {
                let message = format!("Provider '{provider}' is not configured on this agent.");
                router
                    .notify_channel(channel_id, &message, Some(target))
                    .await;
                return;
            }
            send_channel_menu(
                router,
                channel_id,
                target,
                model_menu_text(agent, current_prefs, &provider),
                model_menu_markup(agent, current_prefs, &provider),
            )
            .await;
        }
        [provider, model_parts @ ..] => {
            let Some(provider) = normalize_provider(provider) else {
                router
                    .notify_channel(
                        channel_id,
                        "Unknown provider. Use /models to pick from the configured providers.",
                        Some(target),
                    )
                    .await;
                return;
            };
            if !providers.iter().any(|candidate| candidate == &provider) {
                let message = format!("Provider '{provider}' is not configured on this agent.");
                router
                    .notify_channel(channel_id, &message, Some(target))
                    .await;
                return;
            }
            let model = model_parts.join(" ").trim().to_string();
            if model.is_empty() {
                router
                    .notify_channel(
                        channel_id,
                        "Model name is required. Use /models <provider> first to see suggestions.",
                        Some(target),
                    )
                    .await;
                return;
            }
            let prefs = ChannelPreferences {
                provider: Some(provider.to_string()),
                model: Some(model.clone()),
            };
            {
                let mut map = model_prefs.lock().await;
                map.insert(sender_key.to_string(), prefs.clone());
            }
            session_mgr.save_channel_preferences(sender_key, &prefs);
            let effective_agent = apply_channel_preferences(agent, &prefs);
            let message = if let Some(path) =
                persist_effective_channel_config(&effective_agent, sender_key, sandbox)
            {
                format!(
                    "Model for this conversation set to {provider} / {model}. New runs will use it. Effective runtime config: {path}"
                )
            } else {
                format!(
                    "Model for this conversation set to {provider} / {model}. New runs will use it."
                )
            };
            router
                .notify_channel(channel_id, &message, Some(target))
                .await;
        }
    }
}

pub async fn run_listen(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    sandbox: bool,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    inbound_rx: tokio::sync::mpsc::UnboundedReceiver<that_channels::InboundMessage>,
) -> Result<()> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};

    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = Arc::new(SessionManager::new(&state_dir)?);
    let agent_workspace = resolve_agent_workspace(ws, agent)?;
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;
    let ws_files = load_workspace_files(agent, sandbox);

    // Initial skill discovery + preamble — load plugin registry once.
    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    let found_skills = discover_skills_with_registry(agent, &plugin_registry);
    let plugin_commands = plugin_registry.enabled_commands();
    let plugin_activations = plugin_registry.enabled_activations();
    let bot_commands = build_bot_commands_list(&found_skills, &plugin_commands);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();
    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws_files,
        0,
        "listen",
        &session_summaries,
        Some(&plugin_registry),
        None,
    );
    let skill_roots = resolved_skill_roots_with_registry(agent, &plugin_registry);
    let skills_fp = skills_fingerprint_with_registry(agent, &plugin_registry);

    // Compute initial fingerprint for the agent config file.
    // Must match the resolution order in WorkspaceConfig::load_agent():
    // preferred = agents/<name>/config.toml, legacy = agents/<name>.toml
    let agent_config_path = {
        let agents_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".that-agent")
            .join("agents");
        let preferred = agents_dir.join(&agent.name).join("config.toml");
        let legacy = agents_dir.join(format!("{}.toml", agent.name));
        if preferred.exists() {
            preferred
        } else {
            legacy
        }
    };
    let agent_def_fp = file_mtime_hash(&agent_config_path);

    // Hot state shared between the reload task and message handlers.
    let hot = Arc::new(RwLock::new(HotState {
        found_skills,
        plugin_commands,
        plugin_activations,
        bot_commands,
        preamble,
        skills_fp,
        agent_def_fp,
        agent: agent.clone(),
        skill_roots,
    }));

    // Per-sender state: key = "channel_id:sender_id" → (session_id, history, show_work).
    let sessions: ChannelSessions = Arc::new(Mutex::new(HashMap::new()));
    let model_prefs: ChannelModelPrefs =
        Arc::new(Mutex::new(session_mgr.load_channel_preferences()));
    let channel_index_lock = Arc::new(Mutex::new(()));
    let plugin_runtime_lock = Arc::new(Mutex::new(()));
    // Queued __notify__ messages drained each heartbeat tick.
    let notification_queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Queued deferred inbound messages drained each heartbeat tick.
    let inbound_queue: Arc<Mutex<Vec<that_channels::InboundMessage>>> =
        Arc::new(Mutex::new(Vec::new()));

    eprintln!(
        "[that] Listening on channels: {} (primary: {})\n[that] Agent: {} — Ctrl+C to stop.",
        router.channel_ids().await,
        router.primary_id().await,
        agent.name,
    );

    // Initialize immediate channels (HTTP gateway) — deferred channels (Telegram, etc.)
    // run after the readiness probe so external API calls don't block K8s startup.
    router.initialize().await;

    router.start_listeners().await?;

    // Signal K8s readiness — gateway is bound and listening.
    let _ = std::fs::File::create("/tmp/that-agent-ready");

    // Now initialize deferred channels (external API validation: Telegram getMe, etc.).
    router.initialize_deferred().await;

    // If unbootstrapped, greet on all channels so the user knows to start the ceremony.
    // Otherwise, send a restart marker so the user knows we're back.
    if ws_files.needs_bootstrap() {
        let name = &agent.name;
        router
            .notify_all(&format!(
                "Hey! I'm {name} — I just woke up for the first time. \
                 Send me a message to start our bootstrap ceremony and figure out who I am."
            ))
            .await;
    } else {
        // Build restart summary from last session + active tasks.
        let mut summary = format!("🔄 {} restarted — back online.", agent.name);

        // Find the last human message from a real channel session.
        // Skip heartbeat/system sender keys — only look at channel:conversation:user sessions.
        let channel_sessions = session_mgr.load_channel_sessions();
        let human_session = channel_sessions
            .iter()
            .find(|(key, _)| !key.starts_with("heartbeat:") && !key.starts_with("inbound:"));
        if let Some((_sender, sid)) = human_session {
            if let Ok(entries) = session_mgr.read_transcript(sid) {
                // Walk backwards to find a genuine human message (not system-injected).
                let last_human_msg = entries.iter().rev().find_map(|e| match &e.event {
                    crate::session::TranscriptEvent::UserMessage { content }
                        if !content.starts_with("Heartbeat")
                            && !content.starts_with('[')
                            && !content.starts_with('#') =>
                    {
                        let preview: String = content.chars().take(100).collect();
                        Some(preview)
                    }
                    _ => None,
                });
                if let Some(msg) = last_human_msg {
                    summary.push_str(&format!("\nLast request: \"{msg}\""));
                }
            }
        }

        // Active task summary from task registry.
        let task_reg = crate::agents::AgentTaskRegistry::new(state_dir.join("agent_tasks.json"));
        if let Ok(tasks) = task_reg.list_active() {
            if !tasks.is_empty() {
                summary.push_str(&format!("\nActive tasks: {}", tasks.len()));
                for t in tasks.iter().take(5) {
                    let pad_hint = if t.scratchpad.is_empty() {
                        String::new()
                    } else {
                        format!(" [{} scratchpad notes]", t.scratchpad.len())
                    };
                    summary.push_str(&format!(
                        "\n• {} ({}){}: {}",
                        t.agent,
                        t.state,
                        pad_hint,
                        t.messages
                            .last()
                            .map(|m| m.text.chars().take(80).collect::<String>())
                            .unwrap_or_default()
                    ));
                }
            }
        }

        router.notify_all(&summary).await;
    }

    // ── Boot-time registry hydration ──────────────────────────────────────
    let cluster_registry = Arc::new(that_plugins::cluster::ClusterRegistry::new(
        state_dir.join("cluster_plugins.json"),
    ));
    let channel_registry = Arc::new(that_channels::registry::DynamicChannelRegistry::new(
        state_dir.join("dynamic_channels.json"),
    ));
    let gateway_routes_path = state_dir.join("gateway_routes.json");
    // Expose path to the HTTP gateway adapter so its fallback handler can find dynamic routes.
    std::env::set_var(
        "THAT_GATEWAY_ROUTES_PATH",
        gateway_routes_path.display().to_string(),
    );
    let route_registry = Arc::new(that_channels::DynamicRouteRegistry::new(
        gateway_routes_path,
    ));
    // Re-hydrate persisted gateway channels into the live router.
    if let Ok(entries) = channel_registry.list() {
        for entry in entries {
            let adapter = that_channels::GatewayChannelAdapter::new(entry);
            router.add_channel(Arc::new(adapter)).await;
        }
    }

    // Register initial commands.
    router
        .register_commands(&hot.read().await.bot_commands)
        .await;

    // ── Background hot-reload task ──────────────────────────────────────────
    // Polls skills/plugins every 5 seconds. When the fingerprint changes
    // (new/removed/modified SKILL.md files, plugin state/manifest updates), rebuilds runtime state and
    // re-registers bot commands — all without restarting the process.
    // Also monitors the agent TOML config for changes and hot-reloads it.
    {
        let hot = Arc::clone(&hot);
        let router = Arc::clone(&router);
        let mut agent_hot = agent.clone();
        let agent_workspace = agent_workspace.clone();
        let agent_config_path_hot = agent_config_path.clone();
        let session_mgr = Arc::clone(&session_mgr);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

                // ── Skills hot-reload — load registry once per cycle ─────
                let reload_registry = that_plugins::PluginRegistry::load(&agent_hot.name);
                let new_skills_fp = skills_fingerprint_with_registry(&agent_hot, &reload_registry);
                let skills_changed = new_skills_fp != hot.read().await.skills_fp;

                // ── Agent config hot-reload ───────────────────────────────
                let new_cfg_fp = file_mtime_hash(&agent_config_path_hot);
                let config_changed = new_cfg_fp != hot.read().await.agent_def_fp;

                if config_changed {
                    if let Ok(mut new_agent) = AgentDef::from_file(&agent_config_path_hot) {
                        new_agent.name = agent_hot.name.clone();
                        // Push runtime-mutable config (e.g. allowed_senders) to live adapters
                        // so changes take effect immediately without restarting the process.
                        router
                            .apply_config_updates(&new_agent.channels.adapters)
                            .await;
                        agent_hot = new_agent;
                        info!("Hot-reloaded agent config");
                    }
                }

                if skills_changed || config_changed {
                    let new_skills = discover_skills_with_registry(&agent_hot, &reload_registry);
                    let new_plugin_commands = reload_registry.enabled_commands();
                    let new_plugin_activations = reload_registry.enabled_activations();
                    let new_commands = build_bot_commands_list(&new_skills, &new_plugin_commands);
                    let summaries = session_mgr.session_summaries(5).unwrap_or_default();
                    // Re-read workspace files on hot-reload — agent may have edited them.
                    let new_ws = load_workspace_files(&agent_hot, sandbox);
                    let new_preamble = build_preamble(
                        &agent_workspace,
                        &agent_hot,
                        sandbox,
                        &new_skills,
                        &new_ws,
                        0,
                        "listen",
                        &summaries,
                        Some(&reload_registry),
                        None,
                    );
                    if skills_changed {
                        info!(count = new_skills.len(), "Hot-reloading skills");
                    }
                    let new_skill_roots =
                        resolved_skill_roots_with_registry(&agent_hot, &reload_registry);
                    router.register_commands(&new_commands).await;
                    let mut state = hot.write().await;
                    state.found_skills = new_skills;
                    state.plugin_commands = new_plugin_commands;
                    state.plugin_activations = new_plugin_activations;
                    state.bot_commands = new_commands;
                    state.preamble = new_preamble;
                    state.skills_fp = new_skills_fp;
                    state.agent_def_fp = new_cfg_fp;
                    state.agent = agent_hot.clone();
                    state.skill_roots = new_skill_roots;
                }
            }
        });
    }

    let sender_locks = SenderRunLocks::default();
    let active_sender_runs: ActiveSenderRuns = Arc::default();
    let sender_run_seq = Arc::new(AtomicU64::new(1));

    // ── Background heartbeat monitor ────────────────────────────────────────
    // Polls Heartbeat.md every heartbeat_interval seconds. Due entries are
    // dispatched as autonomous agent runs. Global items use "heartbeat:system";
    // route-aware plugin activation items are isolated by channel/chat sender key.
    {
        let hot = Arc::clone(&hot);
        let container_hb = container.clone();
        let session_mgr_hb = Arc::clone(&session_mgr);
        let sessions_hb = Arc::clone(&sessions);
        let channel_index_lock_hb = Arc::clone(&channel_index_lock);
        let plugin_runtime_lock_hb = Arc::clone(&plugin_runtime_lock);
        let router_hb = Arc::clone(&router);
        let cluster_registry_hb = Arc::clone(&cluster_registry);
        let channel_registry_hb = Arc::clone(&channel_registry);
        let route_registry_hb = Arc::clone(&route_registry);
        let notification_queue_hb = Arc::clone(&notification_queue);
        let inbound_queue_hb = Arc::clone(&inbound_queue);
        let active_runs_hb = Arc::clone(&active_sender_runs);
        let interval_secs = agent.heartbeat_interval.unwrap_or(10).max(1);

        let plugin_dir_hb = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".that-agent")
            .join("plugins");
        // Dedicated liveness ticker — independent of heartbeat work so long-running
        // agent runs don't starve the K8s liveness probe.
        tokio::spawn(async {
            let mut liveness = tokio::time::interval(tokio::time::Duration::from_secs(5));
            liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                liveness.tick().await;
                let _ = tokio::fs::File::create("/tmp/that-agent-alive").await;
            }
        });

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_reconcile = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(120))
                .unwrap_or_else(std::time::Instant::now);
            loop {
                ticker.tick().await;

                // Reconcile plugin deploy status every 60s.
                if last_reconcile.elapsed() >= std::time::Duration::from_secs(60) {
                    if let Err(e) = cluster_registry_hb.reconcile_status(&plugin_dir_hb).await {
                        tracing::warn!(error = %e, "Plugin status reconciliation failed");
                    }
                    last_reconcile = std::time::Instant::now();
                }

                // Snapshot current preamble, agent, and skill roots from hot state.
                let (preamble_hb, current_agent, skill_roots_hb) = {
                    let state = hot.read().await;
                    (
                        state.preamble.clone(),
                        state.agent.clone(),
                        state.skill_roots.clone(),
                    )
                };

                // Ensure Heartbeat.md exists, then load entries.
                if let Some(c) = &container_hb {
                    match heartbeat::ensure_heartbeat_sandbox(c, &current_agent.name) {
                        Ok(true) => {
                            tracing::info!(agent = %current_agent.name, "Bootstrapped Heartbeat.md in sandbox");
                        }
                        Ok(false) => {}
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to bootstrap Heartbeat.md in sandbox"
                            );
                        }
                    }
                } else {
                    match heartbeat::ensure_heartbeat_local(&current_agent.name) {
                        Ok(true) => {
                            tracing::info!(agent = %current_agent.name, "Bootstrapped Heartbeat.md");
                        }
                        Ok(false) => {}
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to bootstrap Heartbeat.md"
                            );
                        }
                    }
                }

                // Load heartbeat entries.
                let entries_opt = if let Some(c) = &container_hb {
                    heartbeat::load_heartbeat_sandbox(c, &current_agent.name)
                } else {
                    heartbeat::load_heartbeat_local(&current_agent.name)
                };
                let mut entries = entries_opt.unwrap_or_default();

                // Find due pending entries sorted urgent-first.
                let mut due_indices: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        matches!(
                            e.status,
                            heartbeat::Status::Pending
                                | heartbeat::Status::Running
                                | heartbeat::Status::Processing
                        ) && heartbeat::is_entry_due(e, current_agent.timezone.as_deref())
                    })
                    .map(|(i, _)| i)
                    .collect();

                due_indices.sort_by_key(|&i| match entries[i].priority {
                    heartbeat::Priority::Urgent => 0u8,
                    heartbeat::Priority::High => 1,
                    heartbeat::Priority::Normal => 2,
                    heartbeat::Priority::Low => 3,
                    heartbeat::Priority::Unknown(_) => 4,
                });

                // Suppress non-urgent heartbeat entries when a task run is active.
                // This prevents the agent from doing unrelated background work (status
                // reports, daily checks) while focused on a parent-dispatched task.
                let has_active_task_run = {
                    let runs = active_runs_hb.lock().await;
                    runs.keys().any(|k| !k.starts_with("heartbeat:"))
                };
                if has_active_task_run && !due_indices.is_empty() {
                    due_indices.retain(|&i| {
                        matches!(
                            entries[i].priority,
                            heartbeat::Priority::Urgent | heartbeat::Priority::High
                        )
                    });
                }

                let plugin_tasks = {
                    let _runtime_guard = plugin_runtime_lock_hb.lock().await;
                    let plugin_registry = that_plugins::PluginRegistry::load(&current_agent.name);
                    match that_plugins::collect_due_heartbeat_tasks(
                        &current_agent.name,
                        &plugin_registry,
                    ) {
                        Ok(tasks) => tasks,
                        Err(err) => {
                            tracing::warn!(
                                agent = %current_agent.name,
                                error = %err,
                                "Failed to collect plugin heartbeat tasks"
                            );
                            Vec::new()
                        }
                    }
                };

                let mut scoped_plugin_tasks: std::collections::BTreeMap<String, PluginTaskEntry> =
                    std::collections::BTreeMap::new();
                let mut unscoped_plugin_tasks: Vec<that_plugins::PluginHeartbeatTask> = Vec::new();
                for item in plugin_tasks {
                    if let Some(route) = item.route.as_ref() {
                        let channel_id = route
                            .channel_id
                            .as_deref()
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(ToOwned::to_owned);
                        if let Some(channel_id) = channel_id {
                            let conversation_id = route
                                .conversation_id
                                .as_deref()
                                .map(str::trim)
                                .filter(|v| !v.is_empty())
                                .map(ToOwned::to_owned);
                            let sender_id = route
                                .sender_id
                                .as_deref()
                                .map(str::trim)
                                .filter(|v| !v.is_empty())
                                .map(ToOwned::to_owned);
                            let key = format!(
                                "{}:{}:{}",
                                channel_id,
                                conversation_id.clone().unwrap_or_default(),
                                sender_id.clone().unwrap_or_default(),
                            );
                            scoped_plugin_tasks
                                .entry(key)
                                .or_insert_with(|| {
                                    (channel_id, conversation_id, sender_id, Vec::new())
                                })
                                .3
                                .push(item);
                            continue;
                        }
                    }
                    unscoped_plugin_tasks.push(item);
                }

                let pending_notifs: Vec<String> =
                    std::mem::take(&mut *notification_queue_hb.lock().await);
                let pending_inbound: Vec<that_channels::InboundMessage> =
                    std::mem::take(&mut *inbound_queue_hb.lock().await);

                if due_indices.is_empty()
                    && unscoped_plugin_tasks.is_empty()
                    && scoped_plugin_tasks.is_empty()
                    && pending_notifs.is_empty()
                    && pending_inbound.is_empty()
                {
                    continue;
                }

                let plugin_only_notify_guidance =
                    "\n\nUse the `channel_notify` tool to keep users informed:\n\
                     - Send a brief summary when meaningful work is completed.\n\
                     - Send a notice if you are blocked or cannot complete an item.\n\
                     - Skip the notification if all items are routine housekeeping with no user-visible outcome.";

                // Mark as running and stamp last_run before dispatch so recurring
                // schedules are tracked and don't retrigger in the same slot.
                let dispatch_started_at = Local::now();
                for &i in &due_indices {
                    entries[i].last_run = Some(dispatch_started_at);
                    entries[i].status = match entries[i].schedule {
                        heartbeat::Schedule::Once => heartbeat::Status::Done,
                        _ => heartbeat::Status::Running,
                    };
                }

                if !due_indices.is_empty() {
                    if let Some(c) = &container_hb {
                        let _ = heartbeat::save_heartbeat_sandbox(c, &current_agent.name, &entries);
                    } else {
                        let _ = heartbeat::save_heartbeat_local(&current_agent.name, &entries);
                    }
                }

                let due_refs: Vec<&heartbeat::HeartbeatEntry> =
                    due_indices.iter().map(|&i| &entries[i]).collect();
                if !due_refs.is_empty()
                    || !unscoped_plugin_tasks.is_empty()
                    || !pending_notifs.is_empty()
                    || !pending_inbound.is_empty()
                {
                    let mut task = if due_refs.is_empty()
                        && pending_notifs.is_empty()
                        && pending_inbound.is_empty()
                    {
                        String::from(
                            "Heartbeat check-in. Process the following plugin-triggered items:\n\n",
                        )
                    } else if due_refs.is_empty() {
                        String::from("Heartbeat check-in.\n\n")
                    } else {
                        heartbeat::format_heartbeat_task(&due_refs)
                    };
                    append_plugin_heartbeat_tasks(&mut task, &unscoped_plugin_tasks);
                    if !pending_notifs.is_empty() {
                        task.push_str("\n\n## Pending agent notifications:\n");
                        for n in &pending_notifs {
                            task.push_str(&format!("- {n}\n"));
                        }
                    }
                    if !pending_inbound.is_empty() {
                        task.push_str("\n\n## Pending inbound requests:\n");
                        for m in &pending_inbound {
                            let cb = m.callback_url.as_deref().unwrap_or("no callback");
                            // Extract task_id from callback URL for scratchpad access.
                            let task_id_hint = m
                                .callback_url
                                .as_deref()
                                .and_then(|u| u.split("task_id=").nth(1))
                                .map(|t| t.split('&').next().unwrap_or(t))
                                .unwrap_or("");
                            let attach_count = m.attachments.len();
                            let tid_line = if task_id_hint.is_empty() {
                                String::new()
                            } else {
                                format!(" (task_id: {task_id_hint})")
                            };
                            task.push_str(&format!(
                                "### [sender: {}]{tid_line} (callback: {}) [{} attachments]\n{}\n\n",
                                m.sender_id, cb, attach_count, m.text
                            ));
                        }

                        const INBOUND_BATCH_WARN_THRESHOLD: usize = 10;
                        if pending_inbound.len() > INBOUND_BATCH_WARN_THRESHOLD {
                            let mut sender_counts: std::collections::HashMap<&str, usize> =
                                std::collections::HashMap::new();
                            for m in &pending_inbound {
                                *sender_counts.entry(&m.sender_id).or_default() += 1;
                            }
                            let breakdown: String = sender_counts
                                .iter()
                                .map(|(s, c)| format!("{s}: {c}"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            warn!(
                                count = pending_inbound.len(),
                                breakdown = %breakdown,
                                "High inbound batch volume"
                            );
                            let warning_msg = format!(
                                "Budget warning: {} inbound requests batched this tick. Breakdown: {}",
                                pending_inbound.len(),
                                breakdown
                            );
                            let _ = router_hb.notify_all(&warning_msg).await;
                            task.push_str(&format!(
                                "\n**BUDGET WARNING**: {} inbound requests batched this tick.\n\
                                 Sender breakdown: {}\n\
                                 Investigate which deployed service is over-requesting. Use `channel_notify` \
                                 to alert the operator and propose corrective action before proceeding.\n",
                                pending_inbound.len(),
                                breakdown
                            ));
                        }
                    }
                    if due_refs.is_empty() && !unscoped_plugin_tasks.is_empty() {
                        task.push_str(plugin_only_notify_guidance);
                    }

                    info!(
                        entries = due_indices.len(),
                        plugin_items = unscoped_plugin_tasks.len(),
                        scoped_plugin_groups = scoped_plugin_tasks.len(),
                        "Dispatching global heartbeat run"
                    );

                    run_agent_for_sender(
                        task,
                        "heartbeat".to_string(),
                        "system".to_string(),
                        None,
                        None,
                        None, // session_hint
                        "heartbeat:system".to_string(),
                        Arc::clone(&sessions_hb),
                        Arc::clone(&channel_index_lock_hb),
                        Arc::clone(&session_mgr_hb),
                        current_agent.clone(),
                        container_hb.clone(),
                        preamble_hb.clone(),
                        Arc::clone(&router_hb),
                        vec![],
                        Some(Arc::clone(&cluster_registry_hb)),
                        Some(Arc::clone(&channel_registry_hb)),
                        Some(Arc::clone(&route_registry_hb)),
                        skill_roots_hb.clone(),
                        None,
                        None,
                        None,
                    )
                    .await;
                }

                for (
                    key,
                    (route_channel_id, route_conversation_id, route_sender_id, route_tasks),
                ) in scoped_plugin_tasks
                {
                    let mut task = String::from(
                        "Heartbeat check-in. Process the following plugin-triggered items:\n\n",
                    );
                    append_plugin_heartbeat_tasks(&mut task, &route_tasks);
                    task.push_str(plugin_only_notify_guidance);

                    let sender_for_route = route_sender_id
                        .clone()
                        .unwrap_or_else(|| "system".to_string());
                    let heartbeat_sender_key = format!(
                        "heartbeat:{}:{}:{}",
                        route_channel_id,
                        route_conversation_id.clone().unwrap_or_default(),
                        sender_for_route,
                    );

                    info!(
                        route = %key,
                        channel = %route_channel_id,
                        conversation = ?route_conversation_id,
                        sender = ?route_sender_id,
                        plugin_items = route_tasks.len(),
                        "Dispatching scoped heartbeat run"
                    );

                    run_agent_for_sender(
                        task,
                        route_channel_id,
                        sender_for_route,
                        route_conversation_id,
                        None,
                        None, // session_hint
                        heartbeat_sender_key,
                        Arc::clone(&sessions_hb),
                        Arc::clone(&channel_index_lock_hb),
                        Arc::clone(&session_mgr_hb),
                        current_agent.clone(),
                        container_hb.clone(),
                        preamble_hb.clone(),
                        Arc::clone(&router_hb),
                        vec![],
                        Some(Arc::clone(&cluster_registry_hb)),
                        Some(Arc::clone(&channel_registry_hb)),
                        Some(Arc::clone(&route_registry_hb)),
                        skill_roots_hb.clone(),
                        None,
                        None,
                        None,
                    )
                    .await;
                }

                // After processing, immediately re-check for inbound messages that
                // arrived while we were busy. Avoids waiting a full tick interval for
                // queued tasks dispatched by the parent during a long run.
                let leftover_inbound = {
                    let q = inbound_queue_hb.lock().await;
                    !q.is_empty()
                };
                let leftover_notifs = {
                    let q = notification_queue_hb.lock().await;
                    !q.is_empty()
                };
                if leftover_inbound || leftover_notifs {
                    debug!(
                        "Heartbeat re-check: {} inbound, {} notifications queued during run",
                        if leftover_inbound { "yes" } else { "no" },
                        if leftover_notifs { "yes" } else { "no" }
                    );
                    // Reset the ticker so the next iteration starts immediately.
                    ticker.reset();
                }
            }
        });
    }

    // ── SIGTERM / SIGINT handler ───────────────────────────────────────────
    // Without this, K8s pod termination (SIGTERM) or OOM-adjacent kills leave
    // no trace in logs, making crashes look "silent".
    let shutdown_signal = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to register SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => error!("Received SIGTERM — shutting down"),
            _ = sigint.recv() => error!("Received SIGINT — shutting down"),
        }
    };

    let inbound_router = that_channels::InboundRouter::new(inbound_rx);
    let inbound_loop = inbound_router
        .run_concurrent(move |msg| {
            let router = Arc::clone(&router);
            let container = container.clone();
            let session_mgr = Arc::clone(&session_mgr);
            let sessions = Arc::clone(&sessions);
            let model_prefs = Arc::clone(&model_prefs);
            let channel_index_lock = Arc::clone(&channel_index_lock);
            let plugin_runtime_lock = Arc::clone(&plugin_runtime_lock);
            let hot = Arc::clone(&hot);
            let sender_locks = Arc::clone(&sender_locks);
            let active_sender_runs = Arc::clone(&active_sender_runs);
            let sender_run_seq = Arc::clone(&sender_run_seq);
            let cluster_registry = Arc::clone(&cluster_registry);
            let channel_registry = Arc::clone(&channel_registry);
            let route_registry = Arc::clone(&route_registry);
            let notification_queue = Arc::clone(&notification_queue);
            let inbound_queue = Arc::clone(&inbound_queue);

            async move {
                if msg.deferred {
                    let mut q = inbound_queue.lock().await;
                    if q.len() < 100 {
                        q.push(msg);
                    } else {
                        warn!("inbound_queue full (100), dropping deferred message from {}", msg.sender_id);
                    }
                    return;
                }
                if msg.sender_id == that_channels::NOTIFY_SENDER_ID {
                    // Relay to channel immediately for user visibility.
                    router.notify_all(&msg.text).await;
                    // Update task registry if this is a task_update notification.
                    if let Some(meta) = &msg.metadata {
                        if let (Some(task_id), Some(task_state)) = (
                            meta.get("task_id").and_then(|v| v.as_str()),
                            meta.get("task_state").and_then(|v| v.as_str()),
                        ) {
                            let cluster_dir = dirs::home_dir()
                                .unwrap_or_default()
                                .join(".that-agent")
                                .join("cluster");
                            let task_reg = crate::agents::AgentTaskRegistry::new(
                                cluster_dir.join("agent_tasks.json"),
                            );
                            let Ok(state) = task_state.parse::<crate::agents::AgentTaskState>()
                            else {
                                warn!("Unknown task state: {task_state}");
                                return;
                            };
                            let full_msg = meta
                                .get("full_message")
                                .and_then(|v| v.as_str());
                            let _ = task_reg.update_state(task_id, state, full_msg);
                        }
                    }
                    // Queue for parent LLM context at next heartbeat turn.
                    let mut q = notification_queue.lock().await;
                    if q.len() < 500 {
                        q.push(msg.text);
                    } else {
                        warn!("notification_queue full (500), dropping notification");
                    }
                    return;
                }
                let sender_key = format!(
                    "{}:{}:{}",
                    msg.channel_id,
                    msg.conversation_id.clone().unwrap_or_default(),
                    msg.sender_id
                );
                let parsed_slash = parse_slash_command(&msg.text);
                if matches!(parsed_slash.as_ref(), Some((cmd, _)) if cmd == "stop") {
                    let outbound_target = that_channels::OutboundTarget {
                        recipient_id: msg
                            .conversation_id
                            .clone()
                            .or_else(|| Some(msg.sender_id.clone())),
                        sender_id: Some(msg.sender_id.clone()),
                        thread_id: msg.session_hint.clone(),
                        session_id: None,
                        reply_to_message_id: msg.message_id.clone(),
                        request_id: msg.session_hint.clone(),
                    };
                    let stopped = stop_active_sender_run(&active_sender_runs, &sender_key).await;
                    let text = if stopped {
                        "Stopped current run."
                    } else {
                        "No active run to stop."
                    };
                    router
                        .notify_channel(&msg.channel_id, text, Some(&outbound_target))
                        .await;
                    return;
                }
                // ── Mid-turn steering: if a run is active for this sender,
                // push the message as a hint instead of blocking on the lock.
                if parsed_slash.is_none() && hot.read().await.agent.steering {
                    let steering_arc = {
                        let runs = active_sender_runs.lock().await;
                        runs.get(&sender_key).map(|r| Arc::clone(&r.steering))
                    };
                    if let Some(q) = steering_arc {
                        q.lock().await.push(msg.text);
                        debug!(sender = %sender_key, "Enqueued steering hint for active run");
                        // Visual acknowledgment — fire-and-forget so we don't block on Telegram API.
                        if let Some(mid) = msg.message_id.clone() {
                            let r = Arc::clone(&router);
                            let ch = msg.channel_id.clone();
                            let chat = msg.conversation_id.clone().unwrap_or_default();
                            tokio::spawn(async move { r.react_to_message(&ch, &chat, &mid, "\u{1FAE1}").await });
                        }
                        return;
                    }
                }

                let sender_lock = {
                    let mut locks = sender_locks.lock().await;
                    locks
                        .entry(sender_key.clone())
                        .or_insert_with(SenderRunLock::default)
                        .clone()
                };
                let sender_guard = sender_lock.lock().await;
                (async {
                    let outbound_target = that_channels::OutboundTarget {
                        recipient_id: msg
                            .conversation_id
                            .clone()
                            .or_else(|| Some(msg.sender_id.clone())),
                        sender_id: Some(msg.sender_id.clone()),
                        thread_id: msg.session_hint.clone(),
                        session_id: None,
                        reply_to_message_id: msg.message_id.clone(),
                        request_id: msg.session_hint.clone(),
                    };

                    // Snapshot the current hot state for this message.
                    let (
                        preamble,
                        bot_commands,
                        found_skills,
                        plugin_commands,
                        plugin_activations,
                        agent,
                        skill_roots,
                    ) = {
                        let state = hot.read().await;
                        (
                            state.preamble.clone(),
                            state.bot_commands.clone(),
                            state.found_skills.clone(),
                            state.plugin_commands.clone(),
                            state.plugin_activations.clone(),
                            state.agent.clone(),
                            state.skill_roots.clone(),
                        )
                    };
                    let channel_prefs = {
                        let prefs = model_prefs.lock().await;
                        prefs.get(&sender_key).cloned().unwrap_or_default()
                    };
                    let effective_agent = apply_channel_preferences(&agent, &channel_prefs);
                    let effective_config_path = if channel_prefs.is_default() {
                        remove_effective_channel_config(&agent.name, &sender_key);
                        None
                    } else {
                        persist_effective_channel_config(
                            &effective_agent,
                            &sender_key,
                            container.is_some(),
                        )
                    };

                    if !plugin_activations.is_empty() {
                        let slash_command = parsed_slash.as_ref().map(|(cmd, _)| cmd.as_str());
                        let slash_args = parsed_slash.as_ref().map(|(_, args)| args.as_str());
                        let mut queued = 0usize;
                        let _guard = plugin_runtime_lock.lock().await;
                        for activation in &plugin_activations {
                            if !activation_matches_message(activation, &msg.text, slash_command) {
                                continue;
                            }
                            let task = render_activation_task(activation, &msg.text, slash_args);
                            match that_plugins::enqueue_activation_task(
                                &agent.name,
                                &activation.plugin_id,
                                &activation.name,
                                &activation.priority,
                                &task,
                                Some(that_plugins::PluginTaskRoute {
                                    channel_id: Some(msg.channel_id.clone()),
                                    conversation_id: msg.conversation_id.clone(),
                                    sender_id: Some(msg.sender_id.clone()),
                                }),
                            ) {
                                Ok(()) => queued += 1,
                                Err(err) => tracing::warn!(
                                    plugin = %activation.plugin_id,
                                    activation = %activation.name,
                                    error = %err,
                                    "Failed to queue plugin activation"
                                ),
                            }
                        }
                        if queued > 0 {
                            tracing::info!(queued, "Queued plugin activations");
                        }
                    }

                    // ── Slash-command dispatch ───────────────────────────────────
                    if let Some((cmd, args)) = parsed_slash {
                        info!(channel = %msg.channel_id, sender = %msg.sender_id, cmd = %cmd, "Slash command");
                        match cmd.as_str() {
                            "help" => {
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        &build_help_text(&bot_commands),
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            "models" => {
                                handle_models_command(
                                    &router,
                                    &session_mgr,
                                    &model_prefs,
                                    &sender_key,
                                    &msg.channel_id,
                                    &outbound_target,
                                    &agent,
                                    &channel_prefs,
                                    container.is_some(),
                                    &args,
                                )
                                .await;
                                return;
                            }
                            "clear" => {
                                // Create a fresh session so the old transcript is abandoned.
                                let new_sid = session_mgr
                                    .create_session()
                                    .unwrap_or_else(|_| "unknown".into());
                                {
                                    let mut map = sessions.lock().await;
                                    let sw = map
                                        .get(&sender_key)
                                        .map(|(_, _, sw)| Arc::clone(sw))
                                        .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
                                    map.insert(
                                        sender_key.clone(),
                                        (new_sid.clone(), Vec::new(), sw),
                                    );
                                }
                                // Persist the new mapping so restarts don't resurrect old history.
                                {
                                    let _guard = channel_index_lock.lock().await;
                                    session_mgr.save_channel_session(&sender_key, &new_sid);
                                }
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        "Conversation cleared.",
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            "compact" => {
                                let mut map = sessions.lock().await;
                                if let Some(entry) = map.get_mut(&sender_key) {
                                    if entry.1.is_empty() {
                                        drop(map);
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "Nothing to compact.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                    } else {
                                        let hist_clone = entry.1.clone();
                                        let sid = entry.0.clone();
                                        drop(map);
                                        // LLM-generated summary of the conversation.
                                        let summary = build_compact_summary(
                                            &effective_agent.provider,
                                            &effective_agent.model,
                                            &effective_agent.name,
                                            container.is_some(),
                                            &hist_clone,
                                        )
                                        .await;
                                        // Reset in-memory history to just the compaction anchor.
                                        let compacted_history = vec![
                                            Message::user(format!(
                                                "[Conversation context summary: {summary}]"
                                            )),
                                            Message::assistant(
                                                "Understood, I have the context from our previous conversation.".to_string(),
                                            ),
                                        ];
                                        {
                                            let mut map = sessions.lock().await;
                                            if let Some(entry) = map.get_mut(&sender_key) {
                                                entry.1 = compacted_history;
                                            }
                                        }
                                        // Write a Compaction marker to the transcript so
                                        // rebuild_history_recent respects it on restart.
                                        let _ = session_mgr.append(
                                            &sid,
                                            &TranscriptEntry {
                                                timestamp: Utc::now(),
                                                run_id: new_run_id(),
                                                event: TranscriptEvent::Compaction {
                                                    summary: summary.clone(),
                                                },
                                            },
                                        );
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "History compacted.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                    }
                                } else {
                                    drop(map);
                                    router
                                        .notify_channel(
                                            &msg.channel_id,
                                            "Nothing to compact.",
                                            Some(&outbound_target),
                                        )
                                        .await;
                                }
                                return;
                            }
                            "stop" => {
                                let stopped =
                                    stop_active_sender_run(&active_sender_runs, &sender_key).await;
                                let text = if stopped {
                                    "Stopped current run."
                                } else {
                                    "No active run to stop."
                                };
                                router
                                    .notify_channel(
                                        &msg.channel_id,
                                        text,
                                        Some(&outbound_target),
                                    )
                                    .await;
                                return;
                            }
                            skill_cmd => {
                                if let Some(plugin_cmd) =
                                    find_plugin_command(skill_cmd, &plugin_commands)
                                {
                                    let effective_task =
                                        render_plugin_command_task(plugin_cmd, &args);
                                    if effective_task.trim().is_empty() {
                                        router
                                            .notify_channel(
                                                &msg.channel_id,
                                                "This plugin command requires arguments.",
                                                Some(&outbound_target),
                                            )
                                            .await;
                                        return;
                                    }
                                    run_agent_for_sender_tracked(
                                        effective_task,
                                        msg.channel_id,
                                        msg.sender_id,
                                        msg.conversation_id,
                                        msg.message_id,
                                        msg.session_hint,
                                        sender_key.clone(),
                                        sessions,
                                        std::sync::Arc::clone(&channel_index_lock),
                                        session_mgr,
                                        effective_agent.clone(),
                                        container,
                                        preamble,
                                        router,
                                        Arc::clone(&active_sender_runs),
                                        Arc::clone(&sender_run_seq),
                                        vec![],
                                        Some(Arc::clone(&cluster_registry)),
                                        Some(Arc::clone(&channel_registry)),
                                        Some(Arc::clone(&route_registry)),
                                        skill_roots,
                                        None,
                                        effective_config_path.clone(),
                                    )
                                    .await;
                                    return;
                                } else if let Some(skill) =
                                    find_skill_by_command(skill_cmd, &found_skills)
                                {
                                    let effective_task = if args.is_empty() {
                                        skill.description.clone()
                                    } else {
                                        args
                                    };
                                    run_agent_for_sender_tracked(
                                        effective_task,
                                        msg.channel_id,
                                        msg.sender_id,
                                        msg.conversation_id,
                                        msg.message_id,
                                        msg.session_hint,
                                        sender_key.clone(),
                                        sessions,
                                        std::sync::Arc::clone(&channel_index_lock),
                                        session_mgr,
                                        effective_agent.clone(),
                                        container,
                                        preamble,
                                        router,
                                        Arc::clone(&active_sender_runs),
                                        Arc::clone(&sender_run_seq),
                                        vec![],
                                        Some(Arc::clone(&cluster_registry)),
                                        Some(Arc::clone(&channel_registry)),
                                        Some(Arc::clone(&route_registry)),
                                        skill_roots,
                                        None,
                                        effective_config_path.clone(),
                                    )
                                    .await;
                                    return;
                                } else {
                                    router
                                        .notify_channel(
                                            &msg.channel_id,
                                            &format!("Unknown command /{cmd} — try /help"),
                                            Some(&outbound_target),
                                        )
                                        .await;
                                    return;
                                }
                            }
                        }
                    }

                    // ── Regular message → agent run ──────────────────────────────
                    run_agent_for_sender_tracked(
                        msg.text,
                        msg.channel_id,
                        msg.sender_id,
                        msg.conversation_id,
                        msg.message_id,
                        msg.session_hint,
                        sender_key.clone(),
                        sessions,
                        std::sync::Arc::clone(&channel_index_lock),
                        session_mgr,
                        effective_agent,
                        container,
                        preamble,
                        router,
                        Arc::clone(&active_sender_runs),
                        Arc::clone(&sender_run_seq),
                        msg.attachments,
                        Some(Arc::clone(&cluster_registry)),
                        Some(Arc::clone(&channel_registry)),
                        Some(Arc::clone(&route_registry)),
                        skill_roots,
                        msg.callback_url,
                        effective_config_path,
                    )
                    .await;
                })
                .await;
                drop(sender_guard);
                evict_sender_lock_if_idle(&sender_locks, &sender_key, &sender_lock).await;
            }
        });

    tokio::select! {
        _ = inbound_loop => {
            error!("Inbound message loop exited unexpectedly — all channel senders dropped?");
        }
        _ = shutdown_signal => {
            // Logged inside the signal handler above.
        }
    }

    Ok(())
}

/// Execute a single channel turn in a cancellable task and track abort handles by sender.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_sender_tracked(
    task: String,
    channel_id: String,
    sender_id: String,
    conversation_id: Option<String>,
    message_id: Option<String>,
    session_hint: Option<String>,
    sender_key: String,
    sessions: ChannelSessions,
    channel_index_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    session_mgr: std::sync::Arc<SessionManager>,
    agent: AgentDef,
    container: Option<String>,
    preamble: String,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    active_runs: ActiveSenderRuns,
    run_seq: Arc<AtomicU64>,
    attachments: Vec<that_channels::InboundAttachment>,
    cluster_registry: Option<Arc<that_plugins::cluster::ClusterRegistry>>,
    channel_registry: Option<Arc<that_channels::registry::DynamicChannelRegistry>>,
    route_registry: Option<Arc<that_channels::DynamicRouteRegistry>>,
    skill_roots: Vec<std::path::PathBuf>,
    callback_url: Option<String>,
    effective_config_path: Option<String>,
) {
    let active_run_id = run_seq.fetch_add(1, Ordering::Relaxed);
    let sender_key_for_task = sender_key.clone();
    let sender_key_for_cleanup = sender_key.clone();
    let steering: SteeringQueue = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let steering_for_task = Arc::clone(&steering);

    let run_task = tokio::spawn(async move {
        run_agent_for_sender(
            task,
            channel_id,
            sender_id,
            conversation_id,
            message_id,
            session_hint,
            sender_key_for_task,
            sessions,
            channel_index_lock,
            session_mgr,
            agent,
            container,
            preamble,
            router,
            attachments,
            cluster_registry,
            channel_registry,
            route_registry,
            skill_roots,
            callback_url,
            effective_config_path,
            Some(steering_for_task),
        )
        .await;
    });
    let abort_handle = run_task.abort_handle();
    {
        let mut runs = active_runs.lock().await;
        runs.insert(
            sender_key.clone(),
            ActiveSenderRun {
                run_id: active_run_id,
                abort: abort_handle,
                steering,
            },
        );
    }

    let _ = run_task.await;

    let mut runs = active_runs.lock().await;
    let should_remove = runs
        .get(&sender_key_for_cleanup)
        .map(|run| run.run_id == active_run_id)
        .unwrap_or(false);
    if should_remove {
        runs.remove(&sender_key_for_cleanup);
    }
}

/// Execute a single agent turn for an inbound message and persist the result.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_sender(
    task: String,
    channel_id: String,
    sender_id: String,
    conversation_id: Option<String>,
    message_id: Option<String>,
    session_hint: Option<String>,
    sender_key: String,
    sessions: ChannelSessions,
    channel_index_lock: std::sync::Arc<tokio::sync::Mutex<()>>,
    session_mgr: std::sync::Arc<SessionManager>,
    agent: AgentDef,
    container: Option<String>,
    preamble: String,
    router: std::sync::Arc<that_channels::ChannelRouter>,
    attachments: Vec<that_channels::InboundAttachment>,
    cluster_registry: Option<Arc<that_plugins::cluster::ClusterRegistry>>,
    channel_registry: Option<Arc<that_channels::registry::DynamicChannelRegistry>>,
    route_registry: Option<Arc<that_channels::DynamicRouteRegistry>>,
    skill_roots: Vec<std::path::PathBuf>,
    callback_url: Option<String>,
    effective_config_path: Option<String>,
    steering: Option<SteeringQueue>,
) {
    struct TypingTaskGuard(Option<tokio::task::JoinHandle<()>>);
    impl TypingTaskGuard {
        fn take(&mut self) -> Option<tokio::task::JoinHandle<()>> {
            self.0.take()
        }
    }
    impl Drop for TypingTaskGuard {
        fn drop(&mut self) {
            if let Some(handle) = self.0.take() {
                handle.abort();
            }
        }
    }

    // Heartbeat-originated runs use sender_key prefixed with `heartbeat:` so
    // they stay silent by default and avoid user-facing typing indicators.
    let is_internal_source = sender_key.starts_with("heartbeat:");
    let base_target = that_channels::OutboundTarget {
        recipient_id: conversation_id.or_else(|| Some(sender_id.clone())),
        sender_id: Some(sender_id.clone()),
        thread_id: session_hint.clone(),
        session_id: None,
        reply_to_message_id: message_id.clone(),
        request_id: session_hint.clone(),
    };

    // Immediately acknowledge the message so the user knows the agent is working.
    // Skip for internal heartbeat sources.
    let mut typing_task = TypingTaskGuard(if !is_internal_source {
        // React to the user's message with 👀 — fire-and-forget so the agent
        // run starts immediately without waiting for the Telegram API round-trip.
        if let Some(mid) = message_id {
            let react_chat = base_target
                .recipient_id
                .as_deref()
                .unwrap_or(sender_id.as_str())
                .to_string();
            let r = Arc::clone(&router);
            let ch = channel_id.clone();
            tokio::spawn(async move { r.react_to_message(&ch, &react_chat, &mid, "👀").await });
        }
        // Send typing indicator immediately and refresh every 4s while the agent runs.
        // Telegram's "typing" action expires after ~5s, so 4s keeps it alive.
        let event = that_channels::ChannelEvent::TypingIndicator;
        let _ = router
            .send_to(&channel_id, &event, Some(&base_target))
            .await;
        let typing_router = std::sync::Arc::clone(&router);
        let typing_channel_id = channel_id.clone();
        let typing_target = base_target.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(4));
            interval.tick().await; // skip first tick (already sent above)
            loop {
                interval.tick().await;
                let event = that_channels::ChannelEvent::TypingIndicator;
                let _ = typing_router
                    .send_to(&typing_channel_id, &event, Some(&typing_target))
                    .await;
            }
        }))
    } else {
        None
    });
    // Look up or create a session for this sender.
    // On the first message after a restart, restore context from the persisted transcript.
    let (session_id, mut history, show_work) = {
        let mut map = sessions.lock().await;
        if let Some((sid, hist, sw)) = map.get(&sender_key) {
            (sid.clone(), hist.clone(), Arc::clone(sw))
        } else {
            // No in-memory state — check persistent index (handles restarts / crashes).
            let channel_index = session_mgr.load_channel_sessions();
            let (sid, hist) = if let Some(prior_sid) = channel_index.get(&sender_key) {
                // Mark interrupted runs before rebuilding history so the
                // Restart event is visible to rebuild_history_recent.
                if let Some(run_id) = session_mgr.mark_restart_if_interrupted(prior_sid) {
                    warn!(
                        session = %prior_sid,
                        interrupted_run = %run_id,
                        "Detected interrupted run after restart"
                    );
                }
                // Restore the last 10 turns (or from the last compaction) from disk.
                let hist = session_mgr
                    .read_transcript(prior_sid)
                    .map(|entries| rebuild_history_recent(&entries, 10))
                    .unwrap_or_default();
                if !hist.is_empty() {
                    info!(
                        session = %prior_sid,
                        sender = %sender_key,
                        turns = hist.len() / 2,
                        "Restored conversation history after restart"
                    );
                }
                (prior_sid.clone(), hist)
            } else {
                // Truly new sender — create a dedicated session.
                let sid = session_mgr
                    .create_session()
                    .unwrap_or_else(|_| "unknown".into());
                (sid, Vec::new())
            };
            let show_work = Arc::new(AtomicBool::new(true));
            map.insert(
                sender_key.clone(),
                (sid.clone(), hist.clone(), Arc::clone(&show_work)),
            );
            (sid, hist, show_work)
        }
    };
    // Persist sender → session mapping so the next restart can recover context.
    {
        let _index_guard = channel_index_lock.lock().await;
        session_mgr.save_channel_session(&sender_key, &session_id);
    }
    let mut route_target = base_target.clone();
    route_target.session_id = Some(session_id.clone());

    info!(
        session = %session_id,
        channel = %channel_id,
        sender = %sender_id,
        "Agent run for inbound message"
    );
    info!(channel = %channel_id, sender = %sender_id, ">>> {task}");

    // ── Pre-process attachments ───────────────────────────────────────────
    let mut enriched_task = task.clone();
    let mut images: Vec<(Vec<u8>, String)> = Vec::new();

    for att in &attachments {
        match att {
            that_channels::InboundAttachment::Audio {
                data, mime_type, ..
            } => {
                if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
                    match crate::transcription::transcribe(&api_key, data, mime_type).await {
                        Ok(transcript) => {
                            enriched_task =
                                format!("[Voice transcript]: {transcript}\n\n{enriched_task}");
                        }
                        Err(e) => {
                            warn!("transcription failed: {e:#}");
                            router
                                .notify_channel(
                                    &channel_id,
                                    &format!("Voice transcription failed: {e:#}"),
                                    Some(&route_target),
                                )
                                .await;
                            return;
                        }
                    }
                } else {
                    router
                        .notify_channel(
                            &channel_id,
                            "Voice messages require OPENAI_API_KEY to be set.",
                            Some(&route_target),
                        )
                        .await;
                    return;
                }
            }
            that_channels::InboundAttachment::Image { data, mime_type } => {
                images.push((data.clone(), mime_type.clone()));
            }
            that_channels::InboundAttachment::Document {
                data,
                mime_type,
                filename,
            } => {
                let fname = filename.as_deref().unwrap_or("attachment.bin").to_string();
                let dir = std::env::temp_dir().join("that-agent-docs");
                let _ = std::fs::create_dir_all(&dir);
                let dest = dir.join(&fname);
                match std::fs::write(&dest, data) {
                    Ok(()) => {
                        enriched_task = format!(
                            "[Document received: {} ({}) saved to {}]\n\n{enriched_task}",
                            fname,
                            mime_type,
                            dest.display()
                        );
                    }
                    Err(e) => {
                        warn!("Failed to save document attachment: {e:#}");
                    }
                }
            }
        }
    }

    if !images.is_empty() {
        enriched_task = format!(
            "{enriched_task}\n\n[System: image attached — after responding, use mem_add to store a brief description]"
        );
    }

    let run_id = new_run_id();
    let _ = session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::RunStart {
                task: enriched_task.clone(),
            },
        },
    );
    let _ = session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::UserMessage {
                content: enriched_task.clone(),
            },
        },
    );
    let task_for_model = append_system_reminder(
        &enriched_task,
        &session_id,
        container.is_some(),
        &agent.name,
    );

    let route_channel_id = if channel_id == "heartbeat" {
        None
    } else {
        Some(channel_id.clone())
    };
    let route_target_for_run = if channel_id == "heartbeat" {
        None
    } else {
        Some(route_target.clone())
    };
    let steering_for_run = steering.filter(|_| agent.steering);
    let run_result = execute_agent_run_channel(
        &agent,
        container,
        &preamble,
        &task_for_model,
        history.clone(),
        std::sync::Arc::clone(&router),
        route_channel_id,
        route_target_for_run,
        is_internal_source,
        show_work,
        Some(&session_id),
        Some(&run_id),
        images,
        cluster_registry,
        channel_registry,
        route_registry,
        skill_roots,
        steering_for_run,
        effective_config_path,
    )
    .await;

    // Stop the typing indicator refresh — the response is on its way.
    if let Some(handle) = typing_task.take() {
        handle.abort();
    }

    match run_result {
        Ok((text, tool_events)) => {
            // If the agent called mem_compact, reset in-memory history to a compact anchor
            // so images and old turns are evicted from context — identical to /compact command.
            let compact_summary = tool_events.iter().find_map(|ev| {
                if let that_channels::ToolLogEvent::Call { name, args } = ev {
                    if name == "mem_compact" {
                        return serde_json::from_str::<serde_json::Value>(args)
                            .ok()
                            .and_then(|v| v.get("summary")?.as_str().map(String::from));
                    }
                }
                None
            });
            if let Some(ref summary) = compact_summary {
                history = vec![
                    Message::user(format!("[Conversation context summary: {summary}]")),
                    Message::assistant(
                        "Understood, I have the context from our previous conversation."
                            .to_string(),
                    ),
                ];
                let _ = session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::Compaction {
                            summary: summary.clone(),
                        },
                    },
                );
            }

            // Log tool calls and results before the final assistant message so the
            // transcript reads in execution order: input → tools → output.
            for ev in tool_events {
                match ev {
                    that_channels::ToolLogEvent::Call { name, args } => {
                        debug!(tool = %name, "  tool_call: {args}");
                        let arguments =
                            serde_json::from_str(&args).unwrap_or_else(|_| serde_json::json!({}));
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: run_id.clone(),
                                event: TranscriptEvent::ToolCall {
                                    tool: name,
                                    arguments,
                                },
                            },
                        );
                    }
                    that_channels::ToolLogEvent::Result {
                        name,
                        result,
                        is_error,
                    } => {
                        debug!(tool = %name, is_error, "  tool_result: {result}");
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: run_id.clone(),
                                event: TranscriptEvent::ToolResult {
                                    tool: name,
                                    result,
                                    is_error,
                                },
                            },
                        );
                    }
                }
            }

            info!(channel = %channel_id, "<<< {text}");
            history.push(Message::user(&task_for_model));
            history.push(Message::assistant(&text));
            if let Some(url) = callback_url {
                let t = text.clone();
                let agent = sender_id.clone();
                tokio::spawn(async move {
                    let _ = reqwest::Client::new()
                        .post(&url)
                        .json(&serde_json::json!({
                            "text": t,
                            "state": "completed",
                            "message": t,
                            "agent": agent,
                        }))
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await;
                });
            }
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::AssistantMessage { content: text },
                },
            );
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id,
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Success,
                        error: None,
                    },
                },
            );
            let mut map = sessions.lock().await;
            if let Some(entry) = map.get_mut(&sender_key) {
                entry.1 = history;
            }
        }
        Err(e) => {
            error!(session = %session_id, "Agent run failed: {e:#}");
            let _ = session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id,
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Error,
                        error: Some(format!("{e:#}")),
                    },
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{channel_config_slug, effective_channel_config_visible_path};

    #[test]
    fn channel_config_slug_normalizes_sender_keys() {
        assert_eq!(channel_config_slug("telegram:123/abc"), "telegram_123_abc");
    }

    #[test]
    fn sandbox_effective_config_path_uses_state_dir() {
        let path = effective_channel_config_visible_path("demo", "telegram:123", true).unwrap();
        assert_eq!(
            path,
            "/home/agent/.that-agent/state/channel-configs/demo/telegram_123.toml"
        );
    }
}
