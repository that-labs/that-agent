//! Memory store implementation using SQLite + FTS5.

use super::{schema, search, AddResult, MemoryEntry, MemoryStats};
use rusqlite::OptionalExtension;
use rusqlite::{params, Connection};
use std::path::Path;

pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open or create the memory database at the given path.
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(schema::CREATE_TABLES)?;
        schema::migrate_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(schema::CREATE_TABLES)?;
        schema::migrate_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Add a new memory entry.
    pub fn add(
        &self,
        content: &str,
        tags: &[String],
        source: Option<&str>,
        session_id: Option<&str>,
        pin: bool,
    ) -> Result<AddResult, Box<dyn std::error::Error>> {
        let content = content.trim();
        if content.is_empty() {
            return Err("memory content cannot be empty".into());
        }
        if content.len() > 1_000_000 {
            return Err("memory content exceeds maximum size (1MB)".into());
        }

        let now = chrono_now();

        // Dedup: if identical content already exists, just touch updated_at
        let existing: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT id, created_at FROM memories WHERE LOWER(content) = LOWER(?1)",
                params![content],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((existing_id, created_at)) = existing {
            self.conn.execute(
                "UPDATE memories SET updated_at = ?1 WHERE id = ?2",
                params![now, existing_id],
            )?;
            return Ok(AddResult {
                id: existing_id,
                content: content.to_string(),
                tags: tags.to_vec(),
                created_at,
            });
        }

        // Near-duplicate detection via trigram FTS + Jaccard similarity
        if let Some((near_id, near_created_at)) = self.find_near_duplicate(content)? {
            self.conn.execute(
                "UPDATE memories SET updated_at = ?1 WHERE id = ?2",
                params![now, near_id],
            )?;
            return Ok(AddResult {
                id: near_id,
                content: content.to_string(),
                tags: tags.to_vec(),
                created_at: near_created_at,
            });
        }

        let id = uuid::Uuid::new_v4().to_string();
        let tags_str = tags.join(",");

        self.conn.execute(
            "INSERT INTO memories (id, content, tags, source, session_id, pinned, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, content, tags_str, source, session_id, if pin { 1i64 } else { 0i64 }, now],
        )?;

        Ok(AddResult {
            id,
            content: content.to_string(),
            tags: tags.to_vec(),
            created_at: now,
        })
    }

    /// Store a durable compaction summary as a pinned memory entry.
    ///
    /// The entry is stored with `source = "compaction"` and `pinned = true`,
    /// so it always floats to the top of recall results via the pin boost.
    pub fn compact(
        &self,
        summary: &str,
        session_id: Option<&str>,
    ) -> Result<AddResult, Box<dyn std::error::Error>> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("compaction summary cannot be empty".into());
        }

        let now = chrono_now();
        let id = uuid::Uuid::new_v4().to_string();

        self.conn.execute(
            "INSERT INTO memories (id, content, tags, source, session_id, pinned, created_at, updated_at) VALUES (?1, ?2, '', 'compaction', ?3, 1, ?4, ?4)",
            params![id, summary, session_id, now],
        )?;

        Ok(AddResult {
            id,
            content: summary.to_string(),
            tags: vec![],
            created_at: now,
        })
    }

    /// Recall memories matching a query using FTS5 BM25 ranking with
    /// recency boost and trigram fallback.
    ///
    /// When `session_id` is `Some`, only memories from that session are returned.
    /// When `None`, all memories are returned (global recall).
    pub fn recall(
        &self,
        query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        let fts_query = search::preprocess_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let mut results = self.recall_bm25(&fts_query, limit, session_id)?;

        // Trigram fallback when BM25+prefix finds nothing
        if results.is_empty() {
            results = self.recall_trigram(query, limit, session_id)?;
        }

        Ok(results)
    }

    /// BM25 recall with recency boost and pin boost.
    ///
    /// Score = bm25(negative, more negative = better)
    ///       + age_days * 0.01 (positive penalty for older memories)
    ///       - pinned * 1000.0 (massive boost for pinned)
    ///
    /// Lower score = better match (ORDER BY rank ASC).
    /// When `session_id` is Some, only memories from that session are returned.
    fn recall_bm25(
        &self,
        fts_query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.content, m.summary, m.tags, m.source, m.session_id,
                    m.created_at, m.updated_at, m.access_count, m.last_accessed,
                    m.pinned,
                    bm25(memories_fts)
                        + (julianday('now') - julianday(m.updated_at)) * 0.01
                        - (m.pinned * 1000.0) as rank
             FROM memories_fts f
             JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1
               AND (m.session_id = ?2 OR ?2 IS NULL)
             ORDER BY rank
             LIMIT ?3",
        )?;

        let rows = stmt.query_map(params![fts_query, session_id, limit as i64], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                summary: row.get(2)?,
                tags: parse_tags(&row.get::<_, String>(3)?),
                source: row.get(4)?,
                session_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                access_count: row.get::<_, i64>(8)? as u64,
                last_accessed: row.get(9)?,
                pinned: row.get::<_, i64>(10)? != 0,
                rank: row.get(11)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(entry) => {
                    let now = chrono_now();
                    if let Err(e) = self.conn.execute(
                        "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
                        params![now, entry.id],
                    ) {
                        tracing::warn!("failed to update access count for {}: {}", entry.id, e);
                    }
                    results.push(entry);
                }
                Err(e) => tracing::warn!("failed to read memory row: {}", e),
            }
        }
        Ok(results)
    }

    /// Trigram substring fallback for queries that BM25+prefix can't match.
    ///
    /// Searches each word separately in the trigram FTS table and deduplicates
    /// results, since trigram MATCH requires exact substring (no wildcards).
    /// When `session_id` is Some, only memories from that session are returned.
    fn recall_trigram(
        &self,
        raw_query: &str,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        // Clean the query but don't add * or filter stop words — trigram needs raw substrings
        let cleaned: String = raw_query
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c.is_whitespace() {
                    c
                } else {
                    ' '
                }
            })
            .collect();

        let words: Vec<String> = cleaned
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| w.len() >= 3) // trigram needs at least 3 chars
            .collect();

        if words.is_empty() {
            return Ok(Vec::new());
        }

        let mut seen_ids = std::collections::HashSet::new();
        let mut results = Vec::new();

        for word in &words {
            let mut stmt = self.conn.prepare(
                "SELECT m.id, m.content, m.summary, m.tags, m.source, m.session_id,
                        m.created_at, m.updated_at, m.access_count, m.last_accessed,
                        m.pinned,
                        (julianday('now') - julianday(m.updated_at)) * 0.01
                            - (m.pinned * 1000.0) as rank
                 FROM memories_fts_trigram f
                 JOIN memories m ON m.rowid = f.rowid
                 WHERE memories_fts_trigram MATCH ?1
                   AND (m.session_id = ?2 OR ?2 IS NULL)
                 ORDER BY rank
                 LIMIT ?3",
            )?;

            let rows = stmt.query_map(params![word, session_id, limit as i64], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    summary: row.get(2)?,
                    tags: parse_tags(&row.get::<_, String>(3)?),
                    source: row.get(4)?,
                    session_id: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    access_count: row.get::<_, i64>(8)? as u64,
                    last_accessed: row.get(9)?,
                    pinned: row.get::<_, i64>(10)? != 0,
                    rank: row.get(11)?,
                })
            })?;

            for row in rows {
                match row {
                    Ok(entry) => {
                        if seen_ids.insert(entry.id.clone()) {
                            results.push(entry);
                        }
                    }
                    Err(e) => tracing::warn!("failed to read trigram row: {}", e),
                }
            }
        }

        // Touch access counts for returned results
        let now = chrono_now();
        for entry in &results {
            if let Err(e) = self.conn.execute(
                "UPDATE memories SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
                params![now, entry.id],
            ) {
                tracing::warn!("failed to update access count for {}: {}", entry.id, e);
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    /// Find a near-duplicate of `content` via trigram search + Jaccard similarity.
    ///
    /// Uses the top-3 longest tokens as trigram search terms (better coverage for
    /// short content where a single token might not be selective enough).
    /// Returns `Some((id, created_at))` if a memory with Jaccard > 0.85 is found.
    fn find_near_duplicate(
        &self,
        content: &str,
    ) -> Result<Option<(String, String)>, Box<dyn std::error::Error>> {
        let new_tokens = search::tokenize_content(content);
        if new_tokens.is_empty() {
            return Ok(None);
        }

        // Top-3 longest tokens as search terms — single longest token can miss
        // candidates when content is short or shares few long words.
        let mut eligible: Vec<&String> = new_tokens.iter().filter(|w| w.len() >= 3).collect();
        eligible.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let search_terms: Vec<String> = eligible.into_iter().take(3).cloned().collect();

        if search_terms.is_empty() {
            return Ok(None);
        }

        // Union candidates from all search terms, deduplicating by id.
        let mut seen_ids = std::collections::HashSet::new();
        let mut candidates: Vec<(String, String, String)> = Vec::new();

        for term in &search_terms {
            let mut stmt = self.conn.prepare(
                "SELECT m.id, m.content, m.created_at
                 FROM memories_fts_trigram f
                 JOIN memories m ON m.rowid = f.rowid
                 WHERE memories_fts_trigram MATCH ?1
                 LIMIT 10",
            )?;
            let rows = stmt.query_map(params![term], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let triple = row?;
                if seen_ids.insert(triple.0.clone()) {
                    candidates.push(triple);
                }
            }
        }

        for (id, candidate_content, created_at) in candidates {
            let candidate_tokens = search::tokenize_content(&candidate_content);
            let sim = search::jaccard_similarity(&new_tokens, &candidate_tokens);
            if sim > 0.85 {
                tracing::debug!(
                    id = %id,
                    similarity = sim,
                    "near-duplicate detected, updating existing memory"
                );
                return Ok(Some((id, created_at)));
            }
        }

        Ok(None)
    }

    /// Search memories with optional tag and session filtering.
    pub fn search(
        &self,
        query: &str,
        tags: Option<&[String]>,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        if query.is_empty() && tags.is_none() {
            // Return most recent
            return self.recent(limit, session_id);
        }

        let mut results = if !query.is_empty() {
            self.recall(query, limit * 2, session_id)?
        } else {
            self.recent(limit * 2, session_id)?
        };

        // Filter by tags if specified
        if let Some(tag_filter) = tags {
            results.retain(|entry| {
                tag_filter
                    .iter()
                    .any(|t| entry.tags.iter().any(|et| et.eq_ignore_ascii_case(t)))
            });
        }

        results.truncate(limit);
        Ok(results)
    }

    /// Get most recent memories, with optional session filter.
    fn recent(
        &self,
        limit: usize,
        session_id: Option<&str>,
    ) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, summary, tags, source, session_id,
                    created_at, updated_at, access_count, last_accessed, pinned
             FROM memories
             WHERE (session_id = ?1 OR ?1 IS NULL)
             ORDER BY created_at DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![session_id, limit as i64], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                summary: row.get(2)?,
                tags: parse_tags(&row.get::<_, String>(3)?),
                source: row.get(4)?,
                session_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                access_count: row.get::<_, i64>(8)? as u64,
                last_accessed: row.get(9)?,
                pinned: row.get::<_, i64>(10)? != 0,
                rank: 0.0,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(entry) => results.push(entry),
                Err(e) => tracing::warn!("failed to read memory row: {}", e),
            }
        }
        Ok(results)
    }

    /// Return recently pinned memories (within 30 days), ordered by most recent first.
    pub fn get_pinned(&self, limit: usize) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, summary, tags, source, session_id,
                    created_at, updated_at, access_count, last_accessed, pinned
             FROM memories
             WHERE pinned = 1 AND updated_at > datetime('now', '-30 days')
             ORDER BY updated_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                content: row.get(1)?,
                summary: row.get(2)?,
                tags: parse_tags(&row.get::<_, String>(3)?),
                source: row.get(4)?,
                session_id: row.get(5)?,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                access_count: row.get::<_, i64>(8)? as u64,
                last_accessed: row.get(9)?,
                pinned: true,
                rank: 0.0,
            })
        })?;
        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(entry) => results.push(entry),
                Err(e) => tracing::warn!("failed to read pinned row: {}", e),
            }
        }
        Ok(results)
    }

    /// Remove a single memory by ID.
    pub fn remove_by_id(&self, id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let deleted = self
            .conn
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        Ok(deleted > 0)
    }

    /// Demote a pinned memory entry to unpinned.
    ///
    /// Safe to call on already-unpinned entries (no-op).
    /// Returns true if the entry existed, false if not found.
    pub fn unpin_by_id(&self, id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let updated = self
            .conn
            .execute("UPDATE memories SET pinned = 0 WHERE id = ?1", params![id])?;
        Ok(updated > 0)
    }

    /// Prune old or low-access memories.
    pub fn prune(
        &self,
        before_days: Option<u64>,
        min_access: Option<u64>,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let mut deleted = 0u64;

        if let Some(days) = before_days {
            let count = self.conn.execute(
                "DELETE FROM memories WHERE pinned = 0 AND created_at < datetime('now', ?1)",
                params![format!("-{} days", days)],
            )?;
            deleted += count as u64;
        }

        if let Some(min) = min_access {
            let count = self.conn.execute(
                "DELETE FROM memories WHERE pinned = 0 AND access_count < ?1",
                params![min as i64],
            )?;
            deleted += count as u64;
        }

        Ok(deleted)
    }

    /// Get memory store statistics.
    pub fn stats(&self) -> Result<MemoryStats, Box<dyn std::error::Error>> {
        let total_memories: i64 =
            self.conn
                .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))?;

        let total_tags: i64 = self.conn.query_row(
            "SELECT count(DISTINCT value) FROM (
                SELECT trim(value) as value FROM memories, json_each('[\"\"]')
                UNION ALL
                SELECT trim(value) as value FROM (
                    WITH RECURSIVE split(str, rest) AS (
                        SELECT '', tags || ',' FROM memories WHERE tags != ''
                        UNION ALL
                        SELECT substr(rest, 1, instr(rest, ',') - 1), substr(rest, instr(rest, ',') + 1)
                        FROM split WHERE rest != ''
                    )
                    SELECT str as value FROM split WHERE str != ''
                )
            ) WHERE value != ''",
            [],
            |row| row.get(0),
        ).unwrap_or(0);

        let oldest: Option<String> = self
            .conn
            .query_row("SELECT min(created_at) FROM memories", [], |row| row.get(0))
            .ok()
            .flatten();

        let newest: Option<String> = self
            .conn
            .query_row("SELECT max(created_at) FROM memories", [], |row| row.get(0))
            .ok()
            .flatten();

        let pinned_count: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM memories WHERE pinned = 1",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Get database file size
        let db_size: i64 = self
            .conn
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count, pragma_page_size",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        Ok(MemoryStats {
            total_memories: total_memories as u64,
            total_tags: total_tags as u64,
            oldest,
            newest,
            db_size_bytes: db_size as u64,
            pinned_count: pinned_count as u64,
        })
    }

    /// Export all memories.
    pub fn export_all(&self) -> Result<Vec<MemoryEntry>, Box<dyn std::error::Error>> {
        self.recent(usize::MAX, None)
    }

    /// Import memories from a list.
    pub fn import(&self, memories: &[MemoryEntry]) -> Result<u64, Box<dyn std::error::Error>> {
        let mut imported = 0u64;
        for entry in memories {
            let tags_str = entry.tags.join(",");
            let result = self.conn.execute(
                "INSERT OR IGNORE INTO memories (id, content, summary, tags, source, session_id, created_at, updated_at, access_count, last_accessed, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    entry.id, entry.content, entry.summary, tags_str, entry.source,
                    entry.session_id, entry.created_at, entry.updated_at,
                    entry.access_count as i64, entry.last_accessed,
                    if entry.pinned { 1i64 } else { 0i64 }
                ],
            )?;
            imported += result as u64;
        }
        Ok(imported)
    }
}

fn parse_tags(tags_str: &str) -> Vec<String> {
    tags_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn chrono_now() -> String {
    // Emit ISO 8601 format compatible with SQLite's datetime() functions.
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Convert epoch seconds to "YYYY-MM-DD HH:MM:SS" UTC
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Civil date from days since 1970-01-01 (algorithm from Howard Hinnant)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_recall() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store
            .add(
                "test memory content",
                &["tag1".to_string(), "tag2".to_string()],
                Some("test"),
                None,
                false,
            )
            .unwrap();
        assert!(!result.id.is_empty());
        assert_eq!(result.content, "test memory content");

        let recalled = store.recall("test memory", 5, None).unwrap();
        assert!(!recalled.is_empty());
        assert!(recalled[0].content.contains("test memory"));
    }

    #[test]
    fn test_add_and_search() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add(
                "search me please",
                &["findable".to_string()],
                None,
                None,
                false,
            )
            .unwrap();
        store
            .add("another memory", &["other".to_string()], None, None, false)
            .unwrap();

        let results = store.search("search", None, 10, None).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_stats_empty_db() {
        let store = MemoryStore::open_in_memory().unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.total_memories, 0);
    }

    #[test]
    fn test_stats_with_data() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("memory 1", &["tag1".to_string()], None, None, false)
            .unwrap();
        store
            .add("memory 2", &["tag2".to_string()], None, None, false)
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.total_memories, 2);
    }

    #[test]
    fn test_prune_by_access() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.add("low access", &[], None, None, false).unwrap();
        let deleted = store.prune(None, Some(5)).unwrap();
        assert_eq!(deleted, 1);
    }

    #[test]
    fn test_export_import() {
        let store1 = MemoryStore::open_in_memory().unwrap();
        store1
            .add("export me", &["export".to_string()], None, None, false)
            .unwrap();
        let exported = store1.export_all().unwrap();
        assert!(!exported.is_empty());

        let store2 = MemoryStore::open_in_memory().unwrap();
        let imported = store2.import(&exported).unwrap();
        assert_eq!(imported, 1);
    }

    #[test]
    fn test_chrono_now_iso_format() {
        let now = chrono_now();
        // Should match "YYYY-MM-DD HH:MM:SS" format
        assert_eq!(now.len(), 19, "timestamp should be 19 chars: {}", now);
        assert_eq!(&now[4..5], "-");
        assert_eq!(&now[7..8], "-");
        assert_eq!(&now[10..11], " ");
        assert_eq!(&now[13..14], ":");
        assert_eq!(&now[16..17], ":");
    }

    #[test]
    fn test_add_empty_content_rejected() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store.add("", &[], None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_add_whitespace_only_rejected() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store.add("   \n\t  ", &[], None, None, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_recall_special_chars_in_query() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.add("i love pizza", &[], None, None, false).unwrap();
        // '?' should not cause FTS5 syntax error
        let results = store.recall("what pizza?", 5, None).unwrap();
        assert!(
            !results.is_empty(),
            "query with ? should match via individual terms"
        );
    }

    #[test]
    fn test_recall_multi_word_matches_individual_terms() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.add("i love pizza", &[], None, None, false).unwrap();
        // "love" is not a stop word, so it should match
        let results = store.recall("love pizza", 5, None).unwrap();
        assert!(
            !results.is_empty(),
            "multi-word query should match via OR of individual terms"
        );
    }

    #[test]
    fn test_recall_pure_punctuation_returns_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.add("some content", &[], None, None, false).unwrap();
        let results = store.recall("???", 5, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_recall_prefix_deploy_finds_deployment() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("deployment pipeline for production", &[], None, None, false)
            .unwrap();
        store
            .add("deploy the service now", &[], None, None, false)
            .unwrap();
        // "deploy" → "deploy*" via prefix matching → finds both
        let results = store.recall("deploy", 5, None).unwrap();
        assert_eq!(
            results.len(),
            2,
            "prefix: 'deploy*' should find both 'deploy' and 'deployment'"
        );
    }

    #[test]
    fn test_recall_prefix_argo_finds_argocd() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("ArgoCD pipeline configuration", &[], None, None, false)
            .unwrap();
        let results = store.recall("argo", 5, None).unwrap();
        assert!(!results.is_empty(), "prefix: 'argo' should find 'ArgoCD'");
    }

    #[test]
    fn test_recall_stop_words_filtered() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("database config in /etc/db.conf", &[], None, None, false)
            .unwrap();
        store
            .add("unrelated stuff about servers", &[], None, None, false)
            .unwrap();
        store
            .add("more unrelated things", &[], None, None, false)
            .unwrap();

        // "where is the" are all stop words — only "db" and "config" matter
        let results = store.recall("where is the db config?", 10, None).unwrap();
        assert!(
            !results.is_empty(),
            "stop words filtered: should still find db config"
        );
        // The first result should be the db config entry
        assert!(
            results[0].content.contains("database") || results[0].content.contains("db"),
            "first result should be the db config entry"
        );
    }

    #[test]
    fn test_recall_all_stop_words_returns_empty() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("something stored", &[], None, None, false)
            .unwrap();
        let results = store.recall("where is the", 5, None).unwrap();
        assert!(results.is_empty(), "all stop words should return empty");
    }

    #[test]
    fn test_dedup_same_content_returns_same_id() {
        let store = MemoryStore::open_in_memory().unwrap();
        let first = store
            .add("exact same content", &[], None, None, false)
            .unwrap();
        let second = store
            .add("exact same content", &[], None, None, false)
            .unwrap();
        assert_eq!(
            first.id, second.id,
            "duplicate content should return same ID"
        );

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_memories, 1, "should only have 1 memory, not 2");
    }

    #[test]
    fn test_dedup_case_insensitive() {
        let store = MemoryStore::open_in_memory().unwrap();
        let first = store.add("Hello World", &[], None, None, false).unwrap();
        let second = store.add("hello world", &[], None, None, false).unwrap();
        assert_eq!(first.id, second.id, "case-insensitive dedup");
    }

    #[test]
    fn test_dedup_different_content_creates_new() {
        let store = MemoryStore::open_in_memory().unwrap();
        let first = store.add("content one", &[], None, None, false).unwrap();
        let second = store.add("content two", &[], None, None, false).unwrap();
        assert_ne!(first.id, second.id);

        let stats = store.stats().unwrap();
        assert_eq!(stats.total_memories, 2);
    }

    #[test]
    fn test_parse_tags() {
        assert_eq!(parse_tags("a,b,c"), vec!["a", "b", "c"]);
        assert_eq!(parse_tags(""), Vec::<String>::new());
        assert_eq!(parse_tags("solo"), vec!["solo"]);
    }

    #[test]
    fn test_recency_boost_newer_ranks_first() {
        let store = MemoryStore::open_in_memory().unwrap();

        // Insert old memory with explicit old timestamp
        store.conn.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('old1', 'deploy config settings', '', 'test', '2020-01-01 00:00:00', '2020-01-01 00:00:00')",
            [],
        ).unwrap();
        // Insert new memory with current timestamp
        store.conn.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('new1', 'deploy config settings updated', '', 'test', datetime('now'), datetime('now'))",
            [],
        ).unwrap();

        let results = store.recall("deploy config", 5, None).unwrap();
        assert!(results.len() >= 2, "should find both memories");
        // Newer memory should rank first due to recency boost
        assert_eq!(results[0].id, "new1", "newer memory should rank first");
    }

    #[test]
    fn test_trigram_fallback_finds_substring() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("ArgoCD workflow configuration", &[], None, None, false)
            .unwrap();

        // "rgoC" won't match via BM25+prefix but trigram should find substring
        let results = store.recall("rgoCD", 5, None).unwrap();
        assert!(
            !results.is_empty(),
            "trigram fallback should find substring match for 'rgoCD'"
        );
        assert!(results[0].content.contains("ArgoCD"));
    }

    #[test]
    fn test_trigram_fallback_short_query_skipped() {
        let store = MemoryStore::open_in_memory().unwrap();
        store.add("hello world", &[], None, None, false).unwrap();

        // Queries with only 1-2 char words after cleaning should return empty from trigram
        // (trigram needs >= 3 chars). But BM25 prefix should still work.
        let results = store.recall("he", 5, None).unwrap();
        // "he" is a stop word, so BM25 returns empty, and trigram skips 2-char words
        assert!(results.is_empty());
    }

    #[test]
    fn test_near_dedup_similar_content_returns_same_id() {
        let store = MemoryStore::open_in_memory().unwrap();
        // 12 non-stop-word tokens; second just adds 1 extra → Jaccard = 12/13 ≈ 0.92
        let first = store
            .add(
                "configure kubernetes cluster deployment argocd pipeline monitoring alerting logging tracing metrics dashboards",
                &[],
                None,
                None,
                false,
            )
            .unwrap();
        let second = store
            .add(
                "configure kubernetes cluster deployment argocd pipeline monitoring alerting logging tracing metrics dashboards grafana",
                &[],
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(first.id, second.id, "near-duplicate should return same ID");

        let stats = store.stats().unwrap();
        assert_eq!(
            stats.total_memories, 1,
            "near-dup should not create new entry"
        );
    }

    #[test]
    fn test_near_dedup_different_content_creates_new() {
        let store = MemoryStore::open_in_memory().unwrap();
        let first = store
            .add("kubernetes cluster configuration", &[], None, None, false)
            .unwrap();
        let second = store
            .add(
                "postgres database optimization tuning",
                &[],
                None,
                None,
                false,
            )
            .unwrap();
        assert_ne!(
            first.id, second.id,
            "different content should create separate entries"
        );
    }

    #[test]
    fn test_pin_boost_outranks_unpinned() {
        let store = MemoryStore::open_in_memory().unwrap();

        // Add unpinned memory
        store.conn.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at, pinned) VALUES ('unpinned1', 'deploy pipeline workflow', '', 'test', datetime('now'), datetime('now'), 0)",
            [],
        ).unwrap();
        // Add pinned memory (older, so without pin boost it would rank lower)
        store.conn.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at, pinned) VALUES ('pinned1', 'deploy pipeline configuration', '', 'test', '2020-01-01 00:00:00', '2020-01-01 00:00:00', 1)",
            [],
        ).unwrap();

        let results = store.recall("deploy pipeline", 5, None).unwrap();
        assert!(results.len() >= 2, "should find both memories");
        assert_eq!(
            results[0].id, "pinned1",
            "pinned memory should rank first despite being older"
        );
    }

    #[test]
    fn test_session_id_filters_recall() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .add("session alpha content", &[], None, Some("session-a"), false)
            .unwrap();
        store
            .add("session beta content", &[], None, Some("session-b"), false)
            .unwrap();
        store
            .add("global memory content", &[], None, None, false)
            .unwrap();

        // Session-scoped recall should only return entries for that session
        let results_a = store.recall("content", 10, Some("session-a")).unwrap();
        assert_eq!(results_a.len(), 1, "should only return session-a entry");
        assert!(results_a[0].content.contains("alpha"));

        // Global recall (no session filter) should return all
        let results_all = store.recall("content", 10, None).unwrap();
        assert_eq!(
            results_all.len(),
            3,
            "global recall should return all entries"
        );
    }

    #[test]
    fn test_compact_creates_pinned_entry() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store
            .compact("Decided to use Postgres for persistence", Some("sess-1"))
            .unwrap();
        assert!(!result.id.is_empty());
        assert_eq!(result.content, "Decided to use Postgres for persistence");

        // Should be pinned and source = compaction
        let row: (i64, String) = store
            .conn
            .query_row(
                "SELECT pinned, source FROM memories WHERE id = ?1",
                rusqlite::params![result.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, 1, "compaction entry should be pinned");
        assert_eq!(row.1, "compaction", "source should be 'compaction'");
    }

    #[test]
    fn test_compact_ranks_first_in_recall() {
        let store = MemoryStore::open_in_memory().unwrap();

        // Add a regular memory
        store
            .add("auth decisions and workflow", &[], None, None, false)
            .unwrap();
        // Add a compaction summary (older timestamp, but pinned)
        store
            .compact("Key auth decisions: JWT tokens, 24h expiry", None)
            .unwrap();

        let results = store.recall("auth decisions", 5, None).unwrap();
        assert!(results.len() >= 2, "should find both entries");
        assert_eq!(
            results[0].source.as_deref(),
            Some("compaction"),
            "compaction entry should rank first due to pin boost"
        );
    }

    #[test]
    fn test_compact_empty_summary_rejected() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store.compact("", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn test_unpin_demotes_pinned_entry() {
        let store = MemoryStore::open_in_memory().unwrap();
        let r = store
            .compact("Key session decisions", Some("sess-1"))
            .unwrap();

        // Confirm it's pinned
        let pinned: i64 = store
            .conn
            .query_row(
                "SELECT pinned FROM memories WHERE id = ?1",
                rusqlite::params![r.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pinned, 1, "compaction should be pinned");

        // Unpin it
        let removed = store.unpin_by_id(&r.id).unwrap();
        assert!(removed, "unpin should return true for existing entry");

        // Confirm pinned = 0
        let pinned_after: i64 = store
            .conn
            .query_row(
                "SELECT pinned FROM memories WHERE id = ?1",
                rusqlite::params![r.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pinned_after, 0, "entry should be unpinned after unpin");
    }

    #[test]
    fn test_unpin_nonexistent_returns_false() {
        let store = MemoryStore::open_in_memory().unwrap();
        let result = store.unpin_by_id("nonexistent-id").unwrap();
        assert!(!result, "unpin on missing id should return false");
    }

    #[test]
    fn test_find_near_duplicate_top3_tokens() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Short content where single longest token might not be selective enough
        let first = store
            .add(
                "postgres connection pool config settings limit timeout retry",
                &[],
                None,
                None,
                false,
            )
            .unwrap();
        // Nearly identical — should be detected as near-duplicate
        let second = store
            .add(
                "postgres connection pool config settings limit timeout retry backoff",
                &[],
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            first.id, second.id,
            "top-3 trigram search should find near-duplicate"
        );
    }
}
