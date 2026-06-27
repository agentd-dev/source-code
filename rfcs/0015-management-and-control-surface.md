# RFC 0015: Management & control surface — vsock serving, the operator MCP profile, the capabilities manifest

**Status:** Proposed (agentctl control-plane track)
**Author:** Andrii Tsok
**Date:** 2026-06-27
**Part of:** the agentd rewrite — control-plane track (RFC 0014); extends the self-MCP server & control protocol (RFC 0005)

> **A2A alignment (RFC 0020).** RFC 0020 (A2A-over-vsock) builds two surfaces here
> into the agent mesh, with no new source of truth: (1) the capabilities manifest
> (§5.2) **is** the A2A **Agent Card** — the on-node gateway projects the manifest
> into A2A's `agent.json` schema; one builder, one document (RFC 0020 §3/§5). (2) the
> vsock serving transport (§3) also carries the A2A **server profile** (feature
> `a2a`) alongside this management/self-MCP profile — same listener, same codec
> (RFC 0004), same one trust domain (§3.3) where the vsock peer is the node-agent
> (RFC 0020 §2/§4). agentd carries none of A2A's HTTP/SSE/OAuth/webhook machinery;
> that lives in the gateway. See RFC 0020.

---

## 1. Problem / Context

RFC 0014 splits the world: `agentd` is the **data plane** (one static binary that
runs the bounded agentic loop), `agentctl` is the **control plane** (a CLI, a
`kubectl agent[s]` plugin, and a Kubernetes operator that provisions, scales, and
observes a *fleet*). The umbrella's load-bearing move is to make **vsock
bidirectional**: agentd already *dials out* over vsock for intelligence (RFC
0006); if it can also **serve** its self-MCP over vsock, an `agentctl` node-agent
DaemonSet manages every local agentd pod host-side — control *and* telemetry —
and the pod can run with **no cluster networking at all** (vsock-out for the
model, vsock-in for management). That is the strongest isolation posture agentd
can offer and the natural backend for `kubectl agent`.

This RFC lands the first sub-RFC of that track — the one the rest depend on
(RFC 0014 §7: "RFC 0015 first — it unlocks the transport + manifest the rest
depend on"). It specifies three things and nothing more:

1. **A new self-MCP transport: `--serve-mcp vsock:PORT`.** The exact same
   NDJSON self-MCP server (RFC 0005), now reachable over a vsock listener, so the
   host-side node-agent is the management peer.
2. **The operator profile of the self-MCP** — a *superset* of the existing work
   profile (RFC 0005 §3.2/§3.3): a few new `agentd://` resources and a small set
   of operator-facing tools (`drain`, `lame-duck`, `pause`/`resume`, instance
   `cancel`), all served over the same transports, with no new protocol.
3. **The capabilities manifest** — the shared spine RFC 0014 §5 sketched and
   explicitly delegated here ("the manifest's exact schema is owned by RFC 0015 §
   capabilities"). One machine-readable document, emitted as a one-shot
   (`agentd --capabilities`) and as a live resource (`agentd://capabilities`),
   that tells agentctl what an instance *is*, what it can do, and which contract
   versions it speaks.

The discipline is RFC 0014 §3 verbatim: **agentd exposes primitives; agentctl
owns policy.** Everything here is a primitive (a resource to read, a tool to
call, a transport to bind). The Kubernetes-facing translation — CRDs, the
reconcile loop, `kubectl agent tree`'s rendering, RBAC, dashboards — lives
entirely in agentctl and is a **non-goal** of this RFC (§9). And the minimalism
moat holds: every surface here is **feature-gated and dependency-free**. The
default build stays `serde + serde_json + libc`; the only new code is a vsock
listener (already behind the `vsock` feature for the *client*, RFC 0006) and a
handful of resources/tools that are pure supervisor-state reads. A surface that
would pull an async runtime, a Kubernetes client, or a TLS/gRPC stack is wrong by
construction (RFC 0014 §3.3) and belongs in agentctl.

---

## 2. Decision

1. **`--serve-mcp vsock:PORT` serves the existing self-MCP over vsock**, in
   addition to today's `stdio` (always) and `unix:PATH` (RFC 0005 §3.6). It is a
   **blocking, thread-per-connection** listener — the unix-server design lifted
   verbatim, only the socket type changes — gated by the **`serve-mcp` AND
   `vsock`** features (`vsock` already exists for the intelligence client). The
   vsock peer **is** the node-agent: one trust domain, exactly as the unix socket
   trusts whoever holds filesystem permission to it (RFC 0012 §3.8). No auth, no
   session model — the transport *is* the boundary (§7).

2. **The operator profile is a superset of the work profile, gated by reachability,
   not a flag.** The self-MCP served over `vsock`/`unix` exposes, in addition to
   the RFC 0005 work surface, the resources `agentd://capabilities`,
   `agentd://inventory`, and `agentd://events` (the last owned by RFC 0016), and
   the tools `drain`, `lame-duck`, `pause`, `resume`, and an instance-level
   `cancel`. `attach` is **not a new tool** — it is `subagent.send` into a warm
   session (RFC 0005); agentctl labels it `kubectl agent attach`. These operator
   tools are listed **only on the management transports** (vsock/unix), never on
   stdio served *into a parent agentd* (a peer subagent must not drain its
   supervisor); §3.4.

3. **The capabilities manifest is the contract spine.** `agentd --capabilities`
   prints the manifest JSON to **stdout and exits `0`** (one-shot, side-effect
   free, before any MCP connect); `agentd://capabilities` returns the same
   document as a live, subscribable resource. Its schema (§5) carries
   `contract_version`, `agentd_version`, `build_features`, `identity`, `mode`,
   `model`, an `intelligence` summary, `mcp_servers` + trifecta tags, `limits`,
   and a **`surfaces`** block declaring which control-plane contracts this binary
   actually serves — so agentctl degrades gracefully (RFC 0014 §7).

4. **Instance identity comes from the Kubernetes downward API, env-only.** agentd
   reads pod name / uid / node / namespace from operator-injected env vars
   (`valueFrom.fieldRef`) and surfaces them in `capabilities.identity` and
   `agentd://status`. **agentd never calls the kube API** — no client, no
   in-cluster config, no service-account token read. Env in, nothing else (§6).

5. **Capability absence is not an error.** An operator tool or resource that a
   binary was *not built with* (or whose surface is off) is simply **not listed**
   — never an error, never a stub (the RFC 0005 §3.2 / RFC 0012 §3.6 "absent, not
   present-but-erroring" rule). agentctl discovers presence from the manifest's
   `surfaces` block and from `tools/list` / `resources/list`, and drives only what
   it sees (§8).

6. **Freeze and version what agentctl couples to.** The manifest schema, the
   operator tool names, and the operator resource URIs are a **public API**
   (RFC 0014 §3.4): they carry `contract_version`, change additively within a
   major, and a breaking change bumps the major. agentctl negotiates on
   `contract_version` and refuses an instance whose major it does not understand.

These decisions are final for the RFC 0015 surface. Each defers to its owner
where it touches another RFC (noted inline).

---

## 3. Mechanism — `--serve-mcp vsock:PORT`

### 3.1 The flag and config surface

`--serve-mcp` already accepts `unix:PATH` (RFC 0005 §3.6) and is plumbed through
`Config::serve_mcp: Option<ServeAddr>` (RFC 0011 §3.1). This RFC adds a third
variant. The flag stays a single value (one served address per process); the
config table row is unchanged but the value space grows:

| Concern | Env | Flag | Value space |
|---|---|---|---|
| Serve self-MCP | `AGENTD_SERVE_MCP` | `--serve-mcp` | `unix:PATH` \| **`vsock:PORT`** \| `vsock:CID:PORT` |

```rust
// config.rs — extends RFC 0005 §3.6 ServeTarget
enum ServeTarget {
    Stdio,                       // always, implicit
    Unix(PathBuf),               // --serve-mcp unix:PATH
    Vsock { cid: u32, port: u32 }, // --serve-mcp vsock:[CID:]PORT   (feature = "vsock")
}
```

- **`vsock:PORT`** binds with `cid = VMADDR_CID_ANY` (the guest accepts from any
  peer CID — in practice its host, since vsock is point-to-point guest↔host).
  This is the normal pod form: the node-agent connects from the host CID
  (`VMADDR_CID_HOST = 2`).
- **`vsock:CID:PORT`** is accepted for symmetry with the intelligence URI grammar
  (RFC 0006 §2: `vsock:<cid>:<port>`) and for test rigs that bind a specific CID;
  the operator normally omits the CID.

**Validation (RFC 0011 §3.3, exit 2 before any side effect):** `--serve-mcp
vsock:…` on a binary built **without** the `vsock` feature is a config error
(`scheme unsupported: vsock requires the 'vsock' build feature`), symmetric to
RFC 0011's `intelligence.scheme_supported()` check for `https`. Port `0` or a
non-numeric port is exit 2. As today, a process serving the self-MCP on
**stdio** cannot also print a `once`-mode result on stdout (RFC 0005 §3.6); a
`vsock`/`unix` served address has no such conflict and may coexist with any mode.

### 3.2 The listener — blocking, thread-per-connection, identical to unix

The vsock server is the unix server (RFC 0005 §3.6) with the socket type swapped.
There is **no new concurrency model, no async runtime, no new framing.** It reuses
`net/vsock.rs` — already in the tree for the intelligence *client* (RFC 0006 §3,
the `vsock` feature) — for the listening half:

```rust
// net/vsock.rs  [feature = "vsock"] — server half (mirrors net/unixsock.rs)
fn serve_vsock(cid: u32, port: u32, sup: SupervisorHandle) -> io::Result<()> {
    let listener = VsockListener::bind(&VsockAddr::new(cid, port))?;  // AF_VSOCK, SOCK_STREAM
    for conn in listener.incoming() {
        let stream = conn?;                       // VsockStream: Read + Write, TcpStream-shaped
        let sup = sup.clone();
        thread::spawn(move || {
            // SAME PeerConn as the unix path: NDJSON framing (read_line/write_line, RFC 0005),
            // one reader thread, tagged events onto the supervisor's merged mpsc (RFC 0002).
            serve_peer(FramedNdjson::new(stream), PeerOrigin::Management, sup);
        });
    }
    Ok(())
}
```

Concretely:

- **Same framing.** NDJSON (`read_line`/`write_line`, RFC 0005 §3.6) — one compact
  JSON object per line, `\n`-terminated, UTF-8. vsock is a reliable byte stream
  (`SOCK_STREAM`), so the unix-socket framing applies unchanged. The MCP wire and
  codec are RFC 0004's; this RFC adds no protocol bytes.
- **Same threading.** One reader thread per accepted connection forwards tagged
  events onto the supervisor's merged `mpsc` (the thread-per-fd model, RFC 0002).
  For the high-fan-in case the RFC 0005 §3.6 escape hatch (`mio`/`libc::poll`
  behind `serve-mcp`) applies identically; in practice the node-agent holds **one**
  long-lived management connection per pod, so fan-in is trivial.
- **Same `PeerConn` / `PeerCaps`.** A vsock peer `initialize`s, negotiates
  capabilities, and gets the same `agentd` `serverInfo` (RFC 0005 §3.1). The only
  difference from a unix peer is `PeerOrigin::Management` (set for vsock and unix;
  `PeerOrigin::Stdio` for the stdio path) — the tag that gates the operator tools
  (§3.4).
- **No new dependency.** `vsock::VsockListener` lives in the same `vsock` crate
  RFC 0006 already vendors for the client; the default build and the cloud-native
  image set (no `vsock`) are byte-for-byte unchanged. This is the minimalism moat
  (RFC 0014 §3.3): the vsock-serving image is a *feature-gated* build, not the
  default.

### 3.3 Trust domain — the vsock peer IS the node-agent

vsock is point-to-point between a guest VM and its host; there is no routing, no
multi-tenant fabric. The peer that connects to the guest's `vsock:PORT` is the
host — i.e. the `agentctl` node-agent DaemonSet running on that node (RFC 0014
§2). This is **one trust domain**, structurally identical to the unix-socket
model (RFC 0012 §3.8): the unix socket trusts whoever holds filesystem permission;
the vsock port trusts whoever owns the host side of the VM boundary. In both cases
**the transport is the access control** — there is no in-band auth, no token, no
session-is-authn confusion (RFC 0012 §2 decision: no auth as core).

Who provisions the device is **not agentd's concern** (RFC 0014 §6): the
node-agent sets up the vsock CID/port and the host-side model service; agentd only
*uses* the CID/port it is given (RFC 0006 makes the same statement for the
client). agentd binds, accepts, and serves. The operator's job is to ensure only
the trusted node-agent can reach that vsock port — exactly the posture a unix
socket's mode/owner gives on the filesystem.

Cluster-network serving (TCP / Streamable HTTP, RFC 0013) remains a *deferred*
alternative transport for the same surface, with the same operator tools — not a
precondition, and explicitly out of this RFC because it needs the auth/hardening
story RFC 0012 §3.8 defers. **vsock needs none of that hardening precisely
because it is not a network** — there is no `Origin` to validate, no DNS to
rebind, no bearer token to pass through.

### 3.4 The operator profile is gated by `PeerOrigin`, not a global flag

The self-MCP `tools/list` is **already per-caller gated** (RFC 0005 §3.2: a
caller's scope narrows the available set; `tools/list_changed` fires on change).
This RFC reuses that exact mechanism. The operator tools (§4) and the operator
resources (§5) are listed **iff** the connecting peer's `PeerOrigin` is
`Management` (vsock or unix) — i.e. iff the peer is reaching agentd over a
management transport:

| Peer origin | Transport | Work tools (RFC 0005) | Operator tools (§4) | Operator resources (§5) |
|---|---|---|---|---|
| `Stdio` | stdin/stdout (spawned by a parent agentd / harness) | yes | **no** | `capabilities` only |
| `Management` | `unix:PATH` / `vsock:PORT` | yes | **yes** | yes |

This is the critical containment: **a peer subagent spawned over stdio (RFC 0005
§3.6) must never be able to `drain` or `lame-duck` its own supervisor.** The
operator tools are a *management* capability, scoped to the management transport,
which is itself scoped to the node-agent trust domain (§3.3). A child agentd that
holds a `subagent.spawn`-style relationship to a parent reaches the parent over
stdio and sees only the work surface. The gate is structural (peer origin), not a
runtime check inside each tool.

`agentd://capabilities` is the one operator-profile resource also visible on
stdio — it is pure read-only self-description and is harmless to expose to a
parent (indeed a parent agentd wants to read a child's manifest before spawning).
Everything else operator-facing requires `PeerOrigin::Management`.

---

## 4. Mechanism — the operator tools

All operator tools are normal MCP self-tools (RFC 0005 §3.2): JSON-Schema
`inputSchema`, called via `tools/call`, returning a `CallToolResult` whose
`structuredContent` carries the machine-readable answer. A refusal (already
draining, unknown handle) is `isError:true` **inside** a successful result so the
caller adapts (RFC 0005 §3.2 distinction); a malformed call (unknown tool, bad
params) is a JSON-RPC `error` (`-32601`/`-32602`). None of these tools invent new
lifecycle machinery — they pull the **same levers** SIGTERM and the control
protocol already own.

| Tool | Purpose | Underlying machinery |
|---|---|---|
| `drain` | trigger graceful drain; return in-flight count + ETA | **identical to SIGTERM** → the RFC 0011 §4.2 drain state machine |
| `lame-duck` | flip `/readyz` to NotReady **without exiting** | readiness flag only (RFC 0010 §3.7); no drain, no exit |
| `pause` / `resume` | suspend / resume the tree at turn boundaries | fan-out `ctrl/pause` / `ctrl/resume` (RFC 0005 §4.3) |
| `cancel` | cancel a run / subtree by handle | wraps `subagent.cancel` (RFC 0005 §3.2) at instance scope |
| *(`attach`)* | interactive steering of a warm session | **is `subagent.send`** (RFC 0005) — not a new tool |

### 4.1 `drain` — the same machinery as SIGTERM

`drain` enters the **exact** drain choreography SIGTERM/SIGINT trigger (RFC 0011
§4.2): flip the one-way `DRAINING` latch → disarm triggers → flip readiness to
NotReady (RFC 0010 §3.7) → reject new `subagent.spawn` with `-32000 "shutting
down"` → wind down in-flight subagents at turn boundaries → ladder stragglers →
flush → exit `0` (a clean drain returns **0, not 143** — RFC 0011 §5). It is the
**programmatic equivalent of the kubelet sending SIGTERM**, so agentctl can drain
a pod over the management transport without a `kill`, and the pod still exits a
clean `0` for dashboards (RFC 0011 §5 / RFC 0016).

It returns immediately (does not block until exit) with a snapshot:

```jsonc
{ "name":"drain",
  "inputSchema":{ "type":"object",
    "properties":{
      "deadline_ms":{"type":"integer"}   // optional override; clamped to <= AGENTD_DRAIN_TIMEOUT
    },
    "additionalProperties":false }}
```
```jsonc
// CallToolResult.structuredContent
{ "draining": true,
  "in_flight": 3,                         // root subagents still winding down
  "eta_ms": 18400,                        // min(remaining drain budget, observed wind-down)
  "drain_timeout_ms": 25000,              // the bound (RFC 0011 §4.2)
  "started_at":"2026-06-27T10:00:00.123Z" }
```

`drain` is **idempotent and monotonic**: the latch is one-way (RFC 0011 §4.2), so a
second `drain` (or a SIGTERM arriving after a `drain` tool call) just returns the
current snapshot — it does **not** map to the second-signal `FORCE` path. Force
remains the second *signal* (RFC 0011 §4.3); there is deliberately no `force` tool
in v1 (agentctl that wants force sends a second SIGTERM, or lets the pod grace
expire — the kubelet SIGKILL). A `drain` call after drain has begun is a no-op
that re-reports.

### 4.2 `lame-duck` — NotReady without exiting (the rolling-update primitive)

`lame-duck` flips readiness to NotReady (RFC 0010 §3.7 — `proc.ready` → not-ready,
`/readyz` → 503, `agentd_ready` gauge → 0) **without** entering drain and
**without** exiting. The supervisor keeps running, keeps serving in-flight work,
keeps liveness green — it just advertises "don't send me new work." This is the
**node-drain / rolling-update primitive**: agentctl flips a pod lame-duck, waits
for in-flight work to bleed off (watching `agentd://inventory` or
`agentd_active_subagents`), then drains or deletes it. Unlike `drain`, it is
**reversible**:

```jsonc
{ "name":"lame-duck",
  "inputSchema":{ "type":"object",
    "properties":{ "ready":{"type":"boolean","default":false} },  // false => NotReady; true => back to Ready
    "additionalProperties":false }}
```
```jsonc
// structuredContent
{ "ready": false, "since":"2026-06-27T10:00:00.123Z", "in_flight": 3 }
```

`lame-duck{ready:false}` is the only sanctioned way to make a *reactive* daemon
NotReady while alive. It does **not** disarm subscriptions or unsubscribe — a
reactive daemon keeps reconciling, it just signals the orchestrator's readiness
gate. (Whether agentctl *also* stops routing reactive events is its policy: it can
pause subscriptions with `pause`, §4.3.) `lame-duck{ready:true}` restores
readiness *iff* the underlying readiness conditions still hold (MCP connected, subs
reconciled — RFC 0010 §3.7); if they do not, the call returns `isError:true` with
the unmet condition, and readiness stays as the supervisor computes it. Readiness
is a *computed* state with a lame-duck **override toward NotReady**; the tool can
never assert Ready over a genuinely-not-ready supervisor.

### 4.3 `pause` / `resume` — tree-wide turn-boundary suspension

`pause` fans `ctrl/pause` (RFC 0005 §4.3) to every in-flight root subagent; each
loop suspends at its next turn boundary (RFC 0007). `resume` fans `ctrl/resume`.
This is the existing per-child control message lifted to an **instance-wide**
operation — useful for live debugging (`kubectl agent pause` to inspect state
without teardown) and for holding a tree while the host model service is swapped
(RFC 0018). Paused is **not** drain and **not** lame-duck: the tree is frozen but
intact, readiness is unchanged unless the operator also lame-ducks.

```jsonc
// pause / resume — no params
{ "name":"pause", "inputSchema":{ "type":"object","additionalProperties":false } }
{ "name":"resume","inputSchema":{ "type":"object","additionalProperties":false } }
```
```jsonc
// structuredContent (both)
{ "paused": true, "affected": 3 }        // count of subtrees that took the message
```

Pause is reported in `agentd://inventory` (§5.3, per-node `paused:true`) and via the
`agentd_paused` gauge (§5.5). A paused instance still answers `ping`, still serves
the management transport, still bumps the liveness heartbeat (RFC 0010 §3.7 — the
supervisor reactor is not paused, only the agentic loops are), so pausing does not
trip liveness.

### 4.4 `cancel` — instance-level cancel by handle (wraps `subagent.cancel`)

`cancel` is the management-transport, instance-scoped wrapper over the existing
`subagent.cancel` tool (RFC 0005 §3.2): cancel a run or a subtree by handle.
"Instance-level" means the caller does not need a prior parent↔child relationship
to the target — the node-agent addresses any handle in *this* instance's tree by
its `agentd://subagent/{handle}` identity. It opens the graceful rung of the kill
ladder (`ctrl/cancel` → grace → `killpg(SIGTERM)` → `killpg(SIGKILL)`, RFC 0003);
the supervisor remains the source of truth for depth/handles.

```jsonc
{ "name":"cancel",
  "inputSchema":{ "type":"object",
    "properties":{
      "handle":{"type":"string"},        // e.g. "0.2"  — omit/"0" => the whole run (root subtree)
      "reason":{"type":"string"}         // surfaced in logs + ctrl/cancel (RFC 0005 §4.3)
    },
    "required":["handle"],
    "additionalProperties":false }}
```
```jsonc
// structuredContent
{ "handle":"0.2", "cancelling": true, "subtree_size": 4 }
```

An unknown handle ⇒ `isError:true` (`"no such handle: 0.2.9"`), not a JSON-RPC
error — it is an observation, and a racing reap may have already removed it.
`cancel{handle:"0"}` (or omitted) cancels the **root** subtree, i.e. the whole run,
*without* exiting the supervisor — distinct from `drain`, which also exits. This
gives agentctl `kubectl agent cancel <run>` (kill the work, keep the pod for the
next reactive trigger) separately from `kubectl agent drain <pod>` (kill the work
*and* exit).

### 4.5 `attach` is `subagent.send` — note it, do not reinvent

Interactive steering of a warm session (`kubectl agent attach`) is **not a new
tool**. It is the existing `subagent.send` (RFC 0005 §3.2), which injects an
instruction/event into a warm subagent session via the `ctrl/inject` control
message (RFC 0005 §4.3). agentctl's `attach` opens a management connection, calls
`subagent.send{handle, event}`, and streams the resulting `agentd://events`
(RFC 0016) and `agentd://subagent/{handle}` `updated` notifications back to the
operator's terminal. agentd adds nothing: the warm-session steering primitive
already exists, and reinventing it would fork the control surface. This RFC only
*names* the mapping so the umbrella's `kubectl agent attach` row (RFC 0014 §4) has
a concrete backing.

---

## 5. Mechanism — the operator resources & the capabilities manifest

### 5.1 New `agentd://` resources (superset of RFC 0005 §3.3)

The operator profile adds three resources to the RFC 0005 §3.3 tree. All are
readable and (where it makes sense) subscribable, emitting
`notifications/resources/updated{uri}` on transition per the RFC 0005 §3.4 rule —
notify-then-read, URI only, no payload.

| URI | Body (on `resources/read`) | Subscribable | Owner |
|---|---|---|---|
| `agentd://capabilities` | the capabilities manifest (§5.2) | yes (re-read on hot reload / model swap) | **this RFC** |
| `agentd://inventory` | the live subagent tree (§5.3) | yes (on any node spawn/exit/transition) | **this RFC** |
| `agentd://status` | instance status incl. downward-API identity (§5.4) | yes | extends RFC 0005 `agentd://run/*` |
| `agentd://events` | live structured event stream | n/a (a stream) | **RFC 0016** — referenced, not defined here |

`agentd://events` is the operator log/telemetry stream and is **specified by
RFC 0016**, not here; this RFC only declares that it is part of the operator
profile and is listed on the management transport. Do not read its schema into
this RFC.

### 5.2 `agentd://capabilities` and `agentd --capabilities` — the manifest

The capabilities manifest is the **shared spine** of the whole control-plane track
(RFC 0014 §5, which delegates the exact schema here). It is emitted two ways from
the **same builder**, so the one-shot and the live resource never drift:

- **One-shot:** `agentd --capabilities` builds the manifest, writes it as JSON to
  **stdout**, and exits **`0`**. It is **side-effect free and runs before any MCP
  connect, LLM call, or socket bind** (the RFC 0011 §3.3 discipline): it reflects
  static config + build features + downward-API env, with the live fields
  (`intelligence.healthy`, counts) reported as their pre-connect/unknown values.
  This is how agentctl probes a binary/image at admission time without starting a
  run.
- **Live resource:** `agentd://capabilities` returns the same document with the
  live fields populated, and emits `updated` when anything in it changes — notably
  on hot reload (RFC 0017) and model/endpoint hot-swap (RFC 0018), so a subscribed
  agentctl re-reads the current capability set without polling.

Full schema (jsonc; every field is part of the frozen contract — §6 of RFC 0014):

```jsonc
{
  "contract_version": "1.0",            // the agentctl<->agentd contract major.minor (RFC 0014 §3.4)
  "agentd_version": "2.1.0",            // env!("CARGO_PKG_VERSION")
  "build_features": ["metrics","serve-mcp","vsock","cron","otel"], // compiled-in cargo features

  "identity": {                          // from the k8s downward API env (§6); fields absent if unset
    "run_id":    "01J8Z3K2QN7…",        // AGENTD_RUN_ID / minted ULID (RFC 0011 §6)
    "instance":  "agent-pod-abc",        // metadata.name      via AGENTD_POD_NAME
    "uid":       "f3c1…-…-…",            // metadata.uid       via AGENTD_POD_UID
    "node":      "node-3",               // spec.nodeName      via AGENTD_NODE_NAME
    "namespace": "agents"                // metadata.namespace via AGENTD_POD_NAMESPACE
  },

  "mode": "reactive",                    // once | loop | reactive | schedule (RFC 0008)
  "model": "claude-opus-4",              // AGENTD_MODEL (the configured model id)

  "intelligence": {                      // summary only; resilience detail is RFC 0018
    "transport": "vsock",                // unix | https | vsock (RFC 0006)
    "endpoints": 2,                      // configured endpoint count (RFC 0018 multi-endpoint)
    "healthy": true                      // last-known reachability; "unknown" pre-connect (one-shot)
  },

  "intelligence_summary": {              // coarse capability hints for placement (no secrets, RFC 0012)
    "toolmode": "native",                // native tool-calling vs JSON-action fallback (RFC 0006)
    "max_context_hint": 200000           // operator-declared context window hint, if configured
  },

  "mcp_servers": [                       // operator-declared client servers (RFC 0004), tags from RFC 0012 §3.1
    { "name":"fs",    "tags":["untrusted_input"] },
    { "name":"vault", "tags":["sensitive"] },
    { "name":"mail",  "tags":["egress"] }
  ],
  "exec_enabled": false,                 // --enable-exec (RFC 0012 §3.6); a trifecta leg if true
  "allow_trifecta": false,               // --allow-trifecta override active (RFC 0012 §3.2)

  "limits": {                            // the bounding box (RFC 0007/0009/0003)
    "max_depth": 4, "max_children": 8, "max_total_subagents": 64,
    "max_steps": 200, "max_tokens": 2000000, "tree_token_budget": 2000000,
    "deadline_ms": 1800000, "drain_timeout_ms": 25000
  },

  "surfaces": {                          // which control-plane contracts THIS binary actually serves
    "management":     "vsock:5005",      // false | "vsock:PORT" | "unix:PATH" — the served self-MCP (§3)
    "operator_tools": ["drain","lame-duck","pause","resume","cancel"], // present on the mgmt transport (§4)
    "metrics":        ":9090",           // false | the /metrics addr (RFC 0010 / RFC 0016)
    "metrics_schema": "1.0",             // frozen metrics schema version (RFC 0016)
    "events":         true,              // agentd://events served (RFC 0016)
    "hot_reload":     true,              // SIGHUP / file-watch reload (RFC 0017)
    "config_validate": true,             // agentd --validate-config available (RFC 0017)
    "exit_codes":     "RFC-0011-§5"      // the exit-code table version this binary honours
  }
}
```

**Decisions baked into the schema:**

- `surfaces` is the **graceful-degradation contract** (RFC 0014 §7). A binary built
  without `serve-mcp` reports `"management": false`; without the metrics surface,
  `"metrics": false`; an instance with none of the control-plane surfaces reports
  them all `false`/absent and agentctl manages it with liveness + exit codes +
  logs only, exactly as today. **agentctl reads `surfaces` and drives only what is
  declared.**
- `surfaces.operator_tools` is the **authoritative list** of operator tools this
  build exposes on the management transport — it mirrors what `tools/list` returns
  to a `Management` peer (§3.4), so agentctl can plan a `kubectl agent` subcommand
  set from the manifest alone, before it opens a management connection.
- **No secrets, ever** (RFC 0012 §3.7): the manifest carries no tokens, no
  credentials, no resolved `{{secret:NAME}}` values — `intelligence.transport` and
  `endpoints` are structural, never the endpoint URL with embedded creds. The
  `Secret` newtype has no `Serialize` (RFC 0012 §3.7), so it cannot reach the
  builder.
- `contract_version` is the field agentctl negotiates on (RFC 0014 §3.4). It is
  the **agentctl↔agentd contract** version, distinct from `agentd_version` (the
  binary release) and from `metrics_schema` (RFC 0016, versioned independently and
  surfaced here so a scraper can branch).

### 5.3 `agentd://inventory` — the live subagent tree (backs `kubectl agent tree`)

`inventory` is the instance-local view of the running subagent tree: per-node
status, depth, and usage. It is the read model behind `kubectl agent tree` and
`kubectl agent describe` (RFC 0014 §4). It is **instance-local** — agentd reports
*its* tree, never a fleet view; cross-instance aggregation is agentctl's job
(RFC 0014 §6). The supervisor already holds every field (it is the source of truth
for the tree — RFC 0003); `inventory` is a pure projection of supervisor state, so
it costs nothing beyond serialization.

```jsonc
// resources/read agentd://inventory
{ "contents":[{ "uri":"agentd://inventory","mimeType":"application/json","text":
  "{ … the object below, stringified … }" }]}
```
```jsonc
{
  "run_id": "01J8Z3K2QN7…",
  "mode": "reactive",
  "draining": false, "paused": false, "ready": true,   // instance-level lifecycle flags (§4)
  "totals": { "active": 3, "total_spawned": 11, "depth": 2,
              "tokens_in": 84120, "tokens_out": 31044, "steps": 218 },
  "nodes": [                                            // one entry per live tree node (RFC 0003/0005)
    { "handle":"0",   "depth":0, "status":"working", "paused":false,
      "scope_summary":["fs","vault","mail"],
      "usage":{ "tokens_in":40100,"tokens_out":15022,"steps":92 },
      "last_event_ms":120 },
    { "handle":"0.2", "depth":1, "status":"working", "paused":false,
      "scope_summary":["fs"],
      "usage":{ "tokens_in":22010,"tokens_out":8011,"steps":61 },
      "last_event_ms":340 },
    { "handle":"0.3", "depth":1, "status":"stalled", "paused":false,
      "scope_summary":["mail"],
      "usage":{ "tokens_in":22010,"tokens_out":8011,"steps":65 },
      "last_event_ms":9400 }                            // high last_event_ms => Detector B watching (RFC 0003)
  ]
}
```

- `status` is the per-node status the supervisor already tracks (working /
  stalled / paused / a terminal status from RFC 0007 §3.4 once reached). This RFC
  introduces **no new status vocabulary** — the terminal-status set is owned by
  RFC 0007 §3.4 and consumed verbatim.
- `inventory` is **subscribable**: it emits `updated` on any node spawn, exit, or
  status transition (the same closed transition set as RFC 0005 §3.4 plus
  spawn/reap). A subscribed agentctl re-reads to refresh `kubectl agent tree`
  without polling. For a deep tree, the same RFC 0005 §3.3 "list vs read"
  cap/prefix-summarize discipline applies — `nodes[]` is capped and summarized
  beyond a depth/breadth threshold rather than unbounded.
- It is **read-only**. Mutations go through the operator *tools* (§4), never by
  writing a resource (MCP resources are not writable here, RFC 0004).

### 5.4 `agentd://status` — instance status + identity

`agentd://status` extends the RFC 0005 `agentd://run/{run_id}` body with the
downward-API identity block (§6) and the instance lifecycle flags, so agentctl has
one canonical status read per instance:

```jsonc
{
  "identity": { "run_id":"01J…","instance":"agent-pod-abc","uid":"f3c1…",
                "node":"node-3","namespace":"agents" },   // §6 — same block as the manifest
  "mode":"reactive", "ready":true, "draining":false, "paused":false,
  "started_at":"2026-06-27T09:58:00Z", "uptime_ms":124000,
  "subscriptions_active":4, "warm_sessions":1,
  "intelligence":{ "transport":"vsock","healthy":true },   // detail in RFC 0018
  "exit_disposition": null                                  // set once terminal (RFC 0011 §5 code)
}
```

This is the **identity-bearing** read: the `identity` block is the one place the
downward-API env surfaces in live state (the manifest carries the same block for
the static probe). agentctl correlates `inventory`/`events`/metrics to a pod via
`identity.uid` + `identity.instance`.

### 5.5 Operator-surface metrics

The metrics schema is **frozen and owned by RFC 0016** (and extends RFC 0010 §3.8).
This RFC adds only the gauges its new lifecycle states need, named in the RFC 0010
convention (`agentd_*`, low-cardinality labels — never `agent_path`/handle in a
label, RFC 0010):

- `agentd_draining` (0/1) — drain latched (`drain` tool or SIGTERM, §4.1).
- `agentd_paused` (0/1) — tree paused (§4.3).
- `agentd_lame_duck` (0/1) — readiness overridden NotReady while alive (§4.2).
- `agentd_ready` (0/1), `agentd_active_subagents`, `agentd_tree_depth` — already
  defined by RFC 0010 §3.8; reused, not redefined.

These three new gauges are registered in the existing hand-written Prometheus
table (RFC 0010 §3.8 — no metrics crate, plain text). Their canonical names/labels
are ratified by RFC 0016 (the freeze authority); listed here so the operator tools'
state is observable. agentctl reads them via the `/metrics` surface declared in
`surfaces.metrics`.

---

## 6. Mechanism — instance identity from the downward API (env-only)

agentd surfaces pod identity (`identity` block in §5.2/§5.4) by reading
**operator-injected environment variables**, set from the Kubernetes downward API
via `valueFrom.fieldRef`. **agentd never calls the kube API** — there is no kube
client, no in-cluster config, no service-account token read; that would pull a
dependency and a cluster coupling the minimalism moat forbids (RFC 0014 §3.3) and
belongs in agentctl. Env in, manifest/status out.

The operator's pod spec injects (conventional names, fixed by this RFC so agentctl
and agentd agree — the one naming contract here):

```yaml
# agentctl's pod template (lives in agentctl, shown for the contract)
env:
  - name: AGENTD_POD_NAME       # -> identity.instance
    valueFrom: { fieldRef: { fieldPath: metadata.name } }
  - name: AGENTD_POD_UID        # -> identity.uid
    valueFrom: { fieldRef: { fieldPath: metadata.uid } }
  - name: AGENTD_POD_NAMESPACE  # -> identity.namespace
    valueFrom: { fieldRef: { fieldPath: metadata.namespace } }
  - name: AGENTD_NODE_NAME      # -> identity.node
    valueFrom: { fieldRef: { fieldPath: spec.nodeName } }
  # AGENTD_POD_GRACE_SECONDS (RFC 0011 §3.3, drain-vs-grace) rides the same downward-API convention
```

```rust
// identity.rs — pure env read, no syscalls beyond getenv, no validation side effects
struct Identity {
    run_id:    Ulid,           // RFC 0011 §6 (always present — minted if unset)
    instance:  Option<String>, // AGENTD_POD_NAME
    uid:       Option<String>, // AGENTD_POD_UID
    namespace: Option<String>, // AGENTD_POD_NAMESPACE
    node:      Option<String>, // AGENTD_NODE_NAME
}
fn from_env() -> Identity { /* getenv each; absent => None; emit nothing durable */ }
```

Rules:

- **Every field is optional.** Outside Kubernetes (bare-metal, local CLI) the env
  vars are simply unset and the `identity` fields are absent/`null`. The manifest
  and `agentd://status` are valid with `run_id` alone (always present, RFC 0011 §6).
  No env var here is *required*; their absence is never a config error.
- **Identity is descriptive, never load-bearing.** agentd uses these for
  correlation/labelling only; it makes **no decision** based on `node`/`namespace`
  (no placement, no scheduling — that is agentctl, RFC 0014 §6). This keeps the
  values pure metadata.
- **Shared with RFC 0011 §3.3.** `AGENTD_POD_GRACE_SECONDS` (the
  `terminationGracePeriodSeconds` hint for drain-vs-grace validation) is the same
  downward-API convention; this RFC and RFC 0011 §10 settle on the
  `AGENTD_POD_*` / `AGENTD_NODE_NAME` family as the one naming contract (§10).
- **No secrets via the downward API.** Only the four identity fields above and the
  grace hint; downward-API `secretKeyRef` for credentials is the RFC 0012 §3.7
  env-secret path, never folded into `identity`.

---

## 7. Trust boundary & who may call operator tools

The access-control model is **structural, transport-scoped, and identical in
shape to RFC 0012 §3.8** — there is no in-band auth in v1 (RFC 0012 §2: no auth as
core). The rule is one sentence:

> **Whoever can reach the management transport may call the operator tools.**

Concretely:

- **unix socket (`--serve-mcp unix:PATH`):** access control is **filesystem
  permission** on the socket — the operator sets the socket mode/owner so only the
  node-agent (or a co-located admin) can connect (RFC 0012 §3.8). Anyone who can
  `connect(2)` the socket is, by construction, trusted to drain/cancel this
  instance.
- **vsock (`--serve-mcp vsock:PORT`):** access control is the **VM boundary** — the
  peer is the host, i.e. the node-agent (§3.3). One trust domain. The operator
  ensures only the trusted node-agent can reach the guest vsock port, exactly as it
  ensures only trusted principals can open the unix socket.
- **stdio:** a peer reaching agentd over stdio is a *parent agentd / harness* that
  spawned it (RFC 0005 §3.6); it gets the **work** profile only — operator tools
  are not listed to it (§3.4). A subagent cannot drain its supervisor.

This is honest minimalism (RFC 0012 §3.8): agentd ships **no network-exposed
control surface it cannot secure**. The management surface is reachable only over a
unix socket (filesystem perms) or vsock (VM boundary) — both confinement
properties of the *deployment*, not in-band tokens agentd would have to validate.
A network-exposed (TCP/HTTP) management transport, with the auth model and
`Origin`/session hardening it requires (RFC 0012 §3.8), is **deferred to RFC 0013**
and out of scope here.

The capability gate (§3.4) and the trust boundary compose: operator tools are
(a) listed only to a `Management` peer, and (b) a `Management` peer exists only on
a transport whose reachability the deployment controls. There is no path by which
an untrusted network principal, or a spawned subagent, calls `drain`.

---

## 8. Edge cases & failure semantics

**Capability absence is not an error.** A binary built without a feature simply
does not list the corresponding tool/resource and reports it `false`/absent in
`surfaces` (§2.5, RFC 0012 §3.6 rule). Examples, all by-design, never errors:

- `serve-mcp` not compiled in → `--serve-mcp` is rejected at config (exit 2,
  §3.1); the running default build has `surfaces.management: false`; agentctl
  manages it by liveness + exit codes + logs only (RFC 0014 §7).
- `vsock` not compiled in → `--serve-mcp vsock:…` is exit 2 (§3.1); `unix:` still
  works. A `vsock`-built binary running with `--serve-mcp unix:…` reports
  `"management":"unix:/run/agentd.sock"` and is fully manageable host-side over the
  unix socket — vsock is one management transport, not the only one.
- The metrics/events/hot-reload surfaces off → reported `false` in `surfaces`;
  agentctl reads the manifest and does not attempt those operations.

**Unknown operator tool / resource on a binary without it.** A `tools/call` for a
tool not in this build's `tools/list` is a JSON-RPC `-32601` (method/tool not
found) — the standard MCP error for an unknown tool (RFC 0005 §3.2), *not* a
custom error and *not* a crash. agentctl should never call it: it learns the set
from `surfaces.operator_tools` + `tools/list` first (§5.2). A `resources/read` of
an unlisted `agentd://` URI is `-32002` with `data.uri` (RFC 0005 §3.3).

**`drain` after drain / `drain` racing SIGTERM.** Idempotent and monotonic
(§4.1): the `DRAINING` latch is one-way (RFC 0011 §4.2). A second `drain`, or a
SIGTERM after a `drain` tool call, re-reports the snapshot; neither maps to FORCE.
FORCE remains the *second signal* (RFC 0011 §4.3). If the management connection
drops mid-drain, drain proceeds regardless — it is supervisor state, not
connection state (RFC 0011 §4.4: SIGKILL safety is a property of state).

**`lame-duck{ready:true}` over a genuinely-not-ready supervisor.** Refused as
`isError:true` with the unmet readiness condition (§4.2). The override can only
push *toward* NotReady; it can never assert Ready over a supervisor whose computed
readiness (MCP connected + subs reconciled, RFC 0010 §3.7) is false.

**`cancel` of a vanished handle.** `isError:true` (`"no such handle"`), not a
JSON-RPC error — a racing reap may have already removed the node (RFC 0003).
`cancel{handle:"0"}` cancels the run without exiting; `drain` cancels *and* exits —
agentctl picks per intent (§4.4).

**Operator tool on stdio.** Not listed (§3.4); a `tools/call` for it is `-32601`.
A spawned subagent therefore cannot discover or invoke `drain`/`lame-duck`/etc.
against its parent.

**`--capabilities` before connectivity.** `agentd --capabilities` never connects,
never binds, never spawns; it exits `0` with the static+env view (§5.2). Live
fields (`intelligence.healthy`, node counts) read as their unknown/zero values.
This is the admission-time probe and must not have side effects (RFC 0011 §3.3).

**Management transport down vs supervisor down.** A dropped management connection
is not a liveness signal — liveness is the supervisor heartbeat (RFC 0010 §3.7),
independent of any served connection. agentctl reconnecting after a node-agent
restart sees the same instance (correlated by `identity.uid`), re-`initialize`s,
and re-subscribes; agentd holds no per-connection durable state (RFC 0011 §7), so
reconnect is a clean re-read.

**Contract-version mismatch.** agentctl reads `contract_version` first; if it does
not understand the **major**, it refuses to drive the instance (RFC 0014 §3.4) and
falls back to liveness + exit-code + log management. agentd never *rejects* an
agentctl on version grounds — it serves the manifest and lets the client decide; a
newer agentctl drives an older agentd within the same major additively.

**`drain` vs grace.** A `drain` tool call honours the same `AGENTD_DRAIN_TIMEOUT`
< `terminationGracePeriodSeconds` invariant (RFC 0011 §3.3): an optional
`deadline_ms` is clamped to `AGENTD_DRAIN_TIMEOUT`, never above it, so a tool call
can never push drain past the pod grace and lose the clean `0` exit.

---

## 9. Non-goals (these stay in agentctl)

Per RFC 0014 §6, restated for this surface:

- **No Kubernetes anything in agentd.** No CRDs, no operator reconcile loop, no
  `kubectl` plugin, no RBAC, no leader election, no in-cluster client. agentd reads
  four downward-API env vars (§6) and nothing else from the cluster.
- **No fleet view, no cross-instance aggregation.** `agentd://inventory` is
  instance-local; `kubectl agent tree` *across* a fleet is agentctl stitching many
  per-instance inventories (RFC 0014 §6).
- **No scheduling/placement decisions.** `identity.node`/`namespace` are
  descriptive metadata; agentd makes no placement decision (RFC 0014 §6).
- **No network-exposed (TCP/HTTP) management transport, no auth model.** Deferred
  with the self-MCP-over-HTTP hardening to RFC 0013 (RFC 0012 §3.8). v1 management
  is vsock + unix only.
- **No new protocol.** The operator profile is MCP (RFC 0004/0005) — more resources
  and tools on the existing wire. No bespoke management RPC (RFC 0014 §3.2).
- **No `force` tool, no dashboards, no alert rules.** Force is the second signal
  (RFC 0011 §4.3); dashboards/alerts are agentctl over the RFC 0016 metrics.
- **Provisioning the vsock device / host model service** — the node-agent's job
  (RFC 0014 §6); agentd only binds the CID/port it is given (§3.3).

agentd's contribution is to make each of these **cheap to build** by exposing the
right primitive — never to implement them.

---

## 10. Open items (for the umbrella author to reconcile)

- **Downward-API env naming — single source of truth.** This RFC fixes
  `AGENTD_POD_NAME` / `AGENTD_POD_UID` / `AGENTD_POD_NAMESPACE` / `AGENTD_NODE_NAME`
  (and reuses `AGENTD_POD_GRACE_SECONDS` from RFC 0011 §3.3/§10). RFC 0011 §10
  flagged the grace-hint name as an unsettled documentation convention; this RFC
  proposes the whole `AGENTD_POD_*`/`AGENTD_NODE_NAME` family as the one contract.
  The umbrella (RFC 0014) should ratify this family once so agentctl's pod template
  and agentd's `identity.rs` cannot drift.
- **`surfaces` schema co-ownership with RFC 0016/0017/0018.** The `surfaces` block
  (§5.2) names fields owned by sibling RFCs (`metrics_schema` → RFC 0016,
  `hot_reload`/`config_validate` → RFC 0017, intelligence detail → RFC 0018). This
  RFC defines the *block and its keys*; the sibling RFCs own the *truth* of each
  flag. The umbrella should confirm `surfaces` is the single manifest location for
  all cross-RFC capability advertisement, so no sibling invents a parallel one.
- **`contract_version` bump policy across sub-RFCs.** Adding `agentd://events`
  (RFC 0016) or hot-reload fields (RFC 0017) to the manifest is additive within
  `1.x`; the umbrella should own the rule that *any* sub-RFC adding a `surfaces`
  key bumps the **minor** (not the major), and pin which changes are major.
- **vsock CID convention for multi-pod-per-VM.** §3.1 binds `VMADDR_CID_ANY` per
  process, assuming one agentd per guest network namespace. If a future deployment
  runs multiple agentd pods sharing one VM/CID, port allocation becomes the
  node-agent's responsibility (it already provisions the device, §3.3); worth a
  one-line confirmation in the umbrella that port-per-pod is the node-agent's job,
  not agentd's.
- **`pause` semantics vs reactive subscriptions.** §4.3 pauses agentic loops but
  leaves subscriptions armed (events still arrive, just not processed until
  `resume`). Whether agentctl's `kubectl agent pause` should *also* imply pausing
  subscription routing is a policy call for agentctl; the umbrella should note that
  agentd offers both levers (`pause` for loops, `lame-duck` for readiness) and the
  composition is agentctl's.

---

## 11. References

- **RFC 0003** — process supervision & recovery: the kill ladder `cancel`/`drain`
  open, the subagent tree `inventory` projects, hierarchical usage accounting.
- **RFC 0004** — MCP client subset & codec: the wire/codec the operator profile
  speaks; resources are read-only, mutations are tools.
- **RFC 0005** — self-MCP server & control protocol: the surface this RFC profiles;
  `subagent.*`/`subscribe`/`resource.read`, the `agentd://` resource tree, the
  unix-listener design vsock mirrors, `subagent.send` (= `attach`), the
  `ctrl/pause`/`resume`/`cancel`/`inject` control messages.
- **RFC 0006** — intelligence transport & wire: the `vsock` feature and
  `VsockStream` (client) this RFC reuses for the listening half; the
  `vsock:<cid>:<port>` URI grammar.
- **RFC 0007** — agentic loop & terminal status: the terminal-status vocabulary
  (§3.4) `inventory.status` consumes verbatim; turn-boundary checkpoints
  `pause`/`cancel` act on.
- **RFC 0008** — execution modes & reactive routing: `mode` in the manifest;
  reactive readiness `lame-duck` overrides.
- **RFC 0009** — subagent process model: depth-minting and the spawn chokepoint
  `cancel` and `inventory` reference.
- **RFC 0010** — observability, health & telemetry: `/readyz`/`/healthz` and the
  readiness flag `lame-duck`/`drain` flip; the `agentd_*` metric naming convention;
  liveness = supervisor heartbeat (independent of the management connection).
- **RFC 0011** — cloud-native contract: the SIGTERM drain choreography `drain`
  reuses (§4.2), the exit-code table a clean drain returns `0` against (§5), the
  `RUN_ID` identity (§6), and the downward-API grace-hint convention (§3.3/§10).
- **RFC 0012** — security posture: the transport-is-the-boundary access model
  (§3.8) vsock extends; trifecta tags surfaced in the manifest; the `Secret`
  newtype that keeps credentials out of the manifest; capability-absence-not-error.
- **RFC 0013** — deferred v2 surface: Streamable HTTP serving + an auth model for a
  network-exposed management transport (out of scope here).
- **RFC 0014** — control-plane (agentctl) contract: the umbrella this slots under;
  the primitives-not-policy principle (§3), the capabilities-manifest spine (§5,
  whose schema this RFC owns), and the contract-versioning/freeze rule (§3.4).
- **RFC 0016** — telemetry & lifecycle contract: owns `agentd://events`, the frozen
  metrics schema (`metrics_schema` in `surfaces`), and the exit-code-table freeze
  this manifest cites — referenced, not redefined here.
- **RFC 0017** — declarative config & hot reload: owns `hot_reload`/`config_validate`
  in `surfaces` and the `agentd://capabilities` `updated`-on-reload behaviour.
- **RFC 0018** — intelligence transport resilience: owns the `intelligence`
  endpoint/health detail summarized in the manifest and the model/endpoint hot-swap
  that triggers a `capabilities` `updated`.
