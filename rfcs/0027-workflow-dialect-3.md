# RFC 0027: Workflow dialect 3 — durable DAGs with start nodes

**Status:** Implemented (agentd 2.0 track, phase P4)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.6, §4.3–4.4); supersedes RFC 0021 §4 (dialects 1/2: cyclic graphs, the blackboard, the twelve kinds) while keeping its reducers, human gate, and checkpoint ideas; builds on RFC 0025 (durability), RFC 0026 (loop), RFC 0028 (tools).

---

## 1. Summary

A dialect-3 workflow is a **directed acyclic graph of steps** that begins at
one or more **start nodes** (the triggers). Each firing of a start node creates
a **run**; every step of a run is a **durable transition** (RFC 0025 §7);
long-lived behaviour comes from start nodes and **structured iteration**
(`foreach`/`batch`/`iterate`), never from back-edges. Data flows through typed
step outputs (JSON Schema), templates and CEL. Runs are concurrent,
addressable, cancellable, resumable, and may spin other workflows as child
runs.

## 2. Definition

```yaml
name: triage                       # unique per instance
version: 3                         # the dialect (default 3 for the new schema; 1/2 documents are refused)
description: "…"
armed: true                        # arm start nodes at boot/restore (default true)
inputs: { schema: {…} }            # JSON Schema of run inputs (start payload / A2A inputs)
concurrency: { max_runs: 4, on_overflow: queue }     # queue | drop | replace
limits: { steps: 500, tokens: 2000000, deadline: 1h, budget: { windows: […] } }
outputs: { schema: {…} }           # optional schema of the finish output
steps:
  <id>: { kind: <kind>, depends_on: [<id>…], when: <CEL>, …kind fields…, <cross-cutting> }
```

Identifiers: `[a-zA-Z_][a-zA-Z0-9_-]{0,63}`. `steps` is a map (deterministic
order); `depends_on` edges + start nodes define the DAG.

## 3. Data model

- `inputs` — validated run inputs.
- `run` — `{id, workflow, start: {node, payload, ts}, principal?, task?, attempt}`.
- `steps.<id>` — `{status, output, error?, attempt, started, finished}`.
- `vars` — the run's variables (`assign` writes; reducers `overwrite|append|merge|union`).
- `memory.<key>` — read-through to agent memory (RFC 0028).
- `item`, `index`, `batch` — inside iteration bodies.
- `env` — a curated, secret-free view (instance name, run id, ts).
- **Templates** `{{path}}` with dotted/JSON-pointer paths and `{{path | default}}`,
  dependency-free; **CEL** (`when:`, `on:`, `value: 'CEL: …'`, `assert:`,
  `filter:`) over the same names (feature `cel`; a non-CEL build refuses a
  definition that uses CEL at validation).
- Large values (> `workflow.inline_max_bytes`, default 64 KiB) are stored as
  **artifacts** and referenced `{"$artifact": id}`; templates dereference
  transparently; iteration inputs stream from artifacts.

## 4. Start nodes

| Kind | Fires when | Options |
|---|---|---|
| `once` | armed (boot/restore) or `workflow.run` | `policy: ensure \| always` |
| `loop` | previous run finished | `interval`, `delay`, `until` (CEL over last outcome), `max_iterations`, `backoff` (`{initial, max, factor}` on failure) |
| `schedule` | cron / interval | `cron` (5-field, RFC 0008 parser), `every`, `tz` (default UTC), `jitter`, `catch_up: none \| one \| all`, `at` (one-shot) |
| `subscribe` | MCP resource update (notify-then-read) | `server`, `uri`, `debounce_ms`, `coalesce`, `filter` (CEL over the read), `claim {server, ttl, renew_fraction}`, `shard`, `deliver: run \| wait`, `on_no_listener: run \| drop` |
| `signal` | a named signal | `name`, `filter`, `deliver: run \| wait` |
| `event` | an internal event | `on` (`workflow.finished|failed`, `subagent.finished`, `budget.exhausted|resumed`, `config.reloaded`, `restore.done`, `human.timeout`, …), `filter` |
| `a2a` | a principal's message | `command`, `roles`, `inputs` (CEL over the message) |
| `manual` | `workflow.run` only | — |

A run's `run.start` names the fired node; sibling start nodes are `skipped`.
Per-start `concurrency` and `inputs` mapping override the workflow's.
Start-node state (last fired, iteration, missed) is durable in the manifest.
`armed: false` / `workflow.pause` disarms without deleting.

## 5. Node catalogue (normative list — semantics in the plan §3.6.3)

Control: `switch`, guarded edges (`when`), `parallel`, `foreach` (with
`batch {size, parallel, rate}`, `collect`, `on_error`), `batch`, `iterate`,
`race`, `join`, `subgraph`, `workflow` (child run: `mode: sync|async|detached`,
`inputs`, `start`, `version`, `cascade`), `wait` (`on: resource | condition |
signal | run | subagent | message`, `timeout`), `sleep`, `assert`, `fail`,
`noop`, `checkpoint`, `finish`.
Data: `assign`/`transform`, `map`, `filter`, `reduce`, `sort`, `dedupe`,
`chunk`, `template`, `parse`, `validate`, `memory.*`, `artifact.*`,
`knowledge.*`, `search.*`.
Integration: `mcp.tool`, `mcp.resource` (`read|list|prompt|complete`), `tool`,
`a2a.send`, `a2a.delegate`, `a2a.wait`, `workflow.signal`, `workflow.wait`,
`workflow.cancel`, `emit`.
Intelligence & agents: `think`, presets `classify|extract|summarize|judge|route`,
`agent`, `subagent`, `human`.

Cross-cutting on every step: `depends_on`, `when`, `retry {max, backoff}`,
`timeout`, `on_error: fail | continue | goto:<id>`, `idempotent`, `on_replay:
retry|skip|fail`, `output_schema`, `cache {key, ttl}`, `budget`, `skills`,
`otel {attributes}`, `description`.

Deliberately absent: a generic `http` node, local code execution, cycles,
non-A2A inbound webhooks.

## 6. Semantics

- **Scheduling**: a step is ready when all `depends_on` are terminal
  (`done|skipped`), and its `when` (if any) evaluates true — else it is
  `skipped`; `finish` terminates the run with `{status, output}`; a run with
  no ready steps and no `finish` reached is `stalled` (an error).
- **Outcomes**: `done | failed | skipped | cancelled | timeout`; `on_error`
  routes `failed`; `goto:<id>` schedules that step even if its deps are not
  terminal (an explicit recovery edge — still acyclic by validation).
- **Iteration**: `foreach` seeds `item`/`index` per element on a scoped view;
  body writes do not flow back; results are collected positionally (a failed
  element's slot carries `{index, error}`); `batch` runs elements in batches
  of `size` with up to `parallel` batches (`rate` paces batch starts); progress
  is durable per batch; `iterate` runs the body until `until`/`while` or
  `max_iterations`.
- **Structured I/O**: `output_schema` validates; `mcp.tool` prefers
  `structuredContent`; `think` uses provider structured output
  (`intelligence.structured_output`) with schema re-ask fallback; `subagent`
  honours `output_contract`.
- **Human**: `human` suspends with `input-required` on the owning A2A task (or
  a mapped tool); the reply is data; `timeout` edge.
- **Cancellation**: `workflow.cancel`/A2A cancel → running effects are told
  (children get `Cancel`, MCP calls get `notifications/cancelled` where the
  server supports it), the run ends `cancelled`, child runs per `cascade`.

## 7. Durability

Run record (RFC 0025 §3.3 `run`): after every completed step, per batch, at
suspension (`wait`/`sleep`/`human`/`join`/`waiting_budget`), at terminal.
Replay: a `running` step is re-executed with the same idempotency key
(`on_replay`); batches resume at the first incomplete batch; suspensions
re-arm from absolute deadlines. `cache` skips a step whose input hash matches
a durable cache entry within `ttl`.

## 8. Validation (author time, fail closed)

At least one start node; every non-start step reachable from a start node;
acyclic; `depends_on`/`goto` targets exist; `finish` reachable; kinds known;
tool/server references exist in the registry/config (disabled tools refused);
schemas well-formed; CEL compiles; caps: steps ≤ 512, nesting ≤ 4,
`batch.parallel` ≤ 8, `iterate.max_iterations` ≤ 10 000, `foreach` no element
cap. Strict unknown-field check per kind (RFC 0021 §4.1 carried over).

## 9. Run record & hash

`state: {workflow, workflow_hash, inputs, status, start, cursor: {steps: {id:
{status, attempt, started, finished, output|$artifact, error}}, batches: {step:
{done, partial}}}, vars, waits, timers, budget: {steps, tokens}, task?,
principal?, children: [run ids], parent?: {run, step}}`. `workflow_hash` =
SHA-256 of the canonical definition JSON; `resume_policy: refuse | force |
restart` governs a mismatch at restore.

## 10. Tools & commands over runs

`workflow.run/create/update/delete/list/status/cancel/pause/resume/signal/wait`
(RFC 0028); A2A commands mirror them (RFC 0029); `agent://runs` and
`agent://run/<id>` read surface; `--workflow-schema` prints the dialect-3 JSON
Schema.

## 11. Compatibility

Dialect 1/2 documents are refused with a message naming the migration
(`docs/workflows.md` §migration): back-edges → `iterate`/`loop` start,
`writes`/blackboard → step outputs + `vars`, `halt` → `finish`, `infer` →
`think`, `branch` → `switch`/`when`, `wait`/`human` unchanged in spirit.

## 12. Test plan

Unit: parser/validator (every kind, every cap, acyclicity, strict fields),
scheduler on a mock executor (all outcomes, `on_error`, `goto`, `race`,
`iterate`), templates/CEL, batching durability (kill between batches on the
memory store), workflow-of-workflows (sync/async/cascade), start nodes with a
fake clock. E2E: the five 1.x shapes as dialect-3 examples; SIGKILL mid-batch
resume; signal between two workflows; human gate over A2A.
