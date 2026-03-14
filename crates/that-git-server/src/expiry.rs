use std::sync::Arc;
use tracing::{info, warn};

use crate::state::AppState;

/// Spawn a background task that checks repo activity hourly and deletes idle repos.
pub fn spawn_expiry_task(state: Arc<AppState>) {
    if state.expiry_hours == 0 {
        return; // disabled
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            interval.tick().await;
            if let Err(e) = expire_repos(&state).await {
                warn!("expiry: {e}");
            }
        }
    });
}

async fn expire_repos(state: &AppState) -> Result<(), String> {
    let mut entries = tokio::fs::read_dir(&state.repo_root)
        .await
        .map_err(|e| format!("readdir: {e}"))?;

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(state.expiry_hours as i64);

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".git") {
            continue;
        }

        // Use filesystem mtime as ground truth (survives pod restarts)
        let mtime = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from);

        if let Some(t) = mtime {
            if t < cutoff {
                info!("expiring idle repo: {name}");
                let _ = tokio::fs::remove_dir_all(entry.path()).await;
            }
        }
    }
    Ok(())
}
