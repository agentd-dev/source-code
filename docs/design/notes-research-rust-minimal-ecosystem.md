# Research notes — Rust crate choices for a near-zero-dependency agent

**Status:** Research artifact (input to RFC 0001 implementation).
**Author:** generated for Andrii Tsok.
**Date:** 2026-06-25.
**Scope:** Concrete crate selection for the rewritten, minimal, MCP-native,
reactive `agent` runtime (see `rfcs/0001-mcp-native-agent-runtime.md`,
especially §12 "Dependency budget"). Bias throughout: *fewest transitive
crates, no async runtime, no C build toolchain unless forced, std-first.*

> Versions below were current as of mid-2026. Always re-confirm with
> `cargo tree -e normal --target x86_64-unknown-linux-gnu` and
> `cargo audit` at implementation time; numbers drift.

---

## 0. Guiding rules used to score each option

1. **No async runtime.** No tokio/async-std/smol anywhere in the core tree.
   Concurrency is OS processes plus a handful of `std::thread`s.
2. **No C/C++ build toolchain in the default build.** Avoid `cmake`, `nasm`,
   `bindgen`, vendored OpenSSL. (This single rule decides the TLS backend.)
3. **Minimise transitive crate count**, not just direct deps. A crate that
   adds 1 line of `use` but 40 transitive crates loses.
4. **std-first.** If `std` already does it acceptably (process spawn, pipes,
   threads, TCP, Unix sockets), do not add a crate.
5. **Feature-gate everything optional.** TLS, vsock, observability, HTTP MCP
   transport must each be a Cargo feature that a minimal build omits.
6. **Trust / maintenance.** Prefer crates by known maintainers (dtolnay,
   rustls project, algesten, nix-rust) with recent releases and `cargo audit`
   cleanliness.

---

## 1. JSON — the one non-negotiable dependency

MCP and the LLM wire format are both JSON, so a JSON library is the single
dependency RFC 0001 §12 concedes up front. Candidates:

### serde + serde_json (the default standard)
- **Versions:** `serde` 1.0.x, `serde_json` 1.0.x (both continuously
  released; mature, ubiquitous). Maintained by dtolnay.
- **Transitive weight (typical):** `serde_json` pulls `serde`, `itoa`, `ryu`,
  and `memchr` — i.e. ~4–5 crates total (all tiny, all dtolnay/BurntSushi,
  all `cargo audit`-clean, all no-C). With `serde_derive` you also pull
  `proc-macro2`, `quote`, `syn` **as build-time/proc-macro deps** (they do
  not ship in the final binary but do dominate clean-build compile time).
- **Pros:** derive macros map JSON-RPC / MCP / OpenAI-chat structs directly;
  best-in-class ergonomics; everyone knows it; `serde_json::Value` gives a
  dynamic fallback for the parts of MCP we don't want to type. Streaming and
  zero-copy where needed.
- **Cons:** `syn`-driven compile time is the main cost; the runtime crate
  set is small and clean. Author has explicitly optimised compile weight.

### miniserde (dtolnay's deliberately-minimal alternative)
- **Transitive weight:** dramatically smaller — implementation is ~12× less
  code than serde+serde_derive+serde_json; depends only on `alloc`-level
  things. Non-recursive serialize/deserialize (no stack-overflow on deep
  nesting — a real safety win for untrusted MCP/LLM payloads).
- **Cons:** intentionally feature-poor — no enums with data, no
  `#[serde(...)]` attribute richness, no `serde` trait ecosystem, no borrow
  deserialization. MCP/JSON-RPC has tagged unions (method + params, result
  vs error) that miniserde models awkwardly. Performance ~1.5–2× slower than
  serde_json (irrelevant at our request rate).

### tinyjson (zero-dependency, DOM-only)
- **Transitive weight:** **0 dependencies**, but needs `std` (uses
  `HashMap`). Pure value tree (`Vec`/`HashMap`); no derive, no typed structs.
- **Cons:** worst parse performance of the benchmarked set; you hand-write
  all (de)serialization against an untyped DOM. For a protocol as shaped as
  MCP this is a lot of error-prone glue.

### Hand-rolled
- A minimal recursive-descent JSON parser is ~300–500 lines and 0 deps. The
  old retired code may already contain prior art. But: JSON number/Unicode
  escaping edge cases, and the volume of MCP/OpenAI structs to (de)serialize
  by hand, make this a maintenance tax that contradicts "small enough to
  audit but also correct." We'd be re-litigating well-solved problems.

### Recommendation — JSON
**Use `serde` + `serde_json` with `derive`.** Rationale: the *runtime*
transitive set is tiny (~4 crates: itoa, ryu, memchr, serde) and impeccably
maintained; the cost is compile-time proc-macros, which do not bloat the
shipped binary and are a one-time build expense. The ergonomic win on
JSON-RPC / MCP / OpenAI-chat struct mapping is large and recurring, and
`serde_json::Value` covers the dynamic parts. miniserde is the fallback *if*
the minimalism audit (RFC §15 phase 5) decides proc-macro compile weight is
unacceptable — but its missing tagged-enum support makes JSON-RPC modeling
painful. Do **not** hand-roll; the surface area (full MCP + OpenAI schemas)
is too large to maintain correctly. Keep all wire types in one module so a
future swap to miniserde/hand-rolled is mechanical.

---

## 2. Blocking HTTP/1.1 client (https intelligence + HTTP MCP)

Needed for `https://` intelligence and HTTP/SSE-transport MCP. Must be
blocking (no async runtime) and feature-gated (many builds use
unix/vsock/stdio only and need **no** HTTP client at all).

### ureq (recommended)
- **Version:** **3.3.0** (2026-03-21); actively developed (3.x line through
  2026), 2.2k+ stars, maintained by algesten. Conservative MSRV policy.
- **Design:** blocking I/O by design ("keeps the API simple and deps to a
  minimum"). API modeled on the `http` crate but **does not hard-depend** on
  pulling a heavy http stack the way reqwest does.
- **TLS:** in 3.x the default `rustls` feature uses **rustls with the `ring`
  provider** (NOT aws-lc-rs) — so the default build needs **no cmake**. This
  is exactly our preferred posture (see §3).
- **Transitive weight (rustls+ring):** rustls 0.23, `ring`, `rustls-pki-types`,
  `webpki-roots` (optional, for bundled roots), `rustls-platform-verifier`
  (optional). Plus ureq's own small set. Notably the default set is
  controllable: `gzip` (flate2) and `rustls` are the only default features;
  `json`, `cookies`, `charset`, `brotli`, `socks-proxy`, `native-tls` are all
  opt-in. We can drop `gzip` and `json` (we use serde_json directly).
- **Plaintext-only mode:** `default-features = false` removes rustls entirely
  → a plain HTTP/1.1 client with a very small tree, ideal for the
  sidecar-terminates-TLS / localhost-plaintext container pattern (RFC §12).
- **Recommendation rationale:** best-maintained, ring-by-default (no C
  toolchain), clean feature gating, blocking. This is the strongest fit.

### minreq
- **Version:** 2.14.x. "Simple, minimal-dependency HTTP client" (neonmoe).
- **Weight:** ~148 KB stripped binary growth with no optional features;
  smallest of the group. Optional `https-rustls`, json, proxy features.
- **Cons:** smaller/less active community; rougher edges on streaming,
  redirects, timeouts, and chunked transfer than ureq; fewer eyes on TLS
  config. Good if we wanted the absolute floor and were willing to own gaps.

### attohttpc
- **Version:** maintained (sbstp). Feature set ~on par with ureq.
- **Cons:** historically OpenSSL-leaning; rustls support arrived later and is
  less first-class than ureq's (e.g. `tls-rustls-native-roots`). Pulls a
  bespoke `http`-types structure. Less aligned with our no-C-toolchain rule
  out of the box.

### Hand-rolled HTTP/1.1
- Feasible for plaintext: HTTP/1.1 request/response over a `TcpStream` /
  `UnixStream` is a few hundred lines (request line, headers, chunked vs
  content-length body). RFC §12 notes "hand-rolled where practical (we have
  prior art)." Attractive for the **unix:/vsock plaintext** intelligence and
  stdio MCP cases where there's no TLS and traffic is local/trusted.
- **Cons:** SSE parsing, chunked decoding, keep-alive, and especially TLS
  integration are where hand-rolling stops paying off. Pairing a hand-rolled
  plaintext path with `ureq` only when `https`/TLS is needed is viable but
  means two code paths to maintain.

### Recommendation — HTTP
**Primary: `ureq` 3.x, feature-gated behind a `http` (or `tls`) cargo
feature, with `default-features = false` and an explicit minimal feature
list (`rustls` only when TLS is needed; drop `gzip`/`json`).** It gives us
ring-backed rustls with no C toolchain, blocking I/O, and clean gating.
For the **plaintext localhost/unix/vsock** transports we may additionally
hand-roll a tiny HTTP/1.1 writer/reader (reusing old prior art) so those
builds carry **zero HTTP-client crates**. Decision point for phase 1: start
with ureq behind a feature; revisit hand-rolling plaintext in the phase-5
minimalism audit if ureq's non-TLS tree is still heavier than desired.

---

## 3. TLS

Only needed when intelligence or an MCP server is reached over `https://`
**and** TLS is not terminated by a sidecar/gateway. RFC §12 explicitly says
the recommended container pattern terminates TLS at the sidecar, so **many
builds carry no TLS at all** — TLS must be feature-gated.

### rustls + ring (recommended TLS backend)
- **Version:** rustls 0.23.x (0.23.40-era in 2026). Pure-Rust TLS, no
  OpenSSL.
- **Crypto backend choice — the key decision:**
  - **aws-lc-rs** is rustls 0.23's *default* provider, but it has
    **build-time deps: `cmake` on all platforms, `nasm` on Windows**, and a
    C/C++ compiler. This violates our no-C-toolchain rule and complicates the
    minimal container image (we'd need build tooling in the builder stage).
  - **ring** requires **no cmake**, "higher chance of compiling cleanly
    without additional developer environment," and is the long-standing
    pure-ish (asm+C but self-contained, no external cmake) backend.
- **How to select ring:** `rustls = { version = "0.23", default-features =
  false, features = ["ring", "std", "tls12"] }` (drop the implicit aws-lc-rs).
  Conveniently, **`ureq` 3.x already defaults its `rustls` feature to ring**,
  so if we go through ureq we get the right backend for free.
- **Roots:** `webpki-roots` (bundled Mozilla roots, no OS dependency — best
  for minimal containers) vs `rustls-native-certs`/platform-verifier (reads
  OS trust store; adds a little weight but matches host policy). For a
  scratch/distroless image, **`webpki-roots` is the minimal choice.**

### native-tls
- Wraps OS TLS (SChannel/Secure Transport/OpenSSL). On Linux that means
  **OpenSSL** — a system or vendored C dependency, the opposite of our goal.
  Rejected for core; not worth a feature.

### Sidecar-terminated / none
- The RFC-preferred default: `agent` speaks **plaintext to localhost**, a
  sidecar/gateway does TLS. In this mode the binary links **no TLS crates**.
  This should be the default container recommendation and the default Cargo
  build (TLS off).

### Recommendation — TLS
**Feature-gate TLS entirely (`tls` feature, off by default).** When on, use
**rustls 0.23 with the `ring` provider and `webpki-roots`** — pure Rust, no
cmake/OpenSSL, minimal container-friendly. Reach it through `ureq`'s default
`rustls`(=ring) feature to avoid configuring rustls twice. Explicitly reject
aws-lc-rs (cmake) and native-tls (OpenSSL). Document the
**sidecar-terminated, TLS-off** build as the recommended container shape.

---

## 4. vsock (enclave / microVM intelligence transport)

For `vsock:<cid>:<port>` intelligence inside a microVM/confidential enclave
(RFC §7.2). Feature-gated (`vsock` feature) — irrelevant outside enclaves.

### vsock crate (recommended)
- **Version:** 0.5.x (rust-vsock/vsock-rs). 19 releases, ongoing attention.
- **API:** `VsockStream` / `VsockListener` **analogous to
  `std::net::TcpStream`/`TcpListener`** — so it slots into the same blocking
  read/write code paths as our TCP/Unix transports with minimal special
  casing. Needs `std`.
- **Deps:** thin — `libc` (for `AF_VSOCK`, `sockaddr_vm`, `VMADDR_*`) and it
  has historically used `nix` for socket syscalls. Since we already take
  `libc`/`nix` for signals+process control (§5), the *incremental* transitive
  cost of the vsock crate is essentially just the crate itself.

### Raw libc (AF_VSOCK by hand)
- `libc` already exposes `sockaddr_vm`, `VMADDR_CID_*`, `AF_VSOCK`. Creating
  the socket, `connect`/`bind`/`listen`, and wrapping the fd in a
  `std::net`-like stream is ~100–150 lines of `unsafe`. Saves one crate.
- **Cons:** all `unsafe`, must hand-roll the `Read`/`Write`/fd-ownership
  wrapper that the vsock crate already gives us safely. Easy to get fd
  lifetime / `FromRawFd` ownership subtly wrong.

### Recommendation — vsock
**Use the `vsock` crate behind a `vsock` feature.** Its incremental cost is
~1 crate (we already pull libc/nix), it gives a safe `TcpStream`-shaped API
that unifies with our other transports, and it removes a block of
hand-written `unsafe` socket code. Only hand-roll over raw libc if the
phase-5 audit finds the crate unmaintained or it conflicts with our `nix`
version — at which point the libc path is a small, well-scoped fallback.

---

## 5. Signals + process control on Unix

We need: `SIGTERM`/`SIGINT` handling with graceful drain (RFC §4.1, §11),
`SIGCHLD`/`waitpid(WNOHANG)` to reap subagents and detect dead/stuck children
(§4.1, hard-req 8), sending `SIGTERM`/`SIGKILL` to subtrees, and ideally
process groups so we can kill a whole subagent subtree. Options:

### std alone
- `std::process::Child` gives spawn, `kill()` (SIGKILL only), `wait()`,
  `try_wait()` (non-blocking reap). `std` does **not** give: custom signal
  handlers, `SIGTERM` send, `SIGCHLD`, process groups, `sigaction`,
  self-pipe/`signalfd`. So `std` is insufficient for clean drain and tree
  control on its own.

### libc (recommended minimal)
- Raw FFI: `sigaction`, `signalfd`/self-pipe trick, `kill(2)`, `killpg`,
  `setpgid`, `waitpid(WNOHANG)`, `prctl(PR_SET_PDEATHSIG)` (Linux — make a
  child die if the supervisor dies), `setrlimit`. Everything we need, one
  crate (`libc` 0.2.x), no transitive deps beyond `core`. All `unsafe`, but
  the surface we use is small and stable.

### nix (recommended ergonomic)
- **Version:** 0.30.1. **Direct deps: just `bitflags`, `cfg-if`, `libc`** —
  i.e. nix adds ~2 small crates on top of libc (both ubiquitous, both also
  pulled by other crates). Maintained by nix-rust.
- Gives safe wrappers: `nix::sys::signal` (`sigaction`, `Signal`, `kill`),
  `nix::sys::wait::{waitpid, WaitStatus, WaitPidFlag::WNOHANG}`,
  `nix::unistd::{setpgid, setsid, Pid}`, `nix::sys::signalfd`. This is
  exactly our use-case and far less `unsafe` to get right (signal-handler
  async-signal-safety, `waitpid` status decoding, pgid handling are easy to
  botch in raw libc).

### Recommendation — signals/process control
**Use `nix` (0.30) as the core Unix primitive, on top of `std::process` for
spawn/pipes.** Reasoning: the things that are subtle and safety-critical
(signal handlers, `waitpid(WNOHANG)` status decoding, process-group kill,
`PR_SET_PDEATHSIG`/`signalfd`) are exactly where nix's safe wrappers earn
their ~2-crate (`bitflags`+`cfg-if`, plus `libc` we'd take anyway) overhead.
We get correct dead/stuck-child detection (hard-req 8) with far less audited
`unsafe`. Keep raw `libc` as a thin direct dep too (some constants/`prctl`
calls), so the pair `libc` + `nix` is the core Unix layer. (`std`-only is
rejected: no `SIGTERM`/`SIGCHLD`/pgroup support.)

> Note: `nix` re-exports the libc bits we need, and `vsock` (§4) already
> wants `libc`/`nix`, so this layer is shared, not additive, across features.

---

## 6. Child-process pipe management / non-blocking reads (no tokio)

The control channel is JSON-lines over each subagent's stdio pipes (RFC
§6.2). We must read events from N children without an async runtime and
without blocking the supervisor, and detect stuck/dead children.

### Pattern A — one reader thread per child pipe + mpsc (recommended)
- For each subagent: spawn a `std::thread` that does
  `BufReader::new(child.stdout).lines()` (or framed JSON reads) and forwards
  parsed events over an `mpsc::Sender` to the supervisor's single select
  loop. Writes (control messages) go directly on `child.stdin` from the
  supervisor (or a dedicated writer thread). Liveness/stuck detection via
  per-child deadlines/heartbeats in the supervisor + `try_wait()`.
- **Cost:** **zero extra crates** — pure `std::thread` + `std::sync::mpsc`.
  Thread-per-pipe is fine: subagent fan-out is bounded by tree depth/breadth
  limits (RFC §6.3), so we're talking tens of threads, not thousands. This is
  the idiomatic non-tokio approach and matches the community guidance
  (threads + channels/callbacks).
- A single supervisor thread `recv()`s the merged channel; combine with a
  timeout (`recv_timeout`) to also drive interval/cron ticks (§7) and
  deadline enforcement in the same loop.

### Pattern B — mio (epoll) over the raw pipe fds
- Register child stdout fds with `mio::Poll` and edge-trigger reads on one
  thread, no per-child thread.
- **Cost:** adds `mio` (+ its small tree). Justified only at high child fan-
  out where thread-per-pipe stack memory matters. At our scale it's
  unnecessary weight and more complex (manual fd readiness, partial-read
  buffering). **Reject for v1**; revisit only if profiling shows thread
  overhead.

### Recommendation — pipes
**Pattern A: thread-per-pipe + `std::sync::mpsc`, zero crates.** It detects
dead children (reader thread sees EOF) and stuck children (supervisor
deadline/heartbeat + `try_wait`), needs no async runtime, and reuses the same
merged-channel select loop for timers. `mio` only if a future high-fan-out
need is proven.

---

## 7. Cron / interval scheduling

Needed for loop/interval mode (§5.2) and time-scheduled runs. Two distinct
needs: (a) plain fixed **intervals**, (b) full **cron expressions**.

### Intervals — hand-rolled (recommended)
- A fixed interval / "loop immediately after completion" is trivial:
  `Instant::now()` + `Duration`, slept via the supervisor's
  `mpsc::recv_timeout(remaining)` so timer ticks and child events share one
  loop with no busy-waiting. **Zero crates.** Do not pull a scheduler crate
  for this.

### Cron expressions — `croner` (recommended *if* cron is required)
- **Version:** current; **only third-party dep is `chrono`** (for the
  date/time math). POSIX/Vixie-compatible plus extensions (`L`, `#`, `W`).
  Pure parsing+`next-occurrence`; the companion `croner-scheduler` adds
  threads but has **no deps except croner** — we don't need it (we own the
  loop). Maintained (Hexagon).
- **Alternatives:** `saffron` (Cloudflare; uses `nom` → more transitive
  crates), `cron` / `cron-parser` (also `chrono`-based). croner is the most
  featureful-yet-lean and the cleanest dep story.
- **Cost note:** `chrono` is the real weight here (and pulls a few crates).
  If we only ever need intervals, we avoid chrono entirely.

### Recommendation — scheduling
**Hand-roll intervals (zero crates) in the supervisor's `recv_timeout`
loop.** Make full **cron a feature-gated extra (`cron` feature) using
`croner`** (which only adds `chrono`). RFC §10's `--interval` is the common
path and needs no crate; cron-expression scheduling is the optional richer
path. Most builds — and the "external operator owns scheduling" container
shape (RFC §11) — carry neither chrono nor croner.

---

## 8. Observability — tracing / OTLP (feature-gated)

RFC §12 explicitly lists OTLP among the "retired stacks" to keep **out of
core**, while hard-req 6 wants first-class logging/healthcheck/tracing. Split
the difference: cheap structured logging always-on; OTLP heavy stack behind a
feature.

### Always-on logging — recommendation: hand-rolled structured stderr, or `log`+tiny sink
- RFC §11 only requires "structured logs to stdout/stderr." A minimal
  JSON-lines logger writing to stderr is ~50 lines, **zero crates**, and is
  the most minimal-bar-aligned choice. Alternatively `log` (facade, 0
  transitive deps) + a tiny custom logger gives a familiar macro surface for
  near-zero cost. **Avoid `tracing` in core** — `tracing` +
  `tracing-subscriber` pull a non-trivial tree (`tracing-core`,
  `sharded-slab`, `thread_local`, `nu-ansi-term`, regex for env-filter,
  etc.).

### Feature-gated OTLP — `tracing` + `opentelemetry-otlp` (only behind `otel`)
- If/when distributed tracing is wanted, gate it: `tracing`,
  `tracing-subscriber`, `tracing-opentelemetry` (0.33-era), `opentelemetry`
  (0.32-era), `opentelemetry-sdk`, `opentelemetry-otlp`. **Caveat:** the
  common OTLP exporter path (`grpc-tonic`) drags in **tonic → hyper →
  tokio** — an async runtime, which we forbid in core. So the `otel` feature
  must use the **HTTP/protobuf or HTTP/json OTLP exporter** (reqwest-blocking
  or our own ureq-based exporter) to avoid tokio, *or* accept that enabling
  `otel` is the one feature that links an async runtime, fully isolated
  behind the gate. Document this loudly.
- Healthcheck (hard-req 6): implement as a trivial signal — a `--health`
  subcommand that checks the supervisor is live, or a tiny readiness file /
  exit code, **no HTTP server in core**. If an HTTP health endpoint is
  wanted, expose it via the already-present self-MCP server rather than
  adding a web framework.

### Recommendation — observability
**Core: zero-crate (or `log`-facade) JSON-lines structured logging to
stderr; healthcheck as a subcommand/exit-code, not a web server.**
**Feature-gated `otel`: `tracing` + `opentelemetry-otlp` via the HTTP
exporter (not grpc-tonic) to avoid pulling tokio into anything but that one
opt-in feature.** This honors hard-req 6 without taxing the minimal binary.

---

## 9. Final recommended Cargo dependency table

### Core (always compiled — the minimal binary)

| Need | Crate | Version | Why / transitive note |
|---|---|---|---|
| JSON wire format | `serde` | 1.0 | derive for MCP/JSON-RPC/OpenAI structs |
| JSON parse/emit | `serde_json` | 1.0 | + `itoa`,`ryu`,`memchr` (~3 tiny crates); `Value` for dynamic parts |
| Unix syscalls (signals, waitpid, pgroups) | `nix` | 0.30 | safe wrappers; direct deps `bitflags`+`cfg-if`+`libc` only |
| Raw libc constants / prctl / rlimit | `libc` | 0.2 | shared with nix & vsock; no transitive deps |
| Process spawn / pipes / threads / timers | `std` | — | `std::process`, `std::thread`, `std::sync::mpsc`, `recv_timeout` — **zero crates** |
| Interval scheduling | (hand-rolled) | — | `Instant`+`Duration` in the recv_timeout loop — **zero crates** |
| Structured logging | (hand-rolled JSON-lines) or `log` | — / 0.4 | stderr; `log` facade has 0 transitive deps if we want macros |
| Plaintext HTTP/1.1 (optional hand-roll) | (hand-rolled) | — | for unix/vsock/localhost transports; **zero crates** |

**Core transitive crate count (target): single digits** — roughly
`serde, serde_json, itoa, ryu, memchr, nix, bitflags, cfg-if, libc`
(~9 crates), plus optional `log`. No async runtime, no C toolchain, no TLS.

### Feature-gated

| Feature | Crate(s) | Version | Adds / cost | Use case |
|---|---|---|---|---|
| `tls` / via `http` | `ureq` | 3.3 | rustls 0.23 + **ring** + `rustls-pki-types` + `webpki-roots` (no cmake) | `https://` intelligence / HTTP MCP with TLS |
| `http` (plaintext) | `ureq` (`default-features=false`) | 3.3 | small, no TLS tree | localhost/sidecar plaintext HTTP MCP |
| `vsock` | `vsock` | 0.5 | ~1 crate (libc/nix already in core); `TcpStream`-shaped API | enclave/microVM intelligence transport |
| `cron` | `croner` | latest | + `chrono` (and its few deps) | cron-expression scheduled runs |
| `otel` | `tracing`, `tracing-subscriber`, `tracing-opentelemetry`, `opentelemetry`, `opentelemetry-sdk`, `opentelemetry-otlp` (**HTTP exporter, not grpc-tonic**) | 2026 lines | large; isolate tokio here if unavoidable | OTLP distributed tracing |

### Explicitly rejected
- **aws-lc-rs** as rustls backend — needs cmake/nasm/C compiler. Use `ring`.
- **native-tls / OpenSSL** — system C dep. Use rustls.
- **tokio / async-std / smol / mio** in core — concurrency is processes +
  threads. (`mio` only reconsidered at proven high child fan-out.)
- **reqwest / hyper** — drag tokio + large trees. `ureq` instead.
- **tracing in core** — non-trivial tree; only behind `otel`.
- **saffron** for cron — `nom` adds more transitive crates than `croner`.
- Hand-rolled JSON — surface area too large to maintain correctly.

---

## 10. Open items to confirm at implementation
1. Run `cargo tree -e normal` on the real Cargo.toml and `cargo audit` /
   `cargo deny`; re-confirm exact transitive counts and advisory status.
2. Confirm `ureq` 3.x non-TLS (`default-features=false`) tree is small enough
   that we *don't* need the hand-rolled plaintext HTTP path; if it is, drop
   the hand-roll to avoid two code paths.
3. Confirm `vsock` 0.5 current `nix`/`libc` version requirements match our
   pinned `nix` 0.30 to avoid duplicate `nix` versions in the tree.
4. Decide `webpki-roots` (bundled, minimal) vs platform-verifier (OS trust)
   default for the `tls` feature — lean bundled for distroless images.
5. For `otel`, prototype the HTTP/protobuf OTLP exporter to verify we can
   keep tokio out, or accept tokio strictly inside the `otel` feature gate.
6. Decide whether the always-on logger is hand-rolled JSON-lines or
   `log`-facade + tiny sink (both near-zero cost).

---

## 11. Sources
- ureq — https://crates.io/crates/ureq , https://lib.rs/crates/ureq ,
  https://docs.rs/crate/ureq/latest , https://github.com/algesten/ureq ,
  https://github.com/algesten/ureq/blob/main/Cargo.toml (v3.3.0; default
  rustls=ring; feature gating; blocking).
- serde_json / miniserde / tinyjson — https://github.com/dtolnay/miniserde ,
  https://docs.rs/miniserde , https://crates.io/crates/miniserde ,
  https://github.com/AnnikaCodes/rust-json-parsing-benchmarks (weight/perf
  tradeoffs).
- rustls / crypto backends — https://docs.rs/rustls/latest/rustls/ ,
  https://docs.rs/crate/rustls/latest , https://crates.io/crates/aws-lc-rs ,
  https://lib.rs/crates/aws-lc-rs (aws-lc-rs default but needs cmake; ring
  needs none).
- minreq / attohttpc — https://github.com/neonmoe/minreq ,
  https://lib.rs/crates/minreq , https://github.com/sbstp/attohttpc ,
  https://shnatsel.medium.com/smoke-testing-rust-http-clients-b8f2ee5db4e6 .
- vsock — https://crates.io/crates/vsock , https://lib.rs/crates/vsock ,
  https://github.com/rust-vsock/vsock-rs (VsockStream/Listener; libc/nix).
- nix / libc — https://crates.io/crates/nix/dependencies (0.30.1 deps:
  bitflags, cfg-if, libc) , https://docs.rs/nix/latest/nix/ ,
  https://github.com/nix-rust/nix .
- non-blocking child pipes — https://users.rust-lang.org/t/how-to-read-from-a-child-process-stdout-pipe-without-blocking/133627 ,
  https://www.nikbrendler.com/rust-process-communication/ (threads+channels).
- cron — https://crates.io/crates/croner , https://lib.rs/crates/croner ,
  https://github.com/Hexagon/croner-rust ,
  https://blog.cloudflare.com/using-one-cron-parser-everywhere-with-rust-and-saffron/ .
- tracing / OTLP — https://github.com/open-telemetry/opentelemetry-rust ,
  https://docs.rs/tracing-opentelemetry , https://crates.io/crates/tracing-opentelemetry
  (grpc-tonic exporter pulls tokio; prefer HTTP exporter to stay async-free).
