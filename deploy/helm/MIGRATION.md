# Migration from Kustomize to Helm

This guide covers migrating an existing Kustomize-based that-agent deployment to the Helm chart.

## Before you start

1. Note your current namespace: `kubectl get ns | grep that-agent`
2. List existing PVCs: `kubectl get pvc -n <namespace>`
3. Back up your secrets: `kubectl get secret that-agent-secrets -n <namespace> -o yaml > secrets-backup.yaml`

## Mapping Kustomize to Helm values

| Kustomize overlay field | Helm value |
|---|---|
| `namespace.yaml` | `helm install -n <namespace>` |
| `patch-configmap.yaml` THAT_AGENT_NAME | `agent.name` |
| `patch-configmap.yaml` THAT_AGENT_PROVIDER | `agent.provider` |
| `patch-configmap.yaml` THAT_AGENT_MODEL | `agent.model` |
| `patch-configmap.yaml` THAT_AGENT_BOOTSTRAP_PROMPT | `agent.bootstrapPrompt` |
| `secret.yaml` ANTHROPIC_API_KEY | `secrets.anthropicApiKey` (or `secrets.existingSecret`) |
| `secret.yaml` OPENAI_API_KEY | `secrets.openaiApiKey` |
| `secret.yaml` TELEGRAM_BOT_TOKEN | `secrets.telegramBotToken` |
| ClusterRole + ClusterRoleBinding present | `accessLevel: cluster-admin` |
| Only namespace Role | `accessLevel: namespace-admin` |

## Preserving existing PVCs

If you have an existing agent with data in a PVC (memory, config, skills), you can mount it directly without creating a new one.

### Step 1: Find your existing PVC names

```bash
kubectl get pvc -n <namespace>
# Example output:
# NAME               STATUS   VOLUME   CAPACITY
# that-agent-home    Bound    pv-xxx   20Gi
# that-agent-buildkit-cache   Bound    pv-yyy   20Gi
```

### Step 2: Remove Kustomize ownership labels

Helm will refuse to adopt resources with different ownership. Remove the old labels:

```bash
kubectl annotate pvc that-agent-home \
  kubectl.kubernetes.io/last-applied-configuration- \
  -n <namespace>

kubectl label pvc that-agent-home \
  app.kubernetes.io/managed-by- \
  -n <namespace>
```

### Step 3: Install Helm chart pointing to existing PVCs

```bash
helm install that-agent deploy/helm/that-agent/ \
  -n <namespace> \
  --set agent.storage.existingClaim=that-agent-home \
  --set buildkit.storage.existingClaim=that-agent-buildkit-cache \
  --set secrets.existingSecret=that-agent-secrets \
  --set agent.name=<your-agent-name> \
  --set accessLevel=cluster-admin
```

This tells the Helm chart to skip PVC creation and mount the existing ones.

### Step 4: Delete old Kustomize resources

After verifying the Helm deployment is running:

```bash
# Delete old deployment (Helm created a new one)
kubectl delete deployment that-agent --ignore-not-found -n <namespace>

# Delete old ConfigMaps (Helm created new ones)
kubectl delete configmap that-agent-config that-agent-entrypoint --ignore-not-found -n <namespace>

# Delete old ServiceAccount, Roles, RoleBindings (Helm recreated them)
kubectl delete sa that-agent --ignore-not-found -n <namespace>
kubectl delete role that-agent-runtime --ignore-not-found -n <namespace>
kubectl delete rolebinding that-agent --ignore-not-found -n <namespace>
kubectl delete clusterrole that-agent-cluster --ignore-not-found
kubectl delete clusterrolebinding that-agent-cluster --ignore-not-found

# Keep: PVCs (mounted by Helm), Secrets (referenced by Helm)
```

## Fresh install (no existing data)

```bash
helm install that-agent deploy/helm/that-agent/ \
  -n that-agent --create-namespace \
  --set agent.name=my-agent \
  --set agent.bootstrapPrompt="You are a helpful assistant" \
  --set secrets.anthropicApiKey=sk-ant-... \
  --set accessLevel=namespace-admin
```

## Access level reference

| Level | Spawns children | Cluster-wide read | Creates ingress |
|---|---|---|---|
| `cluster-admin` | Yes | Yes | Yes |
| `namespace-admin` | Yes (own namespace) | No | Yes (own namespace) |
| `readonly` | No | No | No |

## Disabling optional services

```bash
helm install that-agent deploy/helm/that-agent/ \
  -n that-agent --create-namespace \
  --set gitServer.enabled=false \
  --set buildkit.enabled=false \
  --set cacheProxy.enabled=false
```
