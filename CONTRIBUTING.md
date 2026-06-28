# Contributing to agent

Thanks for contributing! agent is the **reference agent** for the Agent Control
Contract (ACC) that the agentctl control plane consumes.

## Licensing & DCO sign-off

agent is **Apache-2.0** (see [`LICENSE`](LICENSE)) — contributions are accepted
**inbound = outbound** under Apache-2.0 (Apache-2.0 §5); no CLA is required.
Instead, sign off every commit with the **Developer Certificate of Origin**
(certifying you wrote it / may submit it):

```sh
git commit -s -m "your message"   # appends a Signed-off-by: line
```

CI enforces a `Signed-off-by` line on every commit in a PR.

## Source headers

New source files carry an SPDX header on line 1:

```rust
// SPDX-License-Identifier: Apache-2.0
```

## ACC conformance — keep the contract honest

agent is conformant to the ACC **by behaviour**, not by sharing code with the
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
cargo build -p agentd
cargo test  -p agentd --features "serve-mcp,a2a,events,metrics,cluster,hot-reload"
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo run -p agentd-conformance        # the black-box behavioral suite
```

By submitting a contribution you agree it is licensed under Apache-2.0 and that
you have signed off the DCO.
