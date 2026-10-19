# RFC 0050: MCP Tasks — a server's long tool calls, followed to the end

**Status:** Accepted 2026-10-19, as track T of RFC 0048 (§5.1); implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Builds on:** RFC 0045 (MCP Events: an extension declared per request on 2026-07-28 connections, durable state, `subscriptions/listen`), RFC 0027 (durable waits), RFC 0026 (turn workers), ADR 0001 (MCP extensions: the client side only)
**Standard:** the MCP Tasks extension `io.modelcontextprotocol/tasks`, SEP-2663 (Final), on the MCP base revision 2026-07-28. Pinned text: [ext-tasks `specification/2026-07-28/tasks.md` at `899c1bbc86`](https://github.com/modelcontextprotocol/ext-tasks/blob/899c1bbc869567670093face3591b39c28ce3219/specification/2026-07-28/tasks.md) (2026-10-09), cited below as [T]; the input-request shapes are the base revision's multi-round-trip requests, [MRTR](https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns/mrtr).

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

A server that cannot finish a `tools/call` in one answer may hand back a
*task*: a handle the client polls until the work ends [T, Task Creation]. The
extension replaced the 2025-11-25 experimental core tasks; a client that
declares only the old capability counts as not declaring it, so agentd
implements the extension and nothing older.

agentd is a Tasks **client** (ADR 0001). On 2026-07-28 connections it
declares the extension on the requests that may create or follow a task,
accepts either answer to `tools/call`, and follows a task to its end: a
workflow step's as a durable wait that survives a restart, a model's within
the call's own time and then by tools the model calls, a store adapter's
inside its bounded call. A call with no bound of its own does not offer
tasks. It never serves tasks.

## 2. What the client does

| Duty [T] | agentd |
|---|---|
| Declare the extension per request | `_meta["io.modelcontextprotocol/clientCapabilities"].extensions["io.modelcontextprotocol/tasks"] = {}` on a `tools/call` that carries a bound (D1), `tasks/get`, `tasks/update`, `tasks/cancel`, and a `subscriptions/listen` that asks for `taskIds`. The connection's other capabilities ride beside it unchanged. |
| Accept either answer to `tools/call` | A `CallToolResult` is the call's result, as before. A `CreateTaskResult` (`resultType: "task"`) starts a task. It is the base task only (`Result & Task`), so its seed may be in any status without what that status carries elsewhere: a seed that is not `working` is fetched with `tasks/get` at once (at the 250 ms floor), and a final status counts only once its `result` or `error` has come. |
| A `CreateTaskResult` on any other method | An invalid answer, refused as one: `prompts/get`, `resources/read`, `tasks/*`, an extension's methods. So is one on a `tools/call` that did not declare the extension. |
| Track a task | `taskId`, `status`, `statusMessage`, `createdAt`, `lastUpdatedAt`, `ttlMs` (it may change; `null` is unlimited), `pollIntervalMs`, and the input-request keys answered. |
| Poll | `tasks/get {taskId}` at the server's `pollIntervalMs`, within agentd's floor and ceiling (§3, D4). |
| The outcome | `completed` carries `result`, the `CallToolResult` (an `isError: true` result included); `failed` carries `error`, a JSON-RPC error; `cancelled` carries nothing. The outcome is read from the task, never fetched again. |
| `input_required` | Each entry of `inputRequests` is treated as the same request sent directly would be on that connection (D5); a round's answers go back together in one `tasks/update {taskId, inputResponses}`, since a server may refuse a partial set; a key answered once is never asked again. |
| Cancel | `tasks/cancel {taskId}`, never `notifications/cancelled`. Cooperative: the task may still end otherwise, and agentd drops its record at once [T, Task Cancellation]. agentd also cancels a task it gives up while the server may still run it (D2). |
| Push | `subscriptions/listen` with `notifications.taskIds`; `notifications/tasks` carries the same `DetailedTask` a poll would (D9). |
| Routing | On Streamable HTTP, `Mcp-Name: <taskId>` on every `tasks/*` request (the SDK sets it from `params.taskId`, beside `Mcp-Method`). |
| Errors | `-32602` on `tasks/get` or `tasks/update`: the task is unknown or expired, and the wait ends `failed`, saying which. `-32021`: the server needs a capability the request did not declare (D1). `-32603`: an internal error, retried as a transport failure is. A `tasks/get` answer about another `taskId` is no answer about this one, and is retried as a failed poll. |

The wire is rmcp's: rmcp 3.5 models SEP-2663 (`CreateTaskResult`,
`GetTaskResult`, `tasks/get|update|cancel`, the `Mcp-Name` rule), so agentd
adds no wire types of its own beyond the view `agentd-mcp` hands its host
(`wire::Task`). rmcp's `call_tool` helper does not follow tasks, so
`agentd-mcp` drives `tools/call` and its MRTR rounds itself on a task-declaring
connection, through rmcp's per-request `_meta`.

## 3. Design decisions

**D1. Where the extension is declared.** On a 2026-07-28 connection agentd
declares it on every request that may create or follow a task: a
`tools/call` that carries a bound (a turn's, a workflow step's, a store
adapter's; D8), the three `tasks/*` methods, and a listen that asks for task
notifications. A `tools/call` with no bound does not declare it: nothing
could end the follow of a task it became, so the server answers it inline,
or refuses with `-32021`. A connection on 2025-11-25 or older declares
nothing and sends no `tasks/*`.
`mcp.servers[].tasks: auto | off` (default `auto`) lets an operator refuse a
server's tasks; `off` declares nothing, so the server must answer inline. A
server that requires the extension answers `-32021` naming it, and agentd
fails the call with a capability error that names the extension and the
code, never a bare transport error. The setting is part of the server's
dial: a reload that changes it re-dials, and a worker's connection follows
the supervisor's (it travels in the spawn payload). A Company's servers
(RFC 0047) are dialled through the User's Session and declare nothing.

**D2. A task is durable work.** A task a workflow step or a turn left to the
supervisor is a record in the store (`memory` kind, `_mcptask/<handle>`, and
an index, `_mcptask/index`): the server, the tool, the `taskId`, the status
and its message, the times, `ttlMs`, `pollIntervalMs`, the input-request keys
answered, and who asked — a run and step, or a conversation and its
principal. The *handle* (`mt_` and 16 hex digits of SHA-256 over the server
and the `taskId`) names the record everywhere agentd speaks of it — logs,
metrics, the model's tools — because a server may use task ids as bearer
tokens [T, Security Considerations]; the `taskId` itself is kept only in the
record and sent only to its server. On restart agentd reads the records back
and polls again. A `-32602` ends the wait `failed` (`unknown or expired
task`), and so does a task still open more than `ttlMs` past the moment
agentd first saw it (plus 30 s, so a clock that runs ahead never ends one
early). A task with `ttlMs: null` waits until it ends or its waiter goes.
A task agentd gives up while the server may still run it — past the TTL
backstop, or a `tasks/*` request refused for good — is cancelled with
`tasks/cancel`, so a step's `retry:` does not leave two runs of the work at
the server; one the server said it does not know is not.

**D3. Who waits, and how.**

- *A workflow step.* A `tool` step (an MCP route) or an `mcp.tool` step whose
  call becomes a task suspends as a durable wait (`{kind: mcp_task, task:
  <handle>}`) until the task ends; its outcome finishes the step as the call's
  answer would have. The step's `timeout` (for `mcp.tool`, as before, the
  step's or `limits.step_timeout`) bounds the wait, measured from the step's
  start: at it the step ends `timeout` and agentd sends `tasks/cancel`. A run
  cancelled, or a step that ends any other way while its task is open (a
  nested scope torn down, a `race` lost), sends `tasks/cancel` too.
- *A model's turn.* The turn worker follows the task on its own connection
  within the call's own bound (the turn's remaining time, at most 600 s),
  less a margin of half the bound or 30 s, whichever is less, so the model
  still has time to act on what it is told. If the task ends, its outcome is
  the tool result. If the margin is reached first, the worker hands the task
  to the supervisor (a request of the worker's own, `mcp.task.hand_over`,
  which no model can name), which keeps it as a record of the turn's
  conversation, and the model gets a result naming the task's handle and
  status and the tools that follow it: `mcp.task.get {task}` (its status,
  and its outcome once it ended — handed over once, then the record is
  gone) and `mcp.task.cancel {task}`. Both are internal tools, granted to
  root and workflow turns, and answer only the conversation (or run) and
  principal the task was made for; a workflow `tool` step of the same run
  may read a task its `agent` step handed over. A task an `agent` step
  handed over belongs to its run: when the run ends — finished, failed or
  cancelled — an open one is cancelled and an unread outcome dropped. A
  conversation's task is followed until it ends, the model cancels it, or
  its TTL backstop; an ended task nobody reads is dropped after a day. A
  handed-over task is a record like a step's, so a restart follows it on.
- *A cancelled turn.* The worker's follow looks at the turn's cancel at
  least every 100 ms, between polls and while a person is asked. A turn
  cancelled while it follows its task — its run cancelled or failed, the
  drain — sends `tasks/cancel` at once and hands nothing over: nothing would
  read it.
- This deliberately goes past the A2A client's precedent. `a2a.delegate`
  follows a remote task inside its deadline and reports the deadline as an
  error; a tool call a model makes is bounded by the turn, and a task is
  exactly the work that outlives one, so failing it at the turn's bound would
  make every long tool unusable from a model. The workflow side does follow
  the precedent's shape (one wait, one outcome), made durable.

**D4. Polling.** agentd polls at the task's `pollIntervalMs`, never more often
than every **250 ms** nor less often than every **10 minutes**, and every
**2 s** when the server names none; a new `pollIntervalMs` applies from the
next poll. Nothing is polled below the floor: not a seed's first fetch, not a
question a push brought. One poll per task is in flight at a time. A failed
poll (transport, `5xx`, `-32603`, a timeout) — or a failed `tasks/update`
declining a task's questions — is retried after 1 s, doubling to 5 minutes;
each `tasks/*` request is bounded by 30 s. The supervisor polls on threads of
its own, never on its loop. At most **32** tasks may be open on one server
for the supervisor, its calls in flight that declared the extension counted
among them, so a fan-out cannot pass the limit before its tasks are
recorded; at that many its own calls to that server go out without the
extension until one ends (the server then answers inline, or refuses with
`-32021`), and a turn's task handed over past the limit is still followed.

**D5. Input requests.** A task's `inputRequests` are handled with the same
trust as the same requests sent directly on the connection that made the
call ([T, Task Update Requests]: "a task is not a higher-trust channel"):

- A form elicitation (`elicitation/create`) on a connection that declares
  `elicitation` — a turn worker's — becomes the `ask_human`
  gate a direct elicitation opens: the same addressing, the same schema
  shaping (`mcp::elicit::shape_reply`), the same 300 s after which the answer
  is `cancel`. While the person decides, the worker sends no poll; the next
  one follows the answer, and the key is not asked twice.
- The supervisor's connections declare no `elicitation`, as before, so a
  conforming server never asks on them; one that does is declined, as a
  direct elicitation there is. So is any input request on a task whose turn
  has moved on: nobody is waiting who could be asked.
- A URL elicitation is declined, as directly.
- `sampling/createMessage` and `roots/list` are never declared; a task that
  asks for one is cancelled and its wait fails, naming the method, as the
  direct request fails the call.

A round's answers go back together in one `tasks/update`, sent again until
the server acknowledges them; a key joins the answered set when it is given,
so a key the server lists again after it was answered (the update is
eventually consistent) is not asked again. A person's answer is kept only in
memory, never in the store, so a turn's task handed over has every answer
given sent first; one the server still has not acknowledged has its key
released, and the supervisor declines it, as it declines any question whose
turn has moved on. The supervisor declines from the poll's own thread, every
key of the round in one `tasks/update`, before the loop records the keys, so
a restart in between declines again rather than leaving a key unanswered. A
decline the server refuses with `-32603` (or the transport loses) is a failed
poll and waits as one (D4); `-32602` ends the wait as a task the server does
not know; any other refusal fails it.

**D6. Exactly once, and outcome unknown.** A `tools/call` whose answer never
came stays *outcome unknown*, never re-sent, as before: the task, if there
was one, was never seen. A task's outcome is handed to its step or turn
exactly once: the record is removed in the same loop pass that delivers it,
and a poll or a notification that lands after finds no record and is
dropped. A crash between the task's creation and its record's write replays
the step as any running step is replayed (its idempotency key unchanged), and
the orphaned record of an earlier attempt, whose step no longer waits on it,
is cancelled.

**D7. Telemetry.** Events `mcp.task.created`, `mcp.task.status`,
`mcp.task.input.requested`, `mcp.task.input.declined`,
`mcp.task.completed`, `mcp.task.failed`, `mcp.task.cancelled`,
`mcp.task.expired`, `mcp.task.poll.fail`, `mcp.task.handed_over`,
`mcp.task.resumed` and `mcp.task.listen.unavailable` (§5); the gauge
`agent_mcp_tasks_open{server}` and the counter
`agent_mcp_tasks_total{server, outcome}`, both of the tasks the supervisor
follows. Each `tasks/*` request the supervisor sends is a client span under
the span of the call that made the task, so a task's polls sit in that
call's trace; a worker's carry that call's `traceparent` in `_meta`. A line
carries the handle, never the `taskId`; a server's `statusMessage` and an
input key are stripped of control characters and cut at 200 bytes;
results, errors and input requests are never logged. An answer a person
gives is not logged either: the worker's elicitation bridge carries it as
it carries a direct one.

**D8. Children and subagents.** A task belongs to the connection that made
the call. A turn worker follows its own (D3) and hands an unfinished one to
the supervisor, whose connection to the same server polls it from then on —
2026-07-28 has no session, and the server binds a task to the credential,
which both connections present. An instance-tier child is an agentd of its
own and follows its tasks itself. Only a call with a bound offers tasks
(D1). A store adapter's call carries one: it follows its task inline within
it and cancels it there, unrecorded, as the call itself is. The calls with
no bound of their own do not declare the extension, so their server answers
inline or refuses with `-32021`: a flat subagent's (the subagent's deadline
ends the subagent, not the call, and would leave its task running), a mapped
contract's, `knowledge.auto_context`'s, a turn's tool call the supervisor
runs through its registry, and the instruction bindings agentd reports from
its loop.

**D9. Push.** Where the supervisor holds open tasks on a 2026-07-28 server,
its connection's listen asks for their `taskIds`, declaring the extension on
the listen, and takes `notifications/tasks` off the stream; each is applied
as the poll answer it equals. The listen is opened again whenever the set
changes, the old one hung up only once the new one is acknowledged, as a
subscribe's is. A task the acknowledgment names is polled at the ceiling
only, as a backstop for a notification lost while a listen is re-opened;
one it leaves out is polled at the server's interval. A listen asking for
tasks that fails or is refused is not asked for them again on that
connection, so it can never fail the listen that carries everything else;
every task stays on polling.

**D10. The SDK.** rmcp 3.5 models the extension (`CreateTaskResult`,
`GetTaskResult`, `TaskAckResult`, `tasks/*`, the `Mcp-Name` rule) but its
`call_tool` helper refuses a task, and its listen filter has no `taskIds`.
`agentd-mcp` drives a declaring `tools/call` itself — one request per MRTR
round, the connection's capabilities with the extension added through the
request's own `_meta` — and adds `taskIds` to a listen in its own transport,
as it adds MCP Events' members. A `CreateTaskResult` on another method is
refused by rmcp's typed answers, and on an extension's method by
`call_raw`. A follower that agentd keeps (`mcp::tasks::Tracker`) is
agentd's own: the SDK has none.

## 4. Configuration

```yaml
mcp:
  servers:
    - name: builds
      endpoint: https://builds.example.com/mcp
      tasks: auto   # auto (default) | off
```

`tasks` is reloadable: a change re-dials the server. A served instruction
document may set it on a server it declares, as it may the server's other
connection settings.

## 5. Telemetry

| Event | Level | Fields |
|---|---|---|
| `mcp.task.created` | info | `server`, `tool`, `task`, `status`, `ttl_ms`, `poll_interval_ms`, `run`, `step` or `ctx` |
| `mcp.task.status` | info | `server`, `task`, `from`, `to`, `status_message` |
| `mcp.task.input.requested` | info | `server`, `task`, `key`, `method` |
| `mcp.task.input.declined` | warn | `server`, `task`, `key`, `method`, `reason` |
| `mcp.task.completed` | info | `server`, `task`, `is_error` |
| `mcp.task.failed` | warn | `server`, `task`, `code` or `reason`, `detail` |
| `mcp.task.cancelled` | info | `server`, `task`, `reason` |
| `mcp.task.expired` | warn | `server`, `task`, `reason` (`unknown`, `ttl`, `unread`) |
| `mcp.task.poll.fail` | warn | `server`, `task`, `err`, `retry_ms` |
| `mcp.task.handed_over` | info | `server`, `task`, `ctx`, `run` |
| `mcp.task.resumed` | info | `server`, `task`, `status` |
| `mcp.task.listen.unavailable` | info | `server`, `err` |

Input requests the worker answers through its elicitation bridge are said
by that bridge's `ask_human` lines, as a direct elicitation's are.

## 6. Security

- A task is no higher-trust channel (D5): its questions reach a person only
  where a direct question would, and are shaped and timed out the same way.
- Task ids may be bearer tokens: they are kept in the store, sent only to their
  server, and named in logs and to the model by agentd's handle.
- A model reads only the tasks of its own conversation and principal, and a
  result read through `mcp.task.get` is the call's own result, so the tool
  carries no trifecta tag the call did not.
- `statusMessage` is the server's text: logged only stripped and cut, and
  shown to the model as the server's words.
- `tasks: off` is the operator's refusal, and a server that insists is told no
  with an error that names the setting.

## 7. Exit criteria

- Unit tests: the wire view (statuses, payload checks, the invalid answers),
  the poll interval's clamp, the input-key deduplication, the TTL backstop,
  the handle.
- `agentd-mcp` tests against a 2026-07-28 server: the declaration on a
  bounded `tools/call` and only where D1 says, `Mcp-Name` on every
  `tasks/*`, a `CreateTaskResult` on `prompts/get` refused, `-32021` against
  `off`, the inline follow honouring `pollIntervalMs`, a seed that already
  ended fetched for its result, a follow its caller stops, a task given up
  cancelled, and a task handed back with its answers sent or released.
- End-to-end against agentd's own mock server: a step's task working →
  completed at the server's `pollIntervalMs`, failed, a tool error, unknown,
  expired (`-32602`), cancelled at the step's timeout, and its question
  declined on the supervisor's connection; a restart that resumes polling and
  finishes the step without sending the call again; a server that pushes the
  task's end; `tasks: off` against a server that requires the extension; a
  turn's task working → input_required → completed with the gate's answer; a
  turn's task handed over and read once with `mcp.task.get`, and one
  cancelled when its run ends unread; a seed that already ended; a decline
  the server refuses, waiting as a failed poll; a fan-out past the
  per-server limit.
- Docs: interoperability, the MCP reference, connect-mcp, configuration,
  telemetry; the strip lists MCP Tasks (SEP-2663).
