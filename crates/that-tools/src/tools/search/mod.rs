//! Federated web search with provider selection and caching.
//!
//! Provides two main operations:
//! - `search()`: Query multiple search providers with fallback
//! - `fetch()`: Retrieve and extract content from a URL

pub mod aggregator;
pub mod bing;
pub mod brave;
pub mod cache;
pub mod duckduckgo;
pub mod fetch;
pub mod inspect;
pub mod mojeek;
pub mod provider;
pub mod rate_limit;
pub mod searxng;
pub mod tavily;
pub mod yahoo;

use crate::config::SearchConfig;
use crate::output::{self, BudgetedOutput};
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum SearchError {
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("rate limited by {provider}, retry after {retry_after:?}")]
    RateLimited {
        provider: String,
        retry_after: Option<Duration>,
    },
    #[error("parse error: {0}")]
    Parse(String),
    #[error("no API key configured for {0}")]
    NoApiKey(String),
    #[error("all providers failed")]
    AllProvidersFailed,
    #[error("fetch error: {0}")]
    Fetch(String),
}

/// Execute a federated search.
pub fn search(
    query: &str,
    engine: Option<&str>,
    limit: usize,
    no_cache: bool,
    config: &SearchConfig,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, SearchError> {
    let mut agg = aggregator::SearchAggregator::new(config);
    let result = agg.search(query, engine, limit, no_cache)?;
    Ok(output::emit_json(&result, max_tokens))
}

/// Fetch one or more URLs in parallel.
///
/// Returns a JSON array (one item per URL, original order). Each item has:
/// - `url` — the URL fetched
/// - `next_action` — guidance on what to do with the inspection data
/// - `inspection` — (inspect/scrape mode) DOM structure: tag counts, top classes, repeating patterns, content regions, IDs
/// - `scraped_content` — (scrape mode) auto-executed Python scraper output
/// - `content` — (markdown/text mode) extracted page content
/// - `error` — set if the URL could not be fetched
///
/// `mode` values:
/// - `"inspect"` (default) — DOM structure analysis; use the data to write your own extraction script
/// - `"scrape"` — DOM structure analysis + auto-executed Python scraper (result in `scraped_content`)
/// - `"markdown"` — HTML converted to readable markdown
/// - `"text"` — plain text, all markup stripped
pub fn fetch(
    urls: &[String],
    mode: &str,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, SearchError> {
    let results = fetch::fetch_multi(urls, mode);
    Ok(output::emit_json(&results, max_tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_error_display() {
        let err = SearchError::NoApiKey("tavily".into());
        assert!(err.to_string().contains("tavily"));
    }

    #[test]
    fn test_search_error_variants() {
        let _ = SearchError::Http("timeout".into());
        let _ = SearchError::RateLimited {
            provider: "test".into(),
            retry_after: None,
        };
        let _ = SearchError::AllProvidersFailed;
        let _ = SearchError::Fetch("connection refused".into());
    }
}
