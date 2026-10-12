# A2A extensions

A2A 1.0 defines eleven JSON-RPC methods and a message model. Anything an agent
speaks beyond them goes through the protocol's own mechanism,
[extensions](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md):
a URI, declared on the Agent Card, that a client activates per request. This
page lists the three extensions and one protocol binding agentd declares, the
activation rules, what the extended card adds, and the errors the extensions
introduce. It is for integrators building a client or a peer; each URI is
also the address of its normative specification.

## What agentd declares

| URI | Kind | Declared | Applies to | Adds |
|---|---|---|---|---|
| [`https://agentd.dev/a2a/ext/command`](https://agentd.dev/a2a/ext/command) | profile | always | `SendMessage`, `SendStreamingMessage` | command ops: structured operations sent as one DataPart |
| [`https://agentd.dev/a2a/ext/events`](https://agentd.dev/a2a/ext/events) | method | while `a2a.events.enabled` is on | `agentd.events/SubscribeToEvents` | the instance-wide observation feed |
| [`https://agentd.dev/a2a/ext/task-annotations`](https://agentd.dev/a2a/ext/task-annotations) | profile | always | every answer that carries a Task | agentd's facts about a task, in `Task.metadata` |

One custom protocol binding is named by an interface on the card rather than
activated:

| URI | Kind | Used |
|---|---|---|
| [`https://agentd.dev/a2a/binding/jsonrpc-unix`](https://agentd.dev/a2a/binding/jsonrpc-unix) | protocol binding | the JSON-RPC binding over a unix domain socket, on the card of a listener at `unix:///<path>` |

A2A groups extensions into four kinds: data-only (information on the card),
profile (structure or narrower values on core messages), method (new RPC
methods) and state machine (new task states). agentd's are profile and method
extensions. None changes a core structure: custom data rides in a DataPart or
under a URI in a `metadata` map, which is what those fields exist for.

**None is required.** A client that sends no `A2A-Extensions` header gets the
whole core protocol: it converses, reads, lists and cancels its tasks,
subscribes to them and, where push is enabled, registers webhooks. The
extensions add reach and are never a precondition. `-32008` (a required
extension not activated) is part of the negotiation but agentd never sends it.

## Identifiers

Each URI is both an identifier and an address.

- **Matched exactly.** A peer compares the URI byte for byte: neither case nor
  a trailing slash is forgiven.
- **Served at its address.** agentd.dev serves the specification at the URI,
  the JSON Schema bundle at `<uri>/schema.json` and golden examples beside it.
  The binding carries the JSON-RPC binding's own messages and has no schema
  bundle.
- **Never versioned.** A URI's meaning never changes under a peer that already
  speaks it. An incompatible change takes a new URI with a new name, never a
  version suffix. The same holds for the binding and the launch grant type.

| Schema bundle | Validates |
|---|---|
| [`https://agentd.dev/a2a/ext/command/schema.json`](https://agentd.dev/a2a/ext/command/schema.json) | the `data` of a command DataPart, each op's arguments and result |
| [`https://agentd.dev/a2a/ext/events/schema.json`](https://agentd.dev/a2a/ext/events/schema.json) | the method's params, each frame, each event kind's `data` |
| [`https://agentd.dev/a2a/ext/task-annotations/schema.json`](https://agentd.dev/a2a/ext/task-annotations/schema.json) | the object under `metadata[<URI>]` |

The binary prints the same registry and bundles, with no configuration and
no network:

```sh
agentd --extensions
agentd --extension-schema command
```

`--extension-schema` takes the last segment of a URI: `command`, `events` or
`task-annotations`.

To see what a running instance declares, read its card:

```sh
curl -s http://127.0.0.1:8420/.well-known/agent-card.json | jq -r '.capabilities.extensions[].uri'
```

```text
https://agentd.dev/a2a/ext/command
https://agentd.dev/a2a/ext/events
https://agentd.dev/a2a/ext/task-annotations
```

## Activation and the echo

A client names the extensions it uses in the `A2A-Extensions` request header,
comma-separated. The header may repeat, and every line is read. An extension
is activated for a request when it is all three of:

- **requested:** named in the header, matched exactly;
- **declared:** on this instance's card;
- **applicable:** meaningful on the method called. The command extension
  applies to the two sends, the events extension to its own method, and
  task-annotations to every answer that carries a Task (the sends, `GetTask`,
  `ListTasks`, `CancelTask`, `SubscribeToTask` and the feed's `task` events).

The response echoes, in the same header, exactly the extensions the request
activated. Three consequences follow:

- **An unknown URI is dropped, not refused.** A client that talks to many
  agents can send the same header to all of them.
- **Nothing activated means no header.** The absence of the echo is the
  answer.
- **The echo never claims what the answer did not do.** An extension named
  on a method it does not apply to is not echoed, and a refused request
  echoes nothing.

**A command is activated twice.** A DataPart under the `agentd` key is a
command only when the header names the command URI *and* the message lists it
in `message.extensions`. Without both it is refused and never run, so no
client sends a command by accident.

**A method is part of its extension.** `agentd.events/SubscribeToEvents` is
answered only when the instance declares the events extension and the request
activates it.

This request sends the `status` command and names one URI agentd does not
implement:

```sh
curl -si http://127.0.0.1:8420/ \
  -H 'content-type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command, https://example.com/not-ours' \
  -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"role":"ROLE_USER","messageId":"m-1","extensions":["https://agentd.dev/a2a/ext/command"],"parts":[{"data":{"agentd":{"op":"status"}}}]}}}'
```

```text
HTTP/1.1 200 OK
content-type: application/json
a2a-extensions: https://agentd.dev/a2a/ext/command

{"id":1,"jsonrpc":"2.0","result":{"message":{"extensions":["https://agentd.dev/a2a/ext/command"],"parts":[{"data":{"instance":"reviewer","draining":false,…}}],…}}}
```

That example runs against a loopback listener with no credential configured,
where a local caller is the operator. Anywhere else, add
`-H "Authorization: Bearer $TOKEN"`.

> **Note.** A browser can read the echo. A response to a listed origin
> exposes `a2a-extensions`, `retry-after` and `www-authenticate`, and the
> preflight allows the `A2A-Extensions` request header.

## The extended card

The public card is the same for every caller. It declares each extension with
the params that do not depend on who asks: the command extension's public
`ops` list is the vocabulary agentd can answer, identical on every instance,
whatever it has switched on.

`GetExtendedAgentCard` narrows each declaration to this instance and this
caller:

| Extension | The extended card adds |
|---|---|
| command | `ops`: the ops this instance serves and the caller may run. `commands`: the workflow-declared commands the caller may send, each with its workflow and argument schema. `settable`: the paths `admin.set` accepts, for a caller that may run it |
| events | `ring`: how many events the feed keeps for a reconnecting subscriber. `kinds`: the event kinds this caller's feed can carry |

It also adds a `workflow:<name>` skill for each workflow the caller may run.

The extended card is an authenticated read, so it exists only where
authentication means something:

- A listener that **declares a scheme** (a bearer, the device grant or mutual
  TLS) sets `capabilities.extendedAgentCard: true`. It answers the method
  only to a caller named by a declared scheme: a bearer, a session or a
  certificate. Anyone else gets `401`.
- A listener that **declares none** (a loopback listener with nothing
  configured, or a unix socket) sets it `false` and answers `-32004`.

For example, a peer whose principal has role `agent` and a grant for one
workflow-declared command sees this command declaration:

```json
{
  "uri": "https://agentd.dev/a2a/ext/command",
  "params": {
    "dataPartKey": "agentd",
    "schema": "https://agentd.dev/a2a/ext/command/schema.json",
    "ops": [
      {"op": "status", "reply": "message"},
      {"op": "workflow.run", "reply": "task"},
      {"op": "workflow.status", "reply": "message"}
    ],
    "commands": [
      {
        "op": "review.change",
        "workflow": "review",
        "schema": {
          "type": "object",
          "required": ["change"],
          "properties": {"change": {"type": "string"}}
        }
      }
    ]
  }
}
```

## The command extension

A2A has no tool-call primitive, and a method per operation would put agentd's
surface out of reach of a stock client. So every operation is an ordinary
`SendMessage` whose single DataPart holds the command:
`{"agentd": {"op": "…", …arguments}}`. The arguments are validated against
the op's published schema, and an argument the op does not take is refused,
never ignored.

An op answers in one of two ways, published per op as `params.ops[].reply`:

- **A read** (`message`) answers with a Message whose DataPart is the result
  document. No task is created, so a client that polls leaves nothing behind.
- **Work** (`task`) answers with a Task. A result it produces arrives as the
  task's `<taskId>.result` artifact.

| Family | Ops | Served |
|---|---|---|
| Status and configuration | `status`, `config` | always |
| Workflows | `workflow.run`, `workflow.status`, `workflow.cancel`, `workflow.signal` | always |
| Subagents and plans | `subagent.send`, `subagent.kill`, `subagent.status`, `plan.get` | always |
| Introspection | `conversation.get`, `run.get`, `subagent.get`, `debug.events` | while `a2a.introspection.enabled` is on |
| Operator controls | `admin.drain`, `admin.pause`, `admin.resume`, `admin.cancel`, `admin.set` | always |
| Sign-in | `auth.device.pending`, `auth.device.approve`, `auth.device.deny` | while `a2a.device_grant.enabled` is on |
| Sessions | `auth.sessions`, `auth.sessions.revoke` | on a TCP listener |

Who may call each op, its arguments and its result are in the
[command ops table](a2a-reference.md#command-ops). The `admin.*` and `auth.*`
ops, `config` and `debug.events` are operator-only, and no grant reaches
them.

**Workflow-declared commands.** A workflow whose start step is `kind: a2a`
with a `command:` declares one more op: sending that command starts the
workflow, and its arguments are held to the start step's `schema`. A caller
may send it when its grants name the op (an operator always may) and the
start's `roles:` admit its role. The built-in op names are reserved: a
workflow that declares one is refused at validation, so a workflow can never
shadow an operator control.

The normative specification, with the envelope, every op and the reserved
names, is at
[`https://agentd.dev/a2a/ext/command`](https://agentd.dev/a2a/ext/command).

## The events extension

A2A streams one task at a time. A display client needs the whole instance, so
the events extension declares one method, `agentd.events/SubscribeToEvents`,
namespaced under the extension so a method the specification defines later
can never collide with it.

```sh
curl -sN http://127.0.0.1:8420/ \
  -H 'content-type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'A2A-Extensions: https://agentd.dev/a2a/ext/events' \
  -d '{"jsonrpc":"2.0","id":7,"method":"agentd.events/SubscribeToEvents","params":{"fromSeq":0}}'
```

```text
data: {"id":7,"jsonrpc":"2.0","result":{"hello":{"introspection":false,"resume":0,"resync":false,"seq":7,"version":"1.19.1"}}}

data: {"id":7,"jsonrpc":"2.0","result":{"event":{"kind":"step","seq":1,"ts":…,"data":{…}}}}
```

The answer is a server-sent event stream of three frame types:

| Frame | When | Members |
|---|---|---|
| `hello` | first | `seq` (newest in the feed), `resume` (the cursor asked for), `resync`, `introspection`, `version` (the agentd build) |
| `event` | replayed past the cursor, then live | `seq`, `ts` (epoch milliseconds), `kind`, `data` |
| `goodbye` | last | `seq` (the cursor to resume from), `reason`: `deadline` or `revoked` |

- **Reconnecting** resumes from the last `seq` read, passed as `fromSeq`.
- **`resync: true`** means the cursor cannot be honoured: events past it
  left the ring, or the daemon restarted, since the feed lives in memory.
  The client re-reads its state (the `status` command and `ListTasks`) rather
  than applying the replay on top.
- **Visibility is per event.** An operator receives every event; any other
  caller receives the events its principal owns and those addressed to every
  subscriber, so the `seq` values it reads have gaps.
- **The stream ends** with `goodbye` when the listener's stream deadline
  passes (`deadline`), or within one tick when the caller's session is
  revoked (`revoked`), with nothing past the revocation.

The kinds are a closed vocabulary with a schema each: `task`, `run`, `step`,
`conversation`, `subagent`, `child`, `activity` and their `.removed`
departures, `status`, `lifecycle`, `config`, `audit` (while introspection is
on) and `auth` (device sign-ins, revocations, launches). Who receives each
kind is in the specification at
[`https://agentd.dev/a2a/ext/events`](https://agentd.dev/a2a/ext/events).

## The task-annotations extension

agentd knows more about a task than the Task has fields for. An extension may
not add fields to a core structure, so these facts travel as one object in
`Task.metadata` under the extension's URI, only in an answer to a request
that activated it:

| Member | Always | Meaning |
|---|---|---|
| `link` | yes | what the task tracks: `{kind, id}`, `kind` being `run`, `subagent` or `turn` |
| `created` | yes | when the task was created |
| `statusHistory` | yes | every state the task has been in, oldest first, each `{state, ts}` |
| `principal` | no | who started the task |
| `askSchema` | no | the JSON Schema of the answer an `input-required` gate asks for |
| `askChannel` | no | where the gate is also announced, from the instruction document's `::!human` |
| `command` | no | the command op that started the task |

A webhook body never carries the annotations. The normative specification is
at
[`https://agentd.dev/a2a/ext/task-annotations`](https://agentd.dev/a2a/ext/task-annotations).

## The unix socket binding

Two instances on one host can skip TCP and TLS: one listens on a socket
(`a2a.listen: unix:///run/agentd/a.sock`), the other names that socket as a
peer endpoint. The protocol is the JSON-RPC binding unchanged, carried as
HTTP/1.1 over the socket: the same methods, headers, errors and streams. No
`https://` URL can name a socket path, and a stock client picks an interface
by its binding before it sends anything, so the card's interface declares the
binding URI instead of `JSONRPC`. A client that cannot dial a socket never
selects it.

The kernel is the authenticator. The socket is mode `0600`, only the daemon's
own uid or root is served, and every connection that passes is the operator.
A unix listener declares no authentication scheme, so it has no extended
card, and it serves no session ops. The normative specification is at
[`https://agentd.dev/a2a/binding/jsonrpc-unix`](https://agentd.dev/a2a/binding/jsonrpc-unix).

## Errors the extensions add

These refusals sit beside the core error table in the
[A2A API reference](a2a-reference.md#errors). Each carries a
`google.rpc.ErrorInfo` in `error.data` whose `reason` names the case, under
`domain` `agentd.dev`, except `CONTENT_TYPE_NOT_SUPPORTED`, which is the
specification's own reason and carries `a2a-protocol.org`. A malformed
command also carries a `google.rpc.BadRequest` naming each field. Nothing
refused is run, and no task is created.

| Code | Reason | When |
|---|---|---|
| `-32601` | `EXTENSION_NOT_DECLARED` | `agentd.events/SubscribeToEvents` on an instance that does not declare the events extension |
| `-32601` | `EXTENSION_NOT_ACTIVATED` | that method, and `A2A-Extensions` does not name the events URI |
| `-32602` | `EXTENSION_NOT_ACTIVATED` | a DataPart under `agentd`, and the header does not name the command URI |
| `-32602` | `EXTENSION_NOT_MARKED` | a command, and `message.extensions` does not list the command URI |
| `-32602` | `COMMAND_ENVELOPE_AMBIGUOUS` | more than one part carries a command |
| `-32602` | `COMMAND_TASK_ID` | a command message names a `taskId` |
| `-32602` | `UNKNOWN_OP` | no op of that name is served here |
| `-32602` | `INVALID_COMMAND_ARGS` | the arguments do not match the op's schema |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | `acceptedOutputModes` excludes `application/json`, which every command answers with |
| `-32004` | `INTROSPECTION_DISABLED` | an introspection op while `a2a.introspection.enabled` is off |
| `-31403` | `PERMISSION_DENIED` | the caller's role and grants do not reach the op (HTTP 403) |

## Clients

The display clients, `agentd-tui` and `agentd-ui` in the npm package
`@agentd-dev/cli`, are ordinary A2A clients. They read the card, send
`A2A-Version` on every call and activate only extensions the card declares.
Against an agent that declares none of agentd's, they fall back to the core
methods alone. [Use the terminal and web UI](interface.md) covers them, and
[Build a client](build-a-client.md) covers writing your own.

agentd's own outbound client, which the `a2a.send` and `a2a.delegate`
workflow steps use, activates the command extension when it sends a command,
and dials a peer's unix socket through the binding.

## How conformance is checked

Each claim on this page is held by a check that fails when it stops being
true:

- **The method table is the SDK's.** agentd's eleven core method names are
  held to the constants of the A2A project's official SDK, which the listener
  is built on, and the route table serves those and the declared extension
  methods, nothing else.
- **Negotiation is an intersection.** Every method is tested against every
  subset of the registry, with the feed on and off, plus a URI agentd does not
  implement.
- **What agentd.dev publishes is what the binary speaks.** The registry,
  schema bundles and golden examples are generated from the values the
  listener validates against, and CI fails on a difference. A running daemon's
  frames, replies and annotations are validated against the published files,
  and the golden examples against a validator that is not agentd's.
- **The feed pushes only declared kinds.** Every push site is scanned, and
  debug builds check every push against its kind's schema.
- **Someone else's reading of the specification.** In CI, the official Python
  SDK runs as a client of a real agentd and parses every answer strictly into
  the protocol's own types. The TypeScript client runs against the SDK's
  sample server, and agentd's outbound client calls it too. The conformance
  suite, `agentd-conformance`, checks the extensions from the outside, the
  way a specification-only peer would.
- **The pages name only what is served.** Every JSON-RPC method and every
  agentd URI the docs name is held to the tables the listener is built from.

## Next steps

- [A2A API reference](a2a-reference.md): the core methods, the pipeline,
  errors and the full command ops table.
- [The A2A channel](a2a.md): how identity, ownership and delegation work.
- [Build a client](build-a-client.md): a client that uses the feed and the
  commands.
- [Use the terminal and web UI](interface.md): the shipped clients.
