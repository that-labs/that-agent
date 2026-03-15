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
use crate::agent_loop::ToolContext;

const TRUSTED_LOCAL_SANDBOX_ENV: &str = "THAT_TRUSTED_LOCAL_SANDBOX";
const SANDBOX_MODE_ENV: &str = "THAT_SANDBOX_MODE";

// ─── Error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolError(pub String);

/// Result of a tool dispatch: text output + optional images for vision.
pub struct DispatchResult {
    pub text: String,
    pub images: Vec<(Vec<u8>, String)>, // (data, mime_type)
}

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
pub struct MemRemoveArgs {
    pub id: String,
}
#[derive(Debug, Deserialize)]
pub struct MemUnpinArgs {
    pub id: String,
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
pub struct ImageReadArgs {
    pub path: String,
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
    5
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
            name: "image_read".into(),
            description: "Read an image file and return it for visual analysis. \
                Supports PNG, JPEG, GIF, WebP. Max 5 MB. \
                Auto-resizes large images to fit within vision model limits.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the image file" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "code_read".into(),
            description: "Read source code with line numbers and optional symbol annotations. \
                Prefer this over fs_cat for source files — it understands structure. \
                For large files, always use line/end_line to read a specific range — \
                reading the full file may exceed result limits.".into(),
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
                Prefer this over fs_write when updating any file type in-place. \
                Two mutually exclusive modes: (1) search+replace — works on any file type, \
                (2) target_fn+new_body — AST-aware body replacement for supported languages. \
                Provide exactly one mode per call.".into(),
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
                Returns entries ranked by relevance. Optionally filter by tags.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tag filter" },
                    "limit": { "type": "integer", "description": "Max results (default: 10)" }
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
            name: "mem_remove".into(),
            description: "Remove a specific memory entry by its ID. Use after mem_recall \
                to identify the entry to delete.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory entry ID to remove" }
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "mem_unpin".into(),
            description: "Demote a pinned memory entry back to unpinned so it becomes eligible \
                for automatic pruning. The entry is retained; use mem_remove to delete it.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory entry ID to unpin" }
                },
                "required": ["id"]
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
                IMPORTANT: Do NOT use shell_exec for operations that have dedicated tools — \
                use code_read/fs_cat instead of cat, code_grep instead of grep, fs_ls instead of ls, \
                code_edit instead of sed/awk, fs_write instead of echo/tee, list_skills instead of ls on skills dir. \
                Reserve shell_exec for git, build commands, package managers, and runtime operations with no dedicated tool. \
                Default timeout: 5s — most commands finish instantly. \
                Set a higher timeout_secs explicitly for builds, installs, or known slow ops. \
                For long-running processes, redirect output to a file and manage it separately. \
                Non-zero exit codes are returned as data — interpret them, do not panic. \
                For large output, pipe through head/tail/grep to stay within result limits. \
                {shell_mode_note}"
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "cwd": { "type": "string", "description": "Working directory (local mode only)" },
                    "timeout_secs": { "type": "integer", "default": 5, "description": "Seconds before kill. Increase for builds/installs." }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "list_skills".into(),
            description: "List all available skills with their names, descriptions, and reference files. \
                Use this to discover what skills are available before loading one with read_skill. \
                Do NOT use shell_exec to list the skills directory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "read_skill".into(),
            description: "Read a skill's documentation from the host skills directory. \
                Call this when a skill listed in the preamble or from list_skills is relevant to the current task — \
                it returns the full instructions and lists any reference files available for deeper detail. \
                This is the correct way to load skill content; do NOT use the bash tool to read skill files.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name exactly as listed in the preamble skill catalog or from list_skills" },
                    "file": { "type": "string", "description": "File path relative to the skill directory. Defaults to SKILL.md. Use this to load reference files (e.g. \"references/worktree-local.md\") — the available_files field in the response lists all loadable files." }
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
        // ── Cluster plugin management tools ────────────────────────────────
        ToolDef {
            name: "plugin_list".into(),
            description: "List all plugins registered in the cluster. \
                Optionally filter by owning agent name.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_name": { "type": "string", "description": "Filter to plugins owned by this agent" }
                }
            }),
        },
        ToolDef {
            name: "plugin_install".into(),
            description: "Install a plugin from a manifest file into the cluster registry. \
                Deploys the plugin if it declares a deploy target. \
                Use skip_deploy=true to register without deploying.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "manifest_path": { "type": "string", "description": "Path to the plugin manifest TOML file" },
                    "owner_agent": { "type": "string", "description": "Name of the agent that owns this plugin" },
                    "skip_deploy": { "type": "boolean", "description": "Register plugin without deploying (default: false)", "default": false }
                },
                "required": ["manifest_path", "owner_agent"]
            }),
        },
        ToolDef {
            name: "plugin_uninstall".into(),
            description: "Remove a plugin from the cluster registry. \
                When undeploy is true (default), also tears down the running deployment. \
                Only the owner agent or the main agent can uninstall.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "ID of the plugin to uninstall" },
                    "requestor_agent": { "type": "string", "description": "Name of the agent requesting uninstall" },
                    "undeploy": { "type": "boolean", "description": "Also tear down the running deployment (default: true)", "default": true }
                },
                "required": ["plugin_id", "requestor_agent"]
            }),
        },
        ToolDef {
            name: "plugin_status".into(),
            description: "Check the deploy status of a plugin (running, stopped, failed, etc.).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "ID of the plugin to check" }
                },
                "required": ["plugin_id"]
            }),
        },
        ToolDef {
            name: "plugin_set_policy".into(),
            description: "Set the access policy for a plugin. \
                Controls which agents are allowed to use the plugin.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "plugin_id": { "type": "string", "description": "ID of the plugin" },
                    "allow": { "type": "array", "items": { "type": "string" }, "description": "List of agent names allowed access, or [\"*\"] for all" },
                    "requestor_agent": { "type": "string", "description": "Name of the agent requesting the policy change" }
                },
                "required": ["plugin_id", "allow", "requestor_agent"]
            }),
        },
        // ── Dynamic channel management tools ────────────────────────────────
        ToolDef {
            name: "channel_list".into(),
            description: "List all dynamically registered channel adapters.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "channel_register".into(),
            description: "Register a new gateway channel adapter at runtime. \
                The adapter will POST agent responses to the given callback URL.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique identifier for the channel" },
                    "callback_url": { "type": "string", "description": "URL to POST agent responses to" },
                    "capabilities": { "type": "array", "items": { "type": "string" }, "description": "Capability tags for the channel" }
                },
                "required": ["id", "callback_url"]
            }),
        },
        ToolDef {
            name: "channel_unregister".into(),
            description: "Remove a dynamically registered channel adapter.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "ID of the channel to unregister" }
                },
                "required": ["id"]
            }),
        },
        ToolDef {
            name: "provider_list".into(),
            description: "List dynamically registered inference providers available for /models.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "provider_register".into(),
            description: "Register a new OpenAI-compatible inference provider at runtime. \
                Use the provider id later in /models and set the API key in the referenced env var.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Unique provider id, for example groq" },
                    "base_url": { "type": "string", "description": "OpenAI-compatible API base URL, for example https://api.groq.com/openai/v1" },
                    "api_key_env": { "type": "string", "description": "Environment variable name that holds the provider API key" },
                    "models": { "type": "array", "items": { "type": "string" }, "description": "Suggested models to show in /models" },
                    "transport": { "type": "string", "enum": ["openai_chat"], "description": "Provider transport type. Only openai_chat is supported right now." }
                },
                "required": ["id", "base_url", "api_key_env"]
            }),
        },
        ToolDef {
            name: "provider_unregister".into(),
            description: "Remove a dynamically registered inference provider.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Provider id to unregister" }
                },
                "required": ["id"]
            }),
        },
        // ── Dynamic gateway route tools ──────────────────────────────────────
        ToolDef {
            name: "gateway_route_register".into(),
            description: "Register a custom HTTP route on the agent's gateway at runtime. \
                Use handler_type=\"static\" with a JSON body to return a fixed response. \
                Use handler_type=\"shell\" with a shell command whose stdout becomes the response body. \
                The request body (if any) is available as the REQUEST_BODY env var for shell handlers.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "description": "HTTP method (e.g. GET, POST)" },
                    "path": { "type": "string", "description": "URL path (e.g. /v1/admin/plugins)" },
                    "handler_type": { "type": "string", "enum": ["static", "shell"], "description": "Handler type" },
                    "body": { "description": "JSON body to return (for static handler)" },
                    "command": { "type": "string", "description": "Shell command to execute (for shell handler)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout for shell handler (default: 30)" }
                },
                "required": ["method", "path", "handler_type"]
            }),
        },
        ToolDef {
            name: "gateway_route_unregister".into(),
            description: "Remove a previously registered custom gateway route.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "description": "HTTP method" },
                    "path": { "type": "string", "description": "URL path of the route to remove" }
                },
                "required": ["method", "path"]
            }),
        },
        ToolDef {
            name: "gateway_route_list".into(),
            description: "List all custom routes currently registered on the agent's HTTP gateway.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        // ── Identity / workspace file tools ──────────────────────────────────
        ToolDef {
            name: "identity_update".into(),
            description: "Overwrite a permitted workspace file (Agents.md, User.md, Tools.md, Heartbeat.md, \
                Tasks.md, Soul.md, etc.). Use to update operating instructions, user profile, \
                heartbeat schedule, or task list. Read the file first if you need to preserve existing content.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "Workspace file to write (e.g. \"Agents.md\", \"Heartbeat.md\")" },
                    "content": { "type": "string", "description": "Full new content for the file" }
                },
                "required": ["file", "content"]
            }),
        },
        // ── HTTP request tool ─────────────────────────────────────────────────
        ToolDef {
            name: "http_request".into(),
            description: "Make an HTTP request to an external URL. Returns status code and response body. \
                Default timeout: 30s. Follows redirects (max 10). HTTPS only validates certificates. \
                Response body is capped — for large payloads, use shell_exec with curl and pipe through head.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "description": "HTTP method (GET, POST, PUT, PATCH, DELETE)" },
                    "url": { "type": "string", "description": "Full URL to request" },
                    "headers": {
                        "type": "object",
                        "description": "Optional key-value request headers",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": { "type": "string", "description": "Optional request body" },
                    "timeout_secs": { "type": "integer", "description": "Request timeout in seconds (default: 15)" }
                },
                "required": ["method", "url"]
            }),
        },
        // ── Multi-agent lifecycle tools ───────────────────────────────────────
        ToolDef {
            name: "spawn_agent".into(),
            description: "Spawn a persistent sub-agent. In K8s mode creates a Deployment + Service; \
                locally forks a background process. Returns the gateway URL for querying.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Unique name for the sub-agent" },
                    "role": { "type": "string", "description": "Optional role description" },
                    "gateway_port": { "type": "integer", "description": "Port for the child agent's HTTP gateway (local mode only)" },
                    "model": { "type": "string", "description": "Optional model override. Use full IDs: claude-sonnet-4-6, claude-opus-4-6, claude-haiku-4-5, gpt-5.2-codex. Shorthands like sonnet-4-6 or opus are auto-normalized." }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "agent_run".into(),
            description: "Run an ephemeral task agent. Blocks until the task completes and returns \
                the result. Call multiple agent_run in parallel for fan-out work. In K8s mode runs \
                as a Job; locally runs as a foreground process.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Unique worker name" },
                    "role": { "type": "string", "description": "Optional role description for the worker" },
                    "task": { "type": "string", "description": "The task for the worker to execute" },
                    "model": { "type": "string", "description": "Optional model override. Use full IDs: claude-sonnet-4-6, claude-opus-4-6, claude-haiku-4-5, gpt-5.2-codex. Shorthands like sonnet-4-6 or opus are auto-normalized." },
                    "workspace": { "type": "boolean", "description": "If true, share the current git workspace with the worker (default: false)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 300)" }
                },
                "required": ["name", "task"]
            }),
        },
        ToolDef {
            name: "agent_list".into(),
            description: "List all agents — persistent Deployments and ephemeral Jobs — with their \
                role, gateway URL, and status.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "agent_query".into(),
            description: "Send a message to a persistent agent and return its response. \
                Only works for agents with a gateway (persistent, not ephemeral).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the target agent" },
                    "message": { "type": "string", "description": "Message to send" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (default: 60)" }
                },
                "required": ["name", "message"]
            }),
        },
        ToolDef {
            name: "agent_unregister".into(),
            description: "Remove a child agent and all its resources. In K8s mode uses label-scoped \
                deletion; locally removes the registry entry and kills the process.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the agent to remove" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "agent_stop".into(),
            description: "Stop and clean up a running child agent (ephemeral or persistent). \
                Deletes the Job/Deployment and all associated resources. Use when a child is stuck, \
                timed out, or no longer needed.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the agent to stop" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "agent_status".into(),
            description: "Get detailed status of a child agent — running, completed, failed, \
                pod phase, and start time. Works for both ephemeral Jobs and persistent Deployments.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the agent to check" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "agent_logs".into(),
            description: "Get recent log output from a child agent. Works for running or completed agents. \
                Use to inspect what an agent is doing or why it failed.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name of the agent" },
                    "tail": { "type": "integer", "description": "Number of log lines from the end (default: 50)" }
                },
                "required": ["name"]
            }),
        },
        ToolDef {
            name: "workspace_share".into(),
            description: "Share a local git repository with sub-agents via the in-cluster git server. \
                Workers can clone this workspace for coding tasks. Call before agent_run with workspace param.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to a git repo to share" },
                    "name": { "type": "string", "description": "Optional repo name (defaults to folder basename)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "workspace_collect".into(),
            description: "Merge or review a worker's code changes back into your local workspace. \
                Call after agent_run completes for coding tasks.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Local working tree to merge into" },
                    "worker": { "type": "string", "description": "Worker name whose branch to merge" },
                    "strategy": { "type": "string", "description": "\"merge\" (default) or \"review\" (diff only, no merge)" }
                },
                "required": ["path", "worker"]
            }),
        },
        ToolDef {
            name: "workspace_activity".into(),
            description: "Check worker progress on the shared workspace — lists branches, ahead/behind \
                counts vs main, and last commit per branch. Use to monitor parallel workers without cloning.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "repo": { "type": "string", "description": "Repo name (defaults to \"workspace\")" }
                }
            }),
        },
        ToolDef {
            name: "workspace_diff".into(),
            description: "Get a unified diff of a worker's branch vs main, without cloning. \
                Use to review worker output before collecting.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Branch to diff, e.g. \"task/worker-1\"" },
                    "repo": { "type": "string", "description": "Repo name (defaults to \"workspace\")" }
                },
                "required": ["branch"]
            }),
        },
        ToolDef {
            name: "workspace_conflicts".into(),
            description: "Analyze merge conflicts between a worker's branch and main. Returns the list \
                of conflicting files and both sides of the diff. Use when workspace_collect reports a merge failure.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Branch to check, e.g. \"task/worker-1\"" },
                    "repo": { "type": "string", "description": "Repo name (defaults to \"workspace\")" }
                },
                "required": ["branch"]
            }),
        },
    ]
}

// ─── Dispatch ─────────────────────────────────────────────────────────────────

/// Dispatch a tool call by name to the appropriate implementation.
///
/// Returns a `DispatchResult` with text output (JSON string) and optional images
/// for vision-capable LLMs. Errors are formatted as `{"error": "..."}`.
pub async fn dispatch(name: &str, args_json: &str, ctx: &ToolContext) -> DispatchResult {
    // image_read is handled separately because it returns binary image data
    // that must bypass the normal JSON-only path.
    if name == "image_read" {
        return dispatch_image_read(args_json, ctx).await;
    }
    let result = dispatch_inner(name, args_json, ctx).await;
    match result {
        Ok(v) => DispatchResult {
            text: v.to_string(),
            images: vec![],
        },
        Err(e) => DispatchResult {
            text: serde_json::json!({ "error": e.0 }).to_string(),
            images: vec![],
        },
    }
}

async fn dispatch_image_read(args_json: &str, ctx: &ToolContext) -> DispatchResult {
    let err = |msg: &str| DispatchResult {
        text: serde_json::json!({ "error": msg }).to_string(),
        images: vec![],
    };

    let args: ImageReadArgs = match serde_json::from_str(args_json) {
        Ok(a) => a,
        Err(e) => return err(&format!("invalid args: {e}")),
    };

    // Policy check: image_read uses fs_read policy
    if matches!(
        ctx.config.policy.tools.fs_read,
        that_tools::config::PolicyLevel::Deny
    ) {
        return err("policy denied: tool 'fs_read' is not allowed by current policy");
    }

    let path_str = if let Some(c) = ctx.container.as_deref() {
        // Sandbox: docker cp the file out to a temp location
        let cpath = container_path(&args.path);
        let tmp = std::env::temp_dir().join(format!("image_read_{}", uuid::Uuid::new_v4()));
        let output = Command::new("docker")
            .args(["cp", &format!("{c}:{cpath}"), &tmp.to_string_lossy()])
            .output()
            .await;
        match output {
            Ok(o) if o.status.success() => tmp.to_string_lossy().to_string(),
            Ok(o) => {
                return err(&format!(
                    "docker cp failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ))
            }
            Err(e) => return err(&format!("docker cp failed: {e}")),
        }
    } else {
        args.path.clone()
    };

    match that_tools::tools::fs::image::image_read(std::path::Path::new(&path_str)) {
        Ok(result) => {
            let meta = that_tools::tools::fs::image::ImageReadMeta::from(&result);
            let images = vec![(result.data, result.mime_type)];
            // Clean up temp file if we docker-cp'd
            if ctx.container.is_some() {
                let _ = std::fs::remove_file(&path_str);
            }
            DispatchResult {
                text: serde_json::to_string(&meta).unwrap_or_default(),
                images,
            }
        }
        Err(e) => {
            if ctx.container.is_some() {
                let _ = std::fs::remove_file(&path_str);
            }
            err(&e.to_string())
        }
    }
}

async fn dispatch_inner(
    name: &str,
    args_json: &str,
    ctx: &ToolContext,
) -> Result<serde_json::Value, ToolError> {
    let config = &ctx.config;
    let container = &ctx.container;
    let skill_roots = ctx.skill_roots.as_slice();
    let cluster_registry = ctx.cluster_registry.as_deref();
    let channel_registry = ctx.channel_registry.as_deref();
    let router = ctx.router.clone();
    let route_registry = ctx.route_registry.clone();
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
                ToolRequest::MemSearch {
                    query: args.query,
                    tags: args.tags,
                    limit: args.limit,
                    session_id: None,
                },
            )
            .await
        }
        // Legacy alias — routes to the same unified recall path.
        "mem_search" => {
            let args: MemRecallArgs = serde_json::from_str(args_json)
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
        "mem_remove" => {
            let args: MemRemoveArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(config.clone(), ToolRequest::MemRemove { id: args.id }).await
        }
        "mem_unpin" => {
            let args: MemUnpinArgs = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            run_on_host(config.clone(), ToolRequest::MemUnpin { id: args.id }).await
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
        "list_skills" => super::skill::dispatch_list_skills(skill_roots).await,
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
        // ── Cluster plugin tools ──────────────────────────────────────────
        "plugin_list" => {
            #[derive(Deserialize)]
            struct Args {
                agent_name: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json).unwrap_or(Args { agent_name: None });
            let reg =
                cluster_registry.ok_or_else(|| ToolError("cluster registry unavailable".into()))?;
            let plugins = reg.list().map_err(|e| ToolError(e.to_string()))?;
            let filtered: Vec<_> = plugins
                .iter()
                .filter(|p| args.agent_name.as_ref().is_none_or(|a| &p.owner_agent == a))
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "version": p.version,
                        "owner_agent": p.owner_agent,
                        "policy": p.policy.allow,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "plugins": filtered }))
        }
        "plugin_install" => {
            #[derive(Deserialize)]
            struct Args {
                manifest_path: String,
                owner_agent: String,
                #[serde(default)]
                skip_deploy: bool,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let manifest_str = tokio::fs::read_to_string(&args.manifest_path)
                .await
                .map_err(|e| ToolError(format!("read manifest: {e}")))?;
            let manifest: that_plugins::PluginManifest = toml::from_str(&manifest_str)
                .map_err(|e| ToolError(format!("parse manifest: {e}")))?;
            let manifest = manifest
                .validate(&args.manifest_path)
                .map_err(|e| ToolError(e.to_string()))?;
            let plugin_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".that-agent")
                .join("plugins");
            let deployed = if !args.skip_deploy {
                if let Some(deploy) = &manifest.deploy {
                    let backend = that_plugins::deploy::backend_for(deploy, &plugin_dir);
                    backend
                        .deploy(&manifest)
                        .await
                        .map_err(|e| ToolError(e.to_string()))?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            let reg =
                cluster_registry.ok_or_else(|| ToolError("cluster registry unavailable".into()))?;
            let plugin = reg
                .install(manifest, &args.owner_agent)
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({
                "id": plugin.id,
                "version": plugin.version,
                "owner_agent": plugin.owner_agent,
                "deployed": deployed,
            }))
        }
        "plugin_uninstall" => {
            #[derive(Deserialize)]
            struct Args {
                plugin_id: String,
                requestor_agent: String,
                #[serde(default = "default_true")]
                undeploy: bool,
            }
            fn default_true() -> bool {
                true
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg =
                cluster_registry.ok_or_else(|| ToolError("cluster registry unavailable".into()))?;
            let mut undeployed = false;
            if args.undeploy {
                if let Some(plugin) = reg
                    .find(&args.plugin_id)
                    .map_err(|e| ToolError(e.to_string()))?
                {
                    if let Some(deploy) = &plugin.manifest.deploy {
                        let plugin_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
                            .join(".that-agent")
                            .join("plugins");
                        let backend = that_plugins::deploy::backend_for(deploy, &plugin_dir);
                        backend
                            .undeploy(&args.plugin_id)
                            .await
                            .map_err(|e| ToolError(format!("undeploy failed: {e}")))?;
                        undeployed = true;
                    }
                }
            }
            reg.uninstall(&args.plugin_id, &args.requestor_agent)
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "status": "ok", "undeployed": undeployed }))
        }
        "plugin_status" => {
            #[derive(Deserialize)]
            struct Args {
                plugin_id: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg =
                cluster_registry.ok_or_else(|| ToolError("cluster registry unavailable".into()))?;
            let plugins = reg.list().map_err(|e| ToolError(e.to_string()))?;
            let plugin = plugins
                .iter()
                .find(|p| p.id == args.plugin_id)
                .ok_or_else(|| ToolError(format!("plugin '{}' not found", args.plugin_id)))?;
            let plugin_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".that-agent")
                .join("plugins");
            if let Some(deploy) = &plugin.manifest.deploy {
                let backend = that_plugins::deploy::backend_for(deploy, &plugin_dir);
                let status = backend
                    .status(&args.plugin_id)
                    .await
                    .map_err(|e| ToolError(e.to_string()))?;
                let (s, msg) = match status {
                    that_plugins::deploy::DeployStatus::Running => ("running", None),
                    that_plugins::deploy::DeployStatus::Stopped => ("stopped", None),
                    that_plugins::deploy::DeployStatus::Failed(m) => ("failed", Some(m)),
                    that_plugins::deploy::DeployStatus::Pending => ("pending", None),
                    that_plugins::deploy::DeployStatus::Deploying => ("deploying", None),
                    that_plugins::deploy::DeployStatus::Degraded => ("degraded", None),
                };
                Ok(serde_json::json!({ "id": args.plugin_id, "status": s, "message": msg }))
            } else {
                Ok(serde_json::json!({ "id": args.plugin_id, "status": "unknown" }))
            }
        }
        "plugin_set_policy" => {
            #[derive(Deserialize)]
            struct Args {
                plugin_id: String,
                allow: Vec<String>,
                requestor_agent: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg =
                cluster_registry.ok_or_else(|| ToolError("cluster registry unavailable".into()))?;
            reg.set_policy(&args.plugin_id, args.allow.clone(), &args.requestor_agent)
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "id": args.plugin_id, "policy": { "allow": args.allow } }))
        }
        // ── Dynamic channel tools ──────────────────────────────────────────
        "channel_list" => {
            let reg =
                channel_registry.ok_or_else(|| ToolError("channel registry unavailable".into()))?;
            let channels = reg.list().map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "channels": channels }))
        }
        "channel_register" => {
            #[derive(Deserialize)]
            struct Args {
                id: String,
                callback_url: String,
                capabilities: Vec<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg =
                channel_registry.ok_or_else(|| ToolError("channel registry unavailable".into()))?;
            let registered_at = chrono::Utc::now().to_rfc3339();
            let entry = that_channels::registry::ChannelEntry {
                id: args.id.clone(),
                callback_url: args.callback_url,
                capabilities: args.capabilities,
                registered_at: registered_at.clone(),
            };
            reg.register(entry.clone())
                .map_err(|e| ToolError(e.to_string()))?;
            if let Some(r) = router.as_ref() {
                let adapter = that_channels::adapters::GatewayChannelAdapter::new(entry);
                r.add_channel(std::sync::Arc::new(adapter)).await;
            }
            Ok(serde_json::json!({ "id": args.id, "registered_at": registered_at }))
        }
        "channel_unregister" => {
            #[derive(Deserialize)]
            struct Args {
                id: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg =
                channel_registry.ok_or_else(|| ToolError("channel registry unavailable".into()))?;
            reg.unregister(&args.id)
                .map_err(|e| ToolError(e.to_string()))?;
            if let Some(r) = router.as_ref() {
                r.remove_channel(&args.id).await;
            }
            Ok(serde_json::json!({ "status": "ok" }))
        }
        "provider_list" => {
            let registry = crate::provider_registry::DynamicProviderRegistry::from_default_path()
                .ok_or_else(|| ToolError("provider registry unavailable".into()))?;
            let providers = registry.list().map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "providers": providers }))
        }
        "provider_register" => {
            #[derive(Deserialize)]
            struct Args {
                id: String,
                base_url: String,
                api_key_env: String,
                #[serde(default)]
                models: Vec<String>,
                #[serde(default = "default_provider_transport")]
                transport: String,
            }
            fn default_provider_transport() -> String {
                "openai_chat".into()
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if args.transport != "openai_chat" {
                return Err(ToolError(
                    "unsupported transport: only openai_chat is supported right now".into(),
                ));
            }
            let id = crate::provider_registry::normalize_provider_id(&args.id)
                .ok_or_else(|| ToolError("invalid provider id".into()))?;
            if matches!(id.as_str(), "openai" | "anthropic" | "openrouter") {
                return Err(ToolError("cannot override a built-in provider".into()));
            }
            let registry = crate::provider_registry::DynamicProviderRegistry::from_default_path()
                .ok_or_else(|| ToolError("provider registry unavailable".into()))?;
            registry
                .register(crate::provider_registry::ProviderEntry {
                    id: id.clone(),
                    transport: args.transport,
                    base_url: args.base_url.trim().to_string(),
                    api_key_env: args.api_key_env.trim().to_string(),
                    models: args
                        .models
                        .into_iter()
                        .map(|model| model.trim().to_string())
                        .filter(|model| !model.is_empty())
                        .collect(),
                    registered_at: chrono::Utc::now().to_rfc3339(),
                })
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({
                "id": id,
                "transport": "openai_chat",
                "status": "ok"
            }))
        }
        "provider_unregister" => {
            #[derive(Deserialize)]
            struct Args {
                id: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let registry = crate::provider_registry::DynamicProviderRegistry::from_default_path()
                .ok_or_else(|| ToolError("provider registry unavailable".into()))?;
            registry
                .unregister(&args.id)
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "status": "ok" }))
        }
        // ── Dynamic gateway route tools ──────────────────────────────────────
        "gateway_route_register" => {
            #[derive(Deserialize)]
            struct Args {
                method: String,
                path: String,
                handler_type: String,
                body: Option<serde_json::Value>,
                command: Option<String>,
                timeout_secs: Option<u64>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg = route_registry
                .as_deref()
                .ok_or_else(|| ToolError("route registry unavailable".into()))?;
            let handler = match args.handler_type.as_str() {
                "static" => that_channels::RouteHandler::Static {
                    body: args
                        .body
                        .ok_or_else(|| ToolError("body required for static handler".into()))?,
                },
                "shell" => that_channels::RouteHandler::Shell {
                    command: args
                        .command
                        .ok_or_else(|| ToolError("command required for shell handler".into()))?,
                    timeout_secs: args.timeout_secs.unwrap_or(30),
                },
                other => return Err(ToolError(format!("unknown handler_type: {other}"))),
            };
            let route = that_channels::DynamicRoute {
                method: args.method.to_uppercase(),
                path: args.path.clone(),
                handler,
                registered_at: chrono::Utc::now().to_rfc3339(),
            };
            reg.register(route).map_err(|e| ToolError(e.to_string()))?;
            Ok(
                serde_json::json!({ "status": "ok", "path": args.path, "method": args.method.to_uppercase() }),
            )
        }
        "gateway_route_unregister" => {
            #[derive(Deserialize)]
            struct Args {
                method: String,
                path: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let reg = route_registry
                .as_deref()
                .ok_or_else(|| ToolError("route registry unavailable".into()))?;
            reg.unregister(&args.method.to_uppercase(), &args.path)
                .map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "status": "ok" }))
        }
        "gateway_route_list" => {
            let reg = route_registry
                .as_deref()
                .ok_or_else(|| ToolError("route registry unavailable".into()))?;
            let routes = reg.list().map_err(|e| ToolError(e.to_string()))?;
            Ok(serde_json::json!({ "routes": routes }))
        }
        // ── Identity / workspace file tools ──────────────────────────────────
        "identity_update" => {
            #[derive(Deserialize)]
            struct Args {
                file: String,
                content: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let agent_name = resolve_agent_name(config, skill_roots, None)
                .ok_or_else(|| ToolError("Cannot resolve agent name".into()))?;
            match container.as_deref() {
                Some(c) => crate::workspace::save_workspace_file_sandbox(
                    c,
                    &agent_name,
                    &args.file,
                    &args.content,
                ),
                None => crate::workspace::save_workspace_file_local(
                    &agent_name,
                    &args.file,
                    &args.content,
                ),
            }
            .map_err(ToolError)?;
            // Refresh the in-memory Status.md cache when the agent updates it.
            if args.file == "Status.md" {
                crate::orchestration::config::set_agent_status(Some(args.content.clone()));
            }
            Ok(serde_json::json!({ "status": "ok", "file": args.file }))
        }
        // ── HTTP request tool ─────────────────────────────────────────────────
        "http_request" => {
            #[derive(Deserialize)]
            struct Args {
                method: String,
                url: String,
                headers: Option<std::collections::HashMap<String, String>>,
                body: Option<String>,
                timeout_secs: Option<u64>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let method: reqwest::Method = args
                .method
                .parse()
                .map_err(|_| ToolError(format!("invalid HTTP method: {}", args.method)))?;
            let mut req = reqwest::Client::new()
                .request(method, &args.url)
                .timeout(Duration::from_secs(args.timeout_secs.unwrap_or(15)));
            for (k, v) in args.headers.unwrap_or_default() {
                req = req.header(k, v);
            }
            if let Some(body) = args.body {
                req = req.body(body);
            }
            let resp = req.send().await.map_err(|e| ToolError(e.to_string()))?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Ok(serde_json::json!({ "status": status, "body": body }))
        }
        // ── Multi-agent lifecycle tools ───────────────────────────────────────
        "spawn_agent" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
                role: Option<String>,
                gateway_port: Option<u16>,
                model: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let parent = resolve_agent_name(config, skill_roots, None);
            if crate::agents::is_k8s_mode() {
                crate::agents::spawn_persistent_agent_k8s(
                    &args.name,
                    args.role.as_deref(),
                    parent.as_deref().unwrap_or("root"),
                    args.model.as_deref(),
                )
                .await
                .map_err(|e| ToolError(e.to_string()))
            } else {
                let cluster_dir =
                    crate::agents::cluster_dir_from_db(Path::new(&config.memory.db_path))
                        .ok_or_else(|| {
                            ToolError("Cannot derive cluster dir from memory path".into())
                        })?;
                let reg = crate::agents::AgentRegistry::new(cluster_dir.join("agents.json"));
                let entry = crate::agents::spawn_agent(
                    &args.name,
                    args.role.as_deref(),
                    parent.as_deref(),
                    args.gateway_port,
                    args.model.as_deref(),
                    &reg,
                )
                .await
                .map_err(|e| ToolError(e.to_string()))?;
                Ok(serde_json::json!({
                    "name": entry.name,
                    "pid": entry.pid,
                    "gateway_url": entry.gateway_url,
                    "started_at": entry.started_at,
                }))
            }
        }
        "agent_run" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
                role: Option<String>,
                task: String,
                model: Option<String>,
                workspace: Option<bool>,
                timeout_secs: Option<u64>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            let parent =
                resolve_agent_name(config, skill_roots, None).unwrap_or_else(|| "root".to_string());
            if crate::agents::is_k8s_mode() {
                crate::agents::run_ephemeral_agent_k8s(
                    &args.name,
                    args.role.as_deref(),
                    &args.task,
                    &parent,
                    args.model.as_deref(),
                    args.workspace.unwrap_or(false),
                    args.timeout_secs.unwrap_or(300),
                )
                .await
                .map_err(|e| ToolError(e.to_string()))
            } else {
                // Local mode: run as a foreground query process
                let binary = std::env::current_exe()
                    .map_err(|e| ToolError(format!("cannot find binary: {e}")))?;
                let timeout = args.timeout_secs.unwrap_or(300);
                let mut cmd = tokio::process::Command::new(&binary);
                cmd.arg("--agent").arg(&args.name).arg("run").arg("query");
                if let Some(ref role) = args.role {
                    cmd.arg("--role").arg(role);
                }
                // Task is a positional arg — must come last
                cmd.arg(&args.task);
                cmd.env(
                    "THAT_PARENT_GATEWAY_URL",
                    crate::orchestration::support::resolve_gateway_url(),
                );
                if let Ok(tok) = std::env::var("THAT_GATEWAY_TOKEN") {
                    cmd.env("THAT_PARENT_GATEWAY_TOKEN", tok);
                }
                let start = std::time::Instant::now();
                let output = tokio::time::timeout(Duration::from_secs(timeout), cmd.output())
                    .await
                    .map_err(|_| ToolError(format!("agent_run timed out after {timeout}s")))?
                    .map_err(|e| ToolError(e.to_string()))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let status = if output.status.success() {
                    "succeeded"
                } else {
                    "failed"
                };
                Ok(serde_json::json!({
                    "name": args.name,
                    "status": status,
                    "output": stdout,
                    "elapsed_secs": start.elapsed().as_secs(),
                }))
            }
        }
        "agent_list" => {
            if crate::agents::is_k8s_mode() {
                crate::agents::list_agents_k8s()
                    .await
                    .map_err(|e| ToolError(e.to_string()))
            } else {
                let cluster_dir =
                    crate::agents::cluster_dir_from_db(Path::new(&config.memory.db_path))
                        .ok_or_else(|| {
                            ToolError("Cannot derive cluster dir from memory path".into())
                        })?;
                let reg = crate::agents::AgentRegistry::new(cluster_dir.join("agents.json"));
                let entries = reg.list().map_err(|e| ToolError(e.to_string()))?;
                let agents: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "name": e.name,
                            "role": e.role,
                            "parent": e.parent,
                            "pid": e.pid,
                            "gateway_url": e.gateway_url,
                            "started_at": e.started_at,
                            "alive": crate::agents::AgentRegistry::is_alive(e.pid),
                        })
                    })
                    .collect();
                Ok(serde_json::json!({ "agents": agents }))
            }
        }
        "agent_query" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
                message: String,
                timeout_secs: Option<u64>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if crate::agents::is_k8s_mode() {
                crate::agents::query_agent_k8s(
                    &args.name,
                    &args.message,
                    args.timeout_secs.unwrap_or(60),
                )
                .await
                .map_err(|e| ToolError(e.to_string()))
            } else {
                let cluster_dir =
                    crate::agents::cluster_dir_from_db(Path::new(&config.memory.db_path))
                        .ok_or_else(|| {
                            ToolError("Cannot derive cluster dir from memory path".into())
                        })?;
                let reg = crate::agents::AgentRegistry::new(cluster_dir.join("agents.json"));
                let entries = reg.list().map_err(|e| ToolError(e.to_string()))?;
                let entry = entries
                    .iter()
                    .find(|e| e.name == args.name)
                    .ok_or_else(|| {
                        ToolError(format!("agent '{}' not found in registry", args.name))
                    })?;
                let gw = entry.gateway_url.as_deref().ok_or_else(|| {
                    ToolError(format!("agent '{}' has no gateway URL", args.name))
                })?;
                let resp =
                    crate::agents::query_agent(gw, &args.message, args.timeout_secs.unwrap_or(60))
                        .await
                        .map_err(|e| ToolError(e.to_string()))?;
                Ok(serde_json::json!({ "agent": args.name, "response": resp }))
            }
        }
        "agent_unregister" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            if crate::agents::is_k8s_mode() {
                crate::agents::unregister_agent_k8s(&args.name)
                    .await
                    .map_err(|e| ToolError(e.to_string()))
            } else {
                let cluster_dir =
                    crate::agents::cluster_dir_from_db(Path::new(&config.memory.db_path))
                        .ok_or_else(|| {
                            ToolError("Cannot derive cluster dir from memory path".into())
                        })?;
                let reg = crate::agents::AgentRegistry::new(cluster_dir.join("agents.json"));
                reg.unregister(&args.name)
                    .map_err(|e| ToolError(e.to_string()))?;
                Ok(serde_json::json!({ "name": args.name, "status": "unregistered" }))
            }
        }
        "agent_stop" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::agent_stop_k8s(&args.name)
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "agent_status" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::agent_status_k8s(&args.name)
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "agent_logs" => {
            #[derive(Deserialize)]
            struct Args {
                name: String,
                tail: Option<u32>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::agent_logs_k8s(&args.name, args.tail.unwrap_or(50))
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "workspace_share" => {
            #[derive(Deserialize)]
            struct Args {
                path: String,
                name: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::workspace_share(&args.path, args.name.as_deref())
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "workspace_collect" => {
            #[derive(Deserialize)]
            struct Args {
                path: String,
                worker: String,
                strategy: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::workspace_collect(
                &args.path,
                &args.worker,
                args.strategy.as_deref().unwrap_or("merge"),
            )
            .await
            .map_err(|e| ToolError(e.to_string()))
        }
        "workspace_activity" => {
            #[derive(Deserialize)]
            struct Args {
                repo: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json).unwrap_or(Args { repo: None });
            crate::agents::workspace_activity(args.repo.as_deref())
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "workspace_diff" => {
            #[derive(Deserialize)]
            struct Args {
                branch: String,
                repo: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::workspace_branch_diff(&args.branch, args.repo.as_deref())
                .await
                .map_err(|e| ToolError(e.to_string()))
        }
        "workspace_conflicts" => {
            #[derive(Deserialize)]
            struct Args {
                branch: String,
                repo: Option<String>,
            }
            let args: Args = serde_json::from_str(args_json)
                .map_err(|e| ToolError(format!("invalid args: {e}")))?;
            crate::agents::workspace_conflicts(&args.branch, args.repo.as_deref())
                .await
                .map_err(|e| ToolError(e.to_string()))
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
#[allow(clippy::await_holding_lock)]
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

    fn test_ctx(config: ThatToolsConfig, container: Option<String>) -> ToolContext {
        ToolContext {
            config,
            container,
            skill_roots: vec![],
            cluster_registry: None,
            channel_registry: None,
            router: None,
            route_registry: None,
            state_dir: None,
            agent_name: String::new(),
        }
    }

    #[tokio::test]
    async fn fs_cat_still_works_on_host_without_container() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let path = unique_tmp_path("host-fs-read.txt");
        let path_str = path.to_string_lossy().to_string();
        fs::write(&path, "host-read-only").expect("setup should create test file");
        let ctx = test_ctx(ThatToolsConfig::default(), None);
        let result = dispatch(
            "fs_cat",
            &serde_json::json!({"path": path_str}).to_string(),
            &ctx,
        )
        .await;
        assert!(
            result.text.contains("host-read-only"),
            "fs_cat output should contain file content"
        );
        let _ = fs::remove_file(path);
    }

    #[tokio::test]
    async fn fs_write_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let ctx = test_ctx(ThatToolsConfig::default(), None);
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": "tmp/test.txt", "content": "x"}).to_string(),
            &ctx,
        )
        .await;
        assert!(
            result.text.contains("sandbox required"),
            "fs_write should reject local mode"
        );
    }

    #[tokio::test]
    async fn fs_rm_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let ctx = test_ctx(ThatToolsConfig::default(), None);
        let result = dispatch("fs_rm", &serde_json::json!({"path": unique_tmp_path("policy-deny").to_string_lossy().to_string()}).to_string(), &ctx).await;
        assert!(result.text.contains("sandbox required"));
    }

    #[tokio::test]
    async fn shell_exec_requires_sandbox() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let ctx = test_ctx(ThatToolsConfig::default(), None);
        let result = dispatch(
            "shell_exec",
            &serde_json::json!({"command": "printf host-shell"}).to_string(),
            &ctx,
        )
        .await;
        assert!(result.text.contains("sandbox required"));
    }

    #[tokio::test]
    async fn sandbox_mode_attempts_docker_exec_for_fs_write() {
        let _lock = env_lock();
        let _trusted = EnvVarGuard::set(TRUSTED_LOCAL_SANDBOX_ENV, Some("0"));
        let _mode = EnvVarGuard::set(SANDBOX_MODE_ENV, Some("docker"));
        let ctx = test_ctx(
            ThatToolsConfig::default(),
            Some("that-core-missing-container".to_string()),
        );
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": "tmp/test.txt", "content": "x"}).to_string(),
            &ctx,
        )
        .await;
        assert!(
            !result.text.contains("sandbox required"),
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
        let ctx = test_ctx(permissive_config(), None);
        let result = dispatch(
            "fs_write",
            &serde_json::json!({"path": target_str, "content": "trusted"}).to_string(),
            &ctx,
        )
        .await;
        assert!(
            !result.text.contains("error"),
            "trusted local mode should allow fs_write: {}",
            result.text
        );
        let written = fs::read_to_string(&target).expect("file should have been written");
        assert_eq!(written, "trusted");
        let result = dispatch(
            "shell_exec",
            &serde_json::json!({"command": "printf trusted-local-shell"}).to_string(),
            &ctx,
        )
        .await;
        assert!(
            result.text.contains("trusted-local-shell"),
            "shell_exec result should contain output: {}",
            result.text
        );
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
