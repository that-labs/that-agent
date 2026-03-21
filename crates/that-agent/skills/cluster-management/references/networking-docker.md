# Networking — Docker Backend

Networking patterns, DNS, and service discovery when the agent runs on a Docker-based
backend (standalone host or sandboxed container).

## Bridge Networks

Every Docker host has a default bridge network. For proper service discovery, always
create user-defined bridge networks.

**User-defined bridges provide:**
- Automatic DNS resolution between containers by name
- Network isolation — containers on different bridges cannot communicate
- Better security than the default bridge

**Agent rules:**
- Always create dedicated networks for related services (compose does this automatically)
- Never use `--network=host` unless the task explicitly requires it and the user approves
- Use network aliases for stable service names
- When connecting a container to multiple networks, be intentional and document why

## Compose Networking

Docker Compose creates a dedicated network per project. Services within a project
resolve each other by service name automatically.

- Cross-project communication requires explicit external network references
- Create a shared network when services from different compose stacks need to talk
- Port publishing (`ports:`) exposes to the host; internal service-to-service
  communication uses the compose network directly (no port publishing needed)

## DNS

- User-defined bridge: embedded DNS resolves container names and network aliases
- Default bridge: no automatic DNS — must use IPs (avoid this)
- Host network: uses the host's DNS resolver
- Always verify DNS resolution works before declaring a service ready

## Port Convention

All agent-managed services expose port **80** externally:
`-p 80:<internal>` or compose `ports: ["80:<internal>"]`

This ensures consistent, clean URLs across all exposure methods.

## Network Segmentation

Keep workloads isolated by concern using dedicated networks:
- Agent infrastructure (core, registry)
- Plugin services
- Channel bridges
- Monitoring / observability

Cross-group communication should use explicit shared networks with clear justification.

## Security

- Create dedicated networks per concern — do not dump everything on one bridge
- Avoid `--network=host` (breaks isolation)
- Avoid `--privileged` unless strictly necessary
- Use firewall rules on the host to restrict outbound traffic when needed
- External API access works by default via host networking; document which endpoints
  each workload requires

## Health Checks

Every service should have a health check:
- `HEALTHCHECK` directive in the Dockerfile, or
- `healthcheck:` block in compose

Wait for health checks to pass before declaring a service operational.

## Debugging

1. **Inspect the network** — list containers on the network, check IPs and aliases
2. **Test DNS** — exec into a container and resolve service names
3. **Check port bindings** — verify published ports are not conflicting
4. **Container logs** — check for bind errors or connection failures
5. **Host firewall** — verify iptables/nftables rules are not blocking traffic
