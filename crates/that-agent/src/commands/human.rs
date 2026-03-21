use crate::commands::ToolContext;
use crate::tools::output;

pub fn handle_human(
    ctx: &ToolContext,
    command: &crate::tools::cli::HumanCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::tools::cli::HumanCommands;
    match command {
        HumanCommands::Ask { message, timeout } => {
            let result = crate::tools::impls::human::ask(message, *timeout)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Approve { id, response } => {
            let result = crate::tools::impls::human::approve(id, response.as_deref())?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Confirm { id } => {
            let result = crate::tools::impls::human::confirm(id)?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
        HumanCommands::Pending => {
            let result = crate::tools::impls::human::pending()?;
            let budgeted = output::emit_json(&result, ctx.max_tokens);
            println!("{}", ctx.format_output(&budgeted));
            Ok(())
        }
    }
}
