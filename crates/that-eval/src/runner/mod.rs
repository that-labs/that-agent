//! Scenario runner — orchestrates step execution, manages per-session history,
//! and collects results for the report and judge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use that_core::agent_loop::Message;
use that_core::config::AgentDef;
use that_core::orchestration::{
    build_preamble, discover_skills, execute_agent_run_eval, load_workspace_files,
    prepare_container, resolve_agent_workspace, resolved_skill_roots,
};
use that_core::session::{new_run_id, RunStatus, SessionManager, TranscriptEntry, TranscriptEvent};
use that_core::skills::{self, parse_frontmatter};
use tracing::info;

use crate::judge::{JudgeTranscriptEntry, LlmJudge};
use crate::report::{AggregatedUsage, AssertionResult, RunReport, StepResult};
use crate::scenario::{Assertion, Scenario, Step};
use crate::storage::EvalStorage;

/// Runs a single scenario and produces a [`RunReport`].
pub struct ScenarioRunner {
    /// In-memory conversation history per session label.
    histories: HashMap<String, Vec<Message>>,
    /// JSONL session ID per session label.
    session_ids: HashMap<String, String>,
    /// The SessionManager pointing at the eval run's isolated state dir.
    session_mgr: SessionManager,
    /// Collected step results.
    step_results: Vec<StepResult>,
    /// Collected assertion results.
    assertion_results: Vec<AssertionResult>,
    /// Transcript entries for the judge.
    judge_transcript: Vec<JudgeTranscriptEntry>,
    /// Aggregated token usage (best-effort from session JSONL).
    usage: AggregatedUsage,
}

impl ScenarioRunner {
    /// Create a runner backed by the given eval storage.
    pub fn new(storage: &EvalStorage) -> Result<Self> {
        let session_mgr = SessionManager::new(&storage.sessions_dir())?;
        Ok(Self {
            histories: HashMap::new(),
            session_ids: HashMap::new(),
            session_mgr,
            step_results: Vec::new(),
            assertion_results: Vec::new(),
            judge_transcript: Vec::new(),
            usage: AggregatedUsage::default(),
        })
    }

    /// Execute the scenario and return the complete report.
    #[tracing::instrument(name = "eval_run", skip_all, fields(
        scenario  = %scenario.name,
        run_id    = %run_id,
        agent     = %scenario.agent_name,
        openinference.span.kind = "CHAIN",
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
        session.id = %run_id,
        input.value = %scenario.description,
        input.mime_type = "text/plain",
        output.value = tracing::field::Empty,
        output.mime_type = "application/json",
        trace_id  = tracing::field::Empty,
        span_id   = tracing::field::Empty,
    ))]
    pub async fn run(
        mut self,
        scenario: &Scenario,
        run_id: &str,
        no_judge: bool,
    ) -> Result<RunReport> {
        let started_at = Utc::now();
        let wall_start = Instant::now();
        let trace_id = that_core::observability::current_trace_id();
        if let Some(ref tid) = trace_id {
            tracing::Span::current().record("trace_id", tid.as_str());
        }
        let root_span_id = that_core::observability::current_span_id();
        if let Some(ref sid) = root_span_id {
            tracing::Span::current().record("span_id", sid.as_str());
        }

        // Build an AgentDef from the scenario config.
        let agent = self.build_agent_def(scenario);
        self.ensure_agent_memory_schema(&agent);

        // Build preamble once (skills may change during the run due to create_skill,
        // but we rebuild per-prompt step to pick up any new skills).
        let workspace = self.resolve_workspace(&agent)?;
        let container = prepare_container(&agent, &workspace, scenario.sandbox).await?;

        // Seed eval identity files so the agent starts with a real character rather
        // than the bootstrap stub.  Written once before any steps run; each prompt
        // step's load_workspace_files call will then pick them up normally.
        {
            use that_core::workspace;
            let ws = that_core::orchestration::load_workspace_files(&agent, scenario.sandbox);
            if ws.needs_bootstrap() {
                let soul = workspace::eval_soul_md();
                let identity = workspace::eval_identity_md();
                if scenario.sandbox {
                    if let Some(c) = &container {
                        let _ = workspace::save_soul_sandbox(c, &agent.name, soul);
                        let _ = workspace::save_identity_sandbox(c, &agent.name, identity);
                    }
                } else {
                    let _ = workspace::save_soul_local(&agent.name, soul);
                    let _ = workspace::save_identity_local(&agent.name, identity);
                }
            }
        }

        let shell_ctx =
            ShellContext::new(&agent.name, &workspace, container.clone(), scenario.sandbox);

        let step_ctx = StepContext {
            scenario,
            agent: &agent,
            workspace: &workspace,
            container: &container,
            shell_ctx: &shell_ctx,
        };

        // Execute steps
        for (idx, step) in scenario.steps.iter().enumerate() {
            let step_wall = Instant::now();
            let result = self.execute_step(idx, step, &step_ctx).await;
            let duration_ms = step_wall.elapsed().as_millis() as u64;

            match result {
                Ok(mut sr) => {
                    sr.duration_ms = duration_ms;
                    self.step_results.push(sr);
                }
                Err(e) => {
                    let kind = step_kind_label(step);
                    self.step_results.push(StepResult {
                        index: idx,
                        kind,
                        session: None,
                        success: false,
                        error: Some(format!("{e:#}")),
                        response: None,
                        duration_ms,
                    });
                }
            }
        }

        // Run the LLM judge (unless --no-judge).
        let judge_score = if no_judge || scenario.rubric.criteria.is_empty() {
            None
        } else {
            let judge = LlmJudge::with_optional(
                scenario.judge_provider.as_deref(),
                scenario.judge_model.as_deref(),
            );
            match judge
                .judge(scenario, &self.judge_transcript, &self.assertion_results)
                .await
            {
                Ok(score) => Some(score),
                Err(e) => {
                    tracing::warn!(error = %e, "LLM judge failed — score will be missing");
                    None
                }
            }
        };

        let finished_at = Utc::now();
        let duration_ms = wall_start.elapsed().as_millis() as u64;
        let eval_output = serde_json::json!({
            "scenario": scenario.name,
            "steps_total": self.step_results.len(),
            "step_failures": self.step_results.iter().filter(|s| !s.success).count(),
            "assertions_total": self.assertion_results.len(),
            "assertions_passed": self.assertion_results.iter().filter(|a| a.passed).count(),
            "judge_total": judge_score.as_ref().map(|j| j.total),
            "duration_ms": duration_ms,
        })
        .to_string();
        tracing::Span::current().record("output.value", eval_output.as_str());
        tracing::Span::current().record("otel.status_code", "ok");
        tracing::Span::current().record("otel.status_description", "eval run completed");

        Ok(RunReport {
            run_id: run_id.to_string(),
            trace_id,
            root_span_id,
            scenario_name: scenario.name.clone(),
            scenario_description: scenario.description.clone(),
            agent_name: agent.name.clone(),
            provider: agent.provider.clone(),
            model: agent.model.clone(),
            started_at,
            finished_at,
            duration_ms,
            steps: self.step_results,
            assertions: self.assertion_results,
            judge_score,
            usage: self.usage,
        })
    }

    // ── Step dispatch ─────────────────────────────────────────────────────────

    async fn execute_step(
        &mut self,
        idx: usize,
        step: &Step,
        ctx: &StepContext<'_>,
    ) -> Result<StepResult> {
        let scenario = ctx.scenario;
        let agent = ctx.agent;
        let workspace = ctx.workspace;
        let container = ctx.container;
        let shell_ctx = ctx.shell_ctx;
        match step {
            Step::Prompt(p) => {
                let session_id = self.ensure_session(&p.session)?;
                let run_id = new_run_id();

                // Log user message to JSONL
                self.session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::UserMessage {
                            content: p.content.clone(),
                        },
                    },
                )?;

                // Record for judge
                self.judge_transcript.push(JudgeTranscriptEntry {
                    role: "user".to_string(),
                    content: p.content.clone(),
                });

                // Rebuild preamble with current skills (pick up any newly created skills or
                // workspace file edits the agent may have made in prior steps).
                let found_skills = discover_skills(agent, scenario.sandbox);
                let ws = load_workspace_files(agent, scenario.sandbox);
                let session_summaries = self.session_mgr.session_summaries(5).unwrap_or_default();
                let skill_roots = resolved_skill_roots(agent);
                let preamble = build_preamble(
                    workspace,
                    agent,
                    scenario.sandbox,
                    &found_skills,
                    &ws,
                    self.histories.get(&p.session).map(|h| h.len()).unwrap_or(0),
                    &session_id,
                    &session_summaries,
                    None,
                    None,
                );

                let history = self.histories.get(&p.session).cloned();

                let (response, tool_events) = tokio::time::timeout(
                    std::time::Duration::from_secs(scenario.timeout_secs),
                    execute_agent_run_eval(
                        agent,
                        container.clone(),
                        &preamble,
                        &p.content,
                        false,
                        history,
                        Some(&session_id),
                        skill_roots,
                    ),
                )
                .await
                .context("Prompt step timed out")?
                .context("Agent run failed")?;

                // Update history
                let hist = self.histories.entry(p.session.clone()).or_default();
                hist.push(Message::user(&p.content));
                hist.push(Message::assistant(&response));

                // Log assistant response to JSONL
                self.session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id: run_id.clone(),
                        event: TranscriptEvent::AssistantMessage {
                            content: response.clone(),
                        },
                    },
                )?;
                self.session_mgr.append(
                    &session_id,
                    &TranscriptEntry {
                        timestamp: Utc::now(),
                        run_id,
                        event: TranscriptEvent::RunEnd {
                            status: RunStatus::Success,
                            error: None,
                        },
                    },
                )?;

                // Record tool events for judge (ordered between user and assistant)
                for event in &tool_events {
                    self.judge_transcript.push(JudgeTranscriptEntry {
                        role: "tool".to_string(),
                        content: event.clone(),
                    });
                }
                // Record assistant response for judge
                self.judge_transcript.push(JudgeTranscriptEntry {
                    role: "assistant".to_string(),
                    content: response.clone(),
                });

                self.usage.prompt_steps += 1;

                Ok(StepResult {
                    index: idx,
                    kind: "prompt".to_string(),
                    session: Some(p.session.clone()),
                    success: true,
                    error: None,
                    response: Some(response),
                    duration_ms: 0,
                })
            }

            Step::ResetSession(r) => {
                self.histories.remove(&r.session);
                info!(session = %r.session, "Session history cleared");
                // Record session boundary for judge context.
                self.judge_transcript.push(JudgeTranscriptEntry {
                    role: "tool".to_string(),
                    content: format!("SESSION_RESET session={}", r.session),
                });
                Ok(StepResult {
                    index: idx,
                    kind: "reset_session".to_string(),
                    session: Some(r.session.clone()),
                    success: true,
                    error: None,
                    response: None,
                    duration_ms: 0,
                })
            }

            Step::CreateSkill(cs) => {
                let skill_dir =
                    skills::skills_dir_local(&agent.name).context("Cannot resolve skills dir")?;
                let target = skill_dir.join(&cs.name).join("SKILL.md");
                std::fs::create_dir_all(target.parent().unwrap())
                    .with_context(|| format!("Cannot create skill dir for {}", cs.name))?;
                std::fs::write(&target, &cs.content)
                    .with_context(|| format!("Cannot write SKILL.md for {}", cs.name))?;

                // Validate the written skill is parseable — catch missing name/description early.
                if parse_frontmatter(&cs.content).is_none() {
                    anyhow::bail!(
                        "create_skill '{}': SKILL.md was written but failed frontmatter parsing. \
                         Ensure the content has 'name:' and 'description:' at the root level of the frontmatter.",
                        cs.name
                    );
                }

                info!(skill = %cs.name, path = %target.display(), "Created skill");
                Ok(StepResult {
                    index: idx,
                    kind: "create_skill".to_string(),
                    session: None,
                    success: true,
                    error: None,
                    response: None,
                    duration_ms: 0,
                })
            }

            Step::RunCommand(rc) => {
                let rendered = shell_ctx.render(&rc.command);
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(scenario.timeout_secs),
                    run_shell_command(&rendered, shell_ctx),
                )
                .await
                .context("run_command timed out")??;

                let success = output.status.success();
                let error = if success {
                    None
                } else {
                    Some(format!(
                        "exit {}: {}",
                        output.status.code().unwrap_or(-1),
                        String::from_utf8_lossy(&output.stderr)
                    ))
                };
                Ok(StepResult {
                    index: idx,
                    kind: "run_command".to_string(),
                    session: None,
                    success,
                    error,
                    response: None,
                    duration_ms: 0,
                })
            }

            Step::CreateFile(cf) => {
                let rendered_path = shell_ctx.render(&cf.path);
                let path = std::path::Path::new(&rendered_path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("Cannot create dirs for {}", rendered_path))?;
                }
                std::fs::write(path, &cf.content)
                    .with_context(|| format!("Cannot write file {}", rendered_path))?;
                Ok(StepResult {
                    index: idx,
                    kind: "create_file".to_string(),
                    session: None,
                    success: true,
                    error: None,
                    response: None,
                    duration_ms: 0,
                })
            }

            Step::Assert(as_step) => {
                let mut all_passed = true;
                for assertion in &as_step.assertions {
                    let result = run_assertion(assertion, &self.judge_transcript, shell_ctx).await;
                    if !result.passed {
                        all_passed = false;
                    }
                    self.assertion_results.push(result);
                }
                Ok(StepResult {
                    index: idx,
                    kind: "assert".to_string(),
                    session: None,
                    success: all_passed,
                    error: if all_passed {
                        None
                    } else {
                        Some("One or more assertions failed".to_string())
                    },
                    response: None,
                    duration_ms: 0,
                })
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn ensure_session(&mut self, label: &str) -> Result<String> {
        if let Some(id) = self.session_ids.get(label) {
            return Ok(id.clone());
        }
        let id = self.session_mgr.create_session()?;
        self.session_ids.insert(label.to_string(), id.clone());
        Ok(id)
    }

    fn build_agent_def(&self, scenario: &Scenario) -> AgentDef {
        let mut agent = AgentDef {
            name: scenario.agent_name.clone(),
            ..Default::default()
        };
        if let Some(p) = &scenario.provider {
            agent.provider = p.clone();
        }
        if let Some(m) = &scenario.model {
            agent.model = m.clone();
        }
        agent.max_turns = scenario.max_turns;
        agent
    }

    fn resolve_workspace(&self, agent: &AgentDef) -> Result<PathBuf> {
        let ws = that_core::config::WorkspaceConfig::default();
        resolve_agent_workspace(&ws, agent)
    }

    fn ensure_agent_memory_schema(&self, agent: &AgentDef) {
        let cfg = that_tools::config::MemoryConfig {
            db_path: that_core::config::AgentDef::agent_memory_db_path(&agent.name)
                .display()
                .to_string(),
            ..Default::default()
        };
        if let Err(err) = that_tools::tools::memory::ensure_initialized(&cfg) {
            tracing::warn!(
                agent = %agent.name,
                path = %cfg.db_path,
                error = %err,
                "Failed to initialize agent memory DB for eval run"
            );
        }
    }
}

// ── Step context ──────────────────────────────────────────────────────────────

/// Groups the scenario-level state passed unchanged to every step.
struct StepContext<'a> {
    scenario: &'a Scenario,
    agent: &'a AgentDef,
    workspace: &'a PathBuf,
    container: &'a Option<String>,
    shell_ctx: &'a ShellContext,
}

// ── Shell command execution ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ShellContext {
    agent_name: String,
    workspace: String,
    container_name: String,
    sandbox: bool,
}

impl ShellContext {
    fn new(agent_name: &str, workspace: &Path, container: Option<String>, sandbox: bool) -> Self {
        Self {
            agent_name: agent_name.to_string(),
            workspace: workspace.display().to_string(),
            container_name: container.unwrap_or_default(),
            sandbox,
        }
    }

    fn render(&self, input: &str) -> String {
        input
            .replace("{{agent_name}}", &self.agent_name)
            .replace("{{workspace}}", &self.workspace)
            .replace("{{container_name}}", &self.container_name)
            .replace("{{sandbox}}", if self.sandbox { "true" } else { "false" })
    }
}

async fn run_shell_command(command: &str, ctx: &ShellContext) -> Result<std::process::Output> {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("THAT_EVAL_AGENT_NAME", &ctx.agent_name)
        .env("THAT_EVAL_WORKSPACE", &ctx.workspace)
        .env("THAT_EVAL_CONTAINER_NAME", &ctx.container_name)
        .env(
            "THAT_EVAL_SANDBOX",
            if ctx.sandbox { "true" } else { "false" },
        )
        .output()
        .await
        .context("Failed to spawn shell command")
}

// ── Assertion execution ───────────────────────────────────────────────────────

async fn run_assertion(
    assertion: &Assertion,
    transcript: &[JudgeTranscriptEntry],
    shell_ctx: &ShellContext,
) -> AssertionResult {
    match assertion {
        Assertion::FileExists { path } => {
            let rendered = shell_ctx.render(path);
            let exists = std::path::Path::new(&rendered).exists();
            AssertionResult {
                kind: "file_exists".to_string(),
                description: format!("File exists: {rendered}"),
                passed: exists,
                reason: if exists {
                    None
                } else {
                    Some(format!("{rendered} does not exist"))
                },
            }
        }

        Assertion::CommandSucceeds { command } => {
            let rendered = shell_ctx.render(command);
            match run_shell_command(&rendered, shell_ctx).await {
                Ok(out) => {
                    let passed = out.status.success();
                    AssertionResult {
                        kind: "command_succeeds".to_string(),
                        description: format!("Command: {rendered}"),
                        passed,
                        reason: if passed {
                            None
                        } else {
                            Some(format!(
                                "exit {}: {}",
                                out.status.code().unwrap_or(-1),
                                String::from_utf8_lossy(&out.stderr)
                            ))
                        },
                    }
                }
                Err(e) => AssertionResult {
                    kind: "command_succeeds".to_string(),
                    description: format!("Command: {rendered}"),
                    passed: false,
                    reason: Some(format!("{e:#}")),
                },
            }
        }

        Assertion::FileContains { path, contains } => {
            let rendered = shell_ctx.render(path);
            match std::fs::read_to_string(&rendered) {
                Ok(content) => {
                    let passed = content.contains(contains.as_str());
                    AssertionResult {
                        kind: "file_contains".to_string(),
                        description: format!("File {rendered} contains {contains:?}"),
                        passed,
                        reason: if passed {
                            None
                        } else {
                            Some(format!("{rendered} does not contain {contains:?}"))
                        },
                    }
                }
                Err(e) => AssertionResult {
                    kind: "file_contains".to_string(),
                    description: format!("File {rendered} contains {contains:?}"),
                    passed: false,
                    reason: Some(format!("Cannot read {rendered}: {e}")),
                },
            }
        }

        Assertion::ToolCallSeen { tool, min_count } => {
            let prefix = format!("CALL {tool} ");
            let exact = format!("CALL {tool}");
            let count = transcript
                .iter()
                .filter(|entry| entry.role == "tool")
                .filter(|entry| entry.content.starts_with(&prefix) || entry.content == exact)
                .count();

            let passed = count >= *min_count;
            AssertionResult {
                kind: "tool_call_seen".to_string(),
                description: format!("Tool call seen: {tool} (min_count={min_count})"),
                passed,
                reason: if passed {
                    None
                } else {
                    Some(format!(
                        "Observed {count} call(s) to '{tool}', expected at least {min_count}"
                    ))
                },
            }
        }
    }
}

fn step_kind_label(step: &Step) -> String {
    match step {
        Step::Prompt(_) => "prompt",
        Step::ResetSession(_) => "reset_session",
        Step::CreateSkill(_) => "create_skill",
        Step::RunCommand(_) => "run_command",
        Step::CreateFile(_) => "create_file",
        Step::Assert(_) => "assert",
    }
    .to_string()
}
