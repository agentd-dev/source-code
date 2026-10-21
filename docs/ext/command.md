# The command extension

| | |
|---|---|
| URI | `https://agentd.dev/a2a/ext/command` |
| Schema | [`https://agentd.dev/a2a/ext/command/schema.json`](https://agentd.dev/a2a/ext/command/schema.json) (JSON Schema draft 2020-12) |
| Kind | A2A profile extension: no new method, no change to a core structure |
| Required | never |
| Applies to | `SendMessage`, `SendStreamingMessage` |

This page is the normative specification of the URI above. It is served at
that URI.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST, and a version in a name agentd owns would be
a second name for the same thing. An incompatible change takes a new URI with a
new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's meaning MUST NOT
change under a peer that already speaks it.

A2A has no tool-call primitive. The protocol's own answer is a message that
carries structured input, so every operation agentd offers over A2A is an
ordinary `SendMessage` whose message carries one `DataPart` under the `agentd`
key. A client that never activates this extension can still converse, list
tasks and subscribe to them. The extension adds reach, and is never a
precondition.

## Params

The declaration on the agent card carries these `params`. The public card
carries the vocabulary agentd can answer, identical on every instance. The
extended card (`GetExtendedAgentCard`) narrows it to what this instance serves
and this caller may run.

| key | type | card | meaning |
|---|---|---|---|
| `dataPartKey` | string | public | always `"agentd"`: the key under a DataPart's `data` that holds the command |
| `schema` | string | public | the URL of this extension's schema bundle |
| `ops` | `[{op, reply}]` | public | the ops, each with its reply kind (`message` or `task`). On the public card, every op any build serves; on the extended card, the ops this instance serves and the caller may run |
| `commands` | `[{op, workflow, schema?}]` | extended | the commands loaded workflows declare that the caller may fire, each with the workflow it starts and the schema its arguments are held to |
| `settable` | string[] | extended | the paths `admin.set` accepts, on an extended card whose caller may run it |

## Activation

A command is a command only when the request says so, twice:

1. the `A2A-Extensions` request header names this URI, and
2. the message lists this URI in `message.extensions`.

The response's `A2A-Extensions` header echoes the URI when it was activated. A
DataPart under `agentd` sent without both is refused (see [Errors](#errors)) and
never run: a client cannot send a command by accident.

## The envelope

The message carries exactly one part whose `data` is the command:

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
  "params": {
    "message": {
      "role": "ROLE_USER",
      "messageId": "m-1",
      "extensions": ["https://agentd.dev/a2a/ext/command"],
      "parts": [{"data": {"agentd": {"op": "workflow.run", "workflow": "triage", "inputs": {"ticket": 42}}}}]
    }
  }
}
```

The object under `agentd` is the envelope: `op`, and the op's arguments beside
it. Each op's envelope is `$defs/envelopes/$defs/<op>` in the schema bundle,
which is the schema the listener validates the envelope against. An argument the
schema does not name is refused, never ignored.

A command message:

- carries no `taskId`: a command starts its own task, or none;
- may name a `contextId`, and runs in that conversation of the caller's;
- if it lists `configuration.acceptedOutputModes`, includes `application/json`
  among them: every command answers with JSON.

## Replies

An op answers in one of two ways, given by its `reply` column below and by
`params.ops[].reply` on the card.

- **`message`, a read.** The answer is a `Message` (`ROLE_AGENT`) with
  one `DataPart`, media type `application/json`, whose `data` is the op's
  result document. Its `extensions` lists this URI. No task is created, so
  a client that polls a read leaves nothing behind in any task list.
- **`task`, work.** The answer is a `Task`. When it completes with a result, the
  task carries one artifact, `<taskId>.result`, whose `DataPart` holds the
  result and whose `extensions` lists this URI. An op whose result schema is
  `false` completes with no result artifact.

Each op's result schema is `$defs/ops/<op>/result` in the bundle. For a
`message` op it is the document in the reply. For a `task` op it is the data of
the result artifact.

## Ops

Every op agentd serves, in the order the card lists them, followed by the
reserved names. The reserved names are never listed in `params.ops`, and no
workflow may declare them as a command.

- **who may call** is the op's floor. `operator` means the operator role alone,
  and no grant reaches it. `any named caller` means every authenticated
  principal. The remaining rows are open to the roles named and to any principal
  whose `grants` name the op.
- **served** is the switch that serves the op. An op behind a closed switch is
  answered as an unknown op, except the introspection ops, which say that
  introspection is off.

| op | reply | who may call | served | description |
|---|---|---|---|---|
| `status` | message | any named caller | always | Liveness and a snapshot of this instance, as the caller may see it: its runs, conversations and activity; an operator sees everything |
| `config` | message | operator | always | The effective configuration, credentials redacted |
| `workflow.run` | task | `user`, `agent` or a grant | always | Start a workflow; the reply is the task it runs under |
| `workflow.status` | message | `user`, `agent` or a grant | always | The status of one run, or of every run the caller started |
| `workflow.cancel` | task | `user` or a grant | always | Cancel one run by id |
| `workflow.signal` | task | a grant | always | Deliver a signal a workflow is waiting on |
| `subagent.send` | task | `user` or a grant | always | Send a message to a warm subagent |
| `subagent.kill` | task | a grant | always | Stop a subagent |
| `subagent.status` | message | `user` or a grant | always | The status of one subagent, and its result once it has one |
| `plan.get` | message | `user` or a grant | always | One conversation's current plan and its progress |
| `conversation.get` | message | `user` or a grant | `a2a.introspection.enabled` | One conversation's transcript, message bodies included |
| `run.get` | message | `user` or a grant | `a2a.introspection.enabled` | One run with per-step status, timings, errors and output |
| `subagent.get` | message | `user` or a grant | `a2a.introspection.enabled` | One subagent's instruction, attempts, result and error |
| `debug.events` | message | operator | `a2a.introspection.enabled` | A cursor read of the live log ring, across every principal |
| `admin.drain` | task | operator | always | Begin a graceful drain, then exit 0 |
| `admin.pause` | task | operator | always | Hold the instance, or one run, at a safe boundary |
| `admin.resume` | task | operator | always | Clear a prior pause |
| `admin.cancel` | task | operator | always | Cancel one run by id, whoever started it |
| `admin.set` | task | operator | always | Set a runtime-settable path (agent.approval, a2a.introspection.enabled) until the next reload |
| `auth.device.pending` | message | operator | `a2a.device_grant.enabled` | Device sign-ins waiting for an operator's decision |
| `auth.device.approve` | task | operator | `a2a.device_grant.enabled` | Approve a device sign-in {user_code, as, scope?}; every session approved as one name is one principal |
| `auth.device.deny` | task | operator | `a2a.device_grant.enabled` | Refuse a pending device sign-in {user_code}, or all of them {all: true} |
| `auth.sessions` | message | operator | a TCP listener | The signed-in sessions: kind, name, principal and expiry |
| `auth.sessions.revoke` | task | operator | a TCP listener | End one session {sid}, every session of a name {name}, or all {all: true} |
| `company.status` | message | operator | a `kind: company` entry | Each declared Company as the supervisor knows it now: verified or why not, its domain and issuer, the User's accounts there by label and whether the Session its MCP servers act with is signed in, until when it asked for no requests, and its MCP servers |
| `company.user_id.reset` | task | operator | a `kind: company` entry | Forget the User ID one Company knows the User by {company}, with its Session and MCP connections: the Company sees a new User from the next call on |
| `company.sign_in` | task | operator | a `kind: company` entry | Ask the User to sign in at a Company {company, account?, scopes?, new_account?} — to the account `account` labels, the only one held, else `default`: the task is the User's gate, closed when they have signed in on the Company's own page, and says how it ended. The configured User's alone |
| `company.sign_out` | task | operator | a `kind: company` entry | Sign the User out of one account at a Company {company, account?} — the one `account` labels, or the only one held: its Account Token is revoked there and forgotten here, and every other account stays signed in. The configured User's alone |
| `delegation.enrol` | task | any named caller | an identity provider or a `grant: id_jag` server | Sign the caller in at an identity provider {idp, grant?} and keep their delegation grant — a per-person operator, an a2a.login session, or a device session whose name a pin or the first binding makes eligible: the task says where to sign in (a display session's start link is its signIn annotation), and how it ended. The caller's own enrolment alone |
| `delegation.list` | message | any named caller | an identity provider or a `grant: id_jag` server | The delegation grants and the open consent gates — every one to an operator, the caller's own to anyone else; never a secret — each gate with its nonce and argument hash for the person who answers it |
| `delegation.approve` | task | any named caller | an identity provider or a `grant: id_jag` server | Approve one consent gate {gate, nonce, args_hash}, or several of one shape {gates: [...]}: the calls they hold run. Each bound to its gate's nonce and arguments; the person's alone, through a session whose own IdP sign-in is recent enough — or an operator's where the person's template lists one among its approvers |
| `delegation.deny` | task | any named caller | an identity provider or a `grant: id_jag` server | Deny one consent gate {gate, nonce}, or several of one shape {gates: [...]}: the calls they hold are refused, NOT made; a step-up gate's sign-in is cancelled. The person's alone, as delegation.approve says |
| `delegation.revoke` | task | any named caller | an identity provider or a `grant: id_jag` server | Revoke the grants {target?} names — a principal, or idp:<name>#<sub> for every grant of that subject; the caller's own when absent: RFC 7009 at the IdP, then the tombstone, the record and every token. An operator revokes anyone's; anyone else their own alone |
| `delegation.forget` | task | operator | an identity provider or a `grant: id_jag` server | Erase a person {principal}: their grants revoked as delegation.revoke does, then their personal key destroyed — what it sealed in the audit chain, the arguments their consent gates showed, is unreadable from then on, and the chain still verifies |
| `delegation.audit` | task | operator | an identity provider or a `grant: id_jag` server | A person's consent records {principal?} from the audit chain, the latest first, as the task's result — each with what the gate showed opened under their personal key, or `unreadable` once they were forgotten; read off the loop. The per-person operator reads their own; any other operator names a principal |
| `_instance.result` | task | operator | always | reserved: a `mode: sync` child's first result, recorded on the parent's handle; sent by the child over its channel, or by an operator |
| `_instance.emit` | task | operator | always | reserved: one event of a child's `mirror_streams` stream, appended to the parent's stream of the same name; sent by the child over its channel, or by an operator |
| `ask_human` | task | operator | never | reserved: the model's tool for asking a person; not served over A2A |

The two `_instance.*` ops are how a child instance reports to the parent that
started it. A child sends them over the channel its parent handed it at spawn,
which names it `instance:<handle>`: that principal reaches these two ops about
itself and nothing else. Their arguments and results are published under
`$defs/reserved`.

## Workflow-declared commands

A workflow whose start step is `kind: a2a` with a `command:` declares one more
op: sending that command starts the workflow. Its arguments are held to the
start step's `schema`, when it declares one, instead of a row above. The
extended card lists the commands the caller may fire in `params.commands`: those
whose op the caller's grants admit (an operator's always do) and whose start's
`roles:` admit the caller's role. A send meets the same two checks. The schema bundle does not describe these commands. It lets through an op
it does not know, because only the instance knows what its workflows declare.

## Errors

Refusals are JSON-RPC errors whose `data` carries a `google.rpc.ErrorInfo` with
the reason below, and a `google.rpc.BadRequest` naming the field for a malformed
command. Nothing refused is run.

| code | reason | when |
|---|---|---|
| `-32602` | `EXTENSION_NOT_ACTIVATED` | a DataPart under `agentd`, and the `A2A-Extensions` header does not name this URI |
| `-32602` | `EXTENSION_NOT_MARKED` | a command, and `message.extensions` does not list this URI |
| `-32602` | `COMMAND_ENVELOPE_AMBIGUOUS` | more than one part carries a command |
| `-32602` | `COMMAND_TASK_ID` | a command message names a `taskId` |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | `acceptedOutputModes` is given and excludes `application/json` |
| `-32602` | `UNKNOWN_OP` | the op is empty, or nothing serves it: no row, and no loaded workflow declares it |
| `-32602` | `INVALID_COMMAND_ARGS` | the envelope does not match the op's schema; `BadRequest` names each miss |
| `-31403` | `PERMISSION_DENIED` | the caller's role and grants do not reach the op (HTTP 403) |
| `-32004` | `INTROSPECTION_DISABLED` | an introspection op while `a2a.introspection.enabled` is off |

## Security

- **The floor is the table's.** A `user` holding `grants: ["*"]` still reaches
  no `operator` op. An op that drains the instance, relaxes its approval policy
  or reads every principal's log lines is an operator's, whatever a grant says.
- **Ownership is enforced at the object.** An op that names a run, a
  subagent or a conversation acts only on one the caller started. An operator
  acts on any. A caller who may not run a workflow is told it may not, never
  that it does not exist.
- **The public card promises no reach.** It lists the vocabulary, not what
  this instance has switched on or what this caller may run. The extended card
  answers those questions, per caller.
- **The reserved ops are a child's own.** `_instance.*` from anyone but the
  child itself — over its channel, about its own handle — or an operator is
  refused on the request itself, before a task exists.

## Schema and examples

The bundle at [`schema.json`](https://agentd.dev/a2a/ext/command/schema.json)
validates the `data` of a command DataPart at its root:

- `$defs/envelopes/$defs/<op>` holds each op's envelope;
- `$defs/ops/<op>` holds each op's `reply`, `description`, `args` and `result`;
- `$defs/reserved/<op>` holds the same for the `_instance.*` ops.

`agentd --extension-schema command` prints the bundle. It is generated from the
values the listener validates against, and CI fails when the published copy
differs from them. Golden examples, each valid against the bundle, are published
beside it under
[`examples/`](https://github.com/agentd-dev/source-code/tree/main/web/public/a2a/ext/command/examples).

## Related

- [events.md](events.md): the observation feed, which a display client
  bootstraps with the `status` read.
- [task-annotations.md](task-annotations.md): the `command` annotation names the
  op that started a task.
- [../a2a.md](../a2a.md): the listener, its pipeline and its core methods.
