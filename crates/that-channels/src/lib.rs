//! that-channels — Generic communication channel abstraction for the that-agent system.
//!
//! Provides:
//! - [`Channel`] trait — implemented by TUI and Telegram adapters
//! - [`ChannelRouter`] — fan-out broadcast and primary-channel human-ask routing
//! - [`ToolLogEvent`] — log event type for session transcript recording
//! - [`ChannelNotifyTool`] — built-in agent tool for mid-task human notifications
//! - [`ChannelConfig`] — TOML/env-var configuration for channel setup
//! - [`InboundRouter`] — routes inbound messages from external channels to agent sessions
//!
//! ## Circular Dependency Note
//!
//! `that-channels` does NOT depend on `that-core`. The TUI adapter lives in
//! `that-core::tui` (as `TuiChannel`) to avoid a circular dependency.

pub mod adapters;
pub mod channel;
pub mod config;
pub mod factory;
pub mod hook;
pub mod inbound;
pub mod message;
pub mod registry;
pub mod router;
pub mod tool;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskJournalEvent {
    Snapshot {
        tasks: serde_json::Value,
    },
    Created {
        task: serde_json::Value,
    },
    StateUpdated {
        id: String,
        state: String,
        from: Option<String>,
        message: Option<String>,
        timestamp: String,
    },
    MessageAppended {
        id: String,
        from: String,
        text: String,
        timestamp: String,
    },
    ScratchpadAppended {
        id: String,
        from: String,
        note: String,
        kind: String,
        section: String,
        timestamp: String,
    },
    ParticipantAdded {
        id: String,
        participant: String,
        timestamp: String,
    },
}

struct PathLockGuard {
    lock_path: std::path::PathBuf,
}

impl Drop for PathLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

fn path_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    std::path::PathBuf::from(lock)
}

fn acquire_path_lock(path: &std::path::Path) -> anyhow::Result<PathLockGuard> {
    use std::io::ErrorKind;
    use std::time::{Duration, Instant};

    let lock_path = path_lock_path(path);
    let wait_timeout = Duration::from_secs(5);
    let stale_after = Duration::from_secs(120);
    let start = Instant::now();

    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                let _ = writeln!(
                    file,
                    "{} {}",
                    std::process::id(),
                    chrono::Utc::now().to_rfc3339()
                );
                return Ok(PathLockGuard { lock_path });
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock_path)
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .map(|elapsed| elapsed > stale_after)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&lock_path);
                    continue;
                }
                anyhow::ensure!(
                    start.elapsed() < wait_timeout,
                    "timed out waiting for file lock {}",
                    lock_path.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err.into()),
        }
    }
}

pub fn with_path_lock<T, F>(path: &std::path::Path, f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    let _guard = acquire_path_lock(path)?;
    f()
}

pub fn task_journal_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("jsonl")
}

pub fn append_task_journal_event(
    path: &std::path::Path,
    event: &TaskJournalEvent,
) -> anyhow::Result<()> {
    use std::io::Write as _;

    let journal_path = task_journal_path(path);
    if let Some(parent) = journal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&journal_path)?;
    serde_json::to_writer(&mut file, event)?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn read_task_journal_events(path: &std::path::Path) -> anyhow::Result<Vec<TaskJournalEvent>> {
    use std::io::BufRead as _;

    let journal_path = task_journal_path(path);
    match std::fs::File::open(&journal_path) {
        Ok(file) => std::io::BufReader::new(file)
            .lines()
            .filter(|line| {
                line.as_ref()
                    .map(|line| !line.trim().is_empty())
                    .unwrap_or(true)
            })
            .map(|line| Ok(serde_json::from_str::<TaskJournalEvent>(&line?)?))
            .collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err.into()),
    }
}

pub fn seed_task_journal_from_snapshot(path: &std::path::Path) -> anyhow::Result<()> {
    let journal_path = task_journal_path(path);
    if journal_path.exists() {
        return Ok(());
    }
    let snapshot = match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str::<serde_json::Value>(&data)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    append_task_journal_event(path, &TaskJournalEvent::Snapshot { tasks: snapshot })
}

/// Atomic write: serialize `value` as JSON to a tmp file then rename into place.
pub fn atomic_write_json<T: serde::Serialize + ?Sized>(
    path: &std::path::Path,
    value: &T,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub use adapters::{
    DynamicRoute, DynamicRouteRegistry, GatewayChannelAdapter, RouteHandler, NOTIFY_SENDER_ID,
};
pub use channel::{
    BotCommand, Channel, ChannelCapabilities, ChannelEvent, ChannelRef, InboundAttachment,
    InboundMessage, MessageHandle, OutboundTarget,
};
pub use config::{AdapterConfig, AdapterType, ChannelConfig};
pub use factory::{ChannelBuildMode, ChannelFactoryRegistry};
pub use hook::ToolLogEvent;
pub use inbound::InboundRouter;
pub use message::{InlineButton, KeyboardButton, OutboundMessage, ParseMode, ReplyMarkup};
pub use registry::{ChannelEntry, DynamicChannelRegistry};
pub use router::ChannelRouter;
pub use tool::{ChannelNotifyTool, ChannelToolError};
