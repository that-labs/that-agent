//! Database schema for the memory system.
//!
//! Uses SQLite FTS5 for full-text search with BM25 ranking and a
//! secondary trigram FTS5 table for substring fallback matching.
//! Prefix matching via `word*` in queries handles morphological
//! variants: "deploy*" matches deploy/deployment/deploying.

use rusqlite::Connection;

/// Current schema version, tracked via `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 3;

/// SQL statements to initialize the memory database.
///
/// FTS5 uses the default `unicode61` tokenizer (case-folding, unicode-aware).
/// We rely on prefix queries (`word*`) in `preprocess_query()` rather than
/// porter stemming because prefix matching on raw tokens gives more
/// predictable results for technical terms (deploy*, argo*, k8s*, etc.).
pub const CREATE_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    summary TEXT,
    tags TEXT NOT NULL DEFAULT '',
    source TEXT,
    session_id TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    access_count INTEGER NOT NULL DEFAULT 0,
    last_accessed TEXT,
    pinned INTEGER NOT NULL DEFAULT 0,
    embedding BLOB
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    content,
    tags,
    source,
    content='memories',
    content_rowid='rowid',
    tokenize='unicode61'
);

-- Trigger to keep FTS in sync on insert
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, tags, source)
    VALUES (new.rowid, new.content, new.tags, new.source);
END;

-- Trigger to keep FTS in sync on delete
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags, source)
    VALUES ('delete', old.rowid, old.content, old.tags, old.source);
END;

-- Trigger to keep FTS in sync on update
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags, source)
    VALUES ('delete', old.rowid, old.content, old.tags, old.source);
    INSERT INTO memories_fts(rowid, content, tags, source)
    VALUES (new.rowid, new.content, new.tags, new.source);
END;

-- Trigram FTS5 table for substring matching fallback
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts_trigram USING fts5(
    content,
    content='memories',
    content_rowid='rowid',
    tokenize='trigram'
);

-- Trigger to keep trigram FTS in sync on insert
CREATE TRIGGER IF NOT EXISTS memories_ai_trigram AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts_trigram(rowid, content)
    VALUES (new.rowid, new.content);
END;

-- Trigger to keep trigram FTS in sync on delete
CREATE TRIGGER IF NOT EXISTS memories_ad_trigram AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts_trigram(memories_fts_trigram, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

-- Trigger to keep trigram FTS in sync on update
CREATE TRIGGER IF NOT EXISTS memories_au_trigram AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts_trigram(memories_fts_trigram, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts_trigram(rowid, content)
    VALUES (new.rowid, new.content);
END;

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at);
CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);
CREATE INDEX IF NOT EXISTS idx_memories_tags ON memories(tags);
CREATE INDEX IF NOT EXISTS idx_memories_pinned ON memories(pinned);
-- Indexes added in v3 (scoped recall, prune, and access-based retention)
CREATE INDEX IF NOT EXISTS idx_memories_session_id ON memories(session_id);
CREATE INDEX IF NOT EXISTS idx_memories_access_count ON memories(access_count);
CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
"#;

/// SQL to migrate from v0 (old schema without explicit tokenizer) to v1.
/// Safe because FTS5 is external-content — just an index over `memories`.
const MIGRATE_V0_TO_V1: &str = r#"
DROP TRIGGER IF EXISTS memories_ai;
DROP TRIGGER IF EXISTS memories_ad;
DROP TRIGGER IF EXISTS memories_au;
DROP TABLE IF EXISTS memories_fts;

CREATE VIRTUAL TABLE memories_fts USING fts5(
    content,
    tags,
    source,
    content='memories',
    content_rowid='rowid',
    tokenize='unicode61'
);

-- Repopulate FTS from the memories table
INSERT INTO memories_fts(rowid, content, tags, source)
    SELECT rowid, content, tags, source FROM memories;

-- Recreate triggers
CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content, tags, source)
    VALUES (new.rowid, new.content, new.tags, new.source);
END;

CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags, source)
    VALUES ('delete', old.rowid, old.content, old.tags, old.source);
END;

CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content, tags, source)
    VALUES ('delete', old.rowid, old.content, old.tags, old.source);
    INSERT INTO memories_fts(rowid, content, tags, source)
    VALUES (new.rowid, new.content, new.tags, new.source);
END;
"#;

/// SQL to migrate from v1 to v2.
/// Adds trigram FTS5 table for substring matching fallback and updated_at index.
const MIGRATE_V1_TO_V2: &str = r#"
CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts_trigram USING fts5(
    content,
    content='memories',
    content_rowid='rowid',
    tokenize='trigram'
);

-- Populate trigram FTS from existing data
INSERT INTO memories_fts_trigram(rowid, content)
    SELECT rowid, content FROM memories;

-- Sync triggers for trigram FTS
CREATE TRIGGER IF NOT EXISTS memories_ai_trigram AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts_trigram(rowid, content)
    VALUES (new.rowid, new.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad_trigram AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts_trigram(memories_fts_trigram, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER IF NOT EXISTS memories_au_trigram AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts_trigram(memories_fts_trigram, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts_trigram(rowid, content)
    VALUES (new.rowid, new.content);
END;
"#;

/// SQL to migrate from v2 to v3.
/// Adds indexes on session_id, access_count, and last_accessed for scoped
/// recall performance, prune efficiency, and access-based retention queries.
const MIGRATE_V2_TO_V3: &str = r#"
CREATE INDEX IF NOT EXISTS idx_memories_session_id ON memories(session_id);
CREATE INDEX IF NOT EXISTS idx_memories_access_count ON memories(access_count);
CREATE INDEX IF NOT EXISTS idx_memories_last_accessed ON memories(last_accessed);
"#;

/// Check the schema version and migrate if needed.
pub fn migrate_schema(conn: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if version < 1 {
        tracing::info!(from = version, to = 1, "migrating memory schema v0→v1");
        conn.execute_batch(MIGRATE_V0_TO_V1)?;
    }

    if version < 2 {
        tracing::info!(
            from = version.max(1),
            to = 2,
            "migrating memory schema to v2"
        );
        conn.execute_batch(MIGRATE_V1_TO_V2)?;
    }

    if version < 3 {
        tracing::info!(
            from = version.max(2),
            to = 3,
            "migrating memory schema to v3 (adding performance indexes)"
        );
        conn.execute_batch(MIGRATE_V2_TO_V3)?;
    }

    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_sql_is_valid() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        // Verify tables exist
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='memories'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_fts_table_created() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        // FTS5 virtual tables appear in sqlite_master
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='memories_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn test_trigram_fts_table_created() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='memories_fts_trigram'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0, "trigram FTS table should exist");
    }

    #[test]
    fn test_migrate_schema_sets_version() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        migrate_schema(&db).unwrap();
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_migrate_schema_is_idempotent() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        migrate_schema(&db).unwrap();
        // Second call should be a no-op
        migrate_schema(&db).unwrap();
        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_prefix_matching_on_raw_tokens() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        migrate_schema(&db).unwrap();
        db.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('t1', 'deploy service', '', 'test', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('t2', 'deployment config', '', 'test', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        // deploy* should match both "deploy" and "deployment" via prefix
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'deploy*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "deploy* should match both deploy and deployment");
    }

    #[test]
    fn test_migrate_preserves_data() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        // Insert test data
        db.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('t1', 'deployment config', 'infra', 'test', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // Run migration
        migrate_schema(&db).unwrap();
        // Data should still be there
        let count: i64 = db
            .query_row("SELECT count(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        // FTS should still work — prefix deploy* matches deployment
        let fts_count: i64 = db
            .query_row(
                "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'deploy*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_count, 1);
    }

    #[test]
    fn test_migrate_v1_to_v2_creates_trigram_fts() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CREATE_TABLES).unwrap();
        // Simulate a v1 database (no trigram FTS yet)
        db.execute_batch("DROP TRIGGER IF EXISTS memories_ai_trigram; DROP TRIGGER IF EXISTS memories_ad_trigram; DROP TRIGGER IF EXISTS memories_au_trigram; DROP TABLE IF EXISTS memories_fts_trigram;").unwrap();
        db.pragma_update(None, "user_version", 1i64).unwrap();

        // Insert data before migration
        db.execute(
            "INSERT INTO memories (id, content, tags, source, created_at, updated_at) VALUES ('t1', 'kubernetes config', 'infra', 'test', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        // Run migration
        migrate_schema(&db).unwrap();

        // Trigram FTS should exist and contain the data
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM memories_fts_trigram WHERE memories_fts_trigram MATCH 'bernet'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "trigram FTS should find substring 'bernet' in 'kubernetes'"
        );

        let version: i64 = db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}
