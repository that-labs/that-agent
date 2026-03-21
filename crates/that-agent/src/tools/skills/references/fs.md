# Filesystem Tools

Full flag reference for `that fs` — ls, cat, write, mkdir, rm.

## Quick start

```bash
# List a directory
that fs ls /workspace

# Read a file
that fs cat config.json --max-tokens 600

# Write a file — use --content for inline text (preferred for agents)
that fs write notes.md --content "## Title\nLine one.\nLine two."

# Dry-run first when overwriting
that fs write config.json --content '{"key": "value"}' --dry-run
```

---

## ls

```bash
that fs ls [path] [--max-depth N] [--max-tokens N]
```

```bash
that fs ls .
that fs ls src/ --max-depth 3 --max-tokens 300
```

Output: `entries[]` (name, path, type, size), `total`. For source-code-aware project maps, prefer `that code tree`.

---

## cat

```bash
that fs cat <path> [--max-tokens N]
```

Large files are compacted (head + tail). For source code, prefer `that code read --symbols`.

---

## write

```bash
# Inline content (preferred — no stdin escaping issues)
that fs write <path> --content "line1\nline2\nline3" [--dry-run] [--backup]

# From stdin
echo "content" | that fs write <path> [--dry-run] [--backup]
cat file.txt   | that fs write <destination>
```

`--content` interprets `\n` as newline and `\t` as tab. Always `--dry-run` first when overwriting. `--backup` keeps a `.bak` copy.

**When to use write vs edit:**
- `that fs write` — create a new file, or replace the entire content
- `that code edit --search "old" --replace "new"` — change part of a file (ANY file type, not just code)

---

## mkdir

```bash
that fs mkdir <path> [-p]    # -p creates parent directories
```

---

## rm

```bash
that fs rm <path> [-r] [--dry-run]
```

Always `--dry-run` first. `-r` removes directories recursively. This is destructive — policy may deny it.
