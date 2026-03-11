---
name: that-plugins
description: Build practical agent plugins (commands, skills, routines, activations, emojis), install them, and verify hot-reload behavior.
metadata:
  bootstrap: true
  always: false
  version: 1.2.3
---

# that-plugins

Use this skill when the user asks for plugin work: create, update, install, enable, disable, or debug plugins.

## Scope Rules (Separation of Concerns)

A plugin is always agent-scoped and isolated to:
- `~/.that-agent/agents/<agent-name>/plugins/<plugin-id>/`

Do not mix concerns:
- Plugin assets stay in that plugin directory.
- Agent-level skills stay in `~/.that-agent/agents/<agent-name>/skills/`.
- Do not write across plugin directories unless user explicitly asks.

## Plugin Directory Contract

`that plugin create` now scaffolds:
- `plugin.toml`
- `skills/`
- `scripts/` (includes `run.sh`)
- `deploy/` (includes `docker-compose.yml` and `k8s/kustomization.yaml`)
- `state/`
- `artifacts/`
- `Dockerfile`

Runtime-managed state files under plugins root:
- `.plugin-state.toml`
- `.plugin-runtime.toml`

## Scaffold Rule

When creating a new plugin, prefer the built-in scaffold command:

```bash
that --agent <agent-name> plugin create <plugin-id>
```

Do not hand-create the top-level plugin directory structure with ad-hoc shell shortcuts when the
CLI scaffold is available.

If you must create missing subdirectories manually, create each path explicitly:

```bash
mkdir -p skills
mkdir -p scripts
mkdir -p deploy/k8s
mkdir -p state
mkdir -p artifacts
```

Do not use shell brace expansion for plugin scaffolding. A malformed command can create literal
directories containing `{`, `}`, or `,`, which corrupts the plugin layout.

After scaffolding, verify that the plugin root contains only the expected directories and that no
malformed directory names were created.

## Runtime Backends

Supported backend modes:
- `docker` (default runtime path; can deploy/run via Docker socket on local/VPS hosts)
- `kubernetes` (build/push/deploy orchestration path in clusters)
- Agent runtime should be self-aware of active backend from preamble/system-reminder metadata.

Backend execution defaults:
- Docker mode + socket enabled: agent can spawn sibling containers/compose stacks via host Docker socket.
- Docker mode + socket disabled: agent can still run inside sandbox container but should not claim host Docker orchestration.
- Kubernetes mode: agent should read `image_build_backend` from `<system-reminder>` and follow it strictly.
  - `buildkit`: build/push with `buildctl` via `${BUILDKIT_HOST}`.
  - `docker`: build/push with Docker only if daemon is actually available.
  - `none`: require prebuilt image or run a Kubernetes-native build job.

When user asks to "run/deploy service":
- Prefer Docker/Kubernetes-native deploy flow first (based on active backend).
- If user says "run it in Docker", execute Docker commands and return container name + published port(s).
- In Kubernetes mode with `image_build_backend=buildkit`, do not ask for Docker socket access and do not claim Docker is required.
- Do not default to `python3 -m http.server` unless user explicitly wants a temporary static preview.

Plugin manifest can declare runtime/deploy blocks:

```toml
[runtime]
kind = "docker"               # docker | kubernetes
context = "."
dockerfile = "Dockerfile"
command = ["/bin/sh", "scripts/run.sh"]

[deploy]
target = "docker"             # docker | kubernetes
kind = "service"              # docker: service|job, kubernetes: deployment|statefulset|job
compose_file = "deploy/docker-compose.yml"
kustomize_dir = "deploy/k8s"
replicas = 1
```

## Minimal plugin.toml Template

```toml
id = "example_plugin"
version = "0.1.0"
name = "Example Plugin"
description = "Adds plugin-powered workflows"
enabled_by_default = true
capabilities = ["skills", "commands", "routines", "activations", "emojis"]
envvars = [] # Optional required env vars, e.g. ["${SERVICE_API_URL}", "${SERVICE_API_TOKEN}"]
skills_dir = "skills"

[runtime]
kind = "docker"
context = "."
dockerfile = "Dockerfile"
command = ["/bin/sh", "scripts/run.sh"]

[deploy]
target = "docker"
kind = "service"
compose_file = "deploy/docker-compose.yml"
kustomize_dir = "deploy/k8s"
replicas = 1

[[commands]]
command = "example_cmd"
description = "Execute plugin example flow"
task_template = "Run plugin flow with context: {{args}}"

[[routines]]
name = "daily_review"
schedule = "daily"
priority = "normal"
task_template = "Run daily plugin review"
```

## Skill Authoring Paths

Plugin-scoped skill path:
- `~/.that-agent/agents/<agent-name>/plugins/<plugin-id>/skills/<skill-name>/SKILL.md`

Agent-scoped skill path:
- `~/.that-agent/agents/<agent-name>/skills/<skill-name>/SKILL.md`

Use explicit CLI scope:
- Plugin-scoped skill: `that --agent <agent-name> skill create <skill-name> --plugin <plugin-id>`
- Agent-scoped skill: `that --agent <agent-name> skill create <skill-name>`

## Runtime Behavior

- `commands` become slash commands in channel integrations.
- `skills` are discoverable by `read_skill` and the skill catalog.
- `routines` run through heartbeat scheduling.
  Routine schedule supports `once|minutely|hourly|daily|weekly` and cron (`cron: */5 * * * *`).
- `activations` enqueue heartbeat tasks when inbound events match.
- `emojis` are exposed in plugin context for channel formatting.
- `envvars` declares required environment variables for the plugin runtime.
- `read_plugin` can inspect plugin manifest/state before changes.
- `validate_plugin` can verify manifest integrity and missing env vars.
- Deploy is not Kubernetes-only: in Docker mode, deploy can mean creating/running Docker containers or compose stacks via socket access.

## Deploy Lifecycle

Plugin deploy status is reconciled automatically. The preamble shows the current status of each deployed plugin (running, stopped, pending, degraded, failed). Use this information to make informed decisions before modifying or removing plugins.

### Install / Uninstall Flags

- `plugin_install` deploys by default when the manifest declares a deploy target. Use `skip_deploy: true` to register a plugin without deploying — useful when you want to prepare the manifest/skills first and deploy later.
- `plugin_uninstall` tears down the running deployment by default before removing from the registry. Use `undeploy: false` to deregister while keeping the workload running — useful for registry cleanup without disrupting live services.

### Pre-flight Checks

- Before uninstalling, check `plugin_status` to see if a workload is live.
- After uninstall with `undeploy: true`, verify no orphaned resources remain in the target namespace.
- When re-installing after a deregister-only uninstall (`undeploy: false`), use `skip_deploy: true` to avoid deploying over a still-running workload.

### Keeping Plugin Skills Current

Plugin skills describe how to interact with the deployed software. When the underlying service changes (new endpoints, updated CLI flags, different configuration), the plugin's skills must be updated to reflect the current state. After any significant change to the plugin's software or deployment:
1. Review the plugin's skills under its skills directory.
2. Update skill content to match the current software capabilities, endpoints, and usage patterns.
3. Deploy status is reflected in the preamble automatically — skills should focus on usage guidance, not deployment state.

## Kubernetes Hygiene (Required)

When deploying plugin workloads to Kubernetes:
- Reuse the same Deployment/Service names for updates; do not generate new random resource names per attempt.
- Keep `replicas = 1` by default for interactive plugin services (browser/CLI helpers) unless the user asks to scale out.
- Always label resources consistently (for example `app.kubernetes.io/name=<plugin-id>`), then use those labels for rollout checks and cleanup.
- If rollout fails, debug first (`kubectl describe`, `kubectl logs`, `kubectl get events`) and only re-apply after fixing the cause.
- Remove stale failed/evicted pods for the plugin label after successful recovery so the namespace stays clean.
- **Always expose port 80** as the Service's external port (`port: 80`, `targetPort: <container-port>`). This gives every plugin service a clean URL with no port suffix — required for Tailscale-exposed services, HTTP callbacks, and any tooling that assumes standard HTTP. The container may bind any internal port; the Service always fronts it on 80.

## Enable / Disable

Preferred CLI actions:
- `that --agent <agent-name> plugin enable <plugin-id>`
- `that --agent <agent-name> plugin disable <plugin-id>`

Disable semantics:
- Disable means runtime-off, not file deletion.
- After disable, verify both:
  - plugin is marked disabled in state/list output
  - plugin directory/files still exist on disk
- In status/output text, state explicitly: "disabled, files retained".

## Validation Checklist

After changes, verify all:
1. `plugin.toml` parses correctly.
2. Command names are lowercase and safe.
3. Skill files are created in the intended scope (agent vs plugin).
4. Plugin appears in `plugin list`.
5. Plugin command appears in `/help` when listening.
6. Routine/activation behavior is observable in heartbeat runs.
7. Plugin files remain inside plugin-owned directories.
8. Disable behavior is correct: plugin disabled in state and files retained.
9. Durable plugin facts are stored via `mem_add` (plugin id, purpose, commands/skills/routines touched, deploy/runtime notes).
10. For Kubernetes deploys: no stale failed/evicted pods remain for this plugin label after rollout.
11. Plugin root contains no malformed scaffold directories (for example names containing `{`, `}`, or `,`).

## Practical Constraints

- Keep commands specific and task templates deterministic.
- Prefer one plugin per domain (e.g. `crypto_trader`, `customer_ops`).
- Avoid giant multi-purpose plugins.
- If editing existing plugins, preserve backward-compatible command names.

## Done Criteria

A plugin task is done only when:
- files are written in the correct scope,
- plugin is enabled (or explicitly disabled by request),
- runtime reload/behavior was verified,
- and the final plugin outcome was persisted with `mem_add`.

For create/update flows, persist memory at the end:
- Call `mem_add` with a concise summary of what changed and what to reuse later.
- Include tags like `plugin`, `<plugin-id>`, and domain tags (for example `deploy`, `skills`, `routine`).
- Prefer session-scoped memory when this is tied to a specific active session.
