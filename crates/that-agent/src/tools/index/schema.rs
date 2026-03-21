//! SQLite schema definitions and migrations for the symbol index.
//!
//! The index stores symbol definitions, cross-file references, and cached
//! PageRank scores. Schema is versioned for future migrations.

use rusqlite::Connection;

/// Current schema version. Increment when DDL changes.
pub const SCHEMA_VERSION: &str = "1";

/// Initialize the database schema. Creates tables if they don't exist.
pub fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(DDL)?;

    // Set schema version if not present
    conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION],
    )?;

    Ok(())
}

/// Get the current schema version from the database.
pub fn get_schema_version(conn: &Connection) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = 'schema_version'")?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

const DDL: &str = r#"
-- Metadata for schema versioning
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT
);

-- Indexed files with content hash for staleness detection
CREATE TABLE IF NOT EXISTS files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    hash TEXT NOT NULL,
    mtime INTEGER NOT NULL
);

-- Symbol definitions
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    line_end INTEGER NOT NULL,
    byte_start INTEGER NOT NULL,
    byte_end INTEGER NOT NULL
);

-- Cross-file references (import edges, type usages, calls)
CREATE TABLE IF NOT EXISTS refs (
    id INTEGER PRIMARY KEY,
    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    line INTEGER NOT NULL,
    kind TEXT NOT NULL
);

-- Cached PageRank scores (populated by PageRank computation)
CREATE TABLE IF NOT EXISTS file_scores (
    file_id INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    pagerank REAL NOT NULL DEFAULT 0.0,
    symbol_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_refs_symbol ON refs(symbol_id);
CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id);

PRAGMA foreign_keys = ON;
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_schema_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify tables exist by querying them
        let tables = ["files", "symbols", "refs", "file_scores", "meta"];
        for table in &tables {
            let count: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {}", table), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert!(count >= 0, "table {} should be queryable", table);
        }
    }

    #[test]
    fn test_schema_version_is_set() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let version = get_schema_version(&conn).unwrap();
        assert_eq!(version, Some(SCHEMA_VERSION.to_string()));
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // Should not fail on second call
    }
}
