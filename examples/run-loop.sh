#!/usr/bin/env bash
# run-loop.sh — the triage agent on a 5-minute cadence: wake, triage whatever is
# waiting, sleep, repeat. Never exits on its own (a drain signal or a fatal/limit
# class stops it) — deploy it as a long-lived Deployment.
#
# The cadence is a `loop` start node, which fires again each time the previous
# run FINISHES — so `interval` is the gap between runs, not a wall-clock
# schedule, and two runs never overlap. `--mode loop --interval 5m` was the 1.x
# spelling of this; modes were removed in 2.0 and the shape they encoded became
# a start node. See docs/modes-and-triggers.md.

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-https://gw.example/v1}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

# A long-lived agent should bound its cumulative cost: --max-tokens and
# --deadline are tree-wide and lifetime-scoped (the budget is the ultimate
# backpressure). Everything else — the loop, the servers, the instruction —
# lives in the config beside this script.
exec "$AGENTD" \
  --config "$(dirname "$0")/loop-triage.yaml" \
  --max-tokens 1000000 \
  --deadline 2h \
  --log-level info
