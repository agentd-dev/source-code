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
| `GetAgentCard` | Discovery: identity, transport, and the capabilities this instance actually implements |

Errors use the codes the spec assigns, because peers branch on them: `-32601`
for a method that does not exist, `-32001` for a task that does not. A peer
should never have to string-match an error message.

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

Anything agentd wants to say that the spec has no field for goes under
`metadata`, namespaced: `agentd/principal`, `agentd/link`,
`agentd/statusHistory`. That is what proto3 leaves open for extensions, and it
means a strict peer can ignore all of it.

This is verified two ways. `agentd-conformance` asserts the shapes on every path
that emits a task; and `crates/a2a-oracle` — excluded from the default build —
boots the real daemon and parses its responses with
[a2a-rs](https://github.com/emillindfors/a2a-rs), an unrelated implementation of
the same specification, so a misreading on our side has to survive a second
reader before it reaches anyone.

## Roles, and what each may call

| Role | May call |
|---|---|
| `operator` | everything, unconditionally |
| `user` | `workflow.run` / `status` / `cancel`, `subagent.send` / `status`, `plan.get`, `ask_human`, `conversation.get`, `run.get` |
| `agent` | `workflow.run`, `workflow.status` |
| `anonymous` | nothing — denied at every layer, and an explicit `grants: ["*"]` does not rescue it |

Principals are matched in order, so the most specific rule wins:

```yaml
a2a:
  principals:
    - { match: { san: "spiffe://ops/*" },  role: operator }
    - { match: { san: "spiffe://team/*" }, role: user, grants: [workflow.*] }
```

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

## Where the display clients fit

The TUI and web UI are A2A clients. They use the same listener, the same
principals, and the same task surface a peer would; nothing about them is
privileged except that a loopback connection resolves to `operator`. That is
why several surfaces can watch one session at once, and why a client can be
attached from another machine with a rotating pairing code instead of a copied
bearer.

See [interface.md](interface.md) for the client surface, and
[RFC 0029](../rfcs/0029-a2a-conversations-principals-commands.md) for the
normative contract.

## See also

- [mcp.md](mcp.md) — the other direction: where tools and events come from.
- [security.md](security.md) — principals, the trifecta rule, and what the
  listener does not protect you from.
- [operations.md](operations.md) — driving a live daemon: drain, pause, reload.
