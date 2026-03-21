<p align="center">
  <img src="./assets/logo.png" alt="that-agent" width="180" />
</p>

<h1 align="center">that-agent</h1>

<p align="center">
  <strong>The autonomous agent that writes and deploys its own tools.</strong><br>
  <strong>Kubernetes-native. Rust. Runs anywhere — laptop to production.</strong>
</p>

<p align="center">
  <a href="./LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT" /></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Built_with-Rust-orange.svg" alt="Rust" /></a>
  <a href="https://github.com/that-labs/that-agent/actions/workflows/ci.yml"><img src="https://github.com/that-labs/that-agent/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/that-labs/that-agent/pkgs/container/that-agent"><img src="https://img.shields.io/badge/Docker-ghcr.io-blue?logo=docker" alt="Docker" /></a>

  <a href="https://discord.gg/Xqu6kRXW"><img src="https://img.shields.io/discord/1234567890?color=7289da&label=Discord&logo=discord&logoColor=white" alt="Discord" /></a>
</p>

Most agent frameworks configure tools for the agent. `that-agent` gives the agent a Kubernetes namespace and a compiler — it authors, ships, and runs its own services at runtime without operator intervention.

```
14 MB binary · 6 ms startup · 13 MB idle RSS · Kubernetes-native · self-authoring plugins · cluster-aware fleet
```

## The Idea

Most frameworks hand an agent a fixed set of tools. `that-agent` gives the agent a Kubernetes namespace and a compiler.

When the agent needs a new capability — an integration, a scheduled routine, a custom command — it authors a plugin, ships it to the cluster, and runs it. The operator writes nothing. The agent evolves through its own work. That is the loop this project exists to close.

Kubernetes is the execution layer everywhere. On macOS, [k3d](https://k3d.io/) runs k3s inside Docker Desktop in seconds. On a VPS, [k3s](https://k3s.io/) runs directly on containerd — no Docker needed. In production, any conformant cluster works. The agent gets full access to the control plane, operators, and cluster API regardless of where it runs. Same binary, same manifests, same capabilities.

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

The agent keeps persistent state across runs, but workspace sharing depends on how a child is spawned. Ephemeral workers can be given an explicit shared repo/workspace flow; persistent agents keep their own state. Coordination between agents should happen through the task scratchpad, which gives every participant on that task a shared, persistent communication surface without assuming a single shared volume layout. Each task scratchpad has a stable header for goal/workspace/participant policy and a live activity tail for steering, blockers, reviews, and git-visible progress.

## 5-Minute Quickstart

The fastest path to a running agent — a single-node Kubernetes cluster with everything pre-configured:

```bash
# Linux VPS — installs k3s (no Docker needed)
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash

# macOS — installs k3d (runs k3s inside Docker Desktop)
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

The installer detects your platform automatically. On Linux it uses k3s (single binary, containerd). On macOS it uses k3d (k3s-in-Docker). Override with `--k3s` or `--k3d` on any platform. See [Get Started](#get-started) for details and flags.

**Already have a cluster?**

```bash
helm install that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n my-agent --create-namespace \
  --set agent.name=my-agent \
  --set secrets.anthropicApiKey=sk-ant-... \
  --set accessLevel=namespace-admin
```

**Just want Docker Compose?**

```bash
cp .env.example .env    # fill in API keys
docker compose up
```

**Just want to try it locally?**

```bash
cargo install --git https://github.com/that-labs/that-agent that-agent --bin that
echo 'ANTHROPIC_API_KEY=sk-ant-...' > .env
that run "Create a hello-world Python script and verify it runs"
that chat       # interactive session
that listen     # autonomous mode with heartbeat
```

> **Tip:** `cargo run` and `docker run` work for local development, but the agent's full power — deploying services, managing workloads, extending its own gateway — requires a Kubernetes cluster.

## Development Tiers

The agent works at three levels. Each tier adds capabilities:

| Tier | What you need | What works |
|------|---------------|------------|
| **Host** (`cargo run`) | Rust toolchain | Agent loop, tools, memory, sessions, TUI, chat, run — everything except sandboxed execution |
| **Docker** (`docker run`) | Docker | Same + sandboxed file and shell execution, fs isolation |
| **Kubernetes** (`k3d` / `k3s` / any cluster) | Docker + k3d (macOS), k3s (Linux), or existing cluster | Full power — sandbox pods, plugins, sub-agents, in-cluster registry, BuildKit, gateway extension, eval harness with sandbox scenarios |

**Upgrade path:** The binary is the same everywhere — `THAT_SANDBOX_MODE` flips between host, Docker, and Kubernetes. Moving from local dev to a cluster means setting one environment variable and running `helm install`.

**Evals in a cluster:** Scenarios with `sandbox = true` spin up sandbox pods, run assertions inside them, and tear them down — same as production. On k3d, this works identically to a remote cluster. Point `KUBECONFIG` at your k3d cluster and run `that eval`.

## Get Started

### One command — any platform

```bash
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

The installer detects your platform and sets up a production-ready single-node cluster:

| Platform | Cluster tool | Runtime | Prerequisites |
|----------|-------------|---------|--------------|
| **Linux VPS / server** | k3s | containerd (built-in) | None — single binary, no Docker |
| **macOS** | k3d | Docker Desktop | Docker Desktop running |
| **Linux desktop** | k3s (default) or k3d (`--k3d`) | containerd or Docker | None / Docker |

Override the auto-detection with `--k3s` or `--k3d` on any platform. All infrastructure components are enabled by default and can be skipped with flags.

#### What it installs

| Step | Component | What it does | Skip flag |
|------|-----------|-------------|-----------|
| 1 | [k3s](https://k3s.io/) or [k3d](https://k3d.io/) | Lightweight Kubernetes (platform-detected) | `--k3s` / `--k3d` to override |
| 2 | [Cilium](https://cilium.io/) | eBPF-based CNI with L3/L4/L7 network policies and [Hubble](https://docs.cilium.io/en/stable/observability/hubble/) flow observability | `--no-cilium` |
| 3 | [Tailscale Operator](https://tailscale.com/kb/1236/kubernetes-operator) | Expose cluster services to your Tailnet — no public IPs, no port forwarding | `--no-tailscale` |
| 4 | [K9s](https://k9scli.io/) | Terminal-based Kubernetes UI for cluster inspection | `--no-k9s` |
| — | Sub-agent RBAC | ClusterRole for namespace creation and cross-namespace orchestration | `--no-subagents` |
| — | Cluster admin | Bind to `cluster-admin` instead of scoped ClusterRole (opt-in) | `--cluster-admin` |
| 5 | In-cluster registry | Private container registry (NodePort) for agent-built images | always |
| 6–9 | Agent config + deploy | Interactive prompts → Helm values → `helm install` | — |

#### Interactive prompts

The installer asks for:

- **Agent name** — lowercase identifier for the deployment
- **Agent description** — free-text prompt used by the LLM at first boot to generate the agent's identity (`Soul.md`, `Identity.md`)
- **LLM API key** — Anthropic, OpenAI, or OpenRouter (auto-detected from key prefix)
- **Model override** — optional, defaults to the provider's recommended model
- **Telegram bot token + chat ID** — optional, for Telegram channel integration
- **Tailscale OAuth credentials** — client ID + secret for the operator (prompted only when Tailscale is enabled)
- **Tailnet name** — optional (e.g. `myteam` from `myteam.ts.net`), so the agent can construct mesh URLs directly

#### Infrastructure awareness

The installer writes infrastructure metadata into the agent's ConfigMap. At runtime, the agent's system-reminder reflects exactly what's installed:

- **Cilium as CNI** — the agent knows it has L7 network policies available and loads the `cluster-management` skill with Cilium-specific references for zero-trust enforcement and Hubble flow visibility
- **Tailscale Operator + tailnet name** — the agent can construct mesh URLs (`https://<service>.<tailnet>.ts.net`) without wasting turns on discovery
- **K9s** — the agent knows an interactive cluster UI is available on the host

#### Network flags

When Cilium is enabled, k3s is installed with `--flannel-backend=none --disable-network-policy` so Cilium fully owns the networking stack. The agent's `cluster-management` skill teaches it to establish default-deny policies per namespace and layer L7 allow rules on top.

#### Running the install with flags

```bash
# Skip Cilium and K9s, keep Tailscale
bash install.sh --no-cilium --no-k9s

# Force k3d on Linux (uses Docker instead of k3s)
bash install.sh --k3d

# Full cluster-admin for a single-user VPS
bash install.sh --cluster-admin
```

#### Environment variables

All interactive prompts can be pre-set via environment variables, enabling fully non-interactive installs. Set them before piping the script:

```bash
# Fully non-interactive one-liner with cluster-admin
ANTHROPIC_API_KEY=sk-ant-... \
CLUSTER_ADMIN=true \
TS_OAUTH_CLIENT_ID=... \
TS_OAUTH_CLIENT_SECRET=... \
TS_TAILNET_NAME=myteam \
  curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

| Variable | Purpose | Default |
|----------|---------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key (auto-detected) | — |
| `OPENAI_API_KEY` | OpenAI API key (auto-detected) | — |
| `OPENROUTER_API_KEY` | OpenRouter API key (auto-detected) | — |
| `CLAUDE_CODE_OAUTH_TOKEN` | Claude Code OAuth token (auto-detected) | — |
| `TS_OAUTH_CLIENT_ID` | Tailscale OAuth client ID | prompted |
| `TS_OAUTH_CLIENT_SECRET` | Tailscale OAuth client secret | prompted |
| `TS_TAILNET_NAME` | Tailnet name (e.g. `myteam` from `myteam.ts.net`) | prompted |
| `FORCE_K3S` | Force k3s even on macOS | `false` |
| `FORCE_K3D` | Force k3d even on Linux | `false` |
| `INSTALL_CILIUM` | Install Cilium CNI | `true` |
| `INSTALL_TAILSCALE_OPERATOR` | Install Tailscale Operator | `true` |
| `INSTALL_K9S` | Install K9s | `true` |
| `ENABLE_SUBAGENTS` | ClusterRole for cross-namespace sub-agents | `true` |
| `CLUSTER_ADMIN` | Bind to built-in `cluster-admin` ClusterRole | `false` |
| `THAT_AGENT_NAME` | Agent name (non-interactive mode) | prompted |
| `THAT_AGENT_DESCRIPTION` | Agent description (non-interactive mode) | prompted |

**CI / non-interactive mode:**

```bash
ANTHROPIC_API_KEY=sk-ant-... bash install.sh --ci --k3d --no-cilium --no-tailscale --no-k9s
```

The `--ci` flag (or `CI=true` env var) skips all interactive prompts and reads configuration from environment variables. Used by the project's own CI to validate the install flow end-to-end.

#### Post-install

```bash
# Follow agent logs
kubectl -n that-<agent-name> logs -f deploy/that-agent

# Shell into the agent pod and start a TUI chat
kubectl -n that-<agent-name> exec -it deploy/that-agent -- that run chat --agent <agent-name>

# Interactive cluster inspection
k9s

# Delete the k3d cluster (macOS / --k3d installs only)
k3d cluster delete that-agent
```

#### RBAC — what the agent can access

The agent gets two levels of RBAC: a **namespace-scoped Role** for full control within its own namespace, and a **ClusterRole** for bootstrapping sub-agent namespaces.

**Namespace Role** (`that-agent-runtime`) — full access within `that-<agent-name>`:

| API Group | Resources | Verbs | Why |
|-----------|-----------|-------|-----|
| `""` (core) | pods, pods/log, services, endpoints, configmaps, secrets, serviceaccounts, persistentvolumeclaims, events | all | Deploy and manage plugin workloads, read logs, manage config |
| `apps` | deployments, statefulsets, daemonsets, replicasets | all | Create/update/rollback plugin deployments |
| `batch` | jobs, cronjobs | all | Run one-off build jobs and scheduled tasks |
| `networking.k8s.io` | ingresses, networkpolicies | all | Manage service exposure and zero-trust network policies |
| `autoscaling` | horizontalpodautoscalers | all | Scale plugin workloads |
| `policy` | poddisruptionbudgets | all | Manage disruption budgets for resilient deployments |
| `rbac.authorization.k8s.io` | roles, rolebindings | all | Create scoped RBAC for sub-agent ServiceAccounts |
| `*` (wildcard) | `*` | all | Access namespaced custom resources managed by plugins (e.g. Tailscale proxies, CRDs from operator charts) |

**ClusterRole** (`that-agent-cluster`) — cross-namespace operations for sub-agent orchestration:

| API Group | Resources | Verbs | Why |
|-----------|-----------|-------|-----|
| `""` (core) | namespaces | all | Create/delete namespaces for sub-agents |
| `rbac.authorization.k8s.io` | roles, rolebindings | all | Bootstrap RBAC in new sub-agent namespaces so the parent SA gains access |
| `""` (core) | pods, pods/log, services, events | get, list, watch | Monitor sub-agent workloads across namespaces |
| `apps` | deployments, statefulsets | get, list, watch | Watch sub-agent deployment status across namespaces |

**How sub-agent namespace bootstrap works:**

1. Parent agent creates a new namespace for the sub-agent
2. Parent creates a **Role** in that namespace (mirroring `that-agent-runtime` permissions)
3. Parent creates a **RoleBinding** in that namespace, binding its own ServiceAccount to that Role
4. Parent can now deploy the sub-agent and manage resources in that namespace
5. The sub-agent inherits the same pattern if it needs to spawn its own children

This is the least-privilege approach — the ClusterRole only grants the ability to create namespaces and bootstrap RBAC. Actual resource management in each namespace requires the explicit RoleBinding step.

**What this means for cluster admins:**

- The agent **can** create new namespaces and grant itself access to them via RBAC bootstrap
- The agent **cannot** access existing namespaces it hasn't bootstrapped into (no pre-existing RoleBinding = no access)
- The agent **cannot** create or modify ClusterRoles or ClusterRoleBindings (it only has the pre-installed ones)
- The agent **cannot** access Nodes, PersistentVolumes, or other cluster-scoped resources beyond namespaces
- The ClusterRole grants **read-only** cross-namespace access to pods, services, and deployments — not write
- The pod runs as **non-root** (UID 1000) with no host path mounts

**Hardening options:**

| Action | Effect |
|--------|--------|
| Remove the ClusterRole + ClusterRoleBinding | Agent cannot spawn sub-agents in separate namespaces — all work stays in its own namespace |
| Remove the `*/*` wildcard from the namespace Role | Restrict to only the explicitly listed API groups — tighter but may break CRD-based operators |
| Remove `secrets` from core resources | Agent loses ability to manage secrets for its plugins (must be pre-created by operator) |
| Remove namespace `create`/`delete` from ClusterRole | Agent can only use pre-created namespaces for sub-agents (operator provisions them) |
| Add label selectors to namespace management | Restrict namespace operations to only namespaces with a specific label (e.g. `that-agent/managed: "true"`) |

**`--cluster-admin` mode:**

For single-user VPS setups where the agent is the sole operator of the cluster, pass `--cluster-admin` to the installer. This binds the agent's ServiceAccount to the built-in `cluster-admin` ClusterRole — full unrestricted access to all resources in all namespaces. Use this when you want the agent to manage the entire cluster (install operators, configure cluster-wide resources, manage all namespaces) without RBAC friction. **Not recommended for shared or multi-tenant clusters.**

Manifests: [`deploy/helm/that-agent/templates/rbac/`](./deploy/helm/that-agent/templates/rbac/) (namespace and cluster roles, gated by `accessLevel` value).

### Existing Kubernetes cluster

```bash
helm install that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n my-agent --create-namespace \
  --set agent.name=my-agent \
  --set secrets.anthropicApiKey=sk-ant-... \
  --set accessLevel=namespace-admin
```

The chart deploys the agent, git server, BuildKit, and cache proxy. All infrastructure services are configurable via `values.yaml`. See [OPERATORS.md](./OPERATORS.md) for full configuration, environment variables, and observability setup.

Three access levels: `cluster-admin` (full RBAC + child spawning across namespaces), `namespace-admin` (own namespace only), `readonly` (observe only).

### Docker Compose (local development)

The fastest way to run the full stack locally without a Kubernetes cluster:

```bash
cp .env.example .env    # fill in at least one LLM API key
docker compose up
```

This starts the agent, git server, and cache proxy. The agent gateway is available at `http://localhost:8080`. Add BuildKit for image builds:

```bash
docker compose --profile build up
```

> The agent runs and works in Docker, but without a cluster it can't deploy services, manage workloads, or spawn sub-agents. For the full experience, use k3s or any conformant cluster.

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

### From source (via cargo)

```bash
# Install directly from GitHub
cargo install --git https://github.com/that-labs/that-agent that-agent --bin that

# Or clone and build locally
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
- **Sandboxed execution** — Kubernetes-native isolation with Docker fallback for local dev; destructive ops allowed inside the boundary, denied on host by default
- **Multi-channel routing** — Telegram, HTTP, and TUI through a unified abstraction; new channels register at runtime without restart
- **Heartbeat system** — autonomous listen mode with configurable wakeup cycles and scheduled routines

## Security

| # | Control | Status | How |
|---|---------|--------|-----|
| 1 | Tool policy gates | Done | Every call passes Allow / Prompt / Deny; configurable per tool |
| 2 | Sandbox isolation | Done | Kubernetes-native with Docker fallback; destructive tools deny on host by default |
| 3 | Workspace scoping | Done | File tools restricted to agent workspace unless explicitly widened |
| 4 | Secrets via env | Done | No secrets in manifests; injected via Kubernetes secrets or `.env` |
| 5 | Policy hierarchy | Done | Sub-agents cannot elevate beyond the main agent's policy ceiling |
| 6 | Eval sandbox gating | Done | Scenarios requiring destructive ops must explicitly opt into sandbox mode |
| 7 | Audit log | Done | Every tool call recorded with outcome; structured and queryable |

### Production Deployment Warning

> **This is an autonomous agent that can write code, execute commands, and deploy services.** The default sandbox settings are designed for development and single-user experimentation — not multi-tenant production.

Before deploying to production, apply these hardening measures:

| Measure | Why |
|---|---|
| gVisor / Kata Containers runtime class | Stronger workload isolation than default runc |
| Network policies restricting egress | Prevent the agent from reaching unintended services |
| Read-only root filesystem | Limit persistence of unintended modifications |
| Strict seccomp profile | Reduce available syscall surface |
| CPU and memory resource limits | Prevent runaway workloads from starving the node |
| Dedicated namespace with tight RBAC | Contain blast radius of any misconfiguration |
| No host path mounts | Prevent container escape to host filesystem |

## Architecture

Single consolidated crate (`that-agent`) plus standalone `that-git-server`.

| Module | Role |
|---|---|
| `orchestration/` | Agent loop, preamble, sessions, all execution paths |
| `tools/` | Capability plane — fs, code, memory, search, exec, human — with policy gates |
| `sandbox/` | Execution boundary — Kubernetes-native with Docker fallback |
| `channels/` | Channel router and adapters (Telegram, HTTP gateway, TUI) |
| `plugins/` | Runtime extension plane — commands, activations, routines |
| `eval/` | Behavioral scenario harness with LLM judge and structured reports |
| `commands/` | CLI command handlers — the `that` binary |

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
./build.sh
that run "Create a demo app and run its tests"
```

Kubernetes is the primary sandbox backend (`THAT_SANDBOX_MODE=kubernetes`). Docker is available for local development. `--no-sandbox` to run directly on host.

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
Tests:        520+
Binary:       14 MB (release, stripped)
Startup:      ~6 ms (Linux), ~15 ms (macOS)
Idle RSS:     ~13 MB
Providers:    Anthropic, OpenAI, OpenRouter
Channels:     Telegram, HTTP gateway, TUI
Runtime:      Kubernetes (k3d on macOS, k3s on Linux, any conformant cluster in production)
```

## Project Layout

```text
crates/
  that-agent/        # consolidated crate (CLI + orchestration + tools + channels + plugins + eval)
  that-git-server/   # standalone Git Smart HTTP server
evals/scenarios/     # TOML scenario definitions
deploy/helm/         # Helm chart (single chart for root, child, and ephemeral agents)
```

## Community

Come hang out with us! Join the [that-agent Discord](https://discord.gg/Xqu6kRXW) — whether you're building with `that-agent`, hacking on it, or just curious about autonomous agents, you're welcome here. Ask questions, share what you're building, report bugs, or just say hi. We'd love to have you.

## Contributing

Contributions welcome. See [CONTRIBUTING.md](./CONTRIBUTING.md) for guidelines, first-contribution paths, and development workflow.

## License

MIT. See [LICENSE](./LICENSE).

---

**that-agent** — The agent that builds itself. k3d on a laptop, k3s on a VPS, production on any cluster.
