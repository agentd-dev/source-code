# RFC 0010: Observability, health & telemetry

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

agentd is a process tree, not a thread pool, and it is reactive — it spends
most of its life asleep. Three architectural facts dominate the observability
design and break the usual web-service assumptions:

1. **Correlation must survive a process boundary and reconstruct a *tree*.**
   The unit of intelligence is a child *process* (the same binary re-exec'd,
   RFC 0009), nesting into a supervised tree. The OS already shows the tree via
   `ps`/`pstree`; our job is to make the *logs* reassemble that same tree
   off-box, without a backend join.
2. **The supervisor makes no LLM calls and holds no conversation state**
   (RFC 0001, RFC 0002). Its telemetry is about lifecycle/control (spawn, reap,
   limits, triggers, subscriptions, drain); the subagent's telemetry is about
   reasoning (loop steps, tool calls, tokens). Two schemas, labelled
   (`comp:"supervisor"` vs `comp:"agent"`).
3. **The absence of activity is the normal state.** A reactive agent (RFC 0008)
   subscribed to MCP resources idles for hours by design. "Healthy and idle"
   must be distinguishable from "hung," so **health cannot be inferred from
   traffic** — it must be measured at the supervisor's own loop.

The minimalism bar (assessment §2.2) forbids `tracing`/`tracing-subscriber`,
metrics SDKs, and OTLP in the default build. This RFC specifies exactly what
the default build ships and what is feature-gated.

This RFC covers assessment §2.9 and does not contradict it. Where it touches
config flags, signals, and exit codes, those are owned by RFC 0011 — this RFC
references them, it does not redefine them.

---

## 2. Decision

**The default build ships exactly two things: a hand-rolled JSON-lines logger
to stderr (~150 lines reusing the `serde_json` serializer — NOT `tracing`) and
a tiny health surface (exit code + an optional `--health-file`). Everything
heavier is feature-gated.**

- **stdout = the agent's result only; stderr = all telemetry.** One event per
  line, NDJSON.
- A **closed, stable line schema** and a **closed `event` vocabulary** — both
  reproduced below — are the contract for downstream dashboards, metrics, and
  trace mapping.
- **Content capture is off by default** (hashes/lengths only); `--log-content`
  opts in, redaction-aware; secrets never appear (field allowlist).
- **Tree correlation** is a `telemetry` block in the spawn payload; collectors
  reassemble by `run_id` + `agent_path` prefix with **no join**. Default: each
  process self-logs to its own stderr (mode A); `--aggregate-logs` forwards
  child telemetry up the control channel (mode B).
- **W3C trace-context propagation is ON by default** (it is a few JSON/header
  fields); span *export* (OTLP) is gated behind `otel`.
- **Health is mode-aware:** one-shot = exit code; loop/reactive = supervisor
  heartbeat liveness (idle is healthy; a stuck subagent must NOT fail pod
  liveness) + readiness = MCP-connected and subscriptions reconciled.
- **Metrics:** default = derive from logs; `metrics` feature = hand-written
  Prometheus text; `otel` feature = OTLP + GenAI semconv.

---

## 3. Mechanisms

### 3.1 The logger (`obs/log.rs`)

No `tracing`. The same `serde_json` machinery that writes MCP frames writes log
lines. The logger reads an ambient, process-global correlation context
(`LogCtx`), merges caller-supplied fields, serializes one object, writes one
line to a locked, buffered `stderr`, and flushes on newline.

```rust
// obs/log.rs — process-global correlation, set once at startup / after spawn-payload parse.
pub struct LogCtx {
    pub run_id: String,        // ULID, constant across the whole tree
    pub agent_id: String,      // emitting process id; supervisor uses "sup"/"root"
    pub agent_path: String,    // dotted tree path: "0", "0.2", "0.2.1"
    pub comp: Comp,            // Supervisor | Agent | Mcp | Intel
    pub pid: u32,
    pub trace_id: Option<[u8; 16]>, // present when a trace exists (propagation default)
    pub log_level: Level,
    pub log_content: bool,
}

#[derive(Clone, Copy)]
pub enum Level { Trace, Debug, Info, Warn, Error }

#[derive(Clone, Copy)]
pub enum Comp { Supervisor, Agent, Mcp, Intel }

static CTX: OnceLock<RwLock<LogCtx>> = OnceLock::new();
static OUT: OnceLock<Mutex<BufWriter<Stderr>>> = OnceLock::new();

/// The one entry point. `event` is from the closed vocabulary (§3.3).
/// `fields` are event-specific; they are merged after the canonical block,
/// so a caller can never shadow `ts`/`run_id`/etc. (canonical wins).
pub fn log_event(level: Level, event: &str, fields: serde_json::Value);

/// Span helpers — a span is just two ids in TLS, not a tracing machine.
pub fn span_enter(name: &str) -> SpanGuard; // pushes span_id, sets parent_span_id
// SpanGuard on drop emits `<name>.end`-style dur_ms if the caller opted in;
// most call sites emit explicit `*.result`/`*.end` events instead.
```

Mechanics and invariants:

- **Level filter** is a cheap integer compare before any allocation; below-level
  calls cost nothing beyond the compare.
- **One write per line, flush on newline.** `BufWriter` + a `Mutex` so
  interleaving threads never tear a line. A torn JSON line is worse than a lost
  one; the lock is held only for the single `write_all` + `flush`.
- **`span_id`/`parent_span_id`** live in a thread-local stack (`Cell<Option<[u8;8]>>`
  pair). The process tree gives us cross-process context for free (the spawn
  payload), so we never need `tracing`'s implicit async-context plumbing.
- **No panics from the logger.** A write error to stderr is swallowed (best
  effort); telemetry must never take down the supervisor. SIGPIPE is already
  ignored (RFC 0002).
- **Cost:** ~150 lines. Zero new crates — `serde_json` is already core (§2.2).

### 3.2 Canonical line schema (stable, snake_case)

One event per line. NDJSON. Reproduced verbatim from assessment §2.9 — this is
the binding contract; renaming a field is a breaking change.

| Field | Always | Meaning |
|---|---|---|
| `ts` | yes | RFC 3339 UTC, e.g. `2026-06-25T10:00:00.123Z` |
| `level` | yes | `trace`/`debug`/`info`/`warn`/`error` |
| `event` | yes | dotted event type — the primary index key |
| `run_id` | yes | ULID for the whole invocation (the unit of work), stable across the tree |
| `agent_id` | yes | emitting process id (supervisor uses reserved `sup`/`root`) |
| `agent_path` | yes | dotted tree path (`0`, `0.2`, `0.2.1`) — **the cheap superpower:** subtree queries by prefix, no backend join |
| `comp` | yes | `supervisor` \| `agent` \| `mcp` \| `intel` |
| `pid` | yes | joins the log tree to the free OS `pstree` |
| `span_id` / `parent_span_id` | in-span | 8-byte hex |
| `trace_id` | when propagation on | 16-byte hex W3C |
| `dur_ms` | on `*.end`/`*.result` | duration |
| `err` | on errors | `{type, message}` structured |
| event-specific | | `tool`, `server`, `tokens_in`/`tokens_out`, `resource_uri`, `route`, etc. |

Notes:

- **`agent_path` is the superpower.** Because lineage is encoded *in the value*,
  an operator runs `grep '"agent_path":"0.2'` to scope a subtree, and a collector
  builds the tree by prefix with no join. Works with `otel` off.
- **`pid` joins the log tree to the OS tree.** `subagent.spawn` logs the child's
  `pid` so the log tree and `pstree` are joinable; `subagent.stuck` can cite the
  OS process state (`D`/`Z`/running) alongside `last_event_age_ms`.
- **`err` is structured**, never a stringified stack: `{"type":"...","message":"..."}`.
- **`msg` is optional** — a short human string, never the structured payload.
- `ts` is always UTC. No local time, ever.

Example lines (one supervisor, one agent):

```
{"ts":"2026-06-25T10:00:00.012Z","level":"info","event":"subagent.spawn","run_id":"01J...","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":1421,"child_agent_id":"01J...c","child_path":"0.2","instruction_hash":"b1946ac9","tool_scope":["fs.read"],"depth":1}
{"ts":"2026-06-25T10:00:01.534Z","level":"info","event":"tool.result","run_id":"01J...","agent_id":"01J...c","agent_path":"0.2","comp":"agent","pid":1457,"span_id":"a1b2c3d4e5f60718","parent_span_id":"00f067aa0ba902b7","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","server":"fs","tool":"read_file","call_id":"c-7","ok":true,"dur_ms":42,"result_bytes":2048}
```

### 3.3 Closed `event` vocabulary

The `event` string is the backbone — what you filter, count, and alert on. It
is a small, **closed**, dotted set. Adding an event later is cheap; renaming one
breaks dashboards, so they are nailed now. This is the complete v1 list.

**Supervisor / lifecycle (`comp:"supervisor"`):**

| Event | Fields (beyond canonical) |
|---|---|
| `proc.start` | `mode`, `pid`, `version`, `argv_hash` |
| `proc.ready` | (readiness reached — see §3.7) |
| `proc.shutdown` | `signal`, `reason` |
| `proc.exit` | `code`, `uptime_ms` |
| `config.loaded` | `mcp_servers` (count/names), `mode`, limits (no secrets) |
| `mcp.connect` | `server`, `transport`, `tools` (count), `resources` (count) |
| `mcp.connect.fail` | `server`, `transport`, `err` |
| `mcp.disconnect` | `server`, `reason` |
| `trigger.armed` | `kind` (`once`/`loop`/`reactive`/`schedule`), detail |
| `trigger.fired` | `kind`, `resource_uri?`, `route` (`spawn`/`continue`) |
| `subscribe` | `resource_uri`, `server`, `by` (`config`/`agent`) |
| `unsubscribe` | `resource_uri`, `server`, `by` |
| `resource.updated` | `resource_uri`, `server` — the reactive heartbeat-of-meaning |
| `subagent.spawn` | `child_agent_id`, `child_path`, `instruction_hash`, `tool_scope`, `limits`, `depth`, `pid` |
| `subagent.exit` | `child_agent_id`, `code`, `result_status`, `dur_ms` |
| `subagent.signal` | `child_agent_id`, `action` (`pause`/`resume`/`cancel`/`inject`) |
| `subagent.stuck` | `child_agent_id`, `last_event_age_ms`, `proc_state`, `action` (`sigterm`/`sigkill`) |
| `subagent.restart` | `child_agent_id`, `reason`, `restarts_total` |
| `limit.exceeded` | `limit` (`steps`/`tokens`/`deadline`/`depth`/`tree_tokens`/`restart_storm`/`spawn_rate`), `value`, `cap` |

**Agentic loop (`comp:"agent"`, with `intel.*` carrying `comp:"intel"`):**

| Event | Fields (beyond canonical) |
|---|---|
| `loop.start` | `trigger` (`spawn`/`continue`/`resume`), `step` |
| `loop.step` | `step`, `tokens_in`, `tokens_out` |
| `loop.final` | `step`, `result_status`, `result_bytes` |
| `loop.error` | `err`, `step` |
| `intel.call` | `model`, `tokens_in` (estimated) |
| `intel.result` | `model`, `tokens_in`, `tokens_out`, `finish_reason`, `dur_ms` |
| `tool.call` | `server`, `tool`, `call_id`, `args_hash` (`args` only if content capture on) |
| `tool.result` | `server`, `tool`, `call_id`, `ok`, `dur_ms`, `result_bytes` (`result` only if content capture on) |

That is the entire vocabulary: 19 supervisor events + 8 agent events = 27 names.
`comp:"mcp"` is used for transport-level lines folded from
`notifications/message` (RFC 0004) reusing the same event names where they fit
(e.g. `mcp.disconnect`); it does not introduce new `event` strings.

### 3.4 Content capture (off by default)

Mirrors the OTel GenAI stance and RFC 0012 secrets rule exactly:

- **Default:** log hashes and lengths only — `args_hash`, `result_bytes`,
  `instruction_hash`, `tokens_in`/`tokens_out`. Never raw content.
  `*_hash` is the first 8 hex chars of a non-cryptographic FNV-1a / xxHash-style
  digest (a stable correlation aid, not a security primitive).
- **`--log-content` (env `AGENT_LOG_CONTENT`)** opts in to capturing the
  prompt/tool-arg/result bodies (the `gen_ai.input.messages` /
  `gen_ai.output.messages` / `gen_ai.tool.call.arguments` / `...result`
  equivalents). It is loud, gated, and **redaction-aware**: it still strips
  known-secret fields.
- **Secrets never appear, content capture on or off.** Enforced by a **field
  allowlist**: only fields explicitly on the allowlist are serialized into a
  content payload; anything resolved through `secrets::resolve()` (RFC 0006,
  RFC 0012) — the intelligence token, MCP-server env secrets — is structurally
  excluded, and credential-typed values `Debug`-print as `***`.

`--log-content` is a debug/non-prod switch. We do not claim complete redaction
of arbitrary secrets that a model happens to pass as a free-form tool argument;
that is documented (open item, §6).

### 3.5 Tree correlation — the spawn `telemetry` block

The crux. The supervisor (or a parent agent via `subagent.spawn`, RFC 0009)
includes a `telemetry` block in the spawn payload alongside
instruction/seed/scope/limits:

```json
{
  "telemetry": {
    "run_id":         "01J...",          // constant for the whole tree
    "trace_id":       "4bf92f3577b34da6a3ce929d0e0e4736", // constant for the tree (or upstream's)
    "parent_span_id": "00f067aa0ba902b7", // the parent's current span — child's parent
    "agent_path":     "0.2",             // parent path + child index
    "agent_id":       "01J...child",     // child's own id
    "log_level":      "info",
    "log_content":    false
  }
}
```

From these the child constructs its own `LogCtx` (§3.1) in early `main` (after
the re-exec, before any side effect), and every line it emits is *pre-correlated*
to the tree. **No registry, no service discovery, no join-key negotiation** —
lineage is passed down once at exec, exactly like environment inheritance. This
is the process-tree equivalent of OTel context propagation and is why we do not
need `tracing`'s implicit context machinery.

- `run_id` → "all telemetry for this unit of work."
- `trace_id` → "one distributed trace across the whole tree (+ upstream)."
- `agent_path` → "this subtree," by prefix match, no join.
- `parent_span_id` → each child's root span nests under the parent's spawn span.

**Depth/path are minted by the supervisor**, never trusted from the child
(RFC 0009): `agent_path` is `parent_path + "." + child_index`.

**Getting telemetry off-box — two wirings:**

- **(A) default — each process writes its own stderr.** The container
  runtime/collector already captures it. Because every line self-identifies,
  agent does no aggregation and never becomes a logging bottleneck. Cleanest for
  K8s. Reassemble by `run_id` + `agent_path` prefix.
- **(B) `--aggregate-logs`** — child telemetry is framed up the existing control
  channel (RFC 0005) and the supervisor re-emits it on its own stderr. For
  single-stream environments (deeply nested local runs where only the root's
  stderr is captured). **The supervisor forwards, never rewrites** the correlation
  fields, or the tree breaks. Consumers sort by `ts` + `span_id`, never by arrival
  order (forwarded lines can arrive out of order).

In subagent mode, stdout is the control channel to the parent (RFC 0005), so
telemetry still goes to **stderr** in both wirings; mode B additionally frames a
copy up the control channel as a dedicated `log` message kind that the parent
distinguishes from lifecycle/result frames.

### 3.6 W3C context propagation (ON by default; export gated)

Validated by the MCP 2026-07-28 RC adopting SEP-414 (W3C trace-context in
`_meta`, fixed keys `traceparent`/`tracestate`/`baggage`). Propagation is a few
JSON/header fields and is therefore free; only span *export* is heavy and gated.

**Ingest (mint-or-adopt):**

- If an inbound `traceparent` arrives — on an inbound MCP request to agentd's
  self-MCP server (RFC 0005), or via the **`AGENT_TRACEPARENT`** env var when an
  orchestrator starts the pod — adopt its `trace_id` and use its `span_id` as the
  root `parent_span_id`.
- Else **mint a `trace_id` per `run_id`** (16 random bytes) so the run is
  self-correlated. Once minted, `LogCtx.trace_id` is `Some`, and every line
  carries `trace_id`.

`traceparent` is parsed per W3C: `00-<32hex trace_id>-<16hex span_id>-<2hex flags>`.
A malformed inbound value is ignored and we mint instead (never fail a run on a
bad trace header).

**Propagate outward (all default build):**

- **MCP calls:** set `_meta.traceparent` (+ `tracestate`/`baggage` when present)
  on every outbound `tools/call` and `resources/*` (RFC 0004). Two fields in a
  frame we already build → downstream MCP servers' spans line up even if agentd
  only logs.
- **LLM call:** set the standard `traceparent` HTTP header on the intelligence
  request (RFC 0006).
- **Subagents:** the spawn `telemetry` block (§3.5) carries `{trace_id, span_id}`
  so the child continues the same trace.

```rust
// obs/trace.rs (default build — propagation only)
pub fn current_traceparent() -> String; // "00-<trace_id>-<span_id>-01"
pub fn meta_with_trace(meta: &mut serde_json::Map<String, Value>); // inserts traceparent etc.
pub fn ingest(traceparent: Option<&str>) -> (/*trace_id*/[u8;16], /*parent_span*/Option<[u8;8]>);
```

**Span export is gated** (`otel`, §3.9). In the default build `trace.rs` carries
only propagation: there is no exporter, no batching, no SDK. Logs still carry
`trace_id`/`span_id`, so you can correlate logs to any upstream trace with no
backend.

### 3.7 Health (mode-aware)

Health is mode-specific. The big difference from a normal service: a reactive
agent is *supposed* to be idle, so **liveness is measured at the supervisor's
own event loop, not at the agent**.

| Mode | Readiness | Liveness | Terminal health |
|---|---|---|---|
| `once` | implicit (the run is the readiness) | n/a (bounded) | **exit code** is the entire signal |
| `loop`/`schedule` | config parsed, MCP connected, first tick armed → `proc.ready` | heartbeat advances each tick | exit code |
| `reactive` | MCP connected **and** all declared subscriptions reconciled (subscribed + read-after-subscribe, RFC 0003/0008) → `proc.ready` | supervisor heartbeat, **idle is healthy** | exit code |

**Liveness = the supervisor heartbeat.** The reactor (RFC 0002) bumps a
monotonic `last_loop_tick` on **every wake, including idle `recv_timeout`
expiries**. If `now - last_loop_tick` exceeds a threshold, the *supervisor* is
wedged → fail liveness → let the orchestrator restart the pod. **A stuck subagent
must NOT flip liveness** — the supervisor detects/kills it (RFC 0003) and emits
`subagent.stuck`, but the pod stays live; failing liveness on a stuck child would
destroy the whole healthy tree.

**Readiness = `proc.ready` reached and all declared subscriptions reconciled.**
Before that the pod is not "ready" so an orchestrator won't route work or count
it up.

**The health surface — minimal ladder, escalate only on opt-in:**

1. **Exit code (always, free).** Primary for one-shot, final for daemons. The
   stable exit-code table is owned by **RFC 0011** (this RFC consumes it; it is
   not redefined here).
2. **`--health-file PATH` (default daemon surface).** The supervisor writes the
   file every heartbeat — **no socket, no port** — via an atomic write
   (write temp + `rename`):

   ```json
   {"status":"ready","ts":"2026-06-25T10:00:00.123Z","hb":4821,
    "last_loop_tick_ms":34,"active_subagents":2,"run_id":"01J..."}
   ```

   `status` is `ready` | `draining`. A K8s `exec` probe reads it and checks
   `status` + `ts` freshness. One dependency-free file write per tick.
3. **Unix-socket health line (opt-in).** When `--serve-mcp unix:…` (RFC 0005) is
   already on, expose a trivial `health`/`ready` line on a sibling unix socket.
   Reuses existing socket machinery; no new TCP surface.
4. **HTTP `/healthz` + `/readyz` (opt-in, `--health-http ADDR`).** Only when an
   orchestrator wants real HTTP probes. Implemented with the **same hand-rolled
   blocking HTTP code** (RFC 0006) on one thread — no new dependency.
   `/healthz` = liveness (heartbeat fresh → 200, stale → 503);
   `/readyz` = readiness (ready + subs reconciled → 200, else 503). Side-effect-free.

**Default = exit code + `--health-file`** (the latter off for one-shot — a pure
one-shot CLI run carries zero health machinery). HTTP and socket are flag-gated.

### 3.8 Metrics — default = derive from logs

Because the event vocabulary (§3.3) is closed and well-keyed, every counter is a
`count by (event)` over the JSON stream and gauges are recoverable from
`subagent.spawn`/`subagent.exit` deltas. **No in-process metric registry, zero
dependencies.** For a minimal unit of work this is genuinely enough and is the
default.

The metrics that matter (derivable from logs by default; emitted directly under
the `metrics`/`otel` features):

- **Gauges:** `agent_active_subagents`, `agent_tree_depth`,
  `agent_subscriptions_active`, `agent_warm_sessions`, `agent_ready` (0/1),
  `agent_up` (1 while alive).
- **Counters:** `agent_loop_steps_total`, `agent_intel_calls_total{model}`,
  `agent_tokens_total{model,type=in|out}`, `agent_tool_calls_total{server,tool,ok}`,
  `agent_resource_events_total{server}`, `agent_triggers_total{kind,route}`,
  `agent_subagents_spawned_total`, `agent_subagents_exited_total{status}`,
  `agent_subagent_restarts_total{reason}`,
  `agent_subagent_stuck_kills_total{signal}` (the reliability headline metric),
  `agent_limit_exceeded_total{limit}`, `agent_mcp_connect_failures_total{server}`.
- **Histograms (otel only):** `gen_ai.client.operation.duration`,
  `agent_tool_duration_ms{server,tool}`, `agent_loop_step_duration_ms`.

**Cardinality discipline (binding):** **never** put `run_id`, `agent_id`,
`agent_path`, `call_id`, or resource URIs into metric labels — those are
unbounded and live in logs/traces only. Metric labels use bounded values only:
`server`, `tool`, `model`, `kind`, `route`, `status`, `limit`, `signal`,
`reason`, `type`.

**`metrics` feature (Prometheus text):** a tiny in-process table of atomic
counters/gauges feeds a hand-written **Prometheus 0.0.4 text exposition**
(`# HELP`/`# TYPE` + `name{labels} value`) served on the already-opt-in
surface (`/metrics` on `--health-http` or the unix socket). **No `prometheus`
or `metrics` crate** — it is plain text. No async, no SDK.

```rust
// obs/metrics.rs [feature = "metrics"]
pub fn incr(name: &'static str, labels: &[(&'static str, &str)]);
pub fn set_gauge(name: &'static str, value: i64);
pub fn render_prometheus() -> String; // # HELP / # TYPE / name{labels} value
```

### 3.9 `otel` feature — OTLP export + GenAI semconv

The one feature allowed to link heavier deps (`tracing` +
`tracing-opentelemetry` + `opentelemetry-otlp`, **HTTP exporter, not
grpc-tonic** — keeps tokio out of the default build; see assessment §2.2). Maps
our event taxonomy onto the OTel GenAI semantic conventions (experimental; gate
behind `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`):

| agent event/span | `gen_ai.operation.name` | Span name | Key attributes |
|---|---|---|---|
| `subagent.spawn` → `loop.final` | `invoke_agent` | `invoke_agent {agent.name}` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id` (= run/session id) |
| `intel.call` / `intel.result` | `chat` | `chat {model}` | `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons`, `gen_ai.request.max_tokens` |
| `tool.call` / `tool.result` | `execute_tool` | `execute_tool {tool.name}` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `mcp.method.name`, `mcp.session.id`, `server.address`/`server.port` |
| content (opt-in) | — | — | `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` |

Required metric `gen_ai.client.operation.duration`; recommended
`gen_ai.client.token.usage` (filtered by `gen_ai.token.type`). Supervisor
spawn/limit/stuck spans are agent-namespaced custom spans nested under the
GenAI ones.

**Instrument the client side; do not double-instrument.** agentd is an MCP
*client*; the server on the other side may also emit `execute_tool`/MCP spans.
agentd instruments the **client side** of the tool call ("I called this tool and
waited") and *propagates* context (§3.6) so the server's spans nest underneath —
the SEP-414 single-span-tree model. No duplicate spans.

**Export mechanics:** OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, pushed to a
**local collector / sidecar** so agentd stays thin and needs no batching/retry
sophistication (mirrors the terminate-complexity-at-the-sidecar pattern). Using
`tracing` + `tracing-opentelemetry` *inside this gate* is acceptable precisely
because it is opt-in and invisible to the default build.

**Token-accounting honesty (open item alignment):** tokens come from the
intelligence response `usage` (RFC 0006). A normalising gateway may reshape it;
agentd reads from the normalised response and logs `0`/`null` (never a guess)
when absent, to keep `tokens_total` trustworthy.

---

## 4. Interactions with other RFCs

- **RFC 0001 (core):** satisfies the "log structured events to stderr"
  obligation; the two-loop split drives the `comp:"supervisor"` vs `comp:"agent"`
  schema separation.
- **RFC 0002 (reactor):** the supervisor heartbeat is `last_loop_tick`, bumped on
  every reactor wake including idle `recv_timeout` expiries; SIGPIPE-ignore keeps
  the logger from killing the supervisor.
- **RFC 0003 (supervision/recovery):** emits `subagent.spawn/exit/signal/stuck/restart`
  and `limit.exceeded`; the stuck detector cites OS process state via `pid`;
  readiness depends on rebuild + read-after-subscribe reconciliation.
- **RFC 0004 (MCP client):** sets `_meta.traceparent` on outbound calls; folds
  `notifications/message` into `comp:"mcp"` log lines; emits `mcp.connect[.fail]/disconnect`.
- **RFC 0005 (self-MCP + control protocol):** mode-B `--aggregate-logs` frames
  child telemetry up the control channel; ingests inbound `traceparent` on the
  self-MCP server; the unix-socket health line and `/metrics` ride the served
  surface.
- **RFC 0006 (intelligence):** sets the `traceparent` HTTP header on the LLM call;
  `--health-http` and `/metrics` reuse the hand-rolled HTTP client; `usage`
  feeds token metrics; secrets via `resolve()` are allowlist-excluded from logs.
- **RFC 0007 (agentic loop):** emits `loop.start/step/final/error`, `intel.call/result`,
  `tool.call/result`; `result_status` maps to terminal statuses.
- **RFC 0008 (modes/routing):** `trigger.armed/fired{route}`,
  `subscribe/unsubscribe/resource.updated`; `agent_warm_sessions` and
  `trigger.fired{route}` align with the spawn-vs-continue routing rule.
- **RFC 0009 (subagent model):** the spawn `telemetry` block rides the spawn
  payload; depth/`agent_path` are supervisor-minted, never child-trusted.
- **RFC 0011 (cloud-native contract):** **owns** the exit-code table, config
  precedence/validation, and drain choreography. This RFC consumes the exit-code
  table for terminal health and adds the obs flags
  (`--log-level/--log-content/--aggregate-logs/--health-file/--health-http`,
  `AGENT_TRACEPARENT`, `OTEL_EXPORTER_OTLP_ENDPOINT`).
- **RFC 0012 (security):** the secrets field-allowlist and `Debug=***` rule;
  content capture is off by default per the untrusted-content stance.

---

## 5. Non-goals / Deferred

- **No `tracing` in the default build.** It is permitted only inside the `otel`
  gate. The default logger is hand-rolled.
- **No metrics client library, ever** — Prometheus text is hand-written; OTLP
  metrics ride `otel`.
- **No span export in the default build.** Propagation is on; export is gated.
- **No MCP `logging` capability.** The MCP 2026-07-28 RC deprecates it in favor
  of stderr + OpenTelemetry; agentd does not implement or advertise it.
- **No log file management / rotation / shipping in-binary.** stderr only; the
  container runtime / collector owns capture and rotation.
- **No second scheduling/aggregation subsystem.** Mode B reuses the existing
  control channel; the health file/socket/HTTP reuse existing machinery.
- **HTTP serving of `/healthz`/`/readyz`/`/metrics` is opt-in**, never on for a
  one-shot CLI run.

---

## 6. Open items

- **Content-capture redaction completeness.** Even with `--log-content`, a secret
  passed by the model as a free-form tool argument is not guaranteed redacted by
  the field allowlist. Resolution direction: keep `--log-content` a debug/non-prod
  switch and document it as such; a per-tool redaction allow/deny rule is a
  possible later addition, not v1.
- **GenAI semconv is experimental (2026); attribute names may shift.** Mitigation
  is already structural: the default JSON schema (§3.2) is ours and stable; the
  otel mapping lives in one feature-gated module behind an explicit opt-in
  env var. No action needed unless the convention stabilises differently.
