# `agentd` — architecture

> **What:** A bounded workflow runtime. Single binary, one entry
> point, no subcommands.
>
> **Where:** `crates/agentd/` in this repo. 12.8K LOC of Rust,
> 267+ tests across five feature matrices.
>
> **Principle:** The workflow TOML is the source of truth for
> behaviour. The runtime never makes a move the config didn't
> explicitly authorise.

---

## 1. Mental model

A **workflow** is a directed acyclic graph (DAG) of typed nodes
with one or more declared **start nodes** and explicit **edges**.
Control flow lives in the TOML, not in prompt output or in runtime
planning.

```
 ┌───────────── workflow.toml ──────────────┐
 │                                          │
 │  [[start_nodes]]     [[http_routes]]     │
 │        ↓                    ↓            │
 │       ┌──┐    edge    ┌──────┐  edge    │
 │       │a │ ──────────▶│ gate │────┐     │
 │       └──┘            └──────┘    │     │
 │                          │        │     │
 │                   when=  │        │     │
 │                   "true" ▼        ▼     │
 │                     ┌─────┐   ┌──────┐  │
 │                     │post │   │ done │  │
 │                     └─────┘   └──────┘  │
 │                        │         ▲       │
 │                        └─────────┘       │
 │                                          │
 │  [policy]   [auth]   [server.tls]        │
 │  [logging]  [[http_routes.rate_limit]]   │
 └──────────────────────────────────────────┘
                 │
                 ▼
          ┌──────────────┐
          │   runtime    │  src/runtime.rs
          │  (entry)     │
          └──────┬───────┘
                 │
     ┌───────────┴───────────┐
     │                       │
 one-shot                 serve (HTTP)
 mode inferred from       mode inferred from
 absent http_routes       presence of http_routes
     │                       │
     ▼                       ▼
 Engine.run()            HttpServer.spawn()
  • validate             • bind TCP (+ TLS)
  • build engine         • accept loop
  • run 1x               • auth → rate-limit
  • emit outcome         • → handle_connection
                         • → Engine.run()
                         • → HTTP response
                         • SIGTERM drain
```

The engine walks one node at a time. No cycles (validator rejects
them twice — build-time and load-time). No fan-out. No planner.

---

## 2. Module layout (`crates/agentd/src/`)

```
main.rs                    one-line delegate → runtime::run(argv)
lib.rs                     module registry + `pub use rustls` (feature-gated)

runtime.rs                 single-entry dispatcher; mode inference; arg/env parsing;
                           logging merge; engine construction

embedded.rs                EMBEDDED_CONFIG: Option<&'static str> under cfg(embed_config)
                           (baked at build time via `AGENTD_EMBED_CONFIG=path cargo build`)

error.rs                   single `Error` enum — variants cover every subsystem
                           (Config / Workflow / Policy / Tool / Intelligence / Mcp / etc.)

workflow/
  mod.rs                   surface re-exports
  model.rs                 WorkflowDoc + Node + NodeKind + Edge + StartNode +
                           Trigger + HttpRoute + RetryPolicy — the TOML shape
  validator.rs             Kahn's acyclicity + BFS reachability + cross-ref check;
                           issues collected, not fail-fast

engine/
  mod.rs                   re-exports
  context.rs               ExecutionContext + TriggerMeta + RunOptions; dotted-path
                           resolver (single lookup mechanism used everywhere)
  outcome.rs               NodeOutcome + ExecutionOutcome + ExecutionTrace
  handler.rs               NodeHandler trait + HandlerRegistry + 5 control-node
                           handlers (condition/switch/merge/fail/terminate) + StubHandler
  runner.rs                sequential traversal; deadline enforcement; retry wrapper;
                           tracing spans; metrics increment

tools/
  mod.rs                   register_default_tools(&mut registry, policy);
                           resolve_value / resolve_string helpers
  policy.rs                Policy trait + Decision + AllowAll + PolicyRef
  fs.rs                    read_file / write_file / create_dir          (tools-fs)
  env.rs                   read_env                                     (tools-env)
  data.rs                  parse_json / json_select / template_render   (tools-data)
  http.rs                  http_request — hand-rolled HTTP/1.1 client   (tools-http)
  shell.rs                 shell_run — argv-style; timeout; env_clear   (tools-shell)

policy.rs                  ManifestPolicy implementing tools::policy::Policy;
                           glob matchers: "*" / "prefix/*" / "prefix/**" / literal;
                           fs + env + mcp + http + shell sections

intelligence/
  mod.rs
  protocol.rs              Request / Response / Message / Usage; length-framed
                           JSON-RPC 2.0 helpers (wire-compatible with
                           any length-framed
                           JSON-RPC intel server)
  client.rs                IntelligenceClient trait + UnixClient + MockClient
  handler.rs               LlmInferHandler — renders prompt template, dispatches,
                           enforces "must be JSON" when output_schema declared

mcp/
  mod.rs
  protocol.rs              MCP JSON-RPC 2.0: initialize / tools/call / resources/read
  allowlist.rs             McpAllowlist — tool + resource URI patterns
  client.rs                McpClient trait + StdioMcpClient (persistent child) +
                           MockMcpClient
  handler.rs               CallMcpToolHandler + ReadMcpResourceHandler; dry-run
                           aware; is_error → branch = "error"

auth/                      (feature: auth)
  mod.rs                   AuthRef parser (bearer:name / hmac:name / mtls / none);
                           evaluate() dispatcher; Principal; AuthRequest
  config.rs                AuthConfig + BearerDef + HmacDef; env-var-sourced
                           secrets
  bearer.rs                constant-time compare against token set
  hmac.rs                  HMAC-SHA256 via `hmac` + `sha2` crates; Mac::verify_slice
  mtls.rs                  fingerprint-present check; Principal.name = "sha256:..."

server_config.rs           ServerConfig + TlsConfig + ClientAuthConfig + ClientAuthMode
                           (parse-only — rustls builder lives elsewhere)

ratelimit.rs               RateLimitConfig + TokenBucket<C: Clock> + SystemClock +
                           FakeClock; atomic try_take → Ok / Err(retry_after)

signals.rs                 process-global AtomicBool + libc::sigaction handler for
                           SIGTERM + SIGINT; async-signal-safe

triggers/
  mod.rs
  http.rs                  HttpServer + ServerHandle; hand-rolled HTTP/1.1 parser;
                           in-flight counter for drain; per-route rate bucket;
                           auth check → dispatch_accepted routes plain/TLS
  http_tls.rs              (feature: server-tls) rustls ServerConfig builder;
                           PEM loaders; accept_tls returns (TlsStream, fingerprint)

observability/
  mod.rs                   Format + LogTarget + LoggingConfig + ResolvedLogging;
                           apply(&ResolvedLogging) routes to Stderr/Stdout/File writer
  metrics.rs               8 AtomicU64 counters + MetricsSnapshot

testing/
  mod.rs
  fixture.rs               Fixture / FixtureMocks / FixtureTrigger / Expected
  runner.rs                run_fixture + FixtureRunner + FixtureResult +
                           discover_fixtures (auto-discovery helper)
```

Every subsystem is a separate `mod`. None of them transitively pull
a heavy dep — when a feature is off, its code is elided, not just
behind a runtime check.

---

## 3. Execution lifecycle

### 3.1 Startup (`runtime::run`)

```
argv + env
   │
   ▼
parse_args               ← fails fast on unknown flags
   │
   ▼
load_workflow            ← --config FILE | AGENTD_CONFIG | embedded | usage error
   │
   ▼
workflow::validate       ← structural + semantic validation; all issues collected
   │   (fail → exit 5 + JSON report)
   ▼
resolve_logging          ← CLI > env > workflow[logging] > default
   │
   ▼
observability::apply     ← install tracing subscriber NOW — after this all
   │                       events hit the configured target
   ▼
build_policy             ← workflow[policy] → ManifestPolicy (or AllowAll if absent)
   │
   ▼
build_engine             ← control handlers + default tools + maybe intel + maybe mcp
   │                       → Engine { registry, metrics }
   ▼
resolve_mode             ← http_routes nonempty → Serve else Once
(override via --mode)
   │
   ┌──────────┴──────────┐
   ▼                     ▼
run_once_mode      run_serve_mode
```

Key timing detail (R5): **tracing is NOT installed until after the
workflow loads**. This means early failures print plain to stderr,
and the first instrumented event lands on the operator's configured
target (stderr / stdout / file).

### 3.2 One-shot mode

```
run_once_mode
   │
   ▼
pick_once_start                 ← --start NAME || only-manual || only-start || error
   │
   ▼
read --input payload (or Null)
   │
   ▼
TriggerMeta::manual(input)
   │
   ▼
Engine.run(workflow, start, trigger, options)
   │
   ▼
ExecutionOutcome → JSON → stdout
 • Completed → exit 0
 • Failed    → exit 5
 • TimedOut  → exit 5
```

### 3.3 Serve mode

```
run_serve_mode
   │
   ▼
HttpServer::new(...)
   │   ─► with_drain_timeout(Duration)
   ▼
server.spawn()                  ← 3 pre-flight validations:
   │                               1. auth refs resolve to [auth.*] bindings
   │                               2. rate_limit numbers are sane
   │                               3. [server.tls] → build rustls ServerConfig now
   │                              bind TCP + return ServerHandle
   ▼
install_shutdown_handlers       ← libc::sigaction on SIGTERM + SIGINT
   │
   ▼
[loop] check signals::shutdown_requested
   │   ────► every 50ms
   ▼
handle.shutdown_and_drain()     ← stop accepting; wait ≤ drain_timeout for
   │                              in_flight counter → 0
   ▼
exit 0 (clean) or 5 (forced)
```

Inside the accept loop (per connection):

```
accept() → TcpStream
   │
   ▼
set_read_timeout + set_write_timeout       ← 30s defaults
   │
   ▼
InFlightGuard::acquire(counter)            ← RAII; decrements on drop
   │
spawn thread →
   ▼
dispatch_accepted(stream, tls_arc, ...)
   │
   ├── tls_arc = None  ──► handle_connection(tcp_stream, peer_fp=None)
   │
   └── tls_arc = Some ──► accept_tls(tcp) drives handshake
                          → (TlsStream, Option<fingerprint>)
                          → handle_connection(tls_stream, peer_fp)
```

Inside `handle_connection` (generic over `Read + Write`):

```
BufReader::new(stream)
   │
   ▼
parse_request                              ← request-line + headers (lowercased) + body
   │
   ▼  (extract back: reader.into_inner())
Request { method, path, headers, body, peer_cert_fingerprint }
   │
   ▼
GET /healthz? → 200 OK {status:"ok", workflow}; return
   │
   ▼
match route against http_routes            ← unknown path → 404, wrong method → 405
   │                                          (distinguishes correctly)
   ▼
rate-limit check                           ← per (method, path) TokenBucket
   │   Err(retry_after) → 429 + Retry-After header; return
   │
   ▼
auth check                                 ← AuthRef::parse(route.auth).evaluate()
   │   Deny → 401; return
   │   Allow → principal → injected at trigger.principal.{kind, name}
   │
   ▼
body JSON parse                            ← malformed → 400
   │                                          empty → Value::Null (merged with principal)
   ▼
Engine.run(workflow, route.start_node, trigger_http(input), options)
   │
   ▼
ExecutionOutcome → JSON
 • Completed → 200 OK
 • Failed    → 422 Unprocessable Entity
 • TimedOut  → 504 Gateway Timeout
```

### 3.4 Node dispatch (engine)

```
Engine.run_with_trace(workflow, start, trigger, options)
   │
   ▼ resolve_entry(start)         ← entry_node or single-root fallback
   ▼ execution_id = next (monotonic)
   ▼ ctx = ExecutionContext::new(execution_id, workflow, start, trigger, options)
   │       │
   │       └─► node_outputs["trigger"] = flatten(trigger.input + trigger.kind)
   │
   ▼ span workflow.run enter
   │
   ▼ [loop over MAX_STEPS=10_000]
   │   check deadline                       ← Instant::now() >= ctx.deadline → TimedOut
   │   look up node                         ← workflow.node(current_id)
   │   span node.execute enter
   │   metrics.inc_node_executed
   │
   │   dispatch_with_retry(registry, node, ctx)
   │     │
   │     ├── no retry policy → registry.dispatch(node, ctx) once
   │     └── Some(RetryPolicy) → loop up to max_attempts
   │           on Err & retryable(err, policy.on) & attempt < max:
   │             sleep backoff_ms * attempt                  (honours ctx.deadline)
   │             emit tracing node.retry event
   │             retry
   │           else propagate
   │
   │   match outcome:
   │     Terminate(v)  → store; inc_workflow_completed; return Completed
   │     Fail(reason)  → store; inc_workflow_failed;    return Failed
   │     Continue(v,b) → store v in node_outputs[node.id]
   │                     pick_next(workflow, current, b):
   │                       None      → ambiguous = error; or dead-end = Completed
   │                       Some(id)  → current_id = id
   │
   ▼ safety cap tripped → Err (cycle slipped past validator)
```

Retry semantics (R4):

- `RetryOn::Any` → retries every `Err` variant.
- `RetryOn::Transient` → retries only `Error::Tool`, `Error::Intelligence`,
  `Error::Mcp`. Policy violations, schema failures, timeouts, config
  errors, capability-unavailable short-circuit.

---

## 4. Data flow — the one lookup mechanism

`ExecutionContext::resolve_path("node_id.field.subfield")` is the
single mechanism for reading context. Reached from:

- Every `*_from` / `path_from` / `resource_from` / `input_from` /
  `content_from` / `url_from` / `body_from` / `args_from` field.
- `Condition` / `Switch` `expr`.
- `template_render` `{{key}}` substitutions.
- The `LlmInfer` prompt-template key resolution.

Two pre-populated pseudo-nodes live in `node_outputs`:

| Key | Populated with |
|---|---|
| `trigger` | `{kind, ...flattened payload}` — for HTTP mode also `{principal: {kind, name}}` when auth passed |
| `<node_id>` | whatever the node handler's last Continue outcome emitted |

No indexed arrays in paths yet — `node_outputs[0].x` is a future
extension.

---

## 5. The seven invariants

Everything in the codebase is structured around these. Break any
of them and you break the model:

1. **The workflow is acyclic.** Kahn's algorithm rejects cycles at
   both build time (`build.rs`) and load time
   (`workflow::validate`). A cycle that slipped both would trip the
   engine's 10 000-step safety cap with a loud error.

2. **Every edge is declared.** The engine only traverses edges
   present in `workflow.edges`. Intelligence cannot invent a new
   edge; neither can a handler; neither can the MCP server.

3. **Every capability is declared at compile time AND narrowed at
   runtime.** A handler only exists if its Cargo feature is on.
   Policy narrows further — every side-effectful call consults
   `Policy::check_*` before touching anything real.

4. **Secrets never live in the TOML.** `tokens_env` and
   `secret_env` point at env vars; literal `tokens` / `secret`
   exist only for tests and are discouraged in prose.

5. **Auth is checked before the body is parsed.** Even a malformed
   request body doesn't get processed if the caller isn't
   authenticated. In serve mode: auth check → rate limit → body
   parse.

6. **Intelligence is a bounded reasoning step.** An `llm_infer`
   node can:
   - Produce JSON that becomes input to downstream nodes.
   - Emit a branch label consumed by `switch` / `condition`.

   It cannot:
   - Invent a new edge or destination.
   - Bypass the policy layer on a downstream side effect.
   - Reach the network, filesystem, or subprocess directly.

7. **Drain is bounded.** SIGTERM flips a flag; the accept loop
   stops; in-flight requests finish up to `drain_timeout_secs`.
   Exit 0 on clean drain, 5 if the deadline fired.

---

## 6. Security posture (what is defended, against what)

| Threat | Defence |
|---|---|
| Prompt injection tricking the model into new tool calls | Control flow is TOML — LLM output can't add edges or handlers |
| Compromised LLM response with malformed JSON | `output_schema` requires valid JSON before downstream routing |
| Path traversal on fs operations | Every path is policy-matched against allowlist patterns before `fs::*` touches it |
| Symlink escape on fs writes | `write_file` and `create_dir` honour the policy's canonical path — future follow-up: canonicalise the target |
| Symlink escape on shell commands | `shell_run` calls `fs::canonicalize` before the policy check, so a symlink to a denied binary fails |
| Shell interpolation attacks | `shell_run` uses argv; no `sh -c`; operator-supplied args are strings, not command lines |
| PATH-poisoning for shell binaries | `shell_run` only accepts absolute paths and clears the env, then sets a curated `PATH` |
| HTTP body DoS | 1 MiB request cap, 1 MiB response cap, 16 KiB headers cap — configured in `triggers::http` + `tools::http` |
| Connection flood | per-route token bucket (429 + `Retry-After`) |
| Token extraction via side channel | Bearer compare is constant-time; HMAC verify is constant-time via `hmac::verify_slice` |
| Man-in-the-middle on the HTTP trigger | `server-tls` feature terminates TLS in-process via rustls |
| Stolen client cred | mTLS client-cert verification against a pinned CA; workflow can further pin the peer cert fingerprint |
| Slow-loris holding connections open during shutdown | Read/write timeout 30 s; SIGTERM drain with bounded wait |
| Runaway LLM token burn | Per-node `retry` caps attempts; no workflow-level token budget enforced yet (listed in maturity) |
| Runaway subprocess | `shell_run` 30 s default timeout, SIGKILL on deadline; 64 KiB stdout/stderr cap |

The default build ships every defence above. `--no-default-features`
narrows the attack surface further by dropping capabilities.

---

## 7. Configuration precedence

Precedence chain, most specific → least specific:

```
CLI flag  >  AGENTD_* env var  >  workflow [logging] / [auth] / [server] / [policy]  >  built-in default
```

Applies to all R1–R5 knobs. One exception: secrets — there is no
CLI override for secrets (environment-sourced only). This is
deliberate; CLI history is a liability.

---

## 8. Observability

Three surfaces, all always-on (controllable by the level filter):

### Tracing

Two nested spans:

- `workflow.run` — one per run, fields `execution_id`, `workflow_id`, `start_node`, `dry_run`
- `node.execute` — per step, fields `node_id`, `kind`

Typed events on the default target (unless noted):

| Event | Level | Target | Key fields |
|---|---|---|---|
| `workflow.started` | info | `agentd::audit` | — |
| `workflow.completed` | info | `agentd::audit` | `last_node`, `elapsed_ms` |
| `workflow.failed` | warn | `agentd::audit` | `last_node`, `reason`, `elapsed_ms` |
| `workflow.timed_out` | warn | default | `last_node`, `elapsed_ms` |
| `node.completed` | debug | default | `latency_ms` |
| `node.branch` | debug | default | `label` |
| `node.failed` | error | default | `reason`, `latency_ms` |
| `node.retry` | warn | `agentd::audit` | `node_id`, `attempt`, `max_attempts`, `backoff_ms`, `reason` |
| `policy.denied` | warn | `agentd::audit` | `reason`, `latency_ms` |
| `http.auth_denied` | warn | `agentd::audit` | `method`, `path`, `reason` |
| `http.rate_limited` | warn | `agentd::audit` | `method`, `path`, `retry_after_ms` |
| `http.drain_deadline_exceeded` | warn | `agentd::audit` | `in_flight` |
| `http_response.truncated` | warn | `agentd::audit` | `claimed_bytes`, `cap_bytes` |
| `tls.handshake_failed` | warn | `agentd::audit` | `reason` |

Split the audit stream from regular logs with a one-line
`EnvFilter` directive, e.g. `agentd::audit=info,warn`.

### Metrics

Eight atomic counters on `Metrics`:

```
workflow_starts
workflow_completions
workflow_failures
workflow_timeouts
workflow_errored
node_executions
node_failures
policy_denials
```

Snapshot via `engine.metrics().snapshot()` — serde-Serializable.
No HTTP scrape endpoint yet (listed in maturity).

### Logging targets

- `stderr` (default)
- `stdout`
- `file:PATH` — append mode; parent dirs created; synchronous writes
  behind a Mutex. Swap to an external aggregator (vector, filebeat)
  for high-throughput scenarios.

---

## 9. Build-time capability selection

```
cargo build -p agentd                             # default bundle
cargo build -p agentd --no-default-features \
  --features "tools-fs tools-data"               # sealed read-only
cargo build -p agentd --features "server-tls"     # + in-process TLS
cargo build -p agentd --features \
  "tools-http tools-shell server-tls"            # full surface

AGENTD_EMBED_CONFIG=./workflow.toml cargo build   # Mode B (baked config)
```

`build.rs` validates the embedded config (if any) before compile
succeeds. Feature pruning is elision, not runtime gating — features
off = code not in the binary.

See [`operations.md`](operations.md) §Build modes for details.

---

## 10. Testing architecture

```
cargo test -p agentd                           240 tests default
cargo test -p agentd --features "server-tls"   250 tests
cargo test -p agentd --features \
  "tools-http tools-shell server-tls"         267 tests
cargo test -p agentd -- --ignored              5 build-time tests
                                              (build.rs integration)
```

Layout:

- Unit tests — live inside each module (`#[cfg(test)] mod tests`).
- Integration tests — `crates/agentd/tests/*.rs`, one binary per file.
- Fixture suite — `crates/agentd/tests/fixtures/<name>/{workflow.toml,fixture.toml}`,
  auto-discovered by `fixture_suite.rs`.
- Build-time suite — `#[ignore]`'d; spawn subprocess `cargo build`
  with `AGENTD_EMBED_CONFIG=...` and diff the outcome.

See [`capabilities.md`](capabilities.md) §14 for the author-facing
fixture format. See [`testing.md`](testing.md) (future) for deep
test-engineering rationale.
