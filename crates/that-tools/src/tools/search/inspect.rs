//! DOM structure inspector for agent-driven scraping.
//!
//! Analyzes a raw HTML page and returns:
//! - Tag frequency map (what elements exist)
//! - Top CSS classes with element counts and sample text
//! - Repeating patterns (same class >= 3 times = likely content items)
//! - Content regions (high text-density sections)
//! - Notable IDs sorted by word count

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Full structural analysis of a fetched page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageInspection {
    pub url: String,
    pub title: String,
    pub word_count: usize,
    /// Tag frequency (sorted alphabetically). Excludes script/style/meta.
    pub tag_counts: BTreeMap<String, usize>,
    /// Top 20 CSS classes by occurrence, with a text sample from the first match.
    pub top_classes: Vec<ClassSummary>,
    /// CSS classes that repeat >= 3 times with substantial text — likely content containers.
    pub repeating_patterns: Vec<RepeatingPattern>,
    /// Semantic regions and high-word-count ID elements.
    pub content_regions: Vec<ContentRegion>,
    /// Notable IDs sorted by word density (most content-rich first).
    pub notable_ids: Vec<IdSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassSummary {
    pub class: String,
    pub count: usize,
    /// First 100 chars of text from the first matching element.
    pub sample: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepeatingPattern {
    /// CSS selector (e.g. ".result-item")
    pub selector: String,
    pub count: usize,
    /// Comma-separated direct child tag+class signatures (e.g. "h2.title, p.snippet, a.link")
    pub child_structure: String,
    pub sample_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentRegion {
    pub selector: String,
    pub word_count: usize,
    pub sample: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdSummary {
    pub id: String,
    pub tag: String,
    pub word_count: usize,
}

#[derive(Default)]
struct ClassData {
    count: usize,
    sample: String,
    child_sig: String,
}

/// Inspect raw HTML and return a structural analysis for agent-driven scraping.
pub fn inspect_html(html: &str, url: &str) -> PageInspection {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);

    // Extract <title>
    let title = Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|el| el.text().collect::<String>().trim().to_string())
        .unwrap_or_default();

    let mut tag_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut class_map: HashMap<String, ClassData> = HashMap::new();
    let mut id_list: Vec<IdSummary> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();

    let all_sel = Selector::parse("*").unwrap();
    for el in doc.select(&all_sel) {
        let tag = el.value().name().to_string();
        // Noisy/invisible tags — skip for counts
        if matches!(
            tag.as_str(),
            "script" | "style" | "meta" | "link" | "noscript" | "head" | "html"
        ) {
            continue;
        }
        *tag_counts.entry(tag.clone()).or_insert(0) += 1;

        // Class tracking
        for class in el.value().classes() {
            let entry = class_map.entry(class.to_string()).or_default();
            entry.count += 1;
            if entry.sample.is_empty() {
                let raw: String = el.text().collect::<Vec<_>>().join(" ");
                let trimmed: String = raw
                    .split_whitespace()
                    .take(30)
                    .collect::<Vec<_>>()
                    .join(" ");
                if trimmed.split_whitespace().count() > 2 {
                    entry.sample = trimmed;
                    entry.child_sig = child_signature(el);
                }
            }
        }

        // ID tracking
        if let Some(id) = el.value().attr("id") {
            if !id.is_empty() && seen_ids.insert(id.to_string()) {
                let text: String = el.text().collect::<Vec<_>>().join(" ");
                let wc = text.split_whitespace().count();
                id_list.push(IdSummary {
                    id: id.to_string(),
                    tag: tag.clone(),
                    word_count: wc,
                });
            }
        }
    }

    // Total body word count
    let word_count = Selector::parse("body")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|el| el.text().collect::<String>().split_whitespace().count())
        .unwrap_or(0);

    // Sort classes by count descending
    let mut class_vec: Vec<(String, ClassData)> = class_map.into_iter().collect();
    class_vec.sort_by(|a, b| b.1.count.cmp(&a.1.count));

    // Top 20 classes for the overview
    let top_classes: Vec<ClassSummary> = class_vec
        .iter()
        .take(20)
        .map(|(name, data)| ClassSummary {
            class: name.clone(),
            count: data.count,
            sample: data.sample.chars().take(100).collect(),
        })
        .collect();

    // Repeating patterns: classes with 3–80 occurrences, at least 4 words of sample text,
    // not a utility/framework class, not a navigation/chrome element, not a sub-element
    // (date, byline, icon, caption), and not template/placeholder text (e.g. [hour]:[minute]).
    let repeating_patterns: Vec<RepeatingPattern> = class_vec
        .iter()
        .filter(|(name, data)| {
            data.count >= 3
                && data.count <= 80
                && data.sample.split_whitespace().count() > 3
                && !data.sample.contains('[') // template placeholder text
                && !is_utility_class(name)
                && !is_nav_class(name)
                && !is_sub_element_class(name)
        })
        .take(6)
        .map(|(name, data)| {
            let sel = format!(".{}", name);
            RepeatingPattern {
                selector: sel,
                count: data.count,
                child_structure: data.child_sig.clone(),
                sample_text: data.sample.chars().take(100).collect(),
            }
        })
        .collect();

    // Content regions: semantic elements + high-word-count IDs
    let mut content_regions: Vec<ContentRegion> = Vec::new();
    let semantic_candidates = [
        "main",
        "article",
        "#content",
        "#main",
        ".content",
        ".main",
        ".post-content",
        ".article-body",
        ".entry-content",
    ];
    for sel_str in &semantic_candidates {
        if content_regions.len() >= 4 {
            break;
        }
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                let text: String = el.text().collect::<Vec<_>>().join(" ");
                let wc = text.split_whitespace().count();
                if wc > 30 {
                    let sample: String = text
                        .split_whitespace()
                        .take(25)
                        .collect::<Vec<_>>()
                        .join(" ");
                    content_regions.push(ContentRegion {
                        selector: sel_str.to_string(),
                        word_count: wc,
                        sample,
                    });
                }
            }
        }
    }
    // Supplement with high-word-count IDs
    let mut ids_by_wc = id_list.clone();
    ids_by_wc.sort_by(|a, b| b.word_count.cmp(&a.word_count));
    for id_entry in ids_by_wc.iter().take(5) {
        if id_entry.word_count > 50 && content_regions.len() < 5 {
            let sel_str = format!("#{}", id_entry.id);
            // Inner block ensures the Selector borrow ends before sel_str is moved.
            let maybe_sample: Option<String> = {
                let tmp = sel_str.clone();
                Selector::parse(&tmp).ok().and_then(|sel| {
                    doc.select(&sel).next().map(|el| {
                        el.text()
                            .collect::<Vec<_>>()
                            .join(" ")
                            .split_whitespace()
                            .take(25)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                })
            };
            if let Some(sample) = maybe_sample {
                content_regions.push(ContentRegion {
                    selector: sel_str,
                    word_count: id_entry.word_count,
                    sample,
                });
            }
        }
    }
    content_regions.sort_by(|a, b| b.word_count.cmp(&a.word_count));
    content_regions.dedup_by(|a, b| a.selector == b.selector);
    content_regions.truncate(5);

    // Notable IDs: top 10 by word count
    id_list.sort_by(|a, b| b.word_count.cmp(&a.word_count));
    id_list.truncate(10);

    PageInspection {
        url: url.to_string(),
        title,
        word_count,
        tag_counts,
        top_classes,
        repeating_patterns,
        content_regions,
        notable_ids: id_list,
    }
}

/// Returns true for utility/framework classes that carry no semantic meaning
/// and would make poor scraper targets.
///
/// Covers: Tailwind utilities, syntax-highlighting tokens, icon fonts, CSS reset helpers.
fn is_utility_class(name: &str) -> bool {
    // Tailwind: classes with brackets like `h-[60px]`, `text-[14px]`
    if name.contains('[') || name.contains(']') {
        return true;
    }

    // Any class containing `:` is a Tailwind responsive/state/variant class
    // e.g. `sm:`, `md:`, `hover:`, `dark:`, `last:`, `first:`, `focus:`, `group-hover:`
    if name.contains(':') {
        return true;
    }

    // Tailwind-style: many hyphens, short segments (e.g. `border-dashed-gray-700`)
    let hyphen_count = name.chars().filter(|&c| c == '-').count();
    if hyphen_count >= 3 {
        return true;
    }

    // Common single-purpose utility prefixes (Tailwind + Bootstrap + common CSS frameworks)
    let utility_prefixes = [
        "text-",
        "bg-",
        "px-",
        "py-",
        "p-",
        "mx-",
        "my-",
        "m-",
        "w-",
        "h-",
        "min-",
        "max-",
        "flex-",
        "grid-",
        "gap-",
        "space-",
        "border-",
        "rounded-",
        "shadow-",
        "opacity-",
        "z-",
        "top-",
        "bottom-",
        "left-",
        "right-",
        "inset-",
        "overflow-",
        "cursor-",
        "font-",
        "leading-",
        "tracking-",
        "align-",
        "justify-",
        "col-",
        "row-",
        "order-",
        "sr-",
        "items-",
        "self-",
        "place-",
        "duration-",
        "transition-",
        "ease-",
        "delay-",
        "animate-",
        "scale-",
        "rotate-",
        "translate-",
        "skew-",
        "origin-",
        "shrink-",
        "grow-",
        "basis-",
        "pointer-",
        "select-",
        "resize-",
        "appearance-",
        "outline-",
        "ring-",
        "divide-",
        "stroke-",
        "fill-",
        "object-",
        "decoration-",
        "underline-",
        "list-",
        "indent-",
        "whitespace-",
        "break-",
        "line-clamp-",
        "aspect-",
        "columns-",
        // Bootstrap
        "btn-",
        "col-",
        "row-",
        "d-",
        "py-",
        "px-",
        "mt-",
        "mb-",
        "ms-",
        "me-",
        "fw-",
        "fs-",
        "lh-",
        "text-",
        "bg-",
        "border-",
    ];
    if utility_prefixes.iter().any(|p| name.starts_with(p)) {
        return true;
    }

    // Single-word Tailwind layout/display utilities (no hyphen)
    let tailwind_singles: &[&str] = &[
        "flex",
        "grid",
        "block",
        "inline",
        "hidden",
        "contents",
        "relative",
        "absolute",
        "fixed",
        "sticky",
        "static",
        "grow",
        "shrink",
        "truncate",
        "underline",
        "overline",
        "italic",
        "uppercase",
        "lowercase",
        "capitalize",
        "normal",
        "visible",
        "invisible",
        "collapse",
        "isolate",
        "container",
        "clearfix",
        "prose",
    ];
    if tailwind_singles.contains(&name) {
        return true;
    }

    // Syntax highlighting token names (PrismJS, highlight.js, rouge, etc.)
    let syntax_tokens = [
        "token",
        "keyword",
        "operator",
        "punctuation",
        "string",
        "number",
        "boolean",
        "function",
        "class-name",
        "regex",
        "comment",
        "doctype",
        "entity",
        "attr-",
        "tag",
        "atrule",
        "selector",
        "property",
        "unit",
        "hexcode",
        "language-",
        "hljs-",
        "highlight-",
        "rouge-",
        "giallo",
        "chroma",
    ];
    if syntax_tokens.iter().any(|t| name.contains(t)) {
        return true;
    }

    // Icon font classes (FontAwesome, Material Icons, etc.)
    let icon_prefixes = [
        "fa-",
        "fa ",
        "icon-",
        "mi-",
        "material-",
        "bi-",
        "ri-",
        "ph-",
    ];
    if icon_prefixes.iter().any(|p| name.starts_with(p)) || name == "fa" || name == "icon" {
        return true;
    }

    false
}

/// Returns true for classes that are sub-elements of cards, not the card container itself.
/// These carry data (dates, bylines, icons, captions) but are poor scraper targets.
fn is_sub_element_class(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Whole-word or suffix matches only — avoid filtering semantic containers like "rich-text"
    let sub_suffixes = [
        "-date",
        "-byline",
        "-author",
        "-timestamp",
        "-caption",
        "-icon",
        "-icons",
        "-tag",
        "-label",
        "-badge",
        "-meta",
        "-credit",
        "-source",
        "-category",
        "-time",
    ];
    if sub_suffixes.iter().any(|s| lower.ends_with(s)) {
        return true;
    }
    // Prefix matches (e.g. "date-", "byline-")
    let sub_prefixes = [
        "date-",
        "byline-",
        "author-",
        "caption-",
        "icon-",
        "timestamp-",
    ];
    if sub_prefixes.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    false
}

/// Returns true for classes that are navigation/chrome elements, not content containers.
fn is_nav_class(name: &str) -> bool {
    let lower = name.to_lowercase();
    let nav_terms = [
        "nav",
        "navigation",
        "menu",
        "hamburger",
        "header",
        "footer",
        "sidebar",
        "breadcrumb",
        "toolbar",
        "topbar",
        "navbar",
        "menuitem",
        "dropdown",
        "offcanvas",
        "megamenu",
        "subnav",
    ];
    nav_terms.iter().any(|t| lower.contains(t))
}

/// Build the direct child tag+class signature of an element (max 6 unique child types).
fn child_signature(el: scraper::ElementRef) -> String {
    let mut parts: Vec<String> = Vec::new();
    for child_node in el.children() {
        if let Some(child_el) = scraper::ElementRef::wrap(child_node) {
            let tag = child_el.value().name().to_string();
            if matches!(tag.as_str(), "script" | "style") {
                continue;
            }
            let classes: Vec<&str> = child_el.value().classes().take(2).collect();
            let part = if classes.is_empty() {
                tag
            } else {
                format!("{}.{}", tag, classes.join("."))
            };
            if !parts.contains(&part) {
                parts.push(part);
            }
            if parts.len() >= 6 {
                break;
            }
        }
    }
    parts.join(", ")
}

/// Returns true if the tag part of a child_structure entry is an anchor element.
///
/// Child structure entries look like "a", "a.class", "a[href]", "h2.title", etc.
/// We must check for `a` as a whole tag (not just the letter 'a' inside words like "auto").
fn child_is_anchor(part: &str) -> bool {
    part == "a" || part.starts_with("a.") || part.starts_with("a[") || part.starts_with("a#")
}

/// Returns true if `child_structure` contains an anchor (`<a>`) as a direct child.
fn has_anchor_child(child_structure: &str) -> bool {
    if child_structure.is_empty() {
        return false;
    }
    child_structure.split(", ").any(child_is_anchor)
}

/// Build the article/list extraction block (Python, indented 4 spaces for try body).
///
/// Shared by both `build_python_script_from_file` and `build_python_script`.
fn build_extraction_block(patterns: &[RepeatingPattern], regions: &[ContentRegion]) -> String {
    let mut out = String::new();

    // Decide whether to use list/card mode (patterns) or article mode (regions).
    //
    // Use patterns only when at least one has an actual <a> link child — that
    // indicates card containers (search results, article listings, etc.).
    // If the page is primarily prose (high-word-count content regions) and none
    // of the detected patterns has link children, they're nav/UI elements — use
    // article mode instead.
    let has_link_pattern = patterns
        .iter()
        .any(|p| has_anchor_child(&p.child_structure));
    let regions_total_wc: usize = regions.iter().map(|r| r.word_count).sum();
    let use_patterns = !patterns.is_empty() && (has_link_pattern || regions_total_wc < 200);

    if use_patterns {
        let p = patterns
            .iter()
            .find(|p| has_anchor_child(&p.child_structure))
            .unwrap_or(&patterns[0]);
        out.push_str("    results = []\n");
        out.push_str(&format!("    for item in soup.select('{}'):\n", p.selector));

        let children: Vec<&str> = if p.child_structure.is_empty() {
            vec![]
        } else {
            p.child_structure.split(", ").collect()
        };
        // Only match genuine anchor tags (not "article", "aside", etc.)
        let link_sel = children
            .iter()
            .find(|c| child_is_anchor(c))
            .copied()
            .unwrap_or("a[href]");
        let heading_sel = children
            .iter()
            .find(|c| {
                c.starts_with("h1")
                    || c.starts_with("h2")
                    || c.starts_with("h3")
                    || c.starts_with("h4")
            })
            .copied();
        let text_sel = children
            .iter()
            .find(|c| {
                let l = c.to_lowercase();
                c.starts_with('p')
                    || l.contains("snippet")
                    || l.contains("desc")
                    || l.contains("text")
                    || l.contains("content")
            })
            .copied()
            .unwrap_or("p");

        out.push_str("        if item.name == 'a':\n");
        out.push_str("            t = item.get_text(strip=True)\n");
        out.push_str("            u = item.get('href', '')\n");
        out.push_str(
            "            if t or u: results.append({\"title\": t, \"url\": u, \"text\": \"\"})\n",
        );
        out.push_str("            continue\n");

        if let Some(h) = heading_sel {
            out.push_str(&format!(
                "        title_el = item.select_one('{h} a') or item.select_one('{h}')\n"
            ));
        } else {
            out.push_str(&format!(
                "        title_el = item.select_one('{link_sel}')\n"
            ));
        }
        out.push_str(&format!(
            "        link_el  = item.select_one('{link_sel}')\n"
        ));
        out.push_str(&format!(
            "        text_el  = item.select_one('{text_sel}')\n"
        ));
        out.push_str("        t = title_el.get_text(strip=True) if title_el else \"\"\n");
        out.push_str("        u = link_el.get('href', '') if link_el else \"\"\n");
        out.push_str("        x = text_el.get_text(strip=True) if text_el else \"\"\n");
        out.push_str(
            "        if t or u: results.append({\"title\": t, \"url\": u, \"text\": x})\n",
        );
        out.push_str("    print(json.dumps(results))\n");
    } else if !regions.is_empty() {
        // Try selectors from most-specific (smallest) to most-general (largest) so that
        // tightly-scoped article containers are preferred over full-page wrappers that
        // include navigation, sidebars, and other chrome.
        let selectors: Vec<&str> = regions.iter().rev().map(|r| r.selector.as_str()).collect();
        out.push_str("    container = None\n");
        for sel in &selectors {
            out.push_str(&format!(
                "    container = container or soup.select_one('{}')\n",
                sel
            ));
        }
        out.push_str(
            "    container = container or soup.find('article') or soup.find('main') or soup.body\n",
        );
        out.push_str("    sections = []\n");
        out.push_str("    current = {\"heading\": \"\", \"paragraphs\": []}\n");
        out.push_str("    sections.append(current)\n");
        // h5/h6 included for documentation sites that use them as section headings
        out.push_str("    for el in (container.find_all(['h1','h2','h3','h4','h5','h6','p','pre']) if container else []):\n");
        out.push_str("        if el.name in ('h1','h2','h3','h4','h5','h6'):\n");
        out.push_str(
            "            if not current['heading'].strip() and not current['paragraphs']:\n",
        );
        out.push_str("                current['heading'] = el.get_text(strip=True)\n");
        out.push_str("            else:\n");
        out.push_str("                current = {\"heading\": el.get_text(strip=True), \"paragraphs\": []}\n");
        out.push_str("                sections.append(current)\n");
        out.push_str("        elif el.name in ('p', 'pre'):\n");
        out.push_str("            t = el.get_text(strip=True)\n");
        out.push_str("            if len(t) > 20: current[\"paragraphs\"].append(t)\n");
        out.push_str(
            "    sections = [s for s in sections if s['heading'].strip() or s['paragraphs']]\n",
        );
        // Fallback: if structured extraction yielded nothing, dump plain text
        // (handles SPAs where content is in <span> not <p>)
        out.push_str("    if not sections or all(not s['paragraphs'] for s in sections):\n");
        out.push_str(
            "        raw = container.get_text(separator='\\n', strip=True) if container else ''\n",
        );
        out.push_str(
            "        lines = [l.strip() for l in raw.split('\\n') if len(l.strip()) > 20]\n",
        );
        out.push_str("        print(json.dumps({\"text\": '\\n'.join(lines[:300])}))\n");
        out.push_str("    else:\n");
        out.push_str("        print(json.dumps(sections))\n");
    } else {
        out.push_str("    text = soup.get_text(separator=' ', strip=True)\n");
        out.push_str("    print(json.dumps({\"text\": text[:8000]}))\n");
    }

    out
}

/// Generate a Python script with the HTML embedded as base64.
///
/// Avoids re-fetching and avoids temp files entirely — the HTML fetched by Rust
/// is encoded inline in the script. This is safe for parallel invocations.
/// Outputs JSON to stdout. Wraps everything in try/except.
pub fn build_python_script_embedded(
    html: &str,
    patterns: &[RepeatingPattern],
    regions: &[ContentRegion],
) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(html.as_bytes());

    let mut out = String::new();
    out.push_str("import json, sys, base64\n");
    out.push_str("try:\n");
    // Import inside try so an ImportError is caught and reported as JSON,
    // giving run_python() a clean signal (error key present → return None).
    out.push_str("    from bs4 import BeautifulSoup\n");
    out.push_str(&format!(
        "    _html = base64.b64decode({:?}).decode('utf-8', 'replace')\n",
        b64
    ));
    out.push_str("    soup = BeautifulSoup(_html, 'html.parser')\n\n");
    out.push_str(&build_extraction_block(patterns, regions));
    out.push_str("except Exception as e:\n");
    out.push_str("    print(json.dumps({\"error\": str(e)}))\n");
    out.push_str("    sys.exit(1)\n");
    out
}

/// Generate the executable Python script body that re-fetches the URL.
/// Used as a last-resort fallback in auto_scrape when the embedded script fails.
pub fn build_python_script(
    url: &str,
    patterns: &[RepeatingPattern],
    regions: &[ContentRegion],
) -> String {
    const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
        AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

    let mut out = String::new();
    out.push_str("import json, sys\n");
    out.push_str("try:\n");
    out.push_str("    import requests\n");
    out.push_str("    from bs4 import BeautifulSoup\n");
    out.push_str(&format!("    UA = \"{}\"\n", UA));
    out.push_str(&format!(
        "    html = requests.get(\"{}\", headers={{\"User-Agent\": UA}}, timeout=15).text\n",
        url
    ));
    out.push_str("    soup = BeautifulSoup(html, 'html.parser')\n\n");
    out.push_str(&build_extraction_block(patterns, regions));
    out.push_str("except Exception as e:\n");
    out.push_str("    print(json.dumps({\"error\": str(e)}))\n");
    out.push_str("    sys.exit(1)\n");
    out
}

/// Execute the auto-generated Python scraper for a page.
///
/// The HTML fetched by Rust is base64-encoded and embedded directly in the
/// Python script — no temp files, no re-fetch, no race conditions. Safe for
/// parallel invocation.
///
/// Tries `python3 -` (stdin) first since bs4 is often pre-installed.
/// Falls back to `uv run --with beautifulsoup4 python3 -` if that fails.
///
/// Returns the parsed JSON output on success, or `None` on hard failure.
pub fn auto_scrape(
    url: &str,
    html: &str,
    patterns: &[RepeatingPattern],
    regions: &[ContentRegion],
) -> Option<serde_json::Value> {
    let script = build_python_script_embedded(html, patterns, regions);

    // Try python3 first (fast, no uv overhead — bs4 is usually pre-installed)
    let result = run_python(&script, &["python3", "-"]);
    if result.is_some() {
        return result;
    }

    // Fall back to uv run (installs bs4 on demand, safe for parallel calls)
    let result = run_python(
        &script,
        &["uv", "run", "--with", "beautifulsoup4", "python3", "-"],
    );
    if result.is_some() {
        return result;
    }

    // Last resort: re-fetch the URL with requests (slower, may return different HTML)
    let fallback = build_python_script(url, patterns, regions);
    let r = run_python(&fallback, &["python3", "-"]);
    if r.is_some() {
        return r;
    }
    run_python(
        &fallback,
        &[
            "uv",
            "run",
            "--with",
            "requests,beautifulsoup4",
            "python3",
            "-",
        ],
    )
}

fn run_python(script: &str, args: &[&str]) -> Option<serde_json::Value> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // Write stdin in a separate thread so we can concurrently drain stdout.
    // Without this, a large script + any stdout output could deadlock:
    //   main thread blocked writing stdin → child blocked writing stdout → deadlock.
    // Using a thread: main thread drains stdout via wait_with_output() while
    // the write thread feeds stdin, so both pipes stay unblocked.
    // This also ensures wait_with_output() is always called (no zombie processes).
    let script_bytes = script.as_bytes().to_vec();
    let mut stdin_handle = child.stdin.take()?;
    let write_thread = std::thread::spawn(move || {
        let result = stdin_handle.write_all(&script_bytes);
        drop(stdin_handle); // explicit EOF
        result
    });

    let out = child.wait_with_output().ok()?;
    let _ = write_thread.join(); // ignore write errors (EPIPE if child exited early is normal)

    if !out.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Accept JSON value; if it's an error object return None
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if v.get("error").is_some() {
        return None;
    }
    // Reject empty arrays/objects
    match &v {
        serde_json::Value::Array(a) if a.is_empty() => None,
        _ => Some(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inspect_basic() {
        let html = r#"<html><head><title>Test Page</title></head><body>
            <div class="result">
                <h2 class="result-title"><a href="https://example.com">Title One</a></h2>
                <p class="result-snippet">Snippet for result one</p>
            </div>
            <div class="result">
                <h2 class="result-title"><a href="https://example2.com">Title Two</a></h2>
                <p class="result-snippet">Snippet for result two</p>
            </div>
            <div class="result">
                <h2 class="result-title"><a href="https://example3.com">Title Three</a></h2>
                <p class="result-snippet">Snippet for result three</p>
            </div>
        </body></html>"#;

        let inspection = inspect_html(html, "https://test.com");
        assert_eq!(inspection.title, "Test Page");
        assert!(inspection.word_count > 0);

        // Should detect .result as repeating (3 times)
        let result_pattern = inspection
            .repeating_patterns
            .iter()
            .find(|p| p.selector == ".result");
        assert!(
            result_pattern.is_some(),
            "should detect .result as repeating"
        );
        let p = result_pattern.unwrap();
        assert_eq!(p.count, 3);
    }

    #[test]
    fn test_inspect_empty_page() {
        let inspection = inspect_html("<html><body></body></html>", "https://test.com");
        assert_eq!(inspection.word_count, 0);
        assert!(inspection.repeating_patterns.is_empty());
    }

    #[test]
    fn test_inspect_finds_ids() {
        let html = r#"<html><body>
            <div id="main-content">
                <p>Word one two three four five six seven eight nine ten</p>
            </div>
            <div id="sidebar">Short text</div>
        </body></html>"#;

        let inspection = inspect_html(html, "https://test.com");
        assert!(inspection
            .notable_ids
            .iter()
            .any(|i| i.id == "main-content"));
    }

    #[test]
    fn test_child_signature() {
        let html = r##"<html><body>
            <div class="card">
                <h2 class="card-title">Title</h2>
                <p class="card-body">Body text</p>
                <a href="#top">Link</a>
            </div>
        </body></html>"##;

        let doc = scraper::Html::parse_document(html);
        let sel = scraper::Selector::parse(".card").unwrap();
        let el = doc.select(&sel).next().unwrap();
        let sig = child_signature(el);
        assert!(sig.contains("h2"), "should see h2 child");
        assert!(sig.contains("p"), "should see p child");
        assert!(sig.contains('a'), "should see a child");
    }
}
