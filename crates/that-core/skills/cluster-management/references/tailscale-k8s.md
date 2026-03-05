# Tailscale — Kubernetes Backend

Using the Tailscale Operator to expose Kubernetes services to the VPN mesh without
public ingress or LoadBalancer resources.

## How It Works

The Tailscale Operator runs in the cluster and watches for annotated Services. When it
detects the mesh-exposure annotation, it deploys a proxy pod that bridges the Service
onto the mesh network.

- The exposed service gets a stable mesh DNS name (typically `<service>.<tailnet-domain>`)
- Only authenticated mesh members with appropriate ACL permissions can reach it
- The proxy handles mesh authentication and encrypted transport transparently
- No public IP, ingress controller, or firewall rules required

## Exposing a Service

To expose a Kubernetes Service to the mesh:

1. Annotate the Service with the operator's exposure annotation
2. The operator detects the annotation and deploys a proxy
3. The service becomes reachable via mesh DNS from any authorized device
4. Verify reachability from a mesh-connected device after annotation

**Key behaviors:**
- Annotation added → proxy deployed, service exposed
- Annotation removed → proxy torn down, service unexposed
- Service deleted → proxy automatically cleaned up
- The operator reconciles continuously — no manual proxy management needed

## Service Configuration

- The Service must exist and have healthy endpoints before annotating
- Use port **80** on the Service (agent port convention) for clean mesh URLs
- The proxy forwards to the Service's ClusterIP — standard Kubernetes service routing applies
- Hostname on the mesh can be customized via operator annotations if the default is not suitable

## Access Control

Mesh ACLs are managed in the Tailscale admin console or via GitOps policy files:

- **ACL policies** define which mesh identities (users, devices, tags) can reach which services
- **Tags** group exposed services by role or environment
- **Auto-approve** can be configured for operator-managed services
- Update ACLs when exposing new services — stale ACLs are the top cause of "exposed but unreachable"

## Layered Security

| Layer | Responsibility |
|-------|---------------|
| Kubernetes NetworkPolicy (CNI) | Controls pod-to-pod traffic inside the cluster |
| Mesh ACL | Controls which mesh members reach which exposed services |
| Application auth | Controls actions within the service |

All three layers must be configured. The mesh proxy pod is a workload inside the cluster —
it must be allowed by NetworkPolicies to reach the backend Service it proxies.

## Debugging

When a mesh-exposed service is unreachable:

1. **Service health** — are backend pods running and ready?
2. **Operator status** — is the operator pod running and reconciling?
3. **Proxy pod** — is the mesh proxy pod running? Check its logs
4. **Mesh DNS** — can the client resolve the mesh hostname?
5. **ACLs** — does the client's identity have permission to reach this service?
6. **NetworkPolicies** — is the proxy pod allowed to reach the backend Service?

Common issues:
- Operator not running or crashlooping (check operator pod logs)
- NetworkPolicy blocking the proxy pod from reaching the backend
- ACL not updated after adding a new service
- DNS propagation delay after initial exposure (wait a minute and retry)
- OAuth credentials expired or missing (operator needs valid credentials to register proxies)

## Operator Lifecycle

- The operator is deployed once (typically via Helm) and manages all mesh-exposed services
- OAuth client credentials must remain valid — rotate before expiry
- Operator upgrades should be tested in a staging namespace first
- Monitor operator pod health — if it goes down, no new services can be exposed or updated
  (existing proxies continue to function)
