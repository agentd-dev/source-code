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
3. **Distinguish "healthy and idle" from "hung."** A reactive agentd subscribed
   to MCP resources idles for hours by design, so **health is never inferred
   from traffic** — it is measured at the supervisor's own event loop.

The default build ships exactly two things: a hand-rolled JSON-lines logger to
stderr (no `tracing`, no metrics SDK, no OTLP) and a tiny health surface (exit
code + an optional `--health-file`). Everything heavier is feature-gated. Full
rationale is in [RFC 0010](../rfcs/0010-observability-health-telemetry.md).

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
       --intelligence https://gw.example/v1 \
       --mcp fs=https://mcp-fs.internal/mcp \
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
the core set; build-gated surfaces add a few more, noted inline.

### Supervisor / lifecycle (`comp:"supervisor"`)

| Event | Fields beyond canonical |
|---|---|
| `proc.start` | `mode`, `pid`, `version`, `argv_hash` |
| `proc.ready` | readiness reached (see [Health](#health-shape-aware)) |
| `proc.shutdown` | `signal`, `reason` |
| `proc.exit` | `code`, `uptime_ms` |
| `config.loaded` | `mcp_servers` (count/names), `mode`, limits — no secrets |
| `mcp.connect` | `server`, `transport`, `tools` (count), `resources` (count) |
| `mcp.connect.fail` | `server`, `transport`, `err` |
| `mcp.disconnect` | `server`, `reason` |
| `trigger.armed` | `kind` (`once`/`loop`/`schedule`/`subscribe`/`signal`/`event`/`a2a`/`manual`), detail |
| `trigger.fired` | `kind`, `resource_uri?`, `route` (`spawn`/`continue`) |
| `subscribe` | `resource_uri`, `server`, `by` (`config`/`agent`) |
| `unsubscribe` | `resource_uri`, `server`, `by` |
| `resource.updated` | `resource_uri`, `server` — the reactive "heartbeat of meaning" |
| `subagent.spawn` | `node`, `depth` (the child re-exec'd) |
| `subagent.ready` / `subagent.result` / `subagent.failed` / `subagent.exit` | the child's lifecycle: `Ready` → `Result`/`Failed` → reaped (`node`, `status`/`err`/`outcome`) |
| `subagent.stuck` | `node` — liveness classification (not a deadline) condemned the child |
| `subagent.drain` / `subagent.sigterm` / `subagent.sigkill` / `subagent.teardown` | the bounded kill ladder (`reason`, `live`) |
| `drain.start` / `drain.done` / `drain.abandon` | `live`, `drain_ms` — the SIGTERM drain began, completed, or exceeded its budget (the ladder is forced) |
| `limit.exceeded` | `limit` (`tree_tokens`/…) — a tree budget tripped |
| `scope.trifecta_refused` / `scope.trifecta_grant` | `legs` — the Rule-of-Two refused the grant (exit 2) or `--allow-trifecta` overrode it with a warning (RFC 0012) |
| `cgroup.armed` | `memory_max`, `memory_current`, `memory_high` — cgroup-v2 awareness (best-effort, quiet off-cgroup) |
| `a2a.connect` / `a2a.send` / `a2a.delegate` | `peer`/`principal`/`method` — a peer connected, an A2A message/command was served, or a peer delegated a run (RFC 0029, `--features a2a`) |
| `a2a.drain` / `a2a.lameduck` / `a2a.pause` / `a2a.resume` / `a2a.cancel` / `a2a.denied` | an operator admin-command outcome, or an authorization refusal |
| `run.start` · `run.done` / `run.deadline` / `run.refused` / `run.stalled` / `run.dropped` | a workflow run's start + its terminal outcome |
| `workflow.finished` / `workflow.failed` · `workflow.run` / `define` / `loaded` / `deleted` | workflow lifecycle |
| `health.json` | `file` — the health-file heartbeat writer started |

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
| `self.schedule` | `after_s`, `queued` — the agentd scheduled a future self-wake-up (RFC 0008) |
| `self.subscribe` | `action` (`subscribe`/`unsubscribe`), `uri` — the agentd changed its own subscriptions |

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

### Operability: the listener, hot reload, intelligence swap

These events come from the operability surfaces (the A2A listener, hot reload,
the intelligence hot-swap). They are emitted only by the builds that serve them
(`a2a` / `hot-reload` / `config-watch`). The operator/control-plane framing for
each lives in [`docs/operations.md`](operations.md).

| Event | `comp` | Fields beyond canonical |
|---|---|---|
| `a2a.listen` | `supervisor` | `authority`, `bound`, `tls`, `mtls`, `require_auth`, `interface`, `pairing` — the listener bound |
| `a2a.connect` | `supervisor` | `origin`, `conn` — a peer opened a connection (level `debug`) |
| `a2a.denied` | `supervisor` | `principal`, `method`, `op` — an authorization refusal |
| `drain.start` / `drain.done` / `drain.abandon` | `supervisor` | the `a2a.drain` admin method and SIGTERM share this path (see the lifecycle table above) |
| `agent.paused` / `agent.resumed` | `supervisor` | `reason` — an instance-wide `a2a.pause` hold went on or came off |
| `run.paused` / `run.resumed` | `supervisor` | `run`, `reason` — a single run was held or released |
| `config.reloaded` | `supervisor` | `trigger` (`sighup`/`watch`), `changed` (the reloadable group labels; a reload with no material change reports `["nothing"]`) — a reload was **applied** |
| `config.reload.invalid` | `supervisor` | `trigger`, `error` — the candidate did not validate; a clean no-op |
| `config.reload.restart_required` | `supervisor` | `trigger`, `paths` — the diff touched a restart-only path; a clean no-op |
| `config.watch.armed` / `config.watch.fired` / `config.watch.error` | `supervisor` | `file`/`err` — the `lifecycle.watch_config` inotify watcher armed, fired on a ConfigMap swap, or hit an I/O error |
| `intel.swap` | `intel` | `kind` (`model`/`endpoint`), `model_from`, `model_to`, `endpoint_change`, `policy` — a hot-swap was applied at a turn boundary (no URL, no secret) |
| `intel.swap.reject` | `intel` | a parked swap was refused at the turn boundary |

> The admin methods themselves are recorded in the **audit stream**, not as
> separate log events: an `audit` line carries `action:"a2a.drain"` (or
> `"a2a.SendMessage:workflow.run"` for a command DataPart) with the principal,
> role and outcome. See [operations §6](operations.md).

> The intelligence-swap line carries the model *names* (non-secret identifiers),
> the swap kind, and whether the endpoint list changed — **never** the endpoint
> URL or credential. Endpoint identity is transport+index only, carried by the
> `intel.health` / `intel.swap` telemetry, never inline.

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

- If an inbound `traceparent` arrives — on an inbound A2A request to agentd's
  listener, or via the **`AGENT_TRACEPARENT`** env var when an orchestrator starts
  the pod — adopt its `trace_id` and use its `span_id` as the root
  `parent_span_id`.
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

## Health (shape-aware)

An event-driven agentd is *supposed* to be idle, so **liveness is measured at the
supervisor's event loop, not at the agent**. What readiness means depends on the
workflow's start node.

| Start node | Readiness | Liveness | Terminal health |
|---|---|---|---|
| `once` / `manual` | implicit (the run *is* the readiness) | n/a — bounded | **exit code** is the entire signal |
| `loop` / `schedule` | config parsed, MCP connected, first tick armed → `proc.ready` | heartbeat advances each tick | exit code |
| `subscribe` / `signal` / `event` / `a2a` | MCP connected **and** every declared subscription reconciled (subscribed + read-after-subscribe) → `proc.ready` | supervisor heartbeat; **idle is healthy** | exit code |

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
   | 124 | supervisor hard-kill backstop — child unresponsive past the deadline (mnemonic to `timeout(1)`; a self-detected deadline is 7) | — |
   | 137 | killed by SIGKILL (128+9, OS-set) — often OOM | raise memory |
   | 143 | killed by SIGTERM (128+15, OS-set) — ungraceful | — |

   A clean SIGTERM drain returns **0, not 143**. 137/143 are set by the OS when
   the kernel kills us; agentd never exits those itself.

2. **`--health-file PATH` (default daemon surface).** The supervisor writes the
   file once a second — **no socket, no port** — via an atomic
   write-temp-then-`rename`:

   ```json
   {"ts":"2026-06-25T10:00:00.123Z","run_id":"01J8XAMPLE...","mode":"2.0",
    "supervisor_tick_age_ms":34,"alive":true,"draining":false}
   ```

   `alive` is the heartbeat verdict: the supervisor's last loop tick is fresher
   than the liveness window (10s) and no drain is under way. Once a drain begins
   the writer emits one final record with `draining:true` and stops. A Kubernetes
   `exec` probe reads `alive` (or checks `ts` freshness itself). One
   dependency-free file write per second:

   ```yaml
   livenessProbe:
     exec:
       command: ["sh","-c","test $(( $(date +%s) - $(date -d \"$(jq -r .ts /run/agent/health)\" +%s) )) -lt 15"]
     periodSeconds: 5
   ```

3. **A2A readiness (when `a2a.listen` is on).** An authenticated principal reads
   the instance status view via the A2A `status` command (and `ListTasks` for the
   live task/run projection) over the HTTPS listener to learn liveness +
   readiness — no separate health socket.

4. **HTTP `/healthz` + `/readyz` (opt-in, `--features metrics`).** When an orchestrator
   wants real HTTP probes, served on `--metrics-addr` by the same hand-rolled blocking HTTP code on
   one thread — no new dependency. `/healthz` = liveness (heartbeat fresh → 200,
   stale → 503); `/readyz` = readiness (ready + subs reconciled → 200, else
   503). Side-effect-free.

**Default = exit code + `--health-file`.** The health file is off for a one-shot
run — a pure CLI invocation carries zero health machinery. HTTP and socket
surfaces are opt-in and never on for a one-shot.

> `--health-file`, `--log-level` (plus `AGENT_LOG_LEVEL`), `--log-content`,
> `--listen` (the A2A listener), and `--metrics-addr` (behind `metrics`) are the
> observability flags; see [`config/v2/`](../crates/agentd/src/config/v2/) for the
> authoritative flag/env list. `--aggregate-logs` and `--health-http` remain
> roadmap items tracked in
> [`docs/design/01-durable-agent-plan.md`](design/01-durable-agent-plan.md).

---

## Live state reads (the A2A surface)

A control plane reads live state over **A2A** (`a2a.listen`, RFC 0029), on the
same HTTPS listener that carries everything else. Every read resolves to a
**principal** and is authorized against the role matrix — an anonymous caller is
refused — so there is no unauthenticated status port. These reads answer "what is
this instance doing *right now*"; the **metrics** and **OTEL** signals below
answer the time-series and alerting questions.

| Read | How | Who | Body |
|---|---|---|---|
| instance status | `SendMessage` with a `status` command DataPart | any non-anonymous role | instance id, run id, uptime, `draining` / `paused`, the durable store (kind, degraded, generation), armed workflows, live runs, conversations, subagents, OS children, timers, inbox backlog, token budget, tool/skill counts, lifetime counters, instruction source+version, active model, recent activity |
| effective config | `SendMessage` with a `config` command DataPart | operator | the merged settings document — `{{secret:…}}` **references** only, never resolved values |
| one task | `GetTask` | the task's owner (operator sees all) | the durable task: state, history, artifacts |
| all tasks | `ListTasks` | as above | the task/run projection |
| task updates | `SubscribeToTask` (SSE) | as above | status-update frames until terminal |
| identity + skills | `GetAgentCard` | public (pre-auth discovery) | name, description, protocol version, capabilities, the workflows offered as skills |
| the live event feed | `SubscribeToEvents` (SSE) | any non-anonymous role, needs `interface.enabled` | the observation feed: runs, conversations, subagents, tasks, messages, activity, lifecycle and audit — principal-scoped |
| the log ring | `debug.events` command DataPart | operator, needs `interface.debug` | a cursor window of the JSON log lines — see below |

> The redaction discipline is the same as the capabilities manifest and the
> intel-swap log line: the `status` and `config` reads carry structural names,
> transport schemes and header *names* only — never a token, an endpoint URL, or
> a resolved `{{secret:…}}` value.

### `debug.events` — the live log ring

With `interface.debug` on, the same JSON log lines are mirrored into a bounded
in-memory ring you can tail over A2A — the operator live-tail, without a
collector round-trip. Its capacity is `observability.events_ring` (flag
`--events-ring`). A read drains a bounded window with a sequence cursor and
reports the window bounds plus a **`dropped`** count, so a reader knows when the
lossy-by-design ring outran it:

```jsonc
// SendMessage part: {"data":{"agentd":{"op":"debug.events","after":4821,"level":"warn","prefix":"run."}}}
{ "oldest_seq":4700, "newest_seq":4990, "dropped":0,
  "events":[ /* the RFC 0010 JSON log lines, filtered */ ] }
```

The cursor and filters are command arguments: `after` (advance to the last `seq`
you saw), `limit` (default 200, capped at 500), `level` (exact level match), and
`prefix` (a dotted event prefix). The ring never blocks the loop — a slow reader
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
> exactly those in [`obs/metrics.rs::render`](../crates/agentd/src/obs/metrics.rs)
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
> reflects only its own process (cross-process rollup is a deliberate non-goal);
> the `tree_tokens` leg is the supervisor's own bound and is emitted.

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
(`metrics_schema` = `1.1`, owned by `obs::metrics::METRICS_SCHEMA`). The set is
additive within the major — 1.1 added the `agent_budget_tokens_remaining` gauge
and the `tokens_lifetime` limit value; a rename or removal bumps the major. A
control plane authors scalers/alerts against it. Labels carry
**bounded** values only — out-of-vocabulary values fold into an `other` slot so
the cardinality is structurally bounded (the closed label set is a compile-time
array). The same cardinality discipline as the default story applies: **never**
`run_id` / `agent_id` / `agent_path` / `call_id` / a URI in a label.

#### Operability metrics (control plane)

The A2A/hot-reload surfaces add these to the frozen set:

- **`agent_paused`** *(gauge, 0/1)* — `1` while an `a2a.pause` hold is in effect;
  `0` after `a2a.resume`. **Pause is not readiness** — `agent_ready` ignores it
  (it tracks only drain / lame-duck), so a paused instance can still read
  `agent_ready 1`. Read the `paused` field of the A2A `status` command for the
  authoritative answer: the gauge is rendered but the v2 runtime does not yet
  write it, so it reads `0` even while a hold is on.
- **`agent_config_reload_total{result}`** *(counter)* — hot reloads by result.
  The label domain is bounded to `applied` | `rejected` | `other`; a refused
  reload (invalid candidate, or a restart-only diff) currently lands in `other`,
  and either way is a clean no-op with the running config unchanged. The precise
  reason is on the `config.reload.invalid` / `config.reload.restart_required`
  log line.
- **`agent_config_generation`** *(gauge)* — the count of successfully-applied
  reloads, monotonic in practice, so a scraper can detect "this instance has
  picked up generation N" against the controller's desired generation. Like
  `agent_paused` it is rendered but not yet written by the v2 runtime; the
  durable manifest's `lifecycle.config_generation` and the `config.reloaded` log
  line are the reliable signals today.
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

agentd instruments the **client side** of each tool call and *propagates*
context so the MCP server's spans nest underneath — one span tree, no duplicate
spans. Export is OTLP/HTTP to `OTEL_EXPORTER_OTLP_ENDPOINT`, pushed to a local
collector / sidecar so agentd stays thin (no batching/retry sophistication).

**Token-accounting honesty:** tokens come from the intelligence response
`usage`. When absent, agentd logs `0` / `null` — never a guess — so
`agent_tokens_total` stays trustworthy.

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
