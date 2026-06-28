# RFC 0004: MCP client subset & wire codec

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

agentd's entire action space and reactive surface ride on MCP. Every
capability is a tool on some external MCP server; every reactive trigger is a
`resources/subscribe` + `notifications/resources/updated` round-trip. There are
no built-in tools. So the MCP **client** half is load-bearing, and it must be
implemented with the same minimalism bar as the rest of the binary: `std` +
`serde`/`serde_json` + raw `libc`, no async runtime, no MCP SDK crate.

This RFC specifies (a) the shared JSON-RPC 2.0 **codec** (reader-thread +
pending-request map + notification dispatch) that both the MCP layer and the
supervisor↔subagent control channel (RFC 0005) reuse, and (b) the **MCP client
subset** agentd speaks to external servers: lifecycle, tools, resources,
subscriptions, liveness, and the stdio transport with its shutdown ladder.

This covers assessment §2.5 (the CLIENT half) and the §1.3 protocol
corrections. The self-MCP **server** half — `agent://` resources,
`subagent.*` self-tools, unix-socket serving — lives in RFC 0005 and is
explicitly out of scope here. This document targets MCP **2025-11-25** and
interoperates down to **2024-11-05**.

The corrections from §1.3 that this RFC bakes in, non-negotiably:

1. **Notify-then-read.** `notifications/resources/updated` carries only
   `{uri}` (optionally `title`) — no payload, no diff. We `resources/read` on
   wake. Two round-trips, raceable → debounce/coalesce (the routing rule lives
   in RFC 0008; this RFC delivers the raw event).
2. **Item vs list are distinct mechanisms.** Per-URI `resources/subscribe` →
   `updated{uri}` is *not* `notifications/resources/list_changed{}`. Two event
   sources, never conflated.
3. **Templates are not subscribable.** Only concrete URIs. `resources/templates/list`
   is informational in v1.
4. **`isError` vs JSON-RPC `error` is load-bearing.** `isError:true` is a
   *successful* result → observation fed to the model. A JSON-RPC `error` is a
   protocol/transport failure → retry/abort policy.
5. **We declare NO client capabilities.** No `roots`, `sampling`,
   `elicitation`, or `tasks`. We answer `roots/list` with `{"roots":[]}` and
   reject unsolicited `sampling/createMessage`.

---

## 2. Decision

Implement a single hand-rolled JSON-RPC 2.0 codec, then a capability-gated MCP
client on top of it, over the stdio transport only (v1). Concretely:

- **One reader thread per long-lived readable stream.** Each parses framed
  JSON-RPC messages, classifies them by shape, resolves pending requests via a
  shared `Mutex<HashMap<id, oneshot-ish>>`, and forwards notifications + inbound
  requests as tagged events onto the supervisor's merged `mpsc` (RFC 0002).
- **Writes go behind a per-pipe `Mutex<ChildStdin>`.** Requests allocate a
  monotonic `id`, register a pending slot, write the frame, and block on the
  slot's condvar with a deadline. No async runtime.
- **MCP version pinned to `2025-11-25` outbound**, accepting downgrade to
  `2025-06-18` / `2025-03-26` / `2024-11-05` where our feature use overlaps.
  Every capability we send is gated on what the peer advertised. Every `*/list`
  follows `nextCursor` to exhaustion.
- **stdio transport only for v1.** Full line codec (NDJSON), stderr captured
  into structured logs, ordered shutdown ladder close-stdin → SIGTERM →
  SIGKILL. Streamable HTTP client is deferred (RFC 0013); reactivity-over-HTTP
  requires an SSE GET stream we do not build in v1.
- **The codec is shared** with the control channel (RFC 0005), which differs
  only in framing (length-prefix vs NDJSON). Parse/serialize/classify/dispatch
  are identical; `frame.rs` is the only fork point.

This RFC does not specify routing, debounce, or warm-session continuation —
those consume the events this layer produces and are RFC 0008. This RFC stops
at "a typed event lands on the merged `mpsc`."

---

## 3. Mechanisms

### 3.1 The JSON-RPC 2.0 codec — shared core

Three message kinds on the wire, each `id` a string or integer (never `null`),
each message UTF-8, one object per frame, no batching (removed in 2025-06-18;
we neither send nor accept arrays).

```rust
// json/mod.rs — the swap-to-miniserde isolation point. ALL wire types here.
pub type RequestId = serde_json::Value; // String | Number; never Null on the wire

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: V2,                       // serializes/validates as "2.0"
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
pub struct Notification {                  // no id
    pub jsonrpc: V2,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: V2,
    pub id: RequestId,
    // exactly one of result | error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}
```

**Inbound classification (the dispatch rule), by shape, in this order:**

```text
has "method" and has "id"        → Request  TO us   → answer or error -32601
has "id" and ("result"|"error")  → Response to our request → resolve pending(id)
has "method" and no "id"         → Notification → dispatch by method, ignore unknown
otherwise / parse failure        → log "mcp.frame.malformed", skip the frame (do NOT crash the reader)
```

A malformed or non-UTF-8 frame is *skipped*, not fatal — a single bad line must
not kill the connection. We count it (`mcp.frame.malformed`) and continue.

**Standard error codes** we emit/handle: `-32700` parse, `-32600` invalid
request, `-32601` method not found, `-32602` invalid params, `-32603` internal;
MCP-specific `-32002` resource-not-found (with `data.uri`). `initialize`
version mismatch is `-32602` with `data.{supported,requested}`.

### 3.2 Framing — `frame.rs` (the only fork from the control channel)

```rust
// MCP stdio: NDJSON. One compact JSON object per line, \n-terminated,
// NO embedded newlines, UTF-8. We serialize with serde_json::to_writer
// (compact, never pretty) then write a single b'\n'.
pub fn write_ndjson<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<()>;
// Reader: a BufRead over the child's stdout; read_line; trim trailing \r\n;
// skip empty lines; serde_json::from_slice on the bytes.
pub fn read_ndjson<R: BufRead>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<RawFrame>>;
```

Frame size guard: lines are capped at **16 MiB**; a longer line aborts the
connection (`mcp.frame.oversize`) rather than growing an unbounded buffer.
(The control channel of RFC 0005 uses 4-byte-LE length-prefix framing with the
same 16 MiB cap; both call into identical parse/classify after framing.)

### 3.3 The client connection — reader thread + pending map

```rust
// mcp/client.rs
pub struct McpClient {
    name: String,                          // server registry key
    stdin: Mutex<ChildStdin>,              // write side, NDJSON
    next_id: AtomicI64,                     // monotonic request ids
    pending: Arc<Mutex<HashMap<i64, PendingSlot>>>,
    caps: ServerCapabilities,               // frozen post-initialize
    proto: ProtocolVersion,                 // negotiated
    events: mpsc::Sender<SupEvent>,         // merged supervisor channel (RFC 0002)
    child: Child,                            // for the shutdown ladder
    deadline_default: Duration,
}

struct PendingSlot {
    done: Condvar,
    state: Mutex<Option<Result<serde_json::Value, RpcError>>>,
    progress_token: Option<RequestId>,      // if we opted into progress
    sent_at: Instant,
    timeout: Instant,                       // absolute; reset by progress (§3.10), with a ceiling
}
```

The **reader thread** owns the child's stdout `BufRead`. For each frame it
classifies (§3.1) and:

- **Response** → `pending.lock().remove(&id)`, store the `Ok(result)` /
  `Err(error)` into the slot, `notify_one`. If `id` is unknown (late response
  after we cancelled/timed out) → drop + `mcp.response.orphan` counter.
- **Notification** → translate to a `SupEvent` and `events.send(...)` (see
  §3.8/§3.9 for the per-method mapping). The reader does *not* itself act on
  resource updates; it forwards. Unknown `method` → `mcp.notify.unknown`, drop.
- **Request TO us** (`ping`, `roots/list`, `sampling/createMessage`, …) →
  handled inline per §3.7; the reader writes the response/error directly
  through the `stdin` mutex (these are cheap and synchronous).

EOF on stdout → the reader emits `SupEvent::McpEof { name }` and exits; the
supervisor confirms death via the server's child `waitpid` and tears down per
§3.6. Every pending slot is then resolved with a synthetic
`RpcError{code:-32603,message:"connection closed"}` so blocked callers unblock.

**The send path** (called from the agentic loop's tool dispatch, or the
supervisor for subscribe/ping):

```rust
pub fn call(&self, method: &str, params: Value, timeout: Duration)
    -> Result<Value, CallError>
{
    let id = self.next_id.fetch_add(1, Ordering::Relaxed);
    let slot = self.register_pending(id, timeout);
    self.write_request(id, method, params)?;     // behind stdin Mutex, NDJSON
    self.wait_pending(slot)                       // condvar wait_timeout to slot.timeout
}
```

`CallError` distinguishes the load-bearing cases:

```rust
pub enum CallError {
    Rpc(RpcError),     // JSON-RPC error response → protocol failure (§3.5 policy)
    Timeout,           // no response by absolute deadline → cancel (§3.10) + treat as transport
    Transport(io::Error), // write/EOF/closed → transport failure
}
```

Note: `isError:true` is **not** a `CallError` — it arrives as a successful
`result` and is parsed by the tools layer (§3.5).

### 3.4 Lifecycle: initialize + capabilities + initialized

On connect, before any other request (the spec forbids non-`ping` traffic
before the handshake completes):

```jsonc
// → initialize  (we declare NO client capabilities — empty object)
{ "jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocolVersion":"2025-11-25",
  "capabilities":{},
  "clientInfo":{"name":"agent","title":"agent","version":"1.x"}
}}
// ← result
{ "jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"2025-11-25",
  "capabilities":{
    "resources":{"subscribe":true,"listChanged":true},
    "tools":{"listChanged":true},
    "logging":{}, "prompts":{"listChanged":true}, "completions":{}
  },
  "serverInfo":{"name":"…","version":"…"},
  "instructions":"…"                          // optional; folded into the catalogue prompt
}}
// → notifications/initialized
{ "jsonrpc":"2.0","method":"notifications/initialized" }
```

**`capabilities:{}` is deliberate and final for v1.** We do not need to declare
`roots`/`sampling`/`elicitation`/`tasks` to *use* a server's tools and
resources; we only declare a client capability when we intend to *service* it,
and we service none. This is the single cleanest interop posture and the
injection-surface-minimal one.

```rust
#[derive(Deserialize, Default, Clone)]
pub struct ServerCapabilities {
    #[serde(default)] pub resources: Option<ResourcesCap>,
    #[serde(default)] pub tools: Option<ToolsCap>,
    #[serde(default)] pub logging: Option<serde_json::Value>,
    #[serde(default)] pub prompts: Option<serde_json::Value>,
    // tasks/completions parsed-but-unused in v1
}
#[derive(Deserialize, Default, Clone)]
pub struct ResourcesCap { #[serde(default)] pub subscribe: bool,
                          #[serde(default)] pub list_changed: bool }
#[derive(Deserialize, Default, Clone)]
pub struct ToolsCap { #[serde(default)] pub list_changed: bool }
```

The frozen `ServerCapabilities` is the gate for every subsequent call:

```rust
fn assert_cap(&self, need: Cap) -> Result<(), CapMissing> { … }
// resources/subscribe        requires resources.subscribe == true
// expecting list_changed      requires resources.list_changed / tools.list_changed
// otherwise: do not send; degrade (poll, or surface "reactive unavailable")
```

**Version negotiation:** we send `2025-11-25`. If the server echoes it, done.
If it returns another version we support (`2025-06-18`/`2025-03-26`/`2024-11-05`),
we accept and record it on `self.proto`, then gate version-specific shapes off
it (e.g. structured tool output / `outputSchema` are ≥2025-06-18). If it
returns a version we cannot speak → log `mcp.connect.fail`, disconnect, and the
supervisor decides whether the server was *required* (exit code 6 per RFC 0011).
A handshake that does not complete within a bounded `init_timeout` (default
**10s**) is a connect failure.

### 3.5 Tools: list (+ pagination), call, content, isError

**`tools/list` with cursor pagination — we drain all pages, always:**

```rust
pub fn list_tools(&self) -> Result<Vec<Tool>, CallError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = cursor.map(|c| json!({ "cursor": c })).unwrap_or(json!({}));
        let r: ListToolsResult = from_value(self.call("tools/list", params, self.deadline_default)?)?;
        out.extend(r.tools);
        match r.next_cursor { Some(c) => cursor = Some(c), None => break }
    }
    Ok(out)
}
```

Cursors are **opaque** — we never parse or persist them across reconnects; we
loop until `nextCursor` is absent. A server that returns the same cursor twice
is broken; we cap the page loop at **1024 iterations** and abort
(`mcp.list.cursor_loop`).

```rust
#[derive(Deserialize)]
pub struct Tool {
    pub name: String,                       // 1–128 chars [A-Za-z0-9_.-], case-sensitive
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub description: Option<String>,   // UNTRUSTED (tool poisoning — RFC 0012)
    pub input_schema: serde_json::Value,    // JSON Schema object, never null; dialect 2020-12 default
    #[serde(default)] pub output_schema: Option<serde_json::Value>,
    #[serde(default)] pub annotations: Option<serde_json::Value>, // UNTRUSTED
    // execution.taskSupport parsed-but-ignored in v1 (tasks deferred)
}
```

Tool descriptions/annotations are server-controlled and therefore **untrusted**
(ASI01 tool poisoning) — surfaced/logged for operator audit, never auto-trusted
(RFC 0012). On `notifications/tools/list_changed` (gated on `tools.listChanged`)
we re-issue `list_tools` and refresh the catalogue.

**`tools/call` and the content array:**

```jsonc
// → call
{ "jsonrpc":"2.0","id":2,"method":"tools/call",
  "params":{ "name":"get_weather", "arguments":{ "location":"NYC" },
             "_meta":{ "io.modelcontextprotocol/run-id":"<run_id>" } } }
// ← result (success — note isError lives INSIDE result)
{ "jsonrpc":"2.0","id":2,"result":{
  "content":[ { "type":"text","text":"22.5°C" } ],
  "isError":false,
  "structuredContent":{ "temperature":22.5 }       // present iff tool declared outputSchema (≥2025-06-18)
}}
```

```rust
#[derive(Deserialize)]
pub struct CallToolResult {
    #[serde(default)] pub content: Vec<ContentBlock>,
    #[serde(default)] pub is_error: bool,
    #[serde(default)] pub structured_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { data: String, mime_type: String },        // base64
    Audio { data: String, mime_type: String },         // base64
    ResourceLink { uri: String, #[serde(default)] name: Option<String>,
                   #[serde(default)] description: Option<String>,
                   #[serde(default)] mime_type: Option<String> },
    Resource { resource: EmbeddedResource },            // {uri, mimeType, text|blob}
    #[serde(other)] Unknown,                            // forward-compat: never panic on new types
}
```

All blocks may carry optional `annotations` (`audience`, `priority`,
`lastModified`) — parsed-but-ignored in v1.

**THE load-bearing distinction (§1.3 #7):**

| Wire shape | Meaning | agentd handling |
|---|---|---|
| `result.isError == true` | tool-execution error, *successful* JSON-RPC | feed the `content[]` to the model as an **observation**; the model self-corrects; **consumes a step** (RFC 0007) |
| JSON-RPC `error` (top-level) | protocol/transport fault (unknown tool, bad params, server fault) | a `CallError::Rpc` → **transport/abort policy** (§3.5 error policy below) |

```rust
match self.call("tools/call", params, timeout) {
    Ok(v) => {
        let r: CallToolResult = from_value(v)?;
        // r.is_error → Observation::ToolError(r.content); else Observation::ToolOk(r)
        // structuredContent validated against outputSchema if present (best-effort; log mismatch)
    }
    Err(CallError::Rpc(e))     => /* protocol error: not retriable as a tool call */,
    Err(CallError::Timeout)    => /* cancel (§3.10), then transport-retry per policy */,
    Err(CallError::Transport(_)) => /* bounded backoff+jitter retry, else abort exit 6 */,
}
```

**Error policy (the §2.6 taxonomy, this layer's slice):** a JSON-RPC `error` on
`tools/call` (e.g. `-32602` invalid params, `-32601` unknown tool) is *not* a
transport fault — it is surfaced to the loop as a tool failure observation
(recoverable, step-consuming) **unless** it indicates the server is unusable
(`-32603` repeated, or connection-level), in which case it escalates to the
transport-retry/abort path. Transient transport errors (write failure, timeout,
EOF) → bounded retry (backoff+jitter, default 3 attempts) before surfacing as
fatal (exit 6, "required MCP server died"). The retry/abort *policy* knobs are
shared with RFC 0007; this RFC supplies the classification.

### 3.6 Resources: list + read

**`resources/list`** (paginated, drained identically to `tools/list`):

```rust
#[derive(Deserialize)]
pub struct Resource {
    pub uri: String,
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub description: Option<String>,
    #[serde(default)] pub mime_type: Option<String>,
    #[serde(default)] pub size: Option<u64>,
}
```

The agentic loop injects a **compact catalogue** (URIs + descriptions +
size/mime; never bodies — §2.6 "list = awareness, read = attention"); bodies
come via `resources/read`. The catalogue is capped and prefix-summarized if a
server exposes thousands (RFC 0007 owns the catalogue policy; this RFC just
provides the list).

**`resources/read`** — `contents` is an **array** (one URI may yield several
items, e.g. a directory); text in `text`, binary base64 in `blob`:

```jsonc
{ "jsonrpc":"2.0","id":2,"method":"resources/read","params":{"uri":"file:///x"} }
{ "jsonrpc":"2.0","id":2,"result":{ "contents":[
  { "uri":"file:///x","mimeType":"text/x-rust","text":"…" },
  { "uri":"file:///y","mimeType":"image/png","blob":"<base64>" }
]}}
```

```rust
#[derive(Deserialize)]
pub struct ReadResourceResult { pub contents: Vec<EmbeddedResource> }
#[derive(Deserialize)]
pub struct EmbeddedResource {
    pub uri: String,
    #[serde(default)] pub mime_type: Option<String>,
    #[serde(default)] pub text: Option<String>,    // text variant
    #[serde(default)] pub blob: Option<String>,    // base64 binary variant
}
```

Resource-not-found → `-32002` with `data.uri` → surfaced as a tool/observation
error, not a transport abort. `resources/templates/list` is read-but-treated as
**informational only** — templates are *not subscribable* (§1.3 #3), and we
react only to concrete URIs.

### 3.7 Inbound requests we did not solicit (declare-nothing posture)

Because we declared no client capabilities, a conformant server should never
ask us for `roots`/`sampling`/`elicitation`. But we must be robust if one does:

```rust
fn handle_inbound_request(&self, req: Request) {
    match req.method.as_str() {
        "ping"                  => self.reply_result(req.id, json!({})),       // always answer
        "roots/list"            => self.reply_result(req.id, json!({"roots":[]})), // empty list
        "sampling/createMessage"
        | "elicitation/create"  => self.reply_error(req.id, -32601,
                                       "method not found: agentd declares no client capabilities"),
        _                       => self.reply_error(req.id, -32601, "method not found"),
    }
}
```

`ping` is answered promptly with `{}` regardless of state — it is our and the
server's liveness probe (§3.9). `roots/list` gets `{"roots":[]}` (cheaper than
arguing; we simply expose no filesystem scope). `sampling/createMessage` is
**rejected** with `-32601` — agentd does not act as a sampling-capable client
in v1 (the intelligence-sharing direction is deferred to RFC 0013, v2). Every
other unsolicited method → `-32601`.

### 3.8 Reactive core: subscribe / unsubscribe / updated / list_changed

**Two distinct mechanisms — never one (§1.3 #2):**

| Trigger | Pre-req cap | Subscribe call | Notification | Payload |
|---|---|---|---|---|
| Item changed | `resources.subscribe` | `resources/subscribe{uri}` per concrete URI | `notifications/resources/updated` | `{uri}` (+ opt `title`) |
| List changed | `resources.listChanged` | **none** (capability-implied) | `notifications/resources/list_changed` | none |
| Tools changed | `tools.listChanged` | none | `notifications/tools/list_changed` | none |

```jsonc
// → subscribe  (only if caps.resources.subscribe; one concrete URI, never a template)
{ "jsonrpc":"2.0","id":4,"method":"resources/subscribe","params":{"uri":"file:///x"} }
// ← result {}  (empty)
// → unsubscribe
{ "jsonrpc":"2.0","id":5,"method":"resources/unsubscribe","params":{"uri":"file:///x"} }
// ← result {}
// ← server-initiated:
{ "jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file:///x"} }
{ "jsonrpc":"2.0","method":"notifications/resources/list_changed" }
```

The reader thread maps each to a typed `SupEvent` and forwards it — it does
**not** read or route:

```rust
pub enum SupEvent {
    ResourceUpdated   { server: String, uri: String },     // → notify-then-read happens in RFC 0008
    ResourceListChanged { server: String },
    ToolsListChanged  { server: String },
    McpLog            { server: String, level: SyslogLevel, logger: Option<String>, data: Value },
    McpProgress       { server: String, token: RequestId, progress: f64,
                        total: Option<f64>, message: Option<String> },
    McpEof            { server: String },
    // … control-channel / signal / timer variants live in RFC 0002
}
```

**Notify-then-read (§1.3 #1) is NOT done in this layer.** This RFC delivers the
bare `ResourceUpdated{uri}` event. The router (RFC 0008) debounces/coalesces and
issues the follow-up `resources/read` — because the notification carries no
diff, the consumer must read current state, and the read can race a subsequent
update (hence at-least-once + re-read-current-state convergence, RFC 0008).
This RFC's only obligation is: deliver every `updated` exactly as received,
tagged with `server` + `uri`, in order.

**On reconnect** (driven by RFC 0003 rebuild+reconcile): re-issue every declared
`resources/subscribe`, then synthesize one coalesced `ResourceUpdated` per
watched URI so a change missed while disconnected is not lost (edge → level
across the restart boundary). This RFC exposes the re-subscribe primitive; RFC
0003 sequences it.

A server lacking `resources.subscribe` → we **do not** send subscribe; we log
`mcp.subscribe.unsupported` and surface that the reactive trigger is unavailable
for that server (the operator/router degrades to polling or drops the route).

### 3.9 Liveness: ping (both ways) + notifications/cancelled

```jsonc
{ "jsonrpc":"2.0","id":"p7","method":"ping" }   // no params
{ "jsonrpc":"2.0","id":"p7","result":{} }        // empty, prompt
```

agent **pings outbound** on an interval (default `mcp_ping_interval = 30s`)
when a connection is otherwise idle, with a per-ping timeout (default **10s**).
N consecutive missed pongs (default **3**) → the server is declared stale → the
shutdown ladder (§3.11). This is the stdio-MCP slice of the dead/stuck detection
in RFC 0003; the classifier (EOF × pong) is RFC 0003's, fed by the
`McpEof`/missed-pong events this layer raises. We **answer** inbound `ping`
unconditionally (§3.7).

**Cancellation — we send `notifications/cancelled` when abandoning an in-flight
request** (deadline trip, step-budget exhaustion, subagent kill) so we don't
leak in-flight tool work on a server we keep using:

```jsonc
{ "jsonrpc":"2.0","method":"notifications/cancelled",
  "params":{ "requestId":"123","reason":"deadline" } }
```

Rules we honor: only cancel an in-flight request we issued *in the same
direction*; **never cancel `initialize`**; after sending, we stop waiting on the
pending slot and **ignore any late response** (drop + counter); we tolerate the
race where the response already left. We *consume* an inbound
`notifications/cancelled` (for a request the server sent us — rare given our
posture) by dropping the in-flight reply.

### 3.10 Progress + message (consume only)

**Progress (consume):** when we issue a long tool call we MAY opt in by putting
`_meta.progressToken` (unique per active request) in the request params:

```jsonc
{ "jsonrpc":"2.0","method":"notifications/progress",
  "params":{ "progressToken":"abc","progress":50,"total":100,"message":"…" } }
```

On each `notifications/progress` matching an outstanding `progressToken`, we
**reset that request's timeout clock** — but with an **absolute ceiling**
(`mcp_call_max = 600s` default) the progress cannot push past. `progress` must
strictly increase; a non-increasing value is logged (`mcp.progress.nonmono`) and
ignored. We do not *emit* progress from the client.

```rust
// in PendingSlot, on McpProgress for slot.progress_token:
slot.timeout = min(now + self.progress_grace,      // e.g. now + 60s
                   slot.sent_at + self.call_max);   // hard ceiling, never exceeded
```

**Message (consume):** `notifications/message` (`{level, logger?, data}`,
RFC 5424 severities `debug..emergency`) is folded into our JSON-lines logs
(event `mcp.message`, tagged by `server`), level-mapped. We never *emit* it from
the client. `logging/setLevel` is optional and not sent in v1.

### 3.11 stdio transport — framing, stderr, shutdown ladder

**Launch & framing.** The client launches the server as a subprocess
(`Command` with `stdin/stdout = piped`, `stderr = piped`). Server reads
JSON-RPC on stdin, writes on stdout. stdout is **sacred** — only MCP messages,
one compact object per `\n`-delimited line, no embedded newlines, UTF-8. We
serialize compact and append `\n`; we split inbound on `\n`. **An MCP server
launch command is trusted config** — never built from model/server-controlled
strings (RFC 0012).

**stderr capture.** stderr is free-form per spec — we **MUST NOT** treat stderr
output as an error signal. A dedicated reader thread drains the server's stderr
line-by-line into our structured logs (event `mcp.stderr`, tagged by `server`,
truncated per-line), so a chatty server cannot block on a full stderr pipe and
its diagnostics land in our log stream.

**Shutdown ladder (client-initiated, ordered — matches the spec's stdio
sequence and §2.5):**

```text
1. close the child's stdin  (drop ChildStdin → EOF; the polite signal to exit)
2. wait for exit up to  mcp_shutdown_grace  (default 5s; waitpid/try_wait, RFC 0003)
3. still alive?  →  kill(pid, SIGTERM)        wait  mcp_sigterm_grace (default 2s)
4. still alive?  →  kill(pid, SIGKILL)
5. waitpid until reaped (or ECHILD)            — no zombie left
```

`SIGKILL`/`SIGTERM` use the exact syscalls (`libc::kill(pid, SIGTERM)` /
`SIGKILL`); the MCP server is a single child (not its own process group from
agentd's view), so we signal the pid directly here. Subagent process-group kill
(`killpg`) is a different concern (RFC 0003/0009). The whole MCP-server drain is
bounded and counts inside `AGENT_DRAIN_TIMEOUT` (RFC 0011). The reader thread's
`McpEof` confirms exit. A server MAY self-initiate shutdown by closing its
stdout (we observe EOF and run from step 2).

**v1 transport is stdio only.** Streamable HTTP client (and therefore
reactivity-over-HTTP, which requires a long-lived SSE GET stream + `event:`/
`data:`/`id:` framing + `Last-Event-ID` resumption) is **deferred** (RFC 0013).
The hand-rolled HTTP/1.1+SSE client (RFC 0006's `net/http.rs`) carries the LLM
wire, not MCP; it is *not* reused as an MCP transport in v1. The old 2024-11-05
HTTP+SSE two-endpoint transport is **never** implemented.

### 3.12 Server registry

```rust
// mcp/registry.rs
pub struct Registry { servers: HashMap<String, Arc<McpClient>> }   // name → handle
impl Registry {
    pub fn resolve(&self, server: &str) -> Option<&Arc<McpClient>>;
    pub fn caps(&self, server: &str) -> Option<&ServerCapabilities>;
    // tool routing: a scoped subagent sees a SUBSET (RFC 0009/0012); resolve
    // (server, tool) → McpClient::call("tools/call", …). Names are server-qualified
    // to disambiguate same-named tools across servers.
}
```

Config (`--mcp name=cmd`, `--mcp-config FILE`) is RFC 0011's; the registry holds
the post-handshake handles and per-server frozen capabilities. A *required*
server failing handshake → exit 6 (RFC 0011); an optional one → logged and
omitted from the catalogue.

---

## 4. Interactions with other RFCs

- **RFC 0002 (reactor/concurrency):** the codec's reader threads forward every
  inbound notification/request and lifecycle event onto the merged `mpsc` this
  RFC's `SupEvent`s flow into; writes are behind the per-pipe `Mutex` invariant
  ("never block on an untrusted source; abandon-don't-interrupt"). The
  outbound-ping interval and per-call deadlines are armed off the supervisor's
  `recv_timeout` timer.
- **RFC 0003 (supervision/recovery):** consumes `McpEof` + missed-pong signals
  into the EOF×pong classifier; sequences reconnect → re-subscribe →
  read-after-subscribe (this RFC supplies the primitives, §3.8). The stdio
  shutdown ladder here is the MCP-server-specific instance of RFC 0003's bounded
  kill discipline.
- **RFC 0005 (self-MCP server & control protocol):** **shares this exact codec**
  (parse/classify/dispatch/pending-map); diverges only in `frame.rs`
  (length-prefix vs NDJSON) and in having no MCP lifecycle on the private
  control pipe. The server half (answering `initialize`, emitting
  `agent://…updated`) is RFC 0005's, not this one's.
- **RFC 0006 (intelligence transport):** orthogonal — carries the LLM wire
  (OpenAI-compatible/anthropic), not MCP. Its `net/http.rs` is not an MCP
  transport in v1. Do not conflate.
- **RFC 0007 (agentic loop):** consumes `CallToolResult` — the `isError` →
  observation vs `CallError::Rpc` → failure split (§3.5) is exactly the loop's
  error taxonomy slice; the resource catalogue (list=awareness, read=attention)
  is assembled there from §3.6's data.
- **RFC 0008 (reactive routing):** consumes `SupEvent::ResourceUpdated` /
  `ResourceListChanged` / `ToolsListChanged`; owns notify-then-read,
  debounce/coalesce, exactly-one-owner routing, and the follow-up
  `resources/read`. This RFC stops at delivering the raw event.
- **RFC 0011 (config/exit):** server config + `_meta` RUN_ID propagation into
  `tools/call`; required-server handshake failure → exit 6; drain budget bounds
  the shutdown ladder.
- **RFC 0012 (security):** all server content (tool descriptions, annotations,
  resource bodies) is untrusted; SSRF/transport hardening; launch commands are
  trusted config, never model-derived.

---

## 5. Non-goals / Deferred

Explicit DEFER set for the MCP **client** (per §2.5):

- **Streamable HTTP client transport** (and reactivity-over-HTTP via SSE GET
  stream, `Last-Event-ID` resumability, multi-stream fan-out) — RFC 0013.
- **The deprecated 2024-11-05 HTTP+SSE two-endpoint transport** — never.
- **`prompts/*`** — never call as client; ignore a server that offers them.
- **`sampling/createMessage`** — do not declare `sampling`; **reject** inbound
  with `-32601`. Intelligence-sharing-as-client is RFC 0013 (v2).
- **`roots/*`** — do not declare `roots`; answer `roots/list` with
  `{"roots":[]}` if asked.
- **`elicitation/*`** and **`completion/complete`** — interactive UX, not
  headless; reject/ignore.
- **`tasks/*`** (task-augmented durable requests) — do not declare; do not use
  the poll model; fall back to plain request/response + progress + cancel.
  `execution.taskSupport` on tools is parsed-but-ignored. RFC 0013.
- **`logging/setLevel`** — optional, not sent in v1 (we still *consume*
  `notifications/message`).
- **`structuredContent`/`outputSchema` validation** beyond best-effort: we parse
  and surface `structuredContent` but do not hard-fail on schema mismatch in v1.
- **Emitting** progress/message/cancelled-consumption nuance beyond §3.9–3.10 is
  the *server* half — RFC 0005.

The self-MCP **server** subset (lifecycle answer, `subagent.*` tools,
subscribable `agent://` resources, unix-socket serving) is entirely RFC 0005.

---

## 6. Open items

None blocking. Two knobs are tuning-only, not design-open:

- **Default outbound ping interval / missed-pong threshold** (30s / 3) and the
  **progress grace vs absolute call ceiling** (60s / 600s) are starting defaults;
  they may be retuned against real servers in the M7 conformance pass without
  changing any wire shape or state machine.
- Whether `-32603` from a server should escalate to transport-abort after K
  repeats or stay an observation is a policy threshold shared with RFC 0007;
  default K=3 here, owned jointly.
