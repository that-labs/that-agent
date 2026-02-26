<p align="center">
  <img src="./assets/logo.png" alt="that-agent" width="180" />
</p>

# that-agent

An open-source Rust framework for long-lived autonomous agents.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/Built_with-Rust-orange.svg)](https://www.rust-lang.org/)

## The Idea

Most agent frameworks hand the agent a set of tools and call it done. `that-agent` starts from a different premise: **the agent manages its own home.**

Its capabilities, its deployed services, its environment — everything is expressed as plugins it can write, install, upgrade, and remove at runtime. Software is not something an operator configures for the agent; it is something the agent authors and deploys for itself. A new integration, a scheduled routine, a custom command — the agent builds it as a plugin, ships it, and runs it. That is the core loop this project exists to make possible.

The foundation underneath that loop — one runtime, one tool stack, one continuity model across every execution path — is deliberately stable. Sandboxing, memory, channels, and the eval harness are not afterthoughts; they are the substrate that makes autonomous self-management safe and testable. Whether the agent is running a CLI task, holding a TUI conversation, listening on Telegram, or being scored against an eval scenario, the same orchestration loop and the same policy-governed tools are in play. The result is an agent that behaves consistently no matter how you talk to it, evolves through its own work rather than operator intervention, and can be evaluated against a regression suite at any point in its development.

## What You Get

- **Persistent memory** -- SQLite-backed recall that survives restarts and session boundaries
- **Session transcripts** -- full history reconstruction for auditing and continuity
- **Workspace identity** -- structured files (Soul, Identity, Agents, User, Tools) that shape agent character and behavior
- **Heartbeat system** -- autonomous listen mode with configurable wakeup cycles
- **Policy-governed tools** -- every tool call passes through an Allow / Prompt / Deny gate
- **Sandboxed execution** -- Docker and Kubernetes backends for elevated autonomy in isolation
- **Multi-channel routing** -- Telegram, HTTP, and TUI through a unified channel abstraction
- **Plugin system** -- runtime extensions via commands, activations, and routines
- **Eval harness** -- deterministic scenario runner with an LLM judge for regression testing autonomous behavior

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
| `that-core` | Orchestration runtime -- agent loop, preamble, sessions, run/chat/listen/eval/channel paths |
| `that-tools` | Typed capability plane -- fs, code, memory, search, exec, human -- with policy gates |
| `that-sandbox` | Execution boundary -- Docker and Kubernetes backends |
| `that-channels` | Channel abstractions and routing -- adapters for each transport |
| `that-plugins` | Runtime extension plane -- commands, activations, routines |
| `that-eval` | Deterministic scenario harness with LLM judge and structured reports |
| `that-cli` | Operator entrypoint -- the `that` binary |

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

Minimum requirement: one LLM provider key. See `.env.example` for all available options.

### Run

```bash
that run "Set up a small project and verify it compiles"
that chat
that listen
```

`run` executes a single task. `chat` opens an interactive session. `listen` enters autonomous mode with a heartbeat loop.

## Sandbox

Sandbox mode gives the agent an isolated container where destructive operations are allowed by policy.

```bash
./sandbox/build.sh
that run "Create a demo app and run its tests"
```

Docker is the default backend. Set `THAT_SANDBOX_MODE=kubernetes` for cluster-based isolation. Pass `--no-sandbox` to run directly on the host.

## Eval

The eval harness runs reproducible behavior checks defined as TOML scenarios.

```bash
that-eval list-scenarios
that-eval run <scenario>
that-eval run-all
that-eval report <run-id>
```

Scenarios live in `evals/scenarios/`. Reports are stored locally under the agent's eval directory.

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

## Deployment

### VPS one-liner (k3s)

```bash
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

Installs k3s on a fresh Linux VPS, prompts for agent name, description, and API credentials, then deploys the agent into the cluster. The description you provide is interpreted by the LLM at first boot to generate the agent's `Soul.md` and `Identity.md`.

### Existing Kubernetes cluster

```bash
cp -r deploy/k8s/overlays/example deploy/k8s/overlays/my-agent
# edit namespace, configmap, secret, and image reference
kubectl apply -k deploy/k8s/overlays/my-agent
```

See [OPERATORS.md](./OPERATORS.md) for full configuration reference, environment variables, overlay examples, and observability setup.

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](./CONTRIBUTING.md) for guidelines, first-contribution paths, and development workflow.

## License

MIT. See [LICENSE](./LICENSE).
