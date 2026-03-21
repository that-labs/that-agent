//! Token budget engine for that-tools.
//!
//! Every command output passes through this pipeline:
//! 1. Tool produces a typed result struct
//! 2. `ToolContext::emit()` serializes to the requested format
//! 3. If over budget, the *data* is reduced (array truncation, field elision)
//!    **before** serialization — guaranteeing valid output at every budget
//! 4. Final serialized output is measured and returned
//!
//! Key invariant: output is ALWAYS valid JSON when format is JSON.
//! Budget is enforced on the FINAL emitted payload, including envelope.

mod budget;
mod compaction;
mod json_analysis;
mod render;
mod tokenizer;
mod types;

pub use budget::{apply_budget_to_text, emit_json, CompactionStrategy};
pub use render::{compact_json_value, render_markdown, render_raw};
pub use tokenizer::count_tokens;
pub use types::{BudgetedOutput, OutputEnvelope};

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[test]
    fn test_count_tokens_empty() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn test_count_tokens_simple() {
        let count = count_tokens("Hello, world!");
        assert!(count > 0);
        assert!(count < 10);
    }

    #[test]
    fn test_tokenizer_is_cached() {
        // Calling count_tokens multiple times should not panic or slow down
        for _ in 0..100 {
            count_tokens("test string");
        }
    }

    #[test]
    fn test_apply_budget_to_text_within_budget() {
        let text = "Hello, world!";
        let result = apply_budget_to_text(text, 100, CompactionStrategy::HeadTail);
        assert!(!result.truncated);
        assert_eq!(result.content, text);
    }

    #[test]
    fn test_apply_budget_to_text_exceeds_budget() {
        let text = (0..100)
            .map(|i| format!("Line {}: some content here", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = apply_budget_to_text(&text, 50, CompactionStrategy::HeadTail);
        assert!(result.truncated);
        assert!(result.content.contains("lines omitted"));
    }

    #[test]
    fn test_emit_json_within_budget() {
        let data = vec!["file1.rs", "file2.rs"];
        let result = emit_json(&data, Some(100));
        assert!(!result.truncated);
        // Must be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.is_array());
    }

    #[test]
    fn test_emit_json_no_budget() {
        let data = vec!["a", "b", "c"];
        let result = emit_json(&data, None);
        assert!(!result.truncated);
        let _: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    }

    #[test]
    fn test_emit_json_over_budget_produces_valid_json() {
        let data: Vec<String> = (0..500).map(|i| format!("file_{}.rs", i)).collect();
        let result = emit_json(&data, Some(30));
        assert!(result.truncated);
        // CRITICAL: Must still be valid JSON
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "truncated output must be valid JSON: {}",
            result.content
        );
    }

    #[test]
    fn test_emit_json_over_budget_object_produces_valid_json() {
        #[derive(Serialize)]
        struct BigResult {
            entries: Vec<String>,
            total: usize,
            content: String,
        }
        let big = BigResult {
            entries: (0..200).map(|i| format!("entry_{}", i)).collect(),
            total: 200,
            content: "x".repeat(5000),
        };
        let result = emit_json(&big, Some(40));
        assert!(result.truncated);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "truncated object must be valid JSON: {}",
            result.content
        );
    }

    #[test]
    fn test_emit_json_very_small_budget() {
        let data: Vec<String> = (0..100).map(|i| format!("item_{}", i)).collect();
        let result = emit_json(&data, Some(5));
        // Must still be valid JSON, even with extreme budget
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&result.content);
        assert!(
            parsed.is_ok(),
            "extreme budget must produce valid JSON: {}",
            result.content
        );
        // Should use skeleton or budget_exhausted, not {"t":1}
        assert!(
            !result.content.contains(r#""t":1"#),
            "should not use old {{\"t\":1}} fallback"
        );
    }

    #[test]
    fn test_extract_skeleton_keeps_scalars() {
        let value = serde_json::json!({
            "path": "src/main.rs",
            "lines": 100,
            "truncated": false,
            "content": "x".repeat(200),
            "entries": [1, 2, 3]
        });
        let skeleton = json_analysis::extract_skeleton(&value);
        let obj = skeleton.as_object().unwrap();
        assert_eq!(obj.get("path").unwrap(), "src/main.rs");
        assert_eq!(obj.get("lines").unwrap(), 100);
        assert_eq!(obj.get("truncated").unwrap(), true); // overwritten by skeleton marker
        assert!(
            obj.get("content").is_none(),
            "long strings should be dropped"
        );
        assert!(obj.get("entries").is_none(), "arrays should be dropped");
    }

    #[test]
    fn test_apply_budget_to_text_head_only() {
        let text = (0..100)
            .map(|i| format!("Line {}: content", i))
            .collect::<Vec<_>>()
            .join("\n");

        let result = apply_budget_to_text(&text, 30, CompactionStrategy::HeadOnly);
        assert!(result.truncated);
        assert!(result.content.contains("more lines truncated"));
    }

    #[test]
    fn test_apply_budget_to_text_rule_based_prioritizes_errors() {
        let lines = [
            "INFO: Starting up",
            "DEBUG: Loading config",
            "ERROR: Failed to connect to database",
            "INFO: Retrying",
            "WARN: Connection timeout",
            "DEBUG: Some debug info",
        ];
        let text = lines.join("\n");

        let result = apply_budget_to_text(&text, 20, CompactionStrategy::RuleBased);
        assert!(result.truncated);
        assert!(result.content.contains("ERROR"));
    }

    #[test]
    fn test_budgeted_output_original_tokens_preserved() {
        let text = (0..50)
            .map(|i| format!("Line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let original_count = count_tokens(&text);

        let result = apply_budget_to_text(&text, 20, CompactionStrategy::HeadOnly);
        assert!(result.truncated);
        assert_eq!(result.original_tokens, original_count);
        assert!(result.tokens < result.original_tokens);
    }

    #[test]
    fn test_reduce_value_truncates_strings() {
        let value = serde_json::json!({"content": "a".repeat(1000)});
        let reduced = json_analysis::reduce_value(&value, 50, 100);
        let s = reduced["content"].as_str().unwrap();
        assert!(s.len() < 200);
        assert!(s.contains("[truncated]"));
    }

    #[test]
    fn test_reduce_value_truncates_arrays() {
        let value = serde_json::json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let reduced = json_analysis::reduce_value(&value, 1000, 3);
        let arr = reduced.as_array().unwrap();
        assert_eq!(arr.len(), 4); // 3 items + truncation sentinel
        let sentinel = &arr[3];
        assert_eq!(sentinel["_truncated"], true);
        assert_eq!(sentinel["remaining"], 7);
    }

    #[test]
    fn test_render_markdown_tree() {
        let value = serde_json::json!({
            "root": "src",
            "entries": [
                {"path": "cli", "type": "dir", "depth": 1},
                {"path": "cli/mod.rs", "type": "file", "depth": 2},
                {"path": "main.rs", "type": "file", "depth": 1}
            ],
            "total_files": 2,
            "total_dirs": 1
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Tree: src"));
        assert!(md.contains("2 files, 1 directories"));
        assert!(md.contains("cli/"));
        assert!(md.contains("mod.rs"));
        assert!(md.contains("main.rs"));
    }

    #[test]
    fn test_render_markdown_code_read() {
        let value = serde_json::json!({
            "path": "src/main.rs",
            "language": "rust",
            "lines": 100,
            "symbols": [
                {"name": "main", "kind": "function", "line_start": 1, "line_end": 10}
            ],
            "content": "fn main() {\n    println!(\"hello\");\n}",
            "tokens": 20,
            "truncated": false
        });
        let md = render_markdown(&value);
        assert!(md.contains("## src/main.rs"));
        assert!(md.contains("**rust**"));
        assert!(md.contains("100 lines"));
        assert!(md.contains("| `main` | function | 1-10 |"));
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main()"));
    }

    #[test]
    fn test_render_markdown_grep() {
        let value = serde_json::json!({
            "pattern": "TODO",
            "matches": [
                {
                    "file": "main.rs",
                    "line_number": 42,
                    "content": "// TODO: fix this",
                    "context_before": [],
                    "context_after": []
                }
            ],
            "total_matches": 1,
            "files_searched": 5
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Grep: `TODO`"));
        assert!(md.contains("1 matches across 5 files"));
        assert!(md.contains("### main.rs"));
        assert!(md.contains("**42:** `// TODO: fix this`"));
    }

    #[test]
    fn test_render_markdown_symbols() {
        let value = serde_json::json!([
            {"name": "Config", "kind": "struct", "line_start": 10, "line_end": 20, "byte_start": 0, "byte_end": 100},
            {"name": "new", "kind": "function", "line_start": 22, "line_end": 30, "byte_start": 0, "byte_end": 100}
        ]);
        let md = render_markdown(&value);
        assert!(md.contains("## Symbols"));
        assert!(md.contains("| `Config` | struct | 10-20 |"));
        assert!(md.contains("| `new` | function | 22-30 |"));
    }

    #[test]
    fn test_render_markdown_fs_ls() {
        let value = serde_json::json!({
            "entries": [
                {"name": "src", "path": "src", "type": "dir", "size": 96},
                {"name": "main.rs", "path": "main.rs", "type": "file", "size": 1500}
            ],
            "total": 2
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Directory Listing"));
        assert!(md.contains("2 entries"));
        assert!(md.contains("| `src` | dir |"));
        assert!(md.contains("| `main.rs` | file |"));
    }

    #[test]
    fn test_render_markdown_edit() {
        let value = serde_json::json!({
            "path": "file.rs",
            "format": "search-replace",
            "applied": true,
            "validated": true,
            "diff": "- old\n+ new",
            "lines_changed": 1
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Edit: file.rs"));
        assert!(md.contains("**Applied:** true"));
        assert!(md.contains("```diff"));
        assert!(md.contains("- old"));
    }

    #[test]
    fn test_render_markdown_index_build() {
        let value = serde_json::json!({
            "files_indexed": 19,
            "files_skipped": 0,
            "symbols_added": 429,
            "refs_added": 780
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Index Built"));
        assert!(md.contains("**files indexed:** 19"));
        assert!(md.contains("**symbols added:** 429"));
    }

    #[test]
    fn test_render_markdown_index_status() {
        let value = serde_json::json!({
            "path": ".that-tools/index.db",
            "total_files": 19,
            "total_symbols": 429,
            "total_refs": 780,
            "stale_files": 0,
            "schema_version": "1"
        });
        let md = render_markdown(&value);
        assert!(md.contains("## Index Status"));
        assert!(md.contains("**total symbols:** 429"));
    }

    #[test]
    fn test_format_size() {
        assert_eq!(render::format_size(500), "500 B");
        assert_eq!(render::format_size(1536), "1.5 KB");
        assert_eq!(render::format_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn test_compact_json_value_tree_strips_depth() {
        let value = serde_json::json!({
            "root": "src",
            "entries": [
                {"path": "main.rs", "type": "file", "depth": 1},
                {"path": "lib.rs", "type": "file", "depth": 1}
            ],
            "total_files": 2,
            "total_dirs": 0
        });
        let compacted = compact_json_value(&value);
        let entries = compacted["entries"].as_array().unwrap();
        for entry in entries {
            assert!(
                entry.get("depth").is_none(),
                "depth should be stripped in compact"
            );
            assert!(entry.get("path").is_some(), "path should be preserved");
        }
    }

    #[test]
    fn test_compact_json_value_symbols_strips_bytes() {
        let value = serde_json::json!([
            {"name": "Config", "kind": "struct", "line_start": 10, "line_end": 20, "byte_start": 0, "byte_end": 100}
        ]);
        let compacted = compact_json_value(&value);
        let arr = compacted.as_array().unwrap();
        let sym = &arr[0];
        assert!(
            sym.get("byte_start").is_none(),
            "byte_start should be stripped"
        );
        assert!(sym.get("byte_end").is_none(), "byte_end should be stripped");
        assert!(sym.get("name").is_some(), "name should be preserved");
    }

    #[test]
    fn test_compact_json_value_grep_strips_context() {
        let value = serde_json::json!({
            "pattern": "TODO",
            "matches": [
                {
                    "file": "main.rs",
                    "line_number": 42,
                    "content": "// TODO: fix",
                    "context_before": ["line before"],
                    "context_after": ["line after"],
                    "captures": {}
                }
            ],
            "total_matches": 1,
            "files_searched": 5
        });
        let compacted = compact_json_value(&value);
        let m = &compacted["matches"][0];
        assert!(
            m.get("context_before").is_none(),
            "context_before should be stripped"
        );
        assert!(
            m.get("context_after").is_none(),
            "context_after should be stripped"
        );
        assert!(
            m.get("captures").is_none(),
            "empty captures should be stripped"
        );
        assert!(m.get("file").is_some(), "file should be preserved");
    }

    #[test]
    fn test_render_raw_tree() {
        let value = serde_json::json!({
            "root": "src",
            "entries": [
                {"path": "cli", "type": "dir", "depth": 1},
                {"path": "cli/mod.rs", "type": "file", "depth": 2},
                {"path": "main.rs", "type": "file", "depth": 1}
            ],
            "total_files": 2,
            "total_dirs": 1
        });
        let raw = render_raw(&value);
        assert!(raw.contains("cli/"));
        assert!(raw.contains("cli/mod.rs"));
        assert!(raw.contains("main.rs"));
        // Should not contain JSON characters
        assert!(!raw.contains('{'));
    }

    #[test]
    fn test_render_raw_symbols() {
        let value = serde_json::json!([
            {"name": "Config", "kind": "struct", "line_start": 10, "line_end": 20, "byte_start": 0, "byte_end": 100},
            {"name": "new", "kind": "function", "line_start": 22, "line_end": 30, "byte_start": 0, "byte_end": 100}
        ]);
        let raw = render_raw(&value);
        assert!(raw.contains("Config (struct) L:10-20"));
        assert!(raw.contains("new (function) L:22-30"));
    }

    #[test]
    fn test_render_raw_grep() {
        let value = serde_json::json!({
            "pattern": "TODO",
            "matches": [
                {
                    "file": "main.rs",
                    "line_number": 42,
                    "content": "// TODO: fix this",
                    "context_before": [],
                    "context_after": []
                }
            ],
            "total_matches": 1,
            "files_searched": 5
        });
        let raw = render_raw(&value);
        assert!(raw.contains("main.rs:42: // TODO: fix this"));
    }

    #[test]
    fn test_render_raw_fs_ls() {
        let value = serde_json::json!({
            "entries": [
                {"name": "src", "path": "src", "type": "dir", "size": 96},
                {"name": "main.rs", "path": "main.rs", "type": "file", "size": 1500}
            ],
            "total": 2
        });
        let raw = render_raw(&value);
        assert!(raw.contains("src\tdir\t96"));
        assert!(raw.contains("main.rs\tfile\t1500"));
    }

    #[test]
    fn test_render_raw_code_read() {
        let value = serde_json::json!({
            "path": "src/main.rs",
            "language": "rust",
            "lines": 10,
            "symbols": [],
            "content": "fn main() {\n    println!(\"hello\");\n}",
            "tokens": 20,
            "truncated": false
        });
        let raw = render_raw(&value);
        assert_eq!(raw, "fn main() {\n    println!(\"hello\");\n}");
    }

    #[test]
    fn test_render_raw_index() {
        let value = serde_json::json!({
            "files_indexed": 19,
            "files_skipped": 0,
            "symbols_added": 429,
            "refs_added": 780
        });
        let raw = render_raw(&value);
        assert!(raw.contains("files_indexed: 19"));
        assert!(raw.contains("symbols_added: 429"));
    }
}
