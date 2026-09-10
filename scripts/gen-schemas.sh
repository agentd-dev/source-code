#!/usr/bin/env bash
# Emit the published JSON Schemas into web/public/schema/.
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

# Versioned by the DOCUMENT version each schema describes, so pinning
# `config_version: "1"` and pinning a schema URL are the same decision.
"$BIN" --config-schema   > "$OUT/config-1.json"
"$BIN" --workflow-schema > "$OUT/workflow-3.json"

# Unversioned aliases for "whatever this agentd speaks", which is what a
# modeline in a project's own config usually wants.
cp "$OUT/config-1.json"   "$OUT/config.json"
cp "$OUT/workflow-3.json" "$OUT/workflow.json"

# The site's workflow editor offers node kinds and fields from its own copy of
# `$defs.kinds`, enriched with a `category` and a `kind` the schema does not
# carry. Regenerate the schema-derived half and keep the enrichment, so the
# editor can never offer a kind the binary refuses — it had drifted to a
# different set of kinds entirely.
python3 - "$OUT/workflow-3.json" web/lib/workflow-nodes.json <<'PYGEN'
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

echo "wrote $OUT/{config-1,config,workflow-3,workflow}.json + web/lib/workflow-nodes.json"
