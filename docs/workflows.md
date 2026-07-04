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

## What a workflow can do — the capability map

| Capability | Mechanism | Where |
|---|---|---|
| Mix intelligence and determinism per step | twelve node kinds: `agent` (a full reasoning turn), `infer` (one schema-checked structured ask), `tool`/`assign` (zero model tokens) | [Node kinds](#node-kinds) |
| Route on data — or on judgement | `branch`: deterministic predicates (free), CEL expressions, one opt-in semantic tier | [Conditions](#conditions) |
| Accumulate instead of overwrite | `writes_mode` reducers: `append` / `merge` / `union` | [Reducers](#writes_mode--reducers-rfc-0021-5) |
| Process an array without feeding it through the LLM | `foreach`: one body × N items, up to 8 parallel lanes | [Fan-out](#foreach--deterministic-fan-out-over-an-array) |
| Run *different* things at once, then continue | `parallel`: N named bodies, one result object, the same lane pool | [Parallel](#parallel--heterogeneous-branches) |
| Run phases as isolated processes | `subgraph {async}` + `join`: supervised child workflows, fan-in later | [Async subgraphs](#async-subgraphs--join--parallel-phases-as-supervised-children) |
| Wait for the world | `wait`: suspend on an MCP resource update, at zero idle cost | [Waits](#waits) |
| **Ask a human** — over A2A | `human`: the task projects `input-required`; the reply is a spec-native `SendMessage` | [Human gates](#human-gates--a2a-input-required) |
| Survive crashes; fork; time-travel | the **MCP checkpointer**: per-superstep durable state, `--workflow-resume` | [Durable state](#durable-state--the-mcp-checkpointer-rfc-0021-8) |
| Loop safely | layered termination: step budget, shared token pool, wall deadline, visit caps, progress guard — each with a typed `reason` | [Termination](#termination-budgets-and-reasons) |
| Grow the plan mid-run | `workflow.patch`: additive-only self-modification | [Patching](#patching-a-workflow-additive) |
| Stay supervised | the driver runs in a killable child; the supervisor owns the kill ladder, cgroups, drain, and the exit-code contract | [Termination](#termination-budgets-and-reasons) |

Everything below is the same graph language everywhere: what the model authors
via `workflow.define` is exactly what an operator pins with `--workflow` — one
dialect, advertised as `surfaces.workflow.dialect` in the capabilities manifest.

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

Every node has a `kind`. There are twelve (dialect 2, RFC 0021):

| Kind | Does | Key fields | Emits |
|---|---|---|---|
| `agent` | Runs a full ReAct turn on `instruction` (with `reads` folded into context, honoring an optional `output_contract`) against the MCP tools. | `instruction`, `reads?`, `writes?`, `writes_mode?`, `output_contract?`, `retry?`, `edges` | `ok` / `error` |
| `tool` | Calls one MCP `tool` on `server` with `args` (with `$from` references resolved). | `server`, `tool`, `args?`, `writes?`, `writes_mode?`, `retry?`, `edges` | `ok` / `error` |
| `assign` | Pure data shaping — resolves a `value` template (with `$from` references) and writes it. No model, no tool. | `value`, `writes`, `writes_mode?`, `edges` | `ok` / `error` |
| `infer` | ONE structured intelligence call: the model answers `prompt` as a JSON object satisfying `schema` (field → type); invalid answers are automatically re-asked with the validation errors, up to `retries` times. | `prompt`, `schema`, `reads?`, `writes?`, `writes_mode?`, `retries?`, `retry?`, `edges` | `ok` / `error` |
| `branch` | Routes on the blackboard (see [Conditions](#conditions)). | `cases`, `default`, `semantic?` | (per-case goto) |
| `foreach` | Fans out over an array (see [Fan-out](#foreach--deterministic-fan-out-over-an-array)): runs `body` once per item on a scoped board, collecting results positionally. | `items`, `body`, `parallel?`, `on_error?`, `writes?`, `writes_mode?`, `edges` | `ok` / `error` |
| `parallel` | Fans out over NAMED heterogeneous branches (see [Parallel](#parallel--heterogeneous-branches)): each branch body runs concurrently on a scoped board; results collect into ONE OBJECT keyed by branch name. | `branches`, `on_error?`, `writes?`, `writes_mode?`, `edges` | `ok` / `error` |
| `wait` | Suspends until `on_uri` updates or `timeout_ms` elapses, writing the read content. | `on_uri`, `timeout_ms`, `writes?`, `writes_mode?`, `edges` | `updated` / `timeout` |
| `human` | A HUMAN GATE (see [Human gates](#human-gates--a2a-input-required)): publishes `payload`, flips the served A2A task to `input-required`, and suspends until an A2A reply / `reply_uri` update / timeout. | `payload`, `reply_uri?`, `timeout_ms`, `writes?`, `writes_mode?`, `edges` | `replied` / `timeout` / `error` |
| `subgraph` | Runs a nested workflow inline (waits included) — or `async: true`: SPAWNS it as a supervised child process and writes `{"handle"}` immediately. | `graph`, `async?`, `writes?`, `writes_mode?`, `edges` | `ok` / `error` |
| `join` | Fans IN: awaits async-subgraph handles (a handle, a `{"handle"}` object, or an array), collecting results positionally. | `handles`, `timeout_ms`, `writes?`, `writes_mode?`, `edges` | `ok` / `error` / `timeout` |
| `halt` | Terminates the workflow with an author-chosen status, projecting a result. | `status`, `result_from?` | — |

A node that emits a label with no matching edge, an unhandled node, or a dangling
edge fails **closed** to a `Crashed` outcome — a mis-authored workflow never runs
away. **Unknown node fields are define-time errors** (RFC 0021 §4): a typo'd
`writes_mode` is refused, never silently ignored. The manifest advertises the
graph language as `surfaces.workflow.dialect` (currently `2`) — feature-detect
from it, not the version string.

### `writes_mode` — reducers (RFC 0021 §5)

By default a node's `writes` **overwrites** its key. `writes_mode` folds instead:

| Mode | Semantics | Type mismatch |
|---|---|---|
| `overwrite` *(default)* | replace | never errors |
| `append` | absent → `[v]`; array → push | `error` edge |
| `merge` | absent → `v`; object+object → shallow merge, incoming wins | `error` edge |
| `union` | as `append`, skipping a deep-equal duplicate | `error` edge |

```json
{ "kind": "agent", "instruction": "find one more issue", "writes": "issues",
  "writes_mode": "append", "edges": { "ok": "route", "error": "fail" } }
```

Reducers are pure (no model, no tool); the reduce happens **before** the 1 MiB
clamp (the accumulated value is what must fit); a mismatch writes a readable
error marker and takes the `error` edge — never a silent coercion. CEL
`assign.expr` remains the escape hatch for custom folds.

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

## `parallel` — heterogeneous branches

Where `foreach` maps **one body over N items**, `parallel` runs **N different
bodies at once** — "run the security review AND the perf review, then continue"
(RFC 0021 §6):

```json
{ "kind": "parallel",
  "branches": {
    "security": { "start": "s0", "nodes": { "…": "a full sub-graph" } },
    "perf":     { "start": "p0", "nodes": { "…": "a different sub-graph" } }
  },
  "on_error": "continue",
  "writes": "reviews",
  "edges": { "ok": "synthesize", "error": "fail" } }
```

- Each branch runs on a **scoped board** (a clone of the parent's, with
  `branch` = its name seeded); branch writes never flow back — the collected
  result does: **one object keyed by branch name** (a failed branch's slot
  carries `{"branch","error"}`).
- **Bounds**: ≤ 16 branches; concurrency rides the SAME 8-lane pool `foreach`
  uses — one pool, so composing `parallel` inside `foreach` (or vice versa)
  never multiplies lanes. Every branch pre-charges a budget step; all branches
  draw the one shared token pool.
- `on_error`: `fail_fast` (default — any failed branch → the `error` edge) or
  `continue` (`ok` iff at least one branch succeeded; markers stay in place).
- `halt` inside a branch halts the **branch**, not the run.

---

## Human gates — A2A `input-required`

A `human` node is the **human-in-the-loop primitive** (RFC 0021 §7): publish
something for a person (or any A2A peer) to inspect, suspend, and resume on
their reply — **A2A is the conversation channel**.

```json
{ "kind": "human",
  "payload": { "question": "Ship it?", "diff": { "$from": "patch" } },
  "reply_uri": "approvals://deploy-42",
  "timeout_ms": 86400000,
  "writes": "verdict",
  "edges": { "replied": "route_on_verdict", "timeout": "escalate" } }
```

What happens, in order:

1. The resolved `payload` travels up to the supervisor; when the run is a
   **served A2A task**, the task transitions to **`TASK_STATE_INPUT_REQUIRED`**
   with the payload as its status message — a spec-conformant A2A client
   (a human's UI, another agent) *sees* the wait via `GetTask`/`SubscribeToTask`.
2. The workflow suspends. Three resume paths race, **first one wins**:
   - **an A2A `SendMessage` carrying this task's `taskId`** — its text parts
     become the reply (the spec-native human answer);
   - an update on `reply_uri` (any MCP resource — the notify-then-read read
     is the reply);
   - the `timeout_ms` expiry (nothing written; the `timeout` edge).
3. The reply lands on `writes` (through `writes_mode`) and the node takes
   `replied`. The task returns to `working`.

### The conversation on the wire

What a human's UI (or any conformant A2A client) actually sees, end to end.
Dispatch the work and note the task id:

```jsonc
// → SendMessage {"message":{"parts":[{"text":"run the gated deploy"}]},
//                "configuration":{"returnImmediately":true}}
// ← {"task":{"id":"a3","contextId":"ctx-a3","status":{"state":"TASK_STATE_WORKING", …}}}
```

Poll (`GetTask {"id":"a3"}`) or stream (`SubscribeToTask`). When the workflow
reaches its `human` node, the task is **visibly waiting** — and the question is
IN the task:

```jsonc
// ← {"id":"a3", "status":{
//      "state": "TASK_STATE_INPUT_REQUIRED",
//      "message": {"role":"agent","parts":[{"text":"{\"question\":\"Ship it?\",\"diff\":\"+1 -0\"}"}]},
//      "timestamp": "…"}}
```

The human answers with a plain `SendMessage` that **continues the task by id**
— no agentd-specific API, just the A2A spec's multi-turn shape:

```jsonc
// → SendMessage {"message":{"taskId":"a3","parts":[{"text":"yes"}]}}
// ← {"task":{"id":"a3", …}}          // the reply is accepted; the run resumes
```

The reply text lands on the gate's `writes` key (parsed as JSON when it *is*
JSON — reply `{"approve":true,"reason":"lgtm"}` and branch on `/approve`), the
workflow takes `replied`, and the next `GetTask` shows `TASK_STATE_WORKING`,
then the terminal state with the distillate artifact.

The gate deliberately does **not** encode approve/reject — the reply is data,
and routing on it is a `branch` (predicates or CEL on the verdict), so
multi-approver schemes and rejection reasons stay authorable. Notes:

- A reply while another is pending is refused (`-32004 UnsupportedOperation`);
  an unknown `taskId` is `-32001 TaskNotFound`; a message to a live task with
  **no open gate** is `-32004` (agentd runs are single-instruction — the gate
  reply is the one supported mid-task continuation).
- Without `--serve-mcp` the gate degrades to a plain wait on
  `reply_uri`/timeout — never a hard serving requirement.
- In the **reactive-daemon** shape the gate suspends the daemon's workflow like
  a `wait` (the payload appears on `agent://workflow`); it resolves by
  `reply_uri`/timeout — the A2A reply path serves **served async tasks**.
- An unresolvable `$from` in `payload` emits `error` (route it or fail closed).

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

> **Cluster compatibility.** A reactive workflow daemon is a single-instance
> shape: its wait uris are its *own dependencies*, not a partitioned work
> stream — so `--shard N>1`, `--standby`, and `--assign-from` are rejected at
> startup when combined with it (the shard filter would silently drop the
> workflow's own wait updates). `--subscribe` routes may ride the same daemon
> (they then require the usual `--instruction`), but don't point a `--claim`
> route at a uri the workflow also waits on — a wait resolving consumes that
> delivery before the claim gate.

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

## Durable state — the MCP checkpointer (RFC 0021 §8)

A workflow can persist its run slice **after every superstep** — crash-resume,
state history, and fork/time-travel — with **zero new dependencies**: the
checkpointer is *any MCP server* implementing a three-tool profile. Declare the
policy at the graph root:

```json
{ "checkpoint": { "server": "state", "key": "run/{run_id}", "every": 1,
                  "on_error": "continue" },
  "start": "…", "nodes": { "…": "…" } }
```

- `server` names a configured `--mcp` server; `key` is the state lineage
  (`{run_id}` interpolates — a **stable operator-chosen key** makes the run
  resumable across pod replacements); `every` gates the periodic writes
  (a suspension and a `halt` **always** checkpoint).
- The **envelope** is versioned and self-describing:
  `{v:1, seq, workflow_hash, state, ts_ms}` — `seq` is the superstep count
  (monotonic, carried across resume), `workflow_hash` is the SHA-256 of the
  canonical graph JSON (resume verifies it), and `state` is the same serialized
  run slice a `wait` suspension produces (cursor, blackboard, budget, visit
  counts). Its cursor is the next **unexecuted** node — resume is exactly-once
  for checkpointed nodes, at-least-once for the one in flight.
- **The server contract** (any language, any store): `state.put {key,seq,state}`
  (MUST refuse `seq <=` latest with `{ok:false,latest}` — the split-brain
  guard; a refused put is ALWAYS fatal for the run), `state.get {key[,seq]}`,
  `state.list {key}`. Postgres, S3, sqlite, etcd — all are somebody's MCP
  server; agentd links none of them.
- `on_error`: `continue` (default — a failed write degrades durability, never
  the run; `workflow.checkpoint.fail` telemetry records it) or `halt`.

**Resume / fork:**

```console
$ agentd --mode workflow --workflow pipeline.json \
    --mcp state=https://ckpt.internal/mcp \
    --workflow-resume state:run/abc            # latest — the crash-recovery flow
$ agentd … --workflow-resume state:run/abc@17  # a specific seq, under a NEW
                                               # --run-id = a FORK (time-travel)
```

The child fetches the envelope after connecting, **verifies the workflow
hash** (a mismatch is a refusal, exit `5` — the state was not taken from this
graph; `--workflow-resume-force` overrides for deliberate
graph-edit-and-continue, resetting the loop guards but keeping board and
budget), and drives on. **Budgets carry over**: a resumed run does not get a
fresh token pool — the budget is a property of the work, not the process.
agentd never resumes implicitly: a `Job` with `restartPolicy: OnFailure` opts
in by passing `--workflow-resume` with the stable key.

### Crash recovery, mechanically

A checkpoint's cursor is the next **unexecuted** node, so semantics after a
hard kill (OOM, node loss, `kill -9`) are exactly what you want: every
completed node is **exactly-once**; the one that was in flight when the
process died is **at-least-once** (it re-runs — pair it with idempotent tools
/ the `agent/run_id` dedup meta, RFC 0011 §7). A Kubernetes `Job` that
survives pod replacement:

```yaml
spec:
  backoffLimit: 3
  template:
    spec:
      restartPolicy: OnFailure
      containers:
        - name: agent
          image: ghcr.io/agentd-dev/agentd:latest
          args:
            - --mode=workflow
            - --workflow=/etc/agent/pipeline.json     # checkpoint.key: "job/nightly-2026-07-04"
            - --mcp=state=https://ckpt.internal/mcp
            - --workflow-resume=state:job/nightly-2026-07-04   # see the subtlety below
```

One subtlety: `--workflow-resume` of a key that does not exist yet is a
refusal (resuming *nothing* is a config error), so attempt 1 must run without
the flag — an init step that checks `state.list` (or a wrapper that drops the
flag when the key is empty) picks the variant. The explicitness is deliberate:
agentd never silently resumes state you didn't name.

### Fork and time-travel

History is immutable and `@seq`-addressed; a **fork** is a resume from any
recorded superstep under a **new run id** (and therefore a new checkpoint
lineage — the original history is never rewritten):

```console
$ agentd … --workflow-resume state:run/abc@12 --run-id run-abc-fork1
```

Want to fork with an **edited blackboard** (the "what if the review had said
no?" experiment)? The envelope is plain JSON behind a plain MCP server — fetch
it with any MCP client, edit `state.blackboard`, `state.put` it under a new
key, and resume from that. Time-travel needs no agentd surface at all; it
falls out of state-behind-MCP.

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

## A worked example: the gated release pipeline (everything composed)

The dialect-2 surface in one graph — review a change **three ways at once**
(`parallel`), fold the verdicts into one object (`merge` reducers inside the
branches, one object out), **ask a human over A2A** with the full evidence
(`human`), branch on the answer, and survive a mid-pipeline crash
(`checkpoint`):

```json
{
  "dialect": 2,
  "checkpoint": { "server": "state", "key": "release/{run_id}" },
  "start": "reviews",
  "nodes": {
    "reviews": { "kind": "parallel",
      "branches": {
        "security": { "start": "s", "nodes": {
          "s": { "kind": "agent", "instruction": "security-review the change", "reads": ["change"],
                 "writes": "r", "edges": { "ok": "h", "error": "hf" } },
          "h": { "kind": "halt", "status": "completed", "result_from": "r" },
          "hf": { "kind": "halt", "status": "crashed", "result_from": "r" } } },
        "perf": { "start": "p", "nodes": {
          "p": { "kind": "infer", "prompt": "Estimate the perf impact.", "reads": ["change"],
                 "schema": { "risk": "string", "p99_delta_ms": "number" },
                 "writes": "r", "edges": { "ok": "h", "error": "hf" } },
          "h": { "kind": "halt", "status": "completed", "result_from": "r" },
          "hf": { "kind": "halt", "status": "crashed", "result_from": "r" } } },
        "tests": { "start": "t", "nodes": {
          "t": { "kind": "tool", "server": "ci", "tool": "run_suite",
                 "args": { "ref": { "$from": "change", "pointer": "/ref" } },
                 "retry": { "max": 2, "backoff_ms": 5000 },
                 "writes": "r", "edges": { "ok": "h", "error": "hf" } },
          "h": { "kind": "halt", "status": "completed", "result_from": "r" },
          "hf": { "kind": "halt", "status": "crashed", "result_from": "r" } } }
      },
      "on_error": "continue",
      "writes": "evidence", "edges": { "ok": "gate", "error": "gate" } },

    "gate": { "kind": "human",
      "payload": { "question": "Ship it?", "evidence": { "$from": "evidence" } },
      "timeout_ms": 86400000,
      "writes": "verdict",
      "edges": { "replied": "route", "timeout": "abort" } },

    "route": { "kind": "branch",
      "cases": [ { "when": { "op": "eq", "key": "verdict", "pointer": "/approve", "value": true },
                   "goto": "ship" } ],
      "default": "abort" },

    "ship":  { "kind": "tool", "server": "deploy", "tool": "rollout",
               "args": { "ref": { "$from": "change", "pointer": "/ref" } },
               "writes": "rollout", "edges": { "ok": "done", "error": "abort" } },
    "done":  { "kind": "halt", "status": "completed", "result_from": "rollout" },
    "abort": { "kind": "halt", "status": "refused", "result_from": "evidence" }
  }
}
```

What each capability buys here:

- The three reviews run **concurrently on the lane pool** — an agent turn, a
  structured `infer`, and a plain CI tool call, each in the shape it deserves;
  `on_error: continue` means one failed review does not blind the human — its
  error marker lands in `evidence` alongside the others.
- The human sees **everything** (`evidence` rides the A2A task's status
  message), replies `{"approve": true}` from any A2A client, and the
  deterministic branch routes on `/approve` — free, no model call.
- Durability: the fan-out completes as one superstep (no mid-lane checkpoints
  — a deliberate v1 simplification), so a pod lost during the reviews re-runs
  them; a pod lost **after** them resumes at `gate` with the evidence intact —
  the human is never re-asked for already-gathered facts, and a crash while
  *waiting on the human* resumes the wait (suspensions always checkpoint).
- The whole run stays inside the step budget / token pool / deadline, and the
  operator reads its live face on `agent://workflow` and the A2A task states.

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
