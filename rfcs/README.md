# agentd RFCs — index

This directory holds the agentd rewrite RFC set (0001–0013). RFC **0001** is the
readable narrative front door; **0002–0013** specify each mechanism area in depth
and cross-reference one another by number rather than restating detail.

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
| [0005](0005-self-mcp-server-and-control-protocol.md) | Self-MCP server & control protocol | Accepted (shipped v1) | Self-MCP tool/resource surface (`subagent.*`, `subscribe`, `resource.read`, gated `exec`), subscribable `agentd://` state, stdio/unix serving; length-framed supervisor↔subagent control channel. |
| [0006](0006-intelligence-transport-and-wire.md) | Intelligence transport & wire format | Accepted (shipped v1) | unix/https(tls)/vsock transports, OpenAI-compatible + anthropic adapters, native tool-calling + usage, JSON-action fallback, credential handling. |
| [0007](0007-agentic-loop-and-terminal-status.md) | Agentic loop & terminal-status state machine | Accepted (shipped v1) | ReAct turn, the stop-condition disjunction + terminal-status enum (authority), VERIFY grounded in tool/exec, error taxonomy, context compaction, resource list-vs-read. |
| [0008](0008-execution-modes-and-reactive-routing.md) | Execution modes, triggers & reactive routing | Accepted (shipped v1) | once/loop/reactive/schedule as exit predicates; exactly-one-owner routing, spawn-vs-continue, debounce/coalesce, backpressure, self-subscribe; interval/cron as event sources. |
| [0009](0009-subagent-process-model.md) | Subagent process model & nesting | Accepted (shipped v1) | Re-exec subagent mode, rich spawn payload + output contract, narrowed seed, distilled result, sync/async/detach, tool scope, depth/breadth/rate/tree-token caps, the single spawn chokepoint. |
| [0010](0010-observability-health-telemetry.md) | Observability, health & telemetry | Accepted (shipped v1) | JSON-lines logger, line schema + closed event vocabulary, correlation tuple + spawn telemetry block, W3C context propagation, mode-aware health, metrics-from-logs, gated `metrics`/`otel`. |
| [0011](0011-cloud-native-contract.md) | Cloud-native contract: config, signals, exit codes, idempotency | Accepted (shipped v1) | Config precedence + validate-at-startup, drain choreography + `AGENTD_DRAIN_TIMEOUT` < grace, the exit-code table (authority), RUN_ID idempotency, statelessness, cgroup friendliness. |
| [0012](0012-security-posture.md) | Security posture | Accepted (shipped v1) | Granted-MCP-subset as Rule-of-Two trust budget, untrusted-server-content stance, SSRF defenses, gated `exec`, self-MCP hardening, secrets handling. |
| [0013](0013-deferred-v2-surface.md) | Deferred v2 surface | Accepted (v2 surface, deferred) | The explicit defer list: MCP tasks, sampling (as client), roots, Streamable HTTP serving + SSE, MCP-backed session checkpointing — each with its named v1 fallback. |

## Supporting research (non-normative)

In [`docs/design/`](../docs/design/): `notes-mine-existing-code.md` (salvage map of
the retired runtime), `notes-research-*.md` (MCP spec depth, modern agent loops,
minimal Rust ecosystem), and `notes-review-*.md` (reliability, concurrency,
cloud-native, observability, MCP protocol, agent-loop-modes reviews). These feed
the assessment; the assessment is what binds.
