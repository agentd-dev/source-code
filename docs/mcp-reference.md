# MCP support reference

This page lists exactly what agentd's MCP client supports: the protocol
revisions, how a connection is negotiated, the capabilities it uses and the
ones it refuses, its timeouts and retry schedules, every MCP log event, the
MCP Events limits, agentd's own tools, and the known limits. It is for
integrators building servers that agentd connects to and for operators
reading its logs. The concepts are in [MCP: tools and context](mcp.md); the
configuration task is [Connect MCP servers](connect-mcp.md).

## Revisions

agentd speaks two MCP revisions over Streamable HTTP and picks one per
server. The SDK underneath is [`rmcp`](https://crates.io/crates/rmcp), the
official Rust SDK.

| | 2026-07-28 | 2025-11-25 |
|---|---|---|
| First request | `server/discover` | `initialize`, then `notifications/initialized` |
| Session | none | `Mcp-Session-Id` from the `initialize` answer, sent on every request; a lost one is re-dialed |
| Per-request `_meta` | `io.modelcontextprotocol/protocolVersion`, `…/clientInfo`, `…/clientCapabilities` on every request | none; agreed once in the handshake |
| Headers the client sets | `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` on a call, read or prompt, `Mcp-Param-*` for annotated tool arguments | `MCP-Protocol-Version` |
| Tool argument headers | tools are listed once on connect, so every call, a worker's included, carries its `Mcp-Param-*` headers | not used |
| Server push | the `subscriptions/listen` stream only | a `GET` event stream |
| Resource subscriptions | one listen per connection, its filter widened and narrowed | `resources/subscribe` and `resources/unsubscribe` per URI |
| `list_changed` | a listen that asks for it, held from connect | on the `GET` stream |
| Cancellation | closing the request's stream | `notifications/cancelled` |
| Server-to-client requests | inside the call's answer (`input_required`), answered, then called again | requests on the stream, answered |
| Response cache | off | off |

A server that lists only revisions older than 2025-11-25 is reached through
`initialize` at the newest revision it lists that the SDK knows.

`server/discover` results that leave out `resultType`, `ttlMs` or
`cacheScope` are accepted and read as `complete`, `0` and `private`. An
`instructions` member is not read.

## Negotiation

Every connection probes with `server/discover`. What the probe gets decides
the next step:

| The probe got | agentd |
|---|---|
| a `-32022` whose `data.supported` names 2026-07-28 | asks again at that revision |
| a `-32022`, or a DiscoverResult, listing only older revisions | sends `initialize` at the newest of them it knows |
| a `-32022` listing nothing | sends `initialize` at 2025-11-25 |
| a `-32020` or `-32021` (a modern server refusing agentd's request), a `401`, a server never reached, or a version list with nothing agentd can offer | fails the connect, naming the refusal |
| anything else: `-32601`, a `403`, another `4xx` or `5xx` with or without a body, `{}`, a `202`, silence until the connection's timeout | sends `initialize` at 2025-11-25 |

- The probe is bounded by the connection's timeout. A fallback that is
  refused too names both refusals, and no request is sent twice.
- A re-dial, and the dial of a turn worker or subagent, start the way the
  connection they follow reached the server.
- A refused `initialize` is followed by `server/discover` when the server
  answered with a JSON-RPC error or a `4xx` other than `401`. A `5xx`,
  silence or an unreachable server is not re-probed.

What agentd says about itself:

| Connection | `clientInfo` | Client capabilities |
|---|---|---|
| The supervisor's (startup, reload, re-dial, a server joining) | `name: agentd`, `version`: the agentd version, `title`: the instance name | none |
| A turn worker's or subagent's | `name: agentd`, `version` | `elicitation` |

### Wire examples

The probe of a supervisor connection, and a server that speaks 2026-07-28:

```jsonc
// agentd → server
// headers: MCP-Protocol-Version: 2026-07-28, Mcp-Method: server/discover
{"jsonrpc":"2.0","id":0,"method":"server/discover","params":{"_meta":{
  "io.modelcontextprotocol/protocolVersion":"2026-07-28",
  "io.modelcontextprotocol/clientInfo":{"name":"agentd","version":"<agentd version>","title":"<instance>"},
  "io.modelcontextprotocol/clientCapabilities":{}
}}}
// server → agentd
{"jsonrpc":"2.0","id":0,"result":{
  "resultType":"complete",
  "supportedVersions":["2026-07-28"],
  "capabilities":{"resources":{"subscribe":true,"listChanged":true},"tools":{"listChanged":true}},
  "ttlMs":0,"cacheScope":"private",
  "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"<server>","version":"<server version>"}}
}}
```

The handshake sent to a server that predates `server/discover`:

```jsonc
// agentd → server (no MCP-Protocol-Version header on this request)
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2025-11-25",
  "capabilities":{},
  "clientInfo":{"name":"agentd","version":"<agentd version>","title":"<instance>"}
}}
// server → agentd, with the header Mcp-Session-Id: <session>
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},
  "serverInfo":{"name":"<server>","version":"<server version>"}}}
// agentd → server
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

A tool call and its two kinds of failure:

```jsonc
// agentd → server
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
  "name":"get_weather","arguments":{"location":"NYC"},
  "_meta":{"agent/run_id":"<run id>","agent/instance":"<instance>","traceparent":"<w3c traceparent>"}}}
// server → agentd: the tool ran and failed (a successful JSON-RPC response)
{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"unknown city"}],"isError":true}}
// server → agentd: a protocol fault
{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"unknown tool: get_weather"}}
```

## Capabilities

| Feature | Support | How agentd uses it |
|---|---|---|
| `tools/list` | yes | Every page, following `nextCursor` to the end; each page is bounded by the timeout |
| `tools/call` | yes | Turns, subagents, `mcp.tool` and `tool` steps, mapped contracts |
| `notifications/tools/list_changed` | yes | Logged as `mcp.tools_changed`; the supervisor's catalogue is rebuilt at the next reload; a subagent session re-lists at its next turn boundary |
| `resources/list` | yes | A subagent's resource note (at most 50 URIs, first server wins); the `mcp.resource` step's `list` |
| `resources/read` | yes | Notify-then-read, the instruction, `resource.read` in a subagent, the `mcp.resource` step's `read` |
| `resources/templates/list` | yes | The `mcp.resource` step's `templates`; a template is not subscribable |
| `resources/subscribe`, `subscriptions/listen` | yes | `subscribe` starts, `wait {on: resource}`, the instruction; concrete URIs only, and only when the server advertises `resources.subscribe` |
| `notifications/resources/list_changed` | received | Not acted on |
| `prompts/list`, `prompts/get` | yes | Skill discovery; the `mcp.resource` step's `prompt` |
| `completion/complete` | yes | The `mcp.resource` step's `complete` |
| Elicitation | yes | On worker and subagent connections: an `ask_human` gate, answered within 300 s or `cancel` |
| Sampling, roots, tasks | no | Not declared; a server request for them is dropped |
| `notifications/message`, `notifications/progress` | received | Not acted on |
| Response cache (`ttlMs`, `cacheScope`) | off | Every read and list goes to the server |
| MCP Events (SEP-3415, draft) | yes, at 2026-07-28 only | Bindings and grants; the server declares `io.modelcontextprotocol/events`, or a top-level `events` capability |

agentd refuses at load any configured header the client sets itself:
`MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name`, every `Mcp-Param-*`,
`Mcp-Session-Id` and `Last-Event-ID`, in `mcp.servers[].headers` and in an
MCP service's `headers`.

## Request metadata

Every `tools/call` carries keys in `_meta` that let a server correlate and
deduplicate:

| Key | Sent on |
|---|---|
| `agent/run_id` | every call |
| `agent/instance` | every call |
| `traceparent` | every request of any kind: a span of its own in the trace of the unit of work it is made for (an `mcp.resource` step's lists included, one span per page), or the process trace for the connection's own handshake, catalogue lists, subscriptions and MCP Events requests (W3C trace context, SEP-414) |
| `tracestate` | beside `traceparent`, when the trace has one |
| `agent/ctx` | calls made during a conversation turn |
| `agent/idempotency_key` | calls from a turn, from an `mcp.tool` step (stable across the step's retries, or its `idempotency.value`), and from the supervisor's tool registry |
| `agent/attempt`, `agent/run` | calls from an `mcp.tool` step |
| `agent/acting_for`, `agent/labels` | calls from the supervisor's tool registry (a workflow `tool` step, a mapped contract): the principal the work is for, or the instance's autonomous identity |

## Timeouts

| Bound | Value | Applies to |
|---|---|---|
| Connection timeout | `mcp.servers[].timeout`, else `mcp.default_timeout`, default 60 s | each silence on a supervisor connection: the handshake, `tools/list`, registry-routed calls |
| Child connection timeout | 60 s, fixed | each silence on a turn worker's or subagent's connection |
| Turn tool call | the turn's remaining deadline, at most 600 s | the whole of one call the model makes |
| `mcp.tool` step | the step's `timeout`, else `limits.step_timeout`, default 600 s | the whole of the call |
| Management call | 2 s | a subscribe, an unsubscribe or a re-dial handshake made from the supervisor's loop |
| Listen idle | 10 min, never less than the connection timeout | a quiet listen is closed and opened again (`mcp.listen.ended`, `reason: idle`) |
| Elicitation | 300 s | a person answering a server's question; then `cancel` |
| Answer POST | 5 s, set by the SDK | sending agentd's answer to a server request |
| `events/*` request | 30 s | each MCP Events request |

A timeout bounds silence: a call that streams progress is not cut off by
the connection timeout. A call abandoned at its bound holds up no other call
on the connection.

At shutdown agentd closes its connections. There is no server process to
signal, and the close counts inside `lifecycle.drain_timeout` (default
25 s). MCP Events subscriptions are not released at shutdown; the next
start arms them again from their stored cursors.

## Retry schedules

| What | Schedule |
|---|---|
| Dialing a server that was down at startup | 1 s after the first failure, doubling to 60 s |
| Re-dialing a server that lost its session | 1 s, doubling to 30 s; one server per loop pass |
| Asking again for a failed subscription | 1 s, doubling to 30 s |
| Listening again after a listen ended or was unavailable | 250 ms, doubling to 30 s |
| MCP Events: a failed request (transport, `5xx`, timeout) | 1 s, doubling to 5 min, with ±10% jitter |
| MCP Events: `-32025` ResourceExhausted | the same, up to 15 min |
| MCP Events: listing a server's event types again | every 15 min, after a reload, or on its `list_changed` |
| MCP Events: polling | the server's `nextPollMs`, at least 1 s, 30 s when the server names none, at most 15 min, with ±10% jitter |

A poll asks for at most 50 occurrences. While the server says `hasMore` and
each page held something new, agentd polls again at once, ten times in a row
at most.

## Log events

Every event is one NDJSON line on stderr. Fields beyond the common envelope
are listed. [Events and metrics reference](telemetry-reference.md) documents
the envelope.

### Connections

| Event | Level | Fields | When |
|---|---|---|---|
| `mcp.connect` | info | `server`, `protocol`, `tools`, `reason` | A connection is made. `tools` is the advertised count at startup and on joining. `reason` is `joined`, `reload` or `session_lost` when it is not the first dial |
| `mcp.connect.fail` | warn | `server`, `err`, `reason` | A dial failed. Also counted in `agent_mcp_connect_failures_total` |
| `mcp.disconnect` | info, warn | `server`, `reason` | A connection was closed: `reload` or `reload refused` (info), or `session_lost` (warn) |
| `mcp.joined` | info | `server` | A server that was down at startup joined; its subscriptions and bindings are restored |
| `mcp.join.refused` | warn | `server`, `errors` | A joining server answered, but the catalogue with its tools could not be built; it is dialed again later |
| `mcp.tool.dropped` | warn | `server`, `tool`, `reason` | The SDK left a tool out of a 2026-07-28 listing because of an invalid `x-mcp-header` annotation |
| `mcp.tools_changed` | info | `server`, `note` | The server said its tool list changed |
| `mcp.tools_refreshed` | info | `tools` | A subagent session re-listed its tools; `tools` is the new catalogue size |
| `mcp.tools_refresh_failed` | warn | `err` | A subagent's re-list failed; its current catalogue stays |
| `mcp.unsubscribed` | info | `server`, `uri`, `reason` | A subscription nothing wants any more was released (`reason: unowned`) |
| `mcp.unsubscribe.fail` | warn | `server`, `uri`, `reason`, `err` | Releasing it failed; it is tried again later |

### Listens (2026-07-28)

| Event | Level | Fields | When |
|---|---|---|---|
| `mcp.listen.ended` | info | `server`, `reason: idle`, `detail`, `retry_ms` | A quiet listen reached its idle limit and is reopened |
| `mcp.listen.ended` | warn | `server`, `reason`, `retry_ms` | A listen ended for any other reason: the server closed it, or the client fell behind |
| `mcp.listen.resumed` | info | `server` | A listen is held again |
| `mcp.listen.narrowed` | warn | `server`, `dropped` | The server acknowledged a listen without some URIs; they are asked for again |
| `mcp.listen.unavailable` | warn | `server`, `reason`, `retry_ms` | A listen failed; logged once, retried on the listen schedule |
| `mcp.listen.flags_refused` | warn | `server`, `reason` | The server refused the `list_changed` flags; the listen continues without them |

A listen the server refuses outright (`-32601` or `-32602`) is logged once
and not asked for again; the connection works without its `list_changed`.

### Subscriptions

| Event | Level | Fields | When |
|---|---|---|---|
| `start.subscribe.armed` | info | `workflow`, `node`, `server`, `uri`, `reason` | A `subscribe` start is subscribed |
| `start.subscribe.unsupported` | error | `workflow`, `node`, `server`, `uri`, `err` | The server offers no subscriptions; the start can never fire |
| `start.subscribe.fail` | warn | `workflow`, `node`, `server`, `uri`, `err` | A subscribe failed in a way that may pass; asked again on the backoff |
| `start.subscribe.no_server` | warn | `workflow`, `node`, `server` | The start's server is not connected |
| `wait.resubscribed` | info | `server`, `uri`, `reason` | A parked `wait {on: resource}` is subscribed again |
| `wait.resubscribe.fail` | warn | `server`, `uri`, `reason`, `err` | Subscribing it again failed in a way that may pass; retried |
| `wait.resubscribe.fail` | error | `server`, `uri`, `reason`, `err`, `failed` or `parked` | The waits failed, or stay parked because the server is configured but not connected |
| `instruction.subscribe.unsupported` | info | `server`, `uri` | The instruction's server offers no subscriptions; the instruction is followed by its `refresh` alone |
| `instruction.subscribe.fail` | warn | `server`, `uri`, `err` | Subscribing the instruction failed; retried |
| `instruction.event` | info | `server`, `event_type` | An MCP Events occurrence re-read the instruction |

### Skills

| Event | Level | Fields | When |
|---|---|---|---|
| `skills.discovered` | info | `server`, `count`, `skills` | Skills found on a source |
| `skills.source.unavailable` | warn | `server` | A `skills.sources` server is not connected |

### MCP Events

| Event | Level | Fields | When |
|---|---|---|---|
| `mcp.events.listed` | info | `server`, `types` | A server's event types were listed |
| `mcp.events.list.fail` | warn | `server`, `err`, `retry_ms` | Listing failed; retried |
| `mcp.events.list_changed` | info | `server` | The server said its event types changed; listed again |
| `mcp.events.armed` | info | `server`, `event_type`, `mode`, `cursor_present` | A binding is polling, or its webhook subscription is live |
| `mcp.events.arm.fail` | warn, error | `server`, `event_type`, `err`, `retry_ms` | Arming a binding failed; retried |
| `mcp.events.refused` | error | `server`, `event_type`, `reason`, `detail` | The binding cannot work; see [refusals](#mcp-events-refusals) |
| `mcp.events.poll.fail` | warn, error | `server`, `event_type`, `code`, `err`, `retry_ms` | A poll failed; retried. Error level for `-32020` |
| `mcp.events.paused` | warn | `server`, `event_type`, `cause`, `detail` | Intake is held: `cause` is `pressure`, `rate` or `store_degraded`; the cursor is kept |
| `mcp.events.resumed` | info | `server`, `event_type`, `cause` | A paused binding resumed |
| `mcp.events.gap` | warn | `server`, `event_type`, `cause` | A loss the wire shows: `truncated`, `no_replay` or `schema_changed` |
| `mcp.events.rejected` | warn | `server`, `event_type`, `event_id`, `cause` | An occurrence was not appended: `oversize` or `malformed`; the cursor moves past it |
| `mcp.events.schema_mismatch` | warn | `server`, `event_type`, `event_id` | Appended with `schema_ok: false` |
| `mcp.events.subscribed` | info | `server`, `event_type`, `owner`, `reason`, `subscription` | A runtime subscription was made; also audited |
| `mcp.events.unsubscribed` | info | `server`, `event_type`, `owner`, `reason`, `subscription` | A runtime subscription was released; also audited |
| `mcp.events.removed` | info | `server`, `event_type`, `reason` | A binding was removed and its records deleted |
| `mcp.events.terminated` | error | `server`, `event_type`, `code` | The server ended a webhook subscription: `-32024` refuses the binding, `-32023` or `-32026` lists the server again, anything else re-arms after a backoff |
| `mcp.events.delivery_suspended` | warn | `server`, `event_type`, `last_error`, `active` | The server reported failed deliveries; re-armed from the stored cursor |
| `mcp.events.throttled` | info | `server`, `event_type`, `retry_after_ms` | The server is throttling deliveries |
| `mcp.events.mode_changed` | warn | `server`, `event_type`, `from`, `to`, `cause` | A binding moved to the next delivery mode the operator listed, until a reload or a re-dial decides afresh |
| `mcp.events.callback_failed` | error | `server`, `event_type`, `reason`, `url` | The server could not use the webhook URL; after five in a row the next mode is tried |
| `mcp.events.hook.denied` | warn | `server`, `event_type`, `reason` | A webhook delivery failed its signature check; answered `401` |
| `mcp.events.hook.unsaved` | warn | `err` | The stream head was not saved; the delivery is answered `503` |
| `mcp.events.index.failed` | warn | `subscriptions`, `err` | The record of runtime subscriptions could not be saved |

Payloads are never logged; lines carry `event_id`, and text from the server
is stripped of control characters and cut at 200 bytes.

## MCP Events reference

### Limits

| Limit | Value |
|---|---|
| Bindings per server | 64 |
| Bindings and subscriptions per instance | 256 |
| Live runtime subscriptions per grant (`max`) | 1 to 64, default 8 |
| A tool-made subscription's `lifetime` | at most 30 d, default 1 d |
| Runtime subscription arguments, serialized | 4 KiB |
| One occurrence, or one webhook body | 256 KiB, and at most `store.max_value_bytes` |
| An `eventId` | 1 KiB |
| A stored cursor | 4 KiB |
| Duplicate detection | ids seen in the last 35 min, at most 4096 per binding |
| Listing | 20 pages or 1000 types; past that, `list_too_large` |
| Webhook lease (`ttl`) | default 1 h, refreshed at 80% of what the server grants |
| A webhook refusal still outstanding | after 5 min the binding is paused, keeping its cursor |
| `events/*` requests | 4 worker threads of their own, never the main loop |

### MCP Events refusals

`mcp.events.refused` is logged once per cause, at error level. Nothing is
sent where nothing may be, and the refusal holds until a later listing, a
re-dial or a reload shows the cause has cleared.

| `reason` | When |
|---|---|
| `revision` | The connection reached 2025-11-25; no `events/*` request is sent |
| `no_capability` | The server does not declare the events extension; no `events/*` request is sent |
| `method_not_found` | `events/list` answered `-32601`; a gateway may not pass Events through |
| `unknown_event` | The server's listing does not name the type |
| `no_common_mode` | No listed delivery mode is one the type offers and agentd supports |
| `invalid_arguments` | The arguments fail the type's input schema, or the server answered `-32602` |
| `forbidden` | The server answered `-32024`: the credential may not subscribe |
| `not_supported` | The server answered `-32026` for the delivery mode |
| `list_too_large` | The listing ran past 20 pages or 1000 types |

### Stream event

| Field | Value |
|---|---|
| `id` | `mcp/<server>/<type>/<eventId>`, or `mcp/<server>/<type>/sha256:<hash>` when longer than 200 bytes |
| `stream`, `subject` | the binding's `into` |
| `source` | `mcp:<server>` |
| `data.server`, `data.event` | the server and the event type |
| `data.event_id`, `data.occurred_at` | the server's `eventId` and `timestamp` |
| `data.schema_ok` | whether the payload passed the type's payload schema |
| `data.subscription` | the binding's key, assigned by agentd |
| `data.payload` | the server's `data`, verbatim |

### Webhook delivery answers

Routes are `<webhooks.url>/hooks/_mcp/<binding>`. A delivery's Standard
Webhooks signature is checked before its body is used: all four headers, a
timestamp within 5 minutes, and the HMAC over the raw body.

| Status | When |
|---|---|
| `200` | The occurrence is durably appended or is a duplicate, or a `gap` or `terminated` envelope was taken |
| `401` | The signature check failed |
| `405` | The method is not `POST` |
| `410` | No binding at this path, a body that is not a JSON object, or a malformed occurrence; final |
| `413` | The body is over 256 KiB; answered before it is read; final |
| `429` | Pressure, or the binding's `rate` is spent; with `Retry-After` |
| `503` | Not armed yet, shutting down, a degraded store, or the append was not saved; with `Retry-After` |

## Metrics

| Metric | Type | Labels |
|---|---|---|
| `agent_mcp_connect_failures_total` | counter | `server` |
| `agent_mcp_events_total` | counter | `server`, `event`, `mode`, `outcome` (`appended`, `duplicate`, `rejected`, `schema_mismatch`, `refused_pressure`, `followed`, `unauthenticated`) |
| `agent_mcp_events_requests_total` | counter | `server`, `method`, `outcome` (`ok`, `refused`, `failed`) |
| `agent_mcp_event_gaps_total` | counter | `server`, `event`, `cause` |
| `agent_mcp_events_dedup_evicted_total` | counter | `server`, `event` |
| `agent_mcp_event_lag_ms` | gauge | `server`, `event`: append time minus the server's timestamp |
| `agent_subscriptions_active` | gauge | `kind="resource"`: the resource subscriptions the connections hold; `kind="event"`: MCP Events bindings, by `mode` |

[Events and metrics reference](telemetry-reference.md) lists every metric.

## agentd's own tools

These tools are built into agentd and act on the agent, never on the
world. `agentd --capabilities` prints the list as `internal_tools`. "Root"
is the instance's own conversation, "workflow" a workflow step, and
"subagent" a subagent's loop; the grant is the default, which
configuration can narrow.

| Tool | Default grant | Does |
|---|---|---|
| `instruction.read` | root, workflow, subagent | Read the current instruction |
| `instruction.subscribe` | root | Subscribe to the instruction resource, or switch to another URI |
| `subagent.run`, `.send`, `.status`, `.await`, `.list`, `.kill`, `.retire` | root, workflow | Spawn, steer, wait for, list, stop and retire subagents |
| `memory.get`, `.set`, `.list`, `.push`, `.shift`, `.pop`, `.delete` | root, workflow, subagent | Durable memory, including queue operations |
| `artifact.create`, `.get`, `.delete`, `.list` | root, workflow, subagent | Artifacts delivered with the task |
| `message.send` | root, workflow, subagent | Deliver a message into one of the instance's own conversations |
| `events.list`, `.subscribe`, `.unsubscribe`, `.subscriptions` | root, workflow | Runtime MCP Events subscriptions; offered only when a grant sets `tools: true` |
| `workflow.run`, `.cancel`, `.wait` | root, workflow | Start, cancel and wait for runs |
| `workflow.list`, `.status`, `.signal` | root, workflow, subagent | Read runs and send signals |
| `workflow.create`, `.update`, `.delete`, `.pause`, `.resume` | root | Define workflows at runtime, pause and resume runs |
| `plan.create`, `.get`, `.update`, `.clear` | root, workflow, subagent | The conversation's working plan |
| `ask_human` | root, workflow, subagent | Ask a person and wait for a schema-checked answer |
| `sleep`, `await` | root, workflow, subagent | Wait for a duration, or until a condition holds |
| `context.compact` | root, workflow, subagent | Summarize older messages in the context |
| `think` | root, workflow, subagent | One structured reasoning call with no tools |
| `decide` | root, workflow, subagent | Typed questions answered with calibrated confidence |
| `skills.list`, `.load`, `.unload` | root, workflow, subagent | The skills catalogue and the loaded set |
| `finish` | root, subagent | End the current unit of work with a status |
| `status` | everyone, A2A callers included | The instance's runs, subagents, conversations, budget and store |
| `knowledge.search`, `.get`, `.list` | root, workflow, subagent | A contract: implemented by `knowledge.server` or a mapping |
| `search.query`, `.fetch` | root, workflow, subagent | A contract: implemented by `search.server` or a mapping |
| `code.run` | root, workflow | A contract: implemented only by a mapping to a sandbox server |
| `exec` | root, workflow, subagent | A contract: a mapping, or a local command runner with the `exec` build feature and `security.exec.enabled` |

A subagent's loop also gets `resource.read` when its servers expose
resources. It reads one resource by URI.

## Known limits

- **Revision choice.** The probe alone chooses the revision. A server that
  answers `server/discover` but serves `subscriptions/listen` in a shape
  the client cannot read holds no listens, though the same server reached
  through `initialize` might have. No setting makes agentd start with
  `initialize`.
- **Probe cost.** Dialing a server that predates `server/discover` costs
  one extra request. A server that answers the probe only after the
  connection's timeout is reached through `initialize` after that wait.
- **Revision changes.** A server that changes revision is probed again only
  when it refuses `initialize`, or when a reload dials it afresh.
- **Dropped tools.** At 2026-07-28 the SDK leaves out of a listing any tool
  whose `x-mcp-header` annotation it rejects, a nested one for instance.
  agentd logs each as `mcp.tool.dropped`.
- **Message order.** When a server sends a request ahead of a `tools/list`
  answer on the same stream, the SDK may miss the tools' header schemas,
  and an annotated call lacks its `Mcp-Param-*` headers until the next
  list. A notification ahead of the answer is handled.
- **Interactive reads.** A `resources/read` answered with `input_required`
  fails.
- **Answer bound.** The POST that carries agentd's answer to a server
  request is bounded at 5 s by the SDK.
- **Listens do not resume.** A reopened listen starts from the moment it
  reopens. agentd reads every held URI again after reopening, so state
  converges, but changes inside one gap are coalesced and a change reverted
  inside the gap is not delivered.
- **Catalogue refresh.** The supervisor's tool catalogue is rebuilt at a
  reload, not when a server says its tools changed.
- **Child timeouts.** A turn worker's and a subagent's connections use a
  fixed 60 s timeout; a per-server `timeout` does not reach them.
- **Tags are per server.** The union of every key under `tags` applies to
  every tool of the server.
- **MCP Events is a draft.** Its methods, error codes and capability key may
  change. Push delivery is not implemented; `poll` and `webhook` are.
- **Invisible losses.** Two losses do not show on the wire: a server that
  omits its delivery status and gives up deliveries that never reached
  agentd, and anything missed on a type without replay.
- **Replicas.** Each instance holds its own Events subscriptions, so
  replicas need distinct `webhooks.url` values.

## Next steps

- [Connect MCP servers](connect-mcp.md) — declare, authenticate and verify
  a server.
- [React to changes and events](react-to-events.md) — the triggers these
  events report on.
- [MCP: tools and context](mcp.md) — why the client behaves this way.
- [Events and metrics reference](telemetry-reference.md) — the log envelope
  and every metric.
