<p align="center">
  <img src="./assets/logo.png" alt="that-agent" width="180" />
</p>

<h1 align="center">that-agent</h1>

<p align="center">
  <strong>The autonomous agent that writes and deploys its own tools.</strong><br>
  <strong>Rust. Production-ready. Grows with your business.</strong>
</p>

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Built_with-Rust-orange.svg" alt="Rust" /></a>
</p>

Most agent frameworks configure tools for the agent. `that-agent` gives the agent a compiler and a deployment target — it authors, ships, and runs its own plugins at runtime without operator intervention.

```
11 MB binary · <10 ms startup · self-authoring plugins · LLM-judged evals · cluster-aware fleet
```

## The Idea

Most frameworks hand an agent a fixed set of tools. `that-agent` gives the agent a compiler and a deployment target.

When the agent needs a new capability — an integration, a scheduled routine, a custom command — it authors a plugin, ships it to the cluster, and runs it. The operator writes nothing. The agent evolves through its own work. That is the loop this project exists to close.

The substrate beneath that loop is built for production: policy-governed tools, sandbox isolation, persistent memory, multi-channel routing, and a deterministic eval harness where an LLM judge scores behavioral regressions — not code paths, outcomes. Whether the agent runs a CLI task, holds a conversation on Telegram, or handles inbound webhooks, the same orchestration loop and policy gates are in play.

## In Practice

The agent's Kubernetes namespace is a fully equipped workshop. It has BuildKit for container builds, the cluster API for deployment and observability, an HTTP gateway it can extend with new routes, persistent volumes, and any operator installed in the cluster. These are examples of what that unlocks.

### Deploy a web UI

> *"Build me a dashboard for my running jobs."*

The agent writes the frontend, registers new routes on the HTTP gateway, builds a container image with BuildKit, and deploys it to its namespace. The gateway extension is live without a restart. Ask it to add authentication — it extends the same service in the next turn.

### Extend the HTTP gateway

The gateway is a first-class extension surface. Every plugin the agent ships can register new endpoints, webhook receivers, admin panels, or API bridges. The agent owns the routing table for its own namespace — adding a new route is part of the plugin authoring loop, not an operator task.

### Orchestrate ML training

> *"Fine-tune on the dataset in my workspace and alert me when done."*

The agent schedules the training job on GPU nodes, tails the pod logs, checkpoints progress, and sends metrics on completion. Connect it to a heartbeat routine and it monitors runs autonomously — retrying on failure, comparing runs, alerting when something needs human review.

### SRE on call — live cluster introspection

> *"Watch the ingress service and alert me if error rate spikes."*

The agent reads pod logs, describes failing deployments, and correlates cluster events. Attach it to a scheduled heartbeat and it becomes a persistent on-call SRE — watching conditions, filing structured reports, and surfacing only what needs human eyes.

### Secure service exposure with Tailscale

Install the Tailscale Kubernetes operator once. From that point forward, any service the agent deploys can be exposed to your Tailnet with a single annotation — no port forwarding, no public ingress, no manual VPN configuration. The agent understands the operator and uses it as part of the deployment.

### Compliance and security audit

> *"Audit my deployed services against our security baseline."*

The agent inspects running workloads, checks RBAC bindings, reviews pod security contexts and network policies, and produces a structured report. Schedule it as a recurring routine and every deployment stays continuously audited — not just at ship time.

### DevOps and release introspection

> *"Check if the last rollout is healthy and summarize what changed."*

The agent diffs the current and previous deployments, reads rollout status, inspects recent events, and surfaces a plain-language summary. Wire it into a post-deploy hook and every release gets an automatic health narrative written into the audit log.

### Persistent workspace, shared with sub-agents

The agent runs with a persistent volume mounted at `/workspace`. Sub-agents are spawned with a scoped view of the same volume — the parent writes, children read and extend. Work survives restarts, spans multiple agents, and never leaves your cluster. A parent orchestrating a fleet of specialists shares context with all of them through the workspace.

## Get Started

### Fresh VPS — one command

```bash
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

Installs k3s, prompts for agent name and API credentials, deploys the agent. The description you provide is interpreted by the LLM at first boot to generate the agent's identity.

### Docker

```bash
docker run -it --rm \
  -e ANTHROPIC_API_KEY \
  -e THAT_AGENT_NAME=my-agent \
  -v that-agent-home:/home/agent/.that-agent \
  -v that-workspace:/workspace \
  ghcr.io/that-labs/that-agent:latest \
  -c "that chat --agent my-agent --no-sandbox"
```

Two image variants are published to `ghcr.io/that-labs/that-agent`:

| Tag | Contents |
|---|---|
| `latest` / `v*` | Slim — Python, Git, curl, ripgrep, kubectl, Docker CLI |
| `latest-full` / `v*-full` | Full — adds Rust, Go, Node.js, TypeScript, Python dev packages |

### Kubernetes

```bash
cp -r deploy/k8s/overlays/example deploy/k8s/overlays/my-agent
# edit namespace, configmap, secret
kubectl apply -k deploy/k8s/overlays/my-agent
```

The default overlay pulls `ghcr.io/that-labs/that-agent:latest`. Same manifests work on a single node or across multiple regions. See [OPERATORS.md](./OPERATORS.md) for full configuration, environment variables, and observability setup.

### Pre-built binary

Download from [GitHub Releases](https://github.com/that-labs/that-agent/releases/latest):

```bash
# macOS (Apple Silicon)
curl -fsSL https://github.com/that-labs/that-agent/releases/latest/download/that-aarch64-apple-darwin.tar.gz | tar xz
sudo mv that /usr/local/bin/

# macOS (Intel)
curl -fsSL https://github.com/that-labs/that-agent/releases/latest/download/that-x86_64-apple-darwin.tar.gz | tar xz
sudo mv that /usr/local/bin/

# Linux (x86_64)
curl -fsSL https://github.com/that-labs/that-agent/releases/latest/download/that-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv that /usr/local/bin/
```

### From crates.io

```bash
cargo install that-cli
```

### From source

```bash
cargo build --release
# binary: ./target/release/that
```

## What You Get

- **Self-authoring plugins** — the agent writes, installs, upgrades, and removes its own runtime extensions; no operator required
- **Cluster-aware fleet** — plugins deployed by any sub-agent are visible across the whole cluster; policy flows downward, never upward
- **LLM-judged eval harness** — deterministic scenario runner scores autonomous behavior regressions; behavioral evals, not unit tests
- **Hot-reload everything** — channels, plugins, and agent identity update at runtime; no restart needed to grow
- **Persistent memory** — SQLite-backed recall that survives restarts and session boundaries
- **Full session continuity** — transcript reconstruction anchored at last compaction; no context loss on restart
- **Policy-governed tools** — every tool call passes through an Allow / Prompt / Deny gate; configurable per tool and per deployment
- **Sandboxed execution** — Docker and Kubernetes backends; destructive ops allowed inside the boundary, denied on host by default
- **Multi-channel routing** — Telegram, HTTP, and TUI through a unified abstraction; new channels register at runtime without restart
- **Heartbeat system** — autonomous listen mode with configurable wakeup cycles and scheduled routines

## Security

| # | Control | Status | How |
|---|---------|--------|-----|
| 1 | Tool policy gates | Done | Every call passes Allow / Prompt / Deny; configurable per tool |
| 2 | Sandbox isolation | Done | Docker and Kubernetes backends; destructive tools deny on host by default |
| 3 | Workspace scoping | Done | File tools restricted to agent workspace unless explicitly widened |
| 4 | Secrets via env | Done | No secrets in manifests; injected via Kubernetes secrets or `.env` |
| 5 | Policy hierarchy | Done | Sub-agents cannot elevate beyond the main agent's policy ceiling |
| 6 | Eval sandbox gating | Done | Scenarios requiring destructive ops must explicitly opt into sandbox mode |
| 7 | Audit log | Done | Every tool call recorded with outcome; structured and queryable |

## Architecture

```text
that-cli -------> that-core ---------> that-channels
  |                 |   |   |
  |                 |   |   +--------> that-plugins
  |                 |   +------------> that-sandbox
  |                 +----------------> that-tools
  +---------------------------------> that-tools

that-eval -------> that-core + that-tools
```

| Crate | Role |
|---|---|
| `that-core` | Orchestration runtime — agent loop, preamble, sessions, all execution paths |
| `that-tools` | Capability plane — fs, code, memory, search, exec, human, cluster — with policy gates |
| `that-sandbox` | Execution boundary — Docker and Kubernetes backends |
| `that-channels` | Channel router and adapters |
| `that-plugins` | Runtime extension plane — commands, activations, routines |
| `that-eval` | Behavioral scenario harness with LLM judge and structured reports |
| `that-cli` | Operator entrypoint — the `that` binary |

Full narrative in [ARCHITECTURE.md](./ARCHITECTURE.md).

## Quick Start

### Build

```bash
cargo build --release
# binary: ./target/release/that
```

### Configure

```bash
cp .env.example .env
```

Minimum: one LLM provider key. See `.env.example` for all options.

### Run

```bash
that run "Set up a small project and verify it compiles"
that chat
that listen
```

`run` — single task. `chat` — interactive session. `listen` — autonomous mode with heartbeat loop.

## Sandbox

```bash
./sandbox/build.sh
that run "Create a demo app and run its tests"
```

Docker is the default backend. `THAT_SANDBOX_MODE=kubernetes` for cluster-based isolation. `--no-sandbox` to run directly on host.

## Eval

```bash
that-eval list-scenarios
that-eval run <scenario>
that-eval run-all
that-eval report <run-id>
```

Scenarios in `evals/scenarios/`. Each is a TOML file with a natural-language prompt and acceptance criteria. An LLM judge scores the agent's behavior — not the code path, the outcome. Reports stored under the agent's eval directory.

## Project Stats

```
Language:     Rust
Source files: ~148
Tests:        517+
Binary:       11 MB (release)
Startup:      <10 ms
Providers:    Anthropic, OpenAI, OpenRouter
Channels:     Telegram, HTTP gateway, TUI
Deployment:   single VPS · multi-region Kubernetes · Docker sandbox
```

## Project Layout

```text
crates/
  that-cli/          # operator binary
  that-core/         # orchestration runtime
  that-tools/        # tool engine + policy
  that-sandbox/      # container backends
  that-channels/     # channel router + adapters
  that-plugins/      # plugin registry
  that-eval/         # eval runner + judge
evals/scenarios/     # TOML scenario definitions
sandbox/             # Dockerfile + build script
deploy/k8s/          # Kubernetes manifests
```

## Contributing

Contributions welcome. See [CONTRIBUTING.md](./CONTRIBUTING.md) for guidelines, first-contribution paths, and development workflow.

## License

MIT. See [LICENSE](./LICENSE).

---

**that-agent** — The agent that builds itself. Start small. Scale anywhere.
