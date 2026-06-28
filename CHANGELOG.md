# Changelog

All notable changes to **`agent`** (the minimal MCP-native agent runtime, developed
in the `agentd-dev` org). The format is loosely [Keep a Changelog](https://keepachangelog.com);
versions are the released git tags (`vX.Y.Z`) and the published image
`ghcr.io/agentd-dev/agent:X.Y.Z`. `contract_version` is `1.0` and surfaces evolve
additively, but **breaking changes are called out explicitly** below.

## v3.0.0 — rebrand to `agent` (the neutral cutover) · first public release

The runtime is renamed from `agentd` to **`agent`** and fully de-branded to the
neutral Agent Control Contract (ACC) spellings. This is the first public release;
the pre-public `agentd` 2.x development line (the M1–M7 build, the control-plane
wave, and ACC v1 conformance) is preserved in git history but its tags/images are
not published. Still a static, 3-dependency musl binary; `contract_version` stays
`1.0`; the default image feature set is unchanged
(`metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch`).

### ⚠ Breaking — the neutral cutover

The agent now **emits** the neutral ACC tokens and no longer emits the branded
`agentd*` forms (the legacy spellings are still **accepted on input** — graceful
for any straggler — but never emitted):

- **Binary:** `agentd` → **`agent`** (`[[bin]] name`; the Rust crate stays `agentd`
  internally).
- **OCI image:** `ghcr.io/agentd-dev/agentd` → **`ghcr.io/agentd-dev/agent`**.
- **Resource scheme:** emits **`agent://…`** (status / capabilities / inventory /
  run / subagent / session / events / intelligence / capacity / config-effective);
  `agentd://…` still parses on reads.
- **Metric prefix:** **`agent_`** (e.g. `agent_up`, `agent_runs_total`,
  `agent_saturation`, `agent_pending_events`); the `agentd_` series are gone.
- **Manifest version key:** emits **`agent_version`** only; the legacy
  `agentd_version` key is dropped (the manifest root `anyOf` is satisfied by
  `agent_version`).
- **Env convention:** documented as **`AGENT_*`** (downward-API identity, per-endpoint
  tokens); the branded `AGENTD_*` spellings remain accepted on input. The internal
  re-exec marker is `AGENT_SUBAGENT`.
- **`_meta` namespace:** stamps **`agent/*`** (`agent/run_id`, `agent/claim_key`,
  `agent/instance`, `agent/shard`).
- **CLI / logs:** the version banner, `--help`, and error prefixes say `agent`.

agentctl drives the contract, not the binary name, so the rename is transparent
there; only the deployed **image reference** changes. The golden `--capabilities`
fixtures were re-captured from the `agent 3.0.0` binary (neutral tokens) and the
contract-client fixture tests move in lockstep.

### Carried in from the pre-public line (now first-published here)

- **Agent Control Contract (ACC) v1 conformance** — every surface validates against
  its schema; `CONFORMANCE.md` records the per-surface results and the live
  validation. `--budget-exit-code` remaps only the policy budget codes (3/7);
  de-branding input tolerance; uniform `-32601` operator-surface gating.
- The full M1–M7 MCP-native runtime, the control-plane wave (intelligence
  resilience, horizontal scaling, hot reload, A2A, pause/resume), and the audit
  hardening. See git history for the development narrative.
