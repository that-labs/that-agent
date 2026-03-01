//! CLI definition for that-tools using clap derive.
//!
//! Defines the command structure, global flags, and subcommands.
//! Every command supports `--json` output and `--max-tokens` budget control.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub use session_cmds::SessionCommands;
mod session_cmds {
    use clap::Subcommand;
    #[derive(Subcommand, Debug)]
    pub enum SessionCommands {
        /// Initialize or retrieve a session record.
        Init {
            /// Session identifier. A new UUID is generated if omitted.
            #[arg(long)]
            session_id: Option<String>,
        },
        /// Show accumulated token usage and compaction count for a session.
        Stats {
            /// Session identifier.
            #[arg(long)]
            session_id: String,
        },
        /// Add tokens to a session's accumulated context count.
        AddTokens {
            /// Session identifier.
            #[arg(long)]
            session_id: String,
            /// Number of tokens to add.
            #[arg(long)]
            tokens: usize,
        },
        /// Reset the context token counter for a session.
        ///
        /// Call this after a successful compaction to signal that the context
        /// window has been cleared. The next `stats` call will show a low
        /// context_tokens and flush_recommended=false.
        ResetContext {
            /// Session identifier.
            #[arg(long)]
            session_id: String,
            /// Token count to reset to (default: 0).
            #[arg(long, default_value = "0")]
            to: usize,
        },
    }
}

/// that-tools — The Agent Tool Layer.
///
/// Structural code comprehension, federated search, persistent memory,
/// and governance for any LLM-powered agent.
#[derive(Parser, Debug)]
#[command(
    name = "that",
    version,
    about = "The operating layer every agent is forged on.",
    long_about = "that-tools gives any LLM-powered agent structural code comprehension, \
    federated search, persistent local memory, human-in-the-loop governance, \
    and hard token-budget enforcement on every output."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Output format (overrides config default).
    #[arg(long, global = true)]
    pub format: Option<OutputFormatArg>,

    /// Maximum tokens for output (overrides config).
    #[arg(long, global = true)]
    pub max_tokens: Option<usize>,

    /// Verbosity level.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all non-essential output.
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OutputFormatArg {
    Json,
    Compact,
    Markdown,
    Raw,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// File system operations with token-minimal output.
    Fs {
        #[command(subcommand)]
        command: FsCommands,
    },
    /// Code analysis with AST-aware structural comprehension.
    Code {
        #[command(subcommand)]
        command: CodeCommands,
    },
    /// Export the configuration JSON Schema.
    ConfigSchema,
    /// Persistent memory operations.
    Mem {
        #[command(subcommand)]
        command: MemCommands,
    },
    /// Human-in-the-loop interaction.
    Human {
        #[command(subcommand)]
        command: HumanCommands,
    },
    /// Federated web search and URL fetching.
    Search {
        #[command(subcommand)]
        command: SearchCommands,
    },
    /// View tool skill documentation.
    Skills {
        #[command(subcommand)]
        command: SkillsCommands,
    },
    /// Daemon mode (long-lived process).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Execute a shell command with policy governance.
    Exec {
        #[command(subcommand)]
        command: ExecCommands,
    },
    /// Initialize a project with a default configuration profile.
    Init {
        /// Configuration profile to use.
        #[arg(long, default_value = "safe")]
        profile: InitProfile,
        /// Overwrite existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Session tracking: token accumulation and compaction events.
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },
}

#[derive(Subcommand, Debug)]
pub enum FsCommands {
    /// List directory contents (token-minimal output).
    #[command(alias = "list")]
    Ls {
        /// Path to list.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Maximum directory depth.
        #[arg(long)]
        max_depth: Option<usize>,
    },
    /// Read file content (budget-limited output).
    #[command(aliases = ["read", "open", "view"])]
    Cat {
        /// Path to file.
        path: PathBuf,
    },
    /// Write content to a file. Use --content for inline text or pipe via stdin.
    Write {
        /// Destination file path.
        path: PathBuf,
        /// Content to write. Interprets \n as newline. Alternative to piping via stdin.
        #[arg(long)]
        content: Option<String>,
        /// Preview changes without writing.
        #[arg(long)]
        dry_run: bool,
        /// Create a backup (.bak) of existing file before overwriting.
        #[arg(long)]
        backup: bool,
    },
    /// Create a directory.
    Mkdir {
        /// Directory path to create.
        path: PathBuf,
        /// Create parent directories as needed.
        #[arg(long, short)]
        parents: bool,
    },
    /// Remove a file or directory.
    Rm {
        /// Path to remove.
        path: PathBuf,
        /// Remove directories and their contents recursively.
        #[arg(long, short)]
        recursive: bool,
        /// Preview what would be removed without actually removing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum CodeCommands {
    /// Read source file with AST-aware structural context.
    Read {
        /// Path to source file.
        path: PathBuf,
        /// Number of context lines around focus point.
        #[arg(long, short)]
        context: Option<usize>,
        /// Include symbol annotations in output.
        #[arg(long)]
        symbols: bool,
        /// Start line (1-based). Use alone for focus+context, or with --end-line for a range.
        #[arg(long)]
        line: Option<usize>,
        /// End line (1-based). When set, returns exactly lines --line through --end-line.
        #[arg(long)]
        end_line: Option<usize>,
    },
    /// Search code with keyword matching and context.
    #[command(aliases = ["search", "find", "rg"])]
    Grep {
        /// Search pattern.
        pattern: String,
        /// Root directory to search.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Context lines around each match (default from config).
        #[arg(long, short)]
        context: Option<usize>,
        /// Maximum number of returned matches. All files are still searched for accurate total counts.
        #[arg(long)]
        limit: Option<usize>,
        /// Case-insensitive search.
        #[arg(long, short)]
        ignore_case: bool,
        /// Interpret pattern as a regular expression.
        #[arg(long, short = 'r')]
        regex: bool,
        /// Include only files matching these glob patterns (repeatable).
        #[arg(long, short = 'g')]
        include: Vec<String>,
        /// Exclude files matching these glob patterns (repeatable, takes precedence over include).
        #[arg(long, short = 'e')]
        exclude: Vec<String>,
    },
    /// Repository tree map (compact, .gitignore-aware).
    Tree {
        /// Root directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Maximum depth (default from config).
        #[arg(long, short)]
        depth: Option<usize>,
        /// Compact ASCII tree format.
        #[arg(long)]
        compact: bool,
        /// Rank by importance (Phase 2: PageRank).
        #[arg(long)]
        ranked: bool,
    },
    /// List symbols in a file or project.
    Symbols {
        /// Path to file or directory.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Filter by symbol kind.
        #[arg(long)]
        kind: Option<String>,
        /// Filter by symbol name pattern.
        #[arg(long)]
        name: Option<String>,
        /// Include cross-file references from the index.
        #[arg(long)]
        references: bool,
    },
    /// Build or update the symbol index.
    Index {
        /// Project root directory to index.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Show index health status instead of building.
        #[arg(long)]
        status: bool,
    },
    /// Edit a source file with syntax validation.
    Edit {
        /// Path to file to edit.
        path: PathBuf,
        /// Apply a unified diff from stdin.
        #[arg(long, conflicts_with_all = ["search", "replace", "target_fn", "new_body", "whole_file"])]
        patch: bool,
        /// Search text to find in the file.
        #[arg(long, requires = "replace", conflicts_with_all = ["patch", "target_fn", "new_body", "whole_file"])]
        search: Option<String>,
        /// Replacement text (used with --search).
        #[arg(long, requires = "search")]
        replace: Option<String>,
        /// Target function/symbol name for AST-node replacement.
        #[arg(long = "fn", requires = "new_body", conflicts_with_all = ["patch", "search", "replace", "whole_file"])]
        target_fn: Option<String>,
        /// New body for the target symbol (used with --fn).
        #[arg(long, requires = "target_fn")]
        new_body: Option<String>,
        /// Replace entire file content from stdin.
        #[arg(long, conflicts_with_all = ["patch", "search", "replace", "target_fn", "new_body"])]
        whole_file: bool,
        /// Replace all occurrences (with --search/--replace). Without this, only the first match is replaced.
        #[arg(long, requires = "search")]
        all: bool,
        /// Preview changes without applying.
        #[arg(long)]
        dry_run: bool,
    },
    /// Architecture summary: module structure, public API, and dependencies.
    Summary {
        /// Root directory to summarize.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Structural code search using tree-sitter S-expression queries.
    ///
    /// Pattern syntax uses tree-sitter query S-expressions, NOT metavar syntax.
    ///
    /// Examples:
    ///   Rust functions:   '(function_item name: (identifier) @name)'
    ///   Rust structs:     '(struct_item name: (type_identifier) @name)'
    ///   Python functions:  '(function_definition name: (identifier) @name)'
    ///   TS/JS classes:    '(class_declaration name: (type_identifier) @name)'
    ///   Go functions:     '(function_declaration name: (identifier) @name)'
    ///
    /// Use @name captures to extract matched node text.
    AstGrep {
        /// Tree-sitter S-expression query (e.g. '(function_item name: (identifier) @name)').
        pattern: String,
        /// Root directory to search.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Language filter.
        #[arg(long)]
        language: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MemCommands {
    /// Add a memory entry (with automatic near-duplicate detection).
    Add {
        /// Memory content to store.
        content: String,
        /// Tags for categorization.
        #[arg(long, short, value_delimiter = ',')]
        tags: Vec<String>,
        /// Source attribution.
        #[arg(long)]
        source: Option<String>,
        /// Scope this memory to a specific session.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Recall memories with recency-boosted ranking and substring fallback.
    Recall {
        /// Natural language query.
        query: String,
        /// Maximum results to return.
        #[arg(long, default_value = "5")]
        limit: usize,
        /// Restrict recall to a specific session (omit for global recall).
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Search memories with optional tag filtering.
    Search {
        /// Search query.
        query: String,
        /// Filter by tags.
        #[arg(long, short, value_delimiter = ',')]
        tags: Vec<String>,
        /// Maximum results.
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Restrict search to a specific session.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Store a durable compaction summary as a pinned memory entry.
    ///
    /// The summary is stored with source="compaction" and pinned=true,
    /// so it always floats to the top of recall results. Use this before
    /// a context window rolls over to preserve key decisions.
    Compact {
        /// The compaction summary text.
        #[arg(long)]
        summary: String,
        /// Associate the compaction entry with a specific session.
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Demote a pinned memory entry back to unpinned.
    ///
    /// Use this to retire a stale compaction summary so it no longer
    /// dominates recall results. The entry is kept in the store (use
    /// `remove` to delete it entirely).
    Unpin {
        /// Memory ID to unpin (get the ID from recall or stats output).
        id: String,
    },
    /// Remove a specific memory by ID.
    Remove {
        /// Memory ID to remove.
        id: String,
    },
    /// Remove old or low-access memories.
    Prune {
        /// Remove memories older than N days.
        #[arg(long)]
        before_days: Option<u64>,
        /// Remove memories accessed fewer than N times.
        #[arg(long)]
        min_access: Option<u64>,
    },
    /// Show memory store statistics.
    Stats,
    /// Export all memories to JSON on stdout.
    Export,
    /// Import memories from JSON on stdin.
    Import,
}

#[derive(Subcommand, Debug)]
pub enum HumanCommands {
    /// Ask a question and wait for a response.
    Ask {
        /// The question or message to display.
        message: String,
        /// Timeout in seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Approve a pending request.
    Approve {
        /// Request ID to approve.
        id: String,
        /// Optional response message.
        #[arg(long)]
        response: Option<String>,
    },
    /// Confirm a pending action.
    Confirm {
        /// Request ID to confirm.
        id: String,
    },
    /// List pending approval requests.
    Pending,
}

#[derive(Subcommand, Debug)]
pub enum SearchCommands {
    /// Search the web using federated providers.
    Query {
        /// The search query string.
        query: String,
        /// Force a specific search engine.
        #[arg(long)]
        engine: Option<String>,
        /// Maximum number of results.
        #[arg(long, default_value = "5")]
        limit: usize,
        /// Skip cache and force a fresh search.
        #[arg(long)]
        no_cache: bool,
    },
    /// Fetch one or more URLs and analyze or extract their content.
    ///
    /// Fetches all URLs in parallel. Default mode is 'scrape', which runs a
    /// Python scraper automatically and returns structured JSON in scraped_content.
    /// Use 'inspect' to get DOM structure analysis and write your own extractor.
    Fetch {
        /// One or more URLs to fetch (fetched in parallel).
        #[arg(num_args = 1..)]
        urls: Vec<String>,
        /// Fetch mode: scrape (auto-run Python scraper, returns JSON content),
        /// inspect (DOM structure analysis — use data to write your own extractor),
        /// markdown, or text.
        #[arg(long, default_value = "scrape")]
        mode: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SkillsCommands {
    /// List all available skill categories.
    List,
    /// Read documentation for a specific skill.
    Read {
        /// Skill name (code, fs, search, memory, human, index).
        skill: String,
    },
    /// Install skills as SKILL.md files for agent auto-discovery.
    ///
    /// Creates `<path>/that-tools-<name>/SKILL.md` for each skill.
    /// Defaults to ~/.claude/skills/ (Claude Code convention).
    Install {
        /// Specific skill to install (omit to install all).
        skill: Option<String>,
        /// Destination directory for skill folders.
        /// Defaults to ~/.claude/skills/
        #[arg(long)]
        path: Option<std::path::PathBuf>,
        /// Overwrite existing SKILL.md files.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ExecCommands {
    /// Run a shell command.
    Run {
        /// The command to execute (passed to sh -c).
        command: String,
        /// Working directory for the command.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Timeout in seconds (default 30).
        #[arg(long, default_value = "30")]
        timeout: u64,
        /// How to terminate timed-out processes: graceful (SIGTERM then SIGKILL) or immediate (SIGKILL).
        #[arg(long, default_value = "graceful")]
        signal: SignalModeArg,
        /// Stream output lines as JSONL to stderr in real time.
        #[arg(long)]
        stream: bool,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum SignalModeArg {
    Graceful,
    Immediate,
}

/// Profile for `that tools config-init` — controls default policy levels.
#[derive(Debug, Clone, clap::ValueEnum)]
pub enum InitProfile {
    Safe,
    Agent,
    Unrestricted,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommands {
    /// Start the daemon process.
    Start,
    /// Stop the daemon process.
    Stop,
    /// Check daemon status.
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses_fs_ls() {
        let cli = Cli::parse_from(["that", "fs", "ls", "src/"]);
        match cli.command {
            Commands::Fs {
                command: FsCommands::Ls { path, max_depth },
            } => {
                assert_eq!(path, PathBuf::from("src/"));
                assert_eq!(max_depth, None);
            }
            _ => panic!("expected Fs Ls command"),
        }
    }

    #[test]
    fn test_cli_parses_fs_cat() {
        let cli = Cli::parse_from(["that", "fs", "cat", "main.rs"]);
        match cli.command {
            Commands::Fs {
                command: FsCommands::Cat { path },
            } => {
                assert_eq!(path, PathBuf::from("main.rs"));
            }
            _ => panic!("expected Fs Cat command"),
        }
    }

    #[test]
    fn test_cli_parses_code_read() {
        let cli = Cli::parse_from([
            "that",
            "code",
            "read",
            "src/main.rs",
            "--symbols",
            "--context",
            "5",
        ]);
        match cli.command {
            Commands::Code {
                command:
                    CodeCommands::Read {
                        path,
                        symbols,
                        context,
                        line,
                        end_line,
                    },
            } => {
                assert_eq!(path, PathBuf::from("src/main.rs"));
                assert!(symbols);
                assert_eq!(context, Some(5));
                assert_eq!(line, None);
                assert_eq!(end_line, None);
            }
            _ => panic!("expected Code Read command"),
        }
    }

    #[test]
    fn test_cli_parses_code_grep() {
        let cli = Cli::parse_from(["that", "code", "grep", "TODO", ".", "-i", "--limit", "10"]);
        match cli.command {
            Commands::Code {
                command:
                    CodeCommands::Grep {
                        pattern,
                        ignore_case,
                        limit,
                        ..
                    },
            } => {
                assert_eq!(pattern, "TODO");
                assert!(ignore_case);
                assert_eq!(limit, Some(10));
            }
            _ => panic!("expected Code Grep command"),
        }
    }

    #[test]
    fn test_cli_parses_code_tree() {
        let cli = Cli::parse_from(["that", "code", "tree", ".", "--depth", "3", "--compact"]);
        match cli.command {
            Commands::Code {
                command:
                    CodeCommands::Tree {
                        depth,
                        compact,
                        ranked,
                        ..
                    },
            } => {
                assert_eq!(depth, Some(3));
                assert!(compact);
                assert!(!ranked);
            }
            _ => panic!("expected Code Tree command"),
        }
    }

    #[test]
    fn test_cli_global_flags() {
        let cli = Cli::parse_from([
            "that",
            "--max-tokens",
            "256",
            "--format",
            "compact",
            "fs",
            "ls",
        ]);
        assert_eq!(cli.max_tokens, Some(256));
        assert!(matches!(cli.format, Some(OutputFormatArg::Compact)));
    }

    #[test]
    fn test_cli_config_schema() {
        let cli = Cli::parse_from(["that", "config-schema"]);
        assert!(matches!(cli.command, Commands::ConfigSchema));
    }

    #[test]
    fn test_cli_verify() {
        // Verify the CLI definition is valid
        Cli::command().debug_assert();
    }

    #[test]
    fn test_cli_parses_code_index() {
        let cli = Cli::parse_from(["that", "code", "index", ".", "--status"]);
        match cli.command {
            Commands::Code {
                command: CodeCommands::Index { path, status },
            } => {
                assert_eq!(path, PathBuf::from("."));
                assert!(status);
            }
            _ => panic!("expected Code Index command"),
        }
    }

    #[test]
    fn test_cli_parses_code_edit() {
        let cli = Cli::parse_from([
            "that",
            "code",
            "edit",
            "file.rs",
            "--search",
            "old",
            "--replace",
            "new",
            "--dry-run",
        ]);
        match cli.command {
            Commands::Code {
                command:
                    CodeCommands::Edit {
                        path,
                        search,
                        replace,
                        dry_run,
                        ..
                    },
            } => {
                assert_eq!(path, PathBuf::from("file.rs"));
                assert_eq!(search.unwrap(), "old");
                assert_eq!(replace.unwrap(), "new");
                assert!(dry_run);
            }
            _ => panic!("expected Code Edit command"),
        }
    }

    #[test]
    fn test_cli_parses_code_ast_grep() {
        let cli = Cli::parse_from([
            "that",
            "code",
            "ast-grep",
            "(function_item)",
            "src/",
            "--language",
            "rust",
        ]);
        match cli.command {
            Commands::Code {
                command:
                    CodeCommands::AstGrep {
                        pattern,
                        path,
                        language,
                    },
            } => {
                assert_eq!(pattern, "(function_item)");
                assert_eq!(path, PathBuf::from("src/"));
                assert_eq!(language.unwrap(), "rust");
            }
            _ => panic!("expected Code AstGrep command"),
        }
    }

    #[test]
    fn test_cli_parses_code_symbols_references() {
        let cli = Cli::parse_from(["that", "code", "symbols", ".", "--references"]);
        match cli.command {
            Commands::Code {
                command: CodeCommands::Symbols { references, .. },
            } => {
                assert!(references);
            }
            _ => panic!("expected Code Symbols command"),
        }
    }

    #[test]
    fn test_cli_parses_exec_signal_graceful() {
        let cli = Cli::parse_from(["that", "exec", "run", "echo hi", "--signal", "graceful"]);
        match cli.command {
            Commands::Exec {
                command: ExecCommands::Run { signal, stream, .. },
            } => {
                assert!(matches!(signal, SignalModeArg::Graceful));
                assert!(!stream);
            }
            _ => panic!("expected Exec Run command"),
        }
    }

    #[test]
    fn test_cli_parses_exec_signal_immediate() {
        let cli = Cli::parse_from(["that", "exec", "run", "echo hi", "--signal", "immediate"]);
        match cli.command {
            Commands::Exec {
                command: ExecCommands::Run { signal, .. },
            } => {
                assert!(matches!(signal, SignalModeArg::Immediate));
            }
            _ => panic!("expected Exec Run command"),
        }
    }

    #[test]
    fn test_cli_parses_exec_stream() {
        let cli = Cli::parse_from(["that", "exec", "run", "echo hi", "--stream"]);
        match cli.command {
            Commands::Exec {
                command: ExecCommands::Run { stream, .. },
            } => {
                assert!(stream);
            }
            _ => panic!("expected Exec Run command"),
        }
    }

    #[test]
    fn test_cli_parses_init_default() {
        let cli = Cli::parse_from(["that", "init"]);
        match cli.command {
            Commands::Init { profile, force } => {
                assert!(matches!(profile, InitProfile::Safe));
                assert!(!force);
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_cli_parses_init_agent_force() {
        let cli = Cli::parse_from(["that", "init", "--profile", "agent", "--force"]);
        match cli.command {
            Commands::Init { profile, force } => {
                assert!(matches!(profile, InitProfile::Agent));
                assert!(force);
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn test_cli_parses_mem_add_with_session_id() {
        let cli = Cli::parse_from([
            "that",
            "mem",
            "add",
            "some content",
            "--session-id",
            "sess-abc",
        ]);
        match cli.command {
            Commands::Mem {
                command:
                    MemCommands::Add {
                        content,
                        session_id,
                        ..
                    },
            } => {
                assert_eq!(content, "some content");
                assert_eq!(session_id.as_deref(), Some("sess-abc"));
            }
            _ => panic!("expected Mem Add command"),
        }
    }

    #[test]
    fn test_cli_parses_mem_recall_with_session_id() {
        let cli = Cli::parse_from([
            "that",
            "mem",
            "recall",
            "query text",
            "--session-id",
            "sess-abc",
        ]);
        match cli.command {
            Commands::Mem {
                command:
                    MemCommands::Recall {
                        query, session_id, ..
                    },
            } => {
                assert_eq!(query, "query text");
                assert_eq!(session_id.as_deref(), Some("sess-abc"));
            }
            _ => panic!("expected Mem Recall command"),
        }
    }

    #[test]
    fn test_cli_parses_mem_compact() {
        let cli = Cli::parse_from([
            "that",
            "mem",
            "compact",
            "--summary",
            "Key decisions: used Postgres, JWT auth",
            "--session-id",
            "sess-1",
        ]);
        match cli.command {
            Commands::Mem {
                command:
                    MemCommands::Compact {
                        summary,
                        session_id,
                    },
            } => {
                assert!(summary.contains("Postgres"));
                assert_eq!(session_id.as_deref(), Some("sess-1"));
            }
            _ => panic!("expected Mem Compact command"),
        }
    }

    #[test]
    fn test_cli_parses_session_init() {
        let cli = Cli::parse_from(["that", "session", "init", "--session-id", "my-session"]);
        match cli.command {
            Commands::Session {
                command: SessionCommands::Init { session_id },
            } => {
                assert_eq!(session_id.as_deref(), Some("my-session"));
            }
            _ => panic!("expected Session Init command"),
        }
    }

    #[test]
    fn test_cli_parses_session_add_tokens() {
        let cli = Cli::parse_from([
            "that",
            "session",
            "add-tokens",
            "--session-id",
            "sess-x",
            "--tokens",
            "1500",
        ]);
        match cli.command {
            Commands::Session {
                command: SessionCommands::AddTokens { session_id, tokens },
            } => {
                assert_eq!(session_id, "sess-x");
                assert_eq!(tokens, 1500);
            }
            _ => panic!("expected Session AddTokens command"),
        }
    }

    #[test]
    fn test_cli_parses_session_stats() {
        let cli = Cli::parse_from(["that", "session", "stats", "--session-id", "sess-x"]);
        match cli.command {
            Commands::Session {
                command: SessionCommands::Stats { session_id },
            } => {
                assert_eq!(session_id, "sess-x");
            }
            _ => panic!("expected Session Stats command"),
        }
    }
}
