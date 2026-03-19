use std::process::Command;
use std::sync::Arc;

/// Helper: start the git server on a random port, return (addr, state, join_handle).
async fn start_server(
    tmp: &std::path::Path,
) -> (
    String,
    Arc<that_git_server::state::AppState>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(that_git_server::state::AppState::new(
        tmp.to_path_buf(),
        None,
        0, // disable expiry
        false,
    ));

    let app = axum::Router::new()
        .route(
            "/{repo}/info/refs",
            axum::routing::get(that_git_server::git::info_refs),
        )
        .route(
            "/{repo}/git-upload-pack",
            axum::routing::post(that_git_server::git::upload_pack),
        )
        .route(
            "/{repo}/git-receive-pack",
            axum::routing::post(that_git_server::git::receive_pack),
        )
        .route(
            "/api/repos",
            axum::routing::get(that_git_server::api::list_repos),
        )
        .route(
            "/api/repos/{repo}",
            axum::routing::post(that_git_server::api::create_repo)
                .delete(that_git_server::api::delete_repo),
        )
        .route(
            "/api/repos/{repo}/activity",
            axum::routing::get(that_git_server::api::repo_activity),
        )
        .route(
            "/api/repos/{repo}/diff/{*branch}",
            axum::routing::get(that_git_server::api::branch_diff),
        )
        .route(
            "/api/repos/{repo}/conflicts/{*branch}",
            axum::routing::get(that_git_server::api::branch_conflicts),
        )
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (addr, state, handle)
}

/// Run a git command in spawn_blocking so it doesn't deadlock the tokio runtime.
async fn git(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let out = Command::new("git")
            .args(&args)
            .current_dir(&cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        out
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_clone_push_fetch_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let (addr, _state, _handle) = start_server(&repos).await;

    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // Init a local repo, add a file, commit
    git(&["init", "-b", "main"], &work).await;
    git(&["config", "user.email", "test@test.com"], &work).await;
    git(&["config", "user.name", "Test"], &work).await;
    std::fs::write(work.join("hello.txt"), "hello world").unwrap();
    git(&["add", "hello.txt"], &work).await;
    git(&["commit", "-m", "initial"], &work).await;

    // Push to the git server (auto-creates bare repo)
    let server_url = format!("http://{addr}/workspace.git");
    git(&["push", &server_url, "main"], &work).await;

    // Clone into a new directory
    let clone_dir = tmp.path().join("clone");
    git(
        &[
            "clone",
            "--branch",
            "main",
            &server_url,
            clone_dir.to_str().unwrap(),
        ],
        tmp.path(),
    )
    .await;
    assert!(
        clone_dir.join("hello.txt").exists(),
        "hello.txt not found in clone dir; contents: {:?}",
        std::fs::read_dir(&clone_dir).map(|d| d
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect::<Vec<_>>())
    );
    let content = std::fs::read_to_string(clone_dir.join("hello.txt")).unwrap();
    assert_eq!(content, "hello world");

    // Push a change on a task branch from the clone
    git(&["config", "user.email", "test@test.com"], &clone_dir).await;
    git(&["config", "user.name", "Test"], &clone_dir).await;
    git(&["checkout", "-b", "task/worker-1"], &clone_dir).await;
    std::fs::write(clone_dir.join("new.txt"), "worker output").unwrap();
    git(&["add", "new.txt"], &clone_dir).await;
    git(&["commit", "-m", "worker change"], &clone_dir).await;
    git(&["push", &server_url, "task/worker-1"], &clone_dir).await;

    // Fetch the task branch from the original repo
    git(&["fetch", &server_url, "task/worker-1"], &work).await;
    let log = git(&["log", "--oneline", "FETCH_HEAD", "-1"], &work).await;
    let log_str = String::from_utf8_lossy(&log.stdout);
    assert!(log_str.contains("worker change"));
}

#[tokio::test]
async fn api_create_list_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let (addr, _state, _handle) = start_server(&repos).await;

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // List — empty initially
    let resp: Vec<serde_json::Value> = client
        .get(format!("{base}/api/repos"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.is_empty());

    // Create
    let resp = client
        .post(format!("{base}/api/repos/myrepo"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);

    // List — now has one
    let resp: Vec<serde_json::Value> = client
        .get(format!("{base}/api/repos"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp.len(), 1);
    assert_eq!(resp[0]["name"], "myrepo.git");

    // Delete
    let resp = client
        .delete(format!("{base}/api/repos/myrepo"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // List — empty again
    let resp: Vec<serde_json::Value> = client
        .get(format!("{base}/api/repos"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(resp.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_activity_and_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let (addr, _state, _handle) = start_server(&repos).await;

    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let server_url = format!("http://{addr}/workspace.git");

    // Setup: init, commit, push main
    git(&["init", "-b", "main"], &work).await;
    git(&["config", "user.email", "t@t.com"], &work).await;
    git(&["config", "user.name", "T"], &work).await;
    std::fs::write(work.join("f.txt"), "base").unwrap();
    git(&["add", "f.txt"], &work).await;
    git(&["commit", "-m", "base"], &work).await;
    git(&["push", &server_url, "main"], &work).await;

    // Push a task branch
    git(&["checkout", "-b", "task/w1"], &work).await;
    std::fs::write(work.join("f.txt"), "changed").unwrap();
    git(&["add", "f.txt"], &work).await;
    git(&["commit", "-m", "worker change"], &work).await;
    git(&["push", &server_url, "task/w1"], &work).await;

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    // Activity
    let activity: serde_json::Value = client
        .get(format!("{base}/api/repos/workspace/activity"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(activity["repo"], "workspace");
    let branches = activity["branches"].as_array().unwrap();
    assert!(branches.len() >= 2);
    let w1 = branches.iter().find(|b| b["name"] == "task/w1").unwrap();
    assert!(w1["ahead"].as_u64().unwrap() > 0);

    // Diff
    let diff = client
        .get(format!("{base}/api/repos/workspace/diff/task/w1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(diff.contains("changed"));
}

#[tokio::test]
async fn api_delete_nonexistent_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let (addr, _state, _handle) = start_server(&repos).await;

    let resp = reqwest::Client::new()
        .delete(format!("http://{addr}/api/repos/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn api_activity_nonexistent_returns_404() {
    let tmp = tempfile::tempdir().unwrap();
    let repos = tmp.path().join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let (addr, _state, _handle) = start_server(&repos).await;

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/api/repos/nope/activity"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
