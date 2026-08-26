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

echo "wrote $OUT/{config-1,config,workflow-3,workflow}.json"
