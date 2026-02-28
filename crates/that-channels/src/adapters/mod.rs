//! Built-in channel adapter implementations.
//!
//! Each adapter implements the [`Channel`][crate::Channel] trait for a specific
//! communication platform.
//!
//! ## Available Adapters
//!
//! | Adapter | Outbound | Inbound | `ask_human` |
//! |---------|----------|---------|-------------|
//! | [`TelegramAdapter`] | ✓ | ✓ (long-poll) | ✓ |
//! | [`HttpAdapter`] | ✓ | ✓ (HTTP server) | ✓ |
//!
//! ## TUI Adapter
//!
//! The TUI adapter (`TuiChannel`) lives in `that-core::tui` to avoid a circular
//! dependency between `that-channels` and `that-core`.

pub mod gateway;
pub mod gateway_routes;
pub mod http;
pub mod telegram;

pub use gateway::GatewayChannelAdapter;
pub use gateway_routes::{DynamicRoute, DynamicRouteRegistry, RouteHandler};
pub use http::HttpAdapter;
pub use telegram::TelegramAdapter;

/// Validate a callback URL for SSRF safety.
///
/// Rejects non-http(s) schemes and loopback hosts (127.0.0.1, ::1, localhost)
/// unless the `THAT_ALLOW_INTERNAL_CALLBACKS=1` env var is set.
pub fn validate_callback_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid callback URL: {e}"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "callback URL scheme '{other}' not allowed, must be http or https"
            ))
        }
    }

    let allow_internal = std::env::var("THAT_ALLOW_INTERNAL_CALLBACKS")
        .map(|v| v == "1")
        .unwrap_or(false);

    if !allow_internal {
        if let Some(host) = parsed.host_str() {
            if host == "localhost" || host == "127.0.0.1" || host == "::1" {
                return Err(format!(
                    "callback URL host '{host}' is loopback; set THAT_ALLOW_INTERNAL_CALLBACKS=1 to allow"
                ));
            }
        }
    }

    Ok(())
}
