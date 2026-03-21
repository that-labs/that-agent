use serde::Serialize;

/// The result of applying a token budget to output.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetedOutput {
    /// The final output content — always valid in the requested format.
    pub content: String,
    /// Actual token count of the returned content.
    pub tokens: usize,
    /// Whether the output was truncated or compacted.
    pub truncated: bool,
    /// Original token count before compaction (0 if not applicable).
    pub original_tokens: usize,
}

/// An envelope that wraps tool output with budget metadata for agents.
///
/// Only emitted in JSON mode. Human-facing formats (markdown, raw, compact) are unaffected.
/// `flush_recommended` is set when the output was heavily truncated
/// (`original_tokens > tokens * 2`), signalling the agent to consider compaction.
#[derive(Debug, Clone, Serialize)]
pub struct OutputEnvelope {
    /// The actual tool result data.
    pub data: serde_json::Value,
    /// Actual token count of the returned content.
    pub tokens: usize,
    /// Whether the output was truncated to fit the budget.
    pub truncated: bool,
    /// Original token count before compaction (0 when not truncated).
    pub original_tokens: usize,
    /// True when truncation was severe (original > 2x returned), suggesting compaction.
    pub flush_recommended: bool,
}

impl OutputEnvelope {
    pub fn from_budgeted(budgeted: &BudgetedOutput) -> Self {
        let data = serde_json::from_str(&budgeted.content)
            .unwrap_or(serde_json::Value::String(budgeted.content.clone()));
        let flush_recommended =
            budgeted.truncated && budgeted.original_tokens > budgeted.tokens * 2;
        Self {
            data,
            tokens: budgeted.tokens,
            truncated: budgeted.truncated,
            original_tokens: budgeted.original_tokens,
            flush_recommended,
        }
    }
}
