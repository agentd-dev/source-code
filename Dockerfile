# syntax=docker/dockerfile:1
#
# agentd cloud-native appliance image — a fully static musl binary on `scratch`
# — and the builder image it is built in.
#
# The image ships the **cloud-native feature set**
# (`a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models,companies,xaa,x402`): the A2A HTTPS listener
# (RFC 0029, the external channel + delegation peers), the `/healthz`+`/readyz`+
# `/metrics` HTTP probe surface (so k8s liveness/readiness probes work), UTC-cron
# scheduling, OTLP trace+log export, SIGHUP + inotify config hot-reload (a
# ConfigMap volume swap reloads in place), OAuth/SigV4/SPIFFE endpoint auth,
# AAuth agent identity, CEL expressions, §7 instruction signature verification,
# `oci://` instruction pulls with cosign, encrypted instruction envelopes,
# MCP Events bindings (a draft extension, SEP-3415), and three previews that do
# nothing until configured: acting at Companies for a person (the Personal
# Agent Protocol draft), acting for enrolled people through their IdP (ID-JAG,
# behind `security.experimental.id_jag`), and paying with x402 within a spend
# policy (`security.payments`).
# `cel` and `x402` (`k256`/`sha3`, secp256k1 and Keccak-256) are the
# dependency-BEARING members; all but them and `a2a` (which pulls the TLS
# stack) are hand-rolled and add NO dependency, `sign`/`oci`/`decrypt`/
# `companies`/`xaa` reusing the `ring` rustls already links, `mcp-events`
# reusing the MCP socket.
#
# This list is the same one `.github/workflows/release.yml` builds, and
# `release_matrix.rs` fails if the two drift: the image and the standalone
# binaries are the same release and must have the same capabilities.
# HTTPS is the primary transport for both intelligence and MCP, so `tls` is ON by
# DEFAULT: rustls with the `ring` provider + bundled webpki roots, so there is no
# system CA bundle to mount. MCP and A2A are the official SDKs (`rmcp`, and the
# A2A project's `a2a-lf`/`a2a-pb`/`a2a-server-lf`), which bring an async runtime
# into the BUILD — but no C: the crypto provider is `ring` everywhere, and the
# A2A SDK's protoc is a vendored static binary, so the build is pure Rust and
# needs no cmake or C++ compiler. What
# ships is one static musl binary on an empty base, a few MB, no shell, no libc,
# no package manager. Nothing to attack.
#
# The stages, in order:
#
#   toolchain   rust:1.96-alpine, pinned by digest, plus musl-dev
#   cargo-deny  a throwaway stage: CI's cargo-deny, checksum-verified, and a
#               snapshot of the RustSec advisory database
#   builder     the published builder image (`--target builder`): the source
#               with every dependency vendored, cargo-deny and the snapshot,
#               the features file, `agentd-build`, and the release set already
#               compiled once into its target directory; anyone can build
#               their own agentd FROM it, offline, with an allow-listed
#               feature set
#   build       the official build: `agentd-build` with FEATURES, which finds
#               the builder's compile current and only checks and copies it
#   (default)   the official image: the binary on `scratch`
#
# The official image goes through the builder like anyone's: the release is
# built by the same `agentd-build`, over the same vendored source, as a build
# of your own, and release.yml builds it FROM the builder it publishes, by
# digest, in place of the `builder` stage. `packaging/builder/features` is
# the list it offers.
#
# Change the capability surface at build time with FEATURES, and say what the
# binary will report for it in BUILD_INFO_FEATURES — FEATURES with what each
# implies and the default `tls`, sorted, which is what the image's
# `dev.agentd.features` label carries. The build refuses, before it compiles,
# when the two disagree, and says what BUILD_INFO_FEATURES must be:
#   docker build --build-arg FEATURES=a2a,metrics,cron,otel --build-arg BUILD_INFO_FEATURES=a2a,cron,metrics,otel,tls .
# `tls` (default) needs no system CA bundle — the webpki roots are bundled. To drop
# TLS entirely (reach https only via a TLS-terminating sidecar on a loopback
# `http://` port), build with cargo `--no-default-features`.

# The release set, for every stage below: what the official build compiles,
# and what the builder image names as the release in its labels.
ARG FEATURES="a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth,cel,sign,oci,decrypt,mcp-events,mock-models,companies,xaa,x402"
# What `agentd --build-info` reports for FEATURES: FEATURES, what each
# implies and the default `tls`, sorted. The official build checks it
# (`agentd-build --expect-features`) and the image carries it as
# `dev.agentd.features`; builder_image.rs holds it to release.yml.
ARG BUILD_INFO_FEATURES="a2a,aauth,cel,companies,config-watch,cron,decrypt,hot-reload,mcp-events,metrics,mock-models,oauth,oci,otel,sign,tls,x402,xaa"

# ---- toolchain -----------------------------------------------------------
# The digest is the multi-arch index's, so each buildx platform resolves its
# own image under it and a moved tag cannot change what a build compiles with.
# 1.96 is the crates' rust-version.
FROM rust:1.96-alpine@sha256:a41f7740f8b45d45795624eec13a8b42263cc700f19f7e4e86e04d3dda08a479 AS toolchain
# Alpine's host target IS <arch>-unknown-linux-musl, so the release binary is
# static (crt-static is on for musl). Building WITHOUT an explicit --target uses
# that host target, which is exactly what each buildx platform wants — so one
# Dockerfile produces native-static amd64 AND arm64 images.
#
# `musl-dev` is the only build package needed: `ring` ships prebuilt assembly and
# the rest of the graph is pure Rust. No cmake, no C++ compiler.
RUN apk add --no-cache musl-dev

# ---- cargo-deny (throwaway) ------------------------------------------------
# The supply-chain gate CI runs, at the version CI pins (ci.yml's deny job;
# builder_image.rs holds the two equal), as the project's own release binary,
# verified by checksum — so the builder adds no crate to agentd's graph and
# compiles nothing for it. Its advisory database is cloned here, where git is,
# and only the clone reaches the builder.
FROM toolchain AS cargo-deny
ARG TARGETARCH
ARG CARGO_DENY_VERSION="0.19.5"
ARG CARGO_DENY_SHA256_AMD64="5ea64ae09959b5fe1072d898f95caaa89b374678ba6728d5e9ed1366745479b0"
ARG CARGO_DENY_SHA256_ARM64="f23d2b4e343a54af3d925b557294c8c9d00dacb7bb98663f995a4427efebe1db"
RUN apk add --no-cache git
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-musl; sum=$CARGO_DENY_SHA256_AMD64 ;; \
      arm64) triple=aarch64-unknown-linux-musl; sum=$CARGO_DENY_SHA256_ARM64 ;; \
      *) echo "cargo-deny publishes no linux-musl binary for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    name="cargo-deny-${CARGO_DENY_VERSION}-${triple}"; \
    wget -q -O /tmp/deny.tgz \
      "https://github.com/EmbarkStudios/cargo-deny/releases/download/${CARGO_DENY_VERSION}/${name}.tar.gz"; \
    echo "${sum}  /tmp/deny.tgz" | sha256sum -c -; \
    tar -xzf /tmp/deny.tgz -C /tmp; \
    install -m 0755 "/tmp/${name}/cargo-deny" /usr/local/cargo/bin/cargo-deny; \
    rm -rf /tmp/deny.tgz "/tmp/${name}"
WORKDIR /src
COPY deny.toml Cargo.toml ./
# The snapshot `cargo deny check --offline` reads. A second fetch on purpose:
# the first clones, and only a fetch leaves the FETCH_HEAD whose time cargo
# deny reads as the snapshot's age — without it, it asks git, which the
# builder does not carry. A snapshot older than deny.toml's
# `maximum-db-staleness` (cargo deny's default, 90 days) fails every build: a
# builder that old is told to make way for a newer one.
RUN cargo deny fetch db && cargo deny fetch db

# ---- builder: the published builder image --------------------------------
FROM toolchain AS builder
ARG FEATURES
# jq reads the binary's --build-info for agentd-build; it is a package, not a
# crate, and nothing it touches reaches the binary.
RUN apk add --no-cache jq
COPY --from=cargo-deny /usr/local/cargo/bin/cargo-deny /usr/local/cargo/bin/cargo-deny
COPY --from=cargo-deny /usr/local/cargo/advisory-dbs /usr/local/cargo/advisory-dbs
WORKDIR /usr/src/agentd
COPY . .
# Every dependency vendored, and cargo pointed at the copy, so a build here
# reads nothing from the network: `--offline` holds, and what a build compiles
# is exactly this tree. The registry's own copies of the crates are dropped
# (the vendor directory holds them); its index cache stays, because cargo
# deny reads crates' yanked status from it.
RUN set -eu; \
    mkdir -p .cargo; \
    cargo vendor --locked vendor > .cargo/config.toml; \
    rm -rf "$CARGO_HOME/registry/cache" "$CARGO_HOME/registry/src"
COPY --chmod=0755 packaging/builder/agentd-build /usr/local/bin/agentd-build
COPY packaging/builder/features /etc/agentd-build/features
# The release and the commit this builder is made from. release.yml passes
# both; a build from a checkout passes neither, and they stay empty. They are
# declared here, after the vendoring, so a builder told them shares every
# layer above with one that is not.
ARG VERSION=""
ARG REVISION=""
ARG CREATED="1970-01-01T00:00:00Z"
LABEL org.opencontainers.image.title="agentd-builder" \
      org.opencontainers.image.description="Builds agentd with an allow-listed set of cargo features, offline, from the vendored source of one release." \
      org.opencontainers.image.source="https://github.com/agentd-dev/source-code" \
      org.opencontainers.image.documentation="https://github.com/agentd-dev/source-code/blob/main/docs/deployment.md" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="docker.io/library/rust:1.96-alpine" \
      org.opencontainers.image.base.digest="sha256:a41f7740f8b45d45795624eec13a8b42263cc700f19f7e4e86e04d3dda08a479" \
      dev.agentd.builder.release-features="${FEATURES}"
# What a build reports about the tree it compiled, as far as this builder
# was told: the commit (REVISION) with the tree's digest, which
# `agentd-build` recomputes to say whether a build's source is still that
# commit's (`source_modified`), and the release (VERSION) as the builder's
# name. A builder told neither records neither, and a build in it reports
# `revision`, `source_modified` and `builder` as `null` rather than a
# placeholder. REVISION is the caller's word that the context is that
# commit, unmodified: the context has no .git to check it against.
RUN set -eu; \
    if [ -n "${REVISION:-}" ]; then \
        mkdir -p .agentd-build; \
        printf '%s\n' "$REVISION" > .agentd-build/REVISION; \
        agentd-build --source-digest > .agentd-build/SOURCE_DIGEST; \
    fi; \
    if [ -n "${VERSION:-}" ]; then \
        printf 'agentd-builder %s\n' "${VERSION#v}" > /etc/agentd-build/builder; \
    fi
# A build's target directory, pre-warmed: the release set is compiled into
# it here, the same `agentd-build` run the official build makes, so that
# build only checks and copies it, and a build of your own recompiles only
# the crates whose features differ. It adds about 680 MB to the image. A
# BuildKit cache mount at this path keeps your own compiles from one build
# to the next; seed it from the image so the pre-warmed compile is not
# hidden under an empty cache
# (`--mount=type=cache,target=/var/cache/agentd-build/target,from=<this image>,source=/var/cache/agentd-build/target`).
ENV CARGO_TARGET_DIR=/var/cache/agentd-build/target
ARG BUILD_INFO_FEATURES
RUN agentd-build --features "$FEATURES" --expect-features "$BUILD_INFO_FEATURES" --out /tmp/agentd-build \
 && rm -rf /tmp/agentd-build
ENTRYPOINT ["agentd-build"]
CMD ["--help"]

# ---- build: the official build -------------------------------------------
FROM builder AS build
ARG FEATURES
ARG BUILD_INFO_FEATURES
# Release profile (workspace Cargo.toml): LTO'd, stripped, size-optimized,
# panic=abort. `--locked --offline` keeps the build reproducible against
# Cargo.lock and the vendored tree. The builder compiled exactly this, so
# cargo finds it current; the supply-chain gate and the feature check still
# run, here, on what the image ships.
RUN agentd-build --features "$FEATURES" --expect-features "$BUILD_INFO_FEATURES" --out /out

# ---- runtime: scratch ----------------------------------------------------
FROM scratch

# OCI image metadata (populated by CI via --build-arg; harmless defaults locally).
ARG VERSION="1.0.0"
ARG REVISION="unknown"
ARG CREATED="1970-01-01T00:00:00Z"
ARG BUILD_INFO_FEATURES
LABEL org.opencontainers.image.title="agentd" \
      org.opencontainers.image.description="Minimal, MCP-native, reactive agent runtime — one static binary for k8s." \
      org.opencontainers.image.source="https://github.com/agentd-dev/source-code" \
      org.opencontainers.image.documentation="https://github.com/agentd-dev/source-code/blob/main/docs/deployment.md" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}" \
      org.opencontainers.image.base.name="scratch" \
      dev.agentd.features="${BUILD_INFO_FEATURES}"

COPY --from=build /out/agentd /agentd
# The licence and the third-party notices go with the binary: NOTICE carries
# the CC BY 4.0 attribution for the instruction.md registries every binary
# embeds. From the build stage, so they are the source the binary was built
# from; release.yml packs the same two files into each tarball, and
# builder_image.rs holds both to it.
COPY --from=build /usr/src/agentd/LICENSE /usr/src/agentd/NOTICE /usr/share/doc/agentd/
# Non-root by uid (scratch has no /etc/passwd; the kernel just uses the number).
# Matches the k8s manifests' runAsUser/runAsGroup 65532 (examples/k8s/).
USER 65532:65532
# agentd needs INSTRUCTION + an intelligence endpoint (env/flags); an external
# scheduler (e.g. a k8s Job/CronJob/Deployment) drives lifecycle. See
# docs/deployment.md and examples/k8s/.
ENTRYPOINT ["/agentd"]
