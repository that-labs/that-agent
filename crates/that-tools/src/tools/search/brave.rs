//! Brave Search API provider.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;
use serde::Deserialize;

pub struct BraveProvider {
    client: reqwest::blocking::Client,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct BraveResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    results: Option<Vec<BraveResult>>,
}

#[derive(Deserialize)]
struct BraveResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
}

impl Default for BraveProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BraveProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("BRAVE_API_KEY")
            .or_else(|_| std::env::var("THAT_SEARCH_BRAVE_KEY"))
            .ok();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self { client, api_key }
    }

    pub fn parse_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
        let resp: BraveResponse =
            serde_json::from_str(body).map_err(|e| SearchError::Parse(e.to_string()))?;

        let mut results = Vec::new();
        if let Some(web) = resp.web {
            if let Some(items) = web.results {
                for (i, item) in items.iter().enumerate() {
                    let score = 1.0 - (i as f64 * 0.1).min(0.9);
                    results.push(SearchResult {
                        title: item.title.clone().unwrap_or_default(),
                        url: item.url.clone().unwrap_or_default(),
                        snippet: item.description.clone().unwrap_or_default(),
                        source: "brave".into(),
                        score,
                    });
                }
            }
        }
        Ok(results)
    }
}

impl SearchProvider for BraveProvider {
    fn name(&self) -> &str {
        "brave"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| SearchError::NoApiKey("brave".into()))?;

        let response = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", api_key)
            .query(&[("q", &query.query), ("count", &query.limit.to_string())])
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if response.status() == 429 {
            return Err(SearchError::RateLimited {
                provider: "brave".into(),
                retry_after: None,
            });
        }

        if !response.status().is_success() {
            return Err(SearchError::Http(format!(
                "brave status {}",
                response.status()
            )));
        }

        let text = response
            .text()
            .map_err(|e| SearchError::Parse(e.to_string()))?;
        let mut results = Self::parse_response(&text)?;
        results.truncate(query.limit);
        Ok(results)
    }

    fn requires_api_key(&self) -> bool {
        true
    }

    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response() {
        let json = r#"{
            "web": {
                "results": [
                    {"title": "Brave Result", "url": "https://brave.com", "description": "A brave result"}
                ]
            }
        }"#;
        let results = BraveProvider::parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "brave");
    }

    #[test]
    fn test_parse_empty_response() {
        let json = r#"{"web": {"results": []}}"#;
        let results = BraveProvider::parse_response(json).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_no_web() {
        let json = r#"{}"#;
        let results = BraveProvider::parse_response(json).unwrap();
        assert!(results.is_empty());
    }
}
