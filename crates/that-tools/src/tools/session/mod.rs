//! Session tracking for that-tools.
//!
//! Lightweight per-session token accounting and compaction event tracking.
//! State is persisted as JSON at `~/.local/share/that-agent/sessions.json`
//! (configurable via `config.session.sessions_path`).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A single session's tracked state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    /// Cumulative input tokens added across all turns.
    pub input_tokens: usize,
    /// Running context token count (same as input_tokens for now).
    pub context_tokens: usize,
    /// Number of times `that-tools mem compact` has been called for this session.
    pub compaction_count: usize,
    /// Timestamp of the last compaction event.
    pub last_flush_at: Option<String>,
}

/// Stats response for `that-tools session stats`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStats {
    pub session_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub input_tokens: usize,
    pub context_tokens: usize,
    pub compaction_count: usize,
    pub last_flush_at: Option<String>,
    /// True when `context_tokens > soft_threshold_tokens`.
    pub flush_recommended: bool,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct SessionsFile {
    sessions: HashMap<String, SessionRecord>,
}

/// Resolve the default sessions.json path.
pub fn default_sessions_path() -> PathBuf {
    dirs::data_local_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("that-tools")
        .join("sessions.json")
}

fn load_sessions(path: &Path) -> SessionsFile {
    if !path.exists() {
        return SessionsFile::default();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return SessionsFile::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_sessions(path: &Path, file: &SessionsFile) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(file)?;
    std::fs::write(path, content)?;
    Ok(())
}

fn now_iso() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let z = days_since_epoch as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y, m, d, hours, minutes, seconds
    )
}

/// Initialize or retrieve a session.
///
/// If `session_id` is `None`, a new UUID is generated.
/// If the session already exists, it is returned unchanged.
pub fn init_session(
    session_id: Option<String>,
    sessions_path: &Path,
) -> Result<SessionRecord, Box<dyn std::error::Error>> {
    let mut file = load_sessions(sessions_path);
    let sid = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let now = now_iso();
    let record = file
        .sessions
        .entry(sid.clone())
        .or_insert_with(|| SessionRecord {
            session_id: sid,
            created_at: now.clone(),
            updated_at: now,
            input_tokens: 0,
            context_tokens: 0,
            compaction_count: 0,
            last_flush_at: None,
        });
    let record = record.clone();
    save_sessions(sessions_path, &file)?;
    Ok(record)
}

/// Get session statistics, including a `flush_recommended` hint.
pub fn get_stats(
    session_id: &str,
    sessions_path: &Path,
    soft_threshold: usize,
) -> Result<SessionStats, Box<dyn std::error::Error>> {
    let file = load_sessions(sessions_path);
    let record = file
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("session not found: {}", session_id))?;
    let flush_recommended = record.context_tokens > soft_threshold;
    Ok(SessionStats {
        session_id: record.session_id,
        created_at: record.created_at,
        updated_at: record.updated_at,
        input_tokens: record.input_tokens,
        context_tokens: record.context_tokens,
        compaction_count: record.compaction_count,
        last_flush_at: record.last_flush_at,
        flush_recommended,
    })
}

/// Accumulate token usage for a session.
pub fn add_tokens(
    session_id: &str,
    tokens: usize,
    sessions_path: &Path,
) -> Result<SessionRecord, Box<dyn std::error::Error>> {
    let mut file = load_sessions(sessions_path);
    let now = now_iso();
    let record = file
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| format!("session not found: {}", session_id))?;
    record.context_tokens += tokens;
    record.input_tokens += tokens;
    record.updated_at = now;
    let record = record.clone();
    save_sessions(sessions_path, &file)?;
    Ok(record)
}

/// Reset the context token counter for a session to a given value.
///
/// Called automatically after `that-tools mem compact` — the context has been distilled
/// into a pinned summary, so the running token count should reflect a fresh start.
/// `to_tokens` is typically 0 (full reset) or the token count of the new summary.
pub fn reset_context(
    session_id: &str,
    to_tokens: usize,
    sessions_path: &Path,
) -> Result<SessionRecord, Box<dyn std::error::Error>> {
    let mut file = load_sessions(sessions_path);
    let now = now_iso();
    let record = file
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| format!("session not found: {}", session_id))?;
    record.context_tokens = to_tokens;
    record.updated_at = now;
    let record = record.clone();
    save_sessions(sessions_path, &file)?;
    Ok(record)
}

/// Increment the compaction count for a session (called by `that-tools mem compact`).
pub fn increment_compaction(
    session_id: &str,
    sessions_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = load_sessions(sessions_path);
    let now = now_iso();
    if let Some(record) = file.sessions.get_mut(session_id) {
        record.compaction_count += 1;
        record.last_flush_at = Some(now.clone());
        record.updated_at = now;
        save_sessions(sessions_path, &file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sessions_path(tmp: &TempDir) -> PathBuf {
        tmp.path().join("sessions.json")
    }

    #[test]
    fn test_init_creates_session() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        let record = init_session(Some("sess-1".to_string()), &path).unwrap();
        assert_eq!(record.session_id, "sess-1");
        assert_eq!(record.context_tokens, 0);
        assert_eq!(record.compaction_count, 0);
    }

    #[test]
    fn test_init_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        // Second call should return same record, not overwrite
        let record = init_session(Some("sess-1".to_string()), &path).unwrap();
        assert_eq!(record.session_id, "sess-1");
    }

    #[test]
    fn test_init_no_id_generates_uuid() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        let record = init_session(None, &path).unwrap();
        assert!(!record.session_id.is_empty());
        assert_eq!(record.session_id.len(), 36); // UUID length
    }

    #[test]
    fn test_add_tokens_accumulates() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        add_tokens("sess-1", 500, &path).unwrap();
        add_tokens("sess-1", 300, &path).unwrap();
        let stats = get_stats("sess-1", &path, 100_000).unwrap();
        assert_eq!(stats.context_tokens, 800);
        assert_eq!(stats.input_tokens, 800);
    }

    #[test]
    fn test_flush_recommended_when_over_threshold() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        add_tokens("sess-1", 150_000, &path).unwrap();
        let stats = get_stats("sess-1", &path, 100_000).unwrap();
        assert!(
            stats.flush_recommended,
            "should recommend flush when over threshold"
        );
    }

    #[test]
    fn test_flush_not_recommended_under_threshold() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        add_tokens("sess-1", 1000, &path).unwrap();
        let stats = get_stats("sess-1", &path, 100_000).unwrap();
        assert!(!stats.flush_recommended);
    }

    #[test]
    fn test_increment_compaction_count() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        increment_compaction("sess-1", &path).unwrap();
        increment_compaction("sess-1", &path).unwrap();
        let stats = get_stats("sess-1", &path, 100_000).unwrap();
        assert_eq!(stats.compaction_count, 2);
        assert!(stats.last_flush_at.is_some());
    }

    #[test]
    fn test_get_stats_unknown_session_errors() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        let result = get_stats("nonexistent", &path, 100_000);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("session not found"));
    }

    #[test]
    fn test_add_tokens_unknown_session_errors() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        let result = add_tokens("nonexistent", 100, &path);
        assert!(result.is_err());
    }

    #[test]
    fn test_reset_context_clears_tokens() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        add_tokens("sess-1", 90_000, &path).unwrap();

        // Context is high — flush would be recommended
        let before = get_stats("sess-1", &path, 100_000).unwrap();
        assert!(!before.flush_recommended);
        add_tokens("sess-1", 20_000, &path).unwrap();
        let over = get_stats("sess-1", &path, 100_000).unwrap();
        assert!(
            over.flush_recommended,
            "should recommend flush before reset"
        );

        // After compact, context resets to 0
        reset_context("sess-1", 0, &path).unwrap();
        let after = get_stats("sess-1", &path, 100_000).unwrap();
        assert_eq!(after.context_tokens, 0, "context should be reset to 0");
        assert!(
            !after.flush_recommended,
            "flush_recommended should clear after reset"
        );
        // input_tokens preserved (audit trail)
        assert_eq!(
            after.input_tokens, 110_000,
            "input_tokens should be unchanged"
        );
    }

    #[test]
    fn test_reset_context_to_nonzero() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        init_session(Some("sess-1".to_string()), &path).unwrap();
        add_tokens("sess-1", 50_000, &path).unwrap();

        // Reset to the token count of the new compaction summary
        reset_context("sess-1", 500, &path).unwrap();
        let stats = get_stats("sess-1", &path, 100_000).unwrap();
        assert_eq!(stats.context_tokens, 500);
    }

    #[test]
    fn test_reset_context_unknown_session_errors() {
        let tmp = TempDir::new().unwrap();
        let path = sessions_path(&tmp);
        let result = reset_context("nonexistent", 0, &path);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("session not found"));
    }
}
