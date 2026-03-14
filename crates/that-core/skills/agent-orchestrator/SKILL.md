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

## Two Patterns for Delegation

### Ephemeral Agents — one-off tasks that run and return results

Use `agent_run(name, task, role?)` to spawn a short-lived agent. The call blocks
until the agent completes and returns its result. Best for:
- Parallel research, analysis, or batch processing
- Coding tasks when combined with workspace sharing
- Any work with a clear deliverable and bounded scope

Fan out by calling multiple `agent_run` in parallel — each runs as an independent pod.

### Persistent Agents — long-running services you query repeatedly

Use `spawn_agent(name, role)` to create a Deployment + Service that stays alive.
Communicate via `agent_query(name, message)` for synchronous request/response. Best for:
- Coordinators, channel listeners, always-on workers
- Agents you need to query multiple times across different tasks
- Services that maintain state between interactions

Clean up with `agent_unregister(name)` when no longer needed.

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
other or with main.

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

- `agent_list()` — see all children with their role, status, and type
- `workspace_activity()` — branch list with ahead/behind counts and last commit
- `workspace_diff(branch)` — read a worker's changes to decide if guidance is needed

Use these to decide whether to wait, provide feedback via `agent_query`, or collect results.

## Communication

- **Parent → child**: use `agent_query(name, message)` for persistent agents
- **Child → parent**: children POST to the parent's gateway via `$THAT_PARENT_GATEWAY_URL/v1/notify`
- **Progress visibility**: ephemeral workers post progress to your gateway automatically

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

## Memory-Based Team Evolution

After orchestration completes, store learnings in persistent memory:

- **What worked**: which role assignments produced good results for which task types
- **Configuration insights**: agent settings that worked well (model, turn budget)
- **Failure patterns**: common issues and how to prevent them in future runs
- **Team compositions**: effective agent structures for recurring task patterns
