# LangGraph ↔ agentd — the parity notebook (internal)

**Status:** informal engineering notes, not normative (RFC 0021 + `docs/workflows.md`
bind). Side-by-side samples for every major LangGraph pattern and its agentd
dialect-2 equivalent, plus the honest deltas in both directions. Written against
LangGraph 1.x (Graph API) and agentd v1.2.0 (workflow dialect 2). Every agentd
sample below is valid dialect-2 JSON — `parse_graph` accepts them verbatim.

The framing difference that explains everything else:

| | LangGraph | agentd |
|---|---|---|
| The graph is… | a **program** (Python/JS you write, compile, host) | **data** (JSON the agent authors at runtime, or an operator pins) |
| Runs inside… | your application process | a supervised child process of a 3 MiB static binary |
| Nodes are… | arbitrary code | a closed set of 12 kinds — *code lives behind MCP* |
| Trust model | the graph is the app | the model can author the graph, the supervisor can always kill it |
| Interop | Python/JS ecosystems | MCP + A2A wire protocols, no runtime required |

---

## 1. Capability matrix

| Capability | LangGraph | agentd | Parity |
|---|---|---|---|
| Typed shared state | `StateGraph(State)` TypedDict/Pydantic channels | the blackboard (string → JSON, explicit `writes`/`reads`/`$from`) | ✅ (agentd is stringly-keyed but explicit-dataflow) |
| Reducers on state | `Annotated[list, operator.add]`, `add_messages` | `writes_mode: append\|merge\|union` | ✅ |
| Arbitrary compute node | any Python function | ❌ **by design** — `tool` (remote MCP), `assign` (+CEL), `infer`, `agent` | see §“non-goals” |
| Fixed edges | `add_edge(a, b)` | per-node `edges: {label → id}` | ✅ |
| Conditional edges | `add_conditional_edges(fn)` | `branch` (pointer predicates, CEL, semantic tier) | ✅ + agentd’s judged branch is built-in |
| Dynamic goto from a node | `Command(goto=…, update=…)` | `infer`/`agent` writes → `branch` routes | ✅ (two nodes instead of one return) |
| Map-reduce fan-out | `Send` API | `foreach` (≤1024 items, ≤8 lanes) | ✅ |
| Parallel branches (superstep) | conditional edge → multiple targets | `parallel` (named bodies, one result object) | ✅ |
| Subgraphs | nested compiled graphs | `subgraph` (inline) / `subgraph {async}` + `join` (child **process**) | ✅ + process isolation |
| Jump to parent graph | `Command(graph=PARENT)` | ❌ (a subgraph returns via its halt → parent edge) | minor gap |
| Human-in-the-loop | `interrupt(payload)` + `Command(resume=value)` | `human` node → A2A `input-required` → `SendMessage{taskId}` | ✅ and **wire-standard** (any A2A client, no SDK) |
| Checkpointing | per-superstep, Postgres/SQLite savers | per-superstep envelopes → **any MCP server** (`state.put/get/list`) | ✅, storage-agnostic by protocol |
| Threads / resume | `thread_id` + checkpointer | stable `checkpoint.key` + `--workflow-resume server:key` | ✅ |
| Time travel / fork | `get_state_history`, `update_state`, replay | `state.get {seq}` → resume `@seq` under a new run-id; edit-the-board fork via plain MCP calls | ✅ |
| Loop safety | `recursion_limit` (one number, one error) | step budget + token pool + deadline + visit caps + progress guard, typed `reason`s | ✅ agentd richer |
| Retry policies | per-node `retry_policy` | per-node `retry {max, backoff_ms}` | ✅ |
| Node caching | `cache_policy (key_func, ttl)` | ❌ declined — the MCP server caches | stance |
| Deferred nodes (barrier) | `defer=True` | `join` covers async handles only | partial gap |
| Long-term memory | `Store` (namespaces, semantic search) | any MCP server — memory is a tool | stance |
| Streaming | `stream()` modes (values/updates/messages) | `agent://workflow` live resource, `agent://events`, A2A SSE | ✅ different consumer model |
| Runtime graph mutation | ❌ (compile-time) | `workflow.patch` — **the model rewrites its own plan mid-run** | ✅ agentd-only |
| Self-authoring | ❌ (a human writes the graph) | `workflow.define`/`run` self-tools | ✅ agentd-only |
| Deployment story | LangGraph Platform (assistants, cron, webhooks) | agentd **is** the unit: Job/CronJob/reactive Deployment, cron mode, A2A/MCP control plane | ✅ built-in |
| Supervision / blast radius | in-process; your app’s problem | OS process tree, kill ladder, cgroups, a supervisor the model can’t prompt | ✅ agentd-only |
| Functional API | `@task` / `@entrypoint` imperative | ❌ — the plain ReAct loop *is* the imperative mode | stance |

---

## 2. Side-by-side samples

Each pair does the same thing. LangGraph on the left is Python; agentd on the
right is the JSON you `--workflow`-pin or the model `workflow.define`s.

### 2.1 Prompt chain with accumulating state (reducers)

**LangGraph**

```python
from typing import Annotated
from typing_extensions import TypedDict
import operator
from langgraph.graph import StateGraph, START, END

class State(TypedDict):
    findings: Annotated[list, operator.add]   # the reducer: append, don't clobber

def find_issue(state: State):
    issue = llm.invoke("find one more issue")
    return {"findings": [issue]}

def enough(state: State):
    return END if len(state["findings"]) >= 3 else "find"

g = StateGraph(State)
g.add_node("find", find_issue)
g.add_edge(START, "find")
g.add_conditional_edges("find", enough)
app = g.compile()
```

**agentd**

```json
{ "start": "find",
  "nodes": {
    "find":  { "kind": "agent", "instruction": "find one more issue",
               "writes": "findings", "writes_mode": "append",
               "edges": { "ok": "enough", "error": "fail" } },
    "enough": { "kind": "branch",
                "cases": [ { "when": { "op": "exists", "key": "findings", "pointer": "/2" },
                             "goto": "done" } ],
                "default": "find" },
    "done": { "kind": "halt", "status": "completed", "result_from": "findings" },
    "fail": { "kind": "halt", "status": "crashed" }
  } }
```

Same reducer semantics (`operator.add` ↔ `append`), same loop, same exit
predicate — plus agentd’s visit caps and progress guard bound the loop even if
the author forgets to.

### 2.2 Routing (conditional edges)

**LangGraph**

```python
def route(state):
    if state["triage"]["urgency"] >= 8:
        return "page"
    return "draft"

g.add_conditional_edges("classify", route, {"page": "page", "draft": "draft"})
```

**agentd**

```json
{ "kind": "branch",
  "cases": [ { "when": { "op": "gte", "key": "triage", "pointer": "/urgency", "value": 8 },
               "goto": "page" } ],
  "default": "draft" }
```

The routing *function* becomes a routing *predicate* — deterministic, free, and
inspectable on the wire. Anything a predicate can’t express is a CEL expression
(`{"op":"cel","expr":"triage.urgency >= 8 && triage.category != 'spam'"}`) or —
LangGraph has no analog — a **semantic branch** where one bounded model
judgement picks a labelled edge.

### 2.3 `Command(goto + update)` — the LLM decides where to go

**LangGraph**

```python
from langgraph.types import Command

def supervise(state) -> Command:
    decision = llm.with_structured_output(Route).invoke(state["messages"])
    return Command(update={"decision": decision}, goto=decision.next_node)
```

**agentd** — the decision is an `infer` (schema-checked, auto re-asked on a bad
shape), the goto is the branch that reads it:

```json
{ "start": "decide",
  "nodes": {
    "decide": { "kind": "infer", "prompt": "Pick the next step.",
                "reads": ["context"],
                "schema": { "next": "string", "why": "string" },
                "writes": "decision", "edges": { "ok": "goto", "error": "fail" } },
    "goto": { "kind": "branch",
              "cases": [
                { "when": { "op": "eq", "key": "decision", "pointer": "/next", "value": "research" }, "goto": "research" },
                { "when": { "op": "eq", "key": "decision", "pointer": "/next", "value": "write" },    "goto": "write" } ],
              "default": "fail" },
    "research": { "kind": "agent", "instruction": "research it", "writes": "notes", "edges": { "ok": "decide", "error": "fail" } },
    "write":    { "kind": "agent", "instruction": "write it up", "reads": ["notes"], "writes": "doc", "edges": { "ok": "done", "error": "fail" } },
    "done": { "kind": "halt", "status": "completed", "result_from": "doc" },
    "fail": { "kind": "halt", "status": "crashed" }
  } }
```

Two nodes instead of one return value — and the decision is *validated data on
the blackboard*, not a control-flow side effect.

### 2.4 Map-reduce (`Send`) ↔ `foreach`

**LangGraph**

```python
from langgraph.types import Send

def fan_out(state):
    return [Send("summarize", {"doc": d}) for d in state["docs"]]

g.add_conditional_edges("split", fan_out)
```

**agentd**

```json
{ "kind": "foreach",
  "items": { "$from": "docs" },
  "parallel": 8,
  "on_error": "continue",
  "body": { "start": "sum", "nodes": {
      "sum": { "kind": "agent", "instruction": "summarize this document",
               "reads": ["item"], "writes": "s", "edges": { "ok": "h", "error": "hf" } },
      "h":  { "kind": "halt", "status": "completed", "result_from": "s" },
      "hf": { "kind": "halt", "status": "crashed", "result_from": "s" } } },
  "writes": "summaries", "edges": { "ok": "reduce", "error": "reduce" } }
```

Each iteration sees `item`/`index` on a scoped board; results collect
positionally; a `tool`/`assign`-only body costs **zero model tokens per item**
(measured ~146k steps/sec). Bounded by design: ≤1024 items, ≤8 lanes.

### 2.5 Parallel branches (orchestrator-worker superstep) ↔ `parallel`

**LangGraph** — a conditional edge returning several targets runs them in one
superstep; results merge via reducers:

```python
g.add_edge(START, "security_review")
g.add_edge(START, "perf_review")     # both run in the same superstep
g.add_edge("security_review", "synthesize")
g.add_edge("perf_review", "synthesize")
```

**agentd**

```json
{ "kind": "parallel",
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
        "hf": { "kind": "halt", "status": "crashed", "result_from": "r" } } }
  },
  "on_error": "continue",
  "writes": "reviews", "edges": { "ok": "synthesize", "error": "fail" } }
```

agentd’s result is one object keyed by branch name — no reducer coordination
needed for the common join; a synthesizing `agent` node just `reads: ["reviews"]`.

### 2.6 Human-in-the-loop: `interrupt()` ↔ the `human` node over A2A

**LangGraph**

```python
from langgraph.types import interrupt, Command

def approval(state):
    answer = interrupt({"question": "Ship it?", "diff": state["diff"]})
    return {"verdict": answer}

# …elsewhere, after inspecting the paused thread:
app.invoke(Command(resume="yes"), config={"configurable": {"thread_id": "t1"}})
```

**agentd** — the pause and the resume are *wire protocol*, not SDK calls:

```json
{ "kind": "human",
  "payload": { "question": "Ship it?", "diff": { "$from": "patch" } },
  "timeout_ms": 86400000,
  "writes": "verdict",
  "edges": { "replied": "route", "timeout": "escalate" } }
```

What the human’s client (ANY conformant A2A client — no Python, no agentd SDK)
sees and does:

```jsonc
// GetTask {"id":"a3"} →
{ "status": { "state": "TASK_STATE_INPUT_REQUIRED",
              "message": { "role": "agent", "parts": [{ "text": "{\"question\":\"Ship it?\",…}" }] } } }
// the human answers — the A2A spec's own multi-turn shape:
// SendMessage {"message":{"taskId":"a3","parts":[{"text":"yes"}]}}
```

Key differences: LangGraph’s resume value round-trips through *your app’s* API;
agentd’s rides a published interop protocol, so the “app” can be a ticketing
system, a chat bridge, or another agent. LangGraph interrupts anywhere in node
code; agentd gates are explicit nodes (a feature for auditability, a constraint
for flexibility). agentd adds a **timeout edge** natively; and the gate survives
a daemon suspend/restart because the suspension is serialized state.

### 2.7 Checkpointing / threads ↔ the MCP checkpointer

**LangGraph**

```python
from langgraph.checkpoint.postgres import PostgresSaver

with PostgresSaver.from_conn_string(DB_URI) as saver:
    app = g.compile(checkpointer=saver)
    app.invoke(input, config={"configurable": {"thread_id": "job-42"}})
# crash → rerun with the same thread_id resumes from the last checkpoint
```

**agentd** — the “saver” is any MCP server implementing three tools; the
“thread_id” is the checkpoint key:

```json
{ "dialect": 2,
  "checkpoint": { "server": "state", "key": "job-42", "every": 1 },
  "start": "…", "nodes": { "…": "…" } }
```

```console
$ agentd --mode workflow --workflow job.json \
    --mcp state=https://ckpt.internal/mcp \
    --workflow-resume state:job-42        # after the crash
```

Same guarantees, different trust shape: envelopes bind the graph by SHA-256
(resume refuses a foreign graph), a monotonic-seq guard makes a second writer
fatal (split-brain), and the store speaks a wire protocol — Postgres, S3,
sqlite, etcd are all *somebody’s MCP server*; agentd links none of them.

### 2.8 Time travel / fork

**LangGraph**

```python
history = list(app.get_state_history(config))
past = history[3].config                       # pick a checkpoint
app.update_state(past, {"verdict": "no"})      # edit state
app.invoke(None, config=past)                  # replay from there (forks)
```

**agentd**

```console
$ agentd … --workflow-resume state:job-42@12 --run-id job-42-whatif   # fork @seq
```

Editing the board first is three plain MCP calls (fetch envelope → edit
`state.blackboard` → `state.put` under a new key) — time-travel needs no
agentd-side API at all; it falls out of state-behind-MCP. History is immutable;
a fork is a new lineage, never a rewrite.

### 2.9 Evaluator-optimizer (the judge loop)

**LangGraph** — generator + evaluator nodes and a conditional edge looping until
accepted.

**agentd** — the judge is a built-in branch tier:

```json
{ "start": "draft",
  "nodes": {
    "draft":  { "kind": "agent", "instruction": "draft the release note", "writes": "doc",
                "edges": { "ok": "judge", "error": "fail" } },
    "judge":  { "kind": "branch", "cases": [], "default": "revise",
                "semantic": { "prompt": "Is it ready to publish?", "reads": ["doc"],
                              "choices": { "yes": "publish", "no": "revise" } } },
    "revise": { "kind": "agent", "instruction": "revise it", "reads": ["doc"],
                "writes": "doc", "edges": { "ok": "judge", "error": "fail" } },
    "publish": { "kind": "halt", "status": "completed", "result_from": "doc" },
    "fail":    { "kind": "halt", "status": "crashed" }
  } }
```

The visit cap + progress guard end a never-converging loop with a typed reason —
LangGraph’s `recursion_limit` is the single-number version of this.

### 2.10 Orchestrator-worker with process isolation

**LangGraph** — subgraphs/`Send` inside one process.

**agentd** — the phases can be **supervised child processes**:

```json
{ "start": "spawn_a",
  "nodes": {
    "spawn_a": { "kind": "subgraph", "async": true, "graph": { "start": "…", "nodes": { "…": "…" } },
                 "writes": "h1", "edges": { "ok": "spawn_b", "error": "fail" } },
    "spawn_b": { "kind": "subgraph", "async": true, "graph": { "start": "…", "nodes": { "…": "…" } },
                 "writes": "h2", "edges": { "ok": "gather", "error": "fail" } },
    "gather":  { "kind": "assign", "value": [ { "$from": "h1" }, { "$from": "h2" } ],
                 "writes": "hs", "edges": { "ok": "join", "error": "fail" } },
    "join":    { "kind": "join", "handles": { "$from": "hs" }, "timeout_ms": 300000,
                 "writes": "results", "edges": { "ok": "done", "error": "fail", "timeout": "fail" } },
    "done": { "kind": "halt", "status": "completed", "result_from": "results" },
    "fail": { "kind": "halt", "status": "crashed" }
  } }
```

Each async subgraph is a child under the spawn caps, its own budget slice, and
the kill ladder — a runaway phase is one SIGKILL, not a poisoned event loop.

---

## 3. What each side has that the other doesn’t

**LangGraph, honestly ahead:**
- **Arbitrary code nodes** — the whole Python ecosystem inline. agentd’s answer
  is deliberate: code lives behind MCP (`tool`), pure shaping is `assign`/CEL.
  This is the security posture, not an oversight.
- `Command(graph=PARENT)` cross-graph jumps; **deferred barrier nodes**;
  **node caching**; the **functional API**; `Store` with semantic search
  built in.
- Mid-node interrupts (pause *anywhere* in node code, multiple per node).

**agentd, honestly ahead:**
- **The graph is data the model can author and patch mid-run**
  (`workflow.define`/`run`/`patch`) — LangGraph graphs are compile-time.
- **Wire-standard HITL**: `input-required` + `SendMessage` work from any A2A
  client on any stack; no app server to build.
- **Storage-agnostic durability by protocol** (the checkpointer is an MCP
  profile, not a driver list), with graph-hash binding + split-brain fencing.
- **OS-level supervision**: process isolation, cgroups, kill ladder, budgets a
  prompt can’t argue with; typed termination reasons instead of one recursion
  error.
- **Fail-closed dialect** (unknown fields refuse at define time) and a
  feature-detectable `surfaces.workflow` manifest.
- Footprint: a 3 MiB static binary vs a Python runtime; deterministic steps at
  ~146k/sec with zero tokens.

**Deliberate non-goals (see RFC 0021 §15):** node caching, a general barrier
node, a built-in Store, inline code, the functional API.

---

## 4. Migration cheat-sheet (LangGraph → agentd)

| You wrote… | Write instead… |
|---|---|
| `StateGraph(State)` channels | blackboard keys (`writes`/`reads`) |
| `Annotated[list, operator.add]` | `"writes_mode": "append"` |
| a Python node calling an API | a `tool` node against an MCP server that wraps the API |
| a Python node doing pure transforms | `assign` (`value` template or CEL `expr`) |
| `llm.with_structured_output(...)` | `infer` with a `schema` (auto re-ask on bad shape) |
| `add_conditional_edges(fn)` | `branch` cases (pointer preds / CEL / semantic) |
| `Command(goto, update)` | `infer` → `branch` |
| `Send(...)` fan-out | `foreach` |
| parallel `add_edge`s from one node | `parallel` |
| a compiled subgraph | `subgraph` (add `async: true` + `join` for a process) |
| `interrupt()` / `Command(resume)` | `human` node; reply = A2A `SendMessage{taskId}` |
| `checkpointer=` + `thread_id` | `"checkpoint": {server, key}` + `--workflow-resume` |
| `get_state_history` + replay | `state.list`/`state.get` + `--workflow-resume …@seq` |
| `recursion_limit=N` | `--max-steps` (+ the built-in visit/progress guards) |
| `Store` | an MCP memory server |
| LangGraph Platform deployment | a k8s Job/CronJob/Deployment of the binary |

Topology translates mechanically (a compiled LangGraph exports its node/edge
list via `get_graph()`); node *bodies* map per the table. A `langgraph-import`
skeleton generator remains a tooling idea (RFC 0021 §15) — nothing blocks it.
