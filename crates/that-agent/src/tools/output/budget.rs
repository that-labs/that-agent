use serde::Serialize;

use super::compaction::{compact_head_only, compact_head_tail, compact_rule_based};
use super::json_analysis::truncate_json_value;
use super::tokenizer::count_tokens;
use super::types::BudgetedOutput;

/// Strategy for compacting output that exceeds the token budget.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CompactionStrategy {
    /// Keep the first and last portions, elide the middle.
    #[default]
    HeadTail,
    /// Keep only the first N tokens worth of content.
    HeadOnly,
    /// Rule-based extraction of key information.
    RuleBased,
}

/// Applies a token budget to plain text (non-JSON).
///
/// Used for internal content fields (file content, code snippets) before
/// they are placed into a JSON envelope.
pub fn apply_budget_to_text(
    text: &str,
    max_tokens: usize,
    strategy: CompactionStrategy,
) -> BudgetedOutput {
    let original_tokens = count_tokens(text);

    if original_tokens <= max_tokens {
        return BudgetedOutput {
            content: text.to_string(),
            tokens: original_tokens,
            truncated: false,
            original_tokens,
        };
    }

    let compacted = match strategy {
        CompactionStrategy::HeadTail => compact_head_tail(text, max_tokens),
        CompactionStrategy::HeadOnly => compact_head_only(text, max_tokens),
        CompactionStrategy::RuleBased => compact_rule_based(text, max_tokens),
    };

    let tokens = count_tokens(&compacted);
    BudgetedOutput {
        content: compacted,
        tokens,
        truncated: true,
        original_tokens,
    }
}

/// Serializes a value to JSON and enforces a hard token budget on the final output.
///
/// If the serialized JSON exceeds the budget, the value is re-serialized with
/// structural reduction (array truncation, compact formatting) to fit.
///
/// **Invariant**: The returned `content` is ALWAYS valid JSON.
pub fn emit_json<T: Serialize>(value: &T, max_tokens: Option<usize>) -> BudgetedOutput {
    match max_tokens {
        None => {
            let pretty_json =
                serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
            let original_tokens = count_tokens(&pretty_json);
            BudgetedOutput {
                content: pretty_json,
                tokens: original_tokens,
                truncated: false,
                original_tokens,
            }
        }
        Some(budget) => {
            // Compact JSON is the cheapest stable baseline for budget checks.
            let compact_json = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
            let compact_tokens = count_tokens(&compact_json);

            // If compact already exceeds budget, pretty definitely won't fit.
            if compact_tokens > budget {
                let truncated_json = truncate_json_value(value, budget);
                let final_tokens = count_tokens(&truncated_json);
                BudgetedOutput {
                    content: truncated_json,
                    tokens: final_tokens,
                    truncated: true,
                    original_tokens: compact_tokens,
                }
            } else {
                // Compact fits; try pretty and keep it when within budget for readability.
                let pretty_json =
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
                let pretty_tokens = count_tokens(&pretty_json);

                if pretty_tokens <= budget {
                    BudgetedOutput {
                        content: pretty_json,
                        tokens: pretty_tokens,
                        truncated: false,
                        original_tokens: pretty_tokens,
                    }
                } else {
                    BudgetedOutput {
                        content: compact_json,
                        tokens: compact_tokens,
                        truncated: false,
                        original_tokens: pretty_tokens,
                    }
                }
            }
        }
    }
}
