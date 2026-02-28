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
