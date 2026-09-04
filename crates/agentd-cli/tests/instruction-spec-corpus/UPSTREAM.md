# Vendored from the Instruction Document Spec repo

Upstream: `/root/instruction-md/spec` (local-first by the licensor's choice;
public URL to follow) — vendored at commit `b547d404`.

- `core/*.instruction.md` — verbatim upstream fixtures.
- `core/*.expected.json` — DERIVED locally from upstream's `*.expected.yaml`
  (the Rust gate test reads JSON to stay dependency-free); upstream's YAML is
  canonical.
- `registry/kinds.json` — verbatim upstream; the corpus gate test asserts its
  spec-1 entry equals the parser's own closed set (`known_kinds()`), so the
  registry is checked AGAINST the reference implementation, not beside it.

The gate test drift-checks vendored files against upstream whenever the
upstream path exists (override with `INSTRUCTION_SPEC_REPO`); when it does not
(CI, until the repo is published), the drift check skips and the behavioural
fixtures still run.
