//! Provider selection, fallback chain, deduplication, and ranking.

use super::cache::SearchCache;
use super::provider::{SearchOutput, SearchProvider, SearchQuery, SearchResult};
use super::rate_limit::RateLimiter;
use super::SearchError;
use crate::tools::config::SearchConfig;
use std::collections::HashSet;
use std::sync::mpsc;
use std::time::Duration;

pub struct SearchAggregator {
    providers: Vec<Box<dyn SearchProvider>>,
    cache: SearchCache,
    rate_limiter: RateLimiter,
    config: SearchConfig,
}

impl SearchAggregator {
    pub fn new(config: &SearchConfig) -> Self {
        let providers: Vec<Box<dyn SearchProvider>> = vec![
            Box::new(super::tavily::TavilyProvider::new()),
            Box::new(super::brave::BraveProvider::new()),
            Box::new(super::duckduckgo::DuckDuckGoProvider::new()),
            Box::new(super::bing::BingProvider::new()),
            Box::new(super::yahoo::YahooProvider::new()),
            Box::new(super::mojeek::MojeekProvider::new()),
            Box::new(super::searxng::SearxngProvider::new(&config.searxng_url)),
        ];

        let cache_path = if config.persistent_cache {
            let p = dirs::data_local_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
                .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                .join("that-tools")
                .join("search_cache.db");
            Some(p)
        } else {
            None
        };
        let cache = SearchCache::new(cache_path.as_deref(), config.cache_ttl_minutes);

        Self {
            providers,
            cache,
            rate_limiter: RateLimiter::new(),
            config: config.clone(),
        }
    }

    pub fn search(
        &mut self,
        query_str: &str,
        engine: Option<&str>,
        limit: usize,
        no_cache: bool,
    ) -> Result<SearchOutput, SearchError> {
        let engine_name = engine.unwrap_or(&self.config.primary_engine);

        // Check cache
        if !no_cache {
            if let Some(cached) = self.cache.get(query_str, engine_name) {
                return Ok(SearchOutput {
                    query: query_str.to_string(),
                    engine: engine_name.to_string(),
                    results: cached.into_iter().take(limit).collect(),
                    total_results: 0,
                    cached: true,
                });
            }
        }

        let query = SearchQuery {
            query: query_str.to_string(),
            limit: limit.min(self.config.max_results_per_engine),
        };

        // Build provider order
        let provider_order = self.select_providers(engine);

        // Optional hedged requests mode: launch staggered provider calls in parallel
        // and return the first successful response.
        if self.config.hedged_requests && engine.is_none() && provider_order.len() > 1 {
            if let Ok(output) = self.search_hedged(query_str, &query, &provider_order, limit) {
                return Ok(output);
            }
        }

        let mut last_error = None;
        for provider_name in &provider_order {
            // Check rate limit
            if self.rate_limiter.check(provider_name).is_err() {
                continue;
            }

            if let Some(provider) = self.providers.iter().find(|p| p.name() == *provider_name) {
                if !provider.is_available() {
                    continue;
                }

                self.rate_limiter.record_request(provider_name);
                match provider.search(&query) {
                    Ok(mut results) => {
                        self.rate_limiter.record_success(provider_name);
                        deduplicate(&mut results);
                        rank(&mut results);
                        results.truncate(limit);

                        // Cache results
                        self.cache.put(query_str, provider_name, &results);

                        let total = results.len();
                        return Ok(SearchOutput {
                            query: query_str.to_string(),
                            engine: provider_name.to_string(),
                            results,
                            total_results: total,
                            cached: false,
                        });
                    }
                    Err(e) => {
                        tracing::warn!("provider {} failed: {}", provider_name, e);
                        if let SearchError::RateLimited { retry_after, .. } = &e {
                            self.rate_limiter
                                .record_failure(provider_name, *retry_after);
                        } else {
                            self.rate_limiter.record_failure(provider_name, None);
                        }
                        last_error = Some(e);
                    }
                }
            }
        }

        Err(last_error.unwrap_or(SearchError::AllProvidersFailed))
    }

    fn search_hedged(
        &mut self,
        query_str: &str,
        query: &SearchQuery,
        provider_order: &[String],
        limit: usize,
    ) -> Result<SearchOutput, SearchError> {
        let (tx, rx) = mpsc::channel::<(String, Result<Vec<SearchResult>, SearchError>)>();
        let mut launched = 0usize;

        for (idx, provider_name) in provider_order.iter().enumerate() {
            if self.rate_limiter.check(provider_name).is_err() {
                continue;
            }

            let Some(provider) = Self::build_provider(provider_name, &self.config) else {
                continue;
            };
            if !provider.is_available() {
                continue;
            }

            self.rate_limiter.record_request(provider_name);
            let tx = tx.clone();
            let provider_name = provider_name.clone();
            let query = query.clone();
            let stagger = Duration::from_millis((idx as u64) * 150);

            std::thread::spawn(move || {
                if !stagger.is_zero() {
                    std::thread::sleep(stagger);
                }
                let result = provider.search(&query);
                let _ = tx.send((provider_name, result));
            });
            launched += 1;
        }
        drop(tx);

        if launched == 0 {
            return Err(SearchError::AllProvidersFailed);
        }

        let mut last_error = None;
        for _ in 0..launched {
            let Ok((provider_name, result)) = rx.recv() else {
                break;
            };
            match result {
                Ok(mut results) => {
                    self.rate_limiter.record_success(&provider_name);
                    deduplicate(&mut results);
                    rank(&mut results);
                    results.truncate(limit);

                    self.cache.put(query_str, &provider_name, &results);
                    let total = results.len();
                    return Ok(SearchOutput {
                        query: query_str.to_string(),
                        engine: provider_name,
                        results,
                        total_results: total,
                        cached: false,
                    });
                }
                Err(e) => {
                    tracing::warn!("provider {} failed: {}", provider_name, e);
                    if let SearchError::RateLimited { retry_after, .. } = &e {
                        self.rate_limiter
                            .record_failure(&provider_name, *retry_after);
                    } else {
                        self.rate_limiter.record_failure(&provider_name, None);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(SearchError::AllProvidersFailed))
    }

    fn build_provider(name: &str, config: &SearchConfig) -> Option<Box<dyn SearchProvider>> {
        match name {
            "tavily" => Some(Box::new(super::tavily::TavilyProvider::new())),
            "brave" => Some(Box::new(super::brave::BraveProvider::new())),
            "duckduckgo" => Some(Box::new(super::duckduckgo::DuckDuckGoProvider::new())),
            "bing" => Some(Box::new(super::bing::BingProvider::new())),
            "yahoo" => Some(Box::new(super::yahoo::YahooProvider::new())),
            "mojeek" => Some(Box::new(super::mojeek::MojeekProvider::new())),
            "searxng" => Some(Box::new(super::searxng::SearxngProvider::new(
                &config.searxng_url,
            ))),
            _ => None,
        }
    }

    fn select_providers(&self, explicit_engine: Option<&str>) -> Vec<String> {
        if let Some(engine) = explicit_engine {
            vec![engine.to_string()]
        } else {
            let mut order = vec![self.config.primary_engine.clone()];
            for fallback in &self.config.fallback_chain {
                if !order.contains(fallback) {
                    order.push(fallback.clone());
                }
            }
            order
        }
    }
}

/// Deduplicate results by URL.
fn deduplicate(results: &mut Vec<SearchResult>) {
    let mut seen = HashSet::new();
    results.retain(|r| {
        let canonical = canonicalize_url(&r.url);
        seen.insert(canonical)
    });
}

/// Canonicalize URL for deduplication.
fn canonicalize_url(url: &str) -> String {
    url.trim_end_matches('/')
        .replace("http://", "https://")
        .to_lowercase()
}

/// Sort results by score descending.
fn rank(results: &mut [SearchResult]) {
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(url: &str, score: f64) -> SearchResult {
        SearchResult {
            title: "T".into(),
            url: url.into(),
            snippet: "S".into(),
            source: "test".into(),
            score,
        }
    }

    #[test]
    fn test_deduplicate() {
        let mut results = vec![
            make_result("https://example.com/page", 0.9),
            make_result("https://example.com/page/", 0.8),
            make_result("https://other.com", 0.7),
        ];
        deduplicate(&mut results);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_rank() {
        let mut results = vec![
            make_result("https://a.com", 0.3),
            make_result("https://b.com", 0.9),
            make_result("https://c.com", 0.6),
        ];
        rank(&mut results);
        assert_eq!(results[0].url, "https://b.com");
        assert_eq!(results[2].url, "https://a.com");
    }

    #[test]
    fn test_canonicalize_url() {
        assert_eq!(
            canonicalize_url("http://Example.com/page/"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_select_providers_explicit() {
        let config = SearchConfig::default();
        let agg = SearchAggregator::new(&config);
        let order = agg.select_providers(Some("brave"));
        assert_eq!(order, vec!["brave"]);
    }

    #[test]
    fn test_select_providers_default() {
        let config = SearchConfig::default();
        let agg = SearchAggregator::new(&config);
        let order = agg.select_providers(None);
        assert_eq!(order[0], config.primary_engine);
    }
}
