---
name: team-templates
description: Deploy pre-configured multi-agent teams from reusable templates. Use when you need a structured team for adversarial debate, parallel research, review councils, or any coordinated multi-agent workflow.
metadata:
  bootstrap: true
  version: 0.2.0
---

# Team Templates

Team templates are reusable orchestration playbooks that define multi-agent team
compositions, coordination patterns, and termination conditions. Load a template
with `read_skill team-templates <template>`, then deploy it using standard
orchestration tools.

## Available Templates

| Template | Pattern | Use Case |
|----------|---------|----------|
| `dialectic` | Adversarial rounds | Stress-test ideas through structured debate with proponent, opponent, and defender roles |

Load a template to see its full definition:
```
read_skill(name="team-templates", file="references/dialectic.md")
```

## Deployment Workflow

### 1. Spawn members with identity

Use `spawn_agent` with `bootstrap` to give each member a differentiated persona:

```
spawn_agent(name="<team>-<role>", role="<role>", bootstrap={
  soul: "<character description — values, reasoning style, biases>",
  agents: "<operating instructions — what to focus on, what to avoid>"
})
```

Use a consistent naming prefix so the team is identifiable as a unit.

### 2. Dispatch tasks to all members

Use `agent_task(action=send, targets=[...])` to broadcast the same prompt
to all members in one call:

```
agent_task(action=send, targets=["<team>-proponent", "<team>-opponent"], message="<round prompt>")
```

Each target gets its own tracked task. The response includes all task IDs.

### 3. Share scratchpad for coordination

Attach members to a shared task so they can see each other's progress:

```
agent_task(action=share, name="<team>-<member>", task_id="<id>")
```

Write the team contract into the scratchpad header:

```
agent_task(action=scratchpad_write, task_id="<id>", section="header", kind="goal", note="<team objective>")
```

### 4. Run the coordination loop

Follow the template's pattern. The parent acts as coordinator:

1. Send the round prompt to active members
2. Check task status — completed tasks update automatically via callbacks
3. Read scratchpad to collect each member's output
4. Evaluate the termination condition
5. If not done, send the next round prompt with context from previous rounds

Write a steering note between rounds summarizing what changed.

### 5. Synthesize and clean up

When the termination condition is met, read all scratchpad entries and produce
the final synthesis. Then clean up:

```
agent_admin(action=unregister, name="<team>-<role>")
```

## Design Principles

- **One clear role per member** — non-overlapping responsibilities
- **Bounded rounds** — always set a maximum to prevent infinite loops
- **Rich identity via bootstrap** — members with distinct personas produce
  better differentiated output than generic role labels
- **Scratchpad-first coordination** — the scratchpad is the shared truth;
  use headers for stable context, activity for round-by-round progress
- **3-5 members max** — coordination overhead grows with team size
- **On-demand spawning** — defer spawning conditional members until needed
