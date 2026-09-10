# A2A extensions — everything agentd speaks beyond the core protocol

A2A defines eleven JSON-RPC methods and a message model. Real agents need more
than that, so the specification includes a mechanism for the rest:
[extensions](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md).
An extension is a URI, declared on the agent card, that a client may activate
per request.

agentd answers the eleven core methods and the extension methods it declares
here, plus two bootstrap calls the spec's JSON-RPC binding does not define:
`GetAgentCard` (the public card, also served unauthenticated as a GET on
`/.well-known/agent-card.json` and `/.well-known/agent.json`) and `Pair`, the
pairing handshake that trades a rotating code for a session token. Neither can
ride an extension, because both run *before* the mechanism exists: a client has
no card to read declarations from until `GetAgentCard` answers, and no
credential to activate anything with until `Pair` succeeds. Everything else
stays on the spec's own surface. That is not a stylistic preference — it is what
makes an agentd instance callable by a peer that has never heard of agentd.

---

## 1. The rules, briefly

The specification says four things worth memorising.

**Declaration.** An agent lists `AgentExtension` objects inside
`AgentCapabilities` on its card: a `uri`, a human-readable `description`, a
`required` flag, and optional `params`.

**Activation.** A client names the extensions it intends to use in the
`A2A-Extensions` request header, comma-separated. The response echoes the same
header listing what was *actually* activated. Asking for something the agent
does not implement is not an error; it simply is not echoed back.

**URIs are identifiers, not addresses.** Nobody is expected to fetch them. They
should carry a version, and a breaking change takes a *new* URI rather than
redefining an old one.

**What an extension may not do.** It may not change the definition of core data
structures — no new fields on `Task`, no removed fields on `Message`. Custom
data belongs in the `metadata` maps the core structures already provide, or in
a `DataPart`, which exists precisely to carry structured payloads.

The spec groups extensions into four kinds: **data-only** (information on the
card), **profile** (extra structure or narrower values on core messages),
**method** (new RPC methods), and **state machine** (new task states). agentd
uses the first and the third.

---

## 2. What agentd declares

| URI | Kind | Declares |
|---|---|---|
| `https://agentd.dev/a2a/ext/command/v1` | data-only | the **command ops** — structured operations sent as a DataPart on `SendMessage` |
| `https://agentd.dev/a2a/ext/interface/v1` | method | `SubscribeToEvents`, the instance-wide observation feed |

None is `required`. A client that sends no `A2A-Extensions` header at all gets a
complete, working service: it can converse, run workflows, read tasks and
subscribe to them. The extensions add reach, never a precondition.

`GetAgentCard` returns all of this unauthenticated. Try it against a running
instance:

```console
$ curl -s -X POST http://127.0.0.1:8080/ \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"GetAgentCard","params":{}}' \
  | jq '.result.capabilities.extensions[] | {uri, required}'
```

---

## 3. Activating one

```console
$ curl -i -X POST http://127.0.0.1:8080/ \
    -H 'content-type: application/json' \
    -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command/v1' \
    -H 'authorization: Bearer …' \
    -d '{"jsonrpc":"2.0","id":1,"method":"GetAgentCard","params":{}}'

HTTP/1.1 200 OK
A2A-Extensions: https://agentd.dev/a2a/ext/command/v1
```

Two behaviours to rely on:

- **Unknown URIs are dropped, not refused.** Send three, get back the two that
  exist. This keeps a client that supports several agents from having to branch
  per agent before it can make a request.
- **Nothing activated means no header.** The absence of the response header is
  the answer, not an empty one.

For a browser client, both directions are wired: `A2A-Extensions` is in the
CORS preflight's allowed request headers *and* in `Access-Control-Expose-Headers`,
so JavaScript can actually read the echo.

agentd's own clients announce: the Rust peer client sends the header on both the
streaming and non-streaming command paths, and the TypeScript client
(`@agentd/interface`) sends it on a command DataPart and on `SubscribeToEvents`
— and on nothing else, because a plain conversational `SendMessage` uses no
extension and claiming one you are not using is noise.

---

## 4. The command extension — calling an operation

A2A has no tool-call primitive, and inventing an RPC method for each operation
would put agentd's whole surface out of reach of stock clients. The protocol's
own answer is a message carrying structured input, so every agentd operation is
an ordinary `SendMessage` whose part is a `DataPart` under the `agentd` key:

```jsonc
{ "jsonrpc": "2.0", "id": 1, "method": "SendMessage",
  "params": { "message": {
      "role": "ROLE_USER",
      "messageId": "m-1",
      "parts": [ { "data": { "agentd": {
          "op": "workflow.run",
          "workflow": "triage",
          "inputs": { "item": "…" }
      } } } ]
  } } }
```

The reply is a **Task** — the protocol's model for work — carrying the result.
For a quick read like `status` the task is already `COMPLETED` when it comes
back; for `workflow.run` it transitions as the run progresses, and a client can
follow it with `SubscribeToTask` or register a webhook.

### The ops

| Op | Does | Who |
|---|---|---|
| `status` | runs, subagents, conversations, budget | any named caller |
| `config` | the effective configuration, credentials redacted | operator |
| `workflow.run` / `.status` / `.cancel` / `.signal` | workflow control | `user` (run/status/cancel), `agent` (run/status); `.signal` needs an explicit grant or `operator` |
| `subagent.send` / `.kill` / `.status` | subagent control | `user` (send/status); `.kill` needs an explicit grant or `operator` |
| `plan.get` | the working plan | `user` |
| `admin.drain` / `.lameduck` / `.pause` / `.resume` / `.cancel` | lifecycle control — see [operations §2](operations.md) | **operator only** |
| `interface.info` | display surface (needs `interface.enabled`) | any named caller |
| `conversation.get`, `run.get`, `debug.events` | debug reads (need `interface.debug` as well) | `user`, scoped to its own objects; `debug.events` spans every principal's activity and cannot be scoped to the caller, so it needs `operator` or an explicit grant |

The published list is in the extension's `params.ops`, and the same ops appear
as **skills** on the card. Three interface calls sit outside it: `config.set`,
`subagent.get` and `pairing.code` are answered by the listener but enumerated by
`interface.info` instead — that is the first call a display client makes,
precisely so it never offers an action this daemon would refuse.
`GetExtendedAgentCard` narrows the skills to what *this caller* may run, so
"which operations may I use" is answered by A2A's own discovery rather than by
this page.

### The built-in ops are a reserved namespace

A workflow's `a2a` start node registers a command name, which is how a peer
reaches a start node at all. Those names may not collide with a built-in op: a
declared command takes the durable-inbox path, where the per-op authorization
the built-ins carry does not run, so a workflow claiming `admin.drain` would
shadow an operator's control with a run that anyone its `roles:` admits could
fire. On an instance where the model may create workflows, that author is the
model.

The collision is refused at validation — at config load and at
`workflow.create` alike — and the listener dispatches a built-in to its own
handler regardless, so the reservation holds even if a definition slipped
through from somewhere else.

### The admin family answers to the role, not to a grant

`admin.*` is operator-only, and an explicit `grants:` entry does **not** reach
it — not even `grants: ["*"]`. A peer that could drain the instance it is
delegating to would be an operator, and a peer is not one. This is checked
before grants are consulted, and a test pins it.

---

## 5. The interface extension

`SubscribeToEvents` is an instance-wide observation feed: every attached display
client sees the same frames, scoped to what its principal may see. A2A models
per-task streams (`SubscribeToTask`), not per-instance ones, so there is nothing
in the core protocol to map this onto — which is exactly the case a method
extension is for.

It is declared only when `interface.enabled` is set, because the card is a
promise: an instance that will not serve the feed must not advertise it.

---

## 6. There is no legacy path

Earlier builds answered five custom JSON-RPC methods — `a2a.drain`,
`a2a.lameduck`, `a2a.pause`, `a2a.resume`, `a2a.cancel`. They are **gone**, not
deprecated: a call to one now gets `-32601`, the code that served them has been
deleted, and nothing on the card mentions them. The same five operations are
`admin.drain`, `admin.lameduck`, `admin.pause`, `admin.resume` and
`admin.cancel`, sent as a command DataPart:

```jsonc
// then
{ "method": "a2a.pause", "params": { "run": "reconcile-01J8…" } }

// now
{ "method": "SendMessage", "params": { "message": { "parts": [
    { "data": { "agentd": { "op": "admin.pause", "run": "reconcile-01J8…" } } } ] } } }
```

The reply is a Task whose result carries the acknowledgement, rather than the
acknowledgement directly.

## 7. How this is kept honest

Three checks, because a compliance claim that nobody verifies decays:

- **The method set is checked against an independent implementation.** The
  `a2a-oracle` suite boots the real daemon and asserts every method in
  `METHODS` — the spec surface agentd claims — is one `a2a-rs`, a different
  author's reading of the same spec, also names; the one declared extension
  method in that list, `SubscribeToEvents`, is skipped by name. A method we
  invented or misspelled fails there.
- **Every non-spec method must be declared.** `EXTENSION_METHODS` pairs each
  extra method with the extension that declares it, and a unit test refuses any
  method that is in neither the spec list nor a declaration. Both that test and
  the oracle walk the same `METHODS` constant, so the two bootstrap calls this
  page opens with — `GetAgentCard` and `Pair` — sit outside the check by
  construction rather than by accident: they are answered before a card or a
  credential exists, so there is no declaration for them to be checked against.
- **One list feeds three views.** The ops the card renders as skills, the ops
  the extension declares, and the ops `--capabilities` reports all come from
  `command_ops_of`. They cannot drift apart, because there is nothing to drift.

---

## See also

- [a2a.md](a2a.md) — the channel itself: principals, roles, tasks, the card
- [operations.md](operations.md) — the admin ops in an operational context
- [interface.md](interface.md) — the display clients that use the feed
- [The A2A extensions specification](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md)
