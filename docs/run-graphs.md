# Run-graphs

A **run-graph** lets an agent process work as an explicit graph of steps — with
branches, loops, and waits — instead of one flat ReAct loop. agentd already *is*
an implicit single-node graph executor (the loop is a hard-coded cycle, the
reactive router is an event→action edge set); a run-graph reifies that into an
explicit graph the agent (or an operator) authors and agentd drives. Think
LangGraph, but the agent builds and runs the graph **by itself**, over the same
subagents, MCP tools, and structured data it already uses.

> **Feature-gated, opt-in.** Run-graphs compile only under `--features run-graph`
> (default **off** — an agentd built without it is byte-for-byte unchanged). It is
> dependency-free (serde + `serde_json` only). The degenerate single-`agent`-node
> graph reproduces today's one-shot behavior, so the graph is a superset, never a
> replacement.

---

## The two ways to run a graph

A graph is the same serde JSON object either way:

- **Operator-pinned** — run a graph from a file to completion, then exit:
  ```bash
  agentd --mode graph --graph ./pipeline.json --intelligence https://gw.example/v1
  ```
  No `--instruction` is needed (the nodes carry the work), but intelligence is
  still required for `agent` nodes. The projected result prints to stdout and the
  graph status maps onto the normal exit table (see [Termination](#termination)).

- **Agent-authored** — the agent defines and runs a graph itself, mid-reasoning,
  via three self-tools (a root agent only):
  - `graph.define{graph}` — validate + store a graph, returns a `graph_id`.
  - `graph.run{graph_id}` — drive the stored graph to completion, returns its
    status + result as the tool result.
  - `graph.patch{graph_id, patch}` — grow a stored graph **additively** (see
    [Patching](#patching-a-graph-additive)).

  This is the "agent orchestrates by itself" path: the agent decides the shape of
  the work, writes the graph, and runs it — no operator or per-step hand-holding.

Both paths share one driver, so a graph behaves identically whichever way it runs.

---

## The graph model

A graph is pure topology — a `start` node id and a map of `nodes`:

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
    "work":   { "kind": "tool", "server": "fs", "tool": "process", "args": {"id": 1},
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
through the run. A node's `writes` key stores its result; an `agent` node's
`reads` list folds named blackboard values into its context. This is how data
flows between steps — one node's output becomes the next node's input.

### Node kinds

Every node has a `kind`. There are six:

| Kind | Does | Key fields | Emits |
|---|---|---|---|
| `agent` | Runs a full ReAct turn on `instruction` (with `reads` folded into context, honoring an optional `output_contract`) against the MCP tools. | `instruction`, `reads?`, `writes?`, `output_contract?`, `edges` | `ok` / `error` |
| `tool` | Calls one MCP `tool` on `server` with `args`. | `server`, `tool`, `args?`, `writes?`, `edges` | `ok` / `error` |
| `branch` | Routes on the blackboard (see [Conditions](#conditions)). | `cases`, `default`, `semantic?` | (per-case goto) |
| `wait` | Suspends until `on_uri` updates or `timeout_ms` elapses, writing the read content. | `on_uri`, `timeout_ms`, `writes?`, `edges` | `updated` / `timeout` |
| `subgraph` | Runs a nested `graph` inline; writes its result. | `graph`, `async?`, `writes?`, `edges` | `ok` / `error` |
| `halt` | Terminates the graph with an author-chosen status, projecting a result. | `status`, `result_from?` | — |

A node that emits a label with no matching edge, an unhandled node, or a dangling
edge fails **closed** to a `Crashed` outcome — a mis-authored graph never runs away.

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
(empty = the whole value). Operators: `eq`, `ne`, `lt`, `gt`, `exists`, `contains`,
plus `all` / `any` / `not` to compose:

```json
{ "op": "all", "preds": [
  { "op": "gt", "key": "item", "pointer": "/score", "value": 0.8 },
  { "op": "exists", "key": "item", "pointer": "/reviewed" }
] }
```

### Tier 2 — a semantic branch (opt-in)

When the deterministic cases all miss and a `branch` carries a `semantic` spec,
agentd runs **one** tool-less model call to pick a labelled choice — a routing
decision the predicates can't express ("is this document acceptable?"):

```json
{ "kind": "branch", "cases": [], "default": "reject",
  "semantic": { "prompt": "Is the draft acceptable?", "reads": ["draft"],
                "choices": { "approve": "publish", "revise": "rewrite" } } }
```

The model is asked to answer with one label; an unrecognized answer falls through
to `default`. On a build with no reachable intelligence, a semantic branch degrades
safely to its `default`.

---

## Waits

A `wait` node pauses the graph on an external dependency — a job finishing, a flag
flipping — without burning a thread:

```json
{ "kind": "wait", "on_uri": "file:///inbox.json", "timeout_ms": 30000,
  "writes": "event", "edges": { "updated": "handle", "timeout": "giveup" } }
```

agentd subscribes to `on_uri`, blocks until the resource updates (then reads its
current content, notify-then-read) or the timeout elapses, and resumes on the
`updated` or `timeout` edge. A back-edge into a `wait` is a long-lived reactive
loop that costs nothing while idle. The suspended run state is serializable, so a
long wait survives across a process boundary.

> **Scope.** `--mode graph` and `graph.run` resolve waits **in-process** (they
> block until the wait resolves). A fully asynchronous, non-blocking reactive-daemon
> graph is a roadmap item.

---

## Termination

Cyclic graphs need to stop. Four layers guarantee it, and each maps to a distinct
graph status:

1. **Budget** — a total node-visit cap → `Exhausted`.
2. **Per-node visit cap** — a node visited too many times is a runaway cycle →
   `LoopDetected` (a `wait` is exempt — it suspends, it does not spin).
3. **Progress guard** — re-entering a node with an unchanged blackboard means the
   cycle made no progress → `Stalled`.
4. **Author-time validation** — before it ever runs, a graph must have a `start`
   that exists, no dangling edge, at least one `halt` reachable from `start`
   (no-exit is rejected), every `wait` with a non-empty uri and non-zero timeout,
   and node/edge/key/nesting counts within bounds.

The engine statuses are distinct from a node's `halt` status (which is one of the
usual terminal statuses — `completed`, `refused`, …). Reaching a `halt` with
`completed` is `Completed`; any other author status is `Halted`. Under `--mode graph`
the status projects onto the exit table: `Completed` → 0, `Halted` → its terminal
code, `Exhausted` → 7, `LoopDetected` / `Stalled` → 3, `Crashed` → 1.

---

## Patching a graph (additive)

`graph.patch` lets an agent elaborate its own plan at runtime — add nodes and edges
to a stored graph as it learns more, without redefining the whole thing:

```json
{ "graph_id": "g1",
  "patch": { "add_nodes": { "verify": { "kind": "agent", "instruction": "double-check",
                                         "edges": { "ok": "done" } } },
             "add_edges": [ { "from": "work", "label": "error", "to": "verify" } ] } }
```

Patches are **additive only** — never overwrite a node or retarget an existing edge
— so a patch can't strip reachability or a termination guarantee out from under a
run. The grown graph is re-validated; a rejected patch leaves the stored graph
untouched.

---

## A worked example: a review loop

Draft → judge → (publish | revise → back to judge), bounded by the budget:

```json
{
  "start": "draft",
  "nodes": {
    "draft":  { "kind": "agent", "instruction": "draft the announcement", "writes": "draft",
                "edges": { "ok": "judge", "error": "fail" } },
    "judge":  { "kind": "branch", "cases": [], "default": "revise",
                "semantic": { "prompt": "Is the draft ready to publish?", "reads": ["draft"],
                              "choices": { "yes": "publish", "no": "revise" } } },
    "revise": { "kind": "agent", "instruction": "revise the draft", "reads": ["draft"],
                "writes": "draft", "edges": { "ok": "judge", "error": "fail" } },
    "publish":{ "kind": "halt", "status": "completed", "result_from": "draft" },
    "fail":   { "kind": "halt", "status": "crashed" }
  }
}
```

The `revise → judge` back-edge is the loop; the semantic branch decides when to
exit; the visit cap + progress guard stop a draft that never converges.

---

## See also

- [modes-and-triggers.md](modes-and-triggers.md) — the four base modes and reactive
  routing (a run-graph is the explicit form of the same event→action machinery).
- [subagents.md](subagents.md) — the `agent` node runs a subagent turn; the spawn
  caps still apply.
- [configuration.md](configuration.md) — `--graph` / `--mode graph` and the run
  limits a graph inherits.
