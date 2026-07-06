# agentd RFCs — index

> **⚠ Transport superseded by the target-vision pivot (2026-07-02).** These RFCs
> were written when agentd's intelligence, control plane, and A2A rode **unix/vsock**.
> agentd has since pivoted to **HTTPS everywhere**: intelligence, the MCP client, the
> served self-MCP, A2A, and operator control are all HTTP(S) with mTLS/bearer auth
> (loopback `http://` for dev); operator control is unified into the `a2a.*` method
> family; and the `exec` self-tool was **removed** (agentd runs no local code). The
> RFC *contracts* (methods, resources, semantics, exit codes) still hold — only the
> **transport** and the exec surface changed. The authoritative, verified plan is
> **[`../docs/design/00-target-vision-pivot.md`](../docs/design/00-target-vision-pivot.md)**;
> where an RFC and that document diverge on transport/exec, the pivot wins. Affected
> RFCs carry a banner. Run-graphs (an agent-orchestration feature) are documented in
> **[`../docs/workflows.md`](../docs/workflows.md)**.

This directory holds the agentd RFC set. **0001–0013** are the rewrite core
(Accepted, shipped v1): RFC **0001** is the readable narrative front door, and
**0002–0013** specify each mechanism area in depth. **0014–0020** are the
**agentctl control-plane track** (Proposed) — the contract surface agentd exposes
so an external control plane (the `agentctl` CLI + `kubectl agent[s]` plugin +
Kubernetes operator) can provision, supply intelligence over vsock, scale,
observe, and manage a *fleet* of agentd instances; RFC **0014** is the umbrella
and **0015–0020** the concrete contracts (**0020** adds A2A-over-vsock agent-mesh
interop). All cross-reference one another by number rather than restating detail.

**The binding decision record is [`docs/design/00-architecture-assessment.md`](../docs/design/00-architecture-assessment.md).**
Where any RFC and that document diverge, **the assessment wins** and the RFC is
refined to match. The `docs/design/notes-*.md` files are the supporting research
and review notes the assessment synthesizes; they are inputs, not normative.

Terminal-status vocabulary is owned by **RFC 0007 §3.4** (the single authority);
the exit-code table is owned by **RFC 0011 §5**; the MCP wire/codec by **RFC 0004**;
the control protocol + self-MCP surface by **RFC 0005**. Other RFCs reference
these rather than redefining them.

| RFC | Title | Status | Scope (one line) |
|---|---|---|---|
| [0001](0001-mcp-native-agent-runtime.md) | MCP-native agent runtime — core architecture | Accepted (shipped v1) | Front door: thesis, two-loop split, components, modes, deployment shapes, non-goals. |
| [0002](0002-supervisor-reactor-and-concurrency.md) | Supervisor reactor & concurrency model | Accepted (shipped v1) | Thread-per-fd + `mpsc` reactor, self-pipe signals, abandon-don't-interrupt invariant, write path, timers. |
| [0003](0003-process-supervision-and-recovery.md) | Process supervision, dead/stuck detection & recovery | Accepted (shipped v1) | Three-detector model + EOF×pong classifier, PID-1 subreaper, PDEATHSIG, kill ladder, restart governor, token accounting, cgroup-awareness. |
| [0004](0004-mcp-client-subset-and-codec.md) | MCP client subset & wire codec | Accepted (shipped v1) | Shared JSON-RPC codec; MCP 2025-11-25 client subset: tools/resources/subscribe, notify-then-read, ping/cancel/progress, stdio transport + shutdown ladder. |
| [0005](0005-self-mcp-server-and-control-protocol.md) | Self-MCP server & control protocol | Accepted (shipped v1) | Self-MCP tool/resource surface (`subagent.*`, `subscribe`, `resource.read`, gated `exec`), subscribable `agent://` state, stdio/unix serving; length-framed supervisor↔subagent control channel. |
| [0006](0006-intelligence-transport-and-wire.md) | Intelligence transport & wire format | Accepted (shipped v1) | unix/https(tls)/vsock transports, OpenAI-compatible + anthropic adapters, native tool-calling + usage, JSON-action fallback, credential handling. |
| [0007](0007-agentic-loop-and-terminal-status.md) | Agentic loop & terminal-status state machine | Accepted (shipped v1) | ReAct turn, the stop-condition disjunction + terminal-status enum (authority), VERIFY grounded in tool/exec, error taxonomy, context compaction, resource list-vs-read. |
| [0008](0008-execution-modes-and-reactive-routing.md) | Execution modes, triggers & reactive routing | Accepted (shipped v1) | once/loop/reactive/schedule as exit predicates; exactly-one-owner routing, spawn-vs-continue, debounce/coalesce, backpressure, self-subscribe; interval/cron as event sources. |
| [0009](0009-subagent-process-model.md) | Subagent process model & nesting | Accepted (shipped v1) | Re-exec subagent mode, rich spawn payload + output contract, narrowed seed, distilled result, sync/async/detach, tool scope, depth/breadth/rate/tree-token caps, the single spawn chokepoint. |
| [0010](0010-observability-health-telemetry.md) | Observability, health & telemetry | Accepted (shipped v1) | JSON-lines logger, line schema + closed event vocabulary, correlation tuple + spawn telemetry block, W3C context propagation, mode-aware health, metrics-from-logs, gated `metrics`/`otel`. |
| [0011](0011-cloud-native-contract.md) | Cloud-native contract: config, signals, exit codes, idempotency | Accepted (shipped v1) | Config precedence + validate-at-startup, drain choreography + `AGENT_DRAIN_TIMEOUT` < grace, the exit-code table (authority), RUN_ID idempotency, statelessness, cgroup friendliness. |
| [0012](0012-security-posture.md) | Security posture | Accepted (shipped v1) | Granted-MCP-subset as Rule-of-Two trust budget, untrusted-server-content stance, SSRF defenses, gated `exec`, self-MCP hardening, secrets handling. |
| [0013](0013-deferred-v2-surface.md) | Deferred v2 surface | Accepted (v2 surface, deferred) | The explicit defer list: MCP tasks, sampling (as client), roots, Streamable HTTP serving + SSE, MCP-backed session checkpointing — each with its named v1 fallback. |
| [0014](0014-control-plane-contract.md) | agentd as a managed workload — the control-plane (agentctl) contract | Proposed (control-plane track) | Umbrella: data/control-plane split, vsock-as-management, the capabilities-manifest spine, ratified cross-cutting conventions (ownership map, `surfaces{}`, versioning, the k8s env convention), the sub-RFC index. |
| [0015](0015-management-and-control-surface.md) | Management & control surface | Proposed (control-plane track) | `--serve-mcp vsock:PORT`; the operator MCP profile (`agent://capabilities`/`inventory` + `drain`/`lame-duck`/`pause`/`resume`/`cancel`, `attach`=`subagent.send`); the frozen capabilities manifest; downward-API instance identity (env-only). |
| [0016](0016-telemetry-and-lifecycle-contract.md) | Telemetry & lifecycle contract | Proposed (control-plane track) | The frozen Prometheus metrics schema (`metrics_schema`); the exit-code *contract* around RFC 0011 §5; machine-readable run-outcome reports; `agent://events`; stuck-detector liveness; fleet trace correlation. |
| [0017](0017-declarative-config-and-hot-reload.md) | Declarative configuration & hot reload | Proposed (control-plane track) | The config-file layer + JSON schema; `agentd --validate-config`/`--config-schema`; `SIGHUP`/file-watch hot reload of the reloadable subset (servers/subscriptions/model/limits); file-based secret refs. |
| [0018](0018-intelligence-transport-resilience.md) | Intelligence transport resilience | Proposed (control-plane track) | Ordered multi-endpoint intelligence with health-aware failover + circuit-break; per-endpoint health surface; runtime model/endpoint hot-swap (no restart); optional model-discovery handshake. |
| [0019](0019-horizontal-scaling.md) | Horizontal scaling | Proposed (control-plane track) | Cross-replica work-claim/lease (reuses MCP, no bespoke queue); `--shard K/N` static partitioning (FNV-1a/64); the autoscaling signal set (KEDA/HPA inputs); warm-pool/standby. |
| [0020](0020-a2a-interop-over-vsock.md) | A2A interoperability over vsock | Proposed (control-plane track) | Serve **A2A** over vsock (manifest = Agent Card, a run = a Task); the node-agent is a dumb HTTP↔vsock gateway + PEP (TLS/auth/SSE/webhooks/history). The subagent decision: local supervised subagents unchanged; A2A is the external surface + a delegation backend; MCP-self-serving → compat. Likely obviates HTTP serving in agentd. |
| [0021](0021-durable-workflows-and-parity-extensions.md) | Durable workflows & parity extensions | Implemented (workflow track) | Workflow dialect 2: write reducers (`writes_mode`), the in-process `parallel` node, the `human` gate (served payload + A2A `input-required` + reply wait), and the **MCP checkpointer** (per-superstep durable state via a 3-tool MCP profile; `--workflow-resume`, fork/time-travel, budget carry-over) — plus fail-closed dialect hygiene (strict node fields). |
| [0022](0022-embedding-and-code-tools.md) | Embedding & code-registered tools | Implemented | The library consumption surface: the `agentd-core`/`agentd-cli`/`agentd-mcp`/`agentd-net` crate split, the embedder obligations (subagent re-exec dispatch, register-in-main), CODE-REGISTERED tools (`agentd::tools` — self > code > MCP precedence, the reserved `code` workflow server, unshadowable orchestration surface), and the three API-stability tiers. |

## Supporting research (non-normative)

In [`docs/design/`](../docs/design/): `notes-mine-existing-code.md` (salvage map of
the retired runtime), `notes-research-*.md` (MCP spec depth, modern agent loops,
minimal Rust ecosystem), and `notes-review-*.md` (reliability, concurrency,
cloud-native, observability, MCP protocol, agent-loop-modes reviews). These feed
the assessment; the assessment is what binds.
