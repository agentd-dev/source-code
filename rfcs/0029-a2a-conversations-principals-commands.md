# RFC 0029: A2A as the only external channel — principals, conversations, tasks, commands

**Status:** Implemented (agentd 2.0 track, phase P5)
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-08-16
**Part of:** the durable-agent design (`docs/design/01-durable-agent-plan.md` §3.9, §3.15, §4.2, §4.5); supersedes RFC 0005 §served tools (the MCP peer-tool surface) and refines RFC 0020 (A2A serving) and RFC 0015 §4 (operator control stays the `a2a.*` admin family).

---

## 1. Summary

Other agents **and users** (their client is an agent) talk to an agentd
instance over **A2A** — the only external channel. Every caller is an
authenticated **principal** with a **role**; each conversation is an A2A
`contextId` backed by a durable context; each unit of work is an A2A **Task**
that survives restarts. Messages are either **structured commands** (a
DataPart naming a registry tool the role may call — deterministic, no LLM) or
**natural language** (a root turn with tools). Users can chat about the work,
ask status, steer running work, start workflows/subagents and answer the
agent's questions. Operator control remains the `a2a.*` admin methods. The
served-MCP peer tools are removed; MCP serving is a read-only management
surface.

## 2. Principals, roles, authorization

`a2a.principals[]` — `{match: {san | sub | bearer_ref | aauth_agent | any},
role: operator|user|agent|anonymous, grants?: [tool patterns], quotas?:
{budget, rate}}`. Identity: mTLS client SAN, bearer subject (constant-time
compare against a secret ref; roadmap: OAuth introspection through a mapped
MCP tool), AAuth agent id (roadmap: signature verification). Unmatched callers
are `anonymous` (refused unless configured).

| Method / op | operator | user | agent | anonymous |
|---|---|---|---|---|
| `SendMessage` NL / `GetTask` / `CancelTask` (own) / streaming | ✓ | ✓ | ✓ | ✗ |
| command ops (registry tools) | all | per grants (default: `status`, `workflow.run` of `a2a`-startable workflows, `workflow.status/cancel` own, `subagent.send` own, `plan.get`, `ask_human` reply) | per grants (default: `workflow.run` of `a2a`-startable) | ✗ |
| `a2a.Drain / LameDuck / Pause / Resume / Cancel(any)` | ✓ | ✗ | ✗ | ✗ |
| Agent card | ✓ | ✓ | ✓ | ✓ (public card; private methods hidden) |

Every request is audited (`a2a.request {principal, role, method, op?, outcome}`).

## 3. Conversations

A2A `contextId` ⇒ `context/<contextId>` (RFC 0026 §5). A message without a
`contextId` opens a new conversation. Turns are serialized per conversation.
The conversation's plan, loaded skills, preflight verdicts and message history
are durable; a conversation is closed by the principal (`{"op":
"conversation.close"}`) or by `a2a.conversation_ttl`.

## 4. Tasks

A **Task** (A2A spec shape, RFC 0020 mapping kept: `TASK_STATE_*`) is created
for: a root turn's answer (short-lived), a workflow run started for the
principal (task id = run task id, states follow the run), a subagent started
for the principal. `task/<id>` records are durable (RFC 0025); `GetTask` works
across restarts; `SendStreamingMessage`/`SubscribeToTask` stream status,
progress and artifact frames from run/turn events; artifacts are `artifact/*`
records. Cancellation cascades per RFC 0027 §6.

## 5. Message routing

1. **Command DataPart** — `{"agentd": {"op": "<registry tool>", …args…,
   "request_id"?: "…"}}` (`op` must be a granted tool) → validated against the
   tool's `input_schema` → executed deterministically → the result returned as
   a DataPart mirroring `output_schema` (+ text summary). `status` is always
   granted to non-anonymous roles.
2. **Human gate reply** — a message carrying the `taskId` of a task waiting
   `input-required` → the gate's reply (first-signal-wins; RFC 0027 §6).
3. **Natural language** — the wake policy (RFC 0026 §3.1) starts a root turn
   for the conversation with the tools the role grants; the model replies
   and/or acts (start a run → a task, steer a subagent, ask back →
   `input-required`).
4. **Steering** — `subagent.send`, `workflow.signal`, `workflow.cancel/pause/
   resume`, `instruction.subscribe` (operator), `plan.update` — as commands or
   through the model.
5. Everything is a durable inbox event first (RFC 0025 §5) — `SendMessage`
   returns after the inbox write.

## 6. Agent card

`/.well-known/agent-card.json`: name/description from `agent`, skills = `chat`
+ each workflow with an `a2a`/`signal`/`manual` start node (name, description,
inputs schema), capabilities `{streaming: true, pushNotifications: false,
stateTransitionHistory: true}`, security schemes from `a2a.tls`/`a2a.bearer`.
`agentd --capabilities` stays the control-plane manifest (RFC 0014/0015) with
`surfaces.a2a` extended.

## 7. Outbound

`a2a.peers[]` (bearer / mTLS client identity / AAuth signing) for `a2a.send`,
`a2a.delegate` (RFC 0027 nodes) and the root's delegation tool; trace context
propagated (`traceparent`).

## 8. Removed / reduced

Removed: served-MCP tools `status`, `subagent.spawn/send/status/cancel` and the
peer composability posture of RFC 0005; `agent://subagent/<h>` as a peer
surface. Kept (management, read-only, RFC 0015): the HTTP(S) listener + auth,
`agent://status|runs|run/<id>|conversations|subagents|store|budget|
config/effective|events|capabilities|inventory|intelligence` resources for a
`Management` origin, and the `a2a.*` admin family.

## 9. Observability & security

Spans `a2a.request{method,op}`, metrics `agentd_a2a_requests_total{method,role,
outcome}`, `agentd_a2a_tasks{state}`, `agentd_conversations`; audit on every
request and every command; per-principal rate limits (`quotas.rate`) and
budgets (RFC 0026 §7 scope `principal`); deny-by-default authorization; the
lethal-trifecta gate applies to the grant a role's tools carry.

## 10. Test plan

Authorization matrix per role/method/op; command validation and result shapes;
NL turn with tools through the mock LLM; conversation persistence and
`GetTask` after SIGKILL/restart; streaming frames from a run; human gate over
a task; steering a warm subagent; agent card generation; conformance family
`a2a-conversation`.
