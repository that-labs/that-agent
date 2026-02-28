use crate::ToolContext;
use that_tools::output;

pub fn handle_human(
    ctx: &ToolContext,
    command: &that_tools::cli::HumanCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::HumanCommands;
    match command {
        HumanCommands::Ask { message, timeout } => {
            let result = that_tools::tools::human::ask(message, *timeout)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Approve { id, response } => {
            let result = that_tools::tools::human::approve(id, response.as_deref())?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Confirm { id } => {
            let result = that_tools::tools::human::confirm(id)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Pending => {
            let result = that_tools::tools::human::pending()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
