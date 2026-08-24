# Contributing to agentd

Thanks for contributing! agentd is the **reference agent** for the Agent Control
Contract (ACC) that the agentctl control plane consumes.

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

## ACC conformance — keep the contract honest

agentd is conformant to the ACC **by behaviour**, not by sharing code with the
control plane ([`CONFORMANCE.md`](CONFORMANCE.md)). If you change a served
surface (manifest, management profile, metrics, exit codes, events, config, A2A,
env, report):

- keep the change conformant to the contract schemas (the agentctl repo's
  `contract/schemas/*` + `contract/SPEC.md`);
- preserve the hard invariants — the manifest stays `json!`→`Value` (no
  `Serialize`, secret-safe); no credential reaches the manifest/config/identity
  path; branded **and** neutral (`AGENT_*` / `agent://`) spellings stay accepted;
- update `CONFORMANCE.md` and add/extend a conformance check.

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
