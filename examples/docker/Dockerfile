# agentd — container image (multi-stage)
#
# Stage 1 builds a statically-linked `agentd` against musl so the runtime
# stage can be a near-empty base. Stage 2 is a distroless nonroot image: no
# shell, no package manager, runs as UID 65532.
#
# TRANSPORT = HTTPS. The default build links `tls` (rustls + ring + bundled
# webpki roots), so agentd dials `https://` directly — intelligence, the MCP
# client, the served self-MCP, and A2A are all HTTPS:
#   - https://host/...   — the default; agentd holds the trust roots
#   - http://127.0.0.1/… — loopback only, for a same-host TLS-terminating sidecar
# To keep TLS out of the image, build --no-default-features and use a loopback
# http:// sidecar. See the FEATURES build-arg below.
#
# MCP SERVERS ARE REMOTE HTTP ENDPOINTS. agentd ships no tools of its own and runs
# no local code (rfcs/0001, rfcs/0012 — no exec tool). Every tool comes from an MCP
# server agentd CONNECTS to over Streamable HTTP (`--mcp name=https://host/mcp`); it
# spawns no process. So nothing MCP-related is bundled into this image — deploy each
# MCP server as its own service and point --mcp at its URL. See examples/docker/README.md.

# ---------------------------------------------------------------------------
# Stage 1 — build
# ---------------------------------------------------------------------------
FROM rust:1.88-bookworm AS build

# Capability surface is compile-time. The default build links `tls` (the
# transport). Override to add opt-in surfaces, e.g.
# --build-arg FEATURES=serve-https,a2a,cron,workflow — or --no-default-features
# (empty FEATURES + the build below) for the TLS-free loopback-sidecar posture.
ARG FEATURES=""

# Static musl target so the runtime stage needs no libc / dynamic loader.
ARG TARGET=x86_64-unknown-linux-musl
RUN rustup target add ${TARGET} \
    && apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Build the release binary. `--no-default-features` strips the crate's default
# `tls` feature, giving the minimal loopback-sidecar posture; opt TLS back in
# (direct https://) with FEATURES=tls, or a served surface with serve-https.
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
