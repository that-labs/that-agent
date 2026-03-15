use crate::provider_registry::{
    find_registered_provider, load_registered_providers, normalize_provider_id,
};

pub const MODEL_OPTIONS: &[(&str, &str)] = &[
    ("anthropic", "claude-opus-4-6"),
    ("anthropic", "claude-sonnet-4-6"),
    ("anthropic", "claude-haiku-4-5"),
    ("openai", "gpt-5.2-codex"),
    ("openai", "gpt-5.1-codex-mini"),
    ("openrouter", "minimax/minimax-m2.5"),
    ("openrouter", "anthropic/claude-sonnet-4.5"),
    ("openrouter", "minimax/minimax-m2.1"),
    ("openrouter", "qwen/qwen3-coder-next"),
];

const PROVIDER_ORDER: &[&str] = &["openai", "anthropic", "openrouter"];

pub fn normalize_provider(provider: &str) -> Option<String> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "openai" => Some("openai".into()),
        "anthropic" => Some("anthropic".into()),
        "openrouter" => Some("openrouter".into()),
        _ => normalize_provider_id(provider).filter(|id| find_registered_provider(id).is_some()),
    }
}

/// Normalize a shorthand model name to a full model ID.
/// e.g. "sonnet-4-6" → "claude-sonnet-4-6", "opus" → "claude-opus-4-6"
pub fn normalize_model(model: &str) -> String {
    let m = model.trim();
    // Already a full ID — return as-is
    if MODEL_OPTIONS.iter().any(|(_, id)| *id == m) {
        return m.to_string();
    }
    // Try prefixing "claude-" for Anthropic shorthand
    let with_prefix = format!("claude-{m}");
    if MODEL_OPTIONS.iter().any(|(_, id)| *id == with_prefix) {
        return with_prefix;
    }
    // Common aliases
    match m {
        "opus" | "claude-opus" => "claude-opus-4-6".to_string(),
        "sonnet" | "claude-sonnet" => "claude-sonnet-4-6".to_string(),
        "haiku" | "claude-haiku" => "claude-haiku-4-5".to_string(),
        _ => m.to_string(), // pass through unknown models unchanged
    }
}

pub fn suggested_models(provider: &str) -> Vec<String> {
    match provider {
        "openai" | "anthropic" | "openrouter" => MODEL_OPTIONS
            .iter()
            .filter_map(|(candidate_provider, model)| {
                (*candidate_provider == provider).then_some((*model).to_string())
            })
            .collect(),
        _ => find_registered_provider(provider)
            .map(|entry| entry.models)
            .unwrap_or_default(),
    }
}

pub fn provider_is_available(provider: &str) -> bool {
    match normalize_provider(provider) {
        Some(provider) if provider == "anthropic" => {
            has_env("CLAUDE_CODE_OAUTH_TOKEN") || has_env("ANTHROPIC_API_KEY")
        }
        Some(provider) if provider == "openai" => has_env("OPENAI_API_KEY"),
        Some(provider) if provider == "openrouter" => has_env("OPENROUTER_API_KEY"),
        Some(provider) => find_registered_provider(&provider)
            .map(|entry| has_env(&entry.api_key_env))
            .unwrap_or(false),
        _ => false,
    }
}

pub fn available_providers() -> Vec<String> {
    let mut providers: Vec<String> = PROVIDER_ORDER
        .iter()
        .filter(|provider| provider_is_available(provider))
        .map(|provider| (*provider).to_string())
        .collect();
    let mut dynamic: Vec<String> = load_registered_providers()
        .into_iter()
        .filter(|entry| provider_is_available(&entry.id))
        .map(|entry| entry.id)
        .collect();
    dynamic.sort();
    providers.extend(dynamic);
    providers
}

fn has_env(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggested_models_are_grouped_by_provider() {
        let openai = suggested_models("openai");
        assert!(openai.contains(&"gpt-5.2-codex".to_string()));
        assert!(!openai.contains(&"claude-sonnet-4-6".to_string()));
    }

    #[test]
    fn normalize_provider_rejects_unknown_values() {
        assert_eq!(normalize_provider(" OpenAI "), Some("openai".into()));
        assert_eq!(normalize_provider("__missing_provider__"), None);
    }
}
