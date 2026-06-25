#!/usr/bin/env bash
# run-loop.sh — polling/work-until-done daemon: re-enter the instruction on a
# cadence until a bound (max iterations via steps, wall-clock deadline, or
# tree-wide token ceiling) or a drain signal is hit.
#
# Targets v1 behavior (see docs/design/PLAN.md). The binary today validates
# config + scaffold-notices run modes; the supervisor driver lands across
# M1-M3. All flags below exist in crates/agentd/src/config.rs.
#
# --interval D selects the re-entry cadence: D>0 polls every D; D=0 re-enters
# immediately on completion (work-until-done). Here we poll every 5 minutes.
# A wall-clock --deadline turns this into a bounded "Job with a deadline".

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

export AGENTD_INTELLIGENCE="${AGENTD_INTELLIGENCE:-unix:/run/intel.sock}"
# export AGENTD_INTELLIGENCE_TOKEN=...   # set in your environment, not here

exec "$AGENTD" \
  --mode loop \
  --interval 5m \
  --instruction-file "$(dirname "$0")/instructions/triage.md" \
  --model "claude-opus-4" \
  --mcp "inbox=mcp-server-inbox --queue /var/run/inbox" \
  --mcp "tickets=mcp-server-tickets --project OPS" \
  --max-steps 25 \
  --max-tokens 1000000 \
  --deadline 2h \
  --drain-timeout 25s \
  --log-level info
