#!/usr/bin/env bash

# Build and push the that-agent runtime image to a registry.
# Usage:
#   ./scripts/build-and-push.sh [version] [registry] [image_repo] [compat_repo]
# Example:
#   ./scripts/build-and-push.sh v0.1.0 k3d-myregistry.localhost:12345 that-agent that-agent-sandbox

set -euo pipefail

VERSION="${1:-latest}"
REGISTRY="${2:-registry.local:5000}"
IMAGE_REPO="${3:-that-agent}"
COMPAT_REPO="${4:-that-agent-sandbox}"
PUSH_LATEST="${PUSH_LATEST:-0}"
BUILDX_BUILDER="${THAT_BUILDX_BUILDER:-that-builder}"
BUILDX_CREATE_DRIVER="${THAT_BUILDX_DRIVER:-docker-container}"
BUILDX_DRIVER_NETWORK="${THAT_BUILDX_DRIVER_NETWORK:-host}"
BUILDX_AUTO_BOOTSTRAP="${THAT_BUILDX_AUTO_BOOTSTRAP:-1}"
BUILDX_BOOTSTRAP_TIMEOUT="${THAT_BUILDX_BOOTSTRAP_TIMEOUT:-90}"
REGISTRY_INSECURE_MODE="${THAT_REGISTRY_INSECURE:-auto}"
RUNTIME_PROFILE="${THAT_RUNTIME_PROFILE:-slim}"
CARGO_BUILD_JOBS="${THAT_CARGO_BUILD_JOBS:-0}"
CARGO_RELEASE_LTO="${THAT_CARGO_RELEASE_LTO:-thin}"
CARGO_RELEASE_CODEGEN_UNITS="${THAT_CARGO_RELEASE_CODEGEN_UNITS:-16}"
CARGO_RELEASE_OPT_LEVEL="${THAT_CARGO_RELEASE_OPT_LEVEL:-2}"
CARGO_RELEASE_DEBUG="${THAT_CARGO_RELEASE_DEBUG:-0}"
RUST_LINKER="${THAT_RUST_LINKER:-mold}"
PLATFORMS="${THAT_BUILDX_PLATFORMS:-linux/amd64}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
CACHE_DIR="${THAT_SANDBOX_BUILD_CACHE_DIR:-$PROJECT_DIR/.cache/that-sandbox-buildx}"

APP_IMAGE="${REGISTRY}/${IMAGE_REPO}:${VERSION}"
COMPAT_IMAGE="${REGISTRY}/${COMPAT_REPO}:${VERSION}"

APP_LATEST_IMAGE="${REGISTRY}/${IMAGE_REPO}:latest"
COMPAT_LATEST_IMAGE="${REGISTRY}/${COMPAT_REPO}:latest"
REGISTRY_PUSH_ENDPOINT="${THAT_REGISTRY_PUSH_ENDPOINT:-$REGISTRY}"

is_local_registry_host() {
  case "$1" in
    localhost:*|127.0.0.1:*|*.localhost:*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

REGISTRY_INSECURE=0
if [ "$REGISTRY_INSECURE_MODE" = "1" ] || [ "$REGISTRY_INSECURE_MODE" = "true" ]; then
  REGISTRY_INSECURE=1
elif [ "$REGISTRY_INSECURE_MODE" = "auto" ] && is_local_registry_host "$REGISTRY"; then
  REGISTRY_INSECURE=1
fi

if [ "$REGISTRY_INSECURE" = "1" ] && is_local_registry_host "$REGISTRY"; then
  REGISTRY_PORT="${REGISTRY##*:}"
  if [[ "$REGISTRY_PORT" =~ ^[0-9]+$ ]] && [ -z "${THAT_REGISTRY_PUSH_ENDPOINT:-}" ]; then
    # BuildKit container may not resolve *.localhost; use host-network loopback.
    REGISTRY_PUSH_ENDPOINT="127.0.0.1:${REGISTRY_PORT}"
  fi
fi

APP_IMAGE="${REGISTRY_PUSH_ENDPOINT}/${IMAGE_REPO}:${VERSION}"
COMPAT_IMAGE="${REGISTRY_PUSH_ENDPOINT}/${COMPAT_REPO}:${VERSION}"
APP_LATEST_IMAGE="${REGISTRY_PUSH_ENDPOINT}/${IMAGE_REPO}:latest"
COMPAT_LATEST_IMAGE="${REGISTRY_PUSH_ENDPOINT}/${COMPAT_REPO}:latest"

echo "=========================================="
echo "that-agent Build and Push"
echo "=========================================="
echo "Version:            ${VERSION}"
echo "Registry:           ${REGISTRY}"
echo "Primary image repo: ${IMAGE_REPO}"
echo "Compat image repo:  ${COMPAT_REPO}"
echo "Push latest tags:   ${PUSH_LATEST}"
echo "Runtime profile:    ${RUNTIME_PROFILE}"
echo "Buildx driver:      ${BUILDX_CREATE_DRIVER}"
echo "Registry insecure:  ${REGISTRY_INSECURE}"
echo "Push endpoint:      ${REGISTRY_PUSH_ENDPOINT}"
echo "Cargo jobs:         ${CARGO_BUILD_JOBS}"
echo "Release tuning:     lto=${CARGO_RELEASE_LTO}, cgu=${CARGO_RELEASE_CODEGEN_UNITS}, opt=${CARGO_RELEASE_OPT_LEVEL}, debug=${CARGO_RELEASE_DEBUG}"
echo "Rust linker:        ${RUST_LINKER}"
echo "Platforms:          ${PLATFORMS}"
echo "=========================================="

# Create an isolated, minimal Docker build context.
BUILD_CTX="$(mktemp -d)"
trap 'rm -rf "$BUILD_CTX"' EXIT

cp "$PROJECT_DIR/Dockerfile" "$BUILD_CTX/Dockerfile"

rsync -a --prune-empty-dirs \
  --include='/Cargo.toml' \
  --include='/Cargo.lock' \
  --include='/rust-toolchain.toml' \
  --include='/.cargo/***' \
  --include='/crates/***' \
  --exclude='*' \
  "$PROJECT_DIR/" "$BUILD_CTX/"

if [ -d "$PROJECT_DIR/skills" ]; then
  cp -r "$PROJECT_DIR/skills" "$BUILD_CTX/skills"
fi

TAGS=(-t "$APP_IMAGE" -t "$COMPAT_IMAGE")
if [ "$PUSH_LATEST" = "1" ] && [ "$VERSION" != "latest" ]; then
  TAGS+=(-t "$APP_LATEST_IMAGE" -t "$COMPAT_LATEST_IMAGE")
fi

BUILDKITD_CONFIG=""
if [ "$REGISTRY_INSECURE" = "1" ]; then
  mkdir -p "$CACHE_DIR"
  BUILDKITD_CONFIG="${CACHE_DIR}/buildkitd.toml"
  {
    echo "[registry.\"${REGISTRY}\"]"
    echo "  http = true"
    echo "  insecure = true"
    if [ "${REGISTRY_PUSH_ENDPOINT}" != "${REGISTRY}" ]; then
      echo "[registry.\"${REGISTRY_PUSH_ENDPOINT}\"]"
      echo "  http = true"
      echo "  insecure = true"
    fi
  } > "$BUILDKITD_CONFIG"
fi

if docker buildx version >/dev/null 2>&1; then
  if [ "$BUILDX_AUTO_BOOTSTRAP" = "1" ]; then
    if docker buildx inspect "$BUILDX_BUILDER" >/dev/null 2>&1; then
      docker buildx use "$BUILDX_BUILDER" >/dev/null
      if [ "$BUILDX_CREATE_DRIVER" = "docker-container" ]; then
        INSPECT_OUT="$(docker buildx inspect "$BUILDX_BUILDER" 2>/dev/null || true)"
        # buildx output format varies by version; detect network=host permissively.
        if ! printf '%s\n' "$INSPECT_OUT" | grep -Eiq "network[^[:alnum:]]*${BUILDX_DRIVER_NETWORK}"; then
          echo "Warning: builder '${BUILDX_BUILDER}' missing network=${BUILDX_DRIVER_NETWORK}."
          echo "         If registry DNS fails, recreate it:"
          echo "         docker buildx rm ${BUILDX_BUILDER} && docker buildx create --name ${BUILDX_BUILDER} --driver ${BUILDX_CREATE_DRIVER} --driver-opt network=${BUILDX_DRIVER_NETWORK} --use"
        fi
      fi
    else
      echo "Creating buildx builder '${BUILDX_BUILDER}' (driver: ${BUILDX_CREATE_DRIVER})..."
      CREATE_ARGS=(--name "$BUILDX_BUILDER" --driver "$BUILDX_CREATE_DRIVER" --use)
      if [ "$BUILDX_CREATE_DRIVER" = "docker-container" ]; then
        CREATE_ARGS+=(--driver-opt "network=${BUILDX_DRIVER_NETWORK}")
      fi
      if [ -n "$BUILDKITD_CONFIG" ]; then
        CREATE_ARGS+=(--buildkitd-config "$BUILDKITD_CONFIG")
      fi
      if docker buildx create "${CREATE_ARGS[@]}" >/dev/null 2>&1; then
        echo "Using buildx builder '${BUILDX_BUILDER}'."
      else
        echo "Warning: failed to create builder '${BUILDX_BUILDER}', using current buildx builder."
      fi
    fi
  fi

  echo "Bootstrapping buildx builder (timeout: ${BUILDX_BOOTSTRAP_TIMEOUT}s)..."
  if command -v timeout >/dev/null 2>&1; then
    if ! timeout "${BUILDX_BOOTSTRAP_TIMEOUT}" docker buildx inspect --bootstrap >/dev/null 2>&1; then
      echo "Warning: buildx bootstrap timed out/failed; continuing with current builder state."
    fi
  else
    docker buildx inspect --bootstrap >/dev/null 2>&1 || \
      echo "Warning: buildx bootstrap failed; continuing with current builder state."
  fi
  BUILDX_DRIVER="$(docker buildx inspect "$BUILDX_BUILDER" 2>/dev/null | awk '/^Driver:/ {print $2; exit}' || true)"
  if [ -z "${BUILDX_DRIVER}" ]; then
    BUILDX_DRIVER="$BUILDX_CREATE_DRIVER"
    echo "Warning: could not detect active buildx driver; falling back to '${BUILDX_DRIVER}'."
  fi
  USE_BUILDX_CACHE=1
  if [ "${THAT_DISABLE_BUILDX_CACHE:-0}" = "1" ] || [ "$BUILDX_DRIVER" = "docker" ]; then
    USE_BUILDX_CACHE=0
  fi

  echo ""
  echo "Building and pushing with docker buildx..."
  PUSH_ARGS=(--push)

  if [ "$USE_BUILDX_CACHE" = "1" ]; then
    mkdir -p "$CACHE_DIR"
    CACHE_FROM_ARGS=()
    if [ -f "$CACHE_DIR/index.json" ]; then
      CACHE_FROM_ARGS=(--cache-from "type=local,src=$CACHE_DIR")
    fi

    docker buildx build \
      "${PUSH_ARGS[@]}" \
      --platform "$PLATFORMS" \
      "${TAGS[@]}" \
      "${CACHE_FROM_ARGS[@]}" \
      --cache-to "type=local,dest=$CACHE_DIR,mode=max" \
      --build-arg "THAT_RUNTIME_PROFILE=${RUNTIME_PROFILE}" \
      --build-arg "THAT_CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}" \
      --build-arg "THAT_CARGO_RELEASE_LTO=${CARGO_RELEASE_LTO}" \
      --build-arg "THAT_CARGO_RELEASE_CODEGEN_UNITS=${CARGO_RELEASE_CODEGEN_UNITS}" \
      --build-arg "THAT_CARGO_RELEASE_OPT_LEVEL=${CARGO_RELEASE_OPT_LEVEL}" \
      --build-arg "THAT_CARGO_RELEASE_DEBUG=${CARGO_RELEASE_DEBUG}" \
      --build-arg "THAT_RUST_LINKER=${RUST_LINKER}" \
      -f "$BUILD_CTX/Dockerfile" \
      "$BUILD_CTX"
  else
    echo "Buildx cache disabled (driver: ${BUILDX_DRIVER:-unknown})."
    docker buildx build \
      "${PUSH_ARGS[@]}" \
      --platform "$PLATFORMS" \
      "${TAGS[@]}" \
      --build-arg "THAT_RUNTIME_PROFILE=${RUNTIME_PROFILE}" \
      --build-arg "THAT_CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}" \
      --build-arg "THAT_CARGO_RELEASE_LTO=${CARGO_RELEASE_LTO}" \
      --build-arg "THAT_CARGO_RELEASE_CODEGEN_UNITS=${CARGO_RELEASE_CODEGEN_UNITS}" \
      --build-arg "THAT_CARGO_RELEASE_OPT_LEVEL=${CARGO_RELEASE_OPT_LEVEL}" \
      --build-arg "THAT_CARGO_RELEASE_DEBUG=${CARGO_RELEASE_DEBUG}" \
      --build-arg "THAT_RUST_LINKER=${RUST_LINKER}" \
      -f "$BUILD_CTX/Dockerfile" \
      "$BUILD_CTX"
  fi
else
  echo ""
  echo "Building with docker build..."
  docker build \
    --build-arg "THAT_RUNTIME_PROFILE=${RUNTIME_PROFILE}" \
    --build-arg "THAT_CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}" \
    --build-arg "THAT_CARGO_RELEASE_LTO=${CARGO_RELEASE_LTO}" \
    --build-arg "THAT_CARGO_RELEASE_CODEGEN_UNITS=${CARGO_RELEASE_CODEGEN_UNITS}" \
    --build-arg "THAT_CARGO_RELEASE_OPT_LEVEL=${CARGO_RELEASE_OPT_LEVEL}" \
    --build-arg "THAT_CARGO_RELEASE_DEBUG=${CARGO_RELEASE_DEBUG}" \
    --build-arg "THAT_RUST_LINKER=${RUST_LINKER}" \
    "${TAGS[@]}" \
    -f "$BUILD_CTX/Dockerfile" \
    "$BUILD_CTX"

  echo ""
  echo "Pushing images..."
  docker push "$APP_IMAGE"
  docker push "$COMPAT_IMAGE"
  if [ "$PUSH_LATEST" = "1" ] && [ "$VERSION" != "latest" ]; then
    docker push "$APP_LATEST_IMAGE"
    docker push "$COMPAT_LATEST_IMAGE"
  fi
fi

echo ""
echo "=========================================="
echo "Build and push completed"
echo "=========================================="
echo "Primary image: ${APP_IMAGE}"
echo "Compat image:  ${COMPAT_IMAGE}"
if [ "$PUSH_LATEST" = "1" ] && [ "$VERSION" != "latest" ]; then
  echo "Primary latest: ${APP_LATEST_IMAGE}"
  echo "Compat latest:  ${COMPAT_LATEST_IMAGE}"
fi
echo ""
echo "Next steps:"
echo "1. Set the image tag in your agent overlay kustomization (deploy/k8s/overlays/<agent>)."
echo "2. Deploy: kubectl apply -k deploy/k8s/overlays/<agent>"
echo "3. Verify rollout: kubectl -n that-agent-<agent> rollout status deploy/that-agent"
echo "=========================================="
