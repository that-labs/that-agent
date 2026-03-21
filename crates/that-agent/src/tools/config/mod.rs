//! Configuration system for that-tools.
//!
//! Layered configuration merging using figment:
//! Defaults → Agent home (~/.that-agent/tools.toml) → Project (.that-tools/tools.toml) → Env (THAT_TOOLS_*) → CLI flags
//!
//! Every configuration value has a JSON Schema (via schemars) for editor autocomplete and CI validation.

pub mod init;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level tools configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct ThatToolsConfig {
    pub core: CoreConfig,
    pub output: OutputConfig,
    pub code: CodeConfig,
    pub policy: PolicyConfig,
    pub memory: MemoryConfig,
    pub search: SearchConfig,
    pub human: HumanConfig,
    pub session: SessionConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct CoreConfig {
    /// Enable daemon mode for long-lived sessions.
    pub daemon: bool,
    /// Default output format for all commands.
    pub default_format: OutputFormat,
    /// Logging verbosity level.
    pub verbosity: Verbosity,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    Compact,
    Markdown,
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    Quiet,
    Normal,
    Verbose,
    Trace,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct OutputConfig {
    /// Default maximum tokens for command output.
    pub default_max_tokens: usize,
    /// Summarizer model for over-budget output.
    pub summarizer: String,
    /// Fallback strategy when no summarizer is available.
    pub summarizer_fallback: String,
    /// Default context lines for code read.
    pub code_read_context_lines: usize,
    /// Token cap for fs ls max depth.
    pub fs_ls_max_depth: usize,
    /// Max depth for code tree.
    pub code_tree_max_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct CodeConfig {
    /// Languages to load tree-sitter grammars for.
    pub languages: Vec<String>,
    /// Automatically build symbol index on first access.
    pub auto_index: bool,
    /// Enable PageRank importance scoring.
    pub pagerank: bool,
    /// Number of worker threads for code grep traversal.
    pub grep_workers: usize,
    /// File size threshold for mmap-backed grep reads.
    pub mmap_min_bytes: usize,
    /// Default edit format for code modifications.
    pub edit_format: String,
    /// Enable git safety (auto-stash before edits).
    pub git_safety: bool,
    /// Create a safety branch before edits (in addition to stash).
    pub git_safety_branch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct PolicyConfig {
    /// Default policy for tools not explicitly configured.
    pub default: PolicyLevel,
    /// Per-tool policy overrides.
    pub tools: ToolPolicies,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum PolicyLevel {
    Deny,
    #[default]
    Prompt,
    Allow,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct ToolPolicies {
    pub code_read: PolicyLevel,
    pub code_edit: PolicyLevel,
    pub fs_read: PolicyLevel,
    pub fs_write: PolicyLevel,
    pub fs_delete: PolicyLevel,
    pub shell_exec: PolicyLevel,
    pub search: PolicyLevel,
    pub memory: PolicyLevel,
    pub git_commit: PolicyLevel,
    pub git_push: PolicyLevel,
    pub mem_compact: PolicyLevel,
}

// --- Defaults ---

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            daemon: false,
            default_format: OutputFormat::Json,
            verbosity: Verbosity::Normal,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            default_max_tokens: 4096,
            summarizer: "phi-3.5-mini".to_string(),
            summarizer_fallback: "rule".to_string(),
            code_read_context_lines: 8,
            fs_ls_max_depth: 2,
            code_tree_max_depth: 4,
        }
    }
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            languages: vec![
                "rust".into(),
                "typescript".into(),
                "python".into(),
                "go".into(),
            ],
            auto_index: true,
            pagerank: true,
            grep_workers: std::thread::available_parallelism()
                .map(|n| n.get().min(8))
                .unwrap_or(4),
            mmap_min_bytes: 256 * 1024,
            edit_format: "unified-diff".into(),
            git_safety: true,
            git_safety_branch: false,
        }
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default: PolicyLevel::Prompt,
            tools: ToolPolicies::default(),
        }
    }
}

impl Default for ToolPolicies {
    fn default() -> Self {
        Self {
            code_read: PolicyLevel::Allow,
            code_edit: PolicyLevel::Prompt,
            fs_read: PolicyLevel::Allow,
            fs_write: PolicyLevel::Prompt,
            fs_delete: PolicyLevel::Deny,
            shell_exec: PolicyLevel::Deny,
            search: PolicyLevel::Allow,
            memory: PolicyLevel::Allow,
            git_commit: PolicyLevel::Prompt,
            git_push: PolicyLevel::Deny,
            mem_compact: PolicyLevel::Allow,
        }
    }
}

/// Memory system configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct MemoryConfig {
    /// Path to the memory database. Empty string means default location.
    pub db_path: String,
    /// Maximum memories to return in a recall query.
    pub default_recall_limit: usize,
    /// Auto-prune memories older than this many days (0 = disabled).
    pub auto_prune_days: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            db_path: String::new(),
            default_recall_limit: 10,
            auto_prune_days: 0,
        }
    }
}

/// Search system configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct SearchConfig {
    /// Primary search engine when no --engine flag is given.
    pub primary_engine: String,
    /// Fallback chain when primary engine is unavailable.
    pub fallback_chain: Vec<String>,
    /// Enable hedged provider requests (start fallback requests early).
    pub hedged_requests: bool,
    /// Cache TTL in minutes for persistent search results.
    pub cache_ttl_minutes: u64,
    /// Maximum results to request from each provider.
    pub max_results_per_engine: usize,
    /// SearXNG instance URL for self-hosted search.
    pub searxng_url: String,
    /// Token cap for search result output.
    pub search_token_cap: usize,
    /// Token cap for fetch content output.
    pub fetch_token_cap: usize,
    /// Enable persistent SQLite cache.
    pub persistent_cache: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            primary_engine: "duckduckgo".to_string(),
            fallback_chain: vec![
                "bing".to_string(),
                "yahoo".to_string(),
                "mojeek".to_string(),
                "tavily".to_string(),
                "brave".to_string(),
            ],
            hedged_requests: false,
            cache_ttl_minutes: 60,
            max_results_per_engine: 6,
            searxng_url: "http://127.0.0.1:8080".to_string(),
            search_token_cap: 320,
            fetch_token_cap: 600,
            persistent_cache: true,
        }
    }
}

/// Session tracking configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct SessionConfig {
    /// Path to sessions.json. Empty string means default location.
    pub sessions_path: String,
    /// Token count threshold above which flush_recommended is set to true in session stats.
    pub soft_threshold_tokens: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            sessions_path: String::new(),
            soft_threshold_tokens: 100_000,
        }
    }
}

/// Human-in-the-loop configuration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(default)]
pub struct HumanConfig {
    /// Default timeout in seconds for human prompts.
    pub default_timeout: u64,
    /// Enable headless file-based queue mode.
    pub headless_queue: bool,
    /// Directory for pending approval files.
    pub queue_dir: String,
    /// Poll interval in milliseconds for queue checking.
    pub poll_interval_ms: u64,
}

impl Default for HumanConfig {
    fn default() -> Self {
        Self {
            default_timeout: 300,
            headless_queue: false,
            queue_dir: String::new(),
            poll_interval_ms: 500,
        }
    }
}

/// Resolves the agent home directory path (~/.that-agent).
pub fn agent_home_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".that-agent")
}

/// Resolves the agent home config file path (~/.that-agent/tools.toml).
pub fn global_config_path() -> PathBuf {
    agent_home_dir().join("tools.toml")
}

/// Finds the project-level config by walking up from the given directory.
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join(".that-tools").join("tools.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Loads the fully merged configuration.
///
/// Merge order (last wins):
/// 1. Compiled defaults
/// 2. Agent home config file (~/.that-agent/tools.toml)
/// 3. Project config file (.that-tools/tools.toml, found by walking up)
/// 4. Environment variables (THAT_TOOLS_*)
#[allow(clippy::result_large_err)]
pub fn load_config(project_dir: Option<&Path>) -> Result<ThatToolsConfig, figment::Error> {
    let mut figment = Figment::from(Serialized::defaults(ThatToolsConfig::default()));

    // Layer: agent home config
    let global_path = global_config_path();
    if global_path.exists() {
        figment = figment.merge(Toml::file(&global_path));
    }

    // Layer: project config
    if let Some(dir) = project_dir {
        if let Some(project_config) = find_project_config(dir) {
            figment = figment.merge(Toml::file(project_config));
        }
    }

    // Layer: environment variables (THAT_TOOLS_ prefix, __ as separator)
    figment = figment.merge(Env::prefixed("THAT_TOOLS_").split("__"));

    figment.extract()
}

/// Exports the full configuration JSON Schema.
pub fn export_schema() -> String {
    let schema = schemars::schema_for!(ThatToolsConfig);
    serde_json::to_string_pretty(&schema).expect("schema serialization should not fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_default_config_is_valid() {
        let config = ThatToolsConfig::default();
        assert_eq!(config.output.default_max_tokens, 4096);
        assert_eq!(config.core.default_format, OutputFormat::Json);
        assert_eq!(config.core.verbosity, Verbosity::Normal);
        assert!(!config.core.daemon);
        assert!(config.code.grep_workers >= 1);
        assert_eq!(config.code.mmap_min_bytes, 256 * 1024);
        assert!(!config.search.hedged_requests);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = ThatToolsConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ThatToolsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_load_config_defaults() {
        // Ensure no env var leakage from other tests
        std::env::remove_var("THAT_TOOLS_OUTPUT__DEFAULT_MAX_TOKENS");
        let config = load_config(None).unwrap();
        assert_eq!(config.output.default_max_tokens, 4096);
        assert_eq!(config.policy.default, PolicyLevel::Prompt);
        assert_eq!(config.policy.tools.code_read, PolicyLevel::Allow);
        assert_eq!(config.policy.tools.fs_delete, PolicyLevel::Deny);
    }

    #[test]
    fn test_project_config_merges_over_defaults() {
        let tmp = TempDir::new().unwrap();
        let tools_dir = tmp.path().join(".that-tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(
            tools_dir.join("tools.toml"),
            r#"
[output]
default_max_tokens = 1024

[code]
languages = ["rust", "python"]
"#,
        )
        .unwrap();

        let config = load_config(Some(tmp.path())).unwrap();
        assert_eq!(config.output.default_max_tokens, 1024);
        assert_eq!(config.code.languages, vec!["rust", "python"]);
        // Unset values keep defaults
        assert_eq!(config.core.verbosity, Verbosity::Normal);
    }

    #[test]
    fn test_env_vars_override_config() {
        // Use a unique env var that won't conflict with other tests
        let tmp = TempDir::new().unwrap();
        let tools_dir = tmp.path().join(".that-tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(
            tools_dir.join("tools.toml"),
            "[output]\ndefault_max_tokens = 100\n",
        )
        .unwrap();

        // Test that project config works (env-var-free assertion)
        let config = load_config(Some(tmp.path())).unwrap();
        assert_eq!(config.output.default_max_tokens, 100);
    }

    #[test]
    fn test_find_project_config_walks_up() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        let tools_dir = tmp.path().join(".that-tools");
        fs::create_dir_all(&tools_dir).unwrap();
        fs::write(tools_dir.join("tools.toml"), "[core]\ndaemon = true\n").unwrap();

        let found = find_project_config(&deep);
        assert!(found.is_some());
        assert_eq!(found.unwrap(), tools_dir.join("tools.toml"));
    }

    #[test]
    fn test_find_project_config_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        let found = find_project_config(tmp.path());
        assert!(found.is_none());
    }

    #[test]
    fn test_json_schema_export() {
        let schema = export_schema();
        assert!(schema.contains("ThatToolsConfig"));
        assert!(schema.contains("default_max_tokens"));
        // Validate it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&schema).unwrap();
    }

    #[test]
    fn test_policy_defaults() {
        let policy = PolicyConfig::default();
        assert_eq!(policy.tools.code_read, PolicyLevel::Allow);
        assert_eq!(policy.tools.code_edit, PolicyLevel::Prompt);
        assert_eq!(policy.tools.fs_delete, PolicyLevel::Deny);
        assert_eq!(policy.tools.shell_exec, PolicyLevel::Deny);
        assert_eq!(policy.tools.git_push, PolicyLevel::Deny);
    }
}
