# Instruction Document Spec — conformance corpus (vendored)

Fixtures for the draft Instruction Document Spec (see `rfcs/0034`, `rfcs/0039`
and the spec skeleton being aligned with instruction.md, its intended owner).
Each case is a bare instruction document plus its expected observable outcome:
validity, error substrings, and what registers. `instruction_spec_corpus.rs`
runs every case against the real binary, so these pin dialect-1 behaviour as
executable contract. When the spec repo exists upstream, this directory becomes
a vendored copy of it and CI diffs against upstream.

Licensing (approved by the licensor, 2026-09-04): the spec text is CC-BY 4.0;
the conformance corpus — these fixtures and expected outcomes — is
**Apache-2.0**, which flows one-way into this repository's AGPL-3.0-only.

Authoring note: dialect-2 fixtures contain `:::!` fences, and `!` triggers
history expansion in interactive bash/zsh — a fixture authored through a
double-quoted shell string will corrupt. Author fixtures as files, via quoted
heredocs (`<<'EOF'`), or in single quotes.
