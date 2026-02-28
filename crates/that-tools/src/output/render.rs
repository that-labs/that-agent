/// Strip non-essential fields from JSON output for compact format.
///
/// Detects the command type by key signatures and removes fields that
/// add tokens without adding useful information for agents:
/// - Tree entries: remove `depth`
/// - Symbols: remove `byte_start`, `byte_end`
/// - Grep/ast-grep matches: remove `context_before`, `context_after`, empty `captures`
pub fn compact_json_value(value: &serde_json::Value) -> serde_json::Value {
    if let Some(obj) = value.as_object() {
        // Tree result: strip depth from entries
        if obj.contains_key("root")
            && obj.contains_key("entries")
            && obj.contains_key("total_files")
        {
            let mut out = obj.clone();
            if let Some(entries) = out.get_mut("entries").and_then(|v| v.as_array_mut()) {
                for entry in entries.iter_mut() {
                    if let Some(o) = entry.as_object_mut() {
                        o.remove("depth");
                    }
                }
            }
            return serde_json::Value::Object(out);
        }
        // Grep/ast-grep result: strip context and deprecated fields in compact mode
        if obj.contains_key("pattern")
            && obj.contains_key("matches")
            && obj.contains_key("total_matches")
        {
            let mut out = obj.clone();
            // Strip context from file_matches groups
            if let Some(groups) = out.get_mut("file_matches").and_then(|v| v.as_array_mut()) {
                for group in groups.iter_mut() {
                    if let Some(group_matches) =
                        group.get_mut("matches").and_then(|v| v.as_array_mut())
                    {
                        for m in group_matches.iter_mut() {
                            if let Some(o) = m.as_object_mut() {
                                o.remove("context_before");
                                o.remove("context_after");
                            }
                        }
                    }
                }
                // In compact mode with file_matches, strip the deprecated flat matches field
                out.remove("matches");
            } else {
                // Legacy format without file_matches — strip context from flat matches
                if let Some(matches) = out.get_mut("matches").and_then(|v| v.as_array_mut()) {
                    for m in matches.iter_mut() {
                        if let Some(o) = m.as_object_mut() {
                            o.remove("context_before");
                            o.remove("context_after");
                            if let Some(caps) = o.get("captures") {
                                if caps.as_object().is_some_and(|c| c.is_empty()) {
                                    o.remove("captures");
                                }
                            }
                        }
                    }
                }
            }
            return serde_json::Value::Object(out);
        }
    }
    // Search results: strip score, source from individual results
    if let Some(obj) = value.as_object() {
        if obj.contains_key("query")
            && obj.contains_key("results")
            && obj.contains_key("total_results")
        {
            let mut out = obj.clone();
            if let Some(results) = out.get_mut("results").and_then(|v| v.as_array_mut()) {
                for r in results.iter_mut() {
                    if let Some(o) = r.as_object_mut() {
                        o.remove("score");
                        o.remove("source");
                    }
                }
            }
            return serde_json::Value::Object(out);
        }
    }
    // Symbol arrays: strip byte_start, byte_end
    if let Some(arr) = value.as_array() {
        if arr
            .first()
            .is_some_and(|v| v.get("kind").is_some() && v.get("line_start").is_some())
        {
            let compacted: Vec<serde_json::Value> = arr
                .iter()
                .map(|v| {
                    if let Some(o) = v.as_object() {
                        let mut out = o.clone();
                        out.remove("byte_start");
                        out.remove("byte_end");
                        serde_json::Value::Object(out)
                    } else {
                        v.clone()
                    }
                })
                .collect();
            return serde_json::Value::Array(compacted);
        }
    }
    value.clone()
}

/// Render a JSON value as plain text suitable for piping and simple consumption.
///
/// Detects the command type from the JSON structure and renders as:
/// - Tree: newline-separated paths (dirs suffixed with `/`)
/// - Symbols: `name (kind) L:start-end` per line
/// - Grep/ast-grep: `file:line: content` per line
/// - fs ls: `name\ttype\tsize` per line
/// - Code read/fs cat: extract `content` field
/// - Index/edit: `key: value` per line
/// - Generic: fallback key-value rendering
pub fn render_raw(value: &serde_json::Value) -> String {
    if let Some(obj) = value.as_object() {
        // Tree result
        if obj.contains_key("root")
            && obj.contains_key("entries")
            && obj.contains_key("total_files")
        {
            return render_raw_tree(obj);
        }
        // Code read: extract content
        if obj.contains_key("path") && obj.contains_key("language") && obj.contains_key("content") {
            return obj
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        // Grep/ast-grep result (detect by file_matches or flat matches)
        if obj.contains_key("pattern")
            && (obj.contains_key("file_matches") || obj.contains_key("matches"))
            && obj.contains_key("total_matches")
        {
            return render_raw_grep(obj);
        }
        // fs ls
        if obj.contains_key("entries") && obj.contains_key("total") {
            return render_raw_fs_ls(obj);
        }
        // Search results
        if obj.contains_key("query")
            && obj.contains_key("results")
            && obj.contains_key("total_results")
        {
            return render_raw_search(obj);
        }
        // Fetch result
        if obj.contains_key("url") && obj.contains_key("content") && obj.contains_key("word_count")
        {
            return obj
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        // fs cat
        if obj.contains_key("path") && obj.contains_key("content") {
            return obj
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        // Edit result
        if obj.contains_key("applied") && obj.contains_key("validated") {
            return render_raw_kv(obj);
        }
        // Index build/status and other objects: key-value
        return render_raw_kv(obj);
    }
    // Symbol arrays
    if let Some(arr) = value.as_array() {
        if arr
            .first()
            .is_some_and(|v| v.get("kind").is_some() && v.get("line_start").is_some())
        {
            return render_raw_symbols(arr);
        }
        // Generic array
        return arr
            .iter()
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                _ => v.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}

fn render_raw_tree(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    if let Some(entries) = obj.get("entries").and_then(|v| v.as_array()) {
        for entry in entries {
            let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let etype = entry.get("type").and_then(|v| v.as_str()).unwrap_or("file");
            let suffix = if etype == "dir" { "/" } else { "" };
            lines.push(format!("{}{}", path, suffix));
        }
    }
    lines.join("\n")
}

fn render_raw_symbols(arr: &[serde_json::Value]) -> String {
    let mut lines = Vec::new();
    for sym in arr {
        if is_truncation_sentinel(sym) {
            lines.push(format_truncation_sentinel(sym));
            continue;
        }
        let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = sym.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let ls = sym.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0);
        let le = sym.get("line_end").and_then(|v| v.as_u64()).unwrap_or(0);
        let range = if le > ls {
            format!("{}-{}", ls, le)
        } else {
            format!("{}", ls)
        };
        lines.push(format!("{} ({}) L:{}", name, kind, range));
    }
    lines.join("\n")
}

fn render_raw_grep(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    // Prefer file_matches (grouped format), fall back to flat matches
    if let Some(groups) = obj.get("file_matches").and_then(|v| v.as_array()) {
        for group in groups {
            if is_truncation_sentinel(group) {
                lines.push(format_truncation_sentinel(group));
                continue;
            }
            let file = group.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            if let Some(matches) = group.get("matches").and_then(|v| v.as_array()) {
                for m in matches {
                    if is_truncation_sentinel(m) {
                        lines.push(format_truncation_sentinel(m));
                        continue;
                    }
                    let line = m.get("line_number").and_then(|v| v.as_u64()).unwrap_or(0);
                    let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    lines.push(format!("{}:{}: {}", file, line, content.trim()));
                }
            }
        }
    } else if let Some(matches) = obj.get("matches").and_then(|v| v.as_array()) {
        for m in matches {
            if is_truncation_sentinel(m) {
                lines.push(format_truncation_sentinel(m));
                continue;
            }
            let file = m.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            let line = m.get("line_number").and_then(|v| v.as_u64()).unwrap_or(0);
            let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!("{}:{}: {}", file, line, content.trim()));
        }
    }
    lines.join("\n")
}

fn render_raw_fs_ls(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    if let Some(entries) = obj.get("entries").and_then(|v| v.as_array()) {
        for entry in entries {
            if is_truncation_sentinel(entry) {
                lines.push(format_truncation_sentinel(entry));
                continue;
            }
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let etype = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            let size = entry
                .get("size")
                .and_then(|v| v.as_u64())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string());
            lines.push(format!("{}\t{}\t{}", name, etype, size));
        }
    }
    lines.join("\n")
}

fn render_raw_search(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    let query = obj.get("query").and_then(|v| v.as_str()).unwrap_or("?");
    let engine = obj.get("engine").and_then(|v| v.as_str()).unwrap_or("?");
    lines.push(format!("Search: {} ({})", query, engine));
    lines.push(String::new());
    if let Some(results) = obj.get("results").and_then(|v| v.as_array()) {
        for (i, r) in results.iter().enumerate() {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = r.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
            lines.push(format!("{}. {}", i + 1, title));
            if !url.is_empty() {
                lines.push(format!("   {}", url));
            }
            if !snippet.is_empty() {
                lines.push(format!("   {}", snippet));
            }
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn render_raw_kv(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut lines = Vec::new();
    for (key, value) in obj {
        lines.push(format!("{}: {}", key, format_json_value(value)));
    }
    lines.join("\n")
}

/// Render a JSON value as human-readable markdown.
///
/// Detects the command type from the JSON structure and renders accordingly:
/// - Tree results -> indented file listing
/// - Symbol lists -> table with name/kind/lines
/// - Code read -> file header + fenced code block
/// - Grep/ast-grep -> matches grouped by file
/// - Directory listing -> table with name/type/size
/// - Index results -> key-value list
/// - Edit results -> summary list with optional diff
/// - Generic objects/arrays -> fallback rendering
pub fn render_markdown(value: &serde_json::Value) -> String {
    if let Some(obj) = value.as_object() {
        // Detect by key signatures
        if obj.contains_key("root")
            && obj.contains_key("entries")
            && obj.contains_key("total_files")
        {
            return render_tree_md(obj);
        }
        if obj.contains_key("path") && obj.contains_key("language") && obj.contains_key("content") {
            return render_code_read_md(obj);
        }
        if obj.contains_key("pattern")
            && (obj.contains_key("file_matches") || obj.contains_key("matches"))
            && obj.contains_key("total_matches")
        {
            return render_grep_md(obj);
        }
        if obj.contains_key("entries") && obj.contains_key("total") {
            return render_fs_ls_md(obj);
        }
        if obj.contains_key("files_indexed") && obj.contains_key("symbols_added") {
            return render_index_build_md(obj);
        }
        if obj.contains_key("total_symbols") && obj.contains_key("schema_version") {
            return render_index_status_md(obj);
        }
        if obj.contains_key("applied") && obj.contains_key("validated") {
            return render_edit_md(obj);
        }
        // Search results
        if obj.contains_key("query")
            && obj.contains_key("results")
            && obj.contains_key("total_results")
        {
            return render_search_md(obj);
        }
        // Fetch result
        if obj.contains_key("url") && obj.contains_key("content") && obj.contains_key("word_count")
        {
            return render_fetch_md(obj);
        }
        if obj.contains_key("path") && obj.contains_key("content") {
            return render_fs_cat_md(obj);
        }
        // Fallback: render as key-value list
        return render_object_md(obj);
    }
    if let Some(arr) = value.as_array() {
        // Check if it's a symbol array
        if arr
            .first()
            .is_some_and(|v| v.get("kind").is_some() && v.get("line_start").is_some())
        {
            return render_symbols_md(arr);
        }
        return render_array_md(arr);
    }
    value.to_string()
}

fn render_tree_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let root = obj.get("root").and_then(|v| v.as_str()).unwrap_or(".");
    let total_files = obj.get("total_files").and_then(|v| v.as_u64()).unwrap_or(0);
    let total_dirs = obj.get("total_dirs").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut out = format!(
        "## Tree: {}\n\n{} files, {} directories\n\n```\n",
        root, total_files, total_dirs
    );

    if let Some(entries) = obj.get("entries").and_then(|v| v.as_array()) {
        for entry in entries {
            let path = entry.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let etype = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            let depth = entry.get("depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let indent = "  ".repeat(depth.saturating_sub(1));
            let suffix = if etype == "dir" { "/" } else { "" };
            // Extract just the file/dir name from the path
            let name = path.rsplit('/').next().unwrap_or(path);
            out.push_str(&format!("{}{}{}\n", indent, name, suffix));
        }
    }
    out.push_str("```\n");
    out
}

fn render_code_read_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let lang = obj.get("language").and_then(|v| v.as_str()).unwrap_or("");
    let lines = obj.get("lines").and_then(|v| v.as_u64()).unwrap_or(0);
    let tokens = obj.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let truncated = obj
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let trunc_str = if truncated { " (truncated)" } else { "" };
    let mut out = format!(
        "## {}\n\n**{}** | {} lines | {} tokens{}\n\n",
        path, lang, lines, tokens, trunc_str
    );

    // Symbols table if present and non-empty
    if let Some(symbols) = obj.get("symbols").and_then(|v| v.as_array()) {
        if !symbols.is_empty() {
            out.push_str("### Symbols\n\n| Name | Kind | Lines |\n|------|------|-------|\n");
            for sym in symbols {
                // Skip truncation sentinel objects
                if is_truncation_sentinel(sym) {
                    out.push_str(&format!("\n_{}_\n", format_truncation_sentinel(sym)));
                    continue;
                }
                let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let kind = sym.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let ls = sym
                    .get("line")
                    .or_else(|| sym.get("line_start"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let le = sym.get("line_end").and_then(|v| v.as_u64());
                let line_range = match le {
                    Some(end) if end != ls => format!("{}-{}", ls, end),
                    _ => format!("{}", ls),
                };
                out.push_str(&format!("| `{}` | {} | {} |\n", name, kind, line_range));
            }
            out.push('\n');
        }
    }

    // Code content
    if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        out.push_str(&format!("```{}\n{}\n```\n", lang, content));
    }
    out
}

fn render_grep_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let pattern = obj.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
    let total = obj
        .get("total_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let files_searched = obj
        .get("files_searched")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut out = format!(
        "## Grep: `{}`\n\n{} matches across {} files\n\n",
        pattern, total, files_searched
    );

    // Prefer file_matches (grouped format)
    if let Some(groups) = obj.get("file_matches").and_then(|v| v.as_array()) {
        for group in groups {
            if is_truncation_sentinel(group) {
                out.push_str(&format!("_{}_\n\n", format_truncation_sentinel(group)));
                continue;
            }
            let file = group.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            out.push_str(&format!("### {}\n\n", file));

            if let Some(matches) = group.get("matches").and_then(|v| v.as_array()) {
                for m in matches {
                    if is_truncation_sentinel(m) {
                        out.push_str(&format!("_{}_\n\n", format_truncation_sentinel(m)));
                        continue;
                    }
                    render_grep_match_md(&mut out, m);
                }
            }
        }
    } else if let Some(matches) = obj.get("matches").and_then(|v| v.as_array()) {
        // Fall back to flat matches (legacy or ast-grep)
        let mut current_file = String::new();
        for m in matches {
            if is_truncation_sentinel(m) {
                out.push_str(&format!("_{}_\n\n", format_truncation_sentinel(m)));
                continue;
            }
            let file = m.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            if file != current_file {
                out.push_str(&format!("### {}\n\n", file));
                current_file = file.to_string();
            }
            render_grep_match_md(&mut out, m);
        }
    }
    out
}

/// Render a single grep match in markdown (shared between grouped and flat formats).
fn render_grep_match_md(out: &mut String, m: &serde_json::Value) {
    let line = m.get("line_number").and_then(|v| v.as_u64()).unwrap_or(0);
    let content = m.get("content").and_then(|v| v.as_str()).unwrap_or("");

    if let Some(before) = m.get("context_before").and_then(|v| v.as_array()) {
        for ctx_line in before {
            if let Some(s) = ctx_line.as_str() {
                out.push_str(&format!("    {}\n", s));
            }
        }
    }
    out.push_str(&format!("**{}:** `{}`\n", line, content.trim()));
    if let Some(after) = m.get("context_after").and_then(|v| v.as_array()) {
        for ctx_line in after {
            if let Some(s) = ctx_line.as_str() {
                out.push_str(&format!("    {}\n", s));
            }
        }
    }

    // Show captures for ast-grep results
    if let Some(captures) = m.get("captures").and_then(|v| v.as_object()) {
        if !captures.is_empty() {
            let caps: Vec<String> = captures
                .iter()
                .map(|(k, v)| format!("{}=`{}`", k, v.as_str().unwrap_or("?")))
                .collect();
            out.push_str(&format!("  Captures: {}\n", caps.join(", ")));
        }
    }
    out.push('\n');
}

fn render_symbols_md(arr: &[serde_json::Value]) -> String {
    let mut out = String::from("## Symbols\n\n| Name | Kind | Lines |\n|------|------|-------|\n");
    for sym in arr {
        if is_truncation_sentinel(sym) {
            out.push_str(&format!("\n_{}_\n", format_truncation_sentinel(sym)));
            continue;
        }
        let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = sym.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let ls = sym.get("line_start").and_then(|v| v.as_u64()).unwrap_or(0);
        let le = sym.get("line_end").and_then(|v| v.as_u64()).unwrap_or(0);
        let line_range = if le > ls {
            format!("{}-{}", ls, le)
        } else {
            format!("{}", ls)
        };
        out.push_str(&format!("| `{}` | {} | {} |\n", name, kind, line_range));
    }
    out
}

fn render_fs_ls_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let total = obj.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let mut out = format!(
        "## Directory Listing\n\n{} entries\n\n| Name | Type | Size |\n|------|------|------|\n",
        total
    );

    if let Some(entries) = obj.get("entries").and_then(|v| v.as_array()) {
        for entry in entries {
            if is_truncation_sentinel(entry) {
                out.push_str(&format!("\n_{}_\n", format_truncation_sentinel(entry)));
                continue;
            }
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let etype = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            let size = entry
                .get("size")
                .and_then(|v| v.as_u64())
                .map(format_size)
                .unwrap_or_else(|| "-".to_string());
            out.push_str(&format!("| `{}` | {} | {} |\n", name, etype, size));
        }
    }
    out
}

fn render_fs_cat_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let mut out = format!("## {}\n\n", path);
    if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        out.push_str(&format!("```\n{}\n```\n", content));
    }
    out
}

fn render_index_build_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::from("## Index Built\n\n");
    for (key, value) in obj {
        let label = key.replace('_', " ");
        out.push_str(&format!("- **{}:** {}\n", label, format_json_value(value)));
    }
    out
}

fn render_index_status_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::from("## Index Status\n\n");
    for (key, value) in obj {
        let label = key.replace('_', " ");
        out.push_str(&format!("- **{}:** {}\n", label, format_json_value(value)));
    }
    out
}

fn render_edit_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let path = obj.get("path").and_then(|v| v.as_str()).unwrap_or("?");
    let applied = obj
        .get("applied")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let validated = obj
        .get("validated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let lines_changed = obj
        .get("lines_changed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let format = obj.get("format").and_then(|v| v.as_str()).unwrap_or("?");

    let mut out = format!("## Edit: {}\n\n", path);
    out.push_str(&format!("- **Format:** {}\n", format));
    out.push_str(&format!("- **Applied:** {}\n", applied));
    out.push_str(&format!("- **Validated:** {}\n", validated));
    out.push_str(&format!("- **Lines changed:** {}\n", lines_changed));

    if let Some(diff) = obj.get("diff").and_then(|v| v.as_str()) {
        if !diff.is_empty() {
            out.push_str(&format!("\n```diff\n{}\n```\n", diff));
        }
    }
    out
}

fn render_search_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let query = obj.get("query").and_then(|v| v.as_str()).unwrap_or("?");
    let engine = obj.get("engine").and_then(|v| v.as_str()).unwrap_or("?");
    let total = obj
        .get("total_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let cached = obj.get("cached").and_then(|v| v.as_bool()).unwrap_or(false);

    let cache_str = if cached { " (cached)" } else { "" };
    let mut out = format!(
        "## Search: `{}`\n\n{} results via {}{}\n\n",
        query, total, engine, cache_str
    );

    if let Some(results) = obj.get("results").and_then(|v| v.as_array()) {
        for (i, r) in results.iter().enumerate() {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("?");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = r.get("snippet").and_then(|v| v.as_str()).unwrap_or("");

            if url.is_empty() {
                out.push_str(&format!("### {}. {}\n\n", i + 1, title));
            } else {
                out.push_str(&format!("### {}. [{}]({})\n\n", i + 1, title, url));
            }
            if !snippet.is_empty() {
                out.push_str(&format!("{}\n\n", snippet));
            }
        }
    }
    out
}

fn render_fetch_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("?");
    let title = obj.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let word_count = obj.get("word_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let content = obj.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let mut out = format!("## {}\n\n", if title.is_empty() { url } else { title });
    out.push_str(&format!(
        "**Source:** {}\n**Words:** {}\n\n",
        url, word_count
    ));
    out.push_str(content);
    out.push('\n');
    out
}

fn render_object_md(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::new();
    for (key, value) in obj {
        let label = key.replace('_', " ");
        out.push_str(&format!("- **{}:** {}\n", label, format_json_value(value)));
    }
    out
}

fn render_array_md(arr: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for (i, value) in arr.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, format_json_value(value)));
    }
    out
}

/// Check if a JSON value is the array truncation sentinel.
fn is_truncation_sentinel(value: &serde_json::Value) -> bool {
    value
        .get("_truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Format the truncation sentinel for display.
fn format_truncation_sentinel(value: &serde_json::Value) -> String {
    let remaining = value.get("remaining").and_then(|v| v.as_u64()).unwrap_or(0);
    format!("...({} more items)", remaining)
}

fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        _ => value.to_string(),
    }
}

pub(crate) fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
