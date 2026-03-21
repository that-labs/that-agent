//! Human-in-the-loop system for that-tools.
//!
//! Dual-mode: interactive terminal prompts via dialoguer,
//! or file-based JSON queue for headless operation.

pub mod prompt;
pub mod queue;
pub mod types;

pub use types::{ApprovalRequest, ApprovalResponse, ContractType};

use serde::{Deserialize, Serialize};

/// Result of an ask operation.
#[derive(Debug, Serialize, Deserialize)]
pub struct AskResult {
    pub response: String,
    pub approved: bool,
    pub method: String,
    pub elapsed_ms: u64,
}

/// Result of checking pending approvals.
#[derive(Debug, Serialize, Deserialize)]
pub struct PendingResult {
    pub pending: Vec<ApprovalRequest>,
    pub count: usize,
}

/// Ask a human for input.
///
/// Uses terminal if available, falls back to file queue.
pub fn ask(
    message: &str,
    timeout_secs: Option<u64>,
) -> Result<AskResult, Box<dyn std::error::Error>> {
    let start = std::time::Instant::now();

    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // Interactive mode
        let response = prompt::ask_terminal(message, timeout_secs)?;
        Ok(AskResult {
            approved: response.to_lowercase() != "no"
                && response.to_lowercase() != "n"
                && response.to_lowercase() != "deny",
            response,
            method: "terminal".to_string(),
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    } else {
        // Headless mode — use file queue
        let request = ApprovalRequest::new(ContractType::Ask, message.to_string(), timeout_secs);
        let queue = queue::ApprovalQueue::default_path()?;
        queue.submit(&request)?;

        // Poll for response
        let timeout = std::time::Duration::from_secs(timeout_secs.unwrap_or(300));
        let poll_interval = std::time::Duration::from_millis(500);
        let deadline = std::time::Instant::now() + timeout;

        loop {
            if std::time::Instant::now() > deadline {
                return Ok(AskResult {
                    response: "timeout".to_string(),
                    approved: false,
                    method: "queue_timeout".to_string(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }

            if let Some(resp) = queue.check_response(&request.id)? {
                return Ok(AskResult {
                    approved: resp.approved,
                    response: resp.response.unwrap_or_default(),
                    method: "queue".to_string(),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }

            std::thread::sleep(poll_interval);
        }
    }
}

/// Submit an approval request (used by tools that need approval).
#[allow(dead_code)]
pub fn request_approval(
    _contract: ContractType,
    message: &str,
    timeout_secs: Option<u64>,
) -> Result<AskResult, Box<dyn std::error::Error>> {
    ask(message, timeout_secs)
}

/// Approve a pending request by ID.
pub fn approve(
    request_id: &str,
    response: Option<&str>,
) -> Result<ApprovalResponse, Box<dyn std::error::Error>> {
    let queue = queue::ApprovalQueue::default_path()?;
    let resp = ApprovalResponse {
        request_id: request_id.to_string(),
        approved: true,
        response: response.map(String::from),
        timestamp: now_timestamp(),
    };
    queue.respond(&resp)?;
    Ok(resp)
}

/// Confirm (approve) a pending request.
pub fn confirm(request_id: &str) -> Result<ApprovalResponse, Box<dyn std::error::Error>> {
    approve(request_id, Some("confirmed"))
}

/// List pending approval requests.
pub fn pending() -> Result<PendingResult, Box<dyn std::error::Error>> {
    let queue = queue::ApprovalQueue::default_path()?;
    let items = queue.list_pending()?;
    let count = items.len();
    Ok(PendingResult {
        pending: items,
        count,
    })
}

fn now_timestamp() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{}", secs)
}
