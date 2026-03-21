//! Search utilities for the memory system.
//!
//! Provides query preprocessing for FTS5 BM25 matching, plus
//! tokenization and Jaccard similarity for near-duplicate detection.

/// Common English stop words that match too broadly in FTS5.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "at", "be", "by", "do", "does", "for", "from", "has", "he", "how",
    "i", "in", "is", "it", "its", "me", "my", "of", "or", "our", "that", "the", "to", "was", "we",
    "what", "when", "where", "which", "who", "will", "with",
];

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.binary_search(&word).is_ok()
}

/// Preprocess a query string for FTS5 matching.
///
/// Handles:
/// - Stripping non-alphanumeric characters
/// - Filtering out stop words
/// - Appending `*` for prefix matching (e.g. "argo" → "argo*" matches "ArgoCD")
/// - Joining remaining terms with OR
#[allow(dead_code)]
pub fn preprocess_query(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();

    let words: Vec<String> = cleaned
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !is_stop_word(w))
        .collect();

    if words.is_empty() {
        return String::new();
    }

    // Unquoted terms with * suffix for prefix matching, joined with OR
    words
        .iter()
        .map(|w| format!("{}*", w))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Tokenize content for near-duplicate comparison.
///
/// Applies the same cleaning as `preprocess_query()` (strip non-alphanumeric,
/// lowercase, filter stop words) but without `*` suffix or `OR` joining.
/// Returns a sorted, deduplicated list of word tokens.
pub fn tokenize_content(content: &str) -> Vec<String> {
    let cleaned: String = content
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();

    let mut words: Vec<String> = cleaned
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| !is_stop_word(w))
        .collect();

    words.sort();
    words.dedup();
    words
}

/// Compute Jaccard similarity between two token sets.
///
/// Both inputs should be sorted and deduplicated (as returned by `tokenize_content()`).
/// Returns intersection/union as a value in [0.0, 1.0].
pub fn jaccard_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let mut intersection = 0usize;
    let mut i = 0;
    let mut j = 0;

    // Merge-style intersection count on sorted slices
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }

    let union = a.len() + b.len() - intersection;
    intersection as f64 / union as f64
}

/// Build an FTS5 query that matches any of the given tags.
#[allow(dead_code)]
pub fn tags_query(tags: &[String]) -> String {
    tags.iter()
        .map(|t| format!("tags:\"{}\"", t.trim()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_query_simple() {
        let result = preprocess_query("hello world");
        assert!(result.contains("hello*"));
        assert!(result.contains("world*"));
    }

    #[test]
    fn test_preprocess_query_special_chars() {
        let result = preprocess_query("test@#$%query");
        assert!(result.contains("test*"));
        assert!(result.contains("query*"));
        assert!(!result.contains("@"));
    }

    #[test]
    fn test_preprocess_query_empty() {
        assert_eq!(preprocess_query(""), "");
    }

    #[test]
    fn test_preprocess_query_filters_stop_words() {
        let result = preprocess_query("where is the db config?");
        assert_eq!(result, "db* OR config*");
    }

    #[test]
    fn test_preprocess_query_all_stop_words() {
        // If every word is a stop word, the result is empty
        let result = preprocess_query("where is the");
        assert_eq!(result, "");
    }

    #[test]
    fn test_preprocess_query_prefix_format() {
        let result = preprocess_query("argo");
        assert_eq!(result, "argo*");
    }

    #[test]
    fn test_preprocess_query_mixed_case() {
        let result = preprocess_query("Deploy Config");
        assert_eq!(result, "deploy* OR config*");
    }

    #[test]
    fn test_tags_query() {
        let tags = vec!["rust".to_string(), "code".to_string()];
        let result = tags_query(&tags);
        assert!(result.contains("tags:\"rust\""));
        assert!(result.contains("tags:\"code\""));
        assert!(result.contains(" OR "));
    }

    #[test]
    fn test_stop_words_are_sorted() {
        // binary_search requires sorted input
        for pair in STOP_WORDS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "STOP_WORDS not sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn test_tokenize_content_basic() {
        let tokens = tokenize_content("deploy the service now");
        // "the" is a stop word, should be filtered
        assert_eq!(tokens, vec!["deploy", "now", "service"]);
    }

    #[test]
    fn test_tokenize_content_deduplicates() {
        let tokens = tokenize_content("deploy deploy deploy");
        assert_eq!(tokens, vec!["deploy"]);
    }

    #[test]
    fn test_tokenize_content_sorted() {
        let tokens = tokenize_content("zebra apple mango");
        assert_eq!(tokens, vec!["apple", "mango", "zebra"]);
    }

    #[test]
    fn test_tokenize_content_empty() {
        let tokens = tokenize_content("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_tokenize_content_all_stop_words() {
        let tokens = tokenize_content("the is a");
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_jaccard_identical() {
        let a = vec!["deploy".into(), "prod".into()];
        let b = vec!["deploy".into(), "prod".into()];
        assert!((jaccard_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_disjoint() {
        let a = vec!["alpha".into(), "beta".into()];
        let b = vec!["gamma".into(), "delta".into()];
        assert!((jaccard_similarity(&a, &b)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_partial_overlap() {
        // {deploy, prod} vs {deploy, production} → intersection=1, union=3 → 0.333
        let a = vec!["deploy".into(), "prod".into()];
        let b = vec!["deploy".into(), "production".into()];
        let sim = jaccard_similarity(&a, &b);
        assert!((sim - 1.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_jaccard_empty_both() {
        assert!((jaccard_similarity(&[], &[]) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_one_empty() {
        let a = vec!["deploy".into()];
        assert!((jaccard_similarity(&a, &[])).abs() < f64::EPSILON);
        assert!((jaccard_similarity(&[], &a)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_jaccard_near_duplicate_threshold() {
        // "deploy to prod" vs "deploy to production"
        let a = tokenize_content("deploy to prod");
        let b = tokenize_content("deploy to production");
        // a = ["deploy", "prod"], b = ["deploy", "production"]
        // intersection=1, union=3 → 0.333 — NOT a near-duplicate
        let sim = jaccard_similarity(&a, &b);
        assert!(
            sim < 0.85,
            "short strings with word changes shouldn't be near-dup: {}",
            sim
        );

        // Longer content with minor edit should be near-dup
        let c =
            tokenize_content("configure kubernetes cluster with argocd pipeline and monitoring");
        let d =
            tokenize_content("configure kubernetes cluster with argocd pipeline monitoring setup");
        let sim2 = jaccard_similarity(&c, &d);
        assert!(
            sim2 > 0.7,
            "similar long content should have high Jaccard: {}",
            sim2
        );
    }
}
