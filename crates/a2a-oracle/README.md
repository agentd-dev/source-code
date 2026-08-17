# a2a-oracle — a second reader for the A2A specification

agentd's A2A server is hand-written. The risk that carries is not a loud
failure — the conformance suite covers those — but a *plausible* misreading of
the spec: a field named the way we assumed, an enum spelled the way we guessed.
Our own tests cannot catch it, because they were written from the same reading
of the spec as the implementation. A peer built from the schema would simply
fail to parse us, in production, with no useful error.

So this crate boots the real `agentd` binary, drives its real A2A listener over
JSON-RPC, and hands every response to [a2a-rs] — an unrelated Rust
implementation of the same specification, by a different author, with types
derived from the published schema.

Deserialization succeeding means two independent readings agree on what went
over the wire. Deserialization failing means one of us is wrong, and the error
says which field.

```
cargo test --manifest-path crates/a2a-oracle/Cargo.toml
```

## Why it is not part of the workspace

a2a-rs pulls ~180 crates and needs `cmake` (via `aws-lc-sys`). That is fine for
a check you run deliberately and wrong for the shipped binary, which is a
three-dependency static musl build on `FROM scratch`. So this is its own
workspace, excluded from the parent — which also gives it its own target dir, so
its build of `agentd` cannot overwrite the one the conformance suite and the CLI
tests drive with a different feature set.

The findings are folded back into the ordinary CI path as conformance checks
(`a2a-conversation/tasks-are-proto3-json-on-every-path`) and unit tests, so a
regression is caught without the C toolchain. This crate is what *discovers*
them.

## What it found

The first run was not clean. agentd was emitting `"role": "agent"` where proto3
JSON wants `ROLE_AGENT`, epoch milliseconds where `google.protobuf.Timestamp`
wants an RFC 3339 string, its internal task record from `ListTasks` instead of a
`Task`, and state transitions in `history`, which is `repeated Message`. All of
it was valid JSON. None of it was readable by a peer.

[a2a-rs]: https://github.com/emillindfors/a2a-rs
