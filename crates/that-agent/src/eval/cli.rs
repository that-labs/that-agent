//! CLI command definitions for `that-eval`.

use clap::{Parser, Subcommand};

/// Agent evaluation harness for that-agent.
#[derive(Debug, Parser)]
#[command(name = "that-eval", about = "Run agent evaluation scenarios")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Run a single scenario file.
    Run(RunArgs),

    /// Run all scenario files in a directory.
    RunAll(RunAllArgs),

    /// Display a saved report.
    Report(ReportArgs),

    /// List past eval runs.
    List,

    /// List available scenario files in a directory.
    #[command(name = "list-scenarios")]
    ListScenarios(ListScenariosArgs),
}

/// Arguments for `that-eval list-scenarios`.
#[derive(Debug, Parser)]
pub struct ListScenariosArgs {
    /// Directory to scan for scenario TOML files (default: ./evals/scenarios).
    #[arg(default_value = "evals/scenarios")]
    pub dir: std::path::PathBuf,
}

/// Arguments for `that-eval run`.
#[derive(Debug, Parser)]
pub struct RunArgs {
    /// Path to the scenario TOML file.
    pub scenario: std::path::PathBuf,

    /// Override the agent name from the scenario.
    #[arg(long)]
    pub agent: Option<String>,

    /// Override the LLM provider (e.g. anthropic, openai).
    #[arg(long)]
    pub provider: Option<String>,

    /// Override the model (e.g. claude-sonnet-4-6).
    #[arg(long)]
    pub model: Option<String>,

    /// Skip the LLM judge step.
    #[arg(long)]
    pub no_judge: bool,

    /// Fail this run if any step fails.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub fail_on_step_error: bool,

    /// Fail this run if assertion pass percentage is below threshold.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub min_assertion_pass_pct: u8,

    /// Optional judge threshold (0-100). If set, run fails when judge is missing or below this score.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
    pub min_judge_score: Option<u32>,
}

/// Arguments for `that-eval run-all`.
#[derive(Debug, Parser)]
pub struct RunAllArgs {
    /// Directory containing scenario TOML files.
    pub dir: std::path::PathBuf,

    /// Only run scenarios whose tags include at least one of these (comma-separated).
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Skip the LLM judge step.
    #[arg(long)]
    pub no_judge: bool,

    /// Fail the command if any scenario has a failed step.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub fail_on_step_error: bool,

    /// Fail a scenario when assertion pass percentage is below threshold.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub min_assertion_pass_pct: u8,

    /// Optional judge threshold (0-100). If set, each scenario fails when judge is missing or below this score.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=100))]
    pub min_judge_score: Option<u32>,
}

/// Arguments for `that-eval report`.
#[derive(Debug, Parser)]
pub struct ReportArgs {
    /// The run ID to display.
    pub run_id: String,

    /// Output format.
    #[arg(long, default_value = "markdown")]
    pub format: ReportFormat,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ReportFormat {
    Json,
    Markdown,
}
