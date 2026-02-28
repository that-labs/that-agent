use crate::ToolContext;

pub fn handle_search(
    ctx: &ToolContext,
    command: &that_tools::cli::SearchCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::SearchCommands;
    ctx.check_policy("search").map_err(|e| e.to_string())?;
    match command {
        SearchCommands::Query {
            query,
            engine,
            limit,
            no_cache,
        } => {
            let result = that_tools::tools::search::search(
                query,
                engine.as_deref(),
                *limit,
                *no_cache,
                &ctx.config.search,
                ctx.max_tokens,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
        SearchCommands::Fetch { urls, mode } => {
            let result = that_tools::tools::search::fetch(urls, mode, ctx.max_tokens)?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}
