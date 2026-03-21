# Memory Tools

Full flag reference for `that mem` — add, recall, search, compact, remove, prune, stats, export, import.

## add vs compact — when to use each

**Use `that mem add`** when you discover a fact worth keeping independently:
- A project convention, config detail, or API behaviour you had to look up
- A decision made during the session that stands on its own
- Anything a future session would benefit from knowing as an isolated entry

**Use `that mem compact`** when context pressure is building — not to store facts, but to distil *what happened this session* so the next session can orient quickly:
- When `flush_recommended: true` appears in any tool output envelope
- When `that session stats` returns `"flush_recommended": true`
- Before a long context window rolls over or a conversation ends

The distinction: `add` is for *discoveries*, `compact` is for *continuity*. A compaction summary should answer "what was I doing and what should I do next?" — not repeat facts already stored individually.

---

## Quick start

```bash
# Start every session — surface relevant prior knowledge
that mem recall "current project conventions" --limit 5

# Store something you had to discover
that mem add "Uses pnpm not npm. Run: pnpm install." --tags "tooling,setup"

# Tag-filtered retrieval
that mem search "" --tags auth --limit 20
```

---

## add

```bash
that mem add "<content>" [--tags tag1,tag2] [--source "attribution"] [--session-id ID]
```

Near-duplicate detection updates the existing entry instead of creating a duplicate — safe to re-add revised versions.

Use `--session-id` to scope the memory to a specific conversation or run. Omit for global (cross-session) memories.

```bash
that mem add "Auth tokens expire after 1h. Refresh at /api/auth/refresh." \
  --tags "auth,api" --source "api-docs"
that mem add "Do not edit src/generated/ — run codegen script instead." \
  --tags "conventions"
that mem add "Decided to use Postgres" --session-id "$SESSION_ID"
```

---

## recall

```bash
that mem recall "<query>" [--limit N] [--session-id ID] [--max-tokens N]
```

Relevance + recency ranking. Falls back to substring matching when full-text finds nothing.

When `--session-id` is given, only memories from that session are returned.
Omit `--session-id` for global recall across all sessions.

Output: `id`, `content`, `tags`, `source`, `session_id`, `created_at`, `access_count`, `rank`.

---

## search

```bash
that mem search "<query>" [--tags tag1,tag2] [--limit N] [--session-id ID] [--max-tokens N]
```

Use `--tags` when you know the category. Use `recall` for broad relevance-based retrieval.

---

## compact

```bash
that mem compact --summary "<summary text>" [--session-id ID]
```

Store a durable compaction summary as a **pinned** memory entry with `source="compaction"`.
Pinned entries always float to the top of recall results.

Use this before a context window rolls over to preserve key decisions and context.

```bash
that mem compact \
  --summary "Auth: JWT tokens, 24h expiry. DB: Postgres. Next: write migration." \
  --session-id "$SESSION_ID"
```

**When to call compact:** When you see `"flush_recommended": true` in the JSON output envelope.
This signal means the output was heavily truncated (original tokens > 2x returned tokens).

---

## JSON Output Envelope

Every tool command in JSON mode wraps its output in an envelope:

```json
{
  "data": { ... },
  "tokens": 120,
  "truncated": true,
  "original_tokens": 4800,
  "flush_recommended": true
}
```

- `flush_recommended: true` — output was cut to less than half; call `that mem compact` before continuing.
- `truncated: true` — some content was removed to fit the token budget.
- `original_tokens` — how many tokens the full response would have been.

---

## Session Tracking (`that session`)

Track cumulative token usage and compaction events per session.

```bash
# Register or retrieve a session
that session init --session-id "$SESSION_ID"

# Accumulate token usage after each agent turn
that session add-tokens --session-id "$SESSION_ID" --tokens 1200

# Check stats; flush_recommended triggers when tokens > soft_threshold
that session stats --session-id "$SESSION_ID"
```

The `stats` response:

```json
{
  "session_id": "...",
  "context_tokens": 95000,
  "compaction_count": 2,
  "flush_recommended": false
}
```

`flush_recommended` is set when `context_tokens > soft_threshold_tokens` (default: 100,000).
Configure via `[session] soft_threshold_tokens` in `.that-tools/tools.toml`.

---

## remove / prune / stats

```bash
that mem remove <id>                         # delete one entry (get id from recall/search)
that mem prune --before-days 90              # remove entries older than N days
that mem prune --min-access 2                # remove entries accessed fewer than N times
that mem stats                               # total count, tag distribution, storage size
```

---

## export / import

```bash
# Export: output is wrapped in the JSON envelope — extract the inner array for import
that mem export --format json | jq '.data' > backup.json

# Import: expects a JSON array (the inner data, not the envelope)
that mem import < backup.json
```

Export before destructive operations. Import to merge knowledge from another environment.

> **Note:** `that mem export` in JSON mode returns `{"data": [...], "tokens": ..., ...}`.
> Pass only the `.data` array to `that mem import`.

---

## Recommended Agent Pattern

1. On session start: `that session init --session-id "$SESSION_ID"`
2. After each tool call: check the envelope for `flush_recommended: true`
3. After each turn: `that session add-tokens --session-id "$SESSION_ID" --tokens N`
4. Before context fills: `that mem compact --summary "..." --session-id "$SESSION_ID"`
5. On new session: `that mem recall "prior context" --session-id "$PREV_SESSION_ID"` to seed memory
