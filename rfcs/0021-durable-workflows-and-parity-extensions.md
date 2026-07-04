# RFC 0021: Durable workflows & parity extensions — reducers, parallel branches, human gates, and the MCP checkpointer

**Status:** Draft
**Author:** Andrii Tsok
**Date:** 2026-07-04
**Part of:** the workflow surface (docs/workflows.md is the shipped baseline); extends the agentic-loop contract (RFC 0007), reactive routing (RFC 0008), the self-MCP surface (RFC 0005), and the capabilities manifest (RFC 0014/0015)

---

## 1. Problem / Context

agentd's workflow engine (feature `workflow`) ships a declarative, agent- or
operator-authored cyclic graph: ten node kinds
(`agent`/`tool`/`assign`/`infer`/`branch`/`foreach`/`join`/`wait`/`subgraph`/`halt`),
an explicit-dataflow blackboard, two condition tiers (+ CEL), deterministic
fan-out, async subgraphs with fan-in, reactive suspend/resume on `wait`, and
layered termination with typed reasons. That surface already expresses the
graph-shaped majority of what code-first SDKs (LangGraph is the reference
point) express imperatively — with stronger operational guarantees (process
isolation, a supervisor the model cannot prompt, a shared token pool, typed
termination) and one capability those SDKs lack entirely: the graph is **data**,
so the running agent can author and patch it (`workflow.define`/`run`/`patch`).

A structured comparison against LangGraph 1.x (StateGraph channels/reducers,
conditional-edge parallelism, `Send`, `Command`, `interrupt()`/resume,
checkpointers/threads/time-travel, `Store`) leaves exactly **four deltas** worth
closing, and one hygiene defect the closes must not inherit:

1. **No merge semantics on writes.** A node's `writes` overwrites its key.
   Accumulation patterns (append results across loop iterations, merge partial
   objects) need `assign` gymnastics or CEL. LangGraph's reducers
   (`operator.add`, `add_messages`) are its single best authoring ergonomic.
2. **No cheap heterogeneous parallelism.** `foreach` fans **one** body over N
   items; `subgraph async:true` ×N + `join` runs **different** bodies
   concurrently but pays a supervised child *process* per branch. The missing
   shape is "run the security review AND the perf review concurrently, in
   process, then continue."
3. **Human-in-the-loop is an idiom, not a contract.** `wait` + the
   reactive-daemon workflow *can* express approval gates, but every author
   reinvents the resource convention, and the served A2A task never signals
   `input-required` — so a spec-conformant A2A client cannot see that a
   workflow is waiting on a human.
4. **No durable per-step state.** The engine serializes `GraphState` only at a
   `wait` suspension. A crash mid-graph loses the run (recovered by idempotent
   re-trigger, RFC 0011 §7 — correct but coarse). There is no state history, no
   resume-from-checkpoint, no fork/time-travel. This is the one *architectural*
   gap versus checkpointer-based engines.
5. **(Hygiene) Unknown node fields are silently ignored.** `#[serde(tag =
   "kind")]` makes an unknown *kind* a define-time error (fail-closed, good),
   but an unknown *field* on a known kind deserializes silently. A graph using
   `writes_mode` (§5) on a pre-0021 build would not error — it would silently
   **overwrite instead of append**. Every extension in this RFC would inherit
   that hazard; it must be fixed first.

**This RFC owns:** the write-reducer semantics (§5), the `parallel` node (§6),
the `human` gate node + its A2A `input-required` binding (§7), the
MCP-checkpointer contract — envelope, tool profile, policy, resume, fork (§8),
and the dialect-hygiene rules (§4) that make all of it fail-closed on older
builds. **It does not own:** the checkpoint *server* (any MCP server
implementing the §8.3 tool profile — agentd ships none, per the minimalism
moat); node caching and a general deferred-node barrier (declined, §15); the
`Store`/long-term-memory question (memory is an MCP server — a stance, not a
gap); translation of foreign SDK node *code* (topology import is a tooling
idea, not runtime surface, §15).

## 2. Design principles (binding)

- **Zero new dependencies.** Every mechanism below is serde + the existing MCP
  client + the existing lane machinery. The checkpointer's durability lives
  *behind* MCP, exactly like every other capability.
- **Fail-closed dialect evolution.** A graph that uses a capability this build
  does not carry is rejected at define time with a clear message — never a
  silent semantic change.
- **The supervisor stays ignorant.** All four extensions live in the child-side
  driver. The supervisor's kill ladder, budgets, and liveness contract
  (RFC 0002/0003) are unchanged.
- **Additive only.** A graph valid before this RFC runs byte-for-byte
  identically after it.

## 3. Overview of the additions

| § | Addition | Kind | Closes |
|---|---|---|---|
| 4 | Dialect hygiene: strict fields + `dialect` + manifest surface | validation | silent-ignore hazard |
| 5 | `writes_mode: overwrite\|append\|merge\|union` | field on all writing kinds | reducers |
| 6 | `parallel` node — named heterogeneous branches, in-process lanes | node kind #11 | conditional-edge parallelism / supersteps |
| 7 | `human` node — payload publish + A2A `input-required` + reply wait | node kind #12 | `interrupt()`/resume |
| 8 | `checkpoint` policy + the MCP checkpointer tool profile + `--workflow-resume` | graph-level policy + CLI | checkpointers, threads, time-travel |

## 4. Dialect hygiene (prerequisite, binding)

1. **Strict node fields.** All node kinds gain `deny_unknown_fields`
   semantics: an unrecognized field on a known kind is a **define-time
   validation error** (`workflow.define` → error result; `--mode workflow` →
   exit `2`). This is the one behavior change to the existing surface; a graph
   carrying a typo'd field today was already broken silently.
2. **The `dialect` field.** A graph MAY declare `"dialect": <u32>` at the root.
   The baseline shipped surface is dialect **1**; the additions in this RFC are
   dialect **2**. A build validates `dialect <= supported`; a graph *using* a
   §5–§8 construct without declaring `"dialect": 2` is auto-upgraded (the
   construct itself is the signal) — the field exists for humans and tooling,
   not as the gate.
3. **Manifest surface.** `--capabilities` grows
   `surfaces.workflow.dialect: 2` and `surfaces.workflow.checkpoint:
   true|false` (RFC 0014 §5 additive rules). agentctl feature-detects from the
   manifest, never from the version string.

Because pre-0021 builds ignore unknown fields, they cannot reject a dialect-2
graph themselves; the manifest check is the operational gate. Post-0021 builds
are fail-closed forever after (rule 1).

## 5. Write reducers — `writes_mode`

Every writing kind (`agent`, `tool`, `assign`, `infer`, `foreach`, `parallel`,
`human`, `join`, `wait`) accepts:

```json
{ "writes": "results", "writes_mode": "append" }
```

| Mode | Semantics (existing value `E`, incoming `v`) | Type error → |
|---|---|---|
| `overwrite` *(default)* | `E := v` | — (never errors) |
| `append` | absent → `[v]`; `E` array → `E.push(v)`; else **error edge** | node's `error` edge |
| `merge` | absent → `v`; `E` and `v` objects → shallow merge, `v` wins per key; else **error edge** | node's `error` edge |
| `union` | as `append`, but skip `v` if deep-equal to an existing element | node's `error` edge |

Rules (binding): the reduce happens **before** the 1 MiB value clamp — an
over-clamp result is replaced by the standard error marker and takes the
`error` edge (the existing clamp contract, unchanged). Reducers are pure and
synchronous; they never call out. `halt.result_from` reads the reduced value.
A `branch`/`halt` (non-writing kinds) do not accept the field (rule §4.1 makes
that a define-time error). CEL `assign.expr` remains the escape hatch for
custom folds; `writes_mode` is the dependency-free Tier-1.

## 6. The `parallel` node

```json
{ "kind": "parallel",
  "branches": {
    "security": { "start": "s0", "nodes": { … } },
    "perf":     { "start": "p0", "nodes": { … } }
  },
  "on_error": "collect",
  "writes": "reviews",
  "edges": { "ok": "synthesize", "error": "fail" } }
```

- Runs each named branch body **concurrently in-process** on a scoped child
  board (the `foreach` lane machinery, reused verbatim): each branch sees a
  copy-on-read view of the parent board plus its own scratch; parent writes
  land only via the collected result.
- Result: an **object keyed by branch name** (deterministic assembly order:
  lexicographic), written per `writes`/`writes_mode`.
- Bounds (binding): ≤ **16** branches per node; ≤ **8** concurrently active
  lanes shared with `foreach`'s cap (one global lane pool per driver, so
  `parallel` inside `foreach` cannot multiply lanes). Each branch's steps count
  against the run's step budget; all branches draw the one shared token pool.
- `on_error`: `"fail_fast"` (default — first branch error cancels remaining
  branches, node takes `error`) or `"collect"` (every branch runs; errors
  appear as `{"$error": …}` markers in the result object; node takes `ok` iff
  ≥ 1 branch succeeded, else `error`).
- A branch body is a full sub-graph (waits allowed only in the sync-subgraph
  sense; `async` subgraphs inside branches remain legal and are joined
  normally). `halt` inside a branch halts the **branch**, not the run.

Relationship to existing kinds: `foreach` = one body × N items;
`parallel` = N bodies × one board; `subgraph async` + `join` = process-isolated
phases. All three now compose.

## 7. The `human` gate node

```json
{ "kind": "human",
  "payload": { "diff": { "$from": "patch" }, "question": "Ship it?" },
  "reply_uri": "approvals://deploy-42",
  "timeout_ms": 86400000,
  "writes": "verdict",
  "edges": { "replied": "route_on_verdict", "timeout": "escalate" } }
```

Semantics (binding), in order:

1. **Publish.** The resolved `payload` is exposed as the served resource
   `agent://workflow/gate/<node-id>` (read-only; requires a serving build —
   without `--serve-mcp` the node still functions via `reply_uri` alone and the
   publish step is skipped with a `workflow.gate.unserved` telemetry event).
2. **Signal.** If the run is a served A2A task, its state transitions to
   **`input-required`** with the payload as the status message — a
   spec-conformant A2A client now *sees* the wait (RFC 0020 binding). The state
   returns to `working` on resume.
3. **Wait.** The node suspends exactly like `wait`: on **either** (a) an A2A
   `SendMessage` addressed to the waiting task — the message content becomes
   the reply — or (b) an update on `reply_uri` (any MCP resource; the standard
   notify-then-read). First signal wins; the other path is disarmed.
4. **Resume.** The reply value is written per `writes`/`writes_mode`; the node
   takes `replied`. On `timeout_ms` expiry it takes `timeout` (nothing
   written). In a reactive-daemon workflow the suspension serializes into
   `$workflow.suspended` exactly as `wait` does today — a `human` gate survives
   the daemon's suspend/resume cycle by construction.

`human` deliberately does **not** encode approve/reject: the reply is data, and
routing on it is a `branch` (predicates or CEL on the verdict) — one concept
fewer, and rejection reasons, multi-approver schemes, etc. stay authorable.

## 8. The MCP checkpointer

### 8.1 Policy (graph root)

```json
{ "dialect": 2,
  "checkpoint": { "server": "state", "key": "run/{run_id}", "every": 1,
                  "on_error": "continue" },
  "start": "…", "nodes": { … } }
```

- `server` — the **name** of a declared `--mcp` server implementing the §8.3
  profile. Declaring a `checkpoint.server` that is not configured is exit `2`
  / define-time error.
- `key` — the state identity. `{run_id}` interpolates; a stable operator-chosen
  key makes the run **resumable across pod replacements** (the thread-id
  analog).
- `every` — checkpoint after every N successful supersteps (default 1). A
  `wait`/`human` suspension and the terminal step **always** checkpoint,
  regardless of N.
- `on_error` — `continue` (default: a failed checkpoint write emits
  `workflow.checkpoint.fail` and the run proceeds; durability degrades, the
  run does not) or `halt` (the run takes the standard failure path — for
  workflows where replay is worse than stopping).

### 8.2 The envelope

One JSON object, versioned, self-describing:

```json
{ "v": 1, "seq": 17, "run_id": "…", "workflow_sha256": "…",
  "cursor": "<node-id>", "board": { … }, "budget": { "steps": 17, "tokens": 48211 },
  "visits": { "<node-id>": 3, … }, "lanes": null, "ts": "…" }
```

- `workflow_sha256` — hash of the canonical (whitespace-free, key-sorted)
  graph JSON. Resume verifies it; a mismatch is a refusal (see 8.4).
- `board` is the full blackboard — the same serialization the `wait`
  suspension already produces (this envelope **is** that structure plus
  identity/sequence fields; one serializer, two consumers).
- Checkpoints are taken at superstep boundaries only — `lanes` is non-null only
  for a suspension inside `foreach`/`parallel`, and v1 of the envelope declines
  mid-lane checkpoints: the boundary after the fan-out node completes is the
  durable point. (Mid-lane durability is the one deliberate simplification;
  revisit only with evidence.)
- Secrets never enter the envelope by construction: tokens/credentials are not
  representable on the blackboard (RFC 0012; the intelligence/MCP clients never
  surface them into results).

### 8.3 The checkpointer tool profile (server-side contract)

Any MCP server exposing these three tools is a conformant checkpointer:

| Tool | Args | Returns | Semantics |
|---|---|---|---|
| `state.put` | `{key, seq, state}` | `{ok, seq}` | Persist; MUST be atomic per key; MUST reject `seq <=` latest stored (`{ok:false, latest}`) — the driver treats that as fatal-for-this-run (a second writer owns the key: RFC 0019 claim semantics apply upstream) |
| `state.get` | `{key, seq?}` | `{state}` or error | Latest, or the exact `seq` (history read = time travel) |
| `state.list` | `{key}` | `{seqs:[…]}` | The retained history (retention is server policy) |

This is the entire coupling. Postgres, S3, sqlite, etcd — all are somebody's
MCP server, in any language; agentd links none of them. The monotonic-`seq`
rejection doubles as the split-brain guard.

### 8.4 Resume, fork, time-travel

```
agentd --mode workflow --workflow pipeline.json \
       --workflow-resume state:run/abc123          # server-name:key[@seq]
```

- Resume fetches the envelope (`state.get`), verifies `workflow_sha256`
  against the supplied graph, and starts the driver at `cursor` with the
  restored board, **budgets, and visit counts** (a resumed run does not get a
  fresh token pool — the budget is a property of the *work*, not the process;
  binding). Hash mismatch is exit `2` with both hashes in the message;
  `--workflow-resume-force` overrides for deliberate graph-edit-and-continue,
  which resets `visits` but keeps board+budget.
- **Fork / time-travel:** resume `@seq` under a **new** `--run-id` (and
  therefore a new checkpoint `key`). History is immutable; a fork is a new
  lineage. Editing the board before a fork is an `assign`-shaped operator
  action: fetch envelope via any MCP client, edit, `state.put` under the new
  key, resume — no agentd-side surface needed.
- **Crash recovery:** a `Job` with `restartPolicy: OnFailure` and a stable
  `checkpoint.key` re-runs, finds `state.get` non-empty, and — with
  `--workflow-resume` — continues instead of restarting. Whether to
  auto-resume is the operator's/agentctl's call; agentd never resumes
  implicitly (explicitness is the RFC 0011 idempotency stance).

## 9. Invariants (binding)

1. A dialect-1 graph's behavior is bit-identical post-0021.
2. Unknown fields/kinds are define-time errors from this RFC on (fail-closed).
3. Reducers are pure; no reducer invokes intelligence, tools, or I/O.
4. The global lane pool caps concurrent `foreach`+`parallel` lanes at 8; no
   nesting multiplies it.
5. Budgets (steps, tokens, deadline, visit caps, progress guard) govern all
   new kinds identically; a resumed run inherits spent budget.
6. The supervisor contract is untouched: kills, liveness, drain, and the exit
   table (RFC 0011 §5) apply to workflows with checkpoints exactly as without.
7. Checkpointing is observational: disabling it cannot change a run's result,
   only its durability.
8. `human` without serving degrades to `wait`-on-`reply_uri` — never a
   hard requirement on `--serve-mcp`.
9. No new crate dependencies (the moat holds: 3 direct external deps).

## 10. Config / CLI / manifest surface

| Surface | Addition |
|---|---|
| Graph root | `dialect`, `checkpoint{server,key,every,on_error}` |
| Node kinds | `parallel`, `human`; `writes_mode` on writing kinds |
| CLI | `--workflow-resume <server:key[@seq]>`, `--workflow-resume-force` |
| Env | `AGENT_WORKFLOW_RESUME` (same value; flag wins) |
| Manifest | `surfaces.workflow.dialect: 2`, `surfaces.workflow.checkpoint: bool`, `surfaces.workflow.kinds: […12]` |
| Served resources | `agent://workflow/gate/<node-id>` (read-only, gate payloads) |
| Telemetry | `workflow.checkpoint {seq,bytes,ms}`, `workflow.checkpoint.fail {seq,err}`, `workflow.resume {seq,key}`, `workflow.gate.open/close {node,via}`, `workflow.parallel {branches,ok,err}` |

## 11. Security posture

- The envelope holds tool outputs — potentially sensitive. The checkpoint
  server SHOULD be tagged `sensitive` (`--mcp-tags`); the trifecta gate
  (RFC 0012) then treats a workflow with checkpointing + untrusted input +
  egress as the three-leg config it is.
- Gate payloads on `agent://workflow/gate/*` are served under the same
  authenticated HTTPS surface as every other resource; an unauthenticated peer
  sees nothing new.
- A malicious checkpoint server can at worst (a) lose history — degraded
  durability, `on_error` policy applies; or (b) serve a tampered envelope on
  resume. (b) is why `workflow_sha256` binds the graph, and why the envelope's
  `board` values re-enter the run as **untrusted content** under the standard
  RFC 0012 stance (they were tool outputs when first written; they stay tool
  outputs).
- `SendMessage`-as-reply (§7.3a) is gated exactly like every A2A data-plane
  method — the auth model is RFC 0020's, unchanged.

## 12. Failure modes

| Failure | Behavior |
|---|---|
| Checkpoint write fails / times out | per `checkpoint.on_error`: telemetry + continue, or halt path |
| `state.put` seq conflict | fatal for the run (`crashed`, exit 1): a second writer owns the key |
| Resume: key not found | exit `2` (explicit resume of nothing is a config error) |
| Resume: hash mismatch | exit `2`; `--workflow-resume-force` overrides |
| `parallel` branch panics/errors | `on_error` policy (fail_fast cancel vs collect markers) |
| `human` double-signal race | first of {A2A message, `reply_uri` update} wins; the loser is disarmed and logged |
| Gate reply exceeds value clamp | standard clamp: error marker, node takes `error`… via `replied` edge with `{"$error":…}` written — the author branches on it |
| Reducer type error | node's `error` edge (never a silent coercion) |

## 13. Conformance & test plan

Extends the black-box suite (`agentd-conformance`): reducer table
(all four modes × absent/array/object/mismatch), lane-pool saturation
(`parallel` × `foreach` composition never exceeds 8), gate round-trip over
served A2A (`input-required` observed on the wire, `SendMessage` resumes),
gate via `reply_uri` with the mock MCP server, checkpoint-every-step then
kill -9 mid-run then resume-and-complete with budget carried, fork `@seq`
divergence, hash-mismatch refusal, dialect-1 byte-identity regression, and
unknown-field rejection.

## 14. Sequencing

1. §4 hygiene (small, standalone, immediately valuable)
2. §5 reducers (driver-local)
3. §6 `parallel` (lane-pool refactor + node)
4. §7 `human` (compose `wait` + served resource + A2A state)
5. §8 checkpointer (envelope unification with the suspend path first, then
   policy/tools/resume)

Each lands independently green; the dialect flips to 2 with the first of §5–§8.

## 15. Non-goals / declined

- **Node caching** — belongs behind MCP (the server caches; it owns the
  semantics of staleness). Declined at the runtime layer.
- **A general deferred-node barrier** — `join` + `parallel` cover the observed
  shapes; a graph-wide barrier invites deadlock authoring. Revisit on evidence.
- **A built-in `Store` / long-term memory** — memory is an MCP server. Stance,
  not gap.
- **Arbitrary inline code nodes** — the no-local-code posture is the product.
  CEL (pure expressions) and `tool` (remote compute) are the escape hatches.
- **Foreign-SDK import** (LangGraph topology → workflow skeleton) — useful
  *tooling*, not runtime surface; nothing in this RFC blocks it.
- **Mid-lane checkpoint granularity** — §8.2's deliberate simplification.

## 16. Compatibility

Additive except §4.1 (unknown-field strictness), which converts
silently-broken graphs into loudly-broken ones — the change is the fix.
`contract_version` stays `1.0` (manifest additions follow RFC 0014 §5's
additive rules); the workflow dialect becomes the versioned axis for the graph
language itself.
