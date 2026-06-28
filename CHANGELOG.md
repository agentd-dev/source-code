# Changelog

All notable changes to **`agentd`** — the minimal, MCP-native, reactive agent
runtime (developed in the `agentd-dev` org). The format is loosely
[Keep a Changelog](https://keepachangelog.com); versions are the released git tags
(`vX.Y.Z`) and the published image `ghcr.io/agentd-dev/agentd:X.Y.Z`.

## v1.0.0 — first official release

The first official, public release of **`agentd`**: one static musl binary
(serde/serde_json + libc only — 3 dependencies, ~1.3 MB, no async runtime, no TLS,
no C toolchain), built for Kubernetes. It takes an instruction plus tools from MCP
servers and runs the agentic loop — as a one-shot, a loop, a schedule, or a
reactive daemon — supervised, bounded, and observable.

`agentd` is the **reference implementation of the neutral Agent Control Contract
(ACC v1)**: it is named `agentd` (the daemon), but it **speaks the neutral
`agent` contract** so the agentctl control plane drives it without depending on this
binary. Concretely, the product/binary/image is `agentd`, while the wire/config
surfaces are neutral:

- **Resources:** `agent://` (status, capabilities, inventory, run, subagent,
  session, events, intelligence, capacity, config/effective); the legacy `agentd://`
  spelling is still accepted on reads.
- **Metrics:** the `agent_` Prometheus prefix (`agent_up`, `agent_runs_total`,
  `agent_saturation`, `agent_pending_events`, …), `metrics_schema` 1.0.
- **Manifest:** `--capabilities` emits `agent_version` and an honest `surfaces{}`
  discovery block; `contract_version` 1.0.
- **Env:** the downward-API + config convention is `AGENT_*` (the branded `AGENTD_*`
  spellings remain accepted on input); credentials only via `*_TOKEN[_FILE]`.
- **`_meta`:** `agent/*` idempotency/claim keys.

### Highlights

- **MCP-native runtime** (RFCs 0001–0009): supervisor + re-exec'd subagents, the
  ReAct loop with a closed terminal-status set, the MCP client/server subset, the
  self-MCP control surface, and the fork-bomb-safe subagent process model.
- **Cloud-native contract** (RFCs 0010–0016): the frozen exit-code table
  (clean drain = 0, not 143), the run-outcome report, the metrics schema, the
  `agent://events` stream, liveness/readiness probes, and `--budget-exit-code`.
- **Control plane** (RFCs 0014–0020): the operator management surface (drain /
  lame-duck / pause / resume / cancel, Management-gated → `-32601`), intelligence
  resilience + hot-swap, horizontal scaling (sharding + work-claim leases +
  standby), SIGHUP/inotify hot reload, and A2A interop over vsock.
- **Security:** the lethal-trifecta (Rule-of-Two) gate as the single `validate()`
  authority, an exec operator allowlist, and structural secret-freedom (no
  credential reaches the manifest, the config file, or the identity path).
- **ACC v1 conformance:** every contract surface validates against its schema and
  behaves as specified — see `CONFORMANCE.md`.
