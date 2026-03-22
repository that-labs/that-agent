# that-agent

Single consolidated Rust crate (`that-agent`) + standalone `that-git-server`. See README for layout, ARCHITECTURE.md for design detail.

## Release Tagging

When creating a release tag, **always** bump the version in all three places before tagging:
1. `Cargo.toml` — `workspace.package.version`
2. `deploy/helm/that-agent/Chart.yaml` — both `version` and `appVersion`
3. `Cargo.lock` — run `cargo check` after bumping `Cargo.toml` to update it

Tag, commit, and push in one shot. Never tag without bumping versions first.

## Practices: What NOT To Do

### Dependencies

- **No heavy native bindings.** Never add crates that vendor C libraries (OpenSSL, libgit2, libssh2, etc.). We removed `git2` specifically because it pulled 915KB of `openssl_sys` + `libgit2_sys` for work the `git` CLI already does. If a shell command covers the use case, use `std::process::Command` — not a binding crate.
- **No new deps without justifying binary cost.** Run `cargo bloat --release -p that-agent --crates` before and after. If a crate adds >50KB for a non-core feature, it must be feature-gated or rejected.
- **No vendored TLS stacks.** The workspace uses `rustls` everywhere. Never introduce `openssl`, `native-tls`, or `vendored-openssl` features.
- **No duplicate functionality.** Before adding a crate, check if an existing dep or `std` already covers it. One HTTP client (`reqwest`), one TLS (`rustls`), one async runtime (`tokio`).

### Code Size

- **As few lines as possible.** Prefer composition over duplication, thin abstractions over defensive layering, deletion over accumulation. 10 lines instead of 30.
- **No dead code.** If it's unused, delete it. No `#[allow(dead_code)]` on production paths.
- **No over-abstraction.** Three similar lines are better than a premature helper function.

### Agent / Skill Prompts

- NLP-driven, generic language only. No real file paths, component names, or model IDs as examples.
- Model preferences: OpenAI runs use `gpt-5.2-codex` or higher. Never `gpt-4.x`.

## Multi-Agent Communication (A2A Protocol)

All inter-agent communication is **async-first**. Never block the parent agent's turn.

### Golden Rules

1. **Never block on a sub-agent.** `agent_query` spawns a background task via `ChannelHook` and returns immediately. Result arrives as a notification. There is no sync path between agents.
2. **Tasks are the unit of work.** `agent_task_send` creates a tracked task with a UUID. Use `agent_task_status` (zero-cost local file read) to check progress — never `agent_query`.
3. **Callbacks identify the sender.** Every POST to `/v1/task_update` and `/v1/notify` must include `agent` or `sender_id` in the body. Missing it shows `[unknown]` on the channel.
4. **Preamble is the agent's brain.** Any new tool, state, or protocol change that the LLM needs to know about must be reflected in `orchestration/preamble.rs`. If it's not in the preamble, the agent won't use it.
5. **Dual delivery for notifications.** `__notify__` messages relay to the channel immediately (user sees it) AND queue for the parent LLM's next heartbeat turn. Never remove either path.

### Task Lifecycle

`AgentTaskState`: Submitted → Working → InputRequired → Completed / Failed / Canceled

- Registry: `~/.that-agent/cluster/agent_tasks.json` (file-backed, `atomic_write_json`)
- Terminal tasks pruned at 100 to prevent unbounded growth
- Messages per task capped at 30 to prevent context bloat
- `get_and_update()` for single-load mutations (cancel, resume)

### Key Helpers (avoid re-inventing)

- `resolve_agent_gateway(db_path, name)` → `(cluster_dir, gateway_url)` — use instead of the 6-line resolve pattern
- `post_to_agent_inbound(gw, message, sender, callback?)` — use for all POSTs to sub-agent `/v1/inbound`
- `AgentTaskRegistry::from_db_path(path)` — derive registry from memory DB path
- `ToolContext::sender_name()` → agent name or `"parent"` fallback
- `AgentTaskState::from_str()` via serde — never manually match state strings

### Endpoints

| Endpoint | Purpose | Blocks? |
|----------|---------|---------|
| `POST /v1/chat` | Sync query (called by background task, never by main loop) | Yes |
| `POST /v1/chat/stream` | SSE streaming (called by background task) | Yes |
| `POST /v1/inbound` | Async task dispatch — deferred for heartbeat | No |
| `POST /v1/notify` | Zero-cost notification — immediate channel relay + heartbeat queue | No |
| `POST /v1/task_update?task_id=X` | Task state callback — updates registry + notification | No |

### Restart Behavior

On channel mode boot, the agent sends a restart notification with:
- Last user message from transcript
- Active task count + per-task preview

Session restoration happens lazily on the first inbound message per sender (not at boot).

## Architecture Gotchas

### Policy Enforcement

`load_agent_config(container)` **must** be called in every execution path (streaming, TUI, eval, channel). Missing it silently uses restrictive defaults even in sandbox mode — symptom: `policy denied` on tools that should be allowed.

Destructive tools (`fs_delete`, `shell_exec`, `fs_write`, `code_edit`, `git_commit`, `git_push`) default to Deny on host, Allow in sandbox.

### Preamble ↔ Struct Sync

Adding a field to any agent-facing struct (Heartbeat, channel config, etc.) without updating the preamble guidance in `orchestration/preamble.rs` means the LLM will never use it. The preamble is the agent's only source of truth.

### ChannelHook Interceptions

`ChannelHook` in `hooks.rs` intercepts tool calls before dispatch. Tools handled entirely by the hook (return `HookAction::Skip`): `human_ask`, `answer`, `channel_notify`, `channel_send_file`, `channel_send_message`, `channel_send_raw`, `channel_settings`, `agent_query`. Adding a new channel-aware tool? Add its interception arm here — dispatch() will never see it.

`agent_query` is intercepted to spawn a **background task** — never blocks the agent loop. The result is delivered as a channel notification.

### Channel Router

`ChannelRouter` fans out to all adapters. `TuiChannel` lives in `tui/` alongside all other modules (no circular dep in single-crate layout). Channel adapters must clear their text buffer on every tool-call boundary.

### Unicode Truncation

Never `&str[..n]` — panics on multi-byte codepoints. Always char-based:
```rust
s.chars().filter(|c| !c.is_control()).take(120).collect::<String>()
```

### Skills

`name:` and `description:` must be at the YAML **root** (not nested under `metadata:`). Missing root-level fields → silent skip during discovery. Skills hot-reload from `skills/` — no restart needed. Keep body under 400 lines.

### Memory

`Memory.md` is a pointer index — never paste full content. Full content lives in `memory.db` via `mem_recall`. Each agent has an isolated `memory.db`. History reconstruction anchors at the last `Compaction` event.

### Heartbeat

`urgent` fires immediately then follows schedule. `not_before: <RFC3339>` gates firing. `status: done` permanently disables. Schedules: `once | minutely | hourly | daily | weekly | cron: <expr>`. `timezone` on `AgentDef` controls wall-clock schedules.

### Eval Scenarios

Prompts must read like human requests — never name tools, skills, or internal workflows. Scenarios needing destructive ops must set `sandbox = true`.

## Pre-commit

`cargo fmt --all && cargo clippy --workspace -- -D warnings` — CI rejects unformatted code and warnings.
