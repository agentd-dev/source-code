# Design Notes — The Agentic Loop & the Execution Modes

**Lens:** the inner agentic loop + the three execution modes (single / loop /
reactive) + time-scheduled runs + subagent spawn semantics.
**Status:** design input for RFC 0001 (`rfcs/0001-mcp-native-agent-runtime.md`).
**Author:** review subagent. **Date:** 2026-06-25.

This is a durable design artifact. It makes concrete, opinionated
recommendations — not a survey. Where the RFC left an open question (§14), I
take a position and justify it. Where the 2024–2026 production literature has a
clear answer, I cite it and adapt it to agentd's minimalism bar.

---

## 0. TL;DR of the recommendations

1. **One loop implementation, three drivers.** The agentic loop (inside a
   subagent process) is identical in all modes. *Single / loop / reactive* are
   not three loops — they are three things the **supervisor** does to *start*,
   *re-enter*, or *stop* that one loop. Build the loop once; build three thin
   supervisor drivers around it.
2. **Use native tool-calling, not the old JSON-action protocol** as the
   primary path, with the JSON-action shape as a typed fallback for models/
   gateways without tool-calling. The retired code's `{"action":"tool"|"final"}`
   parser (`crates/agentd/src/agent/loop_node.rs`) is a good *fallback* and a
   good *internal control* shape — keep it, demote it.
3. **Stopping is a disjunction of cheap checks**, evaluated every turn: model
   says final · step cap · token budget · deadline · **no-progress (idle)** ·
   per-tool repeat cap · cancel. Each has a *distinct* terminal status so the
   parent and an external scheduler can tell *why* it stopped.
4. **Resources reach the agent two ways at once:** the resource *list*
   (names + descriptions + URIs, not bodies) is injected into context as a
   compact catalogue; resource *bodies* are pulled on demand via a
   `resource.read` tool. List = awareness; read = attention.
5. **Reactive routing is a small, explicit, deterministic rule** (§4.3): a
   subscription is owned by exactly one *route*; a route is either
   `spawn`-per-event or bound to one warm session; bursts are coalesced by a
   per-resource debounce window; events are a bounded per-route queue with an
   explicit overflow policy. This is the part the ecosystem has *not* solved —
   it is agentd's opportunity, so it must be specified precisely.
6. **Context management is minimal and lever-ordered:** (a) drop stale tool
   results first, (b) then summarize-and-restart (compaction) at a high-water
   mark. No vector DB, no memory service in core — a `note.write`/`note.read`
   pair backed by a file is the only "memory," and it is just an MCP tool.
7. **Subagents are synchronous-by-default, async-opt-in.** A child inherits an
   explicit, *narrowed* context seed (never the parent's full transcript),
   returns a *distilled* result (1–2k tokens), and the parent blocks on it
   unless it explicitly fires-and-forgets.

---

## 1. The inner agentic loop (the thing intelligence runs)

This lives in the **subagent process**. The supervisor never runs it (RFC §3).

### 1.1 Turn structure (ReAct, deliberately ordinary)

The RFC (§6.1) already draws the loop. Production guidance from 2024–2026 says
the *shape* should stay boring — ReAct (reason → act → observe) is still the
right default; the engineering value is in the guardrails *around* the loop, not
in exotic loop topologies. Concretely, one **turn** is:

```
1. assemble request        (§1.2)
2. call intelligence        -> assistant message (text and/or tool calls)
3. record usage, bump budgets, append assistant message
4. branch on the message:
     - has tool calls?  -> execute each (§1.4), append results, continue
     - final / no calls -> emit result, end turn  (§1.3 stop rules apply first)
5. evaluate stop conditions (§1.3); if none fire, loop
```

Keep ReAct as the baseline. Do **not** build plan-and-execute, ReWOO, or a
separate reflection pass into core. Reflection-as-a-second-model-call has a known
failure mode in 2025 replication work: a single model critiquing its own output
*reinforces its own blind spots* rather than correcting them. If reflection is
wanted, it belongs to the *instruction* (the model can be told to self-check) or
to an explicit subagent ("spawn a critic with a clean context"), not to runtime
machinery. This keeps the binary small and avoids baking in a pattern that
doesn't reliably help.

### 1.2 Message / context assembly

Each request is assembled fresh from a small set of parts, in this order:

1. **System block** — standing instructions + operating contract + tool-use
   guidance. Anthropic's "right altitude" rule: specific enough to steer,
   flexible enough to generalize; organized into labeled sections
   (`<role>`, `<task>`, `## tools`, `## resources`, `## output`,
   `## limits`). Minimal ≠ short — give enough, no more.
2. **Resource awareness block** (§3) — a compact catalogue of *available* MCP
   resources (URI + name + one-line description + mtime/etag if known), and a
   note that `resource.read` fetches bodies. Bodies are **not** inlined here.
3. **Instruction** — the task (the parent's `instruction` / the event payload
   for a reactive turn).
4. **Context seed** — parent-provided messages/data (for a subagent) or the
   carried-over conversation (for a warm reactive/loop session).
5. **Running transcript** — prior turns: assistant messages, tool calls, and
   tool results, *as managed by §5 compaction*.

The tool catalogue (scoped MCP tools + agentd self-tools) is passed in the
provider's `tools` field (native tool-calling), not stuffed into the system
text, *when the gateway supports it*. Tool definitions must be unambiguous and
non-overlapping — Anthropic's test: "if a human engineer can't say which tool to
use, neither can the agent." Since agentd ships **no** tools of its own beyond
the self-MCP, tool quality is the MCP server author's responsibility; agentd's
job is to pass them through faithfully and cheaply (token-efficiently).

### 1.3 Stopping conditions (the heart of "when does it stop")

This is the question the brief asks most pointedly. The answer is the same for
*all three modes* — the difference between modes is only what the supervisor does
**after** a stop. Evaluate these every turn, cheapest first, and attach a
**distinct terminal status** to each so callers can branch:

| Stop reason | Status | Detection | Notes |
|---|---|---|---|
| Model produced a final answer | `completed` | assistant message has no tool calls (native) or `{"action":"final"}` (fallback) | the normal, desired exit |
| Step cap reached | `exhausted_steps` | turn counter ≥ `max_steps` | hard ceiling; see §7 |
| Token budget reached | `exhausted_tokens` | cumulative prompt+completion ≥ `max_tokens` | checked *before* each call |
| Deadline reached | `deadline` | `now ≥ deadline` | wall-clock; checked every turn |
| No progress (idle) | `stalled` | N consecutive turns with no *new* observable state (§1.3.1) | the under-appreciated one |
| Per-tool repeat cap | `loop_detected` | same tool + ~same args called > K times | guards the "called a broken tool 400×" failure |
| Cancelled | `cancelled` | control-channel `cancel` from parent | hard, cooperative-then-SIGKILL |
| Crash | `crashed` | process exit / panic | supervisor observes via exit code |

Two of these are routinely forgotten and *are* the production-failure stories
of 2024–2026, so they are mandatory in agentd:

- **The global step/token/deadline cap is non-negotiable.** The literature's
  cautionary tale: four agents ran 11 days and billed \$47K on a missing
  `max_steps`. agentd already has the bones for this in the retired
  `BudgetTracker` (`crates/agentd/src/budget.rs`: cumulative token cap with a
  `check_*_budget()` BEFORE the call, `add_*` AFTER) — reuse that exact
  check-before / record-after discipline.
- **Per-tool repeat cap, separate from the global step cap.** ReAct agents
  happily re-call a tool with cosmetically reworded args forever. Maintain a
  small map `hash(tool_name + canonicalized_args) -> count`; if a call repeats
  beyond K (default K=3) with no intervening success, refuse it *as a tool
  result* ("you have called X with these args K times; it is not progressing —
  try a different approach or finish") rather than executing it again. This is
  cheap, model-visible, and self-correcting.

#### 1.3.1 No-progress / idle detection (concrete)

"Idle" for a *working* loop means **no new state across iterations**. Define a
per-turn **progress signature** = a hash of the set of *new* facts the turn
added: (new tool-result content digests) ∪ (assistant text delta digest). If the
signature is unchanged (or a tool returned an identical error) for `idle_limit`
consecutive turns (default 3), stop with `stalled`. This catches the
"confidently spinning, making no real progress" silent-failure mode without a
model call. It is intentionally simple — a content hash, not semantic judgment.

For the **loop mode** specifically (§2.2), "idle" *also* has an outer meaning:
the agent completed and there is genuinely nothing to do. That is handled by the
*supervisor's* re-entry policy (idle backoff), not the inner loop.

### 1.4 Tool-call execution + result feedback

For each tool call in an assistant turn:

1. **Scope check** — is this tool in the subagent's granted scope (§6.3 of
   RFC)? If not, return a tool *result* saying so (recoverable), do not abort.
   The retired loop_node does exactly this ("not in this loop's allowed set")
   and it is the right behavior — keep it.
2. **Route** — resolve the owning MCP server (or the agentd self-MCP) and call
   `tools/call`. agentd self-tools (`subagent.*`, `subscribe`, `exec`) route
   internally.
3. **Result back as an observation** — append the result (or the error) to the
   transcript as a tool-result message. **Tool errors are observations, not
   fatals, by default** — the model sees the error text and adapts. This is the
   single most important resilience property of ReAct and is well-supported in
   the literature ("letting the agent know when a tool is failing and letting it
   adapt").

**Error taxonomy — when an error is NOT just an observation:**

| Class | Examples | Handling |
|---|---|---|
| *Tool-domain error* | file not found, HTTP 404, bad args, validation fail | feed back as observation; model adapts. Default. |
| *Transient transport error* | MCP server timeout, connection reset, 429/503 | **retry with bounded backoff at the transport layer** (e.g. 3 tries, jittered) *before* surfacing; if still failing, surface as observation. |
| *Capability-unavailable* | tool not compiled / server down / not in scope | observation ("tool X unavailable"); model picks another path. |
| *Fatal infrastructure* | intelligence endpoint unreachable, auth rejected, OOM, budget hard-stop mid-call | abort the turn with the matching terminal status; do **not** loop on it. |

The line: errors the *model can do something about* are observations; errors
about *the runtime itself* are terminal. Transport flakiness gets a thin retry
so a single dropped MCP pipe doesn't burn a step.

### 1.5 Malformed model output

Two regimes:

- **Native tool-calling path:** the provider returns structured tool calls;
  malformed JSON *args* are the main risk. Validate against the tool's input
  schema if present; on failure, feed the validation error back as a tool result
  ("your args for X were invalid: …"). Recoverable, costs one step.
- **JSON-action fallback path:** the model must emit one action object. Reuse
  the retired `extract_json_object` + `parse_action` (balanced-brace scan that
  tolerates prose/code-fences — `loop_node.rs` lines ~321–373). On parse
  failure, feed back "your reply was not a valid action object: … reply with
  exactly one JSON object." Recoverable, costs one step.

Either way: malformed output is **recoverable and step-consuming**, never an
abort — but it *does* count toward `max_steps` and toward the per-tool/no-progress
detectors, so a model stuck emitting garbage will terminate as `stalled` rather
than spin forever.

---

## 2. The three execution modes (supervisor drivers)

All three drive the *same* inner loop. Each is a small supervisor state machine.

### 2.1 Single one-shot

```
parse config -> connect MCP -> spawn ONE root subagent with the instruction
             -> stream its events -> on terminal status: print result,
                propagate exit code, kill tree, exit.
```

- **Default mode.** No daemon, no socket, no warm session.
- **Exit code maps to terminal status:** `0` for `completed`; non-zero distinct
  codes for `exhausted_*` / `deadline` / `stalled` / `crashed` so an external
  `Job`/`CronJob` can tell success from a capped/failed run. (Suggested:
  0=completed, 2=exhausted/deadline/stalled [reached a budget, partial result],
  3=tool/policy hard-fail, 1=internal/crash.)
- **Result on stdout, events on stderr** (structured JSON lines), so the human/
  pipe gets the answer and the operator gets the trace.

### 2.2 Loop / interval (continuous agent)

```
loop:
  spawn (or continue) a root subagent for one "shift"
  on terminal status:
    completed | stalled  -> sleep(interval or backoff), then re-enter
    exhausted_* | deadline -> re-enter is allowed but the global tree budget
                              still binds; if the tree budget is spent, EXIT
    crashed             -> restart with capped restart-rate (§8); if rate
                              exceeded, EXIT non-zero
  until: SIGTERM | global deadline | global tree token budget | max_restarts
```

**When does loop mode stop?** Three independent stoppers, any of which exits the
daemon: (a) an outer **global budget** (tree-wide token ceiling and/or an
absolute wall-clock deadline) — the same cumulative caps as the inner loop but
scoped to the whole process lifetime; (b) **SIGTERM/SIGINT** (graceful drain →
kill tree → exit); (c) a **restart-storm breaker** (too many crashes too fast).
A *healthy idle* loop (nothing to do, keeps completing trivially) should not
spin hot — apply **exponential idle backoff** between shifts (e.g. 1s → 2s → …
→ cap at `interval`), reset on any real work. This is the cheap defense against
"polling agent burns money doing nothing."

**Two flavors, one knob:**
- `--interval D` → timer-driven re-entry every D (polling shape).
- `--interval 0` / `continuous` → re-enter immediately on completion
  (work-until-done shape). Here the idle-backoff and global budget are what keep
  it sane.

Whether each shift is a **fresh** subagent (stateless polling) or a **warm**
continuation (carries context) is a config choice; default **fresh** for
interval polling (clean context each poll), **warm** for continuous (it's one
long job). This is the same spawn-vs-continue axis as reactive (§4.3) and shares
that code.

### 2.3 Reactive (the signature mode) — overview

```
subscribe to N resource URIs (config + dynamic via the `subscribe` self-tool)
idle (near-zero cost; just holding MCP connections + notification readers)
on notifications/resources/updated(uri):
    -> ROUTE the event (§4.3) to: a fresh spawn, or a specific warm session
    -> (re)enter the inner loop for that route
on notifications/resources/list_changed:
    -> refresh the resource catalogue (§3); optionally treat as an event if a
       watched glob now matches a new resource
until: SIGTERM | global budget | explicit shutdown
```

Reactive is the mode the ecosystem has *not* built (the protocol supports
`resources/subscribe` + `notifications/resources/updated`, but as of 2026 no
major client consumes them — agents are still stateless request/response). So
the routing/debounce/backpressure rules below are **the** design contribution;
they are specified precisely in §4.

### 2.4 Time-scheduled (interval/cron-ish — minimal)

Keep this *tiny*. The RFC bias and the brief both say "minimal." Two primitives,
both implemented as **internal time events fed into the same reactive router**:

- **Interval** — `--interval D` (already in loop mode; a periodic internal
  event).
- **Cron-ish** — a small 5-field cron parser (min hour dom mon dow), monotonic
  next-fire computation, one timer thread that emits an internal `tick` event.
  The retired tree has `triggers/cron.rs` behind a feature flag — harvest it,
  but expose it as **"a clock is just another event source"**, routed exactly
  like a resource update. No second scheduling subsystem.

Do **not** build calendars, timezones-with-DST gymnastics, or a job store in
core. An external scheduler (the K8s operator, *not in this repo*) is the real
cron for production; agentd's in-process cron is for standalone/daemon
convenience. If timezones matter, the operator passes them; agentd computes in a
single configured TZ (default UTC) and stops there.

---

## 3. "Pay attention to available resources" — list vs read (both)

The brief asks pointedly: does the agent get the resource *list* in context, or
read resources via tools, or both? **Both — and they play different roles.**

- **Awareness (the list, in context).** At loop start and after any
  `list_changed`, the supervisor calls `resources/list` on each scoped server
  and injects a **compact catalogue** into the system/awareness block: for each
  resource, `{uri, name, one-line description, mtime/etag/size if available}`.
  **Never the bodies.** This is what makes the agent *aware* of what it can look
  at without paying for the content. Cap the catalogue (e.g. top-N by relevance
  or a size budget); if a server exposes thousands of resources, summarize by
  prefix/type rather than listing every URI.
- **Attention (the body, via a tool).** Provide a `resource.read(uri)` self-tool
  (thin wrapper over `resources/read`). The agent pulls a body only when it
  decides it needs it. This is ordinary "retrieve on demand," and it keeps
  context lean: bodies enter context only when load-bearing, and §5 can evict
  them once consulted.

This mirrors Anthropic's "smallest set of high-signal tokens" principle: the
list is cheap awareness; reads are deliberate, model-chosen attention. For the
**reactive** case, the event payload should include the *changed URI* (and
etag/version if the server provides one) so the agent knows *what* changed; the
agent then `resource.read`s it if the change is relevant. Re-reading on the
agent's terms (not auto-inlining the new body) avoids dumping large diffs into
context on every notification.

---

## 4. Reactive routing — the precise rule (RFC §14.5, resolved)

This section answers the brief's hardest sub-questions: *which session an update
belongs to; spawn-vs-continue; debounce/coalesce; backpressure; ordering.*

### 4.1 Vocabulary

- **Subscription** — one `(server, resource_uri)` the supervisor is watching.
- **Route** — the binding that says what to do when a subscription fires. A
  route is declared in config or created dynamically (§4.4). Each route has:
  `{ match: uri-or-glob, disposition: spawn | continue(session_id),
     debounce_ms, queue_cap, overflow: drop_oldest|drop_newest|coalesce|block }`.
- **Session** — a warm, suspended inner-loop state (its transcript + scope +
  budgets) that a `continue` route re-enters.

**Invariant — exactly-one-owner:** every incoming
`notifications/resources/updated(uri)` is matched to **exactly one** route by
**first-match in declared order** (most specific / longest-prefix first; exact
URI beats glob). No event fans out to two routes. If nothing matches, the event
is logged and dropped (with a counter). This determinism is what makes the
behavior auditable and replayable.

### 4.2 Spawn-vs-continue — the decision

This is a property of the **route**, not a per-event guess, so it is fully
deterministic:

- **`spawn`** (stateless reaction): each matching event starts a **fresh** root
  subagent whose instruction is templated from the event (`uri`, change kind,
  payload). Use when reactions are independent (e.g. "for each new file in this
  dir, process it"). Concurrency across spawned siblings is bounded by a
  route-level `max_inflight` (default small, e.g. 4) — that *is* the backpressure
  knob for spawn routes.
- **`continue(session_id)`** (stateful reaction): the event is delivered **into
  one specific warm session** and re-enters its loop where it left off. Use when
  the agent is doing one ongoing job and updates are new information for it (the
  RFC's "agent wakes up, reads what changed, keeps working in the same
  context"). A session processes its events **one at a time, in order** (§4.6);
  it is a single consumer of its own queue.

**Self-subscription (the novel capability).** When a running agent calls the
`subscribe(uri)` self-tool mid-reasoning, the supervisor creates a route with
`disposition: continue(this_session)` automatically — i.e. the agent has just
**scheduled its own future continuation**. It then ends its turn; the session
goes warm; the next update on `uri` re-enters *this* session. `unsubscribe`
removes the route and, if the session has no other subscriptions and no pending
work, lets it be garbage-collected (or checkpointed — §6).

### 4.3 The full routing algorithm (per event)

```
on event E = updated(uri, etag?, payload?):
  route = first_match(routes, uri)            # exactly-one-owner, §4.1
  if route is None: metric(unrouted++); log; return
  push E onto route.queue under route.debounce/overflow policy   (§4.4, §4.5)
  ensure route has an active consumer:
     - spawn route:    if inflight < max_inflight, pop & spawn fresh subagent
                       (templated instruction); else leave queued (backpressure)
     - continue route: if session idle, deliver next coalesced event & re-enter;
                       if session busy, leave queued (it will pull on return)
```

### 4.4 Debounce / coalesce (burst handling)

Resources are chatty (an editor saving a file fires many updates/sec). Per
route:

- **Debounce window** `debounce_ms` (default e.g. 250ms): on an event, (re)arm a
  timer; only when it expires without a newer event do we actually deliver. This
  collapses a burst of writes into one wake-up.
- **Coalesce semantics:** multiple events on the *same uri* within the window
  collapse to **one** delivery carrying the **latest** etag/version (we don't
  need the intermediate states — the agent will `resource.read` the current
  value anyway, §3). Events on *different* uris owned by the same `continue`
  session are delivered as a **set** ("these N resources changed") in one
  wake-up, not N separate re-entries. This is the single most important
  efficiency lever for reactive mode.

### 4.5 Backpressure (events outpace processing)

Each route's queue is **bounded** (`queue_cap`). When full, apply the route's
declared `overflow`:

- `coalesce` (**default** for resource routes): newest-wins per uri; the queue
  never grows beyond the number of distinct watched uris. This is almost always
  what you want for "current state changed" semantics — you only care about the
  latest.
- `drop_oldest` / `drop_newest`: for routes where events are discrete items you
  might lose (rare for resources; relevant if a route models a work queue).
  Dropping increments a visible `dropped_events` metric.
- `block`: stop reading notifications from that server until the queue drains
  (true backpressure to the source). Only safe with a server that buffers;
  risky (can stall the connection) — opt-in, not default.

Across all routes, a **global inflight ceiling** and the **tree-wide token
budget** are the ultimate backpressure: when the tree budget is near-spent, the
supervisor stops spawning and only drains warm sessions, then quiesces. The
process never melts down; it degrades to "not starting new work."

### 4.6 Ordering

- **Within one `continue` session:** strict FIFO, single-consumer. The session
  finishes processing one wake-up (runs its loop to a turn-boundary / suspend)
  before the next event is delivered. No interleaving, no reordering. This is
  the exclusive-consumer pattern and it is what makes a warm session's reasoning
  coherent.
- **Across spawn-route siblings:** no ordering guarantee — they're independent
  by construction; if order matters, it's a `continue` route, not a spawn route.
- **Across different sessions/routes:** concurrent, unordered. The
  exactly-one-owner invariant means there's no cross-route race on a single
  event.
- **At-least-once, idempotency expected.** A notification can be redelivered
  (reconnect, restart). Because the agent re-reads current state on wake
  (it acts on *what the resource is now*, not on a delta), processing is
  naturally idempotent for state-changed semantics. We do **not** promise
  exactly-once; we promise "you'll always converge on current state."

### 4.7 Reactive failure & reconnect

- If an MCP server connection drops, the supervisor **re-subscribes on
  reconnect** and immediately treats every watched resource as "possibly
  changed" (one synthetic update per watched uri, coalesced). This recovers any
  updates missed while disconnected — the re-read-current-state model makes this
  safe.
- `notifications/resources/list_changed` → refresh the catalogue (§3) and, for
  glob routes, evaluate whether newly-appeared resources now match (and should
  be subscribed).

---

## 5. Context-window management (minimal, lever-ordered)

The agentic loop's transcript grows; long contexts degrade ("context rot":
precision falls as tokens rise — a gradient, not a cliff). Minimal approach, in
the order Anthropic recommends applying levers:

1. **Lever 1 — clear stale tool results (cheapest, safest).** Once a tool result
   is deep in history and has been superseded, replace its body with a tiny
   stub (`[tool result for read_file(x) elided; re-read if needed]`). "Why would
   the agent need to see the raw result again?" This alone reclaims most of the
   bloat in tool-heavy loops and is *lossless in practice* because the agent can
   re-fetch. Implement as: keep the **last M** tool results verbatim
   (default M≈5, matching Claude Code's "5 most recent files"), stub older ones.
2. **Lever 2 — compaction (summarize-and-restart) at a high-water mark.** When
   estimated prompt tokens cross a threshold (e.g. ~70–80% of the model's
   window, configurable), make **one** model call: "summarize this conversation,
   preserving decisions made, unresolved problems, key facts/IDs, and current
   plan; drop redundant chatter." Start a fresh transcript = `[system] +
   [summary] + [last M verbatim tool results/files]`. Tune by *recall first*
   (capture everything relevant), then precision. This is exactly Claude Code's
   compaction and Anthropic now ships a compaction API — but agentd does it with
   one ordinary model call and a prompt, **no new dependency**.
3. **Lever 3 — externalize to notes (optional, opt-in).** For long-horizon work,
   a `note.write(text)` / `note.read()` self-tool pair backed by a single file
   lets the agent persist durable state *outside* the window and pull it back
   when needed (the Pokémon-agent pattern: tallies + strategy notes surviving
   thousands of steps). This is the *only* "memory" in core, and it is just an
   MCP tool over a file — no vector store, no embedding model, no memory service.

**Token estimation without a tokenizer dependency:** the minimalism bar forbids
shipping a tokenizer. Use the provider's `usage` from the *previous* response as
ground truth for what's already happened, and a cheap heuristic (≈ chars/4, or
bytes-based) to *estimate forward* whether the next request will blow the window;
trigger compaction conservatively (early) so an off-by-some estimate never
causes a hard context overflow. If the provider returns a context-length error
despite the estimate, treat it as a signal to compact-now-and-retry once.

**Subagents are themselves a context lever.** The cleanest way to keep the lead
agent's context small is to push exploratory, token-heavy work into a child with
a *clean* window that returns a 1–2k-token distilled summary (Anthropic: a
subagent may burn tens of thousands of tokens but returns only the distillate).
So §6's spawn semantics and §5's context management are the same lever viewed
from two angles.

---

## 6. Subagent spawn semantics

### 6.1 What a child inherits (context seed) — explicit, narrowed, never the
parent's whole transcript

A child receives over its control channel (RFC §6.2):

- **instruction** — its specific objective (Anthropic: give each subagent "an
  objective, an output format, guidance on tools/sources, and clear task
  boundaries" — vague subagent tasks cause duplicated/again-missing work).
- **context seed** — *only* the slices the parent chooses to pass: relevant
  facts, file paths, IDs, a sub-goal. **Not** the parent's full conversation.
  This is both a context-hygiene win (clean child window) and a security win
  (a child sees only what it's told). The default seed is *small*.
- **tool scope** — a subset of the parent's MCP endpoints/tools (RFC §6.3):
  capability narrows monotonically down the tree, never widens.
- **limits** — its own `max_steps` / `max_tokens` / `deadline`, drawn from and
  bounded by the parent's remaining tree budget and the `max_depth` ceiling.

### 6.2 How results return — distilled, structured

The child runs to a terminal status and returns a **result**: a compact,
structured value (ideally matching an `output_format` the parent specified),
target ≈ 1–2k tokens, **plus** its terminal status and usage accounting. The
parent appends *the distillate* (not the child's transcript) to its own context.
For large outputs, follow the "store-and-reference" pattern: the child writes the
bulk to a resource/file via a tool and returns a lightweight reference, which the
parent (or another child) reads on demand — keeps the coordinator's window lean.

### 6.3 Sync vs async — synchronous by default, async opt-in

The brief asks: parent waits, fire-and-forget, or streaming? **All three exist;
the default is synchronous-blocking**, matching Anthropic's production lead-agent
behavior ("lead agents execute subagents synchronously, waiting for each set to
complete"):

- **Sync (default):** `subagent.spawn(...)` blocks the parent's turn until the
  child reaches a terminal status, then returns its result as the tool result.
  Simplest mental model; deterministic; no orphan management. The parent *is*
  paused (its loop is between turns), so this is cheap.
- **Async / parallel (opt-in):** `subagent.spawn(..., {async:true})` returns a
  **handle** immediately; the parent keeps reasoning and later calls
  `subagent.status(handle)` / `subagent.await(handle)` / receives results via a
  self-resource it can subscribe to (closing the loop with §4 — a child's
  completion *is* a resource update the parent reacts to). This is how you get
  the orchestrator-worker fan-out (lead spawns 3–5 workers in parallel). Bound
  by `max_inflight` children per parent.
- **Fire-and-forget:** `subagent.spawn(..., {detach:true})` — parent doesn't
  await; child runs under the supervisor with its own budget and reports to logs/
  a resource. Use sparingly; detached children still count against the tree
  budget and depth, and the supervisor still reaps them.
- **Streaming:** in all cases the child streams loop events (thought / tool-call
  / tool-result / final) up the control channel for observability and for the
  parent's supervision decisions (pause/cancel). "Streaming results to the
  parent's reasoning" — i.e. partial results influencing the parent mid-flight —
  is **out of scope for v1** (it complicates the parent's context management);
  the async-handle + await covers the real need.

**Scaling rule for the orchestrator (give the model this heuristic):** simple
fact-find → 1 child; comparison → 2–4; complex → more, with clearly divided,
non-overlapping responsibilities. And the explicit *anti*-guidance: **do not
spawn subagents for tightly-coupled work that needs shared context** (most
coding-style tasks) — multi-agent shines for *parallel, independent
exploration*, and it costs ~15× the tokens of a single chat. The model should
prefer staying single-agent unless the task is genuinely parallelizable.

### 6.4 Recursion guards

`max_depth` (default small, e.g. 3–5) and a **tree-wide token ceiling** shared by
all descendants prevent runaway recursion/fan-out. A spawn that would exceed
depth or tree budget is **refused as a tool result** ("spawn denied: max depth /
tree budget reached") — the parent adapts, the tree never explodes. This reuses
the budget-check-before-act discipline from `budget.rs`.

---

## 7. Budgets & limits — what happens at each

Per-subagent and tree-wide, checked **before** the spending act and recorded
**after** (the `budget.rs` pattern). Behavior at each limit is a *graceful,
labeled stop*, never a silent hang and never an uncontrolled overrun:

| Limit | Scope | At-limit behavior | Terminal status |
|---|---|---|---|
| `max_steps` | per subagent | stop after current turn; return best partial result | `exhausted_steps` |
| `max_tokens` | per subagent + tree-wide | refuse the *next* model call; return partial | `exhausted_tokens` |
| `deadline` | per subagent + global | stop at turn boundary; if a model call is in flight, let it finish then stop | `deadline` |
| `max_depth` | tree | refuse `subagent.spawn` (tool result) | n/a (parent continues) |
| tree token ceiling | tree | refuse new spawns + new model calls; drain warm sessions; quiesce | `exhausted_tokens` (tree) |
| `max_inflight` | per parent / per route | queue the spawn/event (backpressure) | n/a |
| per-tool repeat cap K | per subagent | refuse the repeated call (tool result) | may lead to `stalled` |
| RLIMIT_AS / RLIMIT_CPU | per process | kernel kills the process | observed as `crashed` |

Key principle: **at every budget the agent gets a chance to wrap up gracefully**
(return what it has) rather than being guillotined — *except* the OS-level
RLIMIT/SIGKILL backstops, which exist precisely for the case where graceful stop
failed (a wedged or runaway child). The supervisor's hard `SIGKILL` of a subtree
is the ultimate "this child is not responding" recovery (§8).

---

## 8. Failure detection, recovery, stability (the "stay alive" requirements)

This serves brief requirements (6) and (8) — detect dead/stuck subprocesses,
recover, stay stable.

- **Liveness via heartbeats on the control channel.** Each subagent emits a
  lightweight event each turn (or a periodic `tick` if a single model call runs
  long). The supervisor tracks `last_event_at` per child. If a child emits
  nothing for `liveness_timeout` (and isn't legitimately blocked on a model
  call we know is in flight), it is **stuck** → the supervisor cancels it
  cooperatively, then `SIGKILL`s if it doesn't exit within a grace period.
- **Distinguish stuck-in-model-call from stuck-process.** A long but legitimate
  model call shouldn't trip liveness. Track call-start; allow up to the
  intelligence request timeout; only past *that* is it dead. The intelligence
  client must have its own request timeout (bounded HTTP read).
- **Crash containment.** A child panic/segfault is just a non-zero exit the
  supervisor reaps; it never takes down the supervisor (the whole point of
  process isolation, RFC §3). The supervisor records `crashed` and applies the
  mode's restart policy.
- **Restart policy with a storm breaker.** In loop/reactive mode, a crashed
  root subagent may be restarted, but with **exponential backoff and a max
  restart rate** (e.g. ≤ N restarts per window); exceeding it exits the daemon
  non-zero (let the external scheduler decide whether to reschedule the pod).
  This prevents crash-loops from burning money.
- **Graceful drain on SIGTERM/SIGINT (good-citizen requirement).** (a) stop
  accepting new triggers/events; (b) signal warm sessions/children to finish the
  current turn and return partial results within a bounded drain window;
  (c) `SIGKILL` anything still alive at deadline; (d) flush logs; (e) exit with a
  meaningful code. This is the contract a K8s operator relies on.
- **Healthcheck.** A trivial health signal (RFC §11): the supervisor is healthy
  iff its event loop is responsive and at least one MCP connection is up (or it's
  idle-by-design). Expose as a readable self-MCP resource and/or a tiny
  liveness file/endpoint — minimal, no web framework.

---

## 9. Determinism / replay

Full determinism is impossible (the model is non-deterministic, tools touch the
world). What agentd *can* and *should* guarantee is **replayable observability**,
not bit-identical re-execution:

- **Append-only event log per run.** Every turn emits structured JSON-line
  events (the RFC already wants this): request digest, tool calls + args + result
  digests, usage, terminal status, timestamps, and the routing decision for
  reactive events (which route owned the event, debounce/coalesce outcome). This
  is the audit trail and the replay substrate.
- **Record the resolved inputs.** Pin and log: the exact instruction, the model
  id + sampling params, the tool catalogue presented, the resource catalogue,
  and the budgets. Two runs with the same pins are *comparable*; a run is
  *explainable* from its log alone.
- **Seeded determinism where the provider allows it.** If the intelligence
  endpoint accepts a `seed` and `temperature=0`, pass them through for the
  closest-to-deterministic behavior; never depend on it.
- **Replay = re-drive the supervisor from the recorded event sequence** (same
  routing decisions, same coalescing) against *recorded tool results* (a
  record/replay tool transport) — useful for debugging the supervisor/router
  without live MCP servers or model spend. This is a *testing* affordance, not a
  production guarantee, and the retired tree's `testing/` + `engine/record.rs`
  +`engine/checkpoint.rs` are harvestable starting points.
- **Session durability (RFC §14.3): in-memory for v1, checkpoint later.** Warm
  reactive sessions live in memory; a pod restart loses them — acceptable for v1
  because the re-read-current-state model means a restarted agent re-subscribes
  and converges. Optional checkpointing (serialize a session's transcript +
  subscriptions to disk) is a clean later extension that lets an external
  scheduler move/restart the pod without losing long-lived context.

---

## 10. What to harvest from the retired code (parts bin)

Concrete reuse targets in `crates/agentd/src` (demote, don't adopt wholesale):

- `agent/loop_node.rs` — the **JSON-action parser** (`extract_json_object`,
  `parse_action`) and the **recoverable-error / refusal feedback** pattern:
  keep as the *fallback* tool protocol and as the inner-loop's error-feedback
  shape. The overall turn skeleton (call → parse → execute → feed back → step
  cap) is sound; rebuild it around native tool-calling + the §1.3 stop set.
- `budget.rs` — the **check-before / record-after** token-budget discipline and
  the atomic cumulative tracker: reuse directly for per-subagent and tree-wide
  token ceilings; extend with step/deadline/per-tool counters.
- `triggers/cron.rs`, `triggers/fs_watch.rs` — harvest the cron parser and the
  watch loop, but re-expose them as **internal event sources feeding the reactive
  router** (§2.4), not as standalone trigger subsystems.
- `engine/record.rs`, `engine/checkpoint.rs`, `testing/` — starting points for
  §9 event-log / replay / session-checkpoint.
- `intelligence/` (client, providers, protocol) — the OpenAI-compatible adapter
  shape and `usage` accounting are reusable for §5's token estimation and §1.4's
  request timeout.

Explicitly **leave behind**: the workflow DAG, the policy/Rego engine, the
built-in tool families (`tools/fs|http|shell|data`) as *core* tools (they become
MCP servers or the gated `exec`), and the signing subsystem. These violate the
new minimalism + MCP-nativity thesis.

---

## 11. Open questions I'm flagging back

1. **Default numeric knobs** need empirical tuning: `max_steps` (suggest 30–50
   per subagent), `idle_limit` (3), per-tool repeat cap K (3), `debounce_ms`
   (250), `max_depth` (3–5), `max_inflight` (4), compaction high-water (75% of
   window), restart-rate breaker. These are *starting defaults*, all overridable.
2. **Native tool-calling vs JSON-action as primary** depends on the intelligence
   wire decision (RFC §14.2). If the canonical gateway shape is
   OpenAI-compatible, native tool-calling is available and should be primary;
   the JSON-action fallback covers providers/local models without it.
3. **`async` subagents in v1?** I recommend sync-only for Phase 2 (RFC §15) and
   adding async handles + parent self-resource completion in Phase 3 alongside
   reactivity, since they share the subscribe/notify machinery.
4. **Coalesce default for resource routes** (newest-wins) assumes
   "current-state" semantics. If any envisioned resource has *event-stream*
   semantics (each update is a discrete item that must not be lost), that route
   needs `drop_*`/durable handling — confirm whether such resources exist in the
   target use cases.

---

## 12. Sources

Production agent-loop & context-engineering guidance (2024–2026):

- Anthropic — *Building Effective Agents*:
  https://www.anthropic.com/research/building-effective-agents
- Anthropic — *Effective context engineering for AI agents*:
  https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents
- Anthropic — *How we built our multi-agent research system*:
  https://www.anthropic.com/engineering/multi-agent-research-system
- *Agentic Loops: From ReAct to Loop Engineering (2026 Guide)* — Data Science
  Dojo: https://datasciencedojo.com/blog/agentic-loops-explained-from-react-to-loop-engineering-2026-guide/
- *The Agent Loop: ReAct and Its Descendants* — Jatin Bansal:
  https://jatinbansal.com/ai-engineering/agent-loop/
- *ReAct vs Plan-and-Execute vs ReWOO vs Reflexion* — The AI Engineer:
  https://theaiengineer.substack.com/p/the-4-single-agent-patterns
- *Event-Driven AI Agents: Patterns That Scale* — DEV / The Daily Agent:
  https://dev.to/thedailyagent/event-driven-ai-agents-patterns-that-scale-39ld
- *Event-Driven Architecture for AI Agent Systems* — Zylos Research:
  https://zylos.ai/research/2026-03-02-event-driven-architecture-ai-agent-systems
- *MCP Has Notifications. So Why Can't Your Agent Watch Your Inbox?* — Ankit
  Mundada: https://ankitmundada.medium.com/mcp-has-notifications-so-why-cant-your-agent-watch-your-inbox-bb688fde7ac5
- MCP spec — Resources (subscribe / updated / list_changed):
  https://modelcontextprotocol.info/docs/concepts/resources/
