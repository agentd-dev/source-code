# The operator extension

| | |
|---|---|
| URI | `https://agentd.dev/a2a/ext/operator` |
| Schema | [`https://agentd.dev/a2a/ext/operator/schema.json`](https://agentd.dev/a2a/ext/operator/schema.json) (JSON Schema draft 2020-12) |
| Kind | A2A method extension: it adds the methods `agentd.operator/<op>` |
| Required | never |
| Applies to | its own methods; `task-annotations` applies to the ones that answer with a Task |

This page is the normative specification of the URI above. It is served at
that URI.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST, and a version in a name agentd owns would be
a second name for the same thing. An incompatible change takes a new URI with a
new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's meaning MUST NOT
change under a peer that already speaks it.

An operator drains the instance, pauses it, approves a device, revokes a
session, signs the User in at a Company, answers a consent gate; a display
client reads the instance's status, a run, a transcript, a log. None of these is
a message to the agent: each is a call on the instance, with arguments and an
answer. A2A puts such calls in methods (§4.6.2 lets an extension add them), so
this extension declares one method per op, namespaced under the extension's
name — `agentd.operator/admin.drain` — so a method the specification defines
later can never collide with one. The workload — starting a workflow,
signalling it, talking to a subagent — stays a message, under the
[command extension](command.md), where a peer and a stock A2A client reach it.

An op keeps its name: `admin.drain` in a principal's `grants`, an inbound
token's `scopes_to_skills` (`command:admin.drain`) or a policy means the method
`agentd.operator/admin.drain`, and nothing else.

## Params

The declaration on the agent card carries these `params`. The public card
carries the vocabulary agentd can answer, identical on every instance. The
extended card (`GetExtendedAgentCard`) narrows it to what this instance serves
and this caller may call.

| key | type | card | meaning |
|---|---|---|---|
| `methodPrefix` | string | public | always `"agentd.operator/"`: an op's method is this prefix and the op |
| `schema` | string | public | the URL of this extension's schema bundle |
| `ops` | `[{op, reply}]` | public | the ops, each with its reply kind (`message` or `task`). On the public card, every op any build serves; on the extended card, the ops this instance serves and the caller may call |
| `settable` | string[] | extended | the paths `admin.set` accepts, on an extended card whose caller may call it |

## Activation

A method of this extension is answered only when the request's
`A2A-Extensions` header names this URI. Without it the method is `-32601`
(`EXTENSION_NOT_ACTIVATED`), before any authorization runs — so a peer that
activates the command extension alone never reaches an operator op, whatever
its grants. The response's `A2A-Extensions` header echoes this URI, and
`task-annotations` too when it was asked for and the method answers with a
Task.

## The call

A JSON-RPC 2.0 request whose `method` is `agentd.operator/<op>` and whose
`params` are the op's arguments — the same arguments the op's row of the schema
bundle names, and no `op` member. Absent `params` are no arguments.

```json
{"jsonrpc": "2.0", "id": 1, "method": "agentd.operator/admin.pause",
 "params": {"run": "triage-01J…", "reason": "look"}}
```

The params are held to the op's schema (`$defs/ops/<op>/params`); a
member the schema does not name is refused, never ignored. The request states
`A2A-Version: 1.0` like every other.

A workflow calls one with `a2a.send`, `command` naming the op and `args`
its params; the step's output is the result
([node registry](../node-registry.md#a2asend)).

## Replies

A method answers in one of two ways, given by its `reply` column below and by
`params.ops[].reply` on the card.

- **`message`, a read.** The JSON-RPC `result` is the op's document. No task is
  created, so a client that polls a read leaves nothing behind in any task
  list.
- **`task`, work.** The `result` is the `Task` the call created, as `GetTask`
  would answer it — at once, never waited on: the caller follows it with
  `GetTask` or `SubscribeToTask`. When it completes with a result, the task
  carries one artifact, `<taskId>.result`, whose `DataPart` holds the result. A
  method whose result schema is `false` completes with no result artifact, its
  status message saying what was done. Its history holds the call, as a
  `DataPart` under `agentd`.

Each method's result schema is `$defs/ops/<op>/result` in the bundle.
For a `message` method it is the `result` itself. For a `task` method it is the
data of the result artifact.

## Methods

Every method agentd serves, in the order the card lists their ops.

- **who may call** is the op's floor. `operator` means the operator role alone,
  and no grant reaches it. `any named caller` means every authenticated
  principal. The remaining rows are open to the roles named and to any principal
  whose `grants` name the op.
- **served** is the switch that serves the op. A method behind a closed switch
  is `-32601`, except the introspection reads, which say that introspection is
  off.

| method | reply | who may call | served | description |
|---|---|---|---|---|
| `agentd.operator/status` | message | any named caller | always | Liveness and a snapshot of this instance, as the caller may see it: its runs, conversations and activity; an operator sees everything |
| `agentd.operator/config` | message | operator | always | The effective configuration, credentials redacted |
| `agentd.operator/workflow.cancel` | task | `user` or a grant | always | Cancel one run by id |
| `agentd.operator/subagent.status` | message | `user` or a grant | always | The status of one subagent, and its result once it has one |
| `agentd.operator/subagent.log` | message | operator | always | The last lines of one subagent's log {handle, lines?, after?} and a cursor to tail it with: an instance child's own log file, a flat worker's lines from this instance's log ring. Read-only |
| `agentd.operator/plan.get` | message | `user` or a grant | always | One conversation's current plan and its progress |
| `agentd.operator/conversation.get` | message | `user` or a grant | `a2a.introspection.enabled` | One conversation's transcript, message bodies included |
| `agentd.operator/run.get` | message | `user` or a grant | `a2a.introspection.enabled` | One run with per-step status, timings, errors and output |
| `agentd.operator/subagent.get` | message | `user` or a grant | `a2a.introspection.enabled` | One subagent's instruction, attempts, result and error |
| `agentd.operator/debug.events` | message | operator | `a2a.introspection.enabled` | A cursor read of the live log ring, across every principal |
| `agentd.operator/admin.drain` | task | operator | always | Begin a graceful drain, then exit 0 |
| `agentd.operator/admin.pause` | task | operator | always | Hold the instance, or one run, at a safe boundary |
| `agentd.operator/admin.resume` | task | operator | always | Clear a prior pause |
| `agentd.operator/admin.cancel` | task | operator | always | Cancel one run by id, whoever started it |
| `agentd.operator/admin.set` | task | operator | always | Set a runtime-settable path (agent.approval, a2a.introspection.enabled) until the next reload |
| `agentd.operator/auth.device.pending` | message | operator | `a2a.device_grant.enabled` | Device sign-ins waiting for an operator's decision |
| `agentd.operator/auth.device.approve` | task | operator | `a2a.device_grant.enabled` | Approve a device sign-in {user_code, as, scope?}; every session approved as one name is one principal |
| `agentd.operator/auth.device.deny` | task | operator | `a2a.device_grant.enabled` | Refuse a pending device sign-in {user_code}, or all of them {all: true} |
| `agentd.operator/auth.sessions` | message | operator | a TCP listener | The signed-in sessions: kind, name, principal and expiry |
| `agentd.operator/auth.sessions.revoke` | task | operator | a TCP listener | End one session {sid}, every session of a name {name}, or all {all: true} |
| `agentd.operator/payment.allowances` | message | operator | an allowance (`security.payments.allowances`) | What each spending allowance has left: its currency, its period and when the current one started, its budget, what was spent in it and how many payments were made. Read-only |
| `agentd.operator/company.status` | message | operator | a `companies` entry | Each declared Company as the supervisor knows it now: verified or why not, its domain and issuer, the User's accounts there by label and whether the Session its MCP servers act with is signed in, until when it asked for no requests, and its MCP servers |
| `agentd.operator/company.user_id.reset` | task | operator | a `companies` entry | Forget the User ID one Company knows the User by {company}, with its Session and MCP connections: the Company sees a new User from the next call on |
| `agentd.operator/company.sign_in` | task | operator | a `companies` entry | Ask the User to sign in at a Company {company, account?, scopes?, new_account?} — to the account `account` labels, the only one held, else `default`: the task is the User's gate, closed when they have signed in on the Company's own page, and says how it ended. The configured User's alone |
| `agentd.operator/company.sign_out` | task | operator | a `companies` entry | Sign the User out of one account at a Company {company, account?} — the one `account` labels, or the only one held: its Account Token is revoked there and forgotten here, and every other account stays signed in. The configured User's alone |
| `agentd.operator/delegation.enrol` | task | any named caller | an identity provider or a `grant: id_jag` server | Sign the caller in at an identity provider {idp, grant?} and keep their delegation grant — a per-person operator, an a2a.login session, or a device session whose name a pin or the first binding makes eligible: the task says where to sign in (a display session's start link is its signIn annotation), and how it ended. The caller's own enrolment alone |
| `agentd.operator/delegation.list` | message | any named caller | an identity provider or a `grant: id_jag` server | The delegation grants and the open consent gates — every one to an operator, the caller's own to anyone else; never a secret — each gate with its nonce and argument hash for the person who answers it |
| `agentd.operator/delegation.approve` | task | any named caller | an identity provider or a `grant: id_jag` server | Approve one consent gate {gate, nonce, args_hash}, or several of one shape {gates: [...]}: the calls they hold run. Each bound to its gate's nonce and arguments; the person's alone, through a session whose own IdP sign-in is recent enough — or an operator's where the person's template lists one among its approvers |
| `agentd.operator/delegation.deny` | task | any named caller | an identity provider or a `grant: id_jag` server | Deny one consent gate {gate, nonce}, or several of one shape {gates: [...]}: the calls they hold are refused, NOT made; a step-up gate's sign-in is cancelled. The person's alone, as delegation.approve says |
| `agentd.operator/delegation.revoke` | task | any named caller | an identity provider or a `grant: id_jag` server | Revoke the grants {target?} names — a principal, or idp:<name>#<sub> for every grant of that subject; the caller's own when absent: RFC 7009 at the IdP, then the tombstone, the record and every token. An operator revokes anyone's; anyone else their own alone |
| `agentd.operator/delegation.forget` | task | operator | an identity provider or a `grant: id_jag` server | Erase a person {principal}: their grants revoked as delegation.revoke does, then their personal key destroyed — what it sealed in the audit chain, the arguments their consent gates showed, is unreadable from then on, and the chain still verifies |
| `agentd.operator/delegation.audit` | task | operator | an identity provider or a `grant: id_jag` server | A person's consent records {principal?} from the audit chain, the latest first, as the task's result — each with what the gate showed opened under their personal key, or `unreadable` once they were forgotten; read off the loop. The per-person operator reads their own; any other operator names a principal |
A run's lifecycle has no method here where the core protocol already has one: a
run started with `workflow.run` — or by a workflow's declared command — is
tracked by a task, so `CancelTask` on that task cancels the run, and `GetTask`
and `ListTasks` read it. A task's state is coarser than the run's status:
`running`, `suspended`, `paused` and `pending` are all `TASK_STATE_WORKING`, and
`stalled` is `TASK_STATE_FAILED`. The run's exact status, its workflow, and every
run the caller may see — a scheduled or webhook-started run has no task — are in
`agentd.operator/status`'s `runs`. `workflow.cancel` and `admin.cancel` stay
methods because they reach a run by id, task or not: a run a signal started, or
a schedule.

## Errors

Refusals are JSON-RPC errors whose `data` carries a `google.rpc.ErrorInfo` with
the reason below, and a `google.rpc.BadRequest` naming the param for malformed
params. Nothing refused is run.

| code | reason | when |
|---|---|---|
| `-32602` | `INVALID_COMMAND_ARGS` | the params do not match the op's schema; `BadRequest` names each miss as `params.<member>` |
| `-31403` | `PERMISSION_DENIED` | the caller's role and grants do not reach the op (HTTP 403) |
| `-32603` | `DRAINING` | the agent is draining and takes no new work |
| `-32004` | `INTROSPECTION_DISABLED` | an introspection read while `a2a.introspection.enabled` is off |

A method whose switch is off, like a name that is no method, is `-32601`. So
is a method called without this URI in the `A2A-Extensions` header, its
`ErrorInfo` reason `EXTENSION_NOT_ACTIVATED` (see [Activation](#activation)) —
the listener's refusal of every extension method, before authorization.

## Security

- **The floor is the op's.** A `user` holding `grants: ["*"]` still reaches no
  `operator` method. An op that drains the instance, relaxes its approval
  policy or reads every principal's log lines is an operator's, whatever a
  grant says.
- **Ownership is enforced at the object.** A method that names a run, a
  subagent or a conversation acts only on one the caller started. An operator
  acts on any.
- **Activation is not reach.** Naming this URI in the header lets a request
  reach the methods; the op's floor and the caller's grants decide which.
- **An instance child calls none.** The channel a child was handed carries its
  `_instance.*` reports, and no method of this extension.
- **The public card promises no reach.** It lists the vocabulary, not what this
  instance has switched on or what this caller may call.

## Schema

The bundle at [`schema.json`](https://agentd.dev/a2a/ext/operator/schema.json)
validates a call at its root — the JSON-RPC request naming one of the methods,
its `params` held to that method's — and holds, under `$defs/ops/<op>`, each
op's `method`, `reply`, `description`, `params` and `result`.
`agentd --extension-schema operator` prints it. It is generated from the values
the listener validates against, and CI fails when the published copy differs
from them. Golden examples, each a call valid against the bundle, are published
beside it under
[`examples/`](https://github.com/agentd-dev/source-code/tree/main/web/public/a2a/ext/operator/examples).

## Related

- [command.md](command.md): the workload, as a DataPart on `SendMessage`.
- [events.md](events.md): the observation feed, which a display client
  bootstraps with `agentd.operator/status`.
- [task-annotations.md](task-annotations.md): what a task a method created
  carries.
- [../a2a.md](../a2a.md): the listener, its pipeline and its core methods.
