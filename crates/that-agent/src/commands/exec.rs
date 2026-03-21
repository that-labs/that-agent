use crate::commands::ToolContext;

pub fn handle_exec(
    ctx: &ToolContext,
    command: &crate::tools::cli::ExecCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::tools::cli::ExecCommands;
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
                crate::tools::cli::SignalModeArg::Graceful => {
                    crate::tools::impls::exec::SignalMode::Graceful
                }
                crate::tools::cli::SignalModeArg::Immediate => {
                    crate::tools::impls::exec::SignalMode::Immediate
                }
            };
            let result = crate::tools::impls::exec::exec_with_options(
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
