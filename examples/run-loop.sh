#!/usr/bin/env bash
# run-loop.sh — polling/work-until-done daemon: re-enter the instruction on a
# cadence until a bound (max iterations via steps, wall-clock deadline, or
# tree-wide token ceiling) or a drain signal is hit.
#
# All flags below exist in crates/agentd/src/config.rs. Tools come from remote
# MCP servers reached over Streamable HTTP — agentd runs no local code of its own.
#
# --interval D selects the re-entry cadence: D>0 polls every D; D=0 re-enters
# immediately on completion (work-until-done). Here we poll every 5 minutes.
# A wall-clock --deadline turns this into a bounded "Job with a deadline".

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-https://gw.example/v1}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

exec "$AGENTD" \
  --mode loop \
  --interval 5m \
  --instruction-file "$(dirname "$0")/instructions/triage.md" \
  --model "claude-opus-4" \
  --mcp inbox=https://mcp-inbox.internal/mcp \
  --mcp tickets=https://mcp-tickets.internal/mcp \
  --max-steps 25 \
  --max-tokens 1000000 \
  --deadline 2h \
  --drain-timeout 25s \
  --log-level info
