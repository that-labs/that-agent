# Operator Guide

Human operator guide for deploying, configuring, and running agents.

---

## Quick VPS Install

For a fresh Linux VPS with no Kubernetes installed, the installer script handles everything end to end: installs k3s, collects agent configuration interactively, generates a Kubernetes overlay, and deploys the agent.

```bash
# Download and run (review the script first)
curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
```

Or with options:

```bash
bash scripts/install.sh \
  --image your-registry.example.com/that-agent:latest \
  --namespace my-namespace
```

Flags:
- `--image IMAGE:TAG` — container image to deploy (default: `ghcr.io/that-labs/that-agent:latest`)
- `--namespace NS` — Kubernetes namespace (default: `that-<agent-name>`)
- `--no-k3s` — skip k3s installation if a cluster is already reachable via `kubectl`

The script prompts for:
- Agent name and description (the description seeds `THAT_AGENT_BOOTSTRAP_PROMPT`, which the agent uses to generate its identity on first boot)
- LLM provider and API key
- Optional Telegram channel credentials

The generated overlay and secret are written to `~/.that-agent-install/<agent-name>/`. Keep `secret.yaml` safe — it contains your API keys.

---

## Prerequisites

- **Rust stable toolchain** — install via [rustup](https://rustup.rs/)
- **At least one LLM provider API key** (see Configuration Reference below)
- **Docker** — required for sandbox mode (local)
- **Kubernetes** — for cluster deployment: [k3s](https://k3s.io/) (single-node VPS), [k3d](https://k3d.io/) (local dev), or any standard distribution
- **kubectl + kustomize** — required for Kubernetes deployment

---

## Configuration Reference

All configuration is driven by environment variables. Copy `.env.example` to `.env` and fill in the values relevant to your deployment.

### Provider Keys

At least one LLM provider key is required.

| Variable | Description |
|---|---|
| `ANTHROPIC_API_KEY` | Anthropic API key |
| `OPENAI_API_KEY` | OpenAI API key |
| `OPENROUTER_API_KEY` | OpenRouter API key (provides access to models from multiple providers via a single endpoint) |

### Channel Integration

All channel variables are optional. Set the pair for each channel you want to activate.

| Variable | Description |
|---|---|
| `TELEGRAM_BOT_TOKEN` | Telegram Bot API token |
| `TELEGRAM_CHAT_ID` | Target Telegram chat ID |
| `THAT_HTTP_PORT` | HTTP adapter listen port (default: `8080`) |
| `THAT_HTTP_BEARER_TOKEN` | Bearer token for HTTP adapter authentication |

### Observability

| Variable | Description |
|---|---|
| `PHOENIX_TRACING` | Enable Phoenix tracing (`true` / `false`) |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP endpoint for trace export (e.g., `http://localhost:6006`) |

### Agent Behavior

| Variable | Description |
|---|---|
| `THAT_AGENT_MODEL` | Override the default model used by the agent |
| `THAT_AGENT_PROVIDER` | Override the LLM provider (`anthropic`, `openai`, `openrouter`) |
| `THAT_AGENT_MAX_TURNS` | Maximum agent loop turns before automatic stop |
| `THAT_AGENT_PROFILE` | Agent profile name |
| `THAT_OPENAI_WEBSOCKET` | Use WebSocket streaming for OpenAI (`true` by default; set `false` for legacy HTTP streaming) |

---

## Agent Identity

Each agent's identity is defined by a set of workspace files stored in its home directory. Operators can seed these files before the first run or let the agent bootstrap itself on first boot. See [ARCHITECTURE.md § 4](./ARCHITECTURE.md#4-continuity-model) for the workspace file model, bootstrap flow, and template variable reference.

---

## Sandbox Modes

Sandbox mode isolates agent filesystem and shell operations inside a container. Two backends are supported.

### Docker (Default)

Build the sandbox image from the workspace root:

```bash
./sandbox/build.sh
```

The image is a multi-stage build: a Rust builder stage followed by a `python:3.12-slim` runtime stage.

Key behaviors in Docker sandbox mode:

- **All filesystem tools** (`fs_ls`, `fs_cat`, `fs_write`, `fs_mkdir`, `fs_rm`) **and** `shell_exec` route through `docker exec` into the container.
- Relative paths are anchored to `/workspace` inside the container.
- The workspace directory is bind-mounted from the host, so files are accessible from both sides.
- Destructive tools (`fs_delete`, `shell_exec`, `fs_write`, `code_edit`, `git_commit`, `git_push`) are denied on the host and only permitted inside the sandbox.

### Kubernetes

Set the following environment variables to use the Kubernetes backend:

| Variable | Description |
|---|---|
| `THAT_SANDBOX_MODE` | Set to `kubernetes` |
| `THAT_SANDBOX_K8S_NAMESPACE` | Namespace where sandbox pods are created |
| `THAT_TRUSTED_LOCAL_SANDBOX` | Set to `1` to enable elevated permissions inside the pod |

In Kubernetes mode:

- A pod is created in the configured namespace for each sandbox session.
- Tool routing uses `kubectl exec` instead of `docker exec`.
- RBAC scope is auto-detected at startup and injected into system metadata.

---

## Channel Integration

Channels allow the agent to communicate through external messaging platforms. Each channel requires its corresponding environment variables to be set.

### Telegram

Set `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID`.

- Messages are formatted using MarkdownV2 via the `telegram-format` skill.
- **Auto-bootstrap**: if both environment variables are set and no channel adapters exist yet, the Telegram adapter is injected automatically at startup.
- **Deployment note**: use the `Recreate` strategy (not rolling updates) to avoid `getUpdates` polling conflicts. Only one poller may run per bot token at a time.

### HTTP

Set `THAT_HTTP_PORT` (default `8080`) and optionally `THAT_HTTP_BEARER_TOKEN` to require authentication.

- The HTTP adapter exposes a REST + SSE interface for programmatic agent access.
- POST a message to `/chat` with a bearer token in the `Authorization` header.
- The agent streams response tokens back via Server-Sent Events on the same connection.
- Suitable for air-gapped deployments and integration with custom frontends or automation pipelines.

---

## Kubernetes Deployment

### Base Manifests

The base Kubernetes manifests live in `deploy/k8s/base/` and include:

- **Deployment** — `Recreate` strategy, rootless container, BuildKit sidecar
- **ServiceAccount + Role + RoleBinding** — namespace-scoped RBAC
- **PVC** — persistent volume claim for the agent home directory
- **ConfigMap** — runtime configuration
- **Secret** — API keys and tokens
- **Phoenix deployment + service** — tracing infrastructure
- **Entrypoint script** — mounted via ConfigMap

### Creating an Overlay

1. Copy the example overlay directory to create your own:
   ```bash
   cp -r deploy/k8s/overlays/example/ deploy/k8s/overlays/<your-overlay>
   ```
2. Edit the following files in your new overlay:
   - `namespace.yaml` — set your target namespace
   - `patch-configmap.yaml` — set agent name, registry, and other config
   - `kustomization.yaml` — set the container image reference
3. Create your secret from the template:
   ```bash
   cp deploy/k8s/base/secret.yaml.example deploy/k8s/base/secret.yaml
   ```
   Fill in the base64-encoded values for your API keys and tokens.

### Deploying

```bash
kubectl apply -k deploy/k8s/overlays/<your-overlay>
kubectl -n <your-namespace> rollout status deploy/that-agent
```

### Key ConfigMap Variables

| Variable | Description |
|---|---|
| `THAT_SANDBOX_MODE` | Sandbox backend (`docker` or `kubernetes`) |
| `THAT_TRUSTED_LOCAL_SANDBOX` | Enable elevated sandbox permissions (`1`) |
| `THAT_SANDBOX_K8S_NAMESPACE` | Namespace for sandbox pods |
| `THAT_SANDBOX_K8S_REGISTRY` | Container registry for sandbox images |
| `THAT_AGENT_NAME` | Agent identity name |
| `THAT_AGENT_BOOTSTRAP_PROMPT` | Prompt used during first-run bootstrap |
| `PHOENIX_TRACING` | Enable Phoenix tracing |

### BuildKit Sidecar

The deployment includes a rootless BuildKit daemon as a sidecar container for in-cluster image builds:

- Publishes the `buildctl` client binary into a shared volume accessible by the main container.
- Registry configuration is auto-generated from `THAT_SANDBOX_K8S_REGISTRY`.
- An optional DinD (Docker-in-Docker) fallback is available as a patch for environments where BuildKit is not suitable.

### RBAC

- RBAC is **namespace-scoped by default**.
- The agent's permission scope is auto-detected at startup and injected into system metadata, so skills and tools are aware of what operations are permitted.

---

## Eval Harness Operation

The eval harness runs scripted scenarios against the agent and produces scored reports.

### Scenario Format

Scenarios are defined as TOML files in `evals/scenarios/`. Each scenario contains a sequence of steps that exercise the agent's capabilities.

### Commands

| Command | Description |
|---|---|
| `that-eval list-scenarios` | List all available scenarios |
| `that-eval run <scenario>` | Run a single scenario |
| `that-eval run-all <dir>` | Run all scenarios in a directory |
| `that-eval report <run-id>` | Generate a report for a completed run |

### Reports

Reports are stored under `~/.that-agent/evals/<run-id>/`.

### Gate Policy

`EvalGatePolicy` controls pass/fail thresholds:

| Field | Description |
|---|---|
| `fail_on_step_error` | Fail the scenario if any step errors |
| `min_assertion_pass_pct` | Minimum percentage of assertions that must pass |
| `min_judge_score` | Minimum judge score to consider the scenario passed |

### Sandbox Requirement

Scenarios that perform destructive operations (file deletion, unrestricted shell execution) **must** set `sandbox = true` at the top of the TOML file. The runner uses this flag to elevate tool policies. Without it, the agent will be blocked mid-scenario.

---

## Troubleshooting

### 1. "policy denied: tool X is not allowed"

- Verify that the agent config is loaded in the active execution path. All three execution paths (streaming, TUI, eval) must initialize the config with the container reference.
- If running in sandbox mode, confirm the container was properly created and is running.
- Destructive tools are only allowed inside a sandbox. They are denied on the host by default.

### 2. Skill not appearing to the agent

- Check that the YAML frontmatter in the skill file has `name:` and `description:` at the **root level**. Fields nested under a `metadata:` key are not recognized; the skill will be silently skipped during discovery.
- Check eligibility filters (`os`, `binaries`, `envvars`) — the skill may be filtered out because a required binary or environment variable is missing.

### 3. Docker build fails with "file not found for module"

- Check that rsync excludes in the build script use a **leading `/`** to anchor patterns to the source root.
- Without the leading `/`, a pattern like `--exclude='sandbox'` will also match nested directories with the same name (e.g., a Rust module directory), silently stripping them from the build context.

### 4. Telegram polling conflicts

- Only **one poller** may run per bot token at a time. Use the `Recreate` deployment strategy in Kubernetes to ensure the old pod is fully terminated before the new one starts.
- Do not run a local listen-mode instance and a Kubernetes pod simultaneously with the same bot token.

### 5. Eval scenario silently denied

- Set `sandbox = true` in the scenario TOML for any scenario that needs destructive tools. Without this flag, the runner does not elevate policies and the agent's tool calls are silently denied.
