use crate::commands::ToolContext;
use crate::tools::output;

pub fn handle_daemon(
    ctx: &ToolContext,
    command: &crate::tools::cli::DaemonCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::tools::cli::DaemonCommands;
    match command {
        DaemonCommands::Start => {
            let result = crate::tools::daemon::start()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Stop => {
            let result = crate::tools::daemon::stop()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        DaemonCommands::Status => {
            let result = crate::tools::daemon::status();
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
