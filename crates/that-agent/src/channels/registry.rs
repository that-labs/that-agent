use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEntry {
    pub id: String,
    pub callback_url: String,
    pub capabilities: Vec<String>,
    pub registered_at: String,
}

pub struct DynamicChannelRegistry {
    path: PathBuf,
}

impl DynamicChannelRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Upsert a channel entry by id.
    pub fn register(&self, entry: ChannelEntry) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.id != entry.id);
        entries.push(entry);
        self.save(&entries)
    }

    /// Remove a channel entry by id.
    pub fn unregister(&self, id: &str) -> Result<()> {
        let mut entries = self.load()?;
        entries.retain(|e| e.id != id);
        self.save(&entries)
    }

    /// List all registered channel entries.
    pub fn list(&self) -> Result<Vec<ChannelEntry>> {
        self.load()
    }

    fn load(&self) -> Result<Vec<ChannelEntry>> {
        match std::fs::read_to_string(&self.path) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, entries: &[ChannelEntry]) -> Result<()> {
        crate::channels::atomic_write_json(&self.path, entries)
    }
}

/// Produce an RFC 3339-ish UTC timestamp from SystemTime.
pub fn now_rfc3339() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Simple conversion: days/hours/mins/secs from epoch
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // Convert days since epoch to y-m-d
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d)
}
