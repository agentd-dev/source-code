# The task-annotations extension

| | |
|---|---|
| URI | `https://agentd.dev/a2a/ext/task-annotations` |
| Schema | [`https://agentd.dev/a2a/ext/task-annotations/schema.json`](https://agentd.dev/a2a/ext/task-annotations/schema.json) (JSON Schema draft 2020-12) |
| Kind | A2A profile extension: data in `Task.metadata`, no change to a core structure |
| Required | never |
| Applies to | every answer that carries a `Task`: `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, and the `task` events of `agentd.events/SubscribeToEvents` |

This page is the normative specification of the URI above. It is served at
that URI.

**The URI carries no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST, and a version in a name agentd owns would be
a second name for the same thing. An incompatible change takes a new URI with a
new name, never a `/vN` suffix: §4.6.3 and §5.8 say a URI's meaning MUST NOT
change under a peer that already speaks it.

agentd knows more about a task than the A2A `Task` has fields for. It knows
what the task tracks, who started it, and when each state change happened. An
extension may not add fields to a core structure, and ad-hoc metadata keys
cannot be looked up. So these facts travel in `Task.metadata` under this URI,
one object, and a peer that never activates the extension never sees them.

## Params

| key | type | card | meaning |
|---|---|---|---|
| `schema` | string | public | the URL of this extension's schema bundle |

Every card declares the extension.

## Activation

A task carries the annotations only in an answer to a request whose
`A2A-Extensions` header names this URI, on a method the extension applies to.
The response echoes the URI when it was activated. Without activation, the
`Task` carries no annotations. Its `metadata` is absent when nothing else is in
it.

## The annotations

```json
{
  "metadata": {
    "https://agentd.dev/a2a/ext/task-annotations": {
      "link": {"kind": "run", "id": "triage-01J9Z8X4K2"},
      "created": "2026-09-29T10:15:02.114Z",
      "statusHistory": [
        {"state": "TASK_STATE_SUBMITTED", "ts": "2026-09-29T10:15:02.114Z"},
        {"state": "TASK_STATE_WORKING", "ts": "2026-09-29T10:15:02.130Z"}
      ],
      "principal": "user:alice",
      "command": "workflow.run"
    }
  }
}
```

| member | always | meaning |
|---|---|---|
| `link` | yes | what the task tracks: `{kind, id}`, `kind` being `run` (a workflow run), `subagent` (a subagent handle) or `turn` (a conversation turn) |
| `created` | yes | when the task was created (RFC 3339) |
| `statusHistory` | yes | every state the task has been in, oldest first, each `{state, ts}` with the A2A `TaskState` name |
| `principal` | no | who started the task |
| `askSchema` | no | the JSON Schema of the answer an `INPUT_REQUIRED` gate asks for, so a client can render a control rather than a text box |
| `command` | no | the [command](command.md) op that started the task |

The object is closed: the bundle refuses a member it does not name, so a fact
added to the wire without the schema fails agentd's own tests.

## Security

- **Only the task's reader sees them.** They ride on answers the caller could
  already read, so they widen nothing. `principal` names the task's owner,
  and a task is visible only to its owner and to operators.
- **Webhooks never carry them.** A push-notification body carries the task
  without its annotations and without its history. A webhook receiver is not a
  party to the conversation.

## Schema

The bundle at
[`schema.json`](https://agentd.dev/a2a/ext/task-annotations/schema.json)
validates the object under `metadata[<URI>]` at its root.
`agentd --extension-schema task-annotations` prints it, and CI fails when the
published copy differs. Golden examples are published beside it under
[`examples/`](https://github.com/agentd-dev/source-code/tree/main/web/public/a2a/ext/task-annotations/examples).
