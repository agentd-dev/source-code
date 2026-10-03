# What is vendored, and from where

Nothing in this directory is vendored any more: the probes are agentd's (see
`README.md`). What agentd does vendor from the specification is its registry,
and the checks that keep that copy faithful run from
`instruction_spec_corpus.rs`, beside these probes.

Upstream: **https://github.com/instruction-md/specification**. The schema is
vendored from revision `345f27588f5982cbe530254b82aba4ed90a34292`
(specification revision 1.1). CI checks out that exact sha
(`.github/workflows/ci.yml`); moving the pin is a deliberate change, made in
the same commit as a re-vendor.

License provenance: the specification text is CC-BY-4.0, which is what
GitHub's repo badge reports. The probe fixtures here, derived from the
behavioural fixtures upstream published under Apache-2.0, keep that license,
which flows one-way into this repository's AGPL-3.0-only.

## The registry is the vendored JSON Schema

agentd does not transcribe the registry into Rust. The `agentd-instruction`
crate vendors the specification's own `instruction.schema.json`, verbatim, at
`crates/instruction/src/instruction.schema.json`, and reads the kinds, forms,
bodies, keywords and grants from its `x-registry` and `$defs.kinds`. A kind,
form or grant therefore cannot drift from the specification: there is one copy
of the registry, and it is the normative one.

## The drift checks

- `the_schema_registry_agrees_with_the_parser` (`instruction_spec_corpus.rs`)
  checks the schema's two views of its machinery set agree (the flat
  `x-registry.machinery` list vs the per-kind `$defs.kinds.*.x-disposition`),
  and that the names refused bare are exactly `x-registry.reserved-bare`.
- `the_vendored_schema_matches_upstream_when_present`
  (`instruction_spec_corpus.rs`) compares the WHOLE vendored schema
  semantically (as parsed JSON) against upstream's — a reformat is not a
  false alarm, any real change is.
- `the_vendored_corpus_matches_upstream_when_present`
  (`crates/instruction/tests/corpus.rs`) does the same for the crate's
  vendored conformance tree.

The two upstream checks run against `INSTRUCTION_SPEC_REPO`, else the
maintainer default path `/root/instruction-md/specification`, and skip only
when the variable is unset and nothing is at that path. An EXPLICIT
`INSTRUCTION_SPEC_REPO` that has no schema fails rather than skips. CI sets it
to the pinned checkout, and both CI's `spec drift (pinned)` step and
`scripts/ci-gate.sh` run them by name and fail a run in which either skipped
(`release_matrix.rs` holds both gates to that) — a drift check that skips
reports health it never performed.
