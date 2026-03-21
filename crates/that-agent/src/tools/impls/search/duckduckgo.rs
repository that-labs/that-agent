//! DuckDuckGo search provider (HTML scraping, no API key needed).
//!
//! Tries two endpoints in sequence:
//! 1. `https://lite.duckduckgo.com/lite/` — plain-text endpoint, bot-friendly,
//!    results in `td.result-link a` (title + URL) and `td.result-snippet` (snippet).
//! 2. `https://html.duckduckgo.com/html/` (POST) — richer HTML fallback.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;

pub struct DuckDuckGoProvider {
    client: reqwest::blocking::Client,
}

impl Default for DuckDuckGoProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DuckDuckGoProvider {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self { client }
    }

    // ------------------------------------------------------------------
    // Lite endpoint (primary)
    // ------------------------------------------------------------------

    /// Parse results from the DDG lite endpoint (`lite.duckduckgo.com/lite/`).
    ///
    /// The page is a simple table:
    /// - `td.result-link a`    → title text and real destination URL (href)
    /// - `td.result-snippet`   → snippet text (same positional order)
    pub fn parse_lite_results(html: &str) -> Vec<SearchResult> {
        use scraper::{Html, Selector};
        let document = Html::parse_document(html);

        let link_sel = match Selector::parse("td.result-link a") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let snippet_sel = match Selector::parse("td.result-snippet") {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        let links: Vec<_> = document.select(&link_sel).collect();
        let snippets: Vec<_> = document.select(&snippet_sel).collect();

        let mut results = Vec::new();
        for (i, link) in links.iter().enumerate() {
            let href = link.value().attr("href").unwrap_or("");
            if href.is_empty() || href.starts_with('#') || href.starts_with('/') {
                continue;
            }
            if href.contains("duckduckgo.com") {
                continue;
            }

            let title = link.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 3 {
                continue;
            }

            let snippet = snippets
                .get(i)
                .map(|s| s.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            let score = 1.0 - (results.len() as f64 * 0.1).min(0.9);
            results.push(SearchResult {
                title,
                url: href.to_string(),
                snippet,
                source: "duckduckgo".into(),
                score,
            });
        }
        results
    }

    fn search_lite(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let encoded: String =
            url::form_urlencoded::byte_serialize(query.query.as_bytes()).collect();

        // The lite endpoint accepts both GET and POST; GET is simpler and equally reliable.
        let response = self
            .client
            .get(format!(
                "https://lite.duckduckgo.com/lite/?q={}&kl=us-en",
                encoded
            ))
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if !response.status().is_success() {
            return Err(SearchError::Http(format!(
                "DDG lite status {}",
                response.status()
            )));
        }

        let html = response
            .text()
            .map_err(|e| SearchError::Parse(e.to_string()))?;

        let results = Self::parse_lite_results(&html);
        if results.is_empty() {
            return Err(SearchError::Http(
                "DDG lite returned no results".to_string(),
            ));
        }
        Ok(results)
    }

    // ------------------------------------------------------------------
    // HTML endpoint (fallback)
    // ------------------------------------------------------------------

    /// Parse results from DuckDuckGo HTML response.
    ///
    /// The html.duckduckgo.com/html/ endpoint uses:
    /// - `a.result__a` for result title (text) and URL (href)
    /// - `a.result__snippet` for the result snippet text
    pub fn parse_html_results(html: &str) -> Vec<SearchResult> {
        use scraper::{Html, Selector};
        let document = Html::parse_document(html);
        let mut results = Vec::new();

        let title_sel =
            Selector::parse("a.result__a").unwrap_or_else(|_| Selector::parse("a").unwrap());
        let snippet_sel =
            Selector::parse("a.result__snippet").unwrap_or_else(|_| Selector::parse("a").unwrap());

        // Collect snippets indexed by URL for pairing with title links
        let mut snippet_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for el in document.select(&snippet_sel) {
            let href = el.value().attr("href").unwrap_or("").to_string();
            let text = el.text().collect::<String>().trim().to_string();
            if !href.is_empty() && !text.is_empty() {
                snippet_map.insert(href, text);
            }
        }

        for link in document.select(&title_sel) {
            let href = link.value().attr("href").unwrap_or("");
            if href.is_empty() {
                continue;
            }
            // Skip DuckDuckGo internal links
            if href.contains("duckduckgo.com") || href.starts_with('#') || href.starts_with('/') {
                continue;
            }

            let title = link.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 3 {
                continue;
            }

            let snippet = snippet_map.get(href).cloned().unwrap_or_default();
            let score = 1.0 - (results.len() as f64 * 0.1).min(0.9);

            results.push(SearchResult {
                title,
                url: href.to_string(),
                snippet,
                source: "duckduckgo".into(),
                score,
            });
        }

        results
    }

    fn search_html(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let encoded_query: String =
            url::form_urlencoded::byte_serialize(query.query.as_bytes()).collect();
        // kl=wt-wt = no region, kp=-1 = safe-search off, ia=web = force web results.
        let body = format!("q={}&kl=wt-wt&kp=-1&ia=web", encoded_query);

        let response = self
            .client
            .post("https://html.duckduckgo.com/html/")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Origin", "https://html.duckduckgo.com")
            .header("Referer", "https://html.duckduckgo.com/")
            .body(body)
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if !response.status().is_success() {
            return Err(SearchError::Http(format!("status {}", response.status())));
        }

        let html = response
            .text()
            .map_err(|e| SearchError::Parse(e.to_string()))?;

        let results = Self::parse_html_results(&html);
        if results.is_empty() {
            return Err(SearchError::Http(
                "DuckDuckGo returned no results (possible bot challenge)".to_string(),
            ));
        }
        Ok(results)
    }
}

impl SearchProvider for DuckDuckGoProvider {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        // Try the bot-friendly lite endpoint first; fall back to the full HTML endpoint.
        let mut results = match self.search_lite(query) {
            Ok(r) => r,
            Err(lite_err) => {
                tracing::debug!("DDG lite failed ({}), trying html endpoint", lite_err);
                self.search_html(query)?
            }
        };
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

    // --- lite endpoint parser ---

    #[test]
    fn test_parse_lite_empty() {
        let results = DuckDuckGoProvider::parse_lite_results("<html><body></body></html>");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_lite_with_results() {
        let html = r#"
        <html><body>
          <table>
            <tr><td class="result-link"><a href="https://example.com/page1">Example Page One</a></td></tr>
            <tr><td class="result-snippet">First result snippet text</td></tr>
            <tr><td class="result-link"><a href="https://other.org/page2">Other Page Two</a></td></tr>
            <tr><td class="result-snippet">Second result snippet text</td></tr>
          </table>
        </body></html>
        "#;
        let results = DuckDuckGoProvider::parse_lite_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "duckduckgo");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].title, "Example Page One");
        assert_eq!(results[0].snippet, "First result snippet text");
        assert_eq!(results[1].url, "https://other.org/page2");
    }

    #[test]
    fn test_parse_lite_skips_ddg_internal() {
        let html = r#"
        <html><body><table>
          <tr><td class="result-link"><a href="https://duckduckgo.com/settings">Settings</a></td></tr>
          <tr><td class="result-snippet">internal</td></tr>
          <tr><td class="result-link"><a href="https://real-result.com">Real Result Page</a></td></tr>
          <tr><td class="result-snippet">real snippet</td></tr>
        </table></body></html>
        "#;
        let results = DuckDuckGoProvider::parse_lite_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://real-result.com");
    }

    // --- html endpoint parser (legacy) ---

    #[test]
    fn test_parse_empty_html() {
        let results = DuckDuckGoProvider::parse_html_results("<html><body></body></html>");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_html_with_results() {
        let html = r#"
        <html><body>
            <a class="result__a" href="https://example.com/page1">Example Page 1</a>
            <a class="result__snippet" href="https://example.com/page1">This is the first result snippet</a>
            <a class="result__a" href="https://example.com/page2">Example Page 2</a>
            <a class="result__snippet" href="https://example.com/page2">Second result snippet</a>
        </body></html>
        "#;
        let results = DuckDuckGoProvider::parse_html_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "duckduckgo");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].title, "Example Page 1");
        assert_eq!(results[0].snippet, "This is the first result snippet");
    }

    #[test]
    fn test_skips_internal_links() {
        let html = r#"
        <html><body>
        <a class="result__a" href="https://duckduckgo.com/about">About DDG</a>
        <a class="result__a" href="https://example.com">Real Result</a>
        </body></html>
        "#;
        let results = DuckDuckGoProvider::parse_html_results(html);
        for r in &results {
            assert!(!r.url.contains("duckduckgo.com"));
        }
    }

    #[test]
    fn test_provider_metadata() {
        let provider = DuckDuckGoProvider::new();
        assert_eq!(provider.name(), "duckduckgo");
        assert!(!provider.requires_api_key());
        assert!(provider.is_available());
    }
}
