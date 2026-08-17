# Vendored `connectrpc` 0.3.3

Upstream: <https://github.com/anthropics/connect-rust> · crates.io: `connectrpc` 0.3.3 · Apache-2.0

This is an **unmodified copy of the published crate except for three `Cargo.toml`
dependency entries**. No Rust source is changed. It is wired in from the
workspace root:

```toml
[patch.crates-io]
connectrpc = { path = "third_party/connectrpc" }
```

## Why

`connectrpc` is a non-optional dependency of [`a2a-rs`], which agentd uses for
A2A. It declares `rustls`, `tokio-rustls` and `hyper-rustls` **without**
`default-features = false`, so their defaults select the `aws-lc-rs` crypto
provider.

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

Three entries gain `default-features = false` and an explicit `ring`:

| entry | added |
| --- | --- |
| `rustls` | `default-features = false`, `features = ["ring", "std", "tls12", "logging"]` |
| `tokio-rustls` | `default-features = false`, `features = ["ring", "tls12"]` |
| `hyper-rustls` | `default-features = false`, `features += ["ring"]` |

`hyper-rustls`'s root-store features (`native-tokio` / `webpki-tokio`) are
deliberately **not** re-added: the library's only connector is built with
`HttpsConnectorBuilder::with_tls_config(cfg)` (`src/client/mod.rs:418`), where
the caller supplies the `ClientConfig` and its roots. Dropping them removes a
`rustls-native-certs` dependency the code never reaches.

The crate's own tests reference `rustls::crypto::aws_lc_rs` (`src/server.rs:911`),
but that is inside `#[cfg(test)]` and never compiles for a consumer.

## Result

`aws-lc-rs`, `aws-lc-sys` and `rustls-native-certs` leave the graph entirely.
The build needs no C toolchain, `FROM scratch` stays, and `Cross.toml`'s
`cmake` pre-build step is gone.

## Removing this

Delete the directory and the `[patch.crates-io]` stanza as soon as an upstream
`connectrpc` release carries the fix — the patch is version-pinned to `0.3.3`
and must be re-checked on any bump. `cargo tree -i aws-lc-sys` returning
nothing is the test.

**Note this patch does not reach people who `cargo install agentd-cli` or
depend on `agentd-core` from crates.io.** `[patch.crates-io]` applies only to
builds within this workspace, and a published crate cannot turn off a
transitive dependency's features. Those builds still pull `aws-lc-sys` and
still need `cmake` until the fix is upstream. Every artifact we ship — the
release binaries, the container, and any build from this repository — is
unaffected.

[`a2a-rs`]: https://crates.io/crates/a2a-rs
