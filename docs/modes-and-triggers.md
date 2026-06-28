# Modes & triggers

agentd has **one loop**. The "four modes" are not four programs — they are one
supervisor loop with four different **exit predicates**. A `once` run and a
long-lived `reactive` daemon share the same inner agentic loop, the same spawn
chokepoint, the same router; they differ only in *when the supervisor decides to
stop*. This is deliberate (RFC 0008 §2): anything else reproduces the
"the daemon and the job have separate code" footgun.

This page covers the four modes, then deep-dives the signature mode —
**reactive** — and time scheduling.

> **Build status.** The runtime is implemented: config validation, the agentic
> loop, the supervisor + subagent process tree, the MCP client, all four run
> modes, the reactive router, and the self-tools / served self-MCP all ship and
> are covered by tests. The examples below describe live behavior. Where an item
> is deferred past v1 it is marked **(roadmap)**.

---

## The four modes as one loop

A **driver** is a thin supervisor state machine consulted at three points:
on startup, when a root subagent reaches a terminal status, and on every reactor
wake to evaluate the exit predicate. It never owns I/O — the single reactor owns
the merged channel and timers (RFC 0002). The driver only mutates state and says
what to do next.

| Mode | On start | Exit predicate | Deploy shape |
|---|---|---|---|
| `once` | spawn ONE root subagent with the instruction | the first root reaches a terminal status → map status to exit code | Job, CLI |
| `loop` | spawn the first root "shift" | a bound is hit (max steps / wall-clock deadline / tree-wide token ceiling) **or** a drain signal | Job-with-deadline or Deployment |
| `reactive` | arm subscriptions + routes; idle (no root at start) | **never on its own** — only a drain signal or a fatal/limit class | Deployment |
| `schedule` | arm a timer event source; no root at start | per-fire identical to `once`; the daemon form exits only on signal/limit | external CronJob (recommended) or internal `--interval` |

Default mode is `once`. Select with `--mode` (or `AGENT_MODE`):

```
--mode once|loop|reactive|schedule
```

**Invariants shared by all four (binding, RFC 0008 §3.1):**

- The **step / token / deadline cap is non-negotiable.** For `loop`/`reactive`
  the cap is tree-wide and lifetime-scoped (cumulative across all shifts and
  events). When the tree token ceiling is spent, the driver stops spawning,
  drains warm sessions, then the exit predicate fires with the budget exit class.
- **A one-shot root is never restarted.** A crashed `once` root maps `crashed`
  directly to an exit code; there is no restart governor in `once`.
- A driver **never blocks.** The reactor's single wait point is the only place
  the process sleeps.

---

## `once` — Job / CLI

Run the instruction to a terminal status, then exit. No daemon, no socket, no
warm session. Result on stdout, telemetry on stderr (RFC 0010). The exit code
maps the root's terminal status (RFC 0011 §5.2): `completed`→0, `refused`→5,
budget/exhausted (steps / tokens / the run's own `deadline`)→7, … (exit code
`124` is the *supervisor's* hard-kill backstop for a child that won't
self-terminate, not a terminal-status mapping)

```
agentd \
  --instruction "Summarize today's open PRs and post to #eng." \
  --intelligence unix:/run/intelligence.sock \
  --model claude-opus-4-8 \
  --mcp github=mcp-server-github \
  --mcp slack=mcp-server-slack
```

Mode is omitted because `once` is the default. This is the shape an external
scheduler invokes (a k8s `Job`, a CronJob, a CI step). It is 12-factor by
construction: config in env/flags, one process, one task, exit code is the result.

A typical structured-log line (stderr, JSON-lines, RFC 0010):

```json
{"ts":"2026-06-25T14:02:11.481Z","level":"info","event":"run.start","mode":"once","run_id":"0197a3c4f5b2c1a4","model":"claude-opus-4-8"}
{"ts":"2026-06-25T14:02:18.903Z","level":"info","event":"run.terminal","status":"completed","steps":7,"exit_code":0}
```

---

## `loop` — bounded re-entry

Keep working across "shifts" until a bound is hit or a drain signal arrives.
`loop` is for "poll-and-react" or "work-until-done" daemons that still must be
*bounded* — the global cap is the safety net (the $47K-runaway lesson).

`--interval` selects the re-entry cadence:

- `--interval D` with `D > 0` — timer-driven re-entry (polling). The reactor
  wakes every `D` and re-enters; between shifts it parks at near-zero CPU.
- `--interval 0` (or omit `--interval`) — re-enter immediately on completion
  (work-until-done), governed by an exponential idle-backoff so a trivially-
  completing shift cannot spin hot.

```
# Poll a queue every 30s, react, exit on deadline or token budget.
agentd \
  --mode loop \
  --interval 30s \
  --instruction "Drain the review queue: triage each new item." \
  --intelligence unix:/run/intelligence.sock \
  --mcp queue=mcp-server-queue \
  --deadline 4h \
  --max-tokens 2000000
```

Exit predicate (RFC 0008 §3.1.2): a drain signal → `0` (clean) / `143`
(ungraceful); tree token ceiling spent → `7`; the run's own wall-clock
`--deadline` reached → `7` (a budget-class terminal status); restart breaker open
→ `1`. (Exit `124` is reserved for the supervisor's hard-kill backstop, when a
child won't self-terminate.)

> **Fresh vs warm re-entry.** Whether each shift is a fresh subagent or a warm
> continuation of the prior session is a planned `--session fresh|warm` knob
> (RFC 0008 §3.1.2; default fresh for `--interval D>0`, warm for `--interval 0`).
> **(roadmap)** — not yet on the CLI surface in
> [`config.rs`](../crates/agentd/src/config.rs); v1 re-enters fresh.

---

## `reactive` — the signature mode

A `reactive` daemon spawns **no root at start**. It arms MCP resource
subscriptions, parks the reactor in `recv_timeout` at near-zero CPU, and wakes
only on a resource update, a list-changed notification, an internal timer, a
signal, or a subagent control event. It **never exits on its own** — it is a
Deployment, kept alive by the orchestrator. It exits only on a drain signal or a
fatal/limit class (intelligence unreachable after retries → `4`; required MCP
server failed → `6`; tree token ceiling spent → `7`).

`--mode reactive` **requires** at least one `--subscribe` (validated at startup;
a missing subscription is a usage error → exit 2):

```
agentd \
  --mode reactive \
  --subscribe "file:///inbox/*.json" \
  --instruction "When an inbox file appears, validate it and file an issue." \
  --intelligence unix:/run/intelligence.sock \
  --mcp fs=mcp-server-fs --mcp github=mcp-server-github
```

`--subscribe` is repeatable — declare as many resource URIs as you watch:

```
  --subscribe "file:///config/policy.yaml" \
  --subscribe "file:///inbox/*.json"
```

The rest of this section explains what happens between "a resource changed" and
"an agent reacted."

### Notify-then-read

This is the fact that shapes everything. The MCP `notifications/resources/updated`
notification is **payload-less** — it carries only `{uri}` (optionally `title`),
**no diff, no body** (RFC 0004 §3.8, §1.3 #1):

```json
{"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file:///inbox/order-8842.json"}}
```

So the reactive loop is intrinsically **notify-then-read**: the notification only
says *"this URI changed"*; agent (the agent, on its own terms) must follow up
with `resources/read` to learn *what* it now is. Two round-trips, raceable. The
supervisor never reads the changed body itself — it delivers the URI(s) into the
agent, and the agent decides whether to `resource.read`. This keeps large diffs
out of the supervisor and out of the model's context unless the agent asks for
them.

Because the agent acts on **what the resource *is* now** (current state), not on
a diff, redelivery and coalescing are safe — see *delivery semantics* below.

### Routing: exactly-one-owner, first-match

When an `updated{uri}` arrives, exactly one **route** owns it. A route binds a
matcher (exact URI or glob) to a disposition (`spawn` or `continue`) plus
`{debounce, queue_cap, overflow}`. Matching is **first-match in declared order**,
and **no event ever fans out to two routes** (RFC 0008 §3.2.1):

1. exact-URI routes win outright;
2. among glob matches, the longest literal prefix wins;
3. equal specificity → earliest declared.

A non-matching update is logged at `debug` with `route=none`, dropped, and
counted (`unrouted`). Exactly-one-owner is what makes routing auditable and
replayable — the log records the owning route id per event (RFC 0010).

```json
{"ts":"2026-06-25T14:03:02.114Z","level":"info","event":"resource.updated","server":"fs","resource_uri":"file:///inbox/order-8842.json","route":"r0","disposition":"spawn"}
```

The glob matcher is a tiny hand-rolled one over the URI string (`*` within a path
segment, `**` across segments, `?` single char) — no `regex`/`glob` crate.

> The full `--route match=>spawn|continue:sid[,debounce=ms,cap=N,overflow=…]`
> mini-DSL (RFC 0008 §3.8) is still **(roadmap)**. The two shipped per-URI route
> declarations are `--subscribe` (a `spawn` route) and `--continue` (a warm
> `continue` route); a `--subscribe`d URI defaults to a single `spawn` route in
> `reactive`/`schedule`.

### spawn vs continue — a route property, not a per-event guess

Whether an event starts a fresh agent or feeds an existing one is decided **at
the route, deterministically**, never guessed per event (RFC 0008 §3.3).
Determinism is the whole point.

- **`spawn`** — stateless, parallel reaction. Each delivered event starts a
  *fresh* root subagent (depth 0) whose instruction is templated from the event
  (`uri`, `server`, latest `etag`, change kind). Concurrency across siblings is
  bounded by `max_inflight` (default 4) — that is the backpressure knob for spawn
  routes. Siblings are independent: **no cross-sibling ordering.** Use `spawn`
  when each event is independent work (a new inbox file, a new webhook payload).

- **`continue(session)`** — stateful reaction into one *warm* session. The event
  re-enters a specific suspended inner-loop state (its transcript + scope +
  budgets) where it left off. A session is a **single consumer of its own
  queue**: it finishes one wake before the next delivery — strict FIFO, in-order,
  no interleaving. Use `continue` when ordering or accumulated context matters.

> Rule of thumb: **if order matters, it is a `continue` route, not a `spawn`
> route.**

### Debounce + coalesce

Resources are chatty (an editor save fires many updates per second). Per route
(RFC 0008 §3.4):

- **Debounce (default 250 ms).** On a matched event, push it onto the route queue
  and re-arm the debounce timer. Only when the timer expires *with no newer event
  on that route* does the router actually deliver. A burst collapses to one wake.

- **Coalesce (default overflow).** The queue is keyed by URI; a newer event on an
  already-queued URI **replaces** it, keeping the **latest etag** (newest-wins).
  We never need the intermediate states — the agent re-reads current state. For a
  `continue` route, all distinct queued URIs drain into **one** delivery as a set
  ("these N resources changed"), one re-entry, not N. With coalesce the queue
  never grows beyond the number of distinct watched URIs, so backpressure falls
  out for free.

Other overflow policies (`drop_oldest`, `drop_newest`, `block`) exist for routes
modelling discrete work items; every drop increments a visible `dropped_events`
counter. `block` (true source backpressure) is opt-in only. The **ultimate**
backpressure is the tree-wide token budget: when near-spent, the supervisor stops
spawning, drains warm sessions, then quiesces. The process degrades to "not
starting new work," never melts down.

### Delivery semantics: at-least-once + re-read-current-state

agentd promises **convergence on current state, not exactly-once** (RFC 0008
§3.5). Notifications can be redelivered (reconnect, restart, a coalesce edge).
Because the agent `resource.read`s on wake and acts on what the resource *is*
now, reprocessing converges — coalesce is lossless and redelivery is safe.

Ordering guarantees:

- **Within one `continue` session:** strict FIFO, single-consumer, no
  interleaving, no reordering.
- **Across `spawn`-route siblings:** **no** ordering guarantee — independent by
  construction.
- **Across different routes/sessions:** concurrent, unordered. Exactly-one-owner
  means there is no cross-route race on a single event.

**Reconnect recovery.** On MCP server reconnect or supervisor restart, agentd
re-subscribes every *declared* subscription and synthesizes one coalesced
"possibly changed" event per watched URI (read-after-subscribe converts
edge-triggering to level-triggering across the boundary). This recovers any
update missed while disconnected; the re-read-current-state model makes it safe.
Warm sessions and dynamic (self-subscribe) routes are lost on supervisor restart
in v1, recovered by idempotent re-trigger.

### End-to-end: one reactive wake

```
reactor.recv_timeout(min(child_deadlines, route.debounce_timers, clock.next_fire))
 └─ event: ResourceUpdated{ server, uri }            // RFC 0004 reader thread
     ├─ route = first_match(routes, uri)             // exactly-one-owner
     │    └─ None: unrouted++; log(debug, route=none); drop.
     ├─ route.queue.push(uri, etag)                  // newest-wins coalesce
     └─ (re)arm route.debounce_timer = now + debounce
 └─ later wake: debounce_timer expired, no newer event on the route
     └─ deliver:
          spawn:    while inflight<max_inflight { spawn_root_from_event(pop) }
          continue: if session idle { re_enter_session(drain_set) }   // coalesced set
 └─ inner loop (subagent): resource.read(uri) on its own terms, act on current
     state, run to terminal/suspend; supervisor updates budgets & idle-backoff.
```

### Self-subscribe = self-scheduling

This is the capability that closes the loop. When a *running* agentd calls the
`subscribe(uri)` self-tool mid-reasoning (RFC 0005), the supervisor (RFC 0008
§3.6):

1. issues `resources/subscribe` upstream (capability-gated);
2. **auto-creates a `continue(this_session)` route** at the front of the declared
   order (most specific, owned by the caller);
3. returns success. The agentd ends its turn; the session goes **warm**
   (suspended). The next `updated{uri}` re-enters *this* session.

The agentd has just scheduled its own future wake — "wake me when X changes." A
child subagent's completion-as-self-resource is just an `updated` that the
parent's self-subscribe route delivers, which is how async subagents (RFC 0009)
report back. `unsubscribe(uri)` removes the route and subscription.

> **Transport scope.** Reactivity is **stdio-only in v1.** The router is
> transport-agnostic by construction, but only stdio MCP servers deliver
> notifications in v1; reactivity-over-HTTP (an SSE GET stream) is **(roadmap)**
> (RFC 0004 §3.11, RFC 0013). Self-MCP serving is **stdio/unix only** in v1; HTTP
> serving is **(roadmap)**.

---

## `schedule` — time-driven

`schedule` is `loop` with the re-entry cadence supplied by a **clock event
source** instead of completion-driven backoff. Per fire it behaves identically to
`once`: spawn a root, run, map status. Time is "just another event source" — a
clock fire is an internal event delivered to the *same* router, routed to a
`spawn` route (a fresh root per fire). There is no second scheduling subsystem.

### Recommended: external CronJob → `once`

For production, prefer an **external** scheduler (a k8s CronJob, systemd timer,
cron) invoking `agentd --mode once …`. This is robust to clock skew, restarts,
and is 12-factor clean — the schedule lives in the orchestrator, not in the
process. agentd has no calendars, DST handling, timezone job-store, or per-fire
persistence in core (UTC only).

```yaml
# k8s CronJob (the k8s operator is NOT part of this project — illustrative)
apiVersion: batch/v1
kind: CronJob
metadata: { name: nightly-digest }
spec:
  schedule: "0 6 * * *"            # 06:00 UTC daily
  jobTemplate:
    spec:
      template:
        spec:
          restartPolicy: Never
          containers:
            - name: agent
              image: agent:1.x
              args:
                - --mode=once
                - --instruction=Compile the nightly digest and email it.
                - --intelligence=unix:/run/intelligence.sock
                - --mcp=mail=mcp-server-mail
```

### Internal `--interval`

For non-orchestrated deployments, `--mode schedule --interval D` is a standalone
convenience: the timer source fires every `D` and routes a fresh root per fire.
`--mode schedule` **requires** `--interval` (validated at startup; missing → exit
2). UTC only.

```
agentd \
  --mode schedule \
  --interval 15m \
  --instruction "Check the status page; alert on any red." \
  --intelligence unix:/run/intelligence.sock \
  --mcp status=mcp-server-status
```

`--interval` accepts a duration with a unit suffix — `500ms`, `30s`, `15m`, `2h`
— or a bare integer (seconds). The daemon form exits only on a signal or a
limit class.

> **Internal cron.** A 5-field cron expression (`--mode schedule --cron
> "<min hour dom mon dow>"`, UTC, RFC 0008 §3.6) ships behind the `cron` build
> feature. The `--cron` flag is on the CLI surface today
> ([`config.rs`](../crates/agentd/src/config.rs)); build with `--features cron`
> to enable it. For production, prefer an external CronJob → `once`, or
> `--interval`.

---

## CLI / env surface for modes & triggers

These are the flags that actually exist today
([`crates/agentd/src/config.rs`](../crates/agentd/src/config.rs)). Precedence is
built-in default < (config file, later) < env var < flag, validated **before any
side effect** (a bad config exits `2` in milliseconds, RFC 0011).

| Knob | Flag | Env | Default | Notes |
|---|---|---|---|---|
| Mode | `--mode once\|loop\|reactive\|schedule` | `AGENT_MODE` | `once` | the driver |
| Subscribe | `--subscribe <uri>` (repeatable) | — | none | required for `reactive` |
| Continue | `--continue <uri>` (repeatable) | — | none | subscribe an MCP resource, routed to one warm `continue` session |
| Interval | `--interval <dur>` | — | unset | `loop`/`schedule`; required for `schedule` |
| Instruction | `--instruction <TEXT>` / `--instruction-file <PATH>` | `INSTRUCTION` | — | required |
| Intelligence | `--intelligence <URI>` | `AGENT_INTELLIGENCE` | — | `unix:` / `https://` / `vsock:` |
| Model | `--model <NAME>` | `AGENT_MODEL` | — | model id |
| MCP server | `--mcp name=command` (repeatable) | — | none | stdio transport |
| Max steps | `--max-steps <N>` | `AGENT_MAX_STEPS` | 50 | per-run step cap |
| Max tokens | `--max-tokens <N>` | `AGENT_MAX_TOKENS` | 200000 | token budget |
| Deadline | `--deadline <dur>` | `AGENT_DEADLINE` | 600s | wall-clock deadline |
| Max depth | `--max-depth <N>` | — | 4 | subagent tree depth cap |
| Run id | `--run-id <ID>` | `AGENT_RUN_ID` | generated | idempotency key |
| Drain | `--drain-timeout <dur>` | `AGENT_DRAIN_TIMEOUT` | 25s | graceful drain budget |
| Serve MCP | `--serve-mcp <unix:/path>` | `AGENT_SERVE_MCP` | off | stdio/unix only in v1 |
| Health file | `--health-file <PATH>` | — | off | liveness heartbeat |
| Log level | `--log-level <L>` | `AGENT_LOG_LEVEL` | info | trace…error |

**Validation rules that touch modes (config.rs):**

- `--mode reactive` requires at least one `--subscribe <uri>`, else exit 2.
- `--mode schedule` requires `--interval <dur>`, else exit 2.
- An invalid `--mode`/`AGENT_MODE` value is a usage error → exit 2.

Durations accept `ms`/`s`/`m`/`h` suffixes or a bare integer (seconds):
`250ms`, `30s`, `5m`, `2h`, `600`.

See `agentd --help` for the full flag list.

---

## Picking a mode

- **One task, then done** (CI step, k8s Job, a CLI invocation) → `once`.
- **Scheduled task** → external CronJob → `once` (recommended), else `--mode
  schedule --interval`.
- **Poll something / work-until-done with a bound** → `loop` (`--interval D` to
  poll, `--interval 0` to drain-then-done).
- **React to resource changes / be event-driven / let the agent schedule its own
  wakes** → `reactive` with `--subscribe`. This is agentd's signature mode and
  the part of the ecosystem nothing else builds.

---

## References

- RFC 0008 — execution modes & reactive routing (`rfcs/0008-execution-modes-and-reactive-routing.md`)
- RFC 0004 — MCP client subset & codec (`rfcs/0004-mcp-client-subset-and-codec.md`)
- RFC 0005 — self-MCP server & control protocol (self-subscribe self-tools)
- RFC 0007 — agentic loop & terminal status (re-read-current-state)
- RFC 0011 — cloud-native contract (exit codes, config precedence, drain)
- Binding decisions — `docs/design/00-architecture-assessment.md`
- Build progress — `docs/design/PLAN.md`
- The live CLI/env surface — `crates/agentd/src/config.rs`
- [Horizontal scaling](scaling.md) — running the reactive worker as a *fleet*:
  `--shard K/N` partitioning + work-claim leases extend the exactly-one-owner rule
  from intra- to **inter**-instance (RFC 0019).
