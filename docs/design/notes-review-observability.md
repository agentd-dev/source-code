# Design Review — Observability, Health, Tracing, Monitoring

**Lens:** observability / health / tracing / monitoring for a minimal, MCP-native,
reactive, cloud-native *unit of work*.
**Reviews:** RFC 0001 (`rfcs/0001-mcp-native-agent-runtime.md`).
**Author:** observability review pass.
**Date:** 2026-06-25.
**Status:** Design recommendation (durable artifact).

---

## 0. TL;DR recommendation

Ship **one observability primitive by default and one tiny health surface**, and
feature-gate everything heavier:

1. **DEFAULT — structured JSON-lines to stderr.** A *hand-rolled* ~150-line JSON
   logger, not `tracing` + `tracing-subscriber`. One event = one line. Every line
   carries the correlation tuple `{run_id, agent_id, agent_path, span_id,
   parent_span_id, event}`. This is the spine: logs double as the trace (each line
   is effectively a span event) and as the metric source (counters are derivable by
   scraping `event` types). This satisfies RFC §11's "log structured events to
   stdout/stderr" obligation with near-zero dependency weight.

2. **DEFAULT — health via exit code + a liveness file (+ optional tiny health
   line).** One-shot health *is* the exit code. Daemon/reactive health is a
   heartbeat the supervisor writes to a file (and a `Healthy`/`Draining` log event).
   A real HTTP `/healthz` is **opt-in**, only when self-MCP is already served over a
   socket — never pulled in for a one-shot CLI run.

3. **FEATURE-GATED — `metrics` (Prometheus text exposition)** on the same opt-in
   surface; **FEATURE-GATED — `otel` (OTLP traces, W3C trace-context propagation
   into MCP `_meta` and the intelligence call, GenAI semantic conventions)**.

The bias mirrors RFC §12: the minimal build carries JSON logging and nothing else.
You "turn the dial up" toward OpenTelemetry only in deployments that want it, and
you pay the binary/dependency cost only then.

This is *also* consistent with where the ecosystem is going: the **2026-07-28 MCP
spec RC deprecates MCP's own `logging` capability in favor of `stderr` (for stdio
transports) and structured OpenTelemetry** ([MCP RC](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)).
Our default (stderr JSON) is exactly the lightweight half; our `otel` feature is the
heavy half.

---

## 1. What "observable" must mean for *this* architecture

RFC 0001 has three properties that dominate the observability design, and they are
not the usual web-service properties:

- **It is a process tree, not a thread pool.** The unit of intelligence is a child
  *process* (`agentd` re-exec'd, RFC §4.2), nesting to a supervised tree (§6.3).
  Correlation therefore has to survive a **process boundary** and reconstruct a
  *tree*, not just a request. `ps`/`pstree` already show the OS tree; our job is to
  make the *logs* reassemble the same tree off-box.
- **The supervisor makes no LLM calls and holds no conversation state (§4.1).**
  So the supervisor's telemetry is about *lifecycle and control* (spawn, reap,
  limits, triggers, subscriptions, drain), and the subagent's telemetry is about
  *reasoning* (loop steps, tool calls, tokens). These are two different schemas and
  should be labeled as such (`comp: "supervisor"` vs `comp: "agent"`).
- **It is reactive and long-idle.** A reactive agent (§5.3) spends most of its life
  asleep, subscribed to MCP resources, doing *nothing*. "Healthy and idle" must be
  distinguishable from "hung," and "woke on a resource update" must be a
  first-class, traceable event. This is the single biggest difference from a normal
  request/response service: **the absence of activity is the normal state, so health
  cannot be inferred from traffic.**

Everything below is shaped by those three facts.

---

## 2. Structured logging (the default and the spine)

### 2.1 Decision: hand-rolled JSON logger, not `tracing`/`tracing-subscriber`

**Recommendation: hand-roll it.** Reasoning against the RFC §12 minimalism bar:

- `tracing` + `tracing-subscriber` (+ `tracing-opentelemetry` for spans) pulls a
  non-trivial dependency cone (`tracing-core`, `sharded-slab`, `thread_local`,
  `nu-ansi-term`/`matchers`/`regex` via the env-filter, etc.). It is excellent and
  idiomatic, but it is **designed for an async, in-process, many-threaded** world.
  `agentd` is deliberately *processes + a few threads* (§12: "no async runtime").
  We do not get the payoff that justifies `tracing`'s weight.
- The thing `tracing` buys you — span context plumbed implicitly through async tasks
  — we get **for free from the process tree**: a subagent's span context is just its
  spawn payload, passed once at exec time. There is no implicit context to thread.
- JSON serialization is already a non-negotiable dependency (§12: "JSON is the one
  non-negotiable dependency"). The same `serde_json::json!` / `Serialize` machinery
  that writes MCP frames writes log lines. The logger is **~100–200 lines**: a
  `log_event(event, fields)` that grabs the ambient correlation tuple from a
  process-global `LogCtx`, merges caller fields, writes one line to a locked
  `stderr` (or a buffered writer), and flushes on newline.

**When to reconsider:** if/when the `otel` feature lands and we want spans+logs+
metrics correlated through one SDK, `tracing` + `tracing-opentelemetry` becomes the
*natural* implementation **behind the feature gate**. So: hand-rolled JSON in the
default build; `tracing`-based pipeline allowed to appear only inside `--features
otel`. The default build never sees it.

This matches current guidance that the *log line itself* should carry `trace_id`/
`span_id` so logs and traces correlate with zero extra calls
([Dash0 structured logging](https://www.dash0.com/guides/structured-logging-for-modern-applications),
[Uptrace structured logging](https://uptrace.dev/glossary/structured-logging)) — we
just do that injection by hand from `LogCtx` instead of via an SDK.

### 2.2 Streams: stderr for telemetry, stdout for the result

A hard rule for a good cloud-native citizen and for one-shot CLI ergonomics:

- **stdout = the agent's *answer* only** (the final result of a one-shot run, or the
  control-channel JSON when in subagent mode). This keeps `agentd --instruction … |
  jq` clean and keeps the result machine-parseable.
- **stderr = all structured telemetry** (every log line / event). Log collectors in
  K8s capture both streams anyway; separating them means a human or a pipe gets the
  answer without telemetry noise, which is the conventional Unix contract.

(Subagent mode is the one wrinkle: there, stdout is the *control channel* to the
parent, §6.2. Telemetry still goes to stderr; the parent forwards/relabels child
stderr — see §6.)

### 2.3 The line schema

One event per line. Newline-delimited JSON (JSONL/NDJSON). Stable, short, snake_case
keys. Proposed canonical fields:

| Field | Type | Always? | Meaning |
|---|---|---|---|
| `ts` | string | yes | RFC 3339 / ISO-8601 UTC, e.g. `2026-06-25T10:00:00.123Z`. UTC always. |
| `level` | string | yes | `trace`/`debug`/`info`/`warn`/`error`. |
| `event` | string | yes | Dotted event type — the primary index key (see §2.4). |
| `msg` | string | no | Short human string; optional, never the structured payload. |
| `run_id` | string | yes | ULID/UUID for the whole invocation (the unit of work). Stable across the tree. |
| `agent_id` | string | yes | ID of the emitting agent process. Supervisor uses a reserved id (e.g. `root`/`sup`). |
| `agent_path` | string | yes | Tree path, e.g. `0` (root) / `0.2` / `0.2.1`. Encodes parent lineage *in the value* (see §6.1). |
| `comp` | string | yes | `supervisor` \| `agent` \| `mcp` \| `intel`. Which subsystem emitted it. |
| `span_id` | string | when in a span | 8-byte hex; current span (loop step / tool call / inference). |
| `parent_span_id` | string | when nested | Enclosing span. |
| `trace_id` | string | when otel/propagation on | 16-byte hex W3C trace id (see §5). Omitted in the pure-default build unless a `traceparent` was supplied. |
| `dur_ms` | number | on `*.end`/`*.result` | Duration of the span/operation. |
| `err` | object | on errors | `{type, message}`. Structured, never a stringified stack. |
| …event-specific fields | | | e.g. `tool`, `server`, `tokens_in`, `resource_uri`. |

Notes:
- **`agent_path` is the cheap superpower.** Because it encodes lineage as a
  dotted path, an operator can `grep '"agent_path":"0.2'` to get a subtree, and a
  collector can build the tree without a join. It is the log-native equivalent of a
  span parent pointer and works even with `otel` off.
- **Secrets never appear.** RFC §13 forbids credentials in logs/transcripts. The
  logger MUST treat the intelligence token, MCP server env secrets, and tool
  arguments/results as **opt-in** content (see §2.5). Field allowlist by default.

### 2.4 Event taxonomy (the `event` vocabulary)

The `event` string is the backbone — it is what you filter, count, and alert on.
Keep it a small, closed, dotted vocabulary. Proposed v1 set:

**Supervisor / lifecycle (`comp: supervisor`):**
- `proc.start` — process booted; fields: `mode`, `pid`, `version`, `argv_hash`.
- `proc.ready` — config parsed, MCP servers connected, triggers armed (readiness, §3).
- `proc.shutdown` — entering drain; fields: `signal`, `reason`.
- `proc.exit` — final; fields: `code`, `uptime_ms`.
- `config.loaded` — fields: `mcp_servers` (count/names), `mode`, limits (no secrets).
- `mcp.connect` / `mcp.connect.fail` / `mcp.disconnect` — per server; fields:
  `server`, `transport`, `tools` (count), `resources` (count), `err`.
- `trigger.armed` — fields: `kind` (`once`/`loop`/`reactive`/`schedule`), detail.
- `trigger.fired` — fields: `kind`, `resource_uri?`, `route` (`spawn`/`continue`).
- `subscribe` / `unsubscribe` — fields: `resource_uri`, `server`, `by` (config/agent).
- `resource.updated` — an inbound `notifications/resources/updated`; fields:
  `resource_uri`, `server`. **This is the reactive heartbeat-of-meaning.**
- `subagent.spawn` — fields: `child_agent_id`, `child_path`, `instruction_hash`,
  `tool_scope`, `limits`, `depth`.
- `subagent.exit` — fields: `child_agent_id`, `code`, `result_status`, `dur_ms`.
- `subagent.signal` — pause/resume/cancel/inject; fields: `child_agent_id`, `action`.
- `subagent.stuck` — detector tripped (no heartbeat / past deadline); fields:
  `child_agent_id`, `last_event_age_ms`, `action` (`sigterm`/`sigkill`). **(§4.3)**
- `subagent.restart` — supervisor restarted a session/child; fields: `reason`,
  `restarts_total`.
- `limit.exceeded` — fields: `limit` (`steps`/`tokens`/`deadline`/`depth`/`tree_tokens`),
  `value`, `cap`.

**Agentic loop (`comp: agent`):**
- `loop.start` — a turn/continuation begins; fields: `trigger` (`spawn`/`continue`/
  `resume`), `step`.
- `loop.step` — one think→act iteration; fields: `step`, `tokens_in`, `tokens_out`.
- `intel.call` / `intel.result` — the LLM request/response (`comp: intel`); fields:
  `model`, `tokens_in`, `tokens_out`, `finish_reason`, `dur_ms`. **(maps to GenAI
  `chat`/inference span, §5.3)**
- `tool.call` — fields: `server`, `tool`, `call_id`, `args_hash` (`args` only if
  content capture on). **(maps to GenAI `execute_tool`)**
- `tool.result` — fields: `server`, `tool`, `call_id`, `ok`, `dur_ms`,
  `result_bytes` (`result` only if content capture on).
- `loop.final` — agent produced its result; fields: `step`, `result_status`,
  `result_bytes`.
- `loop.error` — fields: `err`, `step`.

This closed set is the contract for §4 (metrics-from-logs) and §5 (span mapping).
Adding an event later is cheap; renaming one is a breaking change to dashboards, so
nail these names now.

### 2.5 Content capture (prompts / tool args / results) — opt-in, off by default

Mirror the OpenTelemetry GenAI stance exactly: **"By default, no prompt content or
tool arguments are captured… these can contain sensitive data"**
([OTel GenAI observability](https://opentelemetry.io/blog/2026/genai-observability/)).

- Default: log **hashes/lengths** (`args_hash`, `result_bytes`, `instruction_hash`),
  never raw content. This is also forced by RFC §13 (no secrets in transcripts).
- `--log-content` (env `AGENTD_LOG_CONTENT`) opts in to capturing
  `gen_ai.input.messages` / `gen_ai.output.messages` / `gen_ai.tool.call.arguments`
  / `gen_ai.tool.call.result` equivalents, for debugging. Loud, gated, redaction-
  aware (still strip the known-secret fields).

---

## 3. Healthcheck (what "healthy" means per mode)

Health is **mode-specific** here, which most designs get wrong by assuming a server.

### 3.1 Liveness vs readiness vs the three modes

| Mode | Readiness ("ready to do work") | Liveness ("not hung") | Terminal health |
|---|---|---|---|
| **one-shot** (§5.1) | Implicit; the run is the readiness. | n/a (bounded). | **exit code** is the entire health signal. |
| **loop/interval** (§5.2) | Config parsed, MCP connected, first tick armed → `proc.ready`. | Heartbeat advances each tick; watchdog if a tick overruns its deadline. | exit code on terminate. |
| **reactive** (§5.3) | Subscriptions established (`resources/subscribe` ACKed) → `proc.ready`. | **Hard part:** the agent is *supposed* to be idle. Liveness = "the supervisor's event loop is still pumping and subscriptions are still live," **not** "work is happening." | exit code on terminate. |

The reactive row is why liveness must be measured at the **supervisor event loop**,
not at the agent. A healthy reactive agent can be idle for hours; that is success,
not a hang. So:

- **Liveness** = the supervisor heartbeat (a monotonically increasing counter +
  timestamp the supervisor updates every loop tick of its own select/poll, *including
  idle waits*). If the heartbeat timestamp goes stale beyond a threshold, the
  *supervisor* is wedged → fail liveness → let the orchestrator restart the pod.
- **Readiness** = `proc.ready` reached and all declared MCP servers connected and
  all declared subscriptions ACKed. Before that, the pod should not be considered
  "ready" (so an orchestrator won't route work / count it as up).

### 3.2 The health *surface* — minimal ladder

Offer the cheapest thing that works, escalating only on opt-in:

1. **Exit codes (always, free).** The primary health signal for one-shot and the
   final signal for daemons. Define a **stable exit-code table** (§3.3). This alone
   satisfies a K8s `Job`/`CronJob`.
2. **Liveness file (default for daemon/reactive).** The supervisor writes
   `--health-file PATH` (e.g. `/run/agentd/health.json`) every heartbeat:
   `{"status":"ready|draining","ts":…,"hb":N,"active_subagents":K,"run_id":…}`. A
   K8s `livenessProbe`/`readinessProbe` can `exec` a 5-line script that checks
   `status` and `ts` freshness. **No socket, no HTTP, no port** — just a file and an
   `fsync`-light atomic write. This is the recommended daemon default because it
   costs one dependency-free file write per tick.
3. **Unix socket health (opt-in).** If `--serve-mcp unix:…` is already on, expose a
   trivial `health` line/endpoint on a sibling unix socket. Reuses the socket
   machinery already present; no new TCP surface.
4. **HTTP `/healthz` + `/readyz` (opt-in, `--health-http :8080`).** Only when an
   orchestrator wants real HTTP probes. Implemented with the **same hand-rolled
   blocking HTTP code** the RFC already plans for `https://` intelligence and
   HTTP-transport MCP (§12) — so it adds *no new dependency*, just a tiny handler
   loop on one thread. `/healthz` = liveness (heartbeat fresh), `/readyz` =
   readiness (ready + subs live). Keep them dependency-free and side-effect-free.

**Recommendation:** default = exit codes + liveness file. HTTP and socket health are
feature/flag-gated. This keeps a one-shot CLI run carrying *zero* health machinery
(matching RFC §8's "pure one-shot CLI run carries none of it" philosophy).

### 3.3 Exit-code table (make it a contract)

A clean citizen returns *meaningful* codes (RFC §11). Proposed:

| Code | Meaning |
|---|---|
| `0` | Success — instruction completed (one-shot) / clean drain (daemon). |
| `1` | Generic/unspecified failure. |
| `2` | Config/usage error (bad flags, missing intelligence). |
| `3` | Intelligence unreachable / auth failure. |
| `4` | MCP server connect failure (a required server never came up). |
| `5` | Limit exceeded (steps/tokens/deadline/depth) without a result. |
| `6` | Subagent tree failure (a required child died/was killed). |
| `124` | Deadline/timeout (mnemonic match to `timeout(1)`). |
| `137` / `143` | Killed by `SIGKILL` / `SIGTERM` (128+signal convention; OS-set). |

Stable codes let an external operator (RFC §11's K8s `Job`) make restart/backoff
decisions without parsing logs.

---

## 4. Metrics (what to count, and how to expose it minimally)

### 4.1 The metrics that matter for this runtime

Drawn straight from the RFC's moving parts and the hard requirements (stuck-kills,
restarts, tree size):

**Gauges (point-in-time):**
- `agentd_active_subagents` — current live children in the tree.
- `agentd_tree_depth` — current max depth.
- `agentd_subscriptions_active` — live MCP resource subscriptions.
- `agentd_warm_sessions` — suspended reactive sessions held warm (§5.3).
- `agentd_ready` (0/1) and `agentd_up` (always 1 while process lives).

**Counters (monotonic):**
- `agentd_loop_steps_total{agent_path}` — agentic iterations.
- `agentd_intel_calls_total{model}` / `agentd_tokens_total{model,type=in|out}`.
- `agentd_tool_calls_total{server,tool,ok}`.
- `agentd_resource_events_total{server}` — `notifications/resources/updated` seen.
- `agentd_triggers_total{kind,route}` — spawn vs continue.
- `agentd_subagents_spawned_total` / `agentd_subagents_exited_total{status}`.
- `agentd_subagent_restarts_total{reason}`.
- `agentd_subagent_stuck_kills_total{signal}` — **the reliability headline metric**.
- `agentd_limit_exceeded_total{limit}`.
- `agentd_mcp_connect_failures_total{server}`.

**Histograms (opt-in, otel build only — keep the default cheap):**
- `agentd_intel_duration_ms` (maps to GenAI `gen_ai.client.operation.duration`).
- `agentd_tool_duration_ms{server,tool}`.
- `agentd_loop_step_duration_ms`.

### 4.2 Exposition: prefer "metrics are derivable from logs," gate a Prometheus endpoint

Three options, in order of weight:

1. **(Default) No metrics endpoint — derive from logs.** Because the event taxonomy
   (§2.4) is closed and well-keyed, every counter above is a
   `count by (event)` over the JSON log stream, and gauges are recoverable from
   `subagent.spawn`/`subagent.exit` deltas. A Loki/ELK/`vector` pipeline computes
   them with no in-process metric registry at all. For a minimal *unit of work* this
   is genuinely enough and costs zero dependencies. This is the recommended default.
2. **(Feature `metrics`) Prometheus text exposition on the opt-in surface.** When
   `--health-http`/`--serve-mcp` is on, also serve `/metrics` in **Prometheus text
   exposition format** (it is plain text — `# TYPE`/`# HELP` + `name{labels} value`
   — trivially hand-writable, *no client library needed*). OpenMetrics is a strict
   superset; emit the Prometheus 0.0.4 text format which every scraper accepts.
   A tiny in-process atomic counter/gauge table feeds it. Still no async, no SDK.
3. **(Feature `otel`) OTLP metrics export.** Histograms + push to a collector. Only
   here do we accept a metrics SDK, and only behind the gate.

**Recommendation:** default to *logs-as-metrics*; `metrics` feature adds a
hand-written Prometheus `/metrics` (no `prometheus`/`metrics` crate needed); OTLP
metrics ride the `otel` feature. Cardinality discipline: **never** put `run_id`,
`agent_id`, `call_id`, or resource URIs into metric labels (unbounded) — those live
in logs/traces; metrics use bounded labels only (`server`, `tool`, `model`, `kind`,
`status`, `limit`).

---

## 5. Tracing (W3C trace-context + GenAI semantic conventions, feature-gated)

### 5.1 The single most important external finding

The **2026-07-28 MCP spec RC adopted SEP-414: W3C Trace Context propagation inside
the MCP `_meta` field**, with **fixed JSON key names `traceparent`, `tracestate`,
and `baggage`** ([MCP RC](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)).
A trace that starts upstream "can follow a tool call through the client SDK, the MCP
server, and whatever the server calls downstream, and show up as a single span tree."

This is a gift to `agentd`'s design: **trace propagation into tools is now a protocol
feature, not something we invent.** And the same RC **deprecates MCP's own `logging`
capability in favor of stderr + OpenTelemetry** — which validates our default
(stderr JSON) and our gated `otel` path as exactly the two halves the ecosystem
chose.

### 5.2 What `agentd` propagates, and where

Even in the **default (no-otel) build**, do the cheap half of context propagation:

- **Ingest.** If a `traceparent` arrives — on an inbound MCP request to `agentd`'s
  self-MCP server (§8), or via `AGENTD_TRACEPARENT` env when an orchestrator starts
  the pod — adopt its `trace_id` and use the incoming `span_id` as `parent_span_id`.
  If none arrives, **mint a `trace_id` per `run_id`** so the run is self-correlated.
- **Propagate outward in `_meta`.** On every outbound MCP `tools/call` and
  `resources/*`, set `_meta.traceparent` (+ `tracestate`/`baggage`) to the current
  span. This is *just two JSON fields in a frame we already build* — essentially free
  and worth doing always, because it makes downstream MCP servers' traces line up
  even if `agentd` itself only logs.
- **Propagate to intelligence.** On the LLM HTTP call, set the standard
  `traceparent` HTTP header. Same near-zero cost.
- **Propagate to subagents.** The spawn payload (§6.2) carries the parent's
  `{trace_id, span_id}` so the child continues the same trace (see §6.1). This is
  the process-tree analog of in-process span context and is the crux of tree
  correlation.

So: **context propagation is on by default (it's a couple of JSON/header fields);
span *export* (OTLP) is feature-gated.** Logs always carry `trace_id`/`span_id` when
a trace exists, so even without an OTLP backend you can correlate logs to any
upstream trace.

### 5.3 Span model → OpenTelemetry GenAI semantic conventions (the `otel` feature)

When built with `--features otel`, map our event taxonomy onto the **OTel GenAI
semantic conventions** (experimental as of 2026, but vendor-supported by Datadog/
Honeycomb/New Relic; gate behind `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`)
([OTel GenAI agent spans](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans/),
[OTel GenAI blog](https://opentelemetry.io/blog/2026/genai-observability/),
[techbytes cheat sheet](https://techbytes.app/posts/opentelemetry-genai-agent-semconv-cheat-sheet-2026/)):

| `agentd` event/span | GenAI `gen_ai.operation.name` | Span name | Key attributes |
|---|---|---|---|
| subagent run (`subagent.spawn`→`loop.final`) | `invoke_agent` | `invoke_agent {agent.name}` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id` (= our session/run id) |
| `intel.call`/`intel.result` | `chat` (inference) | `chat {model}` | `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons`, `gen_ai.request.max_tokens` |
| `tool.call`/`tool.result` | `execute_tool` | `execute_tool {tool.name}` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, and MCP-specific `mcp.method.name`, `mcp.session.id`, plus `server.address`/`server.port` |
| content (opt-in) | — | — | `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`, `gen_ai.tool.call.arguments`, `gen_ai.tool.call.result` |

Metrics in the otel build follow the conventions too: the **required**
`gen_ai.client.operation.duration` histogram and the **recommended**
`gen_ai.client.token.usage` histogram (filter by `gen_ai.token.type`). The
supervisor's spawn/limit/stuck spans are `agentd`-namespaced custom spans nested
under the GenAI ones.

**Important nuance — capture, don't double-instrument.** `agentd` is an MCP *client*.
The MCP server on the other side may *also* emit `execute_tool`/MCP spans. To avoid
duplicate spans, `agentd` instruments the **client side of the tool call** (the
`execute_tool` span representing "I called this tool and waited") and *propagates*
context so the server's own spans (if any) nest underneath — exactly the SEP-414
single-span-tree model. The conventions explicitly anticipate this layering.

### 5.4 Export mechanics (otel feature only)

- **OTLP/HTTP** (protobuf or JSON) to a collector endpoint from `OTEL_EXPORTER_OTLP_ENDPOINT`.
  Prefer pushing to a **local collector / sidecar** so `agentd` itself stays thin and
  doesn't need batching/retry sophistication. This mirrors RFC §7.2's "terminate
  complexity at the sidecar" pattern.
- Implementation may legitimately use `tracing` + `tracing-opentelemetry` + the OTel
  SDK **inside this feature** — the weight is acceptable *because it is opt-in* and
  invisible to the default build.

---

## 6. Correlation across the subagent process tree (the crux)

This is the hardest and most distinctive part, and it has a clean answer because the
RFC already passes a structured spawn payload over the control channel (§6.2).

### 6.1 The correlation contract carried at spawn

When the supervisor (or a parent agent via `subagent.spawn`, §8) launches a child,
the spawn payload — alongside instruction/context/scope/limits — carries a
**`telemetry` block**:

```json
{
  "run_id":        "01J…",            // constant for the whole tree
  "trace_id":      "4bf92f…",         // constant for the whole tree (or upstream's)
  "parent_span_id":"00f067…",         // the parent's current span — child's parent
  "agent_path":    "0.2",             // parent path + child index
  "agent_id":      "01J…child",       // child's own id
  "log_level":     "info",
  "log_content":   false
}
```

From these, the child constructs its own `LogCtx` and every line it emits is
*pre-correlated* to the tree. **No registry, no service discovery, no join key
negotiation** — lineage is passed down once at exec, exactly like environment
inheritance. This is the process-tree equivalent of OTel context propagation, and it
is why we don't need `tracing`'s implicit context machinery (§2.1).

- `run_id` answers "all telemetry for this unit of work."
- `trace_id` answers "one distributed trace across the whole tree (+ upstream)."
- `agent_path` answers "this subtree" via prefix match, with **no backend join**.
- `parent_span_id` lets each child's root span nest under the parent's spawn span.

### 6.2 Getting child telemetry off-box

Two viable wirings; recommend **(A) for K8s, (B) available for nesting depth**:

- **(A) Child writes its own stderr.** Each subagent writes JSON lines directly to
  *its* stderr, which the container runtime/collector already captures. Because every
  line self-identifies (`run_id`/`agent_path`/`trace_id`), no aggregation in
  `agentd` is needed. Cleanest for a cloud-native collector; the supervisor never
  becomes a logging bottleneck.
- **(B) Child telemetry framed up the control channel.** Telemetry events also ride
  the existing event stream the child sends its parent (§6.1 "every loop turn streams
  events… up the control channel"). The supervisor can then re-emit/forward them on
  its own stderr. Useful when only the root process's stderr is captured (e.g. deeply
  nested local runs) — but the supervisor must **forward, never rewrite** the
  correlation fields, or the tree breaks.

**Recommendation:** default to (A) (every process logs to its own stderr; collector
reassembles by `run_id`/`agent_path`), and offer (B) as `--aggregate-logs` for
single-stream environments. Either way, the correlation tuple is invariant.

### 6.3 The OS tree is a free, ground-truth observability source

Because subagents are real processes (RFC §4.2), `pstree`/`ps`/`/proc` already expose
the live tree, RSS, CPU, and state. The supervisor should **log `pid` in
`subagent.spawn`** so the *log* tree and the *OS* tree are joinable, and the
`subagent.stuck` detector (§7) can cite both the last-event age *and* the OS process
state (`D`/`Z`/running) in the same event. This is observability we get for free by
choosing processes over tasks — lean into it.

---

## 7. Reliability signals (RFC requirement #8: detect dead/stuck, recover, stay stable)

Observability and the stability requirement meet here. The supervisor needs *signals*
to act on, and those signals must also be *emitted* so an operator sees them:

- **Heartbeat / liveness per subagent.** Each child emits at least a `loop.step` (or
  a cheap `agent.heartbeat`) on a bounded cadence. The supervisor tracks
  `last_event_age_ms` per child. Past a threshold (or past the child's deadline), it
  emits `subagent.stuck` then escalates `SIGTERM`→`SIGKILL`, emitting
  `subagent.signal`/`subagent.exit`. Increment `agentd_subagent_stuck_kills_total`.
- **Dead detection.** `waitpid`/exit-pipe closure → `subagent.exit{code}`; an
  unexpected exit (non-zero, no `loop.final`) → `subagent.restart` decision logged
  with `reason`.
- **Restart accounting.** `agentd_subagent_restarts_total{reason}` + a restart-storm
  guard (if a session restarts >N times in a window, stop and surface
  `limit.exceeded{limit:"restart_storm"}` rather than thrash).
- **Supervisor self-watchdog.** The supervisor heartbeat (§3.1) is the top-level
  liveness; if its own loop stalls, the health surface goes unhealthy and the
  orchestrator restarts the pod (we don't try to self-heal a wedged supervisor —
  fail loudly, let the scheduler recycle, matching RFC §11's clean-citizen stance and
  the recent commit `e48f2f0 "fail loudly"`).

These are all *already* in the event taxonomy (§2.4), so reliability is observable by
construction, not bolted on.

---

## 8. Configuration surface (additions to RFC §10)

Keep flat and small, matching the RFC's style. Proposed knobs (all with `AGENTD_*`
env equivalents):

| Concern | Flag | Default |
|---|---|---|
| Log level | `--log-level` | `info` |
| Log content capture | `--log-content` | off |
| Log aggregation up-tree | `--aggregate-logs` | off (children self-log) |
| Liveness/health file | `--health-file PATH` | off for one-shot; recommended on for daemon |
| HTTP health | `--health-http ADDR` | off |
| Prometheus metrics | `--metrics` (needs `metrics` feature + a surface) | off |
| OTLP export | `OTEL_EXPORTER_OTLP_ENDPOINT` (needs `otel` feature) | off |
| Inbound trace | `AGENTD_TRACEPARENT` | none (mint per run) |

Cargo features: `metrics` (Prometheus text exposition), `otel` (OTLP + GenAI
semconv + `tracing` pipeline). The **default build has neither**, carrying only the
hand-rolled JSON logger and the file/exit-code health.

---

## 9. Open questions / risks specific to observability

1. **GenAI semconv is experimental (2026).** Attribute names may shift. Mitigation:
   keep the otel mapping in *one* module behind the feature; the default JSON schema
   (§2.3) is *ours* and stable regardless. Gate the experimental opt-in explicitly.
2. **Token accounting source of truth.** Tokens come from the intelligence response
   (`usage`), but a normalising gateway (RFC §7.2) may reshape it. Decide that
   `agentd` reads usage from the *normalised* gateway response and logs `0`/`null`
   (not a guess) when absent — never estimate, to keep `tokens_total` trustworthy.
3. **Content-capture redaction completeness.** Even with `--log-content`, secrets in
   tool args (e.g. a token passed to an MCP tool) must be redacted. Needs an explicit
   redaction allow/deny rule, or content capture stays a debug-only, non-prod switch.
4. **`agentd_warm_sessions` and reactive routing (RFC §14 Q5).** The metric and the
   `trigger.fired{route}` event depend on the still-open spawn-vs-continue routing
   rule; align names once that lands.
5. **Aggregation ordering (mode B).** Forwarded child logs can arrive out of order
   relative to the parent's; rely on `ts` + `span_id` for ordering, never on arrival
   order. Document that consumers sort by `ts`.

---

## 10. Concrete recommendation summary

- **Default build = hand-rolled JSON-lines logger to stderr** (one event/line, the
  §2.3 schema, the §2.4 closed event vocabulary) **+ exit codes + a liveness file.**
  No `tracing`, no metrics SDK, no OTLP, no HTTP server. Honors RFC §12.
- **Context propagation (traceparent in MCP `_meta` per SEP-414, in the LLM HTTP
  header, and in the spawn payload) is ON by default** — it's a couple of JSON/header
  fields and it makes downstream + tree traces line up for free.
- **`metrics` feature** adds a hand-written **Prometheus text `/metrics`** on an
  already-opt-in socket/HTTP surface — no metrics crate. Otherwise metrics are
  derived from the structured logs.
- **`otel` feature** adds **OTLP export + OpenTelemetry GenAI semantic-convention
  spans/metrics** (`invoke_agent`/`chat`/`execute_tool`, `gen_ai.*`, `mcp.*`), pushed
  to a sidecar collector; this is the only place heavier deps (incl. `tracing`) are
  allowed.
- **Tree correlation = a `telemetry` block in the spawn payload** carrying
  `{run_id, trace_id, parent_span_id, agent_path, agent_id}`; every process self-logs
  pre-correlated, the collector reassembles by `run_id` + `agent_path` prefix with no
  join. The OS process tree (`pid` in logs) is a free ground-truth cross-check.
- **Health is mode-aware:** one-shot = exit code; loop/reactive = supervisor
  heartbeat liveness (idle is healthy) + readiness = MCP-connected + subscriptions
  ACKed; HTTP `/healthz`+`/readyz` only on opt-in via the existing hand-rolled HTTP.
- **Reliability signals (`subagent.stuck`/`restart`/`limit.exceeded`,
  `*_stuck_kills_total`) are part of the default event taxonomy** so requirement #8
  is observable by construction.

This gives a *minimal-but-complete* story: the smallest build is just clean JSON logs
+ exit code + heartbeat file (auditable in an afternoon, RFC §12), and the dial turns
all the way up to full W3C-trace-context + OpenTelemetry GenAI conventions for
deployments that want it — without ever taxing the deployments that don't.

---

## Sources

- [OpenTelemetry: Inside the LLM Call — GenAI Observability](https://opentelemetry.io/blog/2026/genai-observability/)
- [OpenTelemetry: GenAI agent & framework spans semconv](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans/)
- [OpenTelemetry GenAI Agent SemConv Cheat Sheet 2026](https://techbytes.app/posts/opentelemetry-genai-agent-semconv-cheat-sheet-2026/)
- [MCP 2026-07-28 Specification Release Candidate (SEP-414 trace context; logging deprecation)](https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/)
- [W3C Trace Context (traceparent / tracestate)](https://www.w3.org/TR/trace-context/)
- [Dash0: Structured logging for modern applications](https://www.dash0.com/guides/structured-logging-for-modern-applications)
- [Uptrace: Structured logging best practices](https://uptrace.dev/glossary/structured-logging)
- [Uptrace: OpenTelemetry for AI systems](https://uptrace.dev/blog/opentelemetry-ai-systems)
- [Greptime: How OpenTelemetry traces LLM calls, agent reasoning, and MCP tools](https://greptime.com/blogs/2026-05-09-opentelemetry-genai-semantic-conventions)
- [Google Cloud: Structured logging](https://docs.cloud.google.com/logging/docs/structured-logging)
