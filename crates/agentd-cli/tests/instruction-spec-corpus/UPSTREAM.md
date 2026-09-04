# Vendored from the Instruction Document Spec repo

Upstream: **https://github.com/instruction-md/specification** (published `main`, tip `f1f800d`; the org's `instruction-md/source-code` stays private — this repo is the open split). Vendored at `f1f800d`; history was rewritten several times pre-publication so hashes are unstable — this file and the drift check key on CONTENT, never the id. Raw base: https://raw.githubusercontent.com/instruction-md/specification/main/ .

License provenance: the repo's root `LICENSE` is CC-BY-4.0 (spec text) and is what GitHub's repo badge reports; the conformance corpus vendored here is Apache-2.0, stated in `https://github.com/instruction-md/specification/blob/main/conformance/LICENSE` — cite that file, not the repo badge, for the corpus's license.

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

## A conformance claim carries its binary version

Both runners print the agentd version they drove, and refuse to report
per-fixture results for a binary that does not implement directive extraction
(it is named and the run stops). This exists because a green here and a red
elsewhere were both once true and neither named its binary: the gate builds
from THIS tree (extraction present); a stale machine install can be an earlier
era's binary that predates the feature. The installed `/usr/local/bin/agentd`
on the dev host was 2.2.0 (2026-08-18) — from the pre-1.x numbering, before
extraction landed in the tree (2026-08-23) — which is why it fails every
directive fixture for one reason. "The corpus is the arbiter" only holds when
the arbiter's verdict names the thing it judged.


## Sigiled-only migration (agentd leads)

agentd now implements the SINGLE sigiled dialect as the only instruction format
(the operator's decision: no legacy variant, breaking changes fine pre-1.0).
This vendored corpus has been re-authored for it — machinery carries `!`, an
unknown BARE name is inert prose, an unknown `:::!` is refused, and nesting
recurses (see fixture 012). The published upstream corpus still encodes the
two-dialect form; the byte drift check is `#[ignore]`d until the spec owner
republishes the corpus for the sigiled-only format. Re-enable it then.
