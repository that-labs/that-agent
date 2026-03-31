# that-agent Architecture

Most agent frameworks treat each interaction mode as a separate product: one system for chat, another for tasks, a third for evaluation. The result is fragmented capabilities, duplicated logic, and agents that forget everything between sessions. **that-agent** solves this with a single Rust runtime, a single tool stack, and a single continuity model shared across every execution path. The mental model is simple: one loop, one policy gate, one memory — regardless of whether the agent is running a CLI task, holding a TUI conversation, listening on Telegram, or being scored by an eval harness.

---

## 1. Principles

### Core philosophy

**The agent manages its own home.** Its capabilities, deployed services, and environment are expressed as plugins it authors, ships, and upgrades at runtime. Software is not something an operator configures for the agent — it is something the agent builds and deploys for itself. A new integration, a scheduled routine, a custom command: the agent writes it as a plugin, deploys it into its sandbox, and owns its lifecycle from that point forward. This is the purpose the entire architecture serves.

The foundation — orchestration, tools, memory, channels, sandbox, eval — is deliberately stable. It is the substrate that makes autonomous self-management safe, testable, and reproducible. Contributions that strengthen the foundation are welcome. Contributions that add new surface area should ask first whether the agent can build and maintain that surface itself.

### Design invariants

1. **One loop, all modes.** Every execution path uses the same multi-turn streaming loop, the same tool stack, and the same policy enforcement. Behavioral divergence between modes is a bug.

2. **Continuity survives restarts.** Sessions, memory, workspace files, and heartbeat state persist to disk. An agent can be stopped and restarted without losing context, identity, or scheduled work.

3. **The sandbox is the trust boundary.** On the host, destructive tools default to Deny. In a sandbox (Kubernetes pod or Docker container), the boundary is the safety perimeter, and destructive tools are elevated to Allow. There is no middle ground.

4. **Skills and plugins extend without recompilation.** New capabilities are added via markdown skill files and TOML plugin manifests at runtime. The framework discovers, validates, and injects them without code changes.

5. **Eval tests autonomy, not tool knowledge.** Scenario prompts must read like human requests. They never name internal tools, skill identifiers, or workflow steps. The agent decides how to accomplish the task.

6. **Every paragraph earns its space.** Architecture, preamble instructions, and eval rubrics are kept dense and intentional. Vague guidance produces vague agent behavior.

---

## 2. Module Map

Single consolidated crate (`that-agent`) with a standalone `that-git-server`. All modules live under one crate — no circular-dep workarounds needed.

| Module | Owns |
|---|---|
| `agent_loop/` | Multi-turn streaming LLM loop with provider backends (Anthropic, OpenAI, OpenRouter), `LoopHook` trait, `ToolContext` struct, retry logic, tracing spans |
| `agents/` | Agent registry (`agents.json`), task registry (`agent_tasks.json`), spawn/query/resolve helpers, `post_to_agent_inbound` |
| `audit.rs` | Structured audit logging — JSONL tool-call audit trail and run event log |
| `auth.rs` | API key resolution — Anthropic OAuth token detection, provider key lookup from environment |
| `channels/` | `Channel` trait, capability model, `ChannelRouter`, inbound routing, notify tool. Adapters: Telegram, HTTP gateway, TUI |
| `commands/` + `cli.rs` | Unified binary. Dispatches orchestration commands or low-level tool invocations |
| `config/` | `AgentDef` (self-contained TOML agent definition), channel config, default values |
| `control/` | Runtime control plane — CLI subcommands for live agent inspection and management |
| `default_skills.rs` | Bundled skill embedding, version-stamped install/upgrade on startup |
| `eval/` | Scenario TOML format, step runner, assertion engine, LLM judge, persisted reports |
| `heartbeat/` | Periodic self-scheduling — parses `Heartbeat.md`, resolves due entries by schedule/priority/urgency, dispatches autonomous runs |
| `hooks.rs` | `ChannelHook` — intercepts tool calls in channel mode, routes to `ChannelRouter` |
| `model_catalog.rs` | Known model/provider pairs, provider normalization, default model selection |
| `observability.rs` | Tracing init with optional Phoenix/OpenTelemetry export, structured Gen AI semantic conventions |
| `orchestration/` | Agent runtime, preamble building, session transcript lifecycle, sandbox coordination. Run modes: task, chat, TUI, listen, eval, channel |
| `plans/` | Lightweight plan file scanner — reads `plan-{n}.md` from agent directory, extracts step progress for preamble injection |
| `plugins/` | Agent-scoped plugin discovery/enablement, commands, activations, routines, runtime queue, cluster registry |
| `provider_registry.rs` | Dynamic LLM provider registration — file-backed registry for user-added providers beyond the built-in three |
| `sandbox/` | Kubernetes-native sandbox lifecycle with Docker fallback, `SandboxMode` enum, `BackendClient` dispatch, exec routing |
| `session/` | Session ID generation, JSONL transcript persistence, `TranscriptEvent` types, history reconstruction |
| `skills/` | Skill discovery — YAML frontmatter parsing, eligibility filters (OS, binaries, envvars), metadata extraction |
| `tasks/` | Folder-based hierarchical task tracking — path helpers, status scanner for autonomous agents |
| `tool_dispatch/` | Agent-loop dispatch layer — bridges orchestration to the tools module, all tool definitions and routing |
| `tools/` | Tool implementations, policy model (Allow/Prompt/Deny), memory engine, search engine, code analysis, filesystem execution |
| `transcription.rs` | Audio transcription via OpenAI Whisper API |
| `tui/` | TUI adapter — terminal rendering, formatting, `TuiHook`, `TuiChannel`, palette, modal dialogs, stats display |
| `workspace/` | Workspace file management — loading, saving, and default templates for agent identity markdown files |

Binaries: `that` (CLI), `that-eval` (eval harness), `that-git-server` (standalone)

---

## 3. Runtime Model

### Four Execution Paths

All four live in the `orchestration/` module. All four **must** call `load_agent_config(container)` — this is the critical policy invariant (see Section 6).

| Function | Mode | Hook Type | When Used |
|---|---|---|---|
| `execute_agent_run_streaming()` | CLI run/chat | AgentHook | `that run`, `that chat` |
| `execute_agent_run_tui()` | TUI interactive | TuiHook | `that tui` |
| `execute_agent_run_eval()` | Eval harness | EvalHook | `that-eval` scenario runner |
| `execute_agent_run_channel()` | Channel listen | ChannelHook | `that listen` with external channels |

### Shared Runtime Sequence

Every execution path follows the same sequence:

1. Resolve workspace and state directories
2. Load or create session ID
3. Prepare sandbox container/pod (or set local mode)
4. Discover skills and collect plugin state
5. Build preamble: Identity + Harness + Tool discipline + Skills + Plugins + Memory guidance
6. Build agent with standard toolset via `all_tool_defs()`
7. Execute multi-turn streaming loop with retries
8. Persist transcript events (user, tool_call, tool_result, assistant, run_end)

### Hook System

Each execution path injects a `LoopHook` implementation that controls how the agent loop communicates with the outside world. The hook receives streaming tokens, tool calls, tool results, errors, and completion events. The four hook types (AgentHook, TuiHook, EvalHook, ChannelHook) adapt these events to their respective UIs and recording backends.

### Standard Tool Stack

All modes share a core tool set. Channel mode adds additional tools for message routing and platform interaction.

| Category | Tools |
|---|---|
| Filesystem | `fs_ls`, `fs_cat`, `fs_write`, `fs_mkdir`, `fs_rm`, `image_read` |
| Code | `code_read`, `code_grep`, `code_tree`, `code_symbols`, `code_summary`, `code_edit` |
| Memory | `mem_add`, `mem_recall`, `mem_compact`, `mem_remove`, `mem_unpin` |
| Search | `search_query`, `search_fetch` |
| Execution | `shell_exec` |
| Human | `human_ask` |
| Skills | `list_skills`, `read_skill` |
| Plugins | `read_plugin`, `validate_plugin`, `plugin_list`, `plugin_install`, `plugin_uninstall`, `plugin_status`, `plugin_set_policy` |
| Worktree | `worktree_create`, `worktree_list`, `worktree_diff`, `worktree_log`, `worktree_merge`, `worktree_discard` |
| Identity | `identity_update` |
| HTTP | `http_request` |
| Providers | `provider_admin` |
| Gateway | `gateway_route_register`, `gateway_route_unregister`, `gateway_route_list` |
| Channels | `channel_list`, `channel_register`, `channel_unregister` |
| Agents | `spawn_agent`, `agent_run`, `agent_query`, `agent_task`, `agent_admin`, `workspace_admin` |
| Channel mode only | `answer`, `channel_notify`, `channel_send_file`, `channel_send_message`, `channel_send_raw`, `channel_settings` |

Agent depth restricts some tools: `spawn_agent` is removed for child agents, `agent_run` is removed at depth > 1.

### Retry Behavior

Transient network and server errors trigger exponential backoff: 1s, 2s, 4s, 8s, 16s — up to 5 retries before the run fails.

---

## 4. Continuity Model

### Sessions

Each session produces a JSONL transcript file. Session IDs follow the format `YYYYMMDD-HHMMSS-XXXX`. Transcript events: `run_start`, `user_message`, `assistant_message`, `tool_call`, `tool_result`, `run_end`, `restart`, `compaction`, `usage`. Sessions support history reconstruction and, in channel mode, sender-to-session mapping.

### Memory

SQLite-backed per-agent memory database. Tools: `mem_add` (store), `mem_recall` (retrieve by key or semantic search), `mem_compact` (consolidate and prune). Memory persists across sessions and restarts. In sandbox mode, memory storage remains on the host (not inside the container) — memory tools execute in the host runtime process.

### Workspace Files

Each agent maintains a set of named markdown files that define its identity, instructions, and context. These are loaded at session start and injected into the preamble. The agent can edit them directly — changes take effect on the next session.

| File | Role | Edit Frequency |
|---|---|---|
| `Soul.md` | Deep identity: character, values, philosophy | Slow — evolves with the agent |
| `Identity.md` | Shallow identity: name, vibe, emoji | Bootstrap-created, rarely changes |
| `Agents.md` | Operating instructions, tool discipline, memory, heartbeat | Agent-editable at any time |
| `User.md` | Who the user is and how to address them | Grows organically |
| `Tools.md` | Local environment notes: devices, SSH, preferences | Environment-specific |
| `Boot.md` | Optional startup checklist | Optional |
| `Bootstrap.md` | First-run ritual — ephemeral, agent deletes on completion | One-time |

**Template variables:** `Agents.md` supports `{max_turns}` and `{warn_at}` placeholders, substituted at preamble build time with the current run's configuration values.

**Bootstrap detection:** An agent needs bootstrapping when both `Soul.md` and `Identity.md` are absent. The `Bootstrap.md` file drives the first-run ritual and is deleted by the agent upon completion.

---

## 5. Safety Model

### Host vs. Sandbox

| Tool | Host Default | Sandbox Mode |
|---|---|---|
| `fs_write` | Deny | Allow |
| `fs_rm` | Deny | Allow |
| `shell_exec` | Deny | Allow |
| `code_edit` | Deny | Allow |
| `git_commit` | Deny | Allow |
| `git_push` | Deny | Allow |

**Host mode:** Destructive tools are blocked. The operator is the safety perimeter.

**Sandbox mode:** The container/pod boundary is the safety perimeter. All destructive tools are elevated to Allow. All filesystem tools AND `shell_exec` route through `docker exec` or `kubectl exec` into the container. Relative paths are anchored to the container's working directory.

### The `load_agent_config` Invariant

`load_agent_config(container)` is the single function that resolves policy for a run. It must be called in **all four execution paths**. Missing it from any path means that path silently uses restrictive defaults even in sandbox mode — the symptom is policy-denied errors on tools that should be allowed. This is the most critical safety invariant in the codebase.

### Kubernetes Sandbox

When `THAT_SANDBOX_MODE=kubernetes` or `THAT_TRUSTED_LOCAL_SANDBOX=1`, the pod is treated as the trust boundary. The same policy elevation logic applies. BuildKit sidecars, registry push, and manifest deployment are handled through sandbox-aware preamble sections.

---

## 6. Extension Planes

### Skills

Skills are markdown files with YAML frontmatter, discovered from the agent's skills directory.

**Frontmatter requirements:** `name:` and `description:` must appear at the YAML root level. Fields nested under a `metadata:` key are not recognized. A skill with missing root-level fields is **silently skipped** during discovery — the agent will never see it.

Minimal valid frontmatter:
```yaml
---
name: my-skill
description: What this skill does
---
```

**Eligibility filters** (all optional):

| Filter | Behavior |
|---|---|
| `os` | Allowed OS names; empty = any |
| `binaries` | All listed binaries must be on PATH |
| `any_bins` | At least one listed binary must be on PATH |
| `envvars` | Required environment variables |

**Injection modes:**

| Flag | Effect |
|---|---|
| `bootstrap: true` | Auto-installed on every startup |
| `always: true` | Full body injected into preamble without requiring `read_skill` |

**Bundled default skills (11):** agent-orchestrator, channel-adapter, channel-notify, channel-whitelist, cluster-management, git-workspace, self-eval, skill-creator, task-manager, telegram-format, that-plugins.

### Plugins

Manifest-driven extensions defined in `plugin.toml`. Each plugin can declare:

| Component | Purpose |
|---|---|
| **Commands** | Named actions the agent can invoke |
| **Activations** | Message-pattern triggers that fire plugin behavior |
| **Routines** | Periodic tasks with schedule and priority |
| **Emojis** | Custom emoji catalogs for channel formatting |
| **Runtime** | Execution configuration (kind, environment) |
| **Deploy** | Deployment manifests and registry configuration |

Plugin state is tracked in `.plugin-state.toml` (enabled/disabled, version). Runtime queue in `.plugin-runtime.toml`. In listen mode, plugin state is hot-reloaded every 5 seconds. Routine and activation tasks are merged into the heartbeat cycle.

### Channels

The `Channel` trait defines the adapter interface. Each adapter declares its capabilities:

| Capability | Meaning |
|---|---|
| `inbound` | Can receive messages from users |
| `ask_human` | Supports blocking bidirectional interactions |
| `typing_indicator` | Can show typing status |
| `command_menu` | Supports platform-native slash commands |
| `max_message_len` | Maximum characters per outbound message before chunking |
| `message_edit` | Can edit previously sent messages |
| `attachments` | Can deliver file attachments natively |
| `inbound_images` | Can receive inbound image attachments |
| `inbound_audio` | Can receive inbound audio attachments |
| `rich_messages` | Supports structured messages with rich UI elements (keyboards, reply markups) |
| `reactions` | Can add emoji reactions to messages |
| `native_api` | Supports raw platform API passthrough |
| `deferred_start` | Initialization requires external network calls — started after readiness probe |

**ChannelRouter** manages all active adapters. `ChannelRouter::new()` returns `(router, inbound_rx)` — the receiver is returned separately to avoid `Sync` issues. Outbound events broadcast to all adapters. `human_ask` routes only to the primary channel (the one that originated the current session).

**MessageHandle** returned by `send_event` carries platform-native message and conversation IDs. Adapters that support editing (e.g., Telegram) populate these fields; others return defaults.

**Adapters:** Telegram, HTTP, TUI. All live under `channels/` in the single-crate layout.

---

## 7. Heartbeat

In listen mode, the agent polls `Heartbeat.md` from its agent directory at a configurable interval (default 60 seconds). Each H2 heading in the file is a scheduled entry with key-value metadata (schedule, priority, last_run) followed by a body description.

**Scheduling:** Entries support `once`, `minutely`, `hourly`, `daily`, `weekly`, and `cron` schedules. Priority levels: `urgent`, `high`, `normal`, `low`. Urgent items are always due regardless of schedule.

**Execution:** Due entries are dispatched as autonomous agent runs. The heartbeat system collects due entries from both the `Heartbeat.md` file and plugin routines/activations, merging them into a single task queue per poll cycle.

---

## 8. Multi-Agent Subsystem

The agent can spawn, query, and coordinate peer agents. All inter-agent communication is async-first — the parent agent's turn is never blocked.

### Agent Registry

`AgentRegistry` (`agents/mod.rs`) is a file-backed registry at `~/.that-agent/cluster/agents.json`. Each entry records name, role, parent, PID, gateway URL, and start time. Liveness is checked via OS signal (`kill -0`). In Kubernetes mode, K8s labels replace the file registry as source of truth.

### Task Registry

`AgentTaskRegistry` tracks work units at `~/.that-agent/cluster/agent_tasks.json`. Tasks are the coordination primitive between agents.

**Lifecycle:** `Submitted` → `Working` → `InputRequired` → `Completed` / `Failed` / `Canceled`

**Limits:** Terminal tasks pruned at 100 to prevent unbounded growth. Messages per task capped at 30 to prevent context bloat. Scratchpad entries capped at 50.

**Scratchpad:** Each task carries a two-tier scratchpad — a stable header (goal, workspace, participant policy) and a live activity tail (steering, blockers, reviews). This gives every participant a shared, persistent communication surface without assuming a single shared volume.

### Endpoints

| Endpoint | Purpose | Blocks? |
|---|---|---|
| `POST /v1/chat` | Sync query (called by background task, never by main loop) | Yes |
| `POST /v1/chat/stream` | SSE streaming (called by background task) | Yes |
| `POST /v1/inbound` | Async task dispatch — deferred for heartbeat | No |
| `POST /v1/notify` | Zero-cost notification — immediate channel relay + heartbeat queue | No |
| `POST /v1/task_update?task_id=X` | Task state callback — updates registry + notification | No |

### Key Helpers

- `resolve_agent_gateway(db_path, name)` → `(cluster_dir, gateway_url)` — canonical resolve pattern
- `post_to_agent_inbound(gw, message, sender, callback?)` — all POSTs to sub-agent `/v1/inbound`
- `AgentTaskRegistry::from_db_path(path)` — derive registry from memory DB path

### Restart Behavior

On channel mode boot, the agent sends a restart notification with the last user message and active task count. Session restoration happens lazily on the first inbound message per sender.

---

## 9. Eval Harness

The eval system (`eval/` module) runs scripted scenarios against the agent and scores results with an LLM judge.

### Scenario Format

TOML files defining:
- **Metadata:** name, description, agent, provider/model overrides, max turns, timeout, tags
- **sandbox:** `true` for scenarios requiring destructive operations (must be set or the agent will be policy-blocked mid-run)
- **steps:** Ordered sequence of actions
- **rubric:** Scoring criteria for the LLM judge

### Step Types

| Step | Purpose |
|---|---|
| `prompt` | Send a user message (with session label for shared history) |
| `reset_session` | Clear in-memory history for a session label (JSONL kept) |
| `create_skill` | Write a SKILL.md to the agent's skills directory |
| `run_command` | Execute a shell command (setup/teardown) |
| `create_file` | Write a file to disk |
| `assert` | Run assertions; failures are recorded but do not abort |

### Assertions

Assertions verify postconditions: file existence, command success, content matching. In sandbox mode, assertions run inside the container using injected environment variables for portable cross-agent assertions.

### LLM Judge

After all steps complete, the full transcript and rubric are passed to an LLM judge. The judge scores each rubric criterion and provides rationale. Reports are persisted for regression tracking.

---

## 10. Invariants

These must remain true as the project evolves:

1. **`load_agent_config(container)` is called in every execution path.** Adding a fifth path without this call creates a silent policy hole.

2. **Tool semantics do not diverge across modes.** The same `ToolRequest` must produce the same `ToolResponse` whether invoked from CLI, TUI, eval, or channel mode. The hook layer handles presentation differences.

3. **Memory and session state survive process restarts.** JSONL transcripts, SQLite memory, workspace files, and heartbeat state are durable. Losing continuity on restart is a regression.

4. **Skill frontmatter requires root-level `name:` and `description:`.** Nesting under `metadata:` causes silent skip. This is intentional — it prevents malformed skills from reaching the agent.

5. **Sandbox mode routes all I/O through the container.** Filesystem tools and shell execution must never touch the host filesystem when a sandbox container is active. The container is the trust boundary — leaking past it is a security defect.

6. **String truncation uses char-based slicing, never byte offsets.** `&str[..n]` panics on multi-byte codepoints. All truncation must use `.chars().take(n)` or `char_indices().nth(n)`.

