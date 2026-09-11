# A2A: the channel other agents and operators use

MCP is how agentd reaches *outward* for capability — tools and events arrive
from servers you name. A2A is the opposite direction: it is how something
**reaches in**. A parent agent delegating work, a peer in a mesh, a human
driving the daemon from a terminal, and the web UI in a browser all speak the
same protocol to the same listener.

The two are easy to confuse because both are JSON-RPC over HTTPS. The
distinction worth holding onto is direction and ownership:

| | MCP | A2A |
|---|---|---|
| Direction | agentd calls out | something calls in |
| agentd is | the client | the server |
| Carries | tools, resources, subscriptions | messages, tasks, commands, control |
| Configured by | `mcp.servers` | `a2a.listen` |
| Absent by default | no servers, no tools | no listener, no external access |

An agentd with neither is a closed box that can still think and answer on
stdout. Adding MCP gives it hands; adding A2A gives other people a door.

## Turning it on

```yaml
a2a:
  listen: https://0.0.0.0:8443
  tls:
    cert: /etc/agentd/tls/server.crt
    key: /etc/agentd/tls/server.key
    client_ca: /etc/agentd/tls/clients-ca.crt   # mTLS: who may connect at all
```

A listener makes the instance **long-lived**, which means it needs a durable
store — a daemon that forgets its tasks on restart is worse than one that never
accepted them. Validation enforces this rather than letting you discover it
after a crash.

> **Trust is per request, never the transport.** A non-loopback bind must
> configure mTLS and/or a bearer; an unauthenticated non-loopback listener is a
> startup error, not a warning. A loopback `http://` bind with no credential is
> allowed for local development only — there, being on the machine *is* the
> authorization.

## What a caller can do

Every request resolves to a **principal** (from the mTLS certificate or the
bearer) and is authorized against a role matrix before anything runs.

| Method | What it does |
|---|---|
| `SendMessage` | Natural language becomes a conversation turn; a command DataPart becomes a registry action (`status`, `workflow.run`, `config`, …) |
| `SendStreamingMessage` | The same, answered as an SSE stream of status and artifact updates |
| `GetTask` / `ListTasks` | Read a durable task, or enumerate them |
| `CancelTask` | Stop one in flight |
| `SubscribeToTask` | Follow one task's transitions |
| `SubscribeToEvents` | The instance-wide observation feed the display clients render (needs `interface.enabled`) |
| `GetAgentCard` | Discovery: identity, transport, and the capabilities this instance actually implements. Also served unauthenticated at `/.well-known/agent-card.json` |
| `GetExtendedAgentCard` | The authenticated card: the same document, with the skills *this caller* may actually run |
| `CreateTaskPushNotificationConfig` etc. | Register a webhook for a task's updates instead of holding a stream open (see below) |

Errors use the codes the spec assigns, because peers branch on them: `-32601`
for a method that does not exist, `-32001` for a task that does not. A peer
should never have to string-match an error message.

### Everything else is a declared extension

Those are the A2A methods, and agentd answers **only** those — the set is
checked against an independent implementation of the spec in CI. Anything
agentd speaks beyond them is declared on the card as an `AgentExtension`, which
is the mechanism [the specification provides](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md)
for exactly this:

| Extension URI | What it declares |
|---|---|
| `https://agentd.dev/a2a/ext/command/v1` | the **command ops** — structured operations sent as a DataPart on `SendMessage`. Data-only: no new method, no changed core structure, never `required` |
| `https://agentd.dev/a2a/ext/interface/v1` | `SubscribeToEvents`, the instance-wide observation feed. A method extension, because A2A has no instance-feed concept |

A client activates one by listing its URI in the **`A2A-Extensions`** request
header (comma-separated); the response echoes the header with the ones actually
activated. Asking for an extension agentd does not implement is not an error —
it simply is not echoed, and none of agentd's extensions is `required`, so a
client that sends no header at all still gets a complete service.

```console
$ curl -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command/v1' …
< A2A-Extensions: https://agentd.dev/a2a/ext/command/v1
```

### Calling an operation: the command DataPart

There is no tool-call primitive in A2A. The protocol's answer is a message that
carries structured input, so an operation is an ordinary `SendMessage` whose
part is a DataPart under the `agentd` key:

```jsonc
{ "jsonrpc":"2.0", "id":1, "method":"SendMessage",
  "params": { "message": { "role":"ROLE_USER", "messageId":"m-1", "parts": [
      { "data": { "agentd": { "op":"workflow.run", "workflow":"triage" } } }
  ] } } }
```

Every op is also published as a **skill** on the agent card, and
`GetExtendedAgentCard` narrows that list to the ops *this caller* may run — so
"what may I ask for" is answered by the protocol's own discovery, not by
reading this page.

### The wire is proto3 JSON

A2A is defined in protocol buffers, and its JSON binding is proto3 JSON — which
is stricter than "some JSON with these field names". Three consequences are
worth stating, because getting them wrong fails silently in the *peer*:

- **Enums are the proto value names.** `TASK_STATE_COMPLETED`, not `completed`;
  `ROLE_AGENT`, not `agent`.
- **Timestamps are RFC 3339 strings.** `status.timestamp` is a
  `google.protobuf.Timestamp`, so `"2026-08-17T13:41:27.824Z"` — not epoch
  milliseconds.
- **Every task is a `Task`.** `ListTasks` returns the same object as `GetTask`
  (minus the artifacts a listing does not resolve), so the state is always at
  `status.state`. The result carries `totalSize`, `pageSize` and
  `nextPageToken`; agentd answers in a single page.
- **Field names are `lowerCamelCase`, not snake_case.** proto3 JSON renames
  every field, so `historyLength` and `nextPageToken` — a snake_case key is not
  an alias, it is an unknown field. The one that catches people is `GetTask`,
  whose task identifier is plain **`id`** (agentd also accepts `taskId`;
  `task_id` is not a field in the protocol at all).

### Being told instead of watching

Streaming assumes the caller can hold a connection open for as long as the work
takes. A caller that cannot — a serverless function, a queue consumer — registers
a webhook, and agentd POSTs each of that task's updates to it. The body is the
`Task`, exactly as a streaming caller would have seen it, so one handler serves
both ways of being told.

```yaml
a2a:
  push:
    enabled: true         # default OFF
    allow_private: false  # default OFF
```

Both defaults are off, and the reason is that **the URL comes from the caller**.
Every delivery is an outbound request to an address a *peer* chose, which is the
shape of an SSRF: pointed at a cloud metadata endpoint, agentd fetches
credentials on the caller's behalf. So `enabled` says you are willing to make
that request at all, and `allow_private` — a separate, larger decision — says you
are willing to make it to a private or loopback address. A target is checked
twice: at registration, where the caller is present to be told why it was
refused, and again at delivery, because a name can resolve somewhere new in
between.

The receiver gets `X-A2A-Notification-Token` echoed back from the config, so it
can tell a real delivery from a stray POST at a URL somebody guessed. A bearer
agentd should *present* goes in `authentication.credentials` with a `Bearer`
scheme, and is never read back out.

Delivery is best-effort by design: a webhook that is down must not fail the task
it was reporting on. Failures are logged as `a2a.push.failed` and dropped.

Anything agentd wants to say that the spec has no field for goes under
`metadata`, namespaced: `agentd/principal`, `agentd/link`,
`agentd/statusHistory`. That is what proto3 leaves open for extensions, and it
means a strict peer can ignore all of it.

This is verified by construction rather than by cross-check. The listener IS
[a2a-rs](https://github.com/emillindfors/a2a-rs)'s JSON-RPC adapter, and every
task, message and card agentd emits is serialized by types that crate generates
from the A2A protobuf — so the wire shape is the schema's, not our reading of
it. `agentd-conformance` then asserts the behaviour those shapes carry on every
path that emits a task.

There used to be a second reader here: an `a2a-oracle` crate that booted the
daemon and re-parsed its responses with a2a-rs. It was worth having while the
server was hand-written. Once the server became a2a-rs, the round trip had the
same generated types on both ends and agreed by construction, so it was retired
— keeping the two assertions that did not depend on a daemon (our method names
and error codes are the SDK's constants) as unit tests.

## Roles, and what each may call

| Role | May call |
|---|---|
| `operator` | everything, unconditionally — including the `admin.*` ops |
| `user` | `workflow.run` / `status` / `cancel`, `subagent.send` / `status`, `plan.get`, `ask_human`, `conversation.get`, `run.get` |
| `agent` | `workflow.run`, `workflow.status` |
| `anonymous` | nothing beyond public discovery (`GetAgentCard`) and the pairing handshake (`Pair`, when `interface.pairing` is on) — denied at every other layer, and an explicit `grants: ["*"]` does not rescue it |

Principals are matched **first-match-wins**, in the order written. agentd does
not rank rules by specificity — so the most specific rule wins only if you put
it first, and a broad rule placed early shadows every rule after it:

```yaml
a2a:
  principals:
    # Narrow first: this one identity is an operator...
    - { match: { san: "spiffe://acme/ops/deployer" }, role: operator }
    # ...everyone else holding an acme certificate is an ordinary user.
    # Swap these two and the broad rule matches the deployer first, silently
    # demoting it — no error, just a caller with fewer grants than intended.
    - { match: { san: "spiffe://acme/*" }, role: user, grants: [workflow.*] }
```

The ordering trap worth knowing before you hit it: if a gateway forwards calls
under one control-plane client certificate and distinguishes the real caller by
a bearer, then the `san` rule matching that certificate must come **last**,
after the `bearer_ref` rules:

```yaml
a2a:
  principals:
    - { match: { bearer_ref: "{{secret:ALICE_TOKEN}}" }, role: user, grants: [workflow.*] }
    - { match: { san: "spiffe://acme/gateway" }, role: agent, grants: [workflow.run] }
```

Put the `san` rule first and it matches every forwarded call — the gateway's
certificate is on all of them — so the bearer rules below become unreachable and
every user collapses into the gateway's identity.

**`client_ca` makes mTLS mandatory for every caller.** Setting it builds a
rustls verifier with no unauthenticated fallback, so a client without a
CA-signed certificate cannot complete the handshake at all — bearer-only clients
and the pairing flow included. The three client-auth mechanisms are not
free-standing alternatives you can mix: `client_ca` gates the *transport*, and
`a2a.bearer` / principals decide *who* an already-admitted connection is. If you
want bearer-only clients on a non-loopback listener, do not set `client_ca`.

## The card is a promise

`GetAgentCard` advertises what this build can do — and only that. If the card
says `streaming: true`, a streaming send really produces update frames; if it
says `pushNotifications: false`, asking for one is refused with a proper error
rather than half-served. Both directions are covered by the conformance suite,
because a peer that believes the card and builds against something absent fails
expensively and late.

Check what a given instance offers before wiring against it:

```console
$ curl -s -X POST https://agent.internal:8443/ \
    -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"GetAgentCard","params":{}}' | jq .result.capabilities
```

## One agent driving another

Composition needs no new protocol: the channel a parent dials is the channel a
worker serves. Deploy a worker that exposes A2A, and point a parent at it as a
peer — the parent delegates as spec-conformant Tasks and gets artifacts back.

```yaml
# the parent: delegate to a worker that speaks A2A
a2a:
  peers:
    - name: reviewer
      endpoint: https://reviewer.internal:8443
```

A workflow step (`a2a.delegate`) or the agent itself can then hand work to
`reviewer` and wait for its result. The worker is an ordinary agentd: its own
instruction, its own tools, its own budget — and its own fence.

### Co-located peers: the unix-socket fast lane

Two instances on the same host (or the same pod) can skip TCP and TLS entirely:
one listens on a **unix domain socket**, the other names that socket as its
peer endpoint. Same protocol, same tasks and artifacts — only the transport and
the authenticator change.

```yaml
# instance B — the worker
a2a:
  listen: unix:///run/agentd/bee.sock

# instance A — the delegator
a2a:
  peers:
    - name: bee
      endpoint: unix:///run/agentd/bee.sock
```

Why this is the fast lane: no TLS handshake per dial, no TCP stack, and data
moves through the kernel's socket buffer — the cheapest IPC that still keeps
the A2A contract (so moving the peer to another host later is a one-line
endpoint change back to `https://`).

Authentication changes shape rather than disappearing. TLS material is
**refused** on a unix listener; the kernel authenticates instead: the socket
file is created `0600`, and every connection's `SO_PEERCRED` uid must be the
daemon's own user (or root) — a different local user is dropped before HTTP,
logged as `a2a.unix.denied`. That is strictly stronger than loopback TCP, which
any local user can dial; a connection that passes gets the loopback trust
posture (operator in the single-user setup), and a configured `a2a.bearer`
still applies on top. Webhooks deliberately do NOT take `unix://` — they are an
external surface.

Two instances can also hold sockets in *both* directions (each listens, each
names the other as a peer) — then either side can `a2a.send`/`a2a.delegate` at
any time, which is the "two agents agree to connect" pattern: the agreement is
the pair of socket paths in their configs, and the filesystem's permissions are
the contract.

## Where the display clients fit

The TUI and web UI are A2A clients. They use the same listener, the same
principals, and the same task surface a peer would; nothing about them is
privileged except that a loopback connection resolves to `operator`. That is
why several surfaces can watch one session at once, and why a client can be
attached from another machine with a rotating pairing code instead of a copied
bearer.

See [interface.md](interface.md) for the client surface.

## See also

- [a2a-extensions.md](a2a-extensions.md) — everything agentd speaks beyond core
  A2A, in one place: the declarations, the `A2A-Extensions` handshake, the
  command DataPart, and the checks that keep the claim true.

- [mcp.md](mcp.md) — the other direction: where tools and events come from.
- [security.md](security.md) — principals, the trifecta rule, and what the
  listener does not protect you from.
- [operations.md](operations.md) — driving a live daemon: drain, pause, reload.
