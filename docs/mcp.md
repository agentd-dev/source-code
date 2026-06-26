# MCP: the universal interface

agentd has no opinions about *what* it can do — it ships almost no tools of its
own. Everything an agent can touch arrives over the **Model Context Protocol**
(MCP, target spec **2025-11-25**). agentd plays both halves of that protocol:

- as a **client**, it connects to external MCP servers and the tools/resources
  they expose become the agent's entire action space;
- as a **server**, it speaks its own MCP (the "self-MCP") so another agentd —
  or any MCP-aware harness — can spawn, steer, read, and subscribe to it.

That symmetry is the whole composition story: an agentd is just an MCP server
that happens to also be an MCP client, so agentds nest and drive each other with
no special-case protocol.

> **Status.** The MCP client and the self-MCP server / subagent tools are
> implemented and tested. This page documents shipped behavior per RFCs
> [0004](../rfcs/0004-mcp-client-subset-and-codec.md) and
> [0005](../rfcs/0005-self-mcp-server-and-control-protocol.md). Items marked
> **(roadmap)** are explicitly deferred past v1.

---

## 1. agentd as MCP client

### 1.1 There are no built-in tools

agentd ships **no** tools of its own except a single, off-by-default gated
`exec` (see §2.4). Every other capability — read a file, query an API, run a
search — is a tool on some MCP server you declare. The agent discovers them with
`tools/list` and invokes them with `tools/call`. If you declare zero servers and
don't pass `--enable-exec`, the agent has an empty toolbox.

This is deliberate: the action space is configuration, not code. Swapping what
an agent can do never means rebuilding agentd.

### 1.2 Declaring servers — `--mcp name=command`

You declare each MCP server with `--mcp name=command`, repeatable. The transport
is **stdio**: agentd launches the command as a subprocess and speaks JSON-RPC
over its stdin/stdout.

```bash
agentd \
  --instruction "Summarize the open TODOs under /work and write a digest" \
  --intelligence unix:/run/intel.sock \
  --mcp fs=mcp-server-fs --root /work \
  --mcp http=mcp-server-http
```

The part after `=` is split on whitespace into an argv, so the first token is
the executable and the rest are its arguments:

```bash
--mcp fs=mcp-server-fs --root /data
#       ^executable     ^^^^^^^^^^^^ args passed to the server
# parsed as name="fs", command=["mcp-server-fs", "--root", "/data"]
```

> The launch command is **trusted config** — it is never built from
> model-controlled or server-controlled strings. Declare servers from your
> deployment config, not from agent output.

Multiple servers coexist; tool names are **server-qualified** internally so two
servers can both expose a `search` tool without colliding. A `--mcp` with an
empty name or empty command is rejected at startup (exit `2`) before any
side effect.

The exact flag surface (from `agentd --help`):

```
TOOLS / MCP:
  --mcp name=command          declare an MCP server (repeatable; stdio)
  --serve-mcp <unix:/path>    serve agentd's own MCP
  --enable-exec               expose the gated exec tool
```

There is no `--mcp` env var; servers are a structural list. (A config-file layer
that carries argv arrays verbatim is a later milestone; the flag is the stable
surface.)

### 1.3 The handshake and capability negotiation

On connect, before anything else, agentd runs the MCP lifecycle. It pins
`protocolVersion: "2025-11-25"` and declares **no client capabilities at all**:

```jsonc
// agentd → server
{ "jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "protocolVersion":"2025-11-25",
    "capabilities":{},                                   // empty, deliberately
    "clientInfo":{"name":"agentd","title":"agentd","version":"0.1.0"}
}}
// server → agentd
{ "jsonrpc":"2.0","id":1,"result":{
    "protocolVersion":"2025-11-25",
    "capabilities":{
      "resources":{"subscribe":true,"listChanged":true},
      "tools":{"listChanged":true}
    },
    "serverInfo":{"name":"mcp-server-fs","version":"…"},
    "instructions":"…"                                   // optional; folded into the prompt
}}
// agentd → server
{ "jsonrpc":"2.0","method":"notifications/initialized" }
```

Why `capabilities:{}`? You only declare a *client* capability when you intend to
*service* it, and agentd services none. It does not offer `roots`, `sampling`,
`elicitation`, or `tasks`. This is the minimal interop posture and the smallest
injection surface. If a server nonetheless asks:

- `ping` → answered with `{}` (always; it's a liveness probe both ways);
- `roots/list` → answered with `{"roots":[]}` (we expose no filesystem scope);
- `sampling/createMessage`, `elicitation/create`, anything else → rejected with
  `-32601` *method not found*.

**Version negotiation.** agentd offers `2025-11-25` and accepts a downgrade to
`2025-06-18`, `2025-03-26`, or `2024-11-05` where the feature use overlaps
(e.g. structured tool output requires ≥ `2025-06-18`). A version it cannot speak,
or a handshake that doesn't complete within the init timeout (default **10s**),
is a connect failure. The negotiated capability set is then **frozen** and gates
every subsequent call: agentd never sends `resources/subscribe` to a server that
didn't advertise `resources.subscribe`; it degrades instead.

A **required** server that fails its handshake aborts the run with exit `6`. An
optional one is logged and simply omitted from the catalogue.

### 1.4 Tools: list and call

`tools/list` is drained across all pages — agentd follows `nextCursor` to
exhaustion (cursors are opaque; the page loop is capped at 1024 iterations to
defend against a broken server that returns the same cursor forever).

```jsonc
// agentd → server
{ "jsonrpc":"2.0","id":2,"method":"tools/call",
  "params":{ "name":"get_weather", "arguments":{"location":"NYC"},
             "_meta":{ "io.modelcontextprotocol/run-id":"<run_id>" } } }
// server → agentd  (success — note isError lives INSIDE result)
{ "jsonrpc":"2.0","id":2,"result":{
    "content":[ { "type":"text","text":"22.5°C" } ],
    "isError":false,
    "structuredContent":{ "temperature":22.5 }     // iff the tool declared an outputSchema
}}
```

The run id flows into every call's `_meta` for end-to-end correlation.

**The load-bearing distinction — `isError` vs JSON-RPC `error`:**

| Wire shape | Meaning | What agentd does |
|---|---|---|
| `result.isError == true` | tool *ran* and reported a failure (a **successful** JSON-RPC response) | feed `content[]` back to the model as an observation; it self-corrects; **consumes a step** |
| top-level JSON-RPC `error` | protocol/transport fault (unknown tool, bad params, server crash) | classify per the retry/abort policy — not handed to the model as a normal observation |

A tool saying "file not found" is an observation the model reasons about. A
server saying "I have no such tool" is a protocol error. Conflating them is a
classic agent bug; agentd keeps them strictly separate.

> **Tool descriptions and annotations are untrusted.** They are
> server-controlled text (the "tool poisoning" surface). agentd surfaces and
> logs them for operator audit but never auto-trusts them. See the security
> notes in [RFC 0012](../rfcs/0012-security-posture.md).

On `notifications/tools/list_changed` (only if the server advertised
`tools.listChanged`) agentd re-issues `tools/list` and refreshes the catalogue.

### 1.5 Resources: list vs read

Resources are agentd's *context* surface, split into two deliberately distinct
operations:

- **`resources/list` = awareness.** A compact catalogue of URIs with their
  descriptions, sizes, and mime types — never bodies. This is injected into the
  agent's prompt so it knows what exists.
- **`resources/read` = attention.** The actual body, fetched on demand.

`resources/read` always returns a `contents` **array** (one URI may yield several
items, e.g. a directory listing), text in `text`, binary base64 in `blob`:

```jsonc
{ "jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"file:///work/todo.md"} }
{ "jsonrpc":"2.0","id":3,"result":{ "contents":[
    { "uri":"file:///work/todo.md","mimeType":"text/markdown","text":"- ship M2\n- …" }
]}}
```

A missing resource returns `-32002` with `data.uri` — surfaced as an observation,
not a transport abort. `resources/templates/list` is read but **informational
only**: templates are not subscribable; agentd reacts to concrete URIs only.

### 1.6 Reactivity: the notify-then-read subscription model

This is how agentd *wakes* on external change. The model has one non-obvious but
load-bearing property: **the update notification carries no payload.**

```jsonc
// agentd → server  (only if caps.resources.subscribe; one CONCRETE uri, never a template)
{ "jsonrpc":"2.0","id":4,"method":"resources/subscribe","params":{"uri":"file:///work/inbox"} }
{ "jsonrpc":"2.0","id":4,"result":{} }

// later — server → agentd
{ "jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"file:///work/inbox"} }
```

The notification says only *"`file:///work/inbox` changed"* — no diff, no new
content. So agentd does **notify-then-read**: on wake it issues a fresh
`resources/read` to learn the current state. Two consequences fall out of this:

1. It's two round-trips, and the read can race a subsequent update. agentd's
   contract is **at-least-once delivery + convergence by re-reading current
   state** — redelivery is harmless because you always act on what the resource
   *is now*, not on a stale diff. (Debounce/coalesce/routing of these wakes is
   the reactive router's job;
   [RFC 0008](../rfcs/0008-execution-modes-and-reactive-routing.md).)
2. On reconnect, agentd re-issues every subscription and then synthesizes one
   coalesced "updated" per watched URI, so a change missed while disconnected is
   not lost (edge-triggered events promoted to level across the restart).

**Two distinct mechanisms — never conflated:**

| Trigger | Capability needed | Subscribe call | Notification | Payload |
|---|---|---|---|---|
| a specific item changed | `resources.subscribe` | `resources/subscribe{uri}` per URI | `notifications/resources/updated` | `{uri}` (+ optional `title`) |
| the *set* of resources changed | `resources.listChanged` | none (capability-implied) | `notifications/resources/list_changed` | none |
| the *set* of tools changed | `tools.listChanged` | none | `notifications/tools/list_changed` | none |

You wire a subscription to a run with `--subscribe` plus `--mode reactive`:

```bash
agentd \
  --instruction "When the inbox changes, triage new items" \
  --intelligence unix:/run/intel.sock \
  --mcp fs=mcp-server-fs --root /work \
  --mode reactive \
  --subscribe file:///work/inbox
```

`--mode reactive` *requires* at least one `--subscribe <uri>`; without it the
config is rejected at startup (exit `2`).

> **v1 scope: reactivity is stdio-only.** Subscriptions ride the stdio transport.
> Reactivity-over-HTTP needs a long-lived SSE GET stream with `Last-Event-ID`
> resumption, which the v1 MCP client does not build. **(roadmap)** —
> [RFC 0013](../rfcs/0013-deferred-v2-surface.md).

### 1.7 Liveness and lifecycle

agentd pings idle connections outbound (default every **30s**, **10s** per-ping
timeout); three consecutive missed pongs marks the server stale and runs the
shutdown ladder. It answers inbound `ping` unconditionally. When it abandons an
in-flight call (deadline trip, step-budget exhaustion, cancel) it sends
`notifications/cancelled{requestId,reason}` so it doesn't leak work on a server
it keeps using — but it **never** cancels `initialize`.

stderr from each server is free-form per spec, so agentd **never** treats stderr
as an error signal; a dedicated thread drains it into the structured log stream
(event `mcp.stderr`, tagged by server). The shutdown ladder is ordered and
bounded: close stdin (EOF) → wait → `SIGTERM` → wait → `SIGKILL` → reap. The
whole drain counts inside `--drain-timeout` (default `25s`).

---

## 2. agentd as MCP server (self-MCP)

agentd is *also* an MCP server. A parent agentd, a peer, or any MCP-aware harness
can `initialize` against it and get a real, capability-negotiated catalogue:
tools to spawn and steer subagents, tools to read and subscribe to state, an
optional gated `exec`, and a tree of subscribable `agentd://` state resources.

It serves this over **stdio always**, and over a **unix socket** when you pass
`--serve-mcp unix:PATH`:

```bash
agentd \
  --instruction "Be a reusable code-review worker" \
  --intelligence unix:/run/intel.sock \
  --mcp fs=mcp-server-fs --root /src \
  --serve-mcp unix:/run/agentd-review.sock
```

> **stdout is sacred.** When serving the self-MCP over stdio, stdout carries MCP
> messages only — all telemetry goes to stderr. A process serving self-MCP on
> stdio therefore cannot *also* print an agent result on stdout; the
> `once`-mode result-on-stdout path and self-MCP-on-stdio are mutually exclusive
> per process (the supervisor picks one by mode).

> **HTTP serving is (roadmap).** v1 serves over stdio + unix only. The full
> Streamable-HTTP surface (POST+GET endpoint, SSE upgrade, `MCP-Session-Id`,
> `Origin`→403, resumability) is deferred to
> [RFC 0013](../rfcs/0013-deferred-v2-surface.md).

### 2.1 Declared capabilities

The self-MCP answers `initialize` and declares exactly two capabilities — and
nothing else:

```jsonc
{ "jsonrpc":"2.0","id":1,"result":{
    "protocolVersion":"2025-11-25",
    "capabilities":{
      "tools":     { "listChanged": true },
      "resources": { "subscribe": true, "listChanged": true }
    },
    "serverInfo":{ "name":"agentd","title":"agentd","version":"0.1.0" },
    "instructions":"agentd self-MCP: spawn/steer subagents, read+subscribe agentd:// state."
}}
```

No `prompts`, `logging`, `completions`, or `tasks`. It answers `ping`, accepts
`notifications/cancelled` for an in-flight served request, and does **not** emit
`notifications/message` or `notifications/progress` in v1.

### 2.2 The `agentd://` tools

`tools/list` returns the catalogue below. Each `inputSchema` is JSON Schema
2020-12. The *available* set is gated per caller — when a caller's scope narrows
(e.g. a child whose grant excludes `exec`), the server emits
`notifications/tools/list_changed` and the caller re-lists.

| Tool | Purpose | Mode |
|---|---|---|
| `subagent.spawn` | create a child subagent from a rich spawn payload | sync \| async \| detach |
| `subagent.send` | inject an instruction/event into a warm subagent session | sync ack |
| `subagent.cancel` | request graceful cancel of a subtree (→ kill ladder) | sync ack |
| `subagent.status` | read a handle's status + usage snapshot | sync |
| `subscribe` | subscribe the **caller agent itself** to an external MCP resource `(server, uri)` | sync ack |
| `unsubscribe` | drop such a subscription | sync ack |
| `resource.read` | read an MCP resource body the caller is aware of | sync |
| `exec` | gated shell exec under the subtree kill ladder + caps | sync; off by default |

`subagent.spawn` (abridged schema — full payload semantics in
[RFC 0009](../rfcs/0009-subagent-process-model.md)):

```jsonc
{ "name":"subagent.spawn","title":"Spawn subagent",
  "inputSchema":{ "type":"object",
    "properties":{
      "instruction":    {"type":"string"},
      "output_contract":{"type":"object"},
      "context_seed":   {"type":"array","items":{"type":"object"}},
      "tool_scope":     {"type":"array","items":{"type":"string"}},
      "limits":         {"type":"object"},
      "async":          {"type":"boolean","default":false},   // return a handle immediately
      "detach":         {"type":"boolean","default":false}    // outlive the parent's turn
    },
    "required":["instruction"],
    "additionalProperties":false }}
```

A **sync** spawn blocks and returns the distilled result, terminal status,
and usage:

```jsonc
{ "jsonrpc":"2.0","id":7,"result":{
    "content":[{"type":"text","text":"{…distillate…}"}],
    "structuredContent":{
      "handle":"0.2",
      "status":"completed",
      "usage":{"tokens_in":1234,"tokens_out":456,"steps":9},
      "result":{ /* distilled structured value, ~1–2k tokens */ }
}}}
```

**Critical invariants enforced at the spawn chokepoint:** the child's depth is
*minted by the supervisor* from the caller's handle (never read from the
request); `tool_scope` must be a **subset** of the caller's scope (monotonic
narrowing); and a spawn that would breach `--max-depth`, a child/total cap, the
spawn-rate bucket, or the tree token ceiling is **refused as a tool result**, not
a crash:

```jsonc
{ "result":{ "isError":true,
  "content":[{"type":"text","text":"spawn refused: max_depth=4 reached at handle 0.2.1.3"}] }}
```

Note the pattern: a cap/scope **refusal** is `isError:true` (so the calling
model adapts), while a malformed `tools/call` (unknown tool, bad params) is a
JSON-RPC `error` (`-32601`/`-32602`) — the same distinction agentd honors as a
client (§1.4).

> **Async / detached spawn ships.** `subagent.spawn` defaults to sync; an
> `{async}` spawn returns immediately with a handle plus a `result_resource` URI
> the caller subscribes to, and `{detach}` lets the child outlive the parent's
> turn. The resource/notify machinery below carries the result.

The `exec` tool appears in `tools/list` **only when** `--enable-exec` is set (and
the target binary exists). Absent that flag it is simply not listed — capability
absence, not a runtime error.

### 2.3 Subscribable `agentd://` state resources

The self-MCP exposes session/run/subagent state as readable **and subscribable**
resources under the custom `agentd://` scheme. This is the substrate for
agent-to-agent reactivity and for async-subagent completion.

| URI | Body on `resources/read` |
|---|---|
| `agentd://run/{run_id}` | run-level status, mode, root handle, aggregate usage, exit disposition |
| `agentd://session/{session_id}` | warm-session status, current turn, last activity |
| `agentd://subagent/{handle}` | per-node status, depth, scope summary, usage, last terminal status |
| `agentd://subagent/{handle}/result` | distilled result once the node is terminal (the async-completion handle) |

`resources/read` returns the standard `contents[]` array with one JSON text item:

```jsonc
{ "result":{ "contents":[
    {"uri":"agentd://subagent/0.2","mimeType":"application/json",
     "text":"{\"handle\":\"0.2\",\"status\":\"working\",\"depth\":1,\"usage\":{…}}"}
]}}
```

**The emission rule — the reactive substrate.** On every state transition of a
resource, agentd emits `notifications/resources/updated{uri}` to every peer
subscribed to that URI — **URI only, no payload, no diff**, exactly like the
client side (§1.6). The peer then `resources/read`s to learn the new state.
Same notify-then-read, same at-least-once + re-read-current-state convergence.

The closed set of transitions that emit:

| Transition | URI emitted |
|---|---|
| node status change (working → stalled → … → terminal) | `agentd://subagent/{handle}` |
| node reaches a **terminal** status | `agentd://subagent/{handle}` **and** `.../result` |
| warm-session turn boundary / new activity | `agentd://session/{session_id}` |
| run aggregate usage / disposition change | `agentd://run/{run_id}` |

`notifications/resources/list_changed` (no params) fires when the *set* of listed
resources changes — a node spawns or is reaped — gated on the peer negotiating
`resources.listChanged`. As on the client side, this is distinct from per-URI
subscribe. agentd never emits to a peer that didn't negotiate the capability, and
never emits `updated` for a URI a peer didn't subscribe to.

### 2.4 Two `subscribe` surfaces — don't confuse them

The word "subscribe" appears in two different roles here:

- **MCP `resources/subscribe`** — a *method a peer calls on agentd's server* to
  get `updated` notifications for one of agentd's own `agentd://` URIs (§2.3).
- **The `subscribe` *tool*** — a *running subagent calls this* (via `tools/call`)
  to subscribe **itself** to an *external* MCP resource reachable through
  agentd's client side. When a running agent self-subscribes, the supervisor
  auto-creates a `continue(this_session)` route — **self-subscribe =
  self-scheduling**, the signature reactive capability.

```jsonc
{ "name":"subscribe","title":"Subscribe to a resource",
  "inputSchema":{ "type":"object",
    "properties":{
      "server":{"type":"string"},   // MCP server name from agentd's client registry
      "uri":{"type":"string"}        // concrete URI (not a template)
    },
    "required":["server","uri"],
    "additionalProperties":false }}
```

Returns `{}` on success, or `isError:true` if the named server didn't advertise
`resources.subscribe` (graceful degrade) or the URI is a template.

### 2.5 The private control protocol is *not* exposed

When agentd spawns a subagent, it re-execs itself and drives the child over a
**private, length-prefixed JSON-RPC control protocol** (no MCP handshake, no
capability negotiation) on the child's stdio pipes. That wire is internal and
deliberately never leaked outward. A peer that wants to spawn or steer a subagent
uses the `subagent.*` **MCP tools** above — it never sees the control frames.
This keeps the public surface a single, clean MCP dialect.
(Details in [RFC 0005 §4](../rfcs/0005-self-mcp-server-and-control-protocol.md).)

---

## 3. Composition: one agentd driving another

Because agentd is symmetric, composition needs no new protocol. A parent agentd
declares a child agentd as just another MCP server:

```bash
# the parent — the child agentd is "just an MCP server" on a unix socket
agentd \
  --instruction "Orchestrate the nightly review across the repo" \
  --intelligence unix:/run/intel.sock \
  --mcp reviewer="agentd --instruction worker --intelligence unix:/run/intel.sock --serve-mcp unix:/run/rev.sock"
```

From the parent's point of view the child is a normal MCP server: it
`initialize`s, lists the `subagent.*` / `subscribe` / `resource.read` tools,
calls them, lists `agentd://` resources, and subscribes to them. Two patterns
fall out:

**Drive** — the parent calls `subagent.spawn` (or `subagent.send` to a warm
session) on the child and gets back a distilled result. The parent never reasons
about the child's internal steps; it gets a clean, bounded answer.

**Subscribe** — the parent subscribes to `agentd://subagent/{handle}/result` on
the child. When the child reaches a terminal status, the child emits
`notifications/resources/updated` on that URI; the parent (woken by its reactive
router) `resources/read`s it to collect the distillate. This is exactly how an
**async** subagent closes the loop — the same notify-then-read machinery, just
across a process boundary.

A worked picture of the reactive close-the-loop:

```
parent agentd                              child agentd (self-MCP, unix:/run/rev.sock)
  │  tools/call subagent.spawn{async}  ──▶
  │  ◀── result_resource: agentd://subagent/0.2/result
  │  resources/subscribe{uri:.../0.2/result}  ──▶
  │                                            … child works …
  │  ◀── notifications/resources/updated{uri:.../0.2/result}
  │  resources/read{uri:.../0.2/result}  ──▶
  │  ◀── contents[]: { distilled result }
```

Because every notification is payload-free and the parent always re-reads current
state, redelivery is safe and the parent converges on the child's actual
terminal result. No exactly-once gymnastics, no diff bookkeeping — the same
discipline agentd applies to every MCP resource, applied to agents themselves.

---

## See also

- [RFC 0004 — MCP client subset & wire codec](../rfcs/0004-mcp-client-subset-and-codec.md)
- [RFC 0005 — Self-MCP server & control protocol](../rfcs/0005-self-mcp-server-and-control-protocol.md)
- [RFC 0008 — Execution modes & reactive routing](../rfcs/0008-execution-modes-and-reactive-routing.md)
- [RFC 0009 — Subagent process model](../rfcs/0009-subagent-process-model.md)
- [RFC 0012 — Security posture](../rfcs/0012-security-posture.md)
- [RFC 0013 — Deferred v2 surface](../rfcs/0013-deferred-v2-surface.md)
- [Build plan & progress](design/PLAN.md)
