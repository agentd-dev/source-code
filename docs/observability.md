# Observability

agentd is a *process tree*, not a thread pool, and it is reactive — it spends
most of its life asleep. That shapes everything below. The contract has three
jobs:

1. **Reassemble the tree off-box.** The unit of intelligence is a child
   *process* (the same binary re-exec'd) nesting into a supervised tree. `ps`
   and `pstree` already show that tree on the box; the logs must reassemble the
   *same* tree off-box, **with no backend join**.
2. **Keep two schemas honest.** The supervisor makes no LLM calls and holds no
   conversation state — its telemetry is lifecycle/control. The subagent's
   telemetry is reasoning (steps, tool calls, tokens). Same line schema, two
   `comp` labels.
3. **Distinguish "healthy and idle" from "hung."** A reactive agent subscribed
   to MCP resources idles for hours by design, so **health is never inferred
   from traffic** — it is measured at the supervisor's own event loop.

The default build ships exactly two things: a hand-rolled JSON-lines logger to
stderr (no `tracing`, no metrics SDK, no OTLP) and a tiny health surface (exit
code + an optional `--health-file`). Everything heavier is feature-gated. Full
rationale is in [RFC 0010](../rfcs/0010-observability-health-telemetry.md).

> **Status.** The runtime is implemented: the supervisor reactor, MCP client,
> intelligence client, the agentic loop, all four run modes, and therefore the
> events below are all live. The examples here describe real behaviour.

---

## stdout vs stderr

The split is absolute:

- **stdout = the agent's result only.** For a `once` run, the final result body
  goes to stdout and nothing else does. Pipe it straight into `jq`.
- **stderr = all telemetry.** One JSON event per line, NDJSON. Every line
  self-identifies (`run_id`, `agent_path`, `pid`, …), so the container
  runtime/collector captures stderr and you reassemble the tree later.

In subagent mode stdout is the control channel back to the parent (RFC 0005),
so telemetry still goes to **stderr** — never mixed into the channel.

```sh
# result on stdout, telemetry on stderr — cleanly separable
agentd --instruction "summarise /data/report.md" \
       --intelligence unix:/run/intel.sock \
       --mcp fs="mcp-server-fs --root /data" \
  > result.json 2> telemetry.ndjson
```

---

## The line schema

One event per line, NDJSON, snake_case keys, stable. Renaming a field is a
breaking change. The canonical block is written first; event-specific fields are
merged after it and can never shadow a canonical key.

| Field | Always | Meaning |
|---|---|---|
| `ts` | yes | RFC 3339 UTC, millisecond precision, e.g. `2026-06-25T10:00:00.123Z`. Always UTC — no local time, ever. |
| `level` | yes | `trace` \| `debug` \| `info` \| `warn` \| `error` |
| `event` | yes | dotted event type from the closed vocabulary — the primary index key |
| `run_id` | yes | ULID for the whole invocation (the unit of work), constant across the entire tree |
| `agent_id` | yes | emitting process id; the supervisor uses the reserved `sup` / `root` |
| `agent_path` | yes | dotted tree path (`0`, `0.2`, `0.2.1`) — **the cheap superpower:** subtree queries by prefix, no backend join |
| `comp` | yes | `supervisor` \| `agent` \| `mcp` \| `intel` |
| `pid` | yes | joins the log tree to the free OS `pstree` |
| `span_id` / `parent_span_id` | in-span | 8-byte hex |
| `trace_id` | when propagation on | 16-byte hex, W3C |
| `dur_ms` | on `*.end` / `*.result` | duration in milliseconds |
| `err` | on errors | structured `{ "type": "...", "message": "..." }` — never a stringified stack |
| `msg` | optional | a short human string; never the structured payload |
| event-specific | | `tool`, `server`, `tokens_in` / `tokens_out`, `resource_uri`, `route`, `call_id`, … |

Example — one supervisor line and one agent line:

```json
{"ts":"2026-06-25T10:00:00.012Z","level":"info","event":"subagent.spawn","run_id":"01J8XAMPLE...","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":1421,"child_agent_id":"01J8...c","child_path":"0.2","instruction_hash":"b1946ac9","tool_scope":["fs.read"],"depth":1}
{"ts":"2026-06-25T10:00:01.534Z","level":"info","event":"tool.result","run_id":"01J8XAMPLE...","agent_id":"01J8...c","agent_path":"0.2","comp":"agent","pid":1457,"span_id":"a1b2c3d4e5f60718","parent_span_id":"00f067aa0ba902b7","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","server":"fs","tool":"read_file","call_id":"c-7","ok":true,"dur_ms":42,"result_bytes":2048}
```

Set verbosity with `--log-level trace|debug|info|warn|error` (default `info`;
env `AGENTD_LOG_LEVEL`). The level filter is a cheap integer compare *before* any
allocation — below-level calls cost essentially nothing.

---

## The closed event vocabulary

The `event` string is the backbone — what you filter, count, and alert on. It is
a small, **closed**, dotted set. Adding an event later is cheap; renaming one
breaks dashboards. The supervisor/lifecycle and agentic-loop events below are
the v1 set (build-gated surfaces add a few more, noted inline).

### Supervisor / lifecycle (`comp:"supervisor"`)

| Event | Fields beyond canonical |
|---|---|
| `proc.start` | `mode`, `pid`, `version`, `argv_hash` |
| `proc.ready` | readiness reached (see [Health](#health-mode-aware)) |
| `proc.shutdown` | `signal`, `reason` |
| `proc.exit` | `code`, `uptime_ms` |
| `config.loaded` | `mcp_servers` (count/names), `mode`, limits — no secrets |
| `mcp.connect` | `server`, `transport`, `tools` (count), `resources` (count) |
| `mcp.connect.fail` | `server`, `transport`, `err` |
| `mcp.disconnect` | `server`, `reason` |
| `trigger.armed` | `kind` (`once`/`loop`/`reactive`/`schedule`), detail |
| `trigger.fired` | `kind`, `resource_uri?`, `route` (`spawn`/`continue`) |
| `subscribe` | `resource_uri`, `server`, `by` (`config`/`agent`) |
| `unsubscribe` | `resource_uri`, `server`, `by` |
| `resource.updated` | `resource_uri`, `server` — the reactive "heartbeat of meaning" |
| `subagent.spawn` | `node`, `depth` (the child re-exec'd) |
| `subagent.ready` / `subagent.result` / `subagent.failed` / `subagent.exit` | the child's lifecycle: `Ready` → `Result`/`Failed` → reaped (`node`, `status`/`err`/`outcome`) |
| `subagent.stuck` | `node` — liveness classification (not a deadline) condemned the child |
| `subagent.drain` / `subagent.sigterm` / `subagent.sigkill` / `subagent.teardown` | the bounded kill ladder (`reason`, `live`) |
| `drain.timeout` | `live`, `drain_ms` — the drain budget was exceeded; the ladder is forced |
| `limit.exceeded` | `limit` (`tree_tokens`/…) — a tree budget tripped |
| `scope.trifecta_refused` / `scope.trifecta_grant` | `legs` — the Rule-of-Two refused the grant (exit 2) or `--allow-trifecta` overrode it with a warning (RFC 0012) |
| `cgroup.detected` | `memory_max`, `memory_current`, `memory_high` — cgroup-v2 awareness (best-effort, quiet off-cgroup) |
| `mcp.serving` | `path`, `tools` — the served self-MCP is bound (`--serve-mcp`, RFC 0005) |
| `mcp.spawn` | `handle`, `servers` — a peer delegated a run via served `subagent.spawn` |
| `schedule.fired` · `run.completed`/`run.failed`/`run.killed` | the `loop`/`schedule` driver's per-fire run + outcome |
| `reactive.handled`/`reactive.failed`/`reactive.killed` | one reaction's outcome (reactive mode) |
| `health.armed` | `file` — the health-file heartbeat writer started |

### Agentic loop (`comp:"agent"`; `intel.*` carry `comp:"intel"`)

| Event | Fields beyond canonical |
|---|---|
| `loop.start` | `trigger` (`spawn`/`continue`/`resume`), `step` |
| `loop.step` | `step`, `tokens_in`, `tokens_out` |
| `loop.final` | `step`, `result_status`, `result_bytes` |
| `loop.error` | `err`, `step` |
| `intel.call` | `model`, `tokens_in` (estimated) |
| `intel.result` | `model`, `tokens_in`, `tokens_out`, `finish_reason`, `dur_ms` |
| `tool.call` | `tool`, `id`, (`args` only with content capture on) |
| `tool.result` | `tool`, `is_error`, `bytes` (`content` only with content capture on) |
| `self.schedule` | `after_s`, `queued` — the agent scheduled a future self-wake-up (RFC 0008) |
| `self.subscribe` | `action` (`subscribe`/`unsubscribe`), `uri` — the agent changed its own subscriptions |

`comp:"mcp"` is used for transport-level lines folded from MCP
`notifications/message`; it reuses these event names (e.g. `mcp.disconnect`) and
introduces **no** new `event` strings.

> **Emission notes (vocabulary vs wire).** A graceful shutdown is
> `proc.exit{reason:"drain"}` (there is no separate `proc.shutdown`); the
> restart-governor breaker tripping is `proc.exit{reason:"restart_breaker"}`; the
> child kill path is the `subagent.drain → sigterm → sigkill` ladder above (no
> generic `subagent.signal`/`subagent.restart`). The reactive self-tools emit
> the canonical `trigger.armed`/`trigger.fired` with `kind:"self_schedule"` /
> `kind:"self_subscribe"`. Build-gated surfaces also emit `metrics.*` /
> `cron.unavailable` / `mcp.serve_unavailable` when a flag needs a feature.

---

## Tree correlation

This is the whole trick: lineage is encoded *in the values*, so collectors
rebuild the tree by string prefix and never run a join.

- **`run_id`** → "all telemetry for this unit of work." One ULID, constant from
  the root supervisor down through every nested subagent.
- **`agent_path`** → "this subtree." `0` is the root; `0.2` is its third child;
  `0.2.1` is that child's second child. Querying a subtree is a prefix match:

  ```sh
  # everything under subagent 0.2 (including its descendants)
  grep '"agent_path":"0.2' telemetry.ndjson | jq -c '{ts,comp,event}'
  ```

- **`pid`** → joins the log tree to the OS tree. `subagent.spawn` logs the
  child's `pid`, so the NDJSON tree and `pstree` are joinable; `subagent.stuck`
  can cite OS process state (`proc_state`: `D` / `Z` / running) next to
  `last_event_age_ms`.

Lineage is handed down once at spawn, exactly like environment inheritance. The
supervisor includes a `telemetry` block in the spawn payload (alongside
instruction / scope / limits); the child builds its own correlation context from
it in early `main`, before any side effect, so **every line it emits is
pre-correlated**:

```json
{
  "telemetry": {
    "run_id":         "01J8XAMPLE...",
    "trace_id":       "4bf92f3577b34da6a3ce929d0e0e4736",
    "parent_span_id": "00f067aa0ba902b7",
    "agent_path":     "0.2",
    "agent_id":       "01J8...child",
    "log_level":      "info",
    "log_content":    false
  }
}
```

**Depth and path are minted by the supervisor, never trusted from the child:**
`agent_path = parent_path + "." + child_index`. No registry, no service
discovery, no join-key negotiation.

### Getting telemetry off-box — two wirings

- **(A) default — each process writes its own stderr.** The container
  runtime/collector captures it; agentd does no aggregation and never becomes a
  logging bottleneck. Cleanest for Kubernetes. Reassemble by `run_id` +
  `agent_path` prefix.
- **(B) `--aggregate-logs` (roadmap)** — child telemetry is framed up the
  existing control channel and the supervisor re-emits it on its own stderr, for
  single-stream environments (deeply nested local runs where only the root's
  stderr is captured). The supervisor **forwards, never rewrites** the
  correlation fields. Consumers sort by `ts` + `span_id`, never by arrival order
  (forwarded lines can arrive out of order).

> The correlation scheme above is identical for sync and async spawns.

---

## Content capture (off by default)

The default logs **hashes and lengths only** — never raw content:

- `instruction_hash`, `args_hash`, `result_bytes`, `tokens_in` / `tokens_out`.
- `*_hash` is the first 8 hex chars of a fast non-cryptographic digest — a
  stable correlation aid, **not** a security primitive.

`--log-content` (env `AGENTD_LOG_CONTENT`) opts in to capturing
prompt / tool-arg / result bodies. It is loud, gated, and redaction-aware. It is
a debug/non-prod switch.

**Secrets never appear, capture on or off.** A field allowlist governs what is
serialized; values resolved through the secrets path (the intelligence token,
MCP-server env secrets) are structurally excluded and credential-typed values
`Debug`-print as `***`. Note the honest limit: a secret a model passes as a
free-form tool argument is not guaranteed to be redacted under `--log-content`,
which is exactly why it is non-prod.

---

## W3C trace-context propagation (on by default)

Propagation is a few JSON/header fields, so it is free and **on by default**.
Span *export* is heavy and gated behind the `otel` feature — see
[Metrics & traces](#metrics--traces). With export off, your logs still carry
`trace_id` / `span_id`, so you can correlate them to any upstream trace with no
backend.

**Ingest (mint-or-adopt):**

- If an inbound `traceparent` arrives — on an inbound MCP request to agentd's
  self-MCP server, or via the **`AGENTD_TRACEPARENT`** env var when an
  orchestrator starts the pod — adopt its `trace_id` and use its `span_id` as the
  root `parent_span_id`.
- Otherwise **mint one `trace_id` per `run_id`** (16 random bytes) so the run is
  self-correlated. A malformed inbound header is ignored and we mint instead — a
  bad trace header never fails a run.

`traceparent` is parsed per W3C: `00-<32hex trace_id>-<16hex span_id>-<2hex flags>`.

**Propagate outward (all in the default build):**

- **MCP calls:** `_meta.traceparent` (+ `tracestate` / `baggage` when present)
  on every outbound `tools/call` and `resources/*`, so downstream MCP servers'
  spans line up.
- **LLM call:** the standard `traceparent` HTTP header on the intelligence
  request.
- **Subagents:** the spawn `telemetry` block carries `{trace_id, parent_span_id}`
  so the child continues the same trace.

---

## Health (mode-aware)

A reactive agent is *supposed* to be idle, so **liveness is measured at the
supervisor's event loop, not at the agent**.

| Mode | Readiness | Liveness | Terminal health |
|---|---|---|---|
| `once` | implicit (the run *is* the readiness) | n/a — bounded | **exit code** is the entire signal |
| `loop` / `schedule` | config parsed, MCP connected, first tick armed → `proc.ready` | heartbeat advances each tick | exit code |
| `reactive` | MCP connected **and** all declared subscriptions reconciled (subscribed + read-after-subscribe) → `proc.ready` | supervisor heartbeat; **idle is healthy** | exit code |

**Liveness = the supervisor heartbeat.** The reactor bumps a monotonic
`last_loop_tick` on *every* wake, including idle timeout expiries. If
`now - last_loop_tick` exceeds a threshold, the *supervisor* is wedged → fail
liveness → let the orchestrator restart the pod. **A stuck subagent must NOT
flip liveness** — the supervisor detects and kills it (emitting `subagent.stuck`)
while the pod stays live; failing liveness on a stuck child would destroy the
whole healthy tree.

**Readiness = `proc.ready` reached and subscriptions reconciled.** Before that
the pod is not "ready", so an orchestrator won't route work to it.

### The health surface — a minimal ladder

1. **Exit code (always, free).** Primary for one-shot, final for daemons. The
   stable table (owned by RFC 0011):

   | Code | Meaning | Scheduler hint |
   |---|---|---|
   | 0 | success (one-shot completed / clean SIGTERM drain) | Complete |
   | 1 | generic / unspecified failure | retriable |
   | 2 | config / usage error (validation) | non-retriable |
   | 3 | partial result | policy |
   | 4 | intelligence unreachable / auth after retries | retriable |
   | 5 | semantic — task cannot be done / refused | non-retriable |
   | 6 | required MCP server failed to connect / handshake / died | retriable |
   | 7 | budget exceeded (steps / tokens / deadline / tree) | policy |
   | 124 | hard wall-clock deadline (mnemonic to `timeout(1)`) | — |
   | 137 | killed by SIGKILL (128+9, OS-set) — often OOM | raise memory |
   | 143 | killed by SIGTERM (128+15, OS-set) — ungraceful | — |

   A clean SIGTERM drain returns **0, not 143**. 137/143 are set by the OS when
   the kernel kills us; agentd never exits those itself.

2. **`--health-file PATH` (default daemon surface).** The supervisor writes the
   file every heartbeat — **no socket, no port** — via an atomic
   write-temp-then-`rename`:

   ```json
   {"status":"ready","ts":"2026-06-25T10:00:00.123Z","hb":4821,
    "last_loop_tick_ms":34,"active_subagents":2,"run_id":"01J8XAMPLE..."}
   ```

   `status` is `ready` | `draining`. A Kubernetes `exec` probe reads it and
   checks `status` plus `ts` freshness. One dependency-free file write per tick:

   ```yaml
   livenessProbe:
     exec:
       command: ["sh","-c","test $(( $(date +%s) - $(date -d \"$(jq -r .ts /run/agentd/health)\" +%s) )) -lt 15"]
     periodSeconds: 5
   ```

3. **Unix-socket health line (opt-in, roadmap).** When `--serve-mcp unix:…` is
   already on, expose a trivial `health` / `ready` line on a sibling unix
   socket — reuses existing socket machinery, no new TCP surface.

4. **HTTP `/healthz` + `/readyz` (opt-in, `--features metrics`).** When an orchestrator
   wants real HTTP probes, served on `--metrics-addr` by the same hand-rolled blocking HTTP code on
   one thread — no new dependency. `/healthz` = liveness (heartbeat fresh → 200,
   stale → 503); `/readyz` = readiness (ready + subs reconciled → 200, else
   503). Side-effect-free.

**Default = exit code + `--health-file`.** The health file is off for a one-shot
run — a pure CLI invocation carries zero health machinery. HTTP and socket
surfaces are opt-in and never on for a one-shot.

> `--health-file`, `--log-level` (plus `AGENTD_LOG_LEVEL`), `--log-content`,
> `--serve-mcp`, and `--metrics-addr` (behind `metrics`) are all live; see
> [`config.rs`](../crates/agentd/src/config.rs) for the authoritative flag/env
> list. `--aggregate-logs` and `--health-http` remain roadmap items tracked in
> [`docs/design/PLAN.md`](design/PLAN.md).

---

## Metrics & traces

### Default: derive metrics from logs

The event vocabulary is closed and well-keyed, so every counter is a
`count by (event)` over the NDJSON stream, and gauges are recoverable from
`subagent.spawn` / `subagent.exit` deltas. **No in-process registry, zero
dependencies** — for a minimal unit of work this is genuinely enough, and it is
the default.

```sh
# tool calls by server, ok vs error
jq -r 'select(.event=="tool.result") | "\(.server)\t\(.ok)"' telemetry.ndjson \
  | sort | uniq -c

# token total for the run
jq '[ select(.event=="intel.result") | .tokens_out ] | add' telemetry.ndjson
```

The metrics that matter (derivable from logs by default; emitted directly under
the features below):

- **Gauges:** `agentd_active_subagents`, `agentd_tree_depth`,
  `agentd_subscriptions_active`, `agentd_warm_sessions`, `agentd_ready` (0/1),
  `agentd_up`.
- **Counters:** `agentd_loop_steps_total`, `agentd_intel_calls_total{model}`,
  `agentd_tokens_total{model,type=in|out}`,
  `agentd_tool_calls_total{server,tool,ok}`,
  `agentd_resource_events_total{server}`, `agentd_triggers_total{kind,route}`,
  `agentd_subagents_spawned_total`, `agentd_subagents_exited_total{status}`,
  `agentd_subagent_restarts_total{reason}`,
  `agentd_subagent_stuck_kills_total{signal}` (the reliability headline),
  `agentd_limit_exceeded_total{limit}`,
  `agentd_mcp_connect_failures_total{server}`.

**Cardinality discipline (binding):** **never** put `run_id`, `agent_id`,
`agent_path`, `call_id`, or resource URIs into metric labels — they are unbounded
and live in logs/traces only. Labels use bounded values only: `server`, `tool`,
`model`, `kind`, `route`, `status`, `limit`, `signal`, `reason`, `type`.

### `metrics` feature — Prometheus text (`--features metrics`)

A tiny in-process table of atomic counters/gauges feeds a hand-written
**Prometheus 0.0.4 text exposition** (`# HELP` / `# TYPE` + `name{labels} value`)
served on the already-opt-in surface (`/metrics`). **No `prometheus` or `metrics`
crate** — it is plain text, no async, no SDK.

### `otel` feature — OTLP export + GenAI semconv (`--features otel`)

The `otel` feature exports spans without adding dependencies — hand-rolled
OTLP-over-HTTP/JSON over the existing HTTP client + `serde_json` + the run's
trace ids (no `opentelemetry`/`tracing` crates, no protobuf). It POSTs one batch
per finished run to `OTEL_EXPORTER_OTLP_ENDPOINT`, mapping the event taxonomy
onto the OTel GenAI semantic conventions:

| agentd event/span | `gen_ai.operation.name` | Key attributes |
|---|---|---|
| `subagent.spawn` → `loop.final` | `invoke_agent` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id` |
| `intel.call` / `intel.result` | `chat` | `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons` |
| `tool.call` / `tool.result` | `execute_tool` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `mcp.method.name`, `server.address` |

agentd instruments the **client side** of each tool call and *propagates*
context so the MCP server's spans nest underneath — one span tree, no duplicate
spans. Export is OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, pushed to a local
collector / sidecar so agentd stays thin (no batching/retry sophistication).

**Token-accounting honesty:** tokens come from the intelligence response
`usage`. When absent, agentd logs `0` / `null` — never a guess — so
`agentd_tokens_total` stays trustworthy.

---

## Non-goals

- **No `tracing` in the default build** — only inside the `otel` gate.
- **No metrics client library, ever** — Prometheus text is hand-written; OTLP
  metrics ride `otel`.
- **No span export in the default build** — propagation is on, export is gated.
- **No MCP `logging` capability** — agentd does not implement or advertise it
  (the spec deprecates it in favour of stderr + OpenTelemetry).
- **No log file management / rotation / shipping in-binary** — stderr only; the
  container runtime / collector owns capture and rotation.
- **HTTP `/healthz` / `/readyz` / `/metrics` are opt-in**, never on for a
  one-shot CLI run.

See [RFC 0010](../rfcs/0010-observability-health-telemetry.md) for the full
specification and rationale.
