# Orchestration Patterns Reference

Detailed patterns for structuring multi-agent workflows. Each pattern describes
when to use it, how to set it up, and what to watch out for.

## Fan-Out / Fan-In

**When**: You have a large task that can be decomposed into independent subtasks.

**Structure**:
1. Parent analyzes the task and identifies independent work units
2. Create one tracked task per work unit with `agent_task(action=send, ...)`
3. If units must coordinate, attach peers with `agent_task(action=share, ...)` so they see the same scratchpad
4. Workers run concurrently on isolated branches or workspaces
5. Parent supervises through scratchpad reads, task status, and workspace activity
6. Parent synthesizes the combined output

**Best practices**:
- Keep subtasks genuinely independent unless they explicitly need a shared scratchpad
- Put stable goal/workspace/participant policy in the scratchpad header, and live coordination in the activity tail
- For coding tasks, use `workspace_share` + `workspace=true` so each worker gets its own branch
- Monitor with `workspace_activity()` to see who's pushed and how far along
- Merge results sequentially with `workspace_collect` — simplest changes first

## Pipeline (Sequential Delegation)

**When**: Work flows through stages where each stage depends on the previous one.

**Structure**:
1. Parent creates a tracked task for stage one
2. Parent reviews the scratchpad and output from stage one
3. Parent either steers the same task into the next stage or starts a new tracked task for stage two
4. Continue until all stages are complete
5. Parent synthesizes the final result

**Best practices**:
- Each stage should produce clear, well-defined artifacts
- Review between stages with the scratchpad plus `workspace_diff` to catch issues early
- Re-share the workspace between stages so each agent builds on the last
- Keep pipeline stages focused — if a stage is too large, decompose it

## Explorer / Developer Split

**When**: A task requires both research and implementation.

**Structure**:
1. `agent_task(action=send, name=explorer, ...)` — explores the problem space
2. Parent reviews the explorer's scratchpad header/activity and formulates a plan
3. `workspace_share(path)` then `agent_task(action=send, name=developer, ...)` or `agent_run(..., workspace=true)` depending on whether mid-flight supervision is needed
4. Developer implements the solution on its task branch
5. Parent reviews with the scratchpad, `workspace_diff`, and `workspace_collect`

**Best practices**:
- Give the explorer a clear research question, not a vague directive
- Promote the stable findings into the developer task header before implementation starts
- The developer should receive a concrete plan, not raw research output
- Consider adding a reviewer step after the developer completes

## Review Pattern

**When**: Code changes need independent verification before merging.

**Structure**:
1. Developer agent pushes changes to its task branch
2. Parent uses `workspace_diff(branch)` to get the diff
3. Parent creates or steers a reviewer task and writes review criteria into the scratchpad header/activity
4. If changes pass review, parent merges with `workspace_collect`
5. If changes need revision, parent sends feedback to the developer via `agent_task(action=send, task_id, ...)`

**Best practices**:
- Pass the actual diff content to the reviewer, not just "review the code"
- Define clear review criteria (tests pass, no security issues, style compliance)
- Keep review cycles bounded — set a max number of revision rounds
- Use `workspace_conflicts(branch)` if the reviewer's feedback leads to rebases

## Specialist Team

**When**: A complex project requires diverse expertise.

**Structure**:
1. Parent shares workspace: `workspace_share(path)`
2. Spawn specialists in parallel, each with a focused role and a tracked task when supervision is needed
3. If specialists must coordinate, attach them to the same task or cross-reference task IDs in scratchpad notes
4. Monitor with `agent_task(action=status)`, scratchpad reads, and `workspace_activity()` to track all branches
5. Review each specialist's work with `workspace_diff(branch)`
6. Merge branches in dependency order with `workspace_collect`
7. Resolve conflicts using the `git-workspace` skill protocol

**Best practices**:
- Define clear interface contracts between specialists
- Keep parent-visible coordination in scratchpad activity notes, not private reasoning
- Merge frequently to catch integration issues early
- Re-share after each merge so remaining workers can pull the latest
- Store team composition in memory for reuse on similar projects
