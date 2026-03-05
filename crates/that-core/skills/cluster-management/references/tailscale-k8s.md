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

To expose a Kubernetes Service to the mesh, annotate it:

```
tailscale.com/expose: "true"
```

Optionally set a custom hostname on the mesh (defaults to the Service name):

```
tailscale.com/hostname: "my-service"
```

Apply with `kubectl annotate` or patch the Service manifest directly.

**What happens next:**
1. The operator detects the annotation and creates a proxy StatefulSet
2. The proxy authenticates to the tailnet and registers a device
3. The service becomes reachable at `https://<hostname>.<tailnet-name>.ts.net`
4. If using MagicDNS, also reachable at `http://<hostname>` from mesh devices

**Key behaviors:**
- Annotation added → proxy deployed, service exposed
- Annotation removed → proxy torn down, service unexposed
- Service deleted → proxy automatically cleaned up
- The operator reconciles continuously — no manual proxy management needed

## Discovering the Mesh URL

After annotating a Service, the agent needs to find the actual tailnet URL.

**Method 1 — Check the proxy device status:**
```
kubectl get svc <service-name> -n <namespace> -o jsonpath='{.metadata.annotations}'
```
Look for `tailscale.com/hostname` — if set, the URL is `https://<hostname>.<tailnet-name>.ts.net`.

**Method 2 — List tailscale operator proxy pods:**
```
kubectl get statefulsets -A -l tailscale.com/parent-resource-type=svc
```
The StatefulSet name corresponds to the device name on the tailnet.

**Method 3 — Query the tailnet device list from inside the cluster:**
```
kubectl get pods -A -l tailscale.com/parent-resource-type=svc -o wide
```
Match the parent service name to find the proxy, then derive the URL.

**URL pattern:**
- With HTTPS (MagicDNS + auto-certs): `https://<hostname>.<tailnet-name>.ts.net`
- Without HTTPS: `http://<hostname>.<tailnet-name>.ts.net`
- The tailnet name is visible in the Tailscale admin console or via `tailscale status`

**Important:** The mesh URL is only reachable from devices that are connected to the
same tailnet. It is not a public URL. The agent should always clarify this when
reporting the URL to the user.

## Service Configuration

- The Service must exist and have healthy endpoints before annotating
- Use port **80** on the Service (agent port convention) for clean mesh URLs
- The proxy forwards to the Service's ClusterIP — standard Kubernetes service routing applies
- Use `tailscale.com/hostname` to set a human-friendly name instead of the default

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
