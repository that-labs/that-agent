//! that-agent — Consolidated autonomous agent framework.
//!
//! Provides orchestration, tools, channels, plugins, eval, and CLI
//! in a single crate.

// ── from that-core (promoted to root) ──
pub mod agent_loop;
pub mod agents;
pub mod audit;
pub mod auth;
pub mod config;
pub mod control;
pub mod default_skills;
pub mod heartbeat;
pub mod hooks;
pub mod model_catalog;
pub mod observability;
pub mod orchestration;
pub mod plans;
pub mod provider_registry;
pub mod session;
pub mod skills;
pub mod tasks;
pub mod tool_dispatch;
pub mod transcription;
#[cfg(feature = "tui")]
pub mod tui;
pub mod workspace;

// ── from other crates (as modules) ──
pub mod channels;
pub mod eval;
pub mod plugins;
pub mod sandbox;
pub mod tools;

// ── CLI layer ──
pub mod cli;
pub mod commands;
