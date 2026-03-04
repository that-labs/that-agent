use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Open adapter kind identifier (e.g. "telegram", "tui", or a custom type).
///
/// Unlike a closed enum, this allows third-party channel plugins to register
/// custom kinds without changing this crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AdapterType(String);

impl AdapterType {
    pub const TUI: &'static str = "tui";
    pub const TELEGRAM: &'static str = "telegram";
    pub const HTTP: &'static str = "http";

    /// Normalize an adapter kind.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().trim().to_ascii_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_tui(&self) -> bool {
        self.as_str() == Self::TUI
    }
}

impl From<&str> for AdapterType {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for AdapterType {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl Serialize for AdapterType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AdapterType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::new(value))
    }
}

impl std::fmt::Display for AdapterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Per-adapter configuration block.
///
/// String fields support `${ENV_VAR}` syntax for environment variable expansion.
/// Call `ChannelConfig::resolve_env_vars()` after loading to expand them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterConfig {
    /// Stable adapter ID used for routing and as the primary selector.
    ///
    /// If omitted, defaults to the adapter type (e.g. "telegram").
    /// When multiple adapters share the same ID, the router auto-suffixes
    /// them (`<id>-2`, `<id>-3`, ...).
    #[serde(default)]
    pub id: Option<String>,

    /// The channel type.
    #[serde(rename = "type")]
    pub adapter_type: AdapterType,

    /// Whether this adapter is active. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    // ── Telegram (legacy top-level — prefer nested `telegram:` in new configs) ──
    /// Telegram Bot API token. Supports `${TELEGRAM_BOT_TOKEN}`.
    pub bot_token: Option<String>,
    /// Telegram chat ID to send messages to.
    pub chat_id: Option<String>,
    /// Additional chat IDs the bot will listen and respond in.
    #[serde(default)]
    pub allowed_chats: Vec<String>,

    // ── Access control ────────────────────────────────────────────────────────
    /// Allowlist of user IDs permitted to send messages to the agent.
    ///
    /// Applies to any adapter that supports sender filtering.
    /// If empty, all users are accepted.
    #[serde(default)]
    pub allowed_senders: Vec<String>,

    /// Adapter-specific extension settings for custom channel factories.
    ///
    /// Unknown TOML keys are captured here, which allows new adapter types to
    /// carry custom configuration without changing this struct.
    #[serde(flatten, default)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl AdapterConfig {
    /// Adapter ID before uniqueness suffixing.
    pub fn base_id(&self) -> String {
        self.id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.adapter_type.to_string())
    }

    /// Read an adapter-specific extension value.
    pub fn extra_value(&self, key: &str) -> Option<&serde_json::Value> {
        self.extra.get(key)
    }
}

fn default_true() -> bool {
    true
}

/// Top-level channel configuration block.
///
/// ## Example TOML
///
/// ```toml
/// [channels]
/// primary = "ops"
///
/// [[channels.adapters]]
/// type = "tui"
/// enabled = true
///
/// [[channels.adapters]]
/// id = "ops"
/// type = "telegram"
/// enabled = true
/// bot_token = "${TELEGRAM_BOT_TOKEN}"
/// chat_id = "${TELEGRAM_CHAT_ID}"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelConfig {
    /// ID of the primary channel used for `human_ask` interactions.
    ///
    /// Prefer adapter IDs (e.g. `"ops"`). For backward compatibility,
    /// adapter type names (e.g. `"telegram"`) are also accepted.
    #[serde(default)]
    pub primary: Option<String>,

    /// List of adapter configuration blocks.
    #[serde(default)]
    pub adapters: Vec<AdapterConfig>,
}

impl ChannelConfig {
    /// Expand `${ENV_VAR}` placeholders in all string fields.
    ///
    /// Fields that reference undefined env vars are set to `None`.
    pub fn resolve_env_vars(&mut self) {
        for adapter in &mut self.adapters {
            resolve_field(&mut adapter.id);
            resolve_field(&mut adapter.bot_token);
            resolve_field(&mut adapter.chat_id);
            for value in adapter.extra.values_mut() {
                resolve_value_env_vars(value);
            }
            adapter.extra.retain(|_, v| !v.is_null());
        }
    }

    /// Return the enabled adapters only.
    pub fn enabled_adapters(&self) -> impl Iterator<Item = &AdapterConfig> {
        self.adapters.iter().filter(|a| a.enabled)
    }
}

/// Expand a `${VAR}` placeholder in a field, replacing it with the env var value.
/// If the env var is not set, the field is set to `None`.
fn resolve_field(field: &mut Option<String>) {
    if let Some(val) = field.as_deref() {
        if is_env_placeholder(val) {
            *field = resolve_env_placeholder(val);
        }
    }
}

fn resolve_value_env_vars(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            if let Some(resolved) = resolve_env_placeholder(s) {
                *value = serde_json::Value::String(resolved);
            } else if is_env_placeholder(s) {
                *value = serde_json::Value::Null;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_value_env_vars(item);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                resolve_value_env_vars(item);
            }
        }
        _ => {}
    }
}

fn resolve_env_placeholder(value: &str) -> Option<String> {
    if is_env_placeholder(value) {
        let var_name = &value[2..value.len() - 1];
        return std::env::var(var_name).ok();
    }
    None
}

fn is_env_placeholder(value: &str) -> bool {
    value.starts_with("${") && value.ends_with('}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_env_vars_clears_missing_placeholders_and_preserves_plain_values() {
        let mut extra = HashMap::new();
        extra.insert(
            "api_key".to_string(),
            serde_json::Value::String("${THAT_CHANNELS_TEST_MISSING_API_KEY}".to_string()),
        );
        extra.insert(
            "region".to_string(),
            serde_json::Value::String("us-east-1".to_string()),
        );

        let adapter = AdapterConfig {
            id: Some("${THAT_CHANNELS_TEST_MISSING_ID}".to_string()),
            adapter_type: AdapterType::from("custom"),
            enabled: true,
            bot_token: None,
            chat_id: None,
            allowed_chats: Vec::new(),
            allowed_senders: Vec::new(),
            extra,
        };

        let mut config = ChannelConfig {
            primary: None,
            adapters: vec![adapter],
        };
        config.resolve_env_vars();

        let adapter = &config.adapters[0];
        assert_eq!(adapter.id, None);
        assert_eq!(
            adapter.extra.get("region").and_then(|v| v.as_str()),
            Some("us-east-1")
        );
        assert!(!adapter.extra.contains_key("api_key"));
    }
}
