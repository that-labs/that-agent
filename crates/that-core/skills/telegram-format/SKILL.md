---
name: telegram-format
description: Formatting and style guide for responses delivered through Telegram. Applies when a Telegram channel is active.
metadata:
  bootstrap: false
  always: false
  envvars:
    - TELEGRAM_BOT_TOKEN
  version: 1.0.0
---

# Telegram Formatting Guide

You are sending responses through Telegram. Telegram uses **MarkdownV2** formatting.
Follow these rules to ensure messages render correctly.

## MarkdownV2 Syntax

| Effect | Syntax |
|--------|--------|
| Bold | `*bold text*` |
| Italic | `_italic text_` |
| Underline | `__underline__` |
| Strikethrough | `~strikethrough~` |
| Spoiler | `\|\|hidden text\|\|` |
| Inline code | `` `code` `` |
| Code block | ` ```language\ncode\n``` ` |
| Link | `[label](url)` |

## Special Characters — Must Be Escaped

The following characters **must** be preceded by a backslash `\` when used literally outside
of formatting constructs:

`. ! ( ) - _ * [ ] { } # + = | ~ > < \`

Failure to escape these will cause a Telegram API parse error and the message will not be sent.

## Practical Rules

- Keep each message under 4,096 characters. If your response is longer, break it into
  clearly delimited sections sent as separate notifications.
- Prefer code blocks for any multi-line technical content (logs, JSON, code snippets).
- Avoid markdown headings (`#`, `##`) — Telegram does not support them.
- Hyperlinks work well; use them for references to documentation or external resources.
- When using `channel_notify`, format the notification in plain text unless you are certain
  the content will render cleanly.

## Notifications via channel_notify — PLAIN TEXT ONLY

**IMPORTANT: `channel_notify` sends as plain text — do NOT escape any characters.**
No backslashes before `.` `(` `)` `-` or any other character. Write naturally.
The escaping rules above apply ONLY to the main conversational reply (which uses MarkdownV2),
never to `channel_notify`.

When sending proactive updates, keep them short. Emoji are fine.
Save rich formatting for the main conversational reply.
