use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Handler strategy for a dynamic gateway route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RouteHandler {
    /// Returns a fixed JSON body on every request.
    Static { body: serde_json::Value },
    /// Executes a shell command; stdout becomes the response body.
    ///
    /// If the request has a body, it is passed as the `REQUEST_BODY` env var.
    Shell {
        command: String,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
}

fn default_timeout() -> u64 {
    30
}

/// A single entry in the dynamic route registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicRoute {
    /// HTTP method (e.g. "GET", "POST").
    pub method: String,
    /// URL path (e.g. "/v1/admin/plugins").
    pub path: String,
    pub handler: RouteHandler,
    /// RFC 3339 timestamp of when the route was registered.
    pub registered_at: String,
}

/// File-backed registry of agent-defined gateway routes.
///
/// Persisted at `<state_dir>/gateway_routes.json`. Two instances pointing to
/// the same file are always in sync because each operation reads/writes the
/// file directly.
pub struct DynamicRouteRegistry {
    pub path: PathBuf,
}

impl DynamicRouteRegistry {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Upsert a route (keyed by method + path).
    pub fn register(&self, route: DynamicRoute) -> Result<()> {
        let mut routes = self.load()?;
        routes.retain(|r| !(r.method == route.method && r.path == route.path));
        routes.push(route);
        self.save(&routes)
    }

    /// Remove a route by method + path.
    pub fn unregister(&self, method: &str, path: &str) -> Result<()> {
        let mut routes = self.load()?;
        routes.retain(|r| !(r.method == method && r.path == path));
        self.save(&routes)
    }

    /// Return all registered routes.
    pub fn list(&self) -> Result<Vec<DynamicRoute>> {
        self.load()
    }

    /// Look up a route by method + path.
    pub fn lookup(&self, method: &str, path: &str) -> Result<Option<DynamicRoute>> {
        let routes = self.load()?;
        Ok(routes
            .into_iter()
            .find(|r| r.method.eq_ignore_ascii_case(method) && r.path == path))
    }

    fn load(&self) -> Result<Vec<DynamicRoute>> {
        match std::fs::read_to_string(&self.path) {
            Ok(data) => Ok(serde_json::from_str(&data)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, routes: &[DynamicRoute]) -> Result<()> {
        crate::atomic_write_json(&self.path, routes)
    }
}

/// Execute a shell route handler; returns (status_code, response_body).
///
/// On success returns (200, stdout). On non-zero exit or timeout returns (500, error JSON).
pub async fn execute_shell_handler(
    command: &str,
    timeout_secs: u64,
    request_body: Option<String>,
) -> (u16, String) {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(body) = request_body {
        cmd.env("REQUEST_BODY", body);
    }

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output()).await;

    match result {
        Ok(Ok(out)) if out.status.success() => {
            (200, String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            (
                500,
                serde_json::json!({ "error": stderr.trim() }).to_string(),
            )
        }
        Ok(Err(e)) => (
            500,
            serde_json::json!({ "error": e.to_string() }).to_string(),
        ),
        Err(_) => (500, serde_json::json!({ "error": "timed out" }).to_string()),
    }
}
