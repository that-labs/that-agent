//! Tool schemas and dispatch for the agentic loop.
//!
//! `all_tool_defs()` returns the JSON schemas sent to the LLM.
//! `dispatch()` routes a tool call by name to the appropriate implementation.
//!
//! In sandbox mode (`container` is `Some`) ALL filesystem tools route through
//! `docker exec` so that reads and writes operate on the container's filesystem,
//! not the host. This ensures the agent's view is consistent: files written with
//! `fs_write` are visible to `shell_exec` git commands and vice versa.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use that_tools::{
    tools::dispatch::{execute_tool, ToolRequest},
    ThatToolsConfig,
};
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::agent_loop::types::ToolDef;

const TRUSTED_LOCAL_SANDBOX_ENV: &str = "THAT_TRUSTED_LOCAL_SANDBOX";
const SANDBOX_MODE_ENV: &str = "THAT_SANDBOX_MODE";

// ─── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolError(pub String);

// ─── Internal helpers ─────────────────────────────────────────────────────────

async fn run_on_host(
    config: ThatToolsConfig,
    request: ToolRequest,
) -> Result<serde_json::Value, ToolError> {
    let max_tokens = Some(config.output.default_max_tokens);
    tokio::task::spawn_blocking(move || execute_tool(&config, &request, max_tokens))
        .await
        .map(|r| r.output)
        .map_err(|e| ToolError(e.to_string()))
}

fn sandbox_required() -> ToolError {
    ToolError(
        "sandbox required: this tool only runs inside the Docker container. \
        Start the agent in sandbox mode (do not pass --no-sandbox), or set \
        THAT_TRUSTED_LOCAL_SANDBOX=1 in a trusted Kubernetes pod."
            .to_string(),
    )
}

fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn parse_first_command_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .take(8)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn is_env_assignment_token(token: &str) -> bool {
    let Some(eq_idx) = token.find('=') else {
        return false;
    };
    if eq_idx == 0 {
        return false;
    }
    token[..eq_idx]
        .chars()
        .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn command_uses_docker_daemon(command: &str) -> bool {
    let tokens = parse_first_command_tokens(command);
    if tokens.is_empty() {
        return false;
    }
    let mut idx = 0usize;
    while idx < tokens.len() && is_env_assignment_token(&tokens[idx]) {
        idx += 1;
    }
    if idx >= tokens.len() || tokens[idx] != "docker" {
        return false;
    }
    let sub = tokens.get(idx + 1).map(String::as_str).unwrap_or("");
    match sub {
        "build" | "push" | "run" => true,
        "buildx" => matches!(tokens.get(idx + 2).map(String::as_str), Some("build")),
        "compose" => matches!(
            tokens.get(idx + 2).map(String::as_str),
            Some("build" | "up")
        ),
        _ => false,
    }
}

fn shell_exec_backend_guard(command: &str) -> Option<String> {
    if !command_uses_docker_daemon(command) {
        return None;
    }
    let backend = std::env::var("THAT_IMAGE_BUILD_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let docker_available = parse_env_bool("THAT_DOCKER_DAEMON_AVAILABLE").unwrap_or(false);

    match backend.as_str() {
        "buildkit" if !docker_available => Some(
            "Docker daemon workloads are blocked in this runtime: image_build_backend=buildkit and docker_daemon_available=false. \
Use BuildKit instead, for example: `buildctl --addr ${BUILDKIT_HOST} build --frontend dockerfile.v0 --local context=. --local dockerfile=. --opt filename=Dockerfile --output type=image,name=<registry>/<image>:<tag>,push=true`."
                .to_string(),
        ),
        "none" if !docker_available => Some(
            "Docker daemon workloads are blocked in this runtime: image_build_backend=none and docker_daemon_available=false. \
Use a prebuilt image or a Kubernetes-native build job."
                .to_string(),
        ),
        _ => None,
    }
}

pub fn trusted_local_sandbox_enabled() -> bool {
    if let Some(explicit) = parse_env_bool(TRUSTED_LOCAL_SANDBOX_ENV) {
        return explicit;
    }
    matches!(
        std::env::var(SANDBOX_MODE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    )
}

fn container_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/workspace/{path}")
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn docker_exec_sh(container: &str, cmd: &str) -> Result<serde_json::Value, ToolError> {
    let out = Command::new("docker")
        .args(["exec", container, "bash", "-c", cmd])
        .output()
        .await
        .map_err(|e| ToolError(format!("docker exec failed: {e}")))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() {
        Ok(serde_json::json!({ "output": stdout }))
    } else {
        Err(ToolError(format!(
            "exit {}: {stderr}",
            out.status.code().unwrap_or(-1)
        )))
    }
}

async fn docker_exec_write(
    container: &str,
    path: &str,
    content: &str,
) -> Result<serde_json::Value, ToolError> {
    let cpath = container_path(path);
    let safe = cpath.replace('\'', "'\\''");
    let cmd = format!("mkdir -p \"$(dirname '{safe}')\" && cat > '{safe}'");
    let mut child = Command::new("docker")
        .args(["exec", "-i", container, "bash", "-c", &cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError(format!("docker exec write failed: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .await
            .map_err(|e| ToolError(format!("stdin write error: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .await
        .map_err(|e| ToolError(format!("docker exec write wait error: {e}")))?;
    if out.status.success() {
        Ok(serde_json::json!({ "written": cpath }))
    } else {
        Err(ToolError(format!(
            "write failed (exit {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
        )))
    }
}

// ─── Args structs (used by dispatch for deserialization) ──────────────────────

#[derive(Debug, Deserialize)]
pub struct FsLsArgs {
    pub path: String,
    pub max_depth: Option<usize>,
}
#[derive(Debug, Deserialize)]
pub struct FsCatArgs {
    pub path: String,
}
#[derive(Debug, Deserialize)]
pub struct FsWriteArgs {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub backup: bool,
}
#[derive(Debug, Deserialize)]
pub struct FsMkdirArgs {
    pub path: String,
    #[serde(default)]
    pub parents: bool,
}
#[derive(Debug, Deserialize)]
pub struct FsRmArgs {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub dry_run: bool,
}
#[derive(Debug, Deserialize)]
pub struct CodeReadArgs {
    pub path: String,
    pub context: Option<usize>,
    #[serde(default)]
    pub symbols: bool,
    pub line: Option<usize>,
    pub end_line: Option<usize>,
}
#[derive(Debug, Deserialize)]
pub struct CodeGrepArgs {
    pub pattern: String,
    pub path: String,
    pub context: Option<usize>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub ignore_case: bool,
    #[serde(default)]
    pub regex: bool,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}
#[derive(Debug, Deserialize)]
pub struct CodeTreeArgs {
    pub path: String,
    pub depth: Option<usize>,
    #[serde(default)]
    pub compact: bool,
    #[serde(default)]
    pub ranked: bool,
}
#[derive(Debug, Deserialize)]
pub struct CodeSymbolsArgs {
    pub path: String,
    pub kind: Option<String>,
    pub name: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct CodeSummaryArgs {
    pub path: String,
}
#[derive(Debug, Deserialize)]
pub struct CodeEditArgs {
    pub path: String,
    pub search: Option<String>,
    pub replace: Option<String>,
    pub target_fn: Option<String>,
    pub new_body: Option<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub dry_run: bool,
}
#[derive(Debug, Deserialize)]
pub struct MemAddArgs {
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub session_id: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct MemRecallArgs {
    pub query: String,
    pub limit: Option<usize>,
    pub session_id: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct MemSearchArgs {
    pub query: String,
    pub tags: Option<Vec<String>>,
    pub limit: Option<usize>,
    pub session_id: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct MemCompactArgs {
    pub summary: String,
    pub session_id: Option<String>,
}
#[derive(Debug, Deserialize)]
pub struct SearchQueryArgs {
    pub query: String,
    pub engine: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub no_cache: bool,
}
#[derive(Debug, Deserialize)]
pub struct SearchFetchArgs {
    pub urls: Vec<String>,
    #[serde(default = "default_fetch_mode")]
    pub mode: String,
}
#[derive(Debug, Deserialize)]
pub struct HumanAskArgs {
    pub message: String,
    pub timeout: Option<u64>,
}
#[derive(Debug, Deserialize)]
pub struct ShellExecArgs {
    pub command: String,
    pub cwd: Option<String>,
    #[serde(default = "default_shell_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
pub struct ReadPluginArgs {
    pub plugin_id: Option<String>,
    #[serde(default)]
    pub include_files: bool,
    pub agent_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ValidatePluginArgs {
    pub plugin_id: Option<String>,
    pub agent_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeCreateArgs {
    pub base_repo: String,
    pub agent_name: String,
    pub branch_prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeListArgs {
    pub base_repo: String,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeDiffArgs {
    pub base_repo: String,
    pub agent_name: String,
    pub base_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeLogArgs {
    pub base_repo: String,
    pub agent_name: String,
    pub max_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeMergeArgs {
    pub base_repo: String,
    pub agent_name: String,
    pub target_branch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorktreeDiscardArgs {
    pub base_repo: String,
    pub agent_name: String,
    #[serde(default)]
    pub force: bool,
}

fn default_search_limit() -> usize {
    10
}
fn default_fetch_mode() -> String {
    "markdown".to_string()
}
fn default_shell_timeout() -> u64 {
    60
}

fn resolve_agent_name(
    config: &ThatToolsConfig,
    skill_roots: &[PathBuf],
    explicit: Option<&str>,
) -> Option<String> {
    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(name.to_string());
    }
    if let Some(name) = agent_name_from_path(Path::new(&config.memory.db_path)) {
        return Some(name);
    }
    skill_roots
        .iter()
        .find_map(|root| agent_name_from_path(root))
}

fn agent_name_from_path(path: &Path) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    for i in 0..parts.len().saturating_sub(1) {
        if parts[i] == "agents" {
            let candidate = parts[i + 1].trim();
            if !candidate.is_empty() && candidate != "plugins" && candidate != "skills" {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

#[derive(Debug, Serialize)]
pub struct ShellExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

// ─── Tool definitions ─────────────────────────────────────────────────────────

/// Return JSON schema definitions for all standard agent tools.
///
/// `container` controls the sandbox-mode note in shell_exec's description.
pub fn all_tool_defs(container: &Option<String>) -> Vec<ToolDef> {
    let shell_mode_note = if container.is_some() {
        "Runs inside the isolated Docker container — full filesystem and network access within the sandbox."
    } else if trusted_local_sandbox_enabled() {
        "Runs directly inside this trusted runtime (Kubernetes pod-local sandbox mode)."
    } else {
        "Requires sandbox mode. Start the agent without --no-sandbox to enable this tool."
    };

    vec![
        ToolDef {
            name: "fs_ls".into(),
            description: "List directory contents with file sizes and types.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list" },
                    "max_depth": { "type": "integer", "description": "Max recursion depth (default: 1)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs_cat".into(),
            description: "Read a file's raw contents within the token budget.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "File path to read" } },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs_write".into(),
            description: "Write content to a file, creating it if it does not exist. \
                Prefer code_edit for targeted updates to existing files. \
                Use fs_write for new files, bootstrapping, or intentional full rewrites. \
                Set dry_run=true to preview without writing. \
                Set backup=true to keep a .bak copy of the original.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": false },
                    "backup": { "type": "boolean", "default": false }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "fs_mkdir".into(),
            description: "Create a directory. Set parents=true to create all missing parent directories.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "parents": { "type": "boolean", "default": false }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "fs_rm".into(),
            description: "Remove a file or directory. Set recursive=true for directories. \
                Set dry_run=true to preview what would be deleted.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "recursive": { "type": "boolean", "default": false },
                    "dry_run": { "type": "boolean", "default": false }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_read".into(),
            description: "Read source code with line numbers and optional symbol annotations. \
                Prefer this over fs_cat for source files — it understands structure. \
                Use line/end_line to read a specific range.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory path" },
                    "context": { "type": "integer", "description": "Lines of context around each symbol" },
                    "symbols": { "type": "boolean", "description": "Annotate symbols in output", "default": false },
                    "line": { "type": "integer", "description": "Start line (1-based)" },
                    "end_line": { "type": "integer", "description": "End line (1-based, inclusive)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_grep".into(),
            description: "Search source code for a pattern. .gitignore-aware. \
                Returns matches with file path, line number, and optional context lines.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (literal or regex)" },
                    "path": { "type": "string", "description": "Directory or file to search" },
                    "context": { "type": "integer", "description": "Lines of context around each match" },
                    "limit": { "type": "integer", "description": "Max matches to return" },
                    "ignore_case": { "type": "boolean", "default": false },
                    "regex": { "type": "boolean", "description": "Treat pattern as regex", "default": false },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns to include" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Glob patterns to exclude" }
                },
                "required": ["pattern", "path"]
            }),
        },
        ToolDef {
            name: "code_tree".into(),
            description: "Show repository structure as a tree. .gitignore-aware. \
                Use depth to control how deep to recurse. \
                Use ranked=true to sort by PageRank importance.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "depth": { "type": "integer", "description": "Max directory depth" },
                    "compact": { "type": "boolean", "default": false },
                    "ranked": { "type": "boolean", "description": "Sort by importance", "default": false }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_symbols".into(),
            description: "Find symbols (functions, structs, classes, methods) in source files using AST parsing. \
                Filter by kind (fn, struct, class, impl, etc.) or name pattern.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File or directory" },
                    "kind": { "type": "string", "description": "Symbol kind: fn, struct, class, impl, enum, trait, etc." },
                    "name": { "type": "string", "description": "Filter by name (substring match)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_summary".into(),
            description: "Summarise a source file's structure: top-level symbols, imports, and doc comments. \
                Token-efficient — use before code_read when you need orientation, not full content.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_edit".into(),
            description: "Edit an existing file with surgical precision. \
                Prefer this over fs_write when updating plugin/code/skill files in-place. \
                Supported modes: search+replace or target_fn+new_body (AST node body replacement).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to edit" },
                    "search": { "type": "string", "description": "Exact text to find (used with replace)" },
                    "replace": { "type": "string", "description": "Replacement text (used with search)" },
                    "target_fn": { "type": "string", "description": "Function/symbol name for AST body replacement (used with new_body)" },
                    "new_body": { "type": "string", "description": "New symbol body (used with target_fn)" },
                    "all": { "type": "boolean", "default": false, "description": "Replace all occurrences in search+replace mode" },
                    "dry_run": { "type": "boolean", "default": false }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "mem_add".into(),
            description: "Store a piece of information in persistent memory. \
                Use tags to categorise entries for later retrieval. \
                Memory persists across sessions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Information to remember" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Categorisation tags" },
                    "source": { "type": "string", "description": "Optional source file or URL" },
                    "session_id": { "type": "string" }
                },
                "required": ["content"]
            }),
        },
        ToolDef {
            name: "mem_recall".into(),
            description: "Recall information from persistent memory using semantic search. \
                Returns entries ranked by relevance to the query.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max results (default: 10)" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "mem_search".into(),
            description: "Search persistent memory by keyword and optional tag filters. \
                Prefer mem_recall for semantic queries; use mem_search for tag-based lookup.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "mem_compact".into(),
            description: "Compact memory for a session: persist a summary and prune short-term entries. \
                Call this at the end of a long session to preserve key findings.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "Session summary to persist" },
                    "session_id": { "type": "string" }
                },
                "required": ["summary"]
            }),
        },
        ToolDef {
            name: "search_query".into(),
            description: "Search the web and return ranked results with titles, URLs, and snippets.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "engine": { "type": "string", "description": "Search engine to use (default: configured engine)" },
                    "limit": { "type": "integer", "default": 10 },
                    "no_cache": { "type": "boolean", "default": false }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "search_fetch".into(),
            description: "Fetch one or more URLs and return their content. \
                mode: 'markdown' (default) converts HTML to readable text; 'raw' returns HTML.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "urls": { "type": "array", "items": { "type": "string" } },
                    "mode": { "type": "string", "enum": ["markdown", "raw"], "default": "markdown" }
                },
                "required": ["urls"]
            }),
        },
        ToolDef {
            name: "human_ask".into(),
            description: "Ask the user a question and wait for their response. \
                Use this only when you genuinely need human input to proceed. \
                Always runs on the host terminal — never blocked by sandbox mode.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Question to present to the user" },
                    "timeout": { "type": "integer", "description": "Seconds to wait (default: unlimited)" }
                },
                "required": ["message"]
            }),
        },
        ToolDef {
            name: "shell_exec".into(),
            description: format!(
                "Execute a shell command and return stdout, stderr, and exit code. \
                Non-zero exit codes are returned as data — interpret them, do not panic. \
                {shell_mode_note}"
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "cwd": { "type": "string", "description": "Working directory (local mode only)" },
                    "timeout_secs": { "type": "integer", "default": 60 }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "read_skill".into(),
            description: "Read a skill's documentation from the host skills directory. \
                Call this when a skill listed in the preamble is relevant to the current task — \
                it returns the full instructions and lists any reference files available for deeper detail. \
                This is the correct way to load skill content; do NOT use the bash tool to read skill files.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name from the catalog (e.g. \"that-tools\", \"that-tools-code\")" },
                    "file": { "type": "string", "description": "File to read within the skill directory. Defaults to SKILL.md." }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "read_plugin".into(),
            description: "Read plugin metadata (manifest, state, and optional directory entries). \
                Use this before changing plugins so decisions are grounded in the current plugin state.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "Plugin id to inspect. If omitted, returns the plugin list." },
                    "include_files": { "type": "boolean", "default": false, "description": "Include top-level files/dirs for the selected plugin." },
                    "agent_name": { "type": "string", "description": "Optional explicit agent name; usually auto-detected." }
                }
            }),
        },
        ToolDef {
            name: "validate_plugin".into(),
            description: "Validate plugin integrity and prerequisites. \
                Checks command safety/duplicates, activation command validity, expected directories/files, and required env vars.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "Plugin id to validate. If omitted, validates all plugins for the agent." },
                    "agent_name": { "type": "string", "description": "Optional explicit agent name; usually auto-detected." }
                }
            }),
        },
        // ── Git worktree tools ──────────────────────────────────────────────
        ToolDef {
            name: "worktree_create".into(),
            description: "Create an isolated git worktree branch for a named agent. \
                Each agent gets its own worktree under .worktrees/ with a timestamped branch.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" },
                    "agent_name": { "type": "string", "description": "Name of the agent (used for directory and branch naming)" },
                    "branch_prefix": { "type": "string", "description": "Optional prefix override for branch naming (default: agent/{agent_name})" }
                },
                "required": ["base_repo", "agent_name"]
            }),
        },
        ToolDef {
            name: "worktree_list".into(),
            description: "List all active git worktrees in a repository, \
                including agent name, branch, and path for each.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" }
                },
                "required": ["base_repo"]
            }),
        },
        ToolDef {
            name: "worktree_diff".into(),
            description: "Show the diff of an agent's worktree changes against the base branch. \
                Uses three-dot diff to isolate changes since divergence.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" },
                    "agent_name": { "type": "string", "description": "Agent whose worktree to diff" },
                    "base_branch": { "type": "string", "description": "Branch to diff against (default: main or master)" }
                },
                "required": ["base_repo", "agent_name"]
            }),
        },
        ToolDef {
            name: "worktree_log".into(),
            description: "Show the commit log for an agent's worktree since it diverged from the base branch.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" },
                    "agent_name": { "type": "string", "description": "Agent whose worktree log to show" },
                    "max_count": { "type": "integer", "description": "Maximum number of commits to show" }
                },
                "required": ["base_repo", "agent_name"]
            }),
        },
        ToolDef {
            name: "worktree_merge".into(),
            description: "Merge an agent's worktree branch into a target branch (default: main). \
                Creates a no-fast-forward merge commit. Reports conflicts if any.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" },
                    "agent_name": { "type": "string", "description": "Agent whose worktree branch to merge" },
                    "target_branch": { "type": "string", "description": "Branch to merge into (default: main or master)" }
                },
                "required": ["base_repo", "agent_name"]
            }),
        },
        ToolDef {
            name: "worktree_discard".into(),
            description: "Remove an agent's worktree and optionally delete its branch. \
                Set force=true to also delete the branch.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "base_repo": { "type": "string", "description": "Path to the git repository root" },
                    "agent_name": { "type": "string", "description": "Agent whose worktree to remove" },
                    "force": { "type": "boolean", "description": "Also delete the worktree branch", "default": false }
                },
                "required": ["base_repo", "agent_name"]
            }),
        },
    ]
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

/// Dispatch a tool call by name to the appropriate implementation.
///
/// Returns a JSON string suitable for injection into the LLM conversation as a
/// tool result. Errors are formatted as `{"error": "..."}`.
pub async fn dispatch(
    name: &str,
    args_json: &str,
    config: &ThatToolsConfig,
    container: &Option<String>,
    skill_roots: &[PathBuf],
) -> String {
    let result = dispatch_inner(name, args_json, config, container, skill_roots).await;
    match result {
        Ok(v) => v.to_string(),
        Err(e) => serde_json::json!({ "error": e.0 }).to_string(),
    }
}

async fn dispatch_inner(
    name: &str,
    args_json: &str,
    config: &ThatToolsConfig,
    container: &Option<String>,
    skill_roots: &[PathBuf],
) -> Result<serde_json::Value, ToolError> {
    match name {
        "fs_ls" => {
            let args: FsLsArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let depth = args.max_depth.unwrap_or(1);
                let cpath = container_path(&args.path);
                docker_exec_sh(c, &format!("find '{cpath}' -maxdepth {depth} | sort")).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::FsLs {
                        path: args.path,
                        max_depth: args.max_depth,
                    },
                )
                .await
            }
        }
        "fs_cat" => {
            let args: FsCatArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                docker_exec_sh(c, &format!("cat '{cpath}'")).await
            } else {
                run_on_host(config.clone(), ToolRequest::FsCat { path: args.path }).await
            }
        }
        "fs_write" => {
            let args: FsWriteArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                docker_exec_write(c, &args.path, &args.content).await
            } else if trusted_local_sandbox_enabled() {
                run_on_host(
                    config.clone(),
                    ToolRequest::FsWrite {
                        path: args.path,
                        content: args.content,
                        dry_run: args.dry_run,
                        backup: args.backup,
                    },
                )
                .await
            } else {
                Err(sandbox_required())
            }
        }
        "fs_mkdir" => {
            let args: FsMkdirArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let flag = if args.parents { "-p " } else { "" };
                docker_exec_sh(c, &format!("mkdir {flag}'{cpath}'")).await
            } else if trusted_local_sandbox_enabled() {
                run_on_host(
                    config.clone(),
                    ToolRequest::FsMkdir {
                        path: args.path,
                        parents: args.parents,
                    },
                )
                .await
            } else {
                Err(sandbox_required())
            }
        }
        "fs_rm" => {
            let args: FsRmArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let flag = if args.recursive { "-rf" } else { "-f" };
                docker_exec_sh(c, &format!("rm {flag} '{cpath}'")).await
            } else if trusted_local_sandbox_enabled() {
                run_on_host(
                    config.clone(),
                    ToolRequest::FsRm {
                        path: args.path,
                        recursive: args.recursive,
                        dry_run: args.dry_run,
                    },
                )
                .await
            } else {
                Err(sandbox_required())
            }
        }
        "code_read" => {
            let args: CodeReadArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let mut cmd = format!("/usr/local/bin/that code read '{cpath}'");
                if let Some(line) = args.line {
                    cmd.push_str(&format!(" --line {line}"));
                }
                if let Some(end_line) = args.end_line {
                    cmd.push_str(&format!(" --end-line {end_line}"));
                }
                if args.symbols {
                    cmd.push_str(" --symbols");
                }
                docker_exec_sh(c, &cmd).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::CodeRead {
                        path: args.path,
                        context: args.context,
                        symbols: args.symbols,
                        line: args.line,
                        end_line: args.end_line,
                    },
                )
                .await
            }
        }
        "code_grep" => {
            let args: CodeGrepArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let mut cmd = format!("/usr/local/bin/that code grep '{}' '{cpath}'", args.pattern);
                if let Some(ctx) = args.context {
                    cmd.push_str(&format!(" --context {ctx}"));
                }
                if let Some(limit) = args.limit {
                    cmd.push_str(&format!(" --limit {limit}"));
                }
                if args.ignore_case {
                    cmd.push_str(" --ignore-case");
                }
                if args.regex {
                    cmd.push_str(" --regex");
                }
                for pat in &args.include {
                    cmd.push_str(&format!(" --include '{pat}'"));
                }
                for pat in &args.exclude {
                    cmd.push_str(&format!(" --exclude '{pat}'"));
                }
                docker_exec_sh(c, &cmd).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::CodeGrep {
                        pattern: args.pattern,
                        path: args.path,
                        context: args.context,
                        limit: args.limit,
                        ignore_case: args.ignore_case,
                        regex: args.regex,
                        include: args.include,
                        exclude: args.exclude,
                    },
                )
                .await
            }
        }
        "code_tree" => {
            let args: CodeTreeArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let mut cmd = format!("/usr/local/bin/that code tree '{cpath}'");
                if let Some(depth) = args.depth {
                    cmd.push_str(&format!(" --depth {depth}"));
                }
                if args.compact {
                    cmd.push_str(" --compact");
                }
                if args.ranked {
                    cmd.push_str(" --ranked");
                }
                docker_exec_sh(c, &cmd).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::CodeTree {
                        path: args.path,
                        depth: args.depth,
                        compact: args.compact,
                        ranked: args.ranked,
                    },
                )
                .await
            }
        }
        "code_symbols" => {
            let args: CodeSymbolsArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let mut cmd = format!("/usr/local/bin/that code symbols '{cpath}'");
                if let Some(kind) = &args.kind {
                    cmd.push_str(&format!(" --kind '{kind}'"));
                }
                if let Some(nm) = &args.name {
                    cmd.push_str(&format!(" --name '{nm}'"));
                }
                docker_exec_sh(c, &cmd).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::CodeSymbols {
                        path: args.path,
                        kind: args.kind,
                        name: args.name,
                    },
                )
                .await
            }
        }
        "code_summary" => {
            let args: CodeSummaryArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                docker_exec_sh(c, &format!("/usr/local/bin/that code summary '{cpath}'")).await
            } else {
                run_on_host(config.clone(), ToolRequest::CodeSummary { path: args.path }).await
            }
        }
        "code_edit" => {
            let args: CodeEditArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if let Some(c) = container.as_deref() {
                let cpath = container_path(&args.path);
                let mut cmd = format!(
                    "/usr/local/bin/that code edit {}",
                    shell_single_quote(&cpath)
                );
                if let (Some(search), Some(replace)) = (&args.search, &args.replace) {
                    cmd.push_str(&format!(
                        " --search {} --replace {}",
                        shell_single_quote(search),
                        shell_single_quote(replace)
                    ));
                    if args.all {
                        cmd.push_str(" --all");
                    }
                } else if let (Some(target_fn), Some(new_body)) = (&args.target_fn, &args.new_body)
                {
                    cmd.push_str(&format!(
                        " --fn {} --new-body {}",
                        shell_single_quote(target_fn),
                        shell_single_quote(new_body)
                    ));
                } else {
                    return Err(ToolError(
                        "code_edit requires search+replace or target_fn+new_body".to_string(),
                    ));
                }
                if args.dry_run {
                    cmd.push_str(" --dry-run");
                }
                docker_exec_sh(c, &cmd).await
            } else {
                run_on_host(
                    config.clone(),
                    ToolRequest::CodeEdit {
                        path: args.path,
                        search: args.search,
                        replace: args.replace,
                        target_fn: args.target_fn,
                        new_body: args.new_body,
                        all: args.all,
                        dry_run: args.dry_run,
                    },
                )
                .await
            }
        }
        "mem_add" => {
            let args: MemAddArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::MemAdd {
                    content: args.content,
                    tags: args.tags,
                    source: args.source,
                    session_id: args.session_id,
                },
            )
            .await
        }
        "mem_recall" => {
            let args: MemRecallArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::MemRecall {
                    query: args.query,
                    limit: args.limit,
                    session_id: None,
                },
            )
            .await
        }
        "mem_search" => {
            let args: MemSearchArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::MemSearch {
                    query: args.query,
                    tags: args.tags,
                    limit: args.limit,
                    session_id: None,
                },
            )
            .await
        }
        "mem_compact" => {
            let args: MemCompactArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::MemCompact {
                    summary: args.summary,
                    session_id: args.session_id,
                },
            )
            .await
        }
        "search_query" => {
            let args: SearchQueryArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::SearchQuery {
                    query: args.query,
                    engine: args.engine,
                    limit: args.limit,
                    no_cache: args.no_cache,
                },
            )
            .await
        }
        "search_fetch" => {
            let args: SearchFetchArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::SearchFetch {
                    urls: args.urls,
                    mode: args.mode,
                },
            )
            .await
        }
        "human_ask" => {
            let args: HumanAskArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(
                config.clone(),
                ToolRequest::HumanAsk {
                    message: args.message,
                    timeout: args.timeout,
                },
            )
            .await
        }
        "shell_exec" => dispatch_shell_exec(args_json, config, container)
            .await
            .map(|o| serde_json::to_value(o).unwrap_or_default()),
        "read_skill" => super::skill::dispatch_read_skill(args_json, skill_roots).await,
        "read_plugin" => {
            let args: ReadPluginArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let agent_name = resolve_agent_name(config, skill_roots, args.agent_name.as_deref())
                .ok_or_else(|| {
                    ToolError(
                        "Unable to resolve agent name for read_plugin. Provide agent_name."
                            .to_string(),
                    )
                })?;
            that_plugins::read_plugin_snapshot(
                &agent_name,
                args.plugin_id.as_deref(),
                args.include_files,
            )
            .map_err(|e| ToolError(format!("read_plugin failed: {e}")))
        }
        "validate_plugin" => {
            let args: ValidatePluginArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let agent_name = resolve_agent_name(config, skill_roots, args.agent_name.as_deref())
                .ok_or_else(|| {
                    ToolError(
                        "Unable to resolve agent name for validate_plugin. Provide agent_name."
                            .to_string(),
                    )
                })?;
            serde_json::to_value(
                that_plugins::validate_plugin_snapshot(&agent_name, args.plugin_id.as_deref())
                    .map_err(|e| ToolError(format!("validate_plugin failed: {e}")))?,
            )
            .map_err(|e| ToolError(format!("serialize validate_plugin output failed: {e}")))
        }
        // ── Git worktree tools ──────────────────────────────────────────
        "worktree_create" => {
            let args: WorktreeCreateArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let result = that_tools::tools::code::worktree::create_worktree(
                Path::new(&args.base_repo),
                &args.agent_name,
                args.branch_prefix.as_deref(),
            )
            .map_err(|e| ToolError(format!("worktree_create failed: {e}")))?;
            Ok(serde_json::json!({
                "agent_name": result.agent_name,
                "branch": result.branch,
                "path": result.path.to_string_lossy(),
            }))
        }
        "worktree_list" => {
            let args: WorktreeListArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let worktrees =
                that_tools::tools::code::worktree::list_worktrees(Path::new(&args.base_repo))
                    .map_err(|e| ToolError(format!("worktree_list failed: {e}")))?;
            let items: Vec<_> = worktrees
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "agent_name": w.agent_name,
                        "branch": w.branch,
                        "path": w.path.to_string_lossy(),
                        "is_main": w.is_main,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "worktrees": items }))
        }
        "worktree_diff" => {
            let args: WorktreeDiffArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let diff = that_tools::tools::code::worktree::worktree_diff(
                Path::new(&args.base_repo),
                &args.agent_name,
                args.base_branch.as_deref(),
            )
            .map_err(|e| ToolError(format!("worktree_diff failed: {e}")))?;
            Ok(serde_json::json!({ "diff": diff }))
        }
        "worktree_log" => {
            let args: WorktreeLogArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let log = that_tools::tools::code::worktree::worktree_log(
                Path::new(&args.base_repo),
                &args.agent_name,
                args.max_count,
            )
            .map_err(|e| ToolError(format!("worktree_log failed: {e}")))?;
            Ok(serde_json::json!({ "log": log }))
        }
        "worktree_merge" => {
            let args: WorktreeMergeArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let result = that_tools::tools::code::worktree::merge_worktree(
                Path::new(&args.base_repo),
                &args.agent_name,
                args.target_branch.as_deref(),
            )
            .map_err(|e| ToolError(format!("worktree_merge failed: {e}")))?;
            Ok(serde_json::json!({
                "success": result.success,
                "commits_merged": result.commits_merged,
                "message": result.message,
                "conflicts": result.conflicts,
            }))
        }
        "worktree_discard" => {
            let args: WorktreeDiscardArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            that_tools::tools::code::worktree::remove_worktree(
                Path::new(&args.base_repo),
                &args.agent_name,
                args.force,
            )
            .map_err(|e| ToolError(format!("worktree_discard failed: {e}")))?;
            Ok(serde_json::json!({ "status": "removed" }))
        }
        other => Err(ToolError(format!("unknown tool: {other}"))),
    }
}

async fn dispatch_shell_exec(
    args_json: &str,
    config: &ThatToolsConfig,
    container: &Option<String>,
) -> Result<ShellExecOutput, ToolError> {
    let args: ShellExecArgs =
        serde_json::from_str(args_json).map_err(|e| ToolError(format!("invalid args: {e}")))?;
    if let Some(msg) = shell_exec_backend_guard(&args.command) {
        return Err(ToolError(msg));
    }

    if container.is_none() && trusted_local_sandbox_enabled() {
        let result = run_on_host(
            config.clone(),
            ToolRequest::ShellExec {
                command: args.command,
                cwd: args.cwd,
                timeout_secs: args.timeout_secs,
                signal: None,
            },
        )
        .await?;
        let parsed: that_tools::tools::exec::ExecResult = serde_json::from_value(result)
            .map_err(|e| ToolError(format!("failed to decode local shell_exec output: {e}")))?;
        return Ok(ShellExecOutput {
            stdout: parsed.stdout,
            stderr: parsed.stderr,
            exit_code: parsed.exit_code.unwrap_or(-1),
            timed_out: parsed.timed_out,
        });
    }

    let c = container.as_deref().ok_or_else(sandbox_required)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let mut cmd = Command::new("docker");
    cmd.arg("exec").arg("-i");

    if let Some(agent_name) = c.strip_prefix("that-agent-") {
        cmd.arg("--env").arg("SHELL=/bin/bash");
        cmd.arg("--env").arg(format!(
            "BASH_ENV=/home/agent/.that-agent/agents/{agent_name}/.bashrc"
        ));
    }

    const FORWARD_PREFIXES: &[&str] = &[
        "ANTHROPIC_",
        "OPENAI_",
        "AZURE_OPENAI_",
        "GOOGLE_",
        "COHERE_",
        "MISTRAL_",
        "TELEGRAM_",
        "DISCORD_",
        "WHATSAPP_",
        "THAT_",
        "RUST_LOG",
    ];
    for (key, val) in std::env::vars() {
        if FORWARD_PREFIXES.iter().any(|p| key.starts_with(p)) {
            cmd.arg("--env").arg(format!("{key}={val}"));
        }
    }

    let allow_otel = std::env::var("THAT_SHELL_EXEC_ALLOW_OTEL")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    if !allow_otel {
        // Prevent child processes in the sandbox from inheriting the agent's
        // OTLP endpoint and emitting unintended traces.
        cmd.arg("--env").arg("OTEL_SDK_DISABLED=true");
        cmd.arg("--env")
            .arg("OTEL_PYTHON_DISABLED_INSTRUMENTATIONS=all");
        cmd.arg("--env").arg("OTEL_EXPORTER_OTLP_ENDPOINT=");
        cmd.arg("--env").arg("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=");
    }

    cmd.arg(c)
        .arg("bash")
        .arg("-c")
        .arg(&args.command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let child = cmd
        .spawn()
        .map_err(|e| ToolError(format!("Failed to spawn docker exec: {e}")))?;
    let pid = child.id();

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(ShellExecOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            exit_code: out.status.code().unwrap_or(-1),
            timed_out: false,
        }),
        Ok(Err(e)) => Err(ToolError(format!("docker exec error: {e}"))),
        Err(_) => {
            if let Some(pid) = pid {
                let _ = std::process::Command::new("kill")
                    .args(["-9", &pid.to_string()])
                    .status();
            }
            Ok(ShellExecOutput {
                stdout: String::new(),
                stderr: format!("Command timed out after {}s", args.timeout_secs),
                exit_code: -1,
                timed_out: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn permissive_config() -> ThatToolsConfig {
        let mut cfg = ThatToolsConfig::default();
        cfg.policy.tools.fs_write = that_tools::config::PolicyLevel::Allow;
        cfg.policy.tools.fs_delete = that_tools::config::PolicyLevel::Allow;
        cfg.policy.tools.shell_exec = that_tools::config::PolicyLevel::Allow;
        cfg
    }

    struct EnvVarGuard {
        key: &'static str,
        old: Option<String>,
    }
    impl EnvVarGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let old = std::env::var(key).ok();
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            Self { key, old }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn unique_tmp_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "that-core-typed-tool-{suffix}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[tokio::test]
    async fn fs_cat_still_works_on_host_without_container() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let config = ThatToolsConfig::default();
        let path = unique_tmp_path("host-fs-read.txt");
        let path_str = path.to_string_lossy().to_string();
        fs::write(&path, "host-read-only").expect("setup should create test file");
        let result = dispatch(
            "fs_cat",
            &serde_json::json!({"path": path_str}).to_string(),
            &config,
            &None,
            &[],
        )
        .await;
        assert!(
            result.contains("host-read-only"),
            "fs_cat output should contain file content"
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn fs_write_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": "tmp/test.txt", "content": "x"}).to_string(),
            &ThatToolsConfig::default(),
            &None,
            &[],
        )
        .await;
        assert!(
            result.contains("sandbox required"),
            "fs_write should reject local mode"
        );
    }

    #[tokio::test]
    async fn fs_rm_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let result = dispatch("fs_rm", &serde_json::json!({"path": unique_tmp_path("policy-deny").to_string_lossy().to_string()}).to_string(), &ThatToolsConfig::default(), &None, &[]).await;
        assert!(result.contains("sandbox required"));
    }

    #[tokio::test]
    async fn shell_exec_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let result = dispatch(
            "shell_exec",
            &serde_json::json!({"command": "printf host-shell"}).to_string(),
            &ThatToolsConfig::default(),
            &None,
            &[],
        )
        .await;
        assert!(result.contains("sandbox required"));
    }

    #[tokio::test]
    async fn sandbox_mode_attempts_docker_exec_for_fs_write() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": "tmp/test.txt", "content": "x"}).to_string(),
            &ThatToolsConfig::default(),
            &Some("that-core-missing-container".to_string()),
            &[],
        )
        .await;
        assert!(
            !result.contains("sandbox required"),
            "sandbox mode should attempt docker execution"
        );
    }

    #[tokio::test]
    async fn trusted_local_mode_allows_fs_write_and_shell_exec_without_container() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("1"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("kubernetes"));
        let target = unique_tmp_path("trusted-local-write.txt");
        let target_str = target.to_string_lossy().to_string();
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": target_str, "content": "trusted"}).to_string(),
            &permissive_config(),
            &None,
            &[],
        )
        .await;
        assert!(
            !result.contains("error"),
            "trusted local mode should allow fs_write: {result}"
        );
        let written = fs::read_to_string(&target).expect("file should have been written");
        assert_eq!(written, "trusted");
        let result = dispatch(
            "shell_exec",
            &serde_json::json!({"command": "printf trusted-local-shell"}).to_string(),
            &permissive_config(),
            &None,
            &[],
        )
        .await;
        assert!(result.contains("trusted-local-shell"));
        let _ = fs::remove_file(target);
    }

    #[test]
    fn command_uses_docker_daemon_detects_daemon_commands() {
        assert!(command_uses_docker_daemon("docker build -t app ."));
        assert!(command_uses_docker_daemon(
            "DOCKER_BUILDKIT=1 docker build -t app ."
        ));
        assert!(command_uses_docker_daemon(
            "docker buildx build --push -t app ."
        ));
        assert!(command_uses_docker_daemon("docker compose up -d"));
        assert!(!command_uses_docker_daemon(
            "buildctl --addr $BUILDKIT_HOST debug workers"
        ));
        assert!(!command_uses_docker_daemon("kubectl apply -k deploy/k8s"));
    }

    #[test]
    fn shell_exec_backend_guard_blocks_docker_when_backend_is_buildkit() {
        let _lock = env_lock();
        let _backend = EnvVarGuard::set("THAT_IMAGE_BUILD_BACKEND", Some("buildkit"));
        let _docker = EnvVarGuard::set("THAT_DOCKER_DAEMON_AVAILABLE", Some("false"));

        let blocked = shell_exec_backend_guard("docker build -t app .");
        assert!(
            blocked.is_some(),
            "docker build should be blocked under buildkit backend"
        );

        let allowed = shell_exec_backend_guard("buildctl --addr ${BUILDKIT_HOST} debug workers");
        assert!(
            allowed.is_none(),
            "buildctl should remain allowed under buildkit backend"
        );
    }
}
