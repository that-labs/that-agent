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

pub fn normalize_provider(provider: &str) -> Option<&'static str> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "openrouter" => Some("openrouter"),
        _ => None,
    }
}

pub fn suggested_models(provider: &str) -> Vec<&'static str> {
    MODEL_OPTIONS
        .iter()
        .filter_map(|(candidate_provider, model)| {
            (*candidate_provider == provider).then_some(*model)
        })
        .collect()
}

pub fn provider_is_available(provider: &str) -> bool {
    match normalize_provider(provider) {
        Some("anthropic") => has_env("CLAUDE_CODE_OAUTH_TOKEN") || has_env("ANTHROPIC_API_KEY"),
        Some("openai") => has_env("OPENAI_API_KEY"),
        Some("openrouter") => has_env("OPENROUTER_API_KEY"),
        _ => false,
    }
}

pub fn available_providers() -> Vec<&'static str> {
    PROVIDER_ORDER
        .iter()
        .copied()
        .filter(|provider| provider_is_available(provider))
        .collect()
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
        assert!(openai.contains(&"gpt-5.2-codex"));
        assert!(!openai.contains(&"claude-sonnet-4-6"));
    }

    #[test]
    fn normalize_provider_rejects_unknown_values() {
        assert_eq!(normalize_provider(" OpenAI "), Some("openai"));
        assert_eq!(normalize_provider("unknown"), None);
    }
}
