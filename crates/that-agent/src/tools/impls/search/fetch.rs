//! URL fetching, HTML-to-markdown extraction, and parallel multi-URL fetch.
//!
//! Four modes:
//! - `inspect`  — DOM structure analysis (default); use the data to write your own extraction script
//! - `scrape`   — DOM structure analysis + auto-executed Python scraper (result in `scraped_content`)
//! - `markdown` — HTML converted to readable markdown
//! - `text`     — plain text stripped of all markup

use super::inspect;
use super::SearchError;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

/// Result of fetching a URL in markdown/text mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    pub url: String,
    pub title: String,
    pub content: String,
    pub content_type: String,
    pub word_count: usize,
}

/// Unified per-URL result returned by `fetch_multi`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchItem {
    pub url: String,
    /// Present in `inspect` mode (always) and `scrape` mode when auto-scraping produced no results.
    /// Tells the agent what to do next with the inspection data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Populated in `inspect` and `scrape` modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection: Option<inspect::PageInspection>,
    /// Populated in `scrape` mode — the auto-executed scraper output (JSON).
    /// If scraping failed the agent should use the inspection data to write its own script.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scraped_content: Option<serde_json::Value>,
    /// Populated in `markdown` and `text` modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<FetchResult>,
}

/// Maximum response body size (10 MB).
const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

/// Human-like user agent used for all fetch requests.
const HUMAN_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Shared HTTP client for fetch requests to maximize connection reuse.
static FETCH_CLIENT: LazyLock<reqwest::blocking::Client> = LazyLock::new(|| {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(HUMAN_UA)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
});

/// Check if a URL targets a private/internal network address (SSRF protection).
fn is_private_url(url: &str) -> bool {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            // Block localhost variants
            if host == "localhost"
                || host == "127.0.0.1"
                || host == "::1"
                || host == "[::1]"
                || host == "0.0.0.0"
            {
                return true;
            }
            // Block common private IP ranges
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                return match ip {
                    std::net::IpAddr::V4(v4) => {
                        v4.is_loopback()
                            || v4.is_private()
                            || v4.is_link_local()
                            || v4.is_broadcast()
                            || v4.is_unspecified()
                            || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64
                        // 100.64.0.0/10
                    }
                    std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
                };
            }
            // Block metadata endpoints
            if host == "metadata.google.internal" || host == "169.254.169.254" {
                return true;
            }
        }
        // Block non-HTTP schemes
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return true;
        }
    }
    false
}

/// Fetch multiple URLs in parallel, returning one `FetchItem` per URL in original order.
///
/// `mode` is one of:
/// - `"inspect"` (default) — DOM structure analysis; use the data to write your own extraction script
/// - `"scrape"` — DOM structure analysis + auto-executed Python scraper
/// - `"markdown"` — HTML converted to markdown
/// - `"text"` — plain text
pub fn fetch_multi(urls: &[String], mode: &str) -> Vec<FetchItem> {
    use std::sync::mpsc;

    let n = urls.len();
    let (tx, rx) = mpsc::channel::<(usize, FetchItem)>();

    for (idx, url) in urls.iter().enumerate() {
        let tx = tx.clone();
        let url = url.clone();
        let mode = mode.to_string();

        std::thread::spawn(move || {
            let item = match mode.as_str() {
                "inspect" | "" => match fetch_raw_html(&url) {
                    Ok(html) => FetchItem {
                        url: url.clone(),
                        next_action: Some(
                            "Page fetched. Read the inspection fields: \
                             content_regions (best containers to target), \
                             repeating_patterns (list/card selectors with child structure), \
                             tag_counts and top_classes (element frequency and samples). \
                             Use this to write your own extraction script."
                                .to_string(),
                        ),
                        error: None,
                        inspection: Some(inspect::inspect_html(&html, &url)),
                        scraped_content: None,
                        content: None,
                    },
                    Err(e) => FetchItem {
                        url: url.clone(),
                        next_action: None,
                        error: Some(e.to_string()),
                        inspection: None,
                        scraped_content: None,
                        content: None,
                    },
                },
                // scrape mode: inspect DOM + auto-execute the generated Python scraper.
                // Returns scraped_content with the parsed results.
                // Falls back gracefully: if scraping fails, inspection data is still available.
                "scrape" => match fetch_raw_html(&url) {
                    Ok(html) => {
                        let inspection = inspect::inspect_html(&html, &url);
                        let scraped = inspect::auto_scrape(
                            &url,
                            &html,
                            &inspection.repeating_patterns,
                            &inspection.content_regions,
                        );
                        let next = if scraped.is_some() {
                            None
                        } else {
                            Some(
                                "Auto-scrape returned no results. \
                                 Use the inspection data to write your own extraction script."
                                    .to_string(),
                            )
                        };
                        FetchItem {
                            url: url.clone(),
                            next_action: next,
                            error: None,
                            inspection: Some(inspection),
                            scraped_content: scraped,
                            content: None,
                        }
                    }
                    Err(e) => FetchItem {
                        url: url.clone(),
                        next_action: None,
                        error: Some(e.to_string()),
                        inspection: None,
                        scraped_content: None,
                        content: None,
                    },
                },
                "markdown" | "text" => match fetch_url(&url, &mode) {
                    Ok(result) => FetchItem {
                        url: url.clone(),
                        next_action: None,
                        error: None,
                        inspection: None,
                        scraped_content: None,
                        content: Some(result),
                    },
                    Err(e) => FetchItem {
                        url: url.clone(),
                        next_action: None,
                        error: Some(e.to_string()),
                        inspection: None,
                        scraped_content: None,
                        content: None,
                    },
                },
                other => FetchItem {
                    url: url.clone(),
                    next_action: None,
                    error: Some(format!(
                        "unknown mode '{}'; use scrape, inspect, markdown, or text",
                        other
                    )),
                    inspection: None,
                    scraped_content: None,
                    content: None,
                },
            };
            let _ = tx.send((idx, item));
        });
    }
    drop(tx);

    // Collect and restore original URL order
    let mut indexed: Vec<(usize, FetchItem)> = Vec::with_capacity(n);
    for _ in 0..n {
        if let Ok(pair) = rx.recv() {
            indexed.push(pair);
        }
    }
    indexed.sort_by_key(|(i, _)| *i);
    indexed.into_iter().map(|(_, item)| item).collect()
}

/// Fetch a URL and return the raw HTML string (used by inspect mode).
fn fetch_raw_html(url: &str) -> Result<String, SearchError> {
    if is_private_url(url) {
        return Err(SearchError::Fetch(
            "URL targets a private or internal address".to_string(),
        ));
    }

    let response = FETCH_CLIENT
        .get(url)
        .send()
        .map_err(|e| SearchError::Fetch(e.to_string()))?;

    if !response.status().is_success() {
        return Err(SearchError::Fetch(format!("status {}", response.status())));
    }

    if let Some(len) = response.content_length() {
        if len as usize > MAX_RESPONSE_BYTES {
            return Err(SearchError::Fetch(format!(
                "response too large: {} bytes",
                len
            )));
        }
    }

    let bytes = response
        .bytes()
        .map_err(|e| SearchError::Fetch(e.to_string()))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SearchError::Fetch(format!(
            "response too large: {} bytes",
            bytes.len()
        )));
    }

    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// Fetch a URL and extract its content in markdown or text mode.
pub fn fetch_url(url: &str, extract_mode: &str) -> Result<FetchResult, SearchError> {
    // SSRF protection: block private/internal URLs
    if is_private_url(url) {
        return Err(SearchError::Fetch(
            "URL targets a private or internal address".to_string(),
        ));
    }

    let response = FETCH_CLIENT
        .get(url)
        .send()
        .map_err(|e| SearchError::Fetch(e.to_string()))?;

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .to_string();

    if !response.status().is_success() {
        return Err(SearchError::Fetch(format!("status {}", response.status())));
    }

    // Check Content-Length header if available to reject oversized responses early
    if let Some(len) = response.content_length() {
        if len as usize > MAX_RESPONSE_BYTES {
            return Err(SearchError::Fetch(format!(
                "response too large: {} bytes (limit: {} bytes)",
                len, MAX_RESPONSE_BYTES
            )));
        }
    }

    // Read body with size limit
    let bytes = response
        .bytes()
        .map_err(|e| SearchError::Fetch(e.to_string()))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(SearchError::Fetch(format!(
            "response too large: {} bytes (limit: {} bytes)",
            bytes.len(),
            MAX_RESPONSE_BYTES
        )));
    }
    let html = String::from_utf8_lossy(&bytes).to_string();

    let (title, content) = match extract_mode {
        "markdown" | "md" => extract_markdown(&html),
        "text" | "raw" => extract_text(&html),
        _ => extract_markdown(&html),
    };

    let word_count = content.split_whitespace().count();

    Ok(FetchResult {
        url: url.to_string(),
        title,
        content,
        content_type,
        word_count,
    })
}

/// Extract content from HTML and convert to markdown.
fn extract_markdown(html: &str) -> (String, String) {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);

    // Extract title
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    // Remove unwanted elements
    let body_sel = Selector::parse("article, main, .content, .post, #content, body")
        .unwrap_or_else(|_| Selector::parse("body").unwrap());

    let content_el = document.select(&body_sel).next();
    let mut md = String::new();

    if let Some(el) = content_el {
        html_to_markdown(&el, &mut md, &document);
    }

    // Clean up excessive whitespace
    let cleaned = md
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    // Remove excessive blank lines (more than 2 in a row)
    let mut result = String::new();
    let mut blank_count = 0;
    for line in cleaned.lines() {
        if line.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                result.push('\n');
            }
        } else {
            blank_count = 0;
            result.push_str(line);
            result.push('\n');
        }
    }

    (title, result.trim().to_string())
}

/// Convert HTML element to markdown recursively.
fn html_to_markdown(element: &scraper::ElementRef, output: &mut String, _doc: &scraper::Html) {
    for child in element.children() {
        match child.value() {
            scraper::node::Node::Text(text) => {
                let t = text.text.trim();
                if !t.is_empty() {
                    output.push_str(t);
                    output.push(' ');
                }
            }
            scraper::node::Node::Element(el) => {
                let tag = el.name();
                // Skip unwanted tags
                if matches!(
                    tag,
                    "script" | "style" | "nav" | "footer" | "aside" | "noscript" | "iframe"
                ) {
                    continue;
                }

                if let Some(child_ref) = scraper::ElementRef::wrap(child) {
                    match tag {
                        "h1" => {
                            output.push_str("\n# ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "h2" => {
                            output.push_str("\n## ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "h3" => {
                            output.push_str("\n### ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "h4" | "h5" | "h6" => {
                            output.push_str("\n#### ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "p" => {
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "br" => output.push('\n'),
                        "a" => {
                            let href = el.attr("href").unwrap_or("");
                            output.push('[');
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("](");
                            output.push_str(href);
                            output.push(')');
                        }
                        "strong" | "b" => {
                            output.push_str("**");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("**");
                        }
                        "em" | "i" => {
                            output.push('_');
                            html_to_markdown(&child_ref, output, _doc);
                            output.push('_');
                        }
                        "code" => {
                            output.push('`');
                            html_to_markdown(&child_ref, output, _doc);
                            output.push('`');
                        }
                        "pre" => {
                            output.push_str("\n```\n");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n```\n\n");
                        }
                        "ul" | "ol" => {
                            output.push('\n');
                            html_to_markdown(&child_ref, output, _doc);
                            output.push('\n');
                        }
                        "li" => {
                            output.push_str("- ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push('\n');
                        }
                        "img" => {
                            let alt = el.attr("alt").unwrap_or("");
                            let src = el.attr("src").unwrap_or("");
                            if !src.is_empty() {
                                output.push_str(&format!("![{}]({})", alt, src));
                            }
                        }
                        "blockquote" => {
                            output.push_str("\n> ");
                            html_to_markdown(&child_ref, output, _doc);
                            output.push_str("\n\n");
                        }
                        "hr" => output.push_str("\n---\n\n"),
                        "table" | "thead" | "tbody" | "tr" | "td" | "th" => {
                            // Basic table support: just extract text
                            html_to_markdown(&child_ref, output, _doc);
                            if matches!(tag, "td" | "th") {
                                output.push_str(" | ");
                            }
                            if tag == "tr" {
                                output.push('\n');
                            }
                        }
                        _ => html_to_markdown(&child_ref, output, _doc),
                    }
                }
            }
            _ => {}
        }
    }
}

/// Extract plain text from HTML.
fn extract_text(html: &str) -> (String, String) {
    use scraper::{Html, Selector};
    let document = Html::parse_document(html);

    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let body = Selector::parse("body")
        .ok()
        .and_then(|sel| document.select(&sel).next())
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();

    let cleaned: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    (title, cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_markdown_basic() {
        let html = "<html><head><title>Test Page</title></head><body><h1>Hello</h1><p>World</p></body></html>";
        let (title, content) = extract_markdown(html);
        assert_eq!(title, "Test Page");
        assert!(content.contains("# Hello"));
        assert!(content.contains("World"));
    }

    #[test]
    fn test_extract_markdown_links() {
        let html = "<html><body><a href=\"https://example.com\">Click here</a></body></html>";
        let (_, content) = extract_markdown(html);
        // The link text may have spacing due to HTML text node handling
        assert!(
            content.contains("Click here"),
            "content should contain link text, got: {}",
            content
        );
        assert!(
            content.contains("https://example.com"),
            "content should contain URL, got: {}",
            content
        );
    }

    #[test]
    fn test_extract_markdown_code() {
        let html = "<html><body><pre><code>fn main() {}</code></pre></body></html>";
        let (_, content) = extract_markdown(html);
        assert!(content.contains("```"));
        assert!(content.contains("fn main()"));
    }

    #[test]
    fn test_extract_markdown_strips_scripts() {
        let html = "<html><body><script>alert('xss')</script><p>Safe content</p></body></html>";
        let (_, content) = extract_markdown(html);
        assert!(!content.contains("alert"));
        assert!(content.contains("Safe content"));
    }

    #[test]
    fn test_ssrf_blocks_localhost() {
        assert!(is_private_url("http://localhost/admin"));
        assert!(is_private_url("http://127.0.0.1/secret"));
        assert!(is_private_url("http://0.0.0.0/"));
        assert!(is_private_url("http://[::1]/"));
        assert!(is_private_url("http://169.254.169.254/latest/meta-data"));
        assert!(is_private_url("http://metadata.google.internal/"));
        assert!(is_private_url("ftp://example.com/file"));
    }

    #[test]
    fn test_ssrf_allows_public() {
        assert!(!is_private_url("https://example.com"));
        assert!(!is_private_url("https://api.github.com/repos"));
        assert!(!is_private_url("http://1.2.3.4/page"));
    }

    #[test]
    fn test_ssrf_blocks_private_ranges() {
        assert!(is_private_url("http://10.0.0.1/"));
        assert!(is_private_url("http://192.168.1.1/"));
        assert!(is_private_url("http://172.16.0.1/"));
    }

    #[test]
    fn test_extract_text() {
        let html =
            "<html><head><title>T</title></head><body><p>Hello</p><p>World</p></body></html>";
        let (title, content) = extract_text(html);
        assert_eq!(title, "T");
        assert!(content.contains("Hello"));
        assert!(content.contains("World"));
    }
}
