#!/usr/bin/env bash
# run-reactive.sh — event-reactive daemon: idle until an MCP resource changes,
# wake, triage it, go back to idle. Never exits on its own (a drain signal or a
# fatal/limit class stops it) — deploy it as a long-lived Deployment.
#
# Targets v1 behavior (see docs/design/PLAN.md). Reactivity is STDIO-ONLY in v1
# (reactive-over-HTTP is roadmap, RFC 0013) — the subscribed servers below are
# stdio MCP servers. The binary currently validates config + scaffold-notices
# run modes; the reactor + router land across M1-M3.
#
# --mode reactive REQUIRES at least one --subscribe <uri> (config validates this
# at startup and exits 2 otherwise). All flags below exist in config.rs.

set -euo pipefail

AGENTD="${AGENTD:-agent}"

export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-unix:/run/intel.sock}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

# A reactive daemon should bound its cumulative cost: --max-tokens / --deadline
# here are tree-wide and lifetime-scoped (the budget is the ultimate
# backpressure). A high token ceiling + no hard deadline is typical for a
# kept-alive Deployment; tune to taste.
exec "$AGENTD" \
  --mode reactive \
  --instruction-file "$(dirname "$0")/instructions/triage.md" \
  --model "claude-opus-4" \
  --mcp "inbox=mcp-server-inbox --queue /var/run/inbox" \
  --mcp "tickets=mcp-server-tickets --project OPS" \
  --subscribe "inbox:///items/new" \
  --max-steps 25 \
  --max-tokens 2000000 \
  --health-file /run/agent/health \
  --drain-timeout 25s \
  --log-level info
