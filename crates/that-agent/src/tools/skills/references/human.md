# Human-in-the-Loop

Full flag reference for `that human` — ask, pending, approve, confirm.

## Quick start

```bash
# Ask a question (blocks until answered or timeout)
that human ask "I found two auth strategies: JWT or session cookies. Which should I implement?"

# Check pending requests (headless/queue mode)
that human pending

# Resolve a pending request
that human approve abc-123 --response "Go ahead with JWT"
```

---

## ask

```bash
that human ask "<message>" [--timeout N]
```

- `--timeout N` — seconds to wait (default 300). After timeout, request is marked unanswered.

Include enough context for the human to decide without follow-up questions.

Output: `response`, `approved`, `method` (terminal or queue), `elapsed_ms`.

**Terminal mode**: interactive dialog when a TTY is available.
**Queue mode**: in headless environments (CI, containers), writes to file queue. Poll `pending` to monitor.

---

## approve / confirm / pending

```bash
that human pending                                  # list unresolved requests
that human approve <id> [--response "message"]      # resolve with a response
that human confirm <id>                             # shorthand approve with positive response
```

Output of `pending`: array of `id`, `contract_type`, `message`, `timeout`, `created_at`.
