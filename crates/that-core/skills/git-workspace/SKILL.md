---
name: git-workspace
description: Shared workspace management for multi-agent coding tasks. Covers sharing repos with workers, monitoring progress, reviewing diffs, collecting results, and resolving merge conflicts.
metadata:
  bootstrap: true
  version: 1.0.0
---

# Git Workspace — Multi-Agent Code Collaboration

This skill describes how to share, monitor, and merge code across parallel worker agents
using the shared git workspace. Load this when orchestrating coding tasks across multiple agents.

## Core Workflow

### 1. Share your workspace

Before delegating coding tasks, push your repo to the shared git server:

- Call `workspace_share(path)` with your local repo path
- This creates a bare repo that workers can clone from
- Call it once — all workers share the same repo

### 2. Spawn workers with workspace access

- Use `agent_run(name, task, workspace=true)` — the worker automatically clones the shared repo
- Each worker pushes to its own isolated branch (`task/{worker-name}`)
- Workers cannot interfere with each other or with main

### 3. Monitor progress

While workers are running, check their progress without cloning:

- `workspace_activity()` — see all branches, how many commits ahead/behind main, last commit info
- `workspace_diff(branch)` — read the unified diff of a worker's branch vs main
- Use these to decide if a worker needs guidance or is ready to collect

### 4. Collect results

When a worker finishes:

- `workspace_collect(path, worker)` — merges the worker's branch into your local workspace
- Use `strategy: "review"` to see the diff first without merging
- On success, the worker's task branch is cleaned up automatically

## Conflict Resolution

When `workspace_collect` reports a merge failure, follow this protocol:

### Step 1 — Inspect the conflict

Call `workspace_conflicts(branch)` to get:
- The list of files that conflict
- What the worker changed vs the merge base
- What main changed since the worker branched

### Step 2 — Decide resolution strategy

Based on the conflict analysis:

**Option A — Ask the worker to rebase** (preferred for simple conflicts):
- Send the worker a message via `agent_query` describing which files conflict
- Ask the worker to rebase against main and resolve the conflicts
- The worker pushes again, then you retry `workspace_collect`

**Option B — Resolve manually** (for complex conflicts or when the worker is done):
- Review both diffs from `workspace_conflicts`
- Make the resolution in your local workspace
- Commit the merge resolution yourself

**Option C — Cherry-pick specific changes** (when only parts of the worker's work are needed):
- Use `workspace_diff(branch)` to identify the valuable changes
- Apply them manually to your workspace instead of merging the full branch

### Step 3 — Verify

After resolution, run `workspace_activity()` to confirm the branch state is clean.

## Multi-Worker Coordination

When multiple workers are active simultaneously:

- **Merge sequentially** — collect one worker at a time to keep conflicts manageable
- **Merge the simplest first** — check `workspace_activity()` and merge the worker with the fewest changes first, building up main incrementally
- **Re-share after merging** — if remaining workers need the updated main, call `workspace_share(path)` again so they can pull the latest

## Tool Reference

| Tool | Purpose | When to use |
|------|---------|-------------|
| `workspace_share(path)` | Push local repo to shared server | Before spawning workers |
| `workspace_activity(repo?)` | Branch list + ahead/behind + last commit | Monitor worker progress |
| `workspace_diff(branch, repo?)` | Unified diff of branch vs main | Review before collecting |
| `workspace_conflicts(branch, repo?)` | Conflict file list + both diffs | After a failed merge |
| `workspace_collect(path, worker, strategy?)` | Merge or review worker's branch | When worker is done |
