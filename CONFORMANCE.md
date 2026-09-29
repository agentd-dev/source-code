# agentd conformance

agentd's conformance is judged by **behaviour**, against the specifications and
the documented contracts it serves — never by agreeing with its own types. The
evidence is the black-box suite in
[`crates/agentd-conformance`](crates/agentd-conformance): it knows agentd only
as a binary, drives the real process over its public surfaces, and never links
the agentd library, so a regression in what a caller sees fails a check even
when the implementation's own unit tests still agree with themselves.

This is agentd's own suite. It makes no claim about any external control
plane's contract.

## What it covers

Each check drives the real binary and asserts one contract. The checks are
grouped into families — the process's exit-code table and drain, the security
gates, the durable store, crash-and-restore durability, the internal tool
registry, the A2A conversation surface and the display-client surface. The list
is not repeated here, because a copy would drift: the families are the modules
of [`src/checks/`](crates/agentd-conformance/src/checks), and every check is
registered there as a `Check { id, category, desc, run }`, which both the test
harness and the report runner pick up.

Each check is host-independent: conformance is judged against the
specification, never the environment, so there are no capability-gated checks to
skip.

## Running it

```sh
cargo test -p agentd-conformance             # every check, one #[test] per family
cargo run  -p agentd-conformance             # the same checks as a PASS/FAIL report
cargo run  -p agentd-conformance -- --json   # the machine-readable record
```

The suite builds the agentd binary itself, so no prior `cargo build` is needed.
CI runs it as part of `cargo test --workspace --all-features`.

## Beside the suite

Two further checks hold the A2A surface to the specification rather than to
agentd's reading of it:

- the listener's method table is compared, name by name, with the constants of
  the official `a2a-rs` SDK, so a method agentd invented or misspelled fails, and
  so does a specification method it forgot to answer;
- every task, message and card agentd emits is serialized by the types that SDK
  generates from the A2A protobuf, so the wire shape is the schema's.

## Adding a check

Append a `Check` to the relevant family's `checks()`. Its `run` function takes
`&Harness` and returns an `Outcome` (`pass` / `note` / `fail` /
`require(cond, why)`); the tests and the runner pick it up automatically. A
change to a served surface — the exit codes, the A2A listener, the
configuration's admission rules, the store contract — comes with a check that
fails when the change is reverted.
