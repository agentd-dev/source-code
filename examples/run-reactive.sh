#!/usr/bin/env bash
# run-reactive.sh — event-reactive daemon: idle until an MCP resource changes,
# wake, triage it, go back to idle. Never exits on its own (a drain signal or a
# fatal/limit class stops it) — deploy it as a long-lived Deployment.
#
# Reactivity rides the MCP servers' Streamable-HTTP subscriptions: agentd
# subscribes to the resources below and reacts to pushed
# notifications/resources/updated over HTTP/SSE. The subscribed servers are
# remote HTTP endpoints. All flags below exist in crates/agentd/src/config.rs.
#
# --mode reactive REQUIRES at least one --subscribe <uri> (config validates this
# at startup and exits 2 otherwise). All flags below exist in config.rs.

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-https://gw.example/v1}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

# A reactive daemon should bound its cumulative cost: --max-tokens / --deadline
# here are tree-wide and lifetime-scoped (the budget is the ultimate
# backpressure). A high token ceiling + no hard deadline is typical for a
# kept-alive Deployment; tune to taste.
exec "$AGENTD" \
  --mode reactive \
  --instruction-file "$(dirname "$0")/instructions/triage.md" \
  --model "claude-opus-4" \
  --mcp inbox=https://mcp-inbox.internal/mcp \
  --mcp tickets=https://mcp-tickets.internal/mcp \
  --subscribe "inbox:///items/new" \
  --max-steps 25 \
  --max-tokens 2000000 \
  --health-file /run/agentd/health \
  --drain-timeout 25s \
  --log-level info
