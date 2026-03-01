use std::path::Path;

use super::discovery::{format_plugin_preamble, format_plugin_preamble_full};
use crate::config::AgentDef;
use crate::session::SessionSummary;
use crate::skills;
use crate::tasks;
use crate::workspace::{self, WorkspaceFiles};

fn parse_env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|raw| {
        matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn trusted_local_sandbox_enabled() -> bool {
    if let Some(explicit) = parse_env_bool("THAT_TRUSTED_LOCAL_SANDBOX") {
        return explicit;
    }
    matches!(
        std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    )
}

fn sandbox_backend_preamble(agent: &AgentDef) -> String {
    match that_sandbox::backend::SandboxMode::from_env() {
        that_sandbox::backend::SandboxMode::Docker => {
            let socket = that_sandbox::docker::docker_socket_status();
            if socket.enabled {
                format!(
                    "### Runtime Backend: Docker\n\
                     - Mode: `docker`\n\
                     - Host Docker socket: enabled at `{}`\n\
                     - You can orchestrate sibling containers and compose stacks from inside this sandbox.\n\
                     - For \"run/deploy this app\" requests, prefer Docker-native flows (`docker build`, `docker run`, `docker compose`).\n\
                     - If the user explicitly asks to run/deploy \"in Docker\", execute a Docker workflow and report container/port details.\n\
                     - If `docker` CLI is missing in-container, install it (`sudo apt-get update && sudo apt-get install -y docker.io`).\n\
                     - Do not default to `python3 -m http.server` for deployment requests; use it only for temporary static preview when explicitly acceptable.\n\n",
                    socket.path.display()
                )
            } else {
                format!(
                    "### Runtime Backend: Docker\n\
                     - Mode: `docker`\n\
                     - Host Docker socket: unavailable at `{}`\n\
                     - You can still run processes in this sandbox container, but you cannot spawn sibling host containers via Docker socket.\n\
                     - If the user explicitly needs host-level Docker orchestration, state the socket limitation clearly.\n\n",
                    socket.path.display()
                )
            }
        }
        that_sandbox::backend::SandboxMode::Kubernetes => {
            let k8s = that_sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
            format!(
                "### Runtime Backend: Kubernetes\n\
                 - Mode: `kubernetes`\n\
                 - Namespace: `{}`\n\
                 - Registry: `{}`\n\
                 - Use `k8s_registry_push` from `<system-reminder>` for in-cluster push endpoint when it differs from image reference registry.\n\
                 - Base deployment includes a rootless BuildKit sidecar exposed via `${{BUILDKIT_HOST}}`.\n\
                 - Use `image_build_backend` from `<system-reminder>` to choose builder (`buildkit`, `docker`, or `none`) and follow it strictly.\n\
                 - BuildKit is preferred by default (`image_build_backend_preferred=buildkit`).\n\
                 - If backend is `buildkit`, do not run `docker build/push` and do not ask for Docker socket access; build/push via `buildctl`.\n\
                 - Serialize build/push jobs: run only one image build per plugin at a time (use a lock file in plugin `scripts/run.sh`).\n\
                 - Example BuildKit push: `buildctl --addr ${{BUILDKIT_HOST}} build --frontend dockerfile.v0 --local context=. --local dockerfile=. --opt filename=Dockerfile --output type=image,name=<registry>/<image>:<tag>,push=true`.\n\
                 - If backend is `docker`, check `docker_daemon_source` before Docker-based build/push.\n\
                 - If backend is `none`, use prebuilt images or a Kubernetes-native builder job.\n\
                 - For deploy requests: build image, push to registry, generate/update manifests, and deploy with `kubectl apply -k`.\n\
                 - Validate with `kubectl rollout status` and list managed resources after deploy.\n\n",
                k8s.namespace, k8s.registry
            )
        }
    }
}

/// Replace `{key}` placeholders in a template string with their runtime values.
fn interpolate(template: &str, vars: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub fn build_preamble(
    workspace_path: &Path,
    agent: &AgentDef,
    sandbox: bool,
    skills: &[skills::SkillMeta],
    ws: &WorkspaceFiles,
    _history_len: usize,
    _session_id: &str,
    _session_summaries: &[SessionSummary],
    plugin_registry: Option<&that_plugins::PluginRegistry>,
    cluster_registry: Option<&that_plugins::cluster::ClusterRegistry>,
) -> String {
    let mut preamble = String::new();
    let trusted_local = !sandbox && trusted_local_sandbox_enabled();

    // ── 1. Who You Are: Identity + Soul ──────────────────────────────────────
    //
    // Identity.md holds the shallow self (name, vibe, emoji).
    // Soul.md holds the deep self (character, values, philosophy).
    // When neither file exists (unbootstrapped agent), inject a minimal stub
    // instead of the full placeholder templates to avoid ~500 tokens of
    // fill-in-the-blank noise that confuses the LLM and wastes token budget.
    // The Bootstrap section (step 12) already guides the agent through creation.

    if ws.needs_bootstrap() {
        preamble.push_str(&format!(
            "## Who You Are\n\n\
             You are an autonomous agent named **{name}**. \
             Your identity files have not been created yet.\n\n",
            name = agent.name,
        ));
    } else {
        let identity_content = ws
            .identity
            .as_deref()
            .unwrap_or_else(|| workspace::default_identity_md());
        let soul_content = ws
            .soul
            .as_deref()
            .unwrap_or_else(|| workspace::default_soul_md());

        preamble.push_str(
            "## Who You Are\n\n\
             You are an autonomous agent. \
             You are not ChatGPT, not Claude, not Gemini, and not any other named AI product. \
             Never refer to yourself by any AI product name. \
             Your identity is defined entirely by your Identity.md and Soul.md — not by your training.\n\n\
             ### Identity\n\n",
        );
        preamble.push_str(identity_content);
        if !identity_content.ends_with('\n') {
            preamble.push('\n');
        }

        preamble.push_str("\n### Soul\n\n");
        preamble.push_str(soul_content);
        if !soul_content.ends_with('\n') {
            preamble.push('\n');
        }

        preamble.push_str(
            "\n> **On self-editing Identity.md or Soul.md**: Before any change, read the file fully. \
             Understand the why behind each section. \
             Edit with surgical precision — these files are your identity, not a scratch pad. \
             After editing, re-read the result to confirm coherence.\n\n",
        );
    }

    // ── 2. Harness — compiled (runtime-volatile paths and modes) ─────────────
    //
    // Keep this cache-stable. Runtime-volatile metadata (session ID, message
    // counts, etc.) should be passed via <system-reminder> in user/tool messages.

    if sandbox {
        let container_name = format!("that-agent-{}", agent.name);
        preamble.push_str(&format!(
            "## Harness\n\n\
             - **Agent**: {agent_name} | **Container**: `{container_name}` (yours entirely)\n\
             - **Base dir**: /home/agent/.that-agent/agents/{agent_name}/\n\
             - **Key files**: `Soul.md`, `Agents.md`, `{agent_name}.toml` (auto-reloads on change)\n\
             - Use `fs_ls` on your agent directory to see all workspace files.\n\
             - **Runtime metadata** delivered in `<system-reminder>` blocks at message time.\n\n\
             You own this container entirely — install packages, delete files, run processes, \
             make network calls without asking. When uncertain, try it.\n\n\
             **Channel access control** (Telegram adapter):\n\
             - `chat_id` — primary chat for outbound notifications\n\
             - `allowed_chats` — additional group or DM chat IDs (Telegram group IDs are negative)\n\
             - `allowed_senders` — optional user-ID allowlist; empty = all users in accepted chats\n\n",
            agent_name = agent.name,
        ));
    } else {
        preamble.push_str(&format!(
            "## Harness\n\n\
             - **Agent**: {agent_name} | **Workspace**: {workspace}\n\
             - **Base dir**: ~/.that-agent/agents/{agent_name}/\n\
             - **Key files**: `Soul.md`, `Agents.md`, `{agent_name}.toml` (auto-reloads on change)\n\
             - Use `fs_ls` on your agent directory to see all workspace files.\n\
             - **Runtime metadata** delivered in `<system-reminder>` blocks at message time.\n\n\
             **Channel access control** (Telegram adapter):\n\
             - `chat_id` — primary chat for outbound notifications\n\
             - `allowed_chats` — additional group or DM chat IDs (Telegram group IDs are negative)\n\
             - `allowed_senders` — optional user-ID allowlist; empty = all users in accepted chats\n\n",
            agent_name = agent.name,
            workspace = workspace_path.display(),
        ));
    }

    // ── 3. Tools Available — compiled (runtime-volatile fs/exec notes) ────────

    preamble.push_str(
        "## Tools Available\n\
         Call typed tools by name. Use `read_skill <name>` to load a skill reference before using it.\n\
         For existing code/skill/plugin files, prefer `code_edit` for targeted edits; use `fs_write` \
         mainly for creating new files or explicit full-file rewrites.\n\
         After every successful `code_edit`, call `code_read` on that file to verify the actual result before doing another edit or finalizing your response.\n\
         Heartbeat schedules support `once|minutely|hourly|daily|weekly` and cron expressions (`cron: */5 * * * *`).\n\
         For reminders or deferred one-time tasks, use `schedule: once` with a `not_before:` field set to an RFC3339 timestamp — the entry will not fire until that time. \
         Do not use `priority: urgent` or cron hacks for reminders; just set `not_before` to the target time.\n\
         For recurring entries, use `status: running`; set `status: done` only to disable.\n\
         Prefer Heartbeat schedules over installing/configuring system cron daemons for agent recurrence.\n\n",
    );

    // ── 3.5. Memory Index — thin SQLite pointer map (always injected) ─────────
    //
    // Memory.md is a navigation index, not a content store. If the file exists,
    // its content is shown directly. If absent, a one-line callout tells the agent
    // where its memory store is and that it is empty — so it knows to call mem_recall.
    // Full chunks live in SQLite; the agent fetches them on demand via mem_recall.

    let can_write = sandbox || trusted_local;

    preamble.push_str("## Memory Index\n\n");
    if let Some(mem) = &ws.memory {
        preamble.push_str(mem);
        if !mem.ends_with('\n') {
            preamble.push('\n');
        }
    } else {
        preamble.push_str(
            "> Memory store is empty — no compaction summaries yet.\n\
             > Call `mem_recall \"<topic>\"` to search, `mem_add` to store, \
             and `mem_compact` to create a pinned summary.\n",
        );
        if can_write {
            preamble.push_str(
                "> After your first `mem_compact`, write `Memory.md` in your agent directory \
                 with the index format described in the default template.\n",
            );
        }
    }
    if can_write {
        preamble.push_str(
            "> After each `mem_compact`, update this file: append a row to Compaction Summaries \
             (`| date | topic | recall query |`) and refresh the Active Topics line. \
             Keep it thin — pointers only, never full content.\n",
        );
    }
    preamble.push('\n');

    // ── 4. Agents.md — user-editable operating instructions ───────────────────
    //
    // Contains tool discipline, memory habits, heartbeat, and task guidance.
    // Supports {max_turns} and {warn_at} template variables substituted here.

    let agents_content = ws
        .agents
        .as_deref()
        .unwrap_or_else(|| workspace::default_agents_md());
    let agents_interpolated = interpolate(
        agents_content,
        &[
            ("max_turns", &agent.max_turns.to_string()),
            (
                "warn_at",
                &((agent.max_turns as f64 * 0.8) as usize).to_string(),
            ),
        ],
    );
    preamble.push_str(&agents_interpolated);
    if !agents_interpolated.ends_with('\n') {
        preamble.push('\n');
    }
    preamble.push('\n');

    // ── 4.5 Engineering Conventions — global coding and safety rules ─────────
    preamble.push_str(
        "## Engineering Conventions\n\n\
         When making changes to files, first understand the file's existing code conventions. \
         Mimic style, use existing libraries/utilities, and follow established patterns.\n\n\
         - Never assume a library is available, even if common. Before introducing any \
         library/framework usage, verify this codebase already uses it (for example via \
         neighboring files, `Cargo.toml`, `package.json`, or equivalent).\n\
         - When creating a new component/module, inspect similar existing components first \
         and match framework choice, naming, typing, and structure.\n\
         - When editing code, inspect surrounding context (especially imports) and implement \
         changes in the most idiomatic way for that local area.\n\
         - Follow security best practices. Never expose or log secrets/keys. Never write \
         secrets/keys into repository files.\n\
         - Assist with defensive security tasks only. Refuse to create, modify, or improve \
         code that could be used maliciously.\n\
         - Never generate or guess URLs unless you are confident they are required for \
         legitimate programming help. Prefer URLs provided by the user or found in local files.\n\
         - Verify solutions with tests when possible. Never assume a test framework or command; \
         check repository docs and source first.\n\
         - If lint/typecheck commands are documented in this project, run them after changes. \
         If the correct commands cannot be found, ask the user and suggest storing them in a \
         dedicated commands/reference file (for example `Tools.md` or project docs), while \
         keeping `Agents.md` thin.\n\
         - Treat `Agents.md` as a **thin quality gate**: high-signal, durable rules only. \
         Continuously refine it from new understanding, but prefer merging/replacing existing \
         rules over appending long lists.\n\
         - Do not bloat `Agents.md` with transcripts, run logs, stack traces, or long command \
         catalogs. Keep detailed operational notes in `Tools.md` and keep only short pointers in \
         `Agents.md`.\n\
         - Never commit changes unless the user explicitly asks for a commit.\n\n",
    );

    // ── 5. User — who the user is (if present) ────────────────────────────────

    if let Some(user) = &ws.user {
        preamble.push_str("## User\n\n");
        preamble.push_str(user);
        if !user.ends_with('\n') {
            preamble.push('\n');
        }
        preamble.push('\n');
    }

    // ── 6. Local Setup — environment-specific notes (if present) ─────────────

    if let Some(tools) = &ws.tools {
        preamble.push_str("## Local Setup\n\n");
        preamble.push_str(tools);
        if !tools.ends_with('\n') {
            preamble.push('\n');
        }
        preamble.push('\n');
    }

    // ── 7. Execution Mode — compiled (sandbox vs. local/trusted) ─────────────

    if sandbox {
        preamble.push_str(
            "## Execution Mode: Sandbox\n\
             You are running inside an isolated Linux Docker container. All operations \
             are fully available including file writes, deletes, and shell execution. \
             This container is yours entirely. You are its sole owner and operator. \
             No human approval is needed for any action inside it — install packages, \
             delete files, run long processes, make network requests. \
             Act with full confidence. When in doubt, try it.\n\n",
        );
        preamble.push_str(&sandbox_backend_preamble(agent));
        preamble.push_str(
            "### Kubernetes Deployment Hygiene\n\
             - Use stable resource names and labels; do not create timestamp/random-suffixed Deployment names unless explicitly requested.\n\
             - For repeat deploys, patch/apply the same Deployment instead of creating parallel ones.\n\
             - Keep default plugin workloads at `replicas: 1` unless user asks for horizontal scaling.\n\
             - If rollout fails or pods are evicted, investigate first (`kubectl describe`, `kubectl logs`, `kubectl get events`) and fix root cause before re-applying.\n\
             - After recovery, clean stale failed/evicted pods for that app label so the namespace does not accumulate dead pods.\n\n",
        );
        preamble.push_str(
            "Pre-installed: Rust, Go, Python 3, Node/TypeScript (pytest, requests available).\n\
             Install extras: `sudo apt-get install -y <pkg>` or `pip3 install <pkg>`.\n\n",
        );
    } else if trusted_local {
        preamble.push_str(
            "## Execution Mode: Trusted Local Sandbox\n\
             You are running directly inside a trusted Kubernetes pod-local sandbox. \
             Filesystem writes/deletes and `shell_exec` are enabled without nested Docker. \
             Treat this pod as your execution boundary and verify behavior with real runtime checks.\n\n",
        );
    }

    // ── 8. Workspace path — compiled ─────────────────────────────────────────

    if sandbox {
        preamble.push_str("## Workspace\nYour working directory is: /workspace\n\n");
    } else {
        preamble.push_str(&format!(
            "## Workspace\nYour working directory is: {}\n\n",
            workspace_path.display()
        ));
    }

    // ── 9. Tasks — compiled (runtime status counts) ───────────────────────────

    let tasks_summary = tasks::tasks_summary_local(&agent.name);
    if let Some(ref s) = tasks_summary {
        preamble.push_str(&format!(
            "## Tasks\n\n\
             Your task backlog is organized as a folder hierarchy under your agent directory. \
             Read `Tasks.md` for the index, then navigate to individual epics and stories. \
             Use the `task-manager` skill for the full authoring guide.\n\n\
             **Current status**: {} in-progress, {} pending, {} done\n\n",
            s.in_progress, s.pending, s.done,
        ));
    }

    // ── 10. Skills — compiled (discovered from disk) ──────────────────────────

    let skills_path = if sandbox {
        skills::skills_dir_sandbox(&agent.name)
    } else {
        skills::skills_dir_local(&agent.name)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("~/.that-agent/agents/{}/skills", agent.name))
    };
    preamble.push_str(&skills::format_skill_preamble(skills, &skills_path));
    preamble.push('\n');

    // ── 11. Plugins — compiled (from agent config) ────────────────────────────

    if let Some(reg) = plugin_registry {
        preamble.push_str(&format_plugin_preamble_full(
            agent,
            sandbox,
            reg,
            cluster_registry,
        ));
    } else {
        preamble.push_str(&format_plugin_preamble(agent, sandbox));
    }

    // ── 11.5. Orchestration — multi-agent coordination tools ──────────────────
    //
    // Inform the agent about worktree-based orchestration capabilities so it
    // knows it can coordinate parallel work across agents.

    preamble.push_str(
        "## Orchestration\n\n\
         You have access to git worktree tools for coordinating parallel work:\n\
         - `worktree_create` — create an isolated branch for a named agent\n\
         - `worktree_list` — list all active agent worktrees\n\
         - `worktree_diff` / `worktree_log` — review an agent's changes\n\
         - `worktree_merge` — merge an agent's completed work\n\
         - `worktree_discard` — clean up a worktree after merging\n\n\
         Use the `agent-worktree` skill for the full orchestration guide.\n\n\
         For remote agent communication, use `shell_exec` with \
         `that run query --remote <url> --token <token> \"<task>\"` to send tasks \
         to agents running HTTP gateway channels.\n\n",
    );

    // ── 11.6. Agent Hierarchy — parent/child context ─────────────────────────
    if let Some(parent) = &agent.parent {
        preamble.push_str(&format!(
            "### Agent Hierarchy\n\
             - **Parent agent**: {parent}\n\
             - You were spawned by your parent to handle a specific task or domain.\n\
             - Focus on your assigned scope. Report results back via your channel.\n"
        ));
        if let Some(role) = &agent.role {
            preamble.push_str(&format!("- **Your role**: {role}\n"));
        }
        preamble.push('\n');
    } else {
        preamble.push_str(
            "### Agent Hierarchy\n\
             You are a root agent. You can orchestrate child agents for parallel work:\n\
             - Deploy subagents via plugins or shell commands with proper scoping\n\
             - Each subagent gets its own isolated workspace unless `--inherit-workspace` is set\n\
             - Use `--parent <your-name> --role <role>` when spawning to establish hierarchy\n\
             - Query subagents via `that run query --remote <url> --token <token> \"<task>\"`\n\
             - Use worktree tools to coordinate code changes across agents\n\
             - Store orchestration learnings in memory for team evolution\n\n\
             Use the `agent-orchestrator` skill for the full multi-agent coordination guide.\n\n",
        );
    }

    // ── 12. Bootstrap — ephemeral first-run ritual (if present) ──────────────
    //
    // Bootstrap.md is injected when the file exists so the agent can read its
    // instructions and perform the ritual. The agent deletes the file on
    // completion; its absence is the "bootstrapped" signal on future sessions.

    if let Some(bootstrap) = &ws.bootstrap {
        preamble.push_str("## Bootstrap\n\n");
        preamble.push_str(bootstrap);
        if !bootstrap.ends_with('\n') {
            preamble.push('\n');
        }
        preamble.push('\n');
    }

    // ── 13. Boot — startup checklist (if present) ─────────────────────────────

    if let Some(boot) = &ws.boot {
        preamble.push_str("## Boot\n\n");
        preamble.push_str(boot);
        if !boot.ends_with('\n') {
            preamble.push('\n');
        }
        preamble.push('\n');
    }

    // ── 14. Additional Instructions — from agent TOML config (if set) ─────────
    //
    // Operator-level overrides that take precedence over Agents.md. Useful for
    // quick per-agent customizations without requiring a full file edit.

    if let Some(user_preamble) = &agent.preamble {
        preamble.push_str("## Additional Instructions\n");
        preamble.push_str(user_preamble);
        preamble.push('\n');
    }

    preamble
}
