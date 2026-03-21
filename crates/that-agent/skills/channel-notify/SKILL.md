---
name: channel-notify
description: Explains when and how to proactively notify the human operator during long-running tasks, without pausing to wait for a response. Also covers sending file attachments mid-task.
metadata:
  bootstrap: true
  always: true
  version: 1.1.0
---

# Channel Notify

You have two built-in tools for reaching the human operator mid-task without interrupting your
work or waiting for a reply:

- **`channel_notify`** — send a short text message.
- **`channel_send_file`** — send a file from the filesystem as an attachment.

Use them when you want to share something meaningful as you go, rather than revealing everything
only in the final response.

## `channel_notify` — Text Notifications

Use `channel_notify` when:

- You have completed a significant phase of a long-running task and the human would benefit
  from knowing where things stand before you continue.
- You discovered something important — a relevant finding, a risk, or a change in direction —
  that would be good for the human to know about now.
- A task is taking longer than expected and you want to reassure the operator that progress
  is being made.
- You have reached a natural checkpoint before a potentially irreversible action (but note:
  if you actually need approval, use the `human_ask` tool instead).

Do **not** use `channel_notify` when:

- You actually need the human to make a decision or give approval — use `human_ask` instead.
- The task is short and the final response will speak for itself.
- The update adds noise without useful information (avoid "still working…" spam).

## `channel_send_file` — File Attachments

Use `channel_send_file` when you have generated a file (report, export, image, log, etc.) and
want to deliver it to the operator mid-task without waiting for a reply.

Parameters:
- `path` *(required)* — path to the file on the local filesystem.
- `caption` *(optional)* — a short description shown alongside the file.
- `channel` *(optional)* — specific channel ID to target; omit to deliver to all active channels.

Channels with native file support (such as Telegram) will deliver the file as a proper
attachment. Other channels will receive a text notification describing the filename and size.

## Tone and Formatting

Keep notifications concise and specific. A good notification tells the human what happened or
was found, not just that you are working. Follow the active channel's formatting conventions
(visible in the **Active Channels** section of your context).
