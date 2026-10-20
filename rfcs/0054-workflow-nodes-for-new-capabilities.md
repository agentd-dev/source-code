# RFC 0054: Workflow nodes for the capabilities agentd shipped in October 2026

**Status:** Accepted 2026-10-19 under the user's rule; implemented
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-19
**Builds on:** RFC 0050 (MCP Tasks), RFC 0051 (skills), RFC 0047 (Companies), RFC 0044 P1 (delegation), RFC 0052 (Client ID Metadata Document), RFC 0053 (AGENTS.md), RFC 0027 (durable waits), RFC 0043 (human gates on the A2A listener)

agentd paths are relative to `crates/agentd/src` unless they start with
`crates/`, `docs/` or `rfcs/`.

---

## 1. Summary

The user's rule (2026-10-19): *"all capabilities we do — we thinking on how
they could fit workflows as well... so when possible — meaningfully adding
nodes to workflows!"* This RFC applies it to the six capabilities agentd
shipped this month. *Meaningful* means a node or a field that lets a
workflow express something it cannot express today, not a new name for a
tool call. Where `tool`, `mcp.tool`, `http`, `wait {on: …}`, a start node or
a `human` gate already expresses a capability, this RFC says so and adds
nothing.

The outcome:

| Capability | Decision |
|---|---|
| MCP Tasks (RFC 0050) | Two fields on `mcp.tool` and `tool`: `detach` and `input`. One new wait form: `wait {on: mcp_task}`. `mcp.task.get` and `mcp.task.cancel` reach a run's detached tasks from its `tool` steps. |
| Skills (RFC 0051) | No new node or field. An ambiguous reference in a step's `skills:` is refused at load, naming the candidates, and the skills a `think` step (and each preset that runs as one) names now reach its prompt. |
| Companies (RFC 0047) | Nothing new. |
| Delegation (RFC 0044 P1) | Nothing new. The guide documents how a run reaches a delegated server and how the person's gate shows. |
| Client ID Metadata Document (RFC 0052) | Nothing. |
| AGENTS.md (RFC 0053) | Nothing. |

## 2. The capabilities

### 2.1 MCP Tasks (RFC 0050)

| | |
|---|---|
| **Today** | An `mcp.tool` step, or a `tool` step on an MCP server's route, whose call becomes a task suspends on a durable wait until the task ends. The step's `timeout` bounds that wait and cancels the task when it passes. An `agent` step's turn hands an unfinished task over, and a `tool` step of the same run reads it with `mcp.task.get`. A question the task asks (`input_required`) is declined, because the supervisor's connections declare no `elicitation`. |
| **Missing** | (1) A task that outlives the step that started it. The step holds its place until the task ends, and its `timeout` cancels the task. A workflow cannot start a build, ask a person something, and then collect the build. It cannot wait in slices either (wait 10 minutes, report progress, keep waiting) without the first slice cancelling the work. (2) A fan-out of long tasks. A `foreach` element holds one of at most 8 lanes for as long as its task runs, so 200 two-hour jobs take 50 hours. With a detached start, each element takes a lane only for the call. (3) An answer to the task's question. Today the answer is always `decline`, so a tool that asks before it acts can never finish. |
| **Decision** | `detach: true` on `mcp.tool` and `tool` (D1, D2). A new wait form, `wait {on: mcp_task, task}` (D3). `input: {to, timeout}` on `mcp.tool` and `tool`, which puts the task's question to a person through the gate the `human` node uses (D4). The existing `mcp.task.get` and `mcp.task.cancel` tools reach a run's detached tasks from its `tool` steps (D2). |
| **Why fields, not a node** | A separate `mcp.task.start` node would repeat everything `mcp.tool` checks and carries: the server, the arguments, `idempotency`, `breaker`, `rate`, the catalogue's pacing, the `_meta` and the trace. `detach` says "the same call, not waited for", which is all that changes. The wait is a `wait` form because every other durable wait in agentd is one (`on: run`, `on: subagent`, `on: message`), so it gets `timeout` and `on_timeout` without new machinery. The question is a field of the step that made the call because the server asks it about that call; a separate node would have nothing to wait on until the task asked. |

### 2.2 Skills (RFC 0051)

| | |
|---|---|
| **Today** | `skills:` on `agent`, `think`, `classify`, `extract`, `summarize`, `judge`, `route` and `subagent` takes references: `<name>`, `<origin>/<name>`, `<origin>/<path>`, and `local/<name>` for agentd's own skills. Each is resolved when the step runs. An unknown or ambiguous one is logged (`skills.unknown`) and told in the step's prompt. |
| **Missing** | Nothing a workflow cannot express. There were two defects. An ambiguous bare name in a step was found only when the step ran, and only by the model reading a note in its prompt. A `think` step, and each preset that runs as one (`classify`, `extract`, `summarize`, `judge` and `route` on a chat model), loaded the skills it named and logged `skills.loaded`, but then left their bodies out of the prompt. |
| **Decision** | No new node or field. At load, a reference that the catalogue resolves to more than one skill is refused, and the error names every candidate in its full `<origin>/<path>` form. The check runs at startup, on a reload that builds the catalogue again (its section or the inline skills moved, or a local skills folder is configured), which re-reads the workflows to check them, for a stored definition and for one the agent writes. An entry that is not a non-empty string is refused at load. A reference that resolves to no skill is still told at run time: its server may be down at load, or the skill may be reachable only through `skills/get`. A `think` step's prompt now carries the bodies of the skills it names, and a line for each that loaded nothing. |
| **Why** | The qualified forms already express every choice. What was missing was the refusal: a skills name is now a load error like any other ambiguous name. |

### 2.3 Companies (RFC 0047)

| | |
|---|---|
| **Today** | Every `company.*` tool is a `tool` step: `company.sign_in`, `company.converse`, `company.read`, `company.handoff`, `company.close`, `company.api`, and `company.operation.get`, `.confirm` and `.cancel`. Each operation tool made from `api_allow` (P5a) is a `tool` step too. `http` with `auth: {kind: company, company}` is a deterministic `company.api` call. The stream `company.<name>` carries conversation events, with the subject set to the event type (`message`, `state`, `authorization`, `user_requested`, `cursor_expired`) and the correlation set to the conversation id. It also carries an operation's final state once a re-read reaches it, with the subject `operation` and the correlation set to the operation id. `wait {on: event, stream: company.<name>, subject, match}` waits for either. The gates (sign-in, approval, cross-Company) belong to the tools, so a step that calls one waits on its gate. |
| **Considered** | `wait {on: company_operation, company, operation}`, for an operation's final state, and `wait {on: company_message, company, conversation}`, for the Company agent's next message. |
| **Decision** | Nothing new. |
| **Why** | Neither condition the user's rule names holds. **The match is expressible.** `match` is CEL over `event`, `inputs` and `vars`, so an `assign` puts the operation or conversation id into a run variable, and `match: "event.correlation == vars.operation"` names it. `company.converse` and `company.read` with `wait` already return the next message inline. **Ownership and cleanup do not differ.** An operation and a conversation belong to the Company and the User, not to the run. A run that ends owes them nothing, and agentd keeps reading an operation that is not yet final whoever waits for it. An event wait is anchored where the stream stood when it armed, so a final state appended before the wait armed is not seen. That holds for every event wait, and the guide's pattern covers it: give the wait a `timeout` whose `on_timeout` step reads the operation again with `company.operation.get`. |

### 2.4 Delegation (RFC 0044 P1)

| | |
|---|---|
| **Today** | An `mcp.tool` step, or a `tool` step, on a delegated server's tool goes the delegated way: the person's grant, a mint and the supervisor's pool. It is admitted only for a run whose principal is the enrolled person. Any other run, including a run a subagent started and every autonomous start, fails the step with the delegation code (`not_enrolled`, `delegation_not_permitted`, …). A consent gate also gates a step's own egress (`Runtime::step_consent`): an `http` step, an `a2a.send` or `a2a.delegate`, an `emit` that forwards, or an `mcp.tool` or `mcp.resource` step at a server tagged `egress`, in a run that holds delegated data. The step suspends on the person's gate. Approved, it runs with the spec the person saw. Denied, or unanswered within `identity.delegation.consent_timeout`, it fails with `delegation refused (policy_denied)`, and `on_error` routes that failure. A delegated call declares no tasks, so `detach` on one is answered inline. |
| **Missing** | Nothing a workflow cannot express: the gate is the step's, and a branch on a refusal is `on_error: "goto:<step>"`. |
| **Decision** | Nothing new. The guide documents the reach, the gate and its failure text. |

### 2.5 Client ID Metadata Document (RFC 0052) and AGENTS.md (RFC 0053)

| | |
|---|---|
| **Today** | The Client ID Metadata Document is how agentd names itself at an authorization server where it has no registration. It is the operator's configuration (`security.client`) and is presented at a login, not by a step. AGENTS.md is text in the stable part of every turn's system prompt, so an `agent` step's prompt already carries it. |
| **Decision** | Nothing. |
| **Why** | Neither is something a run does. A node would be a renamed configuration key. |

## 3. Design: MCP Tasks in workflows

**D1. `detach: true` on `mcp.tool` and `tool`.** When the call becomes a
task, the step finishes `done` at once with `{task: {server, id, status}}`.
`id` is agentd's handle for the task (`mt_` and 16 hex digits, RFC 0050 D2),
never the server's task id, and `status` is the status the server's
`CreateTaskResult` gave. When the server answers at once, the step's output
is that answer, exactly as without `detach`. That happens when it chose to,
when the server is at its 32 open tasks, when it runs `tasks: off`, or when
it is a Company's or a delegated server, which declare no tasks. `detach` is
a literal boolean (a template would decide at run time what the load
checks), it is refused beside `cache` (a cached output would hand a later
run a task it does not own), and on a `tool` step whose tool is not on an
MCP server's route it is refused at startup. A detaching step's `timeout`
bounds the call until the server answers or makes the task. From then on
the task is bounded by its own TTL, by any wait on it, and by the run.

**D2. The run owns a detached task.** A detached task is a record like a
step's (`_mcptask/<handle>`, RFC 0050 D2), owned by the run and the step that
detached it (`Owner::Detached`). It is polled, pushed and limited per server
as every other task is. **When the run ends — completed, failed or
cancelled — with the task still open, agentd sends `tasks/cancel`**, and the
record goes. That is the rule an `agent` step's handed-over task already
follows (RFC 0050 D3), and the rule a `workflow` step's async child follows
(`cascade`). A task that has ended is kept, its outcome included, until the
run ends, so any number of waits in the run can read it. `tool {name:
mcp.task.get, args: {task}}` reads its status and, once it has ended, its
outcome, without dropping it. `tool {name: mcp.task.cancel, args: {task}}`
sends `tasks/cancel` and keeps following the task. Cancelling is
cooperative (RFC 0050), so a wait on the task resolves with what the server
then says: usually `cancelled`, or `completed` if the work finished first.
A crash between the task's record and the step's checkpoint replays the
call under the same idempotency key (RFC 0050 D6). A server that answers it
with the same task makes the same handle, and agentd keeps one record.

**D3. `wait {on: mcp_task, task: <handle>}`.** The wait resolves when that
task ends, with `{task, server, tool, status, ok, result?, error?}`. `status`
is `completed`, `failed` or `cancelled`, or `failed` when agentd stopped
following the task (the server forgot it, it outlived its TTL, a request
about it failed for good), with `error` saying which. `result` is the tool's
result, read as an `mcp.tool` step reads one. `ok` is `true` only for a
completed task whose result is not an error. **The wait resolves `done`
however the task ended**, as `wait {on: run}` does, so the workflow branches
on `status` or `ok`. A task that ended before the wait armed resolves it at
once: unlike an event wait, this one reads the record, not a stream.
**The wait's `timeout` ends the wait, not the task.** The step ends
`timeout`, `on_timeout` routes it, and the task stays the run's: a later
wait may collect it, `mcp.task.cancel` may stop it, and the run's end
cancels it. A `task` that names no task detached in this run fails the
step, saying so. That covers a task another run detached, a step's own
task, a turn's task, and an empty `task` rendered from a detaching step the
server answered at once. The wait is durable: a restart restores the record
and the suspended step, polls the task again, and resolves the wait when it
ends.

`on: mcp_task` follows the registry's naming: `on` values are single words
(`run`, `subagent`, `resource`), and `mcp_task` is the kind of wait record
both a step's own task and this wait keep. The wait takes `task` and nothing
else, because the handle already names the server.

**D4. `input: {to?, timeout?}` puts a task's question to a person.** On an
`mcp.tool` or `tool` step, both its own task (the step suspended on it) and
one it detached, an `input_required` whose request is a form elicitation
becomes the gate a `human` node opens. It is asked over the A2A listener
under the same ownership rules, `agent.ask_human_unowned` and
`agent.ask_human_fallback`, and `agent.approval`, but on a task of its own:
one made for the question, in the run's conversation (`run-<id>` for a run
started in none), held by whoever the run runs for or else the operator. A
task holds one gate, and a task's question comes at any moment, so on the
run's task it would take the gate a later `human` step or policy gate of
the run needs, or be refused while one of theirs is open. Cancelling the
question's task answers `cancel` and leaves the run alone. A blocking send
that started the run is not interrupted by the question; a client following
the run lists its conversation's tasks. `to` names who answers, in the
`human` node's addressee forms (`operator`, `{role: operator, labels}`),
and is checked at load as a `human` node's is.
`timeout` is how long the person has; its default is a `human` node's 24
hours, and the task's own TTL still applies. A step that waits on its own
task ends the task at the step's `timeout` (an `mcp.tool` step's
`limits.step_timeout` by default), and the question with it, so the gate's
deadline is the earlier of the two; a question that may take longer belongs
to a detached task. The question the person sees names the server, the tool
and the task's handle beside the server's own message. Its schema is the
request's `requestedSchema`, so a reply that does not fit is asked again
with the reason, as a `human` gate's is. An answer goes to the server as
the elicitation's `accept` with the answer as its `content`. A gate that
times out, fails or finds nobody to ask answers `cancel`.

The keys of one round are asked one at a time, so the person answers one
form after another, in key order. Their answers go to the server together
in one `tasks/update`, since a server may refuse a partial set (RFC 0050
D5). A URL elicitation in the round is answered `decline`, as it would be
if sent directly, and a request agentd never declares cancels the task (RFC
0050). An answer is kept only in memory until the server acknowledges it.
The record stores a key as answered only once it is acknowledged, so a
restart ends the question's task and asks the person again on a new one
rather than leaving a key unanswered. `input` is taken as written, not
templated: the question arrives long after the step rendered. Without
`input`, the question is declined, as before. A turn's handed-over task is
unchanged.

**D5. Telemetry.** New lines: `mcp.task.detached` (info: `server`, `task`,
`run`, `step`), when a detaching step finishes with its task;
`mcp.task.input.asked` (info: `server`, `task`, `key`, `run`, `step`), when
a question goes to a person; `mcp.task.input.answered` (info: `server`,
`task`, `key`, `action`), with `accept`, `decline` or `cancel` and never the
content; `mcp.task.input.interrupted` (info: `task`, `gate`), when a restart
ends a question's task; `wait.resolved` for a `wait {on: mcp_task}`, as for
every wait. The
gate's own `human.ask` and `human.answered` lines, and the `human.asked` and
`human.answered` events, say the rest. `mcp.task.cancelled` gains the reasons
`the run ended` (a detached task) and `mcp.task.cancel` (from a step).

**D6. Load-time refusals.** `detach` that is not a boolean; `detach` beside
`cache`; `input` that is not an object of `to` and `timeout`, or that is
templated; an `input.to` that is malformed or names someone who could never
see the gate; an `input.timeout` that is no duration; `detach` or `input` on
a `tool` step whose tool is not on an MCP server's route; a `wait` whose `on`
is none of the forms; `on: mcp_task` without `task`; `task` on any other
`on`.

## 4. Exit criteria

- Units: each load-time refusal of D6, and the skills reference checks of
  §2.2.
- End to end against agentd's mock MCP server (`mock.job`): a detached task
  collected by a wait once it completes; one cancelled with `mcp.task.cancel`;
  one that expires; a wait's timeout that leaves the task to the run, which
  cancels it at its end; a `foreach` that detaches several tasks and
  collects them; a step whose server answers at once; a restart while
  waiting on a detached task, the call not sent again; a step task's
  question answered (by the operator's stand-in in the default build, by a
  person over the A2A listener with `a2a`) and the answer reaching the
  server; over A2A, two tasks' questions and a `human` gate open at once, a
  run that ends with a question open keeping its own task completed, and a
  restart ending a question's task and asking again on a new one; a step's
  own task's question ending with the step's timeout; the same question
  declined with no `input`; a qualified skills reference and an ambiguous
  one refused at load, and on a reload whose new skill makes it ambiguous; a
  `think` step's skill in its prompt.
- Docs: the node registry, a "Capabilities in workflows" section in the
  workflow guide with one sample each that validates, the telemetry
  reference, the CHANGELOG.
