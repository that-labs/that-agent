---
name: agent-orchestrator
description: Deploy, scope, and manage child agents for parallel task execution. Covers spawning, workspace sharing, progress monitoring, conflict resolution, and result aggregation.
metadata:
  bootstrap: true
  version: 2.0.0
---

# Multi-Agent Orchestration

This skill describes how to deploy, scope, and manage child agents as a root (parent)
agent. Use it when you need to parallelize work, delegate specialized tasks, or
build a team of agents that collaborate on a shared goal.

## Core Rule

For autonomous multi-agent work, default to `agent_task`, not ad hoc messaging.
Each tracked task has one shared scratchpad:
- `header`: stable shared contract for overall goal, workspace/repo context, participants, and policy
- `entries`: live coordination tail for plans, steering, blockers, reviews, and git-visible progress

The parent should supervise through task status plus scratchpad reads. Child agents
should externalize concise coordination notes there instead of relying on hidden reasoning
or direct peer chatter.

## Two Patterns for Delegation

### Ephemeral Agents — one-off tasks that run and return results

Use `agent_run(name, task, role?)` to spawn a short-lived agent. The call blocks
until the agent completes and returns its result. Best for:
- Parallel research, analysis, or batch processing
- Coding tasks when combined with workspace sharing
- Any work with a clear deliverable and bounded scope

Fan out by calling multiple `agent_run` in parallel — each runs as an independent pod.

Use `agent_run` when you do not need shared coordination state or mid-flight steering.
If the parent may need to supervise, redirect, pause, or attach peers, use `agent_task`.

### Persistent Agents — long-running services you query repeatedly

Use `spawn_agent(name, role)` to create a Deployment + Service that stays alive.
Communicate via `agent_task(action=send, name, message)` for tracked work and
`agent_query(name, message)` for simple synchronous request/response. Best for:
- Coordinators, channel listeners, always-on workers
- Agents you need to query multiple times across different tasks
- Services that maintain state between interactions

Clean up with `agent_unregister(name)` when no longer needed.

## Tracked Task Workflow

1. `agent_task(action=send, name, message)` to create a task
2. `agent_task(action=scratchpad_read, task_id)` to inspect the shared header and live tail
3. `agent_task(action=send, task_id, message)` to steer a running task
4. `agent_task(action=share, name, task_id)` to attach a peer to the same scratchpad-backed task
5. `agent_task(action=status, task_id)` to supervise progress without blocking
6. `agent_task(action=cancel, task_id)` if the worker is drifting or no longer needed

Use `agent_task(action=scratchpad_write, task_id, section="header", kind=...)` only for durable
shared context changes. Use the default activity section for progress, blockers, reviews, and
steering notes.

## Coding Tasks — Shared Workspace

When delegating code changes, use the git workspace tools to share a repository
with workers and collect their results.

### Workflow

1. `workspace_share(path)` — push your local repo to the shared git server
2. `agent_run(name, task, workspace=true)` — worker clones the repo automatically
3. `workspace_activity()` — monitor which workers have pushed and how far along they are
4. `workspace_diff(branch)` — review a worker's changes without cloning
5. `workspace_collect(path, worker)` — merge the worker's branch into your workspace

Each worker pushes to its own isolated branch. Workers cannot interfere with each
other or with main. Git push, auto-merge, and merge-conflict events should be treated
as parent-visible coordination signals and mirrored into the tracked task scratchpad when available.

Load `read_skill git-workspace` for the full guide on conflict resolution,
multi-worker merge strategy, and progress monitoring.

## Scoping Principles

Effective orchestration depends on clear boundaries:

- **One role per agent**: each child should have a single, clear responsibility
- **Bounded turn budget**: set appropriate limits so children do not run indefinitely
- **Clear deliverables**: define what "done" looks like for each child's task
- **Minimal context**: give the child only the information it needs — not your full history

### Common Roles

| Role | Purpose |
|------|---------|
| explorer | Research, codebase analysis, information gathering |
| developer | Implementation, code changes, feature development |
| reviewer | Code review, testing, quality assurance |
| deployer | Build, push, and deploy artifacts |
| researcher | Web search, documentation review, knowledge synthesis |

## Monitoring Progress

While workers are running:

- `agent_task(action=status)` — see tracked tasks with owners, participants, and scratchpad revision
- `agent_task(action=scratchpad_read, task_id)` — inspect the shared header and recent coordination activity
- `agent_list()` — see all children with their role, status, and type
- `workspace_activity()` — branch list with ahead/behind counts and last commit
- `workspace_diff(branch)` — read a worker's changes to decide if guidance is needed

Use these to decide whether to wait, steer via `agent_task(action=send, task_id, ...)`,
cancel the task, or collect results.

## Communication

- **Parent → child**: use `agent_task(action=send, ...)` for tracked work; use `agent_query(name, message)` only for simple blocking questions
- **Peer ↔ peer**: share the same task with `agent_task(action=share, ...)` and coordinate through the task scratchpad
- **Child → parent**: children POST to the parent's gateway via `$THAT_PARENT_GATEWAY_URL/v1/notify` for zero-cost status, and keep durable coordination context in the task scratchpad
- **Progress visibility**: workspace git events and scratchpad activity are the parent-visible supervision log

## Result Collection

After a child completes work:

1. Review changes with `workspace_diff(branch)` or `workspace_collect(path, worker, strategy="review")`
2. Merge with `workspace_collect(path, worker)` — cleans up the task branch on success
3. If merge fails, use `workspace_conflicts(branch)` to inspect and resolve

When collecting from multiple workers, merge sequentially — simplest changes first —
to keep conflicts manageable. Re-share the workspace after merging if remaining workers
need the updated main.

## Error Handling

- If a child agent times out, inspect its partial output and consider retrying with a narrower scope
- If `workspace_collect` reports a merge conflict, load `read_skill git-workspace` for the resolution protocol
- Consider splitting failed tasks into smaller, more focused subtasks
- Store failure patterns in memory to avoid repeating them

## Deployment Model

All agents — root, persistent children, and ephemeral workers — are deployed
via the same Helm chart. This ensures every agent gets identical security
contexts, probes, labels, and network policy regardless of when it was created.

- `spawn_agent` and `agent_run` deploy children as Helm releases
- `agent_unregister` removes a child by uninstalling its Helm release
- `agent_list` discovers all managed agents via their labels

### Infrastructure Inheritance

Children automatically inherit the parent's infrastructure context at deploy time.
You do not need to pass these manually — the harness forwards them:

- **Registry**: push endpoint, TLS mode, and credential secret
- **BuildKit**: children reuse the parent's BuildKit service (no dedicated sidecar)
- **Image version**: children run the same agent image as the parent
- **API credentials**: children share the parent's secret for LLM provider keys

The child's `<system-reminder>` reflects the same registry and build backend
values the parent sees. If a child reports infrastructure issues (registry
unreachable, build failures), check your own `<system-reminder>` for the
authoritative values and relay them — the child should have the same context.

### Upgrading Children

When you are upgraded to a new version, your existing children may still run
the previous version. To bring them up to date:

1. List all managed Helm releases in the namespace
2. Compare each child's chart version to your own
3. For each outdated child, run a Helm upgrade with the current chart version
4. Verify each child is healthy after upgrade before moving to the next

Children inherit the parent's secret for credentials — no key rotation needed
during upgrades. Their persistent volume claims survive the upgrade.

### Migrating Legacy Children

If you discover children that were deployed before the Helm migration (created
via raw manifests, not managed by Helm), they need to be adopted:

1. Identify legacy children — they appear in the namespace with managed labels
   but have no corresponding Helm release
2. Note each child's name and its persistent volume claim name
3. Delete the legacy resources (deployment, service, service account, role
   binding, config map) — do NOT delete the persistent volume claim
4. Re-deploy the child using `spawn_agent`, which creates a proper Helm release
5. The new deployment automatically mounts the surviving persistent volume,
   preserving the child's memory, config, and identity

Always migrate one child at a time and verify it is healthy before proceeding
to the next. If a child has active tasks, wait for them to complete or cancel
them before migrating.

## Memory-Based Team Evolution

After orchestration completes, store learnings in persistent memory:

- **What worked**: which role assignments produced good results for which task types
- **Configuration insights**: agent settings that worked well (model, turn budget)
- **Failure patterns**: common issues and how to prevent them in future runs
- **Team compositions**: effective agent structures for recurring task patterns
