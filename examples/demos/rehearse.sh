#!/usr/bin/env bash
# Rehearse both investor demos end to end, with no model account and no
# registry: agentd's mock model (mock:file:mock/*.json), the stand-in MCP
# servers, and the local policy file served by the registry stand-in. Every
# beat is asserted from the logs agentd writes (and, for what reached the
# outside world, from the stand-ins' own lines).
#
#   examples/demos/rehearse.sh            # both demos
#   examples/demos/rehearse.sh ap         # demo (a) only
#   examples/demos/rehearse.sh close      # demo (b) only
#
# Needs: agentd on PATH (or AGENTD=/path/to/agentd), a build with the
# release features (mcp-events, a2a, cel, mock-models); node >= 20; curl.
# Uses ports 8701-8703 and 8420-8422 on 127.0.0.1, so stop a live demo first.
# Leaves nothing running; KEEP=1 keeps the logs and state folder.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENTD="${AGENTD:-agentd}"
WHICH="${1:-all}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/agentd-demos-rehearse.XXXXXX")"
export AGENTD_DEMO_HOME="$WORK"
PIDS=()
PASS=0
FAIL=0

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  if [ "${KEEP:-0}" = 1 ] || [ "$FAIL" -gt 0 ]; then
    echo "logs and state kept in $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

say()  { printf '\n== %s\n' "$*"; }
ok()   { PASS=$((PASS + 1)); printf '  ok    %s\n' "$*"; }
bad()  { FAIL=$((FAIL + 1)); printf '  FAIL  %s\n' "$*"; }
# check "<what>" <command…>: one assertion.
check() { local what="$1"; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }
# has <log> <fixed string>…: every string is on one line of the log.
has() {
  local log="$1"; shift
  local lines; lines="$(grep -F -- "$1" "$log" 2>/dev/null)" || return 1
  shift
  for s in "$@"; do lines="$(grep -F -- "$s" <<<"$lines")" || return 1; done
}
count() { grep -F -c -- "$2" "$1" 2>/dev/null || true; }
# until_has <seconds> <log> <fixed string>…: wait for such a line.
until_has() {
  local secs="$1"; shift
  local end=$((SECONDS + secs))
  while [ "$SECONDS" -lt "$end" ]; do has "$@" && return 0; sleep 0.2; done
  return 1
}
# start <name> <command…>: run in the background, its output in $WORK/<name>.log.
start() {
  local name="$1"; shift
  "$@" >"$WORK/$name.log" 2>&1 &
  PIDS+=("$!")
}
port_free() { ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
# answer <a2a url> <bearer> <task> <text>: answer a gate, as an A2A client does.
answer() {
  curl -sS "$1/" -H 'content-type: application/json' -H 'A2A-Version: 1.0' \
    -H "Authorization: Bearer $2" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"SendMessage\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"messageId\":\"answer-$3\",\"taskId\":\"$3\",\"parts\":[{\"text\":\"$4\"}]}}}"
}
task_of() { grep -F '"event":"human.ask"' "$1" | sed -n "${2:-1}p" | sed -E 's/.*"task":"([^"]+)".*/\1/'; }

# ---- preflight --------------------------------------------------------------
command -v "$AGENTD" >/dev/null || { echo "agentd not found: put it on PATH or set AGENTD=" >&2; exit 2; }
command -v node >/dev/null || { echo "node >= 20 is needed for the stand-ins" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is needed to answer the gates" >&2; exit 2; }
for p in 8701 8702 8703 8420 8421 8422; do
  port_free "$p" || { echo "127.0.0.1:$p is in use: stop the live demo first" >&2; exit 2; }
done
if [ ! -d "$HERE/standins/node_modules/@modelcontextprotocol/server" ]; then
  (cd "$HERE/standins" && npm ci --no-audit --no-fund >/dev/null)
fi
# Demo credentials for this run only; no model key, no registry secret.
CONTROLLER_TOKEN="ctl-$RANDOM$RANDOM$RANDOM"; CFO_TOKEN="cfo-$RANDOM$RANDOM$RANDOM"
RECON_PEER_TOKEN="peer-$RANDOM$RANDOM$RANDOM"
export CONTROLLER_TOKEN CFO_TOKEN RECON_PEER_TOKEN
# `--intelligence` points the model at the mock; `--intelligence-token ""`
# drops the config's {{secret:OPENAI_API_KEY}} reference, so no key is read.
mock() { MOCK=(--intelligence "mock:file:$HERE/mock/$1.json" --intelligence-token ""); }
echo "agentd: $("$AGENTD" --version)   work: $WORK"

# ---- demo (a): accounts payable --------------------------------------------
demo_ap() {
  say "demo (a): accounts payable"
  local ns="$WORK/payables.log" reg="$WORK/registry.log" ap="$WORK/ap-agent.log"
  cp "$HERE/instruction/ap-policy-v1.md" "$WORK/ap-policy.md"
  start payables node "$HERE/standins/payables.mjs"
  start registry node "$HERE/standins/registry.mjs" --file "$WORK/ap-policy.md"
  if ! { until_has 10 "$ns" "listening" && until_has 10 "$reg" "serving"; }; then
    bad "the stand-ins start"; return
  fi
  mock ap-agent
  start ap-agent "$AGENTD" -c "$HERE/ap-agent.yaml" -c "$HERE/ap-agent.local.yaml" "${MOCK[@]}"

  check "agentd follows the policy over MCP: v1 loaded" \
    until_has 20 "$ap" '"event":"instruction.loaded"' '"version_id":"ver_local_1_'
  check "the invoice.created binding is armed (MCP Events, poll)" \
    until_has 20 "$ap" '"event":"mcp.events.armed"' '"event_type":"invoice.created"'
  check "the md.instruction.activity type is followed for the policy" \
    until_has 20 "$ap" '"event":"mcp.events.armed"' '"event_type":"md.instruction.activity"'

  say "beat 1: an invoice under the limit (INV-2041, GBP 3,240.00)"
  node "$HERE/standins/control.mjs" invoice >/dev/null
  check "the event starts a run (stream start)" \
    until_has 20 "$ap" '"event":"start.fired"' '"event_id":"mcp/payables/invoice.created/evt-INV-2041-'
  check "decided: approved, and recorded in payables (invoices.approve)" \
    until_has 20 "$ns" 'invoices.approve invoiceId=INV-2041'
  check "the run ends at its 'approved' finish" until_has 10 "$ap" '"event":"step.start"' '"step":"approved"'

  say "beat 2: an invoice above the limit (INV-2042, GBP 18,500.00)"
  node "$HERE/standins/control.mjs" invoice >/dev/null
  check "decided: escalate, put on hold in payables (invoices.hold)" \
    until_has 20 "$ns" 'invoices.hold invoiceId=INV-2042' 'awaiting controller approval'
  check "the human gate is raised above the limit (human.ask)" \
    until_has 20 "$ap" '"event":"human.ask"' 'INV-2042'
  local gate; gate="$(task_of "$ap" 1)"
  check "the gate is a task on the A2A listener" test -n "$gate"
  check "nothing approved INV-2042 before a person answered" bash -c "! grep -qF 'invoices.approve invoiceId=INV-2042' '$ns'"
  answer http://127.0.0.1:8420 "$CONTROLLER_TOKEN" "$gate" approve >"$WORK/answer-ap.json"
  check "the controller's answer is taken, as the addressee (human.answered)" \
    until_has 20 "$ap" '"event":"human.answered"' '"labels":{"approver":"controller"}'
  check "and only then payables records the approval" \
    until_has 20 "$ns" 'invoices.approve invoiceId=INV-2042' 'financial controller'
  check "the run ends at its 'approved_by_person' finish" \
    until_has 10 "$ap" '"event":"step.start"' '"step":"approved_by_person"'

  say "beat 3: the controller releases v2 (approval limit GBP 25,000)"
  cp "$HERE/instruction/ap-policy-v2.md" "$WORK/ap-policy.md"
  check "the registry stand-in releases version 2" until_has 10 "$reg" 'released' 'ver_local_2_'
  check "agentd applies v2: instruction.applied, v1 → v2" \
    until_has 20 "$ap" '"event":"instruction.applied"' '"new_version_id":"ver_local_2_' '"old_version_id":"ver_local_1_'

  say "beat 4: the same amount again (INV-2043, GBP 18,500.00), under v2"
  node "$HERE/standins/control.mjs" invoice >/dev/null
  check "decided differently: approved by the agent, no hold" \
    until_has 20 "$ns" 'invoices.approve invoiceId=INV-2043' 'GBP 25,000'
  sleep 1
  check "the run ends at its 'approved' finish (the second in the demo)" \
    test "$(grep -F '"event":"step.start"' "$ap" | grep -cF '"step":"approved"')" = 2
  check "no gate was raised for INV-2043 (one gate in the whole demo)" test "$(count "$ap" '"event":"human.ask"')" = 1
  check "INV-2043 was never put on hold" bash -c "! grep -qF 'invoices.hold invoiceId=INV-2043' '$ns'"
  check "the deciding model held no write tool (no agent-made approve/hold)" \
    bash -c "! grep -F '\"event\":\"tool.call\"' '$ap' | grep -qE 'invoices\\.(approve|hold)'"
}

# ---- demo (b): month-end close ---------------------------------------------
demo_close() {
  say "demo (b): month-end close"
  local erp="$WORK/erp.log" recon="$WORK/recon-agent.log" close="$WORK/close-agent.log"
  start erp node "$HERE/standins/erp.mjs"
  until_has 10 "$erp" "listening" || { bad "the ERP stand-in starts"; return; }
  mock recon-agent
  start recon-agent "$AGENTD" -c "$HERE/recon-agent.yaml" "${MOCK[@]}"
  mock close-agent
  start close-agent "$AGENTD" -c "$HERE/close-agent.yaml" "${MOCK[@]}"

  check "the recon agent is up, its a2a start armed" \
    until_has 20 "$recon" '"event":"proc.ready"'
  check "the close agent subscribes to erp://bank-feed/today" \
    until_has 20 "$close" '"event":"start.subscribe.armed"' 'erp://bank-feed/today'

  say "beat 1: bank lines land in the feed (four, one update)"
  node "$HERE/standins/control.mjs" bankline --count 4 >/dev/null
  check "the resource subscription fires the workflow (subscribe start)" \
    until_has 20 "$close" '"event":"start.fired"' '"kind":"subscribe"'
  check "the close agent delegates to the recon agent over A2A" \
    until_has 30 "$recon" '"event":"run.start"' '"workflow":"unmatched-lines"'
  check "the recon agent reads the ERP and answers" \
    until_has 30 "$recon" '"event":"run.done"' '"workflow":"unmatched-lines"' '"status":"completed"'
  check "the A2A step completes in the close agent" \
    until_has 30 "$close" '"event":"step.done"' '"step":"unmatched"' '"status":"done"'

  say "beat 2: drafts, never a post"
  check "journal.draft is called, one per unmatched line" until_has 30 "$erp" 'journal.draft journalId=JE-DRAFT-004'
  check "four drafts in the ERP" test "$(count "$erp" 'journal.draft journalId=')" = 4
  check "the model's journal.post attempt is refused inside agentd" \
    until_has 20 "$close" '"event":"tool.result"' '"is_error":true' '"tool":"erp.journal.post"'
  check "the ERP never received journal.post" bash -c "! grep -qF 'journal.post' '$erp'"

  say "beat 3: the review gate"
  check "the review gate is raised for the controller (human.ask)" \
    until_has 20 "$close" '"event":"human.ask"' 'Month-end close'
  local gate; gate="$(task_of "$close" 1)"
  answer http://127.0.0.1:8421 "$CONTROLLER_TOKEN" "$gate" reviewed >"$WORK/answer-close.json"
  check "the controller's review is taken, as the addressee (human.answered)" \
    until_has 20 "$close" '"event":"human.answered"' '"labels":{"approver":"controller"}'
  check "the run completes at its finish" \
    until_has 20 "$close" '"event":"run.done"' '"workflow":"month-end-close"' '"status":"completed"'
  check "still nothing posted" bash -c "! grep -qF 'journal.post' '$erp'"
}

case "$WHICH" in
  all) demo_ap; demo_close ;;
  ap) demo_ap ;;
  close) demo_close ;;
  *) echo "usage: $0 [all|ap|close]" >&2; exit 2 ;;
esac

say "rehearsal: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
