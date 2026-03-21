# Symbol Index

Full flag reference for `that code index` — build, update, and health-check the cross-file symbol index.

## Quick start

```bash
# Build the index (run once per session)
that code index .

# Check health before relying on ranked/reference results
that code index . --status

# After indexing: importance-ranked file tree
that code tree . --ranked --compact

# After indexing: where is a symbol used across the codebase?
that code symbols src/config.rs --references
```

---

## index

```bash
that code index [path]           # build or update (incremental — only changed files re-parsed)
that code index [path] --status  # report health
```

`--status` output: `total_files`, `total_symbols`, `total_refs`, `stale_files`, `schema_version`.

**Decision:** if `stale_files > 0`, rebuild before using `--references` or `--ranked`.

The index is stored in `.that-agent/index.db` at the project root. Build time is proportional to project size; subsequent runs are fast.
