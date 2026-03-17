use std::path::Path;

use super::discovery::{format_plugin_preamble, format_plugin_preamble_full};
use crate::config::AgentDef;
use crate::plans;
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
fn task_delegation_preamble() -> &'static str {
    "### Task-based delegation (async, preferred)\n\n\
     - `agent_task(action=send, name, message)` — dispatch task, get task_id immediately\n\
     - `agent_task(action=send, name, message, task_id=X)` — steer a running task\n\
     - `agent_task(action=status)` — check all tasks (free local read, zero cost)\n\
     - `agent_task(action=cancel, task_id)` — graceful stop\n\
     - `agent_task(action=resume, task_id)` — resume canceled task\n\
     - `agent_task(action=scratchpad_read, task_id)` — read shared notes for a task (zero cost)\n\
     - `agent_task(action=scratchpad_write, task_id, note)` — append a note to a task's scratchpad\n\n\
     Task states: submitted → working → input_required → completed/failed/canceled\n\n\
     Task updates arrive in your heartbeat check-in. Use `agent_task(action=status)` proactively — it costs nothing.\n\
     When a task is `input_required`, the sub-agent needs your input — reply with `agent_task(action=send, task_id=X, message=...)` or ask the human.\n\
     **React only to `input_required` or terminal states.** For `working` updates, acknowledge silently unless you spot something worth steering.\n\n\
     | Pattern | Tool | When |\n\
     |---------|------|------|\n\
     | Tracked async work | `agent_task(action=send)` | Default for any real work |\n\
     | Steer running task | `agent_task(action=send, task_id=X)` | Redirect, add context |\n\
     | Quick answer needed | `agent_query` | Simple questions, <30s |\n\
     | Parallel ephemeral | `agent_run` (×N) | Fan-out coding with workspace |\n\n\
     **Never use `agent_query` to check sub-agent status** — it blocks your turn. Use `agent_task(action=status)` instead (instant, free).\n\
     After a restart, check `agent_task(action=status)` and `agent_admin(action=list)` before contacting sub-agents — they may also be restarting.\n\n\
     **Share locations, not content.** Task messages have size limits. Never embed large files, skill bodies, or repo contents \
     in `agent_task(action=send)`. Instead, tell the sub-agent *where* to find the resource (repo URL, file path, skill name) and let it \
     fetch the content itself. Example: \"Clone repo X and install all skills from the skills/ directory\" — not the skill text.\n\n\
     ### Task Scratchpad\n\n\
     Every task has a shared scratchpad — a persistent notepad both parent and sub-agent can read and write.\n\n\
     **Parent (dispatcher):** On `agent_task(action=send)`, the harness automatically writes to the scratchpad:\n\
     - Workspace root path (derived from your agent name)\n\
     - Parent gateway URL\n\
     Add extra context with `agent_task(action=scratchpad_write)` only for things not already auto-provided:\n\
     - Key subdirectory locations or specific file paths\n\
     - Environment variables or credentials the sub-agent needs\n\
     This prevents the sub-agent from guessing paths or exploring blindly.\n\n\
     **Sub-agent (worker):** Before starting any filesystem exploration or heavy tool use:\n\
     1. `agent_task(action=scratchpad_read, task_id)` — ALWAYS read first for workspace paths and context from the dispatcher\n\
     2. After every meaningful milestone or if blocked for 3+ turns, `agent_task(action=scratchpad_write, task_id, note)` your progress or blocker\n\
     If `agent_task(action=scratchpad_read)` returns workspace paths, use them directly — do not explore to rediscover them.\n\n\
     #### Anti-loop protection\n\n\
     The harness tracks consecutive turns where you only use exploration tools \
     (filesystem listing, file reading, grep, search, shell). If you spend 8+ turns exploring \
     without producing output or using non-exploration tools, you will receive a warning. \
     At 12 turns the harness forces you to stop. Duplicate tool calls (same tool + same arguments) \
     accelerate the counter. To avoid triggering this:\n\
     - Read the scratchpad FIRST for paths and context\n\
     - Use provided paths directly instead of searching for them\n\
     - If blocked, report `input_required` early rather than continuing to explore\n\n\
     The scratchpad is the first place to look when you're unsure about paths, workspace layout, or task expectations.\n\n\
     Sub-agent notifications are relayed to the channel immediately AND queued for your next heartbeat turn.\n\n"
}

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
             - **Persistent home**: /home/agent/.that-agent/\n\
             - **Agent home**: /home/agent/.that-agent/agents/{agent_name}/\n\
             - **Task workspace**: /workspace\n\
             - **Key files**: `Soul.md`, `Agents.md`, `Status.md`, `{agent_name}.toml` (auto-reloads on change)\n\
             - Use `/workspace` for project/task files and generated artifacts you want in the visible work tree.\n\
             - Use your agent home for persistent identity, memory, tasks, plugins, and runtime-managed files.\n\
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
             - **Persistent home**: ~/.that-agent/\n\
             - **Agent home**: ~/.that-agent/agents/{agent_name}/\n\
             - **Key files**: `Soul.md`, `Agents.md`, `Status.md`, `{agent_name}.toml` (auto-reloads on change)\n\
             - Use the workspace for project/task files and generated artifacts you want in the visible work tree.\n\
             - Use your agent home for persistent identity, memory, tasks, plugins, and runtime-managed files.\n\
             - **Runtime metadata** delivered in `<system-reminder>` blocks at message time.\n\n\
             **Channel access control** (Telegram adapter):\n\
             - `chat_id` — primary chat for outbound notifications\n\
             - `allowed_chats` — additional group or DM chat IDs (Telegram group IDs are negative)\n\
             - `allowed_senders` — optional user-ID allowlist; empty = all users in accepted chats\n\n",
            agent_name = agent.name,
            workspace = workspace_path.display(),
        ));
    }

    // ── 2.5 Harness Evolution — compiled ──────────────────────────────────────

    preamble.push_str(
        "## Harness Evolution\n\n\
         You can evolve your runtime behavior through workspace files, skills, plugins, \
         gateway routes, dynamic channels, deployed services, and sub-agents.\n\n\
         Docker and Kubernetes are not only isolation backends; they are your execution \
         and deployment surfaces. Use them as the primary way to extend yourself operationally.\n\n\
         Changes to workspace files, plugins, routes, channels, and deployed services can \
         take effect immediately or on the next session/reload.\n\n\
         Changes to the compiled Rust harness, tool schemas, or orchestration logic require \
         editing source code and then rebuilding, restarting, or redeploying the agent. \
         Do not assume those changes are live until verified.\n\n\
         When uncertain about current capability, inspect your tool surface, plugin state, \
         runtime reminders, and workspace files. Do not guess.\n\n\
         ### Status.md — Live Awareness\n\n\
         `Status.md` is your self-maintained awareness file. Its content appears in every \
         `<system-reminder>` as `agent_status:` so you always see it.\n\n\
         Use `identity_update(file=\"Status.md\", content=\"...\")` to update it. Keep it concise. \
         Track: active deployments and their URLs, spawned child agents, key capabilities you've \
         discovered, and anything you need to remember across turns. Remove entries when they're \
         no longer relevant. This is your live operational context — not a log.\n\n",
    );

    // ── 3. Tools Available — compiled (runtime-volatile fs/exec notes) ────────

    preamble.push_str(
        "## Tools Available\n\
         Call typed tools by name. Use `read_skill <name>` to load a skill reference before using it.\n\
         Heartbeat fields: `schedule` (`once|minutely|hourly|daily|weekly|cron: <expr>`), \
         `status` (`running|done`), `priority` (`normal|urgent`), `not_before` (RFC3339 timestamp), \
         `human_approved` (`true` required for `minutely` and schedules firing more than twice per hour after explicit human approval).\n\
         Your Agents.md defines tool habits and workflow preferences.\n\n",
    );

    // ── 3.1. Communication — keep responses human ─────────────────────────────

    preamble.push_str(
        "## Communication\n\n\
         Your Soul.md defines your character. Your Agents.md defines how you talk to humans. \
         Follow them — they are your voice, not suggestions.\n\n\
         Your messages to humans are composed messages, not work logs. Never dump raw tool \
         output, file paths with line numbers, or verification checklists unless the human \
         explicitly asked for that level of detail.\n\n\
         ### answer vs channel_notify\n\n\
         - `answer` — deliver your **final answer** to the human. Must be the last tool you call. \
         The message is sent with proper channel formatting.\n\
         - `channel_notify` — send **mid-turn progress updates** only. Not for final answers.\n\n",
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
        preamble.push_str("> Memory store is empty. Your Agents.md describes how to use it.\n");
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
            "Pre-installed: Python 3, bash, git, curl, wget, jq, ripgrep, fd, tree, vim, kubectl, Docker CLI, buildctl.\n\
             If the workspace contains a `Dockerfile`, read it before describing or changing the runtime image.\n\
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
        preamble.push_str(
            "## Workspace\n\
             Your task/project working directory is: /workspace\n\
             Persistent agent state lives under: /home/agent/.that-agent\n\n",
        );
    } else {
        preamble.push_str(&format!(
            "## Workspace\n\
             Your task/project working directory is: {}\n\
             Persistent agent state lives under: ~/.that-agent\n\n",
            workspace_path.display(),
        ));
    }

    // ── 9. Tasks — compiled (runtime status counts) ───────────────────────────

    let tasks_summary = tasks::tasks_summary_local(&agent.name);
    if let Some(ref s) = tasks_summary {
        preamble.push_str(&format!(
            "## Tasks\n\n\
             Your task backlog is organized as a folder hierarchy under your agent directory. \
             Read `Tasks.md` for the index, then navigate to individual epics and stories. \
             For any complex or multi-step task, create or update the relevant task entry before deep work, \
             keep status current while you work, clear stale `in-progress` markers when finished, \
             send `channel_notify` updates at meaningful checkpoints, and write a `mem_add` summary of what was done.\n\n\
             **Current status**: {} in-progress, {} pending, {} done\n\n",
            s.in_progress, s.pending, s.done,
        ));
    }

    // ── 9.5 Plans — compiled (active plan summaries) ──────────────────────────

    let active_plans = plans::scan_plans_local(&agent.name);
    if !active_plans.is_empty() {
        preamble.push_str("## Active Plans\n\n");
        for p in &active_plans {
            let vars = if p.variables.is_empty() {
                String::new()
            } else {
                let pairs: Vec<String> = p
                    .variables
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                format!(" — Variables: {}", pairs.join(", "))
            };
            preamble.push_str(&format!(
                "- **plan-{}.md**: {} ({}/{} steps){}\n",
                p.number, p.title, p.steps_done, p.steps_total, vars,
            ));
        }
        preamble.push('\n');
    }

    // Plan guidance (always present, near tasks section).
    preamble.push_str(
        "### Plan Files\n\n\
         For multi-step work, create `plan-{n}.md` in your agent directory.\n\
         Format: H1 title, `**Status**: active`, checklist steps (`- [ ]`/`- [x]`), \
         optional `## Variables` section with `- key: value` pairs.\n\
         Check off steps as you go, set status to `done` when finished.\n\
         On restart, read active plans and resume from the first unchecked step.\n\
         Use variables to persist extracted names, URLs, and values between turns.\n\
         Keep plans under 50 lines; reference task files for complex sub-work.\n\
         For fallback strategies: `**Fallback**: <alternative approach if primary fails>`.\n\n\
         ### Task Dependencies\n\n\
         Use `**Blocked-by**: <task ref>` in task files to express dependencies between tasks.\n\n",
    );

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
    // Mode-aware: K8s mode uses pod-based orchestration, local mode uses worktrees.

    let is_k8s = matches!(
        std::env::var("THAT_SANDBOX_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "k8s" | "kubernetes"
    );

    if is_k8s {
        preamble.push_str(
            "## Orchestration — Multi-Agent (Kubernetes)\n\n\
             You can delegate work to child agents running as separate pods in your namespace.\n\n\
             ### Delegation tools\n\n\
             **Ephemeral agents** (`agent_run`) — one-off tasks that run and return results:\n\
             - `agent_run(name, task, role?)` → blocks until done, returns the agent's full output\n\
             - Call multiple `agent_run` in one turn — they run **in parallel** automatically\n\
             - Use for: analysis, research, coding tasks, batch processing, reviews\n\n\
             **Persistent agents** (`spawn_agent`) — long-running services:\n\
             - `spawn_agent(name, role)` → creates a Deployment + Service\n\
             - `agent_query(name, message)` → synchronous request/response via gateway\n\
             - `agent_query(name, message, stream=true)` → streaming: sub-agent tool calls shown on channel in real-time\n\
             - `agent_task(action=query_async, name, message)` → fire-and-forget: returns immediately, result arrives as notification\n\
             - `agent_admin(action=unregister, name)` → tear down when no longer needed\n\
             - Use for: coordinators, channel listeners, always-on workers\n\n\
             ### Orchestration workflow\n\n\
             **Step 1 — Prepare.** Analyze the task, identify independent work units. \
             For coding tasks: `workspace_admin(action=share, path)` FIRST.\n\
             **Step 2 — Dispatch.** Call multiple `agent_run` in a single turn for parallel execution. \
             Give each agent a clear, self-contained task with all context it needs.\n\
             **Step 3 — Collect.** When all `agent_run` calls return, you receive every agent's output. \
             Read each result carefully.\n\
             **Step 4 — Deliver.** Synthesize findings into a complete, structured answer for the human. \
             Never send empty or placeholder messages. If an agent failed, explain what happened.\n\
             **Step 5 — Merge (coding).** Use `workspace_admin(action=activity)` to see which workers pushed, \
             then `workspace_admin(action=collect, path, worker)` to merge each one sequentially.\n\n\
             ### Rules\n\
             - NEVER simulate agent_run with shell_exec — use the actual tool\n\
             - For coding tasks: ALWAYS call `workspace_admin(action=share, path)` BEFORE `agent_run` with `workspace=true`\n\
             - Workers push to their own task branch — no conflicts between parallel workers\n\
             - After all agent_run calls return, you MUST deliver substance to the human — \
             read the output, extract key findings, organize into clear sections\n\n\
             ### Monitoring worker progress (coding tasks)\n\
             - `workspace_admin(action=activity)` → see all branches, ahead/behind counts, last commit per worker\n\
             - `workspace_admin(action=diff, branch)` → review a worker's changes without cloning\n\
             - `workspace_admin(action=collect, path, worker)` → merge worker's branch into your workspace\n\
             - `workspace_admin(action=conflicts, branch)` → on merge failure, see conflicting files and both diffs\n\
             - Load `read_skill git-workspace` for the full conflict resolution guide\n\n",
        );
        preamble.push_str(task_delegation_preamble());
        preamble.push_str(
            "### Limitations\n\
             - Children cannot spawn their own sub-agents (restricted RBAC)\n\
             - Ephemeral agents have resource limits and a turn budget\n\
             - Children share your API keys but have separate memory stores\n\n",
        );
    } else {
        preamble.push_str(
            "## Orchestration\n\n\
         You have access to git worktree tools for coordinating parallel work:\n\
         - `worktree_create` — create an isolated branch for a named agent\n\
         - `worktree_list` — list all active agent worktrees\n\
         - `worktree_diff` / `worktree_log` — review an agent's changes\n\
         - `worktree_merge` — merge an agent's completed work\n\
         - `worktree_discard` — clean up a worktree after merging\n\n\
         Load `read_skill git-workspace worktree-local` for the full orchestration guide.\n\n\
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
         | `POST /v1/inbound` | Queued for next heartbeat tick (returns 202). Batched with scheduled tasks. Response delivered via `callback_url` if provided, otherwise the agent uses `answer`. | Plugins, services, and bridges that need the agent to act autonomously in the background. |\n\
         | `POST /v1/chat` | Synchronous (blocks until done, returns full response). | One-shot queries where the caller needs the answer inline. |\n\
         | `POST /v1/notify` | Zero-cost queue (returns 202). No LLM turn — batched into the next heartbeat tick. | Status reports, progress updates, fire-and-forget notifications. |\n\
         | `GET /v1/scratchpad?task_id=X` | Read scratchpad entries for a task (returns 200). | Sub-agents reading parent-side scratchpad via HTTP fallback. |\n\
         | `POST /v1/scratchpad?task_id=X` | Append a note `{note, from}` to a task's scratchpad (returns 200). | Sub-agents writing to parent-side scratchpad when local registry unavailable. |\n\n\
         **Key rule for plugins and deployed services:** When building a service that sends \
         work to the agent (e.g. a content scanner with approve/reject buttons), always use \
         `/v1/inbound` so the agent processes the request asynchronously in the background. \
         Inbound messages are batched — they queue until the next heartbeat tick, not processed immediately. \
         Never use `/v1/chat` from a plugin — it blocks the caller until inference completes \
         and makes tool calls visible on the user's channel, which breaks the async UX.\n\n\
         **`/v1/inbound` request body:**\n\
         ```json\n\
         {\"message\": \"<task>\", \"sender_id\": \"<service-name>\", \
         \"callback_url\": \"<optional-url-for-response>\"}\n\
         ```\n\
         - If `callback_url` is provided, the agent POSTs `{\"text\": \"<response>\"}` back when done.\n\
         - If omitted, the agent uses `answer` to deliver results on the originating channel.\n\
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
         **Async request (queued for parent's next heartbeat tick, response delivered to callback):**\n\
         ```\n\
         POST $THAT_PARENT_GATEWAY_URL/v1/inbound\n\
         Authorization: Bearer $THAT_PARENT_GATEWAY_TOKEN\n\
         {\"message\": \"<task>\", \"sender_id\": \"<your-name>\", \
         \"callback_url\": \"http://<your-gateway>/v1/inbound\"}\n\
         ```\n\
         The parent queues the request and processes it at the next heartbeat tick, then POSTs \
         `{\"text\": \"<response>\"}` back to your `callback_url`.\n\n\
         Use `/v1/notify` for progress updates. Use `/v1/inbound` + `callback_url` only \
         when you genuinely need the parent to reason and respond.\n\n",
        );
        preamble.push_str(task_delegation_preamble());
    }

    // ── 11.6. Agent Hierarchy — parent/child context ─────────────────────────
    if let Some(parent) = &agent.parent {
        if is_k8s {
            preamble.push_str(&format!(
                "### Agent Hierarchy\n\
                 - **Parent agent**: {parent}\n\
                 - You were spawned to handle a specific task — focus on your assigned scope\n\
                 - You cannot spawn sub-agents of your own (restricted RBAC)\n\n\
                 ### Your workflow\n\
                 1. Check if `$GIT_REPO_URL` is set. If yes, clone it: `git clone $GIT_REPO_URL workspace && cd workspace`\n\
                 2. If `$GIT_REPO_URL` is NOT set and your task requires source code, \
                 **stop immediately** and return this message: \
                 \"ERROR: No workspace available. Parent must call workspace_admin(action=share, path=...) and retry with workspace=true.\"\n\
                 3. Do your assigned work using the tools available to you\n\
                 4. Commit and push your changes: `git push $GIT_REPO_URL HEAD:refs/heads/task/$THAT_AGENT_NAME`\n\
                 5. Your final text output is returned directly to the parent — make it complete and structured\n\n\
                 ### Communication\n\
                 - Your final output (last assistant message) is what the parent receives as the agent_run result\n\
                 - For progress updates during long work: POST to `$THAT_PARENT_GATEWAY_URL/v1/notify`\n\
                 - Do NOT waste turns searching for code that isn't there — if the workspace is missing, fail fast\n\
                 - Do NOT use `agent_query` — you are an ephemeral worker, not a persistent agent\n\
                 - Do NOT manually construct service URLs — use environment variables\n\
                 - Do NOT try to access the parent's filesystem — use the git workspace for code sharing\n"
            ));
        } else {
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
        }
        if let Some(role) = &agent.role {
            preamble.push_str(&format!("- **Your role**: {role}\n"));
        }
        preamble.push('\n');
    } else if is_k8s {
        preamble.push_str(
            "### Agent Hierarchy\n\
             You are a root agent running in Kubernetes.\n\
             - Use `spawn_agent` for persistent child agents and `agent_run` for ephemeral tasks\n\
             - Children automatically receive your gateway URL for notifications\n\
             - Children run in the same namespace with restricted permissions\n\
             - Use `agent_admin(action=list)` to see all your children and their status\n\
             - Use `agent_admin(action=unregister, name)` to clean up persistent children when done\n\n",
        );
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
            "`{prefix}` messages are soft mid-run nudges from the human or parent agents — use them immediately when they provide paths or context.\n\n",
        ));
    }

    if let Some(user_preamble) = &agent.preamble {
        preamble.push_str("## Additional Instructions\n");
        preamble.push_str(user_preamble);
        preamble.push('\n');
    }

    preamble
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_preamble_mentions_vim_and_dockerfile() {
        let agent = AgentDef::default();
        let preamble = build_preamble(
            Path::new("/workspace"),
            &agent,
            true,
            &[],
            &WorkspaceFiles::default(),
            0,
            "session",
            &[],
            None,
            None,
        );

        assert!(preamble.contains("vim"));
        assert!(preamble.contains("If the workspace contains a `Dockerfile`, read it"));
    }
}
