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
| agentd is | the client | the server (and, to its peers, a client) |
| Carries | tools, resources, subscriptions | messages, tasks, commands |
| Configured by | `mcp.servers` | `a2a.listen` (and `a2a.peers` to call out) |
| Absent by default | no servers, no tools | no listener, no external access |

An agentd with neither is a closed box that can still think and answer on
stdout. Adding MCP gives it hands; adding A2A gives other people a door.

agentd speaks **A2A 1.0**. Everything on this page is what the listener does;
the three extensions it declares on top each have a specification of their own,
served at the URI they are named by, and [a2a-extensions.md](a2a-extensions.md)
is the overview of them.

## Turning it on

```yaml
a2a:
  listen: https://0.0.0.0:8443
  url: https://agent.example.com:8443      # what callers dial; required on a wildcard bind
  bearer: "{{secret:AGENTD_OPERATOR_TOKEN}}"
  tls:
    cert: /etc/agentd/tls/server.crt
    key: /etc/agentd/tls/server.key
```

The load refuses a listener that would be served on terms nobody chose:

- **A non-loopback bind needs client auth**: `a2a.bearer` or `a2a.tls.client_ca`.
  An unauthenticated non-loopback listener is a startup error, not a warning.
- **Plaintext is loopback only.** `http://` is for development on
  `127.0.0.1`, `::1` or `localhost`; everything else is `https://` with
  `a2a.tls.cert` and `a2a.tls.key`.
- **A wildcard bind needs `a2a.url`.** `0.0.0.0` is no address a caller can
  dial, so the card and the OAuth issuer publish `a2a.url` instead of guessing.
- **A unix socket takes no TLS.** On `unix://` the kernel authenticates the
  peer (see [co-located peers](#co-located-peers-the-unix-binding)), and
  `a2a.tls` must be unset.

A loopback bind with no credential at all loads, with a warning: every local
process that presents nothing is then the operator (see
[the implicit operator](#the-implicit-operator)).

A listener makes the instance **long-lived**, and a long-lived instance keeps
its tasks in a durable store — the local file store when `store` is not set.
`store.kind: none` on a listener is refused: a daemon that forgets its tasks on
restart is worse than one that never accepted them.

### `a2a.url` is an origin

`a2a.url` is `scheme://host[:port]` and nothing more — no path, query or
fragment, lowercase, no default port, and `https` unless the host is loopback.
Two things are served at the root of that origin and only there: the
well-known Agent Card and the OAuth authorization server metadata. A path
would put the listener somewhere neither discovery rule looks.

Without `a2a.url`, a concrete bind advertises `scheme://host:port/`, and a unix
listener advertises `unix://<path>`. `a2a.url` is restart-only.

### Browsers: `a2a.cors.origins`

```yaml
a2a:
  cors:
    origins: [https://console.example.com]
```

A request carrying `Origin` — to `POST /` or to any `/oauth2/*` endpoint — is
served only when that origin is listed, compared as an origin (scheme, host,
port with its default). Anything else, `Origin: null` included, is `403` with no
CORS headers, before its body is read: a page the operator never authorised
cannot drive the listener through their browser. `*` is refused at load, and a
UI served from loopback must be listed like any other; the one origin admitted
unlisted is the UI `agentd ui` itself launches, for that process only. A listed
origin gets its own origin echoed in `Access-Control-Allow-Origin`, with
`A2A-Extensions`, `Retry-After` and `WWW-Authenticate` exposed to the page.
Admission is not trust: a browser still [signs in](#the-implicit-operator).
The list is reloadable.

## The protocol

### The eleven methods

agentd answers exactly the methods A2A 1.0 defines for the JSON-RPC binding,
matched by exact name. Every other name — a misspelling, an A2A 0.3 name such
as `message/send`, anything — is `-32601` for every caller, before any
authorization could make the answers differ.

| Method | What it does |
|---|---|
| `SendMessage` | A conversation turn, or, as a command DataPart, a registry op. Answers a Task, or a Message for a read op |
| `SendStreamingMessage` | The same, answered as an SSE stream |
| `GetTask` | One task the caller may see, with its history |
| `ListTasks` | The caller's tasks, filtered and paged |
| `CancelTask` | Cancel one task in flight |
| `SubscribeToTask` | Follow one task's updates as an SSE stream |
| `CreateTaskPushNotificationConfig` | Register a webhook for one task |
| `GetTaskPushNotificationConfig` | Read one webhook registration back |
| `ListTaskPushNotificationConfigs` | Page through one task's webhooks |
| `DeleteTaskPushNotificationConfig` | Remove one |
| `GetExtendedAgentCard` | The authenticated card: the public one plus what *this caller* may run |

The public Agent Card is not a method; it is a document at
[`/.well-known/agent-card.json`](#discovery-the-public-card).

### Everything else is a declared extension

Anything agentd speaks beyond those methods is declared on the card as an
extension — the mechanism A2A provides for exactly this — and a client
activates it by naming its URI in the **`A2A-Extensions`** request header:

| URI | Kind | What it adds |
|---|---|---|
| [`https://agentd.dev/a2a/ext/command`](https://agentd.dev/a2a/ext/command) | profile | the **command ops**: structured operations sent as a DataPart on `SendMessage` |
| [`https://agentd.dev/a2a/ext/events`](https://agentd.dev/a2a/ext/events) | method | `agentd.events/SubscribeToEvents`, the instance-wide observation feed; declared when `a2a.events.enabled` is on |
| [`https://agentd.dev/a2a/ext/task-annotations`](https://agentd.dev/a2a/ext/task-annotations) | profile | agentd's facts about a task, under `metadata[<URI>]`, on every method that returns tasks |

None of them is `required`: a client that sends no `A2A-Extensions` header at all
still gets the whole core protocol. The response echoes, in the same header, the
extensions the request actually activated; an extension agentd does not
implement is ignored rather than refused. Each URI is its extension's
normative specification, served at that address.

agentd's extension and binding URIs carry no version. A2A 1.0.1 §4.6.3 makes a
version in an extension URI a SHOULD, not a MUST; what it and §5.8 make a MUST
is that a URI's meaning never changes under a peer that already speaks it. So an
incompatible change takes a new URI with a new name.

### Discovery: the public card

```console
$ curl -s https://agent.example.com:8443/.well-known/agent-card.json | jq .capabilities
$ curl -sI https://agent.example.com:8443/.well-known/agent-card.json
HTTP/1.1 200 OK
content-type: application/json
etag: "4f1c…"
cache-control: public, max-age=60
access-control-allow-origin: *
```

The card is a public document: `GET` or `HEAD`, no credential, no
`A2A-Version`, readable cross-origin by any page (`Access-Control-Allow-Origin:
*`, never with credentials). It carries an `ETag` and answers a matching
`If-None-Match` with `304`. When the runtime cannot produce it within five
seconds, the answer is `503` with `Retry-After: 5`, never a stale or partial
card.

What it says:

- **`supportedInterfaces`**: one entry — the advertised URL with
  `protocolBinding: JSONRPC` and `protocolVersion: 1.0`, or, on a unix listener,
  the `unix://` URL with agentd's [unix binding](https://agentd.dev/a2a/binding/jsonrpc-unix).
- **`capabilities`**: `streaming: true`; `pushNotifications` exactly when
  `a2a.push.enabled`; `extendedAgentCard` exactly when the listener declares an
  authentication scheme; and the extension declarations.
- **`skills`**: one, `conversation`. Workflows are what a caller may *run*, so
  they belong on the extended card, which knows who is asking.
- **`securitySchemes` / `securityRequirements`**: what the listener enforces
  (see [what the card declares](#what-the-card-declares)).

The public card is the same for every caller and varies only with the
listener's own settings — push, the events extension, the authentication
posture, the URL, `agent.name` and `agent.description`. It never names a
workflow, and never depends on who asked.

**The card is a promise.** If it says `pushNotifications: false`, registering a
webhook is refused with `-32003`; if it declares no scheme, it has no extended
card. The conformance suite holds both directions, because a peer that believes
the card and builds against something absent fails expensively and late.

`agentd --capabilities` prints the same facts from a config file without
starting anything: the methods, the command ops, the declared extensions, the
URL and the authentication posture.

### A request, step by step

Every `POST /` runs through one pipeline, in this order. Each refusal is final,
is plain `application/json` — never an SSE frame — and costs the runtime
nothing:

1. **Origin.** A request carrying `Origin` must name one of `a2a.cors.origins`,
   matched exactly as an origin. Refused: `403` with an empty body.
2. **Failed-credential limit.** A source over its limit has its bearer refused
   `429` before the bearer is checked (see [the source limiter](#failed-credentials-and-the-source-limiter)).
3. **Content type** must be `application/json` (parameters allowed). Otherwise
   `415` with an empty body — before a byte is read as JSON.
4. **Who is calling**, from the headers and the connection alone
   ([the evidence](#the-evidence-in-order)). Nobody, or a credential that
   failed: `401`. A certificate no rule gives a role: `403`.
5. **JSON**, then the **envelope** (below).
6. **`A2A-Version`**: `-32009` unless the header says 1.0.
7. **The caller's rate**: `429` (see [rate limits](#rate-limits)).
8. **The method**, from the route table: `-32601` for an unknown name.
9. **Extensions**: what `A2A-Extensions` activates, and `-32601` for an
   extension method whose extension is not declared or not activated.
10. **The method's authorization**: `403`; and for `GetExtendedAgentCard`, the
    [declared-credential gate](#the-extended-card).
11. **The checks that need the params**: a send's message shape, the task it
    names, its command envelope and the caller's reach to the op; a subscribe's
    task, read first as the caller.
12. **The answer.**

The order is the point. Nothing that costs the runtime anything runs before the
caller is known and has asked in the protocol this listener speaks.

### The envelope and `A2A-Version`

```console
$ curl -s https://agent.example.com:8443/ \
    -H 'content-type: application/json' \
    -H 'A2A-Version: 1.0' \
    -H "Authorization: Bearer $TOKEN" \
    -d '{"jsonrpc":"2.0","id":1,"method":"ListTasks","params":{"pageSize":10}}'
```

The body is one JSON-RPC 2.0 request object:

- `jsonrpc` is exactly `"2.0"` and `method` a non-empty string;
- **`id` is required**, a string or an integer. JSON-RPC calls a request without
  one a notification, which a server executes and never answers — for a send,
  a task started for a caller who can never learn its id. So a missing or
  `null` id is `-32600` "A2A requests must carry an id";
- `params`, when present, is an object (`-32602` otherwise);
- a batch (an array) is `-32600`.

**`A2A-Version: 1.0` is required on every `POST /`.** Any patch component is
accepted (`1.0.3`); a missing, empty or other value is `-32009` with an
ErrorInfo naming `supportedVersions: "1.0"`. A missing header is refused rather
than assumed, because the specification reads its absence as 0.3, and a 0.3
client that happened to use a 1.0 method name would otherwise be answered in
1.0 semantics. The header is read from the header only — never the query
string. Note that **clients built on a2a-rs up to 0.10 send no `A2A-Version`
header**, so they get `-32009` from agentd, as the specification says they
must.

The card, the `OPTIONS` preflights and the `/oauth2/*` endpoints take no
`A2A-Version`: they are not JSON-RPC.

### Errors

A peer branches on the code, never on the message, so every code means one
thing:

| Code | HTTP | Meaning |
|---|---|---|
| `-32700` | 200 | the body is not JSON |
| `-32600` | 200 | not a JSON-RPC request: a batch, a missing `id`, a bad `jsonrpc` or `method` |
| `-32601` | 200 | no such method, or an extension method whose extension is not declared or not activated |
| `-32602` | 200 | bad params: a malformed message or command, a filter the server cannot honour, a bad page size or token |
| `-32603` | 200 | internal error; also the draining refusal (`DRAINING`) |
| `-32603` | 429 | a rate or failed-credential limit (`RATE_LIMITED`), with `Retry-After` |
| `-32001` | 200 | no such task — or one the caller may not see; the same answer, so ids cannot be probed |
| `-32002` | 200 | the task is already terminal and cannot be canceled |
| `-32003` | 200 | push notifications are not enabled on this agent |
| `-32004` | 200 | unsupported operation: a message to a terminal task, subscribing to a finished task, the extended card where none is declared, an introspection op while introspection is off |
| `-32005` | 200 | a part's media type is not accepted (a raw or url part), or `acceptedOutputModes` excludes what a command answers with |
| `-32008` | 200 | a required extension was not activated |
| `-32009` | 200 | `A2A-Version` is missing or not 1.0 |
| `-31401` | 401 | who you are: no credential, or one that is invalid, expired or revoked |
| `-31403` | 403 | what you may do: a known principal whose role and grants do not reach the call |

`-32007` is never sent: a listener either declares a scheme and serves the
extended card, or declares none and answers `-32004`.

The two identity codes sit outside JSON-RPC's reserved range on purpose, so a
client that branches on a spec code is never told the wrong thing, and they
travel with their HTTP status so a proxy, a browser and a plain HTTP client all
see them. A `401` carries `WWW-Authenticate: Bearer realm="agentd"`, plus
`error="invalid_token"` when a credential was presented; a `403` to a bearer
caller carries `error="insufficient_scope"`. The listener's `401` has
`id: null`, and so does the `403` for a certificate no rule names — both are
decided before the body is read; a `403` for a method or an op echoes the
request's `id`. Every error carries a `google.rpc.ErrorInfo` in `error.data`, whose
`reason` names the case and whose `domain` is `a2a-protocol.org` for a reason
the specification defines and `agentd.dev` for one agentd coined.

The HTTP statuses outside JSON-RPC are `403` with an empty body (an origin not
listed), `415` (not `application/json`) and, on the card, `304` and `503`.

**Error fidelity.** Most methods are answered by
[a2a-rs](https://github.com/emillindfors/a2a-rs), whose own rendering of an
error would reword it and replace its details. When the refusal came from
agentd's runtime, the listener puts the runtime's own error object back —
code, message and data, byte for byte — and sets the HTTP status from the code,
so a refusal reads the same whichever path produced it. That includes the first
frame of a stream: an error there becomes the plain JSON answer the same refusal
gets on a unary call. The few codes a2a-rs invents outside the specification
(`-32100`…`-32102`) become `-32603` "internal error" with no data, because one
of them would otherwise name another principal's context.

### The wire is proto3 JSON

A2A is defined in protocol buffers, and its JSON binding is proto3 JSON — which
is stricter than "some JSON with these field names". Four consequences, because
getting them wrong fails silently in the *peer*:

- **Enums are the proto value names.** `TASK_STATE_COMPLETED`, not `completed`;
  `ROLE_USER`, not `user`.
- **Timestamps are RFC 3339 strings**, `"2026-08-17T13:41:27.824Z"` — not epoch
  milliseconds.
- **Every task is a Task.** `ListTasks` returns the same object as `GetTask`,
  cut to what the listing asked for, so the state is always at `status.state`.
- **Field names are `lowerCamelCase`.** `historyLength`, `nextPageToken`,
  `contextId`; a snake_case key is not an alias, it is an unknown field, and an
  unknown field is ignored as proto3 JSON says it must be. `GetTask` names its
  task with `id`.

Behind the pipeline the listener is a2a-rs's JSON-RPC adapter, and every task,
message and card agentd emits is serialized by the types that crate generates
from the A2A protobuf — so the wire shape is the schema's, not our reading of
it.

## Who is calling

### The evidence, in order

Every request resolves to a **principal** — an id, a role and grants — from its
headers and its connection, before any of its body is read. The evidence is
consulted in a fixed order, and each kind decides only for itself. The result
also records **how** the caller was named (its *via*), because some decisions
depend on that:

| Step | Evidence | Result | Via |
|---|---|---|---|
| 0 | a unix-socket peer of the daemon's own uid | the operator | unix |
| 1 | an `Authorization: Bearer` session token (`agentd_at_…`) | that session's principal, or `401` | session |
| 1 | any other bearer, on a listener with a bearer mechanism | `a2a.bearer` → the operator; a `bearer_ref` rule → its principal; neither → `401` | bearer |
| 2 | a verified client certificate | the first `san`/`sub` rule it matches; the operator while no rule exists at all; else `403` | cert |
| 3 | nothing presented, and an `any` rule exists | that rule's principal | any rule |
| 4 | nothing presented, a local peer, a loopback listener with no mechanism, **no `Origin`** | the operator | implicit |
| 5 | anything else | `401` | — |

A bearer that matches nothing is a failed request, never a fallback to what else
the request carried: otherwise "present any junk" would be as good as
presenting nothing. A bearer sent to a listener with no bearer mechanism at all
is ignored, as any unasked-for header is. A rule with `role: anonymous` names
nobody.

### The implicit operator

Step 4 is the single-operator development posture: on a listener bound to
loopback, with no `a2a.bearer`, no principals, no `client_ca` and no device
grant, a process on the same host that presents nothing is the operator. Being
on the machine *is* the authorization there.

Two limits keep that from reaching further than the machine:

- **It needs a loopback bind**, not merely a loopback peer. On a wildcard or
  other bind, "the connection came from 127.0.0.1" grants nothing — a
  same-host reverse proxy relays every remote caller from 127.0.0.1, and would
  otherwise make each of them the operator.
- **A request carrying `Origin` is never the implicit operator** — any value,
  `Origin: null` included, which is what a `file://` page, a sandboxed iframe
  or a `data:` URL sends. A page any site served can make the operator's
  browser POST to `127.0.0.1`, and admitting its origin through CORS is not
  trusting it. So a browser always authenticates: with a session (the
  [device grant](#the-device-authorization-grant), or the
  [launch grant](#the-launch-grant) `agentd ui` uses), a bearer or a
  certificate — or it matches an explicit `any` rule, which can never carry the
  operator role. Such a browser is told why: its `401` says browser requests
  must authenticate.

Configuring any credential mechanism ends the implicit operator, and so does a
[principals reload](#reloading-principals) that adds the first rule.

### Principals

```yaml
a2a:
  principals:
    # Narrow first: this one identity is an operator...
    - { match: { san: "spiffe://acme/ops/deployer" }, role: operator }
    # ...everyone else holding an acme certificate is an ordinary user.
    - { match: { san: "spiffe://acme/*" }, role: user, grants: [workflow.run] }
    # A bearer rule names one caller, so it carries the id its work is owned by.
    - { id: ci-bot, match: { bearer_ref: "{{secret:CI_TOKEN}}" }, role: agent }
```

A rule matches one kind of evidence — `san` (a glob over the certificate's
SANs and subject CN; a SPIFFE X.509-SVID's `spiffe://…` arrives as a URI SAN),
`sub` (the subject CN exactly), `bearer_ref` (a secret, compared in constant
time) or `any: true` — and gives a `role` (`operator`, `user`, `agent`,
`anonymous`), optional `grants`, and optional `quotas`.

Certificate rules are **first-match-wins**, in the order written: agentd does
not rank them by specificity, so a broad rule placed early shadows every rule
after it — swap the two `san` rules above and the deployer is silently a user.
A presented bearer is resolved before any certificate rule, whatever the order,
so a gateway that forwards callers under one client certificate and tells them
apart by bearer gets each caller's bearer rule.

**Principal ids** are what work is owned by. The operator role is always
`operator`; a user or agent is `<role>:<id>` when the rule declares `id` —
required on `bearer_ref` and `any` rules, unique across rules — and otherwise,
for a certificate, `<role>:cn=<CN>` or `<role>:san=<first SAN>`. The `=` keeps
derived ids out of the declared namespace, so a certificate whose CN equals a
declared id can never inherit that principal's work.

**What each role may call.** Every named role may call every core method (on its
own tasks — see [ownership](#ownership)); `anonymous` may call nothing. Which
command ops a role reaches by default, which need a grant, and which are the
operator's alone is the op table in the command extension's specification,
[https://agentd.dev/a2a/ext/command](https://agentd.dev/a2a/ext/command). An
operator-only op stays the operator's whatever a grant says: `grants: ["*"]`
does not lift a `user` to it.

**`client_ca` makes mTLS mandatory for every caller.** Setting it builds a
verifier with no unauthenticated fallback, so a client without a CA-signed
certificate cannot complete the handshake at all — bearer-only clients
included. `client_ca` gates the *transport*; `a2a.bearer` and the principals
decide *who* an admitted connection is. If you want bearer-only or signed-in
browser clients on a non-loopback listener, do not set `client_ca` (the device
grant refuses to load beside it).

### Ownership

What a non-operator starts is its own, and only its own:

- **Tasks.** `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask` and the
  push-config methods see only the caller's tasks. Someone else's task is
  `-32001` "task not found" — the same answer as a task that does not exist, so
  ids cannot be probed.
- **Runs, subagents and conversations.** Every command op that names one acts
  only on the caller's (`-32001` "no such run", "no such subagent", "no such
  conversation" otherwise). An object nobody owns is the operator's; the
  operator acts on anything.
- **`contextId` is the caller's own name.** A non-operator's `contextId` is
  never a key into the runtime: it is bound, per principal, to a fresh key of
  the runtime's (`ctx-<32 hex>`), so two callers who pick the same id — `root`
  included — hold two conversations, and neither can join, read or charge the
  other's. A task always shows its owner the `contextId` the owner used. An
  operator addresses conversations by the runtime's keys, which the `status`
  op lists.
- **The model acts for its caller.** When a turn runs for a non-operator, the
  model's own `workflow.cancel`, `workflow.signal` and `subagent.*` tools meet
  the same ownership checks the ops do, and a signal it sends wakes only runs
  that caller owns. A caller cannot reach through the model what it cannot reach
  through the listener.

Ownership is recorded by principal id — never by credential or session — so a
person who signs in again under the same name finds what that name started.

### The extended card

`GetExtendedAgentCard` is the public card plus what *this caller* may use: a
`workflow:<name>` skill for each workflow the caller may run, the command ops
this instance serves and the caller may run, the commands its workflows
declare, and, for the events extension, the kinds this caller's feed can carry.

It is an authenticated read, so it is served only where authentication means
something:

- a listener that **declares a scheme** (a bearer, the device grant, or mTLS)
  sets `extendedAgentCard: true`, and answers the method only to a caller named
  by a declared scheme — a bearer, a session or a certificate. A caller named by
  an `any` rule, or anybody who presented nothing, gets the `401` challenge;
- a listener that **declares none** (a no-auth loopback, a unix socket) sets
  `extendedAgentCard: false` and answers `-32004`.

## Signing in

### What the card declares

The card's `securitySchemes` and `securityRequirements` are derived from the
same posture the resolver enforces, so a card never promises a credential the
listener does not check, or hides one it does:

| Listener | `securitySchemes` | `securityRequirements` |
|---|---|---|
| loopback, nothing configured | none | none |
| unix socket | none | none |
| `a2a.bearer` or a `bearer_ref` rule | `bearer` | `{bearer}` |
| `a2a.device_grant.enabled` | `bearer`, `device_code` (OAuth 2.0 `deviceCode` flow) | `{bearer}`, `{device_code}` |
| `a2a.tls.client_ca` | `mtls` (plus `bearer` when one is configured) | `{mtls}` (plus `{mtls, bearer}`) |

An `any` rule adds the empty alternative `{}` — "or nothing at all" — and it is
the only thing that does. The `device_code` scheme's flow names
`<origin>/oauth2/device_authorization` and `<origin>/oauth2/token`, with
`oauth2MetadataUrl` when the origin is `https`.

### The device authorization grant

A person signs a terminal or a browser in without the daemon's bearer ever being
copied into it. The listener origin is an OAuth 2.0 authorization server for the
card's `device_code` scheme:

```yaml
a2a:
  listen: https://0.0.0.0:8443
  url: https://agent.example.com:8443
  bearer: "{{secret:AGENTD_OPERATOR_TOKEN}}"   # the operator who approves codes
  tls: { cert: /etc/agentd/tls/server.crt, key: /etc/agentd/tls/server.key }
  device_grant:
    enabled: true
    scopes: [user]          # user (default); add operator to allow operator sessions
    token_ttl: 8h           # 5m..30d
    code_ttl: 10m           # 1m..30m
```

The grant needs an operator credential to approve with (`a2a.bearer`, or an
operator rule matched by `bearer_ref`), and it is refused beside `client_ca`
(a device session has no certificate) and on a unix listener (the kernel
already vouches for every peer). It is restart-only.

| Endpoint | Standard | What it does |
|---|---|---|
| `POST /oauth2/device_authorization` | RFC 8628 §3.1 | a client asks for a code: `client_id`, optional `scope` |
| `POST /oauth2/token` | RFC 6749 §3.2, RFC 8628 §3.4 | the client polls with its `device_code` |
| `POST /oauth2/revoke` | RFC 7009 | end a session by its token |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 | the metadata; the issuer is the origin |
| `GET /oauth2/device` | — | the default verification page: plain text telling the person how an operator approves |

The flow:

```console
$ curl -s https://agent.example.com:8443/oauth2/device_authorization -d client_id=my-cli
{"device_code":"…","user_code":"BCDF-GHJK","verification_uri":"https://agent.example.com:8443/oauth2/device","expires_in":600,"interval":5}

# an operator approves the code under a name (the command extension's auth.device.approve)
$ curl -s https://agent.example.com:8443/ \
    -H 'content-type: application/json' -H 'A2A-Version: 1.0' \
    -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command' \
    -H "Authorization: Bearer $OPERATOR_TOKEN" \
    -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"role":"ROLE_USER","messageId":"m-1",
         "extensions":["https://agentd.dev/a2a/ext/command"],
         "parts":[{"data":{"agentd":{"op":"auth.device.approve","user_code":"BCDF-GHJK","as":"alice"}}}]}}}'

# the client's next poll is its session
$ curl -s https://agent.example.com:8443/oauth2/token \
    -d grant_type=urn:ietf:params:oauth:grant-type:device_code -d device_code=… -d client_id=my-cli
{"access_token":"agentd_at_…","token_type":"Bearer","expires_in":28800,"scope":"user"}
```

**An approval names the person.** `auth.device.approve` requires `as`, a
lowercase name (`^[a-z0-9][a-z0-9._-]{0,63}$`), and the session's principal is
`user:<name>`. Every session approved under one name is **one principal**: they
share what that name owns — tasks, runs, subagents, conversations — its status
scope and its rate bucket, so a person's history survives signing in again and
token expiry. The flip side is stated in the answer: approving a name that was
approved before sets `existing: true`, because reusing a name for a different
person hands that person the name's history. A name any configured
principal rule declares now is refused, and so is one a `user`-role rule ever
declared, while the store remembers it — as are reserved names such as
`operator`.

**Scopes.** A session is a `user` unless the operator explicitly approves
`scope: operator`, which `a2a.device_grant.scopes` must list; then the principal
is `operator` and the name is recorded. An approver may narrow a request,
never widen it, and `agent` is never grantable. `auth.device.pending` lists
what waits and `auth.device.deny` refuses it.

**Tokens.** A session token is `agentd_at_` plus 256 random bits, opaque, and
held only as its SHA-256, in memory: a restart revokes every session. There are
no refresh tokens; a client signs in again when `token_ttl` runs out. No
configured secret may start with `agentd_at_`.

**Revocation** is per session, by its **sid** (`ds_…`), which every audit line
of that caller carries: `auth.sessions` lists the sessions, and
`auth.sessions.revoke` ends one `{sid}`, every session of a name `{name}`, or
all `{all: true}`. `POST /oauth2/revoke` ends the session whose token it is
given. Revocation reaches what the session already opened: its streams close
and its blocking waits answer `401` within a tenth of a second, and the
observation feed says goodbye with reason `revoked` — by sid, so a sibling
session of the same name keeps working. Revoking never deletes what the
principal owns.

**`/oauth2/*` is the authorization server, not A2A.** Its endpoints take
`application/x-www-form-urlencoded` bodies (at most 4 KiB; unknown parameters
ignored, a repeated one refused), answer OAuth JSON with `Cache-Control:
no-store`, need no `A2A-Version`, and sit behind the same origin gate as
`POST /`. Without the device grant they are not routes at all (with the one
exception below). Per source, a client may ask for 5 codes and then one per
12 s, and may hold 4 waiting at once; codes, tokens and user codes are never
logged.

### The launch grant

`agentd tui` and `agentd ui` sign their client in without any credential of the
daemon's ever reaching it (see [interface.md](interface.md#launcher)). While such
a launch is installed in the daemon's process, `/oauth2/token` also accepts the
extension grant type **`https://agentd.dev/oauth/grant-type/launch`** (RFC 6749
§4.5):

- with a **`code`** the launcher minted in-process and handed its own client —
  single-use, valid for 60 seconds, bound to the client and, for the web UI, to
  the launched origin; or
- with a **`request_code`** from `POST /oauth2/launch_authorization`, which a
  browser tab of the launched UI makes and the person at the launcher's
  terminal approves by typing the code the tab shows.

Both are redeemed **only from a loopback peer**, and either buys an operator
session (`ls_…`; eight hours for a browser tab, the launcher's lifetime for the
TUI), listed and revoked like any other. A failed presentation counts against
its source, but a live code or an approved request is always honoured.

The grant is **not declared on the card**, and adds no scheme and changes no
posture: nothing outside the launcher's process can mint a code or approve a
request — there is no op, route or config key that does — so a card that
advertised it would advertise something no reader of the card could use. When
the device grant also serves RFC 8414 metadata, it lists the launch grant type
while a launch is installed.

### Failed credentials and the source limiter

A presented credential that fails — a bearer or session token that names nobody,
a certificate no rule gives a role — counts against its **source** (an IPv4
address, or an IPv6 /64): 20 failures, forgiven one every 3 seconds. Once a
source is over, every request from it that presents a bearer is refused `429`
*before* the bearer is checked: a limiter that still checked each guess would
slow nobody down, only turn a wrong guess's `401` into a `429` while a right one
sailed through.

What is never counted: an origin refusal (a web page could otherwise lock the
local console out), a request that presents nothing, and the refusal of a caller
who did authenticate. What is never refused by it: a request that presents
nothing (the implicit operator, an `any` rule), a client certificate, which is
proven in the handshake and cannot be guessed, and a **live session token** —
the launched console and every signed-in device keep working through a flood,
because a session token is 256 bits agentd minted, so answering it "valid, or
`429`" tells a guesser nothing. The residual is a client presenting a
*configured* bearer (`a2a.bearer`, a `bearer_ref` rule) that shares a source
with a guesser — behind one NAT, or on 127.0.0.1 — which waits until the source
drains.

Refusals any stranger can provoke for free are logged once per source and reason
per minute, with a count of what the window left out.

### Reloading principals

`a2a.principals` is reloadable, and the rules and the posture they imply are one
value swapped at once: after a reload, the next request is resolved under the
new rules *and* the new posture, and the next card read declares it. A SIGHUP
that adds the first rule to a no-auth loopback daemon ends the implicit operator
and adds the scheme to the card in the same instant. `a2a.bearer`, `a2a.tls.*`,
`a2a.listen` and `a2a.device_grant` are restart-only.

## Tasks

### Sending

A `SendMessage` params is the specification's SendMessageRequest; fields it does
not define are ignored. The message is a user's: `role` must be `ROLE_USER`.

- **No `taskId`** starts a new task, with an id the server mints.
- **A `taskId`** continues that task — the way to answer a question the task is
  waiting on. A task the caller cannot see is `-32001`; a terminal one is
  `-32004` "it accepts no further messages". A command never names a `taskId`.
- **Parts**: text is accepted, and a DataPart without the `agentd` key is read
  as JSON. A raw or url part refuses the whole message with `-32005`.
- **Blocking.** A unary send waits for the task to settle — a terminal state,
  or `input-required` — unless it sets `configuration.returnImmediately`.
- **While draining**, a send is `-32603` with reason `DRAINING`.

### Listing

`ListTasks` returns the caller's tasks — only its own for a non-operator, so
`totalSize` counts only what the caller may know exists:

- filters: `contextId` (the caller's own name for it), `status`, and
  `statusTimestampAfter` (inclusive);
- newest status first; `pageSize` defaults to 50 and must be 1–100; `pageToken`
  is opaque, and one this server did not issue is `-32602`;
- `historyLength` unset omits the history, `n` keeps the newest `n`;
  artifacts ride along only with `includeArtifacts: true`;
- the result always carries `tasks`, `nextPageToken`, `pageSize` and
  `totalSize` — the four fields A2A marks required, present even on an empty
  page (`[]`, `0`) — and **`nextPageToken` is `""` on the last page**, never
  absent, so "no token" is never readable as "there is more".

A filter the server cannot honour is refused with `-32602` rather than ignored.
`ListTaskPushNotificationConfigs` pages the same way on the wire (`pageSize`
1–100, default 50; `nextPageToken` always present, `""` last).

How long a finished task is kept is `store.retention.tasks`; unset keeps them.

### Streams and reconnection

`SendStreamingMessage` answers `text/event-stream`: a Task frame, then status and
artifact updates, closing at a terminal or interrupted state. A read op answers
with exactly one frame carrying its Message, then closes. Any refusal before the
first frame is plain JSON with its status, never an error inside a `200` stream.

`SubscribeToTask` follows one task the caller may see. An unknown or unseen task
is `-32001` as JSON, and no stream is opened. A task that already finished is
`-32004` — unless the request carries **`Last-Event-ID`**: that is SSE
reconnection of a stream the caller already held, and the stored events are
replayed from that point. It is the transport's reconnection rule, not a
protocol extension, and the usual reason for it is that the task finished while
the caller was away.

### Rate limits

A rule's `quotas.rate` (`"<burst>/<per>s"`, e.g. `"20/60s"`) admits one token per
request from that principal, after authentication; a device session's rate is
`a2a.device_grant.rate`, one bucket per name. A refusal is `429` with
`Retry-After` and `-32603` / `RATE_LIMITED`, and nothing reaches the runtime —
no task is created, nothing is charged. Operators are exempt.

### Asking a human

When the agent needs a person — the `ask_human` tool, a workflow `human` step, a
`security.policies` gate — the task flips to **`input-required`** with the
question as its status message. That is core A2A: a blocking `SendMessage`
returns on it, a stream or a push delivers it, and a `SendMessage` naming that
`taskId` answers it. A caller who owns the asking work is always asked there.
Work no caller owns (a schedule, a webhook, a subagent) is asked on the listener
only with `agent.ask_human_unowned: gate`; otherwise `agent.ask_human_fallback`
applies. A policy gate is answered with a decision, `approve` or `deny`, and
approving runs the call it held.

### Being told instead of watching

Streaming assumes the caller can hold a connection open for as long as the work
takes. A caller that cannot — a serverless function, a queue consumer —
registers a webhook with `CreateTaskPushNotificationConfig`, and agentd POSTs
each of that task's updates to it:

```yaml
a2a:
  push:
    enabled: true         # default OFF
    allow_private: false  # default OFF
```

Both defaults are off because **the URL comes from the caller**. Every delivery
is an outbound request to an address a *peer* chose, which is the shape of an
SSRF: pointed at a cloud metadata endpoint, agentd fetches credentials on the
caller's behalf. So `enabled` says you are willing to make that request at all,
and `allow_private` — a separate, larger decision — says you are willing to make
it to a private or loopback address. The URL must be `https` (plaintext only to
loopback), and it is checked twice: at registration, where the caller is present
to be told why, and again at delivery, because a name can resolve somewhere new
in between. With push off, registration is `-32003`.

Each delivery is a `POST` with `content-type: application/a2a+json`, whose body
is a StreamResponse carrying the Task — status and artifacts, as a streaming
caller would see them — so one handler serves both ways of being told. It
carries no `history` and no annotations: the receiver is told that the task
moved, not handed the conversation, which a `GetTask` reads.

- `authentication: {scheme, credentials}` in the config is presented as
  `Authorization: <scheme> <credentials>`. It is never read back: reads echo the
  scheme only.
- `token`, when set, travels as **`X-A2A-Notification-Token`** — an interop
  header the official a2a-python 1.x receiver reads; it is not part of A2A 1.0,
  which delivers credentials through `authentication`.

Delivery is best-effort by design: a webhook that is down must not fail the task
it was reporting on. Failures are logged as `a2a.push.failed` and dropped.

## One agent driving another

Composition needs no new protocol: the channel a parent dials is the channel a
worker serves. Deploy a worker that exposes A2A, and point a parent at it as a
peer:

```yaml
# the parent
a2a:
  peers:
    - name: reviewer
      endpoint: https://reviewer.internal:8443
      auth: { kind: static, token: "{{secret:REVIEWER_TOKEN}}" }
```

A workflow's `a2a.delegate` step hands `reviewer` an objective and waits for its
result; `a2a.send` notifies it and moves on. The worker is an ordinary agentd:
its own instruction, its own tools, its own budget — and its own fence.

A delegation is several requests, each carrying `A2A-Version: 1.0` and the
peer's credential:

1. **Read the card** at `<origin>/.well-known/agent-card.json`. It is cached per
   origin and credential for the response's `max-age`, at most five minutes
   (one minute when it names none; at most five seconds for a peer on this host,
   whose port or socket may belong to a different process by the next
   delegation). A card read under an AWS SigV4 signer or an mTLS client identity
   is not cached, and a delegation that fails drops the entry it used.
2. **Take the card's terms, or refuse them.** agentd selects a `JSONRPC`
   interface at A2A 1.0 (its unix binding, for a `unix://` peer): the one whose
   URL is the configured endpoint, else the first. It echoes that interface's
   `tenant` on every request. A peer with no such interface is refused — so an
   A2A 0.3 peer is refused — and so is a peer that requires an extension agentd
   does not implement.
3. **Send.** `SendStreamingMessage` when the card says the peer streams, and
   when no card is served at all; otherwise `SendMessage` with
   `returnImmediately`, then `GetTask` until the task settles. A stream refused
   with `-32004` gets one unary attempt, and no more.
4. **Return** the completed task's artifacts, or the message a peer answered
   with; anything else is an error.

**The dial always goes to the configured URL.** The card decides whether the
peer speaks this protocol and under which tenant, never where agentd connects,
so a card that names another host cannot redirect a delegation or the
credentials it carries.

### Co-located peers: the unix binding

Two instances on the same host (or the same pod) can skip TCP and TLS entirely:
one listens on a **unix domain socket**, the other names that socket as its
peer endpoint.

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

No `https://` URL can name a socket, so the card declares agentd's custom binding
[`https://agentd.dev/a2a/binding/jsonrpc-unix`](https://agentd.dev/a2a/binding/jsonrpc-unix):
the JSON-RPC binding, unchanged, over the socket — same methods, same
`A2A-Version` and `A2A-Extensions` headers, same errors. A client that cannot
dial a socket never selects it.

The kernel is the authenticator. The socket file is `0600`, and every
connection's `SO_PEERCRED` uid must be the daemon's own (or root) — a different
local user is dropped before HTTP, logged as `a2a.unix.denied`. Every connection
that passes is **the operator**, whatever headers it carries. That is strictly
stronger than loopback TCP, which any local user can dial. A unix listener
declares no scheme and issues no sessions. Webhooks never take `unix://`: they
are an external surface.

Two instances can also hold sockets in *both* directions — each listens, each
names the other as a peer — so either side can delegate at any time: the
agreement is the pair of socket paths in their configs, and the filesystem's
permissions are the contract.

## Where the display clients fit

The TUI and the web UI are ordinary A2A clients of this listener: they read the
card, send `A2A-Version`, activate only extensions the card declares, and fall
back to the core protocol when an extension is absent. Nothing about them is
privileged. They sign in the way the card says — the device grant, or the launch
grant when `agentd tui` / `agentd ui` started them. A TUI started by hand
against a no-auth loopback daemon is the implicit operator, like any local
process; a browser always signs in. See [interface.md](interface.md).

## See also

- [a2a-extensions.md](a2a-extensions.md) — the three extensions and the unix
  binding in one place: activation, errors, and how the claim is kept honest.
- [command](https://agentd.dev/a2a/ext/command),
  [events](https://agentd.dev/a2a/ext/events),
  [task-annotations](https://agentd.dev/a2a/ext/task-annotations) and
  [jsonrpc-unix](https://agentd.dev/a2a/binding/jsonrpc-unix) — the normative
  specifications, each served at its URI (the sources are `docs/ext/*.md`).
- [security.md](security.md) — the threat model, caller scopes, and what the
  listener does not protect you from.
- [mcp.md](mcp.md) — the other direction: where tools and events come from.
- [operations.md](operations.md) — driving a live daemon: drain, pause, reload.
