# agentd — Maturity & Production Readiness

**Purpose:** honest, dated snapshot of what's production-ready, what's
pre-production-ready-with-caveats, and what's still aspirational. Read
before deploying.

**Date:** as of **v1.2.0** (2026-06-11); kept current with the release
tags. Reassess quarterly or after any RFC-scale change.

**Scope:** `crates/agentd/` only.

**TL;DR.** The runtime is **production-ready for the deployment
shape it targets** (single-tenant micro-agent, one workflow per
process, standalone or small horizontal fan-out on Linux). Every
posture concern that's a *runtime responsibility* — bounded
execution, policy, TLS / mTLS / OIDC / HMAC, signed workflows,
audit, hot reload, Prometheus, OTLP — is Green with tests. The
remaining Red cells are **shape choices**, not gaps: durability
(use an upstream queue), fleet-wide rate limits (use your LB),
multi-workflow inside one process (run multiple processes),
browser surfaces (front it with a real gateway), secret-fetching
(mount env vars or files from your orchestrator — k8s `Secret`,
systemd `EnvironmentFile`, Vault Agent sidecar, SOPS, etc.).

**When this runtime is the right tool.** You want a small, fast,
single-binary runtime that executes a declarative TOML workflow,
refuses to invent capabilities at run time, and fits in a
container or a systemd unit. You're happy for durability, secret-
fetching, and fleet concerns to live upstream. You expect to
rotate certs / tokens / policy in-place without dropping traffic.

**When to reach for something else.** You need multi-tenant
shells, durable queueing built into the runtime, browser-facing
HTTP (cookies / CORS / WebSocket), HTTP/2 or HTTP/3, or Windows-
native production deployments (Windows compiles but the hot-reload
surface is trimmed — see §1.5).

---

## 1. Readiness by concern

### Legend

- **Green** — covered end-to-end by tests, safe to rely on.
- **Yellow** — works and tested, but has a known sharp edge; call it
  out in your runbook.
- **Red** — not wired, not advertised as working, or a known gap.

### 1.1 Correctness / DAG runtime

| Concern | Status | Notes |
|---|---|---|
| TOML parse + structural validation | Green | `deny_unknown_fields` on every struct; 100+ parser tests. |
| Full validator (acyclicity, reachability, start-node shape, fan-in/out, edge / route / trigger integrity) | Green | Kahn + BFS implementations; tests cover every error code. |
| Node execution dispatch (24 kinds) | Green | All 24 kinds wired and tested. |
| `ExecutionContext::resolve_path` (single dotted-path lookup) | Green | Every node input flows through it; property-like tests in `context.rs`. |
| Condition / switch / merge / fail / terminate control flow | Green | Branch selection, merge barrier, fail propagation all covered by fixture tests. |
| Retry (per-node, `max_attempts` + `backoff_ms` + `on: Any\|Transient`, optional `jitter`) | Green | Bounded; deterministic by default (`jitter = 0.0`). Operator opts in to bounded random jitter `∈ [1-j, 1+j]` (clamp `[0.0, 0.5]`) to avoid thundering-herd retries. See §2.11. |
| Run timeout (`--timeout-secs`) | Green | Enforced at the engine level; short-circuits pending nodes. |
| Process-wide resource budgets | Green | `[budget]` block caps memory (RLIMIT_AS), CPU (RLIMIT_CPU), wall-clock per run, and cumulative fs-write bytes. Applies at startup; audit events on apply / apply-failed / fs_write_denied. Micro-agent (1 workflow/process) = per-process == per-workflow. |
| `--dry-run` mode | Green | Every tool handler respects the flag; no external side effects. |
| Execution trace in the outcome JSON | Green | Stable schema; used by `testing::fixture`. |

### 1.2 HTTP surface (serve mode)

| Concern | Status | Notes |
|---|---|---|
| HTTP/1.1 request parsing (req line, headers, body) | Green | Hand-rolled; 16 KiB header cap, 1 MiB body cap; malformed requests get precise status codes. |
| Routing `(METHOD, PATH)` → start node | Green | Strict exact match; 404 / 405 otherwise. |
| `/healthz` always-live | Green | No auth, not rate-limited. |
| Bearer auth | Green | Constant-time compare; tests cover missing header, wrong prefix, trailing whitespace, empty token. |
| HMAC-SHA256 webhooks | Green | `hmac` + `sha2` crates; optional timestamp skew check; canonical header / prefix configurable. |
| mTLS (required mode) | Green | rustls 0.23 + aws-lc-rs; peer cert fingerprint exposed as principal. |
| TLS termination | Green | PEM loader handles chain + PKCS1/8/SEC1 keys; startup fails loudly on bad cert paths. |
| Rate limit (per-route token bucket) | Green | Validated at spawn; counter drift under contention is acceptable. |
| Per-route idempotency keys | Green | `idempotency_key` replays the recorded response on redelivery (TTL'd, under `--state-dir`); failures stay retryable; concurrent duplicates 409; keyed route without a state dir fails the bind. End-to-end replay test through a real server. |
| Webhook body parsing (JSON / urlencoded / multipart fields) | Green | Content-type-aware; strict decoding fails 400; multipart file parts dropped with an audit note (by design — documents parse upstream or via MCP). |
| `llm_infer` output schema validation | Green (with `schema`) | With the `schema` feature, an `output_schema` that names a file is validated against that JSON Schema, with bounded `output_repairs` re-prompts on failure. Without the feature, `output_schema` enforces valid-JSON only. |
| Graceful drain on SIGTERM/SIGINT | Green | In-flight counter + bounded timeout; exits `0` on clean drain, `5` on timeout. |
| HTTP/1.1 keep-alive | Green | Up to 100 requests per connection; idle-timeout = socket read_timeout (30s). Client opts out per-request via `Connection: close`. |
| HTTP/2, HTTP/3 | Red | Not implemented. HTTP/1.1 keep-alive covers the common-case latency concern. |
| CORS, OPTIONS preflight, cookies | Red | Not implemented. Not a browser-facing surface. |
| WebSocket / SSE | Red | Not implemented. |

### 1.3 Security posture

| Concern | Status | Notes |
|---|---|---|
| Fail-closed policy (fs / env / http / shell / mcp) | Green | Missing `[policy]` means allow-all — appropriate for dev only; audit-log emitted on denial. |
| Policy-as-code (Rego) | Green | `[policy.rego]` layers an OPA-compatible Rego module on top of the static allowlist (AND semantics). Feature `policy-rego`; pure-Rust via `regorus`. Thread-local engines — regorus isn't `Send`. |
| Matcher grammar (`*`, `prefix/**`, `prefix/*`, literal) | Green | Exhaustive tests; fail-closed on unknown syntax. |
| TLS 1.2 + 1.3, modern cipher suites | Green | rustls defaults; `aws-lc-rs` crypto provider. |
| mTLS client identity in the workflow | Green | Injected as `trigger.principal = { kind: "mtls", name: "sha256:<hex>" }`. |
| Bearer / HMAC principal injection | Green | `{ kind, name }`. |
| Peer CN / SAN extraction (mTLS) | Green | `x509-parser` parses CN + DNS SANs out of the peer cert and attaches them to `trigger.principal`. Fingerprint stays authoritative for auth; CN / SAN are advisory for routing only. See §2.7. |
| Audit-event sink (dedicated JSONL file with redaction) | Green | `[logging.audit]` renders a dedicated JSONL sink for target `agentd::audit` with field-level redaction; built-in mask list + operator extension. See §2.2. |
| TLS hot reload | Green | `SIGHUP` rebuilds `rustls::ServerConfig` from the workflow's `[server.tls]` block (cert / key / optional client-auth CA) and swaps via `ArcSwap` without dropping in-flight sessions. See row 1.5 below + §2.4. |
| Secret injection (env-var only) | Green | The supported path is **environment variables**, read at request time by each auth binding. `[auth.bearer.<name>].tokens_env`, `[auth.hmac.<name>].secret_env`, `--intel-http-bearer-file`, `AGENTD_INTEL_HTTP_BEARER`. Rotation is SIGHUP-free — each request re-reads `std::env::var(...)`, so the operator replaces the env var and the next request sees the new value. For non-env surfaces (TLS cert/key, JWKS) SIGHUP re-reads the file contents from disk. The harness embeds **no vendor SDKs**; any KMS / Vault / Secrets-Manager integration lives in the orchestrator (k8s `Secret` + `envFrom`, systemd `EnvironmentFile`, Vault Agent sidecar writing a `.token` file, SOPS-decrypted `.env`, AWS SSM `GetParameter` → env). Declaring a `[secrets]` block in the workflow TOML is an error — the binary rejects it at startup with a pointer to the env-var path. |
| Rate-limit sharing across replicas | Red | Per-process. Use an upstream limiter if fleet-wide limits matter. |
| Workflow signature verification | Green | Ed25519 detached sig over the TOML; fail-closed via `[signing].required` or `--signing-required`. Feature `signing`. See §2.0 above and RFC 0002. |
| OIDC / JWT bearer (OAuth2-compatible) | Green | Validates `iss` / `aud` / `exp` / `nbf` + RSA or ECDSA signature against an operator-pinned JWKS (inline or file). Optional subject allowlist. `none` / HS* explicitly rejected to prevent alg-confusion. Feature `auth-oidc`. Live HTTPS JWKS fetch deferred to v2 — operators rotate externally today. |

### 1.4 Observability

| Concern | Status | Notes |
|---|---|---|
| Structured logs (text + JSON) | Green | `tracing-subscriber` fmt; JSON is OTLP-compatible shape. |
| Per-execution spans + typed fields | Green | `execution_id`, `workflow`, `node_id`, `kind`, `outcome`, `latency_ms`. |
| File / stdout / stderr log targets | Green | Synchronous file writes under Mutex — fine at moderate rates. |
| Workflow `[logging]` config | Green | Precedence: CLI > env > workflow > default. |
| `AtomicU64` metrics counters | Green | Scraped via `GET /metrics` in Prometheus text format; every counter labelled with the workflow name plus an `agentd_build_info` gauge. |
| `/metrics` (Prometheus) endpoint | Green | Always-live on the same bind as `/healthz`; no auth, not rate-limited. |
| Distributed tracing (W3C trace-context propagation) | Green | Inbound `traceparent` parsed + attached to the request span; outbound propagation on `http_request` keeps the trace-id + flags and swaps in this run's fresh span id as the parent (W3C-correct). Direct in-process OTLP exporter lands under the `otel` Cargo feature. JSON-log → filelog-receiver path still works as a zero-dep alternative. |
| Log rotation | Green | `[logging].rotation = "daily" \| "hourly" \| "minutely" \| "never"` rolls `file:` targets without an external cron. `"never"` (default) keeps the hand-rolled writer so external `logrotate` still works. See §2.9. |

### 1.5 Operations

| Concern | Status | Notes |
|---|---|---|
| Single-binary deployment | Green | Static-ish bin, ~6–8 MB. |
| Stateless restart | Green | No cache, no spool, no on-disk state outside workflow-driven writes. |
| Graceful shutdown | Green | SIGTERM/SIGINT → drain → exit. Bounded. |
| Embedded workflow (Mode B) | Green | Build-time validator + `include_str!` bake. |
| External workflow reload (SIGHUP) | Green | SIGHUP rebuilds TLS, prepared auth (+JWKS), policy (incl. Rego), `[[http_routes]]` + rate-limit buckets, intelligence-client bearer, MCP stdio child, and `[policy.mcp]` allowlist — each swapped atomically via `ArcSwap`. Fail-forward: any single component's reload failing leaves the old value live. CLI-arg-shaped bits (bind addr, `--mcp-stdio` command, `--intel-unix` endpoint) still need a restart. See §2.8. |
| Crash-recovery of in-flight runs | Yellow | `--checkpoint-each-node` (with `--state-dir`) snapshots after every node; a crashed run is `--resume RUN_ID` / `--resume-incomplete`-recoverable from its last completed node. At-least-once for the interrupted node (re-runs it). Off by default — an opt-in I/O cost. A `pause_for_approval` node gives declared, human-gated durability. |
| Container / k8s friendliness | Green | `/healthz`, SIGTERM drain, JSON logs. |
| Windows support | Green | Compiles (verified via `x86_64-pc-windows-gnu` cross-build). Shutdown: Ctrl+C / Ctrl+Break via `ctrlc`. Hot reload: `--reload-file PATH` (cross-platform SIGHUP replacement; also available on Unix as a second reload channel). Resource budgets: Job Object-backed memory + CPU caps with `KILL_ON_JOB_CLOSE`. `--intel-unix` is explicitly rejected on Windows (use `--intel-http`). No Windows release bin shipped from this repo — operators build from source; the release-binary CI job targets Linux only. |
| macOS support | Yellow | Compiles and runs; no aarch64-apple-darwin release bin shipped from this repo. |

### 1.6 Integrations

| Concern | Status | Notes |
|---|---|---|
| Outbound HTTPS (`http_request` / loop http tool) | Green (with `tools-http-tls`) | `https://` URLs route through ureq + rustls (the intel-remote stack). Policy allowlist, 1 MiB caps, non-2xx `error` branch, and traceparent propagation all match the plaintext path. Redirects deliberately not followed — the allowlist vetted the exact URL. Real-handshake round-trip tests (rcgen CA + rustls server) in `tools::http::tests::tls_roundtrip`. Without the feature, https fails loudly. |
| MCP stdio client (tools + resources) | Green | Persistent child process; NDJSON JSON-RPC 2.0; allowlist enforced. |
| MCP trigger (inbound) | Green | Works with `trigger-mcp` feature. |
| Intelligence adapter over Unix socket | Green | Length-framed JSON-RPC; any server speaking the shape plugs in. |
| Intelligence adapter over HTTP | Green | `intel-http` feature ships a full plain-HTTP JSON-RPC client with optional bearer auth; 7 unit tests cover happy path, bearer attachment, error surfacing, URL parsing. HTTPS upstreams are deferred — terminate TLS at a sidecar for v1. |
| Multiple MCP servers | Green | `[[mcp_servers]]` TOML block declares any number of named servers, each with its own spawn command + per-server allowlist. `call_mcp_tool` / `read_mcp_resource` nodes route to a target via `server = "name"`; single-server workflows can still omit the field. `--mcp-stdio` stays as a legacy single-server shortcut, mapped to an implicit `{name = "default"}` entry. Per-server respawn on SIGHUP; adding/removing whole entries still requires a restart. |
| OpenTelemetry export | Green | Direct in-process OTLP exporter via the `otel` Cargo feature — pulls tokio + opentelemetry_sdk + opentelemetry-otlp. JSON log stream via `filelog` receiver remains the zero-dep alternative for operators who don't want the ~50-crate dep expansion. See §2.10. |

---

## 2. Named gaps with effort sizing

Everything below is **deliberately deferred**. No work is in flight on
these right now; each entry is a handle to grab if a deployment needs
it.

### 2.0 Supply-chain: workflow signature verification

**Closed.** (RFC 0002 v1). Ed25519 detached signatures over
the workflow TOML bytes; fail-closed when `[signing].required = true`
or `--signing-required` / `AGENTD_SIGNING_REQUIRED=1` is set.
`signing` Cargo feature; off by default. External path looks for
`<config>.toml.sig` alongside the TOML; embedded mode bakes the
signature via `AGENTD_EMBED_CONFIG_SIG` in `build.rs`. v2 follow-up:
ECDSA-P256 + Sigstore keyless via cosign.

### 2.1 `diff_compute` node

**Closed.** Structural JSON diff between two
context values; emits `{added, removed, changed, unchanged}` with
dot/bracket-notation paths. 8 unit tests cover the edge cases
(nested objects, arrays by index, type mismatches, root-scalar
change, identical-values unchanged). See
[`docs/agent/capabilities.md §diff_compute`](capabilities.md).

### 2.2 Dedicated audit-event file sink with redaction

**Closed.** `[logging.audit]` block renders a
dedicated tracing layer keyed on target `agentd::audit`, writing
JSONL with field-level redaction (built-in list + operator extension
via `redact_fields`). Default-redacted names: token, secret,
password, authorization, api_key, bearer, jwt, cookie, session,
reason. Events continue flowing to the main stream in parallel so
ops dashboards aren't affected.

### 2.3 Prometheus `/metrics` endpoint

**Closed 2026-04-23.** `MetricsSnapshot::to_prometheus` emits 0.0.4
text-exposition content; `GET /metrics` is always live alongside
`/healthz`. Every counter is labelled with the workflow name plus an
`agentd_build_info` gauge carrying the crate version. Tests:
`metrics::prometheus_text_is_well_formed`,
`triggers::http::metrics_endpoint_returns_prometheus_text`.

### 2.4 TLS cert hot reload

**Closed.** SIGHUP re-reads the workflow
TOML, rebuilds `rustls::ServerConfig`, and swaps via
`arc_swap::ArcSwap` without dropping in-flight TLS sessions. Also
rotates mTLS client CAs on the same signal. Audit events: `reload.tls`,
`reload.started`, `reload.succeeded`, `reload.failed`. Bad reloads
keep the old config live — fail-forward-then-recover rather than
fail-closed.

### 2.5 Fleet-wide rate limiting

Per-process today. A lightweight Redis-backed token-bucket adapter is
the shortest path, but most deployments should lean on an upstream LB
instead.

**Effort:** ~1–2 days including tests for the distributed case.

### 2.6 HTTP keep-alive / connection reuse

**Closed.** Up to 100 requests per connection;
the socket's existing `read_timeout` (30s) doubles as the
idle-timeout between requests. Clients opt out per-request via
`Connection: close`; the server reflects the decision via
`Connection: keep-alive|close` response header. Engine-level
errors always close the connection (fail-closed on server
misbehaviour). Tests: `keepalive_serves_two_requests_on_one_connection`,
`keepalive_closes_when_client_requests_close`.

### 2.7 x509 principal extraction (mTLS CN / SAN)

**Closed.** `x509-parser` (pure-Rust, added
under the `server-tls` feature) extracts Common Name + DNS SANs from
the peer cert and attaches them to `trigger.principal`:

    {
      "kind": "mtls",
      "name": "sha256:<hex>",   // fingerprint — authoritative identity
      "cn": "svc-a",
      "sans": ["svc-a.internal", "svc-a.prod.internal"]
    }

Fingerprint remains the pinned identity for auth decisions; CN / SAN
are advisory — they drive workflow routing AFTER policy has already
authorised the request. SANs are lowercased for case-insensitive
matching. Malformed DER returns empty CN / SAN rather than
panicking — the fingerprint stays load-bearing even when the parser
can't help.

Tests: `parse_subject_extracts_cn_and_sans`,
`parse_subject_handles_garbage`.

### 2.8 Workflow hot reload — full scope (SIGHUP v2)

**Closed.** Key insight: we never had to
rebuild the engine. Every lifecycle-sensitive component got its
own ArcSwap-wrapped adapter (`ReloadablePolicy`,
`ReloadableIntelClient`, `ReloadableMcpClient`,
`ReloadableMcpAllowlist`, `HttpReloadable`). Tool handlers keep
the exact same `Arc<dyn …>` they captured at registration; the
wrapper's `Policy` / `McpClient` / `IntelligenceClient` impl
defers to whatever the ArcSwap currently points at. Swapping is
lock-free, in-flight calls complete against their pre-swap
snapshot, next call sees the new state.

Reload sequence on SIGHUP (each step fail-forward — a single
failure keeps the old value live, emits `reload.failed
stage=<name>`, and the process stays healthy):

1. **TLS** — rebuild `rustls::ServerConfig` (v1 behaviour).
2. **Auth** — re-parse JWKS + bearer/HMAC bindings (v1).
3. **Policy** — rebuild `ManifestPolicy` (re-compiles Rego,
   re-reads inline data). Per-thread Rego engines self-invalidate
   via the fresh `RegoSpec.id`.
4. **MCP allowlist** — swap from the new `[policy.mcp]` block.
5. **MCP stdio child** — spawn new process, swap, old child
   drops when the last snapshot goes away (kills + waits).
6. **Intelligence client** — rebuild from CLI args; bearer file /
   `AGENTD_INTEL_HTTP_BEARER` env var re-read so rotation picks up.
7. **Routes + rate-limit buckets** — rebuild `HttpReloadable`,
   swap. Token counters reset to full capacity (a policy-rotation
   shouldn't let a flooding client keep their allowance).

Audit events: `reload.started`, `reload.tls`, `reload.auth`,
`reload.policy`, `reload.mcp_allowlist`, `reload.mcp_respawn`,
`reload.mcp_respawn_failed`, `reload.intel`, `reload.routes`,
`reload.succeeded`, `reload.failed`.

Tests: route add / route remove / rate-limit bucket rebuild /
policy swap round-trip / intel client swap / MCP client swap /
MCP allowlist swap — all exercise the `Arc<dyn …>` trait-object
boundary (the thing handlers actually hold) to prove the swap
takes effect without re-registering.

Scope boundary (still restart-required): bind address,
`--mcp-stdio` command vector, `--intel-unix` / `--intel-http`
endpoint. These are CLI-arg-shaped at process start and the
reload preserves the same arg set — matches the blast radius
operators expect from SIGHUP.

### 2.9 Log rotation for `file:` target

**Closed.** `[logging].rotation` opts a `file:`
target into time-based rollover via `tracing-appender`:

    [logging]
    target = "file:/var/log/agent.log"
    rotation = "daily"    # "daily" | "hourly" | "minutely" | "never"

Yields `agent.log.2026-04-23`, `agent.log.2026-04-24`, ... — the
operator's filename is split into `dir=/var/log`,
`filename_prefix=agent.log` so the rotation suffix lands at the end.
`"never"` (the default) keeps the hand-rolled synchronous
`FileWriter`, so external `logrotate`-style setups still work
unchanged. Size-based rotation is out of scope — `tracing-appender`
is time-based only, and enterprise ops runbooks almost always ask
for daily anyway.

Tests: `rotating_writer_writes_and_rotates`,
`rotation_never_is_not_accepted_by_rotating_writer`,
`logrotation_parses_from_toml`.

### 2.10 Direct in-process OTLP exporter

**Closed.** New `otel` Cargo feature installs
a tracing-opentelemetry layer alongside the main fmt + audit
layers. Every `tracing::Span` is exported over OTLP gRPC (tonic
transport) to an operator-configured collector endpoint. Sampling
is ratio-based at the SDK layer — `[otel].sample_ratio = 0.1` caps
the fleet-wide export rate without losing the 10% trail.

    [otel]
    endpoint = "http://otel-collector:4317"
    service_name = "agent"
    resource_attrs = { env = "prod", region = "eu-west-1" }
    sample_ratio = 1.0

Implementation: dedicated `agent-otel` tokio runtime (1 worker),
`OnceLock`-pinned `TracerProvider`, graceful flush on shutdown via
`force_flush` + `shutdown_tracer_provider`. The JSON logs path
through the `filelog` receiver still works for operators who don't
want the ~50-crate tokio/tonic dep expansion — it's just the `otel`
feature they don't compile in.

**Outbound `traceparent` propagation (closed 2026-04-23).** When
the HTTP trigger parses an inbound `traceparent`, the trace id +
flags are threaded onto `ExecutionContext.trace_context`. The
`http_request` tool reads `ctx.outbound_traceparent()` and emits
a `traceparent: <version>-<trace_id>-<fresh_span_id>-<flags>`
header — the parent id is a per-run 16-hex span id, W3C-correct,
so downstream services see the agent as their direct parent rather
than whoever called the agent. Cron / fs-watch / manual runs emit
nothing; the runtime is a propagator, not an originator.

### 2.11 Retry jitter

**Closed.** `[[nodes]].retry.jitter` multiplies
the linear backoff by a random factor in `[1 - j, 1 + j]`, clamped to
`[0.0, 0.5]`:

    retry = { max_attempts = 3, backoff_ms = 500, jitter = 0.2 }

Default `0.0` (fully deterministic) so existing workflow tests stay
reproducible; operators opt in per-node when the upstream is
shared across a fleet. Implementation keeps the backoff function
pure (`backoff_for(attempt, rng_bits)`) — the engine supplies
entropy, so unit tests pass fixed bits and assert exact backoffs
without a Rng trait.

---

## 3. Target deployment bars

### "OK for dev"

Any shape. Defaults and default feature set are fine.

### "OK for pre-production / internal staging"

- Build Shape A or B (default features + maybe `server-tls`).
- `[policy]` declared (no allow-all).
- Bearer or HMAC auth on every route you don't want public.
- `--log-format json --log-level info`.
- Healthcheck wired on `/healthz`.
- Stateless — no persistence assumed.

### "OK for production (with caveats)"

Everything above, plus:

- TLS termination: either in-process (`server-tls` feature, with SIGHUP
  hot reload of the cert / key — see §1.5) or upstream at a real LB.
- mTLS for inter-service traffic when that's your posture.
- Audit events piped to a retention-compliant sink (either a dedicated
  log collector with target filtering, or wait on gap §2.2).
- Upstream fleet rate-limit when request volumes matter.
- Durable queue in front if you need at-least-once across a fleet.
  Single-node crash recovery is opt-in (§1.5 — `--checkpoint-each-node`);
  a queue boundary with idempotency is still upstream of the runtime.
- `drain_timeout_secs` + orchestrator grace period set consistently.

### "NOT for production today"

- Browser-facing surfaces (no CORS, no cookies, no CSRF).
- Workloads that need sub-millisecond tail latencies (no keep-alive,
  one thread per connection).
- Workloads needing durability beyond single-node checkpoint/resume —
  multi-day, queue-backed, or exactly-once execution (durability is
  opt-in and single-node; see §1.5).
- Multi-tenant shells where different workflows need different auth
  backends inside one process (bindings are shared across routes).

---

## 4. Test coverage snapshot

- **Main crate:** ~26.4k lines of Rust source.
- **Integration tests:** ~2.1k lines across 4 test binaries.
- **Total `#[test]` blocks:** 471.
- **Stability:** the test suite has been run three times in a row
  without flake under default features and with every feature combo
  in §2 of `configuration.md`.
- **Fixture suites:** `intel_classify`, `linear`, `switch_branch`
  under `tests/fixtures/` — each is a complete TOML + expected-outcome
  pair exercised by `testing::fixture::Fixture::run`.

What the tests **don't** cover (deliberate):

- End-to-end TLS + mTLS handshake failures on real sockets — we test
  the config loader and accept path, not every libssl-side edge case.
- Load / stress testing — no k6 / vegeta harness checked in. Run your
  own for your expected RPS.
- Chaos (kernel-level signals other than SIGTERM/SIGINT,
  out-of-descriptors, disk-full on log file) — handled at the "does
  it crash vs. does it degrade" level, not under a chaos framework.

---

## 5. What "not in scope" means

The runtime intentionally does **not**:

- Persist state by default. A crash loses in-flight work unless
  `--checkpoint-each-node` is enabled — opt-in checkpoint/resume gives
  single-node recovery (§1.5), but it is not on by default.
- Distribute work across a fleet, or guarantee exactly-once across a
  queue boundary (single process is the unit of correctness; see the
  roadmap's scale-out section).
- Broker between multiple workflows in one process (one workflow per
  process is the deployment shape).
- Take a non-TOML input at run time — the TypeScript SDK authors
  workflows, but it compiles to the TOML the runtime executes; TOML
  stays the only thing the engine loads.
- Act as an LLM orchestration framework — it runs inference via the
  intelligence adapter, but chaining / planning logic stays in the
  workflow graph.

If a deployment wants those behaviours, the right move is an upstream
or sibling service that composes `agentd` as a building block, not
extending `agentd` itself.

---

## 6. Feedback / track new gaps

File an issue referencing this document and the concern row. When a
gap closes, the row moves from its current colour to Green with a
linked commit / PR. Quarterly review rewrites §1 and §2 from scratch.
