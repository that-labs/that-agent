# Kubernetes Deployment

For full deployment instructions, configuration reference, and troubleshooting, see [OPERATORS.md](../../OPERATORS.md).

## Quick Reference

```bash
# Deploy an overlay
kubectl apply -k deploy/k8s/overlays/<your-overlay>
kubectl -n <your-namespace> rollout status deploy/that-agent

# Create a new overlay from the example
cp -r deploy/k8s/overlays/example deploy/k8s/overlays/<your-agent>
```

See `deploy/k8s/overlays/example/` for a starter template.

## Agent Hierarchy Labels

The base kustomization applies `that-agent/managed: "true"` to all resources.
For multi-agent deployments, child agent overlays should patch in hierarchy labels
so you can query agents by parent or role.

When the parent agent spawns children via `spawn_agent` or `agent_run`, it
automatically applies these labels to all child resources:

```
that-agent/managed: "true"
that-agent/name: "<child-name>"
that-agent/parent: "<parent-name>"
that-agent/type: "persistent" | "ephemeral"
that-agent/role: "<role>"
```

### Setting Hierarchy Labels in Overlays

Create a patch file in your child agent overlay to add parent and role labels:

```yaml
# overlays/<child-agent>/patch-labels.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: that-agent
spec:
  template:
    metadata:
      labels:
        that-agent/parent: "<parent-agent-name>"
        that-agent/role: "<agent-role>"
```

Reference it in your overlay's `kustomization.yaml`:

```yaml
patches:
  - path: patch-labels.yaml
```

Also set the configmap values for the hierarchy env vars:

```yaml
configMapGenerator:
  - name: that-agent-config
    behavior: merge
    literals:
      - THAT_AGENT_PARENT=<parent-agent-name>
      - THAT_AGENT_ROLE=<agent-role>
```

### Querying by Hierarchy

```bash
# List all agents managed by the platform
kubectl get pods -l that-agent/managed=true

# List all child agents of a specific parent
kubectl get pods -l that-agent/parent=<parent-agent-name>

# List all agents with a specific role
kubectl get pods -l that-agent/role=<role>

# List all persistent vs ephemeral agents
kubectl get deployments,jobs -l that-agent/type=persistent
kubectl get jobs -l that-agent/type=ephemeral

# Clean up a specific child's resources
kubectl delete deployment,service,job,sa,rolebinding,configmap -l that-agent/name=<child-name>
```
