---
name: that-tools
description: CLI toolkit for agent tasks. Use for source code (read, grep, tree, symbols, edit), filesystem (ls, cat, write), web search (query, fetch), persistent memory (add, recall), shell execution (exec run), and human-in-the-loop (ask, approve). Subcommand names differ from intuition — `grep` not 'search', `ls` not 'list', `cat` not 'read', `add` not 'store', `query` not 'search'. Paths always positional — no `--path` flag. `that code edit --search/--replace` works on ANY file type (md, json, yaml, not just code) — use it for partial changes; `that fs write` only for new files or full rewrites. Never use exec+sed/cat/grep/apply_patch.
---

# that-tools

All operations: `that-tools <group> <subcommand> [args]`. Paths are **always positional** — no `--path` flag.

## Quick Reference

| Group | Subcommands | Watch out |
|-------|-------------|-----------|
| `that code` | `grep` `read` `tree` `symbols` `edit` `ast-grep` `index` `summary` | Use `grep` not `search`; `grep <pattern> [path]` — pattern first; `read --line N --end-line M` not sed |
| `that fs` | `ls` `cat` `write` `mkdir` `rm` | `write` = new file or full replace only; use `that code edit --search/--replace` for partial changes to ANY file |
| `that search` | `query` `fetch` | Use `query` not `search`; never use curl/wget |
| `that mem` | `add` `recall` `search` `compact` `unpin` `remove` `prune` `stats` `export` `import` | `add` = individual fact; `compact` = session distillation; `unpin` = retire stale summary |
| `that session` | `init` `add-tokens` `stats` `reset-context` | `compact` auto-resets context; `reset-context` for manual override |
| `that exec` | `run` | Only subcommand is `run`. Never use for sed/cat/grep/apply_patch |
| `that human` | `ask` `pending` `approve` `confirm` | Use `pending` not `list` |
| `that code index` | `[path]` `[path] --status` | `--status` is a flag, not a subcommand |

## Most common commands

```bash
that code grep "pattern" [path] --context 2 --limit 20   # search code
that code read src/file.rs --symbols --max-tokens 400     # read with symbol index
that code read src/file.rs --line 100 --end-line 200      # read a section (no sed!)
that code tree . --depth 3 --compact                      # map repo structure
that fs ls /path                                          # list directory
that fs cat config.json --max-tokens 600                  # read a file
that fs write notes.md --content "line1\nline2"           # write inline content
that search query "topic" --limit 5 --max-tokens 1200     # web search
that search fetch https://... --mode scrape               # fetch a URL
that mem recall "topic"                                   # retrieve memories
that mem add "fact" --tags "category"                     # store a specific fact
that mem compact --summary "decisions made this session" --session-id "$SESSION_ID"  # distil + auto-reset context
that mem unpin <id>                                       # retire stale compaction summary
that session stats --session-id "$SESSION_ID"             # check flush_recommended
that session reset-context --session-id "$SESSION_ID"    # manual context reset after compaction
that exec run "command" --timeout 30 --max-tokens 1000    # run shell command
```

## Before working with files or code

Load the detailed reference for the tool group you're about to use:

```bash
that skills read code   # before working with source code (grep, read, edit, symbols)
that skills read fs     # before reading or writing files (ls, cat, write)
```

## Full reference

For complete flag reference and examples, run `that skills read <name>`:

```bash
that skills read code     # grep, read, tree, symbols, edit, ast-grep, summary
that skills read fs       # ls, cat, write, mkdir, rm
that skills read search   # query, fetch (scrape/inspect/markdown/text modes)
that skills read memory   # add, recall, search, remove, prune, stats, export/import
that skills read exec     # run with timeout, streaming, signal modes
that skills read human    # ask, approve, confirm, pending
that skills read index    # code index build and --status
```
