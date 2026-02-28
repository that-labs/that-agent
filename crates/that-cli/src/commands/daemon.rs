use crate::ToolContext;
use that_tools::output;

pub fn handle_daemon(
    ctx: &ToolContext,
    command: &that_tools::cli::DaemonCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::DaemonCommands;
    match command {
        DaemonCommands::Start => {
            let result = that_tools::daemon::start()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Stop => {
            let result = that_tools::daemon::stop()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Status => {
            let result = that_tools::daemon::status();
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
