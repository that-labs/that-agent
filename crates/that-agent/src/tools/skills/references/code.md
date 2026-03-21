# Code Tools

Full flag reference for `that code` — grep, read, tree, symbols, edit, ast-grep, index, summary.

## Quick start

```bash
# Read a file with symbols
that code read src/main.rs --symbols --max-tokens 400

# Read a specific line range (replaces sed -n 'N,Mp')
that code read src/main.rs --line 100 --end-line 200

# Search for a pattern (pattern first, path second — no --path flag)
that code grep "clanker" /workspace/project --context 2

# Map the repo
that code tree . --depth 3 --compact --max-tokens 300

# List all functions in a file
that code symbols src/main.rs --kind function
```

---

## read

```bash
that code read <file> [--symbols] [--line N] [--end-line N] [--context N] [--max-tokens N]
```

- `--symbols` — prepend function/struct/class index. Use first on unfamiliar files.
- `--line N --end-line M` — read exactly lines N through M (replaces sed/head/tail).
- `--line N` + `--context N` — focus output around a specific line (center ± radius).

**Never use `that exec run "sed -n 'N,Mp'"`** — use `--line N --end-line M` instead.

---

## grep

```bash
that code grep <pattern> [path] [-i] [-r] [--context N] [--limit N]
  [--include "*.rs"] [--exclude "target/**"] [--max-tokens N]
```

`<pattern>` is the first argument. `[path]` defaults to `.`. **There is no `--path` flag.**

```bash
that code grep "fn handle_" src/ --context 2 --limit 20
that code grep "TODO" . -i --limit 50 --max-tokens 800
that code grep "import.*axios" -r --include "*.ts" --context 3
```

Output: `file_matches[]`, `returned_matches`, `total_matches`, `matched_files`.

---

## tree

```bash
that code tree [path] [--depth N] [--compact] [--ranked] [--max-tokens N]
```

First orientation step in an unfamiliar project. `--compact` for dense ASCII output.

---

## symbols

```bash
that code symbols [path] [--kind KIND] [--name PATTERN] [--references]
```

Kinds: `function` `struct` `class` `enum` `interface` `method` `module` `constant` `type`

---

## edit

Works on **any text file** — markdown, JSON, YAML, config files, and source code alike.

```bash
# Targeted search/replace — preferred for all partial changes to any file
that code edit <file> --search "old text" --replace "new text" [--dry-run]

# Multi-line search/replace — \n is interpreted as newline
that code edit notes.md --search "## Name\nMos" --replace "## Name\nMoshon"

# Replace a named function's body — provide the inner body only, signature is kept automatically
that code edit <file> --fn function_name --new-body "\treturn nil, nil" [--dry-run]

# Unified diff from stdin
that code edit <file> --patch < changes.diff
```

Always `--dry-run` first. Creates a git checkpoint before applying.

Output fields:
- `applied` — true if the file was written (false on dry-run)
- `validated` — true if tree-sitter syntax-checked the result (only for .rs/.go/.py/.ts). `false` on .md/.json/etc. is normal — it just means no syntax check was done, not that the edit failed.

**Use `--search/--replace` for any partial change to any file** — even `.md`, `.json`, `.yaml`. Only use `that fs write` when creating a new file or replacing it entirely.

**Never use `apply_patch`** — use `that code edit` instead.

---

## ast-grep

Structural search using tree-sitter S-expressions — matches code shape, not text.

```bash
that code ast-grep "(function_item name: (identifier) @name)" src/
that code ast-grep "(function_definition name: (identifier) @name)" . --language python
```

Languages: `rust` `typescript` `javascript` `python` `go`

---

## index + summary

```bash
that code index .                      # build cross-file symbol index
that code index . --status             # check if stale (stale_files > 0 → rebuild)
that code summary . --max-tokens 2000  # module structure + public API overview
```

Build index once per session. Enables `--references` on `symbols` and `--ranked` on `tree`.
