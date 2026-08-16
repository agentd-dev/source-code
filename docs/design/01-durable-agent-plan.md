# agentd 2.0 — the durable, workflow-driven agent: design & implementation plan

**Status:** DRAFT for review (2026-08-16). Nothing in this document is built.
**Decision requested:** proceed / revise / stop — per phase (§6) and per open
question (§8).
**Scope:** the internal wiring and behaviour of the next major agentd — the
requirements Andrii listed on 2026-08-16 (R1–R17, then R18 knowledge/search,
R19 skills, R20 conversation preflight + plan) plus the "no modes" idea (I1).
**Decided so far (2026-08-16):** D5 strict durability for intake — yes; the
overall scope/phasing — accepted.
**Supersedes, when adopted:** the mode model (RFC 0008), the served-MCP
composability surface (RFC 0005 §tools), the nested subagent process tree (RFC
0009 §process model), the workflow dialect 1/2 topology (RFC 0021 §4), and the
flag/env parameter set (docs/configuration.md §3). Everything else (supervision,
MCP client, intelligence transport, security posture, observability schemas,
A2A conformance, the config *mechanism* just built) is carried forward.

---

## 0. Executive summary

agentd becomes **one durable agent process that runs workflows**:

- It starts, loads its base configuration (settings, MCP servers, tools,
  workflows, store), **restores its durable state from a remote store**, and
  runs the workflows it is configured to run. There are no execution modes:
  a one-shot job, a cron job, a reactive daemon and a long-lived service are
  all just workflows with different *triggers* and one *lifecycle policy*
  (§3.13). `agentd --instruction "…"` keeps working as sugar for a one-node
  workflow.
- **Every unit of progress is durable.** Incoming A2A messages, trigger
  firings, workflow steps, agent turns, subagent results, memory writes,
  artifacts and timers are recorded in a remote **state store** reached
  through a small adapter contract (`put/get/list/delete`) that maps onto
  **any MCP server's tools** (argument/result mapping in CEL / JSON templates)
  or onto **plain HTTP GET/PUT/POST**. agentd links no database client. A
  killed process resumes from its last checkpoint: at-least-once for effects,
  exactly-once for state transitions, with idempotency keys on every effect.
- **A2A is the only external channel.** Other agents *and* users (their client
  is an agent) talk to agentd over authenticated A2A: chat about what it is
  doing, ask for status, steer the current work, start workflows/subagents,
  answer its questions. Operator control stays the `a2a.*` admin family. The
  served-MCP tool surface for peers ("agent as an MCP server for other
  agents") is removed; MCP serving is reduced to a read-only management/
  observability surface (or dropped — decision D7).
- **A complex, deterministic agent loop** in the supervisor process owns the
  state and dispatches everything: events in (A2A, triggers, steps, subagents,
  timers, MCP notifications, signals), decisions out. Intelligence is invoked
  only where judgment is needed — a user message, a `think`/`agent` node, a
  configured wake-up — and always in a **child process** ("turn worker") that
  never owns state: it proposes tool calls, the supervisor executes them. In a
  conversation the agent may **preflight** with `think` (classify the request,
  reason, draft a plan) and keeps a **per-conversation plan** it manages with
  `plan.*` tools — durable with the context, shown in every prompt, bound to
  the runs/subagents that carry the work.
- **Workflows are DAGs (dialect 3)** that begin at **start nodes** — the
  triggers: `once`, `loop`, `schedule`, `subscribe` (MCP resource), `a2a`,
  `manual` — with a rich node set: transforms, CEL, tool/MCP calls (tools,
  resources, prompts, completions), conditions and branches, `think` (prompt
  + structured output), memory, artifacts, `foreach`/`batch` without an
  element cap, structured iteration, subagents in sync/async/warm modes,
  `wait`/`sleep`/`await`/`human`, and **workflows spinning workflows** (a
  `workflow` node runs another workflow as a child run — sync, async or
  detached — with `signal`/`event` start nodes for cross-run orchestration)
  — every step durable, many runs concurrently, per-run and lifetime limits.
- **A token governor** paces intelligence spend against durable windowed
  budgets (per second/minute/hour/day, lifetime; per instance/run/
  conversation/principal): when a window is spent, work **waits** for the
  next window (durably), slows, degrades to a cheaper model, or refuses new
  intake — it never breaks.
- **Internal tools** (`instruction.*`, `subagent.*`, `code.run`, `memory.*`,
  `finish`, `ask_human`, `sleep`, `await`, `artifact.*`, `workflow.*`,
  `context.compact`, `think`) are *contracts* with JSON schemas: a built-in
  implementation by default, **overridable by a mapped MCP tool** and
  **disable-able** by config. The same registry serves the root agent, the
  workflow engine and subagents (precedence internal > code > MCP).
- **Knowledge, search and skills** are first-class but **remote**: `knowledge.*`
  tools front a RAG MCP server over open-format documents (Markdown/PDF/…),
  `search.*` tools front a search MCP server (mapping-only, with default MCP
  profiles so a compliant server needs no mapping); **skills** are instruction
  bundles discovered from MCP servers (latest protocol: prompts/resources),
  catalogued in every prompt and **preloaded into context when referenced**
  in the instruction or a chat message (`@skill:name`) or when the model asks.
- **Structured I/O everywhere** (JSON Schema on tools and nodes; MCP
  `structuredContent`/`outputSchema`; provider structured output for `think`;
  A2A DataParts for commands), **OTEL end-to-end** (traces, metrics, logs)
  plus an **audit trail** of principals × actions × state transitions.

This is a breaking major (**agentd 2.0.0**), delivered in **eight phases**
(§6), each leaving a green, releasable tree, tracked in `progress.md`.

---

## 1. Requirements and how this plan reads them

| # | Requirement (Andrii, 2026-08-16) | Reading adopted here | Where |
|---|---|---|---|
| R1 | Works in multiple modes (as today) | **Replaced by I1**: modes dissolve into triggers + one lifecycle policy; the shapes (once / loop / reactive / schedule / workflow) remain expressible and get sugar. | §3.6.6, §3.13, §9 |
| R2 | Other agents and users communicate via A2A (users' clients act as agents) | A2A is the sole external channel; each user/agent is an authenticated principal with an A2A context (conversation) and tasks. | §3.9 |
| R3 | Durable agent: store/restore state in a remote store configured as MCP tools (dynamic mapping, CEL) or REST GET/POST; no DB clients | A store **adapter contract** with two adapters (`mcp` mapped, `http`); one durable **state model** with write-ahead of events and per-entity checkpoints; a documented **restore protocol**. | §3.4, §3.5, §5 |
| R4 | Multiple MCP servers by config | Kept; extended with per-server tool namespaces, resource subscriptions as triggers, prompts/completions. | §3.10 |
| R5 | Internal tools (list) — overridable by MCP tools with mapping; some disable-able | An internal **tool registry** of schema'd contracts; `tools.overrides` (mapping) + `tools.disabled`. `code.run` is mapping-only (no built-in local execution). | §3.7 |
| R6 | Maintains its own context; self-compaction | Root context (self) + per-conversation threads; automatic and tool-driven compaction; durable versions. | §3.8 |
| R7 | Workflows: DAGs — transforms, CEL, tool/MCP calls, conditions, branches, think + condition on result, memory, MCP resources/entities, subagents in modes with await | Workflow **dialect 3**: DAG + structured iteration; the node catalogue in §3.6.3. | §3.6 |
| R8 | Via A2A a user talks about the work, asks status, steers, starts work | Message routing: natural language → root turn with tools; structured DataPart commands → deterministic; steering = messages into running work. | §3.9.3 |
| R9 | Base settings, workflows, MCP tools defined so it "just starts and works" | Config schema **v2** (nested; on the mechanism built 2026-08-16: YAML/JSON, multi-file, path env/flags, hot reload). | §3.12 |
| R10 | Workflow processing durable | Every step is a durable transition (§3.6.4); foreach/batch progress recorded per batch. | §3.6.4 |
| R11 | A2A authenticated | mTLS / bearer today; **principals + roles + authorization matrix**, AAuth signature verification later; audit of every principal action. | §3.9.5, §3.14 |
| R12 | All controlled by a complex agent loop | The **event loop** in the supervisor (§3.3): events, wake policy, turn lifecycle, effect executors, checkpoints. | §3.3 |
| R13 | Multiple workflows running | A run registry with concurrency/limit policies; runs are first-class durable entities. | §3.6.5 |
| R14 | Remove anything ACP; A2A only | No "ACP" exists in the tree (grep). Read as: **remove non-A2A agent-communication surfaces** — the served-MCP `subagent.*`/`status` tools for peers and the "agent-as-MCP-server for composition" posture. **Please confirm (D1).** | §3.15, §8 |
| R15 | Rich node set; foreach over huge arrays without an element cap; batch node to parallelize | `foreach` streams elements (artifact-backed), `batch` groups + parallelizes; per-batch durability; large outputs auto-externalized to artifacts. | §3.6.3, §3.6.4 |
| R16 | Complete OTEL observability, logging, audit | OTLP traces + metrics + logs (hand-rolled JSON exporter, as today's traces), audit event stream, served read surface. | §3.11 |
| R17 | Structured inputs/outputs wherever possible, esp. MCP | JSON Schema on every tool/node; MCP `structuredContent`/`outputSchema`; provider structured output; A2A DataParts. | §3.7.4, §3.6.2 |
| I1 | **Remove all modes**: started ⇒ runs the workflows, maintains context, replies/acts on A2A | Adopted as the organizing principle (§3.1). | §3.1, §3.13 |
| R18 | Knowledge (a remote MCP server doing RAG over open-knowledge-format documents) and search tools | `knowledge.*` / `search.*` internal tool contracts, mapping-only, with **default MCP profiles**; optional retrieval into the turn context; usable from workflows. | §3.16 |
| R19 | Skills: define skill sources, load via MCP (latest protocol version), reference them in the instruction or chat messages, agent preloads them | Skill catalogue from MCP prompts/resources; `@skill:name` references and `skills.load` preload the body into the context (progressive disclosure); per-context loaded set is durable. | §3.16, §3.8 |
| R21 | Intelligence budgets over time (daily, hourly, per second, other tactics) to control burn; slow down, await the next window — never breaking | The **token governor** (§3.17): durable windowed counters (tokens/requests), scopes, tactics `wait \| slow \| degrade \| refuse \| fail`, `waiting_budget` as a durable state with a timer. | §3.17 |
| R22 | Workflows start from **start nodes of different kinds** (loop, once, schedule, subscribe-to-resource, …) — the triggers | The DAG begins at one or more **start nodes**: `once`, `loop`, `schedule`, `subscribe`, `a2a`, `manual`; the trigger payload is `steps.<start>.output`; per-start concurrency/ownership options. | §3.6.6 |
| R23 | A workflow may spin another workflow as a node; review what nodes/triggers may be missing | `workflow` node (child run: `sync \| async \| detached`), `signal`/`event` start nodes, `race`, `chunk`, `cache`, `assert`, `fail`, intelligence presets, `workflow.signal/wait/cancel` — the completeness pass in §3.6.3 and §3.6.6, with a "deliberately absent" list. | §3.6.3, §3.6.6 |
| R20 | In conversation the agent can `think` to preflight/reason and create a plan; tools to manage its own temporary conversation plan | A configurable **preflight** phase (`think` with a structured verdict) before acting, and **`plan.*` tools** over a per-context plan (items with status, notes, bindings to runs/subagents), durable in the context state and rendered into every prompt. | §3.3.1, §3.7.2, §3.8.4 |

---

## 2. Where we are — the as-built inventory this plan reuses

Verdicts: **keep** (as is), **evolve** (extend in place), **replace** (rewrite
with a new contract), **remove**.

| Area | Today (module) | Verdict | Notes |
|---|---|---|---|
| Config mechanism (YAML/JSON, multi-file merge, path env/flags, hot reload) | `config/{mod,file,yaml,paths,watch}.rs` (2026-08-16) | **keep** | The parameter SET is redefined (§3.12); the mechanism is exactly what v2 needs. |
| Typed `Config` + `--validate-config`/`--config-schema`/`--capabilities` | `config/mod.rs`, `capabilities.rs` | **evolve** | Schema v2; manifest `surfaces{}` gains `store`, `tools`, `workflows`, `a2a.auth`. |
| Supervision: spawn/kill ladder/reaper/liveness/cgroup/restart governor/subreaper/PDEATHSIG | `supervisor/*` | **keep** | The v2 loop is built on the same reactor + tree primitives. |
| Supervisor↔child control protocol (length-prefixed JSON frames) | `subagent/protocol.rs`, `subagent/control.rs` | **evolve** | Adds `ToolRequest/ToolResult` (child proposes, supervisor executes), `Checkpoint` frames, `Role` (root turn / subagent / workflow). |
| Nested subagent orchestration inside the child (`Orchestrator`) | `subagent/orchestrator.rs` | **replace** | Spawning moves to the supervisor (flat tree, D3); the orchestrator's caps/limits/distillation logic is reused. |
| ReAct loop (`run_loop`, `Session`, stop conditions, action dispatch) | `agentloop/*` | **evolve** | Becomes the turn-worker body: same loop, tools resolved via the registry, internal tools round-trip to the supervisor. |
| Modes + reactive router (exactly-one-owner, debounce/coalesce, spawn vs continue), timers, warm registry | `triggers/{mode,router,timer,warm}.rs` | **replace** | Semantics preserved as **trigger** options + subagent `warm` mode; `Mode` and mode drivers removed. |
| Workflow model + driver + exec (dialect 2, cycles, blackboard, checkpointer profile) | `graph/{mod,driver,exec,sha}.rs` | **replace** (engine) / **keep** (pieces) | Dialect 3 is a DAG engine with per-step durability; `resolve_refs`, `Pred`, `check_schema`, `strict_check`, the lane engine, `parse_json_answer`, the human-gate wait/A2A bridge are reused. |
| CEL seam | `cel.rs` | **keep/promote** | Central to mapping/conditions/transforms; ship in the release build (D2). |
| Code-registered tools | `tools.rs` | **keep** | Third tier of the registry (internal > code > MCP). |
| MCP client (legacy + modern dialects, subscriptions, tasks ext, prompts/completions, signer seam) | `crates/mcp`, `mcp/{mod,auth,oauth}.rs` | **keep** | Plus per-server tool namespaces and `outputSchema` capture. |
| Served self-MCP (HTTP(S) listener, sessions, tools `status`/`subagent.*`, `agent://` resources, events ring, hot-config view) | `mcp/server.rs` (5.4K) | **evolve/shrink** | Listener + auth + A2A dispatch stay; peer **tools** removed (D1); resources become the management read surface (D7). |
| A2A server (SendMessage/GetTask/Cancel/streaming/admin, human gate `input-required`) + client (`a2a.delegate`) | `mcp/a2a.rs`, `mcp/a2a_client.rs` | **evolve** | Conversations (contextId), principals, command DataParts, task durability, steering, agent card. |
| Intelligence (OpenAI/Anthropic adapters, failover, breaker, discovery, hot swap, signing) | `intel/*` | **keep** | Plus provider structured-output support for `think`. |
| Observability (JSON lines, event ring, health, metrics schema 1.1, OTLP traces, report) | `obs/*`, `report.rs` | **evolve** | OTLP metrics + logs exporters, audit stream, v2 event vocabulary. |
| Cluster (shard, work-claim, standby) | `cluster/*` | **evolve** (D8) | Becomes trigger-level ownership options (`claim`, `shard`), or deferred. |
| AAuth (client signing) | `aauth/*` | **keep** | Later: verify AAuth signatures on inbound A2A (R11 roadmap). |
| Security posture (trifecta gate, SSRF, secrets, no local exec) | `sec/*`, `net/ssrf.rs` | **keep** | `code.run` stays mapping-only. |
| Knowledge / search / skills | — (nothing today; MCP prompts/resources/completions client calls exist) | **new** | Contracts + mappings + skill catalogue on top of the existing MCP client. |
| Conformance suite | `crates/agentd-conformance` | **evolve** | v2 families: durability/crash-restore, A2A conversation, store adapters, tools, skills. |

---

## 3. Target architecture

### 3.1 The model in one paragraph

An **agentd instance** = one supervisor process (durable coordinator) + child
processes (turn workers, subagents), one **state store** (remote), N **MCP
servers**, one **A2A endpoint**. The supervisor runs the **agent loop**: it
consumes events, applies deterministic policy, invokes intelligence in children
when judgment is required, executes effects through the **tool registry**, and
checkpoints every transition. **Workflows** are DAG programs the loop schedules;
**the root agent** is the LLM-driven persona that talks to principals and
steers the instance using the same tools; **subagents** are children with a
narrowed brief. All state that matters survives the process (§3.4).

```
                        A2A (users, agents, operator)          triggers: cron · MCP resource · A2A · timer
                                   │                                          │
   ┌───────────────────────────────▼──────────────────────────────────────────▼───────────────┐
   │ SUPERVISOR  (never reasons; owns state; single writer)                                    │
   │  agent loop:  events → policy → effects → checkpoint            ┌──────────────┐          │
   │  ├ run registry (workflow engine v3, DAG steps)                  │ tool registry│          │
   │  ├ conversations (A2A contexts, root context, compaction)        │ internal>code>MCP        │
   │  ├ subagent registry (children, limits, results)                 └──────┬───────┘          │
   │  ├ timers/waits (durable, absolute deadlines)                           │                  │
   │  └ store adapter ── put/get/list/delete ── remote store (MCP tools | HTTP)                 │
   └──────┬───────────────────────┬────────────────────────────────────────────┬────────────────┘
          │ spawn (re-exec)       │ spawn                                      │ HTTPS
   ┌──────▼──────┐        ┌───────▼─────┐                               ┌──────▼──────┐
   │ turn worker │        │ subagent    │  … flat: all children of      │ MCP servers │
   │ (one LLM    │        │ (agent loop │  the supervisor (D3)          │ tools/res.  │
   │  turn; tool │        │  or workflow│                               └─────────────┘
   │  requests ↔)│        │  driver)    │
   └─────────────┘        └─────────────┘
```

### 3.2 Process model

- **Supervisor** (the `agentd` PID): loads config, connects the store,
  restores, connects MCP servers, arms triggers, serves A2A, runs the loop.
  Never calls the LLM. Single writer of durable state.
- **Turn worker** (child): runs **one intelligence turn** (root conversation
  turn, `think` node, or an `agent` node's bounded run) with the context slice
  the supervisor hands it. It calls MCP tools itself (effects inside a turn are
  the turn's own at-least-once unit) but **internal tools are round-tripped**
  to the supervisor (`ToolRequest` → executes → `ToolResult`) so state changes
  are made by the state owner. Result: messages + usage + proposed effects.
- **Subagent** (child): today's payload model (instruction, output contract,
  context seed, narrowed servers/limits) running the agent loop or a workflow;
  modes `sync | async | detached | warm` (§3.7.2). Reports `Turn`/`Result`
  frames; may issue internal `ToolRequest`s within its grant.
- **Flat tree (D3):** every child is a direct child of the supervisor;
  delegation depth is *logical* (a subagent asking for a subagent goes through
  the supervisor). Rationale: the durability owner sees every child; restore
  can re-spawn from durable payloads; kill/limits are uniform. Per-child
  cgroups, PDEATHSIG, the kill ladder and the reaper are unchanged.

### 3.3 The agent loop (R12)

A single-threaded reactor over an event queue (today's `mpsc` + 200 ms tick).
Effect executors run off-loop (a bounded thread pool for MCP/HTTP calls; child
processes for LLM turns and subagents) and post completion events.

**Events** (all carry `trace`, `principal?`, `ts`; the durable ones are
written to the **inbox** before they are acted on — §3.4.3):

| Event | Source | Durable? |
|---|---|---|
| `A2aMessage {ctx, task?, principal, parts}` | A2A server | yes (ack after persist) |
| `A2aControl {cancel/pause/resume/drain/…}` | A2A admin | yes |
| `TriggerFired {trigger, payload}` | cron/timer/MCP notification/A2A | yes |
| `StepReady/StepDone {run, step, outcome}` | workflow engine / executors | via run state |
| `TurnDone {ctx/run, messages, usage, effects}` | turn worker | via context/run state |
| `ToolRequest {child, call}` | turn worker / subagent | no (in-turn) |
| `SubagentEvent {id, Ready/Turn/Result/Failed/Gate}` | children | via subagent record |
| `TimerFired {timer}` | timer wheel (durable deadlines) | timer record |
| `McpNotification {server, method, params}` | MCP clients | only when it maps to a trigger/wait |
| `StoreConflict/StoreDown` | store adapter | — (policy) |
| `Signal {TERM, HUP, CHLD}` | signals | — |

**Wake policy** — which events wake the intelligence (root turn) vs. are
handled deterministically. Configurable (`agent.wake_on`), default:

- wake: `A2aMessage` from a `user`/`agent` principal that is not a structured
  command; `human` gate reply; `SubagentEvent::Result` addressed to the root;
  `WorkflowFinished` when `report: think`; an unhandled `WorkflowFailed`.
- deterministic (no LLM): triggers (start the run), step scheduling, timers,
  structured commands, status queries, operator control, MCP notifications.

**Turn lifecycle** (root): build the turn input (system: base instruction +
capabilities + tool list; context: self summary + conversation thread +
selected memory; the event) → spawn/reuse a turn worker → stream
`ToolRequest`s (executed as durable effects, each recorded under the turn) →
`TurnDone` → persist context delta → deliver replies (A2A message/artifacts,
at-least-once with message ids) → maybe compact (§3.8.2). Per-conversation
turns are serialized; conversations run in parallel up to `agent.max_parallel_turns`.

**Invariants:** the loop never blocks on I/O; state mutation happens only in
the loop; every mutation is followed by a checkpoint decision (§3.4.4); a
crashed child never loses committed state.

#### 3.3.1 Conversation preflight and the plan (R20)

Before the model answers a conversation event, the loop may run a **preflight**
`think` (structured, no tools) whose verdict shapes the turn:

```json
{ "intent": "chat | question | status | command | task | steer | clarify",
  "needs_plan": true, "plan": [{"title": "…", "detail": "…"}],
  "clarifications": ["…"], "risk": "low | medium | high",
  "tools_needed": ["workflow.run", "knowledge.search"], "skills": ["review-pr"] }
```

Policy `agent.preflight: never | auto | always` (default `auto`: preflight
when the message is longer than a threshold, mentions work verbs, or the
conversation has an open plan). The verdict is recorded on the context
(auditable "why did it do that"), can short-circuit trivial intents (`status`
→ deterministic answer, `clarify` → ask back without acting), preloads
`skills`, and — when `needs_plan` — seeds the **conversation plan** via
`plan.create`. The main turn then runs with the plan in its prompt; the model
advances it with `plan.update` (`in_progress`/`done`/`blocked`, notes), binds
items to the runs/subagents it starts (status propagates automatically when a
bound run/subagent finishes), and `plan.clear`s it when the goal is met. The
plan is **temporary by intent** (it belongs to the conversation, not to
memory) but **durable by construction** (part of `context` state, restored
with it, compacted with it — see §3.8.4).

### 3.4 Durable state (R3, R10)

#### 3.4.1 Entities and keys

All keys are `<prefix>/<instance>/<kind>/<id>` (prefix from config; instance =
`identity.instance` or `agent.name`). Each record is a versioned **envelope**:

```json
{ "v": 2, "kind": "run", "id": "…", "seq": 17, "ts": 1723800000000,
  "instance": "agentd-0", "hash": "<sha256 of the referenced definition, where applicable>",
  "state": { … kind-specific … } }
```

| kind | id | state (summary) | written when |
|---|---|---|---|
| `manifest` | `agent` | instance metadata, generation, live entity index `{kind,id,seq}`, start-node state (last fired, iteration, missed), **budget counters per window/scope**, lifecycle | on entity add/remove, budget settle (debounced) |
| `inbox` | event id (ULID) | a durable event awaiting processing (A2A message, trigger firing) | on receipt (before ack) → deleted/marked when processed |
| `context` | `root` \| `<a2a contextId>` | messages (compacted), summary blocks, token estimate, version | after each turn / compaction |
| `run` | run id | workflow hash + inputs, per-step `{status, attempt, started, finished, output \| artifact_ref, error}`, vars, pending waits/timers, budget | after each step (or batch), at suspension/terminal |
| `subagent` | handle | spawn payload, mode, status, result/distillate, attempt | on spawn/status change/result |
| `task` | A2A task id | A2A task state/history/artifacts, principal, linked run/subagent | on every task transition |
| `memory` | key | value + metadata (`ts`, `ttl`, `by`) | on set/delete (index record `memory/_index` if the store has no `list`) |
| `artifact` | artifact id | metadata + content (inline or chunked) | on create/delete |
| `timer` | timer id | absolute deadline, owner (run/step or ctx), payload | on arm/disarm |
| `audit` | ULID | append-only audit event (§3.11) | when audit sink = store |

#### 3.4.2 The store contract

```
put(key, seq, envelope)  → Ok | Conflict{latest_seq} | Err       // seq must be > latest (CAS)
get(key[, seq])          → Some(envelope) | None | Err
list(prefix)             → [ {key, seq} ]  (optional; absence handled by index records)
delete(key)              → Ok | Err       (optional; absence = tombstone via put)
```

Two adapters (§3.5), one behaviour: **`seq` conflict is fatal for the writer**
(a second instance owns the key — the split-brain guard, RFC 0021 §12).

#### 3.4.3 Write-ahead of events ("accept means durable")

An `A2aMessage` or `TriggerFired` is written to `inbox` **before** the loop
acts on it, and an A2A `SendMessage` is acknowledged (task id returned) only
after the write. Processing marks the inbox record done (or deletes it). On
restore, undone inbox records are re-delivered — at-least-once — with the
original event id, so any effect they cause carries a stable idempotency key.
Durability level per source is configurable (`store.durability.a2a: strict`,
`store.durability.steps: eventual{debounce}`) with the trade-off documented:
strict = one store round-trip on the request path.

#### 3.4.4 Checkpoint policy

- **Always**: inbox on receipt; run on step completion (or batch completion),
  suspension and terminal; context after a turn; task on transition; timer on
  arm; subagent on spawn/result; memory/artifact on write.
- **Debounced** (`store.checkpoint.debounce_ms`, default 250 ms): manifest;
  high-frequency step progress inside a `batch` (progress is also recovered
  from the batch's own records).
- **Failure policy** (`store.on_error`): `halt` (default for strict sources —
  refuse new intake, keep serving status, drain) or `degrade` (log
  `store.write.fail`, keep going, retry with backoff; the run report and
  `agent://status` show `durability: degraded`).

#### 3.4.5 Restore protocol (startup, §4.1)

1. `get manifest` → none ⇒ fresh instance (write manifest gen 1).
2. For each indexed entity, `get` latest; verify `hash` where the definition
   is referenced (a changed workflow definition ⇒ the run is `resume_policy`:
   `refuse | force | restart`); entities newer than the manifest are taken as
   authoritative (entity-first write order); listed-but-missing entities are
   marked `lost` (logged, audited).
3. Rebuild registries; re-arm timers from absolute deadlines (fire immediately
   if past); re-arm MCP subscriptions for pending waits/triggers; re-open
   in-flight A2A tasks as `working`; re-spawn subagents whose parent step is
   pending (`attempt+1`, from the durable payload); re-deliver undone inbox
   events.
4. Emit `restore.done {entities, runs_resumed, events_replayed, lost}` +
   audit; bump generation.

#### 3.4.6 Effects, replay, idempotency

- A **step** = record intent (`status: running, attempt: n`) → perform →
  record result. Crash between = at-least-once on retry.
- Every effect carries an **idempotency key** derived from `(instance, run,
  step, attempt)` (or `(ctx, turn, call)`) — surfaced to MCP servers as
  `_meta["agent/idempotency_key"]` (extends today's `agent/claim_key`
  convention) and to HTTP stores as a header — so a well-behaved server can
  collapse a replay.
- Per node `on_replay: retry (default) | skip | fail`; LLM calls are treated
  as retryable; `human` gates re-arm; `sleep` re-computes the remainder.

### 3.5 State store adapters (R3)

**Config (`store`)** — one of:

```yaml
store:
  kind: mcp                       # mcp | http | none (dev only; refuses to start with durability required)
  prefix: agentd                  # key prefix
  mcp:
    server: state                 # a declared MCP server
    put:  { tool: state.put,  args: 'CEL: {"key": key, "seq": seq, "state": envelope}', ok: 'result.structuredContent.ok == true', conflict: 'has(result.structuredContent.latest)' }
    get:  { tool: state.get,  args: '{"key": key, "seq": seq}', value: 'result.structuredContent.state' }
    list: { tool: state.list, args: '{"prefix": prefix}', keys: 'result.structuredContent.keys' }   # optional
    delete: { tool: state.delete, args: '{"key": key}' }                                        # optional
  # or:
  http:
    base_url: https://state.internal/v1
    headers: { authorization: "Bearer {{secret:STATE_TOKEN}}" }
    get:  { method: GET, url: "{base_url}/kv/{key}" , value: 'body' }
    put:  { method: PUT, url: "{base_url}/kv/{key}?seq={seq}", body: 'envelope', conflict_status: 409 }
    list: { method: GET, url: "{base_url}/kv?prefix={prefix}", keys: 'body.keys' }
    delete: { method: DELETE, url: "{base_url}/kv/{key}" }
```

- **Mapping language:** the values `key`, `seq`, `prefix`, `envelope`,
  `instance` are the adapter's canonical inputs; `args`/`body`/`url` are
  templates (`{name}` interpolation, or `CEL:` expressions when the `cel`
  feature is present); result extraction (`value`, `ok`, `conflict`, `keys`)
  is a JSON-pointer or CEL over `result`/`body`/`status`. The **default MCP
  mapping is today's checkpointer profile** (`state.put/get/list`), so the
  existing mock (`mcp/mock_http.rs`) and conformance `workmcp`/`confmcp`
  patterns carry over.
- **Timeouts/retries:** management-timeout class (short) with bounded retry;
  `Conflict` never retried.
- **What is not built:** a database driver, a schema, a queue. Anything
  beyond `put/get/list/delete` is the store server's business.

### 3.6 Workflow engine v3 (R7, R10, R13, R15)

#### 3.6.1 Definition (dialect 3)

A workflow is a **DAG** of named steps with explicit dependencies and
optional guarded edges. It begins at one or more **start nodes** — the
triggers (§3.6.6): `once`, `loop`, `schedule`, `subscribe`, `a2a`, `manual`.
Each firing of a start node instantiates a *run* of the DAG whose
`steps.<start>.output` is the trigger payload. Long-lived behaviour comes from
start nodes and **structured iteration** (`foreach`, `batch`, `iterate` with a
bounded body sub-DAG) — never from back-edges. Runs are durable, concurrent,
and addressable.

```yaml
workflows:
  - name: triage
    armed: true                                  # arm the start nodes at boot/restore (default true)
    inputs: { schema: {…json schema…} }          # validates the start payload / A2A inputs
    concurrency: { max_runs: 4, on_overflow: queue }   # queue | drop | replace
    limits: { steps: 500, tokens: 2_000_000, deadline: 1h, budget: { windows: [{ per: hour, tokens: 200000 }] } }
    steps:
      inbox:   { kind: subscribe, server: queue, uri: queue://inbox, debounce_ms: 500, coalesce: true,
                 claim: { server: coord, ttl: 30s }, deliver: run }         # start node: each update = a run
      hourly:  { kind: schedule, cron: "0 * * * *", catch_up: one }         # start node: cron
      asked:   { kind: a2a, command: triage }                               # start node: a command / NL intent
      fetch:   { kind: mcp.tool, depends_on: [inbox, hourly, asked], server: queue, tool: pop, args: { n: 50 }, output_schema: {…} }
      each:    { kind: foreach, over: '{{steps.fetch.output.items}}', batch: { size: 10, parallel: 4 },
                 body: { steps: { classify: { kind: think, prompt: "…{{item}}…", output_schema: {…} },
                                  route:    { kind: switch, on: '{{steps.classify.output.kind}}', cases: { bug: file_bug, other: ignore } },
                                  file_bug: { kind: subagent, mode: sync, instruction: "…", output_contract: {…} },
                                  ignore:   { kind: assign, value: { skipped: true } } } },
                 on_error: continue, collect: { into: results, mode: append } }
      summary: { kind: think, depends_on: [each], prompt: "summarize {{steps.each.output}}", output_schema: {…} }
      notify:  { kind: a2a.send, to: user:andrii, parts: [{ data: '{{steps.summary.output}}' }] }
      done:    { kind: finish, status: completed, output: '{{steps.summary.output}}' }
```

A start node with several siblings: a run starts from whichever fired
(`depends_on: [inbox, hourly, asked]` on the first real step means "any of
them" — the fired one is `run.start`, the others are `skipped`). Data flow:
`inputs`, `run.start` (which start node fired + payload), `steps.<id>.output`,
`vars` (from `assign`), `memory.<key>` (read), `item`/`index`/`batch` inside
iteration bodies, `env` (a curated, secret-free view). Templates: `{{…}}` interpolation with JSON-pointer/dotted
paths and defaults (dependency-free); **CEL** for expressions (`when:`,
`on:`, `value: 'CEL: …'`, `assert:`). Every step: `depends_on`, `when` (CEL
guard; skipped steps count as `skipped`), `retry {max, backoff}`,
`timeout`, `on_error: fail | continue | goto:<step>`, `idempotent`, `output_schema`.

#### 3.6.2 Structured I/O (R17)

Step outputs are JSON; `output_schema` validates them (schema failure = error
outcome, retried per policy). `mcp.tool` prefers `structuredContent` and uses
the server's `outputSchema` when present; `think` requests **provider
structured output** (OpenAI `response_format: json_schema` / Anthropic tool
forcing) with schema re-ask as fallback; `subagent` results honour the
`output_contract` schema; workflow `inputs.schema` validates run inputs (A2A
DataParts, trigger payloads).

#### 3.6.3 Node catalogue

Grouped; every step also carries the cross-cutting features listed after the
table. "Preset" = sugar over `think` with a fixed prompt frame + output schema.

**Start nodes (the triggers — one or more per workflow, §3.6.6)**

| Kind | Fires a run when… | Options |
|---|---|---|
| `once` | the workflow is armed (boot/restore) or `workflow.run` is called | `policy: ensure \| always` |
| `loop` | the previous run finished (re-run continuously) | `interval`, `delay`, `until` (CEL over the last outcome), `max_iterations`, `backoff` on failure; durable iteration counter |
| `schedule` | cron / interval, independent of run completion | `cron`, `every`, `tz` (default UTC), `jitter`, `catch_up: none \| one \| all`, `at` (one-shot at a time) |
| `subscribe` | an MCP resource updates (notify-then-read) | `server`, `uri`, `debounce_ms`, `coalesce`, `filter` (CEL over the read), `claim`, `shard`, `deliver: run \| wait`, `on_no_listener: run \| drop` |
| `signal` | a named signal arrives (`workflow.signal` from another run/tool, an A2A command, a subagent) | `name`, `filter` (CEL over payload), `deliver: run \| wait` |
| `event` | an internal event matches: `workflow.finished`/`failed`, `subagent.finished`, `budget.exhausted`/`resumed`, `config.reloaded`, `restore.done`, `human.timeout`… | `on`, `filter` (CEL) — reactive orchestration ("on failure, notify") |
| `a2a` | a principal's message: a command DataPart or an NL intent the root routes here; a peer's `SendMessage` | `command`, `roles`, `inputs` (CEL over the message) |
| `manual` | only `workflow.run` (tools/commands) | — |

**Control flow**

| Kind | Purpose | Notes |
|---|---|---|
| `switch` | multi-way branch on a value/CEL | cases → step; `default` |
| guarded edges (`when`) | conditional dependency on any step | skipped steps are `skipped`, not failed |
| `parallel` | static fan-out branches, fan-in object | one lane pool with foreach |
| `foreach` | dynamic fan-out over an array (no element cap) | sequential by default; `batch {size, parallel, rate}` groups elements and runs batches concurrently (optionally rate-paced); per-batch durable progress; `collect` reducer; artifact-backed |
| `batch` | explicit batching over an array (variant of foreach) | `by: key` (group), `size`, `parallel`, `rate` |
| `iterate` | bounded structured loop (`while`/`until` CEL, `max_iterations`) with a body sub-DAG | replaces cycles (distinct from the `loop` start node) |
| `race` | run branches, keep the first to finish, cancel the rest | timeouts, `min_success` |
| `join` | fan-in of async workflows/subagents/subgraphs | timeout, partials, `min` |
| `subgraph` | an inline sub-DAG in the same run (grouping/scoping, foreach/iterate bodies) | shares the run's durability |
| `workflow` | **spin another workflow as a child run** — `mode: sync` (wait for it), `async` (get a run id; `join`/`workflow.wait` later), `detached` (fire and forget) | `name`, `inputs` (CEL/template), `start` (which start node), `version` (hash pin), `on_error`; the child is its own durable run; the parent step records the child id; cancellation propagates per `cascade: true` |
| `wait` | suspend until: an MCP resource updates \| a CEL condition over memory/resources holds \| a **signal** arrives \| a **run** or **subagent** finishes \| an A2A message arrives on the owning conversation \| a deadline | durable; `timeout` edge |
| `sleep` | durable timer | absolute deadline recorded |
| `assert` | a CEL condition must hold, else `error` outcome | cheap guardrails |
| `fail` | raise a deliberate error (`message`, `code`) | drives `on_error` routing |
| `noop` | structural placeholder | |
| `checkpoint` | explicit checkpoint / named savepoint | rarely needed |
| `finish` | terminal (status, output) | maps to exit codes for job-shaped runs |

**Data**

| Kind | Purpose | Notes |
|---|---|---|
| `assign` / `transform` | data shaping | template or CEL; `writes` var; reducers append/merge/union |
| `map` / `filter` / `reduce` / `sort` / `dedupe` | array ops without a model | CEL element expressions; artifact-backed inputs stream |
| `chunk` | split text (by tokens/chars/lines) or arrays into chunks | the usual step before `foreach`/`batch` over a large document |
| `template` | render text from a template (prompt building, messages) | `{{…}}` + CEL; output string |
| `parse` | text → JSON/YAML/CSV/lines | for tool results that arrive as text |
| `validate` | JSON-Schema-check a value | error outcome on miss |
| `memory.get/set/list/delete` | agent memory | via the registry (overridable) |
| `artifact.create/get/delete` | artifacts | store-backed; A2A artifact delivery |
| `knowledge.search/get` · `search.query/fetch` | knowledge/search (§3.16) | via the registry (mapping-only) |

**Integration**

| Kind | Purpose | Notes |
|---|---|---|
| `mcp.tool` | call an MCP tool | server-qualified; `_meta` idempotency key; structured result |
| `mcp.resource` | `resources/read` / `resources/list` / `prompts/get` / `completion/complete` | MCP entities as data |
| `tool` | call an **internal** or code tool by name | same registry, same schemas |
| `a2a.send` / `a2a.delegate` / `a2a.wait` | message a principal / delegate to a remote agent (task) / wait for a message on a conversation | outbound auth |
| `workflow.signal` / `workflow.wait` / `workflow.cancel` | signal a named event into a run (or start a `signal` workflow) / await a run / cancel one | cross-run coordination |
| `emit` | append a note to the root context / an audit event / a custom metric | wake policy aware |

**Intelligence & agents**

| Kind | Purpose | Notes |
|---|---|---|
| `think` | one structured intelligence call | prompt + `output_schema` + `check` (CEL) + retries; no tools; `skills:` preload |
| `classify` / `extract` / `summarize` / `judge` / `route` | **presets** over `think` | enum output; schema extraction; summary with length; rubric verdict; semantic switch (today's Tier-2 branch) |
| `agent` | a bounded agentic run (ReAct with tools) in a turn worker | limits, tools grant, output contract, `skills:` |
| `subagent` | spawn a subagent: `mode: sync \| async \| detached \| warm` | `subagent.send` messages to warm; `subagent.await`/`join`; `subagent.kill` |
| `human` | ask a human (A2A `input-required` on the owning task, or a mapped tool) | reply is data; timeout edge |

**Cross-cutting step features:** `depends_on`, `when` (CEL guard), `retry {max,
backoff}`, `timeout`, `on_error: fail | continue | goto:<step>`, `idempotent`,
`on_replay`, `output_schema`, `cache {key: CEL, ttl}` (memoize a step's output
by input hash — a replay or a repeated identical input skips the effect;
durable), `budget` (per-step token cap), `skills`, `otel {attributes}`,
`description`.

**Deliberately absent:** a generic `http` node (egress goes through MCP
servers or `search.fetch` — the universal-interface posture), local code
execution (`code.run` is mapping-only), back-edges/cycles (use `iterate` /
`loop` start), inbound webhooks other than A2A.

Validation (author time, fail closed): at least one start node and no
non-start root; DAG acyclicity, dependency existence, schema well-formedness,
CEL compile-check, tool existence (registry incl. disabled), server existence,
`finish` reachability, caps (steps ≤ 512 per graph, nesting ≤ 4,
`batch.parallel ≤ 8`, `iterate.max_iterations ≤ 10_000`).

#### 3.6.4 Durability of runs

- Run state (§3.4.1) is written after every completed step; `foreach`/`batch`
  record progress **per batch** (`done_batches`, `partial_results` reference)
  so a crash resumes at the next batch, never from element 0.
- Large step outputs (> `workflow.inline_max_bytes`, default 64 KiB) are
  written as **artifacts** and referenced (`{"$artifact": id}`); templates
  dereference transparently; `foreach.over` streams from an artifact.
- Suspensions (`wait`, `sleep`, `human`, `join`) persist their absolute
  deadline and their arm parameters; restore re-arms them.
- The run's `workflow_hash` binds it to its definition; `resume_policy`
  governs mismatch (`refuse` default; `force` re-validates and continues).

#### 3.6.5 Many runs

A **run registry** (durable) with per-workflow `concurrency` policy, global
`limits.max_runs`, per-run limits + the instance lifetime token budget
(`budget.rs`), status/cancel/pause/resume via internal tools and A2A commands,
`agent://runs` read surface. Runs are A2A tasks when started by a principal.

#### 3.6.6 Start nodes are the triggers (replacing modes and the reactive router)

The five 1.x shapes map onto start-node kinds: `once` (one-shot),
`loop` (re-run on completion, `interval`/`until`/`max_iterations`/`backoff`),
`schedule` (cron/interval, `catch_up`), `subscribe` (MCP resource,
notify-then-read; **debounce/coalesce** and **exactly-one-owner** routing
preserved from `triggers/router.rs`; `claim`/`shard` carry the cluster
semantics — D8; `deliver: run | wait`), `a2a` (command or intent), `manual`;
plus two v2 kinds for orchestration between workflows: `signal` (a named
event from `workflow.signal`, an A2A command or a subagent) and `event` (an
internal lifecycle event with a CEL filter — "when any run fails, run the
notifier").
Each firing = a run whose `run.start` carries the payload; per-start-node
options: `concurrency` override, `inputs` mapping (`CEL:` over the payload).
Start-node state (last fired, iteration, missed firings) is durable in the
manifest; `armed: false` (or `workflow.pause`) disarms a workflow's start
nodes without deleting it. A run started by `workflow.run` uses the `manual`
start (or an explicit `start:` argument).

### 3.7 Tools (R5, R17)

#### 3.7.1 The registry

One registry with three tiers, dispatch precedence **internal > code > MCP**
(unshadowable orchestration surface, RFC 0022 §4 kept). Every tool has
`name`, `description`, `input_schema`, `output_schema`, `class`, `grant`
(who may call: root / workflows / subagents / A2A principals). The LLM-facing
tool defs are generated from the registry (dotted names wire-sanitized, as
today).

#### 3.7.2 Internal tools (contracts)

| Tool | Args (schema summary) | Semantics | Built-in? | Notes |
|---|---|---|---|---|
| `instruction.read` | `{}` | the current instruction — `agent.instruction` is ONE field: a value that is a single-token URI (`scheme://…`) which a configured MCP server serves (`mcp://<server>/<uri>` or a URI a server lists) is **read + subscribed** at startup and on reload; anything else is the static text | yes | one field, parsed — no separate `instruction_uri` |
| `instruction.subscribe` | `{uri?}` | (re)subscribe to the instruction resource (or switch to another); an update re-reads and wakes the root (`instruction.updated` note) | yes | dynamic re-instruction |
| `subagent.run` | `{instruction, mode: sync\|async\|detached\|warm, workflow?, tools?, servers?, limits?, context?, output_contract?}` | spawn a subagent (flat) | yes | caps: depth (logical), breadth, rate, tree tokens |
| `subagent.send` | `{handle, message}` | A2A-style message into a warm subagent (steer) | yes | |
| `subagent.kill` | `{handle, reason?}` | cancel + kill ladder | yes | |
| `subagent.status` / `subagent.await` / `subagent.list` | | | yes | |
| `code.run` | `{language, code, files?, timeout?}` | run code **in a mapped MCP sandbox** | **no** (mapping-only; disabled unless mapped) | preserves "no local execution" (RFC 0012) |
| `memory.get` / `memory.set` / `memory.list` / `memory.delete` | `{key}` / `{key, value, ttl?}` / `{prefix?}` | durable KV in the store namespace | yes | overridable by e.g. a memory MCP server |
| `artifact.create` / `artifact.get` / `artifact.delete` / `artifact.list` | `{name, mime, content\|from_step}` | store-backed artifacts; A2A artifact delivery | yes | |
| `workflow.run` / `workflow.create` / `workflow.update` / `workflow.delete` / `workflow.list` / `workflow.status` / `workflow.cancel` / `workflow.pause` / `workflow.resume` | | run registry ops; definitions validated by `parse_graph` v3 | yes | `create/update/delete` may be restricted by grant |
| `ask_human` | `{question, schema?, to?, timeout?}` | A2A `input-required` on the conversation/task; reply as data | yes | overridable (e.g. a Slack MCP tool) |
| `sleep` | `{duration}` | durable timer | yes | |
| `await` | `{condition: CEL, on?: [resource\|memory\|step], timeout?}` | durable wait until a condition holds | yes | |
| `context.compact` | `{target_tokens?, keep_last?}` | compaction of the calling context | yes | |
| `think` | `{prompt, output_schema?, reads?}` | structured reasoning call, result appended/returned | yes | |
| `finish` | `{status, output?, reason?}` | terminate the calling unit (root: exit per lifecycle; run: halt) | yes | |
| `plan.create` / `plan.get` / `plan.update` / `plan.clear` | `{goal, items: [{title, detail?}]}` / `{}` / `{item, status?, note?, bind?: {run\|subagent}, insert?, reorder?}` / `{}` | the calling context's working plan (§3.3.1, §3.8.4); bound items track their run/subagent | yes | per context (conversation or root); rendered in every prompt |
| `knowledge.search` / `knowledge.get` / `knowledge.list` | `{query, top_k?, filters?}` / `{id\|uri}` / `{prefix?}` | RAG over the configured knowledge server's documents | **no** (mapping-only; default profile `knowledge.*`) | §3.16.1 |
| `search.query` / `search.fetch` | `{query, kind?: web\|docs\|code, limit?}` / `{url}` | web/doc search + page fetch through a search MCP server | **no** (mapping-only; default profile `search.*`) | §3.16.2; fetch happens server-side |
| `skills.list` / `skills.load` / `skills.unload` | `{}` / `{name, version?}` / `{name}` | skill catalogue; preload a skill body into the calling context; drop it | yes (over MCP prompts/resources) | §3.16.3 |
| `status` | `{}` | instance/run/subagent status | yes | also an A2A command |

#### 3.7.3 Overrides and disabling

```yaml
tools:
  disabled: [code.run, workflow.delete]
  overrides:
    memory.get:  { server: mem, tool: search, args: 'CEL: {"query": args.key, "limit": 1}', result: 'CEL: result.structuredContent.results[0].text' }
    ask_human:   { server: slack, tool: post_and_wait, args: '{"channel": "#ops", "text": "{{args.question}}"}', result: 'result.structuredContent.reply' }
    code.run:    { server: sandbox, tool: execute, args: '{"lang": "{{args.language}}", "code": "{{args.code}}"}', result: 'result.structuredContent' }
    knowledge.search: { server: kb, tool: rag_query, args: 'CEL: {"q": args.query, "k": has(args.top_k) ? args.top_k : 5}', result: 'CEL: {"hits": result.structuredContent.matches.map(m, {"id": m.doc_id, "uri": m.uri, "title": m.title, "score": m.score, "snippet": m.text})}' }
```

An override keeps the internal **contract** (name, schemas, semantics for the
callers) and swaps the **implementation**: args mapped to the MCP tool's
schema, result mapped back to the internal output schema (validated). Startup
validation: the server exists, the tool is advertised, the mapping compiles.
A disabled tool is not offered to models and fails workflow validation.

#### 3.7.4 Structured I/O rules

Internal tools validate args against `input_schema` (a hand-rolled JSON Schema
subset validator already exists for MCP results — reused) and results against
`output_schema`; MCP tools: `structuredContent` preferred, text JSON as
fallback; the LLM sees `outputSchema` when a server publishes it.

### 3.8 Context and memory (R6)

#### 3.8.1 Contexts

- **Root context** (`context/root`): the agent's own working memory — base
  instruction, capability notes, a rolling log of significant events (runs
  started/finished, failures, decisions), summaries. Single writer (the loop).
- **Conversation contexts** (`context/<a2a contextId>`): per principal thread;
  turns are serialized per conversation.
- A turn's prompt = system (instruction + registry + the **skill catalogue**
  names/descriptions + the bodies of the context's **loaded skills**) + root
  summary + the context's **plan** (§3.8.4) + the conversation thread +
  selected memory (`memory.list` by prefix, optional relevance later) +
  optional **knowledge retrieval** for the event (`knowledge.auto_context`) +
  the preflight verdict (if any) + the event.

#### 3.8.2 Compaction

Trigger: token estimate > `context.compact_at` (default 70 % of the model
window) or `context.compact` tool. Method: a `think` call summarizes older
messages into a **summary block** (structured: goals, decisions, open items,
facts), keeps the last `keep_last` messages verbatim, bumps the context
version, checkpoints. Also compact on `restore` if the model window changed.

#### 3.8.3 Memory

Durable KV (`memory/<key>`), JSON values, optional TTL, size caps, list by
prefix (store `list` or the `_index` record). Overridable by an MCP memory
server (§3.7.3). Exposed to workflows (`memory.<key>` reads; `memory.*` nodes)
and to models (tools).

#### 3.8.4 The context plan (R20)

Each context (root or conversation) may hold one **plan**: `{goal, created,
updated, items: [{id, title, detail, status: pending|in_progress|done|blocked|
skipped, note, bound: {run|subagent|task}?, updated}]}` — a small, ordered
checklist the model owns through `plan.*` (§3.7.2). It is stored in the
`context` record (durable, restored, versioned), rendered as a compact block
in the prompt (`Plan: 2/5 done — [3] in progress: …`), kept verbatim across
compaction (the summary block references it rather than absorbing it), auto-
advanced by bindings (a bound run/subagent reaching a terminal state marks the
item `done`/`blocked` with the outcome), cleared by `plan.clear` or when the
conversation ends, and capped (`context.plan.max_items`, default 32). The root
context's plan is the agent's own agenda across conversations (what it is
working on for whom); conversation plans are per-thread and never leak into
memory unless the model explicitly `memory.set`s a summary.

### 3.9 A2A surface (R2, R8, R11)

#### 3.9.1 Roles

Every A2A caller is a **principal** (identity from mTLS SAN / bearer subject /
AAuth agent id) with a **role**: `operator` (admin family, full control),
`user` (chat, start/steer own work, ask status), `agent` (peer: tasks per its
grant), `anonymous` (refused unless configured). Users' clients are agents:
same protocol, role by config (`a2a.principals`).

#### 3.9.2 Objects

- **Conversation** = A2A `contextId` → `context/<id>` (durable thread).
- **Task** = a unit of work a principal started: a root turn's answer (short
  task), a workflow run, a subagent. Task ids are stable across restarts
  (`task/<id>` records); `GetTask` after a restart works.
- **Artifacts** = `artifact/*` delivered on tasks; **streaming** via
  `SendStreamingMessage` (status/progress/artifact frames from run/turn events).

#### 3.9.3 Message routing

1. Structured **command DataPart** (`{"agentd": {"op": "workflow.run", "name": "triage", "inputs": {…}}}`,
   `op ∈ registry tools granted to the role`) → executed deterministically,
   audited, no LLM. Status: `{"op": "status"}`. Steering:
   `{"op": "subagent.send", …}` / `{"op": "workflow.cancel", …}` /
   `{"op": "instruction.update", …}`.
2. **Natural language** → root turn (wake policy) with the tools the role
   grants; the model replies and/or acts (start a run, steer, ask back).
3. A message carrying `taskId` of a **waiting `human` gate** → the gate reply
   (as today), first-signal-wins.
4. Everything is a durable inbox event first (§3.4.3).

#### 3.9.4 Agent card & discovery

`/.well-known/agent-card.json` (A2A) generated from config + registry: skills
= the workflows whose start nodes accept A2A (`a2a`/`manual`/`signal`) + `chat`;
security schemes = configured auth;
capabilities = streaming, artifacts. `agentd --capabilities` stays the
control-plane manifest.

#### 3.9.5 Authentication & authorization (R11)

Inbound: mTLS client CA and/or bearer (today) → **principal resolution**
(`a2a.principals: [{match: {san|sub|bearer_ref}, role, grants}]`), method/tool
authorization matrix, per-principal rate limits, audit of every call. Roadmap:
AAuth signature verification (agentd already signs outbound; add the verify
half via `ring`), OAuth 2.1 bearer introspection through a mapped MCP tool
(no OAuth server client built in). Outbound (peers): bearer / mTLS / AAuth
signing.

### 3.10 MCP servers (R4)

Kept as configured today (`mcp.servers[]`, HTTPS, headers with secret refs,
tags, OAuth client-credentials, AAuth). Additions: per-server **tool
namespace** (`ns: fs` ⇒ tools appear as `fs.read`; collisions impossible),
`outputSchema` capture, `resources/subscribe` as **triggers/waits**,
`prompts/get` + `completion/complete` as `mcp.resource` node ops, `tools/list_changed`
refresh (exists for warm sessions) applied registry-wide, and the tool
overrides (§3.7.3). Reload semantics unchanged (re-handshake at the quiesce
boundary).

### 3.11 Observability & audit (R16)

- **Traces:** one trace per inbox event; spans: `agent.turn`, `workflow.run`,
  `workflow.step`, `tool.call` (internal/MCP), `subagent`, `store.put/get`,
  `a2a.request`; GenAI semconv retained; context propagated to children (as
  today) and to MCP servers/A2A peers (`traceparent`).
- **Metrics:** today's schema (1.1) + `agentd_runs{workflow,status}`,
  `agentd_steps_total{kind,status}`, `agentd_store_ops_total{op,result}`,
  `agentd_store_latency`, `agentd_inbox_pending`, `agentd_turns_total{ctx_kind}`,
  `agentd_context_tokens`; exported by `/metrics` and **OTLP metrics**
  (hand-rolled JSON exporter, same as traces).
- **Logs:** JSON lines (closed vocabulary v2) + **OTLP logs** export
  (optional), correlated by trace/run/step.
- **Audit:** an append-only event stream `audit.*` — `{ts, principal, role,
  action, target, outcome, request_id, trace}` for every A2A call, tool call
  by a principal, config reload, store conflict, restore, kill — emitted as
  logs and optionally persisted to the store (`store.audit: true`).
- **Read surface:** `agent://status|runs|run/<id>|conversations|subagents|
  store|config/effective|events` — served over the management listener (MCP
  resources, D7) and mirrored as A2A `status` command output.

### 3.12 Configuration schema v2 (R9)

Built on the mechanism landed 2026-08-16 (`config/{file,yaml,paths}` — YAML/JSON,
multi-file merge, `AGENTD_<PATH>` env, `--<path>` flags). The parameter set
becomes one nested document; the named flags of v1 become **aliases** of paths
(kept for the common ones: `--instruction`, `--config`, `--intelligence`,
`--model`, `--mcp`, `--log-level`, …) so the quickstart still reads naturally.

```yaml
agent:
  name: triage-bot                    # instance identity (falls back to downward API / hostname)
  preflight: auto                     # never | auto | always — a structured `think` before acting on a message
  instruction: |                      # static text — or a single resource URI a configured MCP
    You are …                         # server serves (e.g. `mcp://docs/agent-instruction`, `docs://agent`):
                                      # then agentd reads it and subscribes; an update re-instructs (§3.7.2)
  wake_on: [a2a_message, human_reply, subagent_result, workflow_failed]
  tools: { mcp: all, internal: all }  # what the ROOT agent may call
  max_parallel_turns: 4
intelligence:
  endpoints: [https://gw.example/v1, https://fallback/v1]
  model: my-model
  token: "{{secret:INTEL_TOKEN}}"     # or token_file
  headers: { anthropic-version: "2023-06-01" }
  swap_policy: finish-on-old
  structured_output: auto             # auto | json_schema | tool | prompt
  budget:                             # the token/request governor (§3.17) — never breaks work, it paces it
    windows:
      - { per: second, requests: 5 }
      - { per: minute, tokens: 60000 }
      - { per: hour,   tokens: 1500000 }
      - { per: day,    tokens: 20000000, reset: "00:00Z" }
    lifetime_tokens: 0                # 0 = unbounded (was --budget-tokens-lifetime)
    scope: [instance]                 # also: run | conversation | principal (sub-budgets, §3.17.2)
    on_exhausted: wait                # wait | slow | degrade | refuse | fail
    slow: { factor: 0.5 }             # for `slow`: pace to a fraction of the window rate
    degrade: { model: my-cheap-model } # for `degrade`: switch model until the window opens
    reserve: { estimate: context }    # pre-dispatch reservation from the context-size estimate
mcp:
  servers:
    - { name: fs, endpoint: https://mcp-fs.internal/mcp, ns: fs, tags: {"*": [sensitive]} }
    - { name: state, endpoint: https://state.internal/mcp }
tools:
  disabled: [code.run]
  overrides: { memory.get: {…}, ask_human: {…} }
store:
  kind: mcp
  mcp: { server: state, put: {…}, get: {…} }
  checkpoint: { debounce_ms: 250 }
  durability: { a2a: strict, steps: eventual }
  on_error: halt
memory: { max_value_bytes: 65536, list_default_limit: 100 }
context: { compact_at: 0.7, keep_last: 12, plan: { max_items: 32 } }
knowledge:
  server: kb                          # an MCP server speaking the knowledge.* profile (or mapped via tools.overrides)
  auto_context: { on: turn, top_k: 5, max_bytes: 16384 }
search:
  server: websearch                   # search.* profile (or mapped)
skills:
  sources:
    - { server: skills, discover: auto }   # prompts | resources | auto
  reference_prefix: "@skill:"
  max_loaded: 8
  max_bytes: 32768
workflows:
  - name: triage
    armed: true                       # arm its start nodes (once/loop/schedule/subscribe/a2a) at boot
    file: ./workflows/triage.yaml     # or inline `steps:`; or `uri: mcp://…` (read + subscribe: definition updates)
limits: { max_runs: 8, run: { steps: 500, tokens: 2000000, deadline: 1h }, lifetime_tokens: 0, subagents: { depth: 3, breadth: 8, rate: "8/2s" } }
lifecycle: { run_until: auto, idle_grace: 5s, drain_timeout: 25s, exit_code_map: {…} }
a2a:
  listen: https://0.0.0.0:8443
  tls: { cert: /tls/tls.crt, key: /tls/tls.key, client_ca: /tls/clients.crt }
  bearer: "{{secret:A2A_BEARER}}"
  principals:
    - { match: { san: "spiffe://prod/agentctl" }, role: operator }
    - { match: { san: "spiffe://prod/users/*" }, role: user }
    - { match: { bearer: any }, role: agent, grants: [workflow.run:triage] }
  peers: [{ name: research, endpoint: https://research.internal:8443, headers: {…} }]
observability:
  log_level: info
  otel: { endpoint: https://otel.internal:4318, traces: true, metrics: true, logs: false }
  metrics_addr: 0.0.0.0:9090
  audit: { sink: [log, store] }
security: { allow_trifecta: false, tls_ca: /pki/ca.pem, aauth: {…} }
```

Rules carried over: secrets only by reference; `deny_unknown_fields`;
`--validate-config`; `--config-schema` (schema v2, `contract_version: 2.0`);
reloadable vs restart-only partition redefined per path (`agent.instruction`,
`intelligence.*`, `mcp.servers`, `tools.*`, `workflows[*]` definitions,
`observability.log_level`, `context.*` reloadable; `store.*`, `a2a.listen/tls`,
`agent.name` restart-only).

### 3.13 Lifecycle: startup, exit, signals

**Startup:** parse+validate config → connect store (or refuse) → **restore**
(§3.4.5) → connect MCP servers (contained failures) → build registry (validate
overrides) → **arm start nodes** (`once` fires unless a live run was
restored) → bind A2A → `proc.ready`.

**Exit policy** (`lifecycle.run_until`): `auto` = `idle` when there is no A2A
listener and no long-lived start node (`loop`/`schedule`/`subscribe`) — the
job shape — else `drained`. `idle` = exit
when no run/turn/subagent/timer is live for `idle_grace`. Exit code = the
RFC 0011 table applied to the *outcome* (job shape: the `once`-started workflow's
`finish` status; daemon: 0 on clean drain). SIGTERM = drain (release claims,
checkpoint, finish in-flight turns within `drain_timeout`); SIGHUP = reload.

### 3.14 Security posture updates

Unchanged: no local execution (`code.run` mapping-only), trifecta gate over the
root grant (now also over per-workflow/subagent grants), SSRF egress guard,
secrets by reference, `exe` never request-derived, one spawn chokepoint (the
supervisor). New: principal/role authorization on every A2A op and tool grant;
audit trail; store keys namespaced per instance; artifact/memory size caps;
turn workers get only the tools their grant allows.

### 3.15 Removals

| Removed | Replacement |
|---|---|
| `--mode once/loop/reactive/schedule/workflow`, `Mode`, mode drivers (`triggers/mode.rs`) | workflows with start nodes + `lifecycle.run_until`; `--instruction` sugar |
| `--subscribe`/`--continue`/`--interval`/`--cron` flags | start nodes (`subscribe`/`loop`/`schedule`) in the workflow definition |
| served-MCP peer tools (`subagent.spawn/send/status/cancel`, `status` tool) and the "compose agents via MCP" posture (R14 reading) | A2A `SendMessage` (commands + NL), `a2a.*` admin |
| nested subagent process tree (`Orchestrator` in the child) | flat tree, supervisor spawns; logical depth |
| workflow dialect 1/2 cycles, blackboard `writes` model, `MAX_FOREACH_ITEMS` cap | dialect 3 DAG, step outputs + vars, artifact-backed iteration |
| `--report-file` per-run report as the durability record | run/task entities in the store (the report stays as an optional file export) |
| ad-hoc named env vars (`AGENT_MAX_STEPS` …) as the primary surface | path-derived names; a short alias list for the quickstart |

### 3.16 Knowledge, search and skills (R18, R19)

All three are **remote capabilities behind MCP** — agentd holds contracts,
mappings, a cache and the context wiring; never an index, a crawler or a
document parser.

#### 3.16.1 Knowledge (RAG over open-format documents)

- A **knowledge server** is any MCP server that indexes documents in open
  formats (Markdown, plain text, PDF, HTML, …) and answers retrieval queries.
  agentd defines the **`knowledge.*` profile** (`knowledge.search {query, top_k,
  filters} → {hits: [{id, uri, title, score, snippet, metadata}]}`,
  `knowledge.get {id|uri} → {content, mime, metadata}`, `knowledge.list
  {prefix} → {docs}`); a server that advertises tools with these names needs
  no mapping; any other server is mapped through `tools.overrides` (§3.7.3).
- **Where it is used:** the root/agent turns (as tools, per grant); optional
  **auto-context** (`knowledge.auto_context: {on: turn|never, top_k, max_bytes}`)
  retrieves for the incoming message before the turn and injects hits as a
  labelled system block (with sources) — RAG-at-turn without a tool call;
  workflows via `tool: knowledge.search` / `knowledge.get` steps; subagents
  per grant.
- **Documents as data:** MCP resources of the knowledge server are also
  reachable through `mcp.resource` (`resources/read` of a hit's `uri`), and
  `resources/subscribe` can trigger a workflow when a document changes.

#### 3.16.2 Search

- The **`search.*` profile** (`search.query {query, kind: web|docs|code, limit,
  freshness?} → {results: [{title, url, snippet, source, published?}]}`,
  `search.fetch {url, max_bytes?} → {content, mime, final_url}`), mapping-only
  (Brave/Tavily/SearXNG-style MCP servers map onto it). Fetching happens on
  the search server; agentd applies its egress classifier only to what it
  dials itself.
- Grants: `search.*` is a typical `untrusted_input` + `egress` capability —
  the trifecta gate applies to the grant that carries it.

#### 3.16.3 Skills

- A **skill** is a named instruction bundle (`name`, `description`,
  `when_to_use`, `body` (Markdown), optional `arguments`, optional attached
  resources) — the SKILL.md idiom. Skills are **discovered from MCP servers**
  (`skills.sources[]`): via **prompts** (`prompts/list` = catalogue; `prompts/get`
  = body, arguments honoured) or **resources** (a `skill://<name>` URI scheme or
  `mimeType: text/x-skill+markdown`; an optional index resource `skill://`
  lists them), over the **latest MCP dialect the server speaks** (modern
  2026-07-28 stateless included). Discovery runs at startup and on
  `list_changed`; the catalogue (names + descriptions + version hash) is
  cached in memory and refreshed; bodies are fetched on load and cached by
  hash.
- **Referencing:** `@skill:<name>` (configurable prefix) in `agent.instruction`,
  in a workflow `agent`/`think` step (`skills: [name]`), or in a **chat
  message** — the loop resolves references before the turn and **preloads**
  the bodies into the context's loaded-skill set (progressive disclosure: the
  catalogue is always visible; a body only when referenced or when the model
  calls `skills.load`). Unknown references are reported back to the caller.
- **Durability:** the loaded-skill set (name + version hash) is part of the
  context state (`context.skills`), so a restored conversation reloads the
  same skills; skill bodies themselves are re-fetched (cache), never stored.
- **Limits:** `skills.max_loaded` per context, `skills.max_bytes` per skill,
  eviction LRU on compaction (a compaction keeps the names, drops bodies not
  referenced in the kept window).

### 3.17 Intelligence budgets — the token governor

Today: one lifetime cap (`budget.rs`, RFC 0025) and per-run boxes. v2 makes
**how fast tokens burn** a first-class, durable control so an instance never
"breaks" on a quota — it paces.

#### 3.17.1 Windows and counters

`intelligence.budget.windows[]` — each `{per: second|minute|hour|day|week,
tokens?, requests?, reset?}`; a rolling window (`second`/`minute`/`hour`) is a
token bucket; a calendar window (`day`/`week`) resets at `reset` (UTC).
Counters are fed by the usage every turn/`think`/`agent` step reports
(`AgentMsg::Usage`, provider `usage`), attributed to `(instance, run,
conversation, principal, model)`, and are **durable** (part of the manifest,
debounced): a restart never re-opens a spent daily budget. `lifetime_tokens`
stays the hard ceiling.

#### 3.17.2 Scopes

`scope: [instance]` is the global governor; optional sub-budgets per `run`
(`workflows[].limits.budget`), per `conversation` (`agent.conversation_budget`)
and per `principal` (`a2a.principals[].budget`) nest under it — the tightest
applicable window wins.

#### 3.17.3 Tactics (`on_exhausted`)

| Tactic | Behaviour |
|---|---|
| `wait` (default) | the unit of work that needs intelligence (a turn, a `think`/`agent` step, a subagent spawn) enters **`waiting_budget`** — a durable state with the window-open time as a durable timer; runs show `waiting_budget`, A2A tasks stay `working` with a status message ("budget window opens at T"); nothing fails; resumes automatically |
| `slow` | admit at `slow.factor` × the window rate (pacing) instead of stopping; combined with `wait` at the hard edge |
| `degrade` | switch to `degrade.model` (a cheaper/faster model — the same endpoint list) until the window opens; the swap is logged and audited |
| `refuse` | new intake (A2A work, triggers) is declined with a budget message; in-flight work continues (bounded by the hard ceiling) |
| `fail` | the unit fails with `ExhaustedTokens` (today's behaviour) — for job-shaped runs that must not linger |

The governor sits in the **loop's dispatcher** (the single place turns/steps
are admitted): before spawning a turn worker it **reserves** an estimate
(`reserve.estimate: context` = tokens of the prompt + a completion allowance;
`fixed: N`; `none`) and settles on the reported usage. Long turns are
throttled between tool rounds (the child asks the supervisor for admission per
model call — a `BudgetRequest/BudgetGrant` frame, or the round-trip already
present for internal tools). Streaming intake pressure (a chatty user) is
answered with `working` + an ETA rather than errors.

#### 3.17.4 Observability

`agentd_budget_tokens{window,scope}`, `agentd_budget_waiting{scope}`,
`agentd_budget_events_total{tactic}`; `agent://budget` (windows, remaining,
next reset, waiting units); audit `budget.exhausted`/`budget.resumed`;
`status` command reports it; optional **cost** view via
`intelligence.pricing: {model: {input_per_1k, output_per_1k}}` (currency
budgets = the same windows in cost units).

---

## 4. Behavioural specifications (sequences)

### 4.1 Startup + restore
```
load config ─► validate ─► store.get(manifest)
  none  ─► write manifest gen=1 ─► connect MCP ─► registry ─► arm start nodes (once fires) ─► bind A2A ─► ready
  some  ─► get entities (runs, tasks, contexts, subagents, timers, inbox) ─► verify hashes
        ─► rebuild registries ─► re-arm timers/waits/subscriptions ─► re-spawn pending subagents
        ─► re-open working tasks ─► re-deliver undone inbox events (in ts order) ─► ready ─► audit restore.done
```
A `once` start node with a live restored run of its workflow does not fire
again (`once` means "ensure one run", policy `ensure | always`); `loop`
resumes its iteration counter; `schedule` applies `catch_up`.

### 4.2 A2A message → turn
```
A2A SendMessage ─► authn (mTLS/bearer) ─► principal/role ─► inbox.put(event) ─► ack (task id)
  command DataPart ─► authz(op) ─► registry.call (durable effect) ─► task update ─► reply
  NL ─► conversation ctx (get/create) ─► [preflight think ⇒ verdict; skills preload; plan.create if needs_plan]
     ─► turn worker spawn(ctx slice incl. plan, tools by grant)
     ─► ToolRequest* (durable effects, idempotency (ctx,turn,call); plan.update as it works) ─► TurnDone
     ─► ctx.put (messages + plan + verdict) ─► reply message/artifacts (message id = idempotency) ─► task done ─► inbox done
```

### 4.3 Start node fires → run
```
start node fires (once at arm; loop on completion; schedule cron/timer; subscribe: MCP resources/updated → debounce/coalesce → claim/shard gate; a2a command; manual)
  ─► inbox.put ─► concurrency policy ─► run.create(inputs, run.start=payload) ─► schedule ready steps
  ─► for each step: run.put(running,attempt) ─► execute (executor pool / turn worker / child)
                    ─► run.put(done|failed, output|artifact) ─► schedule dependents
  ─► finish ─► run terminal ─► task/report ─► notify (root note / A2A) ─► maybe wake root
```

### 4.4 Crash mid-step (the durability contract, tested by SIGKILL e2e)
```
step S running (attempt 1) ─► process killed ─► restart ─► restore: run R has S=running
  ─► S re-executed (attempt 2) with the SAME idempotency key ─► result recorded ─► continue
batch B of foreach: batches 0..k done are recorded ─► resume at k+1
in-flight turn: context has no delta for it ─► inbox event undone ─► turn re-run
sleep with deadline D: re-armed for max(0, D-now)
```

### 4.5 Human gate
```
human step ─► run.put(suspended, gate) ─► A2A task input-required (owner ctx) or mapped tool
  ─► reply (SendMessage taskId | mapped tool result | timeout) ─► resume with data ─► next step
```

### 4.6 Subagent lifecycle (sync / async / warm)
```
subagent.run ─► caps ─► subagent.put(spawned, payload) ─► spawn child (re-exec, PDEATHSIG, cgroup)
  sync:  await Result ─► distill ─► return to caller (turn or step)
  async: return handle ─► Result event later ─► subagent.put(done) ─► join/await
  warm:  stays alive; subagent.send injects messages; kill/timeout ends it
child ToolRequest (memory/artifact/ask_human within its grant) ─► supervisor executes ─► ToolResult
```

### 4.6b Skill reference → preload
```
message/instruction contains @skill:review-pr ─► catalogue lookup (cached; refresh on miss)
  ─► prompts/get | resources/read (by hash) ─► ctx.skills += {name, hash} ─► ctx.put
  ─► turn prompt: catalogue + loaded bodies ─► unknown skill ⇒ reply/audit `skill.unknown`
```

### 4.6c Budget window exhausted
```
dispatcher: reserve(estimate) ─► window short ─► tactic:
  wait   ─► unit → waiting_budget (durable) ─► timer(next reset) ─► A2A task working {"budget window opens at T"} ─► resume
  slow   ─► pace admissions to factor×rate ─► hard edge ⇒ wait
  degrade ─► model := degrade.model ─► continue ─► window opens ⇒ model restored
  refuse ─► new intake declined (A2A: rejected with reason; triggers: dropped/queued) ─► in-flight continues
restart while waiting ─► counters restored from manifest ─► timer re-armed ─► resume at T
```

### 4.7 Compaction
```
after turn: est_tokens > compact_at*window ─► think(summarize older) ─► ctx v+1 (summary block + last N) ─► ctx.put
```

### 4.8 Finish and drain
```
finish (root) ─► lifecycle: job shape ⇒ exit code from status; daemon ⇒ note + continue unless `finish.exit`
SIGTERM ─► draining latch ─► stop intake (A2A 503 for new work, status still served) ─► release claims
  ─► finish/checkpoint in-flight within drain_timeout ─► kill ladder ─► exit 0
```

---

## 5. Data contracts (drafts to be frozen in RFCs, §6 P0)

- **Envelope** (§3.4.1) — `v, kind, id, seq, ts, instance, hash?, state`.
- **Manifest state** — `{generation, created, updated, entities: [{kind,id,seq}], starts: {"<workflow>.<node>": {last_fired, iteration, missed}}, budget: {windows: {…}, scopes: {…}}, lifecycle: {…}}`.
- **Run state** — `{workflow, workflow_hash, inputs, status, cursor: {steps: {id: {status, attempt, started, finished, output|artifact, error}}, batches: {step: {done, partial}}}, vars, waits: [...], timers: [...], budget: {steps, tokens}, task?, principal?}`.
- **Context state** — `{version, summary: {goals, decisions, open, facts}, messages: [...], skills: [{name, hash}], plan?: {goal, items: [...]}, preflight?: {last verdict}, est_tokens, model_window}`.
- **Skill (cached, not stored)** — `{name, description, when_to_use, arguments?, body, hash, source: {server, kind: prompt|resource, ref}}`; **knowledge hit** — `{id, uri, title, score, snippet, metadata}`.
- **Task state** — A2A `Task` (spec shape) + `{principal, linked: {run|subagent|turn}}`.
- **Subagent state** — `{payload (secret-free), mode, status, attempt, result?, distillate?, child_pid?}`.
- **Memory record** — `{value, ts, ttl?, by}`; **artifact** — `{name, mime, size, chunks|inline, sha256, created_by}`.
- **Inbox event** — `{id (ULID), kind, ts, principal?, payload, status: pending|done}`.
- **Store adapter config** — §3.5; **tool override** — §3.7.3.
- **Workflow dialect 3** — §3.6.1/3.6.3; `--workflow-schema` export.
- **A2A command DataPart** — `{"agentd": {"op": "<tool name>", …args…, "request_id"?: "…"}}`; responses as DataParts mirroring the tool's `output_schema`.
- **Config schema v2** — §3.12 (`contract_version: 2.0`).

---

## 6. Work plan

Each phase ends **green** (fmt/clippy/all tests/conformance), documented, and
recorded in `progress.md`. Sizes are relative (S ≈ days, M ≈ 1–2 weeks,
L ≈ 2–4 weeks of focused work); dependencies are strict unless marked ∥.
Development happens on `develop` toward **agentd 2.0.0**; 1.x stays
releasable from `main` until P8.

### P0 — Freeze the design (S)
- [ ] Review this plan; resolve D1–D12 (§8).
- [ ] Write the normative specs as RFCs: **0025 Durable state & store adapters**,
      **0026 Agent loop & lifecycle (no modes)**, **0027 Workflow dialect 3**,
      **0028 Internal tools & overrides**, **0029 A2A conversations, principals & commands**,
      **0030 Config schema v2**; mark superseded RFC sections.
- [ ] Test strategy doc: mock store (in-process + `mock_http` profile), chaos
      (SIGKILL/restore) suite shape, A2A conversation e2e, conformance v2 families.
- Exit: RFCs accepted; `progress.md` carries the phase checklists.

### P1 — Config schema v2 + lifecycle (M)
- [ ] Nested schema (§3.12) on the existing mechanism; typed `Config` v2;
      alias table for the quickstart flags; `--config-schema` 2.0; `--capabilities` v2 surfaces.
- [ ] Remove `Mode` and mode flags; `lifecycle.run_until`; `--instruction` sugar
      generates the `once → agent → finish` workflow.
- [ ] Reload partition per path; `--validate-config` for the new checks (store,
      overrides, workflows).
- [ ] Docs: `configuration.md` rewritten; migration table (§9).
- Exit: 1.x flags either aliased or listed as removed; all suites green with the shim (`triggers/mode.rs` still driving until P4).

### P2 — Durable state core (L)
- [ ] `store::{Store, Envelope, Key}` contract; `mcp` adapter with mapping
      (templates + CEL), `http` adapter, `none`; conflict semantics; retries; metrics.
- [ ] Entity model + serializers (§5); manifest; inbox; timers; checkpoint policy
      (strict/eventual, debounce); `store.on_error` policy.
- [ ] Restore protocol (§3.4.5) with the entity kinds that exist by then
      (manifest, inbox, timers, memory, artifacts) — runs/contexts join in P3/P4.
- [ ] Mock store: in-process + extend `mcp/mock_http.rs` (put/get/list/delete,
      conflict injection, latency, failure); HTTP mock.
- [ ] Chaos e2e skeleton: SIGKILL between put and ack; restart; assert replay.
- Exit: `agentd` boots against a store, writes/reads the manifest, replays inbox events; unit + e2e green.

### P3 — Agent loop v2 + tools registry + contexts (L)
- [ ] Event loop in the supervisor (events §3.3, wake policy, per-conversation
      serialization, executor pool); flat child tree; `ToolRequest/ToolResult`
      + `Role` in the control protocol; turn worker (reuses `agentloop`).
- [ ] Tool registry (internal > code > MCP), internal tool contracts with
      schemas (§3.7.2), overrides + disabled, grants; schema validator reuse.
- [ ] Root context + conversation contexts + compaction (§3.8); memory tools;
      artifact tools; `sleep`/`await`/`ask_human`/`finish`/`think`/`instruction.*`.
- [ ] Conversation **preflight** (`agent.preflight`, structured verdict, short-
      circuits, skill preload) and the **context plan** (`plan.*` tools, prompt
      rendering, run/subagent bindings, compaction rule, caps).
- [ ] **Token governor** (§3.17): windowed durable counters, scopes, tactics
      (`wait` with durable `waiting_budget` + timer, `slow`, `degrade`, `refuse`,
      `fail`), pre-dispatch reservation, `BudgetRequest/BudgetGrant` frame,
      metrics + `agent://budget`; `--budget-tokens-lifetime` folded in.
- [ ] Subagent registry (`subagent.run/send/kill/status/await/list`) over the
      existing spawn/kill/reaper; warm mode from `triggers/warm.rs`.
- [ ] Knowledge + search profiles (`knowledge.*`, `search.*`) as mapping-only
      contracts with default profiles; `knowledge.auto_context`; mock RAG/search
      MCP servers for tests.
- [ ] Skills: sources discovery (prompts/resources, legacy + modern dialects),
      catalogue cache, `@skill:` resolution in instruction/messages/steps,
      `skills.list/load/unload`, loaded set in context state, limits/eviction.
- [ ] Durable: contexts, subagents, tasks (turn-level) via P2.
- Exit: `agentd --instruction` runs a root turn end to end through the new loop; a killed instance resumes a pending inbox event; tools overridable by the mock MCP.

### P4 — Workflow engine v3 (L)
- [ ] Dialect 3 model + strict validation (§3.6.1/3.6.3); `--workflow-schema`.
- [ ] Engine: DAG scheduling, guarded edges, retries/timeouts/`cache`, `switch`,
      `parallel`, `race`, `foreach`/`batch` (artifact-backed, per-batch durability,
      `rate`), `iterate`, `join`, `subgraph`, **`workflow`** (child runs, cascade),
      `signal`/`event`/`wait` (resource · condition · signal · run · subagent ·
      message), `mcp.tool`/`mcp.resource`/`tool`, data nodes (`map/filter/reduce/
      sort/dedupe/chunk/template/parse/validate`), `think` + presets, `agent`,
      `subagent` (modes), `memory.*`, `artifact.*`, `sleep`/`human`, `assert`/`fail`,
      `a2a.send/delegate/wait`, `workflow.signal/wait/cancel`, `emit`, `finish`.
- [ ] Run registry, concurrency policies, limits, cancel/pause/resume; run
      durability + resume; hash binding.
- [ ] Start nodes (§3.6.6: `once`/`loop`/`schedule`/`subscribe`/`a2a`/`manual`)
      replacing `triggers/{mode,router,timer}.rs`, keeping
      debounce/coalesce/exactly-one-owner; cluster claim/shard as `subscribe` options (D8);
      durable start-node state.
- [ ] Remove dialect 1/2 driver + mode drivers; migrate examples/bench workflows.
- Exit: the five 1.x shapes reproduced as workflows in `examples/`; SIGKILL mid-batch e2e resumes at the next batch; multiple concurrent runs.

### P5 — A2A v2 (M–L)
- [ ] Principals/roles/authorization matrix; audit of calls; agent card.
- [ ] Conversations (contextId ↔ context), command DataParts, NL routing to
      root turns, steering ops, status; task durability + `GetTask` after restart;
      streaming from run/turn events; human gate over tasks (kept).
- [ ] Remove served-MCP peer tools; shrink `mcp/server.rs` to listener + auth +
      A2A + management resources (D7).
- [ ] Outbound peers: bearer/mTLS (kept), AAuth signing; `a2a.send`.
- Exit: a user client chats, asks status, starts `triage`, steers a warm subagent, answers a `human` gate — all authenticated and audited; conformance A2A family green.

### P6 — Observability & audit (M) ∥ with P5
- [ ] Span model (§3.11); OTLP metrics + logs exporters; new metrics; audit
      stream + store sink; `agent://` v2 resources; run report as export.
- Exit: `otel_e2e` proves traces+metrics+logs; audit events for every principal action.

### P7 — Hardening, docs, conformance v2 (M)
- [ ] Chaos matrix (kill points × entity kinds × store failure modes);
      soak (many runs, large foreach); security review (authz matrix, trifecta over grants).
- [ ] Docs rewrite (README, architecture, workflows, a2a, durability, tools,
      configuration, operations, deployment); RFC index; `CONFORMANCE.md` v2.
- [ ] Conformance suite v2 families: durability, a2a-conversation, tools, store.
- Exit: docs match code (a docs-drift check in CI); conformance green.

### P8 — Release 2.0.0 (S)
- [ ] CHANGELOG (breaking list), migration guide, version bumps, release
      workflow (features incl. `cel`), image, site.
- Exit: tag `v2.0.0`; `main` fast-forwarded; `develop` continues.

**Critical path:** P0 → P1 → P2 → P3 → P4 → P5 → P7 → P8 (P6 in parallel from P3).
**First user-visible milestone:** end of P3 (durable root agent over A2A);
**second:** end of P4 (durable workflows, modes gone).

---

## 7. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Store latency on the request path (strict durability) | slower A2A acks, more store load | debounce/eventual for internal transitions; batching; measure in P2; `durability` knobs |
| Store unavailable | intake refused / progress lost | `on_error: halt` default keeps status serving; `degrade` documented; local WAL is a non-goal (remote store is the requirement) |
| Split-brain (two instances, one key space) | corrupt state | `seq` CAS conflicts are fatal; instance-namespaced keys; claim/lease for shared triggers |
| Replay double effects | duplicated side effects | idempotency keys on every effect (`_meta`, headers); `on_replay` per node; documented at-least-once |
| Turn-worker round-trip complexity (child ↔ supervisor tools) | latency, protocol bugs | reuse the proven frame protocol; unit tests with the mock LLM scripts; keep MCP calls in the child |
| Scope of the rewrite | long time-to-green | phases with shims; each phase releasable; P3/P4 milestones demoable |
| CEL dependency in the release build | dep footprint | audit `cel-interpreter`'s tree; templates stay dependency-free; `cel` remains a feature |
| Breaking users of 1.x flags/modes | migration pain | alias table + `--instruction` sugar + migration guide + `--validate-config` hints naming the v2 path |
| Auth model regressions | exposure | authz matrix tests per role; conformance security family; deny by default |

---

## 8. Decisions requested (D1–D12)

| # | Decision | Recommendation |
|---|---|---|
| D1 | What "ACP" denotes (nothing in the tree is named so): read as the served-MCP peer tools / "agents compose via MCP" surface | **Remove them; A2A only** (this plan) |
| D2 | CEL in the release build (templates stay dependency-free; `cel` remains a cargo feature) | **Yes** — mapping/conditions/transforms lean on it |
| D3 | Flat process tree (supervisor spawns every child; logical depth) vs today's nesting | **Flat** — durability owner sees every child |
| D4 | Root LLM turns in a child "turn worker" with tool round-trips vs in-process | **Child** — supervisor never reasons; containment kept |
| D5 | Durability default for A2A intake: ack-after-persist (strict) | **Strict** for A2A/triggers, eventual for step progress — **decided: yes (2026-08-16)** |
| D6 | Store contract minimum: `put/get`; `list/delete` optional (index records otherwise) | **Yes** |
| D7 | Keep MCP serving as a read-only management surface for agentctl (resources) or drop MCP serving entirely | **Keep read-only** until agentctl speaks A2A; drop later |
| D8 | Cluster shard/claim/standby: keep as trigger options in 2.0, or defer | **Keep claim + shard as trigger options**; defer standby |
| D9 | DAG-only with structured `loop` (bounded) vs allowing cycles | **DAG + `loop`** |
| D10 | `code.run` mapping-only (no built-in) | **Yes** — no local execution |
| D11 | Compatibility: 2.0 breaking with aliases + `--instruction` sugar; no 1.x mode shim in the release | **Yes** |
| D12 | Memory: plain KV in the store namespace (no vector/semantic layer in-binary) | **Yes**; semantic memory via an MCP override |
| D13 | Knowledge & search: mapping-only contracts with default `knowledge.*` / `search.*` MCP profiles (no in-binary index/crawler); optional auto-retrieval into turns | **Yes** |
| D14 | Skills: discovered from MCP prompts/resources (SKILL.md idiom), `@skill:name` references, bodies preloaded and tracked per context; no local skill directories in the stock binary (files stay behind an MCP server) | **Yes** — remote-only keeps the posture; embedders can register code skills later |
| D15 | Conversation preflight (`agent.preflight: auto` default) and the plan as part of the `context` record (durable, per context, not memory) | **Yes** — plan tools `plan.create/get/update/clear`; auto-advance via bindings |
| D16 | Triggers are **start nodes** of the DAG (`once`/`loop`/`schedule`/`subscribe`/`a2a`/`manual`) rather than a separate `triggers:` list; `armed` arms/disarms them | **Yes** (Andrii, 2026-08-16) |
| D17 | Token governor default tactic `wait` (durable `waiting_budget`), lifetime cap kept as the hard ceiling; cost budgets via an optional pricing table | **Yes** — pacing over failing |
| D18 | `agent.instruction` is one field: a single-token URI a configured MCP server serves ⇒ read + subscribe; else static text (no `instruction_uri`) | **Yes** (Andrii, 2026-08-16) |

Open questions (non-blocking): bare env names policy (from progress.md);
per-principal quotas; artifact chunk size; whether `think` should be allowed
tools (no — that is `agent`); OTLP logs default off.

---

## 9. Appendix — 1.x → 2.0 mapping

| 1.x | 2.0 |
|---|---|
| `--mode once --instruction X` | `agentd --instruction X` ⇒ workflow `{start: once → agent(X) → finish}`, `run_until: idle` |
| `--mode loop --interval 5m` | start node `loop {interval: 5m}`, `run_until: drained` |
| `--mode reactive --subscribe U` | start node `subscribe {uri: U}` (debounce/coalesce/claim/shard options) |
| `--continue U` | start node `subscribe {uri: U, deliver: wait}` into a run holding a `wait`, or a `warm` subagent |
| `--mode schedule --cron E` | start node `schedule {cron: E, catch_up}` |
| `--mode workflow --workflow F` | `workflows: [{file: F}]` with a `once` start node, `run_until: idle` |
| `--budget-tokens-lifetime N` | `intelligence.budget.lifetime_tokens: N` (+ windowed budgets, §3.17) |
| `--workflow-resume server:key` | automatic (restore) — `resume_policy` for definition changes |
| served `subagent.spawn` (peer MCP tool) | A2A `SendMessage` (`{"agentd": {"op": "subagent.run"}}` or NL) |
| `agent://subagent/<h>` resource | A2A `GetTask` / `agent://runs` (management) |
| dialect 2 graph (cycles, blackboard) | dialect 3 DAG (steps, vars, artifacts, `loop`) |
| `--report-file` | run/task entities (+ optional export) |
| `AGENT_MAX_STEPS` … | `AGENTD_LIMITS_RUN_STEPS` … (path names; short alias list) |

Glossary: **instance** (one agentd process + its state namespace); **principal**
(authenticated A2A caller); **conversation** (A2A context); **run** (one
execution of a workflow DAG); **step** (durable unit of a run); **turn** (one
intelligence call cycle in a child); **effect** (a tool/MCP/A2A action with an
idempotency key); **envelope** (a versioned store record).
