//! that-tools — The Agent Tool Layer (library crate)
//!
//! A Rust library that gives any LLM-powered agent structural code
//! comprehension, policy governance, and hard token-budget enforcement
//! on every output.
//!
//! This crate re-exports all modules from that-tools as a library,
//! plus a `parse_cli_to_request` function for programmatic invocation.

pub mod cli;
pub mod config;
pub mod daemon;
#[cfg(feature = "code-analysis")]
pub mod index;
pub mod output;
pub mod tools;

pub use config::ThatToolsConfig;
pub use tools::dispatch::{execute_tool, ToolRequest, ToolResponse};

/// Parse CLI arg strings into a [`ToolRequest`] for programmatic invocation.
///
/// The args should **not** include the binary name — just the subcommand and
/// its arguments.
///
/// # Example
///
/// ```ignore
/// let req = parse_cli_to_request(&["code", "read", "src/main.rs", "--symbols"])?;
/// ```
pub fn parse_cli_to_request(args: &[&str]) -> Result<ToolRequest, String> {
    use clap::Parser;
    // Prepend a dummy binary name for clap
    let mut full_args = vec!["that"];
    full_args.extend_from_slice(args);
    let parsed = cli::Cli::try_parse_from(full_args).map_err(|e| e.to_string())?;
    cli_to_request(parsed.command)
}

fn cli_to_request(cmd: cli::Commands) -> Result<ToolRequest, String> {
    match cmd {
        cli::Commands::Fs { command } => fs_to_request(command),
        cli::Commands::Code { command } => code_to_request(command),
        cli::Commands::Mem { command } => mem_to_request(command),
        cli::Commands::Search { command } => search_to_request(command),
        cli::Commands::Human { command } => human_to_request(command),
        cli::Commands::Exec { command } => exec_to_request(command),
        _ => Err("Command not supported via dispatch".to_string()),
    }
}

fn fs_to_request(cmd: cli::FsCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::FsCommands::Ls { path, max_depth } => Ok(ToolRequest::FsLs {
            path: path.to_string_lossy().to_string(),
            max_depth,
        }),
        cli::FsCommands::Cat { path } => Ok(ToolRequest::FsCat {
            path: path.to_string_lossy().to_string(),
        }),
        cli::FsCommands::Write {
            path,
            content,
            dry_run,
            backup,
        } => {
            let resolved_content = match content {
                Some(s) => s.replace("\\n", "\n").replace("\\t", "\t"),
                None => return Err("FsWrite via dispatch requires --content flag".into()),
            };
            Ok(ToolRequest::FsWrite {
                path: path.to_string_lossy().to_string(),
                content: resolved_content,
                dry_run,
                backup,
            })
        }
        cli::FsCommands::Mkdir { path, parents } => Ok(ToolRequest::FsMkdir {
            path: path.to_string_lossy().to_string(),
            parents,
        }),
        cli::FsCommands::Rm {
            path,
            recursive,
            dry_run,
        } => Ok(ToolRequest::FsRm {
            path: path.to_string_lossy().to_string(),
            recursive,
            dry_run,
        }),
    }
}

fn code_to_request(cmd: cli::CodeCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::CodeCommands::Read {
            path,
            context,
            symbols,
            line,
            end_line,
        } => Ok(ToolRequest::CodeRead {
            path: path.to_string_lossy().to_string(),
            context,
            symbols,
            line,
            end_line,
        }),
        cli::CodeCommands::Grep {
            pattern,
            path,
            context,
            limit,
            ignore_case,
            regex,
            include,
            exclude,
        } => Ok(ToolRequest::CodeGrep {
            pattern,
            path: path.to_string_lossy().to_string(),
            context,
            limit,
            ignore_case,
            regex,
            include,
            exclude,
        }),
        cli::CodeCommands::Tree {
            path,
            depth,
            compact,
            ranked,
        } => Ok(ToolRequest::CodeTree {
            path: path.to_string_lossy().to_string(),
            depth,
            compact,
            ranked,
        }),
        cli::CodeCommands::Symbols {
            path,
            kind,
            name,
            references: _,
        } => Ok(ToolRequest::CodeSymbols {
            path: path.to_string_lossy().to_string(),
            kind,
            name,
        }),
        cli::CodeCommands::Summary { path } => Ok(ToolRequest::CodeSummary {
            path: path.to_string_lossy().to_string(),
        }),
        cli::CodeCommands::Edit { .. } => Err("CodeEdit is not supported via dispatch".to_string()),
        cli::CodeCommands::Index { .. } => {
            Err("CodeIndex is not supported via dispatch".to_string())
        }
        cli::CodeCommands::AstGrep { .. } => {
            Err("AstGrep is not supported via dispatch".to_string())
        }
    }
}

fn mem_to_request(cmd: cli::MemCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::MemCommands::Add {
            content,
            tags,
            source,
            session_id,
        } => Ok(ToolRequest::MemAdd {
            content,
            tags,
            source,
            session_id,
        }),
        cli::MemCommands::Recall {
            query,
            limit,
            session_id,
        } => Ok(ToolRequest::MemSearch {
            query,
            tags: None,
            limit: Some(limit),
            session_id,
        }),
        cli::MemCommands::Search {
            query,
            tags,
            limit,
            session_id,
        } => {
            let tag_filter = if tags.is_empty() { None } else { Some(tags) };
            Ok(ToolRequest::MemSearch {
                query,
                tags: tag_filter,
                limit: Some(limit),
                session_id,
            })
        }
        cli::MemCommands::Compact {
            summary,
            session_id,
        } => Ok(ToolRequest::MemCompact {
            summary,
            session_id,
        }),
        cli::MemCommands::Unpin { id } => Ok(ToolRequest::MemUnpin { id }),
        cli::MemCommands::Remove { .. } => {
            Err("MemRemove is not supported via dispatch".to_string())
        }
        cli::MemCommands::Prune { .. } => Err("MemPrune is not supported via dispatch".to_string()),
        cli::MemCommands::Stats => Err("MemStats is not supported via dispatch".to_string()),
        cli::MemCommands::Export => Err("MemExport is not supported via dispatch".to_string()),
        cli::MemCommands::Import => Err("MemImport is not supported via dispatch".to_string()),
    }
}

fn search_to_request(cmd: cli::SearchCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::SearchCommands::Query {
            query,
            engine,
            limit,
            no_cache,
        } => Ok(ToolRequest::SearchQuery {
            query,
            engine,
            limit,
            no_cache,
        }),
        cli::SearchCommands::Fetch { urls, mode } => Ok(ToolRequest::SearchFetch { urls, mode }),
    }
}

fn human_to_request(cmd: cli::HumanCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::HumanCommands::Ask { message, timeout } => {
            Ok(ToolRequest::HumanAsk { message, timeout })
        }
        cli::HumanCommands::Approve { .. } => {
            Err("HumanApprove is not supported via dispatch".to_string())
        }
        cli::HumanCommands::Confirm { .. } => {
            Err("HumanConfirm is not supported via dispatch".to_string())
        }
        cli::HumanCommands::Pending => {
            Err("HumanPending is not supported via dispatch".to_string())
        }
    }
}

fn exec_to_request(cmd: cli::ExecCommands) -> Result<ToolRequest, String> {
    match cmd {
        cli::ExecCommands::Run {
            command,
            cwd,
            timeout,
            signal,
            stream: _,
        } => {
            let signal_mode = match signal {
                cli::SignalModeArg::Graceful => tools::exec::SignalMode::Graceful,
                cli::SignalModeArg::Immediate => tools::exec::SignalMode::Immediate,
            };
            Ok(ToolRequest::ShellExec {
                command,
                cwd: cwd.map(|p| p.to_string_lossy().to_string()),
                timeout_secs: timeout,
                signal: Some(signal_mode),
            })
        }
    }
}
