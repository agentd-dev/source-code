# agent — Consolidated Architecture-Decision Document

**Status:** Architecture decision record. Supersedes the open questions in RFC 0001 §14 and the phased sketch in §15. The 10 `notes-*.md` files in this directory are the supporting detail; this document is the binding synthesis.
**Author:** Lead architect (synthesis of the 10 assessment notes).
**Date:** 2026-06-25.
**Inputs:** `rfcs/0001-mcp-native-agent-runtime.md` + `docs/design/notes-{review,research,mine}-*.md`.
**Target spec baseline:** MCP **2025-11-25** (interoperating down to 2024-11-05).

This document is the decision record an autonomous builder executes against. It is opinionated and final for v1; deferrals are explicit. Where it conflicts with RFC 0001's prose, **this document wins** and the relevant RFC (listed in §3) will be refined to match.

---

## 1. Verdict on RFC 0001 — what holds, what must change, what was wrong

### 1.1 What holds (the thesis is sound)

- **The two-loop split is correct and load-bearing.** A *supervisor* with no LLM dependency, owning lifecycle/triggers/process-tree/limits, plus an *agentic loop* that lives only inside subagent processes. Every reviewer independently endorsed this. Keep it exactly.
- **Process-isolated subagents as an OS process tree.** Crash containment, hard cancel via `SIGKILL`, natural nesting, and free observability (`ps`/`pstree`) all fall out of the OS. The cancellation argument is decisive: the only reliable way to stop runaway model work is to kill the process group — async future-drop cannot do it, which removes tokio's main selling point.
- **MCP as the universal interface, reactivity via resource subscriptions.** The MCP review confirms `resources/subscribe` + `notifications/resources/updated` is idiomatic and spec-supported; the reactive thesis is novel and unbuilt in the ecosystem — it is agent's edge.
- **agent is both MCP client and MCP server** (self-wiring). Sound, with directionality corrections below.
- **The minimalism bar** (no async runtime, single-digit core dependencies, no built-in tools, no policy DSL/signing/auth as core). Confirmed achievable: a default Linux build is ~5–9 first-party crates.
- **Plain ReAct in-core; no plan-execute/ReWOO/reflection baked in.** Validated by the literature (Reflexion's self-critique reinforces blind spots). Express richer patterns via model + subagents, not runtime modes.
- **Capability scoping = granted MCP subset.** Structurally sound, and the right injection defense *when interpreted as a Rule-of-Two trust budget* (see §2.12).

### 1.2 What must change (RFC under-specified the hard parts)

1. **Supervision was a slogan, not a mechanism.** RFC §3/§4.1 says "spawn, track, reap, enforce limits" in one line. The reliability review found **12 concrete gaps** that must be specified: a real reactor (poll/epoll over fds), `SIGPIPE` ignore, zombie/orphan reaping (`PR_SET_CHILD_SUBREAPER` + `waitpid(-1, WNOHANG)`), `PR_SET_PDEATHSIG` on every child, a three-detector dead/stuck model, a bounded kill ladder, a restart governor, fork-bomb caps at the spawn chokepoint, hierarchical token accounting, and a supervisor self-heartbeat. All `std`+`libc`, no new deps. These are decided in §2.9.
2. **The supervisor must be a reactor, not a busy-poll.** "Idle at near-zero cost" requires one blocking wait that wakes on any source. RFC named no I/O primitive. Decided: thread-per-fd + `mpsc` (§2.1).
3. **Dead-vs-stuck detection was entirely missing** (an explicit hard requirement, RFC §1.8). Decided: mandatory hard deadline + no-progress watchdog + ping/pong over a control thread kept separate from the agentic loop (§2.9).
4. **Reactive routing was an open question** (RFC §14.5). Decided: exactly-one-owner first-match routes, spawn-vs-continue as a route property, debounce + coalesce, bounded queues (§2.6).
5. **Termination/goal-checking is the biggest loop hole.** RFC's "final" = "model stopped emitting tool calls." Must become an explicit terminal-status state machine with a named VERIFY phase grounded in tool/exec results, never self-judgment (§2.5).
6. **State recovery on supervisor crash was unaddressed.** Without `PDEATHSIG`, "in-memory only" silently means orphan leak. Decided: PDEATHSIG mandatory + rebuild-and-reconcile on restart (§2.9, §2.10).

### 1.3 What was wrong (MCP-protocol corrections — verified against 2025-11-25)

1. **Notify-then-read.** `notifications/resources/updated` carries only `{uri}` (optionally `title`) — **no payload/diff**. RFC §5.3's "deliver the event / reads what changed" implies the event carries the change. It does not. agent must `resources/read` on wake. The reactive loop is two round-trips and can race → mandatory debounce/coalesce.
2. **Item vs list are distinct mechanisms.** Per-URI `resources/subscribe` → `updated{uri}` is *not* the same as the capability-implied `notifications/resources/list_changed{}` (no subscribe, no uri). RFC conflates them. Treat as two event sources.
3. **You cannot subscribe to a resource *template*.** RFC §5.3's `db://query/...` example is wrong — only concrete URIs are subscribable. To react to "any new row," enumerate concrete URIs (via `resources/list`) and subscribe per-URI, or use `list_changed`.
4. **Transport terminology stale.** "HTTP/SSE" → **Streamable HTTP**; the old HTTP+SSE (2024-11-05) two-endpoint transport is deprecated — never implement it. Receiving notifications over HTTP requires a long-lived SSE GET stream, not "a tiny blocking HTTP client." → **v1 keeps reactivity on stdio only.**
5. **Wrong directionality for intelligence-sharing.** "A peer uses agent's intelligence" = `sampling/createMessage`, a *server→client* request where **sampling is a CLIENT capability**. agent cannot *serve* sampling as an MCP server; it would have to act as a sampling-capable *client*. This is a v2 feature; **v1 declares no client capabilities and does not implement sampling in either direction.**
6. **Self-MCP-over-HTTP cost is understated.** A real Streamable HTTP server needs POST+GET endpoints, `MCP-Session-Id`, `MCP-Protocol-Version`, `Origin`→403, SSE upgrade, resumability. → v1 serves the self-MCP over **stdio / unix-socket only**; HTTP serving is deferred behind a feature.
7. **`isError` vs JSON-RPC `error` is load-bearing** and must be distinguished in the loop: `isError:true` (inside a successful result) → observation fed to the model; JSON-RPC `error` → protocol/transport failure with retry/abort policy.
8. **Two 2025-11-25 features are noted but deferred, not adopted in v1:** **tasks** (durable/pollable requests — the spec-native shape for the *external-facing* long-running surface) and **roots** (filesystem-scope signal). They are the right v2 targets; v1 falls back to request/response + progress + cancel.

---

## 2. DECISIONS

These are made, not surveyed. Each is final for v1 unless a milestone explicitly revisits it.

### 2.1 Concurrency model

**DECISION: thread-per-fd with blocking I/O + `std::sync::mpsc`. No async runtime. `mio`/`libc::poll` held in reserve behind a `serve-mcp` feature for the one high-fan-in case (many idle peer connections on the served self-MCP).**

- One reader thread per long-lived readable stream: each MCP-server stdout, each subagent control-channel stdout, the intelligence connection. Each parses frames and forwards tagged events onto one merged `mpsc` into the single supervisor thread.
- The supervisor owns the state machine and `recv_timeout`s the merged channel — that timeout is also the timer tick (deadlines, intervals, backoff).
- Writes go from the owning thread behind a per-pipe `Mutex<ChildStdin>`; child stdin is `O_NONBLOCK` with a bounded outbound queue (a full queue is itself a stuck signal).
- Signal handlers flip `AtomicBool`s **and** write one byte to a self-pipe (so the reactor wakes promptly); `SA_RESTART` deliberately off so blocked syscalls return `EINTR`.
- **Load-bearing invariant:** *the supervisor never blocks on an untrusted source. It reaches every pipe only via an `mpsc` it `recv_timeout`s, and it unblocks a parked reader by closing/killing the producer, never by interrupting the read* (pipes have no `set_read_timeout`).

Scale check: ~8 MCP servers + 1–50 subagents + 1 intelligence ≈ 60–65 threads / ~130 fds — three orders of magnitude inside OS limits. **tokio rejected** (stated non-goal, scores of crates, reintroduces "one stuck thing starves everything," and doesn't solve the cancel problem which is `SIGKILL`). The retired code already ships proven thread-per-fd prior art.

### 2.2 Dependency budget (core vs feature-gated)

**DECISION:** Default Linux build = single-digit first-party crates, no async runtime, no C/C++ toolchain, no TLS.

**Core (always):**

| Need | Crate | Note |
|---|---|---|
| JSON wire format | `serde` + `serde_json` | Non-negotiable. Runtime tree ~4 tiny crates (itoa, ryu/zmij, memchr, serde_core); derive is build-time only. **Do not hand-roll** (MCP+OpenAI schema surface too large); miniserde is the only sanctioned fallback if the phase-7 audit rejects proc-macro compile weight. Keep all wire types in one module so a swap is mechanical. |
| Unix signals / waitpid / pgroups / prctl / setrlimit | `libc` (raw) | `sigaction`, `setpgid`, `killpg`, `waitpid(WNOHANG)`, `prctl(PR_SET_CHILD_SUBREAPER / PR_SET_PDEATHSIG)`, `setrlimit`. **No `signal-hook`.** `nix` is acceptable *only* as an ergonomic wrapper if it is already pulled by the `vsock` feature; it must not enter the default build solely for signals. Default = raw `libc`. |
| Process spawn / pipes / threads / timers | `std` | Zero crates. |
| Structured logging | hand-rolled JSON-lines | ~150 lines reusing the `serde_json` serializer. **Not `tracing`** in the default build. |
| Interval scheduling | hand-rolled | `Instant`+`Duration` in the `recv_timeout` loop. Zero crates. |
| HTTP/1.1 + SSE client | hand-rolled module | Transport-agnostic over `Read + Write`. Zero crates. The single highest-leverage minimalism decision — avoids the `url`→IDNA→ICU 21-crate tax that `ureq` pulls (52→93 crates measured) and the no-SSE-streaming disqualifier of `minreq`. The repo already ships this prior art. |
| Error types | hand-rolled enums or `thiserror` | `thiserror` is optional and may be dropped for hand-rolled `std::error::Error`. |

**Feature-gated (default OFF):**

| Feature | Crate(s) | Use case |
|---|---|---|
| `tls` | `rustls` (**`ring`** provider, not `aws-lc-rs`) + `webpki-roots` | Direct `https://` intelligence/MCP when TLS is not sidecar-terminated. `ring` avoids the cmake/C build dep; `webpki-roots` for hermetic scratch images. The recommended container shape terminates TLS at a sidecar → most builds link no TLS. |
| `vsock` | `vsock` (blocking, `VsockStream`/`VsockListener`) | Enclave/microVM intelligence transport. ~5 small crates; drops into thread-per-fd. **Never `tokio-vsock`.** |
| `serve-mcp` | `mio` or raw `libc::poll` | High-fan-in served self-MCP listener only. |
| `cron` | `croner`(+`chrono`) originally selected; **shipped hand-rolled, zero-dep** (minimalism moat, see PLAN M4) | Optional cron-expression scheduling. |
| `metrics` | none (hand-written Prometheus text) | `/metrics` on the opt-in HTTP/socket surface. |
| `otel` | originally `tracing`+`opentelemetry-otlp`; **shipped hand-rolled OTLP-over-HTTP/JSON, dependency-free** (PLAN M6) | OTLP export + GenAI semconv. |

**Explicitly OUT:** tokio/async-std/smol; hyper/reqwest; `url` + its ICU/IDNA stack; `aws-lc-rs`/`aws-lc-sys`; native-tls/OpenSSL; `signal-hook`; `tokio-vsock`; the retired regorus/jsonschema/ed25519-dalek/jsonwebtoken/x509/OTLP-grpc/arc_swap/notify/toml stacks.

### 2.3 Control protocol (supervisor ↔ subagent)

**DECISION: a minimal JSON-RPC sibling protocol — NOT literally MCP — sharing the codec with the MCP layer.**

- The channel is JSON-RPC 2.0 shapes over the child's stdio pipes (downward: spawn payload + control: pause/resume/cancel/inject/ping; upward: lifecycle + loop events + usage + final result + pong). It reuses the exact codec but has **no MCP lifecycle** (no `initialize`/capabilities handshake on a private pipe — that would be overkill for a non-discovery link).
- **Framing: length-prefixed (4-byte LE + payload, cap 16 MiB), not NDJSON**, for the control channel. Rationale: control payloads (instructions, context seeds, distilled results) may contain newlines; length-framing is more robust than line-delimited. (MCP transport over stdio stays NDJSON per spec; the two codecs share parse/serialize but differ in framing. Lift `read_frame`/`write_frame` from the retired `intelligence/protocol.rs`.)
- The **control reader inside each subagent runs on a dedicated thread, decoupled from the agentic loop**, so ping/pong liveness survives a long in-flight tool/model call. This is a hard design requirement (§2.9).
- The *external-facing* "spawn a child and await its result" surface is **not** this internal protocol leaked outward — it is exposed as MCP self-tools (and, in v2, MCP tasks). Clean separation: private supervision wire vs public MCP surface.

### 2.4 Intelligence wire format + transports + adapters

**DECISION:**

- **Transports (selected by `AGENT_INTELLIGENCE` URI):** `unix:/path` (sidecar gateway), `https://…` (standalone, behind `tls` feature), `vsock:<cid>:<port>` (enclave, behind `vsock` feature). These carry the LLM wire, **not MCP** — do not conflate. All three drive the same transport-agnostic hand-rolled HTTP/framed client over `Read + Write`.
- **Canonical wire shape: OpenAI-compatible `/chat/completions` with native tool-calling** as the in-binary default. This covers vLLM/Ollama/LM-Studio/most hosted gateways and gives the model first-class `tools` + `tool_calls`.
- **Adapters in-binary: exactly two — `openai-compatible` and `anthropic`.** Both salvaged/adapted from the retired `intelligence/providers.rs` (which already has both dialects + `split_system` + key-safe `Debug` + build-time key probe). The hard bias is *fewer adapters, thinner binary, push provider quirks to the gateway* — gemini/others live behind the gateway, not in the binary. The internal `Request`/`Response`/`Usage` types are widened for tool-calling (net-new work; the retired types have no tool fields).
- **JSON-action fallback:** when a gateway/model lacks native tool-calling, fall back to the retired `{"action":"tool"|"final"}` shape parsed via `extract_json_object` (balanced-brace, prose-tolerant — lift verbatim). Native is primary; JSON-action is the demoted fallback.
- **Credentials:** env/flags only (`AGENT_INTELLIGENCE_TOKEN` + provider-specific), via the retired `secrets::resolve(name)` front door (env + file source kept; command/oauth2 dropped). Never logged, never persisted, never in transcripts. Build-time key probe → fast-fail.

### 2.5 MCP client + server minimal subset (v1)

**DECISION — target MCP 2025-11-25, interop down to 2024-11-05. Pin the version, gate every capability on what the peer advertised, follow pagination cursors on every `*/list`.**

**As CLIENT (to external servers) — v1 MUST:**
- Lifecycle: `initialize` + version negotiation + `notifications/initialized`; store each server's negotiated capabilities.
- Tools: `tools/list` (+ cursor pagination) + `tools/call`; parse `content[]` (text/image/audio/resource/resource_link), `isError`, `structuredContent`; handle `notifications/tools/list_changed`.
- Resources: `resources/list` + `resources/read` (`contents[]` is an array; text + base64 blob).
- **Reactive core:** `resources/subscribe` / `unsubscribe` (gated on `resources.subscribe`); consume `notifications/resources/updated` (URI-only → **notify-then-read**) and `notifications/resources/list_changed`.
- Liveness: `ping` both ways; send `notifications/cancelled` when abandoning an in-flight request.
- Consume `notifications/progress` (reset request timeout, with an absolute ceiling) and `notifications/message` (fold into logs).
- Transport: **stdio** (full line codec, stderr capture, ordered shutdown ladder: close-stdin → SIGTERM → SIGKILL).
- **Declare NO client capabilities** (no roots/sampling/elicitation/tasks). Answer `roots/list` with `{"roots":[]}` and reject `sampling/createMessage` if a server sends them unsolicited.

**As SERVER (agent's self-MCP, RFC §8) — v1 MUST:**
- Lifecycle: answer `initialize` (declare caps), accept `initialized`.
- Tools: expose `subagent.spawn/send/cancel/status`, `subscribe`/`unsubscribe`, `resource.read`, gated `exec`; declare `tools:{listChanged:true}` and emit it when the gated set (e.g. scope narrowing) changes.
- Resources: expose session/run/subagent state as readable + **subscribable** resources; declare `resources:{subscribe:true,listChanged:true}`; emit `notifications/resources/updated` on state transitions (this is what makes agent-to-agent reactivity and async subagent completion work). Use a custom `agent://…` scheme (legal; only other agent instances understand its semantics).
- Liveness: answer `ping`; accept `notifications/cancelled`.
- Transport: **stdio always; unix-socket (NDJSON, stdio-like framing) when `--serve-mcp unix:…`.** Streamable HTTP serving is **deferred** (§2.13).

**DEFER (explicit):** Streamable HTTP resumability/SSE-replay; `prompts/*`; `sampling/createMessage` (both directions); `roots/*`; `elicitation/*`; `completion/*`; **`tasks/*`**; emitting `notifications/message`/`progress` from our server; the old 2024-11-05 HTTP+SSE transport (never).

### 2.6 The three execution modes + time-schedule + reactive routing

**DECISION: one supervisor loop, one inner agentic loop, three drivers differing only by EXIT PREDICATE.** Never fork the daemon and the job into divergent code — this is the load-bearing cloud-native simplification.

| Mode | Exit predicate | Deploy shape |
|---|---|---|
| `once` | first root subagent reaches a terminal status | Job, CLI |
| `loop` | a bound hit (max iterations / global deadline / tree token ceiling) or signal | Job-with-deadline or Deployment |
| `reactive` | never on its own; only signal or fatal/limit | Deployment |
| `schedule` (time) | per-fire identical to `once` | external CronJob (recommended) or internal interval/cron |

**Inner loop (inside subagent):** ReAct turn = assemble request (system + instruction + context seed + transcript + scoped tool catalogue via the provider `tools` field + a compact resource *catalogue*) → call intelligence → record usage/bump budgets → branch (tool calls: scope-check → route → append result/error as observation; final: emit). Tool/exec results are the VERIFY ground truth; never self-judgment.

**Stopping = a disjunction of cheap per-turn checks, each with a distinct terminal status:** `completed` · `exhausted_steps` · `exhausted_tokens` · `deadline` · `stalled` (content-hash unchanged for N turns; default 3) · `loop_detected` (per-tool repeat cap K; default 3) · `cancelled` · `crashed`. The global step/token/deadline cap is non-negotiable (the $47K-runaway lesson; the 92%-cost-overrun finding). At every budget the agent wraps up gracefully and returns partials; RLIMIT/SIGKILL are the backstop for wedged children.

**Errors:** tool-domain errors and malformed model output → observations (recoverable, step-consuming). Transient transport errors → bounded transport-layer retry (backoff+jitter) before surfacing. Fatal infra (intelligence unreachable, auth, OOM, hard budget) → abort with matching terminal status.

**Resources: list AND read.** Inject a compact catalogue (URIs + descriptions + mtime/etag; never bodies); the agent pulls bodies via the `resource.read` self-tool. List = awareness; read = attention. Cap the catalogue; summarize by prefix if a server exposes thousands.

**Context management (lever-ordered):** (1) clear stale tool results (keep last M≈5 verbatim, stub older), (2) compaction at ~75% window (one summarize-and-reinitialize model call), (3) optional `note.write/read` file-backed self-tool. Estimate tokens from the previous response's `usage` + a chars/4 forward heuristic (no tokenizer dependency); compact early/conservatively.

**Reactive routing — the precise rule (RFC §14.5 resolved, agent's edge):**
- A **subscription** is `(server, resource_uri)`. A **route** binds a match (uri-or-glob) to a disposition (`spawn` per-event | `continue(session_id)`) with `{debounce_ms (default 250), queue_cap, overflow}`.
- **Exactly-one-owner:** every `updated{uri}` matches exactly one route by first-match in declared order (exact URI beats glob, longest-prefix first). No fan-out. No match → log + drop + counter.
- **spawn-vs-continue is a route property, deterministic** — not a per-event guess. `spawn` = fresh root subagent templated from the event, bounded by route `max_inflight` (default 4). `continue` = deliver into one warm session, single-consumer FIFO, strict in-order.
- **Self-subscribe = self-scheduling:** when a running agent calls the `subscribe` self-tool, the supervisor auto-creates a `continue(this_session)` route → the agent schedules its own future wake. The signature capability.
- **Debounce + coalesce (default):** collapse a burst on the same URI to one delivery carrying the latest etag (the agent re-reads current state anyway); deliver multi-URI changes to one continue-session as a set. Newest-wins coalesce keeps the queue bounded by distinct watched URIs.
- **At-least-once + idempotent via re-read-current-state.** Notifications can be redelivered; the agent acts on *what the resource is now*, so processing converges (we promise convergence, not exactly-once).
- **On reconnect:** re-subscribe and synthesize one coalesced "possibly changed" event per watched URI (recovers missed updates).

**Time-schedule semantics:** external CronJob → `once` is the **recommended production path** (robust to clock skew/restart, 12-factor). Internal interval (`--interval D`, `D=0` = re-enter immediately) and optional `cron`-feature 5-field cron are **standalone conveniences** — implemented as internal time events fed into the *same* reactive router ("a clock is just another event source"). No second scheduling subsystem; no calendars/DST/job-store in core (default TZ = UTC).

### 2.7 Subagent process model (spawn / scope / result / nesting / caps)

**DECISION:**

- **Same binary re-exec'd** (`argv[0]` in subagent mode) — one artifact, instant `SIGKILL`, OS isolation, the process tree *is* the agent tree.
- **Spawn payload (rich, not minimal):** instruction + **output contract** (objective + required output format + tool/source guidance + boundaries — bare instruction strings reproduce Anthropic's vague-delegation failure) + **narrowed context seed** (only the slices the parent chooses, never the full transcript — context-hygiene *and* injection-firewall win) + tool scope (subset of parent's MCP endpoints/tools; narrows monotonically down the tree) + limits + the `telemetry` block (§2.11). Depth is **minted by the supervisor from the caller's handle, never trusted from the child's request.**
- **Result:** a distilled, structured value (~1–2k tokens) + terminal status + usage. Large outputs use store-and-reference (child writes to a resource, returns a handle). The parent appends the distillate, never the child's raw transcript.
- **Sync-default, async-opt-in:** `subagent.spawn` blocks the parent's turn by default (simplest, deterministic, parent is cheaply paused between turns). `{async:true}` returns a handle (`subagent.status`/`await`, or completion-as-self-resource the parent subscribes to — closing the loop with §2.6). `{detach:true}` fire-and-forget, still budget/depth-counted and reaped. **Sync is the default; `{async}`/`{detach}` shipped in M3** (they share the subscribe/notify machinery).
- **Nesting via the self-MCP only:** a child creates children only by calling back into the supervisor-owned `subagent.spawn` — **exactly one unforgeable chokepoint** for all caps.
- **Caps (finite, conservative defaults — enforced at the chokepoint):** `max_depth` (3–5), `max_children` per node, `max_total_subagents` tree-wide, spawn-rate token-bucket, tree-wide token ceiling. A spawn exceeding any cap is **refused as a tool result** (the parent's model adapts), never a crash.
- **`exec` children are folded into the same regime:** mandatory deadline, process-group kill via the §2.9 ladder, counted against subtree budget + breadth/rate caps. `exec` has no control channel, so only the deadline+kill detectors apply (not ping/pong). Reference implementation: the retired `tools/shell.rs::run()` (reader-threads + try_wait + timeout-kill + signal-extract).

### 2.8 Dead/stuck detection + state recovery + supervision/restart

**DECISION — the three-detector model + the EOF×pong classifier:**

- **Detector A — hard deadline (always on, no child cooperation).** Every child carries an absolute `deadline: Instant`; the reactor's timer is armed to the nearest. **A deadline is mandatory** (default finite, never infinity).
- **Detector B — no-progress watchdog.** Stamp `last_event_at` on every received control frame; `now - last_event_at > progress_timeout` → stuck. Reuses the existing event stream, works even if the child's control thread is also wedged.
- **Detector C — active ping/pong** over the control channel, answered by the child's **dedicated control thread** (separate from the agentic loop). Missing N pongs → wedged/`D`-state. The only detector that distinguishes "busy in a long legitimate tool call" (pongs continue) from "process wedged" (pongs stop).
- **The 2×2 classifier:** EOF = channel closed (likely dead → confirm with `waitpid`); no pong + no EOF = stuck-alive; events flowing = healthy; nothing flowing but pongs flowing = busy-healthy. Write this explicitly into the supervisor.
- **Dead:** `SIGCHLD` → self-pipe → `waitpid(-1, WNOHANG)` **in a loop** (SIGCHLD does not queue); classify clean-exit vs signal/non-zero.

**Kill ladder (bounded, the RFC's "graceful drain → bounded kill" made real):** each subagent in its own process group (`setpgid`). On SIGTERM-to-agent or a deadline/stuck verdict, per target subtree, **depth-first deepest-first** (so a parent can't spawn replacements mid-teardown; set a tree-wide draining flag that makes `subagent.spawn` error): `ctrl:cancel` (graceful) → `killpg(SIGTERM)` after grace (~5s) → `killpg(SIGKILL)` after kill-grace (~2s) → `waitpid` until reaped or `ECHILD`. Total drain budget bounded and **< orchestrator `terminationGracePeriodSeconds`** (default `AGENT_DRAIN_TIMEOUT=25s` vs 30s — the top cloud-native footgun). Second SIGTERM/SIGINT → `force` → collapse to immediate SIGKILL of all groups. Walk the supervisor's own parent-edge table *and* `killpg` per node (covers cooperative + orphaned cases).

**PID-1 / orphan discipline (mandatory):** `prctl(PR_SET_CHILD_SUBREAPER, 1)` (and detect `getpid()==1`) so grandchildren orphaned by a dying subagent reparent to agent, not host init; `waitpid(-1, WNOHANG)` reaps *any* child including unknown PIDs. **`prctl(PR_SET_PDEATHSIG, SIGKILL)` in every subagent's early `main`** so a supervisor crash collapses the tree from the leaves up — without it, "in-memory only" silently means orphan leak. `signal(SIGPIPE, SIG_IGN)` at startup (one line, prevents the supervisor dying when it writes to a just-dead child).

**Restart governor (per-handle, loop/reactive only — never restart a one-shot root):** exponential backoff + jitter (cap), circuit breaker (>N failures in a window → open → mark session failed, surface as a self-MCP resource, drop routed events), crash-on-spawn fast-fail (exit before a `ready` frame within ~2s counts heavier — fork-bomb early warning). Clean completion (exit 0 + `final`) is success, not a breaker failure.

**State recovery:** supervisor is stateless; the spawn payload (instruction + seed + scope + limits + usage) is retained for the child's lifetime as the minimum recoverable unit (enables bounded restart). On supervisor restart: **rebuild + reconcile** — re-read config, re-establish MCP, re-issue every *declared* subscription, and **read-after-subscribe** each (converts edge- to level-triggering across the restart boundary — mandatory, not optional). Warm sessions and dynamic subscriptions are lost in v1 (recovered by idempotent re-trigger). Optional MCP-backed checkpoint of supervisor-owned facts (subscription set + handle map + routing table; atomic write + `fsync` file and dir) is a deferred v2 extension — never checkpoint live agentic context or pipes.

**Hierarchical token accounting:** each subagent reports per-turn `usage{tokens,steps}`; the supervisor (source of truth) adds to the node counter and the tree-root counter (O(1) per event). Node over grant → cancel subtree; root over tree ceiling → drain tree + exit budget code. Per-child `RLIMIT_AS`/`RLIMIT_CPU` via `pre_exec` caps a single runaway child. **Honest caveat: aggregate subtree *memory* is NOT enforced in-binary** — that needs cgroups v2 (`memory.max`/`pids.max`/`cgroup.kill`), a deployment concern; agent is cgroup-v2-*aware* (reads `memory.max`, places the tree in a child cgroup when writable, backpressure on `memory.high`) but **never hard-requires cgroup write access** (rlimit + PDEATHSIG fallback).

### 2.9 Observability (logging + health + metrics + tracing) and the log field schema

**DECISION — default build ships exactly two things: a hand-rolled JSON-lines logger to stderr + a tiny health surface. Everything heavier is feature-gated.**

- **Default logging:** ~150-line `log_event(event, fields)` reusing the `serde_json` serializer (not `tracing` — its implicit async span context is moot for processes-plus-threads, and the process tree gives us context propagation for free). **stdout = the agent's result only; stderr = all telemetry.** One event per line, NDJSON.
- **Canonical line schema (stable, snake_case):**

| Field | Always | Meaning |
|---|---|---|
| `ts` | yes | RFC 3339 UTC |
| `level` | yes | trace/debug/info/warn/error |
| `event` | yes | dotted event type — the primary index key |
| `run_id` | yes | ULID for the whole invocation (the unit of work), stable across the tree |
| `agent_id` | yes | emitting process id (supervisor uses reserved `sup`/`root`) |
| `agent_path` | yes | dotted tree path (`0`, `0.2`, `0.2.1`) — **the cheap superpower:** subtree queries by prefix, no backend join |
| `comp` | yes | `supervisor` \| `agent` \| `mcp` \| `intel` |
| `pid` | yes | joins the log tree to the free OS `pstree` |
| `span_id` / `parent_span_id` | in-span | 8-byte hex |
| `trace_id` | when propagation on | 16-byte hex W3C |
| `dur_ms` | on `*.end`/`*.result` | duration |
| `err` | on errors | `{type, message}` structured |
| event-specific | | `tool`, `server`, `tokens_in/out`, `resource_uri`, `route`, etc. |

- **Closed `event` vocabulary (nail now; renaming breaks dashboards):** supervisor — `proc.start/ready/shutdown/exit`, `config.loaded`, `mcp.connect[.fail]/disconnect`, `trigger.armed/fired`, `subscribe/unsubscribe`, `resource.updated`, `subagent.spawn/exit/signal/stuck/restart`, `limit.exceeded`; agent — `loop.start/step/final/error`, `intel.call/result`, `tool.call/result`.
- **Content capture off by default** (RFC §13 + OTel GenAI stance): log hashes/lengths (`args_hash`, `result_bytes`, `instruction_hash`); `--log-content` opts in (redaction-aware). Secrets never appear (field allowlist).
- **Tree correlation = a `telemetry` block in the spawn payload** (`run_id`, `trace_id`, `parent_span_id`, `agent_path`, `agent_id`, `log_level`, `log_content`). Each child self-logs pre-correlated; collectors reassemble by `run_id` + `agent_path` prefix with no join. Default: each process writes its own stderr (A); `--aggregate-logs` forwards child telemetry up the control channel for single-stream environments (B) — forward, never rewrite correlation fields.
- **Context propagation ON by default** (it's a few fields, and validated by MCP's 2026-07-28 RC adopting SEP-414 W3C trace-context in `_meta`): set `_meta.traceparent`/`tracestate`/`baggage` on outbound MCP calls, the `traceparent` HTTP header on the LLM call, and the `{trace_id,span_id}` in the spawn payload. Ingest `AGENT_TRACEPARENT` or an inbound traceparent; else mint a `trace_id` per `run_id`. **Span *export* (OTLP) is gated; propagation is free.**
- **Health (mode-aware):** one-shot = exit code (the stable table, §2.11). loop/reactive = **supervisor heartbeat liveness only** (a monotonic `last_loop_tick` the reactor bumps every wake *including idle waits* — idle is healthy, a stuck subagent must NOT fail pod liveness) + readiness = MCP-connected and subscriptions ACKed + reconciled. Default surface = exit code + a `--health-file` the supervisor writes each tick (no socket/port; K8s `exec` probe reads it). HTTP `/healthz`+`/readyz` and a unix-socket health line are **opt-in** (reuse the hand-rolled HTTP; off for one-shot).
- **Metrics:** default = derive from logs (closed event vocabulary makes every counter a `count by event`). `metrics` feature = hand-written Prometheus text on the opt-in surface (no client lib). `otel` feature = OTLP + GenAI semconv (`invoke_agent`/`chat`/`execute_tool`, `gen_ai.*`, `mcp.*`); instrument the *client* side of tool calls and propagate so server spans nest (no double-instrument). Cardinality discipline: never put `run_id`/`agent_id`/`call_id`/URIs in metric labels.

### 2.10 Config + signals + exit-code contract

**DECISION:**

- **Config precedence (hard rule, top wins):** `built-in default < config file < env var < CLI flag`. Everything env-settable (12-factor III). The file (`AGENT_MCP_CONFIG`) is only for verbose structural bits (MCP server lists), never for per-environment values, **never for secrets** (env/flag only). **Validate fully at startup before any side effect** — bad config → exit 2 in milliseconds, not after an LLM round-trip. Never read config from the network.
- **Signals:** `SIGTERM`/`SIGINT` → one-way `DRAINING` flag → bounded drain (disarm triggers → wind down subagents at turn boundaries → SIGTERM/SIGKILL stragglers via the §2.8 ladder → flush logs → exit). Second signal → force-SIGKILL. `SIGCHLD` → reap loop. `SIGPIPE` → ignored. (Drop the retired SIGHUP/reload half — restart-to-reload in v1.) `AGENT_DRAIN_TIMEOUT` **MUST be < pod terminationGracePeriodSeconds.**
- **Exit-code contract (a public, machine-actionable API for `podFailurePolicy` — extends the retired `runtime.rs` constants):**

| Code | Meaning | Scheduler hint |
|---|---|---|
| 0 | success (one-shot completed / clean drain) — a clean SIGTERM drain returns **0, not 143** | Complete |
| 1 | generic/unspecified failure | retriable |
| 2 | config/usage error (validation) | **non-retriable** (FailJob) |
| 3 | partial result | policy (default retriable) |
| 4 | intelligence unreachable/auth after retries | retriable |
| 5 | semantic — agent concluded task cannot be done/refused | **non-retriable** |
| 6 | required MCP server failed to connect/handshake/died | retriable |
| 7 | budget exceeded (steps/tokens/deadline/tree) without result | policy (usually raise budget) |
| 124 | hard wall-clock deadline (mnemonic to `timeout(1)`) | — |
| 137 / 143 | killed by SIGKILL / SIGTERM (128+signal, OS-set) | OOM ⇒ raise memory |

One-shot maps the root subagent's terminal status to a code (completed→0, refused→5, partial→3, budget→7). loop/reactive daemons exit only 0 (clean drain), 143 (ungraceful), or a fatal class (4/6/137). Treat changes as breaking.

- **Idempotency:** accept `AGENT_RUN_ID`/`--run-id` (default per-process ULID); propagate it into every MCP tool-call `_meta` so backing services can dedupe retries; encourage read-modify-write-through-MCP and make "already done" cheap → exit 0. agent introduces no local non-idempotent side effects (all durable output externalized through MCP backing services — it has no built-in tools, so this falls out structurally).

### 2.11 Security posture

**DECISION — minimalism + structural isolation is the moat; no policy engine, no signing, no auth as core (a conscious reversal of the retired design's "governance is the moat").**

- **Outer boundary** (container/VM/enclave) is the sandbox; agent does not reimplement sandboxing.
- **Capability scoping = granted MCP subset, interpreted as a Rule-of-Two trust budget.** A subagent's scope narrows monotonically down the tree. The supervisor/parent can tag tools (`untrusted_input` / `sensitive` / `egress`) and **warn or refuse** a grant that hands one subagent all three legs of the lethal trifecta without an explicit override. Process isolation = CaMeL-style separation in practice: an untrusted-content reader (no sensitive/egress tools) returns a distilled structured summary to a parent that holds sensitive tools but never sees raw untrusted content — the distilled return doubles as an injection firewall.
- **Treat ALL MCP server content as untrusted — including tool descriptions/schemas/annotations** (tool poisoning / ASI01). Do not auto-trust server metadata; surface/log tool descriptions for operator audit. An MCP server definition = trusting that command; never build launch commands from model/server-controlled strings. stdio is the default transport (limits server access to agent only).
- **SSRF defenses in the hand-rolled HTTP client:** enforce HTTPS in prod, **block RFC-1918 / loopback / link-local (169.254/16) by default**, validate redirects (don't blindly follow cross-host), pin DNS where feasible, explicit localhost opt-out for dev. (Salvage the retired CR/LF-injection-rejecting header construction.)
- **`exec` off by default**, capability-checked (binary must exist → absent, not a runtime error), isolated under the same OS limits + kill ladder; the strongest trifecta leg, so an `exec`-scoped subagent should be the one least exposed to untrusted content.
- **Self-MCP over HTTP needs spec hardening** (non-deterministic session IDs, no sessions-as-authn, no token passthrough, Origin/403, loopback) — which is why v1 prefers **stdio/unix** for serving and defers HTTP.
- **Secrets:** env/flag only, `resolve()` front door, never logged/persisted/in-transcript, `Debug` prints `***`.

### 2.12 Summary of decisions (bullet list)

- **Concurrency:** thread-per-fd + blocking I/O + `std::sync::mpsc`; one supervisor reactor on `recv_timeout`; signals via self-pipe. tokio rejected. `mio`/`poll` only behind `serve-mcp`.
- **Dependencies:** core = `serde`/`serde_json` + raw `libc` + `std` + hand-rolled (logger, HTTP/1.1+SSE client, framing). Feature-gated: `tls` (rustls/ring/webpki-roots), `vsock`, `serve-mcp`, `cron` (hand-rolled, zero-dep), `metrics`, `otel` (hand-rolled OTLP-over-HTTP/JSON, dependency-free). No async runtime, no C toolchain, no `url`/ICU in default.
- **Control protocol:** minimal JSON-RPC sibling (not literal MCP), length-framed, shared codec; control reader on a thread separate from the agentic loop.
- **Intelligence:** OpenAI-compatible `/chat/completions` + native tool-calling canonical; exactly two in-binary adapters (openai-compatible + anthropic); JSON-action fallback; transports unix/https(tls)/vsock; creds env/flag only.
- **MCP:** target 2025-11-25, capability-gated, cursor-paginated; client + self-server minimal subset; reactivity on stdio only in v1; notify-then-read; item-vs-list distinct; no template subscribe; defer tasks/sampling/roots/HTTP-serving.
- **Modes:** one loop, three exit predicates (once/loop/reactive/schedule); time-schedule external-by-default; reactive routing = exactly-one-owner first-match routes, spawn-vs-continue as a route property, debounce+coalesce, bounded queues, at-least-once + re-read-current-state; self-subscribe = self-scheduling.
- **Subagents:** same-binary re-exec; rich spawn payload (output contract + narrowed seed + scope + limits + telemetry); distilled result; sync-default/async-opt-in (async shipped in M3); nesting only via supervisor-owned `subagent.spawn`; finite depth/breadth/rate/tree-token caps refused as tool results; depth minted by supervisor.
- **Reliability:** three-detector dead/stuck (deadline + no-progress + ping/pong) + EOF×pong classifier; PID-1 subreaper + waitpid loop; PDEATHSIG on every child; SIGPIPE ignored; bounded depth-first kill ladder, drain < grace, second-signal force; restart governor (backoff+breaker+fast-fail); rebuild+reconcile (read-after-subscribe mandatory); hierarchical token accounting to root; cgroup-aware not cgroup-required.
- **Observability:** default = hand-rolled JSON-lines to stderr + exit code + health file; closed event vocabulary + correlation tuple (`run_id`/`agent_path`/`pid`); context propagation (W3C `_meta`/header/spawn) on by default; metrics-from-logs default, Prometheus/`otel` gated.
- **Config/signals/exit:** precedence built-in<file<env<flag; validate-at-startup→exit 2; bounded drain; public exit-code table; clean drain = 0; RUN_ID idempotency into `_meta`.
- **Security:** granted-MCP-subset as Rule-of-Two trust budget; all server content untrusted; SSRF defenses; exec off by default; stdio/unix self-MCP; no policy/signing/auth core.

---

## 3. FEATURE → RFC MAP

RFC 0001 is **refined** (not replaced) into the core architecture RFC; each major feature area gets its own numbered RFC so they can be reviewed and built independently.

| RFC | Title | One-line scope |
|---|---|---|
| **0001** (refine) | MCP-native agent runtime — core architecture | The supervisor/agentic two-loop split, thesis, non-goals, deployment shapes; updated to absorb every decision here (notify-then-read, Streamable HTTP naming, PDEATHSIG, reactor, terminal-status state machine). |
| **0002** | Supervisor reactor & concurrency model | Thread-per-fd + `mpsc` reactor, self-pipe signals, the abandon-don't-interrupt invariant, the per-child supervision record, timer/deadline arming. |
| **0003** | Process supervision, dead/stuck detection & recovery | Three-detector model + EOF×pong classifier, PID-1 subreaper + waitpid loop, PDEATHSIG, bounded kill ladder, restart governor, rebuild+reconcile, hierarchical token accounting, cgroup-awareness. |
| **0004** | MCP client subset & wire codec | Target 2025-11-25, capability gating, pagination, tools/resources/subscribe, notify-then-read, item-vs-list, ping/cancel/progress, stdio transport + shutdown ladder, shared JSON-RPC codec. |
| **0005** | Self-MCP server & control protocol | The self-MCP tool/resource surface (`subagent.*`, `subscribe`, `resource.read`, gated `exec`), subscribable `agent://` state resources, stdio/unix serving; the length-framed JSON-RPC supervisor↔subagent control channel. |
| **0006** | Intelligence transport & wire format | unix/https(tls)/vsock transports, OpenAI-compatible + anthropic in-binary adapters, native tool-calling request/response + usage, JSON-action fallback, credential handling. |
| **0007** | Agentic loop & terminal-status state machine | ReAct turn, the stop-condition disjunction with distinct statuses, VERIFY grounded in tool/exec, error taxonomy, malformed-output recovery, context compaction levers, resource list-vs-read. |
| **0008** | Execution modes, triggers & reactive routing | one/loop/reactive/schedule as exit predicates over one loop; the routing rule (exactly-one-owner, spawn-vs-continue, debounce/coalesce, backpressure, ordering, self-subscribe); internal interval/cron as event sources. |
| **0009** | Subagent process model & nesting | Re-exec subagent mode, rich spawn payload + output contract, narrowed seed, distilled result, sync/async/detach, tool scope, depth/breadth/rate/tree-token caps, the single spawn chokepoint. |
| **0010** | Observability, health & telemetry | The JSON-lines logger, line schema + closed event vocabulary, correlation tuple + spawn telemetry block, W3C context propagation, mode-aware health, metrics-from-logs, gated `metrics`/`otel`. |
| **0011** | Cloud-native contract: config, signals, exit codes, idempotency | Config precedence + validate-at-startup, drain choreography + `AGENT_DRAIN_TIMEOUT`<grace, the exit-code table, RUN_ID idempotency, statelessness, cgroup friendliness. |
| **0012** | Security posture | Granted-MCP-subset as Rule-of-Two trust budget, untrusted-server-content stance, SSRF defenses, gated `exec`, self-MCP hardening, secrets handling. |
| **0013** (deferred) | v2 surface: MCP tasks, sampling, roots, Streamable HTTP serving, session checkpointing | The explicit defer list — durable/pollable external surface (tasks), intelligence-sharing (sampling-as-client), roots, HTTP serving + SSE, MCP-backed warm-session checkpoint. |

---

## 4. PHASED BUILD PLAN

Milestones are ordered, each independently shippable and acceptance-tested. The workspace/crate/module layout the new code creates is given first; each milestone names the modules it lands.

### 4.0 Proposed workspace / crate / module layout

Keep the existing 2-crate workspace; the retired `crates/agentd/src` tree is gutted and rebuilt. (Adjust `[profile.release]` comment away from "workflow runtime"; keep `panic="abort"`, LTO, strip, `opt-level="s"`.)

```
crates/
  agent/                      # the single binary (CLI + daemon + subagent re-exec)
    src/
      main.rs                  # arg parse → mode dispatch (supervisor vs subagent re-exec)
      config.rs                # precedence (built-in<file<env<flag), validate-at-startup, exit 2
      exit.rs                  # the public exit-code table + terminal-status→code mapping
      json/                    # shared JSON-RPC 2.0 codec (serde types) — wire types in ONE module
        mod.rs                 #   (swap-to-miniserde isolation point)
        frame.rs               # NDJSON (MCP stdio) + length-prefix (control channel) framing
      wire/
        mcp.rs                 # MCP request/result/notification types (2025-11-25), capability map
        intel.rs               # intelligence Request/Response/Usage (+ tool-calling fields)
      net/
        http.rs                # hand-rolled HTTP/1.1 + SSE client over Read+Write (+ SSRF guards)
        tls.rs                 # rustls/ring wiring                          [feature: tls]
        unixsock.rs            # UnixStream transport
        vsock.rs               # VsockStream transport                       [feature: vsock]
      mcp/
        client.rs              # reader-thread + pending-request map + notification dispatch
        registry.rs            # name→server-handle map; resolve(); per-server caps cache
        config.rs              # --mcp name=cmd parsing; --mcp-config FILE
        server.rs              # self-MCP server (tools/resources); stdio + unix  [serve-mcp for poll]
      intel/
        client.rs              # transport selection (unix/https/vsock) + request timeout
        openai.rs              # openai-compatible adapter (+ native tool-calls)
        anthropic.rs           # anthropic adapter
      loop/
        agent.rs               # the ReAct loop (subagent side)
        stop.rs                # terminal-status disjunction + content-hash/no-progress/repeat-cap
        context.rs             # transcript + compaction levers + resource catalogue
        action.rs              # native tool-call dispatch + JSON-action fallback parser
      supervisor/
        reactor.rs             # the single poll/recv_timeout loop; merged mpsc; timers
        tree.rs                # Child records, parent edges, depth, budgets
        spawn.rs               # re-exec subagent spawn; setpgid; pre_exec rlimit+PDEATHSIG
        reap.rs                # SIGCHLD waitpid loop; subreaper; classify exit
        liveness.rs            # deadline + no-progress + ping/pong; EOF×pong classifier
        kill.rs                # bounded depth-first SIGTERM→SIGKILL ladder; drain budget
        restart.rs             # backoff + jitter + circuit breaker + crash-on-spawn
        budget.rs              # hierarchical token/step accounting to tree root (salvage CAS tracker)
      triggers/
        mode.rs                # once/loop/reactive/schedule drivers (exit predicates)
        router.rs              # reactive routing: routes, exactly-one-owner, debounce/coalesce, queues
        timer.rs               # interval + cron event source                [cron: hand-rolled, feature]
      subagent/
        control.rs             # control-channel reader thread (decoupled from loop) + ping/pong
        protocol.rs            # spawn payload, control messages, upward events, result
      obs/
        log.rs                 # hand-rolled JSON-lines logger + LogCtx + closed event vocab
        health.rs              # heartbeat, --health-file, /healthz+/readyz   [http surface opt-in]
        trace.rs               # W3C context propagation (default) + OTLP export [feature: otel]
        metrics.rs             # atomic counters → Prometheus text            [feature: metrics]
      sec/
        secrets.rs             # resolve(name) env/file front door; Debug=***
        scope.rs               # tool-scope grant + Rule-of-Two tag check
        exec.rs                # gated exec tool (reader-threads + timeout-kill + signal-extract)
      signals.rs               # sigaction (no SA_RESTART) + self-pipe; SIGTERM/INT/CHLD/PIPE
  agentd-conformance/          # MCP client+server conformance + supervisor behavior tests
```

Cargo features: `default = []`; `tls`, `vsock`, `serve-mcp`, `cron`, `metrics`, `otel`.

### M1 — Skeleton: config, one-shot, one MCP server, the loop, budgets

**Deliverables:** single binary; `config.rs` (precedence + validate-at-startup → exit 2) + `exit.rs`; `json/` codec + `wire/mcp.rs` + `wire/intel.rs` (salvage + extend the retired protocol types with tool-calling fields); `net/http.rs` hand-rolled HTTP/1.1 client (consolidate the two retired copies) + `net/unixsock.rs`; `intel/` with the openai-compatible adapter + native tool-calling over `unix:` and `https://` (tls feature); `mcp/client.rs` connecting **one** stdio MCP server (reader-thread + pending-map from the start, even though notifications are unused yet) with `tools/list`+`tools/call`+`resources/list`+`resources/read`; `loop/` ReAct loop with the §2.6 stop-condition disjunction + terminal statuses; `supervisor/budget.rs` (token/step/deadline, salvage the CAS tracker); `obs/log.rs` JSON-lines logger + the line schema; `signals.rs` (SIGTERM/INT/PIPE).
**Module layout created:** `main.rs config.rs exit.rs json/ wire/ net/{http,unixsock,tls} intel/ mcp/{client,registry,config} loop/ supervisor/budget.rs obs/log.rs sec/secrets.rs signals.rs`.
**Acceptance:** `agent --mode once --instruction … --intelligence https://… --mcp fs=…` runs instruction → loop → real MCP `tools/call` → prints result on stdout, JSON events on stderr; exit code maps terminal status (0/3/5/7/124); a missing/invalid flag exits 2 in <50ms; a step/token/deadline cap produces a labeled partial, not a hang; `isError:true` becomes an observation while a JSON-RPC error aborts per policy.

### M2 — Subagent processes: the supervised tree

**Deliverables:** `supervisor/reactor.rs` (the merged-`mpsc`/`recv_timeout` loop) + `tree.rs`; `supervisor/spawn.rs` (re-exec subagent mode; `setpgid`; `pre_exec` rlimit + **PDEATHSIG**); `subagent/{control,protocol}.rs` (length-framed control channel; control reader on a **separate thread**; ping/pong); `supervisor/reap.rs` (SIGCHLD self-pipe + `waitpid(-1,WNOHANG)` loop + `PR_SET_CHILD_SUBREAPER` + PID-1 detect); `supervisor/liveness.rs` (three detectors + EOF×pong classifier); `supervisor/kill.rs` (bounded depth-first ladder + drain budget + second-signal force); `supervisor/restart.rs`; `mcp/server.rs` self-MCP (stdio) exposing `subagent.spawn/send/cancel/status` (sync); `sec/scope.rs` tool-scope grant; depth/breadth/rate caps at the spawn chokepoint with supervisor-minted depth.
**Module layout created:** `supervisor/{reactor,tree,spawn,reap,liveness,kill,restart}.rs subagent/ mcp/server.rs sec/scope.rs`.
**Acceptance:** a parent spawns a scoped child that runs its own loop and returns a distilled result up the control channel; `kill -STOP` a child → no-progress + missing pongs → supervisor declares stuck and runs the ladder to SIGKILL within the drain budget; a child that exits is reaped (no zombie) and an orphaned grandchild reparents to agent and is reaped; killing the supervisor collapses the tree via PDEATHSIG; a spawn past `max_depth`/`max_children` is refused as a tool result, not a crash; a crash-looping child trips the breaker and is marked failed.

### M3 — Reactivity: subscriptions, routing, warm sessions, async subagents

**Deliverables:** `mcp/client.rs` notification dispatch wired to `triggers/router.rs`; `resources/subscribe`/`unsubscribe` + consume `updated`/`list_changed` (capability-gated); `triggers/router.rs` (routes, exactly-one-owner first-match, spawn-vs-continue, debounce+coalesce, bounded queues, FIFO single-consumer per session); warm-session state in `supervisor/tree.rs`; the `subscribe`/`unsubscribe` + `resource.read` self-tools (self-subscribe → auto continue-route = self-scheduling); async `subagent.spawn{async,detach}` + completion-as-self-resource; rebuild+reconcile (read-after-subscribe) on (re)start.
**Module layout created:** `triggers/{router,mode,timer}.rs`; extends `mcp/{client,server}.rs`, `supervisor/tree.rs`.
**Acceptance:** `--mode reactive --subscribe file://…` idles at near-zero CPU and wakes on `notifications/resources/updated`, then `resources/read`s the changed URI; a burst on one URI coalesces to one wake; an event with no matching route is dropped+counted; a self-subscribing agent ends its turn and is re-entered in the same session when its resource updates; a restart re-subscribes and read-after-subscribe re-fires any change missed while down; an async subagent returns a handle and its completion arrives as a subscribable resource update.

### M4 — Composition, transports, exec, schedule

**Deliverables:** serve the self-MCP over `unix:` (`--serve-mcp unix:…`) for peer/parent clients (stdio already works); `net/vsock.rs` + vsock intelligence transport (feature); `sec/exec.rs` gated `exec` self-tool folded into the kill ladder + budgets + caps; `triggers/timer.rs` internal interval (`--interval`) + optional `cron` feature (shipped hand-rolled, zero-dep) as router event sources; `--mode loop`/`schedule` drivers.
**Module layout created:** `net/vsock.rs sec/exec.rs`; extends `mcp/server.rs`, `triggers/{mode,timer}.rs`.
**Acceptance:** a second agent connects to the served unix self-MCP, subscribes to an `agent://session/…` resource, and reacts to the first agent's progress; `--enable-exec` exposes `exec` only when the binary exists, runs under a mandatory deadline, and is killed+reaped by the subtree ladder; `--mode loop --interval 5m` re-enters on the timer with idle backoff and terminates on the global budget; vsock intelligence works inside a microVM.

### M5 — Cloud-native hardening: drain, health, exit codes, idempotency

**Deliverables:** full drain choreography in `signals.rs`+`supervisor/kill.rs` with `AGENT_DRAIN_TIMEOUT`<grace; `obs/health.rs` (supervisor heartbeat + `--health-file`; opt-in `/healthz`+`/readyz` via the hand-rolled HTTP); the complete exit-code table in `exit.rs`; RUN_ID propagation into MCP `_meta`; cgroup-v2 awareness (read `memory.max`, optional child-cgroup placement + `cgroup.kill`, `memory.high` backpressure, never required); supervisor self-watchdog.
**Module layout created:** `obs/health.rs`; extends `signals.rs supervisor/{kill,reap}.rs config.rs`.
**Acceptance:** SIGTERM drains within budget and exits **0** (not 143); a second SIGTERM forces immediate kill; the health file goes stale only when the *supervisor* loop wedges (a stuck subagent does not flip liveness); each exit code matches the table; a retried run with a stable RUN_ID detects "already done" via a backing MCP service and exits 0 cheaply; on a cgroup-writable host the whole tree is reaped by `cgroup.kill`.

### M6 — Observability depth + security tags

**Deliverables:** W3C context propagation by default (`_meta`/HTTP header/spawn telemetry block) in `obs/trace.rs`; the closed event vocabulary fully emitted across supervisor + agent; `--aggregate-logs` (mode B forwarding); `--log-content` redaction-aware capture; `sec/scope.rs` Rule-of-Two tag check (warn/refuse trifecta grants); SSRF guards in `net/http.rs`; `metrics` feature (Prometheus text); `otel` feature (OTLP + GenAI semconv, HTTP exporter).
**Module layout created:** `obs/{trace,metrics}.rs`; extends `obs/log.rs sec/scope.rs net/http.rs`.
**Acceptance:** a trace started upstream flows through agent into MCP `_meta` and the LLM header and into child processes, reassembling as one tree by `run_id`+`agent_path`; a scope grant giving one subagent untrusted-input+sensitive+egress is refused without `--allow-trifecta`; the HTTP client refuses RFC-1918/link-local targets by default; `--features metrics` serves valid Prometheus text; `--features otel` exports `invoke_agent`/`chat`/`execute_tool` spans with `gen_ai.*` attributes.

### M7 — Minimalism audit + conformance + release

**Deliverables:** `cargo tree -e normal` + `cargo audit`/`cargo deny` pass confirming single-digit core; cut any dependency that didn't earn its place (revisit hand-roll-vs-`minreq`, `thiserror`-vs-hand-rolled, miniserde decision); `agentd-conformance` MCP client+server conformance + supervisor behavior + record/replay tests (record/replay tool transport for router debugging without model spend); minimal container image (scratch/distroless, TLS-off default); docs: exit-code table, config table, event vocabulary, the security/trifecta guidance, deployment recipes (CLI / reactive Deployment / external CronJob).
**Module layout created:** fills `agentd-conformance/`; finalizes feature matrix.
**Acceptance:** default build links no async runtime, no TLS, no C toolchain, ≤ single-digit first-party crates; the conformance suite passes against the MCP reference servers and an agent-as-server peer; a stuck/orphan/fork-bomb chaos test leaves no leaked process; the runtime is genuinely readable in an afternoon (size + module-count check).

---

## 5. TOP RISKS + mitigations

1. **Reactivity over HTTP is materially harder than RFC implied (SSE client + server).** *Mitigation:* v1 keeps reactivity on **stdio only**; reactive-over-HTTP (SSE GET client) and self-MCP-over-HTTP (full Streamable HTTP server) are explicitly deferred (RFC 0013). Honest minimalism, not a gap.
2. **Notify-then-read race + chatty resources → spawn storms.** *Mitigation:* mandatory per-route debounce + newest-wins coalesce + bounded queues + at-least-once-via-re-read-current-state; the agent acts on current state, not deltas, so redelivery is safe (§2.6).
3. **Supervisor crash → orphaned subagents burning tokens / holding MCP connections / running `exec` side effects** — the worst leak in the design. *Mitigation:* `PR_SET_PDEATHSIG` on every child + self-enforced child deadline + cgroup `cgroup.kill` teardown where available; PID-1 subreaper keeps the whole tree in agent's reaping domain (§2.8).
4. **`D`-state / wedged children can't be killed even by SIGKILL.** *Mitigation:* the three-detector model *detects and reports* it (stuck-leak metric + distinct unclean-drain exit code) rather than hanging; the supervisor never blocks on the source (abandon-don't-interrupt invariant).
5. **Drain budget ≥ pod terminationGracePeriodSeconds → kubelet SIGKILLs us mid-drain** (the top cloud-native footgun). *Mitigation:* `AGENT_DRAIN_TIMEOUT` defaults to 25s with a documented, loudly-warned coupling that it MUST be < the pod grace (rec 30s); validated at startup.
6. **Fork-bomb / runaway recursion in a model-owned loop.** *Mitigation:* one unforgeable chokepoint (`subagent.spawn` is the only spawn path), supervisor-minted depth, finite conservative defaults (depth 3–5, breadth, tree-token ceiling, spawn-rate bucket), `exec` folded into the same caps; spawns refused as tool results.
7. **Prompt injection via arbitrary MCP servers (the lethal trifecta) — unsolved industry-wide.** *Mitigation:* granted-MCP-subset as a Rule-of-Two trust budget (warn/refuse trifecta grants), process isolation + distilled structured returns as an injection firewall, all server content treated as untrusted (incl. tool descriptions), SSRF defenses, `exec` off by default. Honest framing: structural isolation, not a guarantee.
8. **`serde_json` proc-macro compile weight / the one big dependency.** *Mitigation:* keep all wire types in one module so a swap to miniserde/hand-rolled is mechanical; the phase-7 audit holds the final go/no-go. Runtime tree is already ~4 tiny crates.
9. **GenAI OTel semconv is experimental (attribute names may shift).** *Mitigation:* the default JSON line schema is *ours* and stable; the otel mapping is isolated in one feature-gated module; gate the experimental opt-in explicitly.
10. **Two half-MCP dialects (control channel vs real MCP) drifting.** *Mitigation:* share the JSON-RPC codec but keep the control channel a deliberately minimal sibling (no lifecycle); expose external supervision only as MCP self-tools (and v2 tasks), never by leaking the internal protocol outward.
11. **Premature-completion / confidently-spinning silent failure.** *Mitigation:* named VERIFY phase grounded in tool/exec results (never self-judgment), `stalled` content-hash detector, per-tool repeat cap K, distinct terminal statuses + exit codes so "capped" ≠ "completed".
12. **Token-accounting ceiling assumed to cover memory (it doesn't).** *Mitigation:* state explicitly that only the *token* ceiling is in-binary; aggregate subtree *memory* needs cgroups (deployment layer); per-child `setrlimit` caps a single runaway; document so operators size `resources.limits` correctly.

---

*End of architecture-decision document. The 13 RFCs in §3 are the next authoring phase; M1–M7 in §4 are the build sequence. Every mechanism here is `std`+`libc`+hand-rolled or a feature-gated, justified crate — consistent with the minimalism bar that is the moat.*
</content>
</invoke>
