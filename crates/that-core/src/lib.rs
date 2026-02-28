//! that-core — Agent orchestration, TUI, sessions, and skills (library crate).
//!
//! Provides the runtime components for the `that` agent:
//! - Multi-turn LLM orchestration with streaming and retries
//! - Ratatui-based TUI for interactive chat
//! - Session management with JSONL transcript persistence
//! - Skill discovery and management
//! - Docker sandbox lifecycle
//! - Agent workspace file management (Soul.md, Identity.md, Agents.md, User.md, etc.)
//! - Generic multi-channel communication (via `that-channels`)

pub mod agent_loop;
pub mod agents;
pub mod audit;
pub mod config;
pub mod control;
pub mod default_skills;
pub mod heartbeat;
pub mod hooks;
pub mod observability;
pub mod orchestration;
pub mod sandbox;
pub mod session;
pub mod skills;
pub mod tasks;
pub mod tools;
pub mod transcription;
#[cfg(feature = "tui")]
pub mod tui;
pub mod workspace;

/// Re-export the channel abstraction crate for consumers of that-core.
pub use that_channels;
