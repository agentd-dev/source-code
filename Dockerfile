# syntax=docker/dockerfile:1
#
# agentd appliance image — distroless, nonroot, no shell.
#
# Built with --all-features (the broadest capability surface) to match
# docs/operations.md §8.4. A production deployment that needs a narrower
# surface should compile out the capabilities it doesn't use and build a
# purpose-built image — the default build already drops outbound HTTP and
# shell. See docs/configuration.md (Build modes).

# ---- builder -------------------------------------------------------------
FROM rust:1.88-bookworm AS builder

# Everything in --all-features is pure Rust except aws-lc-rs (the rustls
# crypto provider pulled in by `server-tls`), whose C library builds with
# cmake. GitHub runners ship cmake; this minimal image does not.
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# The release profile (workspace Cargo.toml) is size-optimized, LTO'd,
# stripped, and panic=abort.
RUN cargo build --release -p agentd --all-features

# ---- runtime -------------------------------------------------------------
# distroless/cc — glibc + libgcc, no shell, no package manager. `:nonroot`
# runs as uid:gid 65532:65532. Same debian12 base as the builder, so the
# glibc the binary links against matches.
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/agentd /usr/local/bin/agentd

USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/agentd"]
