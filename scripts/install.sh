#!/usr/bin/env bash
# that-agent installer — cross-platform Kubernetes setup
#
# One-liner usage:
#   curl -fsSL https://raw.githubusercontent.com/that-labs/that-agent/main/scripts/install.sh | bash
#
# Or download and run with flags:
#   bash install.sh [--image <image:tag>] [--namespace <ns>] [--no-cilium] [--no-tailscale] [--no-k9s] [--no-subagents] [--cluster-admin] [--k3d] [--k3s]
#
# What this script does:
#   1. Detects platform and installs Kubernetes (k3s on Linux, k3d on macOS — or override with --k3s/--k3d)
#   2. Installs Helm CLI
#   3. Prompts for agent name, description, and LLM API credentials
#   4. Optionally configures a Telegram channel
#   5. Deploys via Helm chart from OCI registry
#
# Platform strategy:
#   Linux VPS/server → k3s (single binary, containerd, no Docker needed)
#   macOS            → k3d (runs k3s inside Docker — requires Docker Desktop)
#   Linux desktop    → k3s by default, --k3d to use Docker instead

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
AGENT_IMAGE="${AGENT_IMAGE:-ghcr.io/that-labs/that-agent:latest}"
AGENT_NAMESPACE="${AGENT_NAMESPACE:-}"   # derived from agent name if not set
AGENT_NAME="${THAT_AGENT_NAME:-}"        # non-interactive: set via env
AGENT_DESCRIPTION="${THAT_AGENT_DESCRIPTION:-}"
INSTALL_CILIUM="${INSTALL_CILIUM:-true}"
INSTALL_TAILSCALE_OPERATOR="${INSTALL_TAILSCALE_OPERATOR:-true}"
INSTALL_K9S="${INSTALL_K9S:-true}"
ENABLE_SUBAGENTS="${ENABLE_SUBAGENTS:-true}"
CLUSTER_ADMIN="${CLUSTER_ADMIN:-false}"
FORCE_K3S="${FORCE_K3S:-false}"
FORCE_K3D="${FORCE_K3D:-false}"
NON_INTERACTIVE="${CI:-false}"           # auto-detect CI or set --ci
KUBECTL="kubectl"
K3D_CLUSTER_NAME="that-agent"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-/dev/null}")" 2>/dev/null && pwd)" || SCRIPT_DIR=""
REPO_ROOT=""
if [[ -n "${SCRIPT_DIR}" && -f "${SCRIPT_DIR}/../build.sh" ]]; then
  REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
fi
VALUES_DIR=""              # set after we know the agent name

# Helm chart OCI reference
HELM_CHART_OCI="oci://ghcr.io/that-labs/helm/that-agent"

# In-cluster registry (k3s path)
REGISTRY_NODEPORT=30500
REGISTRY_NAMESPACE="that-registry"
# k3d uses its own registry; these are set in install_k3d/install_registry
REGISTRY_PULL_HOST="registry.localhost:5000"
REGISTRY_PUSH_ENDPOINT="registry.${REGISTRY_NAMESPACE}.svc.cluster.local:5000"
K3D_REGISTRY_NAME="that-registry"
K3D_REGISTRY_PORT=5050  # 5000 is taken by macOS AirPlay Receiver

# Resolved during cluster setup
CLUSTER_TOOL=""  # "k3s" or "k3d"

# ── Argument parsing ─────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)      AGENT_IMAGE="$2";    shift 2 ;;
    --namespace)  AGENT_NAMESPACE="$2"; shift 2 ;;
    --k3s)        FORCE_K3S=true;      shift   ;;
    --k3d)        FORCE_K3D=true;      shift   ;;
    --no-cilium)  INSTALL_CILIUM=false; shift  ;;
    --no-tailscale) INSTALL_TAILSCALE_OPERATOR=false; shift ;;
    --no-k9s)     INSTALL_K9S=false;   shift   ;;
    --no-subagents) ENABLE_SUBAGENTS=false; shift ;;
    --cluster-admin) CLUSTER_ADMIN=true; shift ;;
    --ci) NON_INTERACTIVE=true; shift ;;
    --help|-h)
      echo "Usage: $0 [--image IMAGE:TAG] [--namespace NS] [--k3s] [--k3d] [--no-cilium] [--no-tailscale] [--no-k9s] [--no-subagents] [--cluster-admin] [--ci]"
      exit 0
      ;;
    *) die "Unknown option: $1" ;;
  esac
done

if [[ "${FORCE_K3S}" == "true" && "${FORCE_K3D}" == "true" ]]; then
  die "Cannot use both --k3s and --k3d."
fi

# ── Platform detection ────────────────────────────────────────────────────────
PLATFORM="$(uname -s)"
case "${PLATFORM}" in
  Linux|Darwin) ;;
  *) die "Unsupported platform: ${PLATFORM}. This installer supports Linux and macOS." ;;
esac

# Decide cluster tool
if [[ "${FORCE_K3S}" == "true" ]]; then
  CLUSTER_TOOL="k3s"
elif [[ "${FORCE_K3D}" == "true" ]]; then
  CLUSTER_TOOL="k3d"
elif [[ "${PLATFORM}" == "Darwin" ]]; then
  CLUSTER_TOOL="k3d"
else
  # Linux — default to k3s (lean, no Docker dependency)
  CLUSTER_TOOL="k3s"
fi

info "Platform: ${PLATFORM}, cluster tool: ${CLUSTER_TOOL}"

# ── Require root/sudo (k3s on Linux) or Docker (k3d) ─────────────────────────
SUDO=""
if [[ "${CLUSTER_TOOL}" == "k3s" ]]; then
  if [[ $EUID -ne 0 ]]; then
    SUDO="sudo"
    if ! command -v sudo &>/dev/null; then
      die "Run as root or install sudo."
    fi
  fi
elif [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
  if ! command -v docker &>/dev/null; then
    die "k3d requires Docker. Install Docker Desktop (macOS) or Docker Engine (Linux) first."
  fi
  if ! docker info &>/dev/null 2>&1; then
    die "Docker is installed but not running. Start Docker and try again."
  fi
fi

# ── Step 1: Kubernetes cluster ───────────────────────────────────────────────
header "Step 1 — Kubernetes (${CLUSTER_TOOL})"

if command -v kubectl &>/dev/null && kubectl get nodes &>/dev/null 2>&1; then
  ok "Existing cluster reachable — skipping cluster install."
  # Detect which tool manages this cluster
  if command -v k3s &>/dev/null && k3s kubectl get nodes &>/dev/null 2>&1; then
    CLUSTER_TOOL="k3s"
    KUBECTL="k3s kubectl"
  else
    KUBECTL="kubectl"
  fi
  # For k3d: ensure registry exists and set endpoints
  if [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
    if ! docker ps --filter "name=^that-registry$" --format '{{.Names}}' | grep -q "that-registry"; then
      info "k3d registry not found — creating…"
      k3d registry create that-registry --port "0.0.0.0:${K3D_REGISTRY_PORT}" 2>/dev/null || true
      # Connect registry to the k3d cluster network
      K3D_NETWORK="k3d-${K3D_CLUSTER_NAME}"
      docker network connect "${K3D_NETWORK}" "that-registry" 2>/dev/null || true
      ok "Registry created at localhost:${K3D_REGISTRY_PORT}"
    fi
    REGISTRY_PULL_HOST="${K3D_REGISTRY_NAME}:${K3D_REGISTRY_PORT}"
    REGISTRY_PUSH_ENDPOINT="${K3D_REGISTRY_NAME}:${K3D_REGISTRY_PORT}"
  fi
elif [[ "${CLUSTER_TOOL}" == "k3s" ]]; then
  install_k3s
elif [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
  install_k3d
else
  die "No running Kubernetes cluster found."
fi

# Ensure local-path is the default StorageClass
info "Ensuring local-path is the default StorageClass…"
${KUBECTL} patch storageclass local-path \
  -p '{"metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}}}' \
  2>/dev/null && ok "local-path marked as default StorageClass." \
  || warn "Could not patch local-path StorageClass — PVCs may need an explicit storageClassName."

# ── Step 2: Helm CLI ─────────────────────────────────────────────────────────
header "Step 2 — Helm"
install_helm

# ── Step 3: Cilium CNI ────────────────────────────────────────────────────────
if [[ "${INSTALL_CILIUM}" == "true" ]]; then
  header "Step 3 — Cilium CNI"
  install_cilium
else
  info "Skipping Cilium CNI (--no-cilium)."
fi

# ── Step 4: Tailscale Operator ────────────────────────────────────────────────
if [[ "${INSTALL_TAILSCALE_OPERATOR}" == "true" ]]; then
  header "Step 4 — Tailscale Operator"
  install_tailscale
else
  info "Skipping Tailscale Operator (--no-tailscale)."
fi

# ── Step 5: K9s ──────────────────────────────────────────────────────────────
if [[ "${INSTALL_K9S}" == "true" ]]; then
  header "Step 5 — K9s"
  install_k9s
else
  info "Skipping K9s (--no-k9s)."
fi

# ── Step 6: In-cluster image registry ───────────────────────────────────────
header "Step 6 — In-cluster image registry"
install_registry

# ── Step 7: Gather configuration ────────────────────────────────────────────
header "Step 7 — Agent configuration"
gather_config

# ── Step 8: Build or pull image ──────────────────────────────────────────────
header "Step 8 — Container image"
resolve_image

# ── Step 9: Generate Helm values & deploy ─────────────────────────────────────
header "Step 9 — Deploying via Helm"
generate_values
deploy

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
echo "    # Upgrade the agent"
echo "    helm upgrade that-agent ${HELM_CHART_OCI} -n ${AGENT_NAMESPACE} -f ${VALUES_DIR}/values.yaml"
echo ""
echo "    # Remove the agent entirely"
echo "    helm uninstall that-agent -n ${AGENT_NAMESPACE} && ${KUBECTL} delete namespace ${AGENT_NAMESPACE}"
if [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
  echo ""
  echo "    # Delete the entire k3d cluster"
  echo "    k3d cluster delete ${K3D_CLUSTER_NAME}"
fi
echo ""
echo "  Your values file: ${VALUES_DIR}/values.yaml"
echo ""
}

# ═══════════════════════════════════════════════════════════════════════════════
# Functions
# ═══════════════════════════════════════════════════════════════════════════════

install_helm() {
  if command -v helm &>/dev/null; then
    ok "Helm already installed: $(helm version --short 2>/dev/null)"
    return
  fi
  info "Installing Helm…"
  curl -fsSL https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash
  ok "Helm installed: $(helm version --short 2>/dev/null)"
}

install_k3s() {
  info "Installing k3s…"
  K3S_ARGS="--disable traefik --write-kubeconfig-mode 644"
  if [[ "${INSTALL_CILIUM}" == "true" ]]; then
    K3S_ARGS="${K3S_ARGS} --flannel-backend=none --disable-network-policy"
  fi
  curl -sfL https://get.k3s.io | $SUDO sh -s - ${K3S_ARGS}

  info "Waiting for k3s node to be ready…"
  local retries=60
  while ! k3s kubectl get nodes &>/dev/null 2>&1; do
    retries=$((retries - 1))
    [[ $retries -le 0 ]] && die "k3s did not become ready in time. Check: journalctl -u k3s"
    sleep 1
  done
  ok "k3s is ready."

  # Make kubeconfig available at ~/.kube/config
  mkdir -p "${HOME}/.kube"
  $SUDO cp /etc/rancher/k3s/k3s.yaml "${HOME}/.kube/config"
  $SUDO chown "$(id -u):$(id -g)" "${HOME}/.kube/config"
  export KUBECONFIG="${HOME}/.kube/config"
  KUBECTL="kubectl"
  ok "Kubeconfig written to ${HOME}/.kube/config"
}

install_k3d() {
  # Install k3d binary if needed
  if ! command -v k3d &>/dev/null; then
    info "Installing k3d…"
    curl -s https://raw.githubusercontent.com/k3d-io/k3d/main/install.sh | bash
    ok "k3d installed."
  else
    ok "k3d already installed."
  fi

  # Check if cluster already exists
  if k3d cluster list 2>/dev/null | grep -q "^${K3D_CLUSTER_NAME} "; then
    ok "k3d cluster '${K3D_CLUSTER_NAME}' already exists."
    k3d kubeconfig merge "${K3D_CLUSTER_NAME}" --kubeconfig-merge-default --kubeconfig-switch-context 2>/dev/null
  else
    info "Creating k3d cluster '${K3D_CLUSTER_NAME}' with local registry…"
    K3D_ARGS=(
      cluster create "${K3D_CLUSTER_NAME}"
      --registry-create "${K3D_REGISTRY_NAME}:0.0.0.0:${K3D_REGISTRY_PORT}"
      --k3s-arg "--disable=traefik@server:0"
    )
    if [[ "${INSTALL_CILIUM}" == "true" ]]; then
      K3D_ARGS+=(
        --k3s-arg "--flannel-backend=none@server:0"
        --k3s-arg "--disable-network-policy@server:0"
      )
    fi
    k3d "${K3D_ARGS[@]}"
    ok "k3d cluster '${K3D_CLUSTER_NAME}' created with registry at localhost:${K3D_REGISTRY_PORT}."
  fi

  # Set registry endpoints for k3d — the native registry is auto-configured in all k3s nodes
  REGISTRY_PULL_HOST="${K3D_REGISTRY_NAME}:${K3D_REGISTRY_PORT}"
  REGISTRY_PUSH_ENDPOINT="${K3D_REGISTRY_NAME}:${K3D_REGISTRY_PORT}"

  KUBECTL="kubectl"
  local retries=30
  while ! ${KUBECTL} get nodes &>/dev/null 2>&1; do
    retries=$((retries - 1))
    [[ $retries -le 0 ]] && die "k3d cluster not reachable. Check: k3d cluster list"
    sleep 1
  done
  ok "Cluster is reachable."
}

install_cilium() {
  if command -v cilium &>/dev/null; then
    ok "Cilium CLI already installed."
  else
    info "Installing Cilium CLI…"
    CILIUM_CLI_VERSION=$(curl -s https://raw.githubusercontent.com/cilium/cilium-cli/main/stable.txt)
    CLI_ARCH="amd64"
    case "$(uname -m)" in
      aarch64|arm64) CLI_ARCH="arm64" ;;
    esac
    case "${PLATFORM}" in
      Linux)  CLI_OS="linux"  ;;
      Darwin) CLI_OS="darwin" ;;
    esac
    curl -L --fail --remote-name-all \
      "https://github.com/cilium/cilium-cli/releases/download/${CILIUM_CLI_VERSION}/cilium-${CLI_OS}-${CLI_ARCH}.tar.gz" \
      "https://github.com/cilium/cilium-cli/releases/download/${CILIUM_CLI_VERSION}/cilium-${CLI_OS}-${CLI_ARCH}.tar.gz.sha256sum"
    if command -v sha256sum &>/dev/null; then
      sha256sum --check "cilium-${CLI_OS}-${CLI_ARCH}.tar.gz.sha256sum"
    elif command -v shasum &>/dev/null; then
      shasum -a 256 -c "cilium-${CLI_OS}-${CLI_ARCH}.tar.gz.sha256sum"
    fi
    ${SUDO:-} tar xzvfC "cilium-${CLI_OS}-${CLI_ARCH}.tar.gz" /usr/local/bin
    rm -f "cilium-${CLI_OS}-${CLI_ARCH}.tar.gz" "cilium-${CLI_OS}-${CLI_ARCH}.tar.gz.sha256sum"
    ok "Cilium CLI installed."
  fi

  if cilium status --wait --wait-duration=5s &>/dev/null; then
    ok "Cilium is already running in the cluster."
  else
    info "Installing Cilium into the cluster…"
    cilium install --wait
    ok "Cilium CNI is ready."
  fi
}

install_tailscale() {
  echo ""
  echo "  The Tailscale Operator needs OAuth client credentials."
  echo "  Create them at: https://login.tailscale.com/admin/settings/oauth"
  echo "  (Scopes needed: devices, auth_keys)"
  echo ""
  TS_CLIENT_ID="${TS_OAUTH_CLIENT_ID:-}"
  TS_CLIENT_SECRET="${TS_OAUTH_CLIENT_SECRET:-}"
  if [[ -z "${TS_CLIENT_ID}" ]]; then
    read -rp "  Tailscale OAuth client ID: " TS_CLIENT_ID < /dev/tty
  fi
  if [[ -z "${TS_CLIENT_SECRET}" ]]; then
    read -rsp "  Tailscale OAuth client secret: " TS_CLIENT_SECRET < /dev/tty
    echo ""
  fi

  if [[ -z "${TS_CLIENT_ID}" || -z "${TS_CLIENT_SECRET}" ]]; then
    warn "Missing Tailscale OAuth credentials — skipping operator install."
  else
    echo ""
    echo "  Tailnet name (e.g. 'myteam' from myteam.ts.net)."
    echo "  Used so the agent can construct mesh URLs directly without discovery."
    echo ""
    TS_TAILNET_NAME="${TS_TAILNET_NAME:-}"
    if [[ -z "${TS_TAILNET_NAME}" ]]; then
      read -rp "  Tailnet name (optional, press Enter to skip): " TS_TAILNET_NAME < /dev/tty
    fi

    helm repo add tailscale https://pkgs.tailscale.com/helmcharts 2>/dev/null || true
    helm repo update tailscale
    helm upgrade --install tailscale-operator tailscale/tailscale-operator \
      --namespace=tailscale \
      --create-namespace \
      --set-string oauth.clientId="${TS_CLIENT_ID}" \
      --set-string oauth.clientSecret="${TS_CLIENT_SECRET}" \
      --wait
    ok "Tailscale Operator installed."
  fi
}

install_k9s() {
  if command -v k9s &>/dev/null; then
    ok "K9s already installed."
    return
  fi

  info "Installing K9s…"
  K9S_ARCH="amd64"
  case "$(uname -m)" in
    aarch64|arm64) K9S_ARCH="arm64" ;;
  esac
  case "${PLATFORM}" in
    Linux)  K9S_OS="Linux"  ;;
    Darwin) K9S_OS="Darwin" ;;
  esac
  K9S_VERSION=$(curl -s https://api.github.com/repos/derailed/k9s/releases/latest | grep '"tag_name"' | cut -d'"' -f4)
  curl -L --fail -o /tmp/k9s.tar.gz \
    "https://github.com/derailed/k9s/releases/download/${K9S_VERSION}/k9s_${K9S_OS}_${K9S_ARCH}.tar.gz"
  ${SUDO:-} tar xzf /tmp/k9s.tar.gz -C /usr/local/bin k9s
  rm -f /tmp/k9s.tar.gz
  ok "K9s ${K9S_VERSION} installed."
}

install_registry() {
  # k3d already has a native registry created during cluster setup
  if [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
    ok "Using k3d native registry at localhost:${K3D_REGISTRY_PORT} (in-cluster: ${REGISTRY_PULL_HOST})"
    echo ""
    echo "  Push images from your host:"
    echo "    docker tag myimage:latest localhost:${K3D_REGISTRY_PORT}/myimage:latest"
    echo "    docker push localhost:${K3D_REGISTRY_PORT}/myimage:latest"
    echo ""
    echo "  In-cluster pull address: ${REGISTRY_PULL_HOST}/myimage:latest"
    echo ""
    return
  fi

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

  # k3s needs a registries.yaml to resolve the pull hostname
  if [[ "${CLUSTER_TOOL}" == "k3s" ]]; then
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
    local retries=60
    while ! ${KUBECTL} get nodes &>/dev/null 2>&1; do
      retries=$((retries - 1))
      [[ $retries -le 0 ]] && die "k3s did not recover after restart. Check: journalctl -u k3s"
      sleep 1
    done
  fi

  info "Waiting for registry pod to be ready…"
  ${KUBECTL} -n "${REGISTRY_NAMESPACE}" rollout status deploy/registry --timeout=120s
  ok "Registry ready — pull host: ${REGISTRY_PULL_HOST}, push endpoint: ${REGISTRY_PUSH_ENDPOINT}"
}

gather_config() {
  # Detect provider from key prefix
  detect_provider() {
    local key="$1"
    if   [[ "${key}" == sk-ant-oat* ]];   then echo "anthropic"   # Claude Code OAuth
    elif [[ "${key}" == sk-ant-* ]];        then echo "anthropic"
    elif [[ "${key}" == sk-or-* ]];         then echo "openrouter"
    elif [[ "${key}" == sk-* ]];            then echo "openai"
    else echo ""
    fi
  }

  # ── Non-interactive mode (CI / --ci) ──────────────────────────────────────
  if [[ "${NON_INTERACTIVE}" == "true" ]]; then
    AGENT_NAME="${AGENT_NAME:-ci-test-agent}"
    AGENT_DESCRIPTION="${AGENT_DESCRIPTION:-CI test agent}"
    LLM_API_KEY="${ANTHROPIC_API_KEY:-${CLAUDE_CODE_OAUTH_TOKEN:-${OPENAI_API_KEY:-${OPENROUTER_API_KEY:-}}}}"
    [[ -z "${LLM_API_KEY}" ]] && die "Non-interactive mode requires an API key in the environment."
    LLM_PROVIDER="$(detect_provider "${LLM_API_KEY}")"
    [[ -z "${LLM_PROVIDER}" ]] && die "Could not detect provider from key prefix."
    case "${LLM_PROVIDER}" in
      anthropic)  DEFAULT_MODEL="claude-sonnet-4-6" ;;
      openai)     DEFAULT_MODEL="gpt-5.2-codex"     ;;
      openrouter) DEFAULT_MODEL=""                   ;;
    esac
    AGENT_MODEL="${AGENT_MODEL:-${DEFAULT_MODEL}}"
    TELEGRAM_BOT_TOKEN=""
    TELEGRAM_CHAT_ID=""
    [[ -z "${AGENT_NAMESPACE}" ]] && AGENT_NAMESPACE="that-${AGENT_NAME}"
    VALUES_DIR="${HOME}/.that-agent-install/${AGENT_NAME}"
    ok "Non-interactive mode — using defaults."
    echo "  Agent: ${AGENT_NAME} | Provider: ${LLM_PROVIDER} | Namespace: ${AGENT_NAMESPACE}"
    return
  fi

  # ── Interactive mode ──────────────────────────────────────────────────────
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
  LLM_API_KEY="${ANTHROPIC_API_KEY:-${CLAUDE_CODE_OAUTH_TOKEN:-${CLAUDE_CODE_AUTH_TOKEN:-${CLAUDE_CODE_AUTH:-${OPENAI_API_KEY:-${OPENROUTER_API_KEY:-}}}}}}"
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

  # Values output directory
  VALUES_DIR="${HOME}/.that-agent-install/${AGENT_NAME}"

  echo ""
  ok "Configuration collected."
  echo ""
  echo "  Agent name:   ${AGENT_NAME}"
  echo "  Namespace:    ${AGENT_NAMESPACE}"
  echo "  Cluster:      ${CLUSTER_TOOL}"
  echo "  Provider:     ${LLM_PROVIDER}"
  echo "  Model:        ${AGENT_MODEL:-<provider default>}"
  echo "  Telegram:     ${TELEGRAM_BOT_TOKEN:+configured}${TELEGRAM_BOT_TOKEN:-not configured}"
  echo "  Image:        ${AGENT_IMAGE}"
  echo "  Values dir:   ${VALUES_DIR}"
  echo ""
  if [[ "${NON_INTERACTIVE}" != "true" ]]; then
    read -rp "  Proceed with deployment? [Y/n]: " CONFIRM < /dev/tty
    case "${CONFIRM:-y}" in
      [Yy]*) ;;
      *) info "Aborted."; exit 0 ;;
    esac
  fi
}

resolve_image() {
  if [[ "${NON_INTERACTIVE}" == "true" ]]; then
    info "Using image: ${AGENT_IMAGE}"
    return
  fi
  if [[ -n "${REPO_ROOT}" && -f "${REPO_ROOT}/build.sh" ]] && command -v docker &>/dev/null; then
    echo ""
    read -rp "  Local repo detected. Build image from source? [y/N]: " BUILD_LOCAL < /dev/tty
    case "${BUILD_LOCAL:-n}" in
      [Yy]*)
        info "Building that-agent image from source…"
        bash "${REPO_ROOT}/build.sh"
        BUILT_IMAGE="that-agent:latest"
        if [[ "${CLUSTER_TOOL}" == "k3s" ]] && command -v k3s &>/dev/null; then
          info "Importing image into k3s containerd…"
          docker save "${BUILT_IMAGE}" | $SUDO k3s ctr images import -
          ok "Image imported: ${BUILT_IMAGE}"
        elif [[ "${CLUSTER_TOOL}" == "k3d" ]]; then
          info "Pushing image to k3d registry…"
          docker tag "${BUILT_IMAGE}" "localhost:${K3D_REGISTRY_PORT}/${BUILT_IMAGE}"
          docker push "localhost:${K3D_REGISTRY_PORT}/${BUILT_IMAGE}"
          BUILT_IMAGE="${REGISTRY_PULL_HOST}/${BUILT_IMAGE}"
          ok "Image pushed: ${BUILT_IMAGE}"
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
}

generate_values() {
  mkdir -p "${VALUES_DIR}"

  # Determine access level
  local access_level="namespace-admin"
  if [[ "${CLUSTER_ADMIN}" == "true" ]]; then
    access_level="cluster-admin"
  elif [[ "${ENABLE_SUBAGENTS}" == "false" ]]; then
    access_level="readonly"
  fi

  # Determine image repo and tag
  local image_repo="${AGENT_IMAGE%:*}"
  local image_tag="${AGENT_IMAGE##*:}"

  cat > "${VALUES_DIR}/values.yaml" <<EOF
agent:
  name: "${AGENT_NAME}"
  image:
    repository: "${image_repo}"
    tag: "${image_tag}"
    pullPolicy: Always
  provider: "${LLM_PROVIDER}"
  model: "${AGENT_MODEL}"
  maxTurns: 75
  bootstrapPrompt: "${AGENT_DESCRIPTION}"

accessLevel: "${access_level}"

secrets:
  existingSecret: that-agent-secrets

gitServer:
  enabled: true

buildkit:
  enabled: true

cacheProxy:
  enabled: true
EOF

  # Create the K8s secret with API credentials
  local secret_args=()

  # Map API key to the right env var
  if [[ "${LLM_API_KEY}" == sk-ant-oat* ]]; then
    secret_args+=(--from-literal="CLAUDE_CODE_OAUTH_TOKEN=${LLM_API_KEY}")
  else
    case "${LLM_PROVIDER}" in
      anthropic)   secret_args+=(--from-literal="ANTHROPIC_API_KEY=${LLM_API_KEY}") ;;
      openai)      secret_args+=(--from-literal="OPENAI_API_KEY=${LLM_API_KEY}") ;;
      openrouter)  secret_args+=(--from-literal="OPENROUTER_API_KEY=${LLM_API_KEY}") ;;
    esac
  fi

  if [[ -n "${TELEGRAM_BOT_TOKEN}" ]]; then
    secret_args+=(--from-literal="TELEGRAM_BOT_TOKEN=${TELEGRAM_BOT_TOKEN}")
    secret_args+=(--from-literal="TELEGRAM_CHAT_ID=${TELEGRAM_CHAT_ID}")
  fi

  info "Creating namespace and secret…"
  ${KUBECTL} create namespace "${AGENT_NAMESPACE}" 2>/dev/null || true
  ${KUBECTL} -n "${AGENT_NAMESPACE}" create secret generic that-agent-secrets \
    "${secret_args[@]}" \
    --dry-run=client -o yaml | ${KUBECTL} apply -f -

  ok "Values written to ${VALUES_DIR}/values.yaml"
}

deploy() {
  # Try OCI chart first, fall back to local chart if available
  local chart_ref="${HELM_CHART_OCI}"
  if [[ -n "${REPO_ROOT}" && -f "${REPO_ROOT}/deploy/helm/that-agent/Chart.yaml" ]]; then
    chart_ref="${REPO_ROOT}/deploy/helm/that-agent"
    info "Using local Helm chart from repo."
  fi

  info "Running helm upgrade --install…"
  helm upgrade --install that-agent "${chart_ref}" \
    --namespace "${AGENT_NAMESPACE}" \
    --create-namespace \
    -f "${VALUES_DIR}/values.yaml" \
    --wait --timeout 120s || {
      warn "Helm install did not complete in 120s. Check pod status:"
      echo "  ${KUBECTL} -n ${AGENT_NAMESPACE} get pods"
      echo "  ${KUBECTL} -n ${AGENT_NAMESPACE} logs deploy/that-agent"
    }
}

main "$@"
