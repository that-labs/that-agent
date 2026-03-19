# Local Worktree Orchestration

This reference covers git worktree-based multi-agent collaboration for **non-Kubernetes**
deployments (local Docker sandbox, host-based agents). For K8s workspace sharing, use the
parent `git-workspace` skill instead.

## When to Use

Use worktrees when all agents share the same filesystem — they work on isolated branches
via `git worktree` without needing a network git server.

## Core Workflow

### Setting Up Isolated Work

1. `worktree_create(repo, agent_name)` — creates a timestamped branch and dedicated working directory
2. Direct the agent to work exclusively within its worktree path
3. The agent commits changes normally — they stay on its isolated branch

### Reviewing Work

1. `worktree_diff(repo, agent_name)` — see all changes since the branch diverged
2. `worktree_log(repo, agent_name)` — see the commit history
3. If changes need revision, communicate with the agent and let it continue

### Merging Completed Work

1. `worktree_merge(repo, agent_name)` — creates a no-fast-forward merge into the target branch
2. If conflicts occur, the merge is aborted and conflict files are reported
3. `worktree_discard(repo, agent_name)` — clean up the worktree after merging

### Listing Active Worktrees

`worktree_list(repo)` — see all active agent worktrees, their branches, and paths.

## Multi-Agent Pattern

1. Create one worktree per agent — each gets its own isolated branch
2. Assign tasks — each agent works within its worktree directory
3. Review incrementally — check diffs as agents report progress
4. Merge in order — one agent at a time to manage conflicts
5. Clean up — discard worktrees after merging

## Principles

- **Isolation first** — never have two agents commit to the same branch simultaneously
- **Review before merge** — always check the diff before merging
- **Merge sequentially** — one at a time keeps conflict resolution manageable
- **Clean up** — discard worktrees once branches are merged
