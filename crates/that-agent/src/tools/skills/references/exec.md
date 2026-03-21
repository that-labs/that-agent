# Exec

Full flag reference for `that exec run` — timeout, streaming, working directory, and signal modes.

**Never use exec as a substitute for dedicated tools:**

| Don't do this | Do this instead |
|---------------|-----------------|
| `that exec run "sed -n 'N,Mp' file"` | `that code read file --line N --end-line M` |
| `that exec run "cat file"` | `that fs cat file` or `that code read file` |
| `that exec run "grep pattern dir"` | `that code grep pattern dir` |
| `that exec run "apply_patch ..."` | `that code edit file --search "..." --replace "..."` |
| `that exec run "ls dir"` | `that fs ls dir` |
| `that exec run "echo ... > file"` | `that fs write file --content "..."` |

## Quick start

```bash
# Build
that exec run "cargo build --release" --timeout 120 --max-tokens 1000

# Tests
that exec run "pytest tests/ -v" --timeout 60 --stream --max-tokens 2000

# Install dependencies
that exec run "npm install" --cwd frontend/ --timeout 120

# Custom Python extractor
that exec run "uv run --with beautifulsoup4 python3 - <<'PY'
import json, urllib.request
from bs4 import BeautifulSoup
html = urllib.request.urlopen('https://example.com').read()
soup = BeautifulSoup(html, 'html.parser')
print(json.dumps([el.get_text() for el in soup.select('div.result')]))
PY" --timeout 30 --max-tokens 1500
```

---

## run

```bash
that exec run "<command>" [--timeout N] [--cwd <dir>] [--signal graceful|immediate]
  [--stream] [--max-tokens N]
```

- `--timeout N` — kill after N seconds (default 30). Set higher for builds/installs.
- `--cwd <dir>` — working directory.
- `--stream` — emit output lines as JSONL in real time. Use for builds and test suites.
- `--signal graceful` — SIGTERM then SIGKILL (default). For commands needing cleanup.
- `--signal immediate` — SIGKILL directly. For stuck processes.
- `--max-tokens N` — budget: ~60% stdout / 20% stderr / 20% metadata.

Commands run in a new process group — on timeout the entire group is killed (no orphaned processes).

Output: `command`, `exit_code`, `stdout`, `stderr`, `elapsed_ms`, `timed_out`.

> **Policy:** Shell execution may be denied by default. Configure `policy.tools.shell_exec = "allow"` in `.that-agent/config.toml` or run `that init --profile agent`.
