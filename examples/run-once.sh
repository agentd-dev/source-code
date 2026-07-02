#!/usr/bin/env bash
# run-once.sh — one-shot run: execute an instruction to a terminal status, exit.
#
# The flags below are the stable CLI surface (all exist in
# crates/agentd/src/config.rs). Tools come from remote MCP servers reached over
# Streamable HTTP — agentd ships no tools and runs no local code of its own.
#
# Deploy shape: a Job or a CLI invocation. Exit code maps the root subagent's
# terminal status (completed -> 0, refused -> 5, budget/exhausted -> 7, ...).

set -euo pipefail

AGENTD="${AGENTD:-agentd}"

# Intelligence endpoint: https://host/... (loopback http:// for a dev sidecar).
# The token is passed by ENV ONLY (never a flag in CI logs) — it is redacted in
# all agentd output. Here we read it from the environment if present.
export AGENT_INTELLIGENCE="${AGENT_INTELLIGENCE:-https://gw.example/v1}"
# export AGENT_INTELLIGENCE_TOKEN=...   # set in your environment, not here

exec "$AGENTD" \
  --mode once \
  --instruction-file "$(dirname "$0")/instructions/research.md" \
  --model "claude-opus-4" \
  --mcp search=https://mcp-search.internal/mcp \
  --mcp fs=https://mcp-fs.internal/mcp \
  --max-steps 40 \
  --max-tokens 150000 \
  --deadline 5m \
  --log-level info \
  --run-id "research-$(date +%Y%m%d-%H%M%S)"
