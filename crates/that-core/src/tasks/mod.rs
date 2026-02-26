//! Task management — folder-based hierarchical work tracking for autonomous agents.
//!
//! Provides path helpers and a lightweight status scanner. The agent navigates
//! task files directly using its filesystem tools — this module never parses
//! content deeply; it only counts statuses and resolves paths.

use std::path::PathBuf;

// ── Path helpers ─────────────────────────────────────────────────────────────

/// Return the tasks directory for a local agent: `~/.that-agent/agents/<name>/tasks/`.
pub fn tasks_dir_local(agent_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("tasks")
    })
}

/// Return the Tasks.md index path for a local agent: `~/.that-agent/agents/<name>/Tasks.md`.
pub fn tasks_index_path_local(agent_name: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".that-agent")
            .join("agents")
            .join(agent_name)
            .join("Tasks.md")
    })
}

/// Return the tasks directory path inside a sandbox container.
pub fn tasks_dir_sandbox(agent_name: &str) -> String {
    format!("/home/agent/.that-agent/agents/{}/tasks", agent_name)
}

/// Return the Tasks.md index path inside a sandbox container.
pub fn tasks_index_path_sandbox(agent_name: &str) -> String {
    format!("/home/agent/.that-agent/agents/{}/Tasks.md", agent_name)
}

// ── Status summary ───────────────────────────────────────────────────────────

/// A lightweight count of task statuses across the tasks directory.
pub struct TasksSummary {
    pub in_progress: usize,
    pub pending: usize,
    pub done: usize,
}

/// Scan the local tasks directory and count statuses by reading `**Status**:` lines.
///
/// Only scans `epic.md`, `story.md`, and `task-*.md` files. Returns `None` if the
/// tasks directory does not exist or cannot be read.
pub fn tasks_summary_local(agent_name: &str) -> Option<TasksSummary> {
    let dir = tasks_dir_local(agent_name)?;
    if !dir.exists() {
        return None;
    }

    let mut summary = TasksSummary {
        in_progress: 0,
        pending: 0,
        done: 0,
    };

    scan_dir_for_statuses(&dir, &mut summary);
    Some(summary)
}

/// Recursively scan a directory for task/epic/story markdown files and tally statuses.
fn scan_dir_for_statuses(dir: &std::path::Path, summary: &mut TasksSummary) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_for_statuses(&path, summary);
            continue;
        }

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_lowercase();

        // Only inspect recognised task files.
        let is_task_file = name == "epic.md"
            || name == "story.md"
            || (name.starts_with("task-") && name.ends_with(".md"));

        if !is_task_file {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        for line in content.lines() {
            // Match `**Status**: <value>` (case-insensitive on the value).
            if let Some(rest) = line.strip_prefix("**Status**:") {
                let value = rest.trim().to_lowercase();
                match value.as_str() {
                    "in-progress" | "in_progress" | "inprogress" | "active" => {
                        summary.in_progress += 1;
                    }
                    "todo" | "pending" | "open" | "backlog" => {
                        summary.pending += 1;
                    }
                    "done" | "complete" | "completed" | "closed" => {
                        summary.done += 1;
                    }
                    _ => {}
                }
                break; // One status line per file is enough.
            }
        }
    }
}

// ── Default template ─────────────────────────────────────────────────────────

/// Starter Tasks.md content for new agents.
pub fn default_tasks_index() -> &'static str {
    r#"# Tasks

Index of all work. Each epic lives in its own directory under `tasks/`.
Use the task-manager skill for instructions on creating and managing tasks.

## Active Epics

(none yet)

## Summary

0 epics · 0 stories · 0 tasks
"#
}
