#!/usr/bin/env bash
# run-once.sh — one-shot run: execute an instruction to a terminal status, exit.
#
# Targets v1 behavior (see docs/design/PLAN.md). The binary today validates
# config + logs + exits with a scaffold notice for run modes; the agentic loop,
# supervisor, and MCP client land across milestones M1-M3. The flags below are
# the stable v1 surface (all exist in crates/agentd/src/config.rs).
#
# Deploy shape: a Job or a CLI invocation. Exit code maps the root subagent's
# terminal status (completed -> 0, refused -> 5, budget/exhausted -> 7, ...).

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

# Intelligence endpoint: unix:/path | https://host/... | vsock:cid:port.
# The token is passed by ENV ONLY (never a flag in CI logs) — it is redacted in
# all agentd output. Here we read it from the environment if present.
export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-unix:/run/intel.sock}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

exec "$AGENTD" \
  --mode once \
  --instruction-file "$(dirname "$0")/instructions/research.md" \
  --model "claude-opus-4" \
  --mcp "search=mcp-server-websearch" \
  --mcp "fs=mcp-server-fs --root /data --read-only" \
  --max-steps 40 \
  --max-tokens 150000 \
  --deadline 5m \
  --log-level info \
  --run-id "research-$(date +%Y%m%d-%H%M%S)"
