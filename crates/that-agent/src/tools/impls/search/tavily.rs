//! Tavily API search provider.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;
use serde::Deserialize;

pub struct TavilyProvider {
    client: reqwest::blocking::Client,
    api_key: Option<String>,
}

#[derive(Deserialize)]
struct TavilyResponse {
    results: Option<Vec<TavilyResult>>,
    answer: Option<String>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: Option<String>,
    url: Option<String>,
    content: Option<String>,
    score: Option<f64>,
}

impl Default for TavilyProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl TavilyProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("TAVILY_API_KEY")
            .or_else(|_| std::env::var("THAT_SEARCH_TAVILY_KEY"))
            .ok();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self { client, api_key }
    }

    pub fn parse_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
        let resp: TavilyResponse =
            serde_json::from_str(body).map_err(|e| SearchError::Parse(e.to_string()))?;

        let mut results = Vec::new();

        // Include Tavily's AI answer as first result if present
        if let Some(answer) = resp.answer {
            if !answer.is_empty() {
                results.push(SearchResult {
                    title: "AI Answer".into(),
                    url: String::new(),
                    snippet: answer,
                    source: "tavily".into(),
                    score: 1.0,
                });
            }
        }

        if let Some(items) = resp.results {
            for item in items {
                results.push(SearchResult {
                    title: item.title.unwrap_or_default(),
                    url: item.url.unwrap_or_default(),
                    snippet: item.content.unwrap_or_default(),
                    source: "tavily".into(),
                    score: item.score.unwrap_or(0.5),
                });
            }
        }

        Ok(results)
    }
}

impl SearchProvider for TavilyProvider {
    fn name(&self) -> &str {
        "tavily"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| SearchError::NoApiKey("tavily".into()))?;

        let body = serde_json::json!({
            "api_key": api_key,
            "query": query.query,
            "max_results": query.limit,
            "include_answer": true,
        });

        let response = self
            .client
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if response.status() == 429 {
            return Err(SearchError::RateLimited {
                provider: "tavily".into(),
                retry_after: None,
            });
        }

        if !response.status().is_success() {
            return Err(SearchError::Http(format!(
                "tavily status {}",
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
    fn test_parse_response_with_results() {
        let json = r#"{
            "results": [
                {"title": "Result 1", "url": "https://example.com/1", "content": "Snippet 1", "score": 0.9},
                {"title": "Result 2", "url": "https://example.com/2", "content": "Snippet 2", "score": 0.7}
            ]
        }"#;
        let results = TavilyProvider::parse_response(json).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result 1");
        assert_eq!(results[0].source, "tavily");
    }

    #[test]
    fn test_parse_response_with_answer() {
        let json = r#"{
            "answer": "The answer is 42",
            "results": [{"title": "R1", "url": "https://example.com", "content": "S1", "score": 0.8}]
        }"#;
        let results = TavilyProvider::parse_response(json).unwrap();
        assert_eq!(results[0].title, "AI Answer");
        assert!(results[0].snippet.contains("42"));
    }

    #[test]
    fn test_parse_empty_response() {
        let json = r#"{"results": []}"#;
        let results = TavilyProvider::parse_response(json).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_provider_requires_key() {
        let provider = TavilyProvider::new();
        assert!(provider.requires_api_key());
        assert_eq!(provider.name(), "tavily");
    }
}
