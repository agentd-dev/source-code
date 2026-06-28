# Design Review — Concurrency Model & Dependency Minimalism

**Reviewer lens:** the pivotal architecture decision — how the supervisor
multiplexes many I/O sources while staying minimal and dependency-light.
**Target:** RFC 0001 (`rfcs/0001-mcp-native-agent-runtime.md`).
**Status:** durable design note; input to the architecture synthesis.
**Date:** 2026-06-25.

---

## 0. TL;DR (the recommendation up front)

1. **Concurrency model: thread-per-fd with blocking I/O + `std::sync::mpsc`,
   with `mio` held in reserve as a feature-gated fallback for the one part
   that genuinely needs readiness-multiplexing (the served self-MCP / many
   idle peer connections).** This is option **(A)** as the baseline, with a
   surgical, optional dose of **(B)** only where fan-in count gets large.
   **Reject tokio (C) as the core model.** At the expected scale (a handful
   of MCP servers, 1–50 subagent processes, 1 intelligence connection), the
   thread/FD arithmetic is trivially within OS limits, the failure-handling
   and cancellation story is *better* (a stuck pipe is a thread you abandon
   and a process you `SIGKILL`, not a task wedged in a shared executor), and
   the dependency cost is ~zero.

2. **JSON: `serde_json`.** Do not hand-roll, do not use a micro-JSON crate.
   It is the one non-negotiable dependency and it pulls a tiny, audited,
   universally-trusted tree (`itoa`, `memchr`, `ryu`/`zmij`, `serde_core`).
   Hand-rolling a correct JSON-RPC + LLM-wire parser is a liability, not a
   saving.

3. **HTTP/1.1 client: hand-roll it** (the codebase already has proven prior
   art — `intelligence/client.rs` and `triggers/http.rs` speak HTTP/1.1 and
   SSE over raw `TcpStream`). This is the single biggest minimalism win,
   because the off-the-shelf clients drag in a URL/IDNA/ICU stack that
   dwarfs the runtime. If a dependency is preferred over hand-rolling,
   **`minreq` (rustls)** is the minimal pick — but it **cannot stream SSE**,
   which is disqualifying for streaming token output; **`ureq` 3.x** streams
   but pays the `url`→ICU tax. See §5 for the hard numbers.

4. **TLS: `rustls` with the `ring` provider, behind a feature flag, default
   OFF.** The recommended container topology terminates TLS at a
   sidecar/gateway and keeps `agent` plaintext-to-localhost — so most
   builds carry **no TLS at all**. When compiled in, prefer **`ring`** over
   `aws-lc-rs` to avoid the C toolchain / `aws-lc-sys` build dependency.

5. **vsock: the `vsock` crate (blocking, `rust-vsock/vsock`).** Tiny tree
   (`libc`, `nix`, `bitflags`, `cfg-if`, `memoffset` — 5 crates), `std`-net-
   shaped blocking API (`VsockStream`/`VsockListener`) that drops straight
   into the thread-per-fd model. Feature-gated. **Do not** pull
   `tokio-vsock` (it exists only to bridge into an async runtime we are
   rejecting).

6. **Signals: raw `libc::sigaction` writing `AtomicBool`s** (already in the
   parts bin, `src/signals.rs`). No `signal-hook`, no `nix` *for signals*.

7. **Process control: `std::process` + `libc`** for `setpgid`/`killpg`,
   `waitpid`, and `SIGKILL`-the-subtree. No extra crate.

The net dependency budget (§9) for a default Linux build is **single digits
of first-party crates**, with TLS/vsock/streaming-HTTP each opt-in.

---

## 1. What the runtime actually has to multiplex

From RFC §§3–8, a single `agent` process (the supervisor) must concurrently
service:

| Source | Count (expected) | Shape | Liveness concern |
|---|---|---|---|
| MCP server connections (stdio) | ~1–8 | child stdin/stdout pipes, NDJSON JSON-RPC | server hangs mid-call; emits async `notifications/resources/updated` |
| MCP server connections (HTTP/SSE) | 0–few | TCP/TLS socket, request/response + SSE | network stall |
| Subagent child processes | 1–50 (bounded tree) | child stdin/stdout control pipes, JSON lines | runaway loop, deadlock, crash |
| Intelligence connection | 1 | HTTP/TLS or unix or vsock, request + **optional SSE stream** | slow/streaming, can stall mid-token |
| Resource-update notifications | async, sporadic | arrive *on* the MCP server pipes above | the reactive trigger — must not be missed |
| Timers | a few | interval / deadline / loop tick | — |
| OS signals | SIGTERM/SIGINT/SIGHUP/SIGCHLD | async-signal context | must drain + kill tree |
| Served self-MCP (optional) | 0–N peer clients | unix/vsock/HTTP listener | only when `--serve-mcp` |

Two structural facts dominate the decision:

- **The scale is small and bounded.** This is explicitly "one agent unit"
  (RFC §2: "one agent and its subagent tree per process tree; scale by
  running more instances"). We are *not* building a server that fans out to
  thousands of connections. The C10k problem does not exist here.
- **The hard part is not throughput, it is `liveness`** — RFC hard
  requirement (8): "detect dead/stuck subprocesses, recover state, and stay
  stable." The concurrency model is chosen for *robust supervision of a few
  things*, not for *efficient juggling of many*.

A second structural fact, easy to miss: **MCP `notifications` make each MCP
server pipe full-duplex and server-initiated.** The retired code's MCP
client (`src/mcp/client.rs`) was strictly request/response with a single
`Mutex` serialising the pipe — it has *no* path for an unsolicited
`notifications/resources/updated` to arrive while no request is in flight.
The new design's signature feature (reactive subscriptions, RFC §5.3) breaks
that assumption: a reader must be *always* listening on every subscribed MCP
pipe, independently of whether a `tools/call` is outstanding. This single
requirement is what forces a real concurrency model and rules out the old
"one Mutex, one blocking round-trip" shortcut.

---

## 2. The three options, scored

### (A) Thread-per-pipe / -process, blocking I/O, `std` channels

**Shape.** One OS thread per long-lived readable stream:
- one reader thread per MCP-server stdout (parses NDJSON, classifies each
  frame as response-to-request vs. notification, forwards both onto an
  `mpsc` into the supervisor);
- one reader thread per subagent stdout (control-channel events);
- writes happen from the supervisor/owning thread behind a per-pipe
  `Mutex<ChildStdin>` (writes are short and non-blocking in practice);
- a single supervisor thread owns the state machine and `select`s over a
  small set of `mpsc` receivers (one merged channel, tagged by source);
- timers are a thread doing `recv_timeout`; signals set `AtomicBool`s read
  on each loop tick; `SIGCHLD`/`waitpid` reaping is one thread.

**Thread/FD scaling at expected scale.** Worst case ~ (8 MCP readers + 50
subagent readers + 1 intelligence + 1 timer + 1 signal/reaper) ≈ **60–65
threads, ~130 FDs.** Default Linux `ulimit -n` is 1024 and a thread costs
~8 KiB of *committed* stack if you set a small stack (default 8 MiB is
virtual, not resident). **Three orders of magnitude inside every OS limit.**
At 50 subagents the dominant memory cost is the 50 *child processes*, not
the 50 reader threads. Scaling is a non-issue.

**Code complexity.** Lowest conceptual load: every reader is a straight-line
`BufReader::read_line` loop — exactly the code already shipping in
`mcp/client.rs`, `intelligence/client.rs`, and `triggers/http.rs`. No
executor, no `Waker`, no `Pin`, no coloured functions, no `.await` discipline.
The merge point is one `mpsc` and a `match` on a tagged enum. A new
contributor reads it top-to-bottom. This directly serves the RFC's
"small enough to read in an afternoon" bar (§1.1).

**Dependency weight.** **Zero.** `std::thread`, `std::sync::mpsc`,
`std::process`, `std::io`. `libc` only for signals/pgid (already a dep).

**Failure-handling ergonomics — the decisive axis.** A misbehaving source is
*isolated by construction*: it is one thread blocked on one FD. The
supervisor never blocks on it, because it only ever touches that source via
an `mpsc` it `recv_timeout`s. Recovery is "drop the handle, kill the child,
let the reader thread die on EOF/`EPIPE`." There is no shared executor whose
health can be degraded by one wedged task. This is strictly *better* failure
containment than (C), and it is the property RFC requirement (8) is asking
for.

**Cancellation / timeout.** Three complementary mechanisms, all blunt and
reliable:
- **Deadline at the channel:** `recv_timeout` on the supervisor's merged
  channel gives a wall-clock tick; the supervisor enforces step/token/time
  budgets there.
- **Hard cancel of a subagent:** `SIGKILL` the child (or its process group)
  — the OS reclaims everything, the reader thread sees EOF and exits. This
  is the *only* truly reliable cancel of model-driven work, and it is free.
- **Stuck pipe:** see §3. A blocked reader thread is *abandonable* — we stop
  reading from it and kill the owning process; the thread unblocks on the
  resulting EOF or is left parked harmlessly until process exit. We never
  need to *interrupt* a blocked `read`, only to make the thing on the other
  end go away.

**The stuck-pipe case, concretely.** MCP server hangs after we sent
`tools/call`. The reader thread is parked in `read_line`. The supervisor,
waiting on its merged `mpsc` with a per-call deadline, times out, marks the
call failed, and (per policy) respawns that MCP child — closing the old
child's stdout, which unblocks the reader thread into an EOF path where it
exits cleanly. No `read` was ever interrupted; we changed the world the
`read` was waiting on. This is the cleanest possible handling and it needs
no async machinery.

**Weaknesses, stated honestly.**
- **Many *idle* connections are wasteful** — one parked thread each. Only
  bites if `--serve-mcp` accepts many peer agents; the agent-unit core does
  not. Mitigation in §2.5 (B-as-fallback for the listener only).
- **Interrupting a blocked `read` in place** is not possible without closing
  the FD or using `set_read_timeout` (TCP/unix only; **not available on
  pipes**). The design sidesteps this by never *needing* to interrupt in
  place — it kills the producer instead. Worth writing down as an invariant.
- **Write-side blocking** if a child stops draining its stdin and the pipe
  buffer fills. Control writes are tiny (< 64 KiB pipe buffer); a bounded
  write timeout via a watchdog, or simply treating a wedged write as a
  "kill the child" signal, covers it.

### (B) Single/few-thread event loop over epoll/kqueue (`mio` or raw `libc::poll`)

**Shape.** One thread runs `mio::Poll` (or `libc::poll`) over all readable
FDs — MCP pipes, subagent pipes, the intelligence socket, listener sockets —
in non-blocking mode, dispatching readiness to per-source parsers driven by
a hand-written state machine. Timers via `poll` timeout; signals via
`signalfd`/self-pipe registered as just another FD.

**Thread/FD scaling.** Excellent — one thread, O(N) FDs, scales to thousands.
But we do not have thousands; this strength is unneeded at the agent-unit
scale. Where it *does* earn its keep: a served self-MCP that many peer
agents idle-connect to (RFC §8 composition) — there, one poll thread beats
N parked reader threads.

**Code complexity — the real cost.** You must hand-write: non-blocking
partial-read buffering and frame reassembly for *every* source (NDJSON
frames split across `read` calls), a readiness state machine, backpressure
on writes (`WouldBlock` → re-register for writable), and timer-wheel logic.
This is materially more code than (A) and it is the *subtle* kind
(off-by-one in buffer compaction, lost-wakeup on edge-triggered epoll). It
fights the "read it in an afternoon" bar.

**Dependency weight.** `mio` is light and mature: **1.2.1, deps = `log` +
`libc` (Unix) / `windows-sys` (Win)**, features `os-poll` + `os-ext` (the
latter unlocks `SourceFd` for arbitrary pipe FDs and the `unix::pipe`
types). Or **zero deps** with raw `libc::poll` (we already link `libc`).
Either is cheap on *dependencies*; the cost is *code*, not crates.

**Failure handling / cancellation.** A wedged source does **not** block the
loop (non-blocking FDs) — good. But a CPU-spinning or buggy parser *does*
stall every other source, because they share one thread. And you still
cannot cancel model work this way: hard cancel is *still* `SIGKILL` the
child. So (B) buys you nothing on the cancellation axis that (A) didn't
already have, while adding the shared-thread-stall risk.

**Stuck pipe.** Handled gracefully (the loop simply stops getting readiness
for it and you close/kill), but no better than (A) in outcome.

**Verdict on (B):** the right tool *only* for the high-fan-in served-MCP
listener, not for the core supervision of a few processes. Keep it in the
toolbox, feature-gated, behind `--serve-mcp` when peer count is high. Don't
make it the baseline.

### (C) tokio (full async)

**Shape.** `tokio::process::Child` with async pipes, `tokio::select!` over
streams, `tokio::time` for deadlines, `tokio::signal`, async TLS via
`tokio-rustls`, async HTTP via `hyper`/`reqwest`, `tokio-vsock` for vsock.

**Thread/FD scaling.** Best-in-class, irrelevant here. We are nowhere near
the scale where the work-stealing scheduler pays off.

**Code complexity.** The async model is *familiar* but not *simple* for this
problem: `select!` cancellation-safety footguns, `Pin`, `'static` + `Send`
bounds rippling through the supervisor state, coloured functions splitting
the codebase into sync/async halves, and the re-exec subagent (RFC §4.2)
having to stand up a runtime on every child start. It actively works against
auditability.

**Dependency weight — disqualifying.** tokio's full feature set plus a TLS
stack plus an HTTP stack is **scores of crates** (the OTLP feature in the
*old* crate alone pulls "~50 crates" per its own Cargo comment, and that is
just the exporter). RFC §2 and §12 explicitly list "**no async runtime
(tokio)**" as out-of-scope, twice. This isn't a close call; it's a stated
non-goal.

**Failure handling / cancellation.** Async cancellation (drop the future) is
*ergonomic for I/O* but **does not cancel a runaway subagent** — that is a
child process, still killed with `SIGKILL`. And a blocking or long-CPU task
accidentally run on the async worker pool starves the scheduler — the exact
"one stuck thing degrades everything" failure (8) wants to avoid, reintroduced
by the runtime. The much-touted advantage (cheap cancellation) doesn't apply
to the thing we actually need to cancel.

**Verdict on (C):** wrong model for a *process-supervisor* whose unit of
concurrency is an OS process, not a task. Reject as core, in agreement with
the RFC.

### 2.5 The hybrid we actually recommend

- **Core supervision = (A) thread-per-fd.** MCP-server readers, subagent
  readers, intelligence I/O, timers, signals.
- **High-fan-in served self-MCP (optional) = (B) one `mio`/`poll` accept+read
  loop**, compiled only with the `serve-mcp` feature and used when many peer
  connections idle. For a small number of peers, even this can stay
  thread-per-connection and skip `mio` entirely.
- **Never (C).**

This keeps the *default* build at zero concurrency-dependencies, puts the
only event-loop complexity behind a feature that most deployments won't
enable, and matches the OS-process-centric mental model end to end.

---

## 3. Liveness / stuck-detection design (the requirement that actually drives this)

Because the model is "a few processes, each watched by a thread, all merged
into one channel," dead/stuck detection is uniform and simple:

1. **Per-operation deadline** at the supervisor's `recv_timeout` — every
   `tools/call`, every intelligence request, every subagent turn carries a
   deadline; the supervisor enforces it centrally and never trusts the child
   to self-terminate.
2. **Heartbeat / progress frames** — the control channel (RFC §6.1 "every
   loop turn streams events") doubles as a liveness signal. A subagent that
   emits no event within a watchdog window is *stuck* and gets killed.
3. **`SIGCHLD` + `waitpid(WNOHANG)`** reaping in a dedicated thread (or a
   `signalfd` FD if we adopt (B) for the listener) detects *dead* children
   immediately and distinguishes crash (signal/exit code) from clean exit.
4. **Kill-the-subtree** via process groups: spawn each subagent in its own
   process group (`setpgid`), so `killpg(pgid, SIGKILL)` reaps it *and its
   children* atomically — the natural "parent scopes+controls children" of
   RFC §6.3, implemented with two `libc` calls and no crate.
5. **Abandon-don't-interrupt invariant:** we never try to unblock a parked
   reader thread directly; we make its source disappear (close/kill) and let
   it die on EOF. Write this down — it's the property that lets (A) handle
   stuck pipes without `set_read_timeout` on pipes (which the OS doesn't
   offer).

All of this is *easier* in (A) than in (B)/(C): the deadline lives in one
`recv_timeout`, the kill is one syscall, and there is no executor state to
reconcile after a kill.

---

## 4. JSON: `serde_json` (recommendation: keep it, don't hand-roll)

**Decision: `serde_json` + `serde` derive.** Measured tree (from the live
workspace, `cargo tree`): `serde_json` → `itoa`, `memchr`, `zmij` (ryu-class
float formatter), `serde_core`; `serde derive` adds `proc-macro2`, `quote`,
`syn`, `unicode-ident` *at build time only* (proc-macros don't ship in the
binary). This is the most-audited JSON implementation in the ecosystem,
streaming-deserialize capable, and it is load-bearing for *correctness* of
the JSON-RPC and LLM wire formats.

**Why not hand-rolled JSON.** MCP and the LLM wire are not a fixed schema you
can scan for; they carry arbitrary nested tool arguments and results
(`serde_json::Value`), need correct string escaping, UTF-8, number edge
cases, and deep-nesting safety. Hand-rolling this is *more* code than the
thing it saves and a perennial bug source. The RFC itself hedges ("a single
small trusted JSON library *or* a hand-rolled minimal parser if even that is
too much", §12) — the evidence says: it is not too much; take the library.

**Why not a micro-JSON crate** (`tinyjson`, `microjson`, `nanoserde`,
`miniserde`):
- `microjson` is `no_std`, read-only, and explicitly for "extract a small
  amount of data once" — it cannot model arbitrary tool-call payloads.
- `tinyjson` parses to `Vec`/`HashMap` with the worst benchmark performance
  of the field and no `serde` integration — you'd re-hand-roll typing on top.
- `nanoserde`/`miniserde` drop the dependency but also drop `serde`'s
  derive/typing ergonomics that the protocol structs (`RpcRequest`,
  `ToolsCallResult`, etc., already defined in the parts bin) lean on.

The saving is a handful of tiny build-time crates; the cost is correctness
and ergonomics on the *one* wire format the whole runtime is built around.
**Not worth it.** This is the dependency that has unambiguously earned its
place.

---

## 5. HTTP/1.1 client: hand-roll (preferred) ▸ else `minreq` ▸ `ureq` only if streaming demanded

This is the **highest-leverage minimalism decision** in the whole runtime,
because the convenient choices are deceptively heavy. Hard numbers from the
live workspace:

| Option | TLS stack | Pulls `url`/IDNA/ICU? | Streams SSE? | Approx. crates added |
|---|---|---|---|---|
| **Hand-rolled** over `TcpStream`/`UnixStream`/`VsockStream` | rustls only *if* https | **No** | **Yes** (full control) | **0** (+rustls when https) |
| `minreq` (`https-rustls`) | rustls 0.21 + ring | **No** | **No** (buffers whole body) | ~9 (minreq, rustls, rustls-webpki, sct, ring, untrusted, webpki-roots, log, getrandom) |
| `ureq` 3.x (rustls) | rustls 0.23 + ring/aws-lc | **Yes** (`url`→IDNA→**ICU**) | **Yes** (`Body::into_reader`) | ~40+ |

Measured: enabling the old crate's `intel-remote` (which is `ureq` 2.x +
rustls) took the workspace from **52 → 93 unique crates**. The `url`
dependency alone drags in a **21-crate ICU/IDNA stack** (`icu_normalizer`,
`icu_properties`, `zerovec`, `yoke`, `tinystr`, `litemap`, `writeable`,
`zerotrie`, …) purely to parse URLs to RFC-3987. For a binary whose thesis
is "small enough to audit by reading it," shipping ICU is absurd.

**Why hand-roll is the right answer here, not a heroic flourish:**
- The codebase **already ships a hand-rolled HTTP/1.1 client and SSE reader**
  (`intelligence/client.rs`: raw `TcpStream`, `set_read_timeout`,
  `read_line` SSE loop) and a hand-rolled HTTP/1.1 *server*
  (`triggers/http.rs`). This is *proven prior art in this exact repo*, not
  speculation.
- `agent`'s HTTP needs are narrow and known: `POST /chat/completions`
  (and MCP-over-HTTP), a couple of headers, chunked **or** SSE response
  bodies, one well-behaved gateway endpoint. It does **not** need redirects
  (the old code deliberately refuses them on policy grounds), cookies,
  proxies, compression, connection pooling, or IDNA. A correct subset is a
  few hundred lines.
- **Streaming is a first-class need.** The intelligence connection is
  "possibly streaming" (RFC §7.2, §10 `--max-tokens` token output). `minreq`
  **cannot** stream — it buffers the entire body before you can read a byte,
  so token-by-token SSE is impossible with it. Hand-rolling gives exact
  control over the SSE frame loop and per-read timeouts. `ureq` 3.x streams
  but pays the ICU tax.
- Unix-socket and vsock intelligence transports (RFC §7.2) are *trivial*
  hand-rolled (write request bytes to a `UnixStream`/`VsockStream`, read the
  response) and *awkward* through `minreq`/`ureq`, which are TCP/URL-centric
  (ureq 3.x's `Transport` trait could carry them, but that's bespoke glue
  you'd write anyway).

**Recommendation:** hand-roll a ~single-module HTTP/1.1 + SSE client that is
transport-agnostic over `Read + Write` (so the same code drives TCP, unix,
vsock, and rustls streams). Keep `minreq` named in the RFC as the
"if you insist on a crate" fallback **only** for the non-streaming
one-shot case, and explicitly note its no-streaming limitation. Avoid `ureq`
unless a future requirement makes hand-rolled HTTP untenable — and if so,
pull it behind a feature and accept the ICU cost knowingly.

---

## 6. TLS: `rustls` (`ring` provider), feature-gated, default OFF

- **Default container topology terminates TLS at a sidecar/gateway** (RFC
  §7.2 "common same-pod sidecar," §12 "recommended container pattern
  terminates TLS at the sidecar and keeps `agent` plaintext-to-localhost").
  So the *common* build links **no TLS** — the biggest TLS dependency win is
  *not compiling it at all*.
- When `https://` is used directly (standalone CLI), compile `rustls` behind
  a `tls` feature. **Use the `ring` `CryptoProvider`, not `aws-lc-rs`.** The
  old crate defaulted to `aws-lc-rs`, which pulls **`aws-lc-sys`** and a
  **C/CMake build dependency** — hostile to a clean static musl build and a
  minimal container. `ring` is self-contained (some asm, no system C
  toolchain), and it is what `minreq`/`ureq`'s default rustls path already
  uses in the measured trees.
- Pair with `webpki-roots` (compiled-in Mozilla roots) rather than reading OS
  trust stores, for a hermetic, scratch-container-friendly binary.
- **No** `native-tls`/OpenSSL (system C dep, defeats static linking).
- **No** `aws-lc-rs` unless FIPS is a stated requirement (it is not).

Sidecar-terminated TLS is the *primary* recommendation; in-process rustls is
the *fallback* for standalone direct-to-provider use, behind a flag.

---

## 7. vsock: the `vsock` crate (blocking), feature-gated

- **Use `vsock` (rust-vsock/vsock), not `tokio-vsock`.** Measured tree:
  `vsock 0.5.4` → `libc`, `nix`, `bitflags`, `cfg-if`, `memoffset` — **5
  small crates**, all of which (except `nix`) are already in any Unix build's
  vicinity. It exposes `VsockStream`/`VsockListener` that mirror
  `std::net::TcpStream`/`TcpListener`, so they slot directly into the
  thread-per-fd model and the transport-agnostic hand-rolled HTTP client.
- `tokio-vsock` exists solely to provide `AsyncRead`/`AsyncWrite` for the
  async runtime we are rejecting; it would *force* tokio in. Don't.
- Gate behind a `vsock` feature; only enclave/microVM builds pay for it.
- One open item to settle (RFC §14.8): the named-`intelligence` vsock
  addressing/discovery scheme. Orthogonal to the crate choice.
- `nix` enters *here* (as `vsock`'s dependency) — that's acceptable in the
  vsock-only build. It is **not** needed for signals (see §8); don't let it
  leak into the default build.

---

## 8. Signals & process control: raw `libc`, no extra crate

- **Signals:** keep the parts-bin approach (`src/signals.rs`): raw
  `libc::sigaction` installing handlers that only flip `AtomicBool`s
  (async-signal-safe), polled on each supervisor loop tick. **No
  `signal-hook`, no `nix`** for this — they'd add a tree for what is ~40
  lines of audited `unsafe` we already have.
- Deliberately omit `SA_RESTART` so a blocked `accept()`/`read()` returns
  `EINTR` and the loop observes the flag promptly (the old code already does
  this).
- **`SIGCHLD`** handling for reaping: either a flag + `waitpid(WNOHANG)`
  sweep, or (if (B) is in play for the listener) a `signalfd` registered in
  the poll set.
- **Process control:** `std::process::Command` to spawn, plus `libc` for
  `setpgid` (own process group per subagent) and `killpg(pgid, SIGKILL)` to
  reap a subtree atomically. `waitpid` for exit-status classification. All
  `libc`, already a dependency. No `command-group`/`nix` needed, though
  `nix` would make the pgid calls safe-wrapped *if* it's already present for
  vsock.

---

## 9. Dependency budget (concrete)

First-party (non-transitive) crates, by build profile. "✓ build" = present;
"feat" = behind a Cargo feature, default off unless noted.

| Concern | Crate | Default Linux build | Notes |
|---|---|---|---|
| JSON / serde | `serde`, `serde_json` | ✓ (core, non-negotiable) | derive is build-time only |
| Error types | `thiserror` | ✓ (or hand-roll enums to drop it) | optional; could be `std::error::Error` by hand |
| Logging/tracing | `tracing` (+ `tracing-subscriber` fmt/json) | ✓ | observability req (6); consider trimming `tracing-appender`/`time` if size-sensitive |
| Config file (optional) | `toml` (parse only, no `serde_spanned`/edit) | feat | flags+env cover the minimal case; file only for rich MCP lists |
| HTTP/1.1 + SSE client | **hand-rolled module** | ✓ (when any networked transport) | 0 deps; transport-agnostic over `Read+Write` |
| HTTP client fallback | `minreq` (no-stream) / `ureq` 3.x (stream) | feat (discouraged) | `minreq` ~9 crates no-url; `ureq` ~40 w/ ICU |
| TLS | `rustls` (`ring`) + `webpki-roots` | feat `tls`, **off** | sidecar-terminate by default; `ring` not `aws-lc-rs` |
| vsock | `vsock` (blocking) | feat `vsock`, off | pulls `nix`,`bitflags`,`memoffset`,`cfg-if`,`libc` |
| Signals + pgid/kill | `libc` | ✓ (Unix) | `sigaction`, `setpgid`, `killpg`, `waitpid` |
| Concurrency | **`std` only** (`thread`, `mpsc`, `process`, `io`) | ✓ | thread-per-fd; **0 deps** |
| High-fan-in served-MCP loop (optional) | `mio` (`os-poll`,`os-ext`) *or* raw `libc::poll` | feat `serve-mcp` | only when many peer connections; `mio` deps = `log`+`libc` |
| Windows ctrl-c (if ported) | `ctrlc` / `windows-sys` | `cfg(windows)` | Unix is the primary target |

**Explicitly OUT** (reaffirming RFC §12, with evidence): `tokio` & any async
runtime; `hyper`/`reqwest`; `url` (and its ICU/IDNA stack — the measured
21-crate tax); `aws-lc-rs`/`aws-lc-sys` (C build dep); `native-tls`/OpenSSL;
`signal-hook`; `tokio-vsock`; the retired `regorus`/`jsonschema`/
`ed25519-dalek`/`jsonwebtoken`/OTLP(`opentelemetry*`+tokio) stacks.

**Headline numbers:** a default Linux build is **~5–6 first-party crates**
(serde, serde_json, thiserror, tracing(+subscriber), libc) with a small,
audited transitive tree and **no networking/TLS/async weight unless a
feature opts in**. Turning on direct-https adds `rustls`+`ring`+`webpki-roots`
(~7–9 crates) *without* the `url`/ICU tax because HTTP is hand-rolled. That
is a runtime you can genuinely read in an afternoon.

---

## 10. Open questions this review touches (cross-ref RFC §14)

- **§14.1 control protocol:** the thread-per-fd merge model is agnostic to
  whether the supervisor↔subagent channel is literal MCP or a JSON-RPC
  sibling — both are NDJSON over pipes read by the same reader-thread code.
  Recommendation stands: reuse MCP JSON-RPC shapes, share the parser.
- **§14.2 intelligence wire:** hand-rolled HTTP makes the OpenAI-compatible
  `/chat/completions` + SSE adapter cheap and fully under our control;
  pushing other provider dialects to the gateway keeps the client tiny.
- **§14.6 subagent transport:** stdio pipes confirmed viable for v1 under
  (A); a socket-based control channel (unix/vsock) would also drop into the
  same model if ever needed, since the reader code is transport-agnostic.
- **A new invariant to record:** *"the supervisor never blocks on an
  untrusted source; it reaches every pipe/socket only through an `mpsc` it
  `recv_timeout`s, and it unblocks a parked reader by closing/killing the
  producer, never by interrupting the read."* This one sentence is what makes
  thread-per-fd safe against stuck pipes without any async machinery.

---

## 11. Bottom line

The concurrency question and the dependency question have the **same
answer**: lean on the OS. Processes are the unit of isolation and
cancellation; threads with blocking I/O and `mpsc` are the unit of
multiplexing; `libc` is the unit of control. tokio would add scores of
crates to solve a scale problem we don't have while *failing* to solve the
cancellation problem we do have (which is `SIGKILL`, not future-drop). An
event loop (`mio`/`poll`) is the right tool only for a high-fan-in served
listener and belongs behind a feature. JSON is the one library we take
without apology; HTTP we hand-roll to dodge the ICU stack; TLS and vsock are
opt-in features with the lightest viable backends (`rustls`/`ring`,
blocking `vsock`); signals and process control are raw `libc`. The result is
a binary whose default build is single-digit dependencies and whose
concurrency model is small enough to hold in your head — which is exactly the
moat RFC 0001 says it wants.
