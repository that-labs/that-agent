//! that-sandbox — Docker and Kubernetes execution boundary for sandboxed agent operations.

pub mod backend;
pub mod docker;
pub mod kubernetes;
pub mod scope;

pub use backend::{BackendClient, SandboxMode};

pub mod client;
pub use client::SandboxClient;
