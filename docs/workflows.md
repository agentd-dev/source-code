# Workflows

A **workflow** lets an agent process work as an explicit graph of steps — with
branches, loops, and waits — instead of one flat ReAct loop. agentd already *is*
an implicit single-node graph executor (the loop is a hard-coded cycle, the
reactive router is an event→action edge set); a workflow reifies that into an
explicit graph the agent (or an operator) authors and agentd drives. Think
LangGraph, but the agent builds and runs the graph **by itself**, over the same
subagents, MCP tools, and structured data it already uses.

> **Feature-gated, opt-in.** Workflows compile only under `--features workflow`
> (default **off** — an agentd built without it is byte-for-byte unchanged). It is
> dependency-free (serde + `serde_json` only). The degenerate single-`agent`-node
> graph reproduces today's one-shot behavior, so the graph is a superset, never a
> replacement.

---

## The three ways to run a workflow

A workflow is the same serde JSON object every way:

- **Operator-pinned** — run a workflow from a file to completion, then exit:
  ```bash
  agentd --mode workflow --workflow ./pipeline.json --intelligence https://gw.example/v1
  ```
  No `--instruction` is needed (the nodes carry the work), but intelligence is
  still required for `agent`/`infer` nodes. The run is **supervised** exactly like
  `--mode once`: the driver lives in a child process while the supervisor owns the
  kill ladder, cgroup limits, liveness, drain, and the run report. The result
  (with the workflow status, reason, steps, and token cost) prints to stdout and
  the graph status maps onto the normal exit table (see
  [Termination](#termination)).

- **Agent-authored** — the agent defines and runs a workflow itself,
  mid-reasoning, via three self-tools (a root agent only):
  - `workflow.define{workflow}` — validate + store a workflow, returns a `workflow_id`.
  - `workflow.run{workflow_id}` — drive the stored workflow to completion
    synchronously, returning its status + reason + result as the tool result.
  - `workflow.run{workflow_id, detach: true}` — hand the workflow to a **spawned
    subagent** and return a handle immediately; the child process drives it under
    full supervision while the agent keeps working. Collect with
    `subagent.await{handle}` (blocks) or peek with `subagent.status{handle}`.
    Fan-out/fan-in = detach several, then await each.
  - `workflow.patch{workflow_id, patch}` — grow a stored workflow **additively**
    (see [Patching](#patching-a-workflow-additive)).

  This is the "agent orchestrates by itself" path: the agent decides the shape of
  the work, writes the workflow, and runs it — no operator or per-step
  hand-holding.

- **Delegated** — a parent agent hands a whole workflow to a child directly:
  `subagent.spawn{workflow: {...}}` (the `instruction` becomes optional). The
  child drives the graph instead of running a ReAct loop, with the usual scope
  narrowing, depth/breadth/rate caps, and `async`/`detach` dispositions.

All paths share one driver, so a workflow behaves identically wherever it runs.

---

## The graph model

A workflow is pure topology — a `start` node id and a map of `nodes`:

```json
{
  "start": "fetch",
  "nodes": {
    "fetch":  { "kind": "agent", "instruction": "fetch the next item", "writes": "item",
                "edges": { "ok": "route", "error": "done" } },
    "route":  { "kind": "branch",
                "cases": [ { "when": {"op":"eq","key":"item","pointer":"/status","value":"pending"},
                            "goto": "work" } ],
                "default": "done" },
    "work":   { "kind": "tool", "server": "fs", "tool": "process",
                "args": { "id": { "$from": "item", "pointer": "/id" } },
                "writes": "item", "edges": { "ok": "fetch", "error": "done" } },
    "done":   { "kind": "halt", "status": "completed", "result_from": "item" }
  }
}
```

Every node carries its own out-edges as a `label → target` map (a `branch` uses
per-case gotos instead). A target that points back to an ancestor is a **back-edge**
— cycles are legal by construction (`work → fetch` above is a loop). The resume
point, the blackboard, and the budget are **run state**, never part of the graph —
the authored graph stays deterministic, replayable topology.

### The blackboard

All nodes share a **blackboard**: a string-keyed map of JSON values, threaded
through the run. A node's `writes` key stores its result; `reads` lists fold named
blackboard values into a model call's context; `{"$from": …}` references inject
them into tool args and assign templates. This is how data flows between steps —
one node's output becomes the next node's input.

The blackboard is coordination state, not bulk transport: a single value is capped
at 1 MiB (serialized). An oversized node result is replaced by a small error
marker and takes the node's `error` edge — never an unbounded memory sink.

### `$from` references — explicit data flow

Anywhere in `tool.args` or `assign.value`, an object of the form

```json
{ "$from": "item", "pointer": "/id", "default": 0 }
```

is replaced (at node-execution time) by the blackboard value at `item` +
RFC 6901 pointer `/id`. `pointer` and `default` are optional; a missing path
**with** a `default` resolves to it, a missing path **without** one takes the
node's `error` edge before any tool is called. An unknown extra key in a
reference object is rejected (a typo shield).

Pointers support **computed segments**: `{bbkey}` expands to the stringified
scalar at `blackboard[bbkey]`, so `"pointer": "/items/{index}"` addresses a
loop-carried position dynamically. Expanded string values are RFC-6901-escaped
(a value containing `/` cannot smuggle in extra path levels); a missing or
non-scalar placeholder takes the `error` edge.

### Node kinds

Every node has a `kind`. There are ten:

| Kind | Does | Key fields | Emits |
|---|---|---|---|
| `agent` | Runs a full ReAct turn on `instruction` (with `reads` folded into context, honoring an optional `output_contract`) against the MCP tools. | `instruction`, `reads?`, `writes?`, `output_contract?`, `retry?`, `edges` | `ok` / `error` |
| `tool` | Calls one MCP `tool` on `server` with `args` (with `$from` references resolved). | `server`, `tool`, `args?`, `writes?`, `retry?`, `edges` | `ok` / `error` |
| `assign` | Pure data shaping — resolves a `value` template (with `$from` references) and writes it. No model, no tool. | `value`, `writes`, `edges` | `ok` / `error` |
| `infer` | ONE structured intelligence call: the model answers `prompt` as a JSON object satisfying `schema` (field → type); invalid answers are automatically re-asked with the validation errors, up to `retries` times. | `prompt`, `schema`, `reads?`, `writes?`, `retries?`, `retry?`, `edges` | `ok` / `error` |
| `branch` | Routes on the blackboard (see [Conditions](#conditions)). | `cases`, `default`, `semantic?` | (per-case goto) |
| `foreach` | Fans out over an array (see [Fan-out](#foreach--deterministic-fan-out-over-an-array)): runs `body` once per item on a scoped board, collecting results positionally. | `items`, `body`, `parallel?`, `on_error?`, `writes?`, `edges` | `ok` / `error` |
| `wait` | Suspends until `on_uri` updates or `timeout_ms` elapses, writing the read content. | `on_uri`, `timeout_ms`, `writes?`, `edges` | `updated` / `timeout` |
| `subgraph` | Runs a nested workflow inline (waits included) — or `async: true`: SPAWNS it as a supervised child process and writes `{"handle"}` immediately. | `graph`, `async?`, `writes?`, `edges` | `ok` / `error` |
| `join` | Fans IN: awaits async-subgraph handles (a handle, a `{"handle"}` object, or an array), collecting results positionally. | `handles`, `timeout_ms`, `writes?`, `edges` | `ok` / `error` / `timeout` |
| `halt` | Terminates the workflow with an author-chosen status, projecting a result. | `status`, `result_from?` | — |

A node that emits a label with no matching edge, an unhandled node, or a dangling
edge fails **closed** to a `Crashed` outcome — a mis-authored workflow never runs
away.

### `infer` — checked structured intelligence

`infer` is how a workflow turns free-form intelligence into **checked structured
data** the deterministic branches can route on:

```json
{ "kind": "infer", "prompt": "Classify this ticket.", "reads": ["ticket"],
  "schema": { "category": "string", "urgency": "number", "escalate": "boolean" },
  "writes": "triage", "edges": { "ok": "route", "error": "manual" } }
```

The schema is a minimal field → type map (`string` | `number` | `boolean` |
`array` | `object` | `any`) — a floor, not a ceiling (extra fields are allowed).
An answer missing a field or with a wrong type is re-asked with the exact
validation errors folded in (default 1 re-ask, max 3); exhaustion takes the
`error` edge with the reason. A downstream Tier-1 predicate can then branch on
`triage/urgency` deterministically — no second model call.

### `retry` — in-node fallback for flaky steps

The effectful kinds (`agent`, `tool`, `infer`) accept a retry policy:

```json
{ "kind": "tool", "server": "q", "tool": "push", "args": {},
  "retry": { "max": 2, "backoff_ms": 500 },
  "edges": { "ok": "done", "error": "alert" } }
```

On an error result the node re-runs up to `max` more times (cap 5), sleeping
`backoff_ms` between attempts (cap 60s), before following `error`. Retries happen
**within one node visit**, so the loop/stall guards are not tripped by an
intentionally-identical retry — but every retry charges the step budget, so a
retry storm can never outrun the run's cap. (An authored self-edge is NOT a
retry: re-entering a node with an unchanged blackboard is a stall by design —
use `retry` for "try again", edges for "make progress".)

---

## `foreach` — deterministic fan-out over an array

The map primitive: a tool returns `{"items": [...]}` with hundreds of entries,
and each needs the same processing — **without** feeding the array through the
model (a big array through an `agent` node burns tokens per item and can blow
the context):

```json
{ "kind": "foreach",
  "items": { "$from": "scan", "pointer": "/items" },
  "body": {
    "start": "handle",
    "nodes": {
      "handle": { "kind": "tool", "server": "q", "tool": "process",
                  "args": { "id": { "$from": "item", "pointer": "/id" } },
                  "writes": "out", "edges": { "ok": "done", "error": "failed" } },
      "done":   { "kind": "halt", "status": "completed", "result_from": "out" },
      "failed": { "kind": "halt", "status": "crashed", "result_from": "out" }
    }
  },
  "parallel": 4,
  "on_error": "continue",
  "writes": "results",
  "edges": { "ok": "summarize", "error": "triage" } }
```

- `items` resolves against the blackboard (a `$from` reference or a literal
  array; cap 1024 items). Each iteration runs `body` — a full nested workflow
  (waits included) — on a **scoped** blackboard: a clone of the parent board
  with the reserved keys `item` (the element) and `index` (its position)
  seeded. Body writes never flow back; only each body's halt result does,
  collected **positionally** into `writes`. A failed item's slot carries
  `{"index", "error"}` so downstream consumers keep alignment.
- `on_error: "fail_fast"` (default) stops at the first failing item and takes
  the `error` edge with the partial results; `"continue"` processes everything,
  records per-item markers in place, and takes `ok` — branch on the results
  content (e.g. a `len`/`contains` predicate) to decide what failure means.
- `parallel: N` (cap 8) runs items on N worker **lanes, each with its own
  intelligence + MCP connections** — no client is shared across threads, and
  every lane's model usage still lands on the workflow's shared token pool.
  Default 1 = inline sequential (per-item budget/deadline checks between
  items).
- **Cost model**: every item charges one budget step; a body of pure
  `tool`/`assign`/`branch` nodes makes **zero model calls per item** — the
  whole fan-out is deterministic. Put an `infer`/`agent` node in the body only
  where an item genuinely needs intelligence, and the shared pool still bounds
  the total.

---

## Conditions

A `branch` decides where to go next. Conditions are **two-tier**:

### Tier 1 — deterministic predicates (free)

A `case` fires when its `when` predicate holds over the blackboard; the first
matching case wins, else `default`. Predicates are cheap, total (a missing path is
simply `false`), and never call the model:

```json
{ "op": "eq", "key": "item", "pointer": "/status", "value": "ready" }
```

`key` selects a blackboard entry; `pointer` is an RFC 6901 JSON Pointer into it
(empty = the whole value). Operators:

| Op | Holds when |
|---|---|
| `eq` / `ne` | the value deep-equals / does not equal `value` |
| `lt` / `lte` / `gt` / `gte` | numeric comparison against `value` |
| `in` | the value deep-equals one of `values` |
| `exists` | the path resolves to a present, non-null value |
| `contains` | a string contains the substring / an array contains the element |
| `starts_with` / `ends_with` | string prefix / suffix |
| `len` | the length of a string/array/object is within `[min, max]` |
| `all` / `any` / `not` | composition |

```json
{ "op": "all", "preds": [
  { "op": "gte", "key": "triage", "pointer": "/urgency", "value": 8 },
  { "op": "in",  "key": "triage", "pointer": "/category", "values": ["ops", "security"] }
] }
```

**Cross-key comparison**: the comparison `value` of `eq`/`ne`/`lt`/`lte`/`gt`/
`gte`/`contains` (and elements of `in`) may itself be a `{"$from": key,
"pointer": "/p"}` reference — branch on one blackboard value against another
(`"is the retry count below the configured limit?"`) with no model call. An
unresolvable reference makes the predicate `false` (fail-closed, even for
`ne`).

A predicate that can never hold (an empty `in` set, inverted `len` bounds) is
rejected at define time, not silently routed around.

### CEL expressions (`--features cel`)

A build with the `cel` feature adds [CEL](https://cel.dev) — the expression
language Kubernetes admission policies and Envoy use — wherever the structural
ops run out (arithmetic, string functions, collection macros). CEL is
non-Turing-complete, does no I/O, and always terminates, which makes it the one
form of "code" a model can safely author and agentd can immediately execute.
Three surfaces:

- **Branch predicates** — `{"op": "cel", "expr": "..."}` (composable with
  `all`/`any`/`not`); every blackboard key is a top-level identifier:
  ```json
  { "op": "cel", "expr": "results.filter(r, !has(r.error)).size() >= results.size() * 9 / 10" }
  ```
  Must return a bool; a non-bool, an eval error, or an unresolvable reference
  is `false` (fail-closed).
- **Computed `assign`** — `"expr"` instead of `"value"`: filter, map,
  aggregate, and assemble deterministically, with zero model tokens:
  ```json
  { "kind": "assign", "expr": "scan.items.filter(i, i.ok).map(i, i.id)", "writes": "ids",
    "edges": { "ok": "fan" } }
  ```
- **`infer` value constraints** — `"check"` runs over the (schema-valid)
  answer's fields; a type-correct but out-of-bounds answer is re-asked with the
  constraint named:
  ```json
  { "kind": "infer", "prompt": "score it", "schema": { "score": "number" },
    "check": "score >= 0.0 && score <= 1.0", "writes": "s", "edges": { "ok": "next", "error": "manual" } }
  ```

Reactive subscriptions get the same power: a wake condition may be
`{"op": "cel", "expr": "content.items.exists(i, i.urgent)"}` (the resource
content — or the value at the condition's `pointer` — is `content`), so a
daemon wakes only for the states it actually cares about.

Every expression is compile-checked at define/parse time (length-capped at
4 KiB), and a build **without** the feature rejects CEL right there with a
clear message — never a silent mis-evaluation. JSON numbers are normalized to
CEL ints/floats so `count + 1 > limit` behaves the way it reads. This is the
one gated exception to the zero-dependency default build; `--features cel` is
opt-in precisely so the moat holds everywhere else.

### Tier 2 — a semantic branch (opt-in)

When the deterministic cases all miss and a `branch` carries a `semantic` spec,
agentd runs **one** tool-less model call to pick a labelled choice — a routing
decision the predicates can't express ("is this document acceptable?"):

```json
{ "kind": "branch", "cases": [], "default": "reject",
  "semantic": { "prompt": "Is the draft acceptable?", "reads": ["draft"],
                "choices": { "approve": "publish", "revise": "rewrite" } } }
```

The model is asked to answer with one label (exact match first, else the longest
contained label — so overlapping labels resolve to the specific one); an
unrecognized answer falls through to `default`. On a build with no reachable
intelligence, a semantic branch degrades safely to its `default`. Prefer an
`infer` node + Tier-1 predicates when the decision can be made structural — one
extraction can feed many cheap branches.

---

## Waits

A `wait` node pauses the workflow on an external dependency — a job finishing, a
flag flipping — without burning a thread:

```json
{ "kind": "wait", "on_uri": "file:///inbox.json", "timeout_ms": 30000,
  "writes": "event", "edges": { "updated": "handle", "timeout": "giveup" } }
```

agentd subscribes to `on_uri`, blocks until the resource updates (then reads its
current content, notify-then-read) or the timeout elapses, and resumes on the
`updated` or `timeout` edge. A back-edge into a `wait` is a long-lived reactive
loop that costs nothing while idle. The suspended run state is serializable, so a
long wait survives across a process boundary. Waits work inside `subgraph`s too.

> **Scope.** All current paths resolve waits **in-process** (they block until the
> wait resolves, inside the supervised child). A fully asynchronous, non-blocking
> reactive-daemon workflow is a roadmap item.

---

## Async subgraphs + `join` — parallel phases as supervised children

`subgraph { async: true }` spawns the nested workflow as a **child process**
through the same machinery `subagent.spawn` uses — the depth, breadth, and
spawn-rate caps all apply — and writes `{"handle": …}` immediately. A later
`join` collects:

```json
{ "start": "s1",
  "nodes": {
    "s1":     { "kind": "subgraph", "async": true, "graph": { "…": "phase A" },
                "writes": "h1", "edges": { "ok": "s2", "error": "fail" } },
    "s2":     { "kind": "subgraph", "async": true, "graph": { "…": "phase B" },
                "writes": "h2", "edges": { "ok": "gather", "error": "fail" } },
    "gather": { "kind": "assign", "value": [{ "$from": "h1" }, { "$from": "h2" }],
                "writes": "hs", "edges": { "ok": "join" } },
    "join":   { "kind": "join", "handles": { "$from": "hs" }, "timeout_ms": 60000,
                "writes": "results", "edges": { "ok": "done", "error": "triage", "timeout": "late" } },
    "…":      {}
  } }
```

Both phases run **concurrently** while the parent workflow proceeds to the
join. Results collect positionally (a failed child's slot carries
`{"handle", "error"}`); stragglers at the timeout take the `timeout` edge with
the partials written — they keep running and may be joined again. An async
subgraph starts with an EMPTY blackboard (data flows OUT via its halt result,
not in); use `foreach` when items must flow into parallel work.

---

## The reactive-daemon workflow (`--mode reactive --workflow`)

A long-lived workflow whose `wait` nodes hold **no process at all**:

```bash
agentd --mode reactive --workflow ./pipeline.json   --intelligence https://gw.example/v1 --mcp inbox=https://mcp-inbox.internal/mcp
```

The daemon drives the workflow in a supervised child; when it reaches a `wait`,
the child **suspends** — it exits, serializing the run slice (cursor +
blackboard + budget) into its result — and the DAEMON arms the subscription and
the timeout clock. On the resource update (or the timeout) a fresh child
resumes on the `updated`/`timeout` edge, budget continuing where it left off.
No `--subscribe` or `--instruction` is needed: the workflow's waits are the
subscriptions and its nodes are the work.

The daemon's lifetime is the workflow's: a terminal workflow exits with its
projected code, while an event-loop workflow (a back-edge into a `wait`) runs
indefinitely — idling between events with zero child processes alive. The live
state is observable at the Management-only **`agent://workflow`** resource:
`driving`, `suspended` (with the watched uri and spent budget), or `terminal`.

---

## Termination, budgets, and reasons

Cyclic workflows need to stop. The guards, each with a distinct status **and a
recorded `reason`** (which guard tripped, at which node):

1. **Step budget** — a total node-visit cap → `Exhausted`.
2. **Token pool** — one intelligence-token budget for the WHOLE workflow (every
   `agent` turn, `infer` ask, and semantic judgement draws from it) → `Exhausted`.
   N model-calling nodes share one pool; they never multiply a per-node grant.
3. **Wall-clock deadline** — the whole workflow is bounded by the run's
   `--deadline` (checked on every node entry) → `Exhausted`.
4. **Per-node visit cap** — a node visited more than 100 times is a runaway cycle
   → `LoopDetected` (a `wait` is exempt — it suspends, it does not spin).
5. **Progress guard** — re-entering a node with an unchanged blackboard means the
   cycle made no progress → `Stalled`.
6. **Author-time validation** — before it ever runs, a workflow must have a
   `start` that exists, no dangling edge, at least one `halt` reachable from
   `start` (no-exit is rejected), every `wait` with a non-empty uri and non-zero
   timeout, retry/infer caps within bounds, satisfiable predicates, and
   node/edge/key/nesting counts within limits.

The engine statuses are distinct from a node's `halt` status (which is one of the
usual terminal statuses — `completed`, `refused`, …). Reaching a `halt` with
`completed` is `Completed`; any other author status is `Halted`. Under
`--mode workflow` the child projects the status onto the exit table:
`Completed` → 0, `Halted` → its terminal code, `Exhausted` → 7 (deadline/tokens/
steps distinguished by the `reason`), `LoopDetected` / `Stalled` → 3,
`Crashed` → 1. The result body always carries
`{workflow_status, reason, steps, tokens, result}` so the operator sees *why* and
*at what cost*, not just the code.

---

## Patching a workflow (additive)

`workflow.patch` lets an agent elaborate its own plan at runtime — add nodes and
edges to a stored workflow as it learns more, without redefining the whole thing:

```json
{ "workflow_id": "w1",
  "patch": { "add_nodes": { "verify": { "kind": "agent", "instruction": "double-check",
                                         "edges": { "ok": "done" } } },
             "add_edges": [ { "from": "work", "label": "error", "to": "verify" } ] } }
```

Patches are **additive only** — never overwrite a node or retarget an existing edge
— so a patch can't strip reachability or a termination guarantee out from under a
run. The grown workflow is re-validated; a rejected patch leaves the stored one
untouched.

---

## A worked example: structured triage with a review loop

Extract structured data once, branch on it deterministically, loop a draft until
a judge approves — bounded by the budget, the token pool, and the deadline:

```json
{
  "start": "classify",
  "nodes": {
    "classify": { "kind": "infer", "prompt": "Classify the ticket.", "reads": ["ticket"],
                  "schema": { "category": "string", "urgency": "number" },
                  "writes": "triage", "edges": { "ok": "route", "error": "manual" } },
    "route":    { "kind": "branch",
                  "cases": [ { "when": { "op": "gte", "key": "triage", "pointer": "/urgency", "value": 8 },
                               "goto": "page" } ],
                  "default": "draft" },
    "page":     { "kind": "tool", "server": "pager", "tool": "page",
                  "args": { "category": { "$from": "triage", "pointer": "/category" } },
                  "retry": { "max": 2, "backoff_ms": 1000 },
                  "writes": "paged", "edges": { "ok": "done", "error": "manual" } },
    "draft":    { "kind": "agent", "instruction": "draft a response", "reads": ["ticket", "triage"],
                  "writes": "draft", "edges": { "ok": "judge", "error": "manual" } },
    "judge":    { "kind": "branch", "cases": [], "default": "revise",
                  "semantic": { "prompt": "Is the response ready to send?", "reads": ["draft"],
                                "choices": { "yes": "done", "no": "revise" } } },
    "revise":   { "kind": "agent", "instruction": "revise the response", "reads": ["draft"],
                  "writes": "draft", "edges": { "ok": "judge", "error": "manual" } },
    "done":     { "kind": "halt", "status": "completed", "result_from": "draft" },
    "manual":   { "kind": "halt", "status": "refused" }
  }
}
```

The `revise → judge` back-edge is the loop; the semantic branch decides when to
exit; the visit cap + progress guard stop a draft that never converges; the
`infer` output feeds a **free** deterministic branch.

---

## See also

- [modes-and-triggers.md](modes-and-triggers.md) — the four base modes and reactive
  routing (a workflow is the explicit form of the same event→action machinery).
- [subagents.md](subagents.md) — the `agent` node runs a subagent turn;
  `subagent.spawn{workflow}` / `workflow.run{detach}` delegate whole workflows to
  children; the spawn caps still apply.
- [configuration.md](configuration.md) — `--workflow` / `--mode workflow` and the run
  limits a workflow inherits (`--max-steps`, `--max-tokens` = the shared token
  pool, `--deadline` = the whole-workflow wall clock).
