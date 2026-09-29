#!/usr/bin/env bash
# Emit the published JSON Schemas into web/public/schema/, and the A2A
# extension registry and schema bundles into web/lib/ and web/public/a2a/.
#
# They are GENERATED from the binary — the same functions the validator uses —
# and committed, because the site build has no Rust toolchain. CI regenerates
# and diffs, so a schema can never drift from the code it describes: a schema
# that disagrees with the loader is worse than none, since an editor then
# reports valid documents as broken.
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${AGENTD_BIN:-./target/debug/agentd}"
[ -x "$BIN" ] || { echo "no agentd binary at $BIN (cargo build -p agentd-cli --all-features)" >&2; exit 1; }

OUT=web/public/schema
mkdir -p "$OUT"

# One schema per document, each at the URL its `$id` names. Whatever else is in
# the directory is gone first, so a schema the binary no longer emits cannot
# linger at a URL an editor still fetches.
rm -f "$OUT"/config*.json "$OUT"/workflow*.json
"$BIN" --config-schema   > "$OUT/config.json"
"$BIN" --workflow-schema > "$OUT/workflow.json"

# The site's workflow editor offers node kinds and fields from its own copy of
# `$defs.kinds`, enriched with a `category` and a `kind` the schema does not
# carry. Regenerate the schema-derived half and keep the enrichment, so the
# editor can never offer a kind the binary refuses — it had drifted to a
# different set of kinds entirely.
python3 - "$OUT/workflow.json" web/lib/workflow-nodes.json <<'PYGEN'
import json, sys
kinds = json.load(open(sys.argv[1]))["$defs"]["kinds"]
try:
    old = json.load(open(sys.argv[2]))
except OSError:
    old = {}
out = {}
for k in sorted(kinds):
    entry = dict(kinds[k])
    prev = old.get(k, {})
    entry["kind"] = k
    if "category" in prev:
        entry["category"] = prev["category"]
    out[k] = {kk: entry[kk] for kk in sorted(entry)}
json.dump(out, open(sys.argv[2], "w"), indent=2, sort_keys=True)
open(sys.argv[2], "a").write("\n")
missing = [k for k in out if "category" not in out[k]]
if missing:
    print(f"  NOTE: new node kind(s) need a category in web/lib/workflow-nodes.json: {missing}", file=sys.stderr)
PYGEN

# Every A2A extension and binding URI agentd publishes, and each extension's
# schema bundle at `<uri>/schema.json`: A2A's extension guidance says a
# third-party extension's spec should be hosted at its URI, so the site serves
# the spec page at the URI's path and the bundle beside it. The list comes from
# the binary too (`--extensions`), so a new extension is published by adding
# it to the registry — nothing here names one. A stale bundle is removed
# first, like the schemas above; the hand-written examples beside each one are
# not generated and are kept.
rm -f web/public/a2a/ext/*/schema.json
"$BIN" --extensions > web/lib/extensions.json
python3 - "$BIN" web/lib/extensions.json <<'PYEXT'
import json, os, subprocess, sys
binary, registry = sys.argv[1], sys.argv[2]
for entry in json.load(open(registry)):
    if not entry["schema"]:
        continue  # a binding: a spec page, no bundle
    name = entry["path"].rsplit("/", 1)[-1]
    out = os.path.join("web/public", entry["path"], "schema.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        subprocess.run([binary, "--extension-schema", name], stdout=f, check=True)
PYEXT

echo "wrote $OUT/{config,workflow}.json + web/lib/workflow-nodes.json + web/lib/extensions.json + web/public/a2a/ext/*/schema.json"
