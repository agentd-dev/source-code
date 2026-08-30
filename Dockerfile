# syntax=docker/dockerfile:1
#
# agentd cloud-native appliance image — a fully static musl binary on `scratch`.
#
# The image ships the **cloud-native feature set**
# (`a2a,metrics,cron,otel,hot-reload,config-watch`): the A2A v2 HTTPS listener
# (RFC 0029, the external channel + delegation peers), the `/healthz`+`/readyz`+
# `/metrics` HTTP probe surface (so k8s liveness/readiness probes work), UTC-cron
# scheduling, OTLP trace+log export, and SIGHUP + inotify config hot-reload (a
# ConfigMap volume swap reloads in place). All but `a2a` (which pulls the TLS
# stack) are hand-rolled and add NO dependency.
# HTTPS is the primary transport for both intelligence and MCP, so `tls` is ON by
# DEFAULT: rustls with the `ring` provider + bundled webpki roots, so there is no
# system CA bundle to mount. MCP and A2A are the official/published protocol
# implementations (`rmcp`, `a2a-rs`), which bring an async runtime into the BUILD
# — but no C: the crypto provider is `ring` everywhere, so the build is pure Rust
# and needs no cmake or C++ compiler (see third_party/connectrpc/PATCH.md). What
# ships is one static musl binary on an empty base, a few MB, no shell, no libc,
# no package manager. Nothing to attack.
#
# Change the capability surface at build time with FEATURES, e.g.:
#   docker build --build-arg FEATURES=a2a,metrics,cron,otel .
#   docker build --build-arg FEATURES= .          # the flag-free build (still TLS via default)
# `tls` (default) needs no system CA bundle — the webpki roots are bundled. To drop
# TLS entirely (reach https only via a `unix:` TLS-terminating sidecar), build with
# cargo `--no-default-features`.

# ---- builder -------------------------------------------------------------
FROM rust:1.96-alpine AS builder
ARG FEATURES="a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel"
# Alpine's host target IS <arch>-unknown-linux-musl, so the release binary is
# static (crt-static is on for musl). Building WITHOUT an explicit --target uses
# that host target, which is exactly what each buildx platform wants — so one
# Dockerfile produces native-static amd64 AND arm64 images.
#
# `musl-dev` is the only build package needed: `ring` ships prebuilt assembly and
# the rest of the graph is pure Rust. No cmake, no C++ compiler.
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
      org.opencontainers.image.licenses="AGPL-3.0-only" \
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
