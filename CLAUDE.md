# that-agent Workspace

Rust workspace — see README for layout, ARCHITECTURE.md for design detail.

## Practices: What NOT To Do

### Dependencies

- **No heavy native bindings.** Never add crates that vendor C libraries (OpenSSL, libgit2, libssh2, etc.). We removed `git2` specifically because it pulled 915KB of `openssl_sys` + `libgit2_sys` for work the `git` CLI already does. If a shell command covers the use case, use `std::process::Command` — not a binding crate.
- **No new deps without justifying binary cost.** Run `cargo bloat --release -p that-cli --crates` before and after. If a crate adds >50KB for a non-core feature, it must be feature-gated or rejected.
- **No vendored TLS stacks.** The workspace uses `rustls` everywhere. Never introduce `openssl`, `native-tls`, or `vendored-openssl` features.
- **No duplicate functionality.** Before adding a crate, check if an existing dep or `std` already covers it. One HTTP client (`reqwest`), one TLS (`rustls`), one async runtime (`tokio`).

### Code Size

- **As few lines as possible.** Prefer composition over duplication, thin abstractions over defensive layering, deletion over accumulation. 10 lines instead of 30.
- **No dead code.** If it's unused, delete it. No `#[allow(dead_code)]` on production paths.
- **No over-abstraction.** Three similar lines are better than a premature helper function.

### Naming & Legacy

- The old project name was **"anvil"**. All references have been migrated to `that-tools` / `that-agent`. Never reintroduce "anvil" in code, comments, or docs.

### Agent / Skill Prompts

- NLP-driven, generic language only. No real file paths, component names, or model IDs as examples.
- Model preferences: OpenAI runs use `gpt-5.2-codex` or higher. Never `gpt-4.x`.

## Architecture Gotchas

### Policy Enforcement

`load_agent_config(container)` **must** be called in every execution path (streaming, TUI, eval, channel). Missing it silently uses restrictive defaults even in sandbox mode — symptom: `policy denied` on tools that should be allowed.

Destructive tools (`fs_delete`, `shell_exec`, `fs_write`, `code_edit`, `git_commit`, `git_push`) default to Deny on host, Allow in sandbox.

### Preamble ↔ Struct Sync

Adding a field to any agent-facing struct (Heartbeat, channel config, etc.) without updating the preamble guidance in `orchestration/preamble.rs` means the LLM will never use it. The preamble is the agent's only source of truth.

### Channel Router

`ChannelRouter` fans out to all adapters. `TuiChannel` lives in `that-core::tui` (not `that-channels`) to avoid a circular dep. Channel adapters must clear their text buffer on every tool-call boundary.

### Unicode Truncation

Never `&str[..n]` — panics on multi-byte codepoints. Always char-based:
```rust
s.chars().filter(|c| !c.is_control()).take(120).collect::<String>()
```

### Skills

`name:` and `description:` must be at the YAML **root** (not nested under `metadata:`). Missing root-level fields → silent skip during discovery. Skills hot-reload from `skills/` — no restart needed. Keep body under 400 lines.

### Memory

`Memory.md` is a pointer index — never paste full content. Full content lives in `memory.db` via `mem_recall`. Each agent has an isolated `memory.db`. History reconstruction anchors at the last `Compaction` event.

### Heartbeat

`urgent` fires immediately then follows schedule. `not_before: <RFC3339>` gates firing. `status: done` permanently disables. Schedules: `once | minutely | hourly | daily | weekly | cron: <expr>`. `timezone` on `AgentDef` controls wall-clock schedules.

### Eval Scenarios

Prompts must read like human requests — never name tools, skills, or internal workflows. Scenarios needing destructive ops must set `sandbox = true`.

## Pre-commit

`cargo fmt --all && cargo clippy --workspace -- -D warnings` — CI rejects unformatted code and warnings.
