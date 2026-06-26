# syntax=docker/dockerfile:1
#
# agentd minimal appliance image — a fully static musl binary on `scratch`.
#
# RFC 0001 / assessment §4 M7: the *default* build is the minimalism target
# (no async runtime, no TLS, no C toolchain — just serde/serde_json + libc),
# so it links statically against musl and ships on an empty base: ~1 MB, no
# shell, no libc, no package manager — nothing to attack or patch.
#
# Need a heavier capability surface? Pass FEATURES at build time, e.g.
#   docker build --build-arg FEATURES=tls,vsock,metrics,serve-mcp .
# (the rustls `ring` provider stays pure-Rust, no cmake). Reaching the
# intelligence endpoint over `unix:` to a TLS-terminating sidecar keeps the
# default, TLS-free, fully static image.

# ---- builder -------------------------------------------------------------
FROM rust:1.88-alpine AS builder
ARG FEATURES=""
# Alpine's host target IS x86_64-unknown-linux-musl, so the release binary is
# static. musl-dev supplies the static C runtime stubs the linker references;
# the build itself is pure Rust (libc *bindings* only — no C is compiled in the
# default build).
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# Release profile (workspace Cargo.toml): LTO'd, stripped, size-optimized,
# panic=abort. Optional features are opt-in via the build arg.
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release -p agentd --features "$FEATURES"; \
    else \
      cargo build --release -p agentd; \
    fi

# ---- runtime: scratch ----------------------------------------------------
FROM scratch
COPY --from=builder /build/target/release/agentd /agentd
# Non-root by uid (scratch has no /etc/passwd; the kernel just uses the number).
USER 65532:65532
# agentd needs INSTRUCTION + an intelligence endpoint (env/flags); an external
# scheduler (e.g. a k8s operator) drives lifecycle. See docs/deployment.md.
ENTRYPOINT ["/agentd"]
