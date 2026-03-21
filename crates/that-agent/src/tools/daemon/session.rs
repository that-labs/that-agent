//! Session management for daemon mode.
#![allow(dead_code)]

#[cfg(feature = "daemon")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "daemon")]
#[derive(Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: u64,
    pub last_active: u64,
    pub tool_calls: u64,
}

#[cfg(feature = "daemon")]
impl Session {
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            created_at: now,
            last_active: now,
            tool_calls: 0,
        }
    }

    pub fn touch(&mut self) {
        self.last_active = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.tool_calls += 1;
    }
}
