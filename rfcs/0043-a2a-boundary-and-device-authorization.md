# RFC 0043: The A2A boundary, extensions and the device authorization grant

**Status:** Accepted — being implemented for v1.17.0
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-09-27
**Supersedes:** RFC 0032 §3 (discovery), §4 (the feed), §5 (the taskless reads) and §12–§16 (daemon-driven chrome, pairing, `config.set`, composer affordances, human-in-the-loop availability). RFC 0032's `interface` configuration section (§2), its authorization additions (§6), its browser rule (§7), its client description (§8) and its passthrough (§9) are replaced here as well; RFC 0032 carries a dated note at each place.
**Amends:** RFC 0029 (principals, command DataParts, the Agent Card); RFC 0042 §2 (a claim corrected; see §17.3).
**Standards basis:** A2A 1.0 (specification v1.0.1: §3.3.2, §3.3.4, §3.6, §4.6, §5.8, §9, §11.7, §13.3); JSON-RPC 2.0; RFC 6749, RFC 7009, RFC 8414 and RFC 8628 (OAuth 2.0: the framework, revocation, server metadata and the device authorization grant); RFC 8615 (well-known URIs); the WHATWG Fetch standard (CORS, Private Network Access).

---

## 1. Summary

agentd's A2A listener was two things at once: an implementation of the A2A
specification (through the official Rust library, `a2a-rs`), and around it a
private dialect that had grown one convenience at a time — an anonymous `Pair`
method that issued credentials, a `GetAgentCard` RPC, 0.3-era method aliases,
an `a2a.` prefix strip, a UI section that configured listener authentication,
and error codes that told a caller with a bad credential that push
notifications were unsupported. An audit against A2A 1.0.1 found 66 defects
(Appendix B). Most of them are one defect: **a second vocabulary beside the
spec's, with every check written against one of the two.**

This RFC puts the boundary back where the spec draws it. After v1.17.0 agentd
serves only:

- the 11 A2A 1.0 JSON-RPC methods;
- `GET /.well-known/agent-card.json`, public and readable cross-origin;
- one **method** extension — `agentd.events/SubscribeToEvents` under
  `https://agentd.dev/a2a/ext/events/v1`;
- two **profile** extensions — `https://agentd.dev/a2a/ext/command/v2` and
  `https://agentd.dev/a2a/ext/task-annotations/v1`;
- one declared **custom binding** URI for the unix socket,
  `https://agentd.dev/a2a/binding/jsonrpc-unix/v1`;
- an RFC 8628 / 6749 / 7009 / 8414 **authorization server** on the listener
  origin, declared on the card as an `oauth2` scheme with a `deviceCode` flow.

Nothing else is answered. Every removed method, route, configuration key, op,
launcher flag and client flag is refused **by name**, from one table per kind,
with a pointer to its replacement.

The display clients (`agentd-tui`, `agentd-ui`) become ordinary A2A clients.
They discover the card and send `A2A-Version`. They activate only the
extensions the card declares, and fall back to a standards-only mode when it
declares none. They sign in through the device grant, where the operator
approves a **name** and the session acts as `user:<name>`. When they are started
by `agentd tui` / `agentd ui` they sign in instead with a single-use launch code
minted inside the daemon's process. **No long-lived credential ever reaches a
client, and a browser always authenticates.**

## 2. Motivation

The defects fall into six groups. Each is listed with its audit identifier;
Appendix B maps every identifier to the section that closes it.

**A private dialect on a public endpoint.** `Pair` (alias `interface.pair`) was
an undeclared, anonymous JSON-RPC method that minted credentials (F10).
`GetAgentCard`, `agent/card` and any `a2a.`-prefixed name served the card
through a non-spec RPC, which was the clients' only discovery call (F11, F27).
The 0.2-era `/.well-known/agent.json` was still routed (F50). An unknown method
got `-32003 PushNotificationNotSupported` rather than `-32601` (F26). A missing
or bad credential got HTTP 200 with `-32003` or `-32007`, never a 401 or 403
challenge (F24). `A2A-Version` was never read, and neither agentd's own peer
client nor the display clients sent it (F16). The card said
`protocolVersion: "0.3.0"` for a 1.0 wire (F15), and advertised
`extendedAgentCard: true` with no `securitySchemes`, which §13.3 does not
permit (F17). Malformed envelopes got `-32700` or `-32003` instead of `-32600`
(F54).

**Escalations.** A certificate that matched no principal rule on a listener
with both `client_ca` and `a2a.bearer` resolved to operator without the bearer
being checked (F01). Ordinary grant patterns reached the "operator-only" ops, so
a user with `grants: ["*"]` could read the pairing code and mint an operator
session (F02). Any named caller could join another principal's conversation by
naming its `contextId`, including the root one (F03). The command ops acted on
any run or subagent by id, without the ownership check core `GetTask` enforces
(F04). The `status` op returned every principal's runs, outputs and
conversations to every named caller (F05). The extended card advertised
per-workflow authorization that nothing enforced (F25). `agentd ui` handed the
operator's root bearer to `agentd-ui`, which served it unauthenticated as a
cross-origin-includable `/config.js` (F06). The web UI sent its bearer to
whatever `?endpoint=` named and kept credentials past disconnect (F07). Pairing
was an always-armed six-digit code behind one global lockout (F08).

**Decorative negotiation.** Extension activation had no effect: command
DataParts and the feed ran without `A2A-Extensions` (F12), and the echo
reported extensions the instance did not declare (F29). The command extension
answered `SendMessage` with results that were neither a Task nor a Message
(F13). Display-only ops sat under the generic command extension and were
published as skills (F14). Workflow-declared commands and the internal
`_instance.*` protocol were served but declared nowhere (F33). The extension
URIs returned 404 and nothing specified what the extensions carry (F34). Clients
depended on undeclared `agentd/*` task metadata (F35).

**Generic capabilities behind a UI switch.** Human-in-the-loop gates — a core
A2A `input-required` flow — worked only with `interface.enabled` (F19).
Listener authentication (pairing) and browser CORS were configured under the UI
section and invisible to discovery (F39). The daemon owned the clients' chrome
layout (F40). Operator introspection, the log ring and runtime `agent.approval`
were gated on UI flags (F43). The daemon CLI embedded the Node clients' binary
names, flags, environment contract and npm package, and forced UI configuration
(F57). The audit path hard-coded exclusions for one UI's polling (F56).

**Errors lost on one of two paths.** A runtime refusal took one of two routes:
answered by the listener, or passed through `a2a-rs`, which rewrites error data
and always answers HTTP 200. Draining was `-32000` on one path and `-32603` on
the other, and a rate-limit refusal whose error key the library's ports did not
recognise came back as an empty Task — a success (F37). Streams broke §3.1.2 and
§9.4.6: command streams closed while the task was WORKING, errors were framed as
SSE on HTTP 200, and `SubscribeToTask` on a terminal task opened a stream (F38).

**Clients that spoke agentd, not A2A.** No card discovery (F22). Extensions used
without reading the card's declarations (F23). Messages without the REQUIRED
`role` (F21). The 0.3 `configuration.blocking` field (F20). An SSE parser that
got no frames from CRLF servers such as the official Python SDK (F47). Dropped
error information and endless retries on auth failures (F48). A Bearer-only
login through the non-spec `Pair` (F49). No standards-only mode (C9).

None of these was a decision. Each was a place where one vocabulary's rule did
not reach the other's code path. That is the same shape as the defect classes
v1.4.0 ("reported success, changed nothing") and v1.16.0 (the document
boundary) closed, and the remedy is the same: **one vocabulary, one source per
list, and a refusal wherever the old vocabulary is spoken.**

## 3. Goal and rules (normative)

**Goal:** a caller that knows only A2A 1.0 can discover, authenticate to and
use agentd, and a caller that knows agentd's extensions can use them only by
declaring so. Nothing the listener answers is outside the card.

1. **The boundary is the spec's.** The listener answers the 11 core methods, the
   card route, the methods of declared extensions and the declared
   authorization server — nothing else (§4).
2. **Removed means refused by name.** Every removed name is refused with its
   replacement, from one table per kind (`REMOVED_KEYS`, `REMOVED_OPS`,
   `REMOVED_LAUNCHER_FLAGS`, the removed-methods list). There are no aliases,
   no prefix strips and no deprecation period.
3. **The official libraries own the wire; the transport stays agentd's.**
   `a2a-rs` 0.10 serves the core methods and the extended card. agentd owns the
   listener in front of it — TLS, admission, CORS, the pipeline of §5 — and
   never vendor-patches the library (§6).
4. **A scheme is declared iff a verifier backs it**, and every credential the
   listener accepts arrives through a declared scheme (§8.4).
5. **Nothing declared is decorative.** Declared extensions are enforced;
   agentd's DataParts and methods work only when the caller activates the
   extension that defines them (§9).
6. **One source per list.** The ops (`OPS`), the event kinds (`FeedKind`), the
   error codes (`crate::a2a::errors`), the posture (`ListenerAuth`) and the
   launcher contract (`LAUNCH_CONTRACT`) each live in one table from which
   every consumer — dispatch, the card, the manifest, the schema bundle, the
   docs guard — is derived.
7. **A display client is an ordinary A2A client** and never holds a
   long-lived credential (§12, §15).
8. **A browser always authenticates.** A request carrying `Origin` is never the
   implicit operator (§7.2).

## 4. The surface

### 4.1 The core method table

The **Core method table (surface::SpecMethod, Route, route_of, methods_of)** is
the one list of what the listener answers. `SpecMethod` has exactly the 11 core
methods (`SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`,
`CancelTask`, `SubscribeToTask`, the four push-configuration methods and
`GetExtendedAgentCard`), with `const ALL` and an exhaustive `name()`. A route is
`Spec(SpecMethod)` or `Extension { ext_method }`.

`route_of(&str)` takes no settings. It matches the core names and the extension
method names **exactly** — no prefix strip, no alias. Whether an extension
method is available is decided by negotiation alone (§9.1, pipeline step 9), so
there is exactly one place where availability can differ from existence.
`methods_of(&Settings)` is the core names plus the methods of declared
extensions. A test under the `a2a` feature holds `SpecMethod::ALL` equal to
`a2a-rs`'s own method constants, named one by one.

The old `METHODS` and `LOCAL_METHODS` lists are deleted.

### 4.2 Removed JSON-RPC methods and HTTP routes

Each of these gets `-32601` for every caller that reaches pipeline step 8,
before authorization and with no side effect:

- `Pair` and `interface.pair` (their handlers are deleted);
- `GetAgentCard`, `agent/card`, `agent/getAuthenticatedExtendedCard`;
- any `a2a.`-prefixed name;
- the bare `SubscribeToEvents` (now `agentd.events/SubscribeToEvents`);
- 0.3 names (`message/send`, `tasks/get`, `SetTaskPushNotificationConfig`, …).

Removed routes: `GET /.well-known/agent.json` → 404; `agentd-ui`'s
`GET /config.js` → 404.

Answering these `-32601` before authorization is deliberate: a removed method
has no authorization rule to consult, and every caller that got that far gets
the same answer whatever its role, so the answer says nothing about the
caller's permissions. (An unauthenticated caller never gets that far: step 3
answers it 401 first.)

### 4.3 Constants and internal verbs

**surface::A2A_PROTOCOL_VERSION and extension constants** are always compiled:
`A2A_PROTOCOL_VERSION = "1.0"`; `EVENTS_EXTENSION`, `EVENTS_METHOD`,
`TASK_ANNOTATIONS_EXTENSION`, `UNIX_BINDING` and `COMMAND_EXTENSION` (now
`…/command/v2`). The v1 interface extension URI is deleted and never declared
or echoed.

The **Internal bridge verbs** between the listener and the runtime are not wire
methods: `PublicCard` replaces the old `GetAgentCard` verb; the `NewTaskId` verb
and the runtime's `bare()` name normaliser are deleted; `SendMessage` takes
`{message, taskId, newTask, push?}`; `ListTasks` and the push verbs take the
canonical parameter shapes.

### 4.4 The capabilities manifest

The **--capabilities manifest a2a section** is built by
`surface::manifest::a2a_section(&Settings)` from the same sources as the
listener: `methods = methods_of`, `command_ops = command_ops_of`, the declared
extension URIs, `url = configured_url`, `auth = {bearer, mtls, device,
required, implicit_operator}` from `ListenerAuth`, the `events` and
`introspection` switches, and a *count* of `a2a.cors.origins`. There is no
`interface` key, and `Pair` and `GetAgentCard` never appear. A launched UI
origin (§11.2) is not configuration and is not counted.

## 5. The request pipeline

### 5.1 POST / request pipeline (order and refusal at each step)

Every `POST /` passes these steps in order. Each refusal is final and is plain
`application/json`; **no SSE response is ever written for a refusal.**

| Step | Check | Refusal |
|---|---|---|
| 0 | There is no admission before resolution. The auth-failure limiter acts only inside step 3, on a request that would fail anyway (§7.5). | — |
| 1 | `Origin` gate against `a2a.cors.origins` (exact parsed match; no `Origin` passes; `Origin: null` never matches). | 403, empty body, no ACAO. Never counted by the limiter. |
| 2 | `Content-Type` is `application/json` (parameters allowed). | 415, empty body. |
| 3 | Authentication, from headers only, before any body byte is parsed, against one `Arc<Resolver>` snapshot (§7). | 401 `-31401`; 403 `-31403` for no role; 429 once a source's *counted* failures exceed the limit. |
| 4 | Body read (existing cap) and JSON parse. | `-32700`, id null. |
| 5 | Envelope: an object (an array is `-32600` "batch requests are not supported"); `jsonrpc` exactly `"2.0"`; a non-empty string `method`; an `id` that is a string or an i64 (a notification or a null id is `-32600` "A2A requests must carry an id"); `params`, when present, an object. | `-32600` (id null) or `-32602` (id echoed). |
| 6 | `A2A-Version` (§5.2). | `-32009`. |
| 7 | Per-principal rate admission; operators are exempt. | 429. |
| 8 | `route_of(method)`, exact. | `-32601`, for every caller. |
| 9 | Extension negotiation (§9.1). | `-32008` for a missing required extension; `-32601` with `EXTENSION_NOT_ACTIVATED` / `EXTENSION_NOT_DECLARED` for an extension method. |
| 10 | Method authorization, `Principal::may(route)`. For `GetExtendedAgentCard`, the §13.3 credential gate (§8.5). | 403 `-31403`; 401 for the card gate. |
| 11 | `SendMessage` / `SendStreamingMessage`: params parsed as `a2a-rs`'s `SendMessageRequest` (unknown fields ignored) with `role` `ROLE_USER`; the command envelope (§9.2); `Principal::authorize_command`. `SubscribeToTask`: a `GetTask` visibility pre-check as the caller. | `-32602`, `-32005`, 403, or the pre-check's `-32001`. |
| 12 | Dispatch. Message-reply ops are answered by the listener; everything else goes to `a2a-rs` inside a `RequestScope`, and the reply passes the error-fidelity filter (§6). | — |
| 13 | Runtime checks: ownership (`-32001`), task-id rules, draining. | As §5.3. |

JSON-RPC errors use HTTP 200 unless a status is named above or produced by
`errors::http_status_of`.

Two orderings carry the design. Authentication (3) runs **before the body is
parsed** (4), so an unauthenticated caller costs a header read, not a JSON
parse of a body up to the cap. And `route_of` (8) runs **after** version and
rate admission but **before** authorization (10), so an unknown or removed
method answers the same `-32601` to every caller that got that far.

### 5.2 A2A-Version request header

Required on every `POST /`. The value is trimmed and split on `.`; it is
accepted iff major is 1 and minor is 0, and a patch component is ignored
(§3.6: patch numbers "MUST not be considered" in negotiation). A missing, empty
or other value gets `-32009` at HTTP 200 with the id echoed and
`ErrorInfo{reason: VERSION_NOT_SUPPORTED, domain: a2a-protocol.org,
metadata: {supportedVersions: "1.0"}}`.

The header is read from the header only. §9.2 requires the JSON-RPC binding to
carry service parameters as HTTP headers; the query-parameter form in §3.6.1 is
illustrated for GET requests of the HTTP+JSON binding, which agentd does not
serve. An empty value is 0.3 by §3.6.2, which agentd does not speak — so a client
built on `a2a-rs` up to 0.10, which sends no header, is refused with `-32009`.
That is spec-exact, and `docs/a2a.md` and the CHANGELOG say so.

The header is not required on the card route, `OPTIONS /`, `/oauth2/*` or the
RFC 8414 metadata. agentd's outbound client and both display clients send it on
every JSON-RPC request.

### 5.3 The error table

The **Shared error table crate::a2a::errors** is always compiled and is the one
home of every code agentd emits:

| Code | Name | Used for |
|---|---|---|
| `-32700` | `PARSE_ERROR` | Body is not JSON |
| `-32600` | `INVALID_REQUEST` | Envelope (step 5) |
| `-32601` | `METHOD_NOT_FOUND` | Unknown, removed or unavailable method |
| `-32602` | `INVALID_PARAMS` | Params, command envelope, schema |
| `-32603` | `INTERNAL_ERROR` | Also rate limit (`RATE_LIMITED`) and draining (`DRAINING`) |
| `-32001` | `TASK_NOT_FOUND` | Unknown or not-owned object |
| `-32002` | `TASK_NOT_CANCELABLE` | Cancel of a terminal task |
| `-32003` | `PUSH_NOTIFICATION_NOT_SUPPORTED` | Push disabled — only there |
| `-32004` | `UNSUPPORTED_OPERATION` | §3.3.4 capability refusals; message to a terminal task |
| `-32005` | `CONTENT_TYPE_NOT_SUPPORTED` | Raw or url parts; output modes without JSON |
| `-32007` | `EXTENDED_AGENT_CARD_NOT_CONFIGURED` | Never emitted (§8.5) |
| `-32008` | `EXTENSION_SUPPORT_REQUIRED` | A required extension not activated |
| `-32009` | `VERSION_NOT_SUPPORTED` | `A2A-Version` |
| `-31401` | `UNAUTHENTICATED` | HTTP 401 |
| `-31403` | `PERMISSION_DENIED` | HTTP 403 |

The authentication codes sit outside the reserved range (-32768..-32000)
because §3.3.2 leaves authentication to "HTTP `401` … JSON-RPC custom error":
the status carries the meaning, the code is agentd's. Every error carries
`google.rpc` detail objects (`ErrorInfo`, `BadRequest`). `ErrorInfo.domain` is
`a2a-protocol.org` for the spec's own conditions (`VERSION_NOT_SUPPORTED`,
`EXTENSION_SUPPORT_REQUIRED`, `CONTENT_TYPE_NOT_SUPPORTED`,
`UNSUPPORTED_OPERATION`) and `agentd.dev` for every other reason. The reason
constants live beside the codes: `RATE_LIMITED`, `DRAINING`,
`EXTENSION_NOT_ACTIVATED`, `EXTENSION_NOT_DECLARED`, `EXTENSION_NOT_MARKED`,
`COMMAND_ENVELOPE_AMBIGUOUS`, `COMMAND_TASK_ID`, `UNKNOWN_OP`,
`INVALID_COMMAND_ARGS`, `INTROSPECTION_DISABLED`, `UNAUTHENTICATED`,
`PERMISSION_DENIED`, `VERSION_NOT_SUPPORTED`, `EXTENSION_SUPPORT_REQUIRED` and
`CONTENT_TYPE_NOT_SUPPORTED`.

A source-scan guard holds two codes to one emitter each: `-32003` (or its name)
appears only in the push-enabled check, and `-32007` only in the table. That is
the guard against the v1.16 habit of reaching for `-32003` as a generic refusal.

The individual refusals:

- **HTTP 401 authentication challenge (-31401).** Sent when resolution gives
  `Unauthenticated`, and for `GetExtendedAgentCard` without a declared-scheme
  credential. `WWW-Authenticate: Bearer realm="agentd"`, plus
  `error="invalid_token"` with a description when a bearer (or an `agentd_at_`
  session token) was presented. The message is "authentication required",
  "invalid or expired credential", or — when the listener is implicit-operator
  and the request was refused only because it carried `Origin` — "browser
  requests must authenticate: sign in with the device grant, or open the UI
  with `agentd ui`". The card route and `/oauth2/*` are never challenged.
  Audited as `a2a.denied`.
- **HTTP 403 permission denied (-31403).** For no role, a failed `may(route)`
  or a failed `authorize_command`, whether the listener or a runtime belt check
  refused. The message names the method or op and the principal. With a bearer
  credential the response adds `error="insufficient_scope"`. Never an SSE
  frame.
- **Object-level denial = -32001.** An unknown object and an object the
  non-operator caller does not own get the same `-32001` with a fixed message
  per type ("task not found", "no such run", "no such subagent", "no such
  conversation"). The ownership check runs before any lookup whose outcome
  could differ, so the answer never confirms that something exists. An object
  with no principal is operator-only.
- **Per-principal rate refusal.** One token per inbound HTTP request, after
  authentication, from buckets keyed by (principal id, rate spec). HTTP 429,
  `Retry-After`, `-32603` with `RATE_LIMITED` and `retryAfterSeconds`.
  Operators are exempt. The reactor's own rate check and its `-32029` are
  deleted: admission is the listener's.
- **Draining refusal.** While draining, `SendMessage` and
  `SendStreamingMessage` get `-32603` "the agent is draining" with `DRAINING`,
  identical on both paths (§6); a streaming send gets `application/json`.
  `-32000` is gone.
- **CancelTask on a terminal task** gets `-32002` "task is already
  TASK_STATE_<X>", and the task is unchanged.
- **SubscribeToTask visibility and terminal behaviour.** An unknown or
  invisible task gets `-32001` as plain JSON from the listener's pre-check
  (needed because `a2a-rs` 0.10 treats a missing task as "no snapshot" and opens
  a stream anyway). A terminal task without `Last-Event-ID` gets `-32004`
  (`a2a-rs` native). With `Last-Event-ID` the stored events are replayed; that
  is SSE-level reconnection of a stream the caller already held, documented as
  such and not as a protocol extension.

### 5.4 Sending

**SendMessage task-id rules** (§3.4.2: task ids are server-generated):

- an absent or empty `taskId` creates a task with an `a2a-rs` UUIDv4 id;
- a present `taskId` that is missing or not visible → `-32001`, no task created;
  a terminal one → `-32004` "task <id> is <STATE>; it accepts no further
  messages"; otherwise the message continues the task;
- a command DataPart together with a `taskId` → `-32602`;
- `role` other than `ROLE_USER` → `-32602`;
- a `contextId` that differs from the task's → `-32602`;
- unknown params fields are ignored (ProtoJSON), pinned by a test.

The send's intent (`named_task`, `in_send`, `pending_push`) travels in the
request scope (§6.2), so the old `NewTaskId` round trip and the body rewrite are
deleted. An inline push configuration on a new task is registered when the task
is created, not before it exists.

**Message parts / -32005.** Text parts are accepted. A DataPart without the
`agentd` key is rendered to the model as a fenced JSON block. A raw or url part
anywhere refuses the whole message with `-32005` naming the media type and the
accepted ones (`text/plain`, `application/json`).

**SendStreamingMessage stream shapes.** A Message-reply op gets exactly one
frame `{jsonrpc, id: <request id>, result: {message}}` and then close, with no
SSE id. Everything else is framed by `a2a-rs` — a `{task}` frame, then status
and artifact updates, closing at a terminal or interrupted state (§11.7) — and
passes the error-fidelity filter. An error before the first frame is plain
JSON. The hard-coded `id: 1` and SSE-framed refusals are deleted.

## 6. Error fidelity through a2a-rs

### 6.1 The problem

`a2a-rs` 0.10 turns any `JsonRpc` error its ports return into its own shape:
the message is rewritten, `data` becomes
`{reason: JSON_RPC_ERROR, domain: a2a-rs}`, and the answer is always HTTP 200.
So a refusal raised by the runtime reached the caller in one form when the
listener answered it and in another when `a2a-rs` did: draining got two codes,
and a rate-limit refusal the ports did not recognise as an error was
deserialised into an empty Task and returned as a success (F37).

Two fixes were available. Patching the library would make agentd the owner of
a fork of the wire it promised to leave to the official implementation.
Instead the original error travels **beside** the library, out of band.

### 6.2 Error fidelity through a2a-rs

The contract:

- **ports::RequestScope** carries `caller: Principal`, `via: Via`,
  `error: Arc<Mutex<Option<Value>>>`, the send intent (`named_task`, `in_send`,
  `pending_push`) and the activated extension set (`active`). The listener
  enters it with `with_request(scope, fut)` around every `a2a-rs` call.
- When the runtime ports (`call`, `get`, `list`, `cancel`, `process_message`,
  the push verbs, the extended-card source) receive a bridge reply carrying an
  error, they store that JSON-RPC error object verbatim — first write wins —
  and return an `Err` to `a2a-rs`, which uses it only for its own internal
  decisions.
- The listener filters every `a2a-rs` response:
  - a unary JSON error has its `error` replaced byte-for-byte by the stored
    object when there is one, and its HTTP status set by `http_status_of`
    (401 with the challenge, 403 with `insufficient_scope` when a bearer was
    used);
  - an error `a2a-rs` raised itself keeps its body, except that the
    out-of-range `-32100`, `-32101` and `-32102` become `-32603` with the
    message kept;
  - an SSE response is inspected at its first event (cap 8 MiB, the request
    timeout): an error becomes a plain JSON response by the unary rule;
    anything else is re-emitted unchanged, SSE id kept, followed by the rest of
    the body; later in-stream error frames get the same substitution.

So a runtime refusal has the same code, message, data and HTTP status on both
paths, and `a2a-rs`'s own errors keep `a2a-rs`'s details.

### 6.3 Session liveness hook (stream revocation)

A revoked session must stop acting at once, including through a stream it
already holds. The listener owns the `Response`, so it owns the hook:
`App.liveness` yields, for a caller, an optional check `Fn() -> bool`. When it
does:

- `a2a-rs` SSE bodies (`SubscribeToTask`, `SendStreamingMessage`) are wrapped to
  end within one 100 ms tick of the check turning false;
- unary `a2a-rs` calls race the same tick, and answer 401 "the session token is
  unknown, expired or revoked" if the session dies mid-wait;
- the events stream ends with `goodbye{reason: "revoked"}`.

The check is per session id, not per principal id: revoking one session ends
that session's streams only, even when sibling sessions share its principal
`user:<name>` (§10.3).

## 7. Identity

### 7.1 Identity resolution (Evidence, Resolution, Via, SessionVerifier)

Resolution is one always-compiled function over what the request presents:

```text
Evidence   { bearer?, cert?: CertId{subject, sans}, local: bool, origin: bool }
Via        Bearer | Session | Cert | AnyRule | Unix | Implicit
Resolution Named(Principal, Via) | Unauthenticated{presented: bool} | NoRole
```

`origin` is true when the request carried an `Origin` header of **any** value,
`Origin: null` included — `file://` pages, sandboxed iframes and `data:` URLs
send it — i.e. when a browser sent it. `local` is a loopback peer address.

The steps, in order:

0. A unix-socket peer (same uid, enforced at accept) → `Named(operator, Unix)`.
1. A presented bearer:
   - an `agentd_at_` token goes **only** to the session store (every TCP
     listener has one): valid → `Named(p, Session)` with the session id on
     `Principal.session`; otherwise `Unauthenticated{presented: true}`;
   - otherwise, when the listener has a bearer mechanism: the server bearer
     (constant time) → operator; else the `bearer_ref` rules in declared order;
     else `Unauthenticated{presented: true}`;
   - a bearer presented to a listener with no bearer mechanism at all (no
     `a2a.bearer`, no `bearer_ref` rule, no device grant) is ignored, and
     resolution goes on to the certificate or the posture.
2. A verified certificate: `san` / `sub` / `any` rules in order → `Named(rule,
   Cert)`; with no rules configured → operator; else `NoRole`. v1.16 folded
   "a certificate verified" and "the bearer matched" into one flag, so on a
   listener with `client_ca`, `a2a.bearer` and rules, any CA-issued certificate
   that matched no rule became operator without the bearer (F01). Now a
   presented bearer is judged as a bearer in step 1, and a certificate alone is
   operator only while no principal rule is configured.
3. An `any` rule → `Named(rule, AnyRule)`.
4. `local` AND the posture's `implicit_operator` AND NOT `origin` →
   `Named(operator, Implicit)`.
5. Otherwise `Unauthenticated{presented: false}`.

An anonymous-role rule counts as `Unauthenticated` in steps 1 and 3 and as
`NoRole` in step 2. `Resolver::build` refuses a `bearer_ref` secret equal to
`a2a.bearer`, equal to another rule's, or starting with `agentd_at_` — a secret
two rules share would make resolution order-dependent, and the prefix belongs
to the session store.

`Via` exists because a principal alone does not say *how* the caller got there,
and the §13.3 gate depends on exactly that: the extended card requires a caller
that authenticated through a declared scheme (§8.5), and an operator reached
through the implicit posture or a unix peer did not. It rides in the request
scope (§6.2), so the calls `a2a-rs` serves see it too.

### 7.2 Listener posture (surface::auth::ListenerAuth)

The posture is derived from configuration by the always-compiled
`listener_auth_of(&A2a)`; every input lives under `a2a`:

| Field | Definition |
|---|---|
| `bearer` | `a2a.bearer` OR any `bearer_ref` rule |
| `mtls` | https AND `a2a.tls.client_ca` |
| `device` | `a2a.device_grant.enabled` |
| `unix` | the listen address is a unix socket |
| `loopback_bind` | the listen host is 127.0.0.0/8, `::1` or `localhost` |
| `implicit_operator` | `unix` OR (`loopback_bind` AND no `a2a.bearer`, no principals, no `client_ca`, no device grant) |
| `any_rule` | some principal rule matches `any` |
| `required` | NOT (`implicit_operator` OR `any_rule`) |

`declares_any() = !unix && (bearer || mtls || device)`. `ListenerAuth` is the
single source for the resolver, the card's security fields and
`extendedAgentCard`, the manifest's `auth` block, and the extended-card gate.
A launcher's launch slot (§11) is not a mechanism and changes no field.

**Why the implicit operator needs a loopback bind.** "The peer address is
loopback" means "a process on this host" only if nothing on the host relays
for someone else. On a wildcard bind that promise is already broken by design —
the listener is reachable from the network — and a same-host reverse proxy
(nginx, a sidecar, an SSH forward) connects from 127.0.0.1 on behalf of anyone
it serves. So on any bind other than loopback, `local` grants nothing: a
request relayed from 127.0.0.1 to a wildcard listener is `Unauthenticated`. On
a loopback bind the operator chose "this host only", and the one remaining case
— a same-host proxy that forwards to a loopback no-auth listener — is
documented as the operator's to close by configuring any credential, which ends
the implicit operator.

**Why a request carrying Origin is never the implicit operator.** A browser is
a confused deputy: it carries code from every site the user visits. On a
no-auth loopback daemon, a listed origin's JavaScript — or an injection into
it, a malicious extension, a rebound DNS name — would act as operator with no
credential at all. With step 4 excluding `Origin`, a page acts only through a
token it was given, scoped to one tab (§12). Browser UIs therefore always
authenticate: with a session, a bearer or a certificate, or by matching an
explicit `any` rule, which can never carry the operator role. Non-browser
tools (curl, the TUI) are unaffected.

### 7.3 The posture is one value with the rules

Of the posture's inputs only `a2a.principals` is reloadable (`a2a.bearer`,
`a2a.listen`, `a2a.tls.*` and `a2a.device_grant` are restart-only). A reload
that changes the principals rebuilds the resolver, and `Resolver::build`
computes the posture itself and keeps it inside the resolver. The rules and the
posture are therefore **one value behind one `RwLock<Arc<Resolver>>`**: the
reload swaps both atomically, each request reads both from one snapshot, and
the listener keeps no other copy. A reload whose rebuild fails keeps the old
resolver, posture included.

The alternative — a posture computed at listener spawn — is the "reported
success, changed nothing" defect class again: a SIGHUP adding a `bearer_ref`
rule to a no-auth loopback daemon would log success while the daemon went on
treating every local caller as operator. With the posture inside the resolver,
that reload ends the implicit operator at once, and the card (built from the
current settings on every request) gains the scheme.

### 7.4 Principal id

- Operator role → `operator` (every operator credential, operator-scope device
  sessions and launch sessions).
- User and agent roles → `<role>:<id>` when the rule declares `id`; else, from
  the certificate, `<role>:cn=<subject CN>`, else `<role>:san=<first SAN>`.
- User-scope device sessions → `user:<name>`, the name the approver gave (§10.3).

The `cn=` / `san=` marker keeps derived ids out of the declared namespace **by
construction**: `=` is in neither the declared-id charset nor the approval-name
charset, so no certificate can spell a declared id or a device name. Without
it, a rule `{id: deploy-bot, match: {bearer_ref: …}}` beside a rule
`{match: {san: "*.corp"}}` would hand any CA-issued certificate with CN
`deploy-bot` the principal `user:deploy-bot` — and with it that caller's tasks,
conversations, rate bucket and status scope. Uniqueness among declared ids alone
cannot see that merge.

A session id (`ds_…` or `ls_…`) is **never** part of the principal id. It rides
on `Principal.session`, appears as `sid` in every audit event of that caller,
and is the revocation handle. `id` is required on `bearer_ref` and `any` rules —
a rule that names one caller needs a name its tasks are owned by — so the v1.16
collapses `user:unknown` and `user:paired` can no longer be produced. Declared
ids match `[A-Za-z0-9._@:/+-]{1,128}` and are unique across rules. Audit events
carry `rule`, the declared id of the matched rule, so operator-role rules stay
distinguishable.

### 7.5 Failure-only auth throttling

The auth-failure limiter throttles **failures, not callers.** It is per source
(an IPv4 address or an IPv6 /64; 4096 entries, oldest evicted; burst 20, refill
one per 3 s) and it acts only inside step 3:

- a request that authenticates (a bearer, a session, a certificate, a unix
  peer, an `any` rule, the implicit operator) is never refused by it;
- `Unauthenticated{presented: true}` and `NoRole` are counted, and past the
  limit a failing request gets 429 with `Retry-After` instead of its 401/403;
- a step-10 `not_permitted` refusal of a named principal is counted against its
  source, which matters only to later requests from that source that *also*
  fail authentication;
- **step-1 origin refusals are never counted.** They cost nothing and guess
  nothing, and any web page the user visits can make the browser send them — so
  counting them would let any page lock the local console out;
- **uncredentialed 401s (`presented: false`) are never counted and never
  429.** They guess nothing either, and an uncredentialed browser request lands
  here.

So a flood from a host that shares the source (NAT, the same machine) cannot
lock out a caller holding a valid credential. Behind a TLS-terminating proxy
every request has the proxy's source and the limiter acts globally;
`X-Forwarded-For` is never trusted, because trusting it would let a client pick
its own bucket.

### 7.6 Authorization

The **Principal authorization API**:

- `may(route)`: every named role may call every served core method and every
  extension method; anonymous may not.
- `may_command(op)`: an Operator-floor op answers to `role == operator`
  **before any grant is consulted**; an AnyNamed op is always allowed; a Granted
  op needs operator, a role default, a matching grant, or (for `workflow.run`)
  any `workflow.run:` grant; `_instance.*` is operator-only; an op not in the
  table is refused unless it is a workflow-declared command the grants allow.
- `may_run_workflow(name)`: scoped `workflow.run:<pattern>` grants **replace**
  the default for that family.
- `Runtime::may_run(principal, workflow)` = `may_run_workflow` AND the default
  start node's roles admit the caller. It is the single predicate for the
  command op, the model tool, signal starts, `status.workflows` and the
  extended card's workflow skills — so what the card advertises is what is
  enforced (F25).

The v1.16 escalation (F02) was a floor that covered `admin.*` only; the ops the
code and the docs called operator-only (`pairing.code`, `config.set`, `config`,
`debug.events`, `subagent.get`) fell through to grants. The floor is now a
column of the op table (§9.2.1), so every op is classified where it is defined:
`config`, `debug.events` and `admin.*` are Operator; `subagent.get` became an
owner-scoped introspection read; `pairing.code` and `config.set` are gone.

**REMOVED_OPS** refuses retired op names wherever they can appear:
`interface.info` ("read the agent card and the `status` op"), `config.set`
("use admin.set {path, value}"), `pairing.code` ("pairing was replaced by the
OAuth device authorization grant … operators approve with
auth.device.approve {user_code, as}"), `admin.lameduck` ("use admin.drain").
As a grant value or a workflow `a2a` start `command` the refusal is at load,
exit 2 ("grant `<n>` was removed in agentd 1.17.0: <hint>"); a start `command`
may not use a built-in op name or the `_instance.` prefix either. At runtime a
removed op gets `-32602 UNKNOWN_OP` naming the hint.

### 7.7 Ownership

**Ownership helpers, scoped signals and the model path.** `owned_run` and
`owned_subagent` return `-32001` on failure and guard `workflow.status`,
`workflow.cancel`, `workflow.signal`, `subagent.send`, `subagent.kill`,
`subagent.status`, `subagent.get` and `plan.get` (F04). Ownership is recorded
and compared by **principal id only**, never by credential or session id, so a
named device principal keeps its objects across re-login and token expiry.

The same checks guard the **model's** `workflow.cancel`, `workflow.signal` and
`subagent.*` tools whenever the turn acts for a non-operator principal — the
model is a deputy of the caller, and a check that only the wire path enforced
would be one prompt away from bypassed. An unindexed non-operator principal
fails closed. Signals from a non-operator wake only its own runs, fire starts
only where `may_run` holds (the runs belong to the sender), and never trigger
`lifecycle.until_signal`.

An instance-tier child spawn is refused when the parent's TCP listener requires
credentials (read from the current settings, so a principals reload counts) and
the child's peer entry carries none — the child could not authenticate to its
parent, and failing at spawn is clearer than a child that 401s forever.

**contextId namespace** (F03). Wire `contextId`s are per principal:
`ConversationIndex` maps `(principal id, wire id)` to an internal key
`ctx-<32 hex>` minted on first use (a missing `contextId` gets an `a2a-rs`
UUIDv4 wire id bound the same way). A non-operator never joins, reads or charges
another principal's context or the root one; `Task.contextId` on the wire is
always the owner's wire id. Operators use internal keys directly. The index is
rebuilt at restore. An operator's `ListTasks` context filter matches the
internal key or the wire id; a non-operator's matches its own wire id only.

**status op document** (F05). Operators get the full status. Non-operators get
the instance facts (`instance`, `uptime_ms`, `draining`, `paused`, `model`,
`version`, `skills`, `skill_prefix`, `values`), the workflows `may_run` admits,
their own runs and conversations, and activity for tasks they own. The document
is built from tagged items by `status_value_for(&Principal)`.

**Feed visibility.** `FeedVis::admits(principal id, is_operator)` is the only
visibility function, used by the feed and by `status_value_for`; a
`*.removed` event inherits the visibility of the item that left.

**Audit rules.** An audit event is mirrored to the feed unless it is a
successful read (`READ_METHODS` or a Message-reply op) — the v1.16 exclusions
written for one UI's polling (F56) are deleted. Every audit of a session caller
carries `sid`. New log events: `a2a.denied`, `auth.session.revoked`,
`auth.launch.exchanged`, `auth.launch.requested`, `auth.launch.approved` (no
code of any kind is ever logged), `identity.collision`,
`lifecycle.until_signal.refused`, `instance.spawn.refused`,
`turn.refused.not_owner`.

## 8. Discovery and the card

### 8.1 GET /.well-known/agent-card.json

Unauthenticated `GET` or `HEAD`, with no version gate, served from the bridge
verb `PublicCard` with a 5 s timeout:

- 200 with the SDK-canonical card, `ETag: "<first 32 hex of sha256(body)>"`,
  `Cache-Control: public, max-age=60` and the card's CORS headers;
- a matching `If-None-Match` (weak comparison, `*`, or a list) → 304;
- a bridge failure, a timeout or a non-card → 503, `Retry-After: 5`,
  `Cache-Control: no-store`, `application/problem+json` — never a 200 with
  `null` (F31).

`/.well-known/agent.json` is 404.

### 8.2 CORS policy

`POST /`, `OPTIONS /` and every `/oauth2/*` route: `Origin` is parsed into
(scheme, lowercase host with brackets stripped, port with the default applied)
and matched exactly against `a2a.cors.origins`. `Origin: null` or an unparsable
value never matches. There is **no implicit loopback trust and no loopback
opt-in**, and `*` is refused at load. The one exception is the origin of the UI
that `agentd ui` launched, added for that process only (§11.2). CORS admission
never implies trust: an admitted browser request still authenticates (§7.2).

An allowed origin gets `Access-Control-Allow-Origin: <origin>`, `Vary: Origin`
and `Access-Control-Expose-Headers: a2a-extensions, retry-after,
www-authenticate` on every actual response, 401/403/429 included, so a page can
read the challenge and the echo (F30). The preflight answers 204 with
`POST, GET, OPTIONS`, the request headers `content-type, authorization,
last-event-id, a2a-extensions, a2a-version`, `Max-Age: 600`, and
`Access-Control-Allow-Private-Network: true` when requested. A refused origin
gets 403 with no ACAO; `Vary: Origin` is still sent.

The card route is a credential-free public document: every origin gets
`Access-Control-Allow-Origin: *` and no credentials header, and the preflight
answers `GET, HEAD, OPTIONS`. Private Network Access is granted only to listed
origins.

### 8.3 Public Agent Card

- `name` = `agent.name`, else `agentd` (F53); `description` = `agent.description`,
  else the default; `version` = the build version.
- `supportedInterfaces`: on TCP, `[{url: <advertised URL>, protocolBinding:
  "JSONRPC", protocolVersion: "1.0"}]` (F15, F18); on a unix socket,
  `[{url: "unix://<path>", protocolBinding: UNIX_BINDING, protocolVersion:
  "1.0"}]` — a unix socket is never labelled JSONRPC.
- `defaultInputModes` / `defaultOutputModes` = `text/plain`, `application/json`.
- `skills` = exactly one: `conversation` ("Talk to this agent in natural
  language. Authenticate and read the extended card for its workflows and
  commands."). Command ops are never skills (F14, F32, F52).
- `capabilities`: `streaming: true`; `pushNotifications` = `a2a.push.enabled`;
  `extendedAgentCard` = `ListenerAuth.declares_any()`; `extensions` = the static
  declarations. The command/v2 params publish
  `ops: static_vocabulary()` — every served-capable op, posture- and
  caller-independent — so a no-auth client can drive the instance without the
  public card revealing its configuration.
- `securitySchemes` / `securityRequirements` per §8.4.

The card varies only with push, whether events/v1 is declared, the security
fields, the advertised URL, `agent.name` and `agent.description`. It never
varies with workflows, introspection or the caller.

### 8.4 securitySchemes / securityRequirements

Derived from `ListenerAuth` and the advertised origin; absent entirely on a unix
listener:

- `bearer` (`httpAuthSecurityScheme`, scheme `Bearer`) iff bearer or device;
- `device_code` (`oauth2SecurityScheme` with a `deviceCode` flow:
  `deviceAuthorizationUrl` and `tokenUrl` on the origin, the configured scopes,
  and `oauth2MetadataUrl` only on an https origin) iff device;
- `mtls` (`mtlsSecurityScheme`) iff mtls.

Requirements: on mTLS, `[{mtls}]` plus `{mtls, bearer}` when bearer; otherwise
one alternative per declared scheme, plus the anonymous `{}` alternative **only**
with an `any` rule; with nothing declared, both fields are absent. Every URL is
built with one join helper that never produces `//`, and the typed round trip
through `a2a-rs` is a fixpoint.

### 8.5 GetExtendedAgentCard (per-caller extended card) and the §13.3 gating

§13.3 says the extended card "MUST require authentication" using "one of the
schemes declared in the public AgentCard.securitySchemes", and §3.3.4 says that
with `extendedAgentCard` false the call MUST return `UnsupportedOperationError`.
agentd follows both literally:

- with no scheme declared, `extendedAgentCard` is false and the call answers
  `-32004`;
- otherwise the listener requires `Via ∈ {Bearer, Session, Cert}`; any other
  caller gets 401, because it authenticated through no declared scheme. On a
  listener that declares a scheme that caller is in practice one matched by an
  `any` rule: the implicit operator and the unix peer exist only where nothing
  is declared;
- `-32007` is never emitted: when the capability is declared, a per-caller card
  always exists.

The extended card is the public card plus a `workflow:<name>` skill for each
workflow `may_run` admits, command/v2 params narrowed to `served ∩
may_command` with the workflow-declared commands the caller may fire and the
runtime-settable paths when it may run `admin.set`, and events/v1 params with
the ring size and the kinds visible to this caller.

This leaves a no-auth dev daemon without an extended card at all. That is the
price of the MUST, and it is why the public command params carry the static
vocabulary: the clients take their ops from it and their workflows from
`status.workflows` (§15.2).

## 9. Extensions

### 9.1 A2A-Extensions request header (activation) and response echo

Every `A2A-Extensions` field line is read, split on `,`, trimmed, de-duplicated,
with empties dropped; at most 32 tokens are considered, and tokens over 512
bytes or non-UTF-8 lines are ignored. Matching is exact. **Activated =
requested ∩ declared ∩ applies to this method.** The header is the only
activation mechanism, and the activated set travels in the request scope.

The echo — `A2A-Extensions: <activated, registry order>` — is sent only on
handler-produced responses: SSE responses, set before the body streams, and
handler JSON-RPC errors. It is absent on every refusal the listener produces
before dispatch and on an empty set, so it never reports an extension on a
request that did not run under it (F29).

**Extension errors -32601 / -32008.** An extension method whose extension is not
declared or not activated gets `-32601` "method not available: activate <uri>
with the A2A-Extensions header" (or "<uri> is not offered by this instance")
with `EXTENSION_NOT_ACTIVATED` / `EXTENSION_NOT_DECLARED`. A missing required
extension gets `-32008` with `EXTENSION_SUPPORT_REQUIRED` (§3.3.4). agentd
declares no required extension today; the path is unit-tested with a synthetic
declaration.

### 9.2 Extension https://agentd.dev/a2a/ext/command/v2

A profile extension, `required: false`, always declared. The envelope is exactly
one DataPart `{data: {agentd: {op, …args}}, mediaType: "application/json"}`
plus optional text parts; `Message.extensions` MUST list the URI, activation
through `A2A-Extensions` is required, and there is no `taskId`. Built-in args
must match the published schema (`additionalProperties: false`), and only
canonical names exist: `workflow.run {workflow, inputs?}`,
`subagent.send {handle, message}`, reads by `{run}` / `{handle}`,
`admin.set {path, value}`. `_instance.result` and `_instance.emit` — the
parent/child protocol of instance-tier subagents — are reserved and
operator-only, published under `$defs.reserved` in the schema bundle with their
args and results, and never listed in `params.ops` (F33).

The version is in the URI and nowhere else; the v1 in-band protocol number is
gone (F34).

#### 9.2.1 surface::ops::OPS (the single command-op table)

One always-compiled table. Each row is
`OpSpec{name, reply: Message|Task, floor: Operator|AnyNamed|Granted, defaults,
scope, gate, handler, description, args_schema, result_schema}`:

| Op | Reply | Floor | Defaults | Gate |
|---|---|---|---|---|
| `status` | Message | AnyNamed | — | Always |
| `config` | Message | Operator | — | Always |
| `workflow.status` | Message | Granted | user, agent | Always |
| `subagent.status`, `plan.get` | Message | Granted | user | Always |
| `conversation.get`, `run.get`, `subagent.get` | Message | Granted | user | Introspection |
| `debug.events` | Message | Operator | — | Introspection |
| `workflow.run` | Task | Granted | user, agent | Always |
| `workflow.cancel`, `subagent.send` | Task | Granted | user | Always |
| `workflow.signal`, `subagent.kill` | Task | Granted | — | Always |
| `admin.drain` / `pause` / `resume` / `cancel` / `set` | Task | Operator | — | Always |
| `auth.device.pending` | Message | Operator | — | Device grant |
| `auth.device.approve`, `auth.device.deny` | Task | Operator | — | Device grant |
| `auth.sessions` | Message | Operator | — | TCP listener |
| `auth.sessions.revoke` | Task | Operator | — | TCP listener |
| `_instance.*` (prefix) | Task | Operator | — | served, never listed |
| `ask_human` | — | Operator | — | reserved, not served |

Dispatch is one match on `op_spec(op).handler`, so a new op is a row plus its
handler module. Everything else is derived: `is_admin_op`, `is_operator_floor`,
`is_read_op` (reply is Message), `served(op, settings)`,
`command_ops_of(settings)` and `static_vocabulary()`. Every row has a
description (F32), and the published schemas are the rows' `args_schema` /
`result_schema`.

**Command envelope refusals** are `-32602` with `BadRequest.fieldViolations`
and an `ErrorInfo` reason: not activated (`EXTENSION_NOT_ACTIVATED`), not marked
in `message.extensions` (`EXTENSION_NOT_MARKED`), more than one envelope
(`COMMAND_ENVELOPE_AMBIGUOUS`), a `taskId` (`COMMAND_TASK_ID`), an unknown or
removed op (`UNKNOWN_OP`, with the `REMOVED_OPS` hint), a schema failure
(`INVALID_COMMAND_ARGS`). Output modes without `application/json` → `-32005`;
an introspection op while introspection is off → `-32004 INTROSPECTION_DISABLED`.
Nothing executes on any refusal, and the listener check and the runtime's belt
check call the same `check_command`.

**Read-op reply (Message).** A Message-reply op answers with an A2A Message —
`role: ROLE_AGENT`, a `msg-<ulid>` id, the request's `contextId` (else a minted
one), one DataPart with the document, and `extensions: [command/v2]`. No task is
created and no task event is pushed (F13, F42): a read is not work, and a client
may poll it without growing every client's task list.

**Task-op reply and result artifact.** Task ops and workflow commands run
through `a2a-rs`: unary blocks per the spec unless `returnImmediately`;
streaming sends a Task frame then updates, closing at a terminal or interrupted
state. An object result is the artifact `<task>.result` with one DataPart and
`extensions: [command/v2]` when the task came from a command; a string result is
a text part; a part-less artifact is never emitted. `Task.command` records the
op.

**admin.set** replaces `config.set` as a command/v2 Task op on the Operator
floor, over `RUNTIME_SETTABLE = [agent.approval, a2a.introspection.enabled]`,
parsed with the configuration's own types. Success is a COMPLETED task with
`{path, value}`, a `config` feed event and an `admin.set` log line. Turning
introspection on installs the log ring. A SIGHUP overwrites a runtime set: the
files stay the one way to change everything else (RFC 0032 §14's reasoning,
unchanged).

**auth.* operator ops** — the device-grant and session administration of §10:
`auth.device.pending`, `auth.device.approve {user_code, as, scope?}`,
`auth.device.deny {user_code}|{all}`, `auth.sessions`,
`auth.sessions.revoke {sid}|{name}|{all}`. The device ops are served iff the
device grant is on; the session ops on every TCP listener (they also list and
revoke launch sessions). Each call is audited and pushes an operator-only `auth`
feed event.

### 9.3 Extension https://agentd.dev/a2a/ext/events/v1

A method extension, `required: false`, declared iff `a2a.events.enabled`
(restart-only). Public params are `{method, schema}`; the extended card adds the
ring size and the visible kinds. Its one method:

**agentd.events/SubscribeToEvents** — SSE, params `{fromSeq?}` (strict:
`after` → `-32602`), callable by any named principal and filtered by
`FeedVis::admits`. Each frame's result is exactly one of
`{hello: {seq, resume, resync, introspection, version}}` (`version` is the
agentd build, not a protocol number), `{event: {seq, ts, kind, data}}` or
`{goodbye: {seq, reason: "deadline"|"revoked"}}`. Frames carry no SSE id. A task
event's annotations are stripped per subscriber unless task-annotations/v1 is
active.

**FeedKind vocabulary** — one enum with `ALL`, `as_str` and a data schema per
kind: `task`, `task.removed`, `run`, `run.removed`, `step`, `conversation`,
`conversation.removed`, `subagent`, `subagent.removed`, `child`,
`child.removed`, `activity`, `activity.removed`, `status`, `lifecycle`,
`config`, `audit` (operator only, while introspection is on) and `auth`
(operator only; `{event: pending|approved|denied|revoked|launch, client_id,
scope, user_code?, peer?, sid?, name?}`). The kinds `message`, `command` and
`pairing` are deleted: the transcript is `Task.history` (§14.1). The feed push
asserts kind and schema in debug builds, a unit test pushes one sample of every
kind, and a source scan requires every push literal to be in `FeedKind::ALL`.

### 9.4 Extension https://agentd.dev/a2a/ext/task-annotations/v1

A profile extension, `required: false`, always declared, params `{schema}`. When
active on a method that returns tasks, every Task carries
`metadata[<URI>] = {link: {kind: run|subagent|turn, id}, principal?, created,
statusHistory, askSchema?, command?}`. When inactive there is no metadata key.
No `agentd/*` key appears anywhere (F35), and push bodies never carry
annotations.

### 9.5 Custom binding https://agentd.dev/a2a/binding/jsonrpc-unix/v1

JSON-RPC 2.0 over HTTP/1.1 on a unix domain socket, with same-uid peers enforced
by `SO_PEERCRED`. It is identical to the JSONRPC binding in method names,
params, error codes and mapping, and in carrying service parameters
(`A2A-Version`, `A2A-Extensions`) as HTTP headers — which is what §3.2.6
requires a custom binding to specify. The peer is always the operator.

§5.8 says custom bindings SHOULD be identified by a URI, and a unix socket is
not the JSONRPC binding (whose URL is an http(s) URL a stock client can dial). So
the card names it by its own URI, agentd's outbound client accepts it for unix
endpoints, and the TypeScript client refuses it as an unsupported binding.

### 9.6 Published extension specifications and schemas

Each URI resolves to a normative page (`docs/ext/<name>-<version>.md`, rendered
under `agentd.dev/a2a/ext/…`) next to a draft 2020-12 schema bundle with `$id`
`<uri>/schema.json`. `agentd --extension-schema <name>/<version>` prints a
bundle, `agentd --extensions` lists `{uri, path, kind, required, spec,
schema}`, and `scripts/gen-schemas.sh` writes the site copies; CI diffs the
regenerated output. The pages' tables are checked against `OPS`, `FeedKind` and
the card params, so the specification cannot drift from the code (F34).

## 10. The device authorization grant

### 10.1 OAuth 2.0 authorization server on the A2A listener

Pairing is replaced by the standard it imitated. agentd serves an RFC 8628
device authorization grant iff `a2a.device_grant.enabled`, on the listener
**origin**:

| Route | Standard | Shape |
|---|---|---|
| `POST /oauth2/device_authorization` | RFC 8628 §3.1 | `client_id` (1–64 of `[A-Za-z0-9._-]`), `scope?` (one configured scope; default `user`) → `{device_code: 64 hex, user_code: XXXX-XXXX, verification_uri, expires_in, interval: 5}` |
| `POST /oauth2/token` | RFC 8628 §3.4 | `grant_type=urn:ietf:params:oauth:grant-type:device_code`, `device_code`, `client_id` → `{access_token: agentd_at_…, token_type: Bearer, expires_in, scope}`; errors in order `invalid_request`, `unsupported_grant_type`, `invalid_grant`, `expired_token`, `slow_down`, `authorization_pending`, `access_denied` |
| `POST /oauth2/revoke` | RFC 7009 | `token`, `token_type_hint?` and `client_id?` (accepted, the hint ignored) → 200, empty |
| `GET /.well-known/oauth-authorization-server` | RFC 8414 | issuer, the three endpoints, `grant_types_supported`, `response_types_supported: []`, auth methods `none`, `scopes_supported` |
| `GET /oauth2/device` | — | fixed `text/plain` instructions |

The user code alphabet is `BCDFGHJKLMNPQRSTVWXZ` (no vowels, so no words). There
is no refresh token. `/oauth2/device_authorization` is limited per source
(the TCP peer: an IPv4 address or an IPv6 /64, so behind a reverse proxy every
device is one source) — burst 5, one per 12 s; at most 4 pending per source, 16
per network (an IPv6 /48, or the IPv4 address), 64 in all. At the global bound a
newcomer is not refused while some network holds more than one code beyond its
own count: that network's oldest code still waiting on an operator is expired
(its device is told `expired_token`), so a flood displaces only itself. The
device-code token exchange has its own bucket (burst 30, one per second).
Codes and tokens are never logged. Every `/oauth2/*` response is `no-store`
and passes the CORS gate.

**The origin-only issuer.** `a2a.url` may not carry a path. RFC 8615 card
discovery and RFC 8414 metadata both key off the origin, and a path-bearing
base breaks both — the metadata of `https://h/a2a` lives at
`https://h/.well-known/oauth-authorization-server/a2a`, which nothing on the
card could name consistently. So the issuer is the advertised origin without a
trailing slash, and every endpoint URL, on the card and in the metadata, comes
from the same join helper.

**The form rules** (RFC 6749 §3.1, §3.2). Every POST is
`application/x-www-form-urlencoded` only, at most 4 KiB, valid UTF-8 after
percent-decoding. A duplicated parameter is `invalid_request`; an empty value
counts as absent; **unknown parameters are ignored**, as the RFC requires — a
client adding `resource` or `audience` is not refused.

### 10.2 The operator-credential requirement

`a2a.device_grant.enabled` is refused at load without an operator credential:
"a2a.device_grant.enabled needs an operator credential to approve device codes:
set a2a.bearer or add an a2a.principals rule with role: operator and
match.bearer_ref". Enabling the device grant ends the implicit operator (it is
a credential mechanism, §7.2), and approvals are operator ops — so without an
operator credential nobody could ever approve a code. It is also refused on a
unix listener (the peer is already the operator) and with `a2a.tls.client_ca`
(the TLS layer requires a client certificate before `/oauth2/*` is reachable;
see §19).

The consequence, stated plainly: a daemon with no credential configured cannot
issue browser sessions through the device grant. Its browser console is
`agentd ui` (§11).

### 10.3 Session tokens and named device identities

A token is `agentd_at_` plus 64 lowercase hex (256 bits from the OS CSPRNG),
opaque; only its SHA-256 is stored, in memory, so a restart revokes every
session. A session is `{sid, kind, name?, role, principal, client_id, created,
expires, approved_by}`; `Principal.session` is its sid. The prefix is reserved:
an `a2a.bearer` or `bearer_ref` secret using it is refused.

**Named device identities.** `auth.device.approve` requires `as`, the name of
the person being let in:

- `as` matches `^[a-z0-9][a-z0-9._-]{0,63}$` — lowercase, so two spellings never
  name two principals;
- the reserved approval names `operator`, `anonymous`, `unknown`, `launcher`,
  `runtime` and `system` are refused, because they would make audit lines and
  labels lie;
- a name equal to a declared `a2a.principals[].id` is refused, naming the rule;
- the session's principal is `user:<name>` with role user, unless the approver
  explicitly grants scope `operator` (allowed only when the configured scopes
  contain it; the principal is then `operator` and the name is recorded);
  `agent` is never grantable;
- the scope defaults to the requested one, and an approver may narrow a request,
  never widen it.

**Why a name and not a per-session id.** Ownership of tasks, runs, subagents and
conversations is keyed on the principal id (§7.7). A principal per session would
orphan all of it at token expiry — eight hours by default — and a re-login would
start the person from nothing. The operator knows who they are letting in; the
name makes that knowledge the identity.

**Shared-principal semantics.** Several sessions approved with one name are one
principal, by design: they share ownership, one rate bucket
(`a2a.device_grant.rate`, keyed by principal id) and one status scope, so a
person's laptop and phone see the same conversations. Reusing a name for a
different person therefore hands that person the name's history. The approval
reports `existing: true` when the name was approved before, so the operator
learns that at the moment it matters, and the docs say so.

**Per-sid revocation.** `auth.sessions.revoke {sid}` ends exactly that session —
its token turns 401 `invalid_token` and its open streams close within a tick
(§6.3) — and leaves its siblings working; `{name}` ends every session of the
name; `{all: true}` ends all; `/oauth2/revoke` ends the presented token. Revoking
never deletes the principal's tasks or runs.

### 10.4 The identity registry

User-role rule ids and device names both produce `user:<x>`, and ownership
persists (in the durable store) while sessions do not. Without more, this
sequence would merge two people: approve `alice`, who creates tasks; restart
(the session is gone, the tasks stay); declare a rule with `id: alice` — and
the rule's caller inherits the device user's tasks and transcripts.

So the durable store keeps an **identity registry**: `Kind::Identity` records
`id → {kind: rule|device, first_ms}`. Every declared user- or agent-role rule
id is registered as `rule` at listener spawn and at every principals reload; an
approval registers `user:<name>` as `device`. Then:

- an approval refuses a name that is declared now **or** whose `user:<name>` is
  registered as a rule — "the name <n> was a configured principal id on this
  agent and owns its history";
- startup (exit 2) and a reload (the old resolver stays; logged
  `identity.collision`) refuse a declared user-role id whose `user:<id>` is
  registered as a device.

A device name and a rule id therefore never share a principal id, across
restarts, token expiry and rule removal, for as long as the store persists (a
memory store persists no ownership either). There is no command that releases
a registered id (§19).

**Cert-derived ids.** A rule without `id` takes its principal id from the
certificate: `<role>:cn=<CN>` or `<role>:san=<SAN>` (§7.4). Those ids are not
registered, and need not be: the `=` they carry is outside both the declared-id
and the approval-name charsets, so they can never equal a rule id or a device
name — whether or not the device grant is ever allowed alongside `client_ca`
(§10.2).

## 11. The launch grant and the thin launcher

### 11.1 Thin launcher (agentd tui / agentd ui)

`agentd tui [--daemon-log PATH] <daemon args…>` and
`agentd ui [--port N] [--no-open] [--daemon-log PATH] <daemon args…>` (unix, the
`a2a` feature) run the daemon **unchanged** — the args load exactly as
`agentd <args>` would — and exec one documented client. The launcher adds no
configuration key, flag or variable and forces no UI configuration: the feed
needs `a2a.events.enabled` and the debug surface `a2a.introspection.enabled`, in
the operator's own config. v1.16 forced `interface.enabled` through argv and
handed the client `AGENTD_BEARER` (F06, F57); both are gone.

**Refused before anything is spawned** (exit 2, each message pointing to
`agentd-<sub> --endpoint`): a unix `a2a.listen`; port 0 (the endpoint needs a
fixed port); `a2a.tls.client_ca` (the clients present no certificate, so a code
minted there could never be redeemed); and an endpoint — `a2a.url`, else the
concrete bind — whose host is not loopback (the launch grant is redeemed only
from a loopback peer, and an https certificate must match the name the client
dials). A remote deployment's console is `agentd-tui --login`.

**The client contract** — `surface::launch::LAUNCH_CONTRACT`, the one source
the docs guard checks — is exactly one binary per subcommand (`$AGENTD_TUI_BIN`
else `agentd-tui`; `$AGENTD_UI_BIN` else `agentd-ui`) with exactly this argv:

- tui: `--endpoint <url> --launch-fd 3` — fd 3 is the read end of a pipe holding
  one launch code. **Always**, whatever the posture: the code costs nothing on
  a no-auth loopback daemon, and the session it buys survives a reload that adds
  principals and ends the implicit operator, so the argv never depends on the
  posture;
- ui: `--endpoint <url> --listen-fd 3` — fd 3 is a TCP listener the launcher
  bound on `127.0.0.1:<port>` (default 4173).

**File descriptors.** Every fd the launcher creates is close-on-exec in the
parent — the pipe (`pipe2(O_CLOEXEC)`), the UI listener, the saved terminal
(`F_DUPFD_CLOEXEC`), the daemon log — so no process the daemon spawns (the exec
tool, instance children, subagents) inherits the operator's terminal, the pipe
or the UI socket. The fd reaches 3 only in the client's `pre_exec`, clearing
`FD_CLOEXEC` explicitly when the source already is 3. The parent closes the read
end and the listener right after spawn and the write end once the code is
written, so the TUI's read reaches EOF.

**Environment.** The launcher's own, minus every variable the config loader
reads — derived from the loader's own tables, every `ENV_ALIASES` entry under
every prefix included (so `SERVE_BEARER`, which sets `a2a.bearer`, is scrubbed
in all three spellings) — minus every name a `{{secret:NAME}}` reference in the
loaded settings names, minus `AGENTD_BEARER`; nothing is added. The promise is
narrow on purpose: "never passes `a2a.bearer` or a resolved config secret".
Credential variables that code other than the loader reads (the AWS chain) and
secrets referenced only from separately loaded documents pass through, and a
same-uid child could read its parent's environment anyway.

**stdio and lifetimes.** The TUI gets the terminal; the UI child gets
`/dev/null` on stdin, so the launcher keeps the terminal's input for the
terminal-approved sign-in (§11.2). The daemon's output goes to `--daemon-log`
(default under `$XDG_RUNTIME_DIR`, opened `O_CREAT|O_EXCL|O_NOFOLLOW`, mode
0600, so a planted file or symlink is refused). The client exiting drains the
daemon; the daemon exiting sends the client SIGTERM, then SIGKILL after 3 s.

**REMOVED_LAUNCHER_FLAGS**: `--debug` ("the launcher no longer changes
configuration: set a2a.introspection.enabled") and `--inline` ("a display-client
option: the launcher passes the client only its endpoint"), refused before any
config load. `AGENTD_INTERFACE_LOG` is refused through the `interface` catch-all
(§13.2) and replaced by `--daemon-log`. No error text names an npm package.

**Why there is no client-side `--spawn`.** A client that started its own daemon
would be a second way to do what the launcher does, and would need a second copy
of the fd hygiene, the environment scrub and the refusals. It could not mint a
launch code either — minting is in-process by design (§11.2) — so it would be
back to passing a credential. One mechanism, in the process that owns the
daemon.

### 11.2 Launch grant (single-use launch code, launch sessions)

`LAUNCH_GRANT_TYPE = https://agentd.dev/oauth/grant-type/launch/v1`, an RFC 6749
§4.5 extension grant. Minting and approval are **in-process only**: the launcher
creates a `LaunchSlot` and hands it to the daemon it runs. There is no op, route
or configuration key that mints a code or approves a request, so nothing outside
the launcher's process can.

Two ways to redeem, both at `POST /oauth2/token` under the form rules of §10.1,
both accepted **only from a loopback peer address** (a code leaked off-host is
useless even on a wildcard bind; an `ssh -L` forward arrives from the remote
host's loopback and works), and both bound to the slot's bind — a parsed UI
origin, or "no Origin" for the TUI:

**(a) The code — the zero-typing path.** `agentd_lc_` plus 64 hex (256 bits);
only its SHA-256 is stored, with the bind, the `client_id` and an expiry of
60 s; issuing again for a bind invalidates its earlier code. Exchange
`{grant_type, code, client_id}`. **Any presentation of a live code consumes it.**
An unknown, used or expired code, a bind mismatch (any `Origin`, `null`
included, for a no-Origin code; none or another origin for an origin code) or a
`client_id` mismatch → `invalid_grant`, one answer for all — so a stolen code can
at most be burned. On consumption the launcher deletes the launch file. The TUI
receives its code on the inherited fd; the UI receives it in the URL fragment
(`#launch=…`), through a 0600 launch file in a 0700 non-hidden directory under
`$HOME` opened with the desktop opener, or printed as the full URL on the
launcher's terminal (never the daemon log) with `--no-open` or when the opener
fails.

**(b) The terminal-approved request — UI slots only.** It serves every browser
the file cannot reach: sandboxed (snap, flatpak) browsers, an `ssh -L` forward
(which must keep the same local port, since the origin is bound), `--no-open`, a
cold browser slower than 60 s, a second tab, an expired session — all without
restarting. The page POSTs `/oauth2/launch_authorization {client_id}` with the
slot's `Origin` from a loopback peer and gets
`{request_code: agentd_lr_…, user_code: XXXX-XXXX, expires_in: 120,
interval: 2}` (404 when no origin slot is installed). The launcher prints "A
browser tab asks to sign in to agentd: type the code it shows (Enter to skip)"
and reads its own stdin; a line that normalises to a pending user code approves
exactly that request. The page polls `{grant_type, request_code, client_id}`
with the same `Origin`. **The terminal is the trust anchor:** another web origin
cannot start a request (Origin binding and CORS), and another local process can
start one but cannot make the person at the terminal type its code.

**The launch session.** Success either way →
`{access_token: agentd_at_…, token_type: Bearer, scope: operator}` for a session
of kind `launch`: sid `ls_…`, role and principal `operator`, rate-exempt,
`approved_by` `launcher` or `launcher-terminal`. A UI session lives 8 hours (the
tab re-signs-in through (b) while the launcher runs); a TUI session has no
expiry — its token exists only in the TUI's memory — so a long-running console
is not cut off daily. Every launch session is revocable by
`auth.sessions.revoke {sid}` and `/oauth2/revoke`, and ends with the launcher's
process.

**Failure-only limiting.** A live code or an approved request is always
honoured. Only `invalid_grant` answers are counted (per source, burst 20, one per
3 s); past the limit a *failing* presentation gets 429. Launch exchanges never
draw from the device grant's bucket, so a flood of junk from 127.0.0.1 cannot
delay a valid exchange past its 60 s.

A slot changes no posture: `ListenerAuth`, both cards and the manifest are
identical with and without it, and the card declares no scheme for it. While a
slot is installed `/oauth2/token` and `/oauth2/revoke` are served even without
the device grant (accepting only the launch grant type unless the device grant is
also on), and `grant_types_supported` lists the launch grant.

**Why operator.** The session acts for the person who ran `agentd tui|ui` —
who started this daemon, in-process, from their own configuration and
credentials, and who on a no-auth loopback daemon is already the implicit
operator from any non-browser process. A lower role would protect nothing from
that person and would break the console's operator functions (gates, `admin.*`,
`auth.device.approve`).

**Why it recreates no known hole.**

- *F06:* nothing is served over HTTP and `agentd-ui` holds no credential; the
  code reaches the browser only through a 0600 file or the owner's terminal,
  and only in a URL fragment, which is never sent over HTTP.
- *F07:* no credential is ever in a query string; the page strips the fragment
  before any request and exchanges the code only with the endpoint `agentd-ui`
  was started with, from a loopback peer; the session token never appears in a
  URL. The fragment may reach browser history or an extension holding the tabs
  permission before the strip — harmless, because the code is single-use,
  lives 60 s and is consumed within seconds.
- *The browser implicit-operator hole:* the tab authenticates with a session no
  other origin can obtain (Origin binding, CORS and, for (b), terminal
  approval). CORS admission of the launched origin grants nothing by itself.

**Rejected designs.** Passing `a2a.bearer` or any long-lived secret (F06).
Serving a code from `agentd-ui` over HTTP — any local process or rebinding page
could race for it. Opening `http://127.0.0.1:<port>/#launch=…` directly — the
URL would sit on the opener's argv, world-readable in `/proc`. Letting
`agentd-ui` bind its own port — a squatter holding the port would receive the
fragment; the launcher binds it and hands it over.

**Residual risk, stated.** Other local users cannot read the launch file, the
pipe, the terminal or the pre-bound port — but on a no-auth loopback TCP daemon
every local uid is already the implicit operator through plain curl, so these
protections matter on protected daemons, and a multi-user host should use a unix
listener or `a2a.bearer`. Same-uid processes, agentd's own model-driven exec and
instance children included, are equivalent to the user: within the 60 s window
one could read the launch file or `/proc/<tui>/fd/3`. Fd hygiene,
unlink-on-consume and loopback-only redemption shrink that window; they do not
close it, and `docs/security.md` says so.

## 12. Browsers

**Browser credentials in sessionStorage only.** The web UI keeps tokens in
`sessionStorage` (`agentd-ui.cred`, keyed by the endpoint it is bound to,
dropped at expiry and on disconnect). `localStorage` holds only
`{endpoint}` and the layout, never a credential. `sessionStorage` is scoped to
one tab and dies with it; `localStorage` would share the token with every tab of
the origin and outlive the browser session. No credential is ever read from or
written to a query string (`?bearer=` is ignored and stripped), and disconnect
revokes the token (`/oauth2/revoke`) before clearing it.

**agentd-ui web surface.** `agentd-ui [--endpoint URL] [--port N | --listen-fd N]
[--open]` holds no credential of any kind: `AGENTD_BEARER` is refused by name,
`/config.js` is 404, and `GET /bootstrap.json` returns `{endpoint}` only, gated
on `Host`, `Sec-Fetch-Site` and a same-origin `Origin`. Every response carries a
Host check, CORP same-origin, nosniff, `Referrer-Policy: no-referrer` and a CSP
whose `connect-src` is `'self'` plus the endpoint's origin. With no credential
and no launcher slot, the page offers device sign-in when the card declares
`device_code`, and otherwise says the daemon issues browser sessions only through
`a2a.device_grant` or `agentd ui`. The launch code (`#launch=`) is read once and
stripped before any request, and exchanged only with the bootstrap endpoint — a
typed or `?endpoint=` endpoint that differs discards it unsent.

## 13. Configuration moves and refusals

### 13.1 New keys

- **a2a.url** — an optional absolute http(s) URL with no path other than `/`,
  no query and no fragment; https unless the host is loopback. It is the
  advertised interface URL and the OAuth issuer (§10.1). Required for wildcard
  binds (F18); without it a concrete bind advertises `scheme://host:<port>/` and
  a unix socket `unix://<path>`. The manifest reports `configured_url`.
  Restart-only, operator-only.
- **a2a.cors.origins** — exact origins; paths and `*` refused. A locally served
  UI's origin must be listed, except the one `agentd ui` launches. Reloadable
  (a live origin list), operator-only. It gates `POST /` and `/oauth2/*`, and
  PNA on the card.
- **a2a.device_grant** — `{enabled: false, scopes: [user], token_ttl: 8h
  (5m..30d), code_ttl: 10m (1m..30m), verification_uri?, rate?}`. Restart-only,
  operator-only. Refused with `client_ca`, on a unix listener, with empty or
  duplicate scopes or `agent`, and without an operator credential (§10.2).
- **a2a.events.enabled / a2a.introspection.enabled** — the two generic
  switches that replace the UI flags (F19, F43). `a2a.events.enabled`
  (restart-only) declares events/v1 and builds the feed.
  `a2a.introspection.enabled` (reloadable, settable by `admin.set`) gates the
  introspection ops, the `audit` kind and the log ring, independently of the
  feed. Both require `a2a.listen` and are operator-only.
- **a2a.principals[].id and match.aauth_agent removal** — `id` is required on
  `bearer_ref` and `any` rules ("a bearer_ref/any rule names one caller, so it
  needs `id:` — the principal id its tasks and conversations are owned by"),
  optional on `san`/`sub` rules; `match.aauth_agent` is refused by name.
- **New agent/observability/security/store keys** — `agent.description`
  (reloadable, operator-only, so a served document cannot rewrite the public
  card); `agent.ask_human_unowned: gate|fallback` (default `fallback`, §14.3);
  `observability.status_values` (memory keys published in `status.values`);
  `security.policies[].to` (the addressee of an `ask` policy, default
  `{role: operator}`); `store.retention.tasks: {keep_last?, ttl?}` for terminal
  tasks (unset keeps).

### 13.2 REMOVED_KEYS (config keys refused by name)

One table, checked on each file and the merged document, each `:::!config`
fragment, the canonical flags and the `AGENTD_` environment names derived from
the same bindings: `interface`, `interface.enabled`, `interface.debug`,
`interface.display`, `interface.origins`, `interface.pairing`,
`a2a.principals[].match.aauth_agent`. Each is refused (exit 2) as
"<source>: `<path>` was removed in agentd 1.17.0: <hint>". The `interface`
catch-all also refuses every `AGENTD_INTERFACE_*` variable. None exists in the
generated schema, and `--help` lists them.

### 13.3 The migration

| Before (v1.16) | After (v1.17.0) |
|---|---|
| `interface.enabled` | `a2a.events.enabled` (the feed). HITL no longer depends on it; use `agent.ask_human_unowned: gate` for operator-answerable asks no caller owns. |
| `interface.debug` | `a2a.introspection.enabled` (reloadable, `admin.set`-able) |
| `interface.display` | Removed; layout is client-side (`--top/--bottom`, `AGENTD_TUI_TOP/BOTTOM`, `agentd.layout`, `/layout`). Memory values → `observability.status_values`. |
| `interface.origins` | `a2a.cors.origins`, exact origins, no loopback trust |
| `interface.pairing` | `a2a.device_grant` (needs an operator credential); approve with `auth.device.approve {user_code, as}` |
| `Pair`, `pairing.code` | The device grant; `agentd ui` for a signed-in local tab |
| `config.set` | `admin.set {path, value}` |
| `interface.info` | The card and the `status` op |
| `admin.lameduck` | `admin.drain` |
| `GetAgentCard` | `GET /.well-known/agent-card.json` |
| `SubscribeToEvents` | `agentd.events/SubscribeToEvents` under events/v1 |
| `command/v1`, interface/v1 URIs | `command/v2`; events/v1; task-annotations/v1 |
| `agentd tui` / `agentd ui` `--debug`, `--inline` | Config keys; client options |
| `AGENTD_INTERFACE_LOG` | `agentd tui` / `agentd ui` `--daemon-log PATH` |
| `agentd-tui --bearer`, `--code` | `--bearer-file`, `AGENTD_BEARER`, `--login [--scope]` |
| `agentd-ui` `?bearer=`, `/config.js`, `AGENTD_BEARER` | Device sign-in or `agentd ui`; `sessionStorage` only |
| `match.aauth_agent` | `san`, `sub` or `bearer_ref` |

Clients and scripts: every JSON-RPC POST needs `Content-Type:
application/json`, `A2A-Version: 1.0` and an id; messages need `ROLE_USER`;
command DataParts need `A2A-Extensions: …/command/v2` plus
`message.extensions`. A loopback TCP listener with nothing configured still
treats local non-browser callers as operator; once anything is configured, or the
bind is not loopback, an uncredentialed caller gets 401.

## 14. Tasks and human-in-the-loop

### 14.1 Task.history

The transcript moves from a private feed kind to the core Task (F41). The
durable task gains `messages` — inbound caller Messages (ids set to the task's)
and superseded agent status messages (`<task>.status.<seq>`) — bounded at 64
messages / 256 KiB, oldest dropped. The current status and the result artifact
are not duplicated. `historyLength` unset returns all, 0 omits history, and n
the newest n. The feed's `task` event carries at most 4 messages / 32 KiB, and
the `message` and `command` feed kinds are deleted — every A2A client, not only
agentd's, now sees what was said.

### 14.2 The core task methods

**ListTasks** honours every request field (C3, F46): `contextId` (through the
namespace of §7.7), `status`, `statusTimestampAfter`, `pageSize` (default 50,
1..=100), an opaque `pageToken`, `historyLength` and `includeArtifacts`. Results
are owner-scoped for non-operators and sorted by update time, newest first. The
response is `{tasks, nextPageToken ('' on the last page), pageSize,
totalSize}`.

**Push notifications** take the 1.0 shape (F09, F36, C4, C5):
`TaskPushNotificationConfig {taskId, id?, url, token?, authentication?:
{scheme, credentials}}`, validated at registration (SSRF and egress, token-shaped
scheme, no control characters in credentials); read-backs echo only the scheme;
`Get` and `Delete` require the id; list paging as `ListTasks`. Delivery POSTs a
`StreamResponse {task}` (no annotations) with `content-type:
application/a2a+json`, `authorization: <scheme> <credentials>` when registered,
and `x-a2a-notification-token` when a token was registered — a courtesy header
`a2a-python` 1.x reads, documented as not part of 1.0. Terminal tasks are
evicted by `store.retention.tasks` with a `task.removed` event.

### 14.3 HITL availability and policy addressee

Human-in-the-loop is a core A2A flow (`TASK_STATE_INPUT_REQUIRED`), so it no
longer depends on a UI switch (F19). **Ownership decides:**

- an ask owned by a caller's task always gates while the A2A listener serves;
- an ask no caller owns (a scheduled turn, a subagent) gates only with
  `agent.ask_human_unowned: gate`, as a standalone input-required task (owned by
  the caller when there is one, else the operator); otherwise
  `agent.ask_human_fallback` applies, exactly as when no channel exists;
- a policy `ask` gate is addressed to `security.policies[].to`, default
  `{role: operator}` — so a caller approves its own gated call only when the
  operator wrote that down;
- an answer from someone the gate is not addressed to gets `-32602`, and the
  gate stays open.

Workflow waits get the same discipline: **wait {on: message}.from /
a2a.wait.from** — `wait {on: message, conversation?, from?, timeout?}` and
`a2a.wait {conversation?, from, timeout?}` (`from` required) match only the
conversation AND the sender (`from`, else the run's principal, an operator or a
runtime event); `conversation: "*"` means any.

## 15. Clients

### 15.1 TS client request surface

Every JSON-RPC POST carries `content-type: application/json`,
`a2a-version: 1.0`, `accept`, `authorization` only for the endpoint the
credential is bound to, `a2a-extensions` only with declared URIs, and
`last-event-id` only on a `SubscribeToTask` resume. Fetch uses
`redirect: "error"`, `credentials: "omit"`, `cache: "no-store"`, and params carry
the interface tenant. Only `SendMessage`, `SendStreamingMessage`, `GetTask`,
`ListTasks`, `CancelTask`, `SubscribeToTask`, `GetExtendedAgentCard` and
`agentd.events/SubscribeToEvents` are emitted. Messages are `ROLE_USER`, never
pair a `taskId` with a `contextId`, set `returnImmediately` explicitly and never
send `blocking` (F20, F21). URIs and op literals live in one module.

**TS error classification** maps every failure to one of `unauthenticated`
(401, `-31401`, `goodbye revoked`, expiry), `forbidden`, `incompatible`
(`-32009`, `-32008`, discovery errors), `rate-limited`, `protocol`,
`unavailable` (`-32601` / `-32004` on an extension call) and `transient` — so an
auth or version failure stops instead of retrying forever (F48). The SSE parser
follows WHATWG, CRLF included, with an 8 MiB cap (F47).

### 15.2 Client discovery and standards-only mode

**TS discovery and capabilities.** The client fetches the card from the
endpoint's origin with no credential, caps it at 1 MiB and caches it with its
ETag and max-age, revalidating with `If-None-Match` (F22). It selects the first
`JSONRPC` 1.0 interface; `UNIX_BINDING`, `unix:` and wildcard hosts are refused,
a cross-origin interface is followed only without a user credential (over https
or loopback http, with a warning), and a required extension it does not know
stops it (F23). Capabilities come from the public card plus
`GetExtendedAgentCard` when `extendedAgentCard` is true and a credential is
bound; otherwise ops come from the public static vocabulary and workflows from
`status.workflows`.

**Standards-only mode** (C9). `--no-extensions` or `?extensions=off` — or a card
that declares none of agentd's extensions — runs the client on core methods
alone: `ListTasks` paging with `statusTimestampAfter` and `SubscribeToTask`
followers instead of the feed, the transcript from `Task.history`, `status`
read only when offered. The same client drives any A2A 1.0 agent.

In events mode the client resumes from the `goodbye` frame's `seq` (F55). Its
slash commands, the settable paths and the skill prefix come from the card and
the `status` document, not from knowledge of agentd's configuration built into
the client (F58).

### 15.3 agentd-tui CLI

`agentd-tui [--endpoint URL] [--bearer-file PATH] [--login] [--scope
user|operator] [--launch-fd N] [--no-extensions] [--top items] [--bottom items]
[--debug] [--inline] [--insecure]`. `AGENTD_BEARER` is read and then deleted from
the environment. `--bearer` and `--code` are refused by name. `--launch-fd N`
reads at most 256 bytes to EOF from the inherited fd, closes it, and exchanges
the code once with `client_id` `agentd-tui`; the session is held in memory only.
Combining it with another credential is exit 2, and a refused exchange says to
restart `agentd tui`. The TUI never sends `Origin`: started by hand on a no-auth
loopback daemon it is the implicit operator, and started by `agentd tui` it
always holds a launch session.

### 15.4 Outbound peer client

agentd as an A2A client speaks the same 1.0 (C2, C8). Every request sends
`A2A-Version: 1.0` over agentd's own transport (SSRF guard, mTLS and signing
kept). `send` is a typed `ROLE_USER` message with `returnImmediately: true`;
command sends mark the message and send `A2A-Extensions`. `delegate` fetches the
peer's card (cached up to 300 s), selects the first `JSONRPC` 1.0 interface —
or `UNIX_BINDING` for a unix endpoint — always dials the configured URL, echoes
the tenant, refuses a required unknown extension, uses unary + poll when the
peer does not stream, and proceeds streaming-first when the card is
unavailable. A reply may be a Message or a Task.

## 16. How this is kept honest

Every guarantee above has a test that fails when its guarding line is reverted;
the ones that hold the design together are the derived lists:

- the core method table against `a2a-rs`'s constants; the op table's dispatch
  completeness; every feed push against `FeedKind::ALL`; the two single-emitter
  error codes;
- the posture test iterates (posture, caller locality, Origin) against the
  resolver, so `required`, the card and the resolver cannot disagree;
- the extension pages and schema bundles are regenerated and diffed in CI;
- a repository-wide docs guard refuses removed names in the documentation and
  checks the `#launcher` section against `LAUNCH_CONTRACT`;
- the conformance suite gains events, extensions, auth and card-honesty
  families (C6 — its push check had called a 0.3 method name and forbidden the
  spec's `-32003`, so it passed on any server);
- CI runs the official **a2a-python** client against the real daemon, and
  agentd's `a2a.send` / `a2a.delegate` against the stock Python sample server —
  the first checks of agentd by someone else's reading of the spec.

**Test harness helpers.** The e2e and conformance tests build requests through
one spec-shaped client (`a2a_post`, `send_text`, `send_command`, `get_card`),
which sends `application/json`, `A2A-Version`, `ROLE_USER` and explicit
`returnImmediately`, and a guard refuses `"blocking"`, `GetAgentCard` and
hand-built command DataParts anywhere else — so the tests speak the protocol the
server is being held to.

## 17. Compatibility

### 17.1 Breaking, by design

Every removal is refused by name with its replacement (§13.3). Nothing keeps
working silently under an old name: an alias that still answered would be a
second vocabulary again. A client built on `a2a-rs` ≤ 0.10 (no `A2A-Version`
header) gets `-32009`; that is the specification's rule, not agentd's.

### 17.2 RFC 0032

RFC 0032's observation plane survives as events/v1; its discovery, reads,
chrome, pairing, `config.set`, composer shortcuts and HITL availability are
replaced as described here. RFC 0032 carries a dated note at each superseded
section.

### 17.3 A correction to RFC 0042

RFC 0042 §2 said that an `any` rule with `grants: ["*"]` reached every non-admin
command but that "operator control itself was never reachable", because the
admin family answered to the role. That was false: `pairing.code`, `config.set`,
`config`, `debug.events` and `subagent.get` answered to grants, and
`pairing.code` let the caller mint an operator session (F02). RFC 0042 is
corrected in place; the Operator floor of §7.6 closes it.

## 18. Alternatives considered

**Patch `a2a-rs` to keep error data.** Rejected: agentd would own a fork of the
wire. The out-of-band error object (§6) keeps the library unmodified.

**Keep loopback origins trusted for CORS.** Rejected: loopback is where the
listener lives, not where a browser's code comes from. Every web origin a user
visits can target 127.0.0.1.

**Keep pairing, add rate limits.** Rejected: pairing was a non-spec anonymous
RPC issuing credentials, and every hardening would have been agentd's own
protocol. RFC 8628 is the standard it imitated, and clients already implement it.

**A per-session principal for device logins.** Rejected: ownership would be
orphaned at every expiry (§10.3).

**Revoke live sessions when a reload declares a device's name.** Rejected:
sessions are in memory and ownership is durable, so the collision outlives any
session — which is why the registry refuses the declaration instead (§10.4).

**Count every refusal in the auth limiter.** Rejected: it turns the limiter into
a lockout any web page can trigger (§7.5).

**A client-side `--spawn`.** Rejected: two mechanisms, and minting is
in-process (§11.1).

## 19. Deferred

- **The device grant alongside mTLS.** The TLS verifier makes client
  certificates mandatory, so `/oauth2/*` is unreachable without one. Supporting
  both needs an optional-client-auth verifier in `agentd-net`; until then the
  combination is refused at load. (Cert-derived ids need no registry entry if
  it is ever allowed: they cannot spell a device name — §7.4, §10.4.)
- **Per-source limits behind a TLS-terminating proxy.** `X-Forwarded-For` is
  never trusted, so behind a proxy every limiter acts globally; documented.
- **A per-principal cap on concurrent streams.** Per-request admission bounds
  stream opening; a concurrent cap needs a counter shared by the `a2a-rs` and
  feed paths, and a new key.
- **Releasing or renaming a registered principal id.** A release would have to
  decide what happens to the objects the id owns (transfer, archive, delete).
  An operator who wants a fresh identity chooses another name.
- **A per-process incarnation id in the events `hello` frame.** No contract
  carries one; the client's `ahead` check and re-discovery cover the restart
  case, and adding an optional field later is additive under events/v1.
- **Versioning the npm package on its own semver** — release policy, not
  protocol; the client checks exact extension URIs.

## 20. Also in v1.17.0, outside this boundary

The same release fixes an MCP-side defect that is not part of the A2A boundary
but shares its rule — a check that reported success while nothing was checked.
It is recorded here so the release's contracts have one index:

- **MCP client read semantics (no response cache).** The MCP client disables
  rmcp's response cache before any request, so every read and list is one
  request and a failure is an `Err`. With the cache on, an instruction re-read
  against a dead registry returned a cached success, the instruction
  specification's §7.7 freshness deadline never tripped, and `unavailable: refuse` and a pinned source's `freeze` never
  fired.
- **Instruction freshness re-read target and instruction.unavailable.** The
  re-read targets the serving server (`mcp://<server>/<uri>`), and an unanswered
  re-read logs `instruction.unavailable {uri, server, err, policy,
  trust_pinned}`, never `instruction.loaded`.

## 21. Cross-references

- **RFC 0029** — principals, roles and commands; amended by §7 and §9.
- **RFC 0032** — the observation plane; superseded in part (§17.2).
- **RFC 0042** — corrected (§17.3).
- **RFC 0044** — builds on this RFC's listener posture, card schemes, session
  store and named device principals.
- **`docs/a2a.md`**, **`docs/a2a-extensions.md`**, **`docs/ext/`**,
  **`docs/interface.md`**, **`docs/security.md`**, **`docs/configuration.md`** —
  the operator-facing documentation of what ships.

---

## Appendix A. Contract index

Every contract of the v1.17.0 plan, and where this RFC records it.

| Contract | Section |
|---|---|
| POST / request pipeline (order and refusal at each step) | §5.1 |
| Error fidelity through a2a-rs | §6.2 |
| Session liveness hook | §6.3 |
| A2A-Version request header | §5.2 |
| surface::A2A_PROTOCOL_VERSION and extension constants | §4.3 |
| Core method table (surface::SpecMethod, Route, route_of, methods_of) | §4.1 |
| Removed JSON-RPC methods and HTTP routes | §4.2 |
| Shared error table crate::a2a::errors | §5.3 |
| HTTP 401 authentication challenge (-31401) | §5.3 |
| HTTP 403 permission denied (-31403) | §5.3 |
| Object-level denial = -32001 | §5.3 |
| Per-principal rate refusal | §5.3 |
| Draining refusal | §5.3 |
| CancelTask on a terminal task | §5.3 |
| SubscribeToTask visibility and terminal behaviour | §5.3 |
| Identity resolution (Evidence, Resolution, Via, SessionVerifier) | §7.1 |
| Principal id | §7.4, §10.3 |
| Listener posture (surface::auth::ListenerAuth) | §7.2, §7.3 |
| surface::ops::OPS (the single command-op table) | §9.2.1 |
| REMOVED_OPS | §7.6 |
| Principal authorization API | §7.6 |
| Ownership helpers, scoped signals and the model path | §7.7 |
| contextId namespace | §7.7 |
| CORS policy | §8.2 |
| GET /.well-known/agent-card.json | §8.1 |
| Public Agent Card | §8.3 |
| GetExtendedAgentCard (per-caller extended card) | §8.5 |
| securitySchemes / securityRequirements | §8.4 |
| Custom binding https://agentd.dev/a2a/binding/jsonrpc-unix/v1 | §9.5 |
| Extension https://agentd.dev/a2a/ext/command/v2 | §9.2 |
| Extension https://agentd.dev/a2a/ext/events/v1 | §9.3 |
| Extension https://agentd.dev/a2a/ext/task-annotations/v1 | §9.4 |
| A2A-Extensions request header (activation) and response echo | §9.1 |
| Extension errors -32601 / -32008 | §9.1 |
| Command envelope refusals | §9.2.1 |
| Read-op reply (Message) | §9.2.1 |
| Task-op reply and result artifact | §9.2.1 |
| admin.set | §9.2.1 |
| auth.* operator ops | §9.2.1, §10.3 |
| SendStreamingMessage stream shapes | §5.4 |
| SendMessage task-id rules | §5.4 |
| Message parts / -32005 | §5.4 |
| ListTasks | §14.2 |
| Push notifications | §14.2 |
| Task.history | §14.1 |
| agentd.events/SubscribeToEvents | §9.3 |
| FeedKind vocabulary | §9.3 |
| status op document | §7.7 |
| Feed visibility | §7.7 |
| Audit rules | §7.7 |
| OAuth 2.0 authorization server on the A2A listener | §10.1 |
| Session tokens | §10.3 |
| a2a.url | §13.1, §10.1 |
| a2a.cors.origins | §13.1 |
| a2a.device_grant | §13.1, §10.2 |
| a2a.events.enabled / a2a.introspection.enabled | §13.1 |
| a2a.principals[].id and match.aauth_agent removal | §13.1 |
| New agent/observability/security/store keys | §13.1 |
| REMOVED_KEYS (config keys refused by name) | §13.2 |
| REMOVED_LAUNCHER_FLAGS | §11.1 |
| wait {on: message}.from / a2a.wait.from | §14.3 |
| --capabilities manifest a2a section | §4.4 |
| Published extension specifications and schemas | §9.6 |
| Outbound peer client | §15.4 |
| MCP client read semantics (no response cache) | §20 |
| Instruction freshness re-read target and instruction.unavailable | §20 |
| ports::RequestScope | §6.2 |
| Internal bridge verbs | §4.3 |
| HITL availability and policy addressee | §14.3 |
| TS client request surface | §15.1 |
| TS error classification | §15.1 |
| TS discovery and capabilities | §15.2 |
| agentd-tui CLI | §15.3 |
| agentd-ui web surface | §12 |
| Test harness helpers | §16 |
| Thin launcher (agentd tui / agentd ui) | §11.1 |
| Launch grant (single-use launch code, launch sessions) | §11.2 |

## Appendix B. The audit findings this RFC closes

| Id | Finding (short) | Closed in |
|---|---|---|
| C1 | `SendMessage` broke the 1.0 task-id rules | §5.4 |
| C2 | agentd's outbound `send` used the 0.3 role spelling | §15.4 |
| C3 | `ListTasks` ignored every request field | §14.2 |
| C4 | Push-config `Get`/`Delete` without an id acted on arbitrary or all configs | §14.2 |
| C5 | An inline push config on a new task always failed | §5.4, §14.2 |
| C6 | The conformance "disclaimed capability" check passed on any server | §16 |
| C7 | Non-text, non-command parts refused or dropped; `-32005` never used | §5.4 |
| C8 | Outbound delegation never read the peer's card | §15.4 |
| C9 | No standards-only client mode | §15.2 |
| F01 | mTLS + bearer: an unmatched certificate resolved to operator | §7.1 |
| F02 | Ordinary grants reached operator-only ops, incl. minting an operator session | §7.6, §9.2.1 |
| F03 | Any named caller could join another principal's conversation | §7.7 |
| F04 | Command ops acted on any run or subagent by id | §7.7 |
| F05 | `status` returned every principal's objects | §7.7 |
| F06 | `agentd ui` served the root bearer at `/config.js` | §11, §12 |
| F07 | The web UI sent its bearer to any `?endpoint=` and kept it | §11.2, §12 |
| F08 | Pairing: a brute-forceable six-digit code | §10 |
| F09 | 1.0-shaped push credentials were dropped | §14.2 |
| F10 | `Pair` was an undeclared anonymous credential-issuing method | §4.2, §10 |
| F11 | `GetAgentCard` was a non-spec discovery RPC | §4.2, §8.1 |
| F12 | Extension activation had no effect | §9.1, §9.2 |
| F13 | Command results were neither Task nor Message | §9.2.1 |
| F14 | Display-only ops under the command extension, published as skills | §8.3, §9.2.1 |
| F15 | The card declared `protocolVersion` 0.3.0 | §8.3 |
| F16 | No `A2A-Version` negotiation | §5.2 |
| F17 | No `securitySchemes` with `extendedAgentCard: true` | §8.4, §8.5 |
| F18 | The interface URL was the raw bind address | §8.3, §13.1 |
| F19 | HITL worked only with `interface.enabled` | §14.3 |
| F20 | Sends used 0.3 `configuration.blocking` | §15.1 |
| F21 | Client messages omitted `role` | §15.1 |
| F22 | The display clients did no card discovery | §15.2 |
| F23 | The client used extensions without reading the card | §15.2 |
| F24 | Bad credentials got 200 with `-32003`/`-32007` | §5.3 |
| F25 | Per-workflow authorization advertised but not enforced | §7.6 |
| F26 | Unknown methods got `-32003` | §4.1, §5.1 |
| F27 | Undocumented aliases and the `a2a.` prefix strip | §4.2 |
| F28 | The declared-methods guard saw only half the vocabulary | §4.1 |
| F29 | The extensions echo misreported activation | §9.1 |
| F30 | CORS blocked `A2A-Version` and discovery, hid the echo, trusted loopback | §8.2 |
| F31 | The card endpoint answered 200 with `null` on failure | §8.1 |
| F32 | Skills without descriptions | §8.3, §9.2.1 |
| F33 | Workflow commands and `_instance.*` undeclared | §9.2 |
| F34 | Extension URIs 404; no extension specification | §9.6 |
| F35 | Clients depended on undeclared `agentd/*` shapes | §9.4 |
| F36 | Push body was a bare Task; 0.3 token header | §14.2 |
| F37 | Error codes wrong across the two paths | §5.3, §6 |
| F38 | SSE semantics broke §3.1.2 / §9.4.6 | §5.3, §5.4 |
| F39 | Listener auth and CORS configured under the UI section | §13 |
| F40 | The daemon owned the clients' chrome | §13.3 |
| F41 | `Task.history` never populated | §14.1 |
| F42 | Client polling created durable tasks | §9.2.1 |
| F43 | Generic capabilities gated on UI flags | §13.1 |
| F44 | The TS-to-daemon contract test asserted internals | §16 |
| F46 | `ListTasks` pagination ignored by the client | §14.2, §15.2 |
| F47 | The TS SSE parser failed on CRLF | §15.1 |
| F48 | The TS client dropped error information and retried forever | §15.1 |
| F49 | Client auth was Bearer-only through `Pair` | §10, §15 |
| F50 | `/.well-known/agent.json` still routed | §8.1 |
| F51 | Docs and comments contradicted the code | §16 |
| F52 | The public card exposed the operator and debug inventory | §8.3 |
| F53 | The card name was hard-coded | §8.3 |
| F54 | Envelope validation only on the `a2a-rs` path | §5.1 |
| F55 | Feed resume used the highest seen seq | §15.2 (incarnation id: §19) |
| F56 | Audit exclusions hard-coded for one UI | §7.7 |
| F57 | The daemon CLI embedded the Node clients and forced UI config | §11.1 |
| F58 | The client hard-coded agentd configuration knowledge | §15.2 |

There is no F45 in the verified set. The instruction
freshness defect fixed in the same release is §20.
