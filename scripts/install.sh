#!/usr/bin/env bash
# that-agent VPS installer
#
# One-liner usage:
#   curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
#
# Or download and run with flags:
#   bash install.sh [--image <image:tag>] [--namespace <ns>] [--no-k3s]
#
# What this script does:
#   1. Optionally installs k3s (lightweight Kubernetes) if not already present
#   2. Prompts for agent name, description, and LLM API credentials
#   3. Optionally configures a Telegram channel
#   4. Generates a Kubernetes overlay for your agent
#   5. Deploys to the local cluster

set -euo pipefail

main() {

# ── Colour helpers ──────────────────────────────────────────────────────────
if [ -t 1 ]; then
  _BOLD='\033[1m'; _GREEN='\033[0;32m'; _CYAN='\033[0;36m'
  _YELLOW='\033[0;33m'; _RED='\033[0;31m'; _RESET='\033[0m'
else
  _BOLD=''; _GREEN=''; _CYAN=''; _YELLOW=''; _RED=''; _RESET=''
fi

info()  { echo -e "${_CYAN}[that-agent]${_RESET} $*"; }
ok()    { echo -e "${_GREEN}[ok]${_RESET} $*"; }
warn()  { echo -e "${_YELLOW}[warn]${_RESET} $*"; }
die()   { echo -e "${_RED}[error]${_RESET} $*" >&2; exit 1; }
header(){ echo -e "\n${_BOLD}$*${_RESET}"; }

# ── Defaults ────────────────────────────────────────────────────────────────
AGENT_IMAGE="ghcr.io/that-labs/that-agent:latest"
AGENT_NAMESPACE=""          # derived from agent name if not set
INSTALL_K3S=true
KUBECTL="kubectl"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-/dev/null}")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
REPO_ROOT=""
if [[ -n "${SCRIPT_DIR}" && -f "${SCRIPT_DIR}/../build.sh" ]]; then
  REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
fi
OVERLAY_DIR=""              # set after we know the agent name

# In-cluster registry — NodePort on the host so k3s containerd can pull,
# ClusterIP DNS so BuildKit (inside pods) can push.
REGISTRY_NODEPORT=30500
REGISTRY_NAMESPACE="that-registry"
# k3s image refs use this hostname; registries.yaml maps it → localhost:NodePort
REGISTRY_PULL_HOST="registry.localhost:5000"
# BuildKit (running inside a pod) pushes via in-cluster DNS
REGISTRY_PUSH_ENDPOINT="registry.${REGISTRY_NAMESPACE}.svc.cluster.local:5000"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)      AGENT_IMAGE="$2";    shift 2 ;;
    --namespace)  AGENT_NAMESPACE="$2"; shift 2 ;;
    --no-k3s)     INSTALL_K3S=false;   shift   ;;
    --help|-h)
      echo "Usage: $0 [--image IMAGE:TAG] [--namespace NS] [--no-k3s]"
      exit 0
      ;;
    *) die "Unknown option: $1" ;;
  esac
done

# ── Platform check ──────────────────────────────────────────────────────────
case "$(uname -s)" in
  Linux) ;;
  *) die "This installer targets Linux VPS environments. For local dev, use k3d or Docker directly." ;;
esac

# ── Require root or sudo ─────────────────────────────────────────────────────
if [[ $EUID -ne 0 ]]; then
  SUDO="sudo"
  if ! command -v sudo &>/dev/null; then
    die "Run as root or install sudo."
  fi
else
  SUDO=""
fi

# ── Step 1: k3s ─────────────────────────────────────────────────────────────
header "Step 1 — Kubernetes (k3s)"

if command -v k3s &>/dev/null && k3s kubectl get nodes &>/dev/null 2>&1; then
  ok "k3s is already running."
  KUBECTL="k3s kubectl"
elif command -v kubectl &>/dev/null && kubectl get nodes &>/dev/null 2>&1; then
  ok "kubectl found and cluster is reachable — skipping k3s install."
  INSTALL_K3S=false
elif [[ "${INSTALL_K3S}" == "true" ]]; then
  info "Installing k3s…"
  curl -sfL https://get.k3s.io | $SUDO sh -s - \
    --disable traefik \
    --write-kubeconfig-mode 644

  # Give k3s a moment to start
  info "Waiting for k3s node to be ready…"
  local_timeout=60
  while ! k3s kubectl get nodes &>/dev/null 2>&1; do
    local_timeout=$((local_timeout - 1))
    if [[ $local_timeout -le 0 ]]; then
      die "k3s did not become ready in time. Check: journalctl -u k3s"
    fi
    sleep 1
  done
  ok "k3s is ready."
  KUBECTL="k3s kubectl"

  # Make kubeconfig available to the current user
  if [[ $EUID -ne 0 ]]; then
    mkdir -p "${HOME}/.kube"
    $SUDO cp /etc/rancher/k3s/k3s.yaml "${HOME}/.kube/config"
    $SUDO chown "$(id -u):$(id -g)" "${HOME}/.kube/config"
    KUBECTL="kubectl"
  fi
else
  die "No running Kubernetes cluster found. Remove --no-k3s to install k3s automatically."
fi

# Ensure local-path is the default StorageClass so PVCs without an explicit
# storageClassName bind automatically.
info "Ensuring local-path is the default StorageClass…"
${KUBECTL} patch storageclass local-path \
  -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}' \
  2>/dev/null && ok "local-path marked as default StorageClass." \
  || warn "Could not patch local-path StorageClass — PVCs may need an explicit storageClassName."

# ── Step 2: In-cluster image registry ───────────────────────────────────────
header "Step 2 — In-cluster image registry"

info "Deploying registry:2 into namespace '${REGISTRY_NAMESPACE}' (NodePort ${REGISTRY_NODEPORT})…"

${KUBECTL} apply -f - <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: ${REGISTRY_NAMESPACE}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: registry-data
  namespace: ${REGISTRY_NAMESPACE}
spec:
  storageClassName: local-path
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 20Gi
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: registry
  namespace: ${REGISTRY_NAMESPACE}
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: registry
  template:
    metadata:
      labels:
        app: registry
    spec:
      containers:
        - name: registry
          image: registry:2
          ports:
            - containerPort: 5000
          env:
            - name: REGISTRY_STORAGE_DELETE_ENABLED
              value: "true"
          volumeMounts:
            - name: data
              mountPath: /var/lib/registry
          readinessProbe:
            httpGet:
              path: /v2/
              port: 5000
            initialDelaySeconds: 3
            periodSeconds: 5
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: registry-data
---
apiVersion: v1
kind: Service
metadata:
  name: registry
  namespace: ${REGISTRY_NAMESPACE}
spec:
  selector:
    app: registry
  ports:
    - port: 5000
      targetPort: 5000
      nodePort: ${REGISTRY_NODEPORT}
  type: NodePort
EOF

# Tell k3s containerd to resolve the pull hostname via the NodePort on the host.
# BuildKit (inside pods) reaches the registry via in-cluster ClusterIP DNS.
REGISTRIES_YAML="/etc/rancher/k3s/registries.yaml"
info "Writing ${REGISTRIES_YAML} (maps ${REGISTRY_PULL_HOST} → http://localhost:${REGISTRY_NODEPORT})…"
$SUDO tee "${REGISTRIES_YAML}" > /dev/null <<EOF
mirrors:
  "${REGISTRY_PULL_HOST}":
    endpoint:
      - "http://localhost:${REGISTRY_NODEPORT}"
EOF

info "Restarting k3s to apply registry mirror configuration…"
$SUDO systemctl restart k3s
local_timeout=60
while ! ${KUBECTL} get nodes &>/dev/null 2>&1; do
  local_timeout=$((local_timeout - 1))
  [[ $local_timeout -le 0 ]] && die "k3s did not recover after restart. Check: journalctl -u k3s"
  sleep 1
done

info "Waiting for registry pod to be ready…"
${KUBECTL} -n "${REGISTRY_NAMESPACE}" rollout status deploy/registry --timeout=120s
ok "Registry ready — pull host: ${REGISTRY_PULL_HOST}, push endpoint: ${REGISTRY_PUSH_ENDPOINT}"

# ── Step 3: Gather configuration ────────────────────────────────────────────
header "Step 3 — Agent configuration"

echo ""
echo "  This installer will create a long-lived autonomous agent running on your cluster."
echo "  The description you provide will be used to shape the agent's identity and"
echo "  character during its first-run bootstrap."
echo ""

# Agent name
while true; do
  read -rp "  Agent name (lowercase, no spaces) [default: my-agent]: " AGENT_NAME < /dev/tty
  AGENT_NAME="${AGENT_NAME:-my-agent}"
  if [[ "${AGENT_NAME}" =~ ^[a-z][a-z0-9-]*$ ]]; then
    break
  fi
  warn "Name must start with a letter and contain only lowercase letters, digits, and hyphens."
done

# Agent description — used as the bootstrap prompt
echo ""
echo "  Describe your agent in a few sentences. What is its purpose? What should it"
echo "  focus on? What tone or personality do you want it to have?"
echo "  (This is passed to the LLM at first boot to generate Soul.md and Identity.md.)"
echo ""
read -rp "  Agent description: " AGENT_DESCRIPTION < /dev/tty
if [[ -z "${AGENT_DESCRIPTION}" ]]; then
  AGENT_DESCRIPTION="A general-purpose autonomous agent that helps with software development, research, and task automation."
  warn "No description provided. Using default."
fi

# LLM key — auto-detect provider from key prefix, or pick up from env
LLM_API_KEY="${ANTHROPIC_API_KEY:-${CLAUDE_CODE_OAUTH_TOKEN:-${OPENAI_API_KEY:-${OPENROUTER_API_KEY:-}}}}"
if [[ -n "${LLM_API_KEY}" ]]; then
  ok "Detected API key from environment."
else
  echo ""
  echo "  Paste your API key (Anthropic, Claude Code OAuth, OpenAI, or OpenRouter)."
  echo "  The provider will be detected automatically from the key prefix."
  echo ""
  read -rsp "  API key: " LLM_API_KEY < /dev/tty
  echo ""
fi
if [[ -z "${LLM_API_KEY}" ]]; then
  die "API key is required."
fi

# Detect provider from key prefix
detect_provider() {
  local key="$1"
  if   [[ "${key}" == sk-ant-oat01-* ]]; then echo "anthropic"   # Claude Code OAuth
  elif [[ "${key}" == sk-ant-* ]];        then echo "anthropic"
  elif [[ "${key}" == sk-or-* ]];         then echo "openrouter"
  elif [[ "${key}" == sk-* ]];            then echo "openai"
  else echo ""
  fi
}

LLM_PROVIDER="$(detect_provider "${LLM_API_KEY}")"
if [[ -z "${LLM_PROVIDER}" ]]; then
  die "Could not detect provider from key prefix. Expected sk-ant-*, sk-or-*, or sk-*."
fi
ok "Provider: ${LLM_PROVIDER}"

# Model default
case "${LLM_PROVIDER}" in
  anthropic)  DEFAULT_MODEL="claude-sonnet-4-6" ;;
  openai)     DEFAULT_MODEL="gpt-5.2-codex"     ;;
  openrouter) DEFAULT_MODEL=""                   ;;
esac
echo ""
read -rp "  Model override (leave blank for default: ${DEFAULT_MODEL:-provider default}): " AGENT_MODEL < /dev/tty
AGENT_MODEL="${AGENT_MODEL:-${DEFAULT_MODEL}}"

# Telegram (optional)
echo ""
read -rp "  Telegram bot token (optional, press Enter to skip): " TELEGRAM_BOT_TOKEN < /dev/tty
if [[ -n "${TELEGRAM_BOT_TOKEN}" ]]; then
  read -rp "  Telegram chat ID: " TELEGRAM_CHAT_ID < /dev/tty
  if [[ -z "${TELEGRAM_CHAT_ID}" ]]; then
    warn "No chat ID provided — Telegram channel will not be configured."
    TELEGRAM_BOT_TOKEN=""
    TELEGRAM_CHAT_ID=""
  fi
else
  TELEGRAM_CHAT_ID=""
fi

# Namespace
if [[ -z "${AGENT_NAMESPACE}" ]]; then
  AGENT_NAMESPACE="that-${AGENT_NAME}"
fi

# Overlay output directory
OVERLAY_DIR="${HOME}/.that-agent-install/${AGENT_NAME}"

echo ""
ok "Configuration collected."
echo ""
echo "  Agent name:   ${AGENT_NAME}"
echo "  Namespace:    ${AGENT_NAMESPACE}"
echo "  Provider:     ${LLM_PROVIDER}"
echo "  Model:        ${AGENT_MODEL:-<provider default>}"
echo "  Telegram:     ${TELEGRAM_BOT_TOKEN:+configured}${TELEGRAM_BOT_TOKEN:-not configured}"
echo "  Image:        ${AGENT_IMAGE}"
echo "  Overlay dir:  ${OVERLAY_DIR}"
echo ""
read -rp "  Proceed with deployment? [Y/n]: " CONFIRM < /dev/tty
case "${CONFIRM:-y}" in
  [Yy]*) ;;
  *) info "Aborted."; exit 0 ;;
esac

# ── Step 4: Build or pull image ──────────────────────────────────────────────
header "Step 4 — Container image"

if [[ -n "${REPO_ROOT}" && -f "${REPO_ROOT}/build.sh" ]] && command -v docker &>/dev/null; then
  echo ""
  read -rp "  Local repo detected. Build image from source? [y/N]: " BUILD_LOCAL < /dev/tty
  case "${BUILD_LOCAL:-n}" in
    [Yy]*)
      info "Building that-agent image from source…"
      bash "${REPO_ROOT}/build.sh"
      BUILT_IMAGE="that-agent:latest"
      # Import into k3s if we're using k3s
      if command -v k3s &>/dev/null; then
        info "Importing image into k3s containerd…"
        docker save "${BUILT_IMAGE}" | $SUDO k3s ctr images import -
        ok "Image imported: ${BUILT_IMAGE}"
      fi
      AGENT_IMAGE="${BUILT_IMAGE}"
      ;;
    *)
      info "Using image: ${AGENT_IMAGE}"
      ;;
  esac
else
  info "Using image: ${AGENT_IMAGE}"
fi

# ── Step 5: Generate overlay ─────────────────────────────────────────────────
header "Step 5 — Generating Kubernetes overlay"

mkdir -p "${OVERLAY_DIR}"

# Ensure base manifests are available locally
BASE_REF="./base"
if [[ -n "${REPO_ROOT}" && -d "${REPO_ROOT}/deploy/k8s/base" ]]; then
  cp -r "${REPO_ROOT}/deploy/k8s/base" "${OVERLAY_DIR}/base"
  ok "Copied base manifests from local repo."
else
  info "Downloading base manifests from GitHub…"
  mkdir -p "${OVERLAY_DIR}/base"
  curl -fsSL "https://github.com/that-labs/that-agent/archive/refs/heads/main.tar.gz" | \
    tar -xz --strip-components=3 -C "${OVERLAY_DIR}/base" "that-agent-main/deploy/k8s/base/"
  ok "Base manifests downloaded."
fi

# kustomization.yaml
cat > "${OVERLAY_DIR}/kustomization.yaml" <<EOF
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization

namespace: ${AGENT_NAMESPACE}

resources:
  - ${BASE_REF}
  - namespace.yaml
  - secret.yaml

patches:
  - path: patch-configmap.yaml

images:
  - name: that-agent
    newName: ${AGENT_IMAGE%:*}
    newTag: ${AGENT_IMAGE##*:}
EOF

# namespace.yaml
cat > "${OVERLAY_DIR}/namespace.yaml" <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: ${AGENT_NAMESPACE}
EOF

# patch-configmap.yaml
cat > "${OVERLAY_DIR}/patch-configmap.yaml" <<EOF
apiVersion: v1
kind: ConfigMap
metadata:
  name: that-agent-config
data:
  THAT_AGENT_NAME: "${AGENT_NAME}"
  THAT_AGENT_BOOTSTRAP_PROMPT: "${AGENT_DESCRIPTION}"
  THAT_AGENT_PROVIDER: "${LLM_PROVIDER}"
  THAT_AGENT_MODEL: "${AGENT_MODEL}"
  THAT_SANDBOX_MODE: "kubernetes"
  THAT_TRUSTED_LOCAL_SANDBOX: "1"
  THAT_SANDBOX_K8S_NAMESPACE: "${AGENT_NAMESPACE}"
  THAT_SANDBOX_K8S_REGISTRY: "${REGISTRY_PULL_HOST}"
  THAT_SANDBOX_K8S_REGISTRY_PUSH_ENDPOINT: "${REGISTRY_PUSH_ENDPOINT}"
EOF

# secret.yaml — use the correct env var name for the runtime
SECRET_API_KEY_VAR=""
if [[ "${LLM_API_KEY}" == sk-ant-oat01-* ]]; then
  SECRET_API_KEY_VAR="CLAUDE_CODE_OAUTH_TOKEN"
else
  case "${LLM_PROVIDER}" in
    anthropic)   SECRET_API_KEY_VAR="ANTHROPIC_API_KEY" ;;
    openai)      SECRET_API_KEY_VAR="OPENAI_API_KEY"    ;;
    openrouter)  SECRET_API_KEY_VAR="OPENROUTER_API_KEY" ;;
  esac
fi

cat > "${OVERLAY_DIR}/secret.yaml" <<EOF
apiVersion: v1
kind: Secret
metadata:
  name: that-agent-secrets
type: Opaque
stringData:
  ${SECRET_API_KEY_VAR}: "${LLM_API_KEY}"
  TELEGRAM_BOT_TOKEN: "${TELEGRAM_BOT_TOKEN}"
  TELEGRAM_CHAT_ID: "${TELEGRAM_CHAT_ID}"
EOF
$SUDO chmod 600 "${OVERLAY_DIR}/secret.yaml"

ok "Overlay written to ${OVERLAY_DIR}"

# ── Step 6: Deploy ────────────────────────────────────────────────────────────
header "Step 6 — Deploying to cluster"

info "Applying manifests…"
${KUBECTL} apply -k "${OVERLAY_DIR}"

info "Waiting for rollout…"
${KUBECTL} -n "${AGENT_NAMESPACE}" rollout status deploy/that-agent --timeout=120s || {
  warn "Rollout did not complete in 120 s. Check pod status:"
  echo "  ${KUBECTL} -n ${AGENT_NAMESPACE} get pods"
  echo "  ${KUBECTL} -n ${AGENT_NAMESPACE} logs deploy/that-agent"
}

# ── Done ──────────────────────────────────────────────────────────────────────
header "Done"
echo ""
ok "Agent '${AGENT_NAME}' deployed to namespace '${AGENT_NAMESPACE}'."
echo ""
echo "  Useful commands:"
echo ""
echo "    # Follow agent logs"
echo "    ${KUBECTL} -n ${AGENT_NAMESPACE} logs -f deploy/that-agent"
echo ""
echo "    # Open a shell inside the agent pod"
echo "    ${KUBECTL} -n ${AGENT_NAMESPACE} exec -it deploy/that-agent -- bash"
echo ""
echo "    # Restart the agent"
echo "    ${KUBECTL} -n ${AGENT_NAMESPACE} rollout restart deploy/that-agent"
echo ""
echo "    # Remove the agent entirely"
echo "    ${KUBECTL} delete namespace ${AGENT_NAMESPACE}"
echo ""
echo "  Your overlay lives at: ${OVERLAY_DIR}"
echo "  The secret.yaml in that directory contains your API key — keep it safe."
echo ""

}

main "$@"
