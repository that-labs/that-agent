//! Yahoo search provider (HTML scraping via search.yahoo.com, no API key needed).
//!
//! Yahoo result links go through a redirect wrapper on `r.search.yahoo.com`.
//! The actual destination URL is embedded as a percent-encoded segment `/RU=<url>/`
//! in that redirect URL, which `extract_real_url` decodes without any extra deps.

use super::provider::{SearchProvider, SearchQuery, SearchResult};
use super::SearchError;

pub struct YahooProvider {
    client: reqwest::blocking::Client,
}

impl Default for YahooProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl YahooProvider {
    pub fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .user_agent(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                 AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            )
            // Yahoo normally issues 307 redirects to add session parameters — follow them.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self { client }
    }

    /// Extract the real destination URL from a Yahoo redirect wrapper.
    ///
    /// Yahoo wraps result links as:
    /// `https://r.search.yahoo.com/_ylt=.../RV=2/.../RU=https%3A%2F%2Factual.com%2Fpage/RK=.../`
    ///
    /// The real URL lives in the `/RU=<percent-encoded-url>/` path segment.
    pub fn extract_real_url(href: &str) -> String {
        const MARKER: &str = "/RU=";
        if let Some(pos) = href.find(MARKER) {
            let after = &href[pos + MARKER.len()..];
            // The segment ends at the next '/' that belongs to the redirect path
            // (not one that's part of the encoded URL itself — those would be %2F)
            let end = after.find('/').unwrap_or(after.len());
            let encoded = &after[..end];
            let decoded = percent_decode(encoded);
            if decoded.starts_with("http://") || decoded.starts_with("https://") {
                return decoded;
            }
        }
        href.to_string()
    }

    /// Parse Yahoo HTML search results.
    ///
    /// Yahoo wraps each organic result in a `div.algo` (or `div[class~="algo"]`).
    /// The title anchor (`h3.title a`, or variations) may carry a redirect URL;
    /// `extract_real_url` unwraps it.  The snippet lives in `div.compText` or `p`.
    pub fn parse_html_results(html: &str) -> Vec<SearchResult> {
        use scraper::{Html, Selector};
        let document = Html::parse_document(html);
        let mut results = Vec::new();

        // Try a range of known Yahoo result container selectors
        let containers: &[&str] = &["div.algo", "div.Sr", "li[class*='first']"];

        let container_sel = containers
            .iter()
            .find_map(|s| Selector::parse(s).ok())
            .unwrap_or_else(|| Selector::parse("div").unwrap());

        let title_sel = Selector::parse("h3.title a, h3 a.ac-algo, h3 a")
            .unwrap_or_else(|_| Selector::parse("a").unwrap());
        let snippet_sel = Selector::parse("div.compText, div.compText p, p.fz-ms, span.fc-falcon")
            .unwrap_or_else(|_| Selector::parse("p").unwrap());

        for container in document.select(&container_sel) {
            let Some(title_el) = container.select(&title_sel).next() else {
                continue;
            };

            let raw_href = title_el.value().attr("href").unwrap_or("");
            if raw_href.is_empty() {
                continue;
            }

            let url = Self::extract_real_url(raw_href);

            // Skip yahoo internal or empty hrefs
            if url.contains("yahoo.com") || url.starts_with('#') || url.starts_with('/') {
                continue;
            }
            if !url.starts_with("http") {
                continue;
            }

            let title = title_el.text().collect::<String>().trim().to_string();
            if title.is_empty() || title.len() < 3 {
                continue;
            }

            let snippet = container
                .select(&snippet_sel)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .unwrap_or_default();

            let score = 1.0 - (results.len() as f64 * 0.1).min(0.9);
            results.push(SearchResult {
                title,
                url,
                snippet,
                source: "yahoo".into(),
                score,
            });
        }

        results
    }
}

impl SearchProvider for YahooProvider {
    fn name(&self) -> &str {
        "yahoo"
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>, SearchError> {
        let encoded: String =
            url::form_urlencoded::byte_serialize(query.query.as_bytes()).collect();
        let url = format!(
            "https://search.yahoo.com/search?p={}&n={}&ei=UTF-8",
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
            .send()
            .map_err(|e| SearchError::Http(e.to_string()))?;

        let status = response.status().as_u16();
        if status == 429 {
            return Err(SearchError::RateLimited {
                provider: "yahoo".into(),
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
                "Yahoo returned no results (possible bot challenge)".to_string(),
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

/// Minimal percent-decode for ASCII URLs embedded in Yahoo redirect paths.
/// Only handles `%XX` sequences; `+` is left as-is (not form-data).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(char::from(hi * 16 + lo));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_real_url_from_redirect() {
        let redirect = "https://r.search.yahoo.com/_ylt=Awr/RV=2/RO=10/RU=https%3A%2F%2Fexample.com%2Fpage/RK=2/RS=/";
        let real = YahooProvider::extract_real_url(redirect);
        assert_eq!(real, "https://example.com/page");
    }

    #[test]
    fn test_extract_real_url_passthrough() {
        let direct = "https://example.com/page";
        assert_eq!(YahooProvider::extract_real_url(direct), direct);
    }

    #[test]
    fn test_parse_empty_html() {
        let results = YahooProvider::parse_html_results("<html><body></body></html>");
        assert!(results.is_empty());
    }

    #[test]
    fn test_parse_html_with_results() {
        let html = r#"
        <html><body>
          <div id="web">
            <ol>
              <li>
                <div class="algo" data-pos="1">
                  <h3 class="title">
                    <a href="https://r.search.yahoo.com/_ylt=Awr/RV=2/RO=10/RU=https%3A%2F%2Fexample.com%2Fpage1/RK=2/RS=/">
                      Example Page 1
                    </a>
                  </h3>
                  <div class="compText"><p>First result snippet</p></div>
                </div>
              </li>
              <li>
                <div class="algo" data-pos="2">
                  <h3 class="title">
                    <a href="https://r.search.yahoo.com/_ylt=Awr/RV=2/RO=10/RU=https%3A%2F%2Fother.org%2Fpage2/RK=2/RS=/">
                      Other Page 2
                    </a>
                  </h3>
                  <div class="compText"><p>Second result snippet</p></div>
                </div>
              </li>
            </ol>
          </div>
        </body></html>
        "#;
        let results = YahooProvider::parse_html_results(html);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].source, "yahoo");
        assert_eq!(results[0].url, "https://example.com/page1");
        assert_eq!(results[0].title.trim(), "Example Page 1");
        assert_eq!(results[1].url, "https://other.org/page2");
    }

    #[test]
    fn test_skips_yahoo_internal_links() {
        let html = r#"
        <html><body>
          <div class="algo">
            <h3 class="title">
              <a href="https://help.yahoo.com/kb">Yahoo Help</a>
            </h3>
            <div class="compText"><p>Internal</p></div>
          </div>
          <div class="algo">
            <h3 class="title">
              <a href="https://real-result.com">Real Result</a>
            </h3>
            <div class="compText"><p>Real snippet</p></div>
          </div>
        </body></html>
        "#;
        let results = YahooProvider::parse_html_results(html);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "https://real-result.com");
    }

    #[test]
    fn test_provider_metadata() {
        let provider = YahooProvider::new();
        assert_eq!(provider.name(), "yahoo");
        assert!(!provider.requires_api_key());
        assert!(provider.is_available());
    }

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com"),
            "https://example.com"
        );
        assert_eq!(percent_decode("no-encoding"), "no-encoding");
    }
}
