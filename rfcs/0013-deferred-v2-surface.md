# RFC 0013: Deferred v2 Surface — tasks, sampling, roots, Streamable HTTP serving, session checkpointing

**Status:** Accepted (v2 surface, deferred)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

> **Forward note (control-plane track).** Two items here are dependencies of the
> agentctl control-plane track (RFC 0014): **Streamable HTTP serving** is the
> cluster-network alternative to the vsock management transport (RFC 0015 §3),
> and **MCP-backed session checkpointing** is the durability primitive a stateful
> fleet agent would use to resume across a pod reschedule (RFC 0014 §4). The track
> does **not** pull either forward; it references them as the named v2 path.

> **A2A alignment (RFC 0020).** RFC 0020 (A2A-over-vsock) reframes **D7b — self-MCP-
> over-HTTP serving** from *deferred for agentd* to **out of scope for agentd**.
> Serving A2A and the management surface over vsock behind an on-node HTTP↔vsock
> gateway (RFC 0020 §2–§3; the gateway is the agentctl node-agent, RFC 0014) puts
> HTTP termination in the gateway — so agentd **may never need an HTTP server at
> all**. D7b's v2 server work is thus the gateway's concern, not a deferred agentd
> surface; see RFC 0020 §8. (D7a inbound HTTP-client reactivity is untouched.)

---

## 1. Problem / Context

Every other RFC in this set (0001–0012) specifies what v1 **builds**. This
RFC specifies what v1 **deliberately does not build**, so that the absence is
a recorded decision rather than an oversight, and so the v1 fallback for each
deferred item is named and implemented elsewhere. This is the explicit DEFER
list from the architecture-decision document: assessment §1.3 (MCP-protocol
corrections), §2.5 (the MCP client/server minimal subset and its `DEFER`
line), §2.8 (the deferred checkpoint), and the top-risks list in §5.

The bias is the project's moat: **minimalism is the edge, and a deferred
surface that is cheap to add later but expensive to maintain now is a
liability in v1.** Each item below is a real MCP-spec feature or a real
operational capability that a maximalist runtime would ship. For each we state
WHAT it is on the wire / in the OS, WHY it is out of v1 scope, and the exact
v1 FALLBACK that an engineer implements in the named core RFC instead.

**Nothing in this RFC is v1 scope.** This document authorises no v1 code. It
exists to (a) prevent re-litigation of these decisions during the M1–M7 build,
(b) record the wire shapes and entry points so v2 work starts from a precise
baseline rather than a fresh spec read, and (c) make each fallback's
load-bearing invariant explicit so v2 does not silently break it.

The defer list, verbatim from assessment §2.5:

> **DEFER (explicit):** Streamable HTTP resumability/SSE-replay; `prompts/*`;
> `sampling/createMessage` (both directions); `roots/*`; `elicitation/*`;
> `completion/*`; **`tasks/*`**; emitting `notifications/message`/`progress`
> from our server; the old 2024-11-05 HTTP+SSE transport (never).

Plus from §2.8: the optional MCP-backed checkpoint of supervisor-owned facts.
Plus operational deferrals surfaced in §2.4 / §2.6 / risk #1: a third
intelligence adapter, and richer cron calendars/DST.

The 2024-11-05 HTTP+SSE two-endpoint transport is in a class of its own: it is
not deferred, it is **never** implemented (assessment §1.3.4, §2.2 "Explicitly
OUT", notes §10.3). It is mentioned here only to be excluded from "Streamable
HTTP serving" — the modern transport is the v2 target; the old one is dead.

---

## 2. Decision

The following are **out of v1 scope** and targeted at v2. Each has a named v1
fallback already specified in another RFC; this RFC does not re-specify the
fallback, it cross-references it.

| # | Deferred item | Direction / locus | v1 fallback (RFC) |
|---|---|---|---|
| D1 | MCP `tasks/*` — durable/pollable requests | client + server, MCP wire | request/response + `progress` + `cancelled` (RFC 0004); self-MCP async via subscribable `agentd://` resource (RFC 0005/0008) |
| D2 | `sampling/createMessage` | server→client; CLIENT capability | declare no client caps; reject if sent; own the LLM directly in the loop (RFC 0006/0007) |
| D3 | `roots/*` | server→client; CLIENT capability | declare no `roots`; answer `roots/list`→`{"roots":[]}` (RFC 0004) |
| D4 | `elicitation/*` | server→client; CLIENT capability | declare no `elicitation`; headless, no interactive turn (RFC 0004) |
| D5 | `completion/complete` | client→server | never call it; no autocomplete UX headless (RFC 0004) |
| D6 | `prompts/*` | client + server | declare no `prompts`; ignore server prompts (RFC 0004/0005) |
| D7a | Reactive-over-HTTP (SSE GET client) | client transport | reactivity on stdio only (RFC 0004/0008) |
| D7b | Self-MCP-over-HTTP serving (full Streamable HTTP server) | server transport | serve self-MCP on stdio + unix-socket only (RFC 0005) |
| D8 | MCP-backed warm-session + supervisor-fact checkpoint | supervisor state | stateless rebuild + reconcile (read-after-subscribe), idempotent re-trigger (RFC 0003/0011) |
| D9 | Emitting `notifications/message` / `notifications/progress` from our server | server | log to stderr (RFC 0010); update self-resources (RFC 0005) |
| D10 | Richer cron — calendars, DST, missed-tick catch-up, job-store | trigger | 5-field UTC cron (`cron` feature) + external CronJob (RFC 0008) |
| D11 | A third+ in-binary intelligence adapter | intelligence | two adapters (openai-compatible + anthropic); push quirks to the gateway (RFC 0006) |

The remainder of this RFC is the per-item detail: WHAT / WHY / FALLBACK plus
the wire shapes and code entry points a v2 implementer needs.

---

## 3. Mechanisms (per deferred item)

### D1 — MCP `tasks/*` (durable / pollable requests)

**WHAT.** `tasks` is the 2025-11-25 spec feature for task-augmented requests:
a request the receiver may execute *durably and asynchronously*, returning a
task handle the caller polls, lists, and cancels, instead of blocking for a
single response. It is the **spec-native shape for the external-facing
long-running surface** (assessment §1.3.8). On the wire it manifests as:

- Capability negotiation in `initialize`. Client side:
  `capabilities.tasks.requests.{tools:{call:{}}, sampling:{createMessage:{}}, elicitation:{create:{}}}`.
  Server side: `capabilities.tasks.{list:{}, cancel:{}, requests.{...}}`
  (notes §1.1–§1.2).
- `execution.taskSupport` on a tool descriptor: `"forbidden" | "optional" |
  "required"` (notes §2.1) — the server declaring whether a given tool may be
  invoked as a task.
- A task lifecycle: a task-augmented `tools/call` returns a task handle rather
  than (or in addition to) a terminal result; `tasks/list` enumerates;
  `tasks/cancel` cancels; the caller polls or is notified of completion.

**WHY deferred.** It is a *meaningful surface*, not a flag: a durable request
lifecycle, a task store, status modelling, list/cancel methods, and a second
notion of "in-flight" parallel to the JSON-RPC request id. v1's long-running
needs are already met by progress + cancel for **inbound** tool calls and by
the supervised subagent tree for **internal** async work (RFC 0009). Adding
`tasks` to v1 would duplicate, in the MCP layer, the durable-handle semantics
the supervisor already provides over its own control channel — without buying
anything v1 deployments require.

**v1 FALLBACK.**

- *As client* (RFC 0004): never advertise `tasks`; never set
  `_meta`-task fields; treat every `tools/call` as synchronous request/response.
  Tolerate long calls with **`notifications/progress`** (reset the per-request
  timeout on each progress, with an absolute ceiling) and abandon with
  **`notifications/cancelled`** on a deadline/budget trip (RFC 0004 §liveness;
  notes §8.2–§8.3). If a server marks a tool `execution.taskSupport:"required"`,
  v1 logs `limit.exceeded`-style and treats the tool as unavailable (do not
  fabricate a task lifecycle).
- *As server* (RFC 0005/0008): the external "spawn a child and await its
  result" surface is the `subagent.spawn` self-tool, **not** `tasks`. Async
  completion is delivered the agentd-native way: an `{async:true}` spawn
  returns a handle and the parent subscribes to a subscribable
  `agentd://subagent/{id}/result` resource; completion arrives as
  `notifications/resources/updated{uri}` → `resources/read` (assessment §2.7,
  §2.5; notes §3.5). This reuses the reactive machinery (RFC 0008) instead of a
  parallel task store.

**v2 entry point.** `wire/mcp.rs` capability map + `mcp/client.rs` and
`mcp/server.rs`. `tasks` maps cleanly onto the existing async-subagent handle
model, so v2 is largely a wire-translation layer over machinery RFC 0009
already built. Keep the internal control protocol (RFC 0005, length-framed
JSON-RPC sibling) **separate** from the public `tasks` surface — risk #10:
never leak the internal supervision wire outward as `tasks`.

### D2 — `sampling/createMessage` (intelligence-sharing)

**WHAT.** Sampling is a **reverse** MCP call: the *server* asks the *client*
to run an LLM generation (`sampling/createMessage`, notes §5). Critically,
**sampling is a CLIENT capability** — server→client direction. "A peer uses
agentd's intelligence" therefore means agentd would have to act as a
*sampling-capable client* that services inbound `sampling/createMessage`
requests by calling its own intelligence endpoint and returning the
completion. agentd **cannot serve sampling as an MCP server** — that is the
wrong directionality (assessment §1.3.5).

Wire shape (notes §5.1–§5.2): `params.{messages[], modelPreferences{hints[],
costPriority, speedPriority, intelligencePriority}, systemPrompt?,
maxTokens, temperature?, stopSequences?, tools?, toolChoice?}` →
`result.{role:"assistant", content, model, stopReason}`. Tool-use sampling
(notes §5.4) adds a multi-turn `tool_use`/`tool_result` exchange gated on the
client declaring `sampling.tools`.

**WHY deferred — both directions.**

- *As client (servicing inbound sampling):* extra surface — agentd's agentic
  loop already owns the LLM directly (RFC 0006/0007); servicing
  server-initiated sampling adds a second, externally-triggered path into the
  intelligence endpoint with its own budget/credential/injection concerns.
- *As server (issuing sampling):* impossible by directionality, and the
  agent-to-agent intelligence-sharing use case is a strong but non-essential
  v2 feature.

**v1 FALLBACK** (assessment §2.5 client list; notes §10.1 "Client-feature
replies"). v1 **declares no client capabilities** — *no `sampling` in either
direction.* If a server sends `sampling/createMessage` unsolicited, v1
**rejects** it: respond with a JSON-RPC error. Use `-32601` (method not found)
since we never advertised the capability (the spec's `-1` user-rejected code
presupposes a sampling-capable client declining a specific request, which is
not our case). The agentic loop calls the intelligence endpoint itself; no MCP
peer borrows it.

**v2 entry point.** `mcp/client.rs` inbound dispatch + a new servicing path
into `intel/client.rs`. `modelPreferences.hints`/priorities map onto agentd's
`--model`/gateway routing (notes §5.3). v2 must add per-peer budget accounting
and treat sampled prompts as untrusted (RFC 0012 trust budget).

### D3 — `roots/*`

**WHAT.** A CLIENT capability (server→client direction): the client declares
`roots:{listChanged}` and answers `roots/list` with `file://` URIs marking
filesystem boundaries; notifies `notifications/roots/list_changed` on change
(notes §6). Not-supported ⇒ `-32601`.

**WHY deferred.** A filesystem-scope *signal* to servers; agentd is headless
and does not need to negotiate filesystem boundaries with the servers it
drives. Marginal value, real surface.

**v1 FALLBACK** (assessment §2.5; notes §6 verdict, §10.1). Do **not** declare
`roots`. If a server calls `roots/list` anyway, answer with the empty list
`{"roots":[]}` (a safe, spec-legal reply that declares no boundaries) rather
than `-32601`, so a roots-curious server interoperates without us advertising
the capability. Never *send* `notifications/roots/list_changed`.

**v2 entry point.** `mcp/client.rs` + `wire/mcp.rs`. Add when a tool server
genuinely needs filesystem-boundary negotiation; the empty-list answer is
already the degenerate case of the real implementation.

### D4 — `elicitation/*`

**WHAT.** A CLIENT capability (server→client): a server requests structured
input from the user mid-call (form-mode or, new in 2025-11-25, URL-mode);
the client renders/answers. Declared as `elicitation:{form:{}, url:{}}`
(notes §1.1).

**WHY deferred.** An *interactive UX surface* — there is no user at a keyboard
in a headless agent. A server's elicitation request has no one to answer it.

**v1 FALLBACK** (assessment §2.5; notes §10.3). Declare no `elicitation`.
agentd never offers an interactive turn. If a server depends on elicitation,
that tool is simply unusable in this deployment — surfaced as a tool-domain
error/observation to the model (RFC 0007 error taxonomy), not a crash.

**v2 entry point.** Only meaningful if agentd ever gains an operator-facing
interactive channel; no concrete v2 plan. Listed for completeness of the
defer record.

### D5 — `completion/complete`

**WHAT.** A client→server method backing argument autocompletion for prompts
and resource-template URIs; gated on the server's `completions` capability
(notes §1.4).

**WHY deferred.** Autocomplete is an interactive editor/UX feature. Headless
agentd composes full requests; it never needs to autocomplete an argument.

**v1 FALLBACK** (assessment §2.5; notes §10.3). Never call `completion/*`.
Ignore the server's `completions` capability. No fallback behaviour is needed
— the feature has no headless analogue.

**v2 entry point.** None planned. Defer-of-record only.

### D6 — `prompts/*`

**WHAT.** Server-offered prompt templates: `prompts/list`, `prompts/get`
(returns `messages[]` with role + content block), and
`notifications/prompts/list_changed` (notes §4). User-controlled
slash-command templates.

**WHY deferred.** Irrelevant to a headless agent — prompts are
human-driven slash-command surfaces. A server that offers prompts is harmless;
we ignore them.

**v1 FALLBACK** (assessment §2.5; notes §4 verdict, §10.3). As **client**:
never call `prompts/*`; ignore the `prompts` capability and any
`notifications/prompts/list_changed`. As **server**: do not declare the
`prompts` capability on the self-MCP; expose nothing under `prompts/*`.

**v2 entry point.** Trivial to add later — `prompts/get` returns the same
content-block types (notes §2.3) the tool path already parses, so the codec
work is reused. No surface beyond a method handler. Low priority.

### D7 — Streamable HTTP (reactivity-over-HTTP **and** self-MCP-over-HTTP serving)

This is two distinct deferrals that share a transport. Both stem from
assessment §1.3.4, §1.3.6, §2.2, §2.13, and risk #1.

**Transport terminology (binding).** "HTTP/SSE" is stale. The modern transport
is **Streamable HTTP** (single `/mcp` endpoint, POST + GET, introduced
2025-03-26). The old **2024-11-05 HTTP+SSE** two-endpoint transport is
deprecated and **NEVER implemented** (assessment §2.2 "Explicitly OUT"; notes
§9.2 backwards-compat). v2's HTTP work targets Streamable HTTP only.

#### D7a — Reactive-over-HTTP (SSE GET client)

**WHAT.** Receiving `notifications/resources/updated` from a *network* MCP
server over Streamable HTTP requires the client to hold open a **long-lived
SSE GET stream** (`GET /mcp`, `Accept: text/event-stream`), parse `event:`/
`data:`/`id:` frames, and reconnect with `Last-Event-ID` for replay (notes
§9.2). This is materially more than "a tiny blocking HTTP client" — it is a
streaming, resumable, reconnecting consumer (assessment §1.3.4, risk #1).

**WHY deferred.** The reactive thesis — agentd's edge — is delivered fully
over **stdio resource subscriptions** (assessment §1.3.4: "v1 keeps reactivity
on stdio only"). An SSE GET client is a large, fiddly addition for a transport
v1 deployments do not need: the recommended shape connects MCP servers as
stdio children or unix-socket peers, where subscriptions already work.

**v1 FALLBACK** (assessment §2.5, §2.12; notes §10.1, §10.3). Reactivity is
**stdio-only**. `resources/subscribe` + consume `notifications/resources/
updated` (notify-then-read) over the stdio line codec (RFC 0004). Network
servers can still be used **synchronously** over the hand-rolled HTTP/1.1
client (RFC 0006-shared `net/http.rs`) for non-reactive `tools/call` /
`resources/read`, but their push notifications are not consumed — the reactive
trigger is unavailable for HTTP servers in v1, and that degradation is
surfaced (notes §10.4 capability gating: "degrade gracefully if a server lacks
... fall back to poll, or surface that the reactive trigger is unavailable").

#### D7b — Self-MCP-over-HTTP serving (full Streamable HTTP server)

**WHAT.** Serving agentd's own self-MCP (RFC 0005) over Streamable HTTP is a
real server: POST + GET on one endpoint, single-`application/json` **and**
SSE-upgrade responses, `MCP-Session-Id` assignment + echo (400 on missing,
404 on ended), `MCP-Protocol-Version` header (400 on invalid), `Origin`
validation → **403**, optional resumability (SSE `id:` / `Last-Event-ID`
replay on the same stream only), and `DELETE` session termination (notes
§9.2). Plus the security hardening of assessment §2.11: non-deterministic
session IDs, sessions-not-as-authn, no token passthrough, loopback binding.

**WHY deferred.** The full Streamable HTTP server is a large, security-sensitive
surface (assessment §1.3.6 "self-MCP-over-HTTP cost is understated"). The two
common deployment shapes — stdio children and unix-socket peers — need none of
it. Shipping it in v1 would import the SSE-server + session-management +
Origin-hardening surface for a transport most builds do not use.

**v1 FALLBACK** (assessment §2.5, §2.13; notes §10.2). The self-MCP serves on
**stdio always**, and on **unix-socket** (NDJSON, stdio-like framing) when
`--serve-mcp unix:…` is set (RFC 0005). Agent-to-agent reactivity (a peer
agentd subscribing to our `agentd://…` state resources) works over unix-socket
— full symmetry without HTTP. HTTP serving is behind the (default-off,
v1-unimplemented) `serve-mcp` feature's HTTP path; v1 ships unix only.

**v2 entry point.** `mcp/server.rs` HTTP path + `net/http.rs` server side;
gated behind `serve-mcp` (assessment §2.2). `mio`/`libc::poll` is the
reserved primitive for the high-fan-in many-idle-peer-connection case
(assessment §2.1, §2.2). v2 MUST implement the §2.11 hardening before exposing
HTTP serving: non-deterministic session IDs, Origin→403, loopback default,
no-token-passthrough.

### D8 — MCP-backed warm-session + supervisor-fact checkpointing

**WHAT.** An optional durable checkpoint of **supervisor-owned facts** —
specifically the *subscription set* + the *handle map* (live subagent handles)
+ the *routing table* — written with an **atomic write + `fsync`** of both the
file and its directory, to an MCP-backed store (never local disk, which is not
durable across pod reschedule; notes §4.4 "Explicitly reject"). Plus, the
heavier variant: serializing **warm reactive-session context** keyed by
`RUN_ID` so multi-turn sessions survive a restart (notes §4.3 v2 tier).

**WHY deferred.** Adds a serialization format and a store contract that
violate the minimalism bar for the common case (notes §4.3). The v1
rebuild+reconcile pattern makes a checkpoint *rarely necessary*: edge→level
reconciliation recovers missed events without persisted state. Hard
constraint: **never checkpoint live agentic context or pipes** (assessment
§2.8) — only supervisor-owned facts are ever candidate state.

**v1 FALLBACK** (assessment §2.8; notes §4.3 v1 tier, §12). The supervisor is
**stateless**; on restart it does **rebuild + reconcile** (RFC 0003/0011):

1. Re-read config (env/flags/file).
2. Re-establish all MCP connections.
3. Re-issue `resources/subscribe` for **every declared subscription**, and
   **`resources/read` each immediately after subscribe** — converting
   edge-triggering to level-triggering across the restart boundary. This
   read-after-subscribe step is **mandatory, not optional** (assessment §2.8;
   notes §12: "without that reconciliation step, a restart silently drops
   events that occurred while down").
4. Warm in-memory sessions and **dynamic** (self-arranged via the `subscribe`
   self-tool) subscriptions are **lost**; recovered by **idempotent
   re-trigger** — the reconciled resource re-fires if still in the triggering
   state, and reactions are idempotent (RFC 0011: stable `RUN_ID` into MCP
   `_meta`, read-modify-write-through-MCP, "already done" → exit 0 cheaply).

The minimum recoverable unit retained per child for the child's lifetime is
the spawn payload (instruction + seed + scope + limits + usage) — enabling
bounded restart (RFC 0003) — but this is in-memory, not a durable checkpoint.

**v2 entry point.** A new optional `supervisor` checkpoint module writing
through an MCP store keyed by `RUN_ID`, off by default behind a flag. Atomic
write + fsync file **and** dir. v2 must preserve the v1 invariant that
rebuild+reconcile remains correct even with the checkpoint present (the
checkpoint is an optimization, never the source of truth).

### D9 — Emitting `notifications/message` / `notifications/progress` from our server

**WHAT.** As an MCP server, agentd could declare `logging` and emit
`notifications/message` (RFC 5424 severities; notes §7) and emit
`notifications/progress` against inbound `progressToken`s (notes §8.2) to
stream its internals to a driving client.

**WHY deferred.** Nice observability, not load-bearing. v1 has a complete
observability story without it.

**v1 FALLBACK** (assessment §2.5 defer line; RFC 0010). Internals go to
**stderr as JSON-lines** (assessment §2.9: stdout = result only; stderr = all
telemetry) and to **subscribable `agentd://` state resources** (RFC 0005) — a
driving peer subscribes to our state resources for progress rather than
receiving server-pushed `message`/`progress`. Note v1 *consumes* inbound
`notifications/message` and `notifications/progress` as a **client** (RFC
0004); it just does not *emit* them as a server.

**v2 entry point.** `mcp/server.rs`; cheap to add — reuses the notification
serializer already built for resource-updated emission.

### D10 — Richer cron (calendars / DST / missed-tick catch-up / job-store)

**WHAT.** A full internal scheduler: calendar arithmetic, DST handling,
timezone job definitions, missed-tick catch-up after downtime, and a
persistent job-store surviving restarts.

**WHY deferred.** The production time-scheduling path is **external CronJob →
`--mode once`** (assessment §2.6; notes §0, §9-C): strictly more robust to
clock skew and restart, more observable, more 12-factor. Building a robust
internal scheduler duplicates the orchestrator, which is explicitly out of
scope (RFC 0001 §2).

**v1 FALLBACK** (assessment §2.6; RFC 0008). Internal time-scheduling is a
**standalone convenience**: `--interval D` (`D=0` = re-enter immediately) and
an optional **5-field cron** behind the `cron` feature (**hand-rolled 5-field UTC
parser, zero-dep**, a deliberate deviation from the original `croner` choice for
the minimalism moat; assessment §2.2). Both are implemented as **internal time
events fed into the same reactive router** ("a clock is just another event
source") — no second scheduling subsystem. **Default TZ = UTC; no
calendars/DST/job-store/catch-up in core** (assessment §2.6). A missed tick
while down is *not* caught up internally; the external-CronJob path or
idempotent re-trigger covers it.

**v2 entry point.** `triggers/timer.rs`. Any richer calendar work would be a
larger crate (a real scheduler) and must justify itself against the
external-CronJob default before entering even a feature gate.

### D11 — A third+ in-binary intelligence adapter

**WHAT.** A third (or further) LLM provider dialect compiled into the binary
(e.g. a native Gemini adapter), beyond the two v1 ships.

**WHY deferred.** "Fewer adapters, thinner binary, push provider quirks to the
gateway" (assessment §2.4). Each in-binary adapter is maintenance surface and
binary weight; gateways already normalize most providers to the
OpenAI-compatible shape.

**v1 FALLBACK** (assessment §2.4; RFC 0006). Exactly **two** in-binary
adapters: **`openai-compatible`** (canonical: `/chat/completions` + native
tool-calling — covers vLLM/Ollama/LM-Studio/most hosted gateways) and
**`anthropic`**. A model lacking native tool-calling falls back to the
JSON-action `{"action":"tool"|"final"}` shape via `extract_json_object`
(balanced-brace, prose-tolerant). **Other providers live behind the gateway,
not in the binary** — point `AGENTD_INTELLIGENCE` at a gateway that exposes the
OpenAI-compatible shape.

**v2 entry point.** `intel/` — a new `intel/<provider>.rs` adapter behind the
two-adapter bias. The bar for adding one: a provider that a gateway cannot
adequately normalize and that enough deployments need to justify the binary
weight. The internal `Request`/`Response`/`Usage` types (RFC 0006) are the
extension point.

---

## 4. Interactions with other RFCs

- **RFC 0001 (core).** This RFC is the explicit non-goals appendix to the
  core thesis; the "minimalism is the moat" principle is what justifies every
  deferral here.
- **RFC 0004 (MCP client subset & wire codec).** Owns the v1 client fallbacks:
  request/response + progress + cancel instead of `tasks` (D1); declare no
  client capabilities and reject/empty-answer sampling/roots/elicitation
  (D2/D3/D4); never call completion/prompts (D5/D6); stdio-only reactivity
  (D7a). The capability map in `wire/mcp.rs` is where each deferred capability
  is *not* declared.
- **RFC 0005 (self-MCP server & control protocol).** Owns: stdio/unix-only
  serving instead of HTTP (D7b); self-resources / `agentd://` state instead of
  emitting `message`/`progress` (D9); async-subagent-as-subscribable-resource
  instead of `tasks` server-side (D1). The internal control protocol stays a
  minimal JSON-RPC sibling — risk #10 — and is never leaked outward as `tasks`.
- **RFC 0006 (intelligence transport & wire format).** Owns the two-adapter
  decision (D11) and the LLM-direct loop that makes servicing sampling
  unnecessary (D2).
- **RFC 0007 (agentic loop).** The loop owns the LLM directly, which is *why*
  D2 (sampling) is unnecessary; elicitation-dependent tools (D4) surface as
  tool-domain errors per the loop's error taxonomy.
- **RFC 0008 (execution modes, triggers & reactive routing).** Owns
  stdio-subscription reactivity (D7a fallback) and the internal interval/cron
  event sources (D10). Async-subagent completion routed as a resource update
  (D1 fallback) flows through its router.
- **RFC 0003 (process supervision & recovery).** Owns rebuild+reconcile and
  the retained spawn-payload recoverable unit — the v1 alternative to
  checkpointing (D8).
- **RFC 0011 (cloud-native contract).** Owns idempotency (stable `RUN_ID` into
  `_meta`, "already done" → exit 0) — the mechanism that makes the D8 fallback
  (idempotent re-trigger after a stateless restart) correct.
- **RFC 0012 (security posture).** Owns the self-MCP-over-HTTP hardening
  requirements (D7b) that v2 MUST satisfy before HTTP serving is exposed, and
  the trust-budget treatment any future sampling-servicing (D2) inherits.

---

## 5. Non-goals / Deferred

This entire RFC is the deferred list. Restated as hard non-goals for v1:

- **No** MCP `tasks/*` in either direction (D1).
- **No** `sampling/createMessage` in either direction; **no client
  capabilities declared at all** (D2).
- **No** `roots/*` declared (empty-answer only), **no** `elicitation/*`,
  **no** `completion/*`, **no** `prompts/*` (D3–D6).
- **No** reactivity over HTTP and **no** self-MCP HTTP serving — stdio + unix
  only (D7). **Never** the deprecated 2024-11-05 HTTP+SSE transport, in any
  version.
- **No** durable checkpoint — stateless rebuild+reconcile only (D8).
- **No** server-emitted `message`/`progress` notifications (D9).
- **No** calendar/DST/job-store/missed-tick scheduler — UTC 5-field cron +
  external CronJob only (D10).
- **No** third in-binary intelligence adapter — two adapters + gateway (D11).

The recommended v2 ordering (not binding, for planning): D7b + D8 unlock
durable multi-node reactive serving; D1 + D2 unlock agent-to-agent
intelligence-sharing; D6/D9 are cheap add-when-needed; D3/D4/D5/D10/D11 are
opportunistic.

## 6. Open items

None. Every item here is a settled deferral with a named v1 fallback and a v2
entry point; this RFC resolves the open questions rather than raising new ones.
The one upstream uncertainty noted in the source material — whether `RUN_ID`
propagation into MCP `_meta` needs an MCP extension (notes §12) — is in-spec
(`tools/call` carries `_meta`) and owned by RFC 0011, not deferred here.
