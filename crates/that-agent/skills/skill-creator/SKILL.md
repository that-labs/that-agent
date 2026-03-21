---
name: skill-creator
description: Guide for creating effective, reusable skills. Use when you need to create a new skill or improve an existing one that captures specialized knowledge, workflows, or domain expertise worth preserving across sessions and agents.
metadata:
  bootstrap: true
  version: 1.1.0
---

# Skill Creator

Skills are modular, self-contained knowledge packages that extend an agent's capabilities
with specialized workflows, domain expertise, and reusable assets. They are not framework
tutorials or simple how-tos — they are curated, distilled knowledge that an agent would
otherwise have to rediscover repeatedly.

## What Makes a Skill Worth Creating

A skill earns its place when it captures knowledge that is:

- **Non-obvious** — things the agent cannot reliably derive from general training
- **Reusable** — applies across multiple tasks or sessions, not a one-time procedure
- **Costly to rediscover** — domain schemas, tool quirks, organizational conventions, hard-won
  patterns from real failures
- **Directional** — gives the agent a heading, not a script; helps it make better decisions
  under uncertainty

Do not create skills for things that are already well-understood, or for one-off tasks.
A skill that restates widely available knowledge wastes context and degrades trust.

## Where Skills Live

Your skills directory is shown in the **Skills** section of your system context.
Always create and edit skills at that path — it is the only location that is
scanned at startup, hot-reloaded at runtime, and accessible from inside the sandbox.

```
<your skills directory>/
└── skill-name/
    ├── SKILL.md          (required)
    └── references/       (optional — loaded on demand)
        └── topic.md
```

### SKILL.md (required)

Every SKILL.md has two parts:

**Frontmatter (YAML):**
```yaml
---
name: skill-name
description: What the skill does and when to invoke it. This is the trigger — be specific.
metadata:
  bootstrap: true        # only for skills bundled with that-agent itself
  always: false          # inject full body into preamble without requiring read_skill
  version: 1.0.0         # informational semver string; no validation performed
  os:                    # allowed platforms; omit to allow any OS
    - darwin
    - linux
  binaries:              # all of these must be on PATH or the skill is skipped
    - some-tool
  any_bins:              # at least one of these must be on PATH
    - bun
    - node
  envvars:               # environment variables the skill requires; all must be set
    - ${API_KEY}         # resolved from the environment at load time
    - ALIAS: ${VAR}      # exposed as ALIAS, resolved from VAR
---
```

Only `name` and `description` are read during catalog scan. The body is only loaded when
the skill is invoked. All optional fields live under `metadata:` (2-space indent, standard YAML):

- `bootstrap: true` — marks skills auto-installed on agent startup; reserved for skills shipped with that-agent
- `always: true` — injects the full skill body into the system prompt on every turn without requiring `read_skill`; the skill is excluded from the catalog list since its content is already present; only use for compact (under ~100 lines), universally-relevant skills that apply to nearly every task
- `version` — informational semver string; stored but not validated
- `os` — list of allowed OS names (`darwin`, `linux`, `win32`); skill is skipped on non-matching platforms; omit to allow any OS
- `binaries` — list of binary names that must all be executable on PATH; skill is skipped if any are missing
- `any_bins` — list of binary names where at least one must be on PATH; useful for interchangeable runtimes
- `envvars` — environment variable specs resolved from the process environment at load time; use `${VAR}` or `ALIAS: ${VAR}` per entry; skill is skipped if any are unset

**Body (Markdown):**
Instructions the agent follows after loading the skill. Keep it under 400 lines.
Reference files handle everything else.

### References (optional)

Files in `references/` are loaded on demand via `read_skill`. Use them for:
- Schemas, API specs, domain models
- Detailed workflow variants
- Policy or convention documents too long for SKILL.md

Reference them explicitly in SKILL.md so the agent knows they exist and when to read them.

## Design Principles

### Context is a shared resource

The context window is shared between the system prompt, conversation history, all skill
metadata, and the user's actual task. Every token in a skill competes with something else.

**Challenge every line:** does the agent need this, or does it already know it?
Prefer a concrete example over three paragraphs of explanation.

### Set the right degree of freedom

Match specificity to how fragile and consistent the task is:

| Situation | Approach |
|-----------|----------|
| Multiple valid approaches, context-dependent | High-level guidance, decision heuristics |
| Preferred pattern with acceptable variation | Annotated example or pseudocode |
| Fragile sequence where consistency is critical | Explicit step-by-step, few parameters |

### Progressive disclosure

Skills use three loading levels to stay lean:

1. **Metadata** (name + description) — always in context, ~100 words
2. **SKILL.md body** — loaded when the skill triggers, keep under 400 lines
3. **Reference files** — loaded on demand, unlimited depth

When a skill grows large, split details into `references/` files and link to them from
SKILL.md with clear guidance on when to read each.

## Creating a Skill

### 1. Understand what knowledge is worth preserving

Start from real tasks. What did the agent struggle with? What did it have to rediscover?
What would a domain expert tell a new hire on day one that isn't in any docs?

### 2. Identify the reusable assets

For each concrete use case, ask:
- Is there a schema or reference document worth capturing?
- Is there a script that gets rewritten repeatedly?
- Is there a decision pattern that should be consistent?

### 3. Write the skill

Create the skill directory and SKILL.md inside your skills directory
(shown in the **Skills** section of your context):

```bash
mkdir -p <your-skills-directory>/my-skill/references
```

Write frontmatter first — getting the description right is the most important step.
The description determines when the skill triggers. Include both what it does and specific
conditions that should trigger it.

Decide on loading mode:
- Default (no `always:`): progressive loading via `read_skill` — best for most skills
- `always: true`: body injected unconditionally — only for skills under ~100 lines that apply
  to nearly every task; anything larger burns context on irrelevant turns

Set eligibility constraints if the skill is environment-specific:
- `binaries:` — any binary the skill actively uses (e.g. `forge`, `cast`)
- `any_bins:` — when the skill supports interchangeable tools (e.g. `bun` or `node`)
- `os:` — only if behavior or paths differ across platforms and the skill can't handle both
- `envvars:` — only if the skill cannot function at all without the variable

Write the body as instructions to a capable agent, not a tutorial for a beginner.
Use imperative form. Omit what the agent already knows.

### 4. Validate by simulation

Before committing, mentally simulate the agent reading the skill mid-task:
- Does the skill give the agent a clear heading?
- Is there anything in the body it would already know without the skill?
- Are reference files linked with clear loading conditions?
- Is the description specific enough to trigger correctly and not on unrelated tasks?

### 5. Iterate from real use

The best improvements come right after a real task — while the gap between what the skill
said and what was actually needed is still fresh. Keep a short feedback loop.

## What to Avoid

- **Restating general knowledge** — if it's in training data, don't repeat it
- **`always: true` on large skills** — anything over ~100 lines costs context on every single turn regardless of relevance; keep always-skills focused and short
- **Over-constraining with eligibility** — only gate on `binaries`/`envvars` that are actually required; loose gating skips the skill when the environment is fine
- **Auxiliary files** — no README.md, CHANGELOG.md, or INSTALLATION.md in a skill directory
- **Deeply nested references** — keep all reference files one level deep, linked from SKILL.md
- **Vague descriptions** — "helps with data tasks" triggers on everything and nothing
- **Overfitting to one task** — a skill should generalize across multiple instances of the same domain
