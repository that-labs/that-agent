//! File-based approval queue for headless operation.
//!
//! Writes JSON files to a directory for pending requests.
//! Another process can read and respond to these files.

use super::types::{ApprovalRequest, ApprovalResponse};
use std::path::PathBuf;

/// Sanitize an ID to prevent path traversal attacks.
/// Only allows alphanumeric characters, hyphens, and underscores.
fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// File-based approval queue.
pub struct ApprovalQueue {
    dir: PathBuf,
}

impl ApprovalQueue {
    /// Create a queue at the given directory.
    pub fn new(dir: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Create a queue at the default path.
    pub fn default_path() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = dirs::data_local_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("that-tools")
            .join("pending_approvals");
        Self::new(dir)
    }

    /// Submit a new approval request.
    pub fn submit(&self, request: &ApprovalRequest) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.request_path(&request.id);
        let json = serde_json::to_string_pretty(request)?;
        std::fs::write(&path, json)?;
        tracing::debug!("approval request written to {}", path.display());
        Ok(())
    }

    /// Check for a response to a request.
    pub fn check_response(
        &self,
        request_id: &str,
    ) -> Result<Option<ApprovalResponse>, Box<dyn std::error::Error>> {
        let path = self.response_path(request_id);
        if path.exists() {
            let json = std::fs::read_to_string(&path)?;
            let response: ApprovalResponse = serde_json::from_str(&json)?;
            // Clean up files
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(self.request_path(request_id));
            Ok(Some(response))
        } else {
            Ok(None)
        }
    }

    /// Write a response to a pending request.
    pub fn respond(&self, response: &ApprovalResponse) -> Result<(), Box<dyn std::error::Error>> {
        let path = self.response_path(&response.request_id);
        let json = serde_json::to_string_pretty(response)?;
        std::fs::write(&path, json)?;
        tracing::debug!("approval response written to {}", path.display());
        Ok(())
    }

    /// List all pending requests.
    pub fn list_pending(&self) -> Result<Vec<ApprovalRequest>, Box<dyn std::error::Error>> {
        let mut requests = Vec::new();
        if !self.dir.exists() {
            return Ok(requests);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "request") {
                if let Ok(json) = std::fs::read_to_string(&path) {
                    if let Ok(req) = serde_json::from_str::<ApprovalRequest>(&json) {
                        requests.push(req);
                    }
                }
            }
        }
        requests.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        Ok(requests)
    }

    fn request_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.request", sanitize_id(id)))
    }

    fn response_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.response", sanitize_id(id)))
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::ContractType;
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_submit_and_list_pending() {
        let tmp = TempDir::new().unwrap();
        let queue = ApprovalQueue::new(tmp.path().to_path_buf()).unwrap();

        let req = ApprovalRequest::new(ContractType::Ask, "test question".to_string(), Some(30));
        queue.submit(&req).unwrap();

        let pending = queue.list_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].message, "test question");
    }

    #[test]
    fn test_submit_and_respond() {
        let tmp = TempDir::new().unwrap();
        let queue = ApprovalQueue::new(tmp.path().to_path_buf()).unwrap();

        let req = ApprovalRequest::new(ContractType::Approve, "approve?".to_string(), None);
        let req_id = req.id.clone();
        queue.submit(&req).unwrap();

        let response = ApprovalResponse {
            request_id: req_id.clone(),
            approved: true,
            response: Some("yes".to_string()),
            timestamp: "123".to_string(),
        };
        queue.respond(&response).unwrap();

        let result = queue.check_response(&req_id).unwrap();
        assert!(result.is_some());
        assert!(result.unwrap().approved);
    }

    #[test]
    fn test_no_response_returns_none() {
        let tmp = TempDir::new().unwrap();
        let queue = ApprovalQueue::new(tmp.path().to_path_buf()).unwrap();
        let result = queue.check_response("nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_sanitize_id_blocks_traversal() {
        assert_eq!(super::sanitize_id("../../../etc/passwd"), "etcpasswd");
        assert_eq!(super::sanitize_id("normal-id-123"), "normal-id-123");
        assert_eq!(
            super::sanitize_id("id_with_underscores"),
            "id_with_underscores"
        );
        assert_eq!(super::sanitize_id("a/b\\c"), "abc");
    }

    #[test]
    fn test_empty_queue() {
        let tmp = TempDir::new().unwrap();
        let queue = ApprovalQueue::new(tmp.path().to_path_buf()).unwrap();
        let pending = queue.list_pending().unwrap();
        assert!(pending.is_empty());
    }
}
