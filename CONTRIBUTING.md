# Contributing to agentd

Thanks for contributing!

## Licensing & DCO sign-off

agentd is **AGPL-3.0-only** (see [`LICENSE`](LICENSE)) — contributions are
accepted **inbound = outbound** under the same licence; no CLA is required.
Instead, sign off every commit with the **Developer Certificate of Origin**
(certifying you wrote it / may submit it):

```sh
git commit -s -m "your message"   # appends a Signed-off-by: line
```

CI enforces a `Signed-off-by` line on every commit in a PR.

## Source headers

New source files carry an SPDX header on line 1:

```rust
// SPDX-License-Identifier: AGPL-3.0-only
```

## Conformance — keep the served surfaces honest

agentd's conformance is judged by **behaviour**, against the specifications and
the contracts it documents ([`CONFORMANCE.md`](CONFORMANCE.md)). If you change a
served surface (exit codes, the A2A listener, events, config admission, env, the
store contract):

- extend the black-box suite in `crates/agentd-conformance` with a check that
  fails when the change is reverted;
- keep credentials out of every surface a caller can read;
- keep one spelling per name: `AGENTD_` is the only environment prefix, and an
  agentd-owned name carries no version. A renamed or removed key, flag, method
  or variable is deleted outright — no alias, no by-name refusal; the generic
  unknown-name path refuses it.

## Dev workflow

```sh
cargo build -p agentd-core                     # the engine, default features
cargo test --workspace --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all
cargo run -p agentd-conformance                # the black-box behavioural suite
```

Build with **default** features too, not only `--all-features`: the default
build carries a three-dependency moat (`libc`, `serde`, `serde_json`) that a
full-feature build hides, and a new dependency that lands there is a decision,
not an accident. Features are compile-time and each one is a 1:1 forward from
`agentd-cli` to `agentd-core`, so a feature-solo build is the only way to catch
a `cfg` that only compiles when a neighbour is also on.

By submitting a contribution you agree it is licensed under AGPL-3.0-only and
that you have signed off the DCO.
