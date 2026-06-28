# Changelog

All notable changes to `agentd`. The format is loosely [Keep a Changelog](https://keepachangelog.com);
versions are the released git tags (`vX.Y.Z`) and the published image
`ghcr.io/agentd-dev/agentd:X.Y.Z`. Still pre-1.0 in spirit — `contract_version`
remains `1.0` and surfaces evolve additively, but **breaking changes are called
out explicitly** below.

## v2.8.0 — audit hardening

A multi-dimension audit (security, robustness, observability, consistency,
alignment) drove this release. No new feature; the default image set is unchanged
(`metrics,serve-mcp,cron,otel,cluster,hot-reload,config-watch`); still a static
3-dependency musl binary.

### ⚠ Breaking

- **`exec` now requires an operator allowlist of binaries.** `--enable-exec` used
  to be a bare boolean that let the model run **any** absolute-path binary; it now
  **takes a path** (repeatable) and the bare form is rejected at startup (exit 2,
  with an actionable error). `AGENTD_ENABLE_EXEC` is now a `:`-separated path list,
  not a truthy bool. This aligns the code with its always-specified allowlist
  contract (RFC 0012 §3.6).
  **Migrate:** `--enable-exec` → `--enable-exec /usr/bin/git --enable-exec /usr/bin/cargo`
  (one per allowed binary). There is no "allow everything" switch by design — naming
  the binaries *is* the guarantee. See [docs/security.md §6](docs/security.md).

### Security

- The lethal-trifecta (Rule-of-Two) gate is now the single `validate()` authority,
  so `--validate-config` and startup can never disagree (an admission webhook and
  the pod now reach the same verdict), and it is **re-checked on hot reload** — a
  reload that forms a complete trifecta (e.g. adding an egress-tagged MCP server) is
  rejected/audited instead of silently widening the surface live.

### Robustness

- Reactor-thread management MCP calls (reload re-handshake, `list_tools`, claim
  renew/release/settle, drain `work.release`) use a short (2s) timeout, asserted at
  compile time to be under the `/healthz` liveness window — a slow-but-alive server
  can no longer starve the heartbeat into a Kubernetes SIGKILL or blow the drain
  budget.

### Observability

- `/readyz` now flips `NotReady` when every intelligence endpoint is down
  (RFC 0018 §6), via a child→supervisor `AgentMsg::IntelHealth` bridge, plus a new
  `agentd_intel_all_down` gauge. `agentd://intelligence` and `agentd://capacity`
  report honest (latched) health instead of always-healthy fiction.
- The frozen `metrics_schema` 1.0 contract is now honest: supervisor-reachable
  counters are wired, child-process-only / not-yet-implemented series are marked
  reserved (no fabricated permanent-zero), `agentd_tokens_total` is live (the
  missing `AgentMsg::Usage` producer is wired), and `docs/observability.md` matches
  the rendered set.

### CI

- Per-feature solo `clippy -D warnings` across the full feature matrix, plus the
  six previously-missing solo combos.

## v2.7.0

- **Model discovery** (RFC 0018 §5.4): a lazy, cached, silent-degrading
  `GET /v1/models` probe surfaces `discovery` + `models` on `agentd://intelligence`
  and the capabilities manifest, for model-aware placement.
- **Live `mcp_servers` re-handshake** (RFC 0017 §5.3): the MCP server set reloads
  without a restart (the supervisor's server/owner/claim wiring is name-keyed so a
  remove/add never shifts identity).

## v2.6.0

- **inotify file-watch reload** (`--watch-config`, RFC 0017 §5.2): a Kubernetes
  ConfigMap volume swap reloads in place.
- **`agentd://config/effective`** — a live, subscribable, redacted view of the
  reloadable config; served reads now reflect a hot reload.
- **Intelligence hot-swap** (RFC 0018 §5): live endpoint repoint + model swap at
  turn boundaries (`--model-swap` finish-on-old | restart-turn).
- **Work-claim story completed**: continue-claim, renew heartbeat, and a real
  two-instance race conformance test.
- The default cloud image added `config-watch`.

## v2.5.0

- **Intelligence resilience** (RFC 0018): multi-endpoint failover, circuit breaker,
  `agentd://intelligence`.
- **Horizontal scaling** (RFC 0019): sharding (`--shard K/N`), work-claim leases,
  standby, autoscaling signals, `agentd://capacity`.
- **pause/resume** (RFC 0015 §4.3) — tree-wide turn-boundary suspension.
- **Hot reload** (RFC 0017-B §5): SIGHUP, validate-first / all-or-nothing.
- The default cloud image added `cluster` + `hot-reload`.

## v2.4.0 and earlier

Telemetry & lifecycle contract (RFC 0016), declarative config file + admission
(RFC 0017-A), A2A interop over vsock (RFC 0020), and the M1–M7 build of the
MCP-native runtime. See the git history and `rfcs/`.
