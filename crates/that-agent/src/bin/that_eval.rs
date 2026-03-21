//! `that-eval` — Agent evaluation CLI entry point.

use anyhow::Result;
use clap::Parser;
use serde_json::json;
use that_agent::session::new_run_id;
use tracing::info;

use that_agent::eval::cli::{Cli, Commands, ReportFormat};
use that_agent::eval::report::RunReport;
use that_agent::eval::runner::ScenarioRunner;
use that_agent::eval::scenario::Scenario;
use that_agent::eval::storage::EvalStorage;

#[derive(Debug, Clone, Copy)]
struct EvalGatePolicy {
    fail_on_step_error: bool,
    min_assertion_pass_pct: u8,
    min_judge_score: Option<u32>,
}

#[derive(Debug)]
struct EvalGateOutcome {
    passed: bool,
    reasons: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before tracing init so PHOENIX_* / OTEL_* vars are available.
    let _ = dotenvy::dotenv();

    // Initialise tracing (respects RUST_LOG; set PHOENIX_TRACING=true for OTel export).
    that_agent::observability::init_tracing("that_eval=info,that_core=warn");

    let cli = Cli::parse();

    match cli.command {
        Commands::Run(args) => {
            let mut scenario = Scenario::from_file(&args.scenario)?;
            if let Some(v) = args.agent {
                scenario.agent_name = v;
            }
            if let Some(v) = args.provider {
                scenario.provider = Some(v);
            }
            if let Some(v) = args.model {
                scenario.model = Some(v);
            }
            let policy = EvalGatePolicy {
                fail_on_step_error: args.fail_on_step_error,
                min_assertion_pass_pct: args.min_assertion_pass_pct,
                min_judge_score: args.min_judge_score,
            };
            let gate = run_scenario(&scenario, args.no_judge, policy).await?;
            if !gate.passed {
                anyhow::bail!("Eval gate failed: {}", gate.reasons.join(" | "));
            }
        }

        Commands::RunAll(args) => {
            let mut files: Vec<_> = std::fs::read_dir(&args.dir)?
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "toml").unwrap_or(false))
                .map(|e| e.path())
                .collect();
            files.sort();

            let mut ran = 0usize;
            let mut failed = 0usize;
            let policy = EvalGatePolicy {
                fail_on_step_error: args.fail_on_step_error,
                min_assertion_pass_pct: args.min_assertion_pass_pct,
                min_judge_score: args.min_judge_score,
            };
            for path in &files {
                let scenario = match Scenario::from_file(path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[SKIP] {}: {e}", path.display());
                        continue;
                    }
                };

                // Tag filter
                if !args.tags.is_empty() {
                    let has_tag = args.tags.iter().any(|t| scenario.tags.contains(t));
                    if !has_tag {
                        info!(scenario = %scenario.name, "Skipping — no matching tag");
                        continue;
                    }
                }

                println!("\n─── Running: {} ───", scenario.name);
                match run_scenario(&scenario, args.no_judge, policy).await {
                    Ok(gate) if gate.passed => {}
                    Ok(gate) => {
                        failed += 1;
                        eprintln!("[FAIL] {}: {}", scenario.name, gate.reasons.join(" | "));
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("[ERROR] {}: {e:#}", scenario.name);
                    }
                }
                ran += 1;
            }
            println!("\nRan {ran}/{} scenarios ({} failed).", files.len(), failed);
            if failed > 0 {
                anyhow::bail!("{} scenario(s) failed evaluation gates", failed);
            }
        }

        Commands::Report(args) => {
            let report = EvalStorage::load_report(&args.run_id)?;
            match args.format {
                ReportFormat::Json => println!("{}", report.to_json()?),
                ReportFormat::Markdown => println!("{}", report.to_markdown()),
            }
        }

        Commands::List => {
            let runs = EvalStorage::list_runs()?;
            if runs.is_empty() {
                println!("No eval runs found.");
            } else {
                println!("{} eval run(s):\n", runs.len());
                for id in &runs {
                    println!("  {id}");
                }
            }
        }

        Commands::ListScenarios(args) => {
            let dir = &args.dir;
            if !dir.exists() {
                anyhow::bail!("Directory not found: {}", dir.display());
            }

            let mut files: Vec<_> = std::fs::read_dir(dir)?
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "toml").unwrap_or(false))
                .map(|e| e.path())
                .collect();
            files.sort();

            if files.is_empty() {
                println!("No scenario files found in {}.", dir.display());
            } else {
                println!("{} scenario(s) in {}:\n", files.len(), dir.display());
                println!("{:<30} {:<12} {:<8} Run with", "Name", "Tags", "Steps");
                println!("{}", "─".repeat(80));
                for path in &files {
                    match Scenario::from_file(path) {
                        Ok(s) => {
                            let tags = if s.tags.is_empty() {
                                "—".to_string()
                            } else {
                                s.tags.join(",")
                            };
                            println!(
                                "{:<30} {:<12} {:<8} that-eval run {}",
                                s.name,
                                tags,
                                s.steps.len(),
                                path.display()
                            );
                        }
                        Err(e) => {
                            println!(
                                "{:<30} [parse error: {e}]",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            );
                        }
                    }
                }
            }
        }
    }

    that_agent::observability::shutdown_tracing();
    Ok(())
}

fn evaluate_gate(report: &RunReport, policy: EvalGatePolicy) -> EvalGateOutcome {
    let step_failures = report.steps.iter().filter(|s| !s.success).count();
    let assertions_total = report.assertions.len();
    let assertions_passed = report.assertions.iter().filter(|a| a.passed).count();
    let assertion_pass_pct: u8 = if assertions_total == 0 {
        100
    } else {
        ((assertions_passed * 100) / assertions_total) as u8
    };

    let mut reasons = Vec::new();

    if policy.fail_on_step_error && step_failures > 0 {
        reasons.push(format!("{step_failures} step(s) failed"));
    }
    if assertion_pass_pct < policy.min_assertion_pass_pct {
        reasons.push(format!(
            "assertion pass rate {}% is below required {}%",
            assertion_pass_pct, policy.min_assertion_pass_pct
        ));
    }
    if let Some(min) = policy.min_judge_score {
        match &report.judge_score {
            Some(score) if score.total >= min => {}
            Some(score) => {
                reasons.push(format!(
                    "judge score {} is below required {}",
                    score.total, min
                ));
            }
            None => {
                reasons.push(format!(
                    "judge score missing but min_judge_score={} is required",
                    min
                ));
            }
        }
    }

    EvalGateOutcome {
        passed: reasons.is_empty(),
        reasons,
    }
}

async fn run_scenario(
    scenario: &Scenario,
    no_judge: bool,
    policy: EvalGatePolicy,
) -> Result<EvalGateOutcome> {
    let run_id = new_run_id();
    let storage = EvalStorage::new(&run_id)?;

    println!("Scenario : {}", scenario.name);
    println!("Run ID   : {run_id}");
    println!("Steps    : {}", scenario.steps.len());
    println!(
        "Judge    : {}",
        if no_judge { "disabled" } else { "enabled" }
    );
    println!(
        "Gate     : steps={} assertions>={}%, judge>={}",
        if policy.fail_on_step_error {
            "strict"
        } else {
            "ignore"
        },
        policy.min_assertion_pass_pct,
        policy
            .min_judge_score
            .map(|v| v.to_string())
            .unwrap_or_else(|| "n/a".to_string())
    );
    println!();

    let runner = ScenarioRunner::new(&storage)?;
    let report = runner.run(scenario, &run_id, no_judge).await?;

    storage.write_report(&report)?;

    // Print summary
    let pass_count = report.assertions.iter().filter(|a| a.passed).count();
    let total_count = report.assertions.len();

    println!("\n─── Results ───────────────────────────────────");
    println!("Duration  : {}ms", report.duration_ms);
    println!(
        "Steps     : {} total ({} failed)",
        report.steps.len(),
        report.steps.iter().filter(|s| !s.success).count()
    );
    println!("Assertions: {pass_count}/{total_count} passed");

    if let Some(score) = &report.judge_score {
        println!("Judge     : {}/100", score.total);
        if let Some(rationale) = &score.rationale {
            // Collect chars (Unicode-safe), strip control characters, cap at 120 chars.
            let clean: String = rationale
                .chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect();
            let preview = if rationale.chars().filter(|c| !c.is_control()).count() > 120 {
                format!("{clean}…")
            } else {
                clean
            };
            println!("Rationale : {preview}");
        }
    }

    let gate = evaluate_gate(&report, policy);
    if let Err(err) = maybe_post_eval_annotations(&report, &gate).await {
        eprintln!("[WARN] Failed to publish Phoenix annotations: {err}");
    }
    println!(
        "Gate      : {}",
        if gate.passed {
            "PASS".to_string()
        } else {
            format!("FAIL ({})", gate.reasons.join(" | "))
        }
    );

    if let Some(trace_id) = &report.trace_id {
        let host_hint =
            std::env::var("PHOENIX_HOST").unwrap_or_else(|_| "http://localhost:6006".to_string());
        let project_hint =
            std::env::var("PHOENIX_PROJECT").unwrap_or_else(|_| "<project-name>".to_string());
        println!("Trace ID  : {trace_id}");
        if let Some(span_id) = &report.root_span_id {
            println!("Span ID   : {span_id}");
        }
        println!(
            "Trace Raw : PHOENIX_HOST={host_hint} PHOENIX_PROJECT={project_hint} scripts/phoenix-trace-raw.sh {trace_id} > /tmp/trace-{trace_id}.json"
        );
        println!(
            "Trace Raw : PHOENIX_HOST={host_hint} PHOENIX_PROJECT={project_hint} scripts/phoenix-trace-raw.sh {run_id} > /tmp/trace-{run_id}.json"
        );
    }

    println!();
    println!("Report    : {}", storage.report_md_path().display());
    println!("JSON      : {}", storage.report_json_path().display());

    Ok(gate)
}

async fn maybe_post_eval_annotations(report: &RunReport, gate: &EvalGateOutcome) -> Result<()> {
    let enabled = std::env::var("PHOENIX_EVAL_ANNOTATIONS")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(true);
    if !enabled {
        return Ok(());
    }

    let Some(span_id) = report.root_span_id.as_deref() else {
        return Ok(());
    };
    let Some(trace_id) = report.trace_id.as_deref() else {
        return Ok(());
    };

    let pass_count = report.assertions.iter().filter(|a| a.passed).count();
    let total_count = report.assertions.len();
    let pass_ratio = if total_count == 0 {
        1.0
    } else {
        pass_count as f64 / total_count as f64
    };

    let mut annotations = vec![
        json!({
            "span_id": span_id,
            "name": "eval.gate",
            "annotator_kind": "CODE",
            "result": {
                "label": if gate.passed { "PASS" } else { "FAIL" },
                "score": if gate.passed { 1.0 } else { 0.0 },
                "explanation": if gate.passed {
                    "All evaluation gates passed".to_string()
                } else {
                    gate.reasons.join(" | ")
                },
            },
            "metadata": {
                "trace_id": trace_id,
                "run_id": report.run_id,
                "scenario": report.scenario_name,
                "provider": report.provider,
                "model": report.model,
            }
        }),
        json!({
            "span_id": span_id,
            "name": "eval.assertions",
            "annotator_kind": "CODE",
            "result": {
                "label": format!("{pass_count}/{total_count}"),
                "score": pass_ratio,
                "explanation": format!("{pass_count} of {total_count} assertions passed"),
            },
            "metadata": {
                "trace_id": trace_id,
                "run_id": report.run_id,
            }
        }),
    ];

    if let Some(judge) = &report.judge_score {
        annotations.push(json!({
            "span_id": span_id,
            "name": "eval.judge",
            "annotator_kind": "LLM",
            "result": {
                "label": format!("{}/100", judge.total),
                "score": (judge.total as f64) / 100.0,
                "explanation": judge.rationale.clone().unwrap_or_default(),
            },
            "metadata": {
                "trace_id": trace_id,
                "run_id": report.run_id,
                "breakdown": judge.breakdown,
            }
        }));
    }

    let host =
        std::env::var("PHOENIX_HOST").unwrap_or_else(|_| "http://localhost:6006".to_string());
    let endpoint = format!(
        "{}/v1/span_annotations?sync=false",
        host.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let mut req = client.post(endpoint).json(&json!({ "data": annotations }));
    if let Ok(key) = std::env::var("PHOENIX_API_KEY") {
        if !key.trim().is_empty() {
            req = req.header("api_key", key.clone()).bearer_auth(key);
        }
    }

    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("span_annotations API failed {status}: {body}");
    }

    Ok(())
}
