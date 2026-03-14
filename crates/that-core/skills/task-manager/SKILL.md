---
name: task-manager
description: Guide for creating and managing hierarchical tasks using the folder-based epic/story/task structure. Use when planning multi-session work, tracking a project backlog, or organizing complex goals into actionable steps.
metadata:
  bootstrap: true
  always: false
  version: 1.1.0
---

# Task Manager

This skill describes how to create, navigate, and update the agent's folder-based task system.
The system lives in the agent directory under `tasks/` and uses a three-level hierarchy:
**Epic → Story → Task**. Every file is a markdown document focused on intent — capturing
*why* something matters, not just *what* to do.

## When This Is Mandatory

Use the task system for any work that is more than a trivial one-shot. If the task has multiple
meaningful steps, touches multiple files/subsystems, may span more than one session, or needs
checkpoint visibility, you must create or update task files before going deep into implementation.

The minimum loop is:

1. Read `Tasks.md` first
2. Create or update the relevant epic/story/task entry
3. Mark the active task `in-progress` before substantial work
4. Keep status current while you work
5. Use `channel_notify` at meaningful checkpoints so the human sees progress
6. Mark the task tree done when finished so no stale `in-progress` entry remains
7. Write a `mem_add` memory chunk summarizing what changed, what matters, and any follow-up

## Directory Structure

```
<agent-dir>/
  Tasks.md                           ← Index: lists all epics with status
  tasks/
    epic-NNN-<slug>/
      epic.md                        ← Epic intent, goal, success criteria
      story-NNN-<slug>/
        story.md                     ← Story intent, acceptance criteria
        task-NNN-<slug>.md           ← Individual task with full intent
```

- **NNN** — a zero-padded sequence number (001, 002, …) for stable sorting
- **slug** — a short, lowercase, hyphenated label derived from the title
- Numbers restart at 001 within each parent (stories restart per epic, tasks per story)

## File Formats

### Tasks.md (index)

```markdown
# Tasks

Index of all work. Each epic lives in its own directory under `tasks/`.

## Active Epics

- [Short epic title](tasks/epic-001-<slug>/epic.md) — in-progress
- [Short epic title](tasks/epic-002-<slug>/epic.md) — todo

## Summary
N epics · N stories · N tasks · N done
```

Keep `Tasks.md` as a thin index only. No detail lives here — follow the links.

### epic.md

```markdown
# Epic: <Title>

**Status**: todo | in-progress | done
**Created**: YYYY-MM-DD
**Goal**: One sentence stating the measurable outcome this epic achieves.

## Intent
A paragraph or two explaining what problem this epic solves, why it matters,
and any constraints or design principles that should guide the work.

## Success Criteria
- [ ] Specific, measurable outcome
- [ ] Another measurable outcome

## Stories
- [Story title](story-001-<slug>/story.md) — todo
```

### story.md

```markdown
# Story: <Title>

**Epic**: <Epic title>
**Status**: todo | in-progress | done
**Priority**: urgent | high | normal | low

## Intent
What user-facing or agent-facing value this story delivers. Written from the
perspective of the capability or behavior being added, and why it matters.

## Acceptance Criteria
- [ ] Concrete, observable outcome 1
- [ ] Concrete, observable outcome 2

## Tasks
- [ ] [Task title](task-001-<slug>.md)
- [x] [Completed task](task-002-<slug>.md)
```

### task-NNN-slug.md

```markdown
# Task: <Title>

**Story**: <Story title>
**Status**: todo | in-progress | done
**Priority**: urgent | high | normal | low

## Intent
What exactly this task accomplishes and why. Should be concrete enough that
completing it has a clear, verifiable signal.

## Definition of Done
- [ ] Specific outcome 1
- [ ] Specific outcome 2
```

## Workflow

### Starting a new project

1. Create `tasks/` directory if it does not exist
2. Create `Tasks.md` with the index template
3. Create the first epic directory: `tasks/epic-001-<slug>/`
4. Write `epic.md` inside it — start with the goal and intent
5. Break the epic into stories; create `story-001-<slug>/` directories
6. Write `story.md` for each story with acceptance criteria
7. Create task files inside each story directory
8. Update `Tasks.md` to list the new epic

### Working on tasks

1. Read `Tasks.md` to orient yourself at the start of a session
2. Follow links to the relevant epic and story
3. Update `**Status**: in-progress` when you begin work
4. Send a short `channel_notify` update when you finish a milestone, hit a blocker, or change direction materially
5. Check off `- [x]` items in acceptance criteria / definition of done as you complete them
6. Update `**Status**: done` when the task is complete
7. Update the parent story's task list checkbox
8. Update the story `**Status**` once all tasks are done
9. Update the epic similarly
10. Store a `mem_add` chunk with the outcome, key decisions, important files, and next steps

### Resuming multi-session work

At the start of a new session on an ongoing project:
1. Read `Tasks.md` to get the current state
2. Find stories or tasks with `**Status**: in-progress`
3. Read those files fully to understand context before acting

## Guiding Principles

**Intent over procedure.** Every file must answer *why* — why does this epic exist,
why does this story matter, why is this task worth doing. Never write files that list
steps without explaining the goal they serve.

**Hierarchy reflects scope.** Epics are large, multi-session goals. Stories are
week-scale units of value. Tasks are hour-scale concrete actions. If a task feels
too large, split it into a story.

**Keep Tasks.md thin.** It is a navigation aid, not a source of truth. All detail
lives in the individual files.

**Update as you go.** Stale task files are worse than no task files. Update status
when work begins and when it completes.

**Clear finished work.** When the work is done, close the task properly. Do not leave stale
`in-progress` markers behind in `Tasks.md`, stories, or tasks.

**Leave a durable breadcrumb.** After finishing meaningful work, add a concise `mem_add`
summary so the next session can resume from facts instead of re-discovery.
