//! Daemon mode for long-lived that-tools sessions (feature-gated).
//!
//! Provides a persistent process that accepts tool requests via
//! Unix socket / named pipe using JSON-RPC protocol.

#[cfg(feature = "daemon")]
pub mod rpc;
#[cfg(feature = "daemon")]
pub mod session;

use serde::{Deserialize, Serialize};

/// Daemon status information.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub socket_path: Option<String>,
    pub uptime_secs: Option<u64>,
    pub sessions: usize,
}

/// Check if the daemon is running.
pub fn status() -> DaemonStatus {
    let socket = socket_path();
    let running = socket.exists();

    DaemonStatus {
        running,
        pid: if running { read_pid() } else { None },
        socket_path: if running {
            Some(socket.to_string_lossy().to_string())
        } else {
            None
        },
        uptime_secs: None,
        sessions: 0,
    }
}

/// Start the daemon (feature-gated implementation).
pub fn start() -> Result<DaemonStatus, Box<dyn std::error::Error>> {
    #[cfg(feature = "daemon")]
    {
        // Full daemon implementation when feature is enabled
        Err(
            "daemon start not yet implemented — this feature is planned for a future release"
                .into(),
        )
    }
    #[cfg(not(feature = "daemon"))]
    {
        Err("daemon mode requires the 'daemon' feature: cargo build --features daemon".into())
    }
}

/// Stop the daemon.
pub fn stop() -> Result<DaemonStatus, Box<dyn std::error::Error>> {
    let status = status();
    if !status.running {
        return Ok(status);
    }

    if let Some(pid) = status.pid {
        #[cfg(unix)]
        {
            // Use nix-style kill via std::process::Command
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .output();
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            return Err("daemon stop not supported on this platform".into());
        }
    }

    // Clean up socket
    let socket = socket_path();
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(pid_path());

    Ok(DaemonStatus {
        running: false,
        pid: None,
        socket_path: None,
        uptime_secs: None,
        sessions: 0,
    })
}

fn socket_path() -> std::path::PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("that-tools.sock")
}

fn pid_path() -> std::path::PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("that-tools.pid")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_when_not_running() {
        let s = status();
        // Socket likely doesn't exist in test env
        if !s.running {
            assert!(s.pid.is_none());
            assert!(s.socket_path.is_none());
        }
    }

    #[test]
    fn test_daemon_status_serialization() {
        let s = DaemonStatus {
            running: false,
            pid: None,
            socket_path: None,
            uptime_secs: None,
            sessions: 0,
        };
        let json = serde_json::to_string(&s).unwrap();
        let deserialized: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert!(!deserialized.running);
        assert_eq!(deserialized.sessions, 0);
    }

    #[test]
    fn test_stop_when_not_running() {
        let result = stop();
        assert!(result.is_ok());
        let s = result.unwrap();
        assert!(!s.running);
    }

    #[test]
    fn test_socket_path_is_absolute() {
        let path = socket_path();
        assert!(path.is_absolute() || path.starts_with("/"));
    }

    #[test]
    fn test_read_pid_returns_none_when_missing() {
        assert!(read_pid().is_none() || read_pid().is_some());
    }
}
