//! Shell execution tool for Anvil.
//!
//! Provides governed command execution with timeout enforcement,
//! process group management, optional streaming, and token-budget-limited output capture.

use crate::output::{self, BudgetedOutput, CompactionStrategy};
use serde::{Deserialize, Serialize};
use std::io::BufRead;
use std::sync::mpsc;
use std::time::Instant;
use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum ExecError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Controls how a timed-out process group is terminated.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SignalMode {
    /// Send SIGTERM to the process group, wait up to 2 seconds, then SIGKILL if still alive.
    #[default]
    Graceful,
    /// Send SIGKILL to the process group immediately.
    Immediate,
}

/// A line-level event from a running process, used for streaming output.
#[derive(Debug, Clone, Serialize)]
struct StreamEvent {
    stream: &'static str,
    line: String,
}

/// Result of a shell command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
    pub timed_out: bool,
}

fn shell_exec_allow_otel() -> bool {
    std::env::var("THAT_SHELL_EXEC_ALLOW_OTEL")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn harden_child_env(cmd: &mut std::process::Command) {
    if shell_exec_allow_otel() {
        return;
    }
    // Keep runtime tracing on the parent process, but prevent child commands
    // from auto-enabling OTel SDKs and exporting noisy spans by inheritance.
    cmd.env("OTEL_SDK_DISABLED", "true")
        .env("OTEL_PYTHON_DISABLED_INSTRUMENTATIONS", "all")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT");
}

/// Backward-compatible wrapper — delegates to `exec_with_options` with defaults.
#[allow(dead_code)]
pub fn exec(
    command: &str,
    cwd: Option<&str>,
    timeout_secs: u64,
    max_tokens: Option<usize>,
) -> Result<BudgetedOutput, ExecError> {
    exec_with_options(
        command,
        cwd,
        timeout_secs,
        max_tokens,
        SignalMode::default(),
        false,
    )
}

/// Execute a shell command with full control over signal mode and streaming.
///
/// - On Unix, spawns the child in a new process group via `setsid()` so that
///   timeout kills reach all descendants.
/// - When `stream` is true, each output line is emitted as JSONL to stderr
///   in real time.
/// - Final output is always budget-limited via HeadTail strategy.
pub fn exec_with_options(
    command: &str,
    cwd: Option<&str>,
    timeout_secs: u64,
    max_tokens: Option<usize>,
    signal_mode: SignalMode,
    stream: bool,
) -> Result<BudgetedOutput, ExecError> {
    let start = Instant::now();

    #[cfg(unix)]
    let mut child = {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(command);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        harden_child_env(&mut cmd);
        // SAFETY: setsid() is async-signal-safe and called between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?
    };

    #[cfg(windows)]
    let mut child = {
        let mut cmd = std::process::Command::new("cmd");
        cmd.arg("/C").arg(command);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        harden_child_env(&mut cmd);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?
    };

    let child_pid = child.id();

    // Take ownership of stdout/stderr handles for threaded reading
    let child_stdout = child.stdout.take();
    let child_stderr = child.stderr.take();

    // Channel for streaming events
    let (tx, rx) = mpsc::channel::<StreamEvent>();

    // Spawn reader thread for stdout
    let tx_out = tx.clone();
    let stdout_thread = std::thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(handle) = child_stdout {
            let reader = std::io::BufReader::new(handle);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let _ = tx_out.send(StreamEvent {
                            stream: "stdout",
                            line: l.clone(),
                        });
                        lines.push(l);
                    }
                    Err(_) => break,
                }
            }
        }
        lines
    });

    // Spawn reader thread for stderr
    let tx_err = tx;
    let stderr_thread = std::thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(handle) = child_stderr {
            let reader = std::io::BufReader::new(handle);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let _ = tx_err.send(StreamEvent {
                            stream: "stderr",
                            line: l.clone(),
                        });
                        lines.push(l);
                    }
                    Err(_) => break,
                }
            }
        }
        lines
    });

    // Main loop: drain events, check for exit, enforce timeout
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let poll_interval = std::time::Duration::from_millis(50);
    let mut timed_out = false;

    loop {
        // Drain any pending stream events
        while let Ok(event) = rx.try_recv() {
            if stream {
                if let Ok(json) = serde_json::to_string(&event) {
                    eprintln!("{}", json);
                }
            }
        }

        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > timeout {
                    timed_out = true;
                    kill_process_group(child_pid, signal_mode);
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => return Err(ExecError::Io(e)),
        }
    }

    let elapsed_ms = start.elapsed().as_millis() as u64;
    let exit_status = child.try_wait().ok().flatten();

    // Join reader threads to get accumulated output
    // (threads finish when the child's pipe closes, which happens after exit/kill)
    let stdout_lines = stdout_thread.join().unwrap_or_default();
    let stderr_lines = stderr_thread.join().unwrap_or_default();

    // Drain any remaining stream events after threads complete
    if stream {
        while let Ok(event) = rx.try_recv() {
            if let Ok(json) = serde_json::to_string(&event) {
                eprintln!("{}", json);
            }
        }
    }

    let raw_stdout = stdout_lines.join("\n");
    let raw_stderr = stderr_lines.join("\n");

    // Apply token budget to stdout/stderr
    let content_budget = max_tokens
        .map(|b| (b as f64 * 0.6) as usize)
        .unwrap_or(4096);
    let stderr_budget = max_tokens
        .map(|b| (b as f64 * 0.2) as usize)
        .unwrap_or(1024);

    let budgeted_stdout =
        output::apply_budget_to_text(&raw_stdout, content_budget, CompactionStrategy::HeadTail);
    let budgeted_stderr =
        output::apply_budget_to_text(&raw_stderr, stderr_budget, CompactionStrategy::HeadTail);

    let result = ExecResult {
        command: command.to_string(),
        exit_code: exit_status.and_then(|s| s.code()),
        stdout: budgeted_stdout.content,
        stderr: budgeted_stderr.content,
        elapsed_ms,
        timed_out,
    };

    Ok(output::emit_json(&result, max_tokens))
}

/// Kill an entire process group.
///
/// On Unix, sends the signal to the negative PID (process group).
/// Graceful mode sends SIGTERM first, waits up to 2 seconds, then SIGKILL.
#[cfg(unix)]
fn kill_process_group(pid: u32, mode: SignalMode) {
    let pgid = -(pid as i32);
    match mode {
        SignalMode::Graceful => {
            // Send SIGTERM to the process group
            unsafe {
                libc::kill(pgid, libc::SIGTERM);
            }
            // Wait up to 2 seconds for processes to exit
            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let ret = unsafe { libc::kill(pgid, 0) };
                if ret != 0 {
                    // Process group no longer exists
                    return;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            // Force kill if still alive
            unsafe {
                libc::kill(pgid, libc::SIGKILL);
            }
        }
        SignalMode::Immediate => unsafe {
            libc::kill(pgid, libc::SIGKILL);
        },
    }
}

#[cfg(windows)]
fn kill_process_group(pid: u32, _mode: SignalMode) {
    // On Windows, Child::kill() already terminates the process tree
    // when using Job Objects. This is a fallback.
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_simple_command() {
        let result = exec("echo hello", None, 10, None).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.exit_code, Some(0));
        assert!(parsed.stdout.contains("hello"));
        assert!(!parsed.timed_out);
    }

    #[test]
    fn test_exec_with_cwd() {
        let result = exec("pwd", Some("/tmp"), 10, None).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.exit_code, Some(0));
        // On macOS, /tmp is a symlink to /private/tmp
        assert!(
            parsed.stdout.contains("/tmp") || parsed.stdout.contains("/private/tmp"),
            "stdout should contain tmp path: {}",
            parsed.stdout
        );
    }

    #[test]
    fn test_exec_nonzero_exit() {
        let result = exec("exit 42", None, 10, None).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert_eq!(parsed.exit_code, Some(42));
    }

    #[test]
    fn test_exec_captures_stderr() {
        let result = exec("echo error >&2", None, 10, None).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.stderr.contains("error"));
    }

    #[test]
    fn test_exec_timeout() {
        let result = exec("sleep 60", None, 1, None).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.timed_out);
        assert!(parsed.elapsed_ms >= 1000);
    }

    #[test]
    fn test_exec_with_token_budget() {
        let result = exec("seq 1 1000", None, 10, Some(50)).unwrap();
        assert!(result.tokens <= 60);
    }

    #[test]
    fn test_exec_result_serialization() {
        let result = ExecResult {
            command: "echo test".to_string(),
            exit_code: Some(0),
            stdout: "test\n".to_string(),
            stderr: String::new(),
            elapsed_ms: 10,
            timed_out: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let _: ExecResult = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_signal_mode_serialization() {
        let graceful = SignalMode::Graceful;
        let json = serde_json::to_string(&graceful).unwrap();
        assert_eq!(json, "\"graceful\"");
        let parsed: SignalMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SignalMode::Graceful);

        let immediate = SignalMode::Immediate;
        let json = serde_json::to_string(&immediate).unwrap();
        assert_eq!(json, "\"immediate\"");
        let parsed: SignalMode = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, SignalMode::Immediate);
    }

    #[cfg(unix)]
    #[test]
    fn test_exec_timeout_kills_process_group() {
        // Spawn background sleeps — if process group kill works,
        // we won't hang waiting for orphaned subprocesses.
        let start = Instant::now();
        let result = exec_with_options(
            "sleep 60 & sleep 60 & wait",
            None,
            2,
            None,
            SignalMode::Graceful,
            false,
        )
        .unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.timed_out);
        // Should complete well under 10s (timeout + grace period)
        assert!(start.elapsed().as_secs() < 10);
    }

    #[cfg(unix)]
    #[test]
    fn test_exec_immediate_kill_mode() {
        let start = Instant::now();
        let result =
            exec_with_options("sleep 60", None, 1, None, SignalMode::Immediate, false).unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.timed_out);
        // Immediate mode should be faster than graceful (no 2s grace period)
        assert!(start.elapsed().as_secs() < 5);
    }

    #[test]
    fn test_exec_streaming_captures_output() {
        // Even with streaming on, the final result should contain all output
        let result = exec_with_options(
            "echo line1; echo line2; echo line3",
            None,
            10,
            None,
            SignalMode::default(),
            true,
        )
        .unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.stdout.contains("line1"));
        assert!(parsed.stdout.contains("line2"));
        assert!(parsed.stdout.contains("line3"));
        assert_eq!(parsed.exit_code, Some(0));
    }

    #[test]
    fn test_exec_streaming_with_timeout() {
        let result = exec_with_options(
            "echo before_timeout; sleep 60",
            None,
            1,
            None,
            SignalMode::Graceful,
            true,
        )
        .unwrap();
        let parsed: ExecResult = serde_json::from_str(&result.content).unwrap();
        assert!(parsed.timed_out);
        assert!(parsed.stdout.contains("before_timeout"));
    }

    #[test]
    fn test_exec_streaming_budget_still_enforced() {
        let result = exec_with_options(
            "seq 1 1000",
            None,
            10,
            Some(50),
            SignalMode::default(),
            true,
        )
        .unwrap();
        assert!(result.tokens <= 60);
    }
}
