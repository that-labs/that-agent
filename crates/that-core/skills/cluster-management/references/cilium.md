# Cilium — CNI & Zero-Trust Networking

Cilium is an eBPF-based CNI that provides pod networking, network policy enforcement at
layers 3/4/7, transparent encryption, and deep flow observability. This reference applies
when the agent runs on Kubernetes with Cilium as the active CNI.

## When This Applies

The agent should consult this reference when:
- System metadata indicates Cilium is the active CNI
- The task involves creating or modifying network policies
- Debugging connectivity issues between workloads
- Reasoning about application-layer (HTTP, gRPC, DNS) access control
- Needing flow-level visibility into traffic patterns

## Network Policy Layers

Cilium enforces policies at multiple layers. The agent should use the most specific
layer that matches the requirement.

### Layer 3/4 — IP and Port

Standard network policies that filter by source/destination identity and port. Use these
as the baseline deny/allow structure.

- **Default deny**: Every namespace managed by the agent should have a baseline policy
  that denies all ingress and egress. This is non-negotiable in a zero-trust posture.
- **Allow by identity**: Permit traffic between specific workloads using label selectors.
  Never use raw IP addresses — workload identity is the only stable selector.
- **Egress to external**: When a workload needs to reach an external API, use FQDN-based
  egress policies to allow DNS names rather than IP ranges. IPs change; DNS names are stable.

### Layer 7 — Application Protocol

Cilium can inspect and filter at the application layer. This is strictly preferable to
blanket port-level access when the workload speaks HTTP, gRPC, or Kafka.

**HTTP filtering:**
- Restrict by method (GET, POST, PUT, DELETE)
- Restrict by path prefix or exact path
- Restrict by header values (e.g., require a specific host header)

**Use cases:**
- A bridge plugin should only POST to the agent gateway's inbound path — not access any
  other endpoint on the gateway service
- An internal API should only accept GET requests from monitoring and POST/PUT from
  authorized writers
- Webhook receivers should only accept POST on their callback path

**Principle:** If you can express the policy at L7, do it. Port-level access is a fallback,
not the default.

### DNS-Based Policies

Cilium can enforce egress policies based on DNS names, resolving them transparently:

- Allow a workload to reach a specific external service by FQDN
- Block all other egress DNS queries or resolutions
- Useful for plugins that need to call a single external API without opening broad internet access

## Zero-Trust Implementation

A zero-trust cluster means every flow is explicitly authorized. The agent builds this
posture incrementally:

1. **Start with deny-all** in every namespace the agent manages
2. **Map required flows** — for each workload, identify what it needs to talk to and why
3. **Write specific allow policies** at the most restrictive layer possible
4. **Verify with flow observability** — use the observability tool to confirm only expected
   flows are allowed and no unexpected traffic gets through
5. **Iterate** — when deploying new workloads, add policies before or alongside the deployment

### Common Flow Patterns

| Source | Destination | Layer | Justification |
|--------|------------|-------|---------------|
| Bridge plugin | Agent gateway inbound | L7 (POST to inbound path) | Deliver external messages |
| Agent pod | External LLM API | DNS egress (FQDN) | Model inference calls |
| Agent pod | Internal registry | L3/L4 (registry port) | Image push/pull |
| Monitoring | All pods | L7 (GET on metrics path) | Scrape metrics |
| All pods | DNS service | L3/L4 (DNS port) | Name resolution |

## Hubble — Flow Observability

Cilium includes Hubble for real-time flow visibility. The agent should use this when:

- **Debugging denied connections** — Hubble shows policy verdicts (forwarded, dropped, denied)
  with the specific policy that caused the decision
- **Auditing traffic patterns** — before tightening policies, observe actual flows to
  understand what a workload communicates with
- **Verifying policy changes** — after applying a new policy, watch flows to confirm it
  behaves as intended

### Observability Workflow

1. Observe flows for the target workload or namespace
2. Filter by verdict (dropped/denied) to find blocked traffic
3. Identify the policy responsible for the verdict
4. Determine if the block is correct (enforce zero-trust) or a misconfiguration (fix policy)
5. After fixing, re-observe to confirm the intended behavior

## Policy Lifecycle

- **Pre-deploy**: Write policies before or alongside workload deployment, never after
- **Test in audit mode**: If the CNI supports audit/log-only mode, use it to validate
  policies before enforcing
- **Version control**: Treat network policies as code — they belong in the deploy manifests
  alongside the workload definition
- **Review on change**: When a workload's communication needs change (new dependency, new
  endpoint), update its policies in the same change set
