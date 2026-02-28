//! Symbol index backed by SQLite.
//!
//! Provides persistent, project-scoped symbol storage with incremental updates.
//! The index is stored at `.that-tools/index.db` relative to the project root.
//! Files are tracked by content hash (SHA-256) — only changed files are re-parsed.

pub mod pagerank;
pub mod references;
pub mod schema;

use crate::tools::code::parse::{self, Language, Symbol};
use ignore::WalkBuilder;
// references module used for cross-file reference extraction in build_references()
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IndexError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Status of the symbol index.
#[derive(Debug, Clone, Serialize)]
pub struct IndexStatus {
    pub path: String,
    pub total_files: usize,
    pub total_symbols: usize,
    pub total_refs: usize,
    pub stale_files: usize,
    pub schema_version: String,
}

/// A reference result from querying the index.
#[derive(Debug, Clone, Serialize)]
pub struct IndexedReference {
    pub file: String,
    pub line: usize,
    pub kind: String,
}

/// The symbol index — wraps a SQLite connection with query/build API.
pub struct SymbolIndex {
    conn: Connection,
    db_path: PathBuf,
}

impl SymbolIndex {
    /// Open or create the index at the given path.
    pub fn open(db_path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }

    /// Open an in-memory index (for testing).
    #[allow(dead_code)]
    pub fn open_in_memory() -> Result<Self, IndexError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            db_path: PathBuf::from(":memory:"),
        })
    }

    /// Build or incrementally update the index for a project root.
    ///
    /// The entire build is wrapped in a SQLite transaction for atomicity —
    /// if any step fails, the index remains in its previous consistent state.
    pub fn build(&self, root: &Path) -> Result<BuildResult, IndexError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match self.build_inner(root) {
            Ok(result) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(result)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn build_inner(&self, root: &Path) -> Result<BuildResult, IndexError> {
        let walker = WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .build();

        let mut files_indexed = 0;
        let mut files_skipped = 0;
        let mut files_failed = 0;
        let mut symbols_added = 0;

        for entry in walker.flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();
            if Language::from_path(path).is_none() {
                continue;
            }

            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let hash = compute_hash(&content);
            let relative = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            // Check if file is already indexed with same hash
            if self.is_file_current(&relative, &hash)? {
                files_skipped += 1;
                continue;
            }

            // Parse and index the file
            let mtime = std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs()
                })
                .unwrap_or(0) as i64;

            let parsed = match parse::parse_file(path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!("failed to parse {}: {}", relative, e);
                    files_failed += 1;
                    continue;
                }
            };

            // Upsert file
            let file_id = self.upsert_file(&relative, &hash, mtime)?;

            // Delete old symbols for this file (cascade deletes refs too)
            self.conn
                .execute("DELETE FROM symbols WHERE file_id = ?1", [file_id])?;

            // Insert symbols
            for sym in &parsed.symbols {
                self.insert_symbol(file_id, sym)?;
                symbols_added += 1;
            }

            files_indexed += 1;
        }

        // Remove deleted files from the index
        let files_removed = self.remove_deleted_files(root)?;

        // After indexing all files, extract cross-file references
        let refs_added = self.build_references(root)?;

        // Recompute PageRank after any index changes
        if files_indexed > 0 || files_removed > 0 {
            if let Err(e) = pagerank::compute_pagerank(self) {
                tracing::warn!("PageRank recomputation failed: {}", e);
            }
        }

        Ok(BuildResult {
            files_indexed,
            files_skipped,
            files_failed,
            files_removed,
            symbols_added,
            refs_added,
        })
    }

    /// Build cross-file references by scanning all indexed files.
    fn build_references(&self, root: &Path) -> Result<usize, IndexError> {
        // Clear existing refs
        self.conn.execute("DELETE FROM refs", [])?;

        // Get all known symbol names
        let known_symbols = self.all_symbol_names()?;
        if known_symbols.is_empty() {
            return Ok(0);
        }

        // Get all indexed files
        let files = self.all_files()?;
        let mut refs_added = 0;

        for (file_id, rel_path) in &files {
            if !is_safe_relative_path(rel_path) {
                tracing::warn!("skipping path with traversal components: {}", rel_path);
                continue;
            }
            let abs_path = root.join(rel_path);
            let content = match std::fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let language = match Language::from_path(&abs_path) {
                Some(l) => l,
                None => continue,
            };

            let refs = references::extract_references(&content, language, &known_symbols);

            for r in &refs {
                // Find all symbol IDs for the referenced symbol name
                let symbol_ids = self.find_symbol_ids(&r.symbol_name)?;
                for symbol_id in symbol_ids {
                    self.conn.execute(
                        "INSERT INTO refs (symbol_id, file_id, line, kind) VALUES (?1, ?2, ?3, ?4)",
                        params![symbol_id, file_id, r.line as i64, r.kind.to_string()],
                    )?;
                    refs_added += 1;
                }
            }
        }

        Ok(refs_added)
    }

    /// Remove indexed files that no longer exist on disk.
    fn remove_deleted_files(&self, root: &Path) -> Result<usize, IndexError> {
        let files = self.all_files()?;
        let mut removed = 0;
        for (file_id, rel_path) in &files {
            if !is_safe_relative_path(rel_path) {
                tracing::warn!("skipping path with traversal components: {}", rel_path);
                continue;
            }
            let abs_path = root.join(rel_path);
            match std::fs::metadata(&abs_path) {
                Ok(_) => { /* file exists, keep in index */ }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File truly deleted — remove from index (CASCADE deletes symbols and refs)
                    self.conn
                        .execute("DELETE FROM files WHERE id = ?1", [file_id])?;
                    self.conn
                        .execute("DELETE FROM file_scores WHERE file_id = ?1", [file_id])?;
                    removed += 1;
                }
                Err(e) => {
                    tracing::warn!("cannot check file {}: {}, skipping removal", rel_path, e);
                }
            }
        }
        Ok(removed)
    }

    /// Check if a file is already indexed with the same content hash.
    fn is_file_current(&self, path: &str, hash: &str) -> Result<bool, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM files WHERE path = ?1")?;
        let mut rows = stmt.query([path])?;
        match rows.next()? {
            Some(row) => {
                let stored_hash: String = row.get(0)?;
                Ok(stored_hash == hash)
            }
            None => Ok(false),
        }
    }

    /// Insert or update a file record, returning its ID.
    fn upsert_file(&self, path: &str, hash: &str, mtime: i64) -> Result<i64, IndexError> {
        self.conn.execute(
            "INSERT INTO files (path, hash, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET hash = ?2, mtime = ?3",
            params![path, hash, mtime],
        )?;

        let mut stmt = self.conn.prepare("SELECT id FROM files WHERE path = ?1")?;
        let id: i64 = stmt.query_row([path], |row| row.get(0))?;
        Ok(id)
    }

    /// Insert a symbol record for a file.
    fn insert_symbol(&self, file_id: i64, sym: &Symbol) -> Result<(), IndexError> {
        self.conn.execute(
            "INSERT INTO symbols (file_id, name, kind, line_start, line_end, byte_start, byte_end)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                file_id,
                sym.name,
                format!("{:?}", sym.kind).to_lowercase(),
                sym.line_start as i64,
                sym.line_end as i64,
                sym.byte_start as i64,
                sym.byte_end as i64,
            ],
        )?;
        Ok(())
    }

    /// Get all known symbol names in the index.
    fn all_symbol_names(&self) -> Result<Vec<String>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT name FROM symbols")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        let mut names = Vec::new();
        for row in rows {
            match row {
                Ok(name) => names.push(name),
                Err(e) => tracing::warn!("failed to read symbol name: {}", e),
            }
        }
        Ok(names)
    }

    /// Get all indexed files as (id, path) pairs.
    pub fn all_files(&self) -> Result<Vec<(i64, String)>, IndexError> {
        let mut stmt = self.conn.prepare("SELECT id, path FROM files")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut files = Vec::new();
        for row in rows {
            match row {
                Ok(file) => files.push(file),
                Err(e) => tracing::warn!("failed to read file entry: {}", e),
            }
        }
        Ok(files)
    }

    /// Find all symbol IDs by name (handles duplicate names across files).
    fn find_symbol_ids(&self, name: &str) -> Result<Vec<i64>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM symbols WHERE name = ?1")?;
        let rows = stmt.query_map([name], |row| row.get(0))?;
        let mut ids = Vec::new();
        for id in rows.flatten() {
            ids.push(id);
        }
        Ok(ids)
    }

    /// Query references for a given symbol name.
    pub fn query_references(&self, symbol_name: &str) -> Result<Vec<IndexedReference>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, r.line, r.kind
             FROM refs r
             JOIN symbols s ON r.symbol_id = s.id
             JOIN files f ON r.file_id = f.id
             WHERE s.name = ?1
             GROUP BY f.path, r.line
             ORDER BY f.path, r.line",
        )?;

        let rows = stmt.query_map([symbol_name], |row| {
            Ok(IndexedReference {
                file: row.get(0)?,
                line: row.get::<_, i64>(1)? as usize,
                kind: row.get(2)?,
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            match row {
                Ok(r) => results.push(r),
                Err(e) => tracing::warn!("failed to read reference: {}", e),
            }
        }
        Ok(results)
    }

    /// Get the status of the index.
    pub fn status(&self, root: &Path) -> Result<IndexStatus, IndexError> {
        let total_files: usize = self
            .conn
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
        let total_symbols: usize =
            self.conn
                .query_row("SELECT count(*) FROM symbols", [], |row| row.get(0))?;
        let total_refs: usize = self
            .conn
            .query_row("SELECT count(*) FROM refs", [], |row| row.get(0))?;

        let version =
            schema::get_schema_version(&self.conn)?.unwrap_or_else(|| "unknown".to_string());

        // Count stale files (hash mismatch with actual content)
        let stale = self.count_stale_files(root)?;

        Ok(IndexStatus {
            path: self.db_path.to_string_lossy().to_string(),
            total_files,
            total_symbols,
            total_refs,
            stale_files: stale,
            schema_version: version,
        })
    }

    /// Count files whose content hash no longer matches.
    fn count_stale_files(&self, root: &Path) -> Result<usize, IndexError> {
        let files = self.all_files()?;
        let mut stale = 0;
        for (_id, rel_path) in &files {
            if !is_safe_relative_path(rel_path) {
                stale += 1;
                continue;
            }
            let abs_path = root.join(rel_path);
            match std::fs::read_to_string(&abs_path) {
                Ok(content) => {
                    let current_hash = compute_hash(&content);
                    if !self.is_file_current(rel_path, &current_hash)? {
                        stale += 1;
                    }
                }
                Err(_) => stale += 1, // File deleted = stale
            }
        }
        Ok(stale)
    }

    /// Get all file-to-file edges (for PageRank graph construction).
    /// Returns (source_file_path, target_file_path, edge_count) tuples.
    pub fn file_edges(&self) -> Result<Vec<(String, String, usize)>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT ref_f.path, sym_f.path, count(*) as cnt
             FROM refs r
             JOIN files ref_f ON r.file_id = ref_f.id
             JOIN symbols s ON r.symbol_id = s.id
             JOIN files sym_f ON s.file_id = sym_f.id
             WHERE ref_f.id != sym_f.id
             GROUP BY ref_f.path, sym_f.path",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, usize>(2)?,
            ))
        })?;
        let mut edges = Vec::new();
        for row in rows {
            match row {
                Ok(edge) => edges.push(edge),
                Err(e) => tracing::warn!("failed to read file edge: {}", e),
            }
        }

        Ok(edges)
    }

    /// Store PageRank scores for files.
    pub fn store_pagerank_scores(&self, scores: &HashMap<String, f64>) -> Result<(), IndexError> {
        // Clear existing scores
        self.conn.execute("DELETE FROM file_scores", [])?;

        let files = self.all_files()?;
        let symbol_counts = self.symbol_counts_by_file()?;

        for (file_id, path) in &files {
            let score = scores.get(path.as_str()).copied().unwrap_or(0.0);
            let count = symbol_counts.get(file_id).copied().unwrap_or(0);
            self.conn.execute(
                "INSERT INTO file_scores (file_id, pagerank, symbol_count) VALUES (?1, ?2, ?3)",
                params![file_id, score, count as i64],
            )?;
        }

        Ok(())
    }

    /// Get PageRank scores for all files.
    pub fn get_pagerank_scores(&self) -> Result<HashMap<String, f64>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, fs.pagerank
             FROM file_scores fs
             JOIN files f ON fs.file_id = f.id",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })?;
        let mut scores = HashMap::new();
        for row in rows {
            match row {
                Ok((path, score)) => {
                    scores.insert(path, score);
                }
                Err(e) => tracing::warn!("failed to read PageRank score: {}", e),
            }
        }

        Ok(scores)
    }

    /// Get symbol counts per file_id.
    fn symbol_counts_by_file(&self) -> Result<HashMap<i64, usize>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT file_id, count(*) FROM symbols GROUP BY file_id")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, usize>(1)?))
        })?;
        let mut counts = HashMap::new();
        for row in rows {
            match row {
                Ok((id, count)) => {
                    counts.insert(id, count);
                }
                Err(e) => tracing::warn!("failed to read symbol count: {}", e),
            }
        }
        Ok(counts)
    }
}

/// Result of a build/update operation.
#[derive(Debug, Clone, Serialize)]
pub struct BuildResult {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub files_removed: usize,
    pub symbols_added: usize,
    pub refs_added: usize,
}

/// Validate that a relative path from the DB doesn't escape the root via `..` components.
fn is_safe_relative_path(rel: &str) -> bool {
    !Path::new(rel)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Compute SHA-256 hash of content.
fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Walk up from `start` to find the nearest directory containing `.that-tools/`.
pub fn find_tools_root(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if current.join(".that-tools").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Resolve the index database path for a project root.
pub fn index_db_path(root: &Path) -> PathBuf {
    root.join(".that-tools").join("index.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_test_project() -> TempDir {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src").join("main.rs"),
            r#"use crate::config::Config;

fn main() {
    let config = Config::new();
    process(&config);
}

fn process(config: &Config) {
    println!("{:?}", config);
}
"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("src").join("config.rs"),
            r#"pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new() -> Self {
        Config { name: "default".to_string() }
    }
}
"#,
        )
        .unwrap();
        tmp
    }

    #[test]
    fn test_open_in_memory() {
        let index = SymbolIndex::open_in_memory().unwrap();
        let status = index.status(Path::new(".")).unwrap();
        assert_eq!(status.total_files, 0);
        assert_eq!(status.total_symbols, 0);
    }

    #[test]
    fn test_build_index() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        let result = index.build(tmp.path()).unwrap();

        assert_eq!(result.files_indexed, 2);
        assert!(result.symbols_added > 0, "should have indexed symbols");
    }

    #[test]
    fn test_incremental_update() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();

        // First build
        let r1 = index.build(tmp.path()).unwrap();
        assert_eq!(r1.files_indexed, 2);

        // Second build with no changes — should skip everything
        let r2 = index.build(tmp.path()).unwrap();
        assert_eq!(r2.files_indexed, 0);
        assert_eq!(r2.files_skipped, 2);
    }

    #[test]
    fn test_incremental_update_after_change() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();

        index.build(tmp.path()).unwrap();

        // Modify a file
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "fn main() {}\nfn new_func() {}\n",
        )
        .unwrap();

        let r2 = index.build(tmp.path()).unwrap();
        assert_eq!(r2.files_indexed, 1, "should re-index only the changed file");
        assert_eq!(r2.files_skipped, 1);
    }

    #[test]
    fn test_query_references() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        let refs = index.query_references("Config").unwrap();
        // Config is referenced in main.rs
        assert!(!refs.is_empty(), "should find references to Config");
    }

    #[test]
    fn test_index_status() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        let status = index.status(tmp.path()).unwrap();
        assert_eq!(status.total_files, 2);
        assert!(status.total_symbols > 0);
        assert_eq!(status.stale_files, 0);
        assert_eq!(status.schema_version, "1");
    }

    #[test]
    fn test_stale_file_detection() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        // Modify a file without re-indexing
        fs::write(
            tmp.path().join("src").join("main.rs"),
            "fn main() { /* modified */ }\n",
        )
        .unwrap();

        let status = index.status(tmp.path()).unwrap();
        assert_eq!(status.stale_files, 1);
    }

    #[test]
    fn test_file_edges() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        let edges = index.file_edges().unwrap();
        // main.rs references Config from config.rs, so there should be edges
        // (may or may not depending on reference resolution)
        // Just verify the query doesn't fail
        let _ = edges;
    }

    #[test]
    fn test_open_file_index() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join(".that-tools").join("index.db");
        let _index = SymbolIndex::open(&db_path).unwrap();
        assert!(db_path.exists(), "database file should be created");
    }

    #[test]
    fn test_compute_hash() {
        let h1 = compute_hash("hello");
        let h2 = compute_hash("hello");
        let h3 = compute_hash("world");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn test_store_and_get_pagerank() {
        let tmp = setup_test_project();
        let index = SymbolIndex::open_in_memory().unwrap();
        index.build(tmp.path()).unwrap();

        let mut scores = HashMap::new();
        scores.insert("src/main.rs".to_string(), 0.75);
        scores.insert("src/config.rs".to_string(), 0.25);

        index.store_pagerank_scores(&scores).unwrap();
        let retrieved = index.get_pagerank_scores().unwrap();

        assert!((retrieved["src/main.rs"] - 0.75).abs() < 1e-9);
        assert!((retrieved["src/config.rs"] - 0.25).abs() < 1e-9);
    }
}
