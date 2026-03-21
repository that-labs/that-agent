//! Run report — collects all per-step results, assertion outcomes, judge score,
//! and aggregated token usage, then serialises to JSON or Markdown.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eval::judge::JudgeScore;

// ── Result types ─────────────────────────────────────────────────────────────

/// Outcome of a single prompt step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepResult {
    /// Zero-based step index.
    pub index: usize,
    /// Human-readable step kind (e.g. "prompt", "create_skill").
    pub kind: String,
    /// Session label (if applicable).
    pub session: Option<String>,
    /// Whether the step completed without error.
    pub success: bool,
    /// Error message if the step failed.
    pub error: Option<String>,
    /// Assistant response text (prompt steps only).
    pub response: Option<String>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Outcome of a single assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    /// Assertion kind (e.g. "file_exists").
    pub kind: String,
    /// Human-readable description.
    pub description: String,
    /// Whether the assertion passed.
    pub passed: bool,
    /// Failure reason if not passed.
    pub reason: Option<String>,
}

/// Aggregated token usage across all prompt steps in the run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregatedUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub prompt_steps: usize,
}

// ── RunReport ────────────────────────────────────────────────────────────────

/// Complete report for one eval run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    /// Unique run identifier.
    pub run_id: String,
    /// Root OpenTelemetry trace ID for this eval run (if tracing export was enabled).
    pub trace_id: Option<String>,
    /// Root OpenTelemetry span ID for this eval run (if tracing export was enabled).
    pub root_span_id: Option<String>,
    /// Scenario name.
    pub scenario_name: String,
    /// Scenario description.
    pub scenario_description: String,
    /// Agent name used.
    pub agent_name: String,
    /// Provider used.
    pub provider: String,
    /// Model used.
    pub model: String,
    /// ISO-8601 timestamp when the run started.
    pub started_at: DateTime<Utc>,
    /// ISO-8601 timestamp when the run finished.
    pub finished_at: DateTime<Utc>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Per-step outcomes.
    pub steps: Vec<StepResult>,
    /// Assertion outcomes (from assert steps).
    pub assertions: Vec<AssertionResult>,
    /// LLM judge score (None if judge was skipped or failed).
    pub judge_score: Option<JudgeScore>,
    /// Aggregated token usage.
    pub usage: AggregatedUsage,
}

impl RunReport {
    /// Serialize to pretty-printed JSON.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }

    /// Render as Markdown.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();

        md.push_str(&format!("# Eval Report: {}\n\n", self.scenario_name));
        md.push_str(&format!("**Run ID:** `{}`\n\n", self.run_id));
        if let Some(trace_id) = &self.trace_id {
            md.push_str(&format!("**Trace ID:** `{}`\n\n", trace_id));
        }
        if let Some(span_id) = &self.root_span_id {
            md.push_str(&format!("**Root Span ID:** `{}`\n\n", span_id));
        }
        md.push_str(&format!(
            "**Description:** {}\n\n",
            self.scenario_description
        ));
        md.push_str(&format!(
            "**Agent:** {} | **Provider:** {} | **Model:** {}\n\n",
            self.agent_name, self.provider, self.model
        ));
        md.push_str(&format!(
            "**Started:** {} | **Duration:** {}ms\n\n",
            self.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
            self.duration_ms
        ));

        // Judge score
        if let Some(score) = &self.judge_score {
            md.push_str("## Judge Score\n\n");
            md.push_str(&format!("**Total: {}/100**\n\n", score.total));

            if !score.breakdown.is_empty() {
                md.push_str("| Criterion | Score |\n|-----------|-------|\n");
                for (k, v) in &score.breakdown {
                    md.push_str(&format!("| {} | {} |\n", k, v));
                }
                md.push('\n');
            }

            if let Some(rationale) = &score.rationale {
                md.push_str(&format!("**Rationale:** {}\n\n", rationale));
            }
        } else {
            md.push_str("## Judge Score\n\n*Judge was skipped or unavailable.*\n\n");
        }

        // Assertions
        if !self.assertions.is_empty() {
            md.push_str("## Assertions\n\n");
            md.push_str(
                "| # | Kind | Description | Result |\n|---|------|-------------|--------|\n",
            );
            for (i, a) in self.assertions.iter().enumerate() {
                let result = if a.passed {
                    "✓ pass".to_string()
                } else {
                    format!("✗ fail — {}", a.reason.as_deref().unwrap_or("unknown"))
                };
                md.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    i + 1,
                    a.kind,
                    a.description,
                    result
                ));
            }
            md.push('\n');
        }

        // Steps
        md.push_str("## Steps\n\n");
        for step in &self.steps {
            let status = if step.success { "✓" } else { "✗" };
            let session = step
                .session
                .as_deref()
                .map(|s| format!(" [session={s}]"))
                .unwrap_or_default();
            md.push_str(&format!(
                "### {} Step {} — {}{}  ({}ms)\n\n",
                status,
                step.index + 1,
                step.kind,
                session,
                step.duration_ms
            ));
            if let Some(err) = &step.error {
                md.push_str(&format!("**Error:** {err}\n\n"));
            }
            if let Some(resp) = &step.response {
                let truncated = if resp.chars().count() > 800 {
                    let end = resp
                        .char_indices()
                        .nth(800)
                        .map(|(i, _)| i)
                        .unwrap_or(resp.len());
                    format!("{}…", &resp[..end])
                } else {
                    resp.clone()
                };
                md.push_str(&format!("**Response:**\n\n```\n{truncated}\n```\n\n"));
            }
        }

        // Usage
        md.push_str("## Token Usage\n\n");
        md.push_str(&format!(
            "- Input tokens: {}\n- Output tokens: {}\n- Prompt steps: {}\n",
            self.usage.input_tokens, self.usage.output_tokens, self.usage.prompt_steps
        ));

        md
    }
}
