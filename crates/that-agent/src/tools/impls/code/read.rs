//! AST-aware code reading for that-tools.
//!
//! Unlike raw `cat`, `code read` returns structural context:
//! line numbers, symbol annotations, and context-aware excerpts.
//! This gives agents *comprehension* instead of raw text.

#[cfg(feature = "code-analysis")]
use crate::tools::impls::code::parse;
use crate::tools::impls::path_guard;
use crate::tools::output::{self, BudgetedOutput, CompactionStrategy};
use serde::Serialize;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReadError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[cfg(feature = "code-analysis")]
    #[error("parse error: {0}")]
    Parse(#[from] parse::ParseError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a `code read` command.
#[derive(Debug, Clone, Serialize)]
pub struct ReadResult {
    pub path: String,
    pub language: Option<String>,
    pub lines: usize,
    pub symbols: Vec<SymbolSummary>,
    pub content: String,
    pub tokens: usize,
    pub truncated: bool,
}

/// Compact symbol summary for output.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolSummary {
    pub name: String,
    pub kind: String,
    pub line: usize,
}

/// Read a source file with AST-aware context.
///
/// Returns structured output including:
/// - Numbered source lines
/// - Symbol annotations (when `show_symbols` is true)
/// - Context-limited excerpts around specified line ranges
///
/// Budget is applied to the FINAL JSON output — always valid JSON.
pub fn code_read(
    path: &Path,
    context_lines: Option<usize>,
    show_symbols: bool,
    max_tokens: Option<usize>,
    focus_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<BudgetedOutput, ReadError> {
    if !path.exists() {
        return Err(ReadError::NotFound(path.to_string_lossy().to_string()));
    }

    // Guard: reject paths that escape the workspace via traversal or symlinks
    let path = &path_guard::guard(path)?;

    let source = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = source.lines().collect();
    let total_lines = lines.len();

    // Try to parse for symbols, fall back gracefully
    #[cfg(feature = "code-analysis")]
    let (language, symbol_summaries) = {
        let (lang, symbols) = match parse::parse_file(path) {
            Ok(parsed) => (
                Some(format!("{:?}", parsed.language).to_lowercase()),
                parsed.symbols,
            ),
            Err(_) => (None, vec![]),
        };
        let summaries: Vec<SymbolSummary> = if show_symbols {
            symbols
                .iter()
                .map(|s| SymbolSummary {
                    name: s.name.clone(),
                    kind: format!("{:?}", s.kind).to_lowercase(),
                    line: s.line_start,
                })
                .collect()
        } else {
            vec![]
        };
        (lang, summaries)
    };
    #[cfg(not(feature = "code-analysis"))]
    let (language, symbol_summaries): (Option<String>, Vec<SymbolSummary>) = {
        let _ = show_symbols;
        (None, vec![])
    };

    // Build the content with line numbers
    let content = if let (Some(start), Some(end)) = (focus_line, end_line) {
        // Explicit range: --line START --end-line END
        build_range_content(&lines, start, end)
    } else if let Some(focus) = focus_line {
        let ctx = context_lines.unwrap_or(8);
        build_focused_content(&lines, focus, ctx)
    } else {
        build_full_content(&lines)
    };

    // Dynamically measure the envelope size to allocate maximum tokens to content.
    // Serialize a ReadResult with empty content to measure what the envelope costs.
    let content_budget = if let Some(budget) = max_tokens {
        let envelope = ReadResult {
            path: path.to_string_lossy().to_string(),
            language: language.clone(),
            lines: total_lines,
            symbols: symbol_summaries.clone(),
            content: String::new(),
            tokens: 0,
            truncated: false,
        };
        let envelope_json = serde_json::to_string(&envelope).unwrap_or_default();
        let envelope_tokens = output::count_tokens(&envelope_json);
        let margin = 5; // small safety margin
        budget.saturating_sub(envelope_tokens + margin).max(1)
    } else {
        usize::MAX
    };
    let budgeted =
        output::apply_budget_to_text(&content, content_budget, CompactionStrategy::HeadTail);

    let result = ReadResult {
        path: path.to_string_lossy().to_string(),
        language,
        lines: total_lines,
        symbols: symbol_summaries,
        content: budgeted.content,
        tokens: budgeted.tokens,
        truncated: budgeted.truncated,
    };

    // Final budget on the complete JSON output — always valid JSON
    Ok(output::emit_json(&result, max_tokens))
}

/// Build content with line numbers for the full file.
fn build_full_content(lines: &[&str]) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>4} | {}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build content focused around a specific line with context.
fn build_focused_content(lines: &[&str], focus: usize, context: usize) -> String {
    let start = focus.saturating_sub(context + 1);
    let end = (focus + context).min(lines.len());

    let mut result = Vec::new();

    if start > 0 {
        result.push(format!("... ({} lines above)", start));
    }

    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        let marker = if i + 1 == focus { ">" } else { " " };
        result.push(format!("{}{:>4} | {}", marker, i + 1, line));
    }

    if end < lines.len() {
        result.push(format!("... ({} lines below)", lines.len() - end));
    }

    result.join("\n")
}

/// Build content for an explicit line range (1-based, inclusive).
fn build_range_content(lines: &[&str], start: usize, end: usize) -> String {
    let start = start.max(1);
    let end = end.min(lines.len());
    let start_idx = start.saturating_sub(1);

    let mut result = Vec::new();

    if start_idx > 0 {
        result.push(format!("... ({} lines above)", start_idx));
    }

    for (i, line) in lines.iter().enumerate().take(end).skip(start_idx) {
        result.push(format!("{:>4} | {}", i + 1, line));
    }

    if end < lines.len() {
        result.push(format!("... ({} lines below)", lines.len() - end));
    }

    result.join("\n")
}

/// Get all symbols from a file without full read.
#[cfg(feature = "code-analysis")]
#[allow(dead_code)]
pub fn get_symbols(path: &Path) -> Result<Vec<parse::Symbol>, ReadError> {
    let parsed = parse::parse_file(path)?;
    Ok(parsed.symbols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_rust_file(tmp: &TempDir) -> std::path::PathBuf {
        let path = tmp.path().join("test.rs");
        fs::write(
            &path,
            r#"use std::io;

/// A configuration struct.
struct Config {
    name: String,
    value: i32,
}

impl Config {
    fn new(name: &str) -> Self {
        Config {
            name: name.to_string(),
            value: 0,
        }
    }

    fn validate(&self) -> bool {
        !self.name.is_empty()
    }
}

fn main() {
    let config = Config::new("test");
    println!("{}", config.validate());
}
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn test_code_read_basic() {
        let tmp = TempDir::new().unwrap();
        let path = create_rust_file(&tmp);

        let result = code_read(&path, None, false, Some(1000), None, None).unwrap();
        // Must be valid JSON
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.contains("Config"));
    }

    #[cfg(feature = "code-analysis")]
    #[test]
    fn test_code_read_with_symbols() {
        let tmp = TempDir::new().unwrap();
        let path = create_rust_file(&tmp);

        let result = code_read(&path, None, true, Some(2000), None, None).unwrap();
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(result.content.contains("symbols"));
        assert!(result.content.contains("Config"));
        assert!(result.content.contains("function"));
    }

    #[test]
    fn test_code_read_focused() {
        let tmp = TempDir::new().unwrap();
        let path = create_rust_file(&tmp);

        let result = code_read(&path, Some(3), false, Some(1000), Some(10), None).unwrap();
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    }

    #[test]
    fn test_code_read_always_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.rs");
        let content: String = (0..200)
            .map(|i| format!("fn func_{}() {{ /* implementation */ }}\n", i))
            .collect();
        fs::write(&path, &content).unwrap();

        // Even with tiny budget, must be valid JSON
        let result = code_read(&path, None, false, Some(15), None, None).unwrap();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "code read must always be valid JSON: {}",
            result.content
        );
    }

    #[test]
    fn test_code_read_not_found() {
        let result = code_read(Path::new("/nonexistent.rs"), None, false, None, None, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_code_read_non_code_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("readme.txt");
        fs::write(&path, "This is a readme file.\nWith multiple lines.\n").unwrap();

        let result = code_read(&path, None, false, Some(1000), None, None).unwrap();
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    }

    #[cfg(feature = "code-analysis")]
    #[test]
    fn test_get_symbols() {
        let tmp = TempDir::new().unwrap();
        let path = create_rust_file(&tmp);

        let symbols = get_symbols(&path).unwrap();
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Config"));
        assert!(names.contains(&"main"));
    }

    #[test]
    fn test_build_full_content_has_line_numbers() {
        let lines = vec!["first", "second", "third"];
        let content = build_full_content(&lines);
        assert!(content.contains("   1 | first"));
        assert!(content.contains("   2 | second"));
        assert!(content.contains("   3 | third"));
    }

    #[test]
    fn test_build_focused_content() {
        let lines: Vec<&str> = (0..20).map(|_| "code").collect();
        let content = build_focused_content(&lines, 10, 2);
        assert!(content.contains("lines above"));
        assert!(content.contains("lines below"));
        assert!(content.contains(">  10"));
    }

    #[test]
    fn test_build_range_content() {
        let lines: Vec<&str> = (0..30).map(|_| "line").collect();
        let content = build_range_content(&lines, 5, 10);
        assert!(content.contains("   5 | line"));
        assert!(content.contains("  10 | line"));
        assert!(content.contains("lines above"));
        assert!(content.contains("lines below"));
        // Lines outside the range are not present as numbered lines
        assert!(!content.contains("   4 | line"));
        assert!(!content.contains("  11 | line"));
    }

    #[test]
    fn test_code_read_range() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("many.rs");
        let content: String = (1..=50)
            .map(|i| format!("fn func_{}() {{}}\n", i))
            .collect();
        fs::write(&path, &content).unwrap();

        let result = code_read(&path, None, false, Some(4096), Some(10), Some(20)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        let file_content = parsed["content"].as_str().unwrap();
        assert!(file_content.contains("func_10"), "should include line 10");
        assert!(file_content.contains("func_20"), "should include line 20");
        assert!(
            !file_content.contains("func_21"),
            "should not include line 21"
        );
    }

    #[test]
    fn test_code_read_150_lines_not_truncated_at_4096() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("medium.rs");
        let content: String = (0..150)
            .map(|i| format!("fn func_{}() {{ /* body */ }}\n", i))
            .collect();
        fs::write(&path, &content).unwrap();

        let result = code_read(&path, None, false, Some(4096), None, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            parsed["truncated"], false,
            "150-line file should NOT be truncated at 4096 token budget"
        );
        assert_eq!(parsed["lines"], 150);
    }

    #[test]
    fn test_code_read_no_budget_no_truncation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("large.rs");
        let content: String = (0..300)
            .map(|i| format!("fn func_{}() {{ /* body */ }}\n", i))
            .collect();
        fs::write(&path, &content).unwrap();

        // With no max_tokens, the fallback should be usize::MAX (no truncation)
        let result = code_read(&path, None, false, None, None, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            parsed["truncated"], false,
            "file should NOT be truncated when no budget is specified"
        );
    }
}
