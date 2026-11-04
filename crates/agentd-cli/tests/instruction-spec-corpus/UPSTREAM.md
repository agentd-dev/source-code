# What is pinned, and from where

Nothing in this directory is vendored: the probes are agentd's (see
`README.md`). agentd reads instruction documents with the official
instruction.md Rust SDK, `instruction-md`, which vendors the specification's
registry (`registry.json`) at one commit, `instruction_md::SPEC_COMMIT`.

Upstream: **https://github.com/instruction-md/specification**. The SDK's
registry is from commit `bf4f9a0db62c79ee1365edec47fb6ffdcfd623bb` (the first
edition, `instruction/1`). CI checks out that exact commit
(`.github/workflows/ci.yml`); moving the pin is a deliberate change, made
with the SDK revision that vendors it.

## The drift checks

- `the_sdk_reads_the_pinned_specification` (`instruction_spec_corpus.rs`)
  holds ci.yml's pin to the SDK's `SPEC_COMMIT`.
- `the_vendored_schema_matches_upstream_when_present`
  (`instruction_spec_corpus.rs`) compares the SDK's registry semantically (as
  parsed JSON) with upstream's `registry.json` at the pin — a reformat is not
  a false alarm, any real change is.

The upstream check runs against `INSTRUCTION_SPEC_REPO`, else the maintainer
default path `/root/instruction-md/specification`, and skips only when the
variable is unset and nothing is at that path. An EXPLICIT
`INSTRUCTION_SPEC_REPO` that has no registry fails rather than skips. CI sets
it to the pinned checkout, and both CI's `spec drift (pinned)` step and
`scripts/ci-gate.sh` run it by name and fail a run in which it skipped
(`release_matrix.rs` holds both gates to that).
