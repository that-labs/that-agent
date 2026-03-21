//! Persistent storage paths for eval run artifacts.
//!
//! Each run is stored under `~/.that-agent/evals/<run-id>/`:
//!   - `report.json`    — machine-readable RunReport
//!   - `report.md`      — human-readable Markdown report
//!   - `sessions/`      — JSONL session transcripts written by SessionManager

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::eval::report::RunReport;

/// Manages the directory layout for a single eval run.
pub struct EvalStorage {
    run_dir: PathBuf,
}

impl EvalStorage {
    /// Create storage rooted at `~/.that-agent/evals/<run_id>/`.
    pub fn new(run_id: &str) -> Result<Self> {
        let base = dirs::home_dir()
            .context("Cannot resolve home directory")?
            .join(".that-agent")
            .join("evals")
            .join(run_id);
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Cannot create eval dir {}", base.display()))?;
        std::fs::create_dir_all(base.join("sessions"))
            .with_context(|| format!("Cannot create sessions dir under {}", base.display()))?;
        Ok(Self { run_dir: base })
    }

    /// Root directory for this run.
    pub fn run_dir(&self) -> &PathBuf {
        &self.run_dir
    }

    /// Path to the sessions sub-directory (used as state_dir for SessionManager).
    pub fn sessions_dir(&self) -> PathBuf {
        self.run_dir.join("sessions")
    }

    /// Path to report.json.
    pub fn report_json_path(&self) -> PathBuf {
        self.run_dir.join("report.json")
    }

    /// Path to report.md.
    pub fn report_md_path(&self) -> PathBuf {
        self.run_dir.join("report.md")
    }

    /// Serialize and write both report formats to disk.
    pub fn write_report(&self, report: &RunReport) -> Result<()> {
        let json = report.to_json()?;
        std::fs::write(self.report_json_path(), &json)
            .with_context(|| format!("Cannot write {}", self.report_json_path().display()))?;

        let md = report.to_markdown();
        std::fs::write(self.report_md_path(), md)
            .with_context(|| format!("Cannot write {}", self.report_md_path().display()))?;

        Ok(())
    }

    /// List all past run IDs sorted newest-first.
    pub fn list_runs() -> Result<Vec<String>> {
        let evals_dir = dirs::home_dir()
            .context("Cannot resolve home directory")?
            .join(".that-agent")
            .join("evals");

        if !evals_dir.exists() {
            return Ok(Vec::new());
        }

        let mut runs: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&evals_dir)
            .with_context(|| format!("Cannot read {}", evals_dir.display()))?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let mtime = e.metadata().ok()?.modified().ok()?;
                Some((mtime, name))
            })
            .collect();

        runs.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(runs.into_iter().map(|(_, name)| name).collect())
    }

    /// Load a report from a run ID.
    pub fn load_report(run_id: &str) -> Result<RunReport> {
        let path = dirs::home_dir()
            .context("Cannot resolve home directory")?
            .join(".that-agent")
            .join("evals")
            .join(run_id)
            .join("report.json");

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        serde_json::from_str(&text).context("Cannot deserialize report.json")
    }
}
