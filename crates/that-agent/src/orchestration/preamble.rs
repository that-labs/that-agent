use std::path::Path;

use super::config::trusted_local_sandbox_enabled;
use super::discovery::{format_plugin_preamble, format_plugin_preamble_full};
use crate::config::AgentDef;
use crate::plans;
use crate::skills;
use crate::tasks;
use crate::workspace::{self, WorkspaceFiles};

fn sandbox_backend_preamble(agent: &AgentDef) -> String {
    match crate::sandbox::backend::SandboxMode::from_env() {
        crate::sandbox::backend::SandboxMode::Docker => {
            let socket = crate::sandbox::docker::docker_socket_status();
            let socket_status = if socket.enabled {
                format!("enabled at `{}`", socket.path.display())
            } else {
                format!("unavailable at `{}`", socket.path.display())
            };
            format!(
                "### Runtime Backend: Docker\n\
                 - Mode: `docker` | Host Docker socket: {socket_status}\n\
                 - Run `read_skill cluster-management sandbox-backends` before any Docker build/deploy work.\n\
                 - Run `read_skill cluster-management` before networking or operational tasks.\n\n",
            )
        }
        crate::sandbox::backend::SandboxMode::Kubernetes => {
            let k8s = crate::sandbox::kubernetes::KubernetesSandboxClient::from_env(&agent.name);
            format!(
                "### Runtime Backend: Kubernetes\n\
                 - Mode: `kubernetes` | Namespace: `{}` | Registry: `{}`\n\
                 - Image delivery and build backend are in `<system-reminder>` — check before building.\n\
                 - **Workspace is source of truth.** Write code in workspace, build images, push to registry, deploy with manifests.\n\
                 - **Never use /tmp for code or ConfigMaps for source.** Code belongs in workspace → git → container image. \
                 ConfigMaps are for configuration data, not application source code or build artifacts.\n\
                 - Run `read_skill cluster-management sandbox-backends` before any build/deploy/registry work.\n\
                 - Run `read_skill cluster-management` before networking, security, or operational tasks.\n\n",
                k8s.namespace, k8s.registry
            )
        }
    }
}

/// Replace `{key}` placeholders in a template string with their runtime values.
fn task_delegation_preamble() -> &'static str {
    "### Task delegation — decision gates\n\n\
     - After a restart, check `agent_task(action=status)` and `agent_admin(action=list)` before contacting sub-agents — they may also be restarting.\n\
     - Read the scratchpad FIRST for paths and context — do not explore to rediscover them.\n\
     - If blocked, report `input_required` early rather than continuing to explore.\n\
     - Sub-agent notifications are relayed to the channel immediately AND queued for your next heartbeat turn.\n\n\
     Run `read_skill agent-orchestrator task-delegation` before delegating tasks — it contains the action catalog and scratchpad protocol.\n\n"
}

fn interpolate(template: &str, vars: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{key}}}"), value);
    }
    result
}

pub fn build_preamble(
    workspace_path: &Path,
    agent: &AgentDef,
    sandbox: bool,
    skills: &[skills::SkillMeta],
    ws: &WorkspaceFiles,
    plugin_registry: Option<&crate::plugins::PluginRegistry>,
    cluster_registry: Option<&crate::plugins::cluster::ClusterRegistry>,
) -> String {
    let mut preamble = String::new();
    let trusted_local = !sandbox && trusted_local_sandbox_enabled();

    // ── 0. Execution Discipline — MUST be position 0 for max attention ──────
    //
    // Anti-deliberation and anti-spin rules. Placed first because LLM attention
    // is strongest at the start of the system prompt. Production failures showed
    // these rules were ignored when buried mid-prompt.

    preamble.push_str(
        "## Execution Discipline\n\n\
         These rules override all other guidance. Follow them on every turn.\n\n\
         - **Act, don't narrate.** Text between tool calls costs tokens and attention. One sentence max. \
         Use your thinking budget for reasoning — output only the tool call.\n\
         - **Commit to a plan on turn 1, then execute.** If an approach FAILS (error, dead end), \
         pivot immediately to the simplest alternative. Do not cycle — if you catch yourself \
         considering a third approach to the same problem, STOP and execute the simplest one.\n\
         - **Delegate before deep-reading.** When work spans multiple areas, spawn sub-agents \
         with topic pointers immediately. Do not read the entire codebase yourself first.\n\
         - **Skills first.** Before building, deploying, delegating, or operating infrastructure, \
         check if a matching skill exists and run `read_skill <name>`. Skills contain proven \
         patterns and anti-patterns — always prefer them over reasoning from scratch.\n\
         - **Diagnose before switching, but never spin.** If something fails, read the error and try \
         one focused fix. If that fix also fails, pivot to a simpler approach. \
         Never retry the same action blindly. Never abandon a viable approach after a single failure.\n\
         - **Measure turns in tool calls, not words.** If you are writing paragraphs before a tool call, \
         you are doing it wrong.\n\n",
    );

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
             make network calls without asking. When uncertain, try it.\n\n",
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
             - **Runtime metadata** delivered in `<system-reminder>` blocks at message time.\n\n",
            agent_name = agent.name,
            workspace = workspace_path.display(),
        ));
    }

    // ── 2.5 Context Layers — compiled ──────────────────────────────────────

    preamble.push_str(
        "### Context Layers\n\n\
         You can extend yourself through workspace files, skills, plugins, channels, deployed services, and sub-agents. \
         When uncertain about current capability, inspect your tool surface and workspace files.\n\n\
         Four context layers appear in `<system-reminder>` — each serves a different purpose:\n\
         - **Status.md** — durable operational state. Persists across sessions.\n\
         - **WorkingNotes.md** — session-scoped findings. Cleared between sessions.\n\
         - **Task scratchpad** — inter-agent coordination. Not for personal notes.\n\
         - **Pinned context** — max 5 always-visible facts. Pin what you'd `mem_recall` every turn.\n\n\
         Run `read_skill task-manager context-layers` when deciding which layer to use.\n\n",
    );

    // ── 3. Tools Available — compiled (runtime-volatile fs/exec notes) ────────

    preamble.push_str(
        "## Tools Available\n\
         Call typed tools by name. Run `read_skill <name>` to load a skill reference before unfamiliar work.\n\
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
                &((agent.max_turns as f64 * 0.6) as usize).to_string(),
            ),
        ],
    );
    preamble.push_str(&agents_interpolated);
    if !agents_interpolated.ends_with('\n') {
        preamble.push('\n');
    }
    preamble.push('\n');

    // ── 4.5 Provided Context — domain knowledge from the parent ──────────────
    //
    // Written by the parent before spawning via GoldBootstrap. Contains links,
    // citations, and background research the sub-agent should treat as ground truth.
    // Only present when this agent was spawned with a bootstrap payload.

    if let Some(ctx) = &ws.context {
        preamble.push_str("## Provided Context\n\n");
        preamble.push_str(ctx);
        if !ctx.ends_with('\n') {
            preamble.push('\n');
        }
        preamble.push('\n');
    }

    // ── 5. Engineering Conventions — safety-critical guardrails only ──────────
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
         legitimate programming help. Prefer URLs provided by the user or found in local files.\n\
         - After creating or modifying executable artifacts, run at least one behavior check before claiming done.\n\
         - For shell scripts, validate syntax and execute at least one path unless blocked by environment.\n\
         - If claiming a skill was used this run, ensure evidence exists in this run; otherwise state it came from prior memory.\n\
         - When creating skills without a user-provided name, use deterministic kebab-case derived from the capability.\n\n\
         ### Failure discipline\n\n\
         - Escalate to the human only when you are genuinely stuck after investigation — not as a first response to friction.\n\n\
         ### Scope discipline\n\n\
         - Do not add features, refactoring, or improvements beyond what was asked. A bug fix does not need surrounding code cleaned up.\n\
         - Do not add error handling for scenarios that cannot happen. Trust internal code and framework guarantees.\n\
         - Do not create abstractions for one-time operations. Three similar lines are better than a premature helper.\n\
         - Do not add comments, docstrings, or type annotations to code you did not change.\n\n\
         ### Blast radius awareness\n\n\
         - Before executing a destructive or hard-to-reverse action, classify its blast radius.\n\
         - **Low risk (proceed):** editing files in your workspace, running tests, reading resources.\n\
         - **Medium risk (pause and verify):** deleting files, modifying shared config, writing to external APIs.\n\
         - **High risk (confirm with human):** dropping data, force-pushing, deleting namespaces, modifying RBAC, sending external messages.\n\
         - When running autonomously (heartbeat/listen mode), default to the safer option if uncertain.\n\n\
         ### Memory habits\n\n\
         - When the human corrects your approach → `mem_add` the correction so you do not repeat the mistake.\n\
         - When you learn coordination-relevant facts (who works on what, deadlines, blockers) → `mem_add` for future sessions.\n\
         - When you discover where external information lives (dashboards, ticket boards, docs) → `mem_add` as a reference pointer.\n\
         - When a non-obvious approach works → `mem_add` so you reuse it instead of re-discovering.\n\
         - Do not memorize things derivable from the code or git history.\n\n",
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
        let env_label = match crate::sandbox::backend::SandboxMode::from_env() {
            crate::sandbox::backend::SandboxMode::Kubernetes => "Kubernetes pod",
            crate::sandbox::backend::SandboxMode::Docker => "Docker container",
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

        // Plan guidance — only injected when active plans exist.
        preamble.push_str(
            "### Plan Files\n\n\
             Format: H1 title, `**Status**: active`, checklist steps (`- [ ]`/`- [x]`), \
             optional `## Variables` section with `- key: value` pairs.\n\
             Check off steps as you go, set status to `done` when finished.\n\
             On restart, resume from the first unchecked step.\n\
             For fallback strategies: `**Fallback**: <alternative approach if primary fails>`.\n\n",
        );
    }

    preamble.push_str(
        "### Task Dependencies\n\n\
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

    {
        let mode_label = if is_k8s { "Kubernetes" } else { "Local" };
        let mode_desc = if is_k8s {
            "child agents running as separate pods in your namespace"
        } else {
            "child agents running as local processes"
        };
        let spawn_desc = if is_k8s {
            "persistent child agents (Deployment + Service)"
        } else {
            "persistent child agents (background process)"
        };

        preamble.push_str(&format!(
            "## Orchestration — Multi-Agent ({mode_label})\n\n\
             You can delegate work to {mode_desc}.\n\n\
             ### Delegation tools\n\n\
             - `agent_task(action=send)` — tracked async work with shared scratchpad (default for real work)\n\
             - `agent_task(action=send, targets=[...])` — broadcast to multiple agents\n\
             - `agent_run` — ephemeral one-shot tasks (blocks until done, parallel fan-out)\n\
             - `spawn_agent` — {spawn_desc}\n\
             - `agent_query` — synchronous request/response (<30s questions only)\n\
             - `agent_admin` — inspect/manage children\n",
        ));

        // Mode-specific extra tools
        if is_k8s {
            preamble
                .push_str("- `workspace_admin` — share repos, collect results, monitor branches\n");
        } else {
            preamble.push_str(
                "- `worktree_create/list/diff/log/merge/discard` — git worktree tools for parallel code changes\n",
            );
        }

        preamble.push_str(
            "\n### Decision rules\n\n\
             - **Never use `agent_query` to check sub-agent status** — it blocks your turn. Use `agent_task(action=status)` (instant, free).\n\
             - **Share locations, not content.** Task messages have size limits. Point sub-agents to resources, don't paste content.\n\
             - **React only to `input_required` or terminal states.** For `working` updates, acknowledge silently unless steering is needed.\n\
             - NEVER simulate agent_run with shell_exec — use the actual tool.\n\
             - After all agent_run calls return, you MUST deliver substance to the human.\n",
        );

        // Mode-specific decision rules
        if is_k8s {
            preamble.push_str(
                "- **Agent-first for autonomous workloads.** Services that poll, listen, or monitor MUST be child agents via `spawn_agent`, not raw K8s Deployments.\n\
                 - For coding tasks: ALWAYS call `workspace_admin(action=share, path)` BEFORE `agent_run` with `workspace=true`.\n",
            );
        } else {
            preamble.push_str(
                "- **Channel token exclusivity.** Each channel adapter token must be used by exactly ONE agent process.\n\
                 - For coding tasks: ALWAYS call `worktree_create` BEFORE `agent_run` with `workspace=true`.\n",
            );
        }

        preamble.push_str(
            "\n### Hierarchy\n\
             - Maximum depth: root (0) → persistent child (1) → ephemeral worker (2)\n\
             - Children share your API keys but have separate memory stores\n\n",
        );

        if !is_k8s {
            preamble.push_str(
                "### Gateway endpoints\n\n\
                 Your gateway exposes `/v1/inbound` (async, queued), `/v1/chat` (sync, blocking), `/v1/notify` (fire-and-forget). \
                 Run `read_skill agent-orchestrator gateway-endpoints` before using gateway endpoints.\n\n",
            );
        }

        preamble
            .push_str("Run `read_skill agent-orchestrator` before multi-agent workflow setup.\n\n");
        preamble.push_str(task_delegation_preamble());
    }

    // ── 11.6. Agent Hierarchy — parent/child context ─────────────────────────
    if let Some(parent) = &agent.parent {
        let agent_depth = crate::orchestration::config::parse_env_u8("THAT_AGENT_DEPTH", 1);
        let delegation_note = if agent_depth <= 1 {
            "- You can delegate bounded tasks to ephemeral workers using `agent_run` (parallel fan-out)\n\
             - You cannot spawn persistent sub-agents — only the root agent can\n\
             - You can query any peer agent via `agent_query` or invite them into a shared task with `agent_task(action=share)`\n"
        } else {
            "- You can query any agent in the cluster via `agent_query` for cross-team input\n"
        };
        preamble.push_str(&format!(
            "### Agent Hierarchy\n\
             - **Parent agent**: {parent}\n\
             - You are a depth-{agent_depth} agent. Maximum: root (0) → persistent child (1) → ephemeral worker (2).\n\
             - Focus on your assigned scope. Read the task scratchpad before exploring.\n\
             - **Scratchpad protocol**: `agent_task(action=scratchpad_read, task_id)` to read; \
             `agent_task(action=scratchpad_write, task_id, note, section, kind)` to write. \
             `section=\"header\"` for stable context, `section=\"activity\"` for progress.\n\
             - **Progress updates**: POST to `$THAT_PARENT_GATEWAY_URL/v1/notify` (zero-cost, no LLM turn).\n\
             - Your final text output is returned directly to the parent — make it complete and structured.\n\
             - If the workspace is missing, fail fast — do NOT waste turns searching.\n\
             {delegation_note}"
        ));
        if let Some(role) = &agent.role {
            preamble.push_str(&format!("- **Your role**: {role}\n"));
        }
        preamble.push('\n');
    } else {
        let env_label = if is_k8s { "Kubernetes" } else { "locally" };
        preamble.push_str(&format!(
            "### Agent Hierarchy\n\
             You are a root agent running {env_label}.\n\
             - Use `spawn_agent` for persistent children and `agent_run` for ephemeral tasks\n\
             - Children automatically receive your gateway URL for notifications\n\
             - Use `agent_admin(action=list)` to see children; `agent_admin(action=unregister, name)` to clean up\n\n",
        ));
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
            None,
            None,
        );

        assert!(preamble.contains("vim"));
        assert!(preamble.contains("If the workspace contains a `Dockerfile`, read it"));
    }
}
