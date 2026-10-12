# A2A API reference

This page is the lookup reference for agentd's A2A listener: its HTTP routes,
the JSON-RPC methods, headers and envelope, the order requests are checked
in, every error, the wire rules, the Agent Card, principals, the command ops,
the sign-in endpoints, the listener settings and the limits. It is for
integrators building a client or a peer, and for operators checking exact
behaviour. [The A2A channel](a2a.md) explains the model behind it.

agentd speaks A2A 1.0 over the JSON-RPC binding. The listener needs the `a2a`
build feature, which the release binaries and image include.

## Routes

| Route | Method | Served | What it is |
|---|---|---|---|
| `/` | `POST` | always | the JSON-RPC endpoint |
| `/` | `OPTIONS` | always | the CORS preflight |
| `/.well-known/agent-card.json` | `GET`, `HEAD`, `OPTIONS` | always | the public Agent Card |
| `/oauth2/device_authorization` | `POST` | device grant on | RFC 8628 §3.1: a client asks for a code |
| `/oauth2/token` | `POST` | device grant on, or a launcher attached | RFC 6749 §3.2, RFC 8628 §3.4: the client polls for its session |
| `/oauth2/revoke` | `POST` | device grant on, or a launcher attached | RFC 7009: end a session by its token |
| `/.well-known/oauth-authorization-server` | `GET` | device grant on | RFC 8414 metadata; the issuer is the listener origin |
| `/oauth2/device` | `GET` | device grant on | the default verification page: plain text telling the person how an operator approves |
| `/oauth2/launch_authorization` | `POST` | `agentd ui` attached | a launched web UI tab asks the launcher's terminal to sign it in |

Every other path, and a route whose condition does not hold, is `404`.

## Methods

agentd answers the eleven methods A2A 1.0 defines for the JSON-RPC binding,
plus the one method its events extension declares, matched by exact name.
Every other name is `-32601` for every caller.

| Method | What it does | Who may call |
|---|---|---|
| `SendMessage` | a conversation turn, or a command op; answers a Task, or a Message for a read op | any named caller |
| `SendStreamingMessage` | the same, answered as a server-sent event stream | any named caller |
| `GetTask` | one task the caller may see, with its history | any named caller, own tasks |
| `ListTasks` | the caller's tasks, filtered and paged | any named caller, own tasks |
| `CancelTask` | cancel one task in flight | any named caller, own tasks |
| `SubscribeToTask` | follow one task's updates as a stream | any named caller, own tasks |
| `CreateTaskPushNotificationConfig` | register a webhook for one task | any named caller, own tasks |
| `GetTaskPushNotificationConfig` | read one webhook registration | any named caller, own tasks |
| `ListTaskPushNotificationConfigs` | page through one task's webhooks | any named caller, own tasks |
| `DeleteTaskPushNotificationConfig` | remove one webhook registration | any named caller, own tasks |
| `GetExtendedAgentCard` | the card for this caller | a caller named by a declared scheme |
| `agentd.events/SubscribeToEvents` | the instance-wide observation feed | any named caller; declared only while `a2a.events.enabled` is on |

The operator may call every method on every task. An `anonymous` principal
may call none. The public card is not a method; it is the document at
`/.well-known/agent-card.json`.

## Headers

| Header | Direction | Rule |
|---|---|---|
| `Content-Type` | request | `application/json`, parameters allowed; anything else is `415` with an empty body |
| `A2A-Version` | request | required on every `POST /`; `1.0` with any patch component (`1.0.3`); read from the header only, never the query string |
| `A2A-Extensions` | request | comma-separated extension URIs; may repeat; the first 32 tokens are considered, and a token over 512 bytes is ignored |
| `A2A-Extensions` | response | the extensions this request activated; absent when none was |
| `Authorization` | request | `Bearer <token>`, scheme case-insensitive |
| `Origin` | request | a browser request; must name an origin in `a2a.cors.origins` |
| `WWW-Authenticate` | response | on `401` and on a bearer caller's `403` |
| `Retry-After` | response | on `429`, and on the card's `503` |
| `traceparent`, `tracestate` | request | W3C Trace Context: the work the request starts continues this trace, one span below it. One `traceparent` at most; a malformed one is ignored. Correlation only: never read by authentication, authorization, rate limits or deduplication |
| `traceparent` | response | on an answer that names a task (`SendMessage`, `SendStreamingMessage`, `GetTask`, `SubscribeToTask`): the span the task's work is done in, which the task-annotations `traceId` and `spanId` name too |

The card, the `OPTIONS` preflights and the `/oauth2/*` endpoints take no
`A2A-Version`: they are not JSON-RPC.

**CORS.** A request carrying `Origin`, to `POST /` or any `/oauth2/*`
endpoint, is served only when that origin is listed in `a2a.cors.origins`,
compared as an origin (scheme, host, port with its default). Anything else,
`Origin: null` included, is `403` with no CORS headers, before the body is
read. A listed origin has its own origin echoed in
`Access-Control-Allow-Origin`, and the response exposes `A2A-Extensions`,
`Retry-After`, `WWW-Authenticate` and `traceparent`. The preflight allows
`Content-Type`, `Authorization`, `Last-Event-ID`, `A2A-Extensions`,
`A2A-Version`, `traceparent` and `tracestate`. The one
origin admitted unlisted is the web UI page `agentd ui` launches, for that
process only.

The public card has its own policy: readable from any origin with
`Access-Control-Allow-Origin: *`, never with credentials.

## The request envelope

The body is one JSON-RPC 2.0 request object.

- `jsonrpc` is exactly `"2.0"` and `method` a non-empty string.
- `id` is required, a string or an integer. A request without one would be a
  notification, which is executed and never answered, so a missing or `null`
  id is `-32600` "A2A requests must carry an id".
- `params`, when present, is an object; otherwise `-32602`.
- A batch (an array) is `-32600`.
- An integer beyond ±2^53 is carried as the double the protocol holds it as.

```sh
curl -s https://agent.example.com:8443/ \
  -H 'content-type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"ListTasks","params":{"pageSize":10}}'
```

## The request pipeline

Every `POST /` runs these checks in this order. Each refusal is final and is
plain `application/json`, never an SSE frame, and no refused request reaches
the runtime.

| Step | Check | Refusal |
|---|---|---|
| 1 | `Origin`, when present, is listed in `a2a.cors.origins` | `403`, empty body |
| 2 | a source over its failed-credential limit presents a bearer that is not a live session | `429`, `RATE_LIMITED` |
| 3 | `Content-Type` is `application/json` | `415`, empty body |
| 4 | who is calling, from the headers and connection ([evidence](#principals)) | `401` nobody or a failed credential; `403` a certificate no rule gives a role |
| 5 | the body is JSON | `-32700` |
| 6 | the JSON-RPC envelope | `-32600`, `-32602` |
| 7 | `A2A-Version` is 1.0 | `-32009` |
| 8 | the caller's rate quota | `429`, `RATE_LIMITED` |
| 9 | the method is in the route table | `-32601` |
| 10 | the extensions `A2A-Extensions` activates; an extension method needs its extension declared and activated | `-32601` |
| 11 | the caller may call the method; the extended card needs a declared scheme's credential | `403`; `401` |
| 12 | the params: a send's message shape, the task it names, its command envelope and the caller's reach to the op | `-32602`, `-32001`, `-32004`, `-32005`, `403` |
| 13 | the answer | |

## Errors

A peer branches on the code, never on the message, so every code means one
thing. A refusal that has a machine-readable reason carries a
`google.rpc.ErrorInfo` in `error.data`: its `reason` names the case, and its
`domain` is `a2a-protocol.org` for a reason the specification defines and
`agentd.dev` for one agentd coined. The plain protocol errors carry the code
and a message only.

| Code | HTTP | Reason | Meaning |
|---|---|---|---|
| `-32700` | 200 | | the body is not JSON |
| `-32600` | 200 | | not a JSON-RPC request: a batch, a missing `id`, a bad `jsonrpc` or `method` |
| `-32601` | 200 | | no such method |
| `-32601` | 200 | `EXTENSION_NOT_DECLARED`, `EXTENSION_NOT_ACTIVATED` | an extension method whose extension is not declared or not activated |
| `-32602` | 200 | | bad params: a malformed message, a filter the server cannot honour, a bad page size or token, a message that names a task and a different `contextId` |
| `-32602` | 200 | see [A2A extensions](a2a-extensions.md#errors-the-extensions-add) | a malformed or unknown command |
| `-32603` | 200 | `DRAINING` | the agent is draining and takes no new work |
| `-32603` | 429 | `RATE_LIMITED` | a rate quota or the failed-credential limit; `Retry-After` and the `retryAfterSeconds` metadata say how long |
| `-32001` | 200 | | no such task, or one the caller may not see: the same answer, so ids cannot be probed |
| `-32002` | 200 | | the task is already terminal and cannot be canceled |
| `-32003` | 200 | | push notifications are not enabled on this agent |
| `-32004` | 200 | `UNSUPPORTED_OPERATION` | the extended card where no scheme is declared |
| `-32004` | 200 | `INTROSPECTION_DISABLED` | an introspection op while introspection is off |
| `-32004` | 200 | | a message to a terminal task; subscribing to a task that has ended |
| `-32005` | 200 | `CONTENT_TYPE_NOT_SUPPORTED` | a raw or url part, or `acceptedOutputModes` excludes what a command answers with |
| `-32009` | 200 | `VERSION_NOT_SUPPORTED` | `A2A-Version` missing or not 1.0; metadata `supportedVersions: "1.0"` |
| `-31401` | 401 | `UNAUTHENTICATED` | who you are: no credential, or one that is invalid, expired or revoked |
| `-31403` | 403 | `PERMISSION_DENIED` | what you may do: a known principal whose role and grants do not reach the call |

`-32007` (no extended card configured) and `-32008` (a required extension
not activated) are never sent. A listener either declares a scheme and serves
the extended card or declares none and answers `-32004`, and none of agentd's
extensions is required.

**The identity codes.** `-31401` and `-31403` sit outside JSON-RPC's reserved
range so a client that branches on a specification code is never told the
wrong thing. They travel with their HTTP status so a proxy, a browser and a
plain HTTP client all see them.

| Response | `WWW-Authenticate` | `id` |
|---|---|---|
| `401`, nothing presented | `Bearer realm="agentd"` | `null` |
| `401`, a credential presented and failed | adds `error="invalid_token"` and a description | `null` |
| `403`, a certificate no rule gives a role | none | `null` |
| `403`, a method or op the caller may not call | `error="insufficient_scope"` when the caller used a bearer | the request's `id` |

A browser refused only for being a browser, on a listener where the same
request without `Origin` would have been the implicit operator, is told
"browser requests must authenticate".

**HTTP statuses outside JSON-RPC:** `403` with an empty body (an origin not
listed), `415` (not `application/json`), and on the card `304` and `503`.

## Wire format

A2A is defined in protocol buffers, and its JSON binding is proto3 JSON.
agentd's tasks, messages and cards are written by the official SDK's
canonical encoder.

- **Enums are the proto value names:** `TASK_STATE_COMPLETED`, `ROLE_USER`.
- **Timestamps are RFC 3339 strings:** `"2026-08-17T13:41:27.824Z"`.
- **Field names are `lowerCamelCase`:** `historyLength`, `nextPageToken`,
  `contextId`. A snake_case key is an unknown field, and an unknown field is
  ignored. `GetTask` names its task with `id`.
- **Every task is a Task.** `ListTasks` returns the same object as `GetTask`,
  cut to what the listing asked for, so the state is always at
  `status.state`.
- **Required fields are always present.** A listing's last page carries
  `nextPageToken: ""`, an empty listing carries `tasks: []` and
  `totalSize: 0`, and a deleted push config answers `{}`.
- **Ids are the SDK's.** Task and context ids are UUIDv7s. A command result
  artifact is `<taskId>.result`.

## Sending messages

`SendMessage` and `SendStreamingMessage` take the specification's send
request; fields it does not define are ignored.

- `message.role` must be `ROLE_USER`.
- **No `taskId`** starts a new task with an id the server mints.
- **A `taskId`** continues that task, which is how a caller answers a
  question the task is waiting on. A task the caller cannot see is `-32001`; a
  terminal one is `-32004`. A message that names a task and a different
  `contextId` is `-32602`.
- **`contextId`** is the caller's own name for a conversation. For a
  non-operator it is bound per principal to a runtime key, so two callers'
  identical names are two conversations. An operator addresses conversations
  by the runtime's keys, which the `status` op lists.
- **Parts:** text parts are accepted, and a DataPart without the `agentd` key
  is read as JSON. A raw or url part refuses the whole message with `-32005`.
  A DataPart under `agentd` is a command (see [Command ops](#command-ops)).
- **Blocking.** A unary send waits for the task to reach a terminal state or
  `input-required`, for at most 25 seconds, unless
  `configuration.returnImmediately` is set. It then answers with the task as
  it stands.
- **Inline push config.** A push config sent with the message is checked like
  a registration and attached only once the message is accepted.
- **While draining**, a send is `-32603` with reason `DRAINING`.

A read op answers with a Message and creates no task. Over
`SendStreamingMessage` it answers with exactly one frame carrying that
Message, then closes.

## Listing tasks

`ListTasks` returns the caller's own tasks; an operator sees every task.
`totalSize` counts only what the caller may know exists.

| Param | Rule |
|---|---|
| `contextId` | the caller's own name for a conversation; an operator may also use a runtime key |
| `status` | one task state |
| `statusTimestampAfter` | RFC 3339, inclusive |
| `pageSize` | unset or `0` is 50; otherwise 1 to 100; larger is `-32602` |
| `pageToken` | opaque; one this server did not issue is `-32602` |
| `historyLength` | unset omits the history; `n` keeps the newest `n` messages |
| `includeArtifacts` | artifacts are included only when `true` |

The order is newest status first. A filter the server cannot honour is
refused with `-32602`, never ignored. The result always carries `tasks`,
`nextPageToken`, `pageSize` and `totalSize`, and `nextPageToken` is `""` on
the last page.

`ListTaskPushNotificationConfigs` pages the same way: `pageSize` 1 to 100,
default 50, ordered by config id, `nextPageToken` always present and `""` on
the last page.

Finished tasks are kept until `store.retention.tasks` drops them; unset keeps
every task. A working or `input-required` task is never dropped, and a
settled task nobody has read back yet is kept for at least 30 seconds.

## Streams

- **`SendStreamingMessage`** answers `text/event-stream`: a Task frame, then
  status and artifact updates, closing when the task reaches a terminal state.
  A task waiting at a gate keeps its stream open.
- **`SubscribeToTask`** opens with the task as it stands, then each
  transition, closing at a terminal state. An unknown or unseen task is
  `-32001` and a task that has ended is `-32004`, both as plain JSON with no
  stream opened; read an ended task with `GetTask`.
- **Reconnecting** means subscribing again. Past transitions are not
  replayed, and `Last-Event-ID` is ignored.
- **A refusal before the first frame** is plain JSON with its status, never
  an error inside a `200` stream.

## Push notifications

A caller registers a webhook for one of its tasks with
`CreateTaskPushNotificationConfig`, and agentd posts each of that task's
updates to it.

| Setting | Default | Effect |
|---|---|---|
| `a2a.push.enabled` | `false` | accept registrations and deliver; off, registration is `-32003` and the card says `pushNotifications: false` |
| `a2a.push.allow_private` | `false` | permit webhook targets on private and loopback addresses |

**Registration.** The config takes `url` (required), `token` and
`authentication {scheme, credentials}`. A config registered without an `id`
gets `push-<ULID>`. The URL must be `https`, or `http` to a loopback host,
and it must pass the SSRF guard, which refuses private and loopback addresses
unless `allow_private` is on. It is checked at registration, where a refusal
is `-32602` naming the reason, and again at every delivery, dialling only the
address the guard vetted. Reads return the registration with the
`authentication` scheme only, never its credentials.

**Delivery.** Each delivery is a `POST` with
`Content-Type: application/a2a+json`, whose body is a stream response
carrying the Task: its status and artifacts, without history and without
task annotations. The receiver is told that the task moved and reads the
conversation with `GetTask`.

| Header | From |
|---|---|
| `Authorization: <scheme> <credentials>` | the registration's `authentication`; reads echo the scheme only |
| `X-A2A-Notification-Token` | the registration's `token`; an interop header the official a2a-python receiver reads |
| `traceparent`, `tracestate` | the task's trace: a span of its own per delivery, under the span the task's work is done in |

Delivery is best effort: a delivery has 10 seconds, a webhook that is down
does not fail the task, and a failed delivery is logged as `a2a.push.failed`
and dropped.

## The Agent Card

`GET /.well-known/agent-card.json` serves the public card with no credential
and no `A2A-Version`. It carries an `ETag` over the served bytes and answers
a matching `If-None-Match` with `304`, and `Cache-Control: public,
max-age=60`. When the runtime cannot produce it within 5 seconds, the answer
is `503` with `Retry-After: 5`, never a stale or partial card.

| Field | Value |
|---|---|
| `name` | `agent.name`, else `agentd` |
| `description` | `agent.description`, else a generic sentence |
| `version` | the agentd build |
| `supportedInterfaces` | one entry: the advertised URL, `protocolBinding` `JSONRPC` (or the unix binding URI on a unix listener), `protocolVersion` `1.0` |
| `capabilities.streaming` | `true` |
| `capabilities.pushNotifications` | `a2a.push.enabled` |
| `capabilities.extendedAgentCard` | `true` exactly when the listener declares an authentication scheme |
| `capabilities.extensions` | the declarations, with their public params ([A2A extensions](a2a-extensions.md)) |
| `defaultInputModes`, `defaultOutputModes` | `text/plain`, `application/json` |
| `skills` | one, `conversation` |
| `securitySchemes`, `securityRequirements` | what the listener enforces; absent when it declares nothing |

The advertised URL is `a2a.url`; without it, a concrete bind advertises
`scheme://host:port/`, and a unix listener advertises `unix://<path>`.

**Security declarations.** The card's schemes are derived from the posture
the listener enforces:

| Listener | `securitySchemes` | `securityRequirements` |
|---|---|---|
| loopback, nothing configured | none | none |
| unix socket | none | none |
| `a2a.bearer` or a `bearer_ref` rule | `bearer` | `{bearer}` |
| `a2a.device_grant.enabled` | `bearer`, `device_code` (OAuth 2.0 device flow) | `{bearer}`, `{device_code}` |
| `a2a.tls.client_ca` | `mtls`, plus `bearer` when one is configured | `{mtls}`, plus `{mtls, bearer}` |

An `any` rule adds the empty alternative `{}` on a listener without a client
CA. The `device_code` scheme's flow names `<origin>/oauth2/device_authorization`
and `<origin>/oauth2/token`, and `oauth2MetadataUrl` when the origin is
`https`.

**The extended card.** `GetExtendedAgentCard` adds a `workflow:<name>` skill
for each workflow the caller may run and narrows each extension's params to
this caller. [A2A extensions](a2a-extensions.md#the-extended-card) lists what
each extension adds.

## Principals

Every request resolves to a principal from its headers and connection,
before its body is read.

| Order | Evidence | Principal |
|---|---|---|
| 1 | a unix-socket peer with the daemon's uid, or root | `operator` |
| 2 | a bearer starting `agentd_at_` | the session's principal; unknown, expired or revoked is `401` |
| 2 | any other bearer, on a listener with a bearer mechanism | `a2a.bearer` is `operator`; a `bearer_ref` rule is its principal; neither is `401` |
| 3 | a verified client certificate | the first `san`, `sub` or `any` rule it matches; `operator` while no rule exists; else `403` |
| 4 | none of the above, and an `any` rule exists | that rule's principal |
| 5 | nothing presented, a loopback peer, a loopback-bound listener with no mechanism, no `Origin` | `operator` (the implicit operator) |
| 6 | anything else | `401` |

A bearer sent to a listener with no bearer mechanism is ignored. A presented
bearer is resolved before any certificate rule.

**Rules** (`a2a.principals[]`):

| Key | Rule |
|---|---|
| `match` | exactly one of `san`, `sub`, `bearer_ref`, `any` |
| `match.san` | a glob over the certificate's SANs and subject CN; a SPIFFE X.509-SVID's `spiffe://…` arrives as a URI SAN; needs `a2a.tls.client_ca` |
| `match.sub` | the subject CN, exactly; needs `a2a.tls.client_ca` |
| `match.bearer_ref` | a secret reference, compared in constant time; may not equal `a2a.bearer` or another rule's secret, or start with `agentd_at_` |
| `match.any` | `true`; never with `role: operator`; needs no client CA |
| `role` | `operator`, `user`, `agent` or `anonymous` (names nobody) |
| `id` | the principal's name: 1 to 128 of `A-Z a-z 0-9 . _ @ : / + -`, unique across rules; required on `bearer_ref` and `any` rules |
| `grants` | op names or patterns beyond the role's defaults |
| `labels` | attributes carried into the run, the MCP request metadata and the audit line |
| `quotas.rate` | `<burst>/<window>`, for example `20/1s` or `30/1m`; operators are exempt |
| `quotas.budget` | a token budget scoped to the principal ([Control spend with budgets](budgets.md)) |

Certificate rules are first-match-wins, in the order written, and are not
ranked by specificity: a broad rule placed early shadows every rule after it.

**Principal ids.** The operator role is always `operator`. A user or agent is
`<role>:<id>` when the rule declares `id`; otherwise, for a certificate,
`<role>:cn=<CN>` or `<role>:san=<first SAN>`. A device session is
`user:<name>`. The `=` keeps certificate-derived ids out of the declared
namespace, so a certificate whose CN equals a declared id never inherits that
principal's work.

**Grants.** A grant names an op exactly, every op (`*`), or the ops that
start with a prefix (`workflow.*`). A grant naming an operator-only op
exactly on a non-operator rule is refused at load; a pattern may cover a
family and grants only its members that are not operator-only. A
`workflow.run:<pattern>` grant scopes `workflow.run` to the matching
workflows and replaces the role's default reach for that op.

**Identity registry.** Device names and `user`-role ids share one namespace
across time, since both spell `user:<name>`. The durable store records every
claim: an approval may not use a name any rule declares or a `user` rule
ever declared, and a `user`-role id that was ever approved as a device name
is refused at startup (exit 2) and by a reload. Names compare with ASCII case
folded.

## Command ops

A command is a `SendMessage` or `SendStreamingMessage` whose message carries
one DataPart `{"agentd": {"op": "<op>", …arguments}}`, with the command
extension URI in both the `A2A-Extensions` header and `message.extensions`.
A command message names no `taskId`.

```sh
curl -s https://agent.example.com:8443/ \
  -H 'content-type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'A2A-Extensions: https://agentd.dev/a2a/ext/command' \
  -H "Authorization: Bearer $OPERATOR_TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"SendMessage","params":{"message":{"role":"ROLE_USER","messageId":"m-1","extensions":["https://agentd.dev/a2a/ext/command"],"parts":[{"data":{"agentd":{"op":"admin.drain","reason":"rolling update"}}}]}}}'
```

A `message` op answers with a Message whose DataPart is the result. A `task`
op answers with a Task whose `<taskId>.result` artifact carries the result.

| Op | Reply | Default callers | Served | Arguments |
|---|---|---|---|---|
| `status` | message | any named caller | always | none |
| `config` | message | operator | always | none |
| `workflow.run` | task | user, agent | always | `workflow`; `inputs` |
| `workflow.status` | message | user, agent | always | `run` |
| `workflow.cancel` | task | user | always | `run` |
| `workflow.signal` | task | a grant | always | `name`; `run`, `payload` |
| `subagent.send` | task | user | always | `handle`, `message` |
| `subagent.kill` | task | a grant | always | `handle`; `reason` |
| `subagent.status` | message | user | always | `handle` |
| `plan.get` | message | user | always | `id` |
| `conversation.get` | message | user | introspection on | `id`; `limit` |
| `run.get` | message | user | introspection on | `run` |
| `subagent.get` | message | user | introspection on | `handle` |
| `debug.events` | message | operator | introspection on | `after`, `level`, `limit`, `prefix` |
| `admin.drain` | task | operator | always | `reason` |
| `admin.pause` | task | operator | always | `run`, `reason` |
| `admin.resume` | task | operator | always | `run` |
| `admin.cancel` | task | operator | always | `run`; `reason` |
| `admin.set` | task | operator | always | `path`, `value` |
| `auth.device.pending` | message | operator | device grant on | none |
| `auth.device.approve` | task | operator | device grant on | `user_code`, `as`; `scope` |
| `auth.device.deny` | task | operator | device grant on | `user_code`, or `all: true` |
| `auth.sessions` | message | operator | a TCP listener | none |
| `auth.sessions.revoke` | task | operator | a TCP listener | `sid`, `name`, or `all: true` |

Arguments before a semicolon are required. "A grant" means no role reaches
the op by default and a principal needs a grant naming it. The operator
reaches every op. An operator-only op is the operator's whatever a grant
says. An op behind a closed switch is answered as an unknown op, except the
introspection ops, which answer `INTROSPECTION_DISABLED`.

- **`admin.drain`** begins a graceful drain, as `SIGTERM` does, and returns
  at once; the process exits `0` when the drain completes, within
  `lifecycle.drain_timeout`.
- **`admin.pause`** with no `run` holds the whole instance at a safe
  boundary: the listener still answers and intake still queues. With `run`,
  it pauses that run alone. **`admin.resume`** clears either.
- **`admin.set`** sets `agent.approval` or `a2a.introspection.enabled` until
  the next reload re-reads the file.
- **`auth.device.approve`**'s `as` is the person's name: lowercase,
  `^[a-z0-9][a-z0-9._-]{0,63}$`, not `operator`, `anonymous`, `unknown`,
  `launcher`, `runtime` or `system`, and not a configured principal id. The
  answer's `existing: true` says the name was approved before.

**Reserved names.** `_instance.result` and `_instance.emit` are how a child
instance reports to its parent; they are the operator's and are not listed in
`params.ops`. `ask_human` is the model's tool and is not served over A2A. No
workflow may declare a built-in op or a reserved name as its command.

**Workflow-declared commands.** A workflow whose start step is `kind: a2a`
with `command:` adds that op. A caller may send it when its grants name the
op (an operator always may) and the start's `roles:` admit its role, and the
arguments are held to the start's `schema`. The extended card lists them in
the command extension's `commands` param.

The normative op table, argument and result schemas are at
[`https://agentd.dev/a2a/ext/command`](https://agentd.dev/a2a/ext/command).
[Operate a running daemon](operations.md) shows the operator ops in use.

## Device authorization grant

With `a2a.device_grant.enabled`, the listener origin is an OAuth 2.0
authorization server. Its endpoints take
`application/x-www-form-urlencoded` bodies of at most 4 KiB (unknown
parameters ignored, a repeated one refused), answer OAuth JSON with
`Cache-Control: no-store` and sit behind the same origin gate as `POST /`.

| Endpoint | Parameters | Answer |
|---|---|---|
| `POST /oauth2/device_authorization` | `client_id` (1 to 64 of `A-Z a-z 0-9 . _ -`); `scope` (`user` or `operator`, default `user`) | `device_code`, `user_code`, `verification_uri`, `expires_in`, `interval` |
| `POST /oauth2/token` | `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code`, `client_id` | `access_token`, `token_type: Bearer`, `expires_in`, `scope` |
| `POST /oauth2/revoke` | `token` | ends the session the token names |

While a code waits, a poll answers `authorization_pending`, and polling
faster than `interval` answers `slow_down`. Other OAuth errors:
`access_denied` (denied), `expired_token` (the code lapsed), `invalid_grant`,
`invalid_request`, `invalid_scope` (a scope `a2a.device_grant.scopes` does
not offer) and `unsupported_grant_type`.

```text
{"device_code":"3f9d…6c14","expires_in":600,"interval":5,"user_code":"WJLG-DRVQ","verification_uri":"https://agent.example.com:8443/oauth2/device"}
{"access_token":"agentd_at_25f1…aa90","expires_in":28800,"scope":"user","token_type":"Bearer"}
```

**Scopes.** A session is a `user` unless the client asked for `operator`,
`a2a.device_grant.scopes` lists it, and the approver grants it. An approver
may narrow a request, never widen it. The session principal is `user:<name>`,
or `operator` for an operator session.

**Sessions.** A session token is `agentd_at_` followed by 256 random bits,
held only as its SHA-256 in memory: a restart ends every session. There are
no refresh tokens. No configured secret may start with `agentd_at_`. A
session id is `ds_` (device) or `ls_` (launch) followed by 16 hex digits, and
every audit line of the caller carries it.

**Revocation.** `auth.sessions.revoke` ends one session `{sid}`, every
session of a name `{name}`, or all `{all: true}`; `POST /oauth2/revoke` ends
the session whose token it is given. Revocation closes the session's open
streams and refuses its waiting requests within 100 ms, and the feed ends
with `goodbye` reason `revoked`, by session id, so a sibling session of the
same name keeps working. Revoking never deletes what the principal owns.

Codes, tokens and user codes are never logged.

## Launch grant

`agentd tui` and `agentd ui` sign their client in with the extension grant
type `https://agentd.dev/oauth/grant-type/launch` (RFC 6749 §4.5) at
`/oauth2/token`, while a launch is installed in the daemon's process:

- with a `code` the launcher minted and handed its client: single use, valid
  for 60 seconds, bound to the client and, for the web UI, to the launched
  origin; or
- with a `request_code` from `POST /oauth2/launch_authorization`, which a
  launched web UI tab makes and the person at the launcher's terminal
  approves by typing the code the tab shows. A request lives 2 minutes, and
  at most 16 wait at once.

Both are redeemed only from a loopback peer, and either buys an operator
session: eight hours for a browser tab, the launcher's lifetime for the TUI.
The grant is not declared on the card; the RFC 8414 metadata lists it while a
launch is installed. [Use the terminal and web UI](interface.md#launcher)
describes the launcher.

## Listener settings

Every `a2a.*` key is operator-only except `a2a.peers`: an instruction
document's `:::!config` fragment may write only the outbound peers. Every
path is also a flag (`--a2a-<path>`) and an environment variable
(`AGENTD_A2A_<PATH>`); `--listen` is short for `a2a.listen`.
[Configuration reference](configuration.md) lists every key.

| Key | Default | Reload | Meaning |
|---|---|---|---|
| `a2a.listen` | no listener | restart | `https://host:port`, `http://host:port` on loopback, or `unix:///path` |
| `a2a.url` | the bind's own URL | restart | the public origin: `scheme://host[:port]`, lowercase, no default port, `https` unless loopback |
| `a2a.tls.cert`, `a2a.tls.key` | none | restart; files re-read on change | the server identity |
| `a2a.tls.client_ca` | none | restart; file re-read on change | require a client certificate of every caller |
| `a2a.bearer` | none | restart | a secret whose presenter is the operator |
| `a2a.principals` | `[]` | reload | the principal rules |
| `a2a.cors.origins` | `[]` | reload | browser origins admitted, exactly; `*` and paths refused |
| `a2a.device_grant.enabled` | `false` | restart | serve the device grant and declare it on the card |
| `a2a.device_grant.scopes` | `[user]` | restart | what a client may request: `user`, `operator` |
| `a2a.device_grant.token_ttl` | `8h` | restart | session lifetime, `5m` to `30d` |
| `a2a.device_grant.code_ttl` | `10m` | restart | how long a code waits, `1m` to `30m` |
| `a2a.device_grant.verification_uri` | the listener's own page | restart | where the approving person is sent: `https://`, or `http://` on loopback |
| `a2a.device_grant.rate` | unlimited | restart | `<burst>/<window>` per signed-in name |
| `a2a.events.enabled` | `false` | restart | declare the events extension and serve the feed |
| `a2a.introspection.enabled` | `false` | reload; `admin.set` | serve the introspection ops and audit events on the feed |
| `a2a.push.enabled` | `false` | reload | accept webhook registrations and deliver |
| `a2a.push.allow_private` | `false` | reload | permit private and loopback webhook targets |
| `a2a.peers` | `[]` | reload | agents this instance dials |

A rotated certificate is served on the next connection without a restart; a
bad intermediate write keeps the last good identity. Whether client
certificates are required is fixed at startup.

**Peers** (`a2a.peers[]`):

| Key | Rule |
|---|---|
| `name` | required, unique; `A-Z a-z 0-9 _ -` |
| `endpoint` | required: `https://host[:port]`, `http://` to a loopback host, or `unix:///path` |
| `auth` | a credential provider: `kind: static`, `oauth2` or `spiffe` ([Outbound credentials](authentication.md)); `aws` is refused |
| `headers` | extra request headers; a credential-shaped header must be a secret reference |
| `client_cert`, `client_key` | an mTLS client identity; set together |
| `service` | a `services:` entry of `kind: peer` that supplies endpoint, auth and tags |

`security.tls_ca` adds a trust anchor for every outbound TLS connection,
peers included.

## Load-time refusals

The load refuses these with exit code 2, before anything starts.

| Refused | Why |
|---|---|
| `https://` listener without `a2a.tls.cert` and `a2a.tls.key` | nothing to serve TLS with |
| non-loopback bind without `a2a.bearer` or `a2a.tls.client_ca` | no open control plane |
| `http://` on a non-loopback bind | plaintext is loopback only |
| wildcard bind (`0.0.0.0`, `::`) without `a2a.url` | no address a caller can dial |
| `a2a.url` not a canonical origin | the card and the OAuth issuer live at the origin root |
| `a2a.tls` on a `unix://` listener | the kernel authenticates the peer |
| `a2a.url`, `cors`, `device_grant`, `events` or `introspection` without `a2a.listen` | they are served on the listener |
| `a2a.listen`, `webhooks.listen` or `a2a.peers` in a build without the `a2a` feature | nothing could serve or dial them |
| `store.kind: none` on a long-lived instance | a daemon must keep its tasks |
| `*` or a path in `a2a.cors.origins` | origins are listed exactly |
| device grant without an operator credential | nobody could approve a code |
| device grant beside `a2a.tls.client_ca` | a device session has no certificate |
| device grant on a `unix://` listener | the kernel already vouches for every peer |
| a principal rule matching a certificate (`san`, `sub`) without `a2a.tls.client_ca` | the listener never asks for a certificate |
| a rule with no matcher, or more than one | a rule matches one way |
| `any` with `role: operator` | anyone cannot be the operator |
| a `bearer_ref` or `any` rule without `id` | the evidence names nobody |
| an invalid or duplicate `id` | an id names one principal |
| a grant naming an operator-only op on a non-operator rule | the floor answers to the role |
| a malformed `quotas.rate` | |
| a duplicate peer name, a bad peer endpoint, `client_cert` without `client_key`, `auth.kind: aws` on a peer | |
| a workflow command that names a built-in op | it would shadow the operator's control |

## Limits and constants

| What | Value |
|---|---|
| Blocking `SendMessage` wait | 25 s, then the task as it stands |
| A unary request's wait on the runtime | 120 s |
| Card build | 5 s, then `503` with `Retry-After: 5` |
| Card caching | `Cache-Control: public, max-age=60`, with an `ETag` |
| Observation feed stream | held 10 minutes, then `goodbye` with reason `deadline` |
| Observation feed replay ring | 1024 events |
| Task history | the newest 64 messages, at most 256 KiB together |
| History on a feed `task` event | the newest 4 messages, at most 32 KiB |
| List page size | default 50, maximum 100 |
| `A2A-Extensions` | 32 tokens considered, 512 bytes each |
| Failed credentials per source | 20, one forgiven every 3 s; 4096 sources tracked; a source is an IPv4 address or an IPv6 /64 |
| Refusal log lines | one per source and reason per 60 s; 20 at once across sources, then one per second |
| Revocation reaching open requests | 100 ms |
| Device codes per source | 5, then one per 12 s |
| Token polls per source | 30, then one per second |
| Waiting device codes | 4 per source, 16 per network (an IPv6 /48), 64 in all |
| Device poll interval | 5 s; `slow_down` adds 5 s |
| User code | 8 letters from `BCDFGHJKLMNPQRSTVWXZ`, shown as `XXXX-XXXX` |
| OAuth form body | 4 KiB |
| Launch code | single use, 60 s |
| Launch request | 2 min; at most 16 waiting |
| Push delivery | 10 s |
| Settled task kept unread | at least 30 s |
| Task retention sweep | every second |
| Peer card cache | the card's `max-age`, at most 5 min; 1 min when it names none; 5 s for a peer on this host |
| Peer card fetch | 5 s |
| Peer request | 30 s per round trip; `GetTask` polls 100 ms apart, bounded by the step's timeout |

A card read under a client certificate or a per-request signature is not
cached, and a delegation that fails drops the card it used.

## Log events

| Event | When |
|---|---|
| `a2a.listen` | the listener bound: authority, advertised URL, TLS, mTLS, device grant, events, introspection, implicit operator |
| `a2a.denied` | a request was refused: principal, rule, method, op, reason, status, session id; at most one line per source and reason per minute, with a `suppressed` count |
| `a2a.unix.denied` | a unix-socket connection from another uid was dropped |
| `a2a.push.registered` | a webhook was registered |
| `a2a.push.failed` | a webhook delivery failed and was dropped |
| `a2a.delegate`, `a2a.send` | a workflow step dialled a peer |

The `a2a.denied` reasons are `auth_failures_limited`, `unauthenticated`,
`invalid_credential`, `no_role`, `rate_limited` and `not_permitted`.
[Events and metrics reference](telemetry-reference.md) lists every event.

## Probes

These read a configuration without starting anything:

| Command | Prints |
|---|---|
| `agentd --capabilities --config <file>` | the manifest; its `a2a` block has `methods`, `command_ops`, `extensions`, `events`, `introspection`, `url`, `cors_origins` and `auth` (`bearer`, `device`, `mtls`, `implicit_operator`, `required`) |
| `agentd --extensions` | every extension and binding URI agentd publishes |
| `agentd --extension-schema <name>` | one extension's schema bundle |
| `agentd --validate-config --config <file>` | the load verdict; exit 0 or 2 |

## Next steps

- [A2A extensions](a2a-extensions.md): activation, the extended card and the
  extension specifications.
- [Connect agents over A2A](connect-agents.md): configurations that use
  these settings.
- [Operate a running daemon](operations.md): the operator ops in practice.
- [CLI, environment and exit codes](cli.md): the flags and exit codes.
