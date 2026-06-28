# syntax=docker/dockerfile:1.7
#
# Multi-stage build for the `agent` workflow runtime.
#
# Build stage uses the official Rust image; runtime is distroless/cc
# for a small, shell-free, nonroot image that still carries glibc +
# libgcc so aws-lc-sys's native crypto links cleanly.
#
# For multi-arch, invoke via `docker buildx build --platform
# linux/amd64,linux/arm64 ...`. QEMU handles the arm64 path when
# building from amd64 runners — slow but correct.
#
# Build context is the workspace root:
#   docker build -f crates/agentd/Dockerfile .

FROM rust:1.85-bookworm AS builder

WORKDIR /src

COPY . .

# Build with --all-features so the shipped image covers every
# capability. Operators who want a smaller attack surface should
# build their own image with a narrower feature set (see
# docs/operations.md §2).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build \
        --release \
        -p agentd \
        --all-features \
        --locked \
 && cp target/release/agentd /usr/local/bin/agentd

# Runtime: distroless/cc:nonroot — ~25 MB, no shell, uid:gid=65532:65532.
FROM gcr.io/distroless/cc-debian12:nonroot

# OCI metadata — surfaces in `docker inspect` and on GHCR's UI.
LABEL org.opencontainers.image.title="agent" \
      org.opencontainers.image.description="Bounded workflow runtime (DAG of typed nodes, TOML-driven)" \
      org.opencontainers.image.source="https://github.com/agentd-dev/source-code" \
      org.opencontainers.image.documentation="https://github.com/agentd-dev/source-code/tree/main/docs" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.vendor="agentd.dev"

COPY --from=builder /usr/local/bin/agentd /usr/local/bin/agentd

# /healthz + /metrics expose on whatever `--bind` is; 8080 matches
# the runtime default when no override is passed.
EXPOSE 8080

USER nonroot:nonroot

# No default CMD — callers supply `--config` / `--bind` / etc., or
# the image ships with `AGENT_EMBED_CONFIG` baked at build time.
ENTRYPOINT ["/usr/local/bin/agentd"]
