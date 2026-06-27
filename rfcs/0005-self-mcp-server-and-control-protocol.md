# RFC 0005: Self-MCP server & supervisor↔subagent control protocol

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

> **A2A alignment (RFC 0020).** RFC 0020 (A2A-over-vsock) makes **A2A the
> standards-aligned PRIMARY external agent surface** — the manifest *is* an Agent
> Card, a run/subagent *is* an A2A Task, and `SendMessage`/`GetTask`/`CancelTask`
> are spawn/status/cancel. The served self-MCP `subagent.*` surface specified here
> is reclassified as the **compat** surface for MCP-ecosystem peers: kept and
> feature-gated, **not** removed (RFC 0020 §2.8, §3, §8). The **private
> supervisor↔subagent control protocol** (§4 — the length-framed channel) is
> **UNAFFECTED**: it is neither MCP nor A2A, and A2A has no process supervision.

---

## 1. Problem / Context

agentd carries two JSON-RPC surfaces that are easy to conflate and must not be:

1. **The public self-MCP server** (assessment §2.5 SERVER half, §2.3). agentd
   is symmetric: as well as being an MCP *client* to external servers (RFC
   0004), it *is* an MCP server. This is what makes agentd composable — a
   parent agentd, a peer agentd, or a driving harness `initialize`s against
   agentd and gets a real MCP catalogue: tools to spawn/steer subagents, tools
   to read and subscribe to state, an optional gated `exec`, and a set of
   **subscribable `agentd://` state resources** whose `notifications/resources/updated`
   emissions are the substrate for agent-to-agent reactivity and for
   async-subagent completion (assessment §2.6, §2.7). This surface is real MCP,
   capability-negotiated, spec-conformant against 2025-11-25.

2. **The private supervisor↔subagent control protocol** (assessment §2.3). When
   the supervisor re-execs itself in subagent mode (RFC 0009), it owns the
   child's stdio pipes. Over those pipes runs a **minimal JSON-RPC sibling
   protocol — deliberately NOT literal MCP**: no `initialize`/capabilities
   handshake (a private parent↔child link is not a discovery surface), and a
   **different framing** (length-prefixed, not NDJSON) because control payloads
   carry newline-bearing blobs (instructions, context seeds, distilled results).
   It shares the codec (serde JSON-RPC types) with the MCP layer but nothing
   else.

The load-bearing rule, stated up front and repeated in §6: **external
supervision is exposed *only* as MCP self-tools; the internal control protocol
is never leaked outward.** A peer that wants to spawn or steer a subagent calls
the `subagent.*` MCP tools; it never sees the length-framed downward/upward
control messages. This keeps "two half-MCP dialects" from drifting (assessment
§5 risk 10) and keeps the private wire free to evolve.

This RFC specifies both surfaces concretely: wire shapes, Rust type sketches,
the resource state machine and its `updated` emission rule, the framing codecs,
the threading model, and the defaults. It does **not** re-derive the MCP client
subset (RFC 0004), the routing/spawn-vs-continue rule (RFC 0008), the spawn
payload contents and caps (RFC 0009), or the loop internals (RFC 0007); it
references them.

---

## 2. Decision

- **The self-MCP server** speaks MCP 2025-11-25 (interop down to 2024-11-05 per
  RFC 0004's negotiation), declaring `tools:{listChanged:true}` and
  `resources:{subscribe:true,listChanged:true}` and nothing else. It exposes the
  tools `subagent.spawn` / `subagent.send` / `subagent.cancel` / `subagent.status`,
  `subscribe` / `unsubscribe`, `resource.read`, and a gated `exec`; and a tree
  of readable + subscribable `agentd://…` resources for session / run / subagent
  state. It serves over **stdio always**, and over a **unix socket** (NDJSON,
  stdio-like framing) when `--serve-mcp unix:PATH` is given. **Streamable HTTP
  serving is DEFERRED** (assessment §2.5, §2.13; RFC 0013) — the full
  POST+GET / SSE-upgrade / `MCP-Session-Id` / `Origin`→403 / resumability
  surface is out of v1.

- **The control protocol** is a minimal JSON-RPC 2.0 sibling over the subagent's
  stdio pipes: **4-byte little-endian length prefix + UTF-8 JSON payload, 16 MiB
  cap**. Downward (supervisor→child): the spawn payload, then `pause` / `resume` /
  `cancel` / `inject` / `ping`. Upward (child→supervisor): `lifecycle` / `event` /
  `usage` / `result` / `pong`. The child's **control reader runs on a dedicated
  thread, decoupled from the agentic loop**, so ping/pong liveness survives a
  long in-flight tool or model call (assessment §2.3, §2.8).

- The two share `crates/agentd/src/json/` (serde JSON-RPC types) and the
  `frame.rs` module, which provides **both** `read_line`/`write_line` (NDJSON,
  for MCP stdio + unix) and `read_frame`/`write_frame` (length-prefixed, for the
  control channel). Lift `read_frame`/`write_frame` from the retired
  `intelligence/protocol.rs` (assessment §2.3).

---

## 3. Mechanisms — the self-MCP SERVER

### 3.1 Lifecycle and declared capabilities

The self-MCP server answers `initialize`, declares its capabilities, and accepts
`notifications/initialized`, per RFC 0004's codec and the MCP 2025-11-25
lifecycle. It pins `protocolVersion: "2025-11-25"` and runs the standard
downgrade path (echo if the peer matches; else offer ours; peer disconnects if
incompatible).

`initialize` result (the only capabilities we declare):

```jsonc
{ "jsonrpc":"2.0","id":1,"result":{
  "protocolVersion":"2025-11-25",
  "capabilities":{
    "tools":     { "listChanged": true },
    "resources": { "subscribe": true, "listChanged": true }
  },
  "serverInfo":{ "name":"agentd", "title":"agentd",
                 "version":"1.x" },
  "instructions":"agentd self-MCP: spawn/steer subagents, read+subscribe agentd:// state."
}}
```

We declare **no** `prompts`, `logging`, `completions`, or `tasks` server
capabilities (all deferred — assessment §2.5). We answer `ping` and accept
`notifications/cancelled` (§3.7).

Rust shape for the served capability set:

```rust
struct SelfServerCaps;
impl SelfServerCaps {
    const PROTOCOL: &str = "2025-11-25";
    fn declared() -> ServerCapabilities {
        ServerCapabilities {
            tools:     Some(ToolsCap     { list_changed: true }),
            resources: Some(ResourcesCap { subscribe: true, list_changed: true }),
            prompts: None, logging: None, completions: None, tasks: None,
        }
    }
}
```

### 3.2 Tool surface — `tools/list`

The served `tools/list` returns the catalogue below (paginated only if a future
gated set grows; v1 returns one page, `nextCursor` absent). Each tool's
`inputSchema` is JSON Schema 2020-12. The *available* set is gated per caller:
when a caller's scope narrows (RFC 0009/0012 — e.g. a child's grant excludes
`exec`), the server emits `notifications/tools/list_changed` and the caller
re-issues `tools/list` (assessment §2.5; review note §4 caveat 4).

| Tool | Purpose | Sync/async |
|---|---|---|
| `subagent.spawn` | create a child subagent from a rich spawn payload (RFC 0009) | sync default; `{async,detach}` (M3, RFC 0008/0009) |
| `subagent.send` | inject an instruction/event into a warm subagent session | sync ack |
| `subagent.cancel` | request graceful cancel (→ kill ladder, RFC 0003) of a subtree | sync ack |
| `subagent.status` | read a handle's status + usage snapshot | sync |
| `subscribe` | subscribe the caller to an MCP resource `(server,uri)`; self-subscribe ⇒ self-schedule (RFC 0008) | sync ack |
| `unsubscribe` | drop a subscription | sync ack |
| `resource.read` | read an MCP resource body the caller is aware of (list-vs-read, RFC 0007) | sync |
| `exec` | gated shell exec under the subtree kill ladder + caps (RFC 0012) | sync; off by default |

`subagent.spawn` schema (abridged; full payload semantics in RFC 0009):

```jsonc
{ "name":"subagent.spawn",
  "title":"Spawn subagent",
  "description":"Create a child subagent with a narrowed tool scope and context seed.",
  "inputSchema":{ "type":"object",
    "properties":{
      "instruction":   {"type":"string"},
      "output_contract":{"type":"object"},          // objective/format/boundaries (RFC 0009)
      "context_seed":  {"type":"array","items":{"type":"object"}},
      "tool_scope":    {"type":"array","items":{"type":"string"}}, // subset of caller's scope
      "limits":        {"type":"object"},            // steps/tokens/deadline_ms
      "async":         {"type":"boolean","default":false},   // M3
      "detach":        {"type":"boolean","default":false}    // M3
    },
    "required":["instruction"],
    "additionalProperties":false }}
```

`subagent.spawn` (sync) returns a `CallToolResult` whose `structuredContent` is
the distilled result + terminal status + usage:

```jsonc
{ "jsonrpc":"2.0","id":7,"result":{
  "content":[{"type":"text","text":"{…distillate…}"}],
  "structuredContent":{
    "handle":"0.2",
    "status":"completed",                  // terminal-status enum (RFC 0007)
    "usage":{"tokens_in":1234,"tokens_out":456,"steps":9},
    "result":{ /* distilled structured value, ~1–2k tokens */ }
  }}}
```

`subagent.spawn{async:true}` returns immediately with a handle and a pointer to
the completion resource the caller may subscribe to (closing the reactive loop —
assessment §2.7; RFC 0008):

```jsonc
{ "result":{
  "content":[{"type":"text","text":"spawned handle 0.2 (async)"}],
  "structuredContent":{
    "handle":"0.2", "status":"working",
    "result_resource":"agentd://subagent/0.2/result"   // subscribe to learn completion
  }}}
```

**Critical invariants enforced at the tool boundary (the single spawn
chokepoint — assessment §2.7):** the child's **depth is minted by the supervisor
from the caller's handle, never read from the request**; `tool_scope` must be a
subset of the caller's current scope (monotonic narrowing); a spawn that would
breach `max_depth` / `max_children` / `max_total_subagents` / the spawn-rate
token bucket / the tree token ceiling is **refused as a tool result**
(`isError:true`), never a crash:

```jsonc
{ "result":{ "isError":true,
  "content":[{"type":"text",
    "text":"spawn refused: max_depth=4 reached at handle 0.2.1.3"}] }}
```

Distinguish (assessment §1.3 item 7, review §2.4): a **refusal/cap/scope** error
is `isError:true` inside a successful result so the caller's model adapts; a
**malformed `tools/call`** (unknown tool, bad params) is a JSON-RPC `error`
(`-32601`/`-32602`).

`exec` is present in `tools/list` **only when** `--enable-exec` is set and the
target binary exists (assessment §2.11; RFC 0012). When absent it is simply not
listed (capability absence, not a runtime error).

### 3.3 Subscribable `agentd://` state resources

The server exposes session/run/subagent state as readable **and subscribable**
resources under the custom `agentd://` scheme (legal per RFC 3986; semantics
understood only by other agentd instances — review §4 caveat 2). This is the
mechanism for agent-to-agent reactivity: a peer or parent subscribes to one of
our state URIs, and on each state transition we emit
`notifications/resources/updated{uri}` — the peer then `resources/read`s to
learn the new state (**notify-then-read**, no payload in the notification;
assessment §1.3 item 1).

Canonical resource tree (`resources/list`):

| URI | Body (on `resources/read`) |
|---|---|
| `agentd://run/{run_id}` | run-level status, mode, root handle, aggregate usage, exit-disposition |
| `agentd://session/{session_id}` | warm-session status, current turn, last activity |
| `agentd://subagent/{handle}` | per-node status, depth, scope summary, usage, last terminal status |
| `agentd://subagent/{handle}/result` | distilled result once terminal (the async-completion handle) |

`resources/list` (one page; cap + prefix-summarize if a deep tree explodes —
assessment §2.6 "list vs read"):

```jsonc
{ "result":{ "resources":[
  {"uri":"agentd://run/01J…","name":"run 01J…","mimeType":"application/json"},
  {"uri":"agentd://subagent/0","name":"root subagent","mimeType":"application/json"},
  {"uri":"agentd://subagent/0.2","name":"subagent 0.2","mimeType":"application/json"},
  {"uri":"agentd://subagent/0.2/result","name":"subagent 0.2 result","mimeType":"application/json"}
]}}
```

`resources/read` returns the standard `contents[]` array (RFC 0004), one JSON
text item:

```jsonc
{ "result":{ "contents":[
  {"uri":"agentd://subagent/0.2","mimeType":"application/json",
   "text":"{\"handle\":\"0.2\",\"status\":\"working\",\"depth\":1,\"usage\":{…}}"}
]}}
```

Not-found ⇒ `-32002` with `data.uri`.

**Subscription bookkeeping.** Per-URI subscriptions are tracked per connected
peer:

```rust
struct SelfResourceState {
    /// concrete agentd:// URI -> set of peer connection ids subscribed to it
    subs: HashMap<String, HashSet<ConnId>>,
    /// peers that negotiated resources.subscribe at initialize
    sub_capable: HashSet<ConnId>,
}
```

`subscribe`/`unsubscribe` (MCP `resources/subscribe` / `resources/unsubscribe`,
`params:{uri}`) return `{}`. We accept a subscribe only to a **concrete** URI we
list (never a template — there is no template subscribe; review §2). Unknown URI
⇒ `-32002`.

### 3.4 The `updated` emission rule — the reactive substrate

**Emit `notifications/resources/updated{uri}` to every subscribed peer on every
state transition of the resource named by `uri`.** This is the single most
important outbound signal of the self-server (review §3, §4): it is what makes
async-subagent completion and agent-to-agent reactivity work.

The supervisor owns all state (assessment §2.1, §2.8), so emission is driven
from the one place state changes — the supervisor reactor — via an internal
event the server thread consumes:

```rust
/// Called by the supervisor whenever a tracked node/session/run transitions.
fn on_state_transition(&self, uri: &str) {
    if let Some(conns) = self.res.subs.get(uri) {
        for conn in conns {
            self.send_notification(*conn, "notifications/resources/updated",
                json!({ "uri": uri }));      // URI ONLY — no payload, no diff
        }
    }
}
```

State transitions that emit (the closed set, aligned with the terminal-status
machine of RFC 0007 and the supervision events of RFC 0003):

| Transition | URI emitted |
|---|---|
| node status change (working→stalled→…→terminal) | `agentd://subagent/{handle}` |
| node reaches a **terminal** status | `agentd://subagent/{handle}` **and** `agentd://subagent/{handle}/result` |
| warm session turn boundary / new activity | `agentd://session/{session_id}` |
| run aggregate usage / disposition change | `agentd://run/{run_id}` |

The completion case is the async-subagent close-the-loop: a parent that called
`subagent.spawn{async}` subscribes to `agentd://subagent/{handle}/result`; when
the child reaches terminal, the supervisor emits `updated` on that URI; the
parent (woken via RFC 0008 routing) `resources/read`s it to collect the
distillate. **At-least-once + idempotent via re-read-current-state** (assessment
§2.6): we promise convergence, not exactly-once; redelivery is safe because the
reader acts on what the resource *is now*.

`notifications/resources/list_changed{}` (no params) is emitted when the set of
listed resources changes (a node spawns or is reaped), gated on the peer having
negotiated `resources.listChanged`. This is the list-level mechanism, **distinct
from per-URI subscribe** (assessment §1.3 item 2; review §3.2): no subscribe, no
URI.

**Never emit to a peer that did not negotiate the capability**, and never emit
`updated` for a URI a peer did not subscribe (capability gating — RFC 0004,
review §1).

### 3.5 The `subscribe` self-tool vs MCP `resources/subscribe`

Two distinct subscribe surfaces meet here and must not be confused:

- **MCP `resources/subscribe`** (a *method* a peer calls on our server) — the
  peer wants `updated` notifications for one of *our* `agentd://` URIs (§3.3).
- **The `subscribe` *tool*** (in `tools/list`) — a *running subagent* calls this
  via `tools/call` to ask the supervisor to subscribe **the agent itself** to an
  *external* MCP resource `(server, uri)` reachable through agentd's client side
  (RFC 0004). When a running agent subscribes, the supervisor auto-creates a
  `continue(this_session)` route — **self-subscribe = self-scheduling**, the
  signature capability (assessment §2.6; routing in RFC 0008).

`subscribe` tool schema:

```jsonc
{ "name":"subscribe","title":"Subscribe to a resource",
  "inputSchema":{ "type":"object",
    "properties":{
      "server":{"type":"string"},     // MCP server name from agentd's client registry
      "uri":{"type":"string"}         // concrete URI (not a template)
    },
    "required":["server","uri"],
    "additionalProperties":false }}
```

Returns `{}` on success; `isError:true` if the named server didn't advertise
`resources.subscribe` (degrade gracefully — RFC 0004), or the URI is a template.

### 3.6 Transports — stdio always; unix when `--serve-mcp unix:…`

- **stdio (always on).** When a parent/peer spawns agentd as a subprocess, the
  self-MCP is served on agentd's own stdin/stdout using **NDJSON** (`read_line`/
  `write_line`): one compact JSON object per line, `\n`-terminated, no embedded
  newlines, UTF-8 (MCP stdio rules — RFC 0004). **stdout is sacred for MCP
  messages only**; all telemetry goes to stderr (assessment §2.9). Note: a
  process serving the self-MCP on stdout cannot *also* print an agent result on
  stdout — the `once`-mode result-on-stdout path (RFC 0011) and self-MCP-on-stdio
  serving are mutually exclusive per process; the supervisor selects one by mode.

- **unix socket (opt-in, `--serve-mcp unix:PATH`).** A `UnixListener` (blocking,
  `net/unixsock.rs`) accepts peer connections; each accepted `UnixStream`
  carries the **same NDJSON framing** (stdio-like, spec-permitted custom
  transport — review §7). One reader thread per accepted connection forwards
  tagged events onto the supervisor's merged `mpsc` (the thread-per-fd model,
  assessment §2.1; RFC 0002). For the **high-fan-in** case (many idle peer
  connections) the listener may use `mio`/`libc::poll` behind the `serve-mcp`
  feature instead of one-thread-per-connection (assessment §2.1, §2.2).

```rust
enum ServeTarget { Stdio, Unix(PathBuf) }   // from --serve-mcp

struct PeerConn { id: ConnId, stream: FramedNdjson, caps: PeerCaps }
```

- **Streamable HTTP serving — DEFERRED** (assessment §2.5, §2.13; review §7).
  The full surface (single POST+GET endpoint, `application/json`-vs-SSE upgrade,
  `MCP-Session-Id`, `MCP-Protocol-Version`, `Origin`→403, GET-SSE for unsolicited
  notifications, resumability) is materially heavier than a request/response
  client and is the reason v1 prefers stdio/unix (it also needs the
  spec-hardening of assessment §2.11: non-deterministic session ids,
  sessions-not-authn, no token passthrough). Tracked in RFC 0013.

### 3.7 Liveness and cancellation on the served side

The server answers `ping` with `{}` promptly (it is a good citizen, and ping is
a driver's liveness probe of agentd — RFC 0004). It accepts
`notifications/cancelled{requestId,reason?}` for an in-flight served request
(e.g. a long sync `subagent.spawn`): on receipt it requests graceful cancel of
the spawned subtree via the kill ladder (RFC 0003) and sends no response for the
cancelled request (MCP cancellation rules — review §9). The server **does not
emit** `notifications/message` or `notifications/progress` in v1 (deferred —
assessment §2.5).

---

## 4. Mechanisms — the PRIVATE control protocol

### 4.1 Framing (distinct from MCP's NDJSON)

The control channel uses **length-prefixed framing**: a 4-byte little-endian
`u32` length, then exactly that many bytes of UTF-8 JSON. Cap: **16 MiB**; a
length over the cap is a protocol fault (close the pipe → child is dead/abandoned
per the abandon-don't-interrupt invariant, assessment §2.1). Rationale
(assessment §2.3): control payloads — instructions, context seeds, distilled
results — may contain newlines, so NDJSON is fragile; length-framing is robust.
MCP stays NDJSON; the two **share parse/serialize** (the serde JSON-RPC types)
and **differ only in framing**.

```rust
const FRAME_CAP: usize = 16 * 1024 * 1024; // 16 MiB

fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    assert!(payload.len() <= FRAME_CAP);
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > FRAME_CAP { return Err(io::Error::new(ErrorKind::InvalidData, "frame too large")); }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}
```

Both lifted from the retired `intelligence/protocol.rs` (assessment §2.3). The
payload is a JSON-RPC 2.0 shape — but with **no MCP lifecycle**: there is no
`initialize`, no capability negotiation, no `notifications/initialized` on this
pipe (assessment §2.3; review §10). The first downward frame *is* the spawn
payload; the link is live immediately.

### 4.2 Pipe topology and the dedicated control-reader thread

A spawned subagent (re-exec'd same binary, RFC 0009) gets three pipes:
control-in (parent→child, the child reads), control-out (child→parent, the child
writes), and stderr (free-form telemetry, captured by the parent — assessment
§2.9). The child's **control reader runs on its own thread**, decoupled from the
agentic loop thread (assessment §2.3, §2.8 Detector C). This is a hard
requirement: **ping/pong liveness must survive a long in-flight tool or model
call.** If the reader shared the loop thread, a multi-minute model call would
stall pong and the supervisor would false-positive "stuck."

```rust
// child side (subagent mode)
fn run_subagent() {
    let (to_loop_tx, to_loop_rx) = mpsc::channel::<DownMsg>();   // control -> loop
    let ctrl_out = Arc::new(Mutex::new(io::stdout_control()));   // loop+reader -> parent

    // dedicated control-reader thread
    {
        let ctrl_out = ctrl_out.clone();
        thread::spawn(move || {
            let mut r = io::stdin_control();
            loop {
                let frame = match read_frame(&mut r) { Ok(f) => f, Err(_) => break /*EOF*/ };
                match parse_down(&frame) {
                    DownMsg::Ping{id}  => write_frame(&mut *ctrl_out.lock().unwrap(),
                                                       &up_pong(id)).ok(),        // answered HERE
                    other              => { to_loop_tx.send(other).ok(); }        // hand to loop
                }
            }
        });
    }

    // agentic loop thread consumes control messages between turns / mid-tool
    agentic_loop(to_loop_rx, ctrl_out);
}
```

`ping` is answered **on the reader thread itself** (it requires no loop state),
so pong flows even while the loop is wedged inside a model call. `pause` /
`resume` / `cancel` / `inject` are forwarded to the loop, which acts on them at
the next safe checkpoint (turn boundary; or, for `cancel`, by aborting the
in-flight call and emitting `notifications/cancelled` to any MCP server it was
calling — RFC 0007). The loop and reader share `ctrl_out` behind a `Mutex` so
upward frames never interleave.

On the **supervisor side**, one reader thread per child control-out parses
upward frames and forwards tagged events onto the merged `mpsc` the reactor
`recv_timeout`s (assessment §2.1; RFC 0002). Every received frame stamps
`last_event_at` on the child's supervision record (Detector B no-progress
watchdog — RFC 0003).

### 4.3 Downward messages (supervisor → child)

All are JSON-RPC notifications (no `id`) **except** `ping`, which is a request
(carries `id` so the matching `pong` can be correlated). The very first frame is
the spawn payload (a one-shot bootstrap, structurally a notification).

```rust
enum DownMsg {
    /// first frame only — full spawn payload (RFC 0009 owns the schema)
    Spawn(SpawnPayload),
    Pause,                          // suspend at next turn boundary
    Resume,
    Cancel { reason: String },      // graceful; supervisor follows with the kill ladder if ignored
    Inject { event: InjectEvent },  // deliver an instruction/event into the warm session (subagent.send)
    Ping   { id: u64 },             // liveness; answered on the reader thread
}
```

Wire (compact, one per frame):

```jsonc
{"jsonrpc":"2.0","method":"ctrl/spawn","params":{ /* SpawnPayload, RFC 0009 */ }}
{"jsonrpc":"2.0","method":"ctrl/pause"}
{"jsonrpc":"2.0","method":"ctrl/resume"}
{"jsonrpc":"2.0","method":"ctrl/cancel","params":{"reason":"deadline"}}
{"jsonrpc":"2.0","method":"ctrl/inject","params":{"event":{ /* … */ }}}
{"jsonrpc":"2.0","id":42,"method":"ctrl/ping"}
```

`ctrl/cancel` is the **graceful** rung of the kill ladder (RFC 0003): the
supervisor sends it, waits the grace window, then escalates to
`killpg(SIGTERM)` → `killpg(SIGKILL)` if the child has not exited. `inject`
backs the `subagent.send` self-tool (§3.2) — the only way a parent steers a warm
child.

### 4.4 Upward messages (child → supervisor)

```rust
enum UpMsg {
    /// ready/started/draining/exiting — the child's own lifecycle beats
    Lifecycle { phase: Phase },     // Ready | Started | Draining | Exiting
    /// loop events folded into telemetry + routing (RFC 0007/0010 event vocab)
    Event { event: String, fields: serde_json::Value },
    /// per-turn token/step usage -> hierarchical accounting (RFC 0003 §budget)
    Usage { tokens_in: u64, tokens_out: u64, steps: u32 },
    /// terminal distilled result + status (RFC 0007 terminal-status enum)
    Result { status: TerminalStatus, result: serde_json::Value,
             usage: Usage },
    /// answer to ctrl/ping
    Pong { id: u64 },
}
```

Wire:

```jsonc
{"jsonrpc":"2.0","method":"ev/lifecycle","params":{"phase":"ready"}}
{"jsonrpc":"2.0","method":"ev/event","params":{"event":"tool.call","fields":{"tool":"fs.read"}}}
{"jsonrpc":"2.0","method":"ev/usage","params":{"tokens_in":812,"tokens_out":210,"steps":1}}
{"jsonrpc":"2.0","method":"ev/result",
  "params":{"status":"completed","result":{ /* distillate */ },
            "usage":{"tokens_in":4011,"tokens_out":1290,"steps":12}}}
{"jsonrpc":"2.0","id":42,"result":{"pong":true}}
```

Notes:

- **`ev/lifecycle{phase:"ready"}`** is the spawn-success signal the restart
  governor keys on: a child that exits before `ready` within ~2s counts as a
  crash-on-spawn (fork-bomb early warning — assessment §2.8; RFC 0003).
- **`ev/usage`** drives hierarchical token accounting: the supervisor (source of
  truth) adds to the node counter and the tree-root counter, O(1) per event
  (assessment §2.8; RFC 0003). Node over grant → cancel subtree; root over
  ceiling → drain tree.
- **`ev/result`** is the terminal frame; after it the child exits and is reaped
  (`waitpid`, RFC 0003). The supervisor maps it both to the parent's
  `subagent.spawn` return value / `agentd://subagent/{handle}/result` resource
  (§3.4) and, for a one-shot root, to the process exit code (RFC 0011).
- **`pong`** uses a JSON-RPC *result* shape (it answers the `ctrl/ping`
  *request*); everything else upward is a notification.

### 4.5 Connection state machine (per child, supervisor view)

```
        spawn fork+exec, write Spawn frame
   ┌──────────────────────────────────────────┐
   │                                           ▼
 (none) ──> Spawning ──ev/lifecycle:ready──> Running ──ev/result──> Terminal ──waitpid──> Reaped
              │  (>2s, no ready)                │  │                                 ▲
              │                                 │  │ EOF on control-out (no result)  │
              └────> CrashOnSpawn ──────────────┘  └──> Dead ─────────────────────────┘
                     (restart governor)            (classify via waitpid)
   Running also: Pause/Resume toggle; missing N pongs OR no-progress -> Stuck -> kill ladder
```

The EOF×pong classifier (assessment §2.8; RFC 0003) reads off this machine: EOF
on control-out ⇒ likely Dead (confirm with `waitpid`); no pong + no EOF ⇒
Stuck-alive; frames flowing ⇒ healthy; only pongs flowing ⇒ busy-healthy.

### 4.6 Defaults

| Knob | Default | Note |
|---|---|---|
| frame cap | 16 MiB | hard; over-cap closes the pipe |
| ping interval | 5 s | reader-thread answered |
| missing-pong threshold (N) | 3 | → Stuck (RFC 0003) |
| no-progress timeout | per-child `progress_timeout` (RFC 0003) | Detector B |
| ready timeout | ~2 s | < ready ⇒ crash-on-spawn |
| graceful cancel grace | ~5 s | then SIGTERM (RFC 0003 ladder) |

(Exact deadline/grace/drain values are owned by RFC 0003; reproduced here only
where the control protocol must agree.)

---

## 5. Interactions with other RFCs

- **RFC 0001 (core).** This RFC realizes the "agentd is both MCP client and its
  own MCP server" thesis (server half) and the private supervision wire.
- **RFC 0002 (reactor & concurrency).** The supervisor's per-child control-out
  reader thread and each unix peer-conn reader thread forward onto the merged
  `mpsc` the reactor `recv_timeout`s; the abandon-don't-interrupt invariant
  governs over-cap/EOF handling.
- **RFC 0003 (supervision & recovery).** Consumes `ev/lifecycle`/`ev/usage`/
  `ev/result`/`pong`; owns the kill ladder that `ctrl/cancel` opens; owns the
  EOF×pong classifier this RFC's state machine feeds; owns hierarchical token
  accounting fed by `ev/usage`.
- **RFC 0004 (MCP client subset & codec).** Shares `json/` + `frame.rs`
  (`read_line`/`write_line` here too); the `subscribe`/`resource.read` self-tools
  drive agentd's *client*-side subscriptions and reads; capability-gating and
  notify-then-read are defined there.
- **RFC 0007 (agentic loop).** The child loop consumes forwarded `DownMsg`,
  emits `UpMsg`; terminal-status enum and VERIFY-grounded `ev/result` come from
  there.
- **RFC 0008 (modes & reactive routing).** Self-subscribe → auto
  `continue(this_session)` route; async-subagent completion delivered as an
  `agentd://…/result` `updated` event routed back to the parent; the
  exactly-one-owner / debounce / coalesce rules govern those wakes.
- **RFC 0009 (subagent process model).** Owns the `SpawnPayload` schema, the
  rich output contract, narrowed seed, depth-minting, and the caps refused at the
  spawn chokepoint exposed here as `subagent.spawn`.
- **RFC 0010 (observability).** `ev/event` frames carry the closed event
  vocabulary; `--aggregate-logs` forwards child telemetry up this control channel
  (mode B), never rewriting correlation fields.
- **RFC 0012 (security).** The gated `exec` self-tool, tool-scope subset
  enforcement on `subagent.spawn`, Rule-of-Two tagging, and the deferred
  HTTP-serving hardening.
- **RFC 0013 (deferred v2).** Streamable HTTP self-serving; MCP `tasks/*` as the
  spec-native external long-running surface (the future replacement for
  `subagent.status` polling — review §8/§10); `sampling/createMessage`
  (intelligence-sharing, as a *client* capability — review §5).

---

## 6. Non-goals / Deferred

- **Streamable HTTP self-serving** — deferred (RFC 0013). v1 serves the self-MCP
  over stdio + unix only. No POST+GET endpoint, no SSE upgrade, no
  `MCP-Session-Id`/`MCP-Protocol-Version`/`Origin`→403, no resumability.
- **MCP `tasks/*` on the served side** — deferred. v1's external long-running
  surface is the `subagent.*` tools (sync + M3 async-via-subscription), *not*
  task-augmented requests. `subagent.spawn` does **not** declare
  `execution.taskSupport`.
- **`sampling/createMessage` in either direction** — deferred (assessment §1.3
  item 5; review §5). agentd declares no `sampling` client capability and serves
  no sampling; intelligence-sharing is a v2 feature wired as a client capability,
  not a server one.
- **`prompts/*`, `roots/*`, `elicitation/*`, `completion/*`, `logging` server
  capability, emitting `notifications/message`/`progress` from our server** — all
  deferred (assessment §2.5).
- **Leaking the control protocol outward.** The length-framed
  downward/upward control messages are private to the parent↔child link. External
  supervision is exposed *only* as the `subagent.*` MCP self-tools. There is no
  supported path by which a peer speaks the control protocol; this is the clean
  separation (assessment §2.3, §5 risk 10).
- **MCP lifecycle on the control channel** — deliberately absent. No
  `initialize`, no capability negotiation on the private pipe.

---

## 7. Open items

None blocking. One judgment call deferred to implementation: whether the unix
peer listener should use one-thread-per-connection (default, simplest) or
`mio`/`libc::poll` behind the `serve-mcp` feature — decided by the measured
fan-in at M4 (assessment §2.1 holds `poll` in reserve for exactly this case);
either way the framing and the merged-`mpsc` contract are unchanged.
