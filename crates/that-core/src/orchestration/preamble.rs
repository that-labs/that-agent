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
            let tailscale = std::env::var("THAT_CLUSTER_TAILSCALE").unwrap_or_default();
            let mut infra = String::new();
            if tailscale == "true" {
                let tailnet = std::env::var("THAT_CLUSTER_TAILNET_NAME").unwrap_or_default();
                if tailnet.is_empty() {
                    infra.push_str(
                        "                     - **Tailscale**: available — load `read_skill cluster-management tailscale-docker` for mesh exposure.\n"
                    );
                } else {
                    infra.push_str(&format!(
                        "                     - **Tailscale**: available — tailnet: `{tailnet}.ts.net` — mesh URLs follow `https://<hostname>.{tailnet}.ts.net`. Load `read_skill cluster-management tailscale-docker` for details.\n"
                    ));
                }
            }
            let skill_hint = "                     - **Cluster management skill**: Use `read_skill cluster-management` for networking and operational patterns. \
                     Load Docker-specific references (networking-docker, operations-docker) for detailed guidance.\n\n";
            if socket.enabled {
                format!(
                    "### Runtime Backend: Docker\n\
                     - Mode: `docker`\n\
                     - Host Docker socket: enabled at `{}`\n\
                     {infra}\
                     - You can orchestrate sibling containers and compose stacks from inside this sandbox.\n\
                     - For \"run/deploy this app\" requests, prefer Docker-native flows (`docker build`, `docker run`, `docker compose`).\n\
                     - If the user explicitly asks to run/deploy \"in Docker\", execute a Docker workflow and report container/port details.\n\
                     - If `docker` CLI is missing in-container, install it (`sudo apt-get update && sudo apt-get install -y docker.io`).\n\
                     - Do not default to `python3 -m http.server` for deployment requests; use it only for temporary static preview when explicitly acceptable.\n\
                     {skill_hint}",
                    socket.path.display()
                )
            } else {
                format!(
                    "### Runtime Backend: Docker\n\
                     - Mode: `docker`\n\
                     - Host Docker socket: unavailable at `{}`\n\
                     {infra}\
                     - You can still run processes in this sandbox container, but you cannot spawn sibling host containers via Docker socket.\n\
                     - If the user explicitly needs host-level Docker orchestration, state the socket limitation clearly.\n\
                     {skill_hint}",
                    socket.path.display()
                )
            }
        }
        that_sandbox::backend::SandboxMode::Kubernetes => {
            let k8s = that_sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
            let cni = std::env::var("THAT_CLUSTER_CNI").unwrap_or_default();
            let tailscale = std::env::var("THAT_CLUSTER_TAILSCALE").unwrap_or_default();
            let k9s = std::env::var("THAT_CLUSTER_K9S").unwrap_or_default();
            let mut infra = String::new();
            if !cni.is_empty() {
                infra.push_str(&format!(
                    "                 - **CNI**: `{cni}` — load `read_skill cluster-management cilium` for L7 policies, zero-trust, and flow observability.\n"
                ));
            }
            if tailscale == "true" {
                let tailnet = std::env::var("THAT_CLUSTER_TAILNET_NAME").unwrap_or_default();
                if tailnet.is_empty() {
                    infra.push_str(
                        "                 - **Tailscale Operator**: installed — load `read_skill cluster-management tailscale-k8s` for mesh exposure.\n"
                    );
                } else {
                    infra.push_str(&format!(
                        "                 - **Tailscale Operator**: installed — tailnet: `{tailnet}.ts.net` — mesh URLs follow `https://<hostname>.{tailnet}.ts.net`. Load `read_skill cluster-management tailscale-k8s` for details.\n"
                    ));
                }
            }
            if k9s == "true" {
                infra.push_str(
                    "                 - **K9s**: available on host for interactive cluster inspection.\n"
                );
            }
            format!(
                "### Runtime Backend: Kubernetes\n\
                 - Mode: `kubernetes`\n\
                 - Namespace: `{}`\n\
                 - Registry: `{}`\n\
                 {infra}\
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
                 - Validate with `kubectl rollout status` and list managed resources after deploy.\n\
                 - **Environment context:** This is a Kubernetes cluster. Resources are namespaced and network-accessible.\n\
                 - **Cluster management skill**: Use `read_skill cluster-management` for networking, security policies, and operational patterns. \
                 Load backend-specific references (networking-k8s, operations-k8s) for detailed guidance.\n\n",
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

        preamble.push('\n');
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
         Heartbeat fields: `schedule` (`once|minutely|hourly|daily|weekly|cron: <expr>`), \
         `status` (`running|done`), `priority` (`normal|urgent`), `not_before` (RFC3339 timestamp).\n\
         Your Agents.md defines tool habits and workflow preferences.\n\n",
    );

    // ── 3.1. Communication — keep responses human ─────────────────────────────

    preamble.push_str(
        "## Communication\n\n\
         By default, focus on the outcome rather than internal process. \
         Your Soul.md and Agents.md may refine your voice and style — follow them.\n\n",
    );

    // ── 3.5. Memory Index — thin SQLite pointer map (always injected) ─────────
    //
    // Memory.md is a navigation index, not a content store. If the file exists,
    // its content is shown directly. If absent, a one-line callout tells the agent
    // where its memory store is and that it is empty — so it knows to call mem_recall.
    // Full chunks live in SQLite; the agent fetches them on demand via mem_recall.

    preamble.push_str("## Memory Index\n\n");
    if let Some(mem) = &ws.memory {
        preamble.push_str(mem);
        if !mem.ends_with('\n') {
            preamble.push('\n');
        }
    } else {
        preamble.push_str(
            "> Memory store is empty. Your Agents.md describes how to use it.\n",
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

    // ── 4.5 Engineering Conventions — safety-critical guardrails only ─────────
    //
    // Coding style, workflow habits, and commit rules belong in Agents.md.
    // The preamble only enforces hard safety constraints that must not be overridden.
    preamble.push_str(
        "## Engineering Conventions\n\n\
         These are safety guardrails. Your Agents.md defines coding style, workflow, and habits.\n\n\
         - Follow security best practices. Never expose or log secrets/keys. Never write \
         secrets/keys into repository files.\n\
         - Assist with defensive security tasks only. Refuse to create, modify, or improve \
         code that could be used maliciously.\n\
         - Never generate or guess URLs unless you are confident they are required for \
         legitimate programming help. Prefer URLs provided by the user or found in local files.\n\n",
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
        let backend_block = sandbox_backend_preamble(agent);
        let env_label = match that_sandbox::backend::SandboxMode::from_env() {
            that_sandbox::backend::SandboxMode::Kubernetes => "Kubernetes pod",
            that_sandbox::backend::SandboxMode::Docker => "Docker container",
        };
        preamble.push_str(&format!(
            "## Execution Mode: Sandbox\n\
             You are running inside an isolated {env_label}. All operations \
             are fully available including file writes, deletes, and shell execution. \
             This environment is yours entirely — no human approval is needed for any action inside it.\n\n",
        ));
        preamble.push_str(&backend_block);
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
         to agents running HTTP gateway channels.\n\n\
         ### Channel token exclusivity\n\n\
         Each channel adapter token (e.g. a Telegram bot token, Discord bot token, Slack app token) \
         must be used by exactly ONE agent process at a time. Never share or reuse a channel token \
         between a parent agent and a sub-agent, or between any two concurrently running agents. \
         Doing so will cause the primary agent's listener to freeze or drop messages, because \
         two processes will compete for the same polling/webhook connection. \
         Sub-agents that need their own channel presence must use a separate, dedicated token.\n\n\
         ### Gateway endpoints — when to use which\n\n\
         Your HTTP gateway exposes three message endpoints. Choosing the right one matters:\n\n\
         | Endpoint | Behavior | Use when |\n\
         |----------|----------|----------|\n\
         | `POST /v1/inbound` | Fire-and-forget (returns 202). Triggers a background agent run. Response delivered via `callback_url` if provided, otherwise the agent uses `channel_notify`. | Plugins, services, and bridges that need the agent to act autonomously in the background. |\n\
         | `POST /v1/chat` | Synchronous (blocks until done, returns full response). | One-shot queries where the caller needs the answer inline. |\n\
         | `POST /v1/notify` | Zero-cost queue (returns 202). No LLM turn — batched into the next heartbeat tick. | Status reports, progress updates, fire-and-forget notifications. |\n\n\
         **Key rule for plugins and deployed services:** When building a service that sends \
         work to the agent (e.g. a content scanner with approve/reject buttons), always use \
         `/v1/inbound` so the agent processes the request asynchronously in the background. \
         Never use `/v1/chat` from a plugin — it blocks the caller until inference completes \
         and makes tool calls visible on the user's channel, which breaks the async UX.\n\n\
         **`/v1/inbound` request body:**\n\
         ```json\n\
         {\"message\": \"<task>\", \"sender_id\": \"<service-name>\", \
         \"callback_url\": \"<optional-url-for-response>\"}\n\
         ```\n\
         - If `callback_url` is provided, the agent POSTs `{\"text\": \"<response>\"}` back when done.\n\
         - If omitted, the agent uses `channel_notify` to report results on the originating channel.\n\
         - Messages from the same `sender_id` are serialized — they queue, not run in parallel. \
         Use distinct `sender_id` values if you need concurrent processing.\n\n\
         ### Sub-agent communication protocol\n\n\
         When a sub-agent needs to reach its parent it has two paths:\n\n\
         **Status report (fire-and-forget, no LLM turn triggered):**\n\
         ```\n\
         POST $THAT_PARENT_GATEWAY_URL/v1/notify\n\
         Authorization: Bearer $THAT_PARENT_GATEWAY_TOKEN\n\
         {\"message\": \"<status text>\", \"agent\": \"<your-name>\"}\n\
         ```\n\
         The notification is queued and surfaced at the parent's next heartbeat tick — \
         it does NOT interrupt an ongoing user conversation or consume API quota.\n\n\
         **Async request (triggers parent LLM turn, response delivered to callback):**\n\
         ```\n\
         POST $THAT_PARENT_GATEWAY_URL/v1/inbound\n\
         Authorization: Bearer $THAT_PARENT_GATEWAY_TOKEN\n\
         {\"message\": \"<task>\", \"sender_id\": \"<your-name>\", \
         \"callback_url\": \"http://<your-gateway>/v1/inbound\"}\n\
         ```\n\
         The parent processes the request as a full agent run and POSTs \
         `{\"text\": \"<response>\"}` back to your `callback_url`.\n\n\
         Use `/v1/notify` for progress updates. Use `/v1/inbound` + `callback_url` only \
         when you genuinely need the parent to reason and respond.\n\n",
    );

    // ── 11.6. Agent Hierarchy — parent/child context ─────────────────────────
    if let Some(parent) = &agent.parent {
        preamble.push_str(&format!(
            "### Agent Hierarchy\n\
             - **Parent agent**: {parent}\n\
             - You were spawned by your parent to handle a specific task or domain.\n\
             - Focus on your assigned scope. Report results back via your channel.\n\
             - `$THAT_PARENT_GATEWAY_URL` — your parent's HTTP gateway base URL\n\
             - `$THAT_PARENT_GATEWAY_TOKEN` — bearer token for that gateway (if auth is on)\n\
             - Use `POST $THAT_PARENT_GATEWAY_URL/v1/notify` for status updates (zero-cost, \
             no LLM turn on the parent side, batched into the next heartbeat).\n\
             - Use `POST $THAT_PARENT_GATEWAY_URL/v1/inbound` with a `callback_url` when \
             you need the parent to reason and reply asynchronously.\n"
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
             - Sub-agents automatically receive `THAT_PARENT_GATEWAY_URL` pointing to your \
             gateway — they will use it for `/v1/notify` (status) and `/v1/inbound` (async tasks)\n\
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

    if agent.steering {
        let prefix = crate::agent_loop::STEERING_HINT_PREFIX;
        preamble.push_str(&format!(
            "`{prefix}` messages are soft mid-run nudges from the human — consider them but don't redirect unless warranted.\n\n",
        ));
    }

    if let Some(user_preamble) = &agent.preamble {
        preamble.push_str("## Additional Instructions\n");
        preamble.push_str(user_preamble);
        preamble.push('\n');
    }

    preamble
}
