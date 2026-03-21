//! Bing search provider (HTML scraping via www.bing.com, no API key needed).
//!
//! Fetches `https://www.bing.com/search?q=...` and parses the standard result
//! list (`li.b_algo`).  Title links carry the actual destination URL directly
//! (no redirect wrapper), and the snippet lives in `.b_caption p`.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;

pub struct BingProvider {
    client: reqwest::blocking::Client,
}

impl Default for BingProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl BingProvider {
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

    /// Parse Bing HTML search results.
    ///
    /// Bing wraps each result in `li.b_algo`.  The title link (`h2 a`) carries
    /// the real destination URL; the snippet is in `.b_caption p` (or the
    /// equivalent lineclamp variants Bing sometimes uses).
    pub fn parse_html_results(html: &str) -> Vec<SearchResult> {
        use scraper::{Html, Selector};
        let document = Html::parse_document(html);
        let mut results = Vec::new();

        let algo_sel = match Selector::parse("li.b_algo") {
            Ok(s) => s,
            Err(_) => return results,
        };
        let title_sel = Selector::parse("h2 a").unwrap_or_else(|_| Selector::parse("a").unwrap());
        let snippet_sel =
            Selector::parse(".b_caption p, p.b_lineclamp4, p.b_lineclamp3, p.b_lineclamp2")
                .unwrap_or_else(|_| Selector::parse("p").unwrap());

        for algo in document.select(&algo_sel) {
            let Some(title_el) = algo.select(&title_sel).next() else {
                continue;
            };

            let href = title_el.value().attr("href").unwrap_or("");
            if href.is_empty() || href.starts_with('#') || href.starts_with('/') {
                continue;
            }
            // Skip Bing-internal tracking/navigation links
            if href.contains("bing.com") || href.contains("microsoft.com") {
                continue;
            }

            let title = title_el.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 3 {
                continue;
            }

            let snippet = algo
                .select(&snippet_sel)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            let score = 1.0 - (results.len() as f64 * 0.1).min(0.9);
            results.push(SearchResult {
                title,
                url: href.to_string(),
                snippet,
                source: "bing".into(),
                score,
            });
        }

        results
    }
}

impl SearchProvider for BingProvider {
    fn name(&self) -> &str {
        "bing"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let encoded: String =
            url::form_urlencoded::byte_serialize(query.query.as_bytes()).collect();
        let url = format!(
            "https://www.bing.com/search?q={}&count={}&setlang=en",
            encoded, query.limit
        );

        let response = self
            .client
            .get(&url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            // Accept cookie consent upfront so Bing serves results directly
            // instead of a cookie-gate page (common on non-US IPs).
            .header(
                "Cookie",
                "SRCHHPGUSR=SRCHLANG=en; _EDGE_S=mkt=en-us; SRCHUID=V=2",
            )
            .header("Referer", "https://www.bing.com/")
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        if response.status().as_u16() == 429 {
            return Err(SearchError::RateLimited {
                provider: "bing".into(),
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
            return Err(SearchError::Http(
                "Bing returned no results (possible bot challenge)".to_string(),
            ));
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
        let results = BingProvider::parse_html_results("<html><body></body></html>");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_html_with_results() {
        let html = r#"
        <html><body>
          <ol id="b_results">
            <li class="b_algo">
              <h2><a href="https://example.com/page1">Example Page 1</a></h2>
              <div class="b_caption"><p>This is the first result snippet</p></div>
            </li>
            <li class="b_algo">
              <h2><a href="https://other.org/page2">Other Page 2</a></h2>
              <div class="b_caption"><p>Second result snippet</p></div>
            </li>
          </ol>
        </body></html>
        "#;
        let results = BingProvider::parse_html_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "bing");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].title, "Example Page 1");
        assert_eq!(results[0].snippet, "This is the first result snippet");
        assert_eq!(results[1].url, "https://other.org/page2");
    }

    #[test]
    fn test_skips_bing_internal_links() {
        let html = r#"
        <html><body>
          <ol id="b_results">
            <li class="b_algo">
              <h2><a href="https://www.bing.com/news">Bing News</a></h2>
              <div class="b_caption"><p>Internal</p></div>
            </li>
            <li class="b_algo">
              <h2><a href="https://real-result.com">Real Result</a></h2>
              <div class="b_caption"><p>Real snippet</p></div>
            </li>
          </ol>
        </body></html>
        "#;
        let results = BingProvider::parse_html_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://real-result.com");
    }

    #[test]
    fn test_provider_metadata() {
        let provider = BingProvider::new();
        assert_eq!(provider.name(), "bing");
        assert!(!provider.requires_api_key());
        assert!(provider.is_available());
    }

    #[test]
    fn test_score_decreases_with_rank() {
        let html = r#"
        <html><body>
          <ol id="b_results">
            <li class="b_algo"><h2><a href="https://a.com">Alpha Result</a></h2><div class="b_caption"><p>snippet</p></div></li>
            <li class="b_algo"><h2><a href="https://b.com">Beta Result</a></h2><div class="b_caption"><p>snippet</p></div></li>
            <li class="b_algo"><h2><a href="https://c.com">Gamma Result</a></h2><div class="b_caption"><p>snippet</p></div></li>
          </ol>
        </body></html>
        "#;
        let results = BingProvider::parse_html_results(html);
        assert_eq!(results.len(), 3);
        assert!(results[0].score > results[1].score);
        assert!(results[1].score > results[2].score);
    }
}
