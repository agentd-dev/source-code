# Salvage notes: mining the retired bounded-workflow `agentd` for the MCP-native rewrite

**Status:** Design artifact (input to RFC 0001 implementation).
**Date:** 2026-06-25.
**Scope:** `/root/agentd-dev/source-code/crates/agentd/src` — the retired
bounded-workflow-DAG runtime, treated as a *parts bin*. Target design is
`rfcs/0001-mcp-native-agent-runtime.md` (minimal, dependency-light, Rust,
MCP-native, reactive, process-tree subagents, agentd-as-MCP-server).

Verdict legend:
- **KEEP-AS-IS** — lift the file/module nearly verbatim; only drop the
  `tracing` audit target names and any workflow-engine coupling.
- **ADAPT** — the core logic is right but the surface (params, traits,
  coupling) must change for the new model.
- **REWRITE** — the idea is reusable but the existing code is too coupled
  to the DAG/policy/workflow world to lift; rebuild small against the RFC.
- **DROP** — does not belong in the new design at all.

A recurring theme: the old code is *single-process, single-workflow,
synchronous, blocking, std-only*. That posture matches RFC 0001's
"no async runtime, OS processes + a few threads" bar almost exactly, so
the transport and OS-primitive code is highly reusable. What is **not**
reusable is the entire DAG/engine/policy/auth/signing superstructure and
the fact that the MCP and intelligence layers were built for a
*request/response-per-node* model with no streaming, no `tools/list`, no
resources/subscribe, and no notifications.

---

## 1. The single most important gap to flag up front

The new design's signature feature — **reactive MCP resource
subscriptions** (`resources/subscribe` + `notifications/resources/updated`)
— **does not exist anywhere in the old code.** The old MCP client
(`mcp/client.rs` + `mcp/protocol.rs`) implements exactly three methods:
`initialize`, `tools/call`, `resources/read`. There is:

- **No `tools/list` / `resources/list`** — the old design relied on the
  *workflow author* declaring tools statically; the new agentic loop must
  discover the tool catalogue at runtime. This must be built.
- **No subscription model and no inbound-notification handling.** The old
  `rpc_call` loop *skips and discards* any notification the server emits
  (`mcp/client.rs:274-307`). The new supervisor must instead *route* those
  notifications as triggers. This is a structural change, not an addition.
- **No concurrent / multiplexed transport.** The old stdio client holds a
  single `Mutex<Inner>` and assumes "one in-flight request at a time"
  (`mcp/client.rs:120`). A reactive client that must read
  server-initiated notifications while a request is outstanding needs a
  reader thread + a correlation map. The RFC's open question §14.6 (stdio
  pipes confirmed for v1) and §5.3 (notifications-as-triggers) both land
  here.

So the MCP layer is **ADAPT-to-REWRITE**: the framing, spawn, init
handshake, and drop/kill logic are excellent and should be lifted; the
request/response *shape* of the client must be replaced with a
read-loop + pending-request-map + notification-dispatch design.

---

## 2. Candidate-by-candidate findings

### 2.1 MCP stdio client — `mcp/client.rs`  → **ADAPT** (lift the transport, rebuild the dispatch)

What it does: spawns an MCP server as a child process
(`StdioMcpClient::spawn` / `spawn_with_env`), lazily performs the
`initialize` + `notifications/initialized` handshake on first call, and
serves `tools/call` / `resources/read` over NDJSON JSON-RPC behind a single
`Mutex`. Includes `Drop` that closes stdin + kills + reaps the child, a
`ReloadableMcpClient` (ArcSwap hot-swap), and a `MockMcpClient`.

**Lift nearly verbatim:**
- `StdioMcpClient::spawn_with_env` (lines 147-186) — the `Command` setup
  (`stdin/stdout piped, stderr null`), pipe-takeout, and child handle
  retention. **Drop the `secrets::resolve` call** inline and replace with
  the new config's env handling (see §2.10).
- `Drop for StdioMcpClient` (lines 211-220) — flush stdin → kill → wait.
  This is exactly the dead-subprocess-cleanup posture the RFC wants
  (req. 8). Reuse the pattern for *subagent* child cleanup too.
- The `initialize()` handshake (lines 222-247), including sending the
  `notifications/initialized` notification as a separate framed line.
- NDJSON framing details inside `rpc_call`: write payload + `\n` + flush;
  read a line, `trim`, skip empty lines, parse, branch on presence of
  `id` to distinguish responses from notifications (lines 274-307). The
  "skip notifications before the response" loop is the seed of the new
  reader, but in the new model notifications must be *dispatched* not
  *discarded*.
- The sh-based fake-server test harness (lines 432-504) is a genuinely
  useful, dependency-free way to integration-test a stdio MCP peer; keep
  the technique.

**Rebuild for the new model:**
- Replace the `Mutex<Inner>` "one in-flight" model with a **dedicated
  reader thread** that owns `ChildStdout`, parses every line, and either
  (a) completes a pending request by `id` via a `HashMap<u64, Sender>` /
  oneshot-style channel, or (b) hands a notification
  (`notifications/resources/updated`, etc.) to a supervisor-provided
  callback/queue. Writer side keeps a `Mutex<ChildStdin>`.
- Add `tools/list`, `resources/list`, `resources/subscribe`,
  `resources/unsubscribe` request methods + their result types.
- The `McpClient` trait (lines 25-31) is too narrow (`call_tool`,
  `read_resource` only) — widen it or replace with a richer client handle.
- `ReloadableMcpClient` (lines 49-106): the ArcSwap hot-reload concept is
  nice but is tied to the old SIGHUP-reload story. **Probably DROP for
  v1** (RFC §3 keeps the runtime tiny; in-memory, restart-to-reload). Keep
  the ArcSwap technique in the back pocket if hot-reconfig returns.

### 2.2 MCP wire types — `mcp/protocol.rs`  → **KEEP-AS-IS** (then extend)

What it does: the JSON-RPC 2.0 envelope types (`RpcRequest`,
`RpcResponse<R>`, `RpcError`, `RpcNotification`), `initialize` params/result,
`ToolsCallParams/Result`, `ResourcesReadParams/Result`. Tolerant parsing
(`#[serde(default)]`, unknown-field collection).

These are clean, minimal, correct serde shapes against the 2024-11-05 /
2025-03-26 MCP revision. Lift them directly. **Extend with:** `tools/list`
+ `resources/list` result types (tool name/description/inputSchema;
resource uri/name/mimeType), `resources/subscribe` params, and the
`notifications/resources/updated` params shape (`{ uri }`). `RpcResponse`
already carries `id: Option<Value>` (line 36) which the new
correlation-map dispatcher needs — the comment even anticipates it
("future multiplexed transports can assert pairing").

`ToolsCallResult` (lines 97-105) with `content`, `is_error`,
`structured_content` is exactly what the agentic loop needs to feed
observations back to the model. Keep.

### 2.3 Intelligence client + transports — `intelligence/client.rs`  → **ADAPT**

What it does: the `IntelligenceClient` trait (`complete(&Request) ->
Response`), a Unix-socket client (length-framed JSON-RPC `complete`), an
HTTP client (hand-rolled HTTP/1.1 POST, optional bearer), a `MockClient`,
and a `ReloadableIntelClient` (ArcSwap).

**Maps directly onto RFC §7.2** which wants exactly three intelligence
transports: `unix:` , `https://`, and `vsock:` , behind one minimal
abstraction. Specifically:
- **`UnixClient` (lines 67-144)** → **KEEP-AS-IS** for the `unix:/path`
  same-pod-sidecar case. The connect-per-call, set timeouts, write one
  frame, read one frame, surface RPC error pattern is exactly right.
- **`HttpClient` (lines 165-333)** + `ParsedEndpoint::parse` +
  `parse_http_status` / `parse_http_headers` → **ADAPT**. This is a
  genuinely small hand-rolled HTTP/1.1 client with a 4 MiB body cap and
  `Connection: close`. It currently speaks the *internal JSON-RPC
  `complete` envelope*, not OpenAI `/chat/completions`. For the
  standalone `https://` case (RFC §7.2) we want the **OpenAI-compatible
  adapter**, so reuse the *connection/parsing machinery* but swap the
  body builder + response parser (the providers.rs logic, §2.4, is the
  better source for *that*). Note: plain-HTTP only — TLS is via `ureq`
  elsewhere; for `https://` standalone we either link rustls or (RFC's
  bias) terminate TLS at a sidecar.
- **`IntelligenceClient` trait (lines 43-45)** → **ADAPT**. `complete`
  is the right single method, but the new loop needs **tool-calling**:
  the request must carry the tool catalogue and the response must be able
  to express tool calls. The old `Request`/`Response` (see §2.5) have no
  tool fields. Widen.
- `MockClient` (lines 389-430) → **KEEP-AS-IS** (test infra).
- `ReloadableIntelClient` (lines 447-489) → **DROP for v1** (same reason
  as the MCP reloadable wrapper — hot-reload is out of scope for the
  minimal runtime).
- **vsock** transport (RFC §7.2 / §12 / §14.8) → **does not exist**;
  must be built. The `UnixClient` is the structural template — a
  vsock client is the same frame-write/frame-read against a
  `VsockStream` instead of `UnixStream`.

### 2.4 Remote LLM providers — `intelligence/providers.rs`  → **ADAPT** (high-value salvage)

What it does: a blocking HTTPS client (`ureq`, rustls) speaking four
dialects — anthropic, openai, gemini, openai-compatible — each mapping the
internal `Request`/`Response` to/from provider wire shapes. Includes
`split_system` (system-prompt extraction), per-request secret resolution,
key-probe-at-build, and `parse_response` per dialect.

This is the **single most valuable piece of LLM-facing code in the repo**
for the standalone-CLI story (RFC §7.2: "a single OpenAI-compatible
`/chat/completions` adapter covers the large majority of providers").
Concretely worth lifting:
- The **openai / openai-compatible** request-build + `parse_response` arms
  (lines 155-177, 254-266). This *is* the `https://` standalone adapter
  the RFC asks for. Reuse near-verbatim.
- `split_system` (lines 106-117) — every provider wants the system prompt
  out-of-band; keep.
- The **anthropic** arm (lines 131-154, 241-252) — given this is an
  Anthropic-built agent, keeping a native anthropic dialect in-binary is
  defensible even under the "fewer adapters" bias. Lift it.
- `RemoteClient`'s `Debug` impl that never prints key material (lines
  27-36) and the build-time key probe (lines 55-67) — good security
  hygiene; keep the pattern.
- `truncate` UTF-8-safe helper (lines 286-296) — trivially reusable.

**Adapt:** the new design (RFC §7.2) biases toward *one normalised shape +
push provider quirks to a gateway*. So in-binary, ship **openai-compatible
(+ maybe anthropic)** and let gemini/others live behind the gateway. Also:
this code has **no tool-calling support** — it serializes plain
`{role, content}` messages only. The agentic loop needs `tools` in the
request and `tool_calls` in the response (OpenAI tool-call shape /
anthropic `tool_use` blocks). That is net-new and is the main adaptation
cost. The `ureq` dependency (blocking, rustls, no async) is acceptable
under the RFC's dep budget for the `https://` feature.

### 2.5 Intelligence wire protocol + framing — `intelligence/protocol.rs`  → **KEEP-AS-IS** (framing) / **ADAPT** (Request/Response)

- **`write_frame` / `read_frame` (lines 88-120)**: 4-byte LE length prefix
  + payload, 16 MiB cap. **KEEP-AS-IS.** This is the JSON length-framing
  helper the task calls out. Reuse for the `unix:` and `vsock:`
  intelligence transports *and* — critically — it is a strong candidate
  for the **supervisor↔subagent control channel** (RFC §6.2, §14.1: "a
  minimal JSON-RPC sibling that shares code with the MCP layer").
  Length-framing is more robust than NDJSON when payloads may contain
  newlines; consider standardizing the control channel on it.
- **`Request`/`Response`/`Message`/`Usage` (lines 19-51)** → **ADAPT.**
  The shapes are clean and the `Usage` token accounting is exactly what
  budgets need, but `Request` has no `tools` field and `Response` has no
  `tool_calls`. Widen for tool-use. `Message{role, content}` with string
  content needs to grow to support tool-result messages.

### 2.6 Hand-rolled HTTP/1.1 client (tool) — `tools/http.rs`  → **ADAPT** (salvage the client, drop the node)

What it does: the `http_request` *workflow node* plus a hand-rolled
plaintext HTTP/1.1 client (`perform_request`, `parse_status_line`,
`parse_response_headers`, `parse_url`), an optional `ureq`-backed TLS path
(`perform_request_tls`), 1 MiB body caps, declared-header rendering with
`{{secret:NAME}}` substitution + CR/LF injection rejection, and
traceparent propagation.

**Salvage (transport):** `parse_url` (lines 162-204), `perform_request`
(lines 425-525), `parse_status_line`, `parse_response_headers` — a tidy,
dependency-free HTTP/1.1 client with body caps. This overlaps heavily with
the `HttpClient` in `intelligence/client.rs`; in the rewrite there should
be **one** small internal HTTP/1.1 client, and this is the better-rounded
implementation (header map, host:port parsing, size caps). Consolidate.

**Salvage (security hygiene):** `render_declared_headers` +
`substitute_secret_placeholders` (lines 215-278) — header-name token
validation, `{{secret:NAME}}`-only substitution, CR/LF rejection. If the
new `exec`/MCP-HTTP-transport path ever sets headers from config, this is
the safe way.

**DROP:** the entire `NodeHandler`/`NodeKind::HttpRequest`/`ctx`/policy
coupling (lines 50-150, 575-602) — there are no workflow nodes or policy
in the new design. The new runtime has **no built-in HTTP tool** at all
(RFC §2: no built-in tool catalogue); HTTP is reached via an MCP server.
The client code survives only as the transport under the `https://`
intelligence path and HTTP-transport MCP.

### 2.7 Hand-rolled HTTP/1.1 server — `triggers/http.rs`  → **REWRITE** (mine the parser, drop everything else)

What it does (2327 lines): a full hand-rolled HTTP/1.1 server — accept
loop, thread-per-connection, keep-alive, request parsing
(`parse_request`), body parsing by content-type (json / urlencoded /
multipart), routing against workflow `http_routes`, TLS/mTLS, auth, rate
limiting, idempotency store, `/healthz` + `/metrics`, graceful drain,
SIGHUP hot-reload of routes/TLS/auth.

This is enormously coupled to the retired model (workflow routes, auth,
rate limit, idempotency, the engine). **Most of it is DROP.** But there
are precise salvage points the new MCP-server-over-HTTP transport (RFC §8,
§11) and the healthcheck (RFC req. 6) need:
- **`parse_request` (lines 1040-1132)** — request line + header map +
  Content-Length body, with `MAX_HEADERS_BYTES` (16 KiB) and
  `MAX_BODY_BYTES` (1 MiB) caps and the 431/413 responses, plus the
  `silent_close` idle-timeout signal. This is a clean, hardened, std-only
  HTTP request reader. **Lift it** as the basis for serving agentd's own
  MCP over HTTP/SSE and for a trivial `/healthz`.
- **The accept loop + graceful-drain machinery** (`spawn`, `ServerHandle`,
  `InFlightGuard`, `shutdown_and_drain`, lines 166-233, 411-565) — the
  shutdown-flag + in-flight-counter + bounded-drain pattern is exactly the
  "clean SIGTERM citizen" posture RFC §11 demands. **Lift the pattern**
  (not the route/TLS/auth-laden specifics).
- **`/healthz` handler** (lines 656-664) — trivial, keep the idea.
- `parse_urlencoded` / `parse_multipart` / `percent_decode` (lines
  1365-1506) — only relevant if agentd's self-MCP ever accepts webhook
  bodies; for v1 (JSON-RPC/SSE MCP transport) **DROP**.
- **DROP entirely:** routing-to-workflow, auth, rate limit, idempotency
  store, TLS hot-reload, `respond`-node handling, `/metrics`-from-engine.

### 2.8 Agent loop — `agent/loop_node.rs`  → **REWRITE** (best conceptual reference in the repo)

What it does: a bounded ReAct loop *inside a DAG node*. Builds a
system+user message set, calls the backend, parses the model's single-JSON
action (`{"action":"tool"|"final", ...}`), routes tool calls through a
`ToolBroker` (the six built-in tools: read_file, write_file, read_env,
http_request, shell_run, call_mcp_tool), feeds results/errors back, and
enforces step cap / token budget / deadline. Recoverable on malformed
output and tool errors.

This is the **closest existing analogue to the new agentic loop (RFC
§6.1)** and the best place to start *conceptually*, but it must be
**rewritten** because:
- It is a `NodeHandler` inside the engine; the new loop runs *in a
  subagent process* talking up a control channel.
- The tool surface is a hard-coded six-tool `ToolBroker` (lines 414-548)
  with policy gates. The new loop's tools are **entirely the scoped MCP
  catalogue + agentd self-tools** — no built-in broker. So the broker is
  **DROP**, but its *shape* (route a named tool call, capture result-or-
  error JSON, append to transcript) is the template.
- It uses a homegrown `{"action":...}` JSON protocol parsed from prose.
  The new loop should use **native LLM tool-calling** (provider tool_use)
  rather than prompt-parsed JSON, so the model owns control flow cleanly.

**Salvage precisely:**
- **`extract_json_object` (lines 343-373)** — balanced-brace, string-aware
  first-JSON-object extractor that tolerates code fences and prose. Robust
  and dependency-free; **KEEP-AS-IS** (useful even with native tool-calls
  for any fallback text parsing).
- The **loop skeleton** (lines 171-294): per-turn deadline check, token
  budget check, call, accumulate usage, dispatch, feed-back-on-error,
  step-cap-exhaustion branch, structured `agentd::audit` events per turn.
  This control structure is exactly right — rebuild it against the new
  client + MCP-catalogue.
- The **recoverability discipline** — malformed model output and failed
  tool calls become a fed-back message that consumes a step rather than
  aborting (lines 237-285). Keep this behaviour; it is what makes the loop
  robust (RFC req. 7).
- `system_prompt` tool-catalogue rendering (lines 375-408) is the right
  idea but should be driven by the *discovered MCP `tools/list`* instead
  of a static match. ADAPT.

### 2.9 Budget / limits — `budget.rs`  → **ADAPT** (split: keep rlimits + token tracker, restructure)

What it does: process-wide caps. `BudgetConfig` (max_memory_mb,
max_cpu_secs, max_run_time_secs, max_fs_write_mb, max_llm_tokens),
`apply_rlimits` (Unix `setrlimit` RLIMIT_AS/RLIMIT_CPU; Windows Job
Object), and `BudgetTracker` (atomic cumulative LLM-token + fs-write
counters with CAS-loop cap enforcement, `clamp_run_time`).

**Strong salvage** — this maps onto RFC §10/§13 budgets (max-steps,
max-tokens, deadline, depth, tree-wide token ceiling) and the "optional OS
resource limits" posture:
- **`apply_rlimits` (Unix path, lines 96-104, 256-285)** + the
  `RlimitResource` musl/gnu alias (lines 251-254) → **KEEP-AS-IS.** This is
  exactly the "rlimit/cgroup applied by the deployment, optionally by us"
  story. The warn-and-continue-on-failure posture is correct for
  sandboxed containers.
- **`BudgetTracker` token + fs-write counters (lines 296-397)** → **ADAPT.**
  The cumulative-LLM-token cap + atomic CAS pattern is exactly what the
  per-run and **tree-wide** token ceiling (RFC §6.3, §14.7) needs. Keep
  the token accounting; the fs-write cap is a built-in-fs-tool concern
  that **DROPs** (no built-in fs tool). Add **max-steps** and
  **max-depth** counters here.
- `clamp_run_time` (lines 70-75) → keep; deadline clamping is still wanted.
- Windows Job Object path (lines 128-240) → keep behind a feature if
  Windows is a target; otherwise DROP (RFC is Unix-first; vsock/signals/
  rlimit are all Unix).
- The `[budget]` TOML shape and `serde(deny_unknown_fields)` → **ADAPT**
  to the new flat env/flag config surface (RFC §10) — these become
  `--max-tokens`, `--max-steps`, `--deadline`, `--max-depth` rather than a
  TOML block.

### 2.10 Secrets — `secrets/mod.rs` (+ `secrets/oauth2.rs`)  → **ADAPT** (keep the front door, trim sources)

What it does: a pluggable secret registry with one resolution front door
(`secrets::resolve(name)`), sources env / file / command / oauth2, an
ArcSwap process-global registry, never-serialize / Debug-prints-`***`
hygiene, and startup-probe fail-fast.

For the new design the relevant requirement is narrow (RFC §7.2, §13):
intelligence credentials come from **env/flags, never logged, never
persisted**. So:
- **The `resolve(name)` single-front-door pattern + env fallback (lines
  255-271, 432-434)** → **KEEP** the shape. One function every credential
  consumer calls, env-var-by-name with fallback. This is precisely how the
  RFC wants `AGENT_INTELLIGENCE_TOKEN` and provider keys handled.
- **`file` source (lines 272-284)** → **KEEP** (k8s Secret mounts / Vault
  Agent sidecars rotate by replacing the file; live per-resolve read).
  Cheap, std-only, and directly useful in containers.
- **never-Serialize + `Debug = ***` (lines 204-209, 219-226)** → **KEEP**
  the discipline. Credentials must stay out of logs/transcripts (RFC §13).
- **`command` (secrets-exec) and `oauth2` (secrets-oauth2) sources** →
  **DROP for v1** (heavier, feature-gated, and `oauth2.rs` pulls `ureq`).
  They can return later as optional features if needed, but they exceed
  the minimalism bar for core.
- The custom `Deserialize` that rejects literal secrets in a TOML
  (`deny_unknown_fields`-by-hand, lines 68-121) → **DROP**; the new config
  is flat env/flags, no `[[secrets]]` TOML block.

`secrets/oauth2.rs` (398 lines) → **DROP** for v1.

### 2.11 Signals — `signals.rs`  → **KEEP-AS-IS** (shutdown half) / **DROP** (reload half)

What it does: process-global `AtomicBool` flags for shutdown
(SIGTERM/SIGINT, Unix; Ctrl+C via `ctrlc`, Windows) and reload (SIGHUP),
plus a file-based reload watcher thread.

- **`install_shutdown_handlers` (Unix, lines 55-86)**, `shutdown_handler`,
  `shutdown_requested()`, and the **deliberate no-`SA_RESTART`** choice so
  blocked syscalls return `EINTR` and the loop observes the flag promptly
  (lines 71-74) → **KEEP-AS-IS.** This is exactly the clean-SIGTERM-citizen
  behaviour RFC §11 demands, and the EINTR detail is the kind of correct
  low-level decision worth preserving. The `AtomicBool` + `SeqCst` +
  signal-safety reasoning is sound.
- **The reload half** — `RELOAD_REQUESTED`, `reload_handler` (SIGHUP),
  `clear_reload`, `spawn_reload_file_watcher` (lines 36-44, 120-187) →
  **DROP for v1.** Hot-reload is out of scope (RFC: restart-to-reload,
  in-memory v1). The supervisor *will* want SIGCHLD handling for reaping
  subagent children — that is net-new and should be added here.
- Windows `ctrlc` path (lines 99-106) → keep behind a feature only if
  Windows is targeted; otherwise DROP (`ctrlc` is a dependency).

### 2.12 Observability / logging — `observability/mod.rs` + `metrics.rs`  → **ADAPT** (logging) / **KEEP-AS-IS** (metrics shape)

What it does: a `tracing`-based logging stack (text/json formats; stderr/
stdout/file targets; rotation via `tracing-appender`; an audit-sink layer;
an optional OTLP exporter), plus `Metrics` (atomic counters → Prometheus
text).

- **The `tracing` install scaffolding** (`init`, `apply`, `install*`,
  `StderrWriter`/`StdoutWriter`/`FileWriter`, `Format`, `LogTarget`,
  `CapturingWriter`) → **ADAPT, keep small.** The JSON-line-to-stdout +
  text-to-tty story (lines 219-456) is exactly RFC §11's "log structured
  events to stdout/stderr." Keep `tracing` + `tracing-subscriber` (they
  earn their place), `Format`/`LogTarget` parsing, and the `CapturingWriter`
  test helper. **DROP** the `[logging]` TOML block, the audit sub-sink,
  rotation (`tracing-appender` dep), and especially **OTLP** (the `otel`
  feature pulls tokio + ~50 crates — explicitly out per RFC §12).
- **`Metrics` / `MetricsSnapshot` / `to_prometheus` (metrics.rs)** →
  **KEEP-AS-IS in shape, ADAPT the counter set.** Atomic-U64-counters →
  Prometheus-text with zero deps is exactly right for RFC req. 6
  (healthcheck/observability) and a trivial `/metrics` over the self-MCP
  HTTP transport. Replace workflow-centric counters
  (`workflow_starts`, `node_executions`, `policy_denials`) with
  agent-centric ones (subagents_spawned, subagents_failed, loop_turns,
  llm_calls, llm_tokens, tool_calls, subscriptions_active,
  triggers_fired). `escape_label` + `PROM_METRICS` table pattern → keep.
- `traceparent.rs` (208 lines): W3C trace-context parse/propagate. **DROP
  for v1** (no inbound HTTP trigger originating traces in the minimal
  runtime); revisit if distributed tracing across agents becomes a goal.

### 2.13 MCP registry + config — `mcp/registry.rs`, `mcp/config.rs`  → **ADAPT**

- **`mcp/config.rs` `McpServerDef`** (name + `command: Vec<String>` +
  env-by-secret-name + allowlists) and `validate_list` → **ADAPT.** The
  new config (RFC §10) declares MCP servers by `name=cmd…` flags +
  `--mcp-config FILE`. The `{name, command: Vec<String>, env}` shape is
  exactly right; **DROP** the `allow_tools`/`allow_resources` allowlists
  (the new authority model is "granted MCP subset" passed parent→child,
  RFC §6.3 — not a per-server tool allowlist string grammar). Keep
  `from_cli_stdio` as the template for the `--mcp` flag parser.
- **`mcp/registry.rs` `McpRegistry` / `McpServerHandle` / `resolve`** →
  **ADAPT.** The supervisor holds exactly this: a name→server-handle map,
  with `resolve(Some(name) | None)` semantics (lines 100-130). Keep the
  map + resolve logic; **DROP** the `ReloadableMcpClient` /
  `ReloadableMcpAllowlist` wrapping (hot-reload out of scope). The handle
  becomes `{name, client}` (no allowlist field).

### 2.14 Process-spawning patterns — scattered  → **ADAPT** (lift the patterns, build the supervisor)

The repo has three good, std-only spawn-and-manage exemplars worth
studying for the new **subagent process supervisor** (RFC §4.1, §6, req. 8):
- **`StdioMcpClient::spawn_with_env`** (mcp/client.rs) — spawn with piped
  stdio + custom env + child-handle retention + kill-on-drop. This is the
  template for spawning *subagents* too (re-exec `argv[0]` in subagent
  mode, RFC §4.2): pipe stdin/stdout for the control channel.
- **`tools/shell.rs` `run()`** (lines 189-264) — spawn + **background
  reader threads** draining stdout/stderr so a chatty child can't block
  the deadline + **try_wait polling loop with kill-on-timeout** + Unix
  signal extraction from `ExitStatus` (`ExitStatusExt::signal`, lines
  247-251) + `env_clear` + curated PATH + output caps (`read_capped`).
  **This is the best dead/stuck-subprocess-handling reference in the repo**
  (RFC req. 8) — the try_wait + deadline + SIGKILL + reap pattern is
  precisely what supervising a runaway subagent needs. Lift the pattern.
- **`secrets` command source** spawn (mod.rs lines 318-345) — simple
  `Command::output()` with status check.

The actual **supervised process *tree*** (parent scopes/controls children,
SIGCHLD reaping, depth/breadth limits, pause/resume/cancel relay) is
**net-new** — none exists. But the spawn/kill/reap/timeout primitives above
are the building blocks.

---

## 3. DROP entirely (no salvage relevant to the new design)

These are the retired-design superstructure; they conflict with RFC §1/§2
(no DAG, no policy DSL, no signing/auth as core):

- **Workflow DAG model/validator/engine:** `workflow/` (mod, model,
  validator), `engine/` (mod, runner, context, handler, outcome, record,
  checkpoint, template). The entire "predeclared validated TOML DAG, model
  fills one node, control flow is structure" model is explicitly retired
  (RFC §1). DROP. (One micro-exception: `engine/template.rs::walk_path`
  dotted-path lookup is a generic helper — re-derive if needed, don't lift
  the module.)
- **Policy / Rego:** `policy.rs`, `tools/policy.rs`, `mcp/allowlist.rs`.
  RFC §2/§13 explicitly removes the embedded policy engine; authority is
  "granted MCP subset." DROP.
- **Signing:** `signing/mod.rs` (Ed25519 detached-signature workflow
  verification). RFC §2: no signing subsystem as core. DROP.
- **Auth:** `auth/` (basic, bearer, hmac, mtls, oidc, config). RFC §2/§13:
  no embedded auth as core; the container/MCP boundary is the security
  model. DROP.
- **Triggers as-is:** `triggers/cron.rs`, `triggers/fs_watch.rs`,
  `triggers/http.rs`, `triggers/http_tls.rs`. The new trigger model is
  one-shot / loop-interval / **reactive-MCP-subscription** + time-schedule
  (RFC §5). `cron` (pulls `cron`+`chrono`) and `fs_watch` (pulls `notify`)
  are the wrong mechanism — reactivity comes from MCP `resources/subscribe`,
  not inotify, and scheduling comes from a simple interval timer or the
  external operator (RFC §11). DROP the trigger *crates*; the loop/interval
  timer is a few lines of `std::thread::sleep`. (Salvage the HTTP *parser*
  from `triggers/http.rs` per §2.7; DROP the rest.)
- **Conformance / testing-as-shipped:** `testing/` (fixture, runner),
  `embedded.rs`, the old `runtime.rs` (2794 lines — wired entirely to the
  workflow/engine/trigger/auth world). DROP `runtime.rs` wholesale; the new
  `main`/supervisor is rebuilt from the RFC. Keep generic test *techniques*
  (sh-based fake MCP server, in-process socket/HTTP fakes) as patterns, not
  files.
- **Built-in tool families:** `tools/fs.rs`, `tools/env.rs`,
  `tools/data.rs`, `tools/shell.rs` (the *node* wrapper). RFC §2: no
  built-in tool catalogue. DROP the tools; the only escape hatch is the
  gated self-MCP `exec` tool (RFC §9), for which `tools/shell.rs::run()`
  is the *implementation reference* (§2.14) but not the node.
- **Other retired bits:** `ratelimit.rs`, `server_config.rs`,
  `observability/audit.rs`, `observability/otel.rs`,
  `agent/{catalog,instructions,planner,review}.rs` (workflow-era agent
  helpers), `intelligence/{handler,backends,mod}.rs` wiring (the
  *handler* is engine-coupled; `backends.rs` `BackendDef`/`ProviderKind`
  validation is mildly reusable for provider selection but DROP the
  named-backend-map concept — the new design has a *single* intelligence
  endpoint, RFC req. 1).

---

## 4. Concrete salvage list (functions/types worth lifting)

| Source | Item | Verdict | Use in new design |
|---|---|---|---|
| `mcp/client.rs` | `StdioMcpClient::spawn_with_env` (Command/pipe setup) | ADAPT | spawn stdio MCP servers + template for subagent spawn |
| `mcp/client.rs` | `Drop for StdioMcpClient` (flush→kill→wait) | KEEP | child cleanup for MCP servers + subagents |
| `mcp/client.rs` | `initialize()` handshake + NDJSON framing in `rpc_call` | ADAPT | MCP init; reader-thread dispatch replaces the in-line loop |
| `mcp/client.rs` | sh-based fake-server test harness | KEEP (technique) | integration-test stdio MCP peers |
| `mcp/protocol.rs` | all JSON-RPC + tools/call + resources/read types | KEEP+extend | MCP wire; add tools/list, resources/list, subscribe, updated-notification |
| `intelligence/protocol.rs` | `write_frame` / `read_frame` (4-byte LE + 16 MiB cap) | KEEP | unix:/vsock: intelligence framing **and** control channel (RFC §6.2) |
| `intelligence/protocol.rs` | `Request`/`Response`/`Message`/`Usage` | ADAPT | LLM wire; widen for tool-calling |
| `intelligence/client.rs` | `UnixClient` | KEEP | `unix:` intelligence transport |
| `intelligence/client.rs` | `HttpClient` + `ParsedEndpoint` + http status/header parsers | ADAPT | `https://` transport machinery (swap body/parse to OpenAI shape) |
| `intelligence/client.rs` | `MockClient` | KEEP | test infra |
| `intelligence/providers.rs` | openai/openai-compatible request-build + `parse_response` | ADAPT | the standalone `https://` OpenAI-compatible adapter |
| `intelligence/providers.rs` | anthropic arm + `split_system` + key-safe `Debug` + build-time key probe + `truncate` | ADAPT/KEEP | in-binary anthropic dialect + hygiene |
| `tools/http.rs` | `parse_url`, `perform_request`, header/status parsers | ADAPT | the **one** consolidated internal HTTP/1.1 client |
| `tools/http.rs` | `render_declared_headers` / `substitute_secret_placeholders` | KEEP | safe header construction (CR/LF + secret-only) |
| `triggers/http.rs` | `parse_request` (+ caps, 431/413, silent_close) | KEEP | serve self-MCP over HTTP + `/healthz` |
| `triggers/http.rs` | accept-loop + `InFlightGuard` + `shutdown_and_drain` | KEEP (pattern) | graceful-drain SIGTERM citizen (RFC §11) |
| `agent/loop_node.rs` | `extract_json_object` | KEEP | robust JSON extraction from model text |
| `agent/loop_node.rs` | loop skeleton (deadline/budget/dispatch/feedback/exhaust + audit events) | REWRITE-from | the agentic loop (RFC §6.1) |
| `agent/loop_node.rs` | recoverable-error discipline + `ToolBroker` *shape* | ADAPT | route tool-call → result/error JSON → transcript (MCP catalogue, not broker) |
| `budget.rs` | `apply_rlimits` (Unix) + `RlimitResource` alias | KEEP | optional OS resource caps (RFC §13) |
| `budget.rs` | `BudgetTracker` token CAS counter + `clamp_run_time` | ADAPT | per-run + tree-wide token ceiling, +steps/+depth |
| `secrets/mod.rs` | `resolve(name)` front door + env fallback + `file` source | KEEP | credential resolution (env/file, RFC §7.2/§13) |
| `secrets/mod.rs` | never-Serialize + `Debug=***` hygiene | KEEP | keep credentials out of logs/transcripts |
| `signals.rs` | `install_shutdown_handlers` (Unix, no-SA_RESTART) + `shutdown_requested` | KEEP | clean SIGTERM/SIGINT; add SIGCHLD reaping |
| `observability/mod.rs` | `tracing` install + `Format`/`LogTarget` + `StderrWriter`/`StdoutWriter` + `CapturingWriter` | ADAPT | structured stdout/stderr logging (RFC §11) |
| `observability/metrics.rs` | `Metrics` atomic counters + `to_prometheus` + `escape_label` | KEEP (re-counter) | `/metrics` healthcheck (RFC req. 6) |
| `tools/shell.rs` | `run()` spawn+reader-threads+try_wait-timeout-kill+signal-extract+`read_capped` | KEEP (pattern) | gated self-MCP `exec` tool (RFC §9) + subagent supervision (req. 8) |
| `mcp/config.rs` | `McpServerDef{name,command,env}` + `from_cli_stdio` | ADAPT | `--mcp name=cmd` parsing (drop allowlists) |
| `mcp/registry.rs` | `McpRegistry`/`resolve` name→handle map | ADAPT | supervisor's MCP server map (drop reloadable wrappers) |

---

## 5. Dependency implications

The salvageable code keeps the rewrite comfortably inside RFC §12's budget:
- **`serde` / `serde_json`** — kept (the one non-negotiable dep).
- **`libc`** — kept for signals + rlimits (Unix).
- **`tracing` + `tracing-subscriber`** — kept for structured logs; drop
  `tracing-appender` (rotation) and the entire `otel`/tokio stack.
- **`ureq` (+ rustls)** — kept *behind a feature* for `https://`
  intelligence; the RFC's bias is to terminate TLS at a sidecar and run
  plaintext-to-localhost, in which case even this is optional.
- **vsock** — net-new, thin crate or raw `libc`, behind a feature.
- **Dropped deps:** `cron`, `chrono`, `notify`, `regorus`, `jsonschema`,
  `ed25519-dalek`, `base64`, `sha2`, `hmac`, `jsonwebtoken`, `x509-parser`,
  `rustls-pemfile`, `arc_swap` (hot-reload), `ctrlc` (if Unix-only),
  `tracing-appender`, the whole `opentelemetry*` family, `toml` (config is
  flat env/flags, not TOML).

The biggest *new* engineering — not salvageable because it doesn't exist —
is: (1) the reactive MCP client (reader thread + correlation map +
notification routing + subscribe), (2) the supervised subagent **process
tree** (spawn/scope/control/reap/depth-limit), (3) the supervisor↔subagent
**control channel** (length-framed JSON-RPC, lifting `read_frame`/
`write_frame`), (4) **native tool-calling** in the intelligence request/
response, and (5) agentd serving its **own MCP server**.
