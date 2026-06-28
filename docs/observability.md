# Observability

agent is a *process tree*, not a thread pool, and it is reactive — it spends
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
agent --instruction "summarise /data/report.md" \
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
env `AGENT_LOG_LEVEL`). The level filter is a cheap integer compare *before* any
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

### Operability: management, hot reload, intelligence swap

These events come from the operability surfaces (the management transport, hot
reload, the intelligence hot-swap). They are emitted only by the builds that
serve them (`serve-mcp` / `hot-reload` / `config-watch`). The operator/control-
plane framing for each lives in [`docs/operations.md`](operations.md).

| Event | `comp` | Fields beyond canonical |
|---|---|---|
| `mcp.connect` / `mcp.disconnect` | `supervisor` | `origin` (`stdio`/`management`), `conn` — a peer joined/left the served transport |
| `mcp.drain` | `supervisor` | `in_flight`, `eta_ms` — the `drain` operator tool tripped the drain latch |
| `mcp.lame_duck` | `supervisor` | `ready` — the `lame-duck` tool flipped the readiness override |
| `mcp.pause` / `mcp.resume` | `supervisor` | `affected` — the `pause`/`resume` tools suspended/continued N live subtrees |
| `config.reload_requested` | `supervisor` | `trigger` (`sighup`/`watch`) — a reload was requested |
| `config.reloaded` | `supervisor` | `changed` (the reloadable group labels), `applied_ms` — a reload was **applied** (a clean no-op with no material change still reports `changed:[]`) |
| `config.reload_rejected` | `supervisor` | `reason` (`invalid`/`restart_required`), `field`, `diagnostics` — a reload was a clean no-op |
| `config.reload.values` | `supervisor` | `model`, `max_tokens`, `max_steps`, `max_depth`, `log_level` — the value-swap step's new template (no secret) |
| `config.watch.armed` / `config.watch.fired` / `config.watch.error` | `supervisor` | `file`/`err` — the `--watch-config` inotify watcher armed, fired on a ConfigMap swap, or hit an I/O error |
| `intel.swap` | `intel` | `kind` (`model`/`endpoint`), `model_from`, `model_to`, `endpoint_change`, `policy` — a hot-swap was applied at a turn boundary (no URL, no secret) |
| `intel.swap.reject` | `intel` | a parked swap was refused at the turn boundary |

> The intelligence-swap line carries the model *names* (non-secret identifiers),
> the swap kind, and whether the endpoint list changed — **never** the endpoint
> URL or credential. Endpoint identity is transport+index only, surfaced by the
> `agent://intelligence` resource (§The served control resources), never inline.

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
  runtime/collector captures it; agent does no aggregation and never becomes a
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

`--log-content` (env `AGENT_LOG_CONTENT`) opts in to capturing
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

- If an inbound `traceparent` arrives — on an inbound MCP request to agent's
  self-MCP server, or via the **`AGENT_TRACEPARENT`** env var when an
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
   the kernel kills us; agent never exits those itself.

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
       command: ["sh","-c","test $(( $(date +%s) - $(date -d \"$(jq -r .ts /run/agent/health)\" +%s) )) -lt 15"]
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

> `--health-file`, `--log-level` (plus `AGENT_LOG_LEVEL`), `--log-content`,
> `--serve-mcp`, and `--metrics-addr` (behind `metrics`) are all live; see
> [`config.rs`](../crates/agent/src/config.rs) for the authoritative flag/env
> list. `--aggregate-logs` and `--health-http` remain roadmap items tracked in
> [`docs/design/PLAN.md`](design/PLAN.md).

---

## The served control resources

When the management transport is on (`--serve-mcp`, [`operations.md`](operations.md)),
a control plane reads live state as `agent://` MCP resources rather than scraping
metrics — each is a structured JSON read, most are **subscribable** (notify-then-
read: a payload-free `notifications/resources/updated`, then a `resources/read`).
Most are **Management-only** (a Stdio peer 404s on them). These complement the
metrics above: metrics are for time-series/alerting, resources for point-in-time
operator reads and event-driven control.

| Resource | Origin | Subscribable | Body |
|---|---|---|---|
| `agent://status` | any | no | run id, mode, version, pid, uptime, spawn counts |
| `agent://capabilities` | any | no | the live capabilities manifest (identity, `surfaces{}`, limits) |
| `agent://run/{id}` | any | yes (each spawn / terminal change) | the served run aggregate; folds in the run-outcome report once terminal |
| `agent://subagent/{handle}` | any | yes (terminal only) | an async child's status / distilled result |
| `agent://session/{handle}` | any | yes (each warm-turn boundary) | a warm session's turn state |
| `agent://inventory` | Management | yes (spawn / exit / status change) | the live subagent-tree projection: lifecycle flags (`draining`/`paused`/`ready`), totals, per-node status/usage |
| `agent://intelligence` | Management | yes (breaker / active / all-down transitions) | endpoint health: the ordered endpoint list (transport + index, **never** the URL/creds), which is active, each one's breaker state / EWMA latency / error rate, the all-down flag, swap policy, discovery |
| `agent://config/effective` | Management | yes (each applied hot reload) | the live, **redacted** reloadable-config view (no token/URL/secret) |
| `agent://capacity` | Management *(cluster build)* | no | the placement view: identity, shard `K/N`, standby, free slots, active subagents, intelligence warmth, saturation |
| `agent://events` | Management *(`events` feature)* | yes (each new event) | the bounded live-event ring — see below |

> The redaction discipline is the same as the capabilities manifest and the
> intel-swap log line: `agent://intelligence` and `agent://config/effective`
> carry transport schemes, structural names, and header *names* only — never a
> token, an endpoint URL, or a resolved `{{secret:…}}` value.

### `agent://events` — the live log ring

With the `events` feature (and a management transport to serve it on), the same
JSON log lines are mirrored into a bounded in-memory ring you can tail over MCP —
the operator live-tail, without a collector round-trip. A read drains a bounded
window with the standard MCP cursor; the envelope (`events_schema` = `1.0`)
reports the window bounds and a **`dropped`** count so a subscriber knows when the
lossy-by-design ring outran it:

```jsonc
// resources/read agent://events?after=4821&level=warn&event=subagent.,limit.
{ "events_schema":"1.0", "oldest_seq":4700, "newest_seq":4990, "dropped":0,
  "events":[ /* the RFC 0010 JSON log lines, filtered */ ] }
```

The cursor + filters ride the query string: `?after=<seq>` (advance to the last
`seq` you saw; a malformed value safely falls back to the whole window),
`?level=<lvl>` (exact level match), `?event=<prefix,prefix>` (a comma-list of
dotted event prefixes). The ring never blocks the supervisor — a slow reader
loses old lines (reflected in `dropped`), never stalls the daemon.

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

- **Gauges:** `agent_active_subagents`, `agent_tree_depth`,
  `agent_tree_breadth`, `agent_subscriptions_active`, `agent_ready` (0/1),
  `agent_up`.
- **Counters:** `agent_loop_steps_total`, `agent_intel_calls_total`,
  `agent_tokens_total{type=in|out}`, `agent_reactions_total`,
  `agent_subagents_spawned_total`, `agent_subagents_exited_total{status}`,
  `agent_subagent_restarts_total{reason}`,
  `agent_subagent_stuck_kills_total{signal}` (the reliability headline),
  `agent_limit_exceeded_total{limit}`,
  `agent_mcp_connect_failures_total{server}`.

> **What the `metrics` build actually renders.** The list above is what an
> agentctl dashboard counts; under `--features metrics` the **emitted** series are
> exactly those in [`obs/metrics.rs::render`](../crates/agent/src/obs/metrics.rs)
> and the frozen RFC 0016 §4.3 set below. Three §4.3 names are **reserved**, not
> emitted in this build (rendered as a `# HELP`/`# TYPE` marker with no sample, the
> same honest-absence shape as `agent_mcp_up`):
> `agent_tool_calls_total{server,tool,ok}` (the tool-call boundary runs in the
> child loop, so a supervisor scrape can't reflect it — derive from `tool.result`
> log lines), and the three latency **histograms** `agent_run_duration_ms`,
> `agent_intel_call_duration_ms`, `agent_tool_call_duration_ms` (no histogram
> exposition machinery in this build — use the `dur_ms` log field). The frozen
> `model` label on `agent_tokens_total` / `agent_intel_calls_total` is likewise
> **deferred**: the call sites carry no model identifier, so the label is reserved
> and intentionally absent (never faked) — per-model splits come from
> `intel.result.usage` log lines. `agent_loop_steps_total`, `agent_refusals_total`,
> and the steps/tokens/deadline/depth legs of `agent_limit_exceeded_total` are
> **process-local** — emitted in the re-exec'd child loop, so the supervisor scrape
> reflects only its own process (cross-process rollup is a v1 non-goal); the
> `tree_tokens` leg is the supervisor's own bound and is live.

**Cardinality discipline (binding):** **never** put `run_id`, `agent_id`,
`agent_path`, `call_id`, or resource URIs into metric labels — they are unbounded
and live in logs/traces only. Labels use bounded values only: `server`, `tool`,
`kind`, `route`, `status`, `limit`, `signal`, `reason`, `type` (the `model` label
is reserved by RFC 0016 §4.3 but not yet emitted — see the note above).

### `metrics` feature — Prometheus text (`--features metrics`)

A tiny in-process table of atomic counters/gauges feeds a hand-written
**Prometheus 0.0.4 text exposition** (`# HELP` / `# TYPE` + `name{labels} value`)
served on the already-opt-in surface (`/metrics`). **No `prometheus` or `metrics`
crate** — it is plain text, no async, no SDK.

The metric **names** and label **keys** are a **frozen, versioned contract**
(`metrics_schema` = `1.0`, surfaced at `surfaces.metrics_schema` in the
capabilities manifest). The set is additive within the major; a rename/removal
bumps the major. A control plane authors scalers/alerts against it. Labels carry
**bounded** values only — out-of-vocabulary values fold into an `other` slot so
the cardinality is structurally bounded (the closed label set is a compile-time
array). The same cardinality discipline as the default story applies: **never**
`run_id` / `agent_id` / `agent_path` / `call_id` / a URI in a label.

#### Operability metrics (control plane)

The management/hot-reload surfaces add these to the frozen set:

- **`agent_paused`** *(gauge, 0/1)* — `1` while the `pause` operator tool has
  frozen the agentic tree at turn boundaries; `0` after `resume`. **Pause is not
  readiness** — `agent_ready` ignores it (it tracks only drain / lame-duck), so
  a paused instance can still read `agent_ready 1`.
- **`agent_config_reload_total{result}`** *(counter)* — hot reloads by result.
  The closed domain is `applied` | `rejected` | `other`. A `rejected` reload is a
  clean no-op (the running config is unchanged).
- **`agent_config_generation`** *(gauge)* — the count of successfully-applied
  reloads, monotonic in practice. A scraper detects "this instance has picked up
  generation N" against the controller's desired generation.
- **`agent_drains_total{phase}`** *(counter)* — drain phase transitions; the
  closed domain is `started` | `completed` | `forced` | `other` (so `completed`
  vs `forced` distinguishes a clean drain from one that overran its budget).
- **`agent_runs_total{status}`** *(counter)* — runs by the RFC 0007 §3.4
  terminal-status vocabulary (`completed`, `refused`, `exhausted_steps`,
  `exhausted_tokens`, `deadline`, `stalled`, `loop_detected`, `cancelled`,
  `crashed`, `other`).
- **`agent_refusals_total{reason}`** *(counter; **process-local**)* — guard trips
  by reason (`trifecta` | `rate` | `budget` | `depth` | `mcp` | `other`). Refusals
  trip in the re-exec'd child loop, so this reflects only the scraped process — the
  headline safety signal is the refusal / `scope.trifecta_refused` log line.
- **`agent_intel_up`** *(gauge, 0/1)* and **`agent_intel_errors_total{reason}`**
  *(counter; `unreachable`|`auth`|`timeout`|`5xx`|`other`)* — intelligence-endpoint
  reachability + error breakdown.
- **`agent_intel_all_down`** *(gauge, 0/1)* — `1` while **every** model endpoint
  is down (the latched last-child-experience truth that also flips `/readyz`
  NotReady, RFC 0018 §6); distinct from `agent_intel_up` (the active endpoint's
  reachability).
- **`agent_restarts_total`**, **`agent_reactor_stalls_total`** *(counters;
  **reserved** in `metrics_schema 1.0`)* — supervisor process restarts observed
  (rebuild+reconcile), and wedged-reactor liveness trips. Both are rendered but
  **not emitted** in this build: there is no in-process rebuild+reconcile path for
  the former (a pod restart is a fresh zeroed process the orchestrator counts), and
  a wedged reactor surfaces as a `/healthz` 503 (a per-scrape heartbeat-age read),
  not a one-shot in-process event, for the latter.
- **`agent_tree_breadth`** *(gauge)* — current max siblings at any tree node
  (alongside the existing `agent_active_subagents` / `agent_tree_depth`).
- **`agent_memory_max_bytes`** / **`agent_memory_current_bytes`** *(gauges)* —
  cgroup-v2 `memory.max` / `memory.current`, emitted only for the fields the
  kernel exposes (absent off-cgroup, keeping `/metrics` clean).

#### Reactive-backlog gauges (the scaling-signal set)

Point-in-time gauges a horizontal scaler reads:

- **`agent_pending_events`** — reactive events received but not yet routed.
- **`agent_inflight_reactions`** — reactions currently executing.
- **`agent_subscriptions_active`** — reconciled declared subscriptions.
- **`agent_reaction_lag_ms`** — age of the oldest un-routed pending event.
- **`agent_saturation`** *(float gauge in `[0,1]`)* — in-flight / capacity
  utilization — the HPA "utilization" target. A zero capacity reads `0.0` and an
  over-cap in-flight clamps to `1.0` (never a div-by-zero, never `> 1`).

#### Horizontal-scaling counters (`cluster` build)

Wired by the `cluster` build's shard gate and claim gate:

- **`agent_shard_skipped_total`** *(counter)* — items dropped as out-of-shard by
  the routing pre-filter.
- **`agent_claims_lost_total`** *(counter)* — work claims lost to another replica
  — the over-provision signal (high & rising under low backlog ⇒ scale down).
- **`agent_claims_granted_total`** / **`agent_claims_released_total`**
  *(counters)* — claims this replica won, and held claims handed back on a non-
  terminal wind-down or drain.

(The legacy bare series — `agent_runs_started_total`, `agent_tokens_input_total`,
`agent_reactions_total`, etc. — are retained alongside the frozen set, additive
within the major. `agent_mcp_up{server}` is **not** emitted in this build — only
the connect-failure counter is.)

### `otel` feature — OTLP export + GenAI semconv (`--features otel`)

The `otel` feature exports spans without adding dependencies — hand-rolled
OTLP-over-HTTP/JSON over the existing HTTP client + `serde_json` + the run's
trace ids (no `opentelemetry`/`tracing` crates, no protobuf). It POSTs one batch
per finished run to `OTEL_EXPORTER_OTLP_ENDPOINT`, mapping the event taxonomy
onto the OTel GenAI semantic conventions:

| agent event/span | `gen_ai.operation.name` | Key attributes |
|---|---|---|
| `subagent.spawn` → `loop.final` | `invoke_agent` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.conversation.id` |
| `intel.call` / `intel.result` | `chat` | `gen_ai.request.model`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.response.finish_reasons` |
| `tool.call` / `tool.result` | `execute_tool` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `mcp.method.name`, `server.address` |

agent instruments the **client side** of each tool call and *propagates*
context so the MCP server's spans nest underneath — one span tree, no duplicate
spans. Export is OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, pushed to a local
collector / sidecar so agent stays thin (no batching/retry sophistication).

**Token-accounting honesty:** tokens come from the intelligence response
`usage`. When absent, agent logs `0` / `null` — never a guess — so
`agent_tokens_total` stays trustworthy.

---

## Non-goals

- **No `tracing` in the default build** — only inside the `otel` gate.
- **No metrics client library, ever** — Prometheus text is hand-written; OTLP
  metrics ride `otel`.
- **No span export in the default build** — propagation is on, export is gated.
- **No MCP `logging` capability** — agent does not implement or advertise it
  (the spec deprecates it in favour of stderr + OpenTelemetry).
- **No log file management / rotation / shipping in-binary** — stderr only; the
  container runtime / collector owns capture and rotation.
- **HTTP `/healthz` / `/readyz` / `/metrics` are opt-in**, never on for a
  one-shot CLI run.

See [RFC 0010](../rfcs/0010-observability-health-telemetry.md) for the full
specification and rationale.
