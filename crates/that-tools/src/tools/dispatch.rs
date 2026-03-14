//! Unified tool dispatch for CLI and MCP.
//!
//! Provides a single entry point for executing any tool request.
//! The public API (`ToolRequest`, `ToolResponse`, `execute_tool`) is the
//! programmatic/SDK entry point used directly by agent runtimes (NativeTool)
//! and the unified CLI.

use crate::config::{PolicyLevel, ThatToolsConfig};
use crate::output::{self, BudgetedOutput};
use serde::{Deserialize, Serialize};

/// A tool invocation request with all parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool", content = "params")]
pub enum ToolRequest {
    // File system
    FsLs {
        path: String,
        max_depth: Option<usize>,
    },
    FsCat {
        path: String,
    },
    FsWrite {
        path: String,
        content: String,
        dry_run: bool,
        backup: bool,
    },
    FsMkdir {
        path: String,
        parents: bool,
    },
    FsRm {
        path: String,
        recursive: bool,
        dry_run: bool,
    },
    // Code
    CodeRead {
        path: String,
        context: Option<usize>,
        symbols: bool,
        line: Option<usize>,
        end_line: Option<usize>,
    },
    CodeGrep {
        pattern: String,
        path: String,
        context: Option<usize>,
        limit: Option<usize>,
        ignore_case: bool,
        regex: bool,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
    },
    CodeTree {
        path: String,
        depth: Option<usize>,
        compact: bool,
        ranked: bool,
    },
    CodeSymbols {
        path: String,
        kind: Option<String>,
        name: Option<String>,
    },
    CodeSummary {
        path: String,
    },
    CodeEdit {
        path: String,
        search: Option<String>,
        replace: Option<String>,
        target_fn: Option<String>,
        new_body: Option<String>,
        #[serde(default)]
        all: bool,
        #[serde(default)]
        dry_run: bool,
    },
    // Memory
    MemAdd {
        content: String,
        tags: Vec<String>,
        source: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
    MemSearch {
        query: String,
        tags: Option<Vec<String>>,
        limit: Option<usize>,
        #[serde(default)]
        session_id: Option<String>,
    },
    MemCompact {
        summary: String,
        #[serde(default)]
        session_id: Option<String>,
    },
    MemUnpin {
        id: String,
    },
    MemRemove {
        id: String,
    },
    // Search
    SearchQuery {
        query: String,
        engine: Option<String>,
        limit: usize,
        no_cache: bool,
    },
    SearchFetch {
        urls: Vec<String>,
        mode: String,
    },
    // Human
    HumanAsk {
        message: String,
        timeout: Option<u64>,
    },
    // Image
    FsImageRead {
        path: String,
    },
    // Exec
    ShellExec {
        command: String,
        cwd: Option<String>,
        timeout_secs: u64,
        signal: Option<crate::tools::exec::SignalMode>,
    },
}

/// A tool execution response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResponse {
    pub success: bool,
    pub output: serde_json::Value,
    pub tokens: usize,
    /// Original token count before any budget truncation (0 when not truncated).
    #[serde(default)]
    pub original_tokens: usize,
    pub truncated: bool,
}

impl ToolResponse {
    pub fn from_budgeted(budgeted: &BudgetedOutput) -> Self {
        let output = serde_json::from_str(&budgeted.content)
            .unwrap_or(serde_json::Value::String(budgeted.content.clone()));
        Self {
            success: true,
            output,
            tokens: budgeted.tokens,
            original_tokens: budgeted.original_tokens,
            truncated: budgeted.truncated,
        }
    }

    pub fn error(message: &str) -> Self {
        Self::error_with_code(message, "EXECUTION_FAILED")
    }

    pub fn error_with_code(message: &str, code: &str) -> Self {
        Self {
            success: false,
            output: serde_json::json!({"error": message, "code": code}),
            tokens: 0,
            original_tokens: 0,
            truncated: false,
        }
    }
}

/// Map a tool request to its policy name for enforcement.
fn policy_name_for(request: &ToolRequest) -> &'static str {
    match request {
        ToolRequest::FsLs { .. } | ToolRequest::FsCat { .. } | ToolRequest::FsImageRead { .. } => {
            "fs_read"
        }
        ToolRequest::FsWrite { .. } | ToolRequest::FsMkdir { .. } => "fs_write",
        ToolRequest::FsRm { .. } => "fs_delete",
        ToolRequest::CodeRead { .. }
        | ToolRequest::CodeGrep { .. }
        | ToolRequest::CodeTree { .. }
        | ToolRequest::CodeSymbols { .. }
        | ToolRequest::CodeSummary { .. } => "code_read",
        ToolRequest::CodeEdit { .. } => "code_edit",
        ToolRequest::MemAdd { .. }
        | ToolRequest::MemSearch { .. }
        | ToolRequest::MemUnpin { .. }
        | ToolRequest::MemRemove { .. } => "memory",
        ToolRequest::MemCompact { .. } => "mem_compact",
        ToolRequest::SearchQuery { .. } | ToolRequest::SearchFetch { .. } => "search",
        ToolRequest::HumanAsk { .. } => "memory", // HITL uses default policy
        ToolRequest::ShellExec { .. } => "shell_exec",
    }
}

/// Check if a tool is allowed by policy. Returns an error response if denied.
fn check_policy(config: &ThatToolsConfig, request: &ToolRequest) -> Option<ToolResponse> {
    let name = policy_name_for(request);
    let level = match name {
        "code_read" => &config.policy.tools.code_read,
        "code_edit" => &config.policy.tools.code_edit,
        "fs_read" => &config.policy.tools.fs_read,
        "fs_write" => &config.policy.tools.fs_write,
        "fs_delete" => &config.policy.tools.fs_delete,
        "shell_exec" => &config.policy.tools.shell_exec,
        "search" => &config.policy.tools.search,
        "memory" => &config.policy.tools.memory,
        "mem_compact" => &config.policy.tools.mem_compact,
        "git_commit" => &config.policy.tools.git_commit,
        "git_push" => &config.policy.tools.git_push,
        _ => &config.policy.default,
    };
    match level {
        PolicyLevel::Allow => None,
        PolicyLevel::Prompt => {
            // When called via dispatch (MCP/programmatic), Prompt behaves as Deny
            // since there's no interactive terminal to prompt the user.
            // CLI handles Prompt differently via ToolContext::check_policy.
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                None // interactive — allow (CLI already prompts)
            } else {
                Some(ToolResponse::error_with_code(
                    &format!(
                        "policy denied: tool '{}' requires approval but no interactive terminal is available",
                        name
                    ),
                    "POLICY_PROMPT_REQUIRED",
                ))
            }
        }
        PolicyLevel::Deny => Some(ToolResponse::error_with_code(
            &format!(
                "policy denied: tool '{}' is not allowed by current policy",
                name
            ),
            "POLICY_DENIED",
        )),
    }
}

/// Execute a tool request with the given configuration and token budget.
pub fn execute_tool(
    config: &ThatToolsConfig,
    request: &ToolRequest,
    max_tokens: Option<usize>,
) -> ToolResponse {
    // Enforce policy before executing any tool
    if let Some(denied) = check_policy(config, request) {
        return denied;
    }
    match request {
        ToolRequest::FsLs { path, max_depth } => {
            let depth = max_depth.or(Some(config.output.fs_ls_max_depth));
            match crate::tools::fs::ls(std::path::Path::new(path), depth, max_tokens) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::FsCat { path } => {
            match crate::tools::fs::cat(std::path::Path::new(path), max_tokens) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::CodeRead {
            path,
            context,
            symbols,
            line,
            end_line,
        } => {
            let ctx_lines = context.or(Some(config.output.code_read_context_lines));
            match crate::tools::code::code_read(
                std::path::Path::new(path),
                ctx_lines,
                *symbols,
                max_tokens,
                *line,
                *end_line,
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::CodeGrep {
            pattern,
            path,
            context,
            limit,
            ignore_case,
            regex,
            include,
            exclude,
        } => {
            match crate::tools::code::code_grep_filtered_with_options(
                std::path::Path::new(path),
                pattern,
                *context,
                max_tokens,
                *limit,
                *ignore_case,
                *regex,
                include,
                exclude,
                crate::tools::code::GrepRuntimeOptions {
                    workers: Some(config.code.grep_workers),
                    mmap_min_bytes: Some(config.code.mmap_min_bytes),
                },
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::CodeTree {
            path,
            depth,
            compact,
            ranked,
        } => {
            let tree_depth = depth.or(Some(config.output.code_tree_max_depth));
            match crate::tools::code::code_tree(
                std::path::Path::new(path),
                tree_depth,
                max_tokens,
                *compact,
                *ranked,
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::CodeSymbols { path, kind, name } => {
            #[cfg(feature = "code-analysis")]
            {
                use crate::tools::code::parse;
                let p = std::path::Path::new(path);
                let symbols_result = if p.is_file() {
                    parse::parse_file(p).map(|parsed| parsed.symbols)
                } else {
                    let mut all = Vec::new();
                    let walker = ignore::WalkBuilder::new(p)
                        .hidden(true)
                        .git_ignore(true)
                        .build();
                    for entry in walker.flatten() {
                        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                            continue;
                        }
                        if parse::Language::from_path(entry.path()).is_some() {
                            if let Ok(parsed) = parse::parse_file(entry.path()) {
                                for mut sym in parsed.symbols {
                                    let rel = entry
                                        .path()
                                        .strip_prefix(p)
                                        .unwrap_or(entry.path())
                                        .to_string_lossy();
                                    sym.name = format!("{}:{}", rel, sym.name);
                                    all.push(sym);
                                }
                            }
                        }
                    }
                    Ok(all)
                };

                match symbols_result {
                    Ok(symbols) => {
                        let filtered: Vec<_> = symbols
                            .into_iter()
                            .filter(|s| {
                                if let Some(k) = kind.as_deref() {
                                    let kind_str = format!("{:?}", s.kind).to_lowercase();
                                    if !kind_str.contains(&k.to_lowercase()) {
                                        return false;
                                    }
                                }
                                if let Some(n) = name.as_deref() {
                                    if !s.name.to_lowercase().contains(&n.to_lowercase()) {
                                        return false;
                                    }
                                }
                                true
                            })
                            .collect();
                        let result = output::emit_json(&filtered, max_tokens);
                        ToolResponse::from_budgeted(&result)
                    }
                    Err(e) => ToolResponse::error(&e.to_string()),
                }
            }
            #[cfg(not(feature = "code-analysis"))]
            {
                let _ = (path, kind, name);
                ToolResponse::error("code-analysis feature is not enabled")
            }
        }
        ToolRequest::CodeSummary { path } => {
            #[cfg(feature = "code-analysis")]
            {
                match crate::tools::code::summary::code_summary(
                    std::path::Path::new(path),
                    max_tokens,
                ) {
                    Ok(result) => ToolResponse::from_budgeted(&result),
                    Err(e) => ToolResponse::error(&e.to_string()),
                }
            }
            #[cfg(not(feature = "code-analysis"))]
            {
                let _ = path;
                ToolResponse::error("code-analysis feature is not enabled")
            }
        }
        ToolRequest::CodeEdit {
            path,
            search,
            replace,
            target_fn,
            new_body,
            all,
            dry_run,
        } => {
            #[cfg(feature = "code-analysis")]
            {
                use crate::tools::code::edit;

                let format = if let (Some(s), Some(r)) = (search.as_ref(), replace.as_ref()) {
                    edit::EditFormat::SearchReplace {
                        search: s.clone(),
                        replace: r.clone(),
                        all: *all,
                    }
                } else if let (Some(name), Some(body)) = (target_fn.as_ref(), new_body.as_ref()) {
                    edit::EditFormat::AstNode {
                        symbol_name: name.clone(),
                        new_body: body.clone(),
                    }
                } else {
                    return ToolResponse::error(
                        "invalid code_edit args: specify search+replace or target_fn+new_body",
                    );
                };

                match edit::code_edit(std::path::Path::new(path), &format, *dry_run, max_tokens) {
                    Ok(result) => ToolResponse::from_budgeted(&result),
                    Err(e) => ToolResponse::error(&e.to_string()),
                }
            }
            #[cfg(not(feature = "code-analysis"))]
            {
                let _ = (path, search, replace, target_fn, new_body, all, dry_run);
                ToolResponse::error("code-analysis feature is not enabled")
            }
        }
        ToolRequest::MemAdd {
            content,
            tags,
            source,
            session_id,
        } => match crate::tools::memory::add(
            content,
            tags,
            source.as_deref(),
            session_id.as_deref(),
            &config.memory,
        ) {
            Ok(result) => {
                // Return only the ID — echoing the full content back bloats the context
                // and causes models to skip producing a user-facing confirmation.
                let compact = serde_json::json!({ "stored": true, "id": result.id });
                let budgeted = output::emit_json(&compact, max_tokens);
                ToolResponse::from_budgeted(&budgeted)
            }
            Err(e) => ToolResponse::error(&e.to_string()),
        },
        ToolRequest::MemSearch {
            query,
            tags,
            limit,
            session_id,
        } => {
            match crate::tools::memory::search(
                query,
                tags.as_deref(),
                limit.unwrap_or(10),
                session_id.as_deref(),
                &config.memory,
            ) {
                Ok(result) => {
                    let budgeted = output::emit_json(&result, max_tokens);
                    ToolResponse::from_budgeted(&budgeted)
                }
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::MemCompact {
            summary,
            session_id,
        } => {
            match crate::tools::memory::compact(summary, session_id.as_deref(), &config.memory) {
                Ok(mut result) => {
                    // Wire session reset + compaction count increment.
                    if let Some(ref sid) = session_id {
                        let sessions_path = if config.session.sessions_path.is_empty() {
                            crate::tools::session::default_sessions_path()
                        } else {
                            std::path::PathBuf::from(&config.session.sessions_path)
                        };
                        if crate::tools::session::reset_context(sid, 0, &sessions_path).is_ok() {
                            result.context_tokens_reset = true;
                        }
                        if let Ok(()) =
                            crate::tools::session::increment_compaction(sid, &sessions_path)
                        {
                            if let Ok(stats) = crate::tools::session::get_stats(
                                sid,
                                &sessions_path,
                                config.session.soft_threshold_tokens,
                            ) {
                                result.compaction_count = Some(stats.compaction_count);
                            }
                        }
                    }
                    let budgeted = output::emit_json(&result, max_tokens);
                    ToolResponse::from_budgeted(&budgeted)
                }
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::MemUnpin { id } => match crate::tools::memory::unpin(id, &config.memory) {
            Ok(result) => {
                let budgeted = output::emit_json(&result, max_tokens);
                ToolResponse::from_budgeted(&budgeted)
            }
            Err(e) => ToolResponse::error(&e.to_string()),
        },
        ToolRequest::MemRemove { id } => match crate::tools::memory::remove(id, &config.memory) {
            Ok(result) => {
                let budgeted = output::emit_json(&result, max_tokens);
                ToolResponse::from_budgeted(&budgeted)
            }
            Err(e) => ToolResponse::error(&e.to_string()),
        },
        ToolRequest::SearchQuery {
            query,
            engine,
            limit,
            no_cache,
        } => {
            match crate::tools::search::search(
                query,
                engine.as_deref(),
                *limit,
                *no_cache,
                &config.search,
                max_tokens,
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::SearchFetch { urls, mode } => {
            match crate::tools::search::fetch(urls, mode, max_tokens) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::FsImageRead { path } => {
            match crate::tools::fs::image::image_read(std::path::Path::new(path)) {
                Ok(result) => {
                    let meta = crate::tools::fs::image::ImageReadMeta::from(&result);
                    let output = serde_json::to_value(&meta).unwrap_or_default();
                    ToolResponse {
                        success: true,
                        output,
                        tokens: 0,
                        original_tokens: 0,
                        truncated: false,
                    }
                }
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::HumanAsk { message, timeout } => {
            match crate::tools::human::ask(message, *timeout) {
                Ok(response) => {
                    let budgeted = output::emit_json(&response, max_tokens);
                    ToolResponse::from_budgeted(&budgeted)
                }
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::FsWrite {
            path,
            content,
            dry_run,
            backup,
        } => {
            match crate::tools::fs::write(
                std::path::Path::new(path),
                content,
                *dry_run,
                *backup,
                max_tokens,
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::FsMkdir { path, parents } => {
            match crate::tools::fs::mkdir(std::path::Path::new(path), *parents, max_tokens) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::FsRm {
            path,
            recursive,
            dry_run,
        } => {
            match crate::tools::fs::rm(std::path::Path::new(path), *recursive, *dry_run, max_tokens)
            {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
        ToolRequest::ShellExec {
            command,
            cwd,
            timeout_secs,
            signal,
        } => {
            let signal_mode = signal.unwrap_or_default();
            match crate::tools::exec::exec_with_options(
                command,
                cwd.as_deref(),
                *timeout_secs,
                max_tokens,
                signal_mode,
                false, // MCP path never streams to stderr
            ) {
                Ok(result) => ToolResponse::from_budgeted(&result),
                Err(e) => ToolResponse::error(&e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_request_serialization() {
        let req = ToolRequest::FsLs {
            path: ".".to_string(),
            max_depth: Some(2),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("FsLs"));
        let _: ToolRequest = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_tool_response_error() {
        let resp = ToolResponse::error("test error");
        assert!(!resp.success);
        assert_eq!(resp.output["error"], "test error");
    }

    #[cfg(feature = "code-analysis")]
    #[test]
    fn test_dispatch_code_summary() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("lib.rs"), "pub fn hello() {}\n").unwrap();

        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeSummary {
            path: tmp.path().to_string_lossy().to_string(),
        };
        let resp = execute_tool(&config, &req, None);
        assert!(resp.success, "code_summary dispatch should succeed");
        assert!(
            resp.output.get("modules").is_some() || resp.output.to_string().contains("modules"),
            "response should contain modules"
        );
    }

    #[cfg(feature = "code-analysis")]
    #[test]
    fn test_dispatch_code_summary_not_found() {
        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeSummary {
            path: "/nonexistent/path".to_string(),
        };
        let resp = execute_tool(&config, &req, None);
        assert!(!resp.success, "should fail for non-existent path");
    }

    #[test]
    fn test_dispatch_code_grep_with_include() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();
        std::fs::write(tmp.path().join("data.txt"), "fn hello() {}\n").unwrap();

        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeGrep {
            pattern: "fn hello".to_string(),
            path: tmp.path().to_string_lossy().to_string(),
            context: None,
            limit: None,
            ignore_case: false,
            regex: false,
            include: vec!["*.rs".to_string()],
            exclude: vec![],
        };
        let resp = execute_tool(&config, &req, None);
        assert!(resp.success);
        let output_str = resp.output.to_string();
        assert!(output_str.contains("main.rs"), "should match .rs file");
        assert!(
            !output_str.contains("data.txt"),
            "should not match .txt file with --include *.rs"
        );
    }

    #[test]
    fn test_dispatch_code_grep_with_exclude() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();
        std::fs::write(tmp.path().join("main_test.rs"), "fn hello() {}\n").unwrap();

        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeGrep {
            pattern: "fn hello".to_string(),
            path: tmp.path().to_string_lossy().to_string(),
            context: None,
            limit: None,
            ignore_case: false,
            regex: false,
            include: vec![],
            exclude: vec!["*test*".to_string()],
        };
        let resp = execute_tool(&config, &req, None);
        assert!(resp.success);
        let output_str = resp.output.to_string();
        assert!(output_str.contains("main.rs"));
        assert!(
            !output_str.contains("main_test.rs"),
            "test file should be excluded"
        );
    }

    #[test]
    fn test_dispatch_code_grep_empty_results() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "fn hello() {}\n").unwrap();

        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeGrep {
            pattern: "NONEXISTENT_PATTERN_XYZ".to_string(),
            path: tmp.path().to_string_lossy().to_string(),
            context: None,
            limit: None,
            ignore_case: false,
            regex: false,
            include: vec![],
            exclude: vec![],
        };
        let resp = execute_tool(&config, &req, None);
        assert!(resp.success, "grep with no matches should still succeed");
        let output_str = resp.output.to_string();
        assert!(
            output_str.contains("\"total_matches\":0")
                || output_str.contains("\"total_matches\": 0")
        );
    }

    #[cfg(feature = "code-analysis")]
    #[test]
    fn test_dispatch_code_summary_with_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src").join("lib.rs"),
            "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
        )
        .unwrap();

        let config = ThatToolsConfig::default();
        let req = ToolRequest::CodeSummary {
            path: tmp.path().to_string_lossy().to_string(),
        };
        let resp = execute_tool(&config, &req, Some(20));
        assert!(resp.success, "should succeed even with tiny budget");
        // Output should be valid JSON
        let json_str = serde_json::to_string(&resp.output).unwrap();
        assert!(!json_str.is_empty());
    }

    #[test]
    fn test_dispatch_policy_denies_code_read() {
        let mut config = ThatToolsConfig::default();
        config.policy.tools.code_read = PolicyLevel::Deny;

        let req = ToolRequest::CodeSummary {
            path: ".".to_string(),
        };
        let resp = execute_tool(&config, &req, None);
        assert!(!resp.success, "should be denied by policy");
        assert!(resp.output.to_string().contains("policy denied"));
    }

    #[test]
    fn test_code_summary_request_serialization() {
        let req = ToolRequest::CodeSummary {
            path: "/some/path".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("CodeSummary"));
        let _: ToolRequest = serde_json::from_str(&json).unwrap();
    }
}
