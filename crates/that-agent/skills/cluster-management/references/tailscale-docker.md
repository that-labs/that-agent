# Tailscale — Docker Backend

Using Tailscale to expose Docker-hosted services to the VPN mesh without public ports.

## How It Works

The Tailscale daemon runs as a system service or a sidecar container on the Docker host.
It authenticates the host to the mesh and can proxy local ports onto the mesh network.

Each exposed port gets a mesh-routable address on the host's mesh identity, reachable
only by authenticated mesh members.

## Exposing a Service

To expose a Docker service to the mesh:

1. Ensure the Tailscale daemon is running and authenticated on the host
2. Configure the daemon to serve/proxy the target container's port
3. The service becomes reachable at `<host-mesh-name>:<port>` from any mesh device
4. Prefer exposing port 80 externally for clean URLs

**Alternatively**, run Tailscale as a sidecar container in the same Docker network:
- The sidecar joins the mesh and forwards traffic to the target service
- This avoids exposing ports on the host at all
- Each sidecar gets its own mesh identity and hostname

## Access Control

- Mesh ACLs determine which devices/users can reach exposed services
- Tags group services by role or environment
- ACLs are a separate security layer from Docker network isolation — both must be configured
- Update ACLs when adding new services; stale ACLs are the most common cause of
  "service exposed but unreachable"

## Layered Security

| Layer | Responsibility |
|-------|---------------|
| Docker network isolation | Controls container-to-container connectivity |
| Mesh ACL | Controls which mesh members reach which services |
| Application auth | Controls actions within the service |

All three layers should be active. Do not rely on any single layer.

## Debugging

When a mesh-exposed service is unreachable:

1. **Service health** — is the container running and healthy?
2. **Daemon status** — is Tailscale connected and authenticated?
3. **DNS resolution** — can the client resolve the mesh hostname?
4. **ACLs** — does the client's mesh identity have permission?
5. **Docker networking** — can the daemon/sidecar reach the target container?
6. **Daemon logs** — connection attempts and failures

Common issues:
- Daemon not authenticated (expired auth key)
- ACL not updated for the new service
- Sidecar not on the same Docker network as the target
- Port mismatch between what the daemon proxies and what the container binds
