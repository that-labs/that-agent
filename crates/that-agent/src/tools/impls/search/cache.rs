//! Dual-layer search cache: in-memory LRU + SQLite persistent.

use super::provider::SearchResult;
use moka::sync::Cache as MokaCache;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

pub struct SearchCache {
    memory: MokaCache<String, Vec<SearchResult>>,
    db: Option<Connection>,
    ttl_minutes: u64,
}

impl SearchCache {
    pub fn new(db_path: Option<&Path>, ttl_minutes: u64) -> Self {
        let memory = MokaCache::builder()
            .max_capacity(1000)
            .time_to_live(Duration::from_secs(ttl_minutes * 60))
            .build();

        let db = db_path.and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            Connection::open(p).ok().inspect(|conn| {
                let _ = conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS search_cache (
                        query_hash TEXT PRIMARY KEY,
                        query TEXT NOT NULL,
                        engine TEXT NOT NULL,
                        results_json TEXT NOT NULL,
                        created_at INTEGER NOT NULL,
                        ttl_minutes INTEGER NOT NULL
                    )",
                );
            })
        });

        Self {
            memory,
            db,
            ttl_minutes,
        }
    }

    pub fn get(&self, query: &str, engine: &str) -> Option<Vec<SearchResult>> {
        let key = cache_key(query, engine);

        // Check memory first
        if let Some(results) = self.memory.get(&key) {
            return Some(results);
        }

        // Check SQLite
        if let Some(ref conn) = self.db {
            if let Ok(results) = self.get_from_db(conn, &key) {
                // Promote to memory cache
                self.memory.insert(key, results.clone());
                return Some(results);
            }
        }

        None
    }

    pub fn put(&self, query: &str, engine: &str, results: &[SearchResult]) {
        let key = cache_key(query, engine);
        self.memory.insert(key.clone(), results.to_vec());

        if let Some(ref conn) = self.db {
            let _ = self.put_to_db(conn, &key, query, engine, results);
        }
    }

    fn get_from_db(
        &self,
        conn: &Connection,
        key: &str,
    ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error>> {
        let (json, created_at, ttl): (String, i64, i64) = conn.query_row(
            "SELECT results_json, created_at, ttl_minutes FROM search_cache WHERE query_hash = ?1",
            params![key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        // Check TTL
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if now - created_at > ttl * 60 {
            let _ = conn.execute(
                "DELETE FROM search_cache WHERE query_hash = ?1",
                params![key],
            );
            return Err("expired".into());
        }

        let results: Vec<SearchResult> = serde_json::from_str(&json)?;
        Ok(results)
    }

    fn put_to_db(
        &self,
        conn: &Connection,
        key: &str,
        query: &str,
        engine: &str,
        results: &[SearchResult],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(results)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        conn.execute(
            "INSERT OR REPLACE INTO search_cache (query_hash, query, engine, results_json, created_at, ttl_minutes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![key, query, engine, json, now, self.ttl_minutes as i64],
        )?;
        Ok(())
    }
}

fn cache_key(query: &str, engine: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}", engine, query.to_lowercase().trim()));
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(title: &str) -> SearchResult {
        SearchResult {
            title: title.into(),
            url: "https://example.com".into(),
            snippet: "test".into(),
            source: "test".into(),
            score: 1.0,
        }
    }

    #[test]
    fn test_memory_cache_put_get() {
        let cache = SearchCache::new(None, 60);
        let results = vec![make_result("test")];
        cache.put("query", "engine", &results);
        let cached = cache.get("query", "engine");
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().len(), 1);
    }

    #[test]
    fn test_cache_miss() {
        let cache = SearchCache::new(None, 60);
        assert!(cache.get("missing", "engine").is_none());
    }

    #[test]
    fn test_cache_key_normalization() {
        let k1 = cache_key("Hello World", "engine");
        let k2 = cache_key("hello world", "engine");
        assert_eq!(k1, k2);
    }

    #[test]
    fn test_sqlite_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("cache.db");
        let cache = SearchCache::new(Some(&db_path), 60);
        let results = vec![make_result("sqlite test")];
        cache.put("sqlite query", "test", &results);

        // Create new cache instance to test persistence
        let cache2 = SearchCache::new(Some(&db_path), 60);
        let cached = cache2.get("sqlite query", "test");
        assert!(cached.is_some());
    }
}
