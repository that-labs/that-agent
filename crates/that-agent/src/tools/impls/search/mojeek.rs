//! Mojeek search provider (HTML scraping via mojeek.com, no API key needed).
//!
//! Mojeek is an independent crawler-based engine that serves plain HTML without
//! JavaScript challenges, making it highly reliable for programmatic access.
//! Results are in `ul.results-standard li`, title+URL in `a.title`, snippet in `p.s`.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;

pub struct MojeekProvider {
    client: reqwest::blocking::Client,
}

impl Default for MojeekProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MojeekProvider {
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

    /// Parse Mojeek HTML search results.
    ///
    /// Each result is an `li` inside `ul.results-standard`.
    /// - `a.title` → title text and destination URL (href is the real URL, no redirect wrapper)
    /// - `p.s`     → snippet text
    pub fn parse_html_results(html: &str) -> Vec<SearchResult> {
        use scraper::{Html, Selector};
        let document = Html::parse_document(html);
        let mut results = Vec::new();

        let item_sel = match Selector::parse("ul.results-standard li") {
            Ok(s) => s,
            Err(_) => return results,
        };
        let title_sel =
            Selector::parse("a.title").unwrap_or_else(|_| Selector::parse("a").unwrap());
        let snippet_sel = Selector::parse("p.s").unwrap_or_else(|_| Selector::parse("p").unwrap());

        for item in document.select(&item_sel) {
            let Some(title_el) = item.select(&title_sel).next() else {
                continue;
            };

            let href = title_el.value().attr("href").unwrap_or("");
            if href.is_empty() || href.starts_with('#') || href.starts_with('/') {
                continue;
            }
            if href.contains("mojeek.com") {
                continue;
            }
            if !href.starts_with("http") {
                continue;
            }

            let title = title_el.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 3 {
                continue;
            }

            let snippet = item
                .select(&snippet_sel)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            let score = 1.0 - (results.len() as f64 * 0.1).min(0.9);
            results.push(SearchResult {
                title,
                url: href.to_string(),
                snippet,
                source: "mojeek".into(),
                score,
            });
        }

        results
    }
}

impl SearchProvider for MojeekProvider {
    fn name(&self) -> &str {
        "mojeek"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let encoded: String =
            url::form_urlencoded::byte_serialize(query.query.as_bytes()).collect();
        let url = format!("https://www.mojeek.com/search?q={}&si=10&fmt=1", encoded);

        let response = self
            .client
            .get(&url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if response.status().as_u16() == 429 {
            return Err(SearchError::RateLimited {
                provider: "mojeek".into(),
                retry_after: None,
            });
        }

        if !response.status().is_success() {
            return Err(SearchError::Http(format!("status {}", response.status())));
        }

        let html = response
            .text()
            .map_err(|e| SearchError::Parse(e.to_string()))?;

        let mut results = Self::parse_html_results(&html);
        if results.is_empty() {
            return Err(SearchError::Http("Mojeek returned no results".to_string()));
        }
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
    fn test_parse_empty_html() {
        let results = MojeekProvider::parse_html_results("<html><body></body></html>");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_html_with_results() {
        let html = r#"
        <html><body>
          <ul class="results-standard">
            <li>
              <a class="title" href="https://example.com/page1">Example Page One</a>
              <p class="s">First result snippet here</p>
            </li>
            <li>
              <a class="title" href="https://other.org/page2">Other Page Two</a>
              <p class="s">Second result snippet here</p>
            </li>
          </ul>
        </body></html>
        "#;
        let results = MojeekProvider::parse_html_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "mojeek");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].title, "Example Page One");
        assert_eq!(results[0].snippet, "First result snippet here");
        assert_eq!(results[1].url, "https://other.org/page2");
    }

    #[test]
    fn test_skips_mojeek_internal_links() {
        let html = r#"
        <html><body>
          <ul class="results-standard">
            <li>
              <a class="title" href="https://www.mojeek.com/about">About Mojeek</a>
              <p class="s">internal</p>
            </li>
            <li>
              <a class="title" href="https://real-result.com">Real Result Page</a>
              <p class="s">Real snippet text</p>
            </li>
          </ul>
        </body></html>
        "#;
        let results = MojeekProvider::parse_html_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://real-result.com");
    }

    #[test]
    fn test_provider_metadata() {
        let provider = MojeekProvider::new();
        assert_eq!(provider.name(), "mojeek");
        assert!(!provider.requires_api_key());
        assert!(provider.is_available());
    }

    #[test]
    fn test_score_decreases_with_rank() {
        let html = r#"
        <html><body>
          <ul class="results-standard">
            <li><a class="title" href="https://a.com">Alpha Result Page</a><p class="s">s</p></li>
            <li><a class="title" href="https://b.com">Beta Result Page</a><p class="s">s</p></li>
            <li><a class="title" href="https://c.com">Gamma Result Page</a><p class="s">s</p></li>
          </ul>
        </body></html>
        "#;
        let results = MojeekProvider::parse_html_results(html);
        assert_eq!(results.len(), 3);
        assert!(results[0].score > results[1].score);
        assert!(results[1].score > results[2].score);
    }
}
