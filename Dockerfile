# syntax=docker/dockerfile:1
#
# agentd cloud-native appliance image — a fully static musl binary on `scratch`.
#
# The image ships the **dependency-free cloud-native feature set**
# (`metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch`): the `/healthz`+
# `/readyz`+`/metrics` HTTP probe surface (so k8s liveness/readiness probes work),
# agentd serving its own MCP for composability, UTC-cron scheduling, OTLP trace
# export, horizontal scaling (sharding + work-claim leases + autoscaling signals +
# the capacity surface), and SIGHUP + inotify config hot-reload (a ConfigMap volume
# swap reloads in place). Every one of those is hand-rolled and adds NO dependency.
# As of v2.0.0, HTTPS is the primary transport for both intelligence and MCP, so
# `tls` is ON by DEFAULT: the binary carries pure-Rust rustls (`ring`, no cmake/C)
# + bundled webpki roots — serde/serde_json + libc + rustls/ring/webpki-roots, no
# async runtime, no C toolchain — links statically against musl, and ships on an
# empty base: ~few MB, no shell, no libc, no package manager. Nothing to attack.
#
# Change the capability surface at build time with FEATURES, e.g.:
#   docker build --build-arg FEATURES=metrics,serve-mcp,cron,otel,vsock .
#   docker build --build-arg FEATURES= .          # the flag-free build (still TLS via default)
# `tls` (default) needs no system CA bundle — the webpki roots are bundled. To drop
# TLS entirely (reach https only via a `unix:` TLS-terminating sidecar), build with
# cargo `--no-default-features`.

# ---- builder -------------------------------------------------------------
FROM rust:1.88-alpine AS builder
ARG FEATURES="metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch,aauth"
# Alpine's host target IS <arch>-unknown-linux-musl, so the release binary is
# static (crt-static is on for musl). Building WITHOUT an explicit --target uses
# that host target, which is exactly what each buildx platform wants — so one
# Dockerfile produces native-static amd64 AND arm64 images. musl-dev supplies the
# static C runtime stubs the linker references; the build is pure Rust (libc
# *bindings* only — no C is compiled in the dependency-free feature set).
RUN apk add --no-cache musl-dev
WORKDIR /build
COPY . .
# Release profile (workspace Cargo.toml): LTO'd, stripped, size-optimized,
# panic=abort. `--locked` keeps the build reproducible against Cargo.lock.
RUN if [ -n "$FEATURES" ]; then \
      cargo build --release --locked -p agentd-cli --features "$FEATURES"; \
    else \
      cargo build --release --locked -p agentd-cli; \
    fi

# ---- runtime: scratch ----------------------------------------------------
FROM scratch

# OCI image metadata (populated by CI via --build-arg; harmless defaults locally).
ARG VERSION="1.0.0"
ARG REVISION="unknown"
ARG CREATED="1970-01-01T00:00:00Z"
LABEL org.opencontainers.image.title="agentd" \
      org.opencontainers.image.description="Minimal, MCP-native, reactive agent runtime — one static binary for k8s." \
      org.opencontainers.image.source="https://github.com/agentd-dev/source-code" \
      org.opencontainers.image.documentation="https://github.com/agentd-dev/source-code/blob/main/docs/deployment.md" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="scratch"

COPY --from=builder /build/target/release/agentd /agentd
# Non-root by uid (scratch has no /etc/passwd; the kernel just uses the number).
# Matches the k8s manifests' runAsUser/runAsGroup 65532 (examples/k8s/).
USER 65532:65532
# agentd needs INSTRUCTION + an intelligence endpoint (env/flags); an external
# scheduler (e.g. a k8s Job/CronJob/Deployment) drives lifecycle. See
# docs/deployment.md and examples/k8s/.
ENTRYPOINT ["/agentd"]
