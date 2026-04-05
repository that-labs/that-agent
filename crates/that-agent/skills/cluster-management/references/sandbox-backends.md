# Sandbox Backend Guide

## Docker Backend

When running in Docker mode, your sandbox is a container with optional host Docker socket access.

### With Docker socket enabled
- You can orchestrate sibling containers and compose stacks from inside this sandbox.
- For "run/deploy this app" requests, prefer Docker-native flows (`docker build`, `docker run`, `docker compose`).
- If the user explicitly asks to run/deploy "in Docker", execute a Docker workflow and report container/port details.
- If `docker` CLI is missing in-container, install it (`sudo apt-get update && sudo apt-get install -y docker.io`).
- Do not default to `python3 -m http.server` for deployment requests; use it only for temporary static preview when explicitly acceptable.

### Without Docker socket
- You can still run processes in this sandbox container, but you cannot spawn sibling host containers.
- If the user needs host-level Docker orchestration, state the socket limitation clearly.

## Kubernetes Backend

When running in Kubernetes mode, your sandbox is a pod in a dedicated namespace.

### Image Delivery
- Check `<system-reminder>` for `k8s_registry_push` and `image_build_backend`.
- If `k8s_registry_push` is present, push images there. BuildKit sidecar is pre-configured for HTTP access; do not add insecure flags.
- If `k8s_registry_push` is absent, the cluster may load images directly. Use `--output type=docker,dest=<file>.tar` to export, then load via the engine's import mechanism.
- Use `image_build_backend` from `<system-reminder>` to choose builder (`buildkit`, `docker`, or `none`) and follow it strictly.

### Build Backend Rules
- If backend is `buildkit`, build/push via `buildctl --addr ${BUILDKIT_HOST}`. Do not run `docker build/push`.
- If backend is `docker`, check `docker_daemon_source` before Docker-based build/push.
- If backend is `none`, use prebuilt images or a Kubernetes-native builder job.
- Serialize build/push jobs: run only one image build per plugin at a time.

### Build Verification
Always verify compilation locally in the workspace first (e.g. `cargo check`, `npm run build`, `go build ./...`) before running any container image build. Fix all compilation errors locally where feedback is instant. Only proceed to image build once the project compiles cleanly. If an image build fails, reproduce and fix the error locally rather than re-running the image build in a loop. Clean up build artifacts before the image build to reclaim disk.

### If a build or push fails
Use the exact endpoints from `<system-reminder>`. Do not guess IPs or scan the network. If the endpoint does not work, report the error to the user or parent agent.

### Workspace is Source of Truth
Always write or edit code and manifest files in your workspace, then apply with `kubectl apply`. Never mutate cluster state directly with `kubectl patch`, `kubectl edit`, `kubectl set`, or `kubectl delete` followed by imperative recreation. This ensures your workspace always reflects the live state and you can re-deploy from disk at any time.

### Anti-patterns — Never Do These
- **Never write code to /tmp.** Code belongs in your workspace where it is versioned and persistent.
- **Never store source code or build artifacts in ConfigMaps.** ConfigMaps are for configuration data (env vars, config files). Application code goes: workspace → container image → registry → deployed pod.
- **Never skip the image build step.** If your service needs code to run, that code must be in a container image, not mounted from a ConfigMap or copied at runtime.

### Deploy Workflow
1. Build image and deliver it (push to registry or import to engine)
2. Generate/update manifests in workspace
3. Deploy with `kubectl apply`
4. Validate with `kubectl rollout status` and list managed resources

### Safe kubectl Commands
Read-only commands are always fine: `get`, `describe`, `logs`, `rollout status`, `top`, `auth can-i`.

## Storage Awareness

Your home directory has finite disk. Before large operations (cloning repos, building projects, installing dependencies), check available space with `df -h .`. After successful builds, clean up build artifacts and caches. If disk usage exceeds 80%, proactively prune old workspaces, build outputs, and unused clones.
