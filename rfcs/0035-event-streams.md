# RFC 0035: Event streams — agentd as an event-driven agent

**Status:** Draft (for discussion)
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
   (unifying, but changes signal's fire-and-forget cost model)?
2. Consumer groups (N instances sharing one offset) — or is that RFC 0019's
   claim/lease shard story wearing a new hat? (Lean: it is 0019's.)
3. Is `correlate` a start node only, or also a mid-graph collector? (Lean:
   start only in B; mid-graph is `wait {on: event}` composition.)
4. Retention `compact: by-subject` (keep-latest-per-subject — the
   `subscribe` semantics reproduced inside a stream) — worth it, or does
   `subscribe` already own that shape?
