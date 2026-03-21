//! Search provider trait and shared types.

use serde::{Deserialize, Serialize};

/// A single search result from any provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: String,
    pub score: f64,
}

/// Input to a search operation.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub query: String,
    pub limit: usize,
}

/// Aggregated search output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutput {
    pub query: String,
    pub engine: String,
    pub results: Vec<SearchResult>,
    pub total_results: usize,
    pub cached: bool,
}

/// Trait that every search backend implements.
#[allow(dead_code)]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, super::SearchError>;
    fn requires_api_key(&self) -> bool;
    fn is_available(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_result_serialization() {
        let result = SearchResult {
            title: "Test".into(),
            url: "https://example.com".into(),
            snippet: "A test result".into(),
            source: "test".into(),
            score: 0.9,
        };
        let json = serde_json::to_string(&result).unwrap();
        let _: SearchResult = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_search_output_serialization() {
        let output = SearchOutput {
            query: "test".into(),
            engine: "test".into(),
            results: vec![],
            total_results: 0,
            cached: false,
        };
        let json = serde_json::to_string(&output).unwrap();
        let _: SearchOutput = serde_json::from_str(&json).unwrap();
    }
}
