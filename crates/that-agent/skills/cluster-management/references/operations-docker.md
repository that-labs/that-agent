# Operations — Docker Backend

Resource lifecycle, storage, cleanup, and capacity management when running on Docker.

## Resource Labeling

- Label every container with ownership, role, and hierarchy metadata
- Use labels for filtering when listing, inspecting, or cleaning up containers
- Compose inherits project-level labels automatically; add custom labels for agent metadata

## Storage

- **Named volumes** for persistent data that survives container recreation
- **Bind mounts** for sharing host directories into containers (workspace, config)
- Clean up orphaned volumes after removing containers — but verify nothing depends on them first
- Never bind-mount sensitive host paths without explicit user approval
- Workload data that can be regenerated (caches, build artifacts) should use tmpfs or ephemeral mounts

## Cleanup

- Remove stopped containers for completed tasks
- Prune unused images periodically to reclaim disk
- Remove orphaned networks after compose stack teardown
- Clean dangling volumes carefully — never prune indiscriminately
- Each deployment should leave the environment cleaner than it found it

## Capacity

- Monitor host disk, memory, and CPU before deploying new workloads
- Set memory and CPU limits on containers to prevent noisy-neighbor issues
- Watch for port conflicts when binding to host ports

## Secrets

- Use environment variables or mounted secret files — never bake secrets into images
- `.env` files for local development only — never commit them
- Docker Secrets (Swarm) or external secret managers for production

## Incident Response

1. **Scope** — single container, compose stack, or host-wide?
2. **Recent changes** — what was deployed or modified?
3. **Gather evidence** — container logs, inspect output, host resource usage
4. **Fix or rollback** — recreate from known-good image/config if root cause is unclear
5. **Document** — store incident details in memory for future reference

Never delete and recreate as a first response — investigate first.
