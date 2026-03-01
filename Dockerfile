# syntax=docker/dockerfile:1.7

############################
# Stage 1: Build that
# Skipped in CI via: --build-context builder=<dir-with-binary>
############################
FROM rust:1-bookworm AS builder
ARG THAT_CARGO_BUILD_JOBS=0
ARG THAT_CARGO_RELEASE_LTO=thin
ARG THAT_CARGO_RELEASE_CODEGEN_UNITS=16
ARG THAT_CARGO_RELEASE_OPT_LEVEL=2
ARG THAT_CARGO_RELEASE_DEBUG=0
ARG THAT_RUST_LINKER=mold

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev git ca-certificates clang mold \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/.cargo-target \
    export CARGO_TARGET_DIR=/build/.cargo-target \
    && export CARGO_PROFILE_RELEASE_LTO="${THAT_CARGO_RELEASE_LTO}" \
    && export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${THAT_CARGO_RELEASE_CODEGEN_UNITS}" \
    && export CARGO_PROFILE_RELEASE_OPT_LEVEL="${THAT_CARGO_RELEASE_OPT_LEVEL}" \
    && export CARGO_PROFILE_RELEASE_DEBUG="${THAT_CARGO_RELEASE_DEBUG}" \
    && export RUSTFLAGS="-C linker=clang -C link-arg=-fuse-ld=${THAT_RUST_LINKER}" \
    && if [ "${THAT_CARGO_BUILD_JOBS}" -gt 0 ] 2>/dev/null; then \
      cargo build --release --bin that --jobs "${THAT_CARGO_BUILD_JOBS}"; \
    else \
      cargo build --release --bin that; \
    fi \
    && cp /build/.cargo-target/release/that /build/that \
    && strip /build/that

FROM moby/buildkit:v0.25.1-rootless AS buildkit-bin

############################
# Stage 2: Runtime
############################
FROM python:3.12-slim-bookworm
ARG THAT_RUNTIME_PROFILE=slim

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update \
    && apt-get install -y --no-install-recommends \
      coreutils bash git curl wget procps \
      jq ripgrep fd-find tree \
      kubernetes-client \
      ca-certificates \
      tini \
      sudo \
      docker.io \
    && if [ "$THAT_RUNTIME_PROFILE" = "full" ]; then \
      apt-get install -y --no-install-recommends \
        build-essential \
        rustc cargo \
        golang \
        nodejs npm; \
    fi \
    && rm -rf /var/lib/apt/lists/*

# fd-find installs as 'fdfind' on Debian — symlink for convenience
RUN ln -sf /usr/bin/fdfind /usr/local/bin/fd

# TypeScript (global install, full profile only)
RUN if [ "$THAT_RUNTIME_PROFILE" = "full" ]; then npm install -g typescript ts-node; fi

# Pre-install common Python testing/dev packages (full profile only)
RUN if [ "$THAT_RUNTIME_PROFILE" = "full" ]; then pip install --no-cache-dir pytest hypothesis requests; fi

# that binary — from builder stage (local) or pre-built via --build-context (CI)
COPY --from=builder /build/that /usr/local/bin/that
COPY --from=buildkit-bin /usr/bin/buildctl /usr/local/bin/buildctl
RUN ln -sf /usr/local/bin/buildctl /usr/bin/buildctl

# Non-root agent user with full passwordless sudo inside the sandbox container
RUN adduser --disabled-password --gecos "" agent \
    --shell /bin/bash \
    && echo "agent ALL=(ALL) NOPASSWD: ALL" >> /etc/sudoers.d/agent \
    && chmod 0440 /etc/sudoers.d/agent

# Writable dirs for agent
RUN mkdir -p /home/agent/go /home/agent/.cargo \
    /home/agent/.that-agent/agents/default/skills \
    && chown -R agent:agent /home/agent

USER agent

# Language environment — PIP_BREAK_SYSTEM_PACKAGES lets pip install without
# requiring a venv (Debian uses PEP 668 externally-managed-environment)
ENV GOPATH=/home/agent/go \
    PATH="/home/agent/go/bin:/home/agent/.cargo/bin:${PATH}" \
    SHELL=/bin/bash \
    PIP_BREAK_SYSTEM_PACKAGES=1

WORKDIR /workspace

# Unrestricted policy via env vars — can't be shadowed by workspace mount
ENV THAT_TOOLS_POLICY__TOOLS__CODE_READ=allow \
    THAT_TOOLS_POLICY__TOOLS__CODE_EDIT=allow \
    THAT_TOOLS_POLICY__TOOLS__FS_READ=allow \
    THAT_TOOLS_POLICY__TOOLS__FS_WRITE=allow \
    THAT_TOOLS_POLICY__TOOLS__FS_DELETE=allow \
    THAT_TOOLS_POLICY__TOOLS__SHELL_EXEC=allow \
    THAT_TOOLS_POLICY__TOOLS__SEARCH=allow \
    THAT_TOOLS_POLICY__TOOLS__MEMORY=allow \
    THAT_TOOLS_POLICY__TOOLS__GIT_COMMIT=allow \
    THAT_TOOLS_POLICY__TOOLS__GIT_PUSH=deny

ENTRYPOINT ["tini", "--", "/bin/bash"]
