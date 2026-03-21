---
name: cluster-management
description: Cluster and infrastructure awareness across all backends. Use when reasoning about networking, security policies, service exposure, node operations, or debugging infrastructure issues. Adapts to the active backend — container runtime, orchestrator, or bare host.
metadata:
  bootstrap: true
  always: false
  version: 1.0.0
---

# Cluster Management

This skill defines how an agent understands, operates, and reasons about the infrastructure
it runs on — regardless of backend. The agent is not just a tenant; it is an
infrastructure-aware operator that adapts its behavior to the active runtime environment.

## Backend Detection

The agent's `<system-reminder>` metadata declares the active backend. Before taking any
infrastructure action, the agent must identify which backend is active and adjust its
approach accordingly.

| Backend | Environment | Characteristics |
|---------|------------|-----------------|
| Docker (standalone) | Single host with Docker daemon | Container networking via bridge/overlay, compose for multi-service, host port mapping for exposure |
| Docker (sandboxed) | Agent runs inside a container, may or may not have socket access | Limited host visibility, sibling containers via socket if available |
| Kubernetes (k3s) | Lightweight single-node or small cluster | Full orchestrator semantics, CNI-managed networking, optional advanced networking stack |
| Kubernetes (managed) | Cloud-managed cluster | Full orchestrator, cloud-native networking, managed load balancers |

The agent should never assume a specific backend. When the preamble says "Docker", do not
attempt orchestrator commands. When it says "Kubernetes", do not attempt to control containers
via Docker socket.

## Infrastructure Surfaces

Every backend exposes a set of surfaces the agent can reason about and operate on:

| Surface | Docker | Kubernetes |
|---------|--------|-----------|
| Networking | Bridge networks, port mapping, DNS via embedded resolver | CNI-managed pod networking, Services, NetworkPolicies |
| Security | Container isolation, network segmentation | Namespace isolation, RBAC, network policies (L3-L7 when CNI supports it) |
| Service exposure | Host port binding, reverse proxy | ClusterIP, NodePort, VPN mesh operator, Ingress |
| Storage | Bind mounts, named volumes | PersistentVolumeClaims, StorageClasses |
| Observability | Container logs, stats | Pod logs, events, flow visibility (when CNI provides it) |

When infrastructure components are declared in system metadata, the agent should load the
relevant reference (`read_skill cluster-management <ref>`) before acting on that surface.

## Security Posture — All Backends

Regardless of backend, the agent follows a least-privilege security model:

### Network Isolation
- **Docker**: Create dedicated networks per concern. Services only join the networks they need.
  Never use `--network=host` unless explicitly required and approved.
- **Kubernetes**: Default-deny network policies per namespace. Layer explicit allow rules
  for justified traffic flows. When the CNI supports L7 policies, prefer application-layer
  restrictions over blanket port access. See the `cilium` reference.

### Identity Over Addresses
- Policies and access rules should reference workload identity (labels, service names,
  network aliases) — never hardcoded IPs. IPs are ephemeral across all backends.

### Egress Control
- Outbound traffic matters as much as inbound. Workloads should only reach the external
  endpoints they need. DNS-based egress filtering is preferred when available.

### Least Privilege Per Workload
- Each deployed service (plugins, bridges, tools) gets only the access it requires.
  A bridge talks to the agent gateway and its external platform — nothing else.

## Service Exposure Model

Services are private by default on every backend. Exposure is intentional and tiered:

| Tier | Docker | Kubernetes |
|------|--------|-----------|
| Internal only | Reachable on dedicated network, no port binding | ClusterIP Service, no external route |
| Mesh / VPN | VPN client on host forwards to container port | VPN operator annotation exposes service to mesh devices |
| Public internet | Host port binding with firewall rules | Ingress controller or LoadBalancer (requires explicit approval) |

Always prefer the narrowest tier that satisfies the requirement. Public exposure requires
user approval and should include rate limiting and authentication.

### Port Convention (All Backends)

All agent-managed services expose port **80** as their external-facing port, mapping
internally to whatever port the process binds. This keeps URLs clean, avoids port-suffix
friction, and ensures compatibility with mesh-exposed services and DNS-only routing.

- **Docker**: `-p 80:<internal-port>` or compose `ports: ["80:<internal-port>"]`
- **Kubernetes**: `port: 80, targetPort: <internal-port>` on the Service

## Operational Hygiene

### Resource Labeling
- Label all resources consistently using a standard convention across backends
- Docker: use container labels for ownership, role, and hierarchy
- Kubernetes: use standard label keys for app identity, component, and managed-by

### Namespace / Network Isolation
- Group workloads by concern (agent core, plugins, bridges, monitoring)
- Docker: one compose project or dedicated network per group
- Kubernetes: one namespace per group with baseline deny-all policy

### Debugging — Universal Order
1. **Events / Logs** — check for scheduling failures, image pull errors, startup crashes
2. **Configuration** — inspect resource specs for misconfigurations
3. **Network** — verify connectivity, DNS resolution, and policy enforcement
4. **State** — check volume mounts, environment variables, secret availability

### Cleanup
- Remove completed jobs, failed containers/pods, and stale resources after operations
- Do not let orphaned workloads accumulate across deployments

## References

Read only the references that match your active backend.

### Kubernetes Backend
| Reference | When to read |
|-----------|-------------|
| `cilium` | CNI-level network policies, L7 rules, Hubble flow visibility, zero-trust enforcement |
| `tailscale-k8s` | Exposing services via mesh operator, managing proxies, debugging mesh connectivity |
| `networking-k8s` | Services, DNS, NetworkPolicies, namespace isolation, rollout patterns |
| `operations-k8s` | Resource lifecycle, storage (PVCs), rollouts, cleanup, capacity |

### Docker Backend
| Reference | When to read |
|-----------|-------------|
| `tailscale-docker` | Exposing services via mesh daemon/sidecar, debugging mesh connectivity |
| `networking-docker` | Bridge networks, compose DNS, port mapping, network segmentation |
| `operations-docker` | Container lifecycle, volumes, cleanup, host capacity |
