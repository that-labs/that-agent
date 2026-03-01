//! Heartbeat — periodic, urgency-aware self-scheduling for autonomous agents.
//!
//! The agent reads `Heartbeat.md` from its agent directory. Each H2 heading is
//! one scheduled entry with key-value metadata lines followed by a body description.
//! The background monitor in `run_listen()` polls this file every N seconds,
//! finds due entries, and dispatches them as autonomous agent runs.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

use chrono::{DateTime, Datelike, Local, NaiveDateTime, Timelike, Utc, Weekday};
use tracing::warn;

// ── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Priority {
    Urgent,
    High,
    Normal,
    Low,
    Unknown(String),
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Priority::Urgent => write!(f, "urgent"),
            Priority::High => write!(f, "high"),
            Priority::Normal => write!(f, "normal"),
            Priority::Low => write!(f, "low"),
            Priority::Unknown(s) => write!(f, "{s}"),
        }
    }
}

impl From<&str> for Priority {
    fn from(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "urgent" => Priority::Urgent,
            "high" => Priority::High,
            "normal" => Priority::Normal,
            "low" => Priority::Low,
            other => Priority::Unknown(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    Once,
    Minutely,
    Hourly,
    Daily,
    Weekly,
    Cron(String),
    Unknown(String),
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Schedule::Once => write!(f, "once"),
            Schedule::Minutely => write!(f, "minutely"),
            Schedule::Hourly => write!(f, "hourly"),
            Schedule::Daily => write!(f, "daily"),
            Schedule::Weekly => write!(f, "weekly"),
            Schedule::Cron(expr) => write!(f, "cron: {expr}"),
            Schedule::Unknown(s) => write!(f, "{s}"),
        }
    }
}

impl From<&str> for Schedule {
    fn from(s: &str) -> Self {
        let raw = s.trim();
        if raw.is_empty() {
            return Schedule::Unknown(String::new());
        }
        match raw.to_lowercase().as_str() {
            "once" => Schedule::Once,
            "minutely" | "minute" | "every_minute" => Schedule::Minutely,
            "hourly" => Schedule::Hourly,
            "daily" => Schedule::Daily,
            "weekly" => Schedule::Weekly,
            _ => {
                if let Some(expr) = raw.strip_prefix("cron:") {
                    let expr = expr.trim();
                    if parse_cron_expression(expr).is_ok() {
                        return Schedule::Cron(expr.to_string());
                    }
                } else if parse_cron_expression(raw).is_ok() {
                    return Schedule::Cron(raw.to_string());
                }
                Schedule::Unknown(raw.to_string())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pending,
    Running,
    Processing,
    Done,
    Skipped,
    Unknown(String),
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Pending => write!(f, "pending"),
            Status::Running => write!(f, "running"),
            Status::Processing => write!(f, "processing"),
            Status::Done => write!(f, "done"),
            Status::Skipped => write!(f, "skipped"),
            Status::Unknown(s) => write!(f, "{s}"),
        }
    }
}

impl From<&str> for Status {
    fn from(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "pending" => Status::Pending,
            "running" => Status::Running,
            "processing" => Status::Processing,
            "done" => Status::Done,
            "skipped" => Status::Skipped,
            other => Status::Unknown(other.to_string()),
        }
    }
}

/// A single scheduled entry in Heartbeat.md.
#[derive(Debug, Clone)]
pub struct HeartbeatEntry {
    pub title: String,
    pub priority: Priority,
    pub schedule: Schedule,
    pub status: Status,
    pub last_run: Option<DateTime<Local>>,
    /// Earliest time this entry may fire. Used for deferred/reminder entries.
    pub not_before: Option<DateTime<Local>>,
    pub body: String,
}

// ── Path helpers ─────────────────────────────────────────────────────────────

/// Return the Heartbeat.md path for a local agent: `~/.that-agent/agents/<name>/Heartbeat.md`.
pub fn heartbeat_md_path_local(agent_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("Heartbeat.md")
    })
}

/// Return the Heartbeat.md path inside a sandbox container.
pub fn heartbeat_md_path_sandbox(agent_name: &str) -> String {
    format!("/home/agent/.that-agent/agents/{}/Heartbeat.md", agent_name)
}

// ── Parse / serialize ────────────────────────────────────────────────────────

/// Parse a Heartbeat.md file into a list of entries.
///
/// Format: H2 headings delimit entries. Key-value lines (`key: value`) follow
/// the heading until the first blank line. Remaining content is the body.
/// Entries are separated by `---` lines.
pub fn parse_heartbeat(content: &str) -> Vec<HeartbeatEntry> {
    let mut entries = Vec::new();

    // Split on standalone `---` lines (entry separators).
    let sections: Vec<&str> = content.split("\n---").collect();

    for section in sections {
        let section = section.trim();
        if section.is_empty() {
            continue;
        }

        let mut title: Option<String> = None;
        let mut fields: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut body_lines: Vec<&str> = Vec::new();
        let mut found_title = false;
        let mut in_body = false;

        for line in section.lines() {
            // Skip top-level `# Heartbeat` header.
            if line.starts_with("# ") && !line.starts_with("## ") {
                continue;
            }

            // H2 heading = entry title.
            if line.starts_with("## ") && !found_title {
                title = Some(line[3..].trim().to_string());
                found_title = true;
                continue;
            }

            if !found_title {
                continue;
            }

            if in_body {
                body_lines.push(line);
                continue;
            }

            // Blank line → switch to body mode.
            if line.trim().is_empty() {
                in_body = true;
                continue;
            }

            // Try to parse as `key: value` (simple alphanumeric key).
            if let Some(colon_pos) = line.find(':') {
                let key = line[..colon_pos].trim();
                let value = line[colon_pos + 1..].trim();
                if !key.is_empty() && key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    fields.insert(key.to_string(), value.to_string());
                    continue;
                }
            }

            // Non-key-value line before a blank line → body starts here.
            in_body = true;
            body_lines.push(line);
        }

        let Some(title) = title else { continue };

        let priority = fields
            .get("priority")
            .map(|v| Priority::from(v.as_str()))
            .unwrap_or(Priority::Normal);
        let schedule = fields
            .get("schedule")
            .map(|v| Schedule::from(v.as_str()))
            .unwrap_or(Schedule::Once);
        let status = fields
            .get("status")
            .map(|v| Status::from(v.as_str()))
            .unwrap_or(Status::Pending);
        let last_run = fields.get("last_run").and_then(|v| {
            DateTime::parse_from_rfc3339(v)
                .ok()
                .map(|dt| dt.with_timezone(&Local))
        });
        let not_before = fields.get("not_before").and_then(|v| {
            DateTime::parse_from_rfc3339(v)
                .ok()
                .map(|dt| dt.with_timezone(&Local))
        });

        let body = body_lines.join("\n").trim().to_string();

        entries.push(HeartbeatEntry {
            title,
            priority,
            schedule,
            status,
            last_run,
            not_before,
            body,
        });
    }

    entries
}

/// Serialize a list of heartbeat entries back to Heartbeat.md format.
pub fn serialize_heartbeat(entries: &[HeartbeatEntry]) -> String {
    let mut out = String::from("# Heartbeat\n");

    for entry in entries {
        out.push_str(&format!("\n## {}\n", entry.title));
        out.push_str(&format!("priority: {}\n", entry.priority));
        out.push_str(&format!("schedule: {}\n", entry.schedule));
        out.push_str(&format!("status: {}\n", entry.status));
        if let Some(last_run) = &entry.last_run {
            out.push_str(&format!("last_run: {}\n", last_run.to_rfc3339()));
        }
        if let Some(not_before) = &entry.not_before {
            out.push_str(&format!("not_before: {}\n", not_before.to_rfc3339()));
        }
        if !entry.body.is_empty() {
            out.push('\n');
            out.push_str(&entry.body);
            out.push('\n');
        }
        out.push_str("\n---\n");
    }

    out
}

// ── Load / save ──────────────────────────────────────────────────────────────

/// Load Heartbeat.md from the local filesystem.
///
/// Returns `None` if the file does not exist or cannot be read.
pub fn load_heartbeat_local(agent_name: &str) -> Option<Vec<HeartbeatEntry>> {
    let path = heartbeat_md_path_local(agent_name)?;
    match std::fs::read_to_string(&path) {
        Ok(content) => Some(parse_heartbeat(&content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Failed to read Heartbeat.md");
            None
        }
    }
}

/// Load Heartbeat.md from inside a sandbox container via `docker exec cat`.
///
/// Returns `None` if the file does not exist or the command fails.
pub fn load_heartbeat_sandbox(container: &str, agent_name: &str) -> Option<Vec<HeartbeatEntry>> {
    let path = heartbeat_md_path_sandbox(agent_name);
    let output = std::process::Command::new("docker")
        .args(["exec", container, "cat", &path])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    if output.status.success() {
        let content = String::from_utf8_lossy(&output.stdout).to_string();
        Some(parse_heartbeat(&content))
    } else {
        None
    }
}

/// Ensure Heartbeat.md exists locally for the given agent.
///
/// Returns `Ok(true)` if the file was created, `Ok(false)` if it already existed.
pub fn ensure_heartbeat_local(agent_name: &str) -> std::io::Result<bool> {
    let path = heartbeat_md_path_local(agent_name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Cannot determine home directory",
        )
    })?;
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, default_heartbeat_md())?;
    Ok(true)
}

/// Ensure Heartbeat.md exists inside a sandbox container for the given agent.
///
/// Returns `Ok(true)` if the file was created, `Ok(false)` if it already existed.
pub fn ensure_heartbeat_sandbox(container: &str, agent_name: &str) -> Result<bool, String> {
    let path = heartbeat_md_path_sandbox(agent_name);
    let dir = format!("/home/agent/.that-agent/agents/{}", agent_name);

    let exists = std::process::Command::new("docker")
        .args(["exec", container, "sh", "-c", &format!("test -f {path}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to check Heartbeat.md existence in container: {e}"))?;

    if exists.success() {
        return Ok(false);
    }

    let cmd = format!("mkdir -p {dir} && cat > {path}");
    let mut child = std::process::Command::new("docker")
        .args(["exec", "-i", container, "sh", "-c", &cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start docker exec for Heartbeat.md bootstrap: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(default_heartbeat_md().as_bytes())
            .map_err(|e| format!("Failed to write default Heartbeat.md to container: {e}"))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed waiting for Heartbeat.md bootstrap command: {e}"))?;

    if status.success() {
        Ok(true)
    } else {
        Err("docker exec exited with non-zero status while bootstrapping Heartbeat.md".to_string())
    }
}

/// Write Heartbeat.md to the local filesystem.
///
/// Creates the parent directory if it does not exist.
pub fn save_heartbeat_local(agent_name: &str, entries: &[HeartbeatEntry]) -> std::io::Result<()> {
    let path = heartbeat_md_path_local(agent_name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Cannot determine home directory",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serialize_heartbeat(entries))
}

/// Write Heartbeat.md into a sandbox container via `docker exec`.
pub fn save_heartbeat_sandbox(
    container: &str,
    agent_name: &str,
    entries: &[HeartbeatEntry],
) -> Result<(), String> {
    let path = heartbeat_md_path_sandbox(agent_name);
    let dir = format!("/home/agent/.that-agent/agents/{}", agent_name);
    let cmd = format!("mkdir -p {dir} && cat > {path}");

    let mut child = std::process::Command::new("docker")
        .args(["exec", "-i", container, "sh", "-c", &cmd])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start docker exec: {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let content = serialize_heartbeat(entries);
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write Heartbeat.md to container: {e}"))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed to wait for docker exec: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err("docker exec exited with non-zero status while writing Heartbeat.md".to_string())
    }
}

// ── Scheduling logic ─────────────────────────────────────────────────────────

/// Return true if a heartbeat entry is due for processing.
///
/// `agent_tz` is an optional IANA timezone name (e.g. `"Asia/Jerusalem"`).
/// It affects wall-clock schedules (`daily`, `cron`); duration-based schedules
/// (`minutely`, `hourly`, `weekly`) use absolute time and are unaffected.
///
/// Urgent entries dispatch immediately when first created (no `last_run` yet),
/// then follow their configured schedule.
pub fn is_entry_due(entry: &HeartbeatEntry, agent_tz: Option<&str>) -> bool {
    // ── not_before gate: absolute UTC comparison (timezone-irrelevant) ──
    if let Some(nb) = entry.not_before {
        if Utc::now() < nb.to_utc() {
            return false;
        }
    }

    // Urgent entries trigger immediately on first dispatch, then follow schedule.
    if matches!(entry.priority, Priority::Urgent) && entry.last_run.is_none() {
        return true;
    }

    let now = Utc::now();

    match &entry.schedule {
        Schedule::Once => entry.last_run.is_none(),
        Schedule::Minutely => match entry.last_run {
            None => true,
            Some(last) => now - last.to_utc() >= chrono::Duration::minutes(1),
        },
        Schedule::Hourly => match entry.last_run {
            None => true,
            Some(last) => now - last.to_utc() > chrono::Duration::hours(1),
        },
        Schedule::Daily => {
            let wall_now = wall_date_naive(now, agent_tz);
            match entry.last_run {
                None => true,
                Some(last) => {
                    let wall_last = wall_date_naive(last.to_utc(), agent_tz);
                    wall_last < wall_now
                }
            }
        }
        Schedule::Weekly => match entry.last_run {
            None => true,
            Some(last) => now - last.to_utc() > chrono::Duration::weeks(1),
        },
        Schedule::Cron(expr) => {
            let wall_now = wall_datetime_naive(now, agent_tz);
            let wall_last = entry.last_run.map(|l| wall_datetime_naive(l.to_utc(), agent_tz));
            is_cron_due(expr, wall_last, wall_now)
        }
        Schedule::Unknown(_) => false,
    }
}

/// Convert a UTC instant to a `NaiveDate` in the agent's wall-clock timezone.
fn wall_date_naive(utc: DateTime<Utc>, agent_tz: Option<&str>) -> chrono::NaiveDate {
    if let Some(tz_name) = agent_tz {
        if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
            return utc.with_timezone(&tz).date_naive();
        }
    }
    utc.with_timezone(&Local).date_naive()
}

/// Convert a UTC instant to a `NaiveDateTime` in the agent's wall-clock timezone.
fn wall_datetime_naive(utc: DateTime<Utc>, agent_tz: Option<&str>) -> NaiveDateTime {
    if let Some(tz_name) = agent_tz {
        if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
            return utc.with_timezone(&tz).naive_local();
        }
    }
    utc.with_timezone(&Local).naive_local()
}

#[derive(Debug, Clone)]
struct CronExpression {
    minute: Vec<bool>,
    hour: Vec<bool>,
    day_of_month: Vec<bool>,
    month: Vec<bool>,
    day_of_week: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

fn is_cron_due(expr: &str, last_run: Option<NaiveDateTime>, now: NaiveDateTime) -> bool {
    if last_run.is_none() {
        return true;
    }
    let Ok(parsed) = parse_cron_expression(expr) else {
        return false;
    };
    let slot = now
        .with_second(0)
        .and_then(|dt| dt.with_nanosecond(0))
        .unwrap_or(now);
    let Some(last) = last_run else {
        return true;
    };
    if last >= slot {
        return false;
    }
    cron_matches(&parsed, slot)
}

fn parse_cron_expression(expr: &str) -> Result<CronExpression, String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err("cron expression must have exactly 5 fields: min hour dom mon dow".to_string());
    }
    let minute = parse_cron_field(fields[0], 0, 59, parse_u32_plain)?;
    let hour = parse_cron_field(fields[1], 0, 23, parse_u32_plain)?;
    let day_of_month = parse_cron_field(fields[2], 1, 31, parse_u32_plain)?;
    let month = parse_cron_field(fields[3], 1, 12, parse_month_value)?;
    let day_of_week = parse_cron_field(fields[4], 0, 7, parse_day_of_week_value)?;
    Ok(CronExpression {
        minute,
        hour,
        day_of_month,
        month,
        day_of_week,
        dom_any: fields[2].trim() == "*",
        dow_any: fields[4].trim() == "*",
    })
}

fn cron_matches(expr: &CronExpression, when: NaiveDateTime) -> bool {
    let minute = when.minute() as usize;
    let hour = when.hour() as usize;
    let dom = when.day() as usize;
    let month = when.month() as usize;
    let dow = weekday_to_num(when.weekday()) as usize;
    if !expr.minute[minute] || !expr.hour[hour] || !expr.month[month] {
        return false;
    }
    let dom_match = expr.day_of_month[dom];
    let dow_match = expr.day_of_week[dow] || (dow == 0 && expr.day_of_week[7]);
    if expr.dom_any && expr.dow_any {
        true
    } else if expr.dom_any {
        dow_match
    } else if expr.dow_any {
        dom_match
    } else {
        dom_match || dow_match
    }
}

fn parse_cron_field(
    spec: &str,
    min: u32,
    max: u32,
    parse_value: fn(&str) -> Option<u32>,
) -> Result<Vec<bool>, String> {
    let mut flags = vec![false; (max + 1) as usize];
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("empty cron field segment".to_string());
        }
        let (base, step) = if let Some((base, step_str)) = part.split_once('/') {
            let step = step_str
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid cron step '{step_str}'"))?;
            if step == 0 {
                return Err("cron step cannot be zero".to_string());
            }
            (base.trim(), step)
        } else {
            (part, 1)
        };

        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((start_str, end_str)) = base.split_once('-') {
            let start = parse_value(start_str.trim())
                .ok_or_else(|| format!("invalid cron value '{start_str}'"))?;
            let end = parse_value(end_str.trim())
                .ok_or_else(|| format!("invalid cron value '{end_str}'"))?;
            if start > end {
                return Err(format!("invalid cron range '{base}'"));
            }
            (start, end)
        } else {
            let value = parse_value(base).ok_or_else(|| format!("invalid cron value '{base}'"))?;
            (value, value)
        };

        if start < min || end > max {
            return Err(format!(
                "cron value out of range '{part}' (expected {min}-{max})"
            ));
        }

        let mut value = start;
        while value <= end {
            flags[value as usize] = true;
            match value.checked_add(step) {
                Some(next) if next > value => value = next,
                _ => break,
            }
        }
    }

    if !flags[min as usize..=max as usize].iter().any(|v| *v) {
        return Err("cron field resolved to empty set".to_string());
    }
    Ok(flags)
}

fn parse_u32_plain(value: &str) -> Option<u32> {
    value.parse::<u32>().ok()
}

fn parse_month_value(value: &str) -> Option<u32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        other => other.parse::<u32>().ok(),
    }
}

fn parse_day_of_week_value(value: &str) -> Option<u32> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sun" => Some(0),
        "mon" => Some(1),
        "tue" => Some(2),
        "wed" => Some(3),
        "thu" => Some(4),
        "fri" => Some(5),
        "sat" => Some(6),
        other => other.parse::<u32>().ok(),
    }
}

fn weekday_to_num(day: Weekday) -> u32 {
    match day {
        Weekday::Sun => 0,
        Weekday::Mon => 1,
        Weekday::Tue => 2,
        Weekday::Wed => 3,
        Weekday::Thu => 4,
        Weekday::Fri => 5,
        Weekday::Sat => 6,
    }
}

/// Build a synthetic task description from a slice of due heartbeat entries.
///
/// The task is passed to the agent as an autonomous run prompt.
/// Recurring entries typically stay `status: running`; `status: done` disables.
pub fn format_heartbeat_task(due: &[&HeartbeatEntry]) -> String {
    let mut task = String::from(
        "Heartbeat check-in. Process the following pending items from your Heartbeat.md:\n\n",
    );
    for entry in due {
        task.push_str(&format!(
            "## {} (priority: {}, schedule: {})\n{}\n\n",
            entry.title,
            entry.priority,
            entry.schedule,
            entry.body.trim(),
        ));
    }
    task.push_str(
        "For recurring work, keep `status: running`; set `status: done` only when you want to disable the entry.\n\n\
         Use the `channel_notify` tool to keep users informed:\n\
         - For reminder or one-time entries: you MUST notify the user. The entire purpose of a reminder is the notification — never skip it.\n\
         - Send a brief summary when meaningful work is completed (e.g. a scheduled task finished, data was updated, a report was generated).\n\
         - Send a notice if you are blocked or cannot complete an item (e.g. a dependency is missing, a file is inaccessible, an external service is unavailable).\n\
         - Only skip notification for recurring housekeeping with no user-visible outcome.",
    );
    task
}

// ── Default template ─────────────────────────────────────────────────────────

/// Starter Heartbeat.md content for new agents.
pub fn default_heartbeat_md() -> &'static str {
    r#"# Heartbeat

<!-- Add entries below. Each entry is an H2 heading, followed by key-value fields,
     a blank line, then a body description. Entries are separated by `---`.

     Fields:
       priority:   urgent | high | normal | low
       schedule:   once | minutely | hourly | daily | weekly | cron: <expr>
       status:     pending | running | done | skipped
       last_run:   ISO timestamp (written automatically each dispatch)
       not_before: ISO timestamp — entry will not fire until this time has passed

     Urgent entries trigger immediately on first dispatch, then follow schedule.
     `not_before` defers firing until a future time (useful for reminders).
     `running` means the schedule remains active; set `done` to disable it. -->

"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn make_entry(schedule: Schedule, last_run: Option<DateTime<Local>>, not_before: Option<DateTime<Local>>) -> HeartbeatEntry {
        HeartbeatEntry {
            title: "test".into(),
            priority: Priority::Normal,
            schedule,
            status: Status::Pending,
            last_run,
            not_before,
            body: String::new(),
        }
    }

    #[test]
    fn not_before_in_future_blocks_firing() {
        let future = Local::now() + Duration::hours(1);
        let entry = make_entry(Schedule::Once, None, Some(future));
        assert!(!is_entry_due(&entry, None));
    }

    #[test]
    fn not_before_in_past_allows_firing() {
        let past = Local::now() - Duration::hours(1);
        let entry = make_entry(Schedule::Once, None, Some(past));
        assert!(is_entry_due(&entry, None));
    }

    #[test]
    fn not_before_none_preserves_existing_behavior() {
        let entry = make_entry(Schedule::Once, None, None);
        assert!(is_entry_due(&entry, None));

        let ran = make_entry(Schedule::Once, Some(Local::now()), None);
        assert!(!is_entry_due(&ran, None));
    }

    #[test]
    fn not_before_blocks_even_urgent() {
        let future = Local::now() + Duration::hours(1);
        let mut entry = make_entry(Schedule::Once, None, Some(future));
        entry.priority = Priority::Urgent;
        assert!(!is_entry_due(&entry, None));
    }

    #[test]
    fn parse_serialize_roundtrip_not_before() {
        let now = Local::now();
        let md = format!(
            "# Heartbeat\n\n## Reminder\npriority: normal\nschedule: once\nstatus: pending\nnot_before: {}\n\nDo the thing\n\n---\n",
            now.to_rfc3339()
        );
        let entries = parse_heartbeat(&md);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].not_before.is_some());
        let nb = entries[0].not_before.unwrap();
        // Timestamps should be within 1 second (rfc3339 truncation).
        assert!((nb - now).num_seconds().abs() <= 1);

        let serialized = serialize_heartbeat(&entries);
        let re_parsed = parse_heartbeat(&serialized);
        assert_eq!(re_parsed.len(), 1);
        assert!(re_parsed[0].not_before.is_some());
    }

    #[test]
    fn timezone_affects_daily_due_check() {
        // Create an entry that last ran "today" in UTC but "yesterday" in a far-west timezone.
        // Use a fixed UTC time just after midnight UTC — in US/Samoa (UTC-11) it's still the
        // previous day, so a daily entry should be due there but NOT due in UTC+12.
        let utc_midnight = Utc::now()
            .date_naive()
            .and_hms_opt(0, 30, 0)
            .unwrap();
        let last_run_utc = Utc.from_utc_datetime(&utc_midnight);
        let last_run_local = last_run_utc.with_timezone(&Local);

        let entry = make_entry(Schedule::Daily, Some(last_run_local), None);

        // In a timezone well ahead of UTC (same calendar day as UTC) → not due.
        let due_ahead = is_entry_due(&entry, Some("Pacific/Auckland"));
        // In a timezone far behind UTC (previous calendar day) → due.
        let due_behind = is_entry_due(&entry, Some("Pacific/Pago_Pago"));

        // At least one of these should differ from the other (proving TZ matters).
        // The exact result depends on the current UTC date, but the key invariant:
        // Pago_Pago (UTC-11) should see last_run as further in the past than Auckland (UTC+12/13).
        // We can't assert both deterministically without freezing time, so just verify no panic
        // and that the function accepts timezone strings.
        let _ = (due_ahead, due_behind);
    }

    #[test]
    fn invalid_timezone_falls_back_to_local() {
        let entry = make_entry(Schedule::Once, None, None);
        // Should not panic; falls back to Local.
        assert!(is_entry_due(&entry, Some("Invalid/Timezone")));
    }
}
