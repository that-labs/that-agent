# that-agent Workspace

Rust workspace — see README for layout, ARCHITECTURE.md for design detail.

## Gotchas & Lessons Learned

### rsync Excludes in Build Scripts
Always anchor excludes with a leading `/` (e.g., `--exclude='/sandbox'`). Unanchored patterns match nested dirs with the same name, silently stripping Rust modules from the build context (E0583).

### gitignore Path Anchoring
Same rule — `tasks/` matches any `tasks/` dir in the tree. Use `/tasks/` to scope to the repo root.

### Workspace File Model

Agent home: `~/.that-agent/agents/<name>/`

| File | Role |
|---|---|
| `Soul.md` | Deep identity: character, values, philosophy |
| `Identity.md` | Name, vibe, emoji — bootstrap-created |
| `Agents.md` | Operating instructions; supports `{max_turns}` / `{warn_at}` placeholders |
| `User.md` | Who the user is |
| `Tools.md` | Local env: devices, SSH, preferences |
| `Memory.md` | Thin index → `memory.db`; updated after every `mem_compact` |
| `Heartbeat.md` | Scheduled autonomous work |
| `Tasks.md` | Epic/story/task index |
| `Bootstrap.md` | First-run ritual — agent deletes on completion |

Workspace: `~/.that-agent/workspaces/<name>/` — bind-mounted at `/workspace` in sandbox.

### Skill Frontmatter

`name:` and `description:` must be at the YAML **root** (not nested under `metadata:`). Missing root-level fields → silent skip during discovery.

```yaml
---
name: my-skill
description: What the skill does and when to use it
metadata:
  bootstrap: true     # bundled skills only
  always: false       # true = inject full body into every preamble; use sparingly
  os: [darwin, linux]
  binaries: [some-tool]
  envvars: [${API_KEY}]
---
```

Skills hot-reload from `skills/` — no restart needed. Failed eligibility (OS, binaries, envvars) → silent skip. Three layers: frontmatter (always), body (on `read_skill`), `references/` (on `read_skill <name> <ref>`). Keep body under 400 lines.

### Policy Enforcement

`load_agent_config(container)` **must** be called in every execution path (streaming, TUI, eval, channel). Missing it silently uses restrictive defaults even in sandbox mode — symptom: `policy denied` on tools that should be allowed.

Destructive tools (`fs_delete`, `shell_exec`, `fs_write`, `code_edit`, `git_commit`, `git_push`) default to Deny on host, Allow in sandbox. The Dockerfile sets `THAT_TOOLS_POLICY__TOOLS__*=allow` inside the container.

### Memory

After every `mem_compact`: update `Memory.md` (append compaction row, refresh Active Topics). It's a pointer index — never paste full content. Full content lives in `memory.db` via `mem_recall`. Each agent has an isolated `memory.db` — no cross-agent sharing.

History reconstruction anchors at the last `Compaction` event to prevent context bloat on restart.

### Heartbeat

- `urgent` entries fire immediately on first dispatch, then follow schedule.
- All others fire when `last_run + interval <= now`.
- `status: done` permanently disables an entry.
- Schedules: `once | minutely | hourly | daily | weekly | cron: <expr>`

### Channel Router

`ChannelRouter` fans out to all adapters. `human_ask` routes to the first adapter with `ask_human` capability. `TuiChannel` lives in `that-core::tui` (not `that-channels`) to avoid a circular dep. Sessions persisted in `channel_sessions.json`.

Channel adapters must clear their text buffer on every tool-call boundary — otherwise multi-turn narration leaks into the final response.

### Eval Scenarios

Prompts must read like human requests — never name tools, skills, or internal workflows. The agent decides autonomously. Bypassing native workflows (e.g., planting a skill the agent should create) gives the judge false signal. Scenarios needing destructive ops must set `sandbox = true`.

### Unicode Truncation

Never `&str[..n]` — panics on multi-byte codepoints. Always char-based:
```rust
let clean: String = s.chars().filter(|c| !c.is_control()).take(120).collect();
```

### `.env` Hygiene

`.gitignore` excludes `.env` and `.env.*`. Never commit secrets. Rotate immediately if exposed in git history.

## The Living System Philosophy

**Everything hot-reloads. The agent never needs to restart to grow.**

Channels, plugins, and agent identity (soul, memory, config) are live surfaces — not startup fixtures. When something new is registered or updated, the running system picks it up. Cold restarts are an ops smell, not a design pattern.

### Plugins = Reusable Cluster Services

A plugin is a deployed, versioned software service (tool, bridge, API, worker) that any authorized agent can invoke. Plugins are not scripts bundled into the agent binary — they are independent deployables the agent manages as infrastructure.

- Every plugin declares its deploy target (`docker-compose`, `kubernetes`, etc.) in its manifest
- A plugin deployed by any agent is visible cluster-wide
- **Policy hierarchy:** the creator sets access policy; the root/main agent always inherits access to plugins created by its sub-agents; sub-agents cannot revoke upward access
- Plugins are the unit of capability reuse — before building something new, an agent checks if a plugin already provides it

### Channels = Hot-Registered I/O Bridges

Channels follow the async gateway pattern:

- Plugin-deployed bridge services (Slack bot, Discord, WhatsApp, etc.) speak a **standard gateway protocol** — POST inbound message + callback URL, receive async response POST
- The agent exposes a single gateway inbound surface; bridges register themselves at runtime via a lightweight registration endpoint
- The agent answers from **one place** — responses are scoped to the originating `channel_id`, never broadcast indiscriminately
- No channel requires a restart to activate; `channel_id → callback_url` registry is live

### Multi-Agent Cluster Awareness

When a sub-agent is deployed to the cluster:
- It inherits awareness of all plugins already deployed
- Any plugin it creates is visible to the main agent automatically
- The main agent's policy is the ceiling — sub-agents can restrict further but not elevate beyond it
- Plugin registry is shared state, not per-agent local config

### Code Minimalism — Critical

**As few lines of code as possible.** Every line is a maintenance burden. Prefer composition over duplication, thin abstractions over defensive layering, and deletion over accumulation. If two features can share a path, they must. If something can be expressed in 10 lines instead of 30, it must be. Complexity is a liability, not a feature.

## Model Preferences

OpenAI runs: `gpt-5.2-codex` or higher. Never `gpt-4.x`.

## Agent / Skill Prompt Guidelines

NLP-driven, generic language only. No real file paths, component names, or model IDs as examples.
