# Design review: MCP protocol correctness of RFC 0001

**Reviewer lens:** Model Context Protocol (MCP) spec conformance.
**RFC under review:** `rfcs/0001-mcp-native-agent-runtime.md` (draft, 2026-06-25).
**Spec verified against:** MCP **2025-11-25** (the current/latest revision; supersedes 2025-06-18, 2025-03-26, 2024-11-05).
**Date:** 2026-06-25.

> Method names, params, capability flags, and notification shapes below are quoted from the
> live spec pages: lifecycle, transports, server/resources, server/tools, client/sampling,
> client/roots, basic/utilities/{tasks,progress,cancellation}, and the 2025-11-25 changelog.

---

## 0. Executive verdict

The RFC's core MCP assumptions are **broadly correct**, and the headline "reactive via
resource subscriptions" trigger model **is supported by the spec** — with one important
nuance the RFC currently glosses over. The biggest *protocol-correctness* gaps are:

1. **agentd-as-server "subscribable session/state resources" is the weakest claim.**
   `resources/subscribe` is spec-defined and fine, BUT the spec deliberately under-specifies
   *what* `notifications/resources/updated` carries (just the `uri`), and **`subscribe` is an
   item-level mechanism only** — there is **no list-level subscribe**. The RFC conflates
   item subscriptions with list-changed notifications in a couple of places. Both exist; they
   are different mechanisms with different shapes. This is fixable by being precise.

2. **The RFC ignores two 2025-11-25 features that are almost purpose-built for agentd:**
   **Tasks** (durable, pollable, deferred-result requests — SEP-1686) and **sampling with
   tools** (SEP-1577). Tasks in particular are a far better fit for "long-running subagent
   work behind an MCP server" than the RFC's hand-rolled control channel, and **sampling**
   is the *correct* spec mechanism for "a peer uses agentd's intelligence" — the RFC mentions
   this use case (§8 intro, and §7 framing) without naming `sampling/createMessage`.

3. **Transport minimalism is sound but the RFC's "HTTP/SSE" wording is stale.** The current
   transports are **stdio** and **Streamable HTTP**. The old **HTTP+SSE** transport
   (2024-11-05) is **deprecated**. The RFC says "HTTP/SSE" in several places; it should say
   **Streamable HTTP** (which itself *optionally* uses SSE within a single endpoint).

4. **A minimal MCP *server* that wants to be subscribable is non-trivial.** Exposing
   subscribable resources over stdio is easy. Exposing them to *external/peer* clients
   requires Streamable HTTP with server-initiated SSE, `Origin` validation, optional
   `MCP-Session-Id`, and the `MCP-Protocol-Version` header — materially more than "a tiny
   blocking HTTP/1.1 client." The dependency budget (§12) under-counts the *server* side.

Everything below is the detail.

---

## 1. Capability negotiation in `initialize` (RFC §4.2, §7.1)

### What the spec actually says

The client **MUST** send `initialize` first, with `protocolVersion`, `capabilities`,
`clientInfo`. Server **MUST** reply with `protocolVersion`, `capabilities`, `serverInfo`,
optional `instructions`. Then the client **MUST** send `notifications/initialized`.

Version negotiation: client sends its latest supported version; if the server supports it,
it **MUST** echo the same string; otherwise it returns another version it supports; if the
client can't accept that, it **SHOULD** disconnect.

**Capabilities that exist (2025-11-25):**

| Side | Capability | Sub-flags |
|---|---|---|
| Client | `roots` | `listChanged` |
| Client | `sampling` | `tools`, `context` (soft-deprecated) |
| Client | `elicitation` | `form`, `url` |
| Client | `tasks` | `list`, `cancel`, `requests.sampling.createMessage`, `requests.elicitation.create` |
| Client | `experimental` | — |
| Server | `prompts` | `listChanged` |
| Server | `resources` | `subscribe`, `listChanged` |
| Server | `tools` | `listChanged` |
| Server | `logging` | — |
| Server | `completions` | — |
| Server | `tasks` | `list`, `cancel`, `requests.tools.call` |
| Server | `experimental` | — |

Example client `initialize` params (abridged from spec):

```json
{
  "protocolVersion": "2025-11-25",
  "capabilities": {
    "roots": { "listChanged": true },
    "sampling": {},
    "elicitation": { "form": {}, "url": {} }
  },
  "clientInfo": { "name": "ExampleClient", "version": "1.0.0" }
}
```

### Implications for agentd

- **agentd-as-client** to N MCP servers: it MUST run the full handshake per connection and
  store each server's *negotiated* capabilities. The RFC's "perform the MCP handshake, and
  hold the catalogue of available tools and resources" (§4.1) is correct but must be
  capability-gated: only call `resources/subscribe` on servers that advertised
  `resources.subscribe: true`; only expect `notifications/resources/list_changed` from servers
  that advertised `resources.listChanged: true`. The RFC does not mention this gating; it
  should, because subscribing to a server that didn't advertise `subscribe` is a protocol
  violation and will (correctly) be rejected.

- **agentd-as-server**: to expose subscribable resources it MUST advertise
  `resources: { subscribe: true, listChanged: true }`. To let a peer use its intelligence it
  must understand that **sampling is a *client* capability** — see §5.

- **Protocol version pinning:** agentd should pin a known protocol version (recommend
  `2025-11-25`, fall back to `2025-06-18`) and implement the version-mismatch path. The RFC
  is silent on which version it targets — it should state one, because feature availability
  (tasks, sampling-with-tools, structured tool output) is version-gated.

---

## 2. Resources: list / read / templates (RFC §7.1)

The RFC's claim "`resources/list` + `resources/read`" is **correct**. Precise shapes:

- **`resources/list`** → `params: { cursor? }`; result `{ resources: [...], nextCursor? }`.
  **Paginated** (cursor-based). The RFC must implement cursor following or it will silently
  see only the first page of resources.
- **`resources/read`** → `params: { uri }`; result `{ contents: [ { uri, mimeType, text } | { uri, mimeType, blob } ] }`.
  Note `contents` is an **array** — a single read can return multiple content items (e.g. a
  directory). Binary content arrives base64 in `blob`, not `text`.
- **`resources/templates/list`** → `params: { cursor? }`; result `{ resourceTemplates: [ { uriTemplate, name, title?, description?, mimeType? } ], nextCursor? }`.
  RFC 6570 URI templates. **The RFC never mentions resource templates.** This matters for
  reactivity: you cannot `subscribe` to a *template* (e.g. `db://query/{id}`) — you can only
  subscribe to *concrete* URIs. If agentd wants to react to "any new row," it must first
  enumerate/resolve concrete URIs (via `resources/list` or template expansion) and subscribe
  to each. **Flag:** §5.3's `db://query/...` example implies template-level reactivity that
  the protocol does not provide.

Standard schemes: `https://`, `file://`, `git://`, plus custom schemes (must satisfy
RFC 3986). agentd's `db://`, and any "another agentd's exposed resource" scheme, are *custom*
schemes — legal, but agentd defines their semantics; no interop guarantee with third parties.

Error codes worth handling: resource-not-found `-32002`, internal `-32603`.

---

## 3. Resource subscriptions — the reactive core (RFC §1.3, §5.3, §7.1, §8)

This is the load-bearing claim. **It is supported. The details matter.**

### 3.1 Item-level subscribe (this is what reactivity rides on)

```json
// request
{ "jsonrpc":"2.0","id":4,"method":"resources/subscribe","params":{ "uri":"file:///project/src/main.rs" } }
// later, server -> client notification
{ "jsonrpc":"2.0","method":"notifications/resources/updated","params":{ "uri":"file:///project/src/main.rs" } }
```

- `resources/subscribe` params = **`{ uri }`** — a single concrete resource URI. Confirmed
  by spec. The RFC's `subscribe(resource_uri)` self-tool and the supervisor's
  `resources/subscribe` usage are both **correct**.
- `resources/unsubscribe` params = `{ uri }`. Correct in the RFC.
- The update notification is **`notifications/resources/updated`** with params **`{ uri }`**
  — **and (in 2025-11-25) optionally a `title`**, but critically **no payload/diff**. The
  spec's canonical flow is: receive `updated` → the client then issues a fresh
  `resources/read` to get new contents. **Flag:** the RFC's §5.3 phrasing "reads what
  changed" and "deliver the event into an existing session" implies the notification carries
  the change. It does **not**. agentd must `resources/read` on wake to learn *what* changed.
  This is a real design consequence: the reactive loop is **notify-then-read**, two round
  trips, and the read can race (resource may change again before the read). The RFC should
  state the notify-then-read pattern explicitly and decide its read-coalescing/debounce rule.

### 3.2 List-level: NOT a subscription — it's a separate notification

- **There is no `resources/subscribe` for "the list."** List-level change is delivered by
  **`notifications/resources/list_changed`** (no params), which a server emits *unsolicited*
  if it advertised `resources.listChanged: true`. The client does not "subscribe" to it; it
  is implied by capability negotiation.
- **Flag:** The task brief and the RFC both speak of "resource **list** updates" as a trigger
  alongside item updates. That is legitimate, but the *mechanism is different*: item updates
  require an explicit `resources/subscribe` per URI and yield `…/updated{uri}`; list updates
  require **no subscribe** and yield `…/list_changed{}` (no uri). agentd's trigger layer must
  treat these as two distinct event sources. The RFC currently blurs them (e.g. "resource /
  resource-list updates" as if one subscription covers both). Corrected model:

  | Trigger | Pre-req | Subscribe call | Notification | Payload |
  |---|---|---|---|---|
  | Item changed | server adv. `resources.subscribe` | `resources/subscribe{uri}` per URI | `notifications/resources/updated` | `{uri}` (+ opt `title`) |
  | List changed | server adv. `resources.listChanged` | none | `notifications/resources/list_changed` | none |

### 3.3 Delivery requires a server→client channel

Notifications are **server-initiated messages**. Over **stdio** they just arrive on the
server's stdout — trivial, and the RFC's default/lightest case works as written. Over
**Streamable HTTP**, server→client notifications require an **SSE stream** the client must be
listening on (an HTTP GET to the MCP endpoint that the server upgrades to
`text/event-stream`, or SSE on a POST response). **Flag:** the RFC's claim that a "tiny
blocking HTTP/1.1 client" suffices for HTTP-transport MCP is **incomplete for reactivity**:
to receive `notifications/resources/updated` from an HTTP MCP server you must maintain a
long-lived SSE GET stream and parse SSE framing (`event:`/`data:`/`id:`, `Last-Event-ID`
resumption, `retry`). A blocking request/response client cannot receive unsolicited
notifications. Reactive + HTTP transport = SSE client is mandatory, not optional.

### 3.4 Verdict on "reactive via subscriptions"

**Yes, the trigger model is spec-supported and idiomatic.** Resources are explicitly
"application-driven," subscriptions are first-class, and an MCP host reacting to
`…/updated` is exactly the intended pattern. The novelty is fine. The corrections are:
(a) notify-then-read, no payload; (b) item vs list are separate mechanisms; (c) can't
subscribe to templates, only concrete URIs; (d) HTTP transport needs a real SSE client.

---

## 4. agentd as an MCP server exposing subscribable session/state (RFC §8)

**Feasible and spec-compliant — with caveats.**

- Exposing run/session/subagent state as **readable resources** (`resources/read`) and
  **subscribable** (`resources/subscribe` + emitting `notifications/resources/updated` when
  state changes) is exactly what the resources capability is for. agentd advertises
  `resources: { subscribe: true, listChanged: true }` and emits `…/updated{uri}` on state
  transitions. **This works and is the right design for agent-to-agent reactivity.**
- Caveat 1 — **payload-less notifications** (§3.1): peer agentd Y subscribes to agentd X's
  `agent://session/123/status`; on change, X emits `…/updated{uri}`; Y must `resources/read`
  to get the new status. Design X's resource granularity so a single read is cheap and
  meaningful.
- Caveat 2 — **custom URI scheme**: `agent://…` is a custom scheme; legal, but only other
  agentd instances will understand its semantics. Fine for self-wiring; not a generic interop
  surface.
- Caveat 3 — **transport** (§7 below): subscribable resources are only useful to a *peer* if
  agentd serves them over a transport the peer can hold an SSE stream on (Streamable HTTP) or
  over stdio (only works if the peer *spawned* agentd as its subprocess). A sibling agentd in
  another pod ⇒ Streamable HTTP server ⇒ the heavier server stack.
- Caveat 4 — **`tools/list_changed` for dynamic scope**: when agentd narrows/grants a
  subagent's tool scope at runtime, and agentd is the server the subagent talks to, agentd
  **SHOULD** emit `notifications/tools/list_changed` (and advertise `tools.listChanged`).
  The RFC's dynamic scoping (§6.3) implies this but never says it.

---

## 5. Sampling — the *correct* mechanism for "a peer uses agentd's intelligence" (RFC §7, §8)

The RFC frames agentd-as-server as letting "*other* MCP clients … wire to it" and the task
brief explicitly calls out "agentd-as-server could let a peer use agentd's intelligence."
**The spec has a first-class feature for exactly this, and the RFC never names it.**

- **`sampling/createMessage`** is a **server→client** request: a *server* asks the *client*
  to run an LLM generation. **Capability is declared by the CLIENT** (`sampling: {}`, or
  `sampling: { tools: {} }` for tool-enabled sampling — SEP-1577, new in 2025-11-25).
- Params: `messages[]` (role `user`/`assistant`, content `text`/`image`/`audio`/`tool_use`/
  `tool_result`), `modelPreferences` (`hints[]`, `costPriority`, `speedPriority`,
  `intelligencePriority`), `systemPrompt`, `maxTokens`, optional `tools[]` + `toolChoice`
  (`{mode: auto|required|none}`). Result: `{ role:"assistant", content, model, stopReason }`
  where `stopReason` ∈ `endTurn` | `toolUse` | …
- **The directionality is the catch.** agentd-as-MCP-*server* cannot *serve* sampling — only
  a client serves sampling. For a peer to "use agentd's intelligence," the relationship must
  be: **the peer acts as the MCP server and agentd acts as the MCP client that fulfils
  `sampling/createMessage` using its configured intelligence endpoint.** That is a coherent
  and powerful pattern (agentd becomes a sampling-capable client / "intelligence provider"),
  but it is the **opposite wiring** from the RFC's "agentd exposes a server; peer connects as
  client." **Recommendation:** add an explicit design note: agentd should optionally declare
  the **`sampling` client capability** on its outbound MCP client connections, so any server
  it connects to (including another agentd-as-server) can request generations from it. This
  is the spec-blessed way to share intelligence between agents, and it reuses agentd's single
  intelligence endpoint. Conversely, if agentd wants its *own* subagents to obtain
  intelligence via MCP rather than a direct LLM transport, the subagent-as-server /
  parent-as-sampling-client shape is available — relevant to open question §14.1.
- **Human-in-the-loop SHOULD:** the sampling spec says there SHOULD be a human able to deny
  sampling requests. agentd is autonomous; document the conscious deviation (auto-approve
  under budget caps) — it is a SHOULD, not a MUST, so this is permissible but should be
  explicit, and gated by the per-call budgets the RFC already defines.

---

## 6. Roots (RFC: not mentioned)

- **`roots/list`** is a **server→client** request; **roots is a CLIENT capability**
  (`roots: { listChanged: true }`). Client returns `{ roots: [ { uri, name? } ] }`; `uri`
  **MUST** be a `file://` URI. Client emits `notifications/roots/list_changed` on change.
- Relevance to agentd:
  - As a **client**, agentd MAY declare `roots` to tell servers (e.g. a filesystem MCP
    server) which directories are in scope. This is a clean, spec-native way to express the
    "tool scope" boundary for filesystem-like servers — complementary to the RFC's
    granted-subset model (§6.3). Worth a sentence.
  - As a **server**, if agentd ever consumes a filesystem-style relationship from a peer, it
    could *request* `roots/list` from that peer. Lower priority.
- **Not a gap that breaks anything**, but roots is the idiomatic "scope of the filesystem"
  signal and the RFC's scoping section would be stronger for referencing it.

---

## 7. Transports (RFC §7.1, §11, §12)

### What the spec defines (2025-11-25)

1. **stdio** — client launches server as subprocess; newline-delimited JSON-RPC over
   stdin/stdout; **no embedded newlines**; server **MUST NOT** write non-MCP to stdout;
   server **MAY** use stderr for *any* logging (clarified in 2025-11-25 — stderr is not
   error-only). Clients SHOULD support stdio whenever possible.
2. **Streamable HTTP** — a **single MCP endpoint** (one URL path) supporting **POST and
   GET**. Client POSTs each JSON-RPC message; server replies either with
   `application/json` (one response) or upgrades to `text/event-stream` (SSE). Client MAY
   open a standalone **GET** SSE stream for unsolicited server→client requests/notifications.
   Required headers/semantics:
   - Client **MUST** send `Accept: application/json, text/event-stream` on POST.
   - **`MCP-Session-Id`**: server MAY assign at init (header on `InitializeResult`); client
     MUST echo it on all subsequent requests; 404 ⇒ client restarts session.
   - **`MCP-Protocol-Version`**: client MUST send on every request post-init; absent ⇒ server
     assumes `2025-03-26`; invalid ⇒ 400.
   - **`Origin` MUST be validated** (DNS-rebinding); invalid ⇒ **HTTP 403** (tightened in
     2025-11-25). Bind to localhost when local.
   - SSE resumability via event `id` + `Last-Event-ID` (GET only); `retry` field respected.

### What's deprecated

- **HTTP+SSE (2024-11-05)** — the old two-endpoint (`/sse` + POST) transport — is
  **deprecated**, replaced by Streamable HTTP. Backwards-compat is a client/server *option*,
  not a requirement.

### Corrections to the RFC

- **Terminology:** §7.1, §11, §12 say "HTTP/SSE." Replace with **"Streamable HTTP"**. "SSE"
  is now an *internal detail* of Streamable HTTP, not a transport name. Citing "the
  deprecated HTTP+SSE" anywhere would be a bug.
- **Minimal client claim (§12):** "a tiny blocking HTTP/1.1 client" is sufficient for a
  *request/response-only* HTTP MCP client that never needs server-initiated messages. The
  moment agentd wants **resource-update notifications over HTTP**, it needs an **SSE-capable
  client** (long-lived GET stream, SSE framing, reconnection). The dependency budget should
  acknowledge this as a feature-flagged addition, or restrict reactive-over-HTTP out of v1
  and keep reactivity to stdio servers (recommended for minimalism). For stdio, notifications
  are free.
- **Minimal server claim (§8, §11, §12):** serving the self-MCP to *external/peer* clients
  over HTTP is **not trivial** — it requires the full Streamable HTTP server surface above
  (POST+GET endpoint, SSE upgrade, session header, protocol-version header, Origin/403,
  resumability). This is more than the RFC's minimalism framing implies. **Recommendation:**
  for v1, serve the self-MCP **only over stdio** (peer/parent spawns agentd as a subprocess)
  and/or a **Unix socket** carrying newline-delimited JSON-RPC (a "stdio-like" framing the
  spec permits as a custom transport). Defer Streamable HTTP serving to a later phase behind
  a flag — which is already roughly the RFC's phase 4, but the *cost* of HTTP serving is
  understated.
- **vsock / unix for intelligence (§7.2)** are *not MCP transports* and don't need to be —
  they carry the LLM wire (OpenAI-compatible), not MCP. No conflict; just don't conflate them
  with MCP transports in prose.

---

## 8. Tasks — the spec feature the RFC should adopt (RFC §5.3, §6.2, §14.1, §14.3)

**New in 2025-11-25 (experimental): `tasks`.** This is the single most relevant feature the
RFC misses. Tasks make a request **durable, pollable, and deferred-result**:

- Requestor adds `params.task = { ttl? }` to a supported request. Receiver immediately
  returns a **`CreateTaskResult`**: `{ task: { taskId, status:"working", createdAt,
  lastUpdatedAt, ttl, pollInterval } }` — **not** the actual result.
- Requestor polls **`tasks/get { taskId }`** (respecting `pollInterval`) until terminal,
  then **`tasks/result { taskId }`** for the real payload (which is exactly the underlying
  result type, e.g. `CallToolResult`). Optional unsolicited **`notifications/tasks/status`**
  (full Task object) may arrive but **MUST NOT** be relied on — polling is the contract.
- Lifecycle: `working` → (`input_required` ↔ `working`) → terminal (`completed` | `failed` |
  `cancelled`). `tasks/list` (paginated) and **`tasks/cancel`** round it out. Task-augmented
  requests are cancelled via **`tasks/cancel`**, *not* `notifications/cancelled`.
- Capability-gated and **per-request-type**: server advertises e.g.
  `tasks.requests.tools.call`; tools further opt in via `execution.taskSupport`
  (`forbidden`(default)|`optional`|`required`). Clients advertise
  `tasks.requests.sampling.createMessage` / `…elicitation.create`.
- Every task-related message carries `_meta["io.modelcontextprotocol/related-task"].taskId`.

### Why agentd should care

- **Long-running MCP tool calls:** agentd's agentic loop will call MCP tools that take
  minutes (builds, deployments, queries). Without tasks, a `tools/call` ties up the request
  and relies on connection liveness + `notifications/progress` (see §9). With tasks, agentd
  can fire-and-poll, survive transport reconnects, and the spec gives it a durable handle —
  directly supporting RFC requirement (8) "detect dead/stuck subprocesses, recover state."
  **Recommendation:** agentd-as-client SHOULD declare and use `tasks` (poll model) for tools
  whose `execution.taskSupport` is `optional`/`required`.
- **agentd-as-server exposing its own long work:** when agentd serves an `exec` or
  "spawn-subagent-and-wait" tool to a peer, it SHOULD expose those with
  `execution.taskSupport: "required"` and back them with tasks. This is the **spec-native
  alternative to the RFC's bespoke `subagent.*` status polling and the open-question §14.1
  control protocol** — a peer gets `taskId`, polls `tasks/get`, retrieves `tasks/result`.
  It does **not** replace the *internal* parent↔child stdio control channel, but it *is* the
  right shape for the *MCP-facing* surface of subagent control.
- **Session durability (§14.3):** tasks' `ttl` + poll model is a ready-made answer for
  exposing warm/suspended reactive sessions to external clients without inventing a protocol.

This is a strong, concrete recommendation: **align the self-MCP's long-running surface with
`tasks` rather than ad-hoc `subagent.status` polling.** It reduces invented surface (RFC's
own goal) by reusing a spec mechanism.

---

## 9. Progress & cancellation (RFC §6.1, §6.2)

- **Progress:** requester attaches `params._meta.progressToken` (string|int, unique across
  active requests) to a request; receiver MAY emit `notifications/progress` with
  `{ progressToken, progress, total?, message? }`. `progress` **MUST** strictly increase.
  Either side may send. agentd's "stream events (thought/tool-call/…) up the control channel"
  (§6.1) is an *internal* concern, but for **MCP tool calls** agentd SHOULD pass a
  `progressToken` so it can render tool progress and **reset request timeouts on progress**
  (the lifecycle spec allows resetting the timeout clock on progress, with a hard ceiling) —
  directly useful for the "stuck subprocess" detection requirement.
- **Cancellation:** `notifications/cancelled { requestId, reason? }`, fire-and-forget, races
  allowed; **`initialize` MUST NOT be cancelled**; task-augmented requests use `tasks/cancel`
  instead. agentd's hard-kill of subagents is OS-level (SIGKILL) and separate, but when it
  abandons an in-flight **MCP** request it SHOULD emit `notifications/cancelled` to let the
  server free resources rather than just dropping the socket.

---

## 10. Control channel as "MCP-flavoured" (RFC §6.2, §14.1)

The RFC keeps the supervisor↔subagent channel "MCP-flavoured (JSON-RPC shapes)" and asks
(§14.1) whether to make it literally MCP. From a protocol-correctness standpoint:

- **Reusing JSON-RPC 2.0 framing is free and wise** (shared codec). But "literally MCP" means
  committing to the full lifecycle (`initialize`/capabilities/`initialized`) and the method
  namespace on a private pipe — overkill for a parent/child link that isn't a discovery
  surface. **Recommendation:** keep the internal channel a *minimal JSON-RPC sibling* (no
  capability negotiation), and reserve *real* MCP for the externally-facing self-MCP server.
  Where the external surface needs "spawn a child and await its result," model it as an MCP
  **tool with `execution.taskSupport: "required"`** (§8) rather than extending the internal
  protocol outward. This cleanly separates "private supervision wire" from "public MCP
  surface" and avoids two half-MCP dialects.

---

## 11. Point-by-point corrections to RFC text

| RFC location | Statement | Status | Correction |
|---|---|---|---|
| §1.3, §5.3 | reactive via `resources/subscribe` + `notifications/resources/updated` | **Correct** | Add: notification carries only `{uri}` (no diff) ⇒ **notify-then-read**; debounce/coalesce reads. |
| §5.3 | watch a `db://query/...` resource for new rows | **Imprecise** | Can't subscribe to a *template*; must resolve concrete URIs (via `resources/list`/template expansion) and subscribe per-URI; or use `notifications/resources/list_changed` for "set changed." |
| brief/§5 | "resource / resource-**list** updates" as one trigger | **Conflation** | Item update = `resources/subscribe`→`…/updated{uri}`. List update = capability-implied `…/list_changed{}` (no subscribe, no uri). Two mechanisms. |
| §7.1 | "`resources/subscribe` + `notifications/resources/updated`" | **Correct names** | Also list: `notifications/resources/list_changed`; both gated by `resources.{subscribe,listChanged}` advertised at init. |
| §7.1, §11, §12 | "HTTP/SSE" transport | **Stale name** | Use **Streamable HTTP**; old **HTTP+SSE is deprecated**. SSE is an internal detail of Streamable HTTP. |
| §12 | "tiny blocking HTTP/1.1 client" suffices for HTTP MCP | **Incomplete** | Receiving notifications over HTTP needs a long-lived **SSE GET stream** + framing + resumption. Blocking req/resp can't get unsolicited notifications. |
| §8, §11 | serve self-MCP over HTTP as a light add-on | **Understated cost** | Streamable HTTP server = single POST+GET endpoint, SSE upgrade, `MCP-Session-Id`, `MCP-Protocol-Version`, `Origin`→403, resumability. Prefer stdio/unix for v1. |
| §8 intro, §7 | peer "uses agentd's intelligence" via agentd-as-server | **Wrong directionality** | Intelligence sharing = **`sampling/createMessage`**, a server→client request; **sampling is a CLIENT capability**. agentd must act as a **sampling-capable client** to provide intelligence to a peer-server. |
| §8 | `subagent.status(handle)` polling, custom | **Reinventing** | Prefer MCP **tasks** (`execution.taskSupport:"required"`, `tasks/get`/`tasks/result`/`tasks/cancel`) for the *external* long-running surface. |
| §6.3 | dynamic tool scope narrowing | **Missing notify** | If agentd serves these tools, advertise `tools.listChanged` and emit `notifications/tools/list_changed` on scope change. |
| §4.1 | "perform the MCP handshake, hold catalogue" | **Correct, underspecified** | Must store per-server negotiated capabilities; gate subscribe/list-changed on them; follow pagination cursors on `*/list`. |
| §7.2 | intelligence over `vsock:`/`unix:`/`https:` | **Correct & non-MCP** | These carry the LLM wire, not MCP; don't conflate with MCP transports. Fine as-is. |
| — (absent) | resource **templates**, **roots**, **tasks**, **sampling-with-tools**, **structured tool output / outputSchema** | **Gaps** | All exist in 2025-11-25; at least acknowledge templates (reactivity limit), sampling (intelligence sharing), tasks (durable work). |

---

## 12. Net recommendations (prioritised)

1. **Pin a protocol version** (target `2025-11-25`; accept `2025-06-18`) and implement
   capability-gating + the version-negotiation/mismatch path. Without this, subscribe and
   list-changed usage is non-conformant against servers that didn't advertise them.
2. **Make the reactive model precise:** notify-then-read (no payload), per-URI subscribe,
   item-vs-list as distinct sources, no template subscription. Specify a read
   coalescing/debounce rule (ties into open question §14.5 routing).
3. **Keep reactivity on stdio for v1**; treat reactive-over-HTTP (SSE client) and
   self-MCP-over-HTTP (full Streamable HTTP server) as later, feature-flagged phases. This
   honors the minimalism bar honestly.
4. **Adopt `tasks`** for both consuming long-running MCP tool calls and exposing agentd's own
   long-running operations to peers — instead of bespoke status polling. Strong fit for the
   stability/recovery requirements.
5. **Use `sampling/createMessage` (client-side capability)** as the spec-native way to share
   agentd's single intelligence endpoint with peer agents; document the auto-approval
   deviation from the human-in-the-loop SHOULD, bounded by existing budgets.
6. **Wire `notifications/progress` into MCP tool calls** (progressToken + timeout reset) and
   emit `notifications/cancelled` when abandoning in-flight MCP requests — both feed the
   dead/stuck detection requirement.
7. **Keep the internal control channel a minimal JSON-RPC sibling** (no MCP lifecycle);
   expose externally-facing supervision as MCP **tools** (task-augmented), not by leaking a
   second MCP dialect.
8. **Reference roots** as the idiomatic filesystem-scope signal alongside the granted-subset
   scoping model.

**Bottom line:** the RFC's MCP-native, subscription-reactive thesis is *sound and
spec-supported*. The corrections are precision (notify-then-read, item-vs-list, templates),
terminology (Streamable HTTP, not HTTP/SSE), an honest accounting of the HTTP server/SSE
client cost, and adoption of three 2025-11-25 features — **tasks, sampling, and (lightly)
roots** — that the design currently reinvents or omits.
