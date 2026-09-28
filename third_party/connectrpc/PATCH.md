# Vendored `connectrpc` 0.9.1

Upstream: <https://github.com/connectrpc/connect-rust> · crates.io: `connectrpc` 0.9.1 · Apache-2.0

This is an **unmodified copy of the published crate except for three `Cargo.toml`
dependency entries**. No Rust source is changed. It is wired in from the
workspace root:

```toml
[patch.crates-io]
connectrpc = { path = "third_party/connectrpc" }
```

## Why

`connectrpc` is a non-optional dependency of [`a2a-rs`], which agentd uses for
A2A, and a2a-rs 0.10 turns on its `tls` feature. Two of the entries that
feature pulls in — `rustls` and `tokio-rustls` — are declared **without**
`default-features = false`, so their defaults select the `aws-lc-rs` crypto
provider. (0.9.1 already declares `hyper-rustls` with its defaults off; 0.3.3,
the copy this one replaces, did not.)

Cargo feature unification is additive and global: one crate asking for
`rustls/default` turns `aws-lc-rs` on for *every* crate in the graph, however
carefully the others opted into `ring`. agentd, `agentd-net` and `a2a-rs`
itself all correctly request `ring` with defaults off; that is not enough.

`aws-lc-sys` is a C and assembly library. Building it needs `cmake`, `make`,
`perl` and a C++ compiler, which turned a from-source build of a pure-Rust
agent into one that requires a full C toolchain — and made the release's
cross-compiled `x86_64-musl` job hang for 90 minutes where the *emulated*
`aarch64` job finished in three.

`rustls` supports `ring`, which is what agentd uses everywhere else. Nothing
needs the C library.

## The change

Three entries gain an explicit `ring`, and the two that still take their
defaults gain `default-features = false`:

| entry | added |
| --- | --- |
| `rustls` | `default-features = false`, `features = ["ring", "std", "tls12", "logging"]` |
| `tokio-rustls` | `default-features = false`, `features = ["ring", "tls12"]` |
| `hyper-rustls` | `features += ["ring"]` (its defaults are already off upstream) |

`hyper-rustls`'s root-store features (`native-tokio` / `webpki-tokio`) are
deliberately **not** added: the library's only connector is built with
`HttpsConnectorBuilder::with_tls_config(cfg)` (`src/client/mod.rs:762`), where
the caller supplies the `ClientConfig` and its roots.

The crate's own tests reference `rustls::crypto::aws_lc_rs` (`src/axum.rs:360`,
`src/server.rs:4907`), but both are inside `#[cfg(test)]` modules and never
compile for a consumer.

`zstd` (and with it the C `zstd-sys`) stays: it is a default feature of
`connectrpc` that a2a-rs does not turn off, it was in the graph before this
copy, and it is not what this patch is about.

## Why 0.9.1 and not the 0.3.3 this replaces

a2a-rs 0.10 requires `connectrpc` 0.9.1. That release also carries the fix
for RUSTSEC-2026-0304 (a finished client-streaming or bidirectional call kept
reading a stalled request body with no time limit). A vendored path copy is
invisible to `cargo deny check advisories` — it matches advisories against
registry sources only — so the older copy was affected without the gate ever
saying so. **Re-vendoring is therefore also the security update; check the
advisory database by hand on every re-vendor.**

## Removing this

Delete the directory and the `[patch.crates-io]` stanza as soon as an upstream
`connectrpc` release carries the fix — the patch is version-pinned to `0.9.1`
and must be redone on any bump (copy the published crate from
`~/.cargo/registry/src/*/connectrpc-<version>/`, drop `.cargo-ok`, and re-apply
the table above to `Cargo.toml`). `cargo tree -e normal --all-features -i
aws-lc-sys` returning nothing is the test.

**Note this patch does not reach people who `cargo install agentd-cli` or
depend on `agentd-core` from crates.io.** `[patch.crates-io]` applies only to
builds within this workspace, and a published crate cannot turn off a
transitive dependency's features. Those builds still pull `aws-lc-sys` and
still need `cmake` until the fix is upstream. Every artifact we ship — the
release binaries, the container, and any build from this repository — is
unaffected.

[`a2a-rs`]: https://crates.io/crates/a2a-rs
