# The command extension

| | |
|---|---|
| URI | `https://agentd.dev/a2a/ext/command` |
| Schema | [`https://agentd.dev/a2a/ext/command/schema.json`](https://agentd.dev/a2a/ext/command/schema.json) (JSON Schema draft 2020-12) |
| Kind | A2A profile extension: a structure on the messages it marks (one command DataPart); no new method, no change to a core structure |
| Required | never |
| Applies to | `SendMessage`, `SendStreamingMessage` |

This page is the normative specification of the URI above. It is served at
that URI.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST, and a version in a name agentd owns would be
a second name for the same thing. An incompatible change takes a new URI with a
new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's meaning MUST NOT
change under a peer that already speaks it.

This extension carries agentd's **workload**: the ops that start work or act
on work a task tracks — starting a workflow, signalling one, talking to and
stopping a subagent, and a child instance's reports home. Each is an ordinary
`SendMessage` whose message carries one `DataPart` under the `agentd` key, so
a peer agent and a stock A2A client reach it with the one method every agent
answers, and the work it starts is a task like any other. A client that never
activates this extension can still converse, list tasks and subscribe to them.
The extension adds reach, and is never a precondition.

The operator surface — the control plane (drain, pause, sign-ins, sessions,
Companies, delegation) and the reads (`status`, `run.get`, …) — is not here.
Those are calls on the instance, not messages to the agent, so they are
JSON-RPC methods of their own, `agentd.operator/<op>`, under the
[operator extension](operator.md). An operator op sent as a command is
refused as an unknown op. Cancelling a run's task is the core `CancelTask`;
reading it, `GetTask` and `ListTasks`.

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

Every op is work, and answers with a `Task` (its `reply` column below, and
`params.ops[].reply` on the card, is `task`). When it completes with a result,
the task carries one artifact, `<taskId>.result`, whose `DataPart` holds the
result and whose `extensions` lists this URI. An op whose result schema is
`false` completes with no result artifact. A read is no command: the reads are
the [operator extension](operator.md)'s methods, which answer with their
document and create no task.

Each op's result schema is `$defs/ops/<op>/result` in the bundle: the data of
the result artifact.

## Ops

Every op this extension serves, in the order the card lists them, followed by
the reserved names. The reserved names are never listed in `params.ops`. No
workflow may declare any of them as a command — nor an
[operator op](operator.md#methods)'s name.

- **who may call** is the op's floor. `operator` means the operator role alone,
  and no grant reaches it. `any named caller` means every authenticated
  principal. The remaining rows are open to the roles named and to any principal
  whose `grants` name the op.
- **served** is the switch that serves the op.

| op | reply | who may call | served | description |
|---|---|---|---|---|
| `workflow.run` | task | `user`, `agent` or a grant | always | Start a workflow; the reply is the task it runs under |
| `workflow.signal` | task | a grant | always | Deliver a signal a workflow is waiting on |
| `subagent.send` | task | `user` or a grant | always | Send a message to a warm subagent |
| `subagent.kill` | task | a grant | always | Stop a subagent |
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
| `-32602` | `UNKNOWN_OP` | the op is empty, or nothing serves it as a command: no row of this extension (an operator op included), and no loaded workflow declares it |
| `-32602` | `INVALID_COMMAND_ARGS` | the envelope does not match the op's schema; `BadRequest` names each miss |
| `-31403` | `PERMISSION_DENIED` | the caller's role and grants do not reach the op (HTTP 403) |
| `-32603` | `DRAINING` | the agent is draining and takes no new work |

## Security

- **The floor is the table's.** A `user` holding `grants: ["*"]` still reaches
  no `operator` op.
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

- [operator.md](operator.md): the control plane and the reads, as methods.
- [events.md](events.md): the observation feed, which a display client
  bootstraps with the `agentd.operator/status` read.
- [task-annotations.md](task-annotations.md): the `command` annotation names the
  op that started a task.
- [../a2a.md](../a2a.md): the listener, its pipeline and its core methods.
