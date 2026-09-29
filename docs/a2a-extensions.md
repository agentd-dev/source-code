# A2A extensions — everything agentd speaks beyond the core protocol

A2A 1.0 defines eleven JSON-RPC methods and a message model. Anything an agent
speaks beyond them goes through the specification's own mechanism,
[extensions](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md):
a URI, declared on the agent card, that a client activates per request.

agentd answers the eleven core methods, the one method its events extension
declares, and nothing else. Its public card is a document, not a method: a
`GET` on `/.well-known/agent-card.json`, served without a credential. That
is what makes an agentd instance callable by a peer that has never heard of
agentd — and this page is the overview of what agentd adds on top: three
extensions and one protocol binding. Each URI is its own normative
specification, served at that address; the pages in [`ext/`](ext/) are their
sources. The core protocol itself — the pipeline, identity, sign-in, tasks — is
[a2a.md](a2a.md).

---

## 1. What agentd declares

| URI | Kind | Declared | Adds |
|---|---|---|---|
| [`https://agentd.dev/a2a/ext/command`](https://agentd.dev/a2a/ext/command) | profile | always | the **command ops**: structured operations sent as one DataPart on `SendMessage` / `SendStreamingMessage` |
| [`https://agentd.dev/a2a/ext/events`](https://agentd.dev/a2a/ext/events) | method | while `a2a.events.enabled` | `agentd.events/SubscribeToEvents`, the instance-wide observation feed |
| [`https://agentd.dev/a2a/ext/task-annotations`](https://agentd.dev/a2a/ext/task-annotations) | profile | always | agentd's facts about a task, in `Task.metadata` under the URI |

and one custom protocol binding, named by an interface on the card rather than
activated:

| URI | Kind | Used |
|---|---|---|
| [`https://agentd.dev/a2a/binding/jsonrpc-unix`](https://agentd.dev/a2a/binding/jsonrpc-unix) | protocol binding | the JSON-RPC binding carried over a unix domain socket, on the card of a listener at `unix:///<path>` |

The specification groups extensions into four kinds: **data-only**
(information on the card), **profile** (structure or narrower values on core
messages), **method** (new RPC methods) and **state machine** (new task
states). agentd's are profile and method extensions. None changes a core
structure: custom data rides in a `DataPart` or under a URI in a `metadata`
map, which is what those fields exist for.

**None is `required`.** A client that sends no `A2A-Extensions` header at all
gets the whole core protocol: it can converse, read, list and cancel tasks,
subscribe to them and, where `a2a.push.enabled` is on, register webhooks. The
extensions add reach, never a precondition.

**The URIs carry no version.** A2A 1.0.1 §4.6.3 makes a version in an
extension URI a SHOULD, not a MUST; what §4.6.3 and §5.8 make a MUST is that a
URI's meaning never changes under a peer that already speaks it. A version in
an identifier agentd owns would be a second name for the same thing, so an
incompatible change takes a new URI with a new name, never a `/vN` suffix. The
same holds for the binding and for the launch grant type.

Each URI is both an identifier and an address. A peer matches it exactly —
neither case nor a trailing slash is forgiven — and can fetch it: agentd.dev
serves the specification at the URI, the schema bundle at `<uri>/schema.json`
and golden examples under `<uri>/examples/`. `agentd --extension-schema
<name>` prints a bundle from the binary, and `agentd --extensions` the
registry.

See what an instance declares:

```console
$ curl -s http://127.0.0.1:8080/.well-known/agent-card.json \
  | jq -r '.capabilities.extensions[].uri'
https://agentd.dev/a2a/ext/command
https://agentd.dev/a2a/ext/events
https://agentd.dev/a2a/ext/task-annotations
```

---

## 2. Activation and the echo

A client names the extensions it uses in the **`A2A-Extensions`** request
header, comma-separated (the header may repeat; every line is read). The
response echoes, in the same header, exactly the extensions that request
activated. An extension is activated when it is:

- **requested** — named in the header, matched exactly;
- **declared** — on this instance's card; and
- **applicable** — meaningful on the method called: the command extension on
  the two sends, the events extension on its own method, the task-annotations
  extension on every answer that carries a `Task` (the sends, `GetTask`,
  `ListTasks`, `CancelTask`, `SubscribeToTask` and the feed's `task` events).

Three consequences to rely on:

- **An unknown URI is dropped, not refused.** Send three, get back the ones
  that exist here. A client that talks to several agents does not have to
  branch per agent before it can make a request.
- **Nothing activated means no header.** The absence of the echo is the
  answer.
- **The echo never claims what the answer did not do.** An extension named on
  a method it does not apply to is not echoed.

```console
$ curl -si http://127.0.0.1:8080/ \
    -H 'content-type: application/json' \
    -H 'A2A-Version: 1.0' \
    -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command, https://example.com/not-ours' \
    -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{
          "role":"ROLE_USER","messageId":"m-1",
          "extensions":["https://agentd.dev/a2a/ext/command"],
          "parts":[{"data":{"agentd":{"op":"status"}}}]}}}'
HTTP/1.1 200 OK
content-type: application/json
a2a-extensions: https://agentd.dev/a2a/ext/command

{"id":1,"jsonrpc":"2.0","result":{"message":{…,"parts":[{"data":{"instance":"demo",…}}]}}}
```

That example runs against a loopback listener with no credential configured,
where the caller is the implicit operator; anywhere else, add
`-H "Authorization: Bearer $TOKEN"`. A browser can read the echo: a real
response to a listed origin carries `Access-Control-Expose-Headers:
a2a-extensions, retry-after, www-authenticate`, and the preflight allows the
request header.

**A command is activated twice.** A DataPart under `agentd` is a command only
when the header names the command URI *and* the message lists it in
`message.extensions`; without both it is refused and never run, so no client
sends a command by accident. **A method is part of its extension.**
`agentd.events/SubscribeToEvents` is answered only when the instance declares
the events extension and the request activates it.

---

## 3. The three extensions

### command — calling an operation

A2A has no tool-call primitive, and a method per operation would put agentd's
surface out of reach of a stock client. The protocol's own answer is a message
carrying structured input, so every operation is an ordinary `SendMessage`
whose one DataPart holds `{"agentd": {"op": "…", …arguments}}`. The arguments
are validated against the op's published schema; one the op does not take is
refused, never ignored.

An op answers in one of two ways, published per op as `params.ops[].reply`:

- a **read** (`status`, `config`, `workflow.status`, `plan.get`,
  `auth.sessions`, …) answers with a **Message** whose DataPart is the result
  document. No task is created, so a client that polls leaves nothing behind;
- **work** (`workflow.run`, `admin.drain`, `auth.device.approve`, …) answers
  with a **Task**, and a result it produces arrives as the task's
  `<taskId>.result` artifact.

The families: `status` and `config`; `workflow.*`, `subagent.*` and
`plan.get`; the introspection reads (`conversation.get`, `run.get`,
`subagent.get`, `debug.events`), served while `a2a.introspection.enabled` is
on; `admin.*` (drain, pause, resume, cancel, and `admin.set` for the
runtime-settable paths); and the sign-in ops:

| Op | Reply | Does |
|---|---|---|
| `auth.device.pending` | message | the device sign-ins waiting for a decision |
| `auth.device.approve` | task | approve one: `{user_code, as, scope?}`. **`as` is required** — the name the device signs in as; every session approved as one name is one principal, `user:<name>`, so what it owns survives a re-login |
| `auth.device.deny` | task | refuse one `{user_code}`, or every pending one `{all: true}` |
| `auth.sessions` | message | the signed-in sessions: kind, name, principal, expiry |
| `auth.sessions.revoke` | task | end one session `{sid}`, every session of a name `{name}`, or all `{all: true}` |

The `auth.device.*` ops are served while `a2a.device_grant.enabled` is on,
`auth.sessions*` on any TCP listener; all five are operator-only. The full op
table — who may call each, what serves it, its arguments and result — is the
[command extension's specification](ext/command.md#ops).

**The floor is the role's, not a grant's.** `admin.*`, `auth.*`, `config` and
`debug.events` are operator-only, and no `grants:` entry reaches them — not
even `grants: ["*"]`. A peer that could drain the instance it delegates to
would be an operator, and a peer is not one. A rule whose role is not
`operator` may not name one of them in `grants:` at all: a grant that can
never apply is a load error, not a control that silently grants nothing.

**The built-in ops are reserved names.** A workflow's `kind: a2a` start step
may declare a `command:`, which becomes one more op. Its name may not collide
with a built-in: a declared command takes the workflow path, where the
built-in's own authorization would not run, so a workflow claiming
`admin.drain` would shadow the operator's control. The collision is refused
at validation — at config load and at `workflow.create` alike — and the
listener dispatches a built-in to its own handler regardless.

### events — the observation feed

A2A streams one task at a time (`SubscribeToTask`). A display client needs the
whole instance, so the events extension declares one method,
`agentd.events/SubscribeToEvents`, namespaced under the extension so a method
the specification defines later can never collide with it.

```console
$ curl -sN http://127.0.0.1:8080/ \
    -H 'content-type: application/json' \
    -H 'A2A-Version: 1.0' \
    -H 'A2A-Extensions: https://agentd.dev/a2a/ext/events' \
    -d '{"jsonrpc":"2.0","id":7,"method":"agentd.events/SubscribeToEvents","params":{"fromSeq":0}}'
data: {"id":7,"jsonrpc":"2.0","result":{"hello":{"introspection":false,"resume":0,"resync":false,"seq":7,"version":"…"}}}

data: {"id":7,"jsonrpc":"2.0","result":{"event":{"kind":"step","seq":1,"ts":…,"data":{…}}}}
```

The stream opens with a `hello` frame, replays the events past `fromSeq`, then
follows live `event` frames, each `{seq, ts, kind, data}`; a reconnect resumes
from the last `seq` it read. The kinds are a closed vocabulary with a schema
each — including **`auth`**: a device sign-in pending, approved or denied, a
session revoked, a launch — and a subscriber is sent only the events its
principal may see. The stream ends with a **`goodbye`** frame carrying the
cursor to resume from and its reason: `deadline` when the listener's stream
deadline elapsed, **`revoked`** when the caller's session was revoked, within
one tick and with nothing past the revocation. The frames, the kinds and who
receives each are the [events extension's specification](ext/events.md).

### task-annotations — what agentd knows about a task

agentd knows more about a task than the `Task` has fields for: what it tracks
(a workflow run, a subagent, a conversation turn), who started it, when each
state change happened, the answer schema of a gate waiting on a person, the
command that started it. An extension may not add fields to a core structure,
so these facts travel as one object in `Task.metadata` under the extension's
URI — only in an answer to a request that activated it. A webhook body never
carries them. The members are the
[task-annotations specification](ext/task-annotations.md).

---

## 4. The unix binding

Two instances on one host, or in one pod, can skip TCP and TLS: one listens on
a socket (`a2a.listen: unix:///run/agentd/a.sock`), the other names that
socket as a peer endpoint. The protocol is the JSON-RPC binding unchanged,
carried as HTTP/1.1 over the socket — but no `https://` URL can name a socket
path, and a stock client picks an interface by its binding before it sends
anything. So the card's interface declares the binding URI above instead of
`JSONRPC`, and a client that cannot dial a socket never selects it.

The kernel is the authenticator: the socket is `0600`, the peer's uid is read
with `SO_PEERCRED`, only the daemon's own uid or root is served, and every
connection that passes is the operator. A unix listener declares no
authentication scheme, so it has no extended card. The details are the
[binding's specification](ext/binding-jsonrpc-unix.md).

---

## 5. The extended card

The public card is the same for every caller: it declares each extension with
the params that do not depend on who asks. The command extension's public
`params.ops` is the **vocabulary agentd can answer** — identical on every
instance, whatever it has switched on — so it says nothing about whether
introspection is on or who may drain.

`GetExtendedAgentCard` narrows each declaration to this instance and **this
caller**:

| Extension | The extended card adds |
|---|---|
| command | `ops`: the ops this instance serves and the caller may run; `commands`: the workflow-declared commands the caller may fire, each with its workflow and argument schema; `settable`: the paths `admin.set` accepts, for a caller that may run it |
| events | `ring`: how far behind a reconnecting subscriber may be; `kinds`: the kinds this caller's feed can carry |

and a `workflow:<name>` skill for each workflow the caller may run.

It is an authenticated read, so it exists only where authentication means
something. A listener that declares a scheme (a bearer, the device grant, mTLS)
sets `capabilities.extendedAgentCard: true` and answers the method only to a
caller named by a declared scheme — a bearer, a session or a certificate;
anyone else gets `401`. A listener that declares none (a no-auth loopback, a
unix socket) sets it `false` and answers `-32004`.

---

## 6. Errors

The refusals the extensions add, beside the core table in
[a2a.md](a2a.md#errors). Each carries a `google.rpc.ErrorInfo` in
`error.data` whose `reason` names the case — under `domain` `agentd.dev`,
except `CONTENT_TYPE_NOT_SUPPORTED`, which is the specification's own reason
and carries `a2a-protocol.org` — and a malformed command also a
`google.rpc.BadRequest` naming the field. Nothing
refused is run, and no task is created.

| Code | Reason | When |
|---|---|---|
| `-32601` | `EXTENSION_NOT_DECLARED` | `agentd.events/SubscribeToEvents` on an instance that does not declare the events extension |
| `-32601` | `EXTENSION_NOT_ACTIVATED` | the method, and the request's `A2A-Extensions` does not name the events URI |
| `-32602` | `EXTENSION_NOT_ACTIVATED` | a DataPart under `agentd`, and the header does not name the command URI |
| `-32602` | `EXTENSION_NOT_MARKED` | a command, and `message.extensions` does not list the command URI |
| `-32602` | `COMMAND_ENVELOPE_AMBIGUOUS` | more than one part carries a command |
| `-32602` | `COMMAND_TASK_ID` | a command message names a `taskId` |
| `-32602` | `UNKNOWN_OP` | no op of that name is served here |
| `-32602` | `INVALID_COMMAND_ARGS` | the arguments do not match the op's schema |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | `acceptedOutputModes` excludes `application/json`, which every command answers with |
| `-32004` | `INTROSPECTION_DISABLED` | an introspection op while `a2a.introspection.enabled` is off |
| `-31403` | `PERMISSION_DENIED` | the caller's role and grants do not reach the op (HTTP 403) |

`-32008` — a required extension not activated — is part of the negotiation
but never sent, because none of agentd's extensions is required.

---

## 7. Clients

The display clients — `agentd-tui` and `agentd-ui`, both in the npm package
`@agentd-dev/cli` — are ordinary A2A clients: they discover the card, send
`A2A-Version` on every call, and activate only extensions the card declares.
Against an instance that declares none of agentd's, or a peer that is not
agentd, they fall back to the core methods alone. How they sign in, and what
they render from the feed, is [interface.md](interface.md).

agentd's own outbound client, the one the `a2a.send` and `a2a.delegate`
workflow nodes use, activates the command extension when it sends a command
and dials a peer's unix socket through the binding.

---

## 8. How this is kept honest

A compliance claim nobody checks decays, so each one here is a test that fails
when it stops being true:

- **The method table is the SDK's.** `spec_methods_are_exactly_the_sdks`
  (`runtime/surface/methods.rs`) holds the eleven core names to the constants
  of a2a-rs, the library the listener is built on, in both directions; and
  `route_table_is_exact` routes those and the declared extension methods, and
  nothing else. a2a-rs is the same library the listener uses, so this checks
  agentd against its own dependency, not against an independent reading.
- **Negotiation is an intersection.**
  `negotiation_intersects_requested_declared_and_applicable`
  (`runtime/surface/ext.rs`) runs every method against every subset of the
  registry, feed on and off, plus a URI agentd does not implement.
- **The wire does what the table says.** `a2a_extensions_e2e.rs` against a
  running daemon: the echo is exactly what was activated; the feed needs its
  extension declared and activated; annotations follow activation; a command
  without activation is refused and not run; an argument the op does not take
  is refused.
- **What agentd.dev publishes is what the binary speaks.** The registry,
  schema bundles and golden examples are generated from the values the
  listener validates against (`scripts/gen-schemas.sh`), and CI fails on a
  difference. `contract_e2e.rs` validates a running daemon's frames, replies,
  results and annotations against the *committed* files a peer downloads, and
  the client's `test/contract.test.mjs` validates the golden examples with a
  validator that is not agentd's (ajv).
- **The feed pushes only declared kinds.** `feed_kinds_guard.rs` scans every
  push site, and debug builds assert every push against its kind's schema.
- **The conformance suite** (`agentd-conformance`, [CONFORMANCE.md](../CONFORMANCE.md))
  checks the `extensions` and `events` families against a daemon the way a
  spec-only peer would.
- **Someone else's reading of the spec.** In CI's contract job, the official
  Python SDK, a2a-sdk 1.1.5, runs as a *client* of a real agentd
  (`interface/test/stock/sdk_client.py`): it resolves the card, parses every
  answer strictly into the protocol's own types, and sends a command the way
  any client activates an extension. The TypeScript client runs against the
  SDK's official sample server (`test/stock.e2e.mjs`), and agentd's outbound
  client calls it too (`test/stock/peer.e2e.mjs`).
- **The pages name only what is served.** `docs_surface_guard.rs` holds every
  JSON-RPC method and every agentd URI named in the docs, READMEs, the Agent
  Skill and the site to the tables above; `unversioned_guard.rs` refuses a
  version in any identifier agentd owns.

---

## See also

- [a2a.md](a2a.md) — the channel itself: the pipeline, principals, sign-in, tasks, the card
- [ext/command.md](ext/command.md), [ext/events.md](ext/events.md),
  [ext/task-annotations.md](ext/task-annotations.md),
  [ext/binding-jsonrpc-unix.md](ext/binding-jsonrpc-unix.md) — the normative specifications
- [operations.md](operations.md) — the admin ops in an operational context
- [interface.md](interface.md) — the display clients that use the feed
- [The A2A extensions specification](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md)
