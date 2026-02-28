use crate::ToolContext;
use that_tools::output;

pub fn handle_mem(
    ctx: &ToolContext,
    command: &that_tools::cli::MemCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::MemCommands;
    ctx.check_policy("memory").map_err(|e| e.to_string())?;
    match command {
        MemCommands::Add {
            content,
            tags,
            source,
            session_id,
        } => {
            let result = that_tools::tools::memory::add(
                content,
                tags,
                source.as_deref(),
                session_id.as_deref(),
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
            let result = that_tools::tools::memory::recall(
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
            let result = that_tools::tools::memory::search(
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
            let mut result = that_tools::tools::memory::compact(
                summary,
                session_id.as_deref(),
                &ctx.config.memory,
            )?;
            if let Some(ref sid) = session_id {
                let sessions_path = if ctx.config.session.sessions_path.is_empty() {
                    that_tools::tools::session::default_sessions_path()
                } else {
                    std::path::PathBuf::from(&ctx.config.session.sessions_path)
                };
                if that_tools::tools::session::reset_context(sid, 0, &sessions_path).is_ok() {
                    result.context_tokens_reset = true;
                }
                if that_tools::tools::session::increment_compaction(sid, &sessions_path).is_ok() {
                    if let Ok(stats) = that_tools::tools::session::get_stats(
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
            let result = that_tools::tools::memory::unpin(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Remove { id } => {
            let result = that_tools::tools::memory::remove(id, &ctx.config.memory)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Prune {
            before_days,
            min_access,
        } => {
            let deleted =
                that_tools::tools::memory::prune(*before_days, *min_access, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"pruned": deleted}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Stats => {
            let stats = that_tools::tools::memory::stats(&ctx.config.memory)?;
            let budgeted = output::emit_json(&stats, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Export => {
            let memories = that_tools::tools::memory::export_memories(&ctx.config.memory)?;
            let budgeted = output::emit_json(&memories, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        MemCommands::Import => {
            let stdin = std::io::read_to_string(std::io::stdin())?;
            let memories: Vec<that_tools::tools::memory::MemoryEntry> =
                serde_json::from_str(&stdin)?;
            let imported =
                that_tools::tools::memory::import_memories(&memories, &ctx.config.memory)?;
            let budgeted =
                output::emit_json(&serde_json::json!({"imported": imported}), ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
