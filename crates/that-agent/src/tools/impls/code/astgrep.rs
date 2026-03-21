//! Structural code search using tree-sitter patterns.
//!
//! Provides ast-grep-like structural pattern matching using tree-sitter's
//! built-in query API (S-expression patterns). Supports metavariable captures
//! via `@name` syntax in tree-sitter query patterns.
//!
//! # Pattern Syntax
//!
//! Uses tree-sitter S-expression queries:
//! - `(function_item name: (identifier) @name)` — match functions, capture name
//! - `(call_expression function: (identifier) @fn)` — match function calls
//! - `(struct_item name: (type_identifier) @name)` — match struct definitions
//!
//! Language is auto-detected from file extensions, or can be forced with `--language`.

use crate::tools::impls::code::parse::Language;
use crate::tools::output::{self, BudgetedOutput};
use ignore::WalkBuilder;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use streaming_iterator::StreamingIterator;
use thiserror::Error;
use tree_sitter::{Parser, Query};

#[derive(Error, Debug)]
pub enum AstGrepError {
    #[error("invalid pattern: {0}")]
    InvalidPattern(String),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A single structural match.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StructuralMatch {
    pub file: String,
    pub line_number: usize,
    pub content: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub captures: HashMap<String, String>,
}

/// Result of a structural search operation.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct StructuralSearchResult {
    pub pattern: String,
    pub matches: Vec<StructuralMatch>,
    pub total_matches: usize,
    pub files_searched: usize,
}

/// Perform a structural search across files.
///
/// Walks the directory tree (respecting .gitignore), parses each supported
/// file with tree-sitter, and runs the pattern query to find matches.
pub fn structural_search(
    root: &Path,
    pattern: &str,
    language_filter: Option<&str>,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, AstGrepError> {
    if !root.exists() {
        return Err(AstGrepError::NotFound(root.to_string_lossy().to_string()));
    }

    let filter_lang = language_filter.map(parse_language_name).transpose()?;

    let mut all_matches = Vec::new();
    let mut files_searched = 0;
    let mut files_attempted = 0;
    let mut last_pattern_error: Option<String> = None;

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();
        let language = match Language::from_path(path) {
            Some(l) => l,
            None => continue,
        };

        // Apply language filter if specified
        if let Some(ref filter) = filter_lang {
            if language != *filter {
                continue;
            }
        }

        // Skip JSON — no meaningful structural patterns
        if language == Language::Json {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        files_attempted += 1;
        match search_in_source(&content, pattern, language, &relative) {
            Ok(matches) => {
                files_searched += 1;
                all_matches.extend(matches);
            }
            Err(AstGrepError::InvalidPattern(msg)) => {
                // If a language filter is specified, the pattern is expected to work for that language
                if filter_lang.is_some() {
                    return Err(AstGrepError::InvalidPattern(msg));
                }
                // Otherwise skip — pattern may be valid for a different language
                tracing::debug!("pattern invalid for {:?}, skipping: {}", language, msg);
                last_pattern_error = Some(msg);
                continue;
            }
            Err(_) => continue,
        }
    }

    // If the pattern was invalid for every file attempted, report the error
    if files_searched == 0 && files_attempted > 0 {
        if let Some(err) = last_pattern_error {
            return Err(AstGrepError::InvalidPattern(err));
        }
    }

    let total = all_matches.len();
    let result = StructuralSearchResult {
        pattern: pattern.to_string(),
        matches: all_matches,
        total_matches: total,
        files_searched,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Search for a pattern in a single source string.
fn search_in_source(
    source: &str,
    pattern: &str,
    language: Language,
    file_path: &str,
) -> Result<Vec<StructuralMatch>, AstGrepError> {
    let ts_lang = language.tree_sitter_language();

    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .map_err(|e| AstGrepError::InvalidPattern(format!("language setup failed: {}", e)))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| AstGrepError::InvalidPattern("parse failed".to_string()))?;

    let query = Query::new(&ts_lang, pattern)
        .map_err(|e| AstGrepError::InvalidPattern(format!("{}", e)))?;

    let capture_names: Vec<String> = query
        .capture_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut cursor = tree_sitter::QueryCursor::new();
    let mut matches = Vec::new();
    let mut seen_matches: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // tree-sitter 0.24 uses StreamingIterator (advance + get pattern)
    let mut query_matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
    while let Some(query_match) = {
        query_matches.advance();
        query_matches.get()
    } {
        // Deduplicate by pattern_index + first capture start byte
        let match_key = query_match
            .captures
            .first()
            .map(|c| c.node.start_byte() as u64)
            .unwrap_or(0);
        let dedup_key = (query_match.pattern_index as u64) * 1_000_000 + match_key;

        if !seen_matches.insert(dedup_key) {
            continue;
        }

        let mut captures: HashMap<String, String> = HashMap::new();
        let mut match_node = None;

        for capture in query_match.captures {
            let name = &capture_names[capture.index as usize];
            let text = capture
                .node
                .utf8_text(source.as_bytes())
                .unwrap_or("")
                .to_string();
            captures.insert(name.clone(), text);

            if match_node.is_none() {
                match_node = Some(capture.node);
            }
        }

        if let Some(node) = match_node {
            let content = node.utf8_text(source.as_bytes()).unwrap_or("").to_string();

            let display_content = if content.len() > 200 {
                // Truncate at char boundary to avoid panic on multi-byte UTF-8
                let truncated: String = content.chars().take(200).collect();
                format!("{}...", truncated)
            } else {
                content
            };

            matches.push(StructuralMatch {
                file: file_path.to_string(),
                line_number: node.start_position().row + 1,
                content: display_content,
                captures,
            });
        }
    }

    Ok(matches)
}

/// Parse a language name string to a Language enum.
fn parse_language_name(name: &str) -> Result<Language, AstGrepError> {
    match name.to_lowercase().as_str() {
        "rust" | "rs" => Ok(Language::Rust),
        "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" => Ok(Language::TypeScript),
        "python" | "py" => Ok(Language::Python),
        "go" | "golang" => Ok(Language::Go),
        "json" => Ok(Language::Json),
        _ => Err(AstGrepError::UnsupportedLanguage(name.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_search_rust_functions() {
        let source = r#"
fn main() {
    println!("hello");
}

fn helper() -> i32 {
    42
}
"#;
        let matches = search_in_source(
            source,
            "(function_item name: (identifier) @name)",
            Language::Rust,
            "test.rs",
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches
            .iter()
            .map(|m| m.captures["name"].as_str())
            .collect();
        assert!(names.contains(&"main"));
        assert!(names.contains(&"helper"));
    }

    #[test]
    fn test_search_typescript_classes() {
        let source = r#"
class UserService {
    getUser(id: string) {
        return { id };
    }
}

class AdminService {
    listUsers() {
        return [];
    }
}
"#;
        let matches = search_in_source(
            source,
            "(class_declaration name: (type_identifier) @name)",
            Language::TypeScript,
            "test.ts",
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        let names: Vec<&str> = matches
            .iter()
            .map(|m| m.captures["name"].as_str())
            .collect();
        assert!(names.contains(&"UserService"));
        assert!(names.contains(&"AdminService"));
    }

    #[test]
    fn test_search_python_functions() {
        let source = r#"
def process_data(items):
    return [x * 2 for x in items]

def validate(input):
    return input is not None
"#;
        let matches = search_in_source(
            source,
            "(function_definition name: (identifier) @name)",
            Language::Python,
            "test.py",
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_search_go_functions() {
        let source = r#"package main

func main() {
    fmt.Println("hello")
}

func helper() int {
    return 42
}
"#;
        let matches = search_in_source(
            source,
            "(function_declaration name: (identifier) @name)",
            Language::Go,
            "test.go",
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_invalid_pattern() {
        let result = search_in_source(
            "fn main() {}",
            "(invalid_node_type_xyz)",
            Language::Rust,
            "test.rs",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_structural_search_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\nfn helper() {}\n").unwrap();
        fs::write(tmp.path().join("lib.rs"), "fn process() {}\n").unwrap();

        let result = structural_search(
            tmp.path(),
            "(function_item name: (identifier) @name)",
            None,
            Some(2000),
        )
        .unwrap();

        let parsed: StructuralSearchResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 3);
        assert_eq!(parsed.files_searched, 2);
    }

    #[test]
    fn test_structural_search_with_language_filter() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(tmp.path().join("main.py"), "def main(): pass\n").unwrap();

        let result = structural_search(
            tmp.path(),
            "(function_item name: (identifier) @name)",
            Some("rust"),
            Some(2000),
        )
        .unwrap();

        let parsed: StructuralSearchResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.total_matches, 1);
        assert_eq!(parsed.files_searched, 1);
    }

    #[test]
    fn test_structural_search_not_found() {
        let result = structural_search(Path::new("/nonexistent"), "(function_item)", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_language_name() {
        assert_eq!(parse_language_name("rust").unwrap(), Language::Rust);
        assert_eq!(parse_language_name("rs").unwrap(), Language::Rust);
        assert_eq!(
            parse_language_name("typescript").unwrap(),
            Language::TypeScript
        );
        assert_eq!(parse_language_name("python").unwrap(), Language::Python);
        assert_eq!(parse_language_name("go").unwrap(), Language::Go);
        assert!(parse_language_name("unknown").is_err());
    }

    #[test]
    fn test_captures_content() {
        let source = "fn main() {\n    let x = 42;\n}\n";
        let matches = search_in_source(
            source,
            "(function_item name: (identifier) @name)",
            Language::Rust,
            "test.rs",
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures["name"], "main");
        assert_eq!(matches[0].line_number, 1);
    }

    #[test]
    fn test_search_struct_fields() {
        let source = r#"
struct Config {
    name: String,
    value: i32,
}
"#;
        let matches = search_in_source(
            source,
            "(struct_item name: (type_identifier) @name)",
            Language::Rust,
            "test.rs",
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].captures["name"], "Config");
    }
}
