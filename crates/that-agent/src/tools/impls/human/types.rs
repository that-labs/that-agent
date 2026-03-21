//! Types for the human-in-the-loop system.

use serde::{Deserialize, Serialize};

/// The type of human interaction requested.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ContractType {
    /// Ask for free-form input.
    Ask,
    /// Request yes/no approval.
    Approve,
    /// Request confirmation of an action.
    Confirm,
    /// Escalate a decision to a human.
    Escalate,
    /// Inform (no response needed).
    Inform,
}

/// A request for human approval or input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub contract: ContractType,
    pub message: String,
    pub timeout_secs: Option<u64>,
    pub timestamp: String,
    pub context: Option<serde_json::Value>,
}

impl ApprovalRequest {
    pub fn new(contract: ContractType, message: String, timeout_secs: Option<u64>) -> Self {
        use std::time::SystemTime;
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            contract,
            message,
            timeout_secs,
            timestamp: format!("{}", secs),
            context: None,
        }
    }
}

/// A response to an approval request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub approved: bool,
    pub response: Option<String>,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approval_request_creation() {
        let req = ApprovalRequest::new(ContractType::Ask, "test".to_string(), Some(30));
        assert!(!req.id.is_empty());
        assert_eq!(req.contract, ContractType::Ask);
        assert_eq!(req.message, "test");
    }

    #[test]
    fn test_contract_type_serialization() {
        let json = serde_json::to_string(&ContractType::Approve).unwrap();
        assert_eq!(json, "\"approve\"");
        let _: ContractType = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_approval_response_serialization() {
        let resp = ApprovalResponse {
            request_id: "test-id".to_string(),
            approved: true,
            response: Some("ok".to_string()),
            timestamp: "123".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let _: ApprovalResponse = serde_json::from_str(&json).unwrap();
    }
}
