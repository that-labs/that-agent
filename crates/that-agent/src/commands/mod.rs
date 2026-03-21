pub mod agent;
pub mod code;
pub mod daemon;
pub mod eval;
pub mod exec;
pub mod fs;
pub mod human;
pub mod memory;
pub mod search;
pub mod secrets;
pub mod session;
pub mod skill;
pub mod tools;

pub use agent::handle_agent_orchestration_command;
pub use eval::handle_eval_command;
pub use secrets::handle_secrets_command;
pub use session::handle_session_command;
pub use skill::handle_skill_command;
pub use tools::handle_tools_command;

use crate::cli::OutputFormatArg;
use crate::tools::config::PolicyLevel;
use crate::tools::output;

/// Execution context for tool invocations.
/// Carries resolved config, format, and budget settings.
pub struct ToolContext {
    pub(crate) config: crate::tools::ThatToolsConfig,
    pub(crate) max_tokens: Option<usize>,
    pub(crate) format: OutputFormatArg,
}

impl ToolContext {
    /// Check if a tool action is allowed by policy.
    pub(crate) fn check_policy(&self, tool_name: &str) -> Result<(), String> {
        let level = self.policy_for(tool_name);
        match level {
            PolicyLevel::Allow => Ok(()),
            PolicyLevel::Prompt => {
                if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    match crate::tools::impls::human::prompt::confirm_terminal(&format!(
                        "Tool '{}' requires approval. Allow?",
                        tool_name
                    )) {
                        Ok(true) => Ok(()),
                        Ok(false) => {
                            Err(format!("policy denied: user denied tool '{}'", tool_name))
                        }
                        Err(e) => Err(format!(
                            "policy denied: could not prompt for tool '{}': {}",
                            tool_name, e
                        )),
                    }
                } else {
                    Err(format!(
                        "policy denied: tool '{}' requires approval but no interactive terminal is available",
                        tool_name
                    ))
                }
            }
            PolicyLevel::Deny => Err(format!(
                "policy denied: tool '{}' is not allowed by current policy",
                tool_name
            )),
        }
    }

    fn policy_for(&self, tool_name: &str) -> &PolicyLevel {
        match tool_name {
            "code_read" => &self.config.policy.tools.code_read,
            "code_edit" => &self.config.policy.tools.code_edit,
            "fs_read" => &self.config.policy.tools.fs_read,
            "fs_write" => &self.config.policy.tools.fs_write,
            "fs_delete" => &self.config.policy.tools.fs_delete,
            "shell_exec" => &self.config.policy.tools.shell_exec,
            "search" => &self.config.policy.tools.search,
            "memory" => &self.config.policy.tools.memory,
            "git_commit" => &self.config.policy.tools.git_commit,
            "git_push" => &self.config.policy.tools.git_push,
            "mem_compact" => &self.config.policy.tools.mem_compact,
            _ => &self.config.policy.default,
        }
    }

    /// Format output according to --format flag.
    pub(crate) fn format_output(&self, budgeted: &output::BudgetedOutput) -> String {
        match self.format {
            OutputFormatArg::Json => {
                let envelope = output::OutputEnvelope::from_budgeted(budgeted);
                serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| budgeted.content.clone())
            }
            OutputFormatArg::Compact => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => {
                        let compacted = output::compact_json_value(&v);
                        serde_json::to_string(&compacted)
                            .unwrap_or_else(|_| budgeted.content.clone())
                    }
                    Err(_) => budgeted.content.clone(),
                }
            }
            OutputFormatArg::Raw => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => output::render_raw(&v),
                    Err(_) => budgeted.content.clone(),
                }
            }
            OutputFormatArg::Markdown => {
                match serde_json::from_str::<serde_json::Value>(&budgeted.content) {
                    Ok(v) => output::render_markdown(&v),
                    Err(_) => budgeted.content.clone(),
                }
            }
        }
    }
}
