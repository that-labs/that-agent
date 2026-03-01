use std::io::{self, BufRead, Write};

use anyhow::Result;
use chrono::Utc;
use tracing::{error, info};

use crate::agent_loop::Message;
use crate::config::{AgentDef, WorkspaceConfig};
use crate::session::{new_run_id, RunStatus, SessionManager, TranscriptEntry, TranscriptEvent};

use self::config::append_system_reminder;

pub mod channel_mode;
pub mod config;
pub mod discovery;
pub mod execution;
pub mod generation;
pub mod handlers;
pub mod hooks;
mod preamble;
pub mod setup;
pub mod support;
#[cfg(feature = "tui")]
pub mod tui_session;

// ── Re-exports ──────────────────────────────────────────────────────────────
pub use channel_mode::run_listen;
pub use config::{
    load_agent_config, CACHE_HIT_WARN_THRESHOLD, EMPTY_CHANNEL_RESPONSE_FALLBACK,
    MAX_EMPTY_CHANNEL_RESPONSE_RETRIES, MAX_NETWORK_RETRIES, RETRY_BASE_DELAY_MS,
};
pub use discovery::{
    activation_matches_message, build_bot_commands_list, build_help_text,
    discover_plugin_activations, discover_plugin_commands, discover_skills,
    discover_skills_with_registry, file_mtime_hash, find_plugin_command, find_skill_by_command,
    format_plugin_preamble, format_plugin_preamble_full, format_plugin_preamble_with_registry,
    parse_slash_command, render_activation_task, render_plugin_command_task, resolved_skill_roots,
    resolved_skill_roots_with_registry, skill_roots_for_agent, skill_to_command,
    skills_fingerprint, skills_fingerprint_with_registry,
};
pub use execution::{
    api_key_for_provider, execute_agent_run_channel, execute_agent_run_eval,
    execute_agent_run_streaming, is_retryable_error,
};
pub use generation::{generate_soul_md, init_workspace};
pub use handlers::{handle_agent_command, handle_session_command, handle_skill_command};
pub use hooks::{AgentHook, EvalHook};
pub use preamble::build_preamble;
pub use setup::{install_that_tools_skills_local, prepare_container, resolve_agent_workspace};
#[cfg(feature = "tui")]
pub use support::build_palette_commands;
pub use support::{build_compact_summary, compact_session, load_workspace_files, show_status};
#[cfg(feature = "tui")]
pub use tui_session::{execute_agent_run_tui, run_chat_tui};

/// Execute a single task in the agent loop.
#[tracing::instrument(name = "task", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
))]
pub async fn run_task(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    task: &str,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.get_or_create_session()?,
    };

    let run_id = new_run_id();

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    if sandbox {
        eprintln!("[SANDBOX] Running inside Docker container — bash tool available");
    }

    info!(session = %session_id, run = %run_id, mode = if sandbox { "sandbox" } else { "local" }, workspace = %agent_workspace.display(), "Starting run");

    // Log run start
    session_mgr.append(
        &session_id,
        &TranscriptEntry {
            timestamp: Utc::now(),
            run_id: run_id.clone(),
            event: TranscriptEvent::RunStart {
                task: task.to_string(),
            },
        },
    )?;

    // Ensure container is ready (sandbox) or skip (local)
    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    let found_skills = discover_skills_with_registry(agent, &plugin_registry);
    info!(count = found_skills.len(), "Discovered skills");

    let ws = load_workspace_files(agent, sandbox);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();

    let skill_roots = resolved_skill_roots_with_registry(agent, &plugin_registry);

    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
        Some(&plugin_registry),
        None,
    );
    let task_for_model = append_system_reminder(task, &session_id, sandbox, &agent.name);

    let response = execute_agent_run_streaming(
        agent,
        container,
        &preamble,
        &task_for_model,
        debug,
        None,
        skill_roots,
    )
    .await;

    // Record the result
    match response {
        Ok(text) => {
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::UserMessage {
                        content: task.to_string(),
                    },
                },
            )?;
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::AssistantMessage {
                        content: text.clone(),
                    },
                },
            )?;
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Success,
                        error: None,
                    },
                },
            )?;
            // Text was already streamed live; just add a trailing newline
            println!();
        }
        Err(e) => {
            let err_str = format!("{e:#}");
            session_mgr.append(
                &session_id,
                &TranscriptEntry {
                    timestamp: Utc::now(),
                    run_id: run_id.clone(),
                    event: TranscriptEvent::RunEnd {
                        status: RunStatus::Error,
                        error: Some(err_str.clone()),
                    },
                },
            )?;
            if sandbox {
                error!(error = %err_str, "[SANDBOX] Run failed");
            } else {
                error!(error = %err_str, "Run failed");
            }
            return Err(e);
        }
    }

    Ok(())
}

/// Interactive chat loop.
#[tracing::instrument(name = "chat_session", skip_all, fields(
    agent    = %agent.name,
    provider = %agent.provider,
    model    = %agent.model,
))]
pub async fn run_chat(
    ws: &WorkspaceConfig,
    agent: &AgentDef,
    session_id: Option<&str>,
    sandbox: bool,
    debug: bool,
) -> Result<()> {
    let state_dir = ws.resolve_state_dir()?;
    let session_mgr = SessionManager::new(&state_dir)?;

    let session_id = match session_id {
        Some(id) => id.to_string(),
        None => session_mgr.get_or_create_session()?,
    };

    // Resolve per-agent workspace
    let agent_workspace = resolve_agent_workspace(ws, agent)?;

    let mode = if sandbox { "sandbox" } else { "local" };
    if sandbox {
        println!("[SANDBOX] that-agent interactive session: {session_id}");
        println!("[SANDBOX] Running inside Docker container — bash tool available");
    } else {
        println!("that-agent interactive session: {session_id} (mode: {mode})");
    }
    println!("Type your message and press Enter. Type 'exit' or Ctrl+D to quit.\n");

    let container = prepare_container(agent, &agent_workspace, sandbox).await?;

    let plugin_registry = that_plugins::PluginRegistry::load(&agent.name);
    let found_skills = discover_skills_with_registry(agent, &plugin_registry);
    info!(count = found_skills.len(), "Discovered skills");

    let ws = load_workspace_files(agent, sandbox);
    let session_summaries = session_mgr.session_summaries(5).unwrap_or_default();

    let skill_roots = resolved_skill_roots_with_registry(agent, &plugin_registry);

    let preamble = build_preamble(
        &agent_workspace,
        agent,
        sandbox,
        &found_skills,
        &ws,
        0,
        &session_id,
        &session_summaries,
        Some(&plugin_registry),
        None,
    );

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let prompt_str = if sandbox { "[SANDBOX]> " } else { "> " };

    let mut history: Vec<Message> = Vec::new();

    loop {
        print!("{prompt_str}");
        stdout.flush()?;

        let mut input = String::new();
        let bytes = stdin.lock().read_line(&mut input)?;
        if bytes == 0 {
            println!();
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        let run_id = new_run_id();

        session_mgr.append(
            &session_id,
            &TranscriptEntry {
                timestamp: Utc::now(),
                run_id: run_id.clone(),
                event: TranscriptEvent::UserMessage {
                    content: input.to_string(),
                },
            },
        )?;

        let task_for_model = append_system_reminder(input, &session_id, sandbox, &agent.name);
        match execute_agent_run_streaming(
            agent,
            container.clone(),
            &preamble,
            &task_for_model,
            debug,
            Some(history.clone()),
            skill_roots.clone(),
        )
        .await
        {
            Ok(text) => {
                history.push(Message::user(&task_for_model));
                history.push(Message::assistant(&text));

                session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::AssistantMessage {
                            content: text.clone(),
                        },
                    },
                )?;
                // Text was already streamed live; just add trailing newlines
                println!("\n");
            }
            Err(e) => {
                let err_str = format!("{e:#}");
                session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::RunEnd {
                            status: RunStatus::Error,
                            error: Some(err_str.clone()),
                        },
                    },
                )?;
                if sandbox {
                    eprintln!("[SANDBOX] Error: {err_str}\n");
                } else {
                    eprintln!("Error: {err_str}\n");
                }
            }
        }
    }

    Ok(())
}

/// Send a one-shot query to a remote agent's HTTP gateway and print the response.
///
/// This is the client side of the HTTP gateway channel adapter, enabling
/// agent-to-agent communication or CLI-driven remote queries.
pub async fn run_remote_query(
    url: &str,
    task: String,
    session: Option<&str>,
    token: Option<&str>,
    timeout_secs: u64,
) -> Result<String> {
    let client = reqwest::Client::new();
    let endpoint = format!("{}/v1/chat", url.trim_end_matches('/'));

    let mut body = serde_json::json!({ "message": task });
    if let Some(sid) = session {
        body["conversation_id"] = serde_json::Value::String(sid.to_string());
    }

    let mut request = client.post(&endpoint).json(&body);
    if let Some(tok) = token {
        request = request.bearer_auth(tok);
    }

    let response =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), request.send())
            .await
            .map_err(|_| anyhow::anyhow!("Remote query timed out after {timeout_secs}s"))?
            .map_err(|e| anyhow::anyhow!("Remote query failed: {e}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Remote agent returned {status}: {body}");
    }

    let result: serde_json::Value = response
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to parse remote response: {e}"))?;

    let text = result["text"].as_str().unwrap_or_default().to_string();

    println!("{text}");
    Ok(text)
}
