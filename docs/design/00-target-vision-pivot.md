# agentd — Target-Vision Pivot: Review, Change Plan, Run-Graph Design

> Status: ACTIVE working plan (2026-07-02). Drives the unix/vsock→HTTPS transport
> pivot + intelligence-HTTPS-only cleanup + the run-graph design. Produced by a
> verified multi-agent review; every load-bearing claim re-checked against code at
> the cited `file:line`. Supersedes the transport prescriptions in RFCs
> 0006/0014/0015/0020 (those get rewritten in Phase 6).
>
> **Locked decisions (user, 2026-07-02):**
> - **Control-plane auth:** mTLS client-cert **primary** + bearer/OAuth **alternative**; the
>   authenticated identity mints `PeerOrigin::Management` (no implicit transport trust).
> - **Control surface:** operator control (drain/pause/resume/lame-duck/cancel) is
>   **unified into the A2A method family** — one HTTPS control protocol (a public
>   wire-contract change, `contract_version` bump, coordinated with agentctl).
> - Plaintext `http://` allowed **loopback-only** (dev/tests); non-loopback rejected at config.
> - HTTPS server framing built in the reusable **`mcp` crate** (symmetric with the client).
> - mTLS *to the model endpoint* (intel client identity): out of scope for the pivot; tracked separately.


# agentd — Target-Vision Review, Change Plan, and Run-Graph Design

This is the merged, corrected, authoritative deliverable. Every load-bearing claim below was re-verified against the code at the cited `file:line`; refuted claims from the drafts have been dropped. Line anchors are exact as of the current tree (agentd crate `version = "1.0.0"`, `default = ["tls"]`).

**One critical baseline the drafts under-stated:** on a *default* build (`Cargo.toml:25` `default = ["tls"]`), `serve-mcp`, `a2a`, `events`, `cluster`, `vsock`, `hot-reload`, `config-watch` are **all opt-in** (`Cargo.toml:33-85`). So a default agentd has **no served MCP/A2A/control listener at all** — the entire serving-side transport story (P3/P4) only exists on feature-enabled builds. This reframes "the biggest gap" as feature-scoped, not universal.

---

## Deep review

### Per-principle verdicts

| # | Principle | Verdict | Load-bearing evidence |
|---|-----------|---------|-----------------------|
| 1 | Tools ONLY from registered MCP servers | **HOLDS** (name the self/control class) | `build_catalogue` (`runner.rs:419`) assembles model-facing task tools purely from connected servers; self/control tools are a *separate* seam (`orchestrator.rs:633-676`) |
| 2 | No local code/command execution | **HOLDS** — no arbitrary exec exists | sole `Command::new(exe)` re-execs agentd itself (`supervisor/spawn.rs:50`); no shell/exec tool anywhere |
| 3 | A2A for agent↔agent comms | **CONTRADICTED-BY-DESIGN** (transport) | A2A client dials only `unix:`/`vsock:`, no `https` arm (`mcp/a2a_client.rs:75-97`) |
| 4 | A2A as operator/control mechanism | **CONTRADICTED-BY-DESIGN + not unified** | control binds unix/vsock only (`config.rs:292-345`); live control on reactive daemon is signal+file (feature-gated) |
| 5 | Hierarchical subagents via internal tools + A2A | **HOLDS** (A2A leg inherits P3) | narrowed scope + supervisor-minted depth + refuse-as-result caps (`orchestrator.rs`, `supervisor/tree.rs`) |
| 6 | Follows provided instructions | **HOLDS** | instruction is required, redacted, threaded to root `SpawnPayload` |
| 7 | MCP-first reactive (subscribe/wait/self-trigger) | **PARTIAL** — strong foundations, real gaps | subscribe + notify-then-read + self-schedule present; no in-turn wait, no condition predicates, root-only |
| 8 | Intelligence over HTTPS only | **VIOLATED** | `Transport` = `Unix` \| `Tcp{tls}` \| `Vsock` (`intel/client.rs:147-150`); validator admits 4 schemes (`config.rs:1938-1942`) |

**Verdict in one line:** agentd's *substance* — model-free supervisor, MCP-only tools, no arbitrary local execution, unforgeable hierarchical subagents, MCP-first reactivity — is faithfully and, on the exec-safety axis, *more strictly* built than its own RFCs. What is wrong is entirely **transport-shaped and spec-shaped**: unix/vsock is load-bearing exactly where the target demands HTTPS, and RFCs 0014/0015/0020 were written to *forbid* the HTTPS-only direction.

---

#### Principle 1 — Tools ONLY from registered MCP servers
**HOLDS, with an explicit naming decision.**

The model's *task*-tool catalogue is assembled exclusively from connected MCP servers: `build_catalogue` (`runner.rs:419`) iterates each registered server's tools and builds the routing map; `dispatch_tool` (`runner.rs:443`) routes each call by name to the owning server. There is no `fs`/`http`/`shell`/`eval` tool family in the tree. A subagent's authority is a pure whitelist intersection over MCP server+tool names, and `tool_scope` on spawn can only *narrow* (retain), never widen (`mcp/server.rs:2790`).

The self/control tools are a **structurally separate** seam — the strongest evidence for the "name the class" resolution. The `SelfHandler` (`orchestrator.rs:633-676`) advertises `subagent.spawn/status/await`, `a2a.delegate` (feature `a2a`, only if peers declared, `:663`), and — **root-only** (`parent_depth == 0`, `:648-672`) — `schedule/subscribe/unsubscribe`. Plus `resource.read`, which appears **only when MCP resources exist** (`runner.rs:107-110`). None shells out. (Note: `subagent.send`/`subagent.cancel` are **not** in-loop self-tools — they are a *served operator* surface at `mcp/server.rs`, reached over the socket; do not conflate the two namespaces.)

**Resolution:** accept these as a named **"self/control" tool class** distinct from the MCP task-tool catalogue. The two are assembled by different code paths (`build_catalogue` vs the `SelfHandler` merge), so the distinction is real and testable. This satisfies the target's intent ("no general capability library, only MCP tools + agentd's own orchestration primitives").

#### Principle 2 — No local code/command execution (resolves P2-vs-P5)
**HOLDS. No arbitrary exec exists; the spawn tension resolves in the code's favor.**

- The **only** `Command::new` in production is `supervisor/spawn.rs:50` (`Command::new(exe)`), where `exe` is a *parameter* documented "normally `std::env::current_exe()`" (`spawn.rs:41`) and is passed `current_exe()` by the callers (`main.rs:223,474`). So the site does not literally hardcode `current_exe()` — **the invariant is a caller obligation**: `exe` must never become request-derived, or subagent-spawn becomes arbitrary-exec and P2 falls. Freeze that.
- No shell surface: no `sh -c`, `/bin/sh`, `execve`, `dlopen`, `posix_spawn` executable hits.
- The env marker is set as `cmd.env(SUBAGENT_ENV, "1")` where **`SUBAGENT_ENV = "AGENT_SUBAGENT"`** (`protocol.rs:24`) — the actual runtime var is `AGENT_SUBAGENT=1`. The doc-comments in `supervisor/spawn.rs:4` and `subagent/control.rs:4` wrongly say `AGENTD_SUBAGENT` — a harmless but real doc drift (fix in Phase 0).
- The spec'd gated `exec` self-tool (RFC 0001 §9's "strongest trifecta leg") was **never built**; the prior production `StubHandler` fallback was already removed (commit `e48f2f0`).

**P2 vs P5 resolved:** spawning a subagent is re-execing the trusted agentd binary — never a user-supplied argv/shell/artifact. The instruction/objective/tool-scope arrive as a serialized `SpawnPayload` over the child's **stdin pipe**, not as command-line args — *data to a model loop, never code to a shell*. The process-supervision machinery (`supervisor/{spawn,kill,tree,reactor}.rs`, `supervisor/cgroup.rs` — note: cgroup is under **supervisor/**, not subagent/) is the *safety layer around* P5 and must **not** be "cleaned up"; removing it would reintroduce escape risk.

**Two spec consequences:** (1) RFC 0001 §9 / 0005 §3.2 / 0012 §3.6 `exec` prose must be **struck**, not merely left unbuilt (a future contributor could re-derive it). (2) The runtime is already stricter than its RFCs — codify that.

#### Principle 3 — A2A for agent↔agent communication
**CONTRADICTED-BY-DESIGN (transport).**

The A2A *choice* is real: agentd serves the A2A method set (`a2a.SendMessage/GetTask/CancelTask/ListTasks` + streaming `a2a.SendStreamingMessage` **and** `a2a.SubscribeToTask`, `mcp/a2a.rs`) and acts as an A2A client for `a2a.delegate` (`mcp/a2a_client.rs`). A served run genuinely *is* an A2A Task.

But the transport is the direct inverse of the target and **build-conditional**: `delegate()` (`a2a_client.rs:75-97`) has one **always-live `Unix` arm**, a `Vsock` arm gated behind `--features vsock`, and a `#[cfg(not(feature="vsock"))]` error fallback — so on a default build the only live dial arm is Unix, and there is **no `https` arm on any build**. RFC 0020 is titled "A2A over vsock" and states agentd "may never implement an HTTP server," re-framing A2A onto raw NDJSON over vsock behind an on-node HTTP↔vsock gateway. Under the target that gateway and the "no-HTTP-server" constraint are deleted and A2A rides HTTPS+SSE directly.

#### Principle 4 — A2A as operator/control mechanism (agentctl)
**CONTRADICTED-BY-DESIGN, and not unified.**

The concept is present: operator tools (`drain`/`lame-duck`/`pause`/`resume`/`cancel`) and A2A task methods are both gated to `PeerOrigin::Management`. Two problems:

1. **Wrong transport, by design.** `ServeTarget` has exactly two variants — `Unix` and `Vsock` (`config.rs:292-345`) — and there is **no HTTPS MCP/A2A/control serve path** anywhere: the reusable crate exposes only `spawn_accept_unix`/`spawn_accept_vsock` (`mcp/server.rs:1221,1252`) and `net/tls.rs` is **client-only** (`connect` at `:73`; no `accept`, no `ServerConfig`). `PeerOrigin::Management` is derived purely from "arrived on the listener," not from authenticated identity.
   - **Scope the claim precisely:** agentd *does* run a plaintext HTTP listener in production — the observability probe (`obs/serve.rs:36`, `TcpListener::bind`). It is GET-target-only (no header/body parse, `read_request_target` at `:164`) and is not control/A2A, but it means an HTTP-over-TCP accept pattern already exists in-tree. The correct claim is "no **HTTPS MCP/A2A/control** serve path," and that pre-existing TCP accept *strengthens* the "mostly plumbing" thesis.
2. **Control isn't unified under A2A.** Two control surfaces exist: an operator profile dispatched via `tools/call` name-match gated on `origin == Management` (`mcp/server.rs`), and A2A as a *separate dotted-method family* (`a2a.*` → `dispatch_a2a`). These are structurally different call conventions; unifying them is a real wire-contract change, not a trivial merge.

**Live control today is signal+file — but feature-gated.** `SIGTERM → drain` is **unconditional**. `SIGHUP → Config::reload` is gated on `feature="hot-reload"` (else SIGHUP terminates by default). The inotify `--watch-config` thread is gated on `feature="config-watch"` (`mode.rs:382`, `#[cfg(all(unix, feature="config-watch"))]`). So on a default build only SIGTERM-drain exists; reload/watch are opt-in. The load-bearing conclusion survives: external control of the reactive daemon is signal+file (on enabled builds), not A2A-over-HTTPS. The internal parent↔child `ControlMsg` channel over stdio pipes is fine and should stay stdio; only the *external* control entry must move to HTTPS.

#### Principle 5 — Hierarchical subagents via internal tools + A2A
**HOLDS (internal-tools half excellent; A2A half inherits P3).**

`subagent.spawn` mints a child `SpawnPayload` at depth+1 with a monotonically **narrowed** scope (child servers ⊆ parent's); **depth is minted by the supervisor, never trusted from the child** (`supervisor/tree.rs`); fork-bomb caps (depth/children/total/tree-token + a live **spawn-rate `TokenBucket`**) are **refused-as-tool-result, never crashes** (`orchestrator.rs:268,369` `spawn_bucket.try_take()`). The `TokenBucket` type is defined in `supervisor/tree.rs:22`, imported at `orchestrator.rs:34`, constructed `:134`. Hierarchical control fans down the tree; teardown is deepest-first with PDEATHSIG + killpg + cgroup.kill. Async runs collectible by handle via `subagent.await`/`status`. The A2A leg exists but rides the wrong transport (P3).

**Note MEMORY.md drift:** the "no spawn-rate bucket" note is wrong — the bucket is live (`orchestrator.rs:95,134,268`); `report.rs:63` `Refusals.rate` counts its refusals. Reconcile the memory note.

#### Principle 6 — Follows provided instructions
**HOLDS.** Instruction is required (empty ⇒ exit 2), redacted in logs (length only), threaded into root `SpawnPayload.instruction`, becomes the first user turn. Config precedence validated once at startup. No rewriting, no injection.

#### Principle 7 — MCP-first reactive
**PARTIAL — strong foundations, three real gaps.**

Present: server-push MCP resource subscriptions with notify-then-read; client-side reactive routing (subscribe → `drain_notifications` → `read_current` → debounced routing); self-scheduling and self-subscribe (**root-only**, `parent_depth == 0`, `orchestrator.rs:648-672` — subagents cannot self-schedule/self-subscribe). The `net` crate already has the streaming substrate (`send_streaming` + `SseReader::next_event`, `net/http.rs`).

Gaps against "WAIT for conditions to *continue work*":
1. **No wait-within-a-turn.** `run_turn` (`runner.rs:184`) is synchronous; waiting happens only *across* runs.
2. **No condition predicates** on subscriptions/routes — any update fires.
3. **Self-schedule/self-subscribe honored only under a daemon**; `--mode once` silently drops them; they are root-only.

Also, the A2A/cluster client waits by **fixed-interval polling** (`GetTask`), not by subscribing — exactly what an HTTPS+SSE binding fixes. P7 is the strongest match in intent, most incomplete in the "wait for a condition" dimension, and the natural seat for the run-graph (directive C).

#### Principle 8 — Intelligence over HTTPS only
**VIOLATED.** `Transport` is a three-way union — `Unix(String)` | `Tcp{host,port,tls}` | `Vsock{cid,port}` (`intel/client.rs:147-150`), with `unix:` always-compiled. The validator `validate_one_intelligence_uri` admits **four** schemes — `https://`, `unix:`, `vsock:`, and plaintext `http://` ("dev only") (`config.rs:1938-1942`). The mock LLM is a `UnixListener` reached only over `unix:`, and many tests dial `unix:` intelligence.

Positive: `tls` is the **default** feature (`Cargo.toml:25`) and the SSRF guard exists (`net/ssrf.rs`, but **zero non-test call sites** — dead today; keep it). The collapse to HTTPS-only is *mechanical*: the wire is HTTP-over-any-`Read+Write`, so removing the non-TLS arms leaves `resolve()` a plain https `Url::parse`; failover/breaker/discovery are transport-agnostic and survive. **Caveat the drafts missed:** intelligence has **no mTLS client-identity wired** — `connect_tls` passes `None` for identity (`intel/client.rs:~397`); if mTLS to the model endpoint is in scope, that is additional work.

---

### Does it match "the whole idea of agentd"?

**Yes on the philosophy; no on the transport skin — and the specs actively fight the target.**

The idea is intact and, on exec-safety, better-built than the RFCs: a model-free supervisor owning the process tree, triggers, and limits, with the ReAct loop only in re-exec'd subagents; tools only from MCP servers; no arbitrary local execution and no `exec` tool. On principles 1, 2, 5, 6 the implementation is faithful; on 7 the foundations are strong.

The transport skin is the inverse of the target and load-bearing: every place the target says HTTPS — intelligence (P8), A2A (P3), operator control (P4) — the code says unix/vsock, and (on the *serving* side and the intel/A2A-*client* dialers) *only* unix/vsock. **The MCP *client* is the one exception the verdicts must not overstate: it already speaks `https://` over TLS** (`mcp/http.rs`, `McpEndpoint::Tcp{tls:true}`; `config.rs:435` "reached over Streamable HTTP, no local process spawn"). So the client side is largely there; the real build-out is the **serving** side. RFC 0014/0015/0020 architecturally *exclude* the target — they must be **superseded**, not spot-patched, and the code must not be "fixed back" toward vsock by anyone trusting the RFC prose.

The fix is mostly **subtraction** (the gateway indirection, two transport arms, the NDJSON-re-framed-as-SSE hack all vanish) plus one real build-out (an authenticated HTTPS serving path reusing the already-present `auth.rs`/`oauth.rs`).

---

## Change plan

Ordered, file-level, green at each step. Two hard ordering constraints: **Phase 0.2 (mocks → TCP) before Phase 1 (remove unix intel)**, and **Phase 2 (build HTTPS server) before Phase 3 (delete unix/vsock use)**. Additive-before-destructive throughout.

### PHASE 0 — De-risking prep (no behavior change)

**Step 0.1 — Fix stale/contradictory docs + env drift (zero risk).**
- `subagent/control.rs:4` and `supervisor/spawn.rs:4` say `AGENTD_SUBAGENT`; the constant is `SUBAGENT_ENV = "AGENT_SUBAGENT"` (`protocol.rs:24`). Align all three. (There is **no** `subagent/spawn.rs` — the file is `supervisor/spawn.rs`.)
- `config.rs:9-11` module doc says the config-file layer is "intentionally not built," but `config_file.rs` exists — confirm it is wired (it is recent), then rewrite the doc to describe the RFC 0017 layer. USAGE `--mcp name=cmd` → `name=endpoint`.
- Memory-only fix: reconcile "no spawn-rate bucket" — the `TokenBucket` is live (`supervisor/tree.rs:22` def; `orchestrator.rs:95,134,268` use).
- **Acceptance:** `cargo build && cargo test` unchanged/green.

**Step 0.2 — Move mocks off unix to loopback TCP (safety net BEFORE any removal).**
- Port `intel/mock.rs` (`UnixListener`) and `mcp/mock_http.rs` to `TcpListener` on `127.0.0.1:0`. Update `--internal-mock-llm`/`--internal-mock-mcp-http` handling in `main.rs`.
- **Reference:** `crates/agentd/tests/mcp_http.rs` already implements a full loopback-TCP HTTP-MCP server (request line + headers + body, `application/json` **and** `text/event-stream` SSE) — copy that pattern rather than the GET-only `obs/serve.rs`.
- **Acceptance:** all tests pass over `http://127.0.0.1:PORT`; no `unix:` in mock modules.

### PHASE 1 — Intelligence HTTPS-only (Directive B; self-contained, no server needed)

Intelligence scheme handling lives in **three** admission points, all edited across 1.1–1.3 (there is no single "edit-once" authority):

**Step 1.1 — Collapse the intel `Transport` enum.**
- `intel/client.rs`: remove `Transport::Unix`/`Vsock` (`:148,150`); in `resolve()` (`:329`) delete the `unix:`/`vsock:` strip branches; in `connect()` delete the `Unix` arm (`:377`) and `Vsock` arm + `connect_vsock`; rewrite the `#[cfg(not(tls))]` error that advises "use unix: to a sidecar."
- **DECISION:** keep plaintext `http://` for loopback dev, or reject entirely? *Recommended default: keep `Tcp{tls:false}` only for loopback (127.0.0.1/::1), reject non-loopback plaintext at config.* (The validator already permits `http://` today, `config.rs:1942`.)
- **Acceptance:** `resolve_unix`/`resolve_vsock` unit tests deleted; `resolve()` accepts only http(s); `cargo test -p agentd intel::` green.

**Step 1.2 — Simplify endpoint scheme reporting.**
- `intel/endpoints.rs`: `scheme_and_addr()` (`:300`) reduce to https/http; drop unix/vsock arms; rewrite the vsock/unix list + redaction tests.

**Step 1.3 — Restrict intel validation to HTTPS.**
- `config.rs`: `validate_one_intelligence_uri` (`:1938`) accept only `https://` (+ loopback `http://` per 1.1); update message + `--intelligence` help. This is the shared startup/`--validate-config`/reload gate.
- `capabilities.rs`: `transport_scheme` (`:190`) collapses to https.
- **Acceptance:** `agentd --validate-config` with `unix:`/`vsock:` intel exits 2; `https://` exits 0.

**Step 1.4 — (Optional now) note mTLS-to-model gap.**
- Intelligence still passes `None` client-identity (`intel/client.rs:~397`). **DECISION:** is mTLS to the model endpoint in scope? *Recommended default: out of scope for the transport pivot; track separately.*

### PHASE 2 — Build the HTTPS served-MCP + A2A transport (enabler; additive, old path stays green)

**Step 2.1 — Add a TLS *server* acceptor to `net`.**
- `net/tls.rs` (+ `Cargo.toml`): add `accept(tcp, ServerConfig) -> TlsStream` (cert/key via already-present `rustls-pemfile`), optional `require_client_auth` for mTLS. `webpki-roots` is client-trust only; add a client-CA roots loader for mTLS verification.
- **Acceptance:** loopback TLS handshake round-trips; `cargo test -p net --features tls` green.

**Step 2.2 — Add a Streamable-HTTP *server* to the `mcp` crate.**
- New `mcp/src/http_server.rs`: `bind_tcp` + `spawn_accept_http` reusing the existing `Handler`/dispatch/`SubRegistry`/`SharedWriter`; only **framing** changes (parse HTTP/1.1 request, `POST` body → JSON-RPC → `application/json` unary or `text/event-stream` for `notifications/*` + A2A streaming). Add a `ServeStream::Http` writer so subscription notifies emit SSE `data:` lines.
- **Reference correction:** model the POST-body/header parsing on the **mcp client encoder (`mcp/src/http.rs`)** and the **existing test SSE-server (`crates/agentd/tests/mcp_http.rs`)** — **not** `obs/serve.rs`, which is GET-target-only and parses no body/headers.
- **Moat note:** the `mcp` crate is documented "deliberately minimal, blocking, one-thread-per-conn, no async." An HTTP/1.1 parser + SSE writer is genuinely net-new subsystem, not a "framing swap" — scope accordingly.
- **Acceptance:** in-crate test: `initialize` → `tools/list` → `resources/subscribe` → receives an SSE `notifications/resources/updated`; `cargo test -p mcp --features tls` green; unix/vsock server untouched.

**Step 2.3 — Add `ServeTarget::Https` + agentd wiring (additive).**
- `config.rs`: add `ServeTarget::Https { bind: SocketAddr, tls }` alongside `Unix`/`Vsock` (`:292`); `parse` accepts `https://HOST:PORT` + cert/key/client-CA sources (new flags `--serve-cert/--serve-key/--serve-client-ca`, kept out of logs via the `sec::secret` front door).
- **Correction to a draft claim:** `ServeTarget` is **not** `Serialize` (`config.rs:291` derives only `Debug,Clone,PartialEq,Eq`) and does **not** travel in `SpawnPayload` — the field `serve_mcp: Option<String>` (`config.rs:507`) carries the raw *string*. So enum changes are **not** a wire-format change; `RESTART_ONLY_FIELDS` keys on the field name `"serve_mcp"` and is unaffected.
- `main.rs`: add an `Https` arm in `serve_self_mcp` (`:363`, alongside `Unix` at `:395` / `Vsock` at `:412`) binding via `spawn_accept_http`; derive `PeerOrigin::Management` from auth (DECISION below).
- `Cargo.toml`: add `serve-https = ["tls","mcp/tls","net/tls"]`.
- **Acceptance:** `agentd --serve-mcp https://127.0.0.1:8443 --serve-cert … --serve-key …` answers `initialize`/`tools/list` over HTTPS; unix/vsock still compile; new `tests/serve_https.rs` green.

**Step 2.4 — Move the A2A surface onto HTTPS (this is a REWRITE, not a dial swap).**
- **Correction the drafts under-scoped:** the A2A *client* does **not** use the mcp Streamable-HTTP client. `a2a_client.rs` speaks NDJSON JSON-RPC via `frame::write_line`/`read_line` with JSON-RPC-`id` correlation (`Conn::call`, `:196-234`) over a `CloneStream`-generic `run<S>` (`:105`) whose only impls are `UnixStream` (`:245`) and `VsockStream` (`:255`). Adding `A2aEndpoint::Https` therefore requires **rewriting `Conn::call`/`run` onto the HTTP-POST + SSE model** and providing a new concrete stream type — the same "build, not rewire" magnitude the server side has.
- `mcp/a2a.rs`: `dispatch_a2a` is transport-agnostic; for streaming (`SendStreamingMessage`/`SubscribeToTask`) emit **real SSE** instead of the gateway-reframed NDJSON convention; delete the gateway plumbing.
- Prefer `SubscribeToTask` over the fixed-interval `GetTask` poll.
- `config.rs`: add `A2aEndpoint::Https(url)` (`:381`).
- **Acceptance:** served A2A Task over HTTPS end-to-end (SendMessage → GetTask terminal + a SubscribeToTask SSE stream); `a2a.delegate` to an HTTPS peer.

**DECISION (server framing home):** build the HTTP server in the reusable `mcp` crate (symmetric with the client, reusable) vs a new agentd module. *Recommended default: build it in `mcp` (`mcp/src/http_server.rs`).*

**DECISION (control-plane auth):** `PeerOrigin::Management` must derive from authenticated identity over HTTPS (no implicit trust boundary). *Recommended default: mTLS client-cert as primary (`net/tls.rs` already has `ClientIdentity`/`from_pem`; add server-side client-auth), bearer/OAuth as alternative (`mcp/auth.rs`,`mcp/oauth.rs` exist).*

**DECISION (obs probe):** keep `/metrics /healthz /readyz` plaintext (`obs/serve.rs:36`)? *Recommended default: keep plaintext but bind loopback-only or document NetworkPolicy; out of scope for the control plane.*

### PHASE 3 — Remove agentd's USE of unix/vsock (Directive A)

The reusable `mcp`/`net` unix+vsock modules **stay** (explicitly allowed); only agentd's *use* and its dependency feature go.

**Step 3.1 — Remove unix/vsock serve arms from agentd.**
- `config.rs`: delete `ServeTarget::Unix`/`Vsock`, `VMADDR_CID_ANY`; `parse` accepts only `https://`.
- `main.rs`: delete `serve_self_mcp_vsock` (`:420`) and the `Unix`/`Vsock` arms (`:395,412`).
- `mcp/server.rs`: replace agentd callers of `spawn_accept_unix` (`:1221`)/`serve_vsock` (`:1252`) with `serve_https`.
- **Acceptance:** `--serve-mcp unix:/x` / `vsock:…` exit 2; `tests/serve_mcp.rs` rewritten to HTTPS.

**Step 3.2 — Remove unix/vsock A2A endpoints — ORDERING HAZARD.**
- `config.rs`: reduce `A2aEndpoint` to `Https(url)`.
- `a2a_client.rs`: delete `Unix`/`Vsock` `delegate` arms (`:75-97`) and **both `CloneStream` impls** (`:245,255`).
- **Hard dependency:** after deleting both `CloneStream` impls there is **no concrete stream type left for `run<S>`/`Conn`** — the crate stops compiling unless the HTTPS `Conn` from **Step 2.4 has fully landed first**. State this ordering explicitly in the work order.
- **Acceptance:** `--a2a-peer name=unix:/x` exits 2; `https://` works.

**Step 3.3 — Restrict MCP *client* endpoints to HTTPS at agentd config — HARDEN BOTH GATES.**
- Two distinct sites: (a) `is_mcp_endpoint` (`config.rs:461`) inside `parse_mcp_spec` — tighten to `https://` (+loopback `http://`); (b) the per-server startup validation at `config.rs:1418` that calls `::mcp::http::McpEndpoint::parse` — the **reusable crate parser we must not touch**, which still admits `unix:`/`vsock:`. Add an agentd-side pre-check at `:1418` rejecting non-http(s) schemes *before* calling the crate parser, else `--mcp name=unix:/x` will **not** exit 2 as the acceptance requires.
- **Acceptance:** `--mcp name=unix:/x` exits 2; `https://` works.

**Step 3.4 — Drop the `vsock` feature/dep; retire `serve-mcp`(unix) → `serve-https`.**
- `Cargo.toml`: remove `vsock = [...]` (`:33`) + the optional `vsock` dep (`:114`); replace `serve-mcp` (`:36`) with `serve-https`; **retarget the real edges: `a2a = ["serve-mcp"]` (`:40`) and `events = ["serve-mcp"]` (`:54`) → `serve-https`.** *Correction:* `cluster = []` (`:63`) has **no** serve-mcp in its list — do **not** treat `:63` as a serve edge; verify the capacity-resource cfg-gates separately.
- **Coupled edit set (must move in lockstep to keep the manifest test green):** `capabilities.rs:75` (`build_features` vsock arm), `:190` (`transport_scheme` vsock), `:458` (`build_features_reflects_cfg` test), and `intel/endpoints.rs:300` (scheme reporter) — plus the `contract_version` bump (frozen public contract parsed by agentctl).
- **Acceptance:** default + `--features "serve-https,a2a,cluster,events"` build green; `grep vsock crates/agentd/src` returns only comments referencing the reusable crates; `cargo test --workspace` green.

**Step 3.5 — Rebase `PeerOrigin::Management` onto authenticated identity.**
- For the HTTPS server, set `Management` only after auth passes (mTLS/bearer); unauthenticated HTTPS peers are denied. The operator-tool gate (`mcp/server.rs` `tools/call` name-match) and the A2A gate (`dispatch_a2a`) key off the newly-minted authenticated origin — closing the "transport is the boundary → no auth" hole.
- **Acceptance:** unauthenticated A2A/operator call → 401/`-32601`; authenticated → success.

### PHASE 4 — agentctl HTTPS control contract

**Step 4.1 — Define the HTTPS control endpoint.** agentctl connects to `https://<host>:<port>` and speaks (i) A2A task methods and (ii) operator/admin methods, both auth=Management. The pod exposes ONE HTTPS port (replacing the vsock-gateway isolation posture — confirm the NetworkPolicy/mTLS story before deleting isolation). `agentd://` self-resources are served + subscribable over the same listener.
- **Acceptance:** end-to-end control cycle over HTTPS (discover → `subagent.spawn` via A2A SendMessage → subscribe `agentd://run/{id}` over SSE → `drain`); `contract_version` bumped and re-verified against agentctl.

**Step 4.2 — DECISION (unify control under A2A) — this is a wire-contract change, not "optional consolidation."** Operator tools are `tools/call` name-matched (`mcp/server.rs`); A2A is a separate `a2a.<Method>` family (`dispatch_a2a`). Folding operator tools into A2A-adjacent admin methods changes the public JSON-RPC surface agentctl parses → `contract_version` bump + agentctl coordination. *Recommended default: land the transport move first (both families on one HTTPS listener); treat unify-under-A2A as an explicit, separately-approved wire change.*

### PHASE 5 — Tools reconciliation + reactive gaps

**Step 5.1 — Classify the self/control tools (decision + small marker).**
- Add a `ToolClass::{Mcp, SelfControl}` tag (or doc note) at the two seams: the MCP catalogue (`runner.rs:419` `build_catalogue`) and the self-tool dispatch (`orchestrator.rs:633-676` — the def push at `:643`/`:649-651` and match at `:658-672`).
- **DECISION:** name a "self/control" class (a) vs re-expose them via a self-MCP server (b). *Recommended default: (a) — honest, doc + marker only; (b) is a large refactor for little gain.*
- **Acceptance:** a test asserts the catalogue = (MCP-server tools ∪ the fixed self-control set), **accounting for the conditional `resource.read`** (present only when resources exist, `runner.rs:107-110`) and the **root-only** `schedule/subscribe/unsubscribe`; no tool resolves to local exec.

**Step 5.2 — Strengthen reactive: condition predicates + in-turn wait.**
- Add an optional condition predicate to a `Route`/subscription (structured JSON match) in `triggers/router.rs`; add a `subagent.await_resource`/`wait_until` self-tool that ends the turn and re-enters on the matching update (reuse warm continue-sessions); wire `notifications/tools/list_changed` re-enumeration.
- **Acceptance:** e2e: agent subscribes with a condition, daemon fires only on match, warm session re-enters with the changed resource; `--mode once` errors clearly on deferred effects or documents the drop.

**Step 5.3 — DESIGN-ONLY run-graph (directive C).** See Section 3; draft as a design RFC, defer implementation.

### PHASE 6 — RFC + docs rewrite (coordinated)

**15 RFC files reference vsock** (`grep -l vsock rfcs/*.md`): 0001, 0002, 0005, 0006, 0009, 0011, 0012 (via cross-ref), 0013, 0014, 0015, 0016, 0017, 0018, 0019, 0020, + README. The drafts named ~10; the coordinated pass must **also** include 0002, 0009, 0011, 0013, 0016, and README.

**Supersede (they forbid the target):**
- **RFC 0015** — remove "make vsock bidirectional"/"v1 management is vsock+unix only"; replace with HTTPS control + auth (biggest rewrite).
- **RFC 0020** — supersede "A2A over vsock"; delete "agentd may never implement an HTTP server," the HTTP↔vsock gateway/PEP, and the vsock trust reasoning; bind A2A to HTTPS+SSE.
- **RFC 0014** — rebase control-plane onto HTTPS; drop/replace the "pod with no cluster network" posture.
- **RFC 0006** — strike unix-as-core + vsock + legacy framed wire; HTTPS-only intelligence.
- **RFC 0018** — keep failover/breaker/discovery; validator https-only.
- **RFC 0012 §3.6/§3.8, RFC 0001 §9, RFC 0005 §3.2** — strike the `exec` self-tool; retire "transport is the boundary / no auth"; keep the `agentd://` scheme.
- Cross-ref ripple: 0002, 0009, 0011, 0013, 0016, 0017, 0019, README.

**Docs:** `docs/{mcp,configuration,intelligence,deployment,security,modes-and-triggers}.md`, `README.md` — remove all unix/vsock examples; document the HTTPS control surface, cert/key flags, auth model. Invert `net`'s steering prose (`net/src/tls.rs` "most builds link none of this [TLS]" → TLS is the default); feature-gate `net::unixsock` (currently unconditionally compiled) behind a `unix` feature so an HTTPS-only agentd doesn't link a unix dialer (crate keeps the capability).

**Acceptance:** no RFC/doc prescribes unix/vsock for intelligence/control/A2A as the target; superseded RFCs marked historical with a pointer to replacements.

### Cross-cutting guardrails
- **Keep** `crates/mcp`/`crates/net` unix+vsock modules (reusable). Only remove agentd's *use* + feature.
- **Do NOT touch** process supervision (`supervisor/{spawn,kill,tree,reactor}.rs`, `supervisor/cgroup.rs` — under **supervisor/**) — the P5 safety layer, not a P2 vector.
- **`exe` must never become request-derived** (`spawn.rs:50` takes a param; caller passes `current_exe()` at `main.rs:223,474`).
- **Token-never-logged** invariant through the auth refactor — cert/key/bearer via `sec::secret`, never logged.
- **Keep** `net/ssrf.rs` (dead, zero non-test call sites) — wire `guard_host(host, allow_private=false)` at any agent/peer-supplied URL surface, or annotate as deliberately-latent. There is **no** `mcp/ssrf.rs`.
- **Preserve** the management-timeout ≤ liveness-window coupling (`obs/health.rs` const assert) and the transport-orthogonal resilience machinery (breaker, sticky-primary, FNV-1a shard hash) through the pivot.

---

## Run-graph extension (design)

**Design only (directive C). No implementation.** Feature-gated (`run-graph`, default off): absent ⇒ agentd is byte-for-byte today; present ⇒ the degenerate single-`Agent`-node graph reproduces today's behavior (the correctness anchor). Serde-only, no new deps.

**Thesis:** agentd already *is* an implicit single-node graph executor — the ReAct loop is a hard-coded cycle, the reactive router is an event→action edge set (`Disposition::{Spawn,Continue}`), self-schedule is a delayed self-loop, and self-subscribe is the agent adding an edge at runtime. The run-graph reifies this into an explicit serde `Graph` the agent authors, driven by a thin driver reusing `Session::run_turn`, `Budget`, `TerminalStatus`, and the `Router`. It adds exactly two genuinely-new node kinds: an explicit **condition/branch** and an explicit **wait-on-resource**.

### (a) Fit onto existing primitives — with the two corrections the drafts got wrong

| Concept | Reuses | Correction |
|---|---|---|
| `Agent` node = one bounded reasoning step | `Session::prepare` (`runner.rs:99`) + `run_turn` (`runner.rs:184`) | run_turn is `:184`, not `:200` |
| `Tool` node = one MCP tool, no model | `dispatch_tool` (defined `runner.rs:443`) | **`dispatch_tool` is private and takes routing built privately in `prepare`; a `Tool` node needs NEW public surface** (pub the fn or add `Session::call_tool_by_name`). Same for `resource_read_tool` (`runner.rs:562`, private). |
| `Branch` node | `Router::best_match` exactly-one-owner discipline + new predicate evaluator | — |
| `Wait` node | `Router::add_route`/`remove_exact`/`due` + `SubscriptionRequest` | — |
| Cyclic re-entry | `ScheduleRequest` (`after_ms=0` = now) + daemon re-invoke | — |
| Cycle termination | `Budget` + `TerminalStatus` | see (d) |
| `Subgraph` node | `Orchestrator::spawn`/`spawn_async` + caps | crosses a **process** boundary (see below) |

**The load-bearing correction on suspend/resume (process boundary).** The drafts claimed the daemon "re-enters `drive_slice`" and that "`Disposition::Continue` delivers to `Session::deliver`." **Both are false.** `Continue` delivers into a **separate warm subagent PROCESS** via IPC: `run_reactive` → `warm.deliver()` → `ControlMsg::Inject` over the control channel (`triggers/warm.rs`). `Session::deliver` is called **only inside the child**, in `wait_for_inject` (`subagent/control.rs`). The daemon holds no live `Session` — it owns child processes + mpsc channels (`Warm { sub: Subagent, rx }`, `triggers/warm.rs:34-37`).

Therefore the **graph driver must live in the CHILD (subagent) process** alongside `run_turn`. A `Wait` node works exactly like a warm continue-session: the child suspends; the **daemon** owns the ephemeral `Router` route (verified real — `add_route`/`remove_exact` in `triggers/mode.rs`) and on the matching `Delivery` does notify-then-read and re-delivers via `ControlMsg::Inject`; the **child** receives it in `wait_for_inject` and re-enters `drive_slice`. This is a real cross-process integration, **not** "the only new code is a retarget."

### (b) Graph model (serde-only)

- **Blackboard** = `BTreeMap<String, serde_json::Value>` of last-written structured node outputs + a bounded visit `trail`; bounded keys/value-size (mirror `DISTILL_CAP`), eviction refused-as-error.
- **Nodes** (`#[serde(tag="kind")]`): `Agent{instruction, output_contract?, reads, writes, limits?}`, `Tool{server, tool, args, writes}`, `Branch{cases, default, semantic?}`, `Wait{on_uri, writes, timeout_ms}`, `Subgraph{graph, async_, writes}`, `Halt{status: TerminalStatus, result_from}`.
- **Edges** = per-node `BTreeMap<EdgeLabel, NodeId>` (back-edge = target is an ancestor; cycles by construction). Every node emits a well-known label set (`ok`/`error`/`exhausted`/case-labels/`updated`/`timeout`); an implicit `→ Halt(Crashed)` safety sink makes a mis-authored graph fail closed.
- **Correction (`resume_point` inconsistency):** the draft's `Graph` struct omitted `resume_point` but the driver read `g.resume_point`. The frozen `Graph` wire type must stay **pure topology**; the resume point + blackboard + budget live on the **persisted slice state**, not on `Graph`.

### (c) Conditions over structured tool results + intelligence (two-tier)

- **Tier 1 — deterministic JSON predicates (free).** `Pred` (`#[serde(tag="op")]`): `Eq/Ne/Lt/Gt/Exists/Contains` over a `blackboard_key + RFC-6901 json-pointer` path (uses built-in `serde_json::Value::pointer` — zero new deps) + `All/Any/Not`. Total & cheap: missing path ⇒ false; first matching case wins; no match ⇒ `default`.
- **Tier 2 — semantic branch (opt-in).** `SemanticBranch{reads, prompt, labels}` runs **one** intelligence call. **Correction:** there is no `IntelClient::classify` — the only entry is `complete(&self, req)` (`intel/client.rs:223`); the mechanism is a single `complete()` with an empty tool list, and label-constraint is **prompt-only with a `default` fallback** (`run_turn` treats no-tool-call text as the final answer, `runner.rs:344-352`), **not** an enforced constrained decode. State that plainly. Tokens charged to the graph budget.

### (d) Cycle termination (four layers, fail-closed)

1. **Graph `Budget`** — reuse `supervisor::budget::Budget` with `max_steps` = node-visits, `max_tokens` = tree ceiling, `deadline`. Checked at the head of `drive_slice` as in `run_turn` (`runner.rs:226`). Runaway ⇒ `ExhaustedSteps/ExhaustedTokens/Deadline`. Tree-token rollup (`charge_tokens` at `tree.rs:271`) bounds subgraphs — **correction:** `KillReason::TreeBudget` lives in `supervisor/reactor.rs:64/68` (not tree.rs; tree.rs has `SpawnRefused::TreeBudget`), and `main.rs:551` maps TreeBudget → `ExhaustedTokens`.
2. **Per-node visit cap ⇒ `LoopDetected`** — first real producer.
3. **Progress guard ⇒ `Stalled`** — blackboard-hash unchanged across a cycle.
4. **Author-time structural validation** — `start` exists; every emitted label has a target; `Wait.timeout_ms ≤ deadline`; MAX_NODES/EDGES/KEYS; **at least one `Halt` reachable from `start`** (back-edges fine; no-exit rejected).

**Correction (spec edit, not free reuse):** `stop.rs:28-31` documents `Stalled` = "output content-hash unchanged for N turns" and `LoopDetected` = "a single tool repeated past cap K" — **turn/tool-level** semantics for the deferred v2 intra-turn detectors (`runner.rs:9-11` "defined but not yet produced"). This design **redefines** them at **graph** granularity. Either own that as an intentional RFC 0007 §3.4 edit, or introduce distinct graph statuses (both currently map to `PARTIAL` at `exit.rs:67`). The only in-tree `Stalled`/`LoopDetected` reference today (`mcp/a2a.rs:659-660`) is a **test-only** classification table (`terminal_to_state`), not a producer — the runner only assigns `Cancelled`/`Completed`.

### (e) How the agent defines the graph

New self-tools on the `Orchestrator` (same seam as `subagent.spawn`, gated `--features run-graph`): `graph.define{graph}` (validate + store, refuse-as-result on invalid), `graph.run{graph_id}`, `graph.patch{...}` (**additive-only** — add nodes/edges, matching `Router::add_route`; no edge-retargeting mid-run, to preserve reachability/termination). A thin secondary `--graph <file.json>` operator entry loads the identical serde `Graph` for pinned/deterministic DAGs. New `--mode graph` arm entered where `run_reactive`/`run_scheduled` are.

### (f) Waits/subscriptions integration

A `Wait` node **suspends** (does not block a thread). The child returns a suspend outcome; the **daemon** installs an ephemeral exact `Router` route + a `ScheduleRequest`-style timeout timer; on `resources/updated{on_uri}` it does notify-then-read, writes the value to the blackboard, `remove_exact`, and re-delivers via `ControlMsg::Inject` so the child re-enters `drive_slice` at the `updated` edge (or `timeout` edge on expiry). A back-edge into a `Wait` = a long-lived reactive loop that costs nothing idle ("idle is healthy," `obs/health.rs`).

**Correction (no persistence today):** the draft claimed suspended state "is serialized into the run's persisted state … the warm-session machinery already carries [it] between events." **False** — a warm session is a **live in-memory** process handle + channel (`triggers/warm.rs:34-37`); nothing is written to disk. A multi-hour `Wait` therefore needs **new durable storage** (e.g. an `agentd://graph/<id>` resource or a state file) — a genuine new-machinery design decision, not reuse.

### Minimalism / moat + directive-A note

No new deps (serde + `serde_json::Value::pointer` only); no new transport, no exec, no control-plane change. **But** downgrade the "~4 files, delegates only" claim: `Tool`/`resource.read` nodes require **new public surface** on `Session`, and `Wait`/`Subgraph` resume crosses the subagent **process boundary** via `WarmRegistry`/the control channel. Also, the "(HTTPS-target) transports" asides are **forward-looking, not current** — today agentd still ships vsock/unix (`mcp/server.rs` serve_vsock, `net::vsock`, `intel/endpoints.rs`) and a stdio subagent control channel. The graph **inherits whatever transport the directive-A/B cleanup lands** and adds none of its own — that is the true, defensible claim, and it is a **dependency on directive A landing first**.

### Incremental delivery
- **P0** — model + validation + `graph.define` (validate/store only); round-trip tests. Freeze the `Graph` **topology-only** wire type after this.
- **P1** — linear driver: `Agent`+`Tool`+`Halt` (add the `Session` public surface here), `GraphBudget→Budget` bridge, blackboard threading. E2E: 3-node ETL.
- **P2** — Tier-1 conditions + cycles + termination (`LoopDetected`/`Stalled` first producers; own the semantic redefinition).
- **P3** — Tier-2 semantic branch (single `complete()`, prompt-only constraint).
- **P4** — `Wait`/subscriptions: child-side suspend + daemon-side ephemeral route + `Inject` resume; **decide durable-blackboard storage** (required, not a footnote).
- **P5** — `Subgraph` (sync/async via spawn, caps inherited) + additive `graph.patch`.
- **P6 (optional)** — `--graph <file>` + `--mode graph` + `agentd://graph/<id>` self-resource.

**Open design questions for the RFC:** (1) durable blackboard across a long `Wait` (new storage — promote to required); (2) transcript strategy in cyclic `Agent` nodes (fresh `Session` per visit vs opt-in warm node); (3) `graph.patch` additive-only; (4) Tier-2 nondeterminism doc norm; (5) whether a node may delegate to a remote **A2A peer** via `a2a.delegate` (`orchestrator.rs:365`, caps already cover it — natural given directives 3/4).

---

## Open decisions

Consolidated; each with a recommended default. Resolve the first four before Phase 2.

- **DECISION (HTTPS server framing home):** build the Streamable-HTTP server in the reusable `mcp` crate vs a new agentd module. *Default: in `mcp` (`mcp/src/http_server.rs`) — symmetric with the client, reusable.*
- **DECISION (control-plane auth):** how `PeerOrigin::Management` is derived over HTTPS. *Default: mTLS client-cert primary (reuse `net/tls.rs` `ClientIdentity`, `mcp/auth.rs`/`oauth.rs`), bearer/OAuth alternative.*
- **DECISION (keep self-MCP vs unify control under A2A):** whether to fold the operator profile into A2A (Phase 4.2). *Default: move transport first (both families on one HTTPS listener); treat unify-under-A2A as a separately-approved wire-contract change with a `contract_version` bump.*
- **DECISION (obs probe transport):** keep `/metrics /healthz /readyz` plaintext (`obs/serve.rs:36`)? *Default: keep plaintext, bind loopback-only or document NetworkPolicy; out of scope.*
- **DECISION (plaintext `http://`):** allow loopback-only `http://` for dev intel/MCP, or reject all non-https. *Default: allow loopback-only `http://`, reject non-loopback plaintext at config.*
- **DECISION (mTLS to intelligence):** wire client-identity for intel (`intel/client.rs:~397` passes `None` today)? *Default: out of scope for the transport pivot; track separately.*
- **DECISION (self/control tool classification):** name a "self/control" class (a) vs re-expose via a self-MCP server (b). *Default: (a) — doc + `ToolClass` marker only.*
- **DECISION (run-graph statuses):** redefine `Stalled`/`LoopDetected` at graph granularity vs add distinct graph statuses. *Default: add distinct graph statuses to avoid overloading two RFC-0007-documented turn/tool-level variants.*
- **DECISION (run-graph durable state):** storage for a suspended graph across a long `Wait` (no persistence exists today). *Default: an `agentd://graph/<id>` self-resource backed by a state file; decide in Phase 4.*