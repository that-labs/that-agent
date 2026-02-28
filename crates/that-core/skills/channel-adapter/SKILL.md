---
name: channel-adapter
description: Guide for building a new channel bridge plugin that connects an external platform to the agent gateway. Use when tasked with creating a new integration (Slack, Discord, WhatsApp, email, etc.).
metadata:
  bootstrap: false
  always: false
  os: [darwin, linux]
---

# Channel Adapter — Building a Bridge Plugin

A channel bridge is a standalone service that connects an external messaging platform to the
agent's gateway. The bridge receives events from the platform, forwards them to the agent, and
delivers the agent's response back to the user.

All bridges follow the same async gateway protocol. The agent does not need to know anything
about the external platform — it only speaks the gateway protocol.

## Gateway Protocol

The agent exposes a single inbound HTTP surface. Every bridge posts messages to this endpoint
and receives responses asynchronously via a callback.

### Inbound Request

Send a `POST` to the agent's gateway inbound endpoint with a JSON body:

```json
{
  "message": "The user's text message",
  "sender_id": "unique-user-identifier",
  "channel_id": "your-bridge-identifier",
  "conversation_id": "optional-thread-or-conversation-id",
  "callback_url": "https://your-bridge/callback",
  "attachments": []
}
```

**Fields:**

| Field | Required | Description |
|---|---|---|
| `message` | yes | The user's text content |
| `sender_id` | yes | Stable identifier for the human user on the external platform |
| `channel_id` | yes | Matches the bridge ID used during registration |
| `conversation_id` | no | Thread or conversation grouping; omit for flat channels |
| `callback_url` | yes | Where the agent should POST the response when ready |
| `attachments` | no | Array of multimodal attachments (see below) |

The gateway responds immediately with `202 Accepted` and a `request_id`. Processing happens
asynchronously.

### Attachments

Each attachment in the array has:

```json
{
  "mime_type": "image/jpeg",
  "data": "<base64-encoded bytes>"
}
```

Supported attachment types:
- **Images** (`image/jpeg`, `image/png`, `image/webp`, `image/gif`) — sent to the LLM as
  vision input for the current turn only. Images are **not** retained in conversation history.
- **Audio** (`audio/ogg`, `audio/mpeg`, `audio/wav`) — the agent transcribes audio
  server-side before processing. The bridge should send raw audio bytes; do **not** transcribe
  on the bridge side.

### Async Callback

When the agent finishes processing, it sends a `POST` to the `callback_url` provided in the
original request:

```json
{
  "request_id": "the-original-request-id",
  "response": "The agent's text reply",
  "channel_id": "your-bridge-identifier"
}
```

The bridge is responsible for delivering this response back to the user on the external
platform using whatever formatting or API the platform requires.

## Capability Declaration

When registering, a bridge declares what attachment types it can receive from users. This
lets the agent know what multimodal inputs are available on each channel.

Supported capabilities:
- `inbound_images` — the bridge can receive and forward image attachments
- `inbound_audio` — the bridge can receive and forward audio attachments

Only declare capabilities the bridge actually supports. If a bridge declares no attachment
capabilities, the agent treats it as text-only.

## Bridge Registration

Use the `channel_register` tool to register the bridge with the agent at startup:

- `id` — a unique, stable identifier for this bridge instance
- `callback_url` — the URL the agent should use for delivering responses
- `capabilities` — list of supported capabilities

Registration is idempotent. Calling it again with the same ID updates the existing entry.
Bridges register themselves at runtime — no restart of the agent is required.

## Bridge Scaffold

A typical bridge plugin follows this lifecycle:

1. **Start up** and connect to the external platform (webhook listener, WebSocket, polling).
2. **Register** with the agent gateway using the channel registration tool.
3. **Listen** for incoming events from the external platform.
4. **Transform** each event into the gateway inbound format — extract the text, sender
   identity, and any attachments (images as base64, audio as raw bytes base64-encoded).
5. **POST** the transformed payload to the agent's gateway inbound endpoint.
6. **Receive** the async callback with the agent's response.
7. **Deliver** the response back to the user through the external platform's API.

### Error Handling

- If the gateway returns a non-2xx status, retry with backoff.
- If the callback never arrives within a reasonable timeout, notify the user that the agent
  is unavailable.
- If the external platform disconnects, attempt reconnection before deregistering.

### Image and Voice Behavior

- **Images** are single-turn context only. They are included in the current LLM request but
  stripped from conversation history afterward. Do not expect the agent to reference images
  from previous turns.
- **Voice messages** are transcribed server-side by the agent. The bridge sends the raw audio
  bytes (base64-encoded) as an attachment — never pre-transcribe on the bridge side. The
  transcribed text replaces the audio in the agent's processing pipeline.

## Deployment

A bridge is a plugin — it declares its deploy target in its manifest and is deployed to the
cluster like any other plugin. Once deployed and registered, it is live immediately. Multiple
bridges can run simultaneously, each with its own channel ID and callback URL. The agent fans
out responses only to the originating channel, never broadcasting across bridges.

**Port convention (required):** Every bridge service must expose port **80** as its external
Service port, regardless of what port the process binds to internally. Map `port: 80` →
`targetPort: <internal-port>` in the Kubernetes Service (or the equivalent in Docker Compose
with `- "80:<internal-port>"`). This keeps callback URLs clean (`http://<service-name>/callback`
with no port suffix) and ensures compatibility with Tailscale-exposed services, DNS-only
routing, and any tooling that assumes standard HTTP port by default.
