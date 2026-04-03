# Task Delegation Protocol

## agent_task Action Catalog

- `agent_task(action=send, name, message)` — dispatch task, get task_id immediately
- `agent_task(action=send, targets=[name1, name2, ...], message)` — broadcast: send the same task to multiple agents, each gets its own task_id. Returns a `broadcast_id` to correlate all tasks. Check the `errors` array — some targets may fail while others succeed.
- `agent_task(action=send, name, message, task_id=X)` — steer a running task
- `agent_task(action=share, name, task_id=X)` — add another agent to the same scratchpad-backed task
- `agent_task(action=status)` — check all tasks (free local read, zero cost)
- `agent_task(action=cancel, task_id)` — graceful stop
- `agent_task(action=resume, task_id)` — resume canceled task
- `agent_task(action=scratchpad_read, task_id)` — read shared notes for a task (zero cost)
- `agent_task(action=scratchpad_write, task_id, note)` — append a note to a task's scratchpad

Task states: submitted → working → input_required → completed/failed/canceled

Task callbacks update the registry automatically — `status` reads always reflect the latest state, no manual polling needed. Task updates arrive in your heartbeat check-in.

When a task is `input_required`, the sub-agent needs your input — reply with `agent_task(action=send, task_id=X, message=...)` or ask the human.

## Delegation Pattern Table

| Pattern | Tool | When |
|---------|------|------|
| Tracked async work | `agent_task(action=send)` | Default for any real work |
| Steer running task | `agent_task(action=send, task_id=X)` | Redirect, add context |
| Add peer collaborator | `agent_task(action=share, name, task_id=X)` | Let multiple agents coordinate on one task via scratchpad |
| Quick answer needed | `agent_query` | Simple questions, <30s |
| Parallel ephemeral | `agent_run` (xN) | Fan-out coding with workspace |
| Warm-start agent | `agent_run(..., bootstrap={identity, soul, agents, context})` | Pre-load identity + domain research |

## Task Scratchpad Protocol

Every task has one shared scratchpad with two sections:
- `header` — stable shared contract: overall goal, workspace/repo context, participants, and coordination policy
- `entries` — live activity tail: plans, steering, blockers, review notes, and git activity

### Parent (dispatcher)

On the first `agent_task(action=send)` for a task, the harness automatically writes header entries for the overall shared goal, workspace root or shared git repo context, coordination contract, and current participants.

Use `agent_task(action=scratchpad_write, task_id, section="header", kind=...)` only when durable shared context truly changed. Use the live activity tail for steering, reviews, approvals, and blockers. Supervise through `agent_task(action=status, task_id=X)` plus `agent_task(action=scratchpad_read, task_id=X)`, and stop drift with `agent_task(action=cancel, task_id=X)`.

### Sub-agent (worker)

Before starting filesystem exploration or heavy tool use:
1. `agent_task(action=scratchpad_read, task_id)` — ALWAYS read first
2. Treat `header` as the cache-friendly shared contract for goal, workspace, participants, and policy
3. Write to the activity tail after meaningful milestones, steering acknowledgements, review decisions, blockers, or git-visible progress
4. If another agent is attached to the same task, coordinate through the scratchpad rather than direct peer chatter

If `agent_task(action=scratchpad_read)` returns workspace paths or repo details, use them directly — do not explore to rediscover them. Git push / auto-merge / conflict events for shared workspaces are mirrored into the activity tail when available.

Do not dump hidden chain-of-thought. Externalize only concise decisions, progress, blockers, and requests that the team needs in order to act.

## Anti-Loop Protection Details

The harness tracks consecutive turns where you only use exploration tools (filesystem listing, file reading, grep, search, shell). At 8 turns a soft warning fires; at 12 turns the harness forces a stop. The streak counter decays (halves) when you use a non-exploration tool, rather than resetting to zero. Duplicate tool calls (same tool + same arguments) accelerate the counter.
