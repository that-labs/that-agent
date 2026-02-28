use crate::ToolContext;

pub fn handle_exec(
    ctx: &ToolContext,
    command: &that_tools::cli::ExecCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use that_tools::cli::ExecCommands;
    ctx.check_policy("shell_exec").map_err(|e| e.to_string())?;
    match command {
        ExecCommands::Run {
            command,
            cwd,
            timeout,
            signal,
            stream,
        } => {
            let signal_mode = match signal {
                that_tools::cli::SignalModeArg::Graceful => {
                    that_tools::tools::exec::SignalMode::Graceful
                }
                that_tools::cli::SignalModeArg::Immediate => {
                    that_tools::tools::exec::SignalMode::Immediate
                }
            };
            let result = that_tools::tools::exec::exec_with_options(
                command,
                cwd.as_deref().map(|p| p.to_str().unwrap_or(".")),
                *timeout,
                ctx.max_tokens,
                signal_mode,
                *stream,
            )?;
            println!("{}", ctx.format_output(&result));
            Ok(())
        }
    }
}
