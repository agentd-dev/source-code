# Vendored from the Instruction Document Spec repo

Upstream: **https://github.com/instruction-md/specification**, pinned at
revision `345f27588f5982cbe530254b82aba4ed90a34292` — the revision the vendored
schema and conformance fixtures were copied from. The org's
`instruction-md/source-code` stays private — this repo is the open split. CI
checks out that exact sha (`.github/workflows/ci.yml`); moving the pin is a
deliberate change, made in the same commit as a re-vendor. Raw base:
https://raw.githubusercontent.com/instruction-md/specification/345f27588f5982cbe530254b82aba4ed90a34292/ .

License provenance: the repo's root `LICENSE` is CC-BY-4.0 (spec text), which is
what GitHub's repo badge reports. The behavioural fixtures vendored here are
Apache-2.0 — cite the corpus's own license file, not the repo badge.

## The registry is the vendored JSON Schema

The reference implementation does not transcribe the registry into Rust. It
**vendors the spec's own `instruction.schema.json`** (at
`crates/instruction/src/instruction.schema.json`) and reads the
kinds, forms, bodies and grants from its `x-registry` and `$defs.kinds`. A
kind, form or grant therefore cannot drift from the specification: there is one
copy of the registry, and it is the normative one.

- `core/*.instruction.md` — verbatim upstream fixtures.
- `core/*.expected.json` — DERIVED locally from upstream's `*.expected.yaml`
  (the Rust gate test reads JSON to stay dependency-free); upstream's YAML is
  canonical.
- The vendored schema — verbatim upstream. Two gate tests guard it:
  `the_schema_registry_agrees_with_the_parser` checks the schema's two views of
  its machinery set agree (the flat `x-registry.machinery` list vs the per-kind
  `$defs.kinds.*.x-disposition`), and `the_vendored_schema_matches_upstream_when_present`
  compares the WHOLE vendored schema SEMANTICALLY against upstream (as parsed
  JSON) — a reformat is not a false alarm, any real change is.

The drift checks (this one, and `the_vendored_corpus_matches_upstream_when_present`
over the crate's `tests/conformance/` tree) run against `INSTRUCTION_SPEC_REPO`,
else a sibling checkout at the default path, and skip only when neither exists.
An EXPLICIT `INSTRUCTION_SPEC_REPO` that has no schema fails rather than skips.
CI sets it to the pinned checkout, and both CI's `spec drift (pinned)` step and
`scripts/ci-gate.sh` fail a run in which either test skipped — a drift check
that skips reports health it never performed.

## A conformance claim carries its binary version

Both runners print the agentd version they drove, and refuse to report
per-fixture results for a binary that does not implement directive extraction
(it is named and the run stops). This exists because a green here and a red
elsewhere were both once true and neither named its binary: the gate builds
from THIS tree (extraction present); a stale machine install can be an earlier
era's binary that predates the feature. "The corpus is the arbiter" only holds
when the arbiter's verdict names the thing it judged.

## agentd-authored forms probes (`019`–`022`)

Fixtures `019`–`022` are authored here, not vendored — small black-box probes
of the §4 forms against agentd's real config: a leaf-form `!mcp` (a document
with no `:::` line at all, which the loader must still recognize), a table set,
a section-form `!workflow`, and the section boundary (a YAML section ends at
its code fence, so a leaf that follows is top-level, not swallowed). Their
workflow bodies use agentd's own step shape, so they exercise the forms without
depending on the spec's illustrative config vocabulary.

## One format, version 1 (sigiled)

The spec is a single format — the sigiled dialect, numbered 1. Each fixture
declares the `grants:` it needs (default none) so the trust ladder's
fail-closed guarantee (fixture 018) is actually exercised. Expected files carry
`spec:` — the version a fixture is written against; the gate SKIPS fixtures
declaring a version this implementation does not speak.
