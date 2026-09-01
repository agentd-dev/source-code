# RFC 0026: The agent loop, process model, contexts, budgets & lifecycle (no modes)

**Status:** Implemented (agentd 2.0 track, phases P1, P3, P5)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.1–3.3, §3.8, §3.13, §3.17); supersedes RFC 0008 (execution modes) and RFC 0009 §process model (nested tree); builds on RFC 0002/0003 (reactor, supervision), RFC 0007 (the ReAct turn), RFC 0025 (durable state).

---

## 1. Summary

An agentd **instance** is one supervisor process that runs an **event loop**
over durable state, plus child processes for intelligence turns ("turn
workers") and subagents. There are **no execution modes**: the instance arms
its workflows' start nodes (RFC 0027 §4), serves A2A (RFC 0029), and reacts to
events. Intelligence is invoked only where judgment is needed and always in a
child that proposes tool calls the supervisor executes. Contexts (root +
per-conversation) are durable and self-compacting; a token governor paces
spend against durable windowed budgets; a lifecycle policy decides when a
job-shaped instance exits.

## 2. Process model

- **Supervisor**: config → store → restore → MCP → registry → arm start nodes →
  A2A → loop. Never calls the LLM. Single writer of durable state. Owns every
  child (flat tree): PDEATHSIG, per-child cgroup, kill ladder, reaper (RFC 0003
  unchanged).
- **Turn worker**: a re-exec'd child (`AGENT_SUBAGENT` dispatch, RFC 0009) with
  `Role::Turn`: it receives a context slice + tool definitions, runs ONE turn
  of the ReAct loop (RFC 0007), calls MCP tools itself, and **round-trips
  internal tools** to the supervisor: `AgentMsg::ToolRequest{id, name, args}` ↔
  `ControlMsg::ToolResult{id, result, is_error}`. It ends with
  `AgentMsg::Turn{messages, usage, effects}` (or `Failed`).
- **Subagent**: `Role::Agent` (bounded agentic run) or `Role::Workflow`
  (drives a workflow definition) with the RFC 0009 payload; modes `sync |
  async | detached | warm`; may issue `ToolRequest`s within its grant.
- **Budget admission**: a child asks `AgentMsg::BudgetRequest{estimate}` before
  each model call; the supervisor answers `ControlMsg::BudgetGrant{ok, wait_ms,
  model?}` (§7).
- Delegation depth is logical (`limits.subagents.depth`); breadth/rate/tree
  token caps are enforced by the supervisor at the one spawn chokepoint.

## 3. The event loop

Single-threaded reactor (200 ms tick, `mpsc`) over:

| Event | Source | Durable via |
|---|---|---|
| `A2aMessage {ctx, task?, principal, parts, id}` | A2A server | inbox (before ack) |
| `A2aControl {op}` | A2A admin family | inbox |
| `StartFired {workflow, node, payload}` | start nodes | inbox |
| `Signal {name, payload, from}` | `workflow.signal`, A2A command, subagent | inbox |
| `StepDone {run, step, outcome}` | executors / children | run record |
| `TurnDone {ctx|run, …}` | turn worker | context / run record |
| `ToolRequest / BudgetRequest` | children | — (in-turn) |
| `SubagentEvent {handle, kind}` | children | subagent record |
| `TimerFired {id}` | timer wheel | timer record |
| `McpNotification {server, method, params}` | MCP clients | only when bound to a start node / wait |
| `Store {conflict|down|degraded}` | store adapter | — |
| `Signal {TERM|HUP|CHLD}` | signals | — |

Rules: the loop never blocks on I/O (executor pool for MCP/HTTP calls;
children for turns); state mutation happens only in the loop; each mutation
is followed by a checkpoint decision (RFC 0025 §5); per-conversation turns are
serialized; conversations run in parallel up to `agent.max_parallel_turns`.

### 3.1 Wake policy

`agent.wake_on` (default `[a2a_message, human_reply, subagent_result,
workflow_failed]`) lists the events that start a **root turn**; everything
else is handled deterministically (start nodes → runs; step scheduling;
timers; structured commands; status; operator control; MCP notifications).

### 3.2 Turn lifecycle (root/conversation)

1. **Preflight** (`agent.preflight: never|auto|always`, default `auto`): a
   structured `think` (no tools) → verdict `{intent, needs_plan, plan?,
   clarifications?, risk, tools_needed?, skills?}` recorded on the context;
   `status`/`clarify` intents may short-circuit; `skills` are preloaded;
   `needs_plan` seeds the context plan (§5.3).
2. Build the prompt: instruction (RFC 0028 §3 `instruction.*`) + registry tool
   defs by grant + skill catalogue + loaded skill bodies + root summary +
   plan + conversation thread + selected memory + optional knowledge
   retrieval + verdict + event.
3. Spawn/reuse a turn worker; serve `ToolRequest`s as durable effects keyed
   `(ctx, turn, call)`; serve `BudgetRequest`s.
4. `TurnDone` → persist the context delta (messages, plan, verdict, skills,
   usage) → deliver replies/artifacts (A2A, message id = idempotency) → mark
   the inbox event done → maybe compact (§5.2).

## 4. Registries (durable)

**Run registry** (RFC 0027 §9), **subagent registry** (payload, mode, status,
attempt, result — re-spawn on restore when the parent step is pending),
**task registry** (RFC 0029 §4), **timer wheel** (absolute deadlines),
**conversation registry** (RFC 0029 §3).

## 5. Contexts

### 5.1 Records

`context/root` — the agent's own working memory: instruction snapshot,
capability notes, a rolling log of significant events, summary blocks.
`context/<contextId>` — one per A2A conversation. State: `{version, summary:
{goals, decisions, open, facts}, messages, skills: [{name, hash}], plan?,
preflight?, est_tokens, model_window}` (RFC 0025 §3.3).

### 5.2 Compaction

Trigger: `est_tokens > context.compact_at × model_window` (default 0.7) or the
`context.compact` tool. Method: a `think` summarizes the older messages into
the structured summary block, keeps the last `context.keep_last` messages
verbatim (default 12), keeps the plan verbatim, evicts skill bodies not
referenced in the kept window (names stay), bumps `version`, checkpoints.
Restore re-compacts if the model window shrank.

### 5.3 The plan

Per context: `{goal, created, updated, items: [{id, title, detail, status:
pending|in_progress|done|blocked|skipped, note?, bound?: {run|subagent|task},
updated}]}` managed with `plan.create/get/update/clear` (RFC 0028 §3),
rendered in every prompt, auto-advanced when a bound run/subagent reaches a
terminal state, capped by `context.plan.max_items` (32). Temporary by intent
(never written to memory unless the model does so explicitly), durable by
construction.

## 6. Subagents

`subagent.run {instruction, mode, workflow?, tools?, servers?, limits?,
context?, output_contract?, skills?}` → the supervisor validates caps, records
`subagent/<handle>` (spawned), spawns the child. `sync` blocks the caller's
step/turn on the result; `async` returns the handle (`subagent.await`, `join`,
`wait on subagent`); `detached` is fire-and-forget (audited); `warm` keeps the
child alive across `subagent.send` messages until `subagent.kill`/timeout.
Distillation of results per RFC 0009 §4. Restore re-spawns pending
non-detached subagents (`attempt+1`).

## 7. The token governor

`intelligence.budget.windows[]` — `{per: second|minute|hour|day|week, tokens?,
requests?, reset?}`; rolling windows are token buckets, calendar windows reset
at `reset` (UTC). Counters (per instance; optional sub-scopes `run`,
`conversation`, `principal`, and per `model`) are fed by reported usage and
are **durable** in the manifest (debounced). `lifetime_tokens` is the hard
ceiling. Tactic `on_exhausted`:

| Tactic | Behaviour |
|---|---|
| `wait` (default) | the unit enters `waiting_budget` (durable) with a timer for the window opening; A2A tasks stay `working` with an ETA message; resumes automatically |
| `slow` | admissions paced to `slow.factor × rate`; `wait` at the hard edge |
| `degrade` | model switched to `degrade.model` until the window opens (audited) |
| `refuse` | new intake declined with a budget message; in-flight work continues |
| `fail` | the unit fails `ExhaustedTokens` (RFC 0007) |

The governor is consulted at dispatch (reservation from
`reserve.estimate: context|fixed|none`) and per model call
(`BudgetRequest/BudgetGrant`); it settles on reported usage.

## 8. Lifecycle

**Startup**: parse+validate config (RFC 0030) → connect store or refuse (RFC
0025) → restore → connect MCP servers (contained failures) → build the registry
(validate overrides, RFC 0028) → arm start nodes (`once` fires unless a live
run was restored) → bind A2A → `proc.ready`.

**Exit policy** `lifecycle.run_until`: `auto` ⇒ `idle` when there is no A2A
listener and no long-lived start node (`loop`/`schedule`/`subscribe`/`signal`/
`event`) — the job shape — else `drained`. `idle` = exit when no run, turn,
subagent, timer or pending inbox event exists for `lifecycle.idle_grace`
(5 s). Exit code = RFC 0011 §5 applied to the outcome (job shape: the
`once`-started workflow's `finish` status; daemon: `0` on a clean drain).
`finish` from the root: job shape ⇒ exit; daemon ⇒ note + continue unless
`{exit: true}`.

**Signals**: SIGTERM = drain (release claims, stop intake — A2A answers `503`
for new work, status still served — finish in-flight turns within
`lifecycle.drain_timeout`, checkpoint, kill ladder, exit `0`); SIGHUP =
reload (RFC 0017 semantics over the v2 reloadable partition, RFC 0030 §6).

## 9. Removed (RFC 0008/0009 supersession)

`--mode` and the mode drivers; `--subscribe/--continue/--interval/--cron`
flags (start nodes now); the reactive router as a component (its
debounce/coalesce/exactly-one-owner semantics live in the `subscribe` start
node); nested spawning inside children (flat tree); the served-MCP peer tools
(RFC 0029). `agentd --instruction X` remains as sugar (RFC 0030 §7).

## 10. Observability & test plan

Spans `agent.turn`, `tool.call`, `subagent`, `budget.wait`; metrics
`agentd_turns_total{ctx_kind,outcome}`, `agentd_context_tokens`,
`agentd_budget_tokens{window,scope}`, `agentd_budget_waiting{scope}`,
`agentd_inbox_pending`; events `turn.start/done`, `preflight.verdict`,
`context.compacted`, `plan.updated`, `budget.exhausted/resumed`, `restore.*`.
Tests: mock-LLM-scripted turns through the new loop (tool round-trip,
preflight, plan, compaction), budget windows (fake clock), SIGKILL/restore of
a pending inbox event, wake-policy matrix.
