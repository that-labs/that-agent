use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderEntry {
    pub id: String,
    pub transport: String,
    pub base_url: String,
    pub api_key_env: String,
    #[serde(default)]
    pub models: Vec<String>,
    pub registered_at: String,
}

pub struct DynamicProviderRegistry {
    path: PathBuf,
}

impl DynamicProviderRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_default_path() -> Option<Self> {
        default_registry_path().map(Self::new)
    }

    pub fn register(&self, mut entry: ProviderEntry) -> Result<()> {
        entry.id =
            normalize_provider_id(&entry.id).ok_or_else(|| anyhow!("invalid provider id"))?;
        if entry.transport.trim().is_empty() {
            return Err(anyhow!("provider transport is required"));
        }
        if entry.base_url.trim().is_empty() {
            return Err(anyhow!("provider base_url is required"));
        }
        if entry.api_key_env.trim().is_empty() {
            return Err(anyhow!("provider api_key_env is required"));
        }
        entry.models.retain(|model| !model.trim().is_empty());
        let mut entries = self.list()?;
        entries.retain(|existing| existing.id != entry.id);
        entries.push(entry);
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        self.save(&entries)
    }

    pub fn unregister(&self, id: &str) -> Result<()> {
        let Some(id) = normalize_provider_id(id) else {
            return Err(anyhow!("invalid provider id"));
        };
        let mut entries = self.list()?;
        entries.retain(|entry| entry.id != id);
        self.save(&entries)
    }

    pub fn list(&self) -> Result<Vec<ProviderEntry>> {
        match std::fs::read_to_string(&self.path) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn get(&self, id: &str) -> Result<Option<ProviderEntry>> {
        let Some(id) = normalize_provider_id(id) else {
            return Ok(None);
        };
        Ok(self.list()?.into_iter().find(|entry| entry.id == id))
    }

    fn save(&self, entries: &[ProviderEntry]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::channels::atomic_write_json(&self.path, entries)
    }
}

pub fn default_registry_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".that-agent")
            .join("state")
            .join("providers.json")
    })
}

pub fn load_registered_providers() -> Vec<ProviderEntry> {
    DynamicProviderRegistry::from_default_path()
        .and_then(|registry| registry.list().ok())
        .unwrap_or_default()
}

pub fn find_registered_provider(id: &str) -> Option<ProviderEntry> {
    DynamicProviderRegistry::from_default_path()
        .and_then(|registry| registry.get(id).ok())
        .flatten()
}

pub fn normalize_provider_id(id: &str) -> Option<String> {
    let normalized = id.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || !normalized
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
    {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::{DynamicProviderRegistry, ProviderEntry};

    #[test]
    fn registry_round_trip_preserves_entry() {
        let path = std::env::temp_dir().join(format!(
            "that-provider-registry-test-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let registry = DynamicProviderRegistry::new(path.clone());
        registry
            .register(ProviderEntry {
                id: "groq".into(),
                transport: "openai_chat".into(),
                base_url: "https://api.groq.com/openai/v1".into(),
                api_key_env: "GROQ_API_KEY".into(),
                models: vec!["llama-3.3-70b-versatile".into()],
                registered_at: "2026-03-12T00:00:00Z".into(),
            })
            .unwrap();

        let entries = registry.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "groq");
        let _ = std::fs::remove_file(path);
    }
}
