use anyhow::Result;
use chrono::Utc;
use crossterm::event::{Event, EventStream};
use futures::FutureExt;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::agent_loop::{self, LoopConfig, Message, ToolCall, ToolContext};
use crate::config::{AgentDef, WorkspaceConfig};
use crate::sandbox::SandboxClient;
use crate::session::{
    new_run_id, rebuild_history, SessionManager, TranscriptEntry, TranscriptEvent,
};
use crate::skills;
use crate::tools::all_tool_defs;
use crate::tui;
use crate::workspace;

use super::config::*;
use super::discovery::*;
use super::execution::{api_key_for_provider, is_retryable_error};
use super::generation::generate_soul_md;
use super::preamble::build_preamble;
use super::setup::{prepare_container, resolve_agent_workspace};
use super::support::*;

/// Interactive chat loop using a Ratatui TUI.
#[tracing::instrument(name = "chat_tui_session", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
    session.id = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    trace_id = tracing::field::Empty,
    span_id = tracing::field::Empty,
))]
pub async fn run_chat_tui(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let mut session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.create_session()?,
    };
    tracing::Span::current().record("session.id", session_id.as_str());
    if let Some(tid) = crate::observability::current_trace_id() {
        tracing::Span::current().record("trace_id", tid.as_str());
    }
    if let Some(sid) = crate::observability::current_span_id() {
        tracing::Span::current().record("span_id", sid.as_str());
    }

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    // Ensure container is ready (sandbox) or skip (local)
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let mut found_skills = discover_skills(agent, sandbox);
    let mut plugin_commands = discover_plugin_commands(agent);
    let mut ws = load_workspace_files(agent, sandbox);
    let needs_onboarding = ws.needs_bootstrap();
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();
    let mut preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
    );

    // Setup TUI terminal
    tui::install_panic_hook();
    let mut terminal = tui::setup_terminal()?;

    // Create channel for agent -> TUI communication
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();

    let mut app = tui::ChatApp::new(agent_rx, debug, sandbox, Some(&state_dir));

    // Build command palette entries (built-in + plugins + skills)
    app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
    app.set_max_turns(agent.max_turns);

    // Show loaded skills in TUI
    if found_skills.is_empty() {
        app.push_system_message("No skills loaded.");
    } else {
        let names: Vec<&str> = found_skills.iter().map(|s| s.name.as_str()).collect();
        app.push_system_message(&format!("Skills: {}", names.join(", ")));
    }

    // Trigger onboarding if Soul.md and Identity.md are both absent (brand-new agent)
    if needs_onboarding {
        app.start_onboarding();
    }
    let mut event_reader = EventStream::new();

    let mut agent = agent.clone();
    let mut history: Vec<Message> = Vec::new();
    let mut agent_handle: Option<tokio::task::JoinHandle<Result<String>>> = None;
    let mut tick_interval = tokio::time::interval(std::time::Duration::from_millis(150));
    let mut stats = tui::UsageStats::new();
    // Per-turn tool call/result pairs collected for history reconstruction.
    // (call_id, tool_name, args_json) and (call_id, result_text)
    let mut turn_tool_calls: Vec<(String, String, String)> = Vec::new();
    let mut turn_tool_results: Vec<(String, String)> = Vec::new();
    // True when a graceful shutdown was requested and compaction is in-flight.
    let mut shutting_down = false;

    loop {
        // Render
        terminal.draw(|f| app.render(f))?;

        tokio::select! {
            // --- Periodic tick for spinner animation ---
            _ = tick_interval.tick() => {
                app.tick();
            }
            // --- Crossterm terminal events ---
            maybe_event = tui::next_crossterm_event(&mut event_reader) => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        match app.handle_key(key) {
                            tui::KeyAction::Submit(text) => {
                                if app.is_onboarding() {
                                    // --- Onboarding: generate Soul.md + Identity.md from description ---
                                    app.push_user_message(&text);
                                    app.set_streaming();

                                    let tx = agent_tx.clone();
                                    let provider = agent.provider.clone();
                                    let model = agent.model.clone();
                                    let desc = text.clone();
                                    let parent_span = tracing::Span::current();

                                    agent_handle = Some(tokio::spawn(
                                        async move {
                                            match generate_soul_md(&provider, &model, &desc).await {
                                                Ok((soul_md, identity_md)) => {
                                                    let _ = tx.send(tui::TuiEvent::OnboardingDone {
                                                        soul_md,
                                                        identity_md,
                                                    });
                                                }
                                                Err(e) => {
                                                    let _ = tx.send(tui::TuiEvent::OnboardingError(
                                                        e.to_string(),
                                                    ));
                                                }
                                            }
                                            Ok(String::new())
                                        }
                                        .instrument(parent_span),
                                    ));
                                } else if text.starts_with('/') {
                                    // --- Slash command dispatch ---
                                    let parts: Vec<&str> = text.splitn(2, ' ').collect();
                                    let cmd = parts[0];
                                    let arg = parts.get(1).map(|s| s.trim()).filter(|s| !s.is_empty());

                                    match cmd {
                                        "/model" => match arg {
                                            Some(name) => {
                                                agent.model = name.to_string();
                                                app.push_system_message(
                                                    &format!("Model changed to: {name}"),
                                                );
                                            }
                                            None => {
                                                let mut items = vec![tui::ModalItem::Header("Anthropic".into())];
                                                for (prov, model) in tui::MODEL_OPTIONS.iter().filter(|(p, _)| *p == "anthropic") {
                                                    items.push(tui::ModalItem::Option {
                                                        label: model.to_string(),
                                                        detail: prov.to_string(),
                                                        active: *model == agent.model,
                                                    });
                                                }
                                                items.push(tui::ModalItem::Separator);
                                                items.push(tui::ModalItem::Header("OpenAI".into()));
                                                for (prov, model) in tui::MODEL_OPTIONS.iter().filter(|(p, _)| *p == "openai") {
                                                    items.push(tui::ModalItem::Option {
                                                        label: model.to_string(),
                                                        detail: prov.to_string(),
                                                        active: *model == agent.model,
                                                    });
                                                }
                                                let modal = tui::Modal::new("Select Model".into(), tui::ModalKind::ModelSelect, items, true);
                                                app.open_modal(modal);
                                            }
                                        },
                                        "/resume" => {
                                            match arg {
                                                Some(prefix) => {
                                                    // Direct resume by prefix
                                                    match session_mgr.find_session_by_prefix(prefix) {
                                                        Ok(Some(resume_id)) => {
                                                            match session_mgr.read_transcript(&resume_id) {
                                                                Ok(entries) => {
                                                                    let new_history = rebuild_history(&entries);
                                                                    app.clear_messages();
                                                                    app.push_system_message(&format!(
                                                                        "Resumed session: {resume_id}"
                                                                    ));
                                                                    for entry in &entries {
                                                                        match &entry.event {
                                                                            TranscriptEvent::UserMessage { content } => {
                                                                                app.push_user_message(content);
                                                                            }
                                                                            TranscriptEvent::AssistantMessage { content } => {
                                                                                app.push_agent_message(content);
                                                                            }
                                                                            _ => {}
                                                                        }
                                                                    }
                                                                    history = new_history;
                                                                    session_id = resume_id;
                                                                }
                                                                Err(e) => {
                                                                    app.push_system_message(&format!(
                                                                        "Failed to resume: {e}"
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                        Ok(None) => {
                                                            app.push_system_message(&format!(
                                                                "No session matching prefix: {prefix}"
                                                            ));
                                                        }
                                                        Err(e) => {
                                                            app.push_system_message(&format!(
                                                                "Error searching sessions: {e}"
                                                            ));
                                                        }
                                                    }
                                                }
                                                None => {
                                                    // Show modal with session list
                                                    match session_mgr.session_summaries(20) {
                                                        Ok(summaries) if !summaries.is_empty() => {
                                                            let mut items = Vec::new();
                                                            for s in &summaries {
                                                                items.push(tui::ModalItem::Option {
                                                                    label: format!("{} ({})", s.timestamp, s.entry_count),
                                                                    detail: s.id.clone(),
                                                                    active: s.id == session_id,
                                                                });
                                                                items.push(tui::ModalItem::Text(
                                                                    format!("  {}", s.preview),
                                                                ));
                                                            }
                                                            let modal = tui::Modal::new(
                                                                "Resume Session".into(),
                                                                tui::ModalKind::SessionResume,
                                                                items,
                                                                true,
                                                            );
                                                            app.open_modal(modal);
                                                        }
                                                        Ok(_) => {
                                                            app.push_system_message("No sessions to resume.");
                                                        }
                                                        Err(e) => {
                                                            app.push_system_message(&format!(
                                                                "Error listing sessions: {e}"
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        "/usage" => {
                                            let cost = stats.estimated_cost(
                                                &agent.provider,
                                                &agent.model,
                                            );
                                            let cache_hit_rate = cache_hit_rate_percent(
                                                stats.input_tokens,
                                                stats.cached_input_tokens,
                                                stats.cache_write_tokens,
                                            );
                                            let mut items = vec![
                                                tui::ModalItem::Header("── Current Session ──".into()),
                                                tui::ModalItem::Text(format!("Input tokens:    {}", stats.input_tokens)),
                                                tui::ModalItem::Text(format!("Output tokens:   {}", stats.output_tokens)),
                                                tui::ModalItem::Text(format!("Cached tokens:   {}", stats.cached_input_tokens)),
                                                tui::ModalItem::Text(format!("Cache hit rate:  {:.2}%", cache_hit_rate)),
                                                tui::ModalItem::Text(format!("Tool calls:      {}", stats.tool_calls)),
                                                tui::ModalItem::Text(format!("Turns (ok/err):  {}/{}", stats.turns_success, stats.turns_error)),
                                                tui::ModalItem::Text(format!("Est. cost:       ${:.4}", cost)),
                                                tui::ModalItem::Text(format!("Model:           {} ({})", agent.model, agent.provider)),
                                            ];

                                            // Historical aggregation
                                            let now = Utc::now();
                                            let periods = [
                                                ("── Last 24 Hours ──", chrono::Duration::hours(24)),
                                                ("── Last 7 Days ──", chrono::Duration::days(7)),
                                                ("── Last 30 Days ──", chrono::Duration::days(30)),
                                            ];
                                            for (label, dur) in &periods {
                                                if let Ok(agg) = session_mgr.aggregate_usage(now - *dur) {
                                                    items.push(tui::ModalItem::Separator);
                                                    items.push(tui::ModalItem::Header(label.to_string()));
                                                    items.push(tui::ModalItem::Text(format!(
                                                        "Sessions: {} | Cost: ${:.2}",
                                                        agg.session_count, agg.estimated_cost
                                                    )));
                                                }
                                            }

                                            let modal = tui::Modal::new("Session Usage".into(), tui::ModalKind::Info, items, false);
                                            app.open_modal(modal);
                                        }
                                        "/skills" => {
                                            if found_skills.is_empty() {
                                                app.push_system_message("No skills found.");
                                            } else {
                                                let items: Vec<tui::ModalItem> = found_skills
                                                    .iter()
                                                    .map(|s| tui::ModalItem::Option {
                                                        label: s.name.clone(),
                                                        detail: s.description.clone(),
                                                        active: false,
                                                    })
                                                    .collect();
                                                let modal = tui::Modal::new(
                                                    "Skills".into(),
                                                    tui::ModalKind::SkillsList,
                                                    items,
                                                    true,
                                                );
                                                app.open_modal(modal);
                                            }
                                        }
                                        "/help" => {
                                            let mut items = vec![
                                                tui::ModalItem::Text("/model          — select or change model".into()),
                                                tui::ModalItem::Text("/model <name>   — change model directly".into()),
                                                tui::ModalItem::Text("/resume         — pick a session to resume".into()),
                                                tui::ModalItem::Text("/resume <prefix>— resume by session ID prefix".into()),
                                                tui::ModalItem::Text("/usage          — show usage stats (current + historical)".into()),
                                                tui::ModalItem::Text("/skills         — browse and manage skills".into()),
                                                tui::ModalItem::Text("/<skill>        — show skill content or send with context".into()),
                                            ];
                                            if !plugin_commands.is_empty() {
                                                items.push(tui::ModalItem::Separator);
                                                items.push(tui::ModalItem::Text("Plugin commands:".into()));
                                                for plugin_cmd in &plugin_commands {
                                                    items.push(tui::ModalItem::Text(format!(
                                                        "/{} — {}",
                                                        plugin_cmd.command, plugin_cmd.description
                                                    )));
                                                }
                                            }
                                            items.extend([
                                                tui::ModalItem::Text("/compact        — compact and save session to memory".into()),
                                                tui::ModalItem::Text("/stop           — stop the active run".into()),
                                                tui::ModalItem::Text("/help           — show this help".into()),
                                            ]);
                                            let modal = tui::Modal::new("Help".into(), tui::ModalKind::Info, items, false);
                                            app.open_modal(modal);
                                        }
                                        "/compact" => {
                                            if history.is_empty() {
                                                app.push_system_message("Nothing to compact — start a conversation first.");
                                            } else {
                                                app.push_system_message("Compacting session…");
                                                let hist_for_compact = history.clone();
                                                let container_for_compact = container.clone();
                                                let session_id_for_compact = session_id.clone();
                                                let provider_for_compact = agent.provider.clone();
                                                let model_for_compact = agent.model.clone();
                                                let name_for_compact = agent.name.clone();
                                                let tx_compact = agent_tx.clone();
                                                let parent_span = tracing::Span::current();
                                                tokio::spawn(
                                                    async move {
                                                        // LLM-generated summary of the conversation.
                                                        let summary = build_compact_summary(
                                                            &provider_for_compact,
                                                            &model_for_compact,
                                                            &name_for_compact,
                                                            sandbox,
                                                            &hist_for_compact,
                                                        )
                                                        .await;
                                                        match compact_session(
                                                            container_for_compact.as_deref(),
                                                            &session_id_for_compact,
                                                            &summary,
                                                        )
                                                        .await
                                                        {
                                                            Ok(msg) => {
                                                                let _ = tx_compact.send(tui::TuiEvent::CompactDone { message: msg, summary });
                                                            }
                                                            Err(e) => {
                                                                let _ = tx_compact.send(tui::TuiEvent::CompactError(e.to_string()));
                                                            }
                                                        }
                                                    }
                                                    .instrument(parent_span),
                                                );
                                            }
                                        }
                                        "/stop" => {
                                            if let Some(handle) = agent_handle.take() {
                                                handle.abort();
                                                app.interrupt_run("Stopped current run.");
                                                turn_tool_calls.clear();
                                                turn_tool_results.clear();
                                            } else {
                                                app.push_system_message("No active run to stop.");
                                            }
                                        }
                                        _ => {
                                            let command_name = cmd.trim_start_matches('/');
                                            if let Some(plugin_cmd) =
                                                find_plugin_command(command_name, &plugin_commands)
                                            {
                                                let effective_task = render_plugin_command_task(
                                                    plugin_cmd,
                                                    arg.unwrap_or(""),
                                                );
                                                if effective_task.trim().is_empty() {
                                                    app.push_system_message(
                                                        "This plugin command requires arguments.",
                                                    );
                                                    continue;
                                                }

                                                app.record_input(&text);
                                                app.push_user_message(&text);
                                                app.set_streaming();

                                                let run_id = new_run_id();
                                                let _ = session_mgr.append(
                                                    &session_id,
                                                    &TranscriptEntry {
                                                        timestamp: Utc::now(),
                                                        run_id: run_id.clone(),
                                                        event: TranscriptEvent::UserMessage {
                                                            content: text.clone(),
                                                        },
                                                    },
                                                );

                                                let tx = agent_tx.clone();
                                                let pre = preamble.clone();
                                                let agent_clone = agent.clone();
                                                let hist = history.clone();
                                                let task_for_model = append_system_reminder(
                                                    &effective_task,
                                                    &session_id,
                                                    sandbox,
                                                    &agent.name,
                                                );
                                                let cont = container.clone();
                                                let session_id_for_trace = session_id.clone();
                                                let run_id_for_trace = run_id.clone();

                                                history.push(Message::user(&task_for_model));

                                                let tx_panic = tx.clone();
                                                let parent_span = tracing::Span::current();
                                                agent_handle = Some(tokio::spawn(
                                                    async move {
                                                        match std::panic::AssertUnwindSafe(
                                                            execute_agent_run_tui(
                                                                &agent_clone,
                                                                cont,
                                                                &pre,
                                                                &task_for_model,
                                                                hist,
                                                                tx,
                                                                Some(&session_id_for_trace),
                                                                Some(&run_id_for_trace),
                                                            ),
                                                        )
                                                        .catch_unwind()
                                                        .await
                                                        {
                                                            Ok(result) => result,
                                                            Err(payload) => {
                                                                let msg = payload
                                                                    .downcast_ref::<String>()
                                                                    .cloned()
                                                                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                                    .unwrap_or_else(|| "unknown panic".to_string());
                                                                let _ = tx_panic.send(tui::TuiEvent::Error(
                                                                    format!("Agent crashed: {msg}"),
                                                                ));
                                                                Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                            }
                                                        }
                                                    }
                                                    .instrument(parent_span),
                                                ));
                                            } else {
                                            // Check if it's a skill command (/<skill-name>)
                                            let skill_name = &cmd[1..]; // strip leading /
                                            if let Some(skill) = found_skills.iter().find(|s| s.name == skill_name) {
                                                let content = read_skill_content(skill);

                                                match (content, arg) {
                                                    (Some(content), Some(user_text)) => {
                                                        // Send message with skill context
                                                        let msg = format!(
                                                            "[Skill: {}]\n{}\n\n---\n\n{}",
                                                            skill.name, content, user_text
                                                        );
                                                        app.record_input(&text);
                                                        app.push_user_message(&text);
                                                        app.set_streaming();

                                                        let run_id = new_run_id();
                                                        let _ = session_mgr.append(
                                                            &session_id,
                                                            &TranscriptEntry {
                                                                timestamp: Utc::now(),
                                                                run_id: run_id.clone(),
                                                                event: TranscriptEvent::UserMessage {
                                                                    content: msg.clone(),
                                                                },
                                                            },
                                                        );

                                                        let tx = agent_tx.clone();
                                                        let pre = preamble.clone();
                                                        let agent_clone = agent.clone();
                                                        let hist = history.clone();
                                                        let task_for_model = append_system_reminder(&msg, &session_id, sandbox, &agent.name);
                                                        let cont = container.clone();
                                                        let session_id_for_trace = session_id.clone();
                                                        let run_id_for_trace = run_id.clone();

                                                        history.push(Message::user(&task_for_model));

                                                        let tx_panic = tx.clone();
                                                        let parent_span = tracing::Span::current();
                                                        agent_handle = Some(tokio::spawn(
                                                            async move {
                                                                match std::panic::AssertUnwindSafe(
                                                                    execute_agent_run_tui(
                                                                        &agent_clone,
                                                                        cont,
                                                                        &pre,
                                                                        &task_for_model,
                                                                        hist,
                                                                        tx,
                                                                        Some(&session_id_for_trace),
                                                                        Some(&run_id_for_trace),
                                                                    ),
                                                                )
                                                                .catch_unwind()
                                                                .await
                                                                {
                                                                    Ok(result) => result,
                                                                    Err(payload) => {
                                                                        let msg = payload
                                                                            .downcast_ref::<String>()
                                                                            .cloned()
                                                                            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                                            .unwrap_or_else(|| "unknown panic".to_string());
                                                                        let _ = tx_panic.send(tui::TuiEvent::Error(
                                                                            format!("Agent crashed: {msg}"),
                                                                        ));
                                                                        Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                                    }
                                                                }
                                                            }
                                                            .instrument(parent_span),
                                                        ));
                                                    }
                                                    (Some(content), None) => {
                                                        // No arg — show skill content in modal
                                                        let items: Vec<tui::ModalItem> = content
                                                            .lines()
                                                            .map(|l| tui::ModalItem::Text(l.to_string()))
                                                            .collect();
                                                        let modal = tui::Modal::new(
                                                            format!("Skill: {}", skill.name),
                                                            tui::ModalKind::SkillView,
                                                            items,
                                                            false,
                                                        );
                                                        app.open_modal(modal);
                                                    }
                                                    (None, _) => {
                                                        app.push_system_message(&format!(
                                                            "Failed to read skill: {}", skill.name
                                                        ));
                                                    }
                                                }
                                            } else {
                                                app.push_system_message(&format!(
                                                    "Unknown command: {cmd}. Type /help for available commands.",
                                                ));
                                            }
                                            }
                                        }
                                    }
                                } else {
                                    // --- Normal message: spawn agent ---
                                    app.record_input(&text);
                                    app.push_user_message(&text);
                                    app.set_streaming();

                                    // Record user message
                                    let run_id = new_run_id();
                                    let _ = session_mgr.append(
                                        &session_id,
                                        &TranscriptEntry {
                                            timestamp: Utc::now(),
                                            run_id: run_id.clone(),
                                            event: TranscriptEvent::UserMessage {
                                                content: text.clone(),
                                            },
                                        },
                                    );

                                    // Spawn agent task
                                    let tx = agent_tx.clone();
                                    let pre = preamble.clone();
                                    let agent_clone = agent.clone();
                                    let hist = history.clone();
                                    let task_text = append_system_reminder(&text, &session_id, sandbox, &agent.name);
                                    let cont = container.clone();
                                    let session_id_for_trace = session_id.clone();
                                    let run_id_for_trace = run_id.clone();

                                    history.push(Message::user(&task_text));

                                    let tx_panic = tx.clone();
                                    let parent_span = tracing::Span::current();
                                    agent_handle = Some(tokio::spawn(
                                        async move {
                                            match std::panic::AssertUnwindSafe(
                                                execute_agent_run_tui(
                                                    &agent_clone,
                                                    cont,
                                                    &pre,
                                                    &task_text,
                                                    hist,
                                                    tx,
                                                    Some(&session_id_for_trace),
                                                    Some(&run_id_for_trace),
                                                ),
                                            )
                                            .catch_unwind()
                                            .await
                                            {
                                                Ok(result) => result,
                                                Err(payload) => {
                                                    let msg = payload
                                                        .downcast_ref::<String>()
                                                        .cloned()
                                                        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                                        .unwrap_or_else(|| "unknown panic".to_string());
                                                    let _ = tx_panic.send(tui::TuiEvent::Error(
                                                        format!("Agent crashed: {msg}"),
                                                    ));
                                                    Err(anyhow::anyhow!("Agent task panicked: {msg}"))
                                                }
                                            }
                                        }
                                        .instrument(parent_span),
                                    ));
                                }
                            }
                            tui::KeyAction::ModalSelect { kind, label, detail } => {
                                match kind {
                                    tui::ModalKind::ModelSelect => {
                                        agent.model = label.clone();
                                        agent.provider = detail.clone();
                                        app.push_system_message(&format!("Model changed to: {label} ({detail})"));
                                    }
                                    tui::ModalKind::SessionResume => {
                                        // detail contains the session ID
                                        let resume_id = detail.clone();
                                        match session_mgr.read_transcript(&resume_id) {
                                            Ok(entries) => {
                                                let new_history = rebuild_history(&entries);
                                                app.clear_messages();
                                                app.push_system_message(&format!(
                                                    "Resumed session: {resume_id}"
                                                ));
                                                // Replay user/assistant messages into chat
                                                for entry in &entries {
                                                    match &entry.event {
                                                        TranscriptEvent::UserMessage { content } => {
                                                            app.push_user_message(content);
                                                        }
                                                        TranscriptEvent::AssistantMessage { content } => {
                                                            app.push_agent_message(content);
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                history = new_history;
                                                session_id = resume_id;
                                            }
                                            Err(e) => {
                                                app.push_system_message(&format!(
                                                    "Failed to resume session: {e}"
                                                ));
                                            }
                                        }
                                    }
                                    tui::ModalKind::SkillsList => {
                                        // label = skill name, detail = description
                                        let skill_name = label;
                                        let content = found_skills
                                            .iter()
                                            .find(|s| s.name == skill_name)
                                            .and_then(read_skill_content);

                                        match content {
                                            Some(content) => {
                                                let items: Vec<tui::ModalItem> = content
                                                    .lines()
                                                    .map(|l| tui::ModalItem::Text(l.to_string()))
                                                    .collect();
                                                let modal = tui::Modal::new(
                                                    format!("Skill: {}", skill_name),
                                                    tui::ModalKind::SkillView,
                                                    items,
                                                    false,
                                                );
                                                app.open_modal(modal);
                                            }
                                            None => {
                                                app.push_system_message(&format!(
                                                    "Failed to read skill: {}", skill_name
                                                ));
                                            }
                                        }
                                    }
                                    tui::ModalKind::SkillView | tui::ModalKind::Info => {}
                                }
                            }
                            tui::KeyAction::ModalDelete { kind, detail } => {
                                if kind == tui::ModalKind::SkillView {
                                    // Delete the skill
                                    if sandbox {
                                        app.push_system_message("Cannot delete skills in sandbox mode.");
                                    } else if let Some(dir) = skills::skills_dir_local(&agent.name) {
                                        match skills::delete_skill_local(&dir, &detail) {
                                            Ok(()) => {
                                                app.push_system_message(&format!(
                                                    "Deleted skill: {}", detail
                                                ));
                                                // Re-discover skills and update palette
                                                found_skills = discover_skills(&agent, sandbox);
                                                app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
                                                // Re-open skills list so the user sees the updated state
                                                if found_skills.is_empty() {
                                                    app.push_system_message("No skills remaining.");
                                                } else {
                                                    let items: Vec<tui::ModalItem> = found_skills
                                                        .iter()
                                                        .map(|s| tui::ModalItem::Option {
                                                            label: s.name.clone(),
                                                            detail: s.description.clone(),
                                                            active: false,
                                                        })
                                                        .collect();
                                                    let modal = tui::Modal::new(
                                                        "Skills".into(),
                                                        tui::ModalKind::SkillsList,
                                                        items,
                                                        true,
                                                    );
                                                    app.open_modal(modal);
                                                }
                                            }
                                            Err(e) => {
                                                app.push_system_message(&format!(
                                                    "Failed to delete skill: {}", e
                                                ));
                                            }
                                        }
                                    } else {
                                        app.push_system_message("Cannot determine skills directory.");
                                    }
                                }
                            }
                            tui::KeyAction::SubmitHumanAsk(response) => {
                                app.send_human_ask_response(response);
                            }
                            tui::KeyAction::InterruptRun => {
                                if let Some(handle) = agent_handle.take() {
                                    handle.abort();
                                    app.interrupt_run("Interrupted current run.");
                                    turn_tool_calls.clear();
                                    turn_tool_results.clear();
                                }
                            }
                            tui::KeyAction::Quit => {
                                if shutting_down || history.is_empty() {
                                    // Already compacting (Esc to skip) or nothing to compact.
                                    break;
                                }
                                // Graceful shutdown: compact the session before exiting.
                                shutting_down = true;
                                app.start_compaction_shutdown();
                                let hist_for_compact = history.clone();
                                let container_for_compact = container.clone();
                                let session_id_for_compact = session_id.clone();
                                let provider_for_compact = agent.provider.clone();
                                let model_for_compact = agent.model.clone();
                                let name_for_compact = agent.name.clone();
                                let tx_compact = agent_tx.clone();
                                let parent_span = tracing::Span::current();
                                tokio::spawn(
                                    async move {
                                        let summary = build_compact_summary(
                                            &provider_for_compact,
                                            &model_for_compact,
                                            &name_for_compact,
                                            sandbox,
                                            &hist_for_compact,
                                        )
                                        .await;
                                        match compact_session(
                                            container_for_compact.as_deref(),
                                            &session_id_for_compact,
                                            &summary,
                                        )
                                        .await
                                        {
                                            Ok(msg) => {
                                                let _ = tx_compact.send(tui::TuiEvent::CompactDone { message: msg, summary });
                                            }
                                            Err(e) => {
                                                let _ = tx_compact
                                                    .send(tui::TuiEvent::CompactError(e.to_string()));
                                            }
                                        }
                                    }
                                    .instrument(parent_span),
                                );
                            }
                            tui::KeyAction::None => {}
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        app.handle_paste(&text);
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        app.handle_mouse(mouse);
                    }
                    Some(Err(_)) => {
                        // Transient crossterm read error — don't kill the TUI
                        continue;
                    }
                    None => {
                        // EventStream ended (stdin closed)
                        break;
                    }
                    _ => {}
                }
            }

            // --- Agent events ---
            maybe_agent_event = app.recv_agent_event() => {
                if let Some(event) = maybe_agent_event {
                    let is_done = matches!(&event, tui::TuiEvent::Done { .. });
                    let is_error = matches!(&event, tui::TuiEvent::Error(_));
                    let is_tool_call = matches!(&event, tui::TuiEvent::ToolCall { .. });

                    // Capture onboarding result before event is consumed
                    let onboarding_inner = match &event {
                        tui::TuiEvent::OnboardingDone { soul_md, identity_md } => {
                            Some((soul_md.clone(), identity_md.clone()))
                        }
                        _ => None,
                    };
                    let is_onboarding_terminal = matches!(
                        &event,
                        tui::TuiEvent::OnboardingDone { .. } | tui::TuiEvent::OnboardingError(_)
                    );

                    // Capture usage data before passing event to app
                    let done_data = match &event {
                        tui::TuiEvent::Done { text, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens } => {
                            Some((text.clone(), *input_tokens, *output_tokens, *cached_input_tokens, *cache_write_tokens))
                        }
                        _ => None,
                    };

                    // Accumulate tool call stats and collect tool data for history
                    if is_tool_call {
                        stats.add_tool_call();
                    }
                    match &event {
                        tui::TuiEvent::ToolCall { call_id, name, args } => {
                            turn_tool_calls.push((call_id.clone(), name.clone(), args.clone()));
                        }
                        tui::TuiEvent::ToolResult { call_id, result, .. } => {
                            turn_tool_results.push((call_id.clone(), result.clone()));
                        }
                        _ => {}
                    }

                    // Check for compaction completion before consuming event
                    let is_compact_terminal = matches!(
                        &event,
                        tui::TuiEvent::CompactDone { .. } | tui::TuiEvent::CompactError(_)
                    );
                    // Extract summary from CompactDone to write transcript marker + reset history.
                    let compact_summary = match &event {
                        tui::TuiEvent::CompactDone { summary, .. } => Some(summary.clone()),
                        _ => None,
                    };

                    app.handle_agent_event(event);

                    // On successful compaction, write transcript marker and reset history.
                    if let Some(summary) = compact_summary {
                        let _ = session_mgr.append(
                            &session_id,
                            &TranscriptEntry {
                                timestamp: Utc::now(),
                                run_id: new_run_id(),
                                event: TranscriptEvent::Compaction {
                                    summary: summary.clone(),
                                },
                            },
                        );
                        history = vec![
                            Message::user(format!(
                                "[Conversation context summary: {summary}]"
                            )),
                            Message::assistant(
                                "Understood, I have the context from our previous conversation.".to_string(),
                            ),
                        ];
                    }

                    // If compaction finished, exit (graceful shutdown) or stay (manual /compact)
                    if is_compact_terminal && shutting_down {
                        break;
                    }

                    // Handle onboarding completion: save Soul.md + Identity.md and rebuild preamble
                    if is_onboarding_terminal {
                        agent_handle = None;
                        if let Some((soul_md, identity_md)) = onboarding_inner {
                            if sandbox {
                                let container_name = SandboxClient::container_name(&agent);
                                if let Err(e) = workspace::save_soul_sandbox(&container_name, &agent.name, &soul_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Soul.md to container: {e}"
                                    ));
                                }
                                if let Err(e) = workspace::save_identity_sandbox(&container_name, &agent.name, &identity_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Identity.md to container: {e}"
                                    ));
                                }
                            } else {
                                if let Err(e) = workspace::save_soul_local(&agent.name, &soul_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Soul.md: {e}"
                                    ));
                                }
                                if let Err(e) = workspace::save_identity_local(&agent.name, &identity_md) {
                                    app.push_system_message(&format!(
                                        "Warning: could not save Identity.md: {e}"
                                    ));
                                }
                            }
                            ws = load_workspace_files(&agent, sandbox);
                            preamble = build_preamble(
                                &agent_workspace,
                                &agent,
                                sandbox,
                                &found_skills,
                                &ws,
                                history.len(),
                                &session_id,
                                &session_mgr.session_summaries(5).unwrap_or_default(),
                            );
                        }
                    }

                    if is_done {
                        // Re-discover skills and plugins after each turn — the agent may have changed them.
                        let prev_skill_count = found_skills.len();
                        let prev_plugin_count = plugin_commands.len();
                        found_skills = discover_skills(&agent, sandbox);
                        plugin_commands = discover_plugin_commands(&agent);
                        app.set_available_commands(build_palette_commands(&found_skills, &plugin_commands));
                        if found_skills.len() != prev_skill_count {
                            let names: Vec<&str> = found_skills.iter().map(|s| s.name.as_str()).collect();
                            app.push_system_message(&format!(
                                "Skills updated: {} loaded ({})",
                                found_skills.len(),
                                names.join(", ")
                            ));
                        }
                        if plugin_commands.len() != prev_plugin_count {
                            let names: Vec<&str> =
                                plugin_commands.iter().map(|c| c.command.as_str()).collect();
                            app.push_system_message(&format!(
                                "Plugin commands updated: {} available ({})",
                                plugin_commands.len(),
                                names.join(", ")
                            ));
                        }

                        // Accumulate usage stats and write usage event to transcript
                        if let Some((_, input, output, cached, cache_write)) = &done_data {
                            stats.add_usage(*input, *output, *cached, *cache_write);
                            stats.record_success();
                            let _ = session_mgr.append(
                                &session_id,
                                &TranscriptEntry {
                                    timestamp: Utc::now(),
                                    run_id: new_run_id(),
                                    event: TranscriptEvent::Usage {
                                        input_tokens: *input,
                                        output_tokens: *output,
                                        cached_input_tokens: *cached,
                                        tool_calls: stats.tool_calls,
                                        model: agent.model.clone(),
                                        provider: agent.provider.clone(),
                                    },
                                },
                            );
                        }

                        // Insert tool call/result pairs collected during this turn.
                        {
                            let result_map: std::collections::HashMap<String, String> =
                                turn_tool_results.drain(..).collect();
                            let collected: Vec<(String, String, String)> =
                                std::mem::take(&mut turn_tool_calls);
                            if !collected.is_empty() {
                                // Group all tool calls in a single assistant message.
                                history.push(Message::Assistant {
                                    content: String::new(),
                                    tool_calls: collected.iter().map(|(call_id, name, args_json)| {
                                        ToolCall {
                                            call_id: call_id.clone(),
                                            name: name.clone(),
                                            args_json: args_json.clone(),
                                        }
                                    }).collect(),
                                });
                                // Then emit each tool result.
                                for (call_id, name, _args) in &collected {
                                    if let Some(result) = result_map.get(call_id) {
                                        history.push(Message::Tool {
                                            call_id: call_id.clone(),
                                            name: name.clone(),
                                            content: result.clone(),
                                        });
                                    }
                                }
                            }
                        }

                        if let Some(handle) = agent_handle.take() {
                            // Get the result from the spawned task
                            if let Ok(Ok(text)) = handle.await {
                                history.push(Message::assistant(&text));

                                let run_id = new_run_id();
                                let _ = session_mgr.append(
                                    &session_id,
                                    &TranscriptEntry {
                                        timestamp: Utc::now(),
                                        run_id,
                                        event: TranscriptEvent::AssistantMessage {
                                            content: text,
                                        },
                                    },
                                );
                            }
                        } else if let Some((text, _, _, _, _)) = done_data {
                            // Fallback: use the text from the Done event
                            if !text.is_empty() {
                                history.push(Message::assistant(&text));
                            }
                        }
                    } else if is_error {
                        stats.record_error();
                        agent_handle = None;
                        turn_tool_calls.clear();
                        turn_tool_results.clear();
                    }
                }
            }
        }
    }

    // Abort any in-flight agent task so it doesn't outlive the TUI.
    if let Some(handle) = agent_handle.take() {
        handle.abort();
    }

    tui::restore_terminal(&mut terminal)?;

    // Keep sandbox container by default so any in-container services remain
    // available after chat exits. Set THAT_SANDBOX_REMOVE_ON_EXIT=1 to restore
    // the old cleanup behavior.
    let remove_on_exit = std::env::var("THAT_SANDBOX_REMOVE_ON_EXIT")
        .ok()
        .map(|v| {
            let normalized = v.trim().to_ascii_lowercase();
            matches!(normalized.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false);
    if sandbox && remove_on_exit {
        SandboxClient::remove(&agent);
    }

    Ok(())
}

/// Build and execute a single agent run, sending events to the TUI via channel.
/// Retries automatically on transient network errors with exponential backoff,
/// notifying the TUI before each retry so it can clear partial streaming state.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "agent_run", skip_all, fields(
    gen_ai.provider      = %agent.provider,
    gen_ai.provider.name = %agent.provider,
    gen_ai.request.model = %agent.model,
    gen_ai.prompt        = tracing::field::Empty,
    gen_ai.completion    = tracing::field::Empty,
    openinference.span.kind = "CHAIN",
    otel.status_code = tracing::field::Empty,
    otel.status_description = tracing::field::Empty,
    session.id = tracing::field::Empty,
    run.id = tracing::field::Empty,
    agent.name = %agent.name,
    input.value = tracing::field::Empty,
    input.mime_type = "text/plain",
    output.value = tracing::field::Empty,
    output.mime_type = "text/plain",
))]
pub async fn execute_agent_run_tui(
    agent: &AgentDef,
    container: Option<String>,
    preamble: &str,
    task: &str,
    history: Vec<Message>,
    tui_tx: mpsc::UnboundedSender<tui::TuiEvent>,
    session_id_for_trace: Option<&str>,
    run_id_for_trace: Option<&str>,
) -> Result<String> {
    if let Some(sid) = session_id_for_trace {
        tracing::Span::current().record("session.id", sid);
    }
    if let Some(rid) = run_id_for_trace {
        tracing::Span::current().record("run.id", rid);
    }
    let preview = task_preview(task, 200);
    tracing::Span::current().record("input.value", preview.as_str());
    tracing::Span::current().record("gen_ai.prompt", preview.as_str());
    let task_for_model = append_memory_bootstrap_reminder(task, history.len());
    let mut attempt = 0u32;
    loop {
        if attempt > 0 {
            let delay_ms = RETRY_BASE_DELAY_MS << (attempt - 1).min(4);
            let _ = tui_tx.send(tui::TuiEvent::Retrying {
                attempt,
                max_attempts: MAX_NETWORK_RETRIES,
                delay_secs: delay_ms / 1_000,
            });
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
        }

        let api_key = match api_key_for_provider(&agent.provider) {
            Ok(k) => k,
            Err(e) => {
                let _ = tui_tx.send(tui::TuiEvent::Error(format!("{e:#}")));
                return Err(e);
            }
        };
        let skill_roots = resolved_skill_roots(agent);
        let tools_config = load_agent_config(&container, agent);
        let hook = tui::TuiHook::new(tui_tx.clone());
        let config = LoopConfig {
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            api_key,
            system: preamble.to_string(),
            max_tokens: agent.max_tokens as u32,
            max_turns: agent.max_turns as u32,
            tools: all_tool_defs(&container),
            history: history.clone(),
            prompt_caching: matches!(agent.provider.as_str(), "anthropic" | "openrouter"),
            openai_websocket: openai_websocket_enabled(),
            debug: false,
            tool_ctx: ToolContext {
                config: tools_config,
                container: container.clone(),
                skill_roots,
                cluster_registry: None,
                channel_registry: None,
                route_registry: None,
                router: None,
                state_dir: dirs::home_dir()
                    .map(|h| h.join(".that-agent").join("agents").join(&agent.name)),
            },
            images: vec![],
        };
        let result = agent_loop::run(&config, &task_for_model, &hook).await;

        match result {
            Ok((text, usage)) => {
                log_prompt_cache_usage(
                    &agent.provider,
                    &agent.model,
                    usage.input_tokens as u64,
                    usage.cache_read_tokens as u64,
                    usage.cache_write_tokens as u64,
                );
                tracing::Span::current().record("gen_ai.completion", text.as_str());
                tracing::Span::current().record("output.value", text.as_str());
                tracing::Span::current().record("otel.status_code", "ok");
                tracing::Span::current().record("otel.status_description", "agent run completed");
                // Ensure the current run is exported promptly for live trace UIs.
                crate::observability::flush_tracing();
                let _ = tui_tx.send(tui::TuiEvent::Done {
                    text: text.clone(),
                    input_tokens: usage.input_tokens as u64,
                    output_tokens: usage.output_tokens as u64,
                    cached_input_tokens: usage.cache_read_tokens as u64,
                    cache_write_tokens: usage.cache_write_tokens as u64,
                });
                return Ok(text);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < MAX_NETWORK_RETRIES {
                    attempt += 1;
                    continue;
                }
                tracing::Span::current().record("otel.status_code", "error");
                tracing::Span::current()
                    .record("otel.status_description", format!("{e:#}").as_str());
                crate::observability::flush_tracing();
                let _ = tui_tx.send(tui::TuiEvent::Error(format!("{e:#}")));
                return Err(e);
            }
        }
    }
}
