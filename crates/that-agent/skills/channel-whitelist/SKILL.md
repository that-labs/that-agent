---
name: channel-whitelist
description: Manage the list of callers authorized to send messages to the agent via external channels.
metadata:
  bootstrap: true
  always: false
---

# Channel Caller Authorization

Each communication channel can be configured with an allowlist of sender IDs.
When the allowlist is non-empty, only those senders may interact with the agent.
Messages from senders not on the list are automatically rejected.

## Finding the Configuration File

Your context includes a value called `THAT_CONFIG_PATH` — this is the exact path
to this agent's channel configuration file on this machine. Use your file reading tool
to inspect it. Read the file before making any changes.

## Configuration Structure

The file is TOML. Look for an `[[channels.adapters]]` block and its `allowed_senders` field:

```toml
[[channels.adapters]]
type = "telegram"
allowed_senders = ["111111111", "222222222"]
```

An empty list (`allowed_senders = []`) means open access — anyone can message the bot.

## Reading the Current Allowlist

1. Find `THAT_CONFIG_PATH` in your context
2. Use your file reading tool to read that file
3. Locate the `allowed_senders` field in the relevant adapter block

## Adding a Caller

1. Read the config file at `THAT_CONFIG_PATH`
2. Find the `allowed_senders` list for the relevant adapter
3. Append the new sender ID (as a quoted string) to the list
4. Write the updated file back using your file writing tool
5. Confirm the change — the update takes effect automatically within seconds (no restart needed)

## Removing a Caller

1. Read the config file at `THAT_CONFIG_PATH`
2. Remove the sender ID from the `allowed_senders` list
3. Write the updated file back
4. Confirm the change

## Security Rules

- Only grant access when explicitly asked by an already-authorized caller.
- Sender IDs are platform-specific numeric or string identifiers, not display names.
- The allowlist is enforced before messages reach you — unauthorized senders are
  rejected at the adapter level and you never see their messages.
