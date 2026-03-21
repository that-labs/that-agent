#!/usr/bin/env bash
set -euo pipefail

# Remove stale health-probe files from a prior container restart.
rm -f /tmp/that-agent-ready /tmp/that-agent-alive

if [ "${THAT_ENTRYPOINT_VALIDATE_ONLY:-0}" = "1" ]; then
  echo "that-agent entrypoint validation mode"
  exit 0
fi

BOOT_START="$(date +%s)"

mkdir -p /home/agent/.that-agent
export PATH="/opt/buildkit/bin:${PATH}"

# ── Agent init (only on first boot with empty PVC) ──────────────────
AGENT_NAME="${THAT_AGENT_NAME:-default}"
AGENT_DIR="/home/agent/.that-agent/agents/${AGENT_NAME}"
AGENT_CONFIG="${AGENT_DIR}/config.toml"
AGENT_IDENTITY="${AGENT_DIR}/Identity.md"

if [ ! -f "${AGENT_CONFIG}" ]; then
  if [ -n "${THAT_AGENT_BOOTSTRAP_PROMPT:-}" ]; then
    that agent init "${AGENT_NAME}" --prompt "${THAT_AGENT_BOOTSTRAP_PROMPT}"
  else
    that agent init "${AGENT_NAME}"
  fi
elif [ -n "${THAT_AGENT_BOOTSTRAP_PROMPT:-}" ] && [ ! -f "${AGENT_IDENTITY}" ]; then
  # config.toml exists but soul generation failed on a previous boot (e.g. no API
  # credits). Retry generating Soul.md / Identity.md so the agent starts with
  # an established identity rather than entering self-bootstrap mode.
  echo "Identity files missing — retrying soul generation for '${AGENT_NAME}'..."
  that agent init "${AGENT_NAME}" --prompt "${THAT_AGENT_BOOTSTRAP_PROMPT}" --force
fi

# ── Telegram channel bootstrap ──────────────────────────────────────
if [ -n "${TELEGRAM_BOT_TOKEN:-}" ] && [ -n "${TELEGRAM_CHAT_ID:-}" ]; then
  if ! grep -q '\[\[channels\.adapters\]\]' "${AGENT_CONFIG}"; then
    sed -i '/^adapters = \[\]$/d' "${AGENT_CONFIG}" || true
    printf '\n%s\n' 'channels.primary = "telegram"' >> "${AGENT_CONFIG}"
    printf '\n%s\n' '[[channels.adapters]]' >> "${AGENT_CONFIG}"
    printf '%s\n' 'id = "telegram"' >> "${AGENT_CONFIG}"
    printf '%s\n' 'type = "telegram"' >> "${AGENT_CONFIG}"
    printf '%s\n' 'enabled = true' >> "${AGENT_CONFIG}"
    printf '%s\n' 'bot_token = "${TELEGRAM_BOT_TOKEN}"' >> "${AGENT_CONFIG}"
    printf '%s\n' 'chat_id = "${TELEGRAM_CHAT_ID}"' >> "${AGENT_CONFIG}"
    echo "Bootstrapped Telegram channel adapter for agent '${AGENT_NAME}'."
  fi
fi

# ── Helper: TCP connectivity check ──────────────────────────────────
can_connect_tcp() {
  local host="$1" port="$2"
  if command -v timeout >/dev/null 2>&1; then
    timeout 1 bash -lc ":</dev/tcp/${host}/${port}" >/dev/null 2>&1
  else
    bash -lc ":</dev/tcp/${host}/${port}" >/dev/null 2>&1
  fi
}

# ── Registry endpoint fixup (fast, no wait) ─────────────────────────
REGISTRY_CANONICAL="${THAT_SANDBOX_K8S_REGISTRY:-}"
REGISTRY_PUSH="${THAT_SANDBOX_K8S_REGISTRY_PUSH_ENDPOINT:-${REGISTRY_CANONICAL}}"

if [ "${THAT_K8S_REGISTRY_AUTOFIX_LOCALHOST_PORT:-1}" = "1" ] && [ -n "${REGISTRY_PUSH}" ]; then
  REG_HOST="${REGISTRY_PUSH%:*}"
  REG_PORT="${REGISTRY_PUSH##*:}"
  if [ "${REG_HOST}" != "${REG_PORT}" ] && [ "${REG_PORT}" != "5000" ] && \
     [ "${REG_HOST#*.localhost}" != "${REG_HOST}" ]; then
    if ! can_connect_tcp "${REG_HOST}" "${REG_PORT}" && can_connect_tcp "${REG_HOST}" "5000"; then
      REGISTRY_PUSH="${REG_HOST}:5000"
      echo "Adjusted registry push endpoint to ${REGISTRY_PUSH} for in-cluster routing."
    fi
  fi
fi
export THAT_K8S_REGISTRY_PUSH_ENDPOINT="${REGISTRY_PUSH}"

# =====================================================================
# Parallel infrastructure probes
#
# BuildKit wait, Docker wait, and RBAC probes are independent — run
# them concurrently so total wall-clock time equals the slowest one
# instead of the sum.
# =====================================================================

PROBE_DIR="$(mktemp -d)"
trap 'rm -rf "${PROBE_DIR}"' EXIT

# ── Probe: BuildKit (sidecar or service) ──────────────────────────────
probe_buildkit() {
  local addr="${BUILDKIT_HOST:-}"
  local wait="${THAT_BUILDKIT_WAIT_SECONDS:-20}"
  if ! command -v buildctl >/dev/null 2>&1; then
    echo "false" > "${PROBE_DIR}/buildkit"
    return
  fi
  # No BUILDKIT_HOST set — no sidecar, no service configured. Skip.
  if [ -z "${addr}" ]; then
    echo "false" > "${PROBE_DIR}/buildkit"
    return
  fi
  local i=0
  while [ "${i}" -lt "${wait}" ]; do
    if buildctl --addr "${addr}" debug workers >/dev/null 2>&1; then
      echo "true" > "${PROBE_DIR}/buildkit"
      return
    fi
    i=$((i + 1))
    sleep 1
  done
  echo "false" > "${PROBE_DIR}/buildkit"
}

# ── Probe: Docker daemon (only when DinD sidecar is configured) ─────
probe_docker() {
  # Skip entirely when no DinD sidecar — docker.io is in the image but
  # there is no daemon to talk to, so polling `docker info` for 10s is
  # pure waste.
  if [ "${THAT_DOCKER_DAEMON_SOURCE:-}" != "dind_sidecar" ]; then
    echo "false" > "${PROBE_DIR}/docker"
    return
  fi
  local wait="${THAT_DOCKER_DAEMON_WAIT_SECONDS:-10}"
  if ! command -v docker >/dev/null 2>&1; then
    echo "false" > "${PROBE_DIR}/docker"
    return
  fi
  local i=0
  while [ "${i}" -lt "${wait}" ]; do
    if docker info >/dev/null 2>&1; then
      echo "true" > "${PROBE_DIR}/docker"
      return
    fi
    i=$((i + 1))
    sleep 1
  done
  echo "false" > "${PROBE_DIR}/docker"
}

# ── Probe: RBAC (single `kubectl auth can-i --list` call) ───────────
probe_rbac() {
  local ns="${THAT_SANDBOX_K8S_NAMESPACE:-${POD_NAMESPACE:-}}"
  local cache="/home/agent/.that-agent/.rbac-cache"
  local cache_key="sa=${POD_NAMESPACE:-unknown}/${THAT_K8S_SERVICE_ACCOUNT:-that-agent},ns=${ns:-unknown}"

  # Reuse cached result if the ServiceAccount + namespace haven't changed
  if [ -f "${cache}" ] && head -1 "${cache}" | grep -qF "${cache_key}"; then
    cat "${cache}" > "${PROBE_DIR}/rbac"
    return
  fi

  # Defaults
  local ns_read=false ns_write=false cl_read=false cl_write=false

  if command -v kubectl >/dev/null 2>&1; then
    if [ -n "${ns}" ]; then
      # One call gets all namespace-scoped permissions
      local ns_perms
      ns_perms="$(kubectl auth can-i --list -n "${ns}" 2>/dev/null)" || true
      if echo "${ns_perms}" | grep -qE '^\*\s|pods.*\[.*list'; then
        ns_read=true
      fi
      if echo "${ns_perms}" | grep -qE '^\*\s|deployments\.apps.*\[.*create'; then
        ns_write=true
      fi
    fi
    # Cluster-scoped permissions — single call
    local cl_perms
    cl_perms="$(kubectl auth can-i --list 2>/dev/null)" || true
    if echo "${cl_perms}" | grep -qE '^\*\s|clusterroles.*\[.*list'; then
      cl_read=true
    fi
    if echo "${cl_perms}" | grep -qE '^\*\s|clusterroles.*\[.*create'; then
      cl_write=true
    fi
  fi

  {
    echo "${cache_key}"
    echo "THAT_RBAC_NAMESPACE_READ=${ns_read}"
    echo "THAT_RBAC_NAMESPACE_WRITE=${ns_write}"
    echo "THAT_RBAC_CLUSTER_READ=${cl_read}"
    echo "THAT_RBAC_CLUSTER_WRITE=${cl_write}"
  } | tee "${cache}" > "${PROBE_DIR}/rbac"
}

# Launch all probes in parallel
probe_buildkit &
pid_bk=$!
probe_docker &
pid_dk=$!
probe_rbac &
pid_rbac=$!

wait "${pid_bk}" "${pid_dk}" "${pid_rbac}" 2>/dev/null || true

# ── Collect results ─────────────────────────────────────────────────

# RBAC
RBAC_NAMESPACE="${THAT_SANDBOX_K8S_NAMESPACE:-${POD_NAMESPACE:-}}"
export THAT_RBAC_SCOPE="namespace:${RBAC_NAMESPACE:-unknown}"
export THAT_RBAC_SUBJECT="system:serviceaccount:${POD_NAMESPACE:-unknown}:${THAT_K8S_SERVICE_ACCOUNT:-that-agent}"
export THAT_RBAC_NAMESPACE_READ=false
export THAT_RBAC_NAMESPACE_WRITE=false
export THAT_RBAC_CLUSTER_READ=false
export THAT_RBAC_CLUSTER_WRITE=false
if [ -f "${PROBE_DIR}/rbac" ]; then
  while IFS='=' read -r key val; do
    case "${key}" in
      THAT_RBAC_*) export "${key}=${val}" ;;
    esac
  done < "${PROBE_DIR}/rbac"
fi

# BuildKit
BUILDKIT_AVAILABLE="$(cat "${PROBE_DIR}/buildkit" 2>/dev/null || echo false)"
export THAT_BUILDKIT_AVAILABLE="${BUILDKIT_AVAILABLE}"
if [ "${BUILDKIT_AVAILABLE}" = "true" ]; then
  export THAT_BUILDKIT_SOURCE="${THAT_BUILDKIT_SOURCE:-buildkit_sidecar}"
fi

# Docker
DOCKER_AVAILABLE="$(cat "${PROBE_DIR}/docker" 2>/dev/null || echo false)"
export THAT_DOCKER_DAEMON_AVAILABLE="${DOCKER_AVAILABLE}"
if [ "${DOCKER_AVAILABLE}" = "true" ]; then
  export THAT_DOCKER_DAEMON_SOURCE="${THAT_DOCKER_DAEMON_SOURCE:-host_or_injected}"
fi

# ── Select image-build backend ──────────────────────────────────────
PREFERRED_BACKEND="$(echo "${THAT_IMAGE_BUILD_BACKEND_PREFERRED:-buildkit}" | tr '[:upper:]' '[:lower:]')"
case "${PREFERRED_BACKEND}" in
  docker)
    if [ "${DOCKER_AVAILABLE}" = "true" ]; then
      export THAT_IMAGE_BUILD_BACKEND="docker"
    elif [ "${BUILDKIT_AVAILABLE}" = "true" ]; then
      export THAT_IMAGE_BUILD_BACKEND="buildkit"
    else
      export THAT_IMAGE_BUILD_BACKEND="none"
    fi
    ;;
  *)
    if [ "${BUILDKIT_AVAILABLE}" = "true" ]; then
      export THAT_IMAGE_BUILD_BACKEND="buildkit"
    elif [ "${DOCKER_AVAILABLE}" = "true" ]; then
      export THAT_IMAGE_BUILD_BACKEND="docker"
    else
      export THAT_IMAGE_BUILD_BACKEND="none"
    fi
    ;;
esac

BOOT_END="$(date +%s)"
echo "Bootstrap completed in $((BOOT_END - BOOT_START))s [buildkit=${BUILDKIT_AVAILABLE} docker=${DOCKER_AVAILABLE} backend=${THAT_IMAGE_BUILD_BACKEND}]"

exec that run listen --agent "${AGENT_NAME}" --no-sandbox
