use crate::commands::ToolContext;
use crate::tools::output;

pub fn handle_mem(
    ctx: &ToolContext,
    command: &crate::tools::cli::MemCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::tools::cli::MemCommands;
    ctx.check_policy("memory").map_err(|e| e.to_string())?;
    // Lazily initialize the memory DB only when a mem command is actually invoked,
    // so bare tool commands (code, fs, etc.) don't create phantom agent directories.
    crate::tools::impls::memory::ensure_initialized(&ctx.config.memory)?;
    match command {
        MemCommands::Add {
            content,
            tags,
            source,
            session_id,
            pin,
        } => {
            let result = crate::tools::impls::memory::add(
                content,
                tags,
                source.as_deref(),
                session_id.as_deref(),
                *pin,
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Recall {
            query,
            limit,
            session_id,
        } => {
            let result = crate::tools::impls::memory::recall(
                query,
                *limit,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Search {
            query,
            tags,
            limit,
            session_id,
        } => {
            let tag_filter = if tags.is_empty() {
                None
            } else {
                Some(tags.as_slice())
            };
            let result = crate::tools::impls::memory::search(
                query,
                tag_filter,
                *limit,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Compact {
            summary,
            session_id,
        } => {
            ctx.check_policy("mem_compact").map_err(|e| e.to_string())?;
            let mut result = crate::tools::impls::memory::compact(
                summary,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            if let Some(ref sid) = session_id {
                let sessions_path = if ctx.config.session.sessions_path.is_empty() {
                    crate::tools::impls::session::default_sessions_path()
                } else {
                    std::path::PathBuf::from(&ctx.config.session.sessions_path)
                };
                if crate::tools::impls::session::reset_context(sid, 0, &sessions_path).is_ok() {
                    result.context_tokens_reset = true;
                }
                if crate::tools::impls::session::increment_compaction(sid, &sessions_path).is_ok() {
                    if let Ok(stats) = crate::tools::impls::session::get_stats(
                        sid,
                        &sessions_path,
                        ctx.config.session.soft_threshold_tokens,
                    ) {
                        result.compaction_count = Some(stats.compaction_count);
                    }
                }
            }
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Unpin { id } => {
            let result = crate::tools::impls::memory::unpin(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Remove { id } => {
            let result = crate::tools::impls::memory::remove(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Prune {
            before_days,
            min_access,
        } => {
            let deleted =
                crate::tools::impls::memory::prune(*before_days, *min_access, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"pruned": deleted}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Stats => {
            let stats = crate::tools::impls::memory::stats(&ctx.config.memory)?;
            let budgeted = output::emit_json(&stats, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Export => {
            let memories = crate::tools::impls::memory::export_memories(&ctx.config.memory)?;
            let budgeted = output::emit_json(&memories, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Import => {
            let stdin = std::io::read_to_string(std::io::stdin())?;
            let memories: Vec<crate::tools::impls::memory::MemoryEntry> =
                serde_json::from_str(&stdin)?;
            let imported =
                crate::tools::impls::memory::import_memories(&memories, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"imported": imported}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
