mod acl;
mod api;
mod expiry;
mod git;
mod hooks;
mod state;

use clap::Parser;
use std::sync::Arc;
use tracing::info;

#[derive(Parser)]
#[command(name = "that-git-server", about = "Git Smart HTTP server for multi-agent coordination")]
struct Args {
    /// Bind address
    #[arg(long, env = "THAT_GIT_BIND_ADDR", default_value = "0.0.0.0:9418")]
    bind: String,

    /// Repository root directory
    #[arg(long, env = "THAT_GIT_REPO_ROOT", default_value = "/repos")]
    repo_root: std::path::PathBuf,

    /// Webhook URL for push notifications (optional)
    #[arg(long, env = "THAT_GIT_WEBHOOK_URL")]
    webhook_url: Option<String>,

    /// Hours before idle repos are auto-deleted
    #[arg(long, env = "THAT_GIT_EXPIRY_HOURS", default_value = "24")]
    expiry_hours: u64,

    /// Auto-merge clean task branches into main
    #[arg(long, env = "THAT_GIT_AUTO_MERGE")]
    auto_merge: bool,

    /// Maximum request body size in bytes (default 512MB)
    #[arg(long, env = "THAT_GIT_MAX_BODY", default_value = "536870912")]
    max_body: usize,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("that_git_server=info").init();

    let args = Args::parse();
    tokio::fs::create_dir_all(&args.repo_root).await.expect("cannot create repo root");

    let state = Arc::new(state::AppState::new(
        args.repo_root,
        args.webhook_url,
        args.expiry_hours,
        args.auto_merge,
    ));

    expiry::spawn_expiry_task(Arc::clone(&state));

    let app = axum::Router::new()
        // Git Smart HTTP
        .route("/{repo}/info/refs", axum::routing::get(git::info_refs))
        .route("/{repo}/git-upload-pack", axum::routing::post(git::upload_pack))
        .route("/{repo}/git-receive-pack", axum::routing::post(git::receive_pack))
        // REST API
        .route("/api/repos", axum::routing::get(api::list_repos))
        .route("/api/repos/{repo}", axum::routing::post(api::create_repo).delete(api::delete_repo))
        .route("/api/repos/{repo}/activity", axum::routing::get(api::repo_activity))
        .route("/api/repos/{repo}/diff/{*branch}", axum::routing::get(api::branch_diff))
        .route("/api/repos/{repo}/conflicts/{*branch}", axum::routing::get(api::branch_conflicts))
        .layer(axum::extract::DefaultBodyLimit::max(args.max_body))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await.expect("bind failed");
    info!("listening on {}", args.bind);
    axum::serve(listener, app).await.expect("server error");
}
