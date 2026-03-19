---
name: self-eval
description: Autonomous self-evaluation and improvement loop. Use when asked to evaluate performance, build test scenarios, create regression judges, run overnight validation suites, or close a feedback loop between eval results and harness improvements.
metadata:
  version: 1.0.0
---

# Self-Evaluation & Improvement

You have an evaluation harness that tests your behavior against scenarios you author.
Compose existing tools (`fs_write`, `shell_exec`, `fs_cat`, `mem_add`, `identity_update`,
heartbeat scheduling) — no special eval tools are needed.

## Execution Guardrails

**Human approval is required before:**
- Running any eval suite (single or batch) — always confirm scope and estimated cost first
- Scheduling recurring eval heartbeats — use `human_approved: true` and explain the schedule
- Making harness changes (skills, Agents.md) based on eval results — present findings first

**Keep evals short and bounded:**
- Individual scenarios should complete within `timeout_secs` (default 120s)
- Scenarios needing longer execution (service startup, ML tasks) must have explicit
  human approval via `human_ask` before running
- Batch runs (`run-all`) must be scoped by `--tags` — never run the full suite without asking
- Always estimate the number of LLM calls and approximate cost before starting

**Background execution:**
- Short evals (single scenario, <2 min) can run in the foreground
- Batch or overnight suites should be scheduled via heartbeat, not run inline
- Always send a `channel_notify` when a background eval completes with a score summary
- Never block the human conversation with long-running eval work

## Memory Isolation (Critical)

Eval runs generate noise — failed attempts, intermediate reasoning, debug artifacts.
**Never store eval-internal state in your main memory.** The human relies on your memory
for real collaboration; polluting it with eval churn degrades the experience.

**Rules:**
- During eval authoring and execution, do NOT call `mem_add` for intermediate results,
  failure analysis, debug notes, or scenario metadata
- After a full eval cycle completes, store ONLY validated improvements as memories:
  - A skill you created or updated that measurably improved a score
  - A behavioral pattern you confirmed works (with before/after scores)
  - A regression you discovered and fixed
- Tag eval-derived memories with `eval-validated` so they are distinguishable
- Format: `mem_add(content="[eval-validated] <concise finding>", tags=["eval-validated", "<domain>"])`
- If an eval run fails or scores poorly, do NOT memorize the failure — fix it and re-run

**What goes where:**
| Information | Store in | NOT in |
|-------------|----------|--------|
| Eval scenario files | Filesystem (evals directory) | Memory |
| Intermediate results | Plan files or Status.md | Memory |
| Failed attempt analysis | Scratchpad or plan file | Memory |
| Confirmed improvement | Memory (tagged `eval-validated`) | — |
| Baseline scores | Status.md | Memory |
| Judge reports | Filesystem (auto-persisted) | Memory |

## Scenario Authoring

Write TOML scenario files to your agent evals directory.
Load `read_skill self-eval scenario-schema` for the full field reference.

**Principles:**
- Prompts must read like human requests — never name tools, skills, or internal mechanics
- Test observable behavior, not implementation details
- Each scenario should test one capability domain (memory, planning, coding, etc.)
- Use `tags` for domain filtering (e.g., `["memory", "regression"]`, `["backend", "e2e"]`)
- Keep scenarios focused: 2-5 steps, clear rubric, deterministic assertions where possible

**Rubric design:**
- Criteria should be independently scorable — no criterion should depend on another
- Weight by importance: core behavior > polish > edge cases
- Prefer assertions for deterministic checks, judge criteria for qualitative assessment
- Write descriptions as observable evidence: "Agent called X before Y" not "Agent understood Z"

## Running Evals

```
# Single scenario
shell_exec("that-eval run <path-to-scenario.toml>")

# All scenarios in a directory
shell_exec("that-eval run-all <directory> --tags <tag>")

# With quality gates
shell_exec("that-eval run <path> --min-judge-score 70 --min-assertion-pass-pct 80")

# List available scenarios
shell_exec("that-eval list-scenarios <directory>")

# Read a past report
shell_exec("that-eval report <run-id> --format markdown")
```

Reports are auto-persisted. Read them with `fs_cat` on the path printed after each run.

## The Improvement Loop

This is the core cycle. Run it deliberately, not reactively.

**Step 1 — Baseline.** Run existing scenarios. Record scores in `Status.md` as your baseline.
Do NOT store baselines in memory.

**Step 2 — Analyze.** Read judge reports (`fs_cat` on the report path). Identify the
lowest-scoring criteria. Look for patterns across multiple scenarios.

**Step 3 — Improve.** Make targeted changes:
- Create or update a skill to address a gap
- Update `Agents.md` via `identity_update` for behavioral adjustments
- Never change multiple things at once — isolate variables

**Step 4 — Re-run.** Execute the same scenarios again. Compare scores against baseline
in `Status.md`.

**Step 5 — Persist or revert.**
- If scores improved: store the validated finding in memory (tagged `eval-validated`),
  keep the skill/instruction change
- If scores stayed flat or regressed: revert the change, do NOT memorize it

**Step 6 — Regression suite.** After confirming improvements, add the scenario to your
recurring eval suite so future changes don't regress it.

## Scheduled Overnight Runs

Use heartbeat entries to schedule eval suites:

```
identity_update(file="Heartbeat.md", ...)
```

Add an entry with `schedule: daily` or `cron:` expression. The heartbeat task should:
1. Run the eval suite via `shell_exec`
2. Read reports and compare against baselines in `Status.md`
3. Send a `channel_notify` summary to the human (scores, regressions, improvements)
4. Only update memory for validated improvements

Require `human_approved: true` for any schedule more frequent than hourly.

## E2E Backend Testing Patterns

Load `read_skill self-eval e2e-patterns` for patterns covering:
- Service setup/teardown via `run_command` steps
- Agent-driven API interaction via `prompt` steps
- State verification via `command_succeeds` assertions
- Multi-service coordination scenarios
- Judge rubrics for E2E test quality assessment

## Anti-Patterns

- Memorizing every eval result (floods memory, degrades human interaction)
- Running evals without a baseline (no way to measure improvement)
- Changing multiple things between eval runs (can't isolate what helped)
- Writing scenarios that test tool names instead of outcomes
- Skipping the re-run step (unvalidated changes are speculation)
- Storing scenario metadata or debug artifacts in memory
