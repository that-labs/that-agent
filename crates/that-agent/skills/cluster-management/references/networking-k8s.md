# Networking — Kubernetes Backend

Networking patterns, DNS, service discovery, and policy enforcement when the agent runs
on a Kubernetes cluster (k3s, managed, or self-hosted).

## Service Discovery

Every Service gets a DNS entry:
- Full form: `<service>.<namespace>.svc.cluster.local`
- Same namespace: `<service>` (short form)
- Cross namespace: `<service>.<namespace>`

Always use DNS names for service-to-service communication. Pod IPs are ephemeral and
change on restart — never reference them in configuration or policies.

## Service Types

| Type | Use Case |
|------|---------|
| ClusterIP | Internal-only (default — always prefer this) |
| NodePort | Static port on every node (infrastructure services like registries) |
| LoadBalancer | Cloud-managed external IP (needs cloud provider or MetalLB) |
| ExternalName | DNS alias to an external service |

Default to ClusterIP. Escalate only when there is a specific, justified reason.

### Headless Services

`clusterIP: None` returns pod IPs directly via DNS instead of load-balancing through
a virtual IP. Use for:
- StatefulSets where clients need to address individual pods
- Custom client-side load balancing

## Port Convention

All agent-managed services expose port **80** on the Service:
`port: 80, targetPort: <internal-port>`

Clean URLs, mesh compatibility, no port-suffix friction.

## Namespace Isolation

Group workloads by concern:
- Agent core (main agent pod, gateway)
- Plugin services (one namespace or sub-namespace per plugin group)
- Channel bridges
- Infrastructure (registry, build tools)
- Monitoring / observability

Each namespace should have a baseline **deny-all** network policy. Traffic between
namespaces requires explicit allow policies.

## Network Policies

### Default Deny Baseline

Every namespace the agent manages must start with a default-deny policy for both
ingress and egress. This is the foundation of zero-trust networking.

### Allow Policies

Layer specific allow rules on top of deny-all:
- Select by labels and namespace selectors — never by IP
- Use the most restrictive match possible
- When the CNI supports L7 policies (see `cilium` reference), prefer L7 over L3/L4

### Common Policy Patterns

| Flow | Ingress/Egress | Selector |
|------|---------------|----------|
| Agent pod → LLM API | Egress from agent namespace, FQDN-based if supported |
| Bridge → Agent gateway | Ingress to agent namespace from bridge namespace, L7 POST to inbound path |
| All pods → DNS | Egress to kube-system (CoreDNS), port 53 |
| Agent → Registry | Egress to registry namespace, registry port |
| Monitoring → All | Ingress from monitoring namespace, metrics port |

### Egress Control

Outbound traffic is open by default unless policies restrict it. The agent should:
- Add egress deny-all alongside ingress deny-all in the baseline
- Whitelist specific external endpoints per workload
- Use FQDN-based egress rules when the CNI supports it (avoids tracking IP changes)

## DNS Debugging

When service discovery fails:
1. Exec into a pod and test resolution (`nslookup <service>.<namespace>`)
2. Verify the Service exists and has endpoints
3. Check CoreDNS pods are running and healthy
4. Verify no network policy blocks DNS traffic (port 53 to kube-system)

## Health Probes

Every workload must have:
- `readinessProbe` — gates traffic routing; Service only sends traffic to ready pods
- `livenessProbe` — restarts unhealthy pods

The agent waits for readiness before declaring a service operational.

## Rollout Patterns

- Always use rolling updates with readiness probes
- Wait for rollout completion before proceeding
- If rollout stalls, investigate (`describe` deployment, check events) — do not force
- Keep previous ReplicaSets for quick rollback

## Debugging Workflow

1. **Events** — check namespace events for scheduling, pull, or policy errors
2. **Logs** — pod logs for application-level failures
3. **Describe** — resource specs for misconfigurations
4. **Network flows** — if the CNI provides flow observability, trace denied connections
5. **DNS** — verify resolution from within a pod
6. **Endpoints** — verify the Service has healthy endpoints backing it
