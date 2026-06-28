# agentd — container image (multi-stage)
#
# Stage 1 builds a statically-linked `agentd` against musl so the runtime
# stage can be a near-empty base. Stage 2 is a distroless nonroot image: no
# shell, no package manager, runs as UID 65532.
#
# DEFAULT FEATURE BUILD = NO TLS. The default (no-feature) build has no
# async runtime, no TLS, and no C toolchain — the minimalism target (see the
# workspace Cargo.toml and rfcs/0001). In that build the intelligence and MCP
# endpoints must be reached over plaintext that something *else* terminates:
#   - unix:/path        — a local socket (e.g. a TLS-terminating sidecar)
#   - http://host/...   — dev only; the client warns
#   - vsock:cid:port    — needs the `vsock` feature (see below)
# To call an https:// endpoint directly, build with the `tls` feature:
#       --features tls           (rustls + ring + bundled webpki roots)
# See the FEATURES build-arg below; pass e.g. --build-arg FEATURES=tls,vsock.
#
# MCP SERVERS ARE STDIO. agentd ships no tools of its own except a gated exec
# (rfcs/0001, rfcs/0012). Every tool comes from an MCP server that agentd
# spawns over **stdio** (`--mcp name=command ...`). Because the transport is
# stdio (a child process, not a network call), each MCP server binary must be
# reachable *inside this container's process namespace*. Two options:
#   1. BUNDLE the server binaries into this image (copy them into the runtime
#      stage and reference them with absolute paths in --mcp), or
#   2. run them as SIDECAR containers in the same pod and have the sidecar
#      expose a stdio bridge — but note stdio cannot cross a container
#      boundary directly, so a bundled binary or a shared process namespace
#      (shareProcessNamespace) is the simpler path in v1.
# This example image bundles nothing; see examples/docker/README.md.

# ---------------------------------------------------------------------------
# Stage 1 — build
# ---------------------------------------------------------------------------
FROM rust:1.88-bookworm AS build

# Capability surface is compile-time. Default (empty) = the minimalism build:
# no TLS, no async runtime, no C/C++ toolchain. Override to add features,
# e.g. --build-arg FEATURES=tls  or  --build-arg FEATURES=tls,vsock,cron.
ARG FEATURES=""

# Static musl target so the runtime stage needs no libc / dynamic loader.
ARG TARGET=x86_64-unknown-linux-musl
RUN rustup target add ${TARGET} \
    && apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Build the release binary. `--no-default-features` is redundant today (the
# crate has no default features) but is explicit about the minimalism intent.
RUN if [ -n "${FEATURES}" ]; then FEAT="--features ${FEATURES}"; else FEAT=""; fi; \
    cargo build --release --locked \
        --target ${TARGET} \
        -p agentd \
        --no-default-features ${FEAT} \
    && cp target/${TARGET}/release/agentd /agentd

# ---------------------------------------------------------------------------
# Stage 2 — runtime (distroless, nonroot, no shell)
# ---------------------------------------------------------------------------
# distroless/static = ca-certificates + tzdata + /etc/passwd nonroot user,
# nothing else. The `:nonroot` tag runs as UID/GID 65532. ca-certificates is
# only used when built --features tls (bundled webpki roots are compiled in,
# so even then this is belt-and-suspenders); a pure default build needs none.
FROM gcr.io/distroless/static-debian12:nonroot AS runtime

# OCI labels (optional, override at build time).
LABEL org.opencontainers.image.title="agentd" \
      org.opencontainers.image.description="Minimal, MCP-native, reactive agent runtime" \
      org.opencontainers.image.source="https://github.com/agentd-dev/source-code"

COPY --from=build /agentd /usr/local/bin/agentd

# Already nonroot via the base tag; stated explicitly for clarity.
USER 65532:65532

# No CMD instruction/intelligence baked in — those are the unit of work and
# are supplied per-run via env (INSTRUCTION, AGENT_INTELLIGENCE) or flags.
# `agentd` with no args validates config and exits 2 (missing instruction),
# which is the correct, fail-fast default for a misconfigured pod.
ENTRYPOINT ["/usr/local/bin/agentd"]
