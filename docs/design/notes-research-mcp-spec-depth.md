# MCP Wire Protocol — Implementation Reference for agentd

**Status:** Research note / durable design artifact.
**For:** RFC 0001 (minimal, MCP-native, reactive agent runtime).
**Author:** research pass (synthesised from the official spec + reference impls).
**Date:** 2026-06-25.
**Spec baseline:** MCP **2025-11-25** (latest *stable*). A release candidate
**2026-07-28** exists in draft only — we do **not** target it. The widely
deployed prior versions are **2025-06-18** and **2025-03-26**; the
**2024-11-05** version is the one we interoperate *down to* (it is the
default a server assumes when no `MCP-Protocol-Version` header is present,
and it is the cutoff for the deprecated HTTP+SSE transport).

> **Scope of this note.** This is a precise wire reference for the JSON-RPC
> shapes our minimal Rust client *and* server must speak, followed by the
> minimal v1 subset and an explicit defer list. Every shape below is quoted
> from the 2025-11-25 spec pages (bibliography at the end).

---

## 0. Foundations every message obeys

### 0.1 JSON-RPC 2.0, three message kinds

MCP is JSON-RPC 2.0 over a transport. Exactly three shapes exist on the wire:

- **Request** — has `id` (string or integer, never null), `method`, optional `params`. Expects a response.
- **Response** — has the same `id`, and *exactly one* of `result` or `error`.
- **Notification** — has `method`, optional `params`, **no `id`**. No response.

```jsonc
// request
{ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { } }
// success response
{ "jsonrpc": "2.0", "id": 1, "result": { } }
// error response
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "…", "data": { } } }
// notification (no id)
{ "jsonrpc": "2.0", "method": "notifications/initialized" }
```

Rules we must honour:

- **All messages MUST be UTF-8.**
- `id` **MUST NOT** be `null` and **MUST NOT** be reused by the sender within a session.
- Batching: JSON-RPC batch arrays were **removed** in 2025-06-18 and are **not**
  part of 2025-11-25. We send and accept **one JSON object per message** only.
  (Good — simpler for a hand-rolled codec.)
- Standard error codes we will encounter / emit:
  - `-32700` Parse error, `-32600` Invalid Request, `-32601` Method not found,
    `-32602` Invalid params, `-32603` Internal error.
  - MCP-specific: `-32002` Resource not found; `-1` (sampling) user-rejected.
  - `initialize` version mismatch is reported as `-32602` with
    `data.supported` / `data.requested`.

### 0.2 `_meta`, `progressToken`, `cursor`

- Any request `params` may carry `_meta`, which may carry a `progressToken`
  (string or int) to opt into progress notifications for that request.
- Listing methods (`*/list`) accept an optional opaque `cursor` and return an
  optional opaque `nextCursor`. Cursors are **opaque** — never parse them; loop
  until `nextCursor` is absent.

---

## 1. Lifecycle: initialize / capabilities / initialized

The init phase **MUST** be first. Sequence: client → `initialize` request;
server → `initialize` result; client → `notifications/initialized`. Before the
server has answered `initialize`, the client SHOULD send nothing but `ping`.
Before the server receives `initialized`, the server SHOULD send nothing but
`ping` and `logging`.

### 1.1 `initialize` request (client → server)

```jsonc
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2025-11-25",
    "capabilities": {
      "roots":       { "listChanged": true },
      "sampling":    { },
      "elicitation": { "form": {}, "url": {} },
      "tasks":       { "requests": { "elicitation": { "create": {} },
                                     "sampling":   { "createMessage": {} } } }
    },
    "clientInfo": {
      "name": "agentd",
      "title": "agentd",
      "version": "1.x",
      "description": "…",      // optional
      "websiteUrl": "…",       // optional
      "icons": [ ]             // optional
    }
  }
}
```

### 1.2 `initialize` result (server → client)

```jsonc
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2025-11-25",
    "capabilities": {
      "logging":   { },
      "prompts":   { "listChanged": true },
      "resources": { "subscribe": true, "listChanged": true },
      "tools":     { "listChanged": true },
      "tasks":     { "list": {}, "cancel": {},
                     "requests": { "tools": { "call": {} } } }
    },
    "serverInfo": { "name": "…", "title": "…", "version": "…" },
    "instructions": "Optional instructions for the client"   // optional
  }
}
```

### 1.3 `notifications/initialized` (client → server)

```jsonc
{ "jsonrpc": "2.0", "method": "notifications/initialized" }
```

### 1.4 Capability map (what each flag means)

| Side | Capability | Sub-fields | Meaning |
|---|---|---|---|
| Client | `roots` | `listChanged` | Client exposes filesystem roots; will notify on change. |
| Client | `sampling` | `tools`, `context` | Client will service `sampling/createMessage`. `tools` ⇒ tool-use sampling allowed; `context` ⇒ legacy `includeContext` allowed. |
| Client | `elicitation` | `form`, `url` | Client can answer server elicitation requests. |
| Client | `tasks` | `requests.*` | Client supports task-augmented requests (new in 2025-11-25). |
| Client | `experimental` | — | Non-standard features. |
| Server | `prompts` | `listChanged` | Server offers prompt templates. |
| Server | `resources` | `subscribe`, `listChanged` | Server offers resources; per-item subscribe; list-change notifications. **Each is independently optional.** |
| Server | `tools` | `listChanged` | Server offers tools. |
| Server | `logging` | — | Server emits `notifications/message`. |
| Server | `completions` | — | Argument autocompletion. |
| Server | `tasks` | `list`, `cancel`, `requests.*` | Server supports task-augmented requests. |

**Negotiation rule we enforce:** only use a capability the *other* side
declared. E.g. do not send `resources/subscribe` unless the server's
`resources.subscribe === true`; do not send `notifications/resources/updated`
to a client that did not subscribe.

### 1.5 Version negotiation

Client sends its latest supported `protocolVersion`. If the server supports it,
it echoes the same string. Otherwise the server returns *a* version it supports
(SHOULD be its latest); the client disconnects if it can't speak that. We
should advertise `2025-11-25` and accept being downgraded to `2025-06-18` /
`2025-03-26` / `2024-11-05` where our feature use overlaps.

---

## 2. Tools — `tools/list`, `tools/call`

### 2.1 `tools/list`

Request (cursor optional). Response: `tools[]` + optional `nextCursor`.

```jsonc
// request
{ "jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"…"} }
// result
{ "jsonrpc":"2.0","id":1,"result":{
  "tools":[{
    "name":"get_weather",
    "title":"Weather Information Provider",      // optional
    "description":"Get current weather…",
    "inputSchema":{ "type":"object",
      "properties":{ "location":{"type":"string"} },
      "required":["location"] },
    "outputSchema":{ },                          // optional
    "annotations":{ },                           // optional, UNTRUSTED
    "execution":{ "taskSupport":"optional" },    // optional: forbidden|optional|required
    "icons":[ ]                                  // optional
  }],
  "nextCursor":"…"                               // optional
}}
```

**`inputSchema` rules:** MUST be a valid JSON Schema *object* (never `null`).
Default dialect is JSON Schema **2020-12** when no `$schema` is present. For a
no-arg tool: `{"type":"object","additionalProperties":false}`. Tool names:
1–128 chars, `[A-Za-z0-9_.-]`, case-sensitive, unique per server.

### 2.2 `tools/call`

```jsonc
// request
{ "jsonrpc":"2.0","id":2,"method":"tools/call",
  "params":{ "name":"get_weather", "arguments":{ "location":"New York" } } }
// result (unstructured)
{ "jsonrpc":"2.0","id":2,"result":{
  "content":[ { "type":"text","text":"…" } ],
  "isError":false
}}
```

### 2.3 Content block types (shared by tools, prompts, sampling)

```jsonc
{ "type":"text", "text":"…" }
{ "type":"image", "data":"<base64>", "mimeType":"image/png" }
{ "type":"audio", "data":"<base64>", "mimeType":"audio/wav" }
{ "type":"resource_link", "uri":"file:///…","name":"main.rs",
  "description":"…","mimeType":"text/x-rust" }
{ "type":"resource", "resource":{
    "uri":"file:///…","mimeType":"text/x-rust","text":"…" } }   // or "blob":"<base64>"
```

All blocks may carry optional `annotations` (`audience`, `priority`,
`lastModified`).

### 2.4 `isError` and `structuredContent`

- `isError: true` is a **tool-execution error** carried *inside a successful
  JSON-RPC result* — the model is meant to read it and self-correct. This is
  **distinct** from a JSON-RPC `error` (a *protocol* error: unknown tool,
  malformed params, server fault).
- `structuredContent` (object) is returned alongside `content` when the tool
  declares an `outputSchema`. For back-compat the server SHOULD *also* place the
  serialized JSON in a `text` block. Client SHOULD validate it against the
  `outputSchema`.

```jsonc
{ "jsonrpc":"2.0","id":5,"result":{
  "content":[{"type":"text","text":"{\"temperature\":22.5}"}],
  "structuredContent":{ "temperature":22.5, "conditions":"…", "humidity":65 }
}}
```

**agentd handling:** map a JSON-RPC `error` from `tools/call` to a loop-level
failure (retry/abort policy); map `isError:true` to an *observation* fed back to
the model. This distinction is load-bearing for the agentic loop.

### 2.5 `notifications/tools/list_changed`

```jsonc
{ "jsonrpc":"2.0","method":"notifications/tools/list_changed" }
```
On receipt (and if server declared `tools.listChanged`), re-issue `tools/list`.

---

## 3. Resources — the reactive core

### 3.1 `resources/list`

```jsonc
{ "jsonrpc":"2.0","id":1,"result":{
  "resources":[{
    "uri":"file:///project/src/main.rs",
    "name":"main.rs",
    "title":"…",        // optional
    "description":"…",  // optional
    "mimeType":"text/x-rust",  // optional
    "size":12345,       // optional, bytes
    "icons":[ ]         // optional
  }],
  "nextCursor":"…"      // optional
}}
```

### 3.2 `resources/read`

```jsonc
// request
{ "jsonrpc":"2.0","id":2,"method":"resources/read",
  "params":{ "uri":"file:///project/src/main.rs" } }
// result — contents[] is an ARRAY (one URI may yield several items, e.g. a dir)
{ "jsonrpc":"2.0","id":2,"result":{
  "contents":[
    { "uri":"file:///…","mimeType":"text/x-rust","text":"…" },   // text variant
    { "uri":"file:///…","mimeType":"image/png","blob":"<base64>" } // binary variant
  ]
}}
```
Error: resource not found ⇒ `-32002` with `data.uri`.

### 3.3 `resources/templates/list` (RFC 6570 URI templates)

```jsonc
{ "jsonrpc":"2.0","id":3,"method":"resources/templates/list" }
{ "jsonrpc":"2.0","id":3,"result":{
  "resourceTemplates":[{
    "uriTemplate":"file:///{path}",
    "name":"Project Files",
    "title":"…","description":"…","mimeType":"…"
  }]
}}
```
Note: `resourceTemplates` has **no** pagination cursor in the spec example.
agentd can treat templates as informational for v1 (we read concrete URIs).

### 3.4 Subscriptions (the trigger mechanism)

```jsonc
// subscribe  (gated by server resources.subscribe == true)
{ "jsonrpc":"2.0","id":4,"method":"resources/subscribe",
  "params":{ "uri":"file:///project/src/main.rs" } }
// → result: {} (empty)
// unsubscribe
{ "jsonrpc":"2.0","id":5,"method":"resources/unsubscribe",
  "params":{ "uri":"file:///project/src/main.rs" } }
// → result: {} (empty)
```

### 3.5 Update + list-changed notifications (server → client)

```jsonc
{ "jsonrpc":"2.0","method":"notifications/resources/updated",
  "params":{ "uri":"file:///project/src/main.rs" } }

{ "jsonrpc":"2.0","method":"notifications/resources/list_changed" }
```

**This is the single most important inbound signal for agentd.** The supervisor
loop maps `notifications/resources/updated{uri}` → spawn-vs-continue routing
(RFC §5.3). Note the update notification carries **only the URI**; to see *what*
changed the supervisor (or the woken subagent) must call `resources/read`.

---

## 4. Prompts — can we skip?

`prompts/list`, `prompts/get` (returns `messages[]` with `role` +
`content` block), and `notifications/prompts/list_changed`.

```jsonc
{ "jsonrpc":"2.0","id":2,"method":"prompts/get",
  "params":{ "name":"code_review","arguments":{ "code":"…" } } }
{ "jsonrpc":"2.0","id":2,"result":{
  "description":"…",
  "messages":[ { "role":"user","content":{ "type":"text","text":"…" } } ]
}}
```

**Verdict for agentd v1: SKIP as both client and server.** Prompts are
user-controlled slash-command templates — irrelevant to a headless agent. We do
not declare the `prompts` capability on our server, and as a client we simply
never call `prompts/*`. (A server that offers prompts is harmless; we ignore
them.) Trivial to add later behind the same content-block code we already need
for tools.

---

## 5. Sampling — `sampling/createMessage` (server → client)

This is a **reverse** call: the MCP *server* asks the *client* to run an LLM
generation. For agentd-as-client this is **optional**; for agentd-as-server it
is a powerful primitive (a peer agentd, or a tool server, can borrow *our*
intelligence endpoint).

### 5.1 Request (server → client)

```jsonc
{ "jsonrpc":"2.0","id":1,"method":"sampling/createMessage",
  "params":{
    "messages":[
      { "role":"user","content":{ "type":"text","text":"What is the capital of France?" } }
    ],
    "modelPreferences":{
      "hints":[ { "name":"claude-3-sonnet" }, { "name":"claude" } ],
      "costPriority":0.3, "speedPriority":0.8, "intelligencePriority":0.5
    },
    "systemPrompt":"You are a helpful assistant.",   // optional
    "includeContext":"none",                          // none|thisServer|allServers (last two soft-deprecated)
    "temperature":0.7,                                // optional
    "maxTokens":100,
    "stopSequences":[ ],                              // optional
    "metadata":{ },                                   // optional, passthrough
    "tools":[ ],                                      // optional, only if client declared sampling.tools
    "toolChoice":{ "mode":"auto" }                    // auto|required|none
  }}
```

### 5.2 Response (client → server)

```jsonc
{ "jsonrpc":"2.0","id":1,"result":{
  "role":"assistant",
  "content":{ "type":"text","text":"The capital of France is Paris." },
  "model":"claude-3-sonnet-20240307",
  "stopReason":"endTurn"        // endTurn | toolUse | maxTokens | stopSequence | …
}}
```

### 5.3 Model preferences (cross-provider abstraction)

`modelPreferences` decouples request from concrete model: three normalized
priorities (`costPriority`, `speedPriority`, `intelligencePriority`, each 0–1)
plus `hints[]` (ordered substrings matched flexibly against model names; advisory
— the *client* picks). This maps cleanly onto agentd's single intelligence
endpoint: we translate `hints`/priorities into our `--model` / gateway routing.

### 5.4 Tool-use sampling (multi-turn)

If client declares `sampling.tools`, the server may include `tools[]` +
`toolChoice`. The client's model may return `content` as an array of
`{"type":"tool_use","id","name","input"}` with `stopReason:"toolUse"`. The
server executes and replies with a follow-up `createMessage` whose next message
is `role:"user"` containing **only** `{"type":"tool_result","toolUseId","content":[…]}`
blocks (one per tool_use id; tool-result messages MUST NOT mix other content).
Errors: user-rejected `-1`; missing/mixed tool results `-32602`.

**agentd verdict:** As **client**, **DEFER** sampling for v1 (don't declare
`sampling`) — our agentic loop already owns the LLM directly; servicing
server-initiated sampling is extra surface. As **server**, **DEFER** issuing
`sampling/createMessage` to v1 too; it's a strong v2 feature for agent-to-agent
intelligence sharing.

---

## 6. Roots — `roots/list` (server → client)

Client declares `roots:{listChanged}`. Server asks for filesystem boundaries.

```jsonc
// server → client
{ "jsonrpc":"2.0","id":1,"method":"roots/list" }
// client → server
{ "jsonrpc":"2.0","id":1,"result":{
  "roots":[ { "uri":"file:///home/user/projects/myproject","name":"My Project" } ]
}}
// client → server, on change
{ "jsonrpc":"2.0","method":"notifications/roots/list_changed" }
```
`uri` **MUST** be a `file://` URI. Not-supported ⇒ `-32601`.

**agentd verdict:** **DEFER** both directions for v1. As a client we may answer
`roots/list` with an **empty list** `{"roots":[]}` if a server insists, but we
need not declare the capability. As a server we don't request roots in v1.

---

## 7. Logging — `logging/setLevel` + `notifications/message`

```jsonc
// client → server
{ "jsonrpc":"2.0","id":1,"method":"logging/setLevel","params":{ "level":"info" } }
// → result {}
// server → client
{ "jsonrpc":"2.0","method":"notifications/message",
  "params":{ "level":"error","logger":"database","data":{ "…":"arbitrary JSON" } } }
```
Levels = RFC 5424 syslog severities, lowest→highest:
`debug, info, notice, warning, error, critical, alert, emergency`.

**agentd verdict:** As **client**, **consume** `notifications/message` and fold
it into our structured logs/tracing (cheap, high observability value — RFC
requirement #6). Sending `logging/setLevel` is optional but trivial. As
**server**, declaring `logging` and emitting `notifications/message` is a clean
way to surface agentd internals to a driving client — **nice-to-have v1, can
defer to early v2.**

---

## 8. Utilities: ping, progress, cancellation, pagination

### 8.1 Ping (either direction)

```jsonc
{ "jsonrpc":"2.0","id":"123","method":"ping" }     // no params
{ "jsonrpc":"2.0","id":"123","result":{} }          // empty
```
Receiver MUST answer promptly with `{}`. Timeout ⇒ sender MAY treat the
connection as stale and tear down / restart the child. **agentd MUST implement
ping both ways** — it is our liveness probe for stdio MCP subprocesses and for
peer agentd connections (RFC requirement #8: detect dead/stuck subprocesses).

### 8.2 Progress

Opt in by putting `_meta.progressToken` (string|int, unique per active request)
in a request. Receiver MAY then emit:

```jsonc
{ "jsonrpc":"2.0","method":"notifications/progress",
  "params":{ "progressToken":"abc123","progress":50,"total":100,"message":"…" } }
```
`progress` MUST strictly increase; `total`/`message` optional; both may be float.
Progress MAY reset the request timeout clock, but an absolute max timeout MUST
still apply. **agentd verdict:** consume progress to keep long tool calls from
tripping timeouts and to stream status up the control channel; emitting it from
our server is optional v1.

### 8.3 Cancellation

```jsonc
{ "jsonrpc":"2.0","method":"notifications/cancelled",
  "params":{ "requestId":"123","reason":"User requested cancellation" } }
```
Rules: may only reference an in-flight request issued *in the same direction*;
`initialize` MUST NOT be cancelled; receiver SHOULD stop work and **send no
response**; sender SHOULD ignore a late response; both sides handle the race
where the response already left. **agentd MUST send cancellation** when a
deadline/step-budget trips or a subagent is killed, so we don't leak in-flight
tool calls on a server we keep using.

### 8.4 Pagination

`*/list` methods: pass optional `cursor`, read optional `nextCursor`; loop until
absent. Cursors are opaque. agentd implements a tiny "drain all pages" helper.

---

## 9. Transports

### 9.1 stdio (our default, dependency-free)

Rules (all MUST unless noted):

- Client launches the server as a **subprocess**. Server reads JSON-RPC from
  **stdin**, writes to **stdout**.
- **One JSON-RPC message per line, newline-delimited (`\n`), and messages MUST
  NOT contain embedded newlines.** So: serialize compact (no pretty-print), append `\n`.
- **stdout is sacred:** the server MUST NOT write anything to stdout that is not
  a valid MCP message; the client MUST NOT write anything to the server's stdin
  that is not a valid MCP message.
- **stderr is free-form:** server MAY write arbitrary UTF-8 logs to stderr;
  client MAY capture/forward/ignore it and MUST NOT treat stderr output as an
  error signal. (We capture stderr into our structured logs, tagged by server.)
- UTF-8 throughout.
- **Shutdown** (client-initiated, in order): close child's **stdin** → wait for
  exit → `SIGTERM` if it lingers → `SIGKILL` if it still lingers. Server MAY
  self-initiate by closing stdout and exiting.

This is a near-perfect match for agentd's process model. Our codec is: a
buffered line reader on stdout (split on `\n`), `serde`-free hand-rollable if we
want, writing `compact_json + "\n"` to stdin. The same line-protocol discipline
(JSON object per line, no embedded newlines) is what RFC §6.2 proposes for the
supervisor↔subagent control channel — strong argument to **share one codec**.

### 9.2 Streamable HTTP (for network/sidecar servers)

Single **MCP endpoint** path (e.g. `/mcp`) handling both POST and GET.

**POST (client → server), every message is its own POST:**
- Client MUST set `Accept: application/json, text/event-stream` (both).
- Body is one JSON-RPC request | notification | response.
- If body is a **notification or response**: server returns **202 Accepted, no
  body** (or an HTTP error; an error body MAY be a JSON-RPC error with no `id`).
- If body is a **request**: server returns **either** `Content-Type:
  application/json` (one JSON result) **or** `Content-Type: text/event-stream`
  (an SSE stream that eventually carries the JSON-RPC response, and MAY carry
  server→client requests/notifications first). Client MUST support both.

**GET (client → server):** opens an SSE stream for **unsolicited** server→client
messages. Server returns `text/event-stream`, or **405** if it offers none.

**Session management:**
- Server MAY assign `MCP-Session-Id` on the `InitializeResult` HTTP response.
  Visible ASCII (0x21–0x7E), globally unique, crypto-secure.
- If assigned, client MUST echo `MCP-Session-Id` on **all** later requests.
  Missing it (post-init) ⇒ server MAY return **400**.
- Server MAY end a session ⇒ later requests with that id get **404** ⇒ client
  MUST re-`initialize` without a session id.
- Client SHOULD `DELETE` the endpoint with the session id to end a session;
  server MAY answer **405** if it forbids client-side termination.

**Protocol version header:** after init, client MUST send
`MCP-Protocol-Version: 2025-11-25` on every HTTP request. Missing ⇒ server
assumes `2025-03-26`. Invalid/unsupported ⇒ **400**.

**Resumability / redelivery:**
- Server MAY attach SSE `id:` to events (globally unique per session, encoding
  the originating stream).
- On disconnect, client SHOULD reconnect via **GET** with `Last-Event-ID`;
  server MAY replay messages after that id **on the same stream only** (MUST NOT
  replay onto a different stream).
- Disconnect is **not** cancellation — to cancel, send
  `notifications/cancelled` explicitly.

**Security:** server MUST validate `Origin` (invalid ⇒ **403**); SHOULD bind
localhost only when local; SHOULD authenticate.

**Backwards-compat with deprecated 2024-11-05 HTTP+SSE:** a client probes by
POSTing `initialize`; on `400/404/405` it falls back to a GET that expects an
`endpoint` SSE event (old two-endpoint transport). We do **not** implement the
old transport.

---

## 10. The MINIMAL subset agentd implements in v1

Guiding principle (RFC §12): smallest surface that makes the three execution
modes and the supervised tree work. Everything below is justified against a
specific RFC requirement.

### 10.1 As an MCP **client** (to external servers) — v1 MUST

| Area | Methods / notifications | Why |
|---|---|---|
| Lifecycle | `initialize` (send), parse result, `notifications/initialized` (send), version negotiation | Mandatory handshake. |
| Tools | `tools/list` (+ pagination), `tools/call`, parse `content[]` (text/image/audio/resource/resource_link), `isError`, `structuredContent`; handle `notifications/tools/list_changed` | The entire action space (RFC §7.1). |
| Resources | `resources/list`, `resources/read` (contents[] text+blob) | Context (RFC §7.1). |
| Resource triggers | `resources/subscribe`, `resources/unsubscribe`, **consume** `notifications/resources/updated` and `notifications/resources/list_changed` | **The signature reactive mode** (RFC §5.3). |
| Liveness | `ping` (send + answer), `notifications/cancelled` (send) | Detect dead/stuck servers; cancel on budget/deadline (RFC #8, #6). |
| Progress | **consume** `notifications/progress` | Avoid false timeouts on long tool calls. |
| Logging | **consume** `notifications/message` | Observability (RFC #6). |
| Transport | **stdio** (full: line codec, stderr capture, ordered shutdown) | Default, dependency-free (RFC §7.1, §12). |
| Client-feature replies | answer `ping`; minimal/declined replies to `roots/list` (empty), `sampling/createMessage` (reject), if a server sends them | Robust interop without declaring the caps. |

We **declare** as client: *(nothing required beyond identifying ourselves)* — we
need **not** declare `roots`, `sampling`, `elicitation`, or `tasks` to use a
server's tools/resources. We only declare a client capability when we intend to
service it. v1 declares **none** of them.

### 10.2 As an MCP **server** (agentd's self-MCP, RFC §8) — v1 MUST

| Area | Methods / notifications | Why |
|---|---|---|
| Lifecycle | answer `initialize` (declare caps), accept `notifications/initialized` | Mandatory. |
| Tools | `tools/list`, `tools/call` exposing `subagent.spawn/send/cancel/status`, `subscribe`/`unsubscribe`, gated `exec`; declare `tools:{listChanged:true}` and emit it when the gated set changes | Self-wiring + internal tools (RFC §8). |
| Resources | `resources/list`, `resources/read` for session/run/subagent state; declare `resources:{subscribe:true,listChanged:true}`; `resources/subscribe`/`unsubscribe`; **emit** `notifications/resources/updated` + `notifications/resources/list_changed` | Agent-to-agent reactivity — a peer subscribes to *our* state (RFC §8). **This is what makes us symmetric.** |
| Liveness | answer `ping`; accept `notifications/cancelled` | Be a good citizen; support driver cancellation. |
| Transport | **stdio** always; **Streamable HTTP** only the minimal non-resumable subset when `--serve-mcp` selects an HTTP/unix transport | RFC §8 (opt-in serving). |

If/when we serve over Streamable HTTP in v1, implement the **stateless / minimal
profile only**: POST→single `application/json` response for requests; **202** for
notifications/responses; optional `MCP-Session-Id`; validate `Origin`/bind
localhost; require `MCP-Protocol-Version`. **GET-SSE, multi-stream, and
resumability are deferred** (see below). For the common deployments (stdio
children; unix-socket peers) we can ship v1 with **stdio-only serving** and add
HTTP in phase 4.

### 10.3 DEFER to post-v1 (explicit)

- **Streamable HTTP resumability** (SSE `id:`/`Last-Event-ID` replay, multi-
  stream fan-out, server-initiated GET-SSE). v1 uses request/response POSTs (and
  stdio); long-poll/streaming is a later optimization. The supervisor's *own*
  reactivity comes from stdio resource subscriptions, not HTTP SSE.
- **`prompts/*`** (both directions) — user-facing slash commands, irrelevant
  headless.
- **`sampling/createMessage`** — as client (don't declare `sampling`) and as
  server (don't issue it). Strong v2 feature for sharing the intelligence
  endpoint between agents.
- **`roots/*`** — don't declare `roots`; answer empty if asked. Add when a tool
  server needs filesystem boundary negotiation.
- **`elicitation/*`** and **`completion/complete`** — interactive UX surfaces, not
  needed headless.
- **`tasks/*`** (task-augmented requests, new in 2025-11-25) — adds a
  durable/async request lifecycle with `tasks/list`/`tasks/cancel` and
  `taskSupport` on tools. Powerful for long-running tool calls but a meaningful
  surface; **defer**, fall back to plain request/response + progress + cancel.
- **Emitting** `notifications/message` and `notifications/progress` from our
  server — nice observability, add when cheap.
- **Old 2024-11-05 HTTP+SSE transport** — never implement; only consume modern
  Streamable HTTP.

### 10.4 Codec & robustness notes for the implementation

- **One JSON object per line, compact, `\n`-terminated, no embedded newlines,
  UTF-8.** Reject/skip lines that don't parse rather than crashing the reader.
- **Correlate by `id`.** Keep a small map of outstanding request ids → waiters.
  Never reuse an id within a session. Tolerate out-of-order responses.
- **Route inbound by shape:** has `id` + `method` ⇒ a request *to us* (answer or
  error `-32601`); has `id` + `result|error` ⇒ a response; no `id` ⇒ a
  notification (dispatch by `method`, ignore unknown).
- **Liveness:** periodic `ping`; on timeout escalate stdin-close → SIGTERM →
  SIGKILL (matches the spec's stdio shutdown ladder and RFC #8).
- **Capability gating:** never emit a subscribe/notification the peer didn't
  negotiate; degrade gracefully if a server lacks `resources.subscribe` (fall
  back to poll, or surface that the reactive trigger is unavailable).
- **Share the codec** between MCP transport and the supervisor↔subagent control
  channel (RFC §6.2, §14 Q1) — both are "JSON-RPC object per line over pipes."

---

## 11. Open mappings into RFC 0001

- **RFC §5.3 reactive trigger** ⇐ `resources/subscribe` +
  `notifications/resources/updated` (URI-only; read to learn the change).
- **RFC §8 self-tools** ⇐ our server's `tools/call` surface.
- **RFC §8 self-resources / agent-to-agent reactivity** ⇐ our server's
  `resources/subscribe` + emitted `notifications/resources/updated`.
- **RFC §14 Q1 control protocol** ⇐ reuse the stdio line codec; the control
  channel is "MCP-flavoured JSON-RPC over the child's pipes."
- **RFC #8 dead/stuck detection** ⇐ `ping` + stdio shutdown ladder + timeouts
  (with progress-aware but absolute caps).

---

## 12. Bibliography (all retrieved 2026-06-25; spec version 2025-11-25)

- Lifecycle — https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle
- Transports (stdio + Streamable HTTP) — https://modelcontextprotocol.io/specification/2025-11-25/basic/transports
- Tools — https://modelcontextprotocol.io/specification/2025-11-25/server/tools
- Resources — https://modelcontextprotocol.io/specification/2025-11-25/server/resources
- Prompts — https://modelcontextprotocol.io/specification/2025-11-25/server/prompts
- Sampling — https://modelcontextprotocol.io/specification/2025-11-25/client/sampling
- Roots — https://modelcontextprotocol.io/specification/2025-11-25/client/roots
- Logging — https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/logging
- Ping — https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/ping
- Progress — https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/progress
- Cancellation — https://modelcontextprotocol.io/specification/2025-11-25/basic/utilities/cancellation
- Pagination — https://modelcontextprotocol.io/specification/2025-11-25/server/utilities/pagination
- Canonical TypeScript schema (source of truth) — https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/2025-11-25/schema.ts
- Spec repo / changelog — https://github.com/modelcontextprotocol/modelcontextprotocol ; https://modelcontextprotocol.info/specification/2025-11-25/changelog/
- Deprecated HTTP+SSE (for back-compat awareness only) — https://modelcontextprotocol.io/specification/2024-11-05/basic/transports
- Reference implementations: TypeScript SDK (`@modelcontextprotocol/sdk`) and
  Python SDK under https://github.com/modelcontextprotocol — consulted for the
  shape of `content` blocks, capability negotiation, and the stdio line codec.
- Release-candidate (NOT targeted) 2026-07-28 — https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/

### Version-delta cheatsheet (why 2025-11-25 is our baseline)
- **2024-11-05:** original; HTTP+SSE two-endpoint transport; the assumed-default
  when no protocol header is sent. We interoperate *down to* this.
- **2025-03-26:** introduced **Streamable HTTP** (single endpoint), replaced
  HTTP+SSE; added audio content, tool annotations.
- **2025-06-18:** **removed JSON-RPC batching**; added structured tool output /
  `outputSchema`, elicitation, resource links, `MCP-Protocol-Version` header
  requirement.
- **2025-11-25 (target):** added **tasks** (task-augmented async requests),
  tool-use in sampling, richer client/server info (icons, websiteUrl),
  `execution.taskSupport` on tools, URL-mode elicitation. JSON Schema 2020-12 is
  the default dialect.
