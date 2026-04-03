# Gateway Endpoints

Your HTTP gateway exposes several endpoints for inter-agent and service communication.

## Endpoint Reference

| Endpoint | Behavior | Use when |
|----------|----------|----------|
| `POST /v1/inbound` | Queued for next heartbeat tick (returns 202). Response delivered via `callback_url` if provided, otherwise via `answer`. | Plugins, services, and bridges that need the agent to act in the background. |
| `POST /v1/chat` | Synchronous — blocks until done, returns full response. | One-shot queries where the caller needs the answer inline. |
| `POST /v1/notify` | Zero-cost queue (returns 202). No LLM turn — batched into next heartbeat. | Status reports, progress updates, fire-and-forget notifications. |
| `GET /v1/scratchpad?task_id=X` | Read a task scratchpad's `header`, `entries`, and revision (returns 200). | Sub-agents reading parent-side scratchpad via HTTP fallback. |
| `POST /v1/scratchpad?task_id=X` | Write `{note, from, section?, kind?}` to a task scratchpad (returns 200). | Sub-agents writing entries when local registry is unavailable. |

## Key Rules

- **Plugins and deployed services** must use `/v1/inbound` so the agent processes requests asynchronously. Inbound messages are batched until the next heartbeat tick.
- **Never use `/v1/chat` from a plugin** — it blocks the caller until inference completes and makes tool calls visible on the user's channel.
- Messages from the same `sender_id` are serialized (queued, not parallel). Use distinct `sender_id` values for concurrent processing.

## `/v1/inbound` Request Body

```json
{
  "message": "<task>",
  "sender_id": "<service-name>",
  "callback_url": "<optional-url-for-response>"
}
```

- If `callback_url` is provided, the agent POSTs `{"text": "<response>"}` back when done.
- If omitted, the agent uses `answer` to deliver results on the originating channel.

## Sub-Agent Communication Protocol

When a sub-agent needs to reach its parent, it has two paths:

### Status report (fire-and-forget)

```
POST $THAT_PARENT_GATEWAY_URL/v1/notify
Authorization: Bearer $THAT_PARENT_GATEWAY_TOKEN
{"message": "<status text>", "agent": "<your-name>"}
```

Queued and surfaced at the parent's next heartbeat tick. Does NOT interrupt ongoing conversations or consume API quota.

### Async request (response delivered to callback)

```
POST $THAT_PARENT_GATEWAY_URL/v1/inbound
Authorization: Bearer $THAT_PARENT_GATEWAY_TOKEN
{"message": "<task>", "sender_id": "<your-name>", "callback_url": "http://<your-gateway>/v1/inbound"}
```

The parent processes it at the next heartbeat tick and POSTs `{"text": "<response>"}` back to the callback.

Use `/v1/notify` for progress updates. Use `/v1/inbound` + `callback_url` only when you need the parent to reason and respond.

## Channel Token Exclusivity

Each channel adapter token (Telegram bot token, Discord bot token, Slack app token, etc.) must be used by exactly ONE agent process at a time. Never share or reuse a channel token between a parent and sub-agent, or between any two concurrent agents. Sub-agents that need their own channel presence must use a separate, dedicated token.
