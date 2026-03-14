use std::io::Write;
use std::path::Path;

use chrono::Utc;

/// Appends a single audit event as a JSON line to `{state_dir}/audit.log`.
///
/// Each line: `{"ts":"2026-...","event":"tool_call","detail":"shell_exec: ls -la"}`
pub fn log_event(state_dir: &Path, event: &str, detail: &str) {
    let path = state_dir.join("audit.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let line = format!(
            "{{\"ts\":\"{}\",\"event\":\"{}\",\"detail\":{}}}\n",
            Utc::now().to_rfc3339(),
            event,
            serde_json::to_string(detail).unwrap_or_default(),
        );
        let _ = f.write_all(line.as_bytes());
    }
}

/// Appends a full-content structured event to `{state_dir}/run.log`.
///
/// Format: `[2026-03-14 10:30:45.123] [agent][EventType] [module] detail...`
pub fn log_run_event(state_dir: &Path, agent: &str, event_type: &str, module: &str, detail: &str) {
    let path = state_dir.join("run.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = Utc::now().format("%Y-%m-%d %H:%M:%S%.3f");
        let line = format!("[{ts}] [{agent}][{event_type}] [{module}] {detail}\n");
        let _ = f.write_all(line.as_bytes());
    }
}

/// Appends a tool error as a JSON line to `{state_dir}/errors.jsonl`.
pub fn log_error(state_dir: &Path, tool: &str, error: &str, context: &str) {
    let path = state_dir.join("errors.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let line = format!(
            "{{\"ts\":\"{}\",\"tool\":{},\"error\":{},\"context\":{}}}\n",
            Utc::now().to_rfc3339(),
            serde_json::to_string(tool).unwrap_or_default(),
            serde_json::to_string(error).unwrap_or_default(),
            serde_json::to_string(context).unwrap_or_default(),
        );
        let _ = f.write_all(line.as_bytes());
    }
}
