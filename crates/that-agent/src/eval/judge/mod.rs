//! LLM judge — builds an evaluation prompt, calls the LLM, and parses the JSON score.

use crate::agent_loop;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::eval::report::AssertionResult;
use crate::eval::scenario::Scenario;

const DEFAULT_JUDGE_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_JUDGE_PROVIDER: &str = "anthropic";

// ── Score types ───────────────────────────────────────────────────────────────

/// Structured score returned by the LLM judge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeScore {
    /// Overall score 0–100.
    pub total: u32,
    /// Per-criterion scores keyed by criterion name.
    pub breakdown: std::collections::HashMap<String, u32>,
    /// Free-text rationale from the judge.
    pub rationale: Option<String>,
}

// ── Transcript entry for judge context ───────────────────────────────────────

/// A simplified transcript entry passed to the judge.
#[derive(Debug, Clone)]
pub struct JudgeTranscriptEntry {
    pub role: String,
    pub content: String,
}

// ── LlmJudge ─────────────────────────────────────────────────────────────────

/// Calls an LLM to score a completed scenario run.
pub struct LlmJudge {
    provider: String,
    model: String,
}

impl LlmJudge {
    /// Create a judge using the default provider/model.
    pub fn new() -> Self {
        Self {
            provider: DEFAULT_JUDGE_PROVIDER.to_string(),
            model: DEFAULT_JUDGE_MODEL.to_string(),
        }
    }

    /// Create a judge with explicit provider and model.
    pub fn with_model(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// Create a judge with optional provider/model overrides.
    ///
    /// Any missing value falls back to the default judge setting.
    pub fn with_optional(provider: Option<&str>, model: Option<&str>) -> Self {
        let mut judge = Self::new();
        if let Some(p) = provider {
            judge.provider = p.to_string();
        }
        if let Some(m) = model {
            judge.model = m.to_string();
        }
        judge
    }

    /// Score a completed run.
    pub async fn judge(
        &self,
        scenario: &Scenario,
        transcript: &[JudgeTranscriptEntry],
        assertions: &[AssertionResult],
    ) -> Result<JudgeScore> {
        let user_prompt = self.build_user_prompt(scenario, transcript, assertions);
        let system_prompt = self.system_prompt();

        let raw = self.call_llm(&system_prompt, &user_prompt).await?;
        self.parse_score(&raw)
    }

    fn system_prompt(&self) -> String {
        "You are an objective agent behavior evaluator. \
         You will be given a scenario description, a scoring rubric, assertion results, \
         and the full conversation transcript. \
         The transcript contains [USER] turns, [ASSISTANT] turns, and [TOOL] entries. \
         Each [TOOL] entry is either a tool call — formatted as 'CALL <tool_name> <arguments>' — \
         or a tool result — formatted as 'RESULT <content>'. Some [TOOL] entries are control \
         markers such as 'SESSION_RESET session=<id>' indicating a deliberate session boundary. \
         [TOOL] entries appear in execution order between the [USER] prompt that triggered them \
         and the final [ASSISTANT] reply. Use [TOOL] entries as direct evidence when scoring \
         criteria such as 'tool was called', 'memory was retrieved', 'compaction was executed', \
         or 'explicit retrieval step performed'. Absence of a relevant CALL line is strong evidence \
         that the action did not occur. \
         Score strictly from observed evidence in transcript, tool events, and assertions. \
         Do not reward intent; reward only demonstrated behavior. \
         Failed assertions are hard negative evidence and must reduce related criterion scores significantly. \
         Penalize contradictions between assistant claims and observed evidence. \
         Keep criterion scores independent, then set total as a weighted overall judgment consistent with the rubric weights. \
         Evaluate the agent's performance against each rubric criterion and return ONLY valid JSON \
         in the exact schema shown in the user prompt. Do not add any text outside the JSON."
            .to_string()
    }

    fn build_user_prompt(
        &self,
        scenario: &Scenario,
        transcript: &[JudgeTranscriptEntry],
        assertions: &[AssertionResult],
    ) -> String {
        let mut prompt = String::new();

        prompt.push_str(&format!("## Scenario: {}\n\n", scenario.name));
        prompt.push_str(&format!("{}\n\n", scenario.description));

        // Rubric
        prompt.push_str("## Rubric\n\n");
        for c in &scenario.rubric.criteria {
            prompt.push_str(&format!(
                "- **{}** (weight {}): {}\n",
                c.name, c.weight, c.description
            ));
        }
        prompt.push('\n');

        // Assertion results
        if !assertions.is_empty() {
            prompt.push_str("## Assertion Results\n\n");
            for a in assertions {
                let status = if a.passed { "PASS" } else { "FAIL" };
                let reason = a
                    .reason
                    .as_deref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                prompt.push_str(&format!(
                    "- [{status}] {}: {}{}\n",
                    a.kind, a.description, reason
                ));
            }
            prompt.push('\n');
        }

        // Transcript
        prompt.push_str("## Transcript\n\n");
        for entry in transcript {
            prompt.push_str(&format!(
                "[{}]: {}\n\n",
                entry.role.to_uppercase(),
                entry.content
            ));
        }

        // JSON schema instruction
        let criterion_names: Vec<String> = scenario
            .rubric
            .criteria
            .iter()
            .map(|c| format!("\"{}\"", c.name))
            .collect();

        prompt.push_str(&format!(
            "\n## Required Output\n\n\
             Return ONLY valid JSON matching this exact schema:\n\
             ```json\n\
             {{\"total\": <0-100>, \"breakdown\": {{{}}}, \"rationale\": \"<explanation>\"}}\n\
             ```\n\n\
             Where each breakdown key uses a score from 0-100 proportional to the criterion weight.",
            criterion_names
                .iter()
                .map(|n| format!("{}: <0-100>", n))
                .collect::<Vec<_>>()
                .join(", ")
        ));

        prompt
    }

    async fn call_llm(&self, system: &str, user: &str) -> Result<String> {
        let api_key = crate::orchestration::api_key_for_provider(&self.provider)?;
        agent_loop::complete_once(&self.provider, &self.model, &api_key, system, user, 2048)
            .await
            .context("Judge LLM call failed")
    }

    fn parse_score(&self, raw: &str) -> Result<JudgeScore> {
        // Try strict and then tolerant parsing on raw/fence/object-block candidates.
        for candidate in json_candidates(raw) {
            if let Ok(score) = serde_json::from_str::<JudgeScore>(candidate) {
                return Ok(score);
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) {
                if let Some(score) = coerce_judge_score(value) {
                    return Ok(score);
                }
            }
        }

        anyhow::bail!(
            "Could not parse judge score from response. Raw output:\n{}",
            &raw[..raw.len().min(500)]
        )
    }
}

impl Default for LlmJudge {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract JSON content from a ```json ... ``` code fence.
fn extract_json_from_fence(text: &str) -> Option<&str> {
    let start = text.find("```json")?.checked_add(7)?;
    let after_fence = text.get(start..)?;
    // skip optional newline
    let after_newline = after_fence.trim_start_matches('\n');
    let end = after_newline.find("```")?;
    Some(after_newline.get(..end)?.trim())
}

fn json_candidates(raw: &str) -> Vec<&str> {
    let mut out = Vec::with_capacity(3);
    out.push(raw);
    if let Some(fenced) = extract_json_from_fence(raw) {
        out.push(fenced);
    }
    if let Some(start) = raw.find('{') {
        if let Some(end) = raw.rfind('}') {
            out.push(&raw[start..=end]);
        }
    }
    out
}

fn score_from_value(v: &serde_json::Value) -> Option<u32> {
    let n = match v {
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    if !n.is_finite() {
        return None;
    }
    Some(n.round().clamp(0.0, 100.0) as u32)
}

fn coerce_judge_score(value: serde_json::Value) -> Option<JudgeScore> {
    let obj = value.as_object()?;
    let total = score_from_value(obj.get("total")?)?;

    let breakdown_obj = obj.get("breakdown")?.as_object()?;
    let mut breakdown = std::collections::HashMap::with_capacity(breakdown_obj.len());
    for (k, v) in breakdown_obj {
        breakdown.insert(k.clone(), score_from_value(v)?);
    }

    let rationale = obj
        .get("rationale")
        .and_then(|r| r.as_str().map(ToString::to_string));

    Some(JudgeScore {
        total,
        breakdown,
        rationale,
    })
}
