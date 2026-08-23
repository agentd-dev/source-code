# RFC 0035: Event streams — agentd as an event-driven agent

**Status:** Phase A implemented (`streams:` config, the `emit` step's stream form, the `stream` start with durable offsets and `from: earliest` replay); Phases B–D draft
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-23
**Part of:** the durable runtime (RFC 0025 — a new store kind; RFC 0027 — new nodes); unifies the admission edges of RFC 0027 §5, RFC 0029, RFC 0032 §4.

---

## 1. Summary

agentd already reacts to five kinds of event — an MCP resource update, an
inbound webhook, an A2A message, an internal signal, a runtime lifecycle
event — and already keeps three event-shaped logs (the durable inbox, the
interface feed, the audit stream). What it lacks is the noun: a first-class
**event** on a named, durable, replayable **stream** that workflows can
*publish to* and *consume from* with offsets.

This RFC proposes that noun and the smallest set of verbs around it:

- **`streams:`** — named, durable, append-only sequences in the store, with
  retention, participating in the pressure system;
- **`emit`** — a step that publishes an event (with the same derived
  idempotency key its retries share);
- **`stream`** — a start node that consumes a stream with a **durable
  consumer offset**, per event or in batches, so events that arrive while
  the daemon is down are *processed after restart* rather than missed;
- **`correlate`** — a start node that fires when a *set* of events sharing a
  correlation key has arrived within a window (the multi-event join);
- **bindings** — webhooks and A2A messages *into* streams, streams *out* to
  webhooks/A2A push, and external brokers (Kafka, NATS, MQTT) bridged
  through MCP servers, keeping the dependency moat intact.

The design rule is the house rule: **sugar over existing pipelines, never a
parallel mechanism**. Streams live in the store adapters that exist;
consumption rides the start-node machinery that exists; delivery is
at-least-once with the idempotency keys that exist; shedding rides the
pressure system that exists.

## 2. Motivation — the audit of what exists

| Machinery | What it is | What it lacks as an event system |
|---|---|---|
| `subscribe` start | MCP resource updated → notify-then-read, debounce/coalesce, `window` | **latest-value semantics** — a value stream, not an event stream; intermediate states are deliberately collapsed |
| `webhook` start | HTTP in → one run, HMAC, dedup, rate | fires a run *now* or drops; nothing buffers for later, no replay after downtime |
| `signal` start / `workflow.signal` | named internal event with payload | **fire-and-forget** — a signal nobody is armed for is lost; no history, no offset |
| `event` start | runtime lifecycle (`workflow.finished`, `lifecycle.shutdown`, …) | closed vocabulary; not for domain events |
| durable inbox | at-least-once admission log; replayed at restore | engine-internal: one consumer (the reactor), no user streams, no query |
| interface feed (RFC 0032) | SSE deltas to display clients | ephemeral, interface-scoped, not consumable by workflows |
| audit stream | append-only who-did-what | write-only by design |

Every row is an event system *fragment*. The costs of not unifying them are
concrete: a webhook that arrives during a restart is gone (the durable inbox
only holds what was admitted); a signal emitted while the interested
workflow is being replaced is lost; "run when order.paid *and*
order.shipped for the same id" is expressible only as hand-rolled
`memory.*` bookkeeping; and integrating a real broker means teaching every
workflow the broker's shape instead of teaching the broker agentd's.

## 3. The model

**Event** — the CloudEvents-compatible minimum:

```
{ id, stream, subject, type, ts, source, correlation?, data }
```

`id` is a ULID (time-ordered, unique); `subject` is the routing string
filters match on (`order.paid`, `sensor.imu`); `correlation` groups events
belonging to one logical flow (an order id, a run id); `data` is the
payload. Mapping to/from CloudEvents 1.0 is mechanical and specified, so
bridged brokers speak a known dialect.

**Stream** — a named, durable, append-only sequence:

```yaml
streams:
  orders:
    retention: { max_events: 100000, max_age: 7d }   # whichever trims first
  telemetry:
    retention: { max_age: 1h }
```

Streams are store entities (a new `Kind::Event`, keyed
`<stream>/<seq>` with a per-stream monotonic sequence in the manifest), so
they inherit the adapter story: `file` on a laptop or robot, `mcp`/`http`
for a fleet. Retention is enforced at append; stream disk usage is inside
`store.file.min_free`'s pressure math, and an appender under `Shed` is
refused like every other admission.

## 4. The verbs

### 4.1 `emit` — publish

```yaml
paid: { kind: emit, depends_on: [charge],
        stream: orders, subject: order.paid,
        correlation: "{{inputs.order_id}}",
        data: { amount: "{{steps.charge.output.json.amount}}" } }
```

At-least-once with the existing discipline: the event id is the step's
derived idempotency key (`sha256(run_id.step_id)`), so a replayed `emit`
after a crash appends the *same* id and consumers dedup by id — the
`mcp.tool` `_meta` story, applied to ourselves.

### 4.2 `stream` — consume, with an offset

```yaml
- name: fulfil
  steps:
    take: { kind: stream, stream: orders, subject: "order.paid",
            from: new }              # new | earliest | a stored offset
    ship: { kind: agent, depends_on: [take], instruction: "arrange shipping" }
    done: { kind: finish, depends_on: [ship] }
```

One run per event (or `batch: {size, window}` for one run per batch — the
`subscribe window` idea, generalized). The consumer's **offset is durable**,
keyed by `(workflow, node)` like start-state: a daemon that was down for an
hour processes the hour's events on restart, in order, exactly as the
restart contract already treats the inbox. `concurrency`/`on_overflow` and
`priority` apply unchanged; a consumer that cannot keep up is *visible*
(offset lag becomes a metric beside `agent_pressure_level`) instead of
silently lossy.

This is the property no current edge has: **`subscribe` collapses history,
`webhook` and `signal` drop it; `stream` keeps it and hands it over on the
consumer's schedule.**

### 4.3 `correlate` — the multi-event join

`depends_on: [a, b, c]` already joins multiple *steps*; `correlate` is the
same idea across *events*:

```yaml
- name: reconcile
  steps:
    both: { kind: correlate, stream: orders,
            on: ["order.paid", "order.shipped"],
            by: correlation, window: 24h,
            on_timeout: fire_partial }    # or: discard
    fix:  { kind: agent, depends_on: [both],
            instruction: "reconcile {{steps.both.output.events}}" }
    done: { kind: finish, depends_on: [fix] }
```

Fires one run when every subject in `on` has arrived sharing one
correlation value inside the window; `on_timeout` decides whether a partial
set fires (for escalation flows: "paid but not shipped in 24h" *is* the
event) or is discarded. State is durable start-state — a restart resumes
half-collected joins. This is deliberately CEP-lite: sets and windows, not a
query language; anything cleverer belongs in a `think`/CEL step downstream.

### 4.4 `wait {on: event}` — mid-run

The existing `wait` gains the same power inline: suspend this run until an
event matching `{stream, subject, correlation}` arrives — the durable
counterpart of `wait {on: signal}`, surviving restarts because the wait
record and the stream both do.

## 5. Bindings — the edges become event sources and sinks

- **Webhook → stream**: a `webhook` node gains `into: {stream, subject}` —
  the request is *appended* (200 on the append) instead of firing a run
  directly. Verification, dedup and `rate` apply before the append. This
  single change gives webhooks replay-after-downtime.
- **A2A → stream**: likewise for messages/commands (`into:`), so a fleet
  peer can feed a stream over mTLS — or a co-located one over the
  unix-socket lane.
- **Stream → outward**: an `emit` with `forward: {webhook: URL}` or
  `forward: {peer: name}` pushes the event out as it appends (at-least-once,
  the A2A push machinery); the durable copy is the source of truth, the
  push is the notification.
- **Brokers via MCP** (the moat-preserving move): an external Kafka/NATS/
  MQTT topic arrives as an MCP server exposing `publish` tools and
  subscribable resources; a small bridge profile in this RFC's appendix
  specifies the resource naming so `stream`-style consumption maps onto it.
  agentd links no broker client — exactly as databases arrive as MCP today.
- **The feed and audit become well-known streams** (`_feed`, `_audit`,
  read-only) — one consumption model for "what is the agent doing", usable
  by workflows themselves (an agent that reacts to its own audit trail is a
  self-monitoring agent).

## 5.5 The runtime's own events — the `_runtime` stream

The deepest integration is the runtime narrating **itself**. agentd already
observes everything below — most of it already has a telemetry line — but a
log line is for humans and collectors; an event on a stream is for
*workflows*. A read-only, opt-in `_runtime` stream turns the daemon's own
life into something its workflows can react to, which is what makes
initialization, REinitialization, degradation and self-healing expressible
as ordinary consumers.

One naming rule: **the subjects ARE the telemetry vocabulary** (RFC 0016's
closed set, extended). No second naming scheme — if the log line says
`pressure.shed`, the event's subject is `pressure.shed`, and an operator who
knows one knows both.

The taxonomy, by subsystem — with each event's *rate class*, because an
event system that does not budget its own volume becomes its own pressure
source (`rare` = per-boot/per-incident; `bounded` = per config/operator
action; `flow` = per unit of work; `hot` = excluded from `_runtime` by
default, opt-in per subject):

| Subject family | Events | Rate | The consumer it exists for |
|---|---|---|---|
| **process** | `proc.start`, `restore.done` (generation, lost entities), `generation.fresh`, `lifecycle.drain.start` / `.done`, `lifecycle.shutdown` | rare | init/deinit workflows; a fleet agent noticing a member rebooted |
| **config** | `config.reloaded` (changed sections), `config.rejected`, `config.restart_required`, `instruction.changed`, `workflows.changed`, `store.config_changed` | bounded | **reinitialization**: re-register the webhook when the reload changed its URL; re-brief a warm subagent when the instruction changed |
| **definitions** | `workflow.loaded`, `.replaced`, `.retiring`, `.unloaded`, `workflow.pin_restored`, `workflow.locked` (a mutation attempt against the immutable lock — a security signal worth an alert, not just a log line) | bounded | GitOps reconcilers; security monitors |
| **pressure & resources** | `pressure.warn` / `.shed` / `.cleared` (cause: disk\|memory, free bytes), `budget.threshold`, `budget.exhausted`, `store.degraded`, `stream.trimmed` (retention dropped unconsumed events — data loss is an event, never silent) | rare→bounded | degradation workflows: shed low-priority behaviors, flush caches, notify the fleet, page a human |
| **capabilities** | `mcp.connected` / `.disconnected` (a server lost = a capability lost), `tools.changed`, `subscription.lost`, `intel.endpoint.down` / `.up`, `intel.all_down`, `intel.swapped` (model failover) | bounded | a robot re-homing when its motor-driver server drops; a workflow pausing itself while the model is unreachable |
| **runs & turns** | `run.started` / `.finished` / `.failed` / `.cancelled` / `.refused`, `run.stalled`, `start.shed`, `schedule.missed` | flow | today's `event` start, generalized — with history and offsets |
| **resilience** | `breaker.open` / `.reopen` / `.closed`, `step.retry_exhausted` | bounded | dependency-outage playbooks: switch provider, open an incident |
| **edges (inbound)** | `webhook.received` / `.rejected` (metadata: path, principal, verdict — bodies ride the `into:` binding, not `_runtime`), `a2a.message`, `a2a.denied` (auth/role/uid refusals), `pairing.granted` / `.revoked` | flow | rate anomaly detection; security audit consumers; "a rejected webhook burst" as a wake-up |
| **children** | `subagent.spawned` / `.finished` / `.killed`, `child.unhealthy`, `restarts.tripped` (the governor's circuit breaker) | flow | supervisors-of-supervisors; a parent agent reacting to a child's death |
| **human** | `human.asked`, `.answered`, `.timeout`, `approval.granted` / `.denied` | bounded | escalation chains; the unanswered-approvals digest |
| **time** | `tick.minute` / `tick.hour` / `tick.day` (coarse clock subjects), `schedule.fired` (which cron/every, for which workflow), `timer.fired` | bounded (coarse ticks) / flow | consumers that want time as *an event among events* — a `correlate` joining `order.paid` with `tick.hour` is a timeout expressed in the same algebra as everything else; sub-minute ticks are deliberately absent (that is what `schedule` and `sleep` are for) |
| **transport** | `listener.bound` / `.lost` (a2a http(s), **unix socket**, webhook), `peer.unreachable` / `.recovered`; on a vsock-profile build (RFC 0014), the same subjects for vsock lanes | rare | init workflows that publish "I am reachable at…" only after `listener.bound`; fleet health |
| **context & memory** | `context.compacted` (what was summarized away), `memory.written` (subject = the key: `memory.written/deploy-state`), `skills.changed` | flow / **hot** for busy keys | an agent that re-grounds after compaction; workflows reacting to a specific memory key changing — the `memory:<key>` display idea, as an event |

Rules that keep this honest:

- **Opt-in, subject-filtered.** `streams: {_runtime: {subjects: ["pressure.*", "config.*", "capabilities.*"]}}` — nothing is appended for subjects nobody enabled, so the default daemon writes exactly what it writes today. `hot` families require naming the subject explicitly.
- **Metadata, not payloads.** `webhook.received` carries the envelope, never the body; `memory.written` carries the key, not the value. The data plane stays where it is; `_runtime` is the control plane's narration. Redaction rules (RFC 0012) apply unchanged.
- **The feedback rule.** Every `_runtime` event carries `source` (the run/workflow that caused it), and an event start or stream consumer **never fires on events its own workflow caused** — the self-trigger suppression the `event` start now enforces (a watcher on `workflow.finished` looping on its own completions was a real bug, found and fixed while drafting this section). Deeper cycles (A watches B, B watches A) remain the author's responsibility; `concurrency.max_runs` and pressure are the backstops.
- **`event` starts become sugar.** `event {on: X}` ≡ a `_runtime` consumer with `from: new` and no history — the existing node keeps working verbatim, and gains `from: earliest` replay for free where the subject is enabled.

## 6. Use cases

1. **Order saga** — services `emit` domain events; `correlate` joins
   paid+shipped by order id; the 24h `on_timeout: fire_partial` run IS the
   escalation. Replay-safe end to end via emit-idempotency + consumer
   offsets.
2. **Robot telemetry** (the PID 1 story) — drivers publish sensor events to
   a 1h-retention stream; a `stream` consumer with `batch: {window: 5s}`
   feeds the perception agent; the flight-recorder property comes free
   because the stream *is* durable store state.
3. **GitHub → triage with no lost webhooks** — `webhook into: {stream:
   gh}`; the triage consumer processes the backlog after a deploy instead
   of missing pushes; `priority: low` on the consumer sheds chatter first
   under pressure.
4. **Fleet coordination** — workers `emit forward: {peer: coordinator}`;
   the coordinator's `correlate` waits for all shards of a job (`by:
   correlation`, the job id) before merging — the map-reduce join with no
   broker deployed.
5. **Self-observation** — a workflow consuming `_feed` that emits an alert
   when step failure rate in a window crosses a bound: the ops sidecar as
   ten lines of YAML.
6. **Human escalation chains** — `human.timeout` today fires one event;
   with streams, unanswered approvals accumulate in a stream a daily digest
   consumer drains.
7. **Reinitialization** — the init/deinit story (RFC 0027 lifecycle idioms)
   completed: a consumer of `config.reloaded` + `instruction.changed`
   re-registers external state that depends on config (webhook URLs, peer
   adverts) *without a restart*; `listener.bound` gates the first
   advertisement so it never races the socket.
8. **Degradation playbooks** — `pressure.warn` starts a cleanup workflow;
   `pressure.shed` notifies the fleet and disables `priority: low` behavior
   at the source; `intel.all_down` switches the robot to its model-free
   reflex workflows until `intel.endpoint.up`.
9. **Security reflexes** — `workflow.locked` (someone — or someTHING —
   tried to rewrite immutable definitions) and `a2a.denied` bursts feed a
   consumer that raises a human gate. The audit stream records; the event
   stream *reacts*.

## 7. Non-goals and honest limits

- **Not a broker.** Single-writer append, store-backed, modest throughput
  (thousands/sec on `file`, adapter-bound otherwise). Fan-out to many
  external consumers, partitions, exactly-once — that is Kafka's job,
  bridged via MCP, not reimplemented.
- **At-least-once, not exactly-once** — consistent with the whole engine;
  idempotent consumers via event id, as everywhere else.
- **Ordering** is per-stream (single writer), not global.
- **No query language.** Subjects match by glob/CEL like every other
  filter; history is a `stream.read` internal tool with offset+limit, not
  SQL.
- **Streams are per instance** like the store itself; a shared `mcp` store
  shares them, with the coordination caveats RFC 0019 already owns.

## 8. Interaction with existing contracts

- **Pressure** (v2.5): appends are admissions — they shed. Retention keeps
  the disk math honest; `agent_stream_lag{stream}` joins metrics schema.
- **Priority** (v2.5): consumers declare it; low-priority consumers shed
  first, their offsets simply hold.
- **Retirement** (v2.6): a retired consumer's offset survives; a
  re-arriving workflow of the same name resumes where it left off — the
  unload story needs no new rules.
- **Directives** (RFC 0034): `:::workflow` bodies use these nodes like any
  other; a `streams:` directive is a plausible future registry entry.
- **Idempotency/breaker/rate** (v2.5–2.6): `emit` is a remote-effect-shaped
  step and takes all three fields with identical semantics.

## 9. Phasing

- **A (core):** `streams:` config + `Kind::Event` + `emit` + `stream` start
  with durable offsets + retention + pressure. The system is useful here.
- **B (join):** `correlate`, `wait {on: event}`, batching windows.
- **C (edges):** webhook/A2A `into:`, `forward:` egress, `_feed`/`_audit`
  as streams.
- **D (bridge):** the MCP broker profile + a reference NATS bridge server.

## 10. Open questions

1. Should `signal` become sugar for a well-known `_signals` stream
   (unifying, but changes signal's fire-and-forget cost model)? — §5.5
   settles the analogous question for `event` starts (they become `_runtime`
   sugar); `signal` likely follows once streams exist.
2. Consumer groups (N instances sharing one offset) — or is that RFC 0019's
   claim/lease shard story wearing a new hat? (Lean: it is 0019's.)
3. Is `correlate` a start node only, or also a mid-graph collector? (Lean:
   start only in B; mid-graph is `wait {on: event}` composition.)
4. Retention `compact: by-subject` (keep-latest-per-subject — the
   `subscribe` semantics reproduced inside a stream) — worth it, or does
   `subscribe` already own that shape?
