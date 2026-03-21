//! SearXNG self-hosted search provider.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;
use serde::Deserialize;

pub struct SearxngProvider {
    client: reqwest::blocking::Client,
    instance_url: String,
}

#[derive(Deserialize)]
struct SearxngResponse {
    results: Option<Vec<SearxngResult>>,
}

#[derive(Deserialize)]
struct SearxngResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
    engine: Option<String>,
}

impl SearxngProvider {
    pub fn new(instance_url: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            client,
            instance_url: instance_url.trim_end_matches('/').to_string(),
        }
    }

    pub fn parse_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
        let resp: SearxngResponse =
            serde_json::from_str(body).map_err(|e| SearchError::Parse(e.to_string()))?;

        let mut results = Vec::new();
        if let Some(items) = resp.results {
            for (i, item) in items.iter().enumerate() {
                let score = item.score.unwrap_or(1.0 - (i as f64 * 0.1).min(0.9));
                results.push(SearchResult {
                    title: item.title.clone().unwrap_or_default(),
                    url: item.url.clone().unwrap_or_default(),
                    snippet: item.content.clone().unwrap_or_default(),
                    source: format!("searxng:{}", item.engine.as_deref().unwrap_or("unknown")),
                    score,
                });
            }
        }
        Ok(results)
    }
}

impl SearchProvider for SearxngProvider {
    fn name(&self) -> &str {
        "searxng"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!("{}/search", self.instance_url);
        let response = self
            .client
            .get(&url)
            .query(&[
                ("q", query.query.as_str()),
                ("format", "json"),
                ("pageno", "1"),
            ])
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if !response.status().is_success() {
            return Err(SearchError::Http(format!(
                "searxng status {}",
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
        false
    }

    fn is_available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response() {
        let json = r#"{
            "results": [
                {"title": "SearXNG Result", "url": "https://example.com", "content": "A result", "score": 0.8, "engine": "google"}
            ]
        }"#;
        let results = SearxngProvider::parse_response(json).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].source.starts_with("searxng:"));
    }

    #[test]
    fn test_parse_empty() {
        let json = r#"{"results": []}"#;
        let results = SearxngProvider::parse_response(json).unwrap();
        assert!(results.is_empty());
    }
}
