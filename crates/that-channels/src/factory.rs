use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;

use crate::adapters::{DynamicRouteRegistry, HttpAdapter, TelegramAdapter};
use crate::channel::{ChannelRef, InboundMessage};
use crate::config::{AdapterConfig, AdapterType, ChannelConfig};
use crate::router::ChannelRouter;

/// Resolve the cluster directory from environment or default location.
fn default_cluster_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("THAT_CLUSTER_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".that-agent").join("cluster"))
        .filter(|p| p.is_dir())
}

type FactoryFn = Arc<dyn Fn(&AdapterConfig, &str) -> Result<ChannelRef> + Send + Sync + 'static>;

/// Router build mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelBuildMode {
    /// Build only external/headless adapters.
    ///
    /// `tui` adapters are skipped because they require a live ratatui loop
    /// in `that-core`.
    Headless,
    /// Build every enabled adapter in the config.
    All,
}

/// Registry of adapter factories keyed by adapter type.
///
/// This removes adapter construction hardcoding from CLI code and gives
/// extension points for custom channel types.
#[derive(Clone, Default)]
pub struct ChannelFactoryRegistry {
    factories: HashMap<AdapterType, FactoryFn>,
}

impl ChannelFactoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a registry preloaded with built-in external adapters.
    ///
    /// Includes: `telegram`.
    /// Excludes: `tui` (lives in `that-core` to avoid circular dependency).
    pub fn with_builtin_adapters() -> Self {
        let mut registry = Self::new();
        registry.register_builtin_adapters();
        registry
    }

    /// Register a channel factory.
    pub fn register_mut<F>(&mut self, adapter_type: impl Into<AdapterType>, factory: F) -> &mut Self
    where
        F: Fn(&AdapterConfig, &str) -> Result<ChannelRef> + Send + Sync + 'static,
    {
        self.factories
            .insert(adapter_type.into(), Arc::new(factory));
        self
    }

    /// Register a channel factory (builder style).
    pub fn register<F>(mut self, adapter_type: impl Into<AdapterType>, factory: F) -> Self
    where
        F: Fn(&AdapterConfig, &str) -> Result<ChannelRef> + Send + Sync + 'static,
    {
        self.register_mut(adapter_type, factory);
        self
    }

    /// Build a channel router from config.
    ///
    /// `route_registry` is passed to the auto-injected HTTP gateway adapter.
    /// Pass `None` to leave dynamic routing disabled on that adapter.
    pub fn build_router(
        &self,
        config: &ChannelConfig,
        mode: ChannelBuildMode,
        route_registry: Option<&DynamicRouteRegistry>,
    ) -> Result<(Arc<ChannelRouter>, mpsc::UnboundedReceiver<InboundMessage>)> {
        let mut channels: Vec<ChannelRef> = Vec::new();
        let mut primary_idx = 0usize;
        let mut id_counts: HashMap<String, usize> = HashMap::new();
        let mut has_http = false;
        let primary = config
            .primary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        for adapter_cfg in config.enabled_adapters() {
            if adapter_cfg.adapter_type.is_http() {
                has_http = true;
            }
            if mode == ChannelBuildMode::Headless && adapter_cfg.adapter_type.is_tui() {
                continue;
            }

            let base_id = adapter_cfg.base_id();
            let entry = id_counts.entry(base_id.clone()).or_insert(0);
            *entry += 1;
            let id = if *entry == 1 {
                base_id.clone()
            } else {
                format!("{base_id}-{}", *entry)
            };

            let factory = self
                .factories
                .get(&adapter_cfg.adapter_type)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No channel factory registered for adapter type '{}'. \
                     Register one via ChannelFactoryRegistry::register[_mut]().",
                        adapter_cfg.adapter_type
                    )
                })?;

            let idx = channels.len();
            if let Some(primary) = primary {
                if primary == id
                    || primary == base_id
                    || primary == adapter_cfg.adapter_type.as_str()
                {
                    primary_idx = idx;
                }
            }

            channels.push(factory(adapter_cfg, &id)?);
        }

        // Always ensure an HTTP gateway is present for in-cluster communication.
        // Reads THAT_GATEWAY_ADDR (default 0.0.0.0:8080). Can be disabled by
        // setting THAT_GATEWAY_ADDR="" if a custom HTTP adapter is already configured.
        if !has_http {
            let bind_addr =
                std::env::var("THAT_GATEWAY_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
            if !bind_addr.is_empty() {
                let adapter = HttpAdapter::new("gateway", &bind_addr, None, 300);
                let adapter = if let Some(reg) = route_registry {
                    adapter.with_route_registry(reg)
                } else {
                    adapter
                };
                // Derive cluster_dir for scratchpad endpoints.
                let adapter = if let Some(dir) = default_cluster_dir() {
                    adapter.with_cluster_dir(dir)
                } else {
                    adapter
                };
                channels.push(Arc::new(adapter));
            }
        }

        if channels.is_empty() {
            anyhow::bail!(
                "No enabled channels configured for mode {:?}. \
                 Add channel adapters under [channels] in the agent config.",
                mode
            );
        }

        let (router, inbound_rx) = ChannelRouter::new(channels, primary_idx);
        Ok((Arc::new(router), inbound_rx))
    }

    fn register_builtin_adapters(&mut self) {
        self.register_mut(AdapterType::TELEGRAM, |cfg, id| {
            let token = required(&cfg.bot_token, "telegram", "bot_token")?;
            let chat_id = required(&cfg.chat_id, "telegram", "chat_id")?;
            Ok(Arc::new(TelegramAdapter::new(
                id.to_string(),
                token,
                chat_id,
                cfg.allowed_chats.clone(),
                cfg.allowed_senders.clone(),
            )))
        });

        let http_factory: FactoryFn = Arc::new(|cfg, id| {
            let bind_addr = cfg
                .extra_value("bind_addr")
                .and_then(|v| v.as_str())
                .unwrap_or("0.0.0.0:8080")
                .to_string();
            let auth_token = cfg
                .extra_value("auth_token")
                .and_then(|v| v.as_str())
                .map(String::from);
            let request_timeout_secs = cfg
                .extra_value("request_timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(60);
            Ok(Arc::new(HttpAdapter::new(
                id,
                &bind_addr,
                auth_token,
                request_timeout_secs,
            )))
        });
        self.factories.insert(
            AdapterType::from(AdapterType::HTTP),
            Arc::clone(&http_factory),
        );
        self.factories
            .insert(AdapterType::from(AdapterType::GATEWAY), http_factory);
    }
}

fn required<'a>(value: &'a Option<String>, adapter: &str, field: &str) -> Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{adapter} adapter: {field} is required"))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use crate::channel::{
        Channel, ChannelCapabilities, ChannelEvent, InboundMessage, MessageHandle, OutboundTarget,
    };

    use super::*;

    /// Serialize tests that mutate `THAT_GATEWAY_ADDR`.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn base_adapter(ty: &str) -> AdapterConfig {
        AdapterConfig {
            id: None,
            adapter_type: AdapterType::from(ty),
            enabled: true,
            bot_token: None,
            chat_id: None,
            allowed_chats: Vec::new(),
            allowed_senders: Vec::new(),
            extra: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn builds_primary_by_id() -> Result<()> {
        let mut a1 = base_adapter(AdapterType::TELEGRAM);
        a1.id = Some("main".into());
        a1.bot_token = Some("token-1".into());
        a1.chat_id = Some("chat-1".into());

        let mut a2 = base_adapter(AdapterType::TELEGRAM);
        a2.id = Some("ops".into());
        a2.bot_token = Some("token-2".into());
        a2.chat_id = Some("chat-2".into());

        let config = ChannelConfig {
            primary: Some("ops".into()),
            adapters: vec![a1, a2],
        };

        let registry = ChannelFactoryRegistry::with_builtin_adapters();
        let (router, _) = registry.build_router(&config, ChannelBuildMode::Headless, None)?;
        assert_eq!(router.primary_id().await, "ops");
        Ok(())
    }

    #[tokio::test]
    async fn auto_suffixes_duplicate_ids() -> Result<()> {
        // Disable auto-gateway injection so we only see the configured adapters.
        let result = {
            let _lock = env_lock();
            std::env::set_var("THAT_GATEWAY_ADDR", "");
            let mut a1 = base_adapter(AdapterType::TELEGRAM);
            a1.bot_token = Some("token-1".into());
            a1.chat_id = Some("chat-1".into());

            let mut a2 = base_adapter(AdapterType::TELEGRAM);
            a2.bot_token = Some("token-2".into());
            a2.chat_id = Some("chat-2".into());

            let config = ChannelConfig {
                primary: None,
                adapters: vec![a1, a2],
            };

            let registry = ChannelFactoryRegistry::with_builtin_adapters();
            let result = registry.build_router(&config, ChannelBuildMode::Headless, None);
            std::env::remove_var("THAT_GATEWAY_ADDR");
            result
        };
        let (router, _) = result?;
        assert_eq!(router.channel_ids().await, "telegram,telegram-2");
        Ok(())
    }

    #[test]
    fn headless_skips_tui() {
        // Disable auto-gateway so only TUI (skipped in Headless mode) is present.
        let _lock = env_lock();
        std::env::set_var("THAT_GATEWAY_ADDR", "");
        let config = ChannelConfig {
            primary: Some("tui".into()),
            adapters: vec![base_adapter(AdapterType::TUI)],
        };
        let registry = ChannelFactoryRegistry::with_builtin_adapters();
        let result = registry.build_router(&config, ChannelBuildMode::Headless, None);
        std::env::remove_var("THAT_GATEWAY_ADDR");
        match result {
            Ok(_) => panic!("expected headless build to fail when only tui adapters are enabled"),
            Err(err) => assert!(err.to_string().contains("No enabled channels configured")),
        }
    }

    #[tokio::test]
    async fn supports_custom_factory_registration() -> Result<()> {
        struct MockChannel {
            id: String,
        }

        #[async_trait]
        impl Channel for MockChannel {
            fn id(&self) -> &str {
                &self.id
            }

            fn capabilities(&self) -> ChannelCapabilities {
                ChannelCapabilities::default()
            }

            fn format_instructions(&self) -> Option<String> {
                None
            }

            async fn send_event(
                &self,
                _event: &ChannelEvent,
                _target: Option<&OutboundTarget>,
            ) -> Result<MessageHandle> {
                Ok(MessageHandle::default())
            }

            async fn ask_human(
                &self,
                _message: &str,
                _timeout: Option<u64>,
                _target: Option<&OutboundTarget>,
            ) -> Result<String> {
                Ok(String::new())
            }

            async fn start_listener(
                &self,
                _tx: mpsc::UnboundedSender<InboundMessage>,
            ) -> Result<()> {
                Ok(())
            }
        }

        let adapter = base_adapter("mock");
        let config = ChannelConfig {
            primary: Some("mock".into()),
            adapters: vec![adapter],
        };

        // Disable auto-gateway so only the registered mock adapter is present.
        let result = {
            let _lock = env_lock();
            std::env::set_var("THAT_GATEWAY_ADDR", "");
            let registry = ChannelFactoryRegistry::new().register("mock", |cfg, id| {
                let id = cfg.id.clone().unwrap_or_else(|| id.to_string());
                Ok(Arc::new(MockChannel { id }))
            });
            let result = registry.build_router(&config, ChannelBuildMode::All, None);
            std::env::remove_var("THAT_GATEWAY_ADDR");
            result
        };
        let (router, _) = result?;
        assert_eq!(router.channel_ids().await, "mock");
        Ok(())
    }
}
