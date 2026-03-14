# Orchestration Patterns Reference

Detailed patterns for structuring multi-agent workflows. Each pattern describes
when to use it, how to set it up, and what to watch out for.

## Fan-Out / Fan-In

**When**: You have a large task that can be decomposed into independent subtasks.

**Structure**:
1. Parent analyzes the task and identifies independent work units
2. Call multiple `agent_run` in parallel — each worker gets one unit
3. Workers run concurrently on isolated branches or workspaces
4. Parent collects results from all workers as they complete
5. Parent synthesizes the combined output

**Best practices**:
- Keep subtasks genuinely independent — shared state creates coordination overhead
- For coding tasks, use `workspace_share` + `workspace=true` so each worker gets its own branch
- Monitor with `workspace_activity()` to see who's pushed and how far along
- Merge results sequentially with `workspace_collect` — simplest changes first

## Pipeline (Sequential Delegation)

**When**: Work flows through stages where each stage depends on the previous one.

**Structure**:
1. Parent spawns Agent A with the first stage task via `agent_run`
2. When A completes, parent reviews output and spawns Agent B with stage two
3. Continue until all stages are complete
4. Parent synthesizes the final result

**Best practices**:
- Each stage should produce clear, well-defined artifacts
- Review between stages with `workspace_diff` to catch issues early
- Re-share the workspace between stages so each agent builds on the last
- Keep pipeline stages focused — if a stage is too large, decompose it

## Explorer / Developer Split

**When**: A task requires both research and implementation.

**Structure**:
1. `agent_run(explorer, research_task)` — explores the problem space
2. Parent reviews the explorer's findings and formulates a plan
3. `workspace_share(path)` then `agent_run(developer, plan, workspace=true)`
4. Developer implements the solution on its task branch
5. Parent reviews with `workspace_diff` and collects with `workspace_collect`

**Best practices**:
- Give the explorer a clear research question, not a vague directive
- Summarize the explorer's findings before passing to the developer
- The developer should receive a concrete plan, not raw research output
- Consider adding a reviewer step after the developer completes

## Review Pattern

**When**: Code changes need independent verification before merging.

**Structure**:
1. Developer agent pushes changes to its task branch
2. Parent uses `workspace_diff(branch)` to get the diff
3. Parent spawns a reviewer: `agent_run(reviewer, "review this diff: {diff}")`
4. If changes pass review, parent merges with `workspace_collect`
5. If changes need revision, parent sends feedback to the developer via `agent_query`

**Best practices**:
- Pass the actual diff content to the reviewer, not just "review the code"
- Define clear review criteria (tests pass, no security issues, style compliance)
- Keep review cycles bounded — set a max number of revision rounds
- Use `workspace_conflicts(branch)` if the reviewer's feedback leads to rebases

## Specialist Team

**When**: A complex project requires diverse expertise.

**Structure**:
1. Parent shares workspace: `workspace_share(path)`
2. Spawn specialists in parallel, each with `workspace=true` and a focused role
3. Monitor with `workspace_activity()` to track all branches
4. Review each specialist's work with `workspace_diff(branch)`
5. Merge branches in dependency order with `workspace_collect`
6. Resolve conflicts using the `git-workspace` skill protocol

**Best practices**:
- Define clear interface contracts between specialists
- Merge frequently to catch integration issues early
- Re-share after each merge so remaining workers can pull the latest
- Store team composition in memory for reuse on similar projects
