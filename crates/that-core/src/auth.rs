pub const ANTHROPIC_OAUTH_ENV_VARS: &[&str] = &[
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_AUTH_TOKEN",
    "CLAUDE_CODE_AUTH",
];

pub fn is_anthropic_oauth_token(value: &str) -> bool {
    value.trim().starts_with("sk-ant-oat")
}

pub fn anthropic_oauth_token_from_env() -> Option<String> {
    first_nonempty_env(ANTHROPIC_OAUTH_ENV_VARS)
}

pub fn anthropic_api_key_from_env() -> Option<String> {
    first_nonempty_env(&["ANTHROPIC_API_KEY"]).or_else(anthropic_oauth_token_from_env)
}

pub fn anthropic_provider_available() -> bool {
    anthropic_api_key_from_env().is_some()
}

fn first_nonempty_env(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| nonempty_env(key))
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{anthropic_api_key_from_env, is_anthropic_oauth_token, ANTHROPIC_OAUTH_ENV_VARS};

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn resolve_with(values: &[(&str, &str)]) -> Option<String> {
        ANTHROPIC_OAUTH_ENV_VARS.iter().find_map(|key| {
            values
                .iter()
                .find_map(|(candidate, value)| (*candidate == *key).then_some(value.trim()))
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
    }

    #[test]
    fn oauth_aliases_resolve_in_priority_order() {
        let resolved = resolve_with(&[
            ("CLAUDE_CODE_AUTH", "legacy"),
            ("CLAUDE_CODE_AUTH_TOKEN", "auth-token"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token"),
        ]);
        assert_eq!(resolved.as_deref(), Some("oauth-token"));
    }

    #[test]
    fn oauth_aliases_skip_blank_values() {
        let resolved = resolve_with(&[
            ("CLAUDE_CODE_OAUTH_TOKEN", "   "),
            ("CLAUDE_CODE_AUTH_TOKEN", "auth-token"),
        ]);
        assert_eq!(resolved.as_deref(), Some("auth-token"));
    }

    #[test]
    fn anthropic_api_key_wins_over_oauth_aliases() {
        let _guard = ENV_LOCK.lock().unwrap();
        let keys = [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_AUTH_TOKEN",
            "CLAUDE_CODE_AUTH",
        ];
        let saved: Vec<_> = keys
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in keys {
            std::env::remove_var(key);
        }
        std::env::set_var("ANTHROPIC_API_KEY", "api-key");
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "oauth-token");

        let resolved = anthropic_api_key_from_env();

        for (key, value) in saved {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        assert_eq!(resolved.as_deref(), Some("api-key"));
    }

    #[test]
    fn oauth_token_prefix_matches_claude_code_tokens() {
        assert!(is_anthropic_oauth_token("sk-ant-oat01-test"));
        assert!(!is_anthropic_oauth_token("sk-ant-api03-test"));
    }
}
