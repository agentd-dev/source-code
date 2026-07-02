# Changelog

All notable changes to **`agentd`** — the minimal, MCP-native, reactive agent
runtime (developed in the `agentd-dev` org). The format is loosely
[Keep a Changelog](https://keepachangelog.com); versions are the released git tags
(`vX.Y.Z`) and the published image `ghcr.io/agentd-dev/agentd:X.Y.Z`.

## v2.0.0 — the HTTPS-everywhere runtime, agent-authored workflows, and CEL

A **breaking** major: every transport is now HTTP(S), the last local-execution
surface is gone, and the runtime gains a full workflow engine the agent drives
by itself. `contract_version` 2.0; the exit-code table, metrics schema, and
`agent://` resource surface are unchanged.

### Breaking

- **HTTPS everywhere.** Intelligence, the MCP client, the served self-MCP, and
  A2A/operator control are all HTTP(S). agentd no longer links unix or vsock
  transports (the reusable `net`/`mcp` crates keep them); plaintext `http://`
  is a loopback-only dev carve-out. `tls` (rustls + ring, bundled roots) is now
  a **default feature**.
- **MCP servers are remote endpoints.** `--mcp name=<https://host/mcp>`
  (Streamable HTTP, sessions + SSE, multi-version negotiation across
  `2025-06-18` / `2025-11-25` / the stateless `2026-07-28` era). The stdio
  `command` transport is removed; agentd spawns no server process. Per-server
  auth: secret-free header templates (`{{secret:NAME}}`), mTLS client identity,
  or OAuth 2.1 client-credentials (`--features oauth`).
- **No local execution, period.** The gated `exec` tool is removed entirely;
  the only process agentd ever starts is itself (`current_exe()` re-exec for
  subagents).
- **Serving requires identity.** `--serve-mcp https://host:port` with
  `--serve-cert`/`--serve-key`, and a non-loopback listener MUST authenticate
  peers (`--serve-client-ca` mTLS and/or `--serve-bearer`); verified identity —
  never the transport — mints the Management origin.
- **Operator control is unified into the A2A method family**: `a2a.Drain`,
  `a2a.LameDuck`, `a2a.Pause`, `a2a.Resume`, `a2a.Cancel` (Management-gated
  JSON-RPC methods, refusals as protocol errors). The operator tools are gone
  from `tools/call`.

### Added

- **Workflows** (`--features workflow`, dependency-free): an explicit cyclic
  graph the agent authors and drives itself — `workflow.define` / `workflow.run`
  (sync or `detach` into a supervised child) / additive `workflow.patch` — or
  the operator pins (`--mode workflow --workflow <file>`, fully supervised).
  Ten node kinds: `agent`, `tool` (args with `{"$from": …}` blackboard
  references and computed `{index}` pointer segments), `assign` (pure data
  shaping), `infer` (schema-checked structured intelligence with automatic
  re-asks), `branch` (deterministic predicates incl. cross-key comparison +
  an optional semantic judgement), `foreach` (deterministic fan-out over an
  array — zero model tokens for tool-only bodies; up to 8 parallel lanes with
  their own connections), `join` + async `subgraph` (parallel phases as
  supervised child processes), `wait`, `subgraph`, `halt`. Termination is
  layered and attributed: step budget, a shared whole-workflow token pool, a
  wall-clock deadline, per-node visit caps, a progress guard, and author-time
  validation — every engine stop carries a `reason` and the token cost.
- **The reactive-daemon workflow** (`--mode reactive --workflow <file>`): waits
  hold no process — the child suspends with its serialized run slice, the
  daemon arms the watch, and a fresh child resumes on update/timeout with the
  budget continuing across processes. Daemon lifetime = workflow lifetime; live
  state at the Management-only `agent://workflow` resource.
- **CEL** (`--features cel` — the one dependency-bearing opt-in, absent from
  default builds and shipped artifacts): `{"op":"cel"}` predicates, computed
  `assign.expr`, `infer.check` value constraints, and reactive wake conditions.
  Compile-checked at define time; fail-closed everywhere; JSON numbers
  normalized so expressions behave as written.
- **A2A**: real SSE streaming for `a2a.SendStreamingMessage` /
  `a2a.SubscribeToTask` (server push of working → artifact → final; the client
  is streaming-first with GetTask recovery, never re-sending a run), and
  **peer client-auth** (bearer header templates and/or an mTLS identity
  presented TO a peer).
- **Reactive precision**: content conditions on subscriptions, the
  `await_resource` in-turn wait, live warm-session tool-catalogue refresh on
  `tools/list_changed`, and the named `ToolClass` boundary (MCP vs
  self/control).
- **Reusable crates**: the MCP wire/version/client/server framework and the
  net transport primitives now live in workspace crates (`mcp` 0.2.0, `net`
  0.2.0) usable outside this binary.

### Fixed / hardened

- The capabilities manifest advertises the complete compiled feature set.
- Inline credential-shaped header values are rejected for MCP servers and A2A
  peers (previously only intelligence headers).
- Incoherent flag combinations exit 2 (`--shard`/`--standby`/`--assign-from`
  with a reactive workflow; blank-instruction subscription reactions).
- Blackboard values are size-capped; oversized results take the error edge.

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
