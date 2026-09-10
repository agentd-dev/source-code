#!/usr/bin/env bash
# run-reactive.sh — event-reactive daemon: idle until an MCP resource changes,
# wake, triage it, go back to idle. Never exits on its own (a drain signal or a
# fatal/limit class stops it) — deploy it as a long-lived Deployment.
#
# Reactivity rides the MCP servers' Streamable-HTTP subscriptions: agentd
# subscribes to the resource named in the config and reacts to pushed
# `notifications/resources/updated` over HTTP/SSE. The subscribed servers are
# remote HTTP endpoints.
#
# The subscription is a `subscribe` start node. `--mode reactive --subscribe
# <uri>` was the 1.x spelling; modes were removed in 2.0 and the shape they
# encoded became a start node. See docs/modes-and-triggers.md.

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-https://gw.example/v1}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

# --max-tokens bounds ONE RUN (`limits.run.tokens`), not the daemon's lifetime
# spend — every wake gets the same allowance again. For a cumulative ceiling set
# `intelligence.budget.lifetime_tokens` in the config. A generous per-run
# allowance and no hard deadline is typical for a kept-alive Deployment; tune to
# taste.
exec "$AGENTD" \
  --config "$(dirname "$0")/reactive-triage.yaml" \
  --max-tokens 2000000 \
  --health-file /run/agentd/health \
  --log-level info
