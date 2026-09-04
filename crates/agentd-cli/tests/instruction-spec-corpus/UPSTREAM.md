# Vendored from the Instruction Document Spec repo

Upstream: `/root/instruction-md/spec` (local-first by the licensor's choice;
public URL to follow) — vendored at commit `aa55b36` (history rewritten twice;
`b547d404` and `a4a46f1` no longer exist — this file keys on content, the
drift check always did). Upstream is uniformly author-, committer- and
in-document-dated 2026-07-18 at the licensor's instruction. One ordering fact,
recorded once in both sessions' logs and here: the spec cites agentd RFC 0034
(dated 2026-08-23) and RFC 0039 (2026-09-04) as its sources and carries an
earlier date than both; agentd's RFC dates are original and unchanged.

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

`core/015-duplicate-name-refused.*` (contributed from here, now upstream
verbatim) pins the identity rule's dialect-1 half — added after the rule
vanished from draft-1-rc through replace-editing with nothing failing in its
absence. Expected files carry `spec:` — the dialect a fixture is written
against; the gate SKIPS fixtures declaring a dialect this implementation does
not speak, which is what lets dialect-2 fixtures enter the shared corpus
without failing dialect-1 runtimes.
