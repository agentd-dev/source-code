# Vendored from the Instruction Document Spec repo

Upstream: `/root/instruction-md/spec` (local-first by the licensor's choice;
public URL to follow) — vendored at commit `a4a46f1` (its parent `62f2dd6`
amends and replaces the original re-home `b547d404`; upstream commits are
author-dated 2026-07-18 at the licensor's instruction, while the work and the
in-document approval dates are 2026-09-04 — recorded here so the discrepancy
is legible, since this checkout's history is the provenance record until the
signing machinery of SPEC §6 exists).

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

Local addition offered upstream: `core/015-duplicate-name-refused.*` pins the
identity rule's dialect-1 half (duplicate `name` per kind is refused) — added
after the rule itself was found to have vanished from draft-1-rc through
replace-editing, with nothing failing in its absence. The fixture is that
failure detector.
