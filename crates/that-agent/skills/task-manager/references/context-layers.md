# Context Layers

Three auto-injected sections appear in every `<system-reminder>` — use the right layer for each type of information.

## Status.md — Durable Operational State

Persists across sessions. Track active deployments (`## Deployments`), spawned child agents (`## Children`), and key capabilities (`## Capabilities`). Remove stale entries. This is not a log.

Update via `identity_update(file="Status.md", content="...")`.

## WorkingNotes.md — Session Working Context

Current findings, decisions, and constraints. Cleared between sessions. Use to remember facts you will need later this session.

Update via `identity_update(file="WorkingNotes.md", content="...")`.

## Task Scratchpad — Inter-Agent Coordination

`agent_task(action=scratchpad_*)` is for shared task coordination between agents. Different purpose entirely — not for personal notes.

## Pinned Context — Always-Visible Facts

Auto-injected pinned memories (max 5 entries, 2000 tokens, last 30 days). Compaction summaries auto-pin. Pinned entries appear every turn without recall.

### When to Pin

Facts you need every turn: active project goal, current blocker, key decision, coordination state with other agents. If you would `mem_recall` it most turns, pin it instead.

### When NOT to Pin

Transient details, session-local context (use WorkingNotes.md), historical records, or anything only relevant to a single task.

### Managing Pins

You have 5 slots. When adding a new pin, check `<pinned-context>` first — if a slot holds stale or resolved information, `mem_unpin` it before pinning the new fact. Treat pinned context like a whiteboard: update it as reality changes, don't let it accumulate.
