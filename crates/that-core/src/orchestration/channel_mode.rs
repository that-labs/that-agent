use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use chrono::{Local, Utc};
use tracing::{debug, error, info, warn};

use crate::agent_loop::Message;
use crate::config::{AgentDef, WorkspaceConfig};
use crate::heartbeat;
use crate::session::{
    new_run_id, rebuild_history_recent, RunStatus, SessionManager, TranscriptEntry, TranscriptEvent,
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

type ChannelSessions =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, (String, Vec<Message>)>>>;
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
pub(super) async fn stop_active_sender_run(
    active_runs: &ActiveSenderRuns,
    sender_key: &str,
) -> bool {
    let active = active_runs.lock().await.remove(sender_key);
    if let Some(run) = active {
        run.abort.abort();
        true
    } else {
        false
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

    // Per-sender state: key = "channel_id:sender_id" → (session_id, history).
    let sessions: ChannelSessions = Arc::new(Mutex::new(HashMap::new()));
    let channel_index_lock = Arc::new(Mutex::new(()));
    let plugin_runtime_lock = Arc::new(Mutex::new(()));

    eprintln!(
        "[that] Listening on channels: {} (primary: {})\n[that] Agent: {} — Ctrl+C to stop.",
        router.channel_ids().await,
        router.primary_id().await,
        agent.name,
    );

    // Validate each channel's config (token check, connectivity) before opening listeners.
    router.initialize().await;

    router.start_listeners().await?;

    // Signal K8s readiness — channels are initialized and listening.
    let _ = std::fs::File::create("/tmp/that-agent-ready");

    // If unbootstrapped, greet on all channels so the user knows to start the ceremony.
    if ws_files.needs_bootstrap() {
        let name = &agent.name;
        router.notify_all(&format!(
            "Hey! I'm {name} — I just woke up for the first time. \
             Send me a message to start our bootstrap ceremony and figure out who I am."
        )).await;
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
        let interval_secs = agent.heartbeat_interval.unwrap_or(10).max(1);

        let plugin_dir_hb = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".that-agent")
            .join("plugins");
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(tokio::time::Duration::from_secs(interval_secs));
            let mut last_reconcile = std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(120))
                .unwrap_or_else(std::time::Instant::now);
            loop {
                ticker.tick().await;

                // Touch liveness file so K8s knows the event loop is alive.
                let _ = tokio::fs::File::create("/tmp/that-agent-alive").await;

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
                        ) && heartbeat::is_entry_due(e)
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

                if due_indices.is_empty()
                    && unscoped_plugin_tasks.is_empty()
                    && scoped_plugin_tasks.is_empty()
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
                if !due_refs.is_empty() || !unscoped_plugin_tasks.is_empty() {
                    let mut task = if due_refs.is_empty() {
                        String::from(
                            "Heartbeat check-in. Process the following plugin-triggered items:\n\n",
                        )
                    } else {
                        heartbeat::format_heartbeat_task(&due_refs)
                    };
                    append_plugin_heartbeat_tasks(&mut task, &unscoped_plugin_tasks);
                    if due_refs.is_empty() {
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
                    )
                    .await;
                }
            }
        });
    }

    let sender_locks = SenderRunLocks::default();
    let active_sender_runs: ActiveSenderRuns = Arc::default();
    let sender_run_seq = Arc::new(AtomicU64::new(1));
    let inbound_router = that_channels::InboundRouter::new(inbound_rx);
    inbound_router
        .run_concurrent(move |msg| {
            let router = Arc::clone(&router);
            let container = container.clone();
            let session_mgr = Arc::clone(&session_mgr);
            let sessions = Arc::clone(&sessions);
            let channel_index_lock = Arc::clone(&channel_index_lock);
            let plugin_runtime_lock = Arc::clone(&plugin_runtime_lock);
            let hot = Arc::clone(&hot);
            let sender_locks = Arc::clone(&sender_locks);
            let active_sender_runs = Arc::clone(&active_sender_runs);
            let sender_run_seq = Arc::clone(&sender_run_seq);
            let cluster_registry = Arc::clone(&cluster_registry);
            let channel_registry = Arc::clone(&channel_registry);
            let route_registry = Arc::clone(&route_registry);

            async move {
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
                        reply_to_message_id: msg.message_id,
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
                        reply_to_message_id: msg.message_id,
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
                            "clear" => {
                                // Create a fresh session so the old transcript is abandoned.
                                let new_sid = session_mgr
                                    .create_session()
                                    .unwrap_or_else(|_| "unknown".into());
                                {
                                    let mut map = sessions.lock().await;
                                    map.insert(sender_key.clone(), (new_sid.clone(), Vec::new()));
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
                                            &agent.provider,
                                            &agent.model,
                                            &agent.name,
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
                                        agent,
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
                                        agent,
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
                        agent,
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
                    )
                    .await;
                })
                .await;
                drop(sender_guard);
                evict_sender_lock_if_idle(&sender_locks, &sender_key, &sender_lock).await;
            }
        })
        .await;

    Ok(())
}

/// Execute a single channel turn in a cancellable task and track abort handles by sender.
#[allow(clippy::too_many_arguments)]
async fn run_agent_for_sender_tracked(
    task: String,
    channel_id: String,
    sender_id: String,
    conversation_id: Option<String>,
    message_id: Option<i64>,
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
) {
    let active_run_id = run_seq.fetch_add(1, Ordering::Relaxed);
    let sender_key_for_task = sender_key.clone();
    let sender_key_for_cleanup = sender_key.clone();

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
    message_id: Option<i64>,
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
        reply_to_message_id: message_id,
    };

    // Immediately acknowledge the message so the user knows the agent is working.
    // Skip for internal heartbeat sources.
    let mut typing_task = TypingTaskGuard(if !is_internal_source {
        // React to the user's message with 👀 so they know the agent saw it.
        if let Some(mid) = message_id {
            let react_chat = base_target
                .recipient_id
                .as_deref()
                .unwrap_or(sender_id.as_str());
            router
                .react_to_message(&channel_id, react_chat, mid, "👀")
                .await;
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
    let (session_id, mut history) = {
        let mut map = sessions.lock().await;
        if let Some(existing) = map.get(&sender_key) {
            existing.clone()
        } else {
            // No in-memory state — check persistent index (handles restarts / crashes).
            let channel_index = session_mgr.load_channel_sessions();
            let (sid, hist) = if let Some(prior_sid) = channel_index.get(&sender_key) {
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
            map.insert(sender_key.clone(), (sid.clone(), hist.clone()));
            (sid, hist)
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
                        Err(e) => warn!("transcription failed: {e:#}"),
                    }
                }
            }
            that_channels::InboundAttachment::Image { data, mime_type } => {
                images.push((data.clone(), mime_type.clone()));
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
        Some(&session_id),
        Some(&run_id),
        images,
        cluster_registry,
        channel_registry,
        route_registry,
        skill_roots,
    )
    .await;

    // Stop the typing indicator refresh — the response is on its way.
    if let Some(handle) = typing_task.take() {
        handle.abort();
    }

    match run_result {
        Ok((text, tool_events)) => {
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
