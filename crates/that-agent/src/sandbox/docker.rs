use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tracing::{info, warn};

const DEFAULT_IMAGE: &str = "that-agent-sandbox";
const DEFAULT_DOCKER_SOCKET_PATH: &str = "/var/run/docker.sock";
const DOCKER_SOCKET_ENABLE_ENV: &str = "THAT_SANDBOX_DOCKER_SOCKET";
const DOCKER_SOCKET_PATH_ENV: &str = "THAT_SANDBOX_DOCKER_SOCKET_PATH";
const DOCKER_SOCKET_GID_ENV: &str = "THAT_SANDBOX_DOCKER_SOCKET_GID";
const DOCKER_SOCKET_FALLBACK_GID: u32 = 0;

#[derive(Debug, PartialEq)]
enum ContainerState {
    Running,
    Stopped,
    NotFound,
}

#[derive(Debug, Clone)]
pub struct DockerSandboxClient {
    pub container_name: String,
}

#[derive(Debug, Clone)]
pub struct DockerSocketStatus {
    pub enabled: bool,
    pub requested: bool,
    pub exists: bool,
    pub path: PathBuf,
    pub gid: Option<u32>,
}

fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

pub fn docker_socket_status() -> DockerSocketStatus {
    let path = std::env::var(DOCKER_SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_DOCKER_SOCKET_PATH));
    let requested = parse_env_bool(DOCKER_SOCKET_ENABLE_ENV).unwrap_or(true);
    let exists = path.exists();
    let enabled = requested && exists;
    let gid = if enabled { socket_gid(&path) } else { None };
    DockerSocketStatus {
        enabled,
        requested,
        exists,
        path,
        gid,
    }
}

fn socket_gid(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.gid())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn required_socket_gids(socket: &DockerSocketStatus) -> Vec<u32> {
    if !socket.enabled {
        return Vec::new();
    }
    let mut gids = vec![DOCKER_SOCKET_FALLBACK_GID];
    if let Some(gid) = socket.gid {
        if !gids.contains(&gid) {
            gids.push(gid);
        }
    }
    gids
}

impl DockerSandboxClient {
    pub fn container_name(agent_name: &str) -> String {
        format!("that-agent-{agent_name}")
    }

    pub fn home_volume_name(agent_name: &str) -> String {
        format!("that-agent-{agent_name}-home")
    }

    /// Look for build.sh in CWD or common project root markers.
    fn find_build_script() -> Option<std::path::PathBuf> {
        // Check CWD first
        let cwd = std::env::current_dir().ok()?;
        let candidate = cwd.join("build.sh");
        if candidate.exists() {
            return Some(candidate);
        }
        // Walk up looking for Cargo.toml (workspace root) + build.sh
        let mut dir = cwd.as_path();
        while let Some(parent) = dir.parent() {
            let script = parent.join("build.sh");
            if script.exists() && parent.join("Cargo.toml").exists() {
                return Some(script);
            }
            dir = parent;
        }
        None
    }

    fn inspect_container(name: &str) -> ContainerState {
        let output = std::process::Command::new("docker")
            .args(["inspect", "--format", "{{.State.Status}}", name])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let status = String::from_utf8_lossy(&out.stdout).trim().to_string();
                match status.as_str() {
                    "running" => ContainerState::Running,
                    _ => ContainerState::Stopped,
                }
            }
            _ => ContainerState::NotFound,
        }
    }

    fn container_has_mount(name: &str, destination: &Path) -> bool {
        let destination = destination.display().to_string();
        let output = std::process::Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{ range .Mounts }}{{ println .Destination }}{{ end }}",
                name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.lines().any(|line| line.trim() == destination)
            }
            _ => false,
        }
    }

    fn container_has_group(name: &str, gid: u32) -> bool {
        let gid = gid.to_string();
        let output = std::process::Command::new("docker")
            .args([
                "inspect",
                "--format",
                "{{ range .HostConfig.GroupAdd }}{{ println . }}{{ end }}",
                name,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                text.lines().any(|line| line.trim() == gid)
            }
            _ => false,
        }
    }

    fn socket_config_matches(name: &str, socket: &DockerSocketStatus) -> bool {
        if !socket.enabled {
            return true;
        }
        if !Self::container_has_mount(name, &socket.path) {
            return false;
        }
        for gid in required_socket_gids(socket) {
            if !Self::container_has_group(name, gid) {
                return false;
            }
        }
        true
    }

    fn create_container(name: &str, image: &str, agent_name: &str, workspace: &Path) -> Result<()> {
        let host_home = dirs::home_dir()
            .context("Failed to resolve home directory")?
            .join(".that-agent");
        std::fs::create_dir_all(&host_home).context("Failed to create ~/.that-agent on host")?;

        let socket = docker_socket_status();
        if socket.enabled {
            info!(
                container = name,
                socket = %socket.path.display(),
                "Docker socket passthrough enabled for sandbox"
            );
            if let Some(gid) = socket.gid {
                info!(
                    container = name,
                    socket_gid = gid,
                    "Adding socket GID to container groups"
                );
            }
            info!(
                container = name,
                socket_fallback_gid = DOCKER_SOCKET_FALLBACK_GID,
                "Adding root-group fallback for Docker Desktop socket compatibility"
            );
        } else if socket.requested && !socket.exists {
            warn!(
                container = name,
                socket = %socket.path.display(),
                "Docker socket passthrough requested but socket path was not found; continuing without host Docker access"
            );
        } else {
            info!(
                container = name,
                "Docker socket passthrough disabled for sandbox"
            );
        }

        // If the workspace is a worktree (lives under .worktrees/), also mount
        // the base repository as read-only so git operations can reference it.
        let base_repo_mount = detect_worktree_base(workspace);

        let mut args = vec![
            "create".to_string(),
            "--name".to_string(),
            name.to_string(),
            "--label".to_string(),
            format!("that-agent.agent={agent_name}"),
            "-v".to_string(),
            format!("{}:/workspace", workspace.display()),
            "-v".to_string(),
            format!("{}:/home/agent/.that-agent", host_home.display()),
            format!(
                "--memory={}",
                std::env::var("THAT_SANDBOX_MEMORY").unwrap_or_else(|_| "2g".into())
            ),
            format!(
                "--cpus={}",
                std::env::var("THAT_SANDBOX_CPU").unwrap_or_else(|_| "2".into())
            ),
            format!(
                "--shm-size={}",
                std::env::var("THAT_SANDBOX_SHM").unwrap_or_else(|_| "1g".into())
            ),
            "--pids-limit".to_string(),
            std::env::var("THAT_SANDBOX_PIDS").unwrap_or_else(|_| "256".into()),
            "--network=bridge".to_string(),
            "-e".to_string(),
            "SHELL=/bin/bash".to_string(),
            "-e".to_string(),
            format!("BASH_ENV=/home/agent/.that-agent/agents/{agent_name}/.bashrc"),
        ];
        if socket.enabled {
            let socket_path = socket.path.display().to_string();
            args.push("-v".to_string());
            args.push(format!("{socket_path}:{socket_path}"));
            for gid in required_socket_gids(&socket) {
                args.push("--group-add".to_string());
                args.push(gid.to_string());
            }
            args.push("-e".to_string());
            args.push(format!("{DOCKER_SOCKET_ENABLE_ENV}=1"));
            args.push("-e".to_string());
            args.push(format!("{DOCKER_SOCKET_PATH_ENV}={socket_path}"));
            if let Some(gid) = socket.gid {
                args.push("-e".to_string());
                args.push(format!("{DOCKER_SOCKET_GID_ENV}={gid}"));
            }
            args.push("-e".to_string());
            args.push(format!("DOCKER_HOST=unix://{socket_path}"));
        } else {
            args.push("-e".to_string());
            args.push(format!("{DOCKER_SOCKET_ENABLE_ENV}=0"));
            args.push("-e".to_string());
            args.push(format!(
                "{}={}",
                DOCKER_SOCKET_PATH_ENV,
                socket.path.display()
            ));
            args.push("-e".to_string());
            args.push(format!("{DOCKER_SOCKET_GID_ENV}=0"));
        }
        // Add hierarchy labels for parent/role when the agent is part of a multi-agent setup.
        if let Ok(parent) = std::env::var("THAT_AGENT_PARENT") {
            if !parent.is_empty() {
                args.push("--label".to_string());
                args.push(format!("that-agent.parent={parent}"));
            }
        }
        if let Ok(role) = std::env::var("THAT_AGENT_ROLE") {
            if !role.is_empty() {
                args.push("--label".to_string());
                args.push(format!("that-agent.role={role}"));
            }
        }
        // Mount the base repository read-only when workspace is a worktree.
        if let Some(ref base_repo) = base_repo_mount {
            args.push("-v".to_string());
            args.push(format!("{}:/base-repo:ro", base_repo.display()));
            args.push("-e".to_string());
            args.push("THAT_WORKTREE_BASE_REPO=/base-repo".to_string());
        }

        args.extend([
            "--entrypoint".to_string(),
            "tail".to_string(),
            image.to_string(),
            "-f".to_string(),
            "/dev/null".to_string(),
        ]);

        let status = std::process::Command::new("docker")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .context("Failed to run docker create")?;

        if !status.success() {
            anyhow::bail!("docker create failed for container '{name}'");
        }

        info!(
            container = name,
            image = image,
            "Created persistent sandbox container"
        );
        Ok(())
    }

    fn start_container(name: &str) -> Result<()> {
        let status = std::process::Command::new("docker")
            .args(["start", name])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .context("Failed to run docker start")?;

        if !status.success() {
            anyhow::bail!("docker start failed for container '{name}'");
        }

        info!(container = name, "Started existing sandbox container");
        Ok(())
    }

    /// Synchronous Docker lifecycle setup. Call via `spawn_blocking` from async contexts.
    pub fn connect_sync(agent_name: &str, workspace: &Path) -> Result<Self> {
        let image =
            std::env::var("THAT_AGENT_SANDBOX_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string());

        let check = std::process::Command::new("docker")
            .args(["image", "inspect", &image])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match check {
            Ok(status) if status.success() => {}
            _ => {
                // Try auto-building if build.sh exists in the working directory or project root.
                let build_script = Self::find_build_script();
                if let Some(script) = build_script {
                    eprintln!(
                        "Sandbox image '{image}' not found — auto-building via {}…",
                        script.display()
                    );
                    let build = std::process::Command::new("bash")
                        .arg(&script)
                        .env("THAT_AGENT_IMAGE", &image)
                        .status();
                    match build {
                        Ok(s) if s.success() => {}
                        _ => {
                            anyhow::bail!(
                                "Auto-build of sandbox image '{image}' failed. Check build output above."
                            );
                        }
                    }
                } else {
                    anyhow::bail!(
                        "Sandbox image '{image}' not found. Build it first:\n  \
                         ./build.sh"
                    );
                }
            }
        }

        let workspace = workspace
            .canonicalize()
            .context("Failed to resolve workspace path")?;

        let name = Self::container_name(agent_name);
        let socket = docker_socket_status();

        match Self::inspect_container(&name) {
            ContainerState::Running => {
                if !Self::socket_config_matches(&name, &socket) {
                    info!(
                        container = %name,
                        socket = %socket.path.display(),
                        "Recreating running sandbox container to apply Docker socket mount configuration"
                    );
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "-f", &name])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    Self::create_container(&name, &image, agent_name, &workspace)?;
                    Self::start_container(&name)?;
                } else {
                    info!(container = %name, "Reusing running sandbox container");
                }
            }
            ContainerState::Stopped => {
                if !Self::socket_config_matches(&name, &socket) {
                    info!(
                        container = %name,
                        socket = %socket.path.display(),
                        "Recreating stopped sandbox container to apply Docker socket mount configuration"
                    );
                    let _ = std::process::Command::new("docker")
                        .args(["rm", "-f", &name])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                    Self::create_container(&name, &image, agent_name, &workspace)?;
                    Self::start_container(&name)?;
                } else {
                    info!(container = %name, "Restarting stopped sandbox container");
                    Self::start_container(&name)?;
                }
            }
            ContainerState::NotFound => {
                info!(container = %name, image = %image, "Creating new sandbox container");
                Self::create_container(&name, &image, agent_name, &workspace)?;
                Self::start_container(&name)?;
            }
        }

        Ok(Self {
            container_name: name,
        })
    }

    pub fn remove(agent_name: &str) {
        let name = Self::container_name(agent_name);
        info!(container = %name, "Removing sandbox container");
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    pub fn remove_home_volume(agent_name: &str) {
        let volume = Self::home_volume_name(agent_name);
        info!(volume = %volume, "Removing agent home volume");
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", &volume])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Detect if a workspace path is inside a `.worktrees/` directory and return
/// the base repository path if so.
///
/// Worktree paths follow the pattern `{base_repo}/.worktrees/{agent_name}/`.
/// When detected, the base repo should be mounted read-only so git operations
/// can reference the shared object store.
fn detect_worktree_base(workspace: &Path) -> Option<std::path::PathBuf> {
    // Walk up the path looking for a `.worktrees` component.
    for ancestor in workspace.ancestors() {
        if ancestor
            .file_name()
            .map(|n| n == ".worktrees")
            .unwrap_or(false)
        {
            // The parent of `.worktrees/` is the base repo.
            return ancestor.parent().map(|p| p.to_path_buf());
        }
    }
    None
}
