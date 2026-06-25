# RFC 0008: Execution modes, triggers & reactive routing

**Status:** Draft
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

agentd must serve four operational shapes — a one-shot CLI/Job run, a long-lived
polling daemon, an event-reactive daemon, and a time-scheduled run — without
forking the codebase into divergent daemons. The architecture-decision document
(§2.6) is explicit and binding: **one supervisor loop, one inner agentic loop,
and a small set of drivers that differ *only* by an exit predicate.** Anything
else reproduces the "the daemon and the job have separate code" footgun and
destroys the cloud-native simplification.

The second half of this RFC is the part the ecosystem has *not* built and which
the assessment names agentd's edge (§1.1, §2.6): **reactive routing.** MCP
supports `resources/subscribe` + `notifications/resources/updated`, but the
notification is **payload-less** — it carries only `{uri}` (optionally `title`),
no diff (§1.3.1, RFC 0004). So the reactive loop is intrinsically
*notify-then-read*, two round-trips, racy, and must be debounced and coalesced.
When an event arrives, *which* agent owns it, whether it spawns a fresh agent or
re-enters a warm session, how bursts collapse, how backpressure behaves, and what
ordering guarantees hold — all of that was RFC 0001 §14.5's open question. This
RFC resolves it as a precise, deterministic, replayable rule.

This RFC owns: the four drivers as exit predicates over the single loop; the
reactive router (subscriptions, routes, exactly-one-owner matching, spawn-vs-
continue, debounce/coalesce, bounded queues, ordering, self-subscribe, reconnect
recovery); and internal interval/cron as event sources feeding the *same* router.

It does **not** own: the inner agentic loop and its terminal-status state machine
(RFC 0007); the supervisor reactor / `mpsc` / timer plumbing (RFC 0002); MCP
client subscribe/notify wire mechanics and capability-gating (RFC 0004); the
self-MCP `subscribe`/`unsubscribe`/`resource.read` self-tools and the control
channel (RFC 0005); subagent spawn payload, scope, caps (RFC 0009); kill ladder,
restart governor, dead/stuck detection (RFC 0003). This RFC consumes those and
specifies the layer above them.

Modules owned here (per assessment §4.0): `triggers/mode.rs` (the drivers),
`triggers/router.rs` (reactive routing), `triggers/timer.rs` (interval + cron
event source).

---

## 2. Decision

1. **One supervisor loop + one inner loop + four drivers differing only by EXIT
   PREDICATE.** The drivers are `once`, `loop`, `reactive`, `schedule`. They are
   not four loops; they are four things the supervisor decides about *when to
   stop*. Re-entry and starting are shared mechanism (the router + spawn).

2. **Reactive routing is exactly-one-owner first-match.** A subscription is
   `(server, resource_uri)`. A route binds a `match` (exact URI or glob) to a
   `disposition` (`spawn` per-event | `continue(session_id)`) plus
   `{debounce_ms=250, queue_cap, overflow}`. Every inbound `updated{uri}` matches
   **exactly one** route, by first-match in declared order (exact beats glob;
   among globs, longest-prefix first). No fan-out. No match → log + drop +
   counter.

3. **spawn-vs-continue is a deterministic route property**, never a per-event
   guess. `spawn` = fresh root subagent templated from the event, bounded by
   route `max_inflight=4`. `continue` = deliver into one warm session as a
   single-consumer FIFO, strict in-order.

4. **Self-subscribe = self-scheduling.** When a running agent calls the
   `subscribe` self-tool (RFC 0005), the supervisor auto-creates a
   `continue(this_session)` route. The agent has scheduled its own future wake.
   This is the signature capability.

5. **Debounce + newest-wins coalesce by default.** A burst on one URI collapses
   to one delivery carrying the latest etag; multi-URI changes to one continue-
   session deliver as a set. The agent re-reads current state, so coalescing is
   lossless for state-changed semantics.

6. **At-least-once, idempotent via re-read-current-state.** We promise
   convergence on current state, not exactly-once. On reconnect we synthesize one
   coalesced "possibly changed" event per watched URI to recover missed updates.

7. **Time is just another event source.** External CronJob → `once` is the
   **recommended** production path. Internal `--interval D` (`D=0` = re-enter
   immediately) and an optional `cron`-feature 5-field expression are standalone
   conveniences, implemented as internal time events fed into the *same* router.
   UTC only; no calendar/DST/job-store in core.

---

## 3. Mechanisms

### 3.1 The four drivers as exit predicates

There is one supervisor reactor (RFC 0002): a single thread that `recv_timeout`s
a merged `mpsc`, owns timers, and drives the per-child state machines. There is
one inner agentic loop (RFC 0007), which lives only inside subagent processes.
A **driver** is a thin supervisor state machine that decides, per mode, (a) what
to do on startup, (b) what to do when a root subagent reaches a terminal status,
and (c) the *exit predicate* — the condition under which the supervisor process
itself terminates.

```rust
/// triggers/mode.rs
#[derive(Clone, Copy, Debug)]
pub enum Mode { Once, Loop, Reactive, Schedule }

/// The driver is consulted by the reactor at three points. It never owns I/O;
/// the reactor owns the merged mpsc + timers (RFC 0002).
pub trait Driver {
    /// Called once, after MCP connect + reconcile, before the first wait.
    fn on_start(&mut self, sup: &mut Supervisor);

    /// Called when a *root* subagent (depth 0) reaches a terminal status.
    /// Returns what the supervisor should do next.
    fn on_root_terminal(
        &mut self,
        sup: &mut Supervisor,
        handle: RootHandle,
        status: TerminalStatus, // RFC 0007
    ) -> RootDisposition;

    /// The exit predicate, evaluated every reactor wake. Some(code) => exit now.
    fn exit_predicate(&self, sup: &Supervisor) -> Option<ExitCode>;
}

pub enum RootDisposition {
    /// stop touching this root (once); the exit predicate handles process exit.
    Done,
    /// re-enter after an idle-backoff sleep (loop); fresh or warm per config.
    ReEnterAfter(Duration),
    /// nothing to do now; the router/timer will re-enter on the next event.
    Quiesce,
    /// crash path: hand to the restart governor (RFC 0003).
    Restart,
}
```

The exit predicates, verbatim from the assessment table (§2.6):

| Mode | `on_start` | Exit predicate (`exit_predicate`) | Deploy shape |
|---|---|---|---|
| `once` | spawn ONE root subagent with the instruction | the first root subagent reaches a terminal status → map status to exit code (RFC 0011 / §3.7) | Job, CLI |
| `loop` | spawn the first root "shift" | a bound is hit (max iterations / global wall-clock deadline / tree-wide token ceiling) **or** a drain signal | Job-with-deadline or Deployment |
| `reactive` | arm subscriptions + routes; idle | **never on its own** — only a drain signal or a fatal/limit class (RFC 0011 exit codes 4/6/7) | Deployment |
| `schedule` | (no root at start; arm a timer event source) | per-fire identical to `once`; the *daemon* form exits only on signal/limit | external CronJob (recommended) or internal interval/cron |

**Shared invariants across all four (binding):**

- The **global step/token/deadline cap is non-negotiable** (the $47K-runaway
  lesson, §2.6). For `loop`/`reactive` the cap is *tree-wide and lifetime-scoped*
  (cumulative across all shifts/events), enforced by the budget tracker (RFC
  0003 `supervisor/budget.rs`). When the tree token ceiling is spent, the driver
  stops spawning, drains warm sessions, then the exit predicate fires with the
  budget exit class.
- **Never restart a one-shot root** (§2.8). `once` has no restart governor; a
  crashed `once` root maps `crashed` → exit code directly.
- A driver **never blocks**. It only mutates supervisor state and returns; the
  reactor's `recv_timeout` is the single wait point.

#### 3.1.1 `once`

```
on_start:        spawn root subagent { instruction, scope, limits, telemetry }
on_root_terminal: RootDisposition::Done
exit_predicate:  Some(map_terminal_to_code(root.status)) once root is terminal
```

Result on stdout, telemetry on stderr (RFC 0010). No daemon, no socket, no warm
session. Exit code maps the root's terminal status (the RFC 0007 §3.4 enum):
`completed`→0, `refused`→5, `exhausted_*`/budget→7 (or →3 with a usable
partial), `deadline`→124, …; the authoritative mapping is RFC 0011 §5.2.

#### 3.1.2 `loop`

```
on_start:        spawn the first root "shift"
on_root_terminal(status):
    completed | stalled        -> ReEnterAfter(idle_backoff.next())   // see 3.1.5
    exhausted_* | deadline      -> if tree budget spent: Quiesce (exit_predicate fires)
                                   else ReEnterAfter(idle_backoff.next())
    crashed                     -> Restart   (RFC 0003 restart governor / breaker)
exit_predicate:
    DRAINING flag set                          -> Some(0 clean | 143 ungraceful)
    || tree token ceiling spent                -> Some(7)
    || global wall-clock deadline reached      -> Some(124)
    || max_restarts exceeded (breaker open)    -> Some(1)   // RFC 0003
```

`--interval D` selects re-entry cadence: `D>0` = timer-driven re-entry (polling);
`D=0` = re-enter immediately on completion (work-until-done). Re-entry is wired
through `triggers/timer.rs` as an internal event (§3.6) so the **same** reactor
path handles it — no separate sleep loop. Whether each shift is a **fresh**
subagent or a **warm** continuation is config (`--session fresh|warm`); default
**fresh** for `--interval D>0`, **warm** for `--interval 0`. Fresh-vs-warm is the
same spawn-vs-continue axis as reactive (§3.3) and shares that code.

#### 3.1.3 `reactive`

```
on_start:        connect + reconcile (3.5); arm declared subscriptions + routes;
                 spawn NO root at start. Idle.
on_root_terminal: (only spawn-route roots reach here) Quiesce  // router owns lifecycle
exit_predicate:
    DRAINING flag set                          -> Some(0 clean | 143)
    || tree token ceiling spent                -> Some(7)   // drain warm sessions first
    || fatal infra (intelligence unreachable/auth after retries) -> Some(4)
    || required MCP server failed              -> Some(6)
```

A `reactive` daemon idles at near-zero CPU (the reactor parks in `recv_timeout`;
RFC 0002) and wakes only on `notifications/resources/updated`,
`notifications/resources/list_changed`, an internal timer event, a signal, or a
subagent control event. It **never exits on its own** — it is a Deployment, kept
alive by the orchestrator.

#### 3.1.4 `schedule`

`schedule` is `loop` with the re-entry cadence supplied by a clock event source
(§3.6) rather than completion-driven backoff. Per fire it behaves identically to
`once` (spawn a root, run, map status). The recommended production path is an
**external** CronJob invoking `agentd --mode once …` (robust to clock skew /
restart / 12-factor). The internal form (`--mode schedule --interval D` or
`--mode schedule --cron "<5-field>"` behind the `cron` feature) is a standalone
convenience for non-orchestrated deployments. There is **no second scheduling
subsystem**: a clock fire is just an internal event delivered to the router,
which routes it to a `spawn` route (a fresh root per fire).

#### 3.1.5 Idle backoff (loop/reactive hygiene)

A healthy idle `loop` (nothing to do, completes trivially) must not spin hot.
The backoff is exponential, reset on real work:

```rust
struct IdleBackoff { base: Duration /*1s*/, cap: Duration /*=--interval or 60s*/, cur: Duration }
impl IdleBackoff {
    fn next(&mut self) -> Duration { let d = self.cur; self.cur = (self.cur*2).min(self.cap); d }
    fn reset(&mut self)            { self.cur = self.base; }
}
```

"Real work" = a root shift that produced new observable state (not `stalled`,
not an immediate trivial `completed`). The router calls `reset()` whenever it
delivers a routed event into a session (a real wake is real work).

### 3.2 Routing vocabulary and data model

```rust
/// triggers/router.rs

/// One concrete thing the supervisor is watching. Templates are NOT subscribable
/// (RFC 0004 §3.2); only concrete URIs. A glob route subscribes per concrete URI
/// discovered via resources/list and re-evaluated on list_changed.
pub struct Subscription { pub server: ServerId, pub uri: String }

#[derive(Clone)]
pub enum Disposition {
    /// fresh root subagent per delivered event, templated from the event.
    Spawn { max_inflight: u16 /*=4*/ },
    /// deliver into one warm session, single-consumer FIFO, strict in-order.
    Continue { session_id: SessionId },
}

#[derive(Clone, Copy)]
pub enum Overflow { Coalesce, DropOldest, DropNewest, Block }

pub struct Route {
    pub id: RouteId,
    pub matcher: Matcher,          // Exact(String) | Glob(GlobPattern)
    pub disposition: Disposition,
    pub debounce: Duration,        // default 250ms
    pub queue_cap: usize,          // default = distinct watched URIs for this route, min 16
    pub overflow: Overflow,        // default Coalesce
    pub queue: RouteQueue,         // bounded, per-route
    pub debounce_timer: Option<Instant>, // armed deadline; the reactor's timer wakes on min()
    pub inflight: u16,             // spawn routes only
}

pub enum Matcher { Exact(String), Glob(String /* e.g. "file:///in/*.json" */) }
```

`RouteId` and declaration order are assigned when routes are parsed (config) or
created (dynamic, §3.4). The router holds `routes: Vec<Route>` in declared order
— **order is the tiebreak substrate** (§3.2.1).

#### 3.2.1 Exactly-one-owner first-match

```rust
/// Returns the single owning route, or None (logged + dropped + counter).
fn first_match<'a>(routes: &'a mut [Route], uri: &str) -> Option<&'a mut Route> {
    // 1. exact-URI routes win outright.
    // 2. among glob matches, longest literal prefix wins.
    // 3. ties broken by declared order (lowest index first).
    let mut best: Option<(usize /*idx*/, i64 /*specificity*/)> = None;
    for (i, r) in routes.iter().enumerate() {
        let spec = match &r.matcher {
            Matcher::Exact(u) if u == uri        => i64::MAX,
            Matcher::Glob(g)  if glob_match(g, uri) => literal_prefix_len(g) as i64,
            _ => continue,
        };
        match best {
            Some((_, bs)) if spec <= bs => {}     // keep earlier/more-specific
            _ => best = Some((i, spec)),
        }
    }
    best.map(|(i, _)| &mut routes[i])
}
```

Specificity order: exact (`i64::MAX`) > longest-literal-prefix glob > shorter
glob; equal specificity → earliest declared. **No event ever fans out to two
routes** — this is what makes routing auditable and replayable (RFC 0010 logs
the owning `route` id per event). A non-matching `updated{uri}` increments the
`unrouted` counter, emits `resource.updated` at `debug` with `route=none`, and is
dropped.

`glob_match` is a tiny hand-rolled matcher (`*` within a path segment, `**` across
segments, `?` single char) over the URI string — no `regex`/`glob` crate (the
minimalism bar, §2.2). `literal_prefix_len` = the length of the matcher up to its
first wildcard meta-char.

### 3.3 spawn-vs-continue — deterministic, per route

This is a **property of the route, decided at declaration time**, never a
per-event guess (§2.6). Determinism is the whole point.

**`Spawn`** — stateless, parallel reaction. Each delivered event starts a *fresh*
root subagent (depth 0) whose instruction is templated from the event:

```rust
struct EventTemplateCtx<'a> {
    uri: &'a str,
    server: &'a str,
    etag: Option<&'a str>,      // latest, from coalesce
    kind: ChangeKind,           // Updated | ListChanged | Synthetic(reconnect) | Timer
}
// instruction = route.template.render(ctx); seed/scope/limits/telemetry from the
// route's spawn template (RFC 0009 payload). Depth minted by supervisor = 0.
```

Concurrency across spawned siblings is bounded by `max_inflight` (default 4) —
**that is the backpressure knob for spawn routes**. While `inflight ==
max_inflight`, further matched events stay queued (subject to overflow, §3.4).
Siblings are independent; **no cross-sibling ordering** (§3.5). When a spawn-route
root reaches terminal status, the supervisor decrements `inflight` and pops the
next queued event if any.

**`Continue(session_id)`** — stateful reaction into one warm session. The event
is delivered into a specific suspended inner-loop state (its transcript + scope +
budgets, held in `supervisor/tree.rs`, RFC 0003) and re-enters that loop where it
left off. A session is a **single consumer of its own queue**: it finishes
processing one wake (runs to a turn-boundary / re-suspend, RFC 0007) before the
next delivery. Strict FIFO, in-order, no interleaving (§3.5).

```rust
fn deliver(router: &mut Router, sup: &mut Supervisor, route: &mut Route) {
    match &route.disposition {
        Disposition::Spawn { max_inflight } => {
            while route.inflight < *max_inflight {
                let Some(ev) = route.queue.pop_coalesced() else { break };
                let h = sup.spawn_root_from_event(&ev, route);   // RFC 0009 chokepoint
                route.inflight += 1; sup.idle_backoff.reset();
                log_event("subagent.spawn", &[("route", route.id), ("resource_uri", ev.uri)]);
            }
        }
        Disposition::Continue { session_id } => {
            if sup.session_is_idle(*session_id) {
                if let Some(ev) = route.queue.pop_coalesced() {
                    sup.re_enter_session(*session_id, ev);        // RFC 0005 deliver
                    sup.idle_backoff.reset();
                }
            } // else: session busy; it pulls on return (sup calls deliver() again)
        }
    }
}
```

### 3.4 Debounce, coalesce, backpressure

Resources are chatty (an editor save fires many updates/sec). Per route:

**Debounce.** On a matched event, push onto the route queue and (re)arm
`debounce_timer = Instant::now() + debounce` (default **250ms**). The reactor
arms its `recv_timeout` to the nearest armed deadline across all routes + child
deadlines (RFC 0002). **Only when the timer expires with no newer event on that
route do we actually `deliver()`.** A burst collapses to one wake.

**Coalesce (default `Overflow::Coalesce`).** The queue is keyed by URI; a new
event on a URI already queued **replaces** it, keeping the **latest etag**
(newest-wins) — we never need intermediate states because the agent re-reads
current state (§3.5, RFC 0007 `resource.read`). For a `Continue` route, all
distinct queued URIs are drained into **one** delivery as a set ("these N
resources changed"), one re-entry, not N:

```rust
struct RouteQueue { by_uri: IndexMap<String, QueuedEvent>, cap: usize }
impl RouteQueue {
    /// newest-wins coalesce; returns whether the queue grew (for backpressure).
    fn push(&mut self, ev: QueuedEvent, overflow: Overflow) -> PushOutcome {
        if let Some(slot) = self.by_uri.get_mut(&ev.uri) { *slot = ev; return PushOutcome::Coalesced; }
        if self.by_uri.len() < self.cap { self.by_uri.insert(ev.uri.clone(), ev); return PushOutcome::Enqueued; }
        match overflow {
            Overflow::Coalesce  => PushOutcome::Coalesced, // already at cap of *distinct* uris
            Overflow::DropOldest => { let k = self.by_uri.keys().next().cloned().unwrap();
                                      self.by_uri.shift_remove(&k); self.by_uri.insert(ev.uri.clone(), ev);
                                      PushOutcome::Dropped(1) }
            Overflow::DropNewest => PushOutcome::Dropped(1),
            Overflow::Block      => PushOutcome::Blocked, // stop reading from server (§3.4 note)
        }
    }
    /// Continue routes drain the whole set; Spawn routes pop one at a time.
    fn pop_coalesced(&mut self) -> Option<QueuedEvent> { /* shift_remove first */ }
    fn drain_set(&mut self) -> Vec<QueuedEvent> { self.by_uri.drain(..).map(|(_,v)| v).collect() }
}
```

`IndexMap` here means an insertion-ordered map; we hand-roll a tiny
`Vec<(String, QueuedEvent)>` + linear scan (route queues are small — bounded by
distinct watched URIs) rather than pull a crate. With `Coalesce`, the queue
**never grows beyond the number of distinct watched URIs** — backpressure falls
out for free.

**Backpressure / overflow (`queue_cap` reached).** Default `Coalesce` (newest-wins,
self-bounding). `DropOldest`/`DropNewest` for routes modelling discrete work items
(rare for resources) — every drop increments a visible `dropped_events` counter
(RFC 0010). `Block` = stop reading notifications from that server until the queue
drains (true source backpressure) — **opt-in only**; it can stall the connection
and is unsafe unless the server buffers. The ultimate backpressure is the
tree-wide token budget: when near-spent, the supervisor stops spawning and only
drains warm sessions, then quiesces (§3.1). The process degrades to "not starting
new work," never melts down.

### 3.5 Ordering, delivery semantics, reconnect

**Ordering (binding, §2.6 / notes §4.6):**

- **Within one `Continue` session:** strict FIFO, single-consumer, no interleaving,
  no reordering. The exclusive-consumer pattern is what keeps a warm session's
  reasoning coherent.
- **Across `Spawn`-route siblings:** **no ordering guarantee** — independent by
  construction. If order matters, it is a `Continue` route, not a `Spawn` route.
- **Across different sessions/routes:** concurrent, unordered. Exactly-one-owner
  means there is no cross-route race on a single event.

**At-least-once + idempotent via re-read-current-state.** Notifications can be
redelivered (reconnect, restart, coalesce edge). Because the agent acts on **what
the resource *is* now** (it `resource.read`s on wake — RFC 0007), processing
converges. We promise **convergence on current state, not exactly-once.** This is
why coalesce is lossless and why redelivery is safe.

**Notify-then-read (binding, RFC 0004 §3.1).** The `updated{uri}` carries no diff.
The router never tries to read the changed body itself; it delivers the URI(s) +
latest etag into the agent, and the *agent* decides whether to `resource.read`.
This keeps large diffs out of the supervisor and out of context unless the agent
asks.

**Reconnect recovery (binding, §2.6 / RFC 0003 reconcile).** On MCP server
reconnect or supervisor restart, the supervisor re-subscribes every *declared*
subscription and **synthesizes one coalesced `Synthetic("possibly changed")` event
per watched URI** (read-after-subscribe converts edge-triggering to
level-triggering across the boundary — mandatory, not optional). This recovers
any update missed while disconnected; the re-read-current-state model makes it
safe. Warm sessions and *dynamic* (self-subscribe) routes are lost on supervisor
restart in v1 (recovered by idempotent re-trigger).

**`list_changed` handling.** `notifications/resources/list_changed` (no URI, RFC
0004 §3.2) refreshes the resource catalogue (RFC 0007) and, for **glob** routes,
re-evaluates membership: newly-appeared concrete URIs matching the glob are
subscribed and seeded with a `Synthetic` event; vanished URIs are unsubscribed.
`list_changed` is a *distinct event source* from `updated` and is not itself a
routable per-URI event (it has no URI to match).

### 3.6 Self-subscribe (self-scheduling) and time as an event source

**Self-subscribe = self-scheduling (the signature capability, §2.6).** When a
running agent calls the `subscribe(uri)` self-tool mid-reasoning (RFC 0005), the
supervisor:

1. creates a `Subscription(self_mcp_or_target_server, uri)` and issues
   `resources/subscribe` upstream (capability-gated, RFC 0004);
2. **auto-creates a route** `Route { matcher: Exact(uri), disposition:
   Continue(this_session), debounce: 250ms, overflow: Coalesce }`, inserted at the
   **front** of the declared order for `this_session` (most specific, owned by the
   caller);
3. returns success as a tool result. The agent then ends its turn; the session
   goes **warm** (suspended). The next `updated{uri}` re-enters *this* session.

`unsubscribe(uri)` removes the route + subscription; if the session has no other
subscriptions and no pending work, it is garbage-collected (or checkpointed — a
v2 extension, §2.8 / RFC 0013). This closes the loop with async subagents (RFC
0009): a child's completion-as-self-resource is just an `updated` the parent's
self-subscribe route delivers.

**Time is just another event source (§2.6).** `triggers/timer.rs` mints internal
events into the same router. No second scheduling subsystem.

```rust
/// triggers/timer.rs — an event source, not a scheduler.
pub enum ClockSource {
    Interval { period: Duration, next: Instant },     // --interval D (D=0 => immediate)
    #[cfg(feature = "cron")]
    Cron { expr: CronExpr, next: Instant },           // 5-field, UTC, croner
}
impl ClockSource {
    /// the reactor folds next_fire() into its recv_timeout min(); on fire it
    /// emits a synthetic event into the router as ChangeKind::Timer.
    fn next_fire(&self) -> Instant { match self { Self::Interval{next,..} => *next,
        #[cfg(feature="cron")] Self::Cron{next,..} => *next } }
    fn fire(&mut self, now: Instant) -> RouterEvent { /* advance next; return Timer event */ }
}
```

- **`--interval D`** (`D=0` = re-enter immediately): a periodic internal event.
  Routed exactly like a resource update — to a `Spawn` route (fresh root per fire)
  or a `Continue` route (one warm session ticked).
- **`--cron "<min hour dom mon dow>"`** (behind the `cron` feature, `croner`,
  which adds only `chrono` — §2.2): monotonic next-fire computed in **UTC**. The
  timer thread/source emits one `Timer` event per fire into the router.

**No calendars, no DST gymnastics, no job store, no per-fire persistence in core.**
Default TZ = UTC. If timezones matter, the external operator passes the schedule;
the recommended production cron is the orchestrator's CronJob → `agentd --mode
once`, not the in-process `cron` feature.

A clock `Timer` event carries no URI; it is routed to the *single* timer-bound
route declared for the mode (config error if a `--cron`/`--interval` is set with
no timer route, or with an ambiguous set). It does not participate in URI
first-match — it has its own dedicated route slot.

### 3.7 End-to-end: one reactive wake

```
reactor.recv_timeout(min(child_deadlines, route.debounce_timers, clock.next_fire))
 └─ event: McpNotification(server, Updated{uri})                 // RFC 0004 reader thread
     ├─ route = first_match(routes, uri)        // §3.2.1 exactly-one-owner
     │    └─ None: unrouted++; log(debug, route=none); drop. return.
     ├─ route.queue.push(QueuedEvent{uri, etag, kind:Updated}, route.overflow)  // §3.4 coalesce
     └─ (re)arm route.debounce_timer = now + route.debounce
 └─ later wake: route.debounce_timer expired, no newer event
     └─ deliver(route):                          // §3.3
          Spawn:    while inflight<max_inflight { spawn_root_from_event(pop) }
          Continue: if session idle { re_enter_session(drain_set) }   // set, coalesced
 └─ inner loop (subagent, RFC 0007): resource.read(uri) on the agent's terms,
     act on current state, run to terminal/suspend; supervisor updates budgets,
     idle_backoff.reset(), and re-calls deliver() if the queue refilled.
```

### 3.8 Config surface (this RFC's slice)

Following the precedence rule built-in < file < env < flag (RFC 0011), validated
at startup before any side effect:

| Knob | Flag / env | Default | Notes |
|---|---|---|---|
| mode | `--mode once\|loop\|reactive\|schedule` | `once` | the driver |
| interval | `--interval D` / `AGENTD_INTERVAL` | unset | loop/schedule; `0`=immediate |
| cron | `--cron "<5-field>"` | unset | `cron` feature only; UTC |
| session | `--session fresh\|warm` | per §3.1.2 | loop re-entry style |
| subscribe | `--subscribe server:uri` (repeatable) | none | static subscriptions |
| route | `--route match=>spawn\|continue:sid[,debounce=ms,cap=N,overflow=…]` | derived | declared order = match priority |
| debounce | `debounce_ms` per route | 250 | §3.4 |
| max_inflight | per spawn route | 4 | §3.3 backpressure |
| queue_cap | per route | distinct watched URIs (min 16) | §3.4 |
| overflow | per route | `coalesce` | §3.4 |

Routes with no explicit `--route` for a `--subscribe`d URI default to a single
`Spawn` route per mode (`reactive`/`schedule`) or a `Continue(root_session)`
(`loop --session warm`). A `--cron`/`--interval` with no consuming route is a
startup config error → exit 2 (RFC 0011).

---

## 4. Interactions with other RFCs

- **RFC 0001 (core architecture):** this RFC fills the two-loop split's "driver"
  layer and resolves §14.5 (reactive routing) and §14.x mode questions.
- **RFC 0002 (reactor & concurrency):** the router and timer source are pure
  state machines invoked by the single `recv_timeout` reactor. The reactor owns
  the merged `mpsc`, the self-pipe signal wake, and the timer arming (it takes
  `min()` over child deadlines, route debounce deadlines, and clock next-fires).
  The abandon-don't-interrupt invariant is the reactor's; routing never blocks.
- **RFC 0003 (supervision/recovery):** spawn-route roots and warm sessions live in
  `supervisor/tree.rs`; restart governor handles `RootDisposition::Restart`; the
  tree-wide token budget is the ultimate backpressure and the `loop`/`reactive`
  exit class; **reconnect reconcile + read-after-subscribe** (§3.5) is the
  recovery path this RFC depends on for at-least-once.
- **RFC 0004 (MCP client):** provides `resources/subscribe`/`unsubscribe`,
  capability-gating, and the `updated{uri}` / `list_changed` notification dispatch
  this router consumes. Notify-then-read and item-vs-list-distinct are inherited
  facts. Reactivity is **stdio-only in v1** (HTTP/SSE deferred, RFC 0013) — the
  router is transport-agnostic but only stdio servers deliver notifications in v1.
- **RFC 0005 (self-MCP + control protocol):** the `subscribe`/`unsubscribe` and
  `resource.read` self-tools; self-subscribe → auto `Continue(this_session)` route
  (§3.6). Delivery into a warm session and re-entry use the control channel.
- **RFC 0007 (agentic loop):** the single inner loop every driver drives; its
  terminal statuses are the inputs to `on_root_terminal`; its
  re-read-current-state behavior is what makes coalesce + at-least-once safe.
- **RFC 0009 (subagent model):** `spawn_root_from_event` goes through the single
  supervisor-owned spawn chokepoint with supervisor-minted depth (0 for roots);
  the spawn template (instruction + seed + scope + limits + telemetry) is a route
  property. Async subagent completion-as-self-resource (M3) reuses self-subscribe.
- **RFC 0010 (observability):** every routed event logs `route` (owning id or
  `none`), debounce/coalesce outcome, and the routing decision (replay substrate);
  counters `unrouted`, `dropped_events`, `coalesced`; idle-backoff state.
- **RFC 0011 (cloud-native contract):** the exit-code mapping for each driver, the
  `DRAINING` flag wiring (drain disarms triggers, winds down sessions at turn
  boundaries), and config precedence + validate-at-startup.

---

## 5. Non-goals / Deferred

- **Reactive over HTTP / Streamable HTTP serving.** v1 keeps reactivity on
  **stdio only** (§2.5, §5 risk 1). The SSE GET client and the full Streamable
  HTTP server are deferred (RFC 0013). The router is transport-agnostic by
  construction, so this is a transport-layer addition, not a router change.
- **MCP `tasks` as the external long-running surface.** Deferred to v2 (RFC 0013);
  v1's external "spawn-and-await" is MCP self-tools (RFC 0005), not tasks.
- **Calendars / DST / timezone job-store / per-fire schedule persistence.** Out of
  core; UTC only; external CronJob is the production scheduler.
- **Streaming partial subagent results into a parent mid-flight.** Out for v1
  (RFC 0009 / notes §6.3); async-handle + completion-as-resource covers the need.
- **`block` overflow as a default.** Opt-in only; default is `coalesce`.
- **Session checkpointing of warm reactive sessions across supervisor restart.**
  v1 loses warm/dynamic state on restart (recovered by idempotent re-trigger);
  MCP-backed checkpoint is a v2 extension (RFC 0013).
- **Cross-route fan-out / multi-owner events.** Explicitly rejected: exactly-one-
  owner is binding.

---

## 6. Open items

None that block implementation. The numeric defaults are *starting* values to be
empirically tuned, all overridable, and not load-bearing for correctness:
`debounce_ms=250`, `max_inflight=4`, idle-backoff base 1s / cap = `--interval`,
`queue_cap` = distinct watched URIs (min 16). One semantic flag carried from the
notes (§11.4): the default `Coalesce` overflow assumes **current-state** resource
semantics; if a target use case exposes a resource with true **event-stream**
semantics (each update a discrete item that must not be lost), that specific route
must be declared with `drop_*` or, in v2, a durable queue — this is a per-route
config choice, not a core change.
