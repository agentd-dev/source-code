# Changelog

All notable changes to **`agentd`** — the minimal, MCP-native, reactive agent
runtime (developed in the `agentd-dev` org). The format is loosely
[Keep a Changelog](https://keepachangelog.com); versions are the released git tags
(`vX.Y.Z`) and the published image `ghcr.io/agentd-dev/agentd:X.Y.Z`.

## v1.4.0 — OpenAI-compatibility fixes + AAuth provider/token validation

Real-provider hardening. Proving the eval harness (RFC 0024) against live OpenAI
surfaced three genuine compatibility bugs in the intelligence dial — all fixed —
and the AAuth agent-identity client gained the provider/token validation the
protocol calls for. The stock binary's behavior is otherwise unchanged; crates
step to `agentd` / `agentd-cli` **1.4.0** (library crates `agentd-mcp` /
`agentd-net` stay **0.3.0** — unchanged).

### Fixed (intelligence dial — OpenAI/Anthropic compatibility)

- **Dotted tool names are sanitized on the wire.** OpenAI/Anthropic reject tool
  names that aren't `^[a-zA-Z0-9_-]+$`, but agentd uses namespaced names
  (`resource.read`, `subagent.spawn`). Each name is now made wire-safe on the
  outbound request (in the tool defs *and* the assistant message-history
  `tool_calls`) and reverse-mapped on the response, so tool-calling against real
  OpenAI works and routing is unaffected. No-op when every name is already legal.
- **Reasoning models use `max_completion_tokens`.** gpt-5 / o-series reject
  `max_tokens`; the request now selects the correct token-limit parameter by
  model, so those models produce tool calls instead of an HTTP 400.
- **Transient `429`/`5xx` are retried before failing.** `complete_once` now
  retries a momentary provider blip on the same endpoint (bounded, short
  backoff) before the error propagates. Previously, `once` mode — which arms no
  higher-level retry loop — surfaced a single transient error as an immediate
  exit 4. A non-transient `4xx` (bad request, auth) still surfaces immediately.

### Added (AAuth [DRAFT], RFC 0023 §7.1 — `--features aauth`)

- **Agent-Provider metadata discovery + issuer validation.** At startup agentd
  fetches `/.well-known/aauth-agent.json` and enforces the AAuth protocol's
  anti-host-poisoning rule (a document whose `issuer` ≠ the configured provider
  aborts enrollment). Best-effort: a provider that publishes no document works.
- **Agent-token claim validation.** The agent token is acted on rather than held
  opaque: agentd refreshes against the token's own `exp`, and fails fast if the
  token's `iss` isn't the configured provider, its `ps` isn't the configured
  Person Server, or its `cnf.jwk` isn't the signing key — each a misconfiguration
  that would otherwise be a silent wall of downstream `401`s.

### Tooling

- **Evaluation harness (RFC 0024, `bench/`).** A dependency-free (stdlib) rig
  that drives the real binary per task and grades it: BFCL tool-calling with a
  faithful AST value-matcher, a τ²-bench-style simulated-user conversation loop,
  an execution-graded coding suite, a sandboxed shell/file environment, and a
  workflow-lift ablation — plus a model×benchmark matrix, run live against
  OpenAI. Not part of the shipped binary.

## v1.3.0 — library split, code-registered tools, AAuth agent identity & lifetime budgets (RFC 0022/0023/0025)

agentd is now consumable as a **library**. The workspace splits into four
publishable crates around one engine; the stock binary is behaviorally
unchanged. Library crates step to `agentd-mcp` **0.3.0** / `agentd-net`
**0.3.0** alongside the `1.3.0` binary.

### Added

- **Crates**: `agentd-core` (the engine — lib name `agentd`), `agentd-cli` (the
  thin binary shell producing the `agentd` command), `agentd-mcp` and
  `agentd-net` (the already-reusable MCP + transport libraries, renamed for
  crates.io; the bare name `agentd` on crates.io belongs to an unrelated
  project). Feature graphs are isomorphic between core and cli; CI gates both
  per matrix row.
- **Code-registered tools** (`agentd::tools`, RFC 0022 §4): an embedder
  registers native Rust tools (`CodeTool::new(name, description, schema,
  handler)`) that join the model's catalogue, are addressable from workflows as
  the reserved server **`code`**, and are callable via the public
  `tools::call`. Dispatch precedence self → code → MCP: the orchestration
  surface is unshadowable and a remote server cannot steal a first-party
  tool's calls (`ToolClass::Code`). The **stock CLI registers nothing** — its
  no-local-code posture holds by construction; the manifest surfaces
  `surfaces.code_tools` only when non-zero.
- The compile-guaranteed embedder reference:
  `crates/agentd/examples/custom-cli.rs` (built by CI, runs offline);
  [docs/embedding.md](docs/embedding.md); RFC 0022 with the three
  API-stability tiers.

### Added (AAuth [DRAFT], RFC 0023 — `--features aauth`)

- Agent-side auth for calling **AAuth-protected MCP servers**: an Ed25519 agent
  identity (`agentd::aauth::AgentKey`), an Agent-Provider enroll + agent-token
  client with cache/refresh (`ApdClient`), and **RFC 9421 HTTP Message
  Signatures** on every outbound MCP request — no shared API key. Wired via a
  dependency-free `RequestSigner` seam in `agentd-mcp` (the crypto stays in
  `agentd-core`; `ring` is a direct edge only under `aauth`, the same crate
  rustls already links). One identity per process tree (rides the spawn
  payload). Config: `--aauth-provider` / `--aauth-key-file` /
  `--aauth-enroll-token` / `--aauth-person-server` (+ `AGENT_AAUTH_*`).
  Manifest: `surfaces.aauth`.
- **Now ships in the release binary and container image** (the `aauth` feature
  joined the default release feature set). Its crypto (`ring`) is already linked
  by the default `tls`/rustls transport, so shipping it adds **no new crate to
  the graph** — unlike `cel`, which stays build-from-source. Still a compile-time
  feature (a trimmed build can drop it) and still `[draft]`.
- The transport runs the full **request reaction loop** (sign → inspect
  `AAuth-Requirement`/`AAuth-Access` → satisfy → re-sign → retry, bounded), so
  **all three access modes run end to end**: **Case A** (identity-based),
  **Case B** (adopt a returned `AAuth-Access` token and replay it), and
  **Case C** (user-scoped — the Person-Server resource-token → user auth-token
  exchange, presented as the `Signature-Key`). Plus **discovery**
  (`/.well-known/aauth-resource.json`), **content-digest** covering when a
  server requires body integrity, and **per-server opt-out** (`aauth: false` on
  a `--mcp` config entry).
- **Federated enrollment** (`--aauth-enroll-assertion-file` /
  `AGENT_AAUTH_ENROLL_ASSERTION_FILE`): the enroll body carries an
  `enrollment_assertion` (e.g. a Kubernetes projected ServiceAccount token),
  **re-read fresh on every enroll** so a rotated token is always current — the
  secret-free fleet path (no shared enrollment secret, no operator key custody).
- **The intelligence dial is signed** (agentctl RFC 0024 §7.1): with an identity
  installed, agentd applies the same RFC 9421 headers to its `--intelligence`
  requests (chat POST + `/v1/models` GET), so a model gateway can attest the
  individual agent by signature instead of source IP. Additive — the bearer, if
  any, still rides alongside.
- Proven by four live-socket e2e tests (`aauth_e2e` Case A; `aauth_flow_e2e`
  Case C over a real `McpClient`; `aauth_enroll_assertion_e2e` federated
  fresh-read; `aauth_intel_sign_e2e` signed model dial). **Draft**: the wire
  tracks the AAuth guide agentd was built against and may shift.
- The `hwk` Signature-Key (enroll + single-key token refresh) presents the raw
  Ed25519 key as **inline `kty`/`crv`/`x` structured-field params** per
  `draft-hardt-httpbis-signature-key` — not a `jwk="<b64url JSON>"` blob, which a
  conformant Agent Provider rejects (`invalid_key`). Verified by driving the real
  binary against a real Agent Provider end to end (the mock-AP unit tests do not
  parse the `hwk` form, so this surfaced only under real integration).

### Added (harness-tracked budgets, RFC 0025)

- **Per-instance lifetime token budget** (`--budget-tokens-lifetime` /
  `AGENT_BUDGET_TOKENS`, config-file `limits.lifetime_tokens`): a cumulative
  token cap across **all** runs/reactions of an instance, distinct from the
  per-run `--max-tokens` box. Bounds a long-lived agent on a path with no
  metering gateway (e.g. an AAuth direct dial). `0`/unset = unbounded (today's
  behaviour). A bounded `once` run folds `min(max_tokens, cap)` and trips
  `EXIT_BUDGET(7)`; a `reactive`/`loop`/`schedule` daemon stops accepting new
  work and **drains cleanly** (exit `0`, or `--budget-exit-code`) with a
  `budget.exhausted` event. Metered where all child tokens converge in the
  supervisor.
- Observability (`metrics_schema` → **1.1**, additive): the gauge
  `agent_budget_tokens_remaining` (the alerting/scaling hook) and the
  `tokens_lifetime` value of `agent_limit_exceeded_total{limit}`; a one-shot
  `limit.threshold` event fires the first time usage crosses 90% of the cap.

### Changed

- `--mcp code=…` is refused (`code` is the reserved code-tools server name).
- Building from source: the binary is now `cargo build -p agentd-cli`
  (release artifacts unchanged).

## v1.2.0 — workflow dialect 2: durable, parallel, human-in-the-loop workflows (RFC 0021)

Workflows now match — and in places exceed — the code-first agent SDKs, while
staying a declarative JSON artifact. Zero new dependencies (the moat holds at 3).
`contract_version` stays `1.0`; feature-detect via `surfaces.workflow.dialect >= 2`.

### Added

- **Human gates over A2A** (`human` node): a workflow suspends and asks a person
  — the served task projects **`TASK_STATE_INPUT_REQUIRED`** with the payload as
  its status message; the reply is a spec-native `SendMessage` carrying the
  `taskId` (the A2A multi-turn shape — no agentd-specific API). Reply /
  `reply_uri` update / timeout race, first wins. Duplicate reply → `-32004`,
  unknown task → `-32001`; degrades to a plain wait without serving.
- **The MCP checkpointer** (`checkpoint` graph policy): per-superstep durable
  run state, with the checkpointer as *any MCP server* implementing
  `state.put`/`state.get`/`state.list` (monotonic-seq guard; a refused put is
  always fatal — the split-brain protection). `--workflow-resume
  <server>:<key>[@seq]` (+ `AGENT_WORKFLOW_RESUME`, `--workflow-resume-force`):
  crash-recovery from the latest envelope (exactly-once for completed nodes,
  at-least-once for the in-flight one), `@seq` under a new run-id = a fork,
  hash-mismatch = refusal (exit `5`). Budgets carry over across resume.
  Envelopes bind the graph by SHA-256 (hand-rolled FIPS 180-4, NIST-vector
  tested).
- **Write reducers** (`writes_mode: overwrite|append|merge|union` on every
  writing node): accumulate instead of clobber; pure, clamp-aware, type
  mismatch → the `error` edge with a readable marker.
- **The `parallel` node**: named heterogeneous branch bodies run concurrently
  on the SAME 8-lane pool `foreach` uses (composition never multiplies
  concurrency); ≤16 branches, step pre-charge, shared token pool, results as
  one object keyed by branch name, `fail_fast|continue`.
- Manifest: `surfaces.workflow.{dialect: 2, checkpoint: true, kinds: [12]}`.

### Changed (fail-closed hardening)

- **Unknown workflow fields are define-time errors** (a typo'd `writes_mode`
  can no longer silently mean overwrite): one strict `parse_graph` front door
  behind `--workflow`, `workflow.define`, and `workflow.patch`. Dialect-1
  graphs are byte-identical on the wire and behaviorally unchanged.
- A2A `SendMessage` now accepts `message.taskId` as a gate-reply continuation
  of an existing task (`-32004` when nothing is waiting).

### Verified

Two new real-process e2e suites: a `--mode workflow` run SIGKILLed mid-node
resumes from its checkpoint on a real HTTP checkpointer and completes with the
pre-crash blackboard; a served A2A task flows WORKING → INPUT_REQUIRED → reply
→ COMPLETED on the wire. 686/686 featured tests (36 new), 384/384 default,
conformance 38/38. RFC 0021 (Implemented) is the normative spec;
[docs/workflows.md](docs/workflows.md) the guide.

## v1.1.0

### Added

- **Bare env spellings for the two required inputs.** `INTELLIGENCE` is now
  accepted alongside `AGENT_INTELLIGENCE` (mirroring the existing bare
  `INSTRUCTION`), so the minimal launch is `INSTRUCTION=… INTELLIGENCE=… agentd`.
  Precedence within the env layer is by specificity — branded `AGENTD_*` >
  neutral `AGENT_*` > bare — so a prefixed spelling always wins over the bare
  one. Additive; no existing spelling changes meaning. `contract_version`
  stays `1.0`.

### Fixed

- **`AGENT_INSTRUCTION` is honoured.** It was silently ignored (de-branded to an
  `AGENTD_INSTRUCTION` nothing read), so following the neutral `AGENT_*`
  convention for the instruction produced a confusing "missing instruction"
  error. It now works like every other `AGENT_*` key.
- **Docs/site consistency sweep** (post-1.0.0-reset): five run modes everywhere
  (the modes page gains the `workflow` mode row); exit `124` correctly
  attributed to the supervisor hard-kill backstop (a self-detected `--deadline`
  is `7`); stale stdio-era claims in `architecture.md` ("MCP servers over
  stdio", "gated `exec`") rewritten to the remote-HTTPS / no-exec reality; one
  stray `agentd://` → `agent://`; wire/log example versions → 1.0.0; the
  landing page's workflow card lists all **ten** node kinds (`join` was
  missing); `use-cases.md` added to the docs index.

## v1.0.0 — first public release

The first public release of **`agentd`**: a small, MCP-native, HTTPS-everywhere
agent runtime built for Kubernetes. It takes an instruction plus tools from remote
MCP servers and runs the agentic loop — as a one-shot, a loop, a schedule, a
reactive daemon, or an agent-authored workflow — supervised, bounded, and
observable.

`agentd` is the **reference implementation of the neutral Agent Control Contract
(ACC 1.0)**. It is named `agentd` (the daemon) but speaks the neutral `agent`
contract, so the agentctl control plane drives it without depending on this binary:
`agent://` resources, the `agent_` Prometheus metric prefix (`metrics_schema` 1.0),
the `AGENT_*` env/config convention, and a `--capabilities` manifest carrying
`contract_version` 1.0.

### Runtime

- **HTTPS everywhere.** Intelligence, the MCP client, the served self-MCP, and the
  A2A / operator control surface are all HTTP(S) over mTLS; plaintext `http://` is a
  loopback-only dev carve-out. There is no unix, vsock, or stdio transport and no
  local execution surface — the only process agentd starts is itself (a
  `current_exe()` re-exec for subagents). TLS (rustls + ring, bundled roots) is a
  default feature.
- **Remote MCP tools.** `--mcp name=<https://host/mcp>` (Streamable HTTP: sessions +
  SSE, multi-version negotiation). Per-server auth is secret-free — header templates
  (`{{secret:NAME}}`), an mTLS client identity, or OAuth 2.1 client-credentials
  (`--features oauth`).
- **Serving requires identity.** `--serve-mcp https://host:port` with
  `--serve-cert`/`--serve-key`; a non-loopback listener MUST authenticate peers
  (`--serve-client-ca` mTLS and/or `--serve-bearer`). Verified identity — never the
  transport — mints the Management origin.
- **Run modes:** once, loop, schedule, reactive daemon, and workflow. Reactive
  subscriptions support content conditions, an in-turn `await_resource` wait, and
  live warm-session tool-catalogue refresh on `tools/list_changed`.

### Workflows

- **Agent-authored workflows** (`--features workflow`, dependency-free): an explicit
  cyclic graph the agent defines and drives — `workflow.define` / `workflow.run`
  (sync or `detach` into a supervised child) / additive `workflow.patch`, or the
  operator-pinned `--mode workflow --workflow <file>`. Ten node kinds (`agent`,
  `tool`, `assign`, `infer`, `branch`, `foreach`, `join`, `subgraph`, `wait`,
  `halt`) with layered, attributed termination (a step budget, a shared token pool,
  a wall-clock deadline, per-node visit caps, and a progress guard).
- **Reactive-daemon workflows** (`--mode reactive --workflow <file>`): waits hold no
  process — the child suspends with its serialized run slice and a fresh child
  resumes on update/timeout, the budget continuing across processes.
- **CEL** (`--features cel`, the one dependency-bearing opt-in): compile-checked,
  fail-closed predicates, computed `assign.expr`, `infer.check` constraints, and
  reactive wake conditions.

### A2A

- Real agent-to-agent interoperability over HTTPS with the **bare PascalCase** method
  binding (`SendMessage`, `GetTask`, `CancelTask`, `ListTasks`,
  `SendStreamingMessage`, `SubscribeToTask`); `SendMessage` returns the
  `{"task": <Task>}` envelope; SSE streaming terminates on the terminal task state
  and stream close. Peer client-auth via bearer header templates and/or a presented
  mTLS identity.
- Operator control is the `a2a.*` method family — `a2a.Drain`, `a2a.LameDuck`,
  `a2a.Pause`, `a2a.Resume`, `a2a.Cancel` — Management-gated JSON-RPC methods
  (refusals as protocol errors).

### Cloud-native contract

- The frozen exit-code table (a clean drain is 0, not 143), the run-outcome report,
  the metrics schema (`metrics_schema` 1.0), the `agent://events` stream,
  liveness/readiness probes, `--budget-exit-code`, horizontal scaling (sharding +
  work-claim leases + standby), and SIGHUP/inotify hot reload of the reloadable
  config subset.

### Security

- The lethal-trifecta (Rule-of-Two) gate as the single `validate()` authority, and
  **structural secret-freedom**: no credential ever reaches the capabilities
  manifest, the config file, or the identity path. The served MCP endpoint is
  hardened (`Origin` validation as a DNS-rebinding defense; a per-`initialize`
  `Mcp-Session-Id`).

### Conformance

- Every contract surface validates against its schema and behaves as specified — see
  `CONFORMANCE.md`.
