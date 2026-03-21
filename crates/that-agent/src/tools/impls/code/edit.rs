//! Multi-format code editing with tree-sitter syntax validation.
//!
//! Supports four edit formats:
//! - Unified diff: apply patches from stdin
//! - Search/replace: exact text replacement with fuzzy fallback
//! - AST-node: target specific symbols by name
//! - Whole file: replace entire content from stdin
//!
//! All edits are validated via tree-sitter — invalid syntax is rejected.

use crate::tools::impls::code::parse::{self, Language};
use crate::tools::output::{self, BudgetedOutput};
use serde::Serialize;
use similar::TextDiff;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EditError {
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("syntax validation failed: {0}")]
    ValidationFailed(String),
    #[error("edit format error: {0}")]
    FormatError(String),
    #[error("symbol not found: {0}")]
    SymbolNotFound(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("patch error: {0}")]
    PatchError(String),
}

/// The edit format specifying how to modify the file.
#[derive(Debug, Clone)]
pub enum EditFormat {
    /// Apply a unified diff from stdin.
    UnifiedDiff,
    /// Search for exact text and replace.
    SearchReplace {
        search: String,
        replace: String,
        /// Replace all occurrences instead of just the first.
        all: bool,
    },
    /// Replace a specific symbol's body by name.
    AstNode {
        symbol_name: String,
        new_body: String,
    },
    /// Replace the entire file content from stdin.
    WholeFile,
}

/// Result of an edit operation.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct EditResult {
    pub path: String,
    pub format: String,
    pub applied: bool,
    pub validated: bool,
    pub diff: String,
    pub lines_changed: usize,
    /// Number of replacements made (only set for search-replace with --all).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacements: Option<usize>,
}

/// Apply an edit to a source file with syntax validation.
///
/// Pipeline:
/// 1. Read current file content
/// 2. Compute new content based on format
/// 3. Validate syntax via tree-sitter (if supported language)
/// 4. If dry_run: return diff preview only
/// 5. If not dry_run: write file, return result with diff
pub fn code_edit(
    path: &Path,
    format: &EditFormat,
    dry_run: bool,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, EditError> {
    if !path.exists() {
        return Err(EditError::NotFound(path.to_string_lossy().to_string()));
    }

    let original = std::fs::read_to_string(path)?;
    let computed = compute_new_content(path, &original, format)?;
    let new_content = computed.content;

    // Validate syntax via tree-sitter
    let language = Language::from_path(path);
    let validated = if let Some(lang) = language {
        validate_syntax(&new_content, lang)?;
        true
    } else {
        tracing::debug!("no tree-sitter grammar for {:?}, skipping validation", path);
        false
    };

    // Generate diff
    let diff = generate_diff(&original, &new_content, path);
    let lines_changed = count_changed_lines(&diff);

    let format_name = match format {
        EditFormat::UnifiedDiff => "unified-diff",
        EditFormat::SearchReplace { .. } => "search-replace",
        EditFormat::AstNode { .. } => "ast-node",
        EditFormat::WholeFile => "whole-file",
    };

    if !dry_run {
        std::fs::write(path, &new_content)?;
    }

    let result = EditResult {
        path: path.to_string_lossy().to_string(),
        format: format_name.to_string(),
        applied: !dry_run,
        validated,
        diff,
        lines_changed,
        replacements: computed.replacements,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Result of computing new content — includes optional replacement count.
struct ComputeResult {
    content: String,
    replacements: Option<usize>,
}

/// Compute the new file content based on the edit format.
fn compute_new_content(
    path: &Path,
    original: &str,
    format: &EditFormat,
) -> Result<ComputeResult, EditError> {
    match format {
        EditFormat::UnifiedDiff => apply_unified_diff(original).map(|c| ComputeResult {
            content: c,
            replacements: None,
        }),
        EditFormat::SearchReplace {
            search,
            replace,
            all,
        } => apply_search_replace(original, search, replace, *all),
        EditFormat::AstNode {
            symbol_name,
            new_body,
        } => apply_ast_node(path, original, symbol_name, new_body).map(|c| ComputeResult {
            content: c,
            replacements: None,
        }),
        EditFormat::WholeFile => read_stdin().map(|c| ComputeResult {
            content: c,
            replacements: None,
        }),
    }
}

/// Apply a unified diff read from stdin.
fn apply_unified_diff(original: &str) -> Result<String, EditError> {
    let patch_text = read_stdin()?;
    let patch = diffy::Patch::from_str(&patch_text)
        .map_err(|e| EditError::PatchError(format!("invalid patch: {}", e)))?;
    diffy::apply(original, &patch)
        .map_err(|e| EditError::PatchError(format!("patch apply failed: {}", e)))
}

/// Apply search/replace on the original content.
fn apply_search_replace(
    original: &str,
    search: &str,
    replace: &str,
    all: bool,
) -> Result<ComputeResult, EditError> {
    if search.is_empty() {
        return Err(EditError::FormatError(
            "search text cannot be empty".to_string(),
        ));
    }
    if original.contains(search) {
        if all {
            let count = original.matches(search).count();
            let content = original.replace(search, replace);
            return Ok(ComputeResult {
                content,
                replacements: Some(count),
            });
        }
        // Single replacement (default)
        Ok(ComputeResult {
            content: original.replacen(search, replace, 1),
            replacements: None,
        })
    } else {
        // Try fuzzy: find the closest matching lines with minimum similarity
        let search_lines: Vec<&str> = search.lines().collect();
        let original_lines: Vec<&str> = original.lines().collect();

        if search_lines.len() == 1 && !original_lines.is_empty() {
            // Single-line fuzzy: find best matching line with similarity threshold
            let best = original_lines
                .iter()
                .enumerate()
                .map(|(idx, line)| {
                    let matching = line
                        .chars()
                        .zip(search.chars())
                        .filter(|(a, b)| a == b)
                        .count();
                    let max_len = line.chars().count().max(search.chars().count()).max(1);
                    let similarity = matching as f64 / max_len as f64;
                    (idx, similarity)
                })
                .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            if let Some((idx, similarity)) = best {
                // Require at least 50% character similarity to prevent unrelated edits
                if similarity >= 0.5 {
                    let mut new_lines: Vec<String> =
                        original_lines.iter().map(|l| l.to_string()).collect();
                    new_lines[idx] = replace.to_string();
                    return Ok(ComputeResult {
                        content: new_lines.join("\n"),
                        replacements: None,
                    });
                }
            }
        }

        // Final fallback: try with `\\` normalised to `\`.
        // LLMs sometimes copy content from code_read JSON output verbatim, retaining
        // the JSON-escaped form of backslashes (\\) rather than the decoded value (\).
        let normalized = search.replace("\\\\", "\\");
        if normalized != search && original.contains(normalized.as_str()) {
            if all {
                let count = original.matches(normalized.as_str()).count();
                return Ok(ComputeResult {
                    content: original.replace(normalized.as_str(), replace),
                    replacements: Some(count),
                });
            }
            return Ok(ComputeResult {
                content: original.replacen(normalized.as_str(), replace, 1),
                replacements: None,
            });
        }

        Err(EditError::FormatError(
            "search text not found in file (exact match failed, fuzzy match below similarity threshold)".to_string()
        ))
    }
}

/// Replace a symbol's body using AST information.
fn apply_ast_node(
    path: &Path,
    original: &str,
    symbol_name: &str,
    new_body: &str,
) -> Result<String, EditError> {
    let language = parse::Language::from_path(path)
        .ok_or_else(|| EditError::FormatError("unsupported language for AST edit".to_string()))?;

    // Parse the already-read content to avoid TOCTOU (file could change between reads)
    let parsed = parse::parse_source(original, language)
        .map_err(|e| EditError::FormatError(format!("parse failed: {}", e)))?;

    let symbol = parsed
        .symbols
        .iter()
        .find(|s| s.name == symbol_name)
        .ok_or_else(|| EditError::SymbolNotFound(symbol_name.to_string()))?;

    // Bounds-check byte offsets before slicing
    if symbol.byte_start > original.len()
        || symbol.byte_end > original.len()
        || symbol.byte_start > symbol.byte_end
    {
        return Err(EditError::FormatError(format!(
            "symbol byte range {}..{} out of bounds for file of {} bytes",
            symbol.byte_start,
            symbol.byte_end,
            original.len()
        )));
    }

    let symbol_text = &original[symbol.byte_start..symbol.byte_end];

    // Determine the indentation of the symbol's first line so the closing brace aligns.
    let indent = original[..symbol.byte_start]
        .rfind('\n')
        .map(|pos| {
            let after_nl = &original[pos + 1..symbol.byte_start];
            let spaces = after_nl.len() - after_nl.trim_start_matches([' ', '\t']).len();
            after_nl[..spaces].to_string()
        })
        .unwrap_or_default();

    // Use tree-sitter to locate the body node rather than searching for `{`.
    // The naive `find('{')` approach breaks for Python (indentation-based syntax) and
    // for any language where `{` appears before the body — e.g. in f-strings, dict
    // literals, or generic type parameters — producing corrupt output that fails
    // syntax validation.
    let new_symbol = if let Some(body_node) = find_body_node(&parsed.tree, symbol.byte_start) {
        let body_start = body_node.start_byte().saturating_sub(symbol.byte_start);
        match language {
            parse::Language::Python => {
                // Python uses indentation, not braces.  The `block` body node starts
                // immediately after the colon + newline.  Keep the full signature
                // (including the colon) and replace only the block content.
                // No closing `}` is emitted.
                let signature = &symbol_text[..body_start];
                format!("{}{}", signature, new_body)
            }
            _ => {
                // Brace-based languages: the body node begins at `{`.
                // Include it in the signature so the caller only supplies inner lines.
                let signature = &symbol_text[..=body_start];
                format!("{}\n{}\n{}}}", signature, new_body, indent)
            }
        }
    } else {
        // Fallback for node shapes where tree-sitter does not expose a `body` field.
        let brace_pos = symbol_text.find('{').ok_or_else(|| {
            EditError::FormatError(
                "symbol has no body block (no '{' found) — use --search/--replace instead"
                    .to_string(),
            )
        })?;
        let signature = &symbol_text[..=brace_pos];
        format!("{}\n{}\n{}}}", signature, new_body, indent)
    };

    let mut result = String::new();
    result.push_str(&original[..symbol.byte_start]);
    result.push_str(&new_symbol);
    result.push_str(&original[symbol.byte_end..]);

    Ok(result)
}

/// Walk the parse tree to find the node that starts at `target_byte` and return
/// its `body` field child.
///
/// Tree-sitter exposes a `body` field for all function/method/class definitions in
/// every supported language (Rust `function_item`, Python `function_definition`,
/// TypeScript `function_declaration`, Go `function_declaration`).  Using the field
/// avoids false positives from `{` characters inside f-strings, dict literals, and
/// type annotations.
///
/// Multiple nodes can share the same `start_byte` (e.g. the root `module` node and
/// the first `function_definition` both start at byte 0).  We keep the last node
/// encountered that has a `body` field — in a depth-first walk the function node is
/// visited after the container, so last-wins gives us the most specific match.
fn find_body_node<'tree>(
    tree: &'tree tree_sitter::Tree,
    target_byte: usize,
) -> Option<tree_sitter::Node<'tree>> {
    let mut best: Option<tree_sitter::Node<'tree>> = None;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.start_byte() == target_byte && node.child_by_field_name("body").is_some() {
            best = Some(node);
        }
        if node.start_byte() <= target_byte && target_byte < node.end_byte() {
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push(child);
            }
        }
    }
    best.and_then(|n| n.child_by_field_name("body"))
}

/// Read content from stdin.
fn read_stdin() -> Result<String, EditError> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(EditError::Io)?;
    Ok(buf)
}

/// Validate that the new content parses without errors.
fn validate_syntax(content: &str, language: Language) -> Result<(), EditError> {
    let parsed = parse::parse_source(content, language)
        .map_err(|e| EditError::ValidationFailed(format!("parse error: {}", e)))?;

    if parsed.tree.root_node().has_error() {
        // Find the first error node for a useful message
        let error_node = find_first_error(parsed.tree.root_node());
        let location = error_node
            .map(|n| {
                format!(
                    "line {}, column {}",
                    n.start_position().row + 1,
                    n.start_position().column + 1
                )
            })
            .unwrap_or_else(|| "unknown location".to_string());

        return Err(EditError::ValidationFailed(format!(
            "syntax error at {}",
            location
        )));
    }

    Ok(())
}

/// Find the first ERROR or MISSING node in the tree (iterative to avoid stack overflow).
fn find_first_error(root: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return Some(node);
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

/// Generate a unified diff between original and new content.
fn generate_diff(original: &str, new: &str, path: &Path) -> String {
    let diff = TextDiff::from_lines(original, new);
    let path_str = path.to_string_lossy();
    diff.unified_diff()
        .header(&format!("a/{}", path_str), &format!("b/{}", path_str))
        .to_string()
}

/// Count the number of changed lines in a diff.
fn count_changed_lines(diff: &str) -> usize {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') || l.starts_with('-'))
                && !l.starts_with("+++")
                && !l.starts_with("---")
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn create_test_file(tmp: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = tmp.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_search_replace_exact() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn main() {\n    println!(\"hello\");\n}\n",
        );

        let format = EditFormat::SearchReplace {
            search: "hello".to_string(),
            replace: "world".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);
        assert!(parsed.validated);
        assert!(parsed.diff.contains("+"));

        // Verify file was actually modified
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("world"));
        assert!(!content.contains("hello"));
    }

    #[test]
    fn test_search_replace_dry_run() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn main() {\n    println!(\"hello\");\n}\n",
        );

        let format = EditFormat::SearchReplace {
            search: "hello".to_string(),
            replace: "world".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, true, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(!parsed.applied, "dry run should not apply");

        // Verify file was NOT modified
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("hello"));
    }

    #[test]
    fn test_validation_rejects_invalid_syntax() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn main() {\n    println!(\"hello\");\n}\n",
        );

        let format = EditFormat::SearchReplace {
            search: "fn main() {\n    println!(\"hello\");\n}".to_string(),
            replace: "fn main() {{{{{".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000));
        assert!(result.is_err(), "should reject invalid syntax");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("syntax"),
            "error should mention syntax: {}",
            err
        );
    }

    #[test]
    fn test_ast_node_replacement() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn hello() {\n    println!(\"hello\");\n}\n\nfn world() {\n    println!(\"world\");\n}\n",
        );

        // --new-body is the inner body only — no signature, no braces
        let format = EditFormat::AstNode {
            symbol_name: "hello".to_string(),
            new_body: "    println!(\"updated\");".to_string(),
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);
        assert_eq!(parsed.format, "ast-node");

        let content = fs::read_to_string(&path).unwrap();
        // Signature is preserved
        assert!(content.contains("fn hello()"), "signature must be kept");
        // Body is replaced
        assert!(content.contains("updated"), "new body must be present");
        assert!(
            !content.contains("println!(\"hello\")"),
            "old body must be gone"
        );
        assert!(
            content.contains("world"),
            "should not affect other functions"
        );
    }

    #[test]
    fn test_ast_node_body_only_no_signature_needed() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        );

        let format = EditFormat::AstNode {
            symbol_name: "add".to_string(),
            new_body: "    a + b + 1".to_string(),
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("fn add(a: i32, b: i32) -> i32 {"));
        assert!(content.contains("a + b + 1"));
    }

    #[test]
    fn test_ast_node_symbol_not_found() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(&tmp, "test.rs", "fn main() {}\n");

        let format = EditFormat::AstNode {
            symbol_name: "nonexistent".to_string(),
            new_body: "fn nonexistent() {}".to_string(),
        };

        let result = code_edit(&path, &format, false, Some(1000));
        assert!(result.is_err());
    }

    #[test]
    fn test_edit_not_found() {
        let result = code_edit(
            Path::new("/nonexistent.rs"),
            &EditFormat::SearchReplace {
                search: "a".into(),
                replace: "b".into(),
                all: false,
            },
            false,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_edit_non_code_file_skips_validation() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(&tmp, "readme.txt", "Hello world\n");

        let format = EditFormat::SearchReplace {
            search: "Hello".to_string(),
            replace: "Goodbye".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);
        assert!(!parsed.validated, "non-code files skip validation");
    }

    #[test]
    fn test_generate_diff() {
        let diff = generate_diff("hello\n", "world\n", Path::new("test.txt"));
        assert!(diff.contains("-hello"));
        assert!(diff.contains("+world"));
    }

    #[test]
    fn test_count_changed_lines() {
        let diff = "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n";
        assert_eq!(count_changed_lines(diff), 2);
    }

    #[test]
    fn test_validate_syntax_valid() {
        let result = validate_syntax("fn main() {}\n", Language::Rust);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_syntax_invalid() {
        let result = validate_syntax("fn main() {{{{{", Language::Rust);
        assert!(result.is_err());
    }

    #[test]
    fn test_search_replace_multiline() {
        let tmp = TempDir::new().unwrap();
        let original = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        let path = create_test_file(&tmp, "test.rs", original);

        let format = EditFormat::SearchReplace {
            search: "    let x = 1;\n    let y = 2;".to_string(),
            replace: "    let z = 3;".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("let z = 3"));
    }

    #[test]
    fn test_edit_result_valid_json() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.rs",
            "fn main() {\n    println!(\"hello\");\n}\n",
        );

        let format = EditFormat::SearchReplace {
            search: "hello".to_string(),
            replace: "world".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let _: serde_json::Value =
            serde_json::from_str(&result.content).expect("edit output must be valid JSON");
    }

    #[test]
    fn test_ast_node_python_function() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "script.py",
            "def greet(name):\n    return 'hello'\n\ndef other():\n    pass\n",
        );

        let format = EditFormat::AstNode {
            symbol_name: "greet".to_string(),
            new_body: "    return f'hi {name}'".to_string(),
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("def greet(name):"),
            "signature must be kept"
        );
        assert!(content.contains("hi {name}"), "new body must be present");
        assert!(!content.contains("'hello'"), "old body must be gone");
        assert!(
            content.contains("def other():"),
            "other function must be untouched"
        );
    }

    #[test]
    fn test_ast_node_python_function_with_fstring_braces() {
        // Regression: `find('{')` previously matched `{` inside f-strings/dicts,
        // producing corrupt Python that failed syntax validation.
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "bump.py",
            "import re\n\ndef bump_version(text, new_version):\n    pattern = re.compile(r'old')\n    return pattern.sub(rf'\\1{new_version}\\2', text)\n",
        );

        let format = EditFormat::AstNode {
            symbol_name: "bump_version".to_string(),
            new_body: "    return text.replace('old', new_version)".to_string(),
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied, "edit must apply");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("def bump_version(text, new_version):"));
        assert!(content.contains("text.replace('old', new_version)"));
        assert!(!content.contains("pattern.sub"));
    }

    #[test]
    fn test_search_replace_backslash_normalization() {
        // Regression: LLMs sometimes copy backslashes from code_read JSON output in
        // their doubled form (\\) rather than the decoded form (\).
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "script.py",
            "def f():\n    return re.sub(r'\\d+', '', text)\n",
        );

        // Agent submits search with doubled backslash (\\d+ instead of \d+)
        let format = EditFormat::SearchReplace {
            search: "    return re.sub(r'\\\\d+', '', text)".to_string(),
            replace: "    return re.sub(r'\\d+', 'X', text)".to_string(),
            all: false,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied, "backslash-normalised search must succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("'X'"), "replacement must be present");
    }

    #[test]
    fn test_search_replace_all() {
        let tmp = TempDir::new().unwrap();
        let path = create_test_file(
            &tmp,
            "test.md",
            "## Name\nSparks\n\n## Identity\nSparks is bold.\n\n## Purpose\nSparks ships.\n",
        );

        let format = EditFormat::SearchReplace {
            search: "Sparks".to_string(),
            replace: "Jizz".to_string(),
            all: true,
        };

        let result = code_edit(&path, &format, false, Some(1000)).unwrap();
        let parsed: EditResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.applied);
        assert_eq!(parsed.replacements, Some(3));

        let content = fs::read_to_string(&path).unwrap();
        assert!(
            !content.contains("Sparks"),
            "all occurrences should be replaced"
        );
        assert_eq!(content.matches("Jizz").count(), 3);
    }
}
