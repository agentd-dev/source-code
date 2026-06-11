# agentd — Operations Guide

**Audience:** operators rolling `agentd` into production or pre-production
environments. Gives you the deployment shapes we support, how to build
the right artifact for each, how auth / TLS / logging are wired, and what
to expect at runtime (signals, exit codes, healthchecks, drain).

**Status:** authoritative as of current release. Matches the binary produced
from `crates/agentd/` at this commit. If something in the code
disagrees with this doc, the code wins — please file a doc fix.

**See also:**

- [`architecture.md`](architecture.md) — module layout, invariants,
  lifecycle.
- [`capabilities.md`](capabilities.md) — what workflows can express.
- [`configuration.md`](configuration.md) — every knob in detail.
- [`maturity.md`](maturity.md) — honest production-readiness snapshot
  + remaining gaps.
- RFC: [`rfcs/0001-bounded-workflow-runtime.md`](../../rfcs/0001-bounded-workflow-runtime.md).

---

## 1. Deployment shapes at a glance

There are two axes: **how the workflow gets into the binary** and **how
the binary is invoked**.

### 1.1 How the workflow reaches the runtime

| Shape | Artifact | How | Best for |
|---|---|---|---|
| **External** | Generic `agentd` binary + a separate `.toml` | `agentd --config /etc/agentd/wf.toml` | Dev, iterating on a workflow without rebuilds, multi-tenant shells where the same binary fronts several workflows over time. |
| **Embedded** | Purpose-built binary (wf baked in) | `AGENTD_EMBED_CONFIG=/abs/path cargo build --release` produces a binary that holds the TOML as `include_str!` | Single-purpose appliances, containers where the config lives in the image, reproducible deploys where "the binary IS the workflow". |

Embedded mode is validated twice: once at `cargo build` (fast structural
check in `build.rs` — `name` present, no duplicate / dangling node ids,
edges and HTTP routes point at real nodes, start-node entry_nodes exist),
and once at startup (the full `workflow::validate` — acyclicity,
reachability, fan-in/fan-out, start-node source constraints, policy
references). External mode skips the build-time pass and runs only the
startup pass.

### 1.2 How the binary is invoked

The mode is **inferred from the workflow** unless you override it:

| Mode | Inferred when | Behaviour |
|---|---|---|
| **one-shot** | No `[[http_routes]]` entries | Pick a start node (auto or `--start`), read `--input FILE` as the trigger payload, run the DAG, emit the outcome JSON to stdout, exit with `0` (Success) or `5` (any non-success). |
| **serve** | At least one `[[http_routes]]` | Bind `--bind` (default `127.0.0.1:8080`), accept requests, dispatch matching routes to the engine, stay up until SIGTERM / SIGINT. |

Override with `--mode once|serve` or `AGENTD_MODE=once|serve`.

No CLI subcommands exist — `agent serve` is not a command. The single
entry point (`runtime.rs`) decides what to do based on flags + workflow
shape. This was an explicit R1 pivot; the help text in `agentd --help` is
the authoritative list of flags.

---

## 2. Build modes

The binary's capability surface is frozen at compile time by Cargo
features. Four common recipes:

### A. Default — tooling for a standalone deploy, HTTP auth on

```bash
cargo build --release -p agentd
```

Features: `tools-fs + tools-env + tools-data + trigger-http + auth`.
No HTTP-outbound, no shell, no MCP, no TLS. Good for local and for
behind-a-reverse-proxy deployments where TLS terminates at the proxy.

### B. Hardened webhook receiver — TLS + HMAC + full tool-less posture

```bash
cargo build --release -p agentd \
  --no-default-features \
  --features "tools-fs,tools-data,trigger-http,auth,server-tls"
```

Drops `tools-env` (no env-reads) and `tools-http` / `tools-shell`. This
is the canonical shape for an externally reachable agent that accepts
signed webhooks and writes fixtures. Add `server-tls` for in-process
mTLS (see §4.4).

### C. Kitchen sink — everything wired

```bash
cargo build --release -p agentd --features "tools-http,tools-shell,tools-mcp,server-tls"
```

Pulls every tool family in. Use sparingly — every feature you don't need
is code that can't be in the binary.

### D. Embedded appliance — workflow baked in + minimal features

```bash
AGENTD_EMBED_CONFIG=/abs/path/to/wf.toml \
  cargo build --release -p agentd --no-default-features \
  --features "tools-fs,trigger-http,auth,server-tls"
```

The resulting binary runs the baked-in workflow if invoked with no
`--config`. Operators can still point `--config /alt.toml` at an
external file to override for debugging without rebuilding.

The build-time validator speaks the same error dictionary as the runtime
validator — any failure there means `cargo build` exits with a
`cargo:warning=agent: …` line and a panic message describing the first
offending issue. Nothing else in `build.rs` runs on a dirty tree: the
embedded path sets `cargo:rerun-if-env-changed=AGENTD_EMBED_CONFIG` and
`cargo:rerun-if-changed=<abs path>`, so incremental builds only
re-validate when the file or env actually changes.

### 2.1 Feature reference

| Feature | Pulls in | Enables |
|---|---|---|
| `tools-fs` | — | `read_file`, `write_file`, `create_dir` |
| `tools-env` | — | `read_env` |
| `tools-data` | — | `parse_json`, `template_render`, `json_select` |
| `tools-http` | — | `http_request` outbound |
| `tools-shell` | — | `shell_run` (allowlisted commands only) |
| `tools-mcp` | — | `call_mcp_tool`, `read_mcp_resource`, `trigger-mcp` plumbing |
| `trigger-http` | — | `HttpServer`, routes, one-shot-vs-serve switch |
| `trigger-mcp` | — | MCP triggers |
| `intel-unix` | — | Intelligence JSON-RPC Unix client (LLM adapter) |
| `intel-http` | — | Intelligence JSON-RPC HTTP client |
| `auth` | `sha2`, `hmac` | Bearer + HMAC-SHA256 webhook verification |
| `server-tls` | `rustls`, `rustls-pemfile` (implies `auth`) | In-process TLS termination + mTLS client-cert verification |
| `schema` | `jsonschema` | Validate `llm_infer` output against a JSON Schema file (off → JSON-only check) |
| `intel-remote` | `ureq` | Remote LLM providers (Anthropic / OpenAI / Gemini / openai-compatible) |

The default feature set is `tools-fs + tools-env + tools-data +
trigger-http + auth`. Everything else is opt-in.

---

## 3. Runtime artifacts

### 3.1 Binary

One statically-linked bin: `target/release/agent`. Linux x86_64 and
aarch64 both supported. No libc calls outside `libc` for `sigaction`
and the standard Rust prelude, so it runs on distroless / scratch
images provided glibc (or musl if you cross-compile) is present.

Release profile: `opt-level="s"`, `lto=true`, `strip=true`,
`panic="abort"`. Expect ~6–8 MB on x86_64.

### 3.2 Filesystem footprint

No state files. The runtime is stateless — no cache, no spool, no DB.
Restart is free. Whatever the workflow itself writes via `write_file` is
governed by the manifest's `policy.fs` allowlist.

### 3.3 Processes

- **one-shot mode**: one process, lives for the duration of the run.
- **serve mode**: one process holding one TCP listener thread plus one
  OS thread per accepted connection. No thread pool — each connection
  spawns its own thread, which returns when the request completes.
  Accept loop runs with `set_nonblocking(true)`; a 50 ms tick polls the
  shutdown flag so SIGTERM is observed promptly.

### 3.4 Network

- **Inbound**: `--bind HOST:PORT` (default `127.0.0.1:8080`). Plain
  HTTP/1.1 unless `[server.tls]` is configured, in which case the same
  socket speaks TLS.
- **Outbound**: only if the workflow uses `http_request`, `call_mcp_tool`
  (stdio child), or the intelligence adapter (Unix socket / HTTP). The
  manifest's `policy.http.allow` controls which URLs are reachable.

### 3.5 Healthcheck + metrics

Two always-live endpoints in serve mode (no auth, not rate-limited):

- `GET /healthz` → `200 OK` with a JSON body — for k8s readiness /
  liveness probes.
- `GET /metrics` → `200 OK` with Prometheus 0.0.4 text-exposition
  content — for scrape targets. Every counter is labelled with the
  workflow name; an `agentd_build_info` gauge carries the crate
  version. The emitted counter names:

  | Counter | Meaning |
  |---|---|
  | `agentd_workflow_starts_total` | Workflow executions started. |
  | `agentd_workflow_completions_total` | Executions reaching a terminal success state. |
  | `agentd_workflow_failures_total` | Executions that ended in a Failed outcome. |
  | `agentd_workflow_timeouts_total` | Executions terminated by the per-run deadline. |
  | `agentd_workflow_errors_total` | Executions that aborted with an engine error. |
  | `agentd_node_executions_total` | Node dispatch attempts (includes retries). |
  | `agentd_node_failures_total` | Node dispatches that returned a non-success status. |
  | `agentd_policy_denials_total` | Tool invocations refused by the manifest policy. |

### 3.6 Run records + inspection

Metrics tell you the aggregate; a **run record** tells you what one run
actually did. In one-shot mode, `--record PATH` (or `AGENTD_RECORD`)
writes a structured JSON account of the run — the per-node trace with
each node's output and timing, the cost (llm calls / tokens / policy
denials), the wall-clock, and the outcome (or the error that aborted
it). It is written whether the run completed, failed, timed out, or
errored.

```bash
agentd --config wf.toml --input event.json --record /tmp/run.json
agentd inspect /tmp/run.json
```

`agentd inspect` renders the record as a readable timeline:

```
run exec-00000001  workflow=demo  status=completed
  start=main  3 ms  1 llm call(s) / 128 tokens  0 policy denial(s)
  path:
     1. classify [llm_infer] continue →alpha  2 ms
        output: {"content":"…","parsed":{"decision":"alpha"}}
     2. done [terminate] terminate  0 ms
```

The record is plain JSON keyed for machine consumption. Its
`execution_id` matches the `execution_id` field in the audit log, so a
record and its audit events line up. A browser inspector at
[agentd.dev/inspect](https://agentd.dev/inspect) renders the same file
visually (paste or upload — it runs entirely client-side, nothing is
uploaded). Records may contain node outputs verbatim — treat a record
file with the same care as the data it processed.

### 3.7 Human-in-the-loop + durable execution

A `pause_for_approval` node suspends a run for a person. When the engine
reaches one it writes a **checkpoint** under `--state-dir` (or
`AGENTD_STATE_DIR`) — the accumulated node outputs and where to resume —
and stops with a `paused` outcome and exit code **7**. A human reviews
(the record / audit trail), then continues the run by id:

```bash
# Runs until the approval gate, then checkpoints and pauses.
agentd --config deploy.toml --state-dir /var/lib/agentd/state --input change.json
#   → {"status":"paused","run_id":"exec-…","last_node":"approve"};  exit 7

# Later, after review — continue from the node after the pause.
agentd --config deploy.toml --state-dir /var/lib/agentd/state --resume exec-…
#   → {"status":"completed", …};  the checkpoint is retired
```

The resume re-enters the same traversal at the pause node's successor
with the checkpoint's node outputs restored; it gets a fresh deadline.
Resuming the *same* workflow is enforced (a checkpoint records its
workflow name). A `pause_for_approval` node without a `--state-dir` is a
configuration error — there is nowhere to persist the run. Run ids are
unique across processes, so concurrent paused runs never collide.

Checkpoints contain node outputs verbatim: a state directory deserves
the same protection as the data the workflow handles. Exit code 7 lets a
supervisor (systemd, a queue worker) distinguish "awaiting approval"
from success (0) or failure (5).

---

## 4. Security posture

### 4.0 Workflow signing (supply-chain)

With the `signing` feature compiled in, the runtime verifies a
detached Ed25519 signature over the workflow TOML before the DAG
validator runs. Fail-closed when `[signing].required = true` (or
`--signing-required` / `AGENTD_SIGNING_REQUIRED=1`).

```bash
openssl genpkey -algorithm Ed25519 -out agent-signing.key
openssl pkey -in agent-signing.key -pubout -out agent-signing.pub
openssl pkeyutl -sign -inkey agent-signing.key \
    -rawin -in workflow.toml | base64 -w 0 > workflow.toml.sig
```

`[signing]` block:

```toml
[signing]
required = true
public_key_file = "/etc/agentd/signing.pub"
algorithm = "ed25519"                   # default; only value supported in v1
```

Audit events on `agentd::audit`:
`signing.verified`, `signing.sig_missing`, `signing.sig_malformed`,
`signing.pubkey_malformed`, `signing.verification_failed`,
`signing.bypassed` (warn), `signing.unsupported`. Every event
carries `key_fingerprint = "<16-hex>"` when the pubkey is loadable,
so log readers can pin the key without seeing the PEM.

Embedded workflows: pair `AGENTD_EMBED_CONFIG=/abs/workflow.toml`
with `AGENTD_EMBED_CONFIG_SIG=/abs/workflow.toml.sig` at build time;
`build.rs` decodes the base64 once and bakes raw signature bytes
into the binary.

Full design: [`rfcs/0002-signed-workflows.md`](../../rfcs/0002-signed-workflows.md).

### 4.05 Resource budgets

The `[budget]` block caps resources **process-wide** (agent is a
micro-agent — one workflow per process, so per-workflow and
per-process are the same unit). Applied at startup via POSIX
`setrlimit` + a runtime counter.

```toml
[budget]
max_memory_mb     = 512     # RLIMIT_AS; SIGKILL on breach
max_cpu_secs      = 300     # RLIMIT_CPU; SIGXCPU then SIGKILL
max_run_time_secs = 60      # clamps --timeout-secs
max_fs_write_mb   = 200     # cumulative write_file bytes
```

Notes:

- `max_memory_mb` maps to `RLIMIT_AS` (virtual address space) —
  the closest POSIX cap on memory usage. `RLIMIT_RSS` is largely
  unenforced on modern Linux.
- `max_run_time_secs` takes the **smaller** of itself and
  `--timeout-secs`, so a CLI flag can shrink the per-run ceiling
  but never widen the declared budget.
- `max_fs_write_mb` is tracked in a shared atomic counter; a
  `write_file` that would cross the cap fails with a `budget.
  fs_write_denied` audit event instead of writing partial data.
- `setrlimit` failures (sandboxed containers without
  `CAP_SYS_RESOURCE`, restricted seccomp) emit a warn audit
  event but don't abort startup — the budget simply doesn't
  apply, and the audit trail records why.

### 4.0b Secret injection model (env vars only)

The harness has **one** supported secret-injection mechanism:
**environment variables, read at request time**. Every
secret-carrying auth field has a `*_env` variant that names an
env var:

| Surface | Config field | Re-read when |
|---|---|---|
| Static bearer tokens | `[auth.bearer.<name>].tokens_env` | Every request |
| HMAC webhook secret | `[auth.hmac.<name>].secret_env` | Every request |
| Intelligence HTTP bearer | `--intel-http-bearer-file` / `AGENTD_INTEL_HTTP_BEARER` | Startup + SIGHUP |

Rotating an env-var secret is **SIGHUP-free**: the request-path
code calls `std::env::var(...)` on every check, so replacing
the env var takes effect for the next request. Fleet-rotating
an env var is an orchestrator-native operation — systemd
`systemctl daemon-reload && systemctl restart` if it lives in
`EnvironmentFile`, k8s rolling-update if it's in a `Secret`.

Non-env secret surfaces (TLS cert/key, OIDC JWKS) read from
files. These are SIGHUP-refreshed — replace the
file on disk, send SIGHUP, the harness re-reads atomically.

**No vendor SDKs inside the harness.** Any KMS / HashiCorp Vault
/ AWS Secrets Manager / Azure Key Vault / GCP Secret Manager
integration lives in the **orchestrator**, not in-process:

- **Kubernetes**: `Secret` → `envFrom:` / `valueFrom.secretKeyRef`.
  Pod update on `Secret` change is handled by the kubelet with
  projection automatic for `volumeMounts`; for `env:` you need
  a rollout, which is the standard pattern.
- **systemd**: `EnvironmentFile=/etc/agentd/secrets.env` +
  `ExecReload=/bin/kill -HUP $MAINPID`.
- **Vault**: Vault Agent sidecar writes a `.env` file or
  templated `EnvironmentFile` (`auto_auth` + `template`). Agent
  handles token renewal; the harness sees only env vars.
- **SOPS / age**: decrypt at deploy time to an `EnvironmentFile`
  or a k8s `Secret`.
- **AWS / GCP / Azure**: CSI drivers (Secrets Store CSI Driver,
  AWS `secrets-manager-secret` operator, etc.) project secrets
  as files or env.

**What's explicitly NOT supported**: a `[secrets]` TOML block.
If the operator adds one, TOML parse rejects it at startup
with an "unknown field `secrets`" error pointing at this doc.
That's deliberate — we don't want a situation where some
workflows pull secrets via the harness and others via the
orchestrator, because that creates two secret-rotation paths
with different semantics.

### 4.1 Authentication

Three mechanisms, each opt-in per route:

- **Static bearer** (`auth.bearer.<binding>.tokens_env`). Constant-time compare
  against `Authorization: Bearer <token>`.
- **HMAC-SHA256** (`auth.hmac.<binding>.secret_env`). Verifies
  `X-Signature: sha256=<hex>` (header name configurable) over the raw
  request body. Optional timestamp-skew check via
  `X-Timestamp: <unix-secs>` and `tolerance_secs`.
- **OIDC / JWT** (`auth.oidc.<binding>`). Validates a signed JWT
  from an identity provider against a pinned JWKS. Requires
  `auth-oidc` Cargo feature. Config:

  ```toml
  [auth.oidc.prod]
  issuer = "https://auth.example.com"
  audience = ["svc-api"]
  jwks_file = "/etc/agentd/jwks.json"   # OR jwks_json = "…inline JSON…"
  subject_allowlist = ["service-a@acme.com"]  # optional
  clock_skew_secs = 60                 # default
  algorithms = ["RS256"]               # default; supports RS256/384/512, ES256/384

  [[http_routes]]
  auth = "oidc:prod"
  ```

  Validates `iss`, `aud`, `exp`, optional `nbf`; rejects `none` and
  HS* algorithms unconditionally to block algorithm-confusion. Live
  JWKS fetch is not yet in-process (v2 follow-up) — operators rotate
  the file via cron / sidecar / config-mgmt. Audit events:
  `oidc.verified` (with subject + issuer) and `oidc.denied` (reason
  codes: `expired`, `bad-issuer`, `bad-audience`, `bad-signature`,
  `not-yet-valid`, `bad-algorithm`, `subject-not-allowed`, `malformed`,
  `unknown-kid`, `algorithm-not-allowed`).

  Principal injection: `{ kind: "oidc", name: "<sub>" }` (falls back
  to issuer when `sub` is absent).

Routes attach auth via `auth = "bearer:prod"` or
`auth = "hmac:webhooks"` in the `[[http_routes]]` block. An unknown
binding name fails the server at **spawn time** — not at first request.
This is intentional: misconfigurations should take down the serve loop
immediately, not silently accept unauth'd traffic.

After successful verification the engine receives the principal as
`trigger.principal = { kind: "bearer"|"hmac", name: "<binding>" }`.
Workflows can branch on this via `json_select`.

See `configuration.md` §Auth for full grammar. Verifier semantics are
covered in `capabilities.md` §Auth.

### 4.2 TLS (single-direction, termination in-process)

Requires `--features server-tls`. Minimal, operator-driven:

```toml
[server.tls]
cert_file = "/etc/agentd/tls/server.pem"
key_file  = "/etc/agentd/tls/server.key"
```

Supported cert formats: any PEM that `rustls-pemfile` understands —
PKCS1, PKCS8, SEC1 keys; RSA and ECDSA certs. Chain support is "whatever
PEM you concatenate". The cert file must contain at least one
`-----BEGIN CERTIFICATE-----` block.

Crypto provider: `aws-lc-rs` (installed once per process via
`OnceLock`). No runtime cipher-suite selection — we take rustls 0.23's
safe default (TLS 1.2 and 1.3, server cipher preference).

**Rotation:** there is no hot reload. Swap the PEM files on disk and
restart the process. SIGTERM drains cleanly (see §5), so a rolling
restart in k8s behind a Service with `terminationGracePeriodSeconds` set
above your `drain_timeout_secs` finishes without dropping in-flight
requests.

**Failure modes:**

- Missing / unreadable `cert_file` / `key_file` → fails at **spawn**
  with a clear `tls: open cert_file /x: No such file or directory`.
- PEM contains zero certs / no recognisable key → same spawn-time
  failure.
- TLS handshake error at runtime → the connection is closed with no
  HTTP-level response. No log noise beyond a warn event.

### 4.3 mTLS (client-cert verification)

```toml
[server.tls.client_auth]
mode    = "required"
ca_file = "/etc/agentd/tls/client-ca.pem"
```

Only `mode = "required"` is wired today. Clients without a valid cert
chained to `ca_file` get their TLS handshake rejected — no HTTP layer
is reached. Successful mTLS attaches a
`principal = { kind: "mtls", name: "sha256:<64-hex>" }` to the trigger
context, where the fingerprint is SHA-256 of the peer cert's DER bytes
(the leaf, not the CA). Workflows can pin by fingerprint via
`json_select` or `condition` nodes on `trigger.principal.name`.

`mode = "optional"` is reserved but intentionally rejected today — the
loader returns `tls.client_auth.mode: only 'required' is supported in
this build`. Add it when there's a real use case.

Peer identity extraction is **fingerprint-only** — we don't ship an
x509 parser. If you need CN / SAN, add `x509-parser` and extend
`accept_tls` in `http_tls.rs`; one to two screenfuls of code.

### 4.4 Cert management

We don't ship a cert-gen path in-tree — `rcgen` is a dev-dep for tests,
not a runtime API. Bring your own PKI. The file shapes we consume:

- Server cert: `-----BEGIN CERTIFICATE-----` block (optionally multiple,
  leaf first).
- Server key: PKCS1 / PKCS8 / SEC1 PEM.
- Client CA (mTLS): one or more `-----BEGIN CERTIFICATE-----` blocks
  that sign valid client certs.

For throwaway dev certs, the `openssl req` one-liner or `mkcert` both
produce output `agentd` accepts.

### 4.5 Rate limiting

Per-route token bucket — `capacity` tokens, refills at `refill_per_sec`:

```toml
[[http_routes]]
path = "/webhook/noisy"
# …
[http_routes.rate_limit]
capacity        = 20
refill_per_sec  = 5
```

Requests that exhaust the bucket get `429 Too Many Requests`. The bucket
is **in-process** and per-route, so horizontal scaling doesn't share
state. At ingress volumes where a fleet-wide limiter matters, put an
upstream rate-limit (nginx, cloud LB) in front and treat these as a
backstop. Numbers are validated at spawn: `capacity > 0`,
`refill_per_sec > 0`, both ≤ a sanity ceiling; bad numbers fail the
server start.

### 4.55 MCP servers (multi-server registry)

Workflows can compose multiple MCP stdio backends. Declare each
under `[[mcp_servers]]`:

```toml
[[mcp_servers]]
name = "github"
command = ["/usr/local/bin/mcp-github", "--repo", "agentd-dev/agentd"]
allow_tools = ["create_issue", "comment_on_*"]
allow_resources = ["issue://**"]

[[mcp_servers]]
name = "linear"
command = ["/usr/local/bin/mcp-linear"]
allow_tools = ["create_ticket"]
allow_resources = ["linear://projects/*"]
```

Nodes route to a server by name:

```toml
[[nodes]]
id = "file_issue"
type = "call_mcp_tool"
server = "github"               # names the target entry
tool   = "create_issue"
args_from = "build.payload"
```

**Resolution rules** (enforced by the validator + runtime):

| Node `server` field | Declared servers | Behaviour |
|---|---|---|
| `Some("name")` | `name` exists | Route to that server. |
| `Some("name")` | `name` missing | Validation error, runtime error. |
| `None` | exactly one | Route to it (back-compat). |
| `None` | zero or >1 | Validation error. |

**Legacy `--mcp-stdio CMD ARGS` flag** still works. It maps to an
implicit `{ name = "default", command = [...], allow_tools = ["*"],
allow_resources = ["*"] }` entry — the pre-registry "single server
with permissive allowlist" semantic preserved. Mixing
`--mcp-stdio` with a TOML entry named `default` is a conflict and
fails at startup.

**Per-server allowlists**. Each `[[mcp_servers]]` entry carries
its own `allow_tools` + `allow_resources`. Empty allowlists
deny-by-default per the fail-closed stance. The global
`[policy.mcp]` block is still applied to the legacy
`--mcp-stdio` "default" server only — new TOML-declared servers
source their allowlist from their own entry.

**Reload semantics** (SIGHUP or `--reload-file`):

- Per-server respawn: each declared server gets a new child
  process; fail-forward — if one server won't start, the rest of
  the reload continues. Audit: `reload.mcp_respawn{server,
  command}` on success, `reload.mcp_respawn_failed{server, ...}`
  on failure.
- Per-server allowlist swap: each server's allowlist is rebuilt
  from the new TOML entry. Audit: `reload.mcp_allowlist{server}`.
- **Adding or removing a whole entry** still requires a process
  restart — the handler registry is frozen at engine construction.
  Dropped entries log `reload.mcp_dropped_from_config{server}` and
  their previous child keeps running until restart.

### 4.6 Policy (the tool allowlist)

The manifest's `[policy]` section is a fail-closed allowlist:
`policy.fs.read`, `policy.fs.write`, `policy.env`, `policy.http.allow`,
`policy.shell.allow`, `policy.mcp`. Omitting `[policy]` entirely means
"allow everything" — **only appropriate for dev**. In production, declare
the block; matchers support `*`, `prefix/**`, `prefix/*`, and literal
paths.

A policy denial returns node status `denied`, logs an audit event, and
terminates that execution branch. The engine does not retry denied
nodes.

---

## 5. Lifecycle — startup, serving, shutdown

### 5.1 Startup sequence

1. Parse argv + `AGENTD_*` env vars (argv wins). Bad flag → exit `2`.
2. Resolve workflow (`--config` > embedded). Missing → exit `2`.
3. Merge `[logging]` block + env + CLI into `ResolvedLogging`; install
   tracing subscriber. Any install error → exit `5`.
4. Full validation pass. Fail → emit validation report JSON to stdout,
   exit `5`.
5. Build engine, register tools, register intelligence / MCP clients
   if flagged.
6. If `--validate-only`, print `{ok: true}` and exit `0`.
7. Infer mode → `once` or `serve`.
   - **once**: pick start node, run, print outcome, exit `0` / `5`.
   - **serve**: validate auth refs, build rate-limit buckets, load TLS
     certs, bind TCP listener, install signal handlers, enter accept
     loop.

Any pre-subscriber error goes to plain stderr. Post-subscriber errors
flow through `tracing` at the level / target you configured.

### 5.2 Serve-mode request lifecycle

```
TCP accept
  → (if TLS) rustls handshake, extract peer-cert fingerprint
  → parse HTTP/1.1 request (max 16 KiB headers, 1 MiB body)
  → route lookup (METHOD, PATH); miss → 404/405
  → auth verify (bearer / HMAC / mTLS identity)        ← route-specific
  → rate-limit bucket take                             ← route-specific
  → input_schema validate                              ← route-specific
  → engine.run(workflow, start_node, trigger_payload)
  → map ExecutionOutcome → HTTP status + JSON body
  → close connection
```

One OS thread per connection (no keep-alive). An `InFlightGuard`
increments / decrements the in-flight counter around the engine call so
graceful drain knows when it's safe to exit.

### 5.3 Shutdown + hot reload

Install: `crate::signals::install_shutdown_handlers()` sets up POSIX
`sigaction` for three signals. Handlers are signal-safe (only
flip `AtomicBool` flags). `SA_RESTART` is **not** set, so a blocked
`accept()` returns `EINTR` and the accept loop immediately observes the
flags.

| Signal | Effect |
|---|---|
| `SIGTERM` / `SIGINT` | Begin graceful drain (see below). |
| `SIGHUP` | **Hot reload** — see §5.4. Keeps serving; no drain. |

### 5.4 Hot reload (SIGHUP)

`kill -HUP $PID` re-reads `--config` and swaps the reloadable
subsystems atomically without dropping in-flight requests. The first reload pass
shipped TLS + auth; a follow-up extended the surface to cover everything
a live-config rotation could reasonably want to change.

**What reloads:**

- **TLS certificate / key** (`[server.tls].cert_file` / `key_file`)
  and **mTLS client CA** (`[server.tls.client_auth].ca_file`). Pulls
  fresh PEM off disk, rebuilds the `rustls::ServerConfig`, swaps. In-
  flight TLS sessions keep the old config via their per-accept
  snapshot; new handshakes use the new one.
- **OIDC JWKS** (`[auth.oidc.<name>].jwks_file` / `jwks_json`). Every
  binding's JWKS is re-parsed.
- **Static-bearer token sets** (`tokens_env` is re-read, so swapping
  the env var + HUP rotates tokens).
- **HMAC secret materialisation** (via `secret_env`).
- **Workflow policy** — `ManifestPolicy` is rebuilt from the new
  `[policy]` block (static matchers + optional Rego). Per-thread Rego
  engines recompile on first use after the swap via a new spec id.
- **MCP allowlist** — `[policy.mcp]` edits swap atomically alongside
  the policy block.
- **MCP stdio child** — a new process is spawned with the same
  `--mcp-stdio` command; on success the ArcSwap flips and the old
  child is killed once its last in-flight call drains. Spawn failure
  keeps the old child live and logs `reload.mcp_respawn_failed`.
- **Intelligence client** — bearer file (`--intel-http-bearer-file`)
  and `AGENTD_INTEL_HTTP_BEARER` env var are re-read so Vault-side-car
  rotations land without a restart.
- **Route table + rate-limit buckets** — `[[http_routes]]`
  additions / removals / renames plus per-route `rate_limit` changes
  all apply on the next connection. Token counters reset to full
  capacity on every swap (a policy rotation shouldn't let a flooding
  client retain their allowance).

**What still needs a restart:**

- `--bind` address, `--mcp-stdio` command / args, `--intel-unix` or
  `--intel-http` endpoint — all CLI-arg-shaped and captured at
  process start.
- Workflow node graph / edges. The engine's handler registry is
  frozen at startup; new or retyped nodes never get a handler.
- `[server.tls]` being present/absent on a server that bound without
  it (you can rotate the cert but can't graduate from plaintext to
  TLS in-place; see `reload_tls(None)` for the drop-TLS direction).

Changes to any of the above require a rolling restart.

**Failure modes.** Reload is **fail-forward**: if a single stage
fails (Rego compile error, MCP child won't start, bad new JWKS), the
old value for *that* stage stays live, an audit event records the
specific failure, and the rest of the reload continues. The process
does not exit. Stages emit `reload.tls` / `reload.auth` /
`reload.policy` / `reload.mcp_allowlist` / `reload.mcp_respawn` /
`reload.intel` / `reload.routes` on success, and
`reload.failed stage=<name>` / `reload.mcp_respawn_failed` on
failure. Top-level: `reload.started` and `reload.succeeded`.

**Embedded builds** (`AGENTD_EMBED_CONFIG=...`) have no on-disk
source to re-read — SIGHUP emits `reload.skipped` and is a no-op.

```bash
# Rotate TLS certs + OIDC JWKS + policy bundle without dropping traffic:
cp new-server.pem /etc/agentd/tls/server.pem
cp new-server.key /etc/agentd/tls/server.key
curl -s https://jwks-provider/jwks > /etc/agentd/jwks.json
vim /etc/agentd/workflow.toml         # edit [policy], [[http_routes]], etc.
systemctl kill -s HUP agentd.service
# or: kill -HUP $(pgrep agent)

# Rotate the intelligence bearer (operator writes the new token,
# no workflow.toml change):
vault read -field=token secret/intel > /etc/agentd/intel.bearer
systemctl kill -s HUP agentd.service
```

### 5.5 Graceful shutdown

Sequence on first signal:

1. Shutdown flag flips to `true`.
2. Accept loop exits; listener is dropped; no new connections.
3. Server waits up to `drain_timeout_secs` (default 30; override via
   `--drain-timeout-secs` or `AGENTD_DRAIN_TIMEOUT_SECS`) for the
   in-flight counter to reach zero.
4. Drain complete: log `drain complete`, exit `0`.
5. Drain timed out: log `drain timed out (forced exit)`, exit `5`.

`kill -9` / crash: nothing to clean up (stateless). A crashed process
loses only its in-flight requests. If you need at-least-once semantics,
put a durable queue upstream.

### 5.6 Exit codes

| Code | Constant | Meaning |
|---|---|---|
| `0` | `EXIT_OK` | Success. One-shot completed successfully; serve-mode drained cleanly. |
| `2` | `EXIT_USAGE` | Argv / env error, missing workflow, unknown flag, invalid bind address, serve mode without `[[http_routes]]`. |
| `5` | `EXIT_SEMANTIC` | Validation failure; engine error; tracing install failure; serve-mode drain timed out; one-shot returned a non-success outcome (failed / timeout / cancelled / denied). |

These match `runtime::EXIT_OK / EXIT_USAGE / EXIT_SEMANTIC` — if you
script around the binary, read the constants from there, not from this
table.

---

## 6. Logging & observability

### 6.1 Precedence

Resolved at startup in this order, last non-empty wins:

```
workflow [logging]  →  AGENTD_LOG* env vars  →  --log-* flags  →  default
```

Defaults: `level = "warn"`, `format = "text"`, `target = "stderr"`,
`enabled = true`. `--quiet` / `AGENTD_QUIET=1` forces `enabled = false`
regardless.

### 6.2 Targets

- `stderr` (default) — human-readable on a TTY, machine-parseable when
  format is `json`.
- `stdout` — useful when `stderr` is reserved for something else in the
  container.
- `file:/var/log/agent.log` — synchronous append-only write under a
  `Mutex`. Creates parent dirs on open. Re-open on restart is append,
  not truncate. Good for moderate rates; at high throughput, log to
  stderr and pipe into a real collector.

### 6.3 Formats

- `text` — `tracing-subscriber` fmt layer with `ansi=false` (no colour
  codes even on a TTY, so log-to-file stays clean).
- `json` — one JSON object per line. Shape matches
  `tracing-subscriber::fmt::format::Json`: `timestamp`, `level`,
  `target`, `span`, `fields: { … }`. Compatible with the ecosystem's
  standard OTLP filelog ingester.

### 6.4 Audit sink with redaction

When `[logging.audit]` is declared, audit events (target
`agentd::audit`) also flow to a dedicated JSONL sink with
field-level redaction — separate from the main log stream so
compliance retention / shipping can diverge. Built-in redaction
always masks: `token`, `secret`, `password`, `authorization`,
`api_key`, `bearer`, `jwt`, `cookie`, `session`, and `reason`
(the latter because auth-denial reasons frequently echo the bad
token prefix).

```toml
[logging.audit]
target = "file:/var/log/agent/audit.jsonl"  # default if omitted
redact_fields = ["custom_sensitive_field"]  # add to built-in list
include_reason = false                       # default; flip to pass reason through
```

Parent dirs are created on first open (`mkdir -p`). Redaction is
case-insensitive on field names. The emitted records match the
shape `tracing-subscriber::fmt::Json` uses (`timestamp`, `level`,
`target`, `fields`) so downstream collectors don't need a separate
parser.

### 6.45 Direct OTLP exporter

With the `otel` Cargo feature compiled in, declare `[otel]` to
push spans over OTLP gRPC to an OpenTelemetry collector (Tempo,
Jaeger, otelcol, Datadog agent, Honeycomb, etc.):

```toml
[otel]
endpoint = "http://otel-collector:4317"     # required
service_name = "agent"                       # default
protocol = "grpc"                            # only value today
sample_ratio = 1.0                           # 0.0..1.0, default 1.0
[otel.resource_attrs]
"deployment.environment" = "prod"
region = "eu-west-1"
```

Notes:

- A dedicated `agent-otel` tokio runtime (1 worker) drives the
  async OTLP batch exporter. The runtime lives for the process
  lifetime; on SIGTERM drain, pending spans flush before exit.
- Inbound `traceparent` (§6.5) becomes the parent `SpanContext`
  of the exported span so trace-continuity works end-to-end.
- Feature-off builds that declare `[otel]` fail at startup with a
  clear rebuild hint — never silently drop exports.
- The JSON-logs → collector-filelog-receiver path (§6.5) still
  works as a zero-dep fallback. Use `otel` for first-class push;
  use JSON-logs if you want to keep the binary small.

Dep footprint: enabling `otel` pulls ~50 crates (tokio, tonic,
hyper, opentelemetry_sdk, prost, etc.). Pick per deployment.

### 6.5 Trace-context propagation (W3C)

If an inbound HTTP request carries a `traceparent` header matching
the [W3C Trace Context spec](https://www.w3.org/TR/trace-context/),
`agentd` parses it and emits `trace_id`, `parent_id`, `trace_flags`,
`sampled` as structured fields on the request span. Every downstream
event (workflow.run, per-node spans, audit events) inherits them
under the JSON log format.

This is the recommended integration for OTLP-backed observability
stacks today: pipe `--log-format json` into your collector's
`filelog` receiver with the trace-id fields mapped to OTLP trace
attributes. A dedicated in-process OTLP exporter is tracked in
[`maturity.md` §2.10](maturity.md).

`traceparent` without a valid 32-hex trace-id, 16-hex parent-id, or
with the all-zero sentinel is silently ignored (logs proceed without
the fields) rather than rejected — matches the spec's "pass through
unknown versions" requirement.

### 6.6 Events you should know about

- **`agentd::audit` target** — auth decisions, policy denials,
  principal-attached-trigger events. Pipe these to a separate sink
  (retention / compliance).
- **Spans**: one per workflow execution, one per node. Fields:
  `execution_id`, `workflow`, `start_node`, `node_id`, `kind`,
  `outcome`, `latency_ms`, `reason` (on failure). Same shape the RFC
  §20 specified.
- **Metrics**: in-process counters (workflow starts / completions /
  failures / timeouts / errors, node executions + failures, policy
  denials, llm calls + tokens) exported on `GET /metrics` in serve mode
  as Prometheus text — see §3.5 for the full counter list. A one-shot
  run's cost is also captured in its [run record](#36-run-records--inspection).

---

## 7. Configuration precedence (quick reference)

| Knob | Sources (highest wins) |
|---|---|
| Workflow source | `--config` → `AGENTD_CONFIG` → embedded → error |
| Mode | `--mode` → `AGENTD_MODE` → inferred from `[[http_routes]]` |
| Bind address | `--bind` → `AGENTD_HTTP_BIND` → `127.0.0.1:8080` |
| Run timeout | `--timeout-secs` → `AGENTD_TIMEOUT_SECS` → `120` |
| Drain timeout | `--drain-timeout-secs` → `AGENTD_DRAIN_TIMEOUT_SECS` → `30` |
| Log level | `--log-level` → `AGENTD_LOG` → workflow `[logging].level` → `warn` |
| Log format | `--log-format` → `AGENTD_LOG_FORMAT` → workflow `[logging].format` → `text` |
| Log target | `--log-target` → `AGENTD_LOG_TARGET` → workflow `[logging].target` → `stderr` |
| Logging enabled | `--quiet`/`AGENTD_QUIET=1` → workflow `[logging].enabled` → `true` |
| Start node (one-shot) | `--start` → `AGENTD_START` → sole manual start → sole start overall → error |
| Input (one-shot) | `--input` → `AGENTD_INPUT` → `Value::Null` |

Full flag list: `agentd --help`. Full env-var list: `configuration.md`.

---

## 8. Canonical deployments

### 8.1 Local one-shot

```bash
agentd --config wf.toml --input payload.json
```

No server. Reads `payload.json` as the trigger, runs, prints outcome
JSON, exits.

### 8.2 Plain HTTP behind a reverse proxy

```bash
agentd --config wf.toml --bind 127.0.0.1:8080 \
      --log-level info --log-format json
```

Put nginx / Caddy / a cloud LB in front for TLS. Inside the workflow,
use `auth = "bearer:prod"` for simple token auth; rate-limit per-route
as needed.

### 8.3 Publicly reachable hardened webhook (Shape B)

```bash
agentd --config wf.toml --bind 0.0.0.0:8443 \
      --log-level info --log-format json --log-target stderr \
      --drain-timeout-secs 60
```

Workflow:

```toml
[server.tls]
cert_file = "/etc/agentd/tls/server.pem"
key_file  = "/etc/agentd/tls/server.key"

[server.tls.client_auth]
mode    = "required"
ca_file = "/etc/agentd/tls/client-ca.pem"

[auth.hmac.webhooks]
secret          = "${WEBHOOK_SECRET}"
header          = "X-Hub-Signature-256"
prefix          = "sha256="
timestamp_header = "X-Timestamp"
tolerance_secs  = 300

[[http_routes]]
method     = "POST"
path       = "/webhook/github"
start_node = "on_push"
auth       = "hmac:webhooks"
[http_routes.rate_limit]
capacity       = 60
refill_per_sec = 10
```

At rest: TLS terminates in-process; mTLS restricts the client surface to
certs signed by the CA; HMAC verifies webhook payloads; rate-limit
throttles storms.

### 8.4 Container image (GHCR)

Pre-built multi-arch images (`linux/amd64` + `linux/arm64`) publish
to `ghcr.io/agentd-dev/agentd`:

| Tag | Meaning |
|---|---|
| `latest` | Latest `agent-v*` tagged release. |
| `1.0.0`, `1.0`, `1` | Specific semver (matches tag `agent-v1.0.0`). |
| `edge` | Latest main-branch build. Unsigned; use for canary only. |
| `sha-<7>` | Specific commit. Unsigned. |

Released tags are cosign-signed (keyless OIDC from GitHub Actions)
and carry an SPDX SBOM attestation. Verify:

```bash
cosign verify ghcr.io/agentd-dev/agentd:1.0.0 \
  --certificate-identity-regexp 'https://github.com/agentd-dev/source-code/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

cosign verify-attestation --type spdxjson ghcr.io/agentd-dev/agentd:1.0.0 \
  --certificate-identity-regexp 'https://github.com/agentd-dev/source-code/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Image base: `gcr.io/distroless/cc-debian12:nonroot` (uid:gid
`65532:65532`, no shell). Release binary built with
`--all-features` — narrower surfaces require a purpose-built image.

### 8.5 Kubernetes pod

Rough shape:

```yaml
spec:
  terminationGracePeriodSeconds: 45           # > drain_timeout_secs
  containers:
    - name: agent
      image: registry/agent:1.0.0
      args:
        - --config=/etc/agentd/wf.toml
        - --bind=0.0.0.0:8080
        - --drain-timeout-secs=30
      ports:
        - containerPort: 8080
      readinessProbe:
        httpGet: { path: /healthz, port: 8080 }
      livenessProbe:
        httpGet: { path: /healthz, port: 8080 }
      volumeMounts:
        - mountPath: /etc/agentd
          name: workflow
      env:
        - name: AGENTD_LOG
          value: info
        - name: AGENTD_LOG_FORMAT
          value: json
```

`/healthz` is wired as an always-live endpoint (no auth, not rate
limited). `terminationGracePeriodSeconds` must exceed
`drain_timeout_secs` or k8s will SIGKILL mid-drain.

### 8.6 Debian / RPM packages + systemd

Pre-built `.deb` and `.rpm` attach to each `agent-v*` release on
GitHub. Both drop a hardened systemd unit with `DynamicUser`,
`ProtectSystem=strict`, empty `CapabilityBoundingSet`,
`MemoryDenyWriteExecute`, and a restrictive `SystemCallFilter`.

```bash
# Debian / Ubuntu
sudo apt install ./agent_0.1.0_amd64.deb

# RHEL / Fedora / Rocky
sudo dnf install ./agent-0.1.0-1.x86_64.rpm

sudo cp my-workflow.toml /etc/agentd/workflow.toml
sudo systemctl enable --now agent
```

Config knobs live in `/etc/default/agent` as `AGENTD_ARGS=...`. See
[`packaging/README.md`](../../packaging/README.md) for full unit
details, drop-in overrides, and the locked-down filesystem /
syscall / network posture.

Build a package locally:

```bash
cargo install cargo-deb cargo-generate-rpm
cargo build --release --manifest-path crates/agentd/Cargo.toml --all-features
cargo deb --manifest-path crates/agentd/Cargo.toml --no-build   # → target/debian/
cargo generate-rpm -p crates/agentd                              # → target/generate-rpm/
```

---

## 9. Runbook basics

### 9.1 Startup fails — where to look

| Symptom | Likely cause | Fix |
|---|---|---|
| `agent: no workflow configured` | No `--config`, no embedded | Pass `--config` or rebuild with `AGENTD_EMBED_CONFIG`. |
| `failed to parse <path>: …` | TOML syntax or unknown field (all structs are `deny_unknown_fields`) | Read the error; fix the file. |
| `workflow `X`: duplicate node id 'Y'` | Validator caught an authoring mistake | Rename / remove the offender. |
| `workflow `X`: cycle at Z` | Validator caught a cycle | DAG only — break the cycle. |
| `http_route #N points at unknown start_node 'Y'` | Route refers to a start that isn't declared | Declare it in `[[start_nodes]]`. |
| `auth: binding 'prod' referenced by routes not declared in [auth.bearer]` | Typo in route `auth =` | Match the binding name. |
| `tls: open cert_file /x: …` | Wrong path / permissions | Check `ls -l`. |
| `tls: <path> contains no certificates` | Empty or malformed PEM | `openssl x509 -in … -noout -text` sanity check. |
| `tls: <path> has no recognised private key` | Key is corrupt / wrong format | Regenerate as PKCS8. |
| `bind 127.0.0.1:8080: Address already in use` | Another process on the port | `ss -ltnp | grep 8080`, kill or rebind. |
| `serve mode requires at least one [[http_routes]]` | `--mode serve` forced on a workflow with no routes | Drop the override or add a route. |

### 9.2 Mid-flight issues

- **Hangs on startup**: check the `[logging].target` — a `file:/path` on
  a full disk or read-only mount will fail the subscriber install.
  Pre-subscriber errors go to **plain stderr** so you always see them.
- **`429` for legitimate traffic**: rate-limit `capacity` / `refill` is
  per-route, per-process. Relax numbers or put a fleet-wide limiter
  upstream.
- **`401` with the right token**: bearer compare is constant-time over
  raw bytes. Trailing whitespace or wrong prefix (should be `Bearer `)
  is the usual cause.
- **`401` on HMAC webhooks**: verify header name, prefix (`sha256=` vs
  raw hex), and whether the sender hashes the canonical raw body.
  Turn on `--log-level debug` for the `agentd::audit` events — the
  reason is logged.
- **Node denied unexpectedly**: read the audit log for the denied
  matcher. A missing `/**` vs `/*` is the usual cause.

### 9.3 Graceful restart / rollout

The drain sequence is the only supported shutdown path. A rolling
restart looks like:

1. Send SIGTERM to old pod / process.
2. Wait for `drain complete` log line (bounded by `drain_timeout_secs`).
3. Start the new pod / process.
4. Readiness probe flips → receive traffic.

Set `terminationGracePeriodSeconds` (k8s) / `TimeoutStopSec` (systemd)
higher than `drain_timeout_secs`, or SIGKILL will interrupt drain.

---

## 10. What this doc does NOT cover

- **Writing workflows** — see `capabilities.md`.
- **Every TOML field** — see `configuration.md`.
- **Internal module boundaries** — see `architecture.md`.
- **Known limitations / open items** — see `maturity.md`.
- **RFC narrative / design rationale** — see
  [`rfcs/0001-bounded-workflow-runtime.md`](../../rfcs/0001-bounded-workflow-runtime.md).
