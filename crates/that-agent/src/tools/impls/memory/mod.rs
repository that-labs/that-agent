//! Persistent memory system for that-tools.
//!
//! SQLite FTS5-based memory storage with recency-boosted BM25 ranking,
//! trigram substring fallback, and near-duplicate detection.
//! Database path comes from config (`memory.db_path`), and defaults are resolved by
//! the caller (the runtime now uses an agent-scoped path).

pub mod schema;
pub mod search;
pub mod store;

use crate::tools::config::MemoryConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use store::MemoryStore;

/// Result of adding a memory.
#[derive(Debug, Serialize, Deserialize)]
pub struct AddResult {
    pub id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub created_at: String,
}

/// Result of `that-tools mem compact`.
///
/// Includes session state fields that are populated by the caller after
/// the memory entry is written and the session context is reset.
#[derive(Debug, Serialize, Deserialize)]
pub struct CompactResult {
    pub id: String,
    pub content: String,
    pub created_at: String,
    pub session_id: Option<String>,
    /// True when the caller successfully reset context_tokens for the session.
    pub context_tokens_reset: bool,
    /// The new compaction_count for the session after this call (None if no session).
    pub compaction_count: Option<usize>,
}

/// Result of removing a memory.
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoveResult {
    pub id: String,
    pub removed: bool,
}

/// Result of unpinning a memory.
#[derive(Debug, Serialize, Deserialize)]
pub struct UnpinResult {
    pub id: String,
    pub unpinned: bool,
}

/// A recalled memory entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub access_count: u64,
    pub last_accessed: Option<String>,
    pub pinned: bool,
    pub rank: f64,
}

/// Memory statistics.
#[derive(Debug, Serialize, Deserialize)]
pub struct MemoryStats {
    pub total_memories: u64,
    pub total_tags: u64,
    pub oldest: Option<String>,
    pub newest: Option<String>,
    pub db_size_bytes: u64,
    pub pinned_count: u64,
}

/// Resolve the default memory database path.
pub fn default_memory_db_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("that-tools")
        .join("memory.db")
}

fn resolve_db_path(config: &MemoryConfig) -> PathBuf {
    if config.db_path.is_empty() {
        default_memory_db_path()
    } else {
        PathBuf::from(&config.db_path)
    }
}

fn ensure_db_dir(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Ensure the memory DB exists and schema is initialized.
///
/// Returns the resolved DB path.
pub fn ensure_initialized(config: &MemoryConfig) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    ensure_db_dir(&db_path)?;
    let _store = MemoryStore::open(&db_path)?;
    Ok(db_path)
}

/// Add a new memory (with automatic near-duplicate detection).
pub fn add(
    content: &str,
    tags: &[String],
    source: Option<&str>,
    session_id: Option<&str>,
    pin: bool,
    config: &MemoryConfig,
) -> Result<AddResult, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    ensure_db_dir(&db_path)?;
    let store = MemoryStore::open(&db_path)?;
    store.add(content, tags, source, session_id, pin)
}

/// Store a durable compaction summary as a pinned memory entry.
///
/// Returns a `CompactResult` with `context_tokens_reset: false` and
/// `compaction_count: None`. Callers are responsible for updating session state
/// (reset_context + increment_compaction) and setting those fields.
pub fn compact(
    summary: &str,
    session_id: Option<&str>,
    config: &MemoryConfig,
) -> Result<CompactResult, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    ensure_db_dir(&db_path)?;
    let store = MemoryStore::open(&db_path)?;
    let add = store.compact(summary, session_id)?;
    Ok(CompactResult {
        id: add.id,
        content: add.content,
        created_at: add.created_at,
        session_id: session_id.map(|s| s.to_string()),
        context_tokens_reset: false,
        compaction_count: None,
    })
}

/// Demote a pinned memory entry to unpinned.
pub fn unpin(id: &str, config: &MemoryConfig) -> Result<UnpinResult, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(UnpinResult {
            id: id.to_string(),
            unpinned: false,
        });
    }
    let store = MemoryStore::open(&db_path)?;
    let unpinned = store.unpin_by_id(id)?;
    Ok(UnpinResult {
        id: id.to_string(),
        unpinned,
    })
}

/// Remove a single memory by ID.
pub fn remove(id: &str, config: &MemoryConfig) -> Result<RemoveResult, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(RemoveResult {
            id: id.to_string(),
            removed: false,
        });
    }
    let store = MemoryStore::open(&db_path)?;
    let removed = store.remove_by_id(id)?;
    Ok(RemoveResult {
        id: id.to_string(),
        removed,
    })
}

/// Return recently pinned memories for auto-injection into system reminders.
pub fn get_pinned(
    limit: usize,
    config: &MemoryConfig,
) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let store = MemoryStore::open(&db_path)?;
    store.get_pinned(limit)
}

/// Recall memories with recency-boosted BM25 ranking and trigram fallback.
///
/// When `session_id` is `Some`, only memories from that session are returned.
pub fn recall(
    query: &str,
    limit: usize,
    session_id: Option<&str>,
    config: &MemoryConfig,
) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let store = MemoryStore::open(&db_path)?;
    store.recall(query, limit, session_id)
}

/// Search memories with optional tag and session filtering.
pub fn search(
    query: &str,
    tags: Option<&[String]>,
    limit: usize,
    session_id: Option<&str>,
    config: &MemoryConfig,
) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let store = MemoryStore::open(&db_path)?;
    store.search(query, tags, limit, session_id)
}

/// Prune old or low-access memories.
pub fn prune(
    before_days: Option<u64>,
    min_access: Option<u64>,
    config: &MemoryConfig,
) -> Result<u64, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(0);
    }
    let store = MemoryStore::open(&db_path)?;
    store.prune(before_days, min_access)
}

/// Get memory statistics.
pub fn stats(config: &MemoryConfig) -> Result<MemoryStats, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(MemoryStats {
            total_memories: 0,
            total_tags: 0,
            oldest: None,
            newest: None,
            db_size_bytes: 0,
            pinned_count: 0,
        });
    }
    let store = MemoryStore::open(&db_path)?;
    store.stats()
}

/// Export all memories to JSON.
pub fn export_memories(
    config: &MemoryConfig,
) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    let store = MemoryStore::open(&db_path)?;
    store.export_all()
}

/// Import memories from JSON.
pub fn import_memories(
    memories: &[MemoryEntry],
    config: &MemoryConfig,
) -> Result<u64, Box<dyn std::error::Error>> {
    let db_path = resolve_db_path(config);
    ensure_db_dir(&db_path)?;
    let store = MemoryStore::open(&db_path)?;
    store.import(memories)
}
