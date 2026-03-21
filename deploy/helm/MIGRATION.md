# Migration from Kustomize to Helm

This guide covers migrating an existing Kustomize-based that-agent deployment to the Helm chart.

Helm cannot adopt resources created by Kustomize — you must delete the old resources first, then install via Helm. PVCs and Secrets survive the migration because they are not deleted.

## Step 1: Record your current state

```bash
# Your namespace
NAMESPACE="that-agent-default"  # adjust to your namespace

# Your agent name (from the ConfigMap)
kubectl get configmap that-agent-config -n $NAMESPACE -o jsonpath='{.data.THAT_AGENT_NAME}'

# Your PVCs (these survive migration)
kubectl get pvc -n $NAMESPACE

# Your secret (this survives migration)
kubectl get secret that-agent-secrets -n $NAMESPACE
```

Save this information — you'll need it for the Helm install.

## Step 2: Delete all Kustomize-managed resources (except PVCs and Secrets)

The old Kustomize deployment created these resources. Delete them in order:

```bash
# Deployments (agent + infrastructure)
kubectl delete deployment that-agent \
  that-agent-git-server \
  that-agent-buildkit \
  that-agent-cache-proxy \
  --ignore-not-found -n $NAMESPACE

# Services
kubectl delete service that-agent \
  that-agent-git-server \
  that-agent-buildkit \
  that-agent-cache-proxy \
  --ignore-not-found -n $NAMESPACE

# RBAC
kubectl delete serviceaccount that-agent --ignore-not-found -n $NAMESPACE
kubectl delete role that-agent-runtime \
  that-agent-child-readonly \
  that-agent-child-sandbox \
  --ignore-not-found -n $NAMESPACE
kubectl delete rolebinding that-agent --ignore-not-found -n $NAMESPACE
kubectl delete clusterrole that-agent-cluster --ignore-not-found
kubectl delete clusterrolebinding that-agent-cluster --ignore-not-found

# ConfigMaps (Helm will recreate these)
kubectl delete configmap -l app.kubernetes.io/name=that-agent \
  --ignore-not-found -n $NAMESPACE

# Network policy
kubectl delete networkpolicy that-agent-inter-agent --ignore-not-found -n $NAMESPACE
```

**What survives:**
- `that-agent-home` PVC (agent memory, config, skills, identity)
- `that-agent-buildkit-cache` PVC (build cache)
- `that-agent-secrets` Secret (API keys)

## Step 3: Clean Kustomize annotations from surviving PVCs

Helm will refuse to manage resources that still carry Kustomize ownership annotations:

```bash
# Agent home PVC
kubectl annotate pvc that-agent-home \
  kubectl.kubernetes.io/last-applied-configuration- \
  -n $NAMESPACE
kubectl label pvc that-agent-home \
  app.kubernetes.io/managed-by- \
  -n $NAMESPACE

# BuildKit cache PVC (if exists)
kubectl annotate pvc that-agent-buildkit-cache \
  kubectl.kubernetes.io/last-applied-configuration- \
  -n $NAMESPACE 2>/dev/null || true
kubectl label pvc that-agent-buildkit-cache \
  app.kubernetes.io/managed-by- \
  -n $NAMESPACE 2>/dev/null || true
```

## Step 4: Install via Helm

```bash
helm install that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n $NAMESPACE \
  --set agent.name=<your-agent-name> \
  --set agent.storage.existingClaim=that-agent-home \
  --set buildkit.storage.existingClaim=that-agent-buildkit-cache \
  --set secrets.existingSecret=that-agent-secrets \
  --set accessLevel=cluster-admin
```

Or with a local chart:

```bash
helm install that-agent deploy/helm/that-agent/ \
  -n $NAMESPACE \
  -f deploy/helm/values-default.yaml
```

## Step 5: Verify

```bash
# Check pods are running
kubectl get pods -n $NAMESPACE

# Check agent logs
kubectl logs -f deploy/that-agent-that-agent -n $NAMESPACE

# Verify memory survived
kubectl exec deploy/that-agent-that-agent -n $NAMESPACE -- \
  that --agent <name> mem search "test"
```

## Step 6: Migrate child agents

After the root agent is running on Helm, ask it to update its children.
The agent knows how to detect legacy children (deployed via raw manifests)
and migrate them to Helm releases — see the `agent-orchestrator` skill.

You can trigger this by messaging the agent:

> Check your child agents and migrate any that are still on the old deployment model.

The agent will:
1. List managed deployments in the namespace
2. Check each against Helm releases
3. Migrate legacy children one at a time (delete raw resources, re-deploy via Helm, preserving PVCs)

## Mapping reference

| Kustomize overlay field | Helm value |
|---|---|
| `namespace.yaml` | `helm install -n <namespace>` |
| `patch-configmap.yaml` THAT_AGENT_NAME | `agent.name` |
| `patch-configmap.yaml` THAT_AGENT_PROVIDER | `agent.provider` |
| `patch-configmap.yaml` THAT_AGENT_MODEL | `agent.model` |
| `patch-configmap.yaml` THAT_AGENT_BOOTSTRAP_PROMPT | `agent.bootstrapPrompt` |
| `secret.yaml` (any key) | `secrets.existingSecret` (recommended) |
| ClusterRole + ClusterRoleBinding present | `accessLevel: cluster-admin` |
| Only namespace Role | `accessLevel: namespace-admin` |

## Access level reference

| Level | Spawns children | Cluster-wide read | Creates ingress |
|---|---|---|---|
| `cluster-admin` | Yes | Yes | Yes |
| `namespace-admin` | Yes (own namespace) | No | Yes (own namespace) |
| `readonly` | No | No | No |

## Disabling optional services

```bash
helm install that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n that-agent --create-namespace \
  --set gitServer.enabled=false \
  --set buildkit.enabled=false \
  --set cacheProxy.enabled=false
```

## Fresh install (no existing data)

```bash
helm install that-agent oci://ghcr.io/that-labs/helm/that-agent \
  -n that-agent --create-namespace \
  --set agent.name=my-agent \
  --set agent.bootstrapPrompt="You are a helpful assistant" \
  --set secrets.anthropicApiKey=sk-ant-... \
  --set accessLevel=namespace-admin
```
