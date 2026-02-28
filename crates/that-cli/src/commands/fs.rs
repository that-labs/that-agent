use crate::ToolContext;

pub fn handle_fs(
    ctx: &ToolContext,
    command: &that_tools::cli::FsCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::FsCommands;
    match command {
        FsCommands::Ls { path, max_depth } => {
            ctx.check_policy("fs_read").map_err(|e| e.to_string())?;
            let depth = max_depth.or(Some(ctx.config.output.fs_ls_max_depth));
            let result = that_tools::tools::fs::ls(path, depth, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Cat { path } => {
            ctx.check_policy("fs_read").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::cat(path, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Write {
            path,
            content,
            dry_run,
            backup,
        } => {
            ctx.check_policy("fs_write").map_err(|e| e.to_string())?;
            let content = match content {
                Some(s) => s.replace("\\n", "\n").replace("\\t", "\t"),
                None => std::io::read_to_string(std::io::stdin())?,
            };
            let result =
                that_tools::tools::fs::write(path, &content, *dry_run, *backup, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Mkdir { path, parents } => {
            ctx.check_policy("fs_write").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::mkdir(path, *parents, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        FsCommands::Rm {
            path,
            recursive,
            dry_run,
        } => {
            ctx.check_policy("fs_delete").map_err(|e| e.to_string())?;
            let result = that_tools::tools::fs::rm(path, *recursive, *dry_run, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}
