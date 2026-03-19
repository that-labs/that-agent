# Scenario TOML Schema Reference

## Top-Level Fields

```toml
name          = "Human-readable scenario name"
description   = "What this scenario measures"
agent_name    = "default"          # agent to run against
provider      = "openai"           # LLM provider override
model         = "gpt-5.2-codex"   # model override
judge_provider = "anthropic"       # judge LLM provider
judge_model    = "claude-sonnet-4-6" # judge model
max_turns     = 10                 # max multi-turn iterations per prompt
sandbox       = false              # run inside sandbox container
tags          = ["domain", "type"] # for filtering with --tags
timeout_secs  = 120                # wall-clock limit per prompt step
```

Only `name` is required. Everything else has sensible defaults.

## Steps

Steps execute in order. Each step is a `[[steps]]` table with a `type` field.

### prompt — Send a user message

```toml
[[steps]]
type    = "prompt"
session = "main"       # session label (shared history within label)
content = "Your request to the agent"
```

Steps sharing the same `session` label share conversation history.
Use different session labels to simulate fresh sessions.

### reset_session — Clear conversation history

```toml
[[steps]]
type    = "reset_session"
session = "main"        # clears in-memory history; JSONL transcript kept
```

### create_skill — Inject a skill before prompting

```toml
[[steps]]
type    = "create_skill"
name    = "my-skill"    # directory name under skills/
content = """
---
name: my-skill
description: What it does
---
Skill body here
"""
```

### run_command — Execute a shell command (setup/teardown)

```toml
[[steps]]
type    = "run_command"
command = "echo setup"
```

Use `{{agent_name}}` as a placeholder — it is substituted at runtime.

### create_file — Write a file to disk

```toml
[[steps]]
type    = "create_file"
path    = "/tmp/test-input.json"
content = '{"key": "value"}'
```

### assert — Run assertions (non-fatal, all collected)

```toml
[[steps]]
type = "assert"

[[steps.assertions]]
kind = "file_exists"
path = "/tmp/expected-output.txt"

[[steps.assertions]]
kind     = "file_contains"
path     = "/tmp/output.txt"
contains = "expected substring"

[[steps.assertions]]
kind    = "command_succeeds"
command = "test -f /tmp/result.json"

[[steps.assertions]]
kind      = "tool_call_seen"
tool      = "mem_add"
min_count = 1            # default: 1
```

## Rubric

The judge scores each criterion independently on 0-100.

```toml
[rubric]

[[rubric.criteria]]
name        = "criterion_name"
description = """
Observable evidence the judge should look for. Be specific:
"Agent called mem_recall before answering" not "Agent used memory well."
"""
weight = 35    # relative importance (default: 25)
```

Weights are relative signals — they don't need to sum to 100.

## Practical Tips

- **Prompt like a human.** "What do we know about the project?" not "Use mem_recall to retrieve memories."
- **One domain per scenario.** Memory, planning, coding, orchestration — test separately.
- **Use assertions for hard checks.** Did a file appear? Did a command succeed? Did a tool get called?
- **Use judge criteria for soft checks.** Quality of reasoning, appropriate tool selection, factual accuracy.
- **Clean up after yourself.** Use `run_command` steps at the end to remove test artifacts.
- **Tag consistently.** Domain tags (`memory`, `coding`, `e2e`) + type tags (`regression`, `benchmark`).
