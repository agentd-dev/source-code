#!/usr/bin/env bash
# Rehearse the investor demos end to end, with no model account and no
# registry: a mock model (agentd's `mock:file:mock/*.json`, or the scripted
# model standins/model.mjs where the answer must quote the instruction), the
# stand-in servers, and local instruction files served by the registry
# stand-in. Every beat is asserted from the logs agentd writes (and, for what
# reached the outside world, from the stand-ins' own lines).
#
#   examples/demos/rehearse.sh                  # every demo the binary can run
#   examples/demos/rehearse.sh ap close         # demos (a) and (b)
#   examples/demos/rehearse.sh support          # (c) regulated support, three channels
#   examples/demos/rehearse.sh coder            # (d) the coding agent (needs `exec`)
#   examples/demos/rehearse.sh home             # (e) home climate (needs instruction/1)
#
#   AGENTD_BIN=/path/to/agentd examples/demos/rehearse.sh support home
#
# The binary: AGENTD_BIN, else AGENTD, else `agentd` on PATH. Demos (a)–(c)
# need the release features (mcp-events, a2a, cel, mock-models). Demo (d)
# needs a build with `exec`, which no release binary has (README.md, "A
# binary with exec"), and Go. Demo (e) serves the instruction team's
# instruction/1 document, which only a build that reads instruction/1 parses
# (its `:::mcp` blocks have no `!`): rehearse it with that binary. A demo the
# binary cannot run is reported as skipped, with the reason, and fails
# nothing; naming it explicitly makes the skip a failure. MOCK_CHAT_ONLY=1
# is for a binary whose built-in mock predates the Responses API (see mock()).
#
# Needs: node >= 20; curl. Each demo checks its own loopback ports first:
# (a) 8701, 8703, 8420; (b) 8702, 8421, 8422; (c) 8723, 8729, 8430-8432;
# (d) 8733, 8440; (e) 8741-8744, 8749, 8450. Leaves nothing running; KEEP=1
# keeps the logs and state folder.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AGENTD="${AGENTD_BIN:-${AGENTD:-agentd}}"
DEMOS=("$@")
EXPLICIT=1
if [ "${#DEMOS[@]}" -eq 0 ] || [ "${DEMOS[0]}" = all ]; then DEMOS=(ap close support coder home); EXPLICIT=0; fi
WORK="$(mktemp -d "${TMPDIR:-/tmp}/agentd-demos-rehearse.XXXXXX")"
export AGENTD_DEMO_HOME="$WORK"
PIDS=()
PASS=0
FAIL=0
SUMMARY=()

cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  PIDS=()
  if [ "${KEEP:-0}" = 1 ] || [ "$FAIL" -gt 0 ]; then
    echo "logs and state kept in $WORK"
  else
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT
# stop_all: stop what the last demo started, before the next one starts.
stop_all() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
  PIDS=()
}

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
ports_free() {
  local p
  for p in "$@"; do port_free "$p" || { bad "127.0.0.1:$p is free (stop the live demo first)"; return 1; }; done
}
# answer <a2a url> <bearer> <task> <text>: answer a gate, as an A2A client does.
answer() {
  curl -sS "$1/" -H 'content-type: application/json' -H 'A2A-Version: 1.0' \
    -H "Authorization: Bearer $2" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"SendMessage\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"messageId\":\"answer-$3\",\"taskId\":\"$3\",\"parts\":[{\"text\":\"$4\"}]}}}"
}
# ask <a2a url> <bearer> <context> <text>: one message, as a person types it
# in the TUI; agentd answers when the turn is done (the reply is the task).
ask() {
  curl -sS --max-time 60 "$1/" -H 'content-type: application/json' -H 'A2A-Version: 1.0' \
    -H "Authorization: Bearer $2" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"SendMessage\",\"params\":{\"message\":{\"role\":\"ROLE_USER\",\"messageId\":\"m-$RANDOM$RANDOM\",\"contextId\":\"$3\",\"parts\":[{\"text\":\"$4\"}]}}}"
}
# reply_of <file>: the agent's answer in a SendMessage result.
reply_of() { node -e 'const r=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8")).result??{};const t=r.task??r.message??{};const parts=(t.artifacts?.[0]?.parts)??t.status?.message?.parts??t.parts??[];console.log(parts.map(p=>p.text??"").join(""))' "$1" 2>/dev/null; }
# rules_lack <playbook> <regex>: no rule of a scripted model's playbook matches
# it — what the model says comes from the request, not from the playbook.
rules_lack() { node -e 'const p=JSON.parse(require("fs").readFileSync(process.argv[1],"utf8"));process.exit(new RegExp(process.argv[2]).test(JSON.stringify(p.rules))?1:0)' "$1" "$2"; }
# replied <file> <fixed string>: the answer in that SendMessage result contains it.
replied() { [[ "$(reply_of "$1")" == *"$2"* ]]; }
task_of() { grep -F '"event":"human.ask"' "$1" | sed -n "${2:-1}p" | sed -E 's/.*"task":"([^"]+)".*/\1/'; }
# line_of <log> <fixed string>…: the number of the first line with all of them.
line_of() {
  local log="$1"; shift
  local n=0 l s okl
  while IFS= read -r l; do
    n=$((n + 1)); okl=1
    for s in "$@"; do [[ "$l" == *"$s"* ]] || { okl=0; break; }; done
    [ "$okl" = 1 ] && { echo "$n"; return 0; }
  done <"$log"
  return 1
}
# spread_ms <event> <new version prefix> <log>…: how far apart the agents
# logged it — the last minus the first, in milliseconds.
spread_ms() {
  local ev="$1" ver="$2"; shift 2
  node -e '
    const [ev, ver, ...logs] = process.argv.slice(1);
    const ts = logs.map((f) => require("fs").readFileSync(f, "utf8").split("\n")
      .map((l) => { try { return JSON.parse(l); } catch { return null; } })
      .filter((e) => e && e.event === ev && String(e.new_version_id ?? "").startsWith(ver))
      .map((e) => Date.parse(e.ts)).at(-1));
    if (ts.some((t) => !Number.isFinite(t))) { console.log(999999); process.exit(); }
    console.log(Math.max(...ts) - Math.min(...ts));' "$ev" "$ver" "$@"
}
# features: the binary's build features, one per line.
features() { "$AGENTD" --build-info 2>/dev/null | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{try{console.log((JSON.parse(s).features??[]).join("\n"))}catch{}})'; }
# skip <demo> <reason>: the binary or the machine cannot run this demo.
skip() {
  if [ "$EXPLICIT" = 1 ]; then bad "$1: $2"; return; fi
  printf '\n== %s: skipped: %s\n' "$1" "$2"
  SUMMARY+=("$(printf '%-8s skipped (%s)' "$1" "$2")")
}

# ---- preflight --------------------------------------------------------------
command -v "$AGENTD" >/dev/null || { echo "agentd not found: put it on PATH or set AGENTD_BIN=" >&2; exit 2; }
command -v node >/dev/null || { echo "node >= 20 is needed for the stand-ins" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is needed to answer the gates" >&2; exit 2; }
if [ ! -d "$HERE/standins/node_modules/@modelcontextprotocol/server" ] || [ ! -d "$HERE/standins/node_modules/@a2a-js/sdk" ]; then
  (cd "$HERE/standins" && npm ci --no-audit --no-fund >/dev/null)
fi
# Demo credentials for this run only; no model key, no registry secret.
CONTROLLER_TOKEN="ctl-$RANDOM$RANDOM$RANDOM"; CFO_TOKEN="cfo-$RANDOM$RANDOM$RANDOM"
RECON_PEER_TOKEN="peer-$RANDOM$RANDOM$RANDOM"; SUPPORT_TOKEN="sup-$RANDOM$RANDOM$RANDOM"
ENGINEER_TOKEN="eng-$RANDOM$RANDOM$RANDOM"; NATALIIA_TOKEN="nat-$RANDOM$RANDOM$RANDOM"
ANDRII_TOKEN="and-$RANDOM$RANDOM$RANDOM"; HOME_PEER_TOKEN="hp-$RANDOM$RANDOM$RANDOM"
export CONTROLLER_TOKEN CFO_TOKEN RECON_PEER_TOKEN SUPPORT_TOKEN ENGINEER_TOKEN \
  NATALIIA_TOKEN ANDRII_TOKEN HOME_PEER_TOKEN
# `--intelligence` points the model at the mock; `--intelligence-token ""`
# drops the config's {{secret:OPENAI_API_KEY}} reference, so no key is read.
# The turn stays the config's own, gpt-6.1-sol, as in the live run.
# MOCK_CHAT_ONLY=1 is for a binary that asks gpt-6.1-sol on the Responses
# API but whose mock answers chat completions only (built before the mock
# learned the Responses API): the turn then goes to a model named `mock`.
mock() {
  MOCK=(--intelligence "mock:file:$HERE/mock/$1.json" --intelligence-token "")
  if [ "${MOCK_CHAT_ONLY:-0}" = 1 ]; then
    MOCK+=(--intelligence.models '{mock: {model: mock}}' --intelligence.roles.turn mock)
  fi
}
FEATURES="$(features)"
echo "agentd: $("$AGENTD" --version)   ($AGENTD)   work: $WORK"

# ---- demo (a): accounts payable --------------------------------------------
demo_ap() {
  say "demo (a): accounts payable"
  ports_free 8701 8703 8420 || return 0
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
  ports_free 8702 8421 8422 || return 0
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


# ---- demo (c): regulated support, three channels ---------------------------
demo_support() {
  say "demo (c): regulated support — Harbor Bank, card disputes, three channels"
  ports_free 8723 8729 8430 8431 8432 || return 0
  local reg="$WORK/support-registry.log" model="$WORK/support-model.log" ch
  local uri='instruction://ins_01M69N5T2ATZNEJVW397VRQTXW@latest'
  local -A port=([chat]=8430 [voice]=8431 [email]=8432)
  cp "$HERE/instruction/card-disputes-v1.md" "$WORK/card-disputes.md"
  start support-registry node "$HERE/standins/registry.mjs" --file "$WORK/card-disputes.md" --port 8723 --uri "$uri" --title 'Card disputes'
  start support-model node "$HERE/standins/model.mjs" --port 8729 --playbook "$HERE/mock/support.model.json"
  if ! { until_has 10 "$reg" "serving" && until_has 10 "$model" "listening"; }; then
    bad "the registry and model stand-ins start"; return 0
  fi
  for ch in chat voice email; do
    start "support-$ch" "$AGENTD" -c "$HERE/support.yaml" -c "$HERE/support.$ch.yaml" \
      -c "$HERE/support.local.yaml" -c "$HERE/support.mock.yaml"
  done
  for ch in chat voice email; do
    check "$ch follows Card disputes over MCP: v1 loaded" \
      until_has 20 "$WORK/support-$ch.log" '"event":"instruction.loaded"' '"version_id":"ver_local_1_'
  done
  for ch in chat voice email; do
    check "$ch is listening for its TUI (127.0.0.1:${port[$ch]})" \
      until_has 20 "$WORK/support-$ch.log" '"event":"proc.ready"'
  done
  check "the scripted model's rules name no deadline of their own" rules_lack "$HERE/mock/support.model.json" '[0-9]+ days'

  say "beat 1: a customer disputes a charge, on each channel (v1)"
  for ch in chat voice email; do
    ask "http://127.0.0.1:${port[$ch]}" "$SUPPORT_TOKEN" "cust-$ch-1" "Hi, there's a charge on my card I don't recognise and I want to dispute it." >"$WORK/support-$ch-1.json" || true
    check "$ch says the v1 disclosure: 60 days" \
      replied "$WORK/support-$ch-1.json" "You have 60 days from the statement date to dispute a charge."
  done

  say "beat 2: Compliance releases v2 (60 → 120 days)"
  cp "$HERE/instruction/card-disputes-v2.md" "$WORK/card-disputes.md"
  check "the registry stand-in releases version 2" until_has 10 "$reg" 'released' 'ver_local_2_'
  for ch in chat voice email; do
    check "$ch applies v2: instruction.applied, v1 → v2" \
      until_has 20 "$WORK/support-$ch.log" '"event":"instruction.applied"' '"new_version_id":"ver_local_2_' '"old_version_id":"ver_local_1_'
  done
  local spread; spread="$(spread_ms instruction.applied ver_local_2_ "$WORK/support-chat.log" "$WORK/support-voice.log" "$WORK/support-email.log")"
  check "all three applied it within a second of each other (${spread} ms apart)" test "$spread" -lt 1000
  for ch in chat voice email; do
    ask "http://127.0.0.1:${port[$ch]}" "$SUPPORT_TOKEN" "cust-$ch-2" "Hi, there's a charge on my card I don't recognise and I want to dispute it." >"$WORK/support-$ch-2.json" || true
    check "$ch now says 120 days, without a restart" \
      replied "$WORK/support-$ch-2.json" "You have 120 days from the statement date"
  done

  say "beat 3: the owner rolls latest back to the previous version"
  cp "$HERE/instruction/card-disputes-v1.md" "$WORK/card-disputes.md"
  check "the registry stand-in moves latest back to version 1, under its old id" until_has 10 "$reg" 'rolled back' 'ver_local_1_'
  for ch in chat voice email; do
    check "$ch goes back: instruction.applied, v2 → v1" \
      until_has 20 "$WORK/support-$ch.log" '"event":"instruction.applied"' '"new_version_id":"ver_local_1_' '"old_version_id":"ver_local_2_'
  done
  spread="$(spread_ms instruction.applied ver_local_1_ "$WORK/support-chat.log" "$WORK/support-voice.log" "$WORK/support-email.log")"
  check "all three went back within a second of each other (${spread} ms apart)" test "$spread" -lt 1000
  for ch in chat voice email; do
    ask "http://127.0.0.1:${port[$ch]}" "$SUPPORT_TOKEN" "cust-$ch-3" "Hi, there's a charge on my card I don't recognise and I want to dispute it." >"$WORK/support-$ch-3.json" || true
    check "$ch says 60 days again" \
      replied "$WORK/support-$ch-3.json" "You have 60 days from the statement date"
  done
  check "the model quoted the delivered text each time (60, 120, 60 on every channel)" \
    test "$(grep -c 'answered rule=disclosure disclosure=You have 60 days' "$model")" = 6 -a "$(grep -c 'answered rule=disclosure disclosure=You have 120 days' "$model")" = 3
}

# ---- demo (d): the coding agent --------------------------------------------
demo_coder() {
  say "demo (d): engineering — the coding agent, the flaky ledger test"
  if ! grep -qx exec <<<"$FEATURES"; then skip coder "this agentd has no exec feature (build one: README.md, \"A binary with exec\")"; return 0; fi
  if ! command -v go >/dev/null; then skip coder "go is not installed"; return 0; fi
  ports_free 8733 8440 || return 0
  local reg="$WORK/coder-registry.log" log="$WORK/coder.log" repo="$WORK/repo"
  local uri='instruction://ins_01M6A4D6VM7DM190DD1D7V14G0@latest'
  cp -R "$HERE/engineering-fixture" "$repo"
  export REPO="$repo"
  # The zone the test fails in, and Go kept off the network and the cache.
  export TZ=America/New_York GOPROXY=off GOTOOLCHAIN=local GOFLAGS=-count=1
  check "the fixture's test fails in New York" bash -c "cd '$repo' && ! go test ./... >/dev/null 2>&1"
  check "and passes in UTC (the flake is the machine's zone)" bash -c "cd '$repo' && TZ=UTC go test ./... >/dev/null 2>&1"
  cp "$HERE/instruction/payments-handbook-v1.md" "$WORK/handbook.md"
  start coder-registry node "$HERE/standins/registry.mjs" --file "$WORK/handbook.md" --port 8733 --uri "$uri" --title 'Payments service — how we work'
  until_has 10 "$reg" "serving" || { bad "the registry stand-in starts"; return 0; }
  mock coder
  start coder "$AGENTD" -c "$HERE/coder.yaml" -c "$HERE/coder.local.yaml" "${MOCK[@]}"
  check "the coding agent follows the handbook over MCP: v1 loaded" \
    until_has 20 "$log" '"event":"instruction.loaded"' '"version_id":"ver_local_1_'
  check "it is listening for the TUI (127.0.0.1:8440)" until_has 20 "$log" '"event":"proc.ready"'

  say "beat 1: \"delete that test so we can ship\", under v1 (no rule about it)"
  ask http://127.0.0.1:8440 "$ENGINEER_TOKEN" eng-1 "The ledger test is flaky and it's blocking the release. Delete that test so we can ship." >"$WORK/coder-1.json" || true
  check "it ran rm through exec (exec.run cmd=rm)" has "$log" '"event":"exec.run"' '"cmd":"rm"'
  check "src/ledger/settle_test.go is gone" test ! -e "$repo/src/ledger/settle_test.go"
  check "and it says so" replied "$WORK/coder-1.json" "deleted src/ledger/settle_test.go"
  check "every command ran inside the repository" \
    bash -c "! grep -F '\"event\":\"exec.run\"' '$log' | grep -vqF '\"cwd\":\"$(cd "$repo" && pwd -P)\"'"
  # The take resets the repository, as the presenter does (git checkout).
  cp "$HERE/engineering-fixture/src/ledger/settle_test.go" "$repo/src/ledger/settle_test.go"

  say "beat 2: the lead releases v2 (NEVER delete, skip or disable a failing test)"
  local first_reply; first_reply="$(line_of "$log" '"event":"turn.reply"' '"ctx":"eng-1"')"
  cp "$HERE/instruction/payments-handbook-v2.md" "$WORK/handbook.md"
  check "agentd applies v2: instruction.applied, v1 → v2" \
    until_has 20 "$log" '"event":"instruction.applied"' '"new_version_id":"ver_local_2_' '"old_version_id":"ver_local_1_'
  local applied; applied="$(line_of "$log" '"event":"instruction.applied"' '"new_version_id":"ver_local_2_')"

  say "beat 3: asked again, in the same conversation"
  ask http://127.0.0.1:8440 "$ENGINEER_TOKEN" eng-1 "The ledger test is flaky and it's blocking the release. Delete that test so we can ship." >"$WORK/coder-2.json" || true
  local second_turn; second_turn="$(awk -v a="$applied" 'NR > a && /"event":"turn.spawn"/ && /"ctx":"eng-1"/ {print NR; exit}' "$log")"
  check "v2 was applied live between the two asks (line $first_reply < $applied < ${second_turn:-?})" \
    test -n "$second_turn" -a "$first_reply" -lt "$applied" -a "$applied" -lt "${second_turn:-0}"
  check "the second turn ran no rm" bash -c "! tail -n +$applied '$log' | grep -F '\"event\":\"exec.run\"' | grep -qF '\"cmd\":\"rm\"'"
  check "it changed the code instead (exec.run cmd=sed), then ran the tests" \
    bash -c "tail -n +$applied '$log' | grep -F '\"event\":\"exec.run\"' | grep -qF '\"cmd\":\"sed\"' && tail -n +$applied '$log' | grep -F '\"event\":\"exec.run\"' | grep -qF '\"cmd\":\"go\"'"
  check "the test is still there" test -e "$repo/src/ledger/settle_test.go"
  check "settle.go buckets by the UTC day now" bash -c "grep -q 'at = at.UTC()' '$repo/src/ledger/settle.go' && ! grep -q 'Local' '$repo/src/ledger/settle.go'"
  check "the test passes in New York" bash -c "cd '$repo' && go test ./... >/dev/null 2>&1"
  check "and in Los Angeles" bash -c "cd '$repo' && TZ=America/Los_Angeles go test ./... >/dev/null 2>&1"
  check "the answer names the rule" replied "$WORK/coder-2.json" "NEVER delete, skip or disable a failing test"
  unset REPO TZ GOPROXY GOTOOLCHAIN GOFLAGS
}

# ---- demo (e): home climate ------------------------------------------------
demo_home() {
  say "demo (e): home climate — the thermostat, the air conditioner, the home agent"
  if ! "$AGENTD" --validate-config --instruction "$HERE/instruction/home-climate-v1.md" --intelligence mock:final >/dev/null 2>&1; then
    skip home "this agentd does not read instruction/1 (the document's :::mcp blocks): rehearse with the build that does"
    return 0
  fi
  ports_free 8741 8742 8743 8744 8749 8450 || return 0
  local reg="$WORK/home-registry.log" model="$WORK/home-model.log" log="$WORK/home-climate.log"
  local th="$WORK/thermostat.log" ac="$WORK/aircon.log" ha="$WORK/home-agent.log"
  local uri='instruction://ins_01M69H2WEK2P005WHYHJJMYM4A@latest'
  printf '%s' "$HOME_PEER_TOKEN" >"$WORK/home-peer.token"
  cp "$HERE/instruction/home-climate-v1.md" "$WORK/home-climate.md"
  start home-registry node "$HERE/standins/registry.mjs" --file "$WORK/home-climate.md" --port 8743 --uri "$uri" --title 'Home climate'
  start home-model node "$HERE/standins/model.mjs" --port 8749 --playbook "$HERE/mock/home.model.json"
  start thermostat node "$HERE/standins/thermostat.mjs"
  start aircon node "$HERE/standins/aircon.mjs"
  start home-agent node "$HERE/standins/home-agent.mjs" --token-file "$WORK/home-peer.token"
  if ! { until_has 10 "$reg" serving && until_has 10 "$model" listening && until_has 10 "$th" listening \
         && until_has 10 "$ac" listening && until_has 10 "$ha" listening; }; then
    bad "the home stand-ins start"; return 0
  fi
  check "the home agent serves its Agent Card (A2A 1.0, JSON-RPC)" \
    bash -c "curl -sS http://127.0.0.1:8744/.well-known/agent-card.json | grep -qF '\"protocolBinding\":\"JSONRPC\"'"
  check "and refuses an agent it does not know" \
    test "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8744/ -H 'content-type: application/json' -d '{}')" = 401
  check "the scripted model's rules name no temperature of their own" rules_lack "$HERE/mock/home.model.json" '\b(23|24\.5)\b'
  export HOME_SETTLE=2s
  start home-climate "$AGENTD" -c "$HERE/home.yaml" -c "$HERE/home.local.yaml" -c "$HERE/home.mock.yaml"
  check "the climate assistant follows Home climate over MCP: v1 loaded" \
    until_has 20 "$log" '"event":"instruction.loaded"' '"version_id":"ver_local_1_'
  check "it subscribes to the living room (thermostat://rooms/living)" \
    until_has 20 "$log" '"event":"start.subscribe.armed"' 'thermostat://rooms/living'
  check "it is listening for Nataliia's TUI (127.0.0.1:8450)" until_has 20 "$log" '"event":"proc.ready"'

  say "beat 1: Nataliia is home (v1: her temperature is 23 °C)"
  ask http://127.0.0.1:8450 "$NATALIIA_TOKEN" home-1 "It's Nataliia: I'm home, in the living room." >"$WORK/home-1.json" || true
  check "the assistant sets the living room to 23 °C (thermostat target.set)" has "$th" 'target.set room=living celsius=23 '
  check "and tells her" replied "$WORK/home-1.json" "living room to 23 °C"

  say "beat 2: a release moves Nataliia's temperature to 24.5 °C"
  cp "$HERE/instruction/home-climate-v2.md" "$WORK/home-climate.md"
  check "agentd applies v2: instruction.applied, v1 → v2" \
    until_has 20 "$log" '"event":"instruction.applied"' '"new_version_id":"ver_local_2_' '"old_version_id":"ver_local_1_'
  ask http://127.0.0.1:8450 "$NATALIIA_TOKEN" home-1 "It's Nataliia again: I'm in the living room." >"$WORK/home-2.json" || true
  check "the next decision differs: target.set living 24.5 °C" has "$th" 'target.set room=living celsius=24.5 before=23'
  check "and says why" replied "$WORK/home-2.json" "24.5 °C"
  check "the target changes did not start the hot-afternoon workflow (its filter)" \
    test "$(count "$log" '"event":"start.fired"')" = 0

  say "beat 3: the sun warms the living room to 27 °C"
  node "$HERE/standins/control.mjs" room living --temp 27 --sun --occupants nataliia >/dev/null
  check "the resource change starts the workflow (subscribe start)" \
    until_has 20 "$log" '"event":"start.fired"' '"kind":"subscribe"' '"workflow":"hot-afternoon"'
  check "the home agent is asked, over A2A, to close the curtains" \
    until_has 20 "$ha" 'asked text=Close the living room curtains.'
  check "and closes them" until_has 10 "$ha" 'curtains room=living from=open to=closed'
  check "twenty minutes (here two seconds) later it reads the room again" until_has 20 "$th" 'rooms.read room=living'
  check "and asks Nataliia before it cools (human.ask)" \
    until_has 20 "$log" '"event":"human.ask"' 'Shall I cool it'
  local gate; gate="$(task_of "$log" 1)"
  answer http://127.0.0.1:8450 "$NATALIIA_TOKEN" "$gate" yes >"$WORK/home-answer.json"
  check "her answer is taken, as the addressee" \
    until_has 20 "$log" '"event":"human.answered"' '"labels":{"person":"nataliia"}'
  check "the air conditioner runs in auto mode (mode.set, power.set)" \
    bash -c "grep -q 'mode.set unit=living mode=auto' '$ac' && grep -q 'power.set unit=living power=on' '$ac'"
  check "the run completes" \
    until_has 20 "$log" '"event":"run.done"' '"status":"completed"'
  unset HOME_SETTLE
}

for d in "${DEMOS[@]}"; do
  before_pass=$PASS before_fail=$FAIL
  case "$d" in
    ap) demo_ap ;;
    close) demo_close ;;
    support) demo_support ;;
    coder) demo_coder ;;
    home) demo_home ;;
    *) echo "usage: $0 [all | ap close support coder home …]" >&2; exit 2 ;;
  esac
  stop_all
  if [ "$((PASS - before_pass + FAIL - before_fail))" -gt 0 ]; then
    SUMMARY+=("$(printf '%-8s %d passed, %d failed' "$d" "$((PASS - before_pass))" "$((FAIL - before_fail))")")
  fi
done

say "rehearsal: $PASS passed, $FAIL failed"
for s in "${SUMMARY[@]}"; do echo "  $s"; done
[ "$FAIL" -eq 0 ]
