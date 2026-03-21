//! Per-provider rate limiting with exponential backoff.

use std::collections::HashMap;
use std::time::{Duration, Instant};

struct ProviderState {
    last_request: Instant,
    min_interval: Duration,
    backoff_until: Option<Instant>,
    consecutive_failures: u32,
}

pub struct RateLimiter {
    providers: HashMap<String, ProviderState>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn check(&self, provider: &str) -> Result<(), Duration> {
        if let Some(state) = self.providers.get(provider) {
            if let Some(until) = state.backoff_until {
                let now = Instant::now();
                if now < until {
                    return Err(until - now);
                }
            }
            let elapsed = state.last_request.elapsed();
            if elapsed < state.min_interval {
                return Err(state.min_interval - elapsed);
            }
        }
        Ok(())
    }

    pub fn record_request(&mut self, provider: &str) {
        let state = self
            .providers
            .entry(provider.to_string())
            .or_insert(ProviderState {
                last_request: Instant::now(),
                min_interval: Duration::from_millis(100),
                backoff_until: None,
                consecutive_failures: 0,
            });
        state.last_request = Instant::now();
    }

    pub fn record_success(&mut self, provider: &str) {
        if let Some(state) = self.providers.get_mut(provider) {
            state.consecutive_failures = 0;
            state.backoff_until = None;
        }
    }

    pub fn record_failure(&mut self, provider: &str, retry_after: Option<Duration>) {
        let state = self
            .providers
            .entry(provider.to_string())
            .or_insert(ProviderState {
                last_request: Instant::now(),
                min_interval: Duration::from_millis(100),
                backoff_until: None,
                consecutive_failures: 0,
            });
        state.consecutive_failures += 1;

        let backoff = if let Some(ra) = retry_after {
            ra
        } else {
            let base = Duration::from_secs(1);
            let multiplier = 2u64.saturating_pow(state.consecutive_failures.min(8));
            let backoff_secs = base.as_secs().saturating_mul(multiplier).min(300);
            Duration::from_secs(backoff_secs)
        };
        state.backoff_until = Some(Instant::now() + backoff);
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_provider_allowed() {
        let limiter = RateLimiter::new();
        assert!(limiter.check("test").is_ok());
    }

    #[test]
    fn test_record_success_resets() {
        let mut limiter = RateLimiter::new();
        limiter.record_failure("test", None);
        limiter.record_success("test");
        let state = limiter.providers.get("test").unwrap();
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.backoff_until.is_none());
    }

    #[test]
    fn test_exponential_backoff() {
        let mut limiter = RateLimiter::new();
        limiter.record_failure("test", None);
        assert!(limiter.check("test").is_err());

        limiter.record_failure("test", None);
        let state = limiter.providers.get("test").unwrap();
        assert_eq!(state.consecutive_failures, 2);
    }

    #[test]
    fn test_backoff_capped() {
        let mut limiter = RateLimiter::new();
        for _ in 0..20 {
            limiter.record_failure("test", None);
        }
        let state = limiter.providers.get("test").unwrap();
        if let Some(until) = state.backoff_until {
            let wait = until.duration_since(Instant::now());
            assert!(wait <= Duration::from_secs(301));
        }
    }
}
