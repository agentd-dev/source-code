# `agentd` — capabilities reference

Every node kind, every tool, every trigger, every policy knob.
Workflow authors read this to know what they can put in a TOML;
operators read it to know what the binary will actually do.

Pairs with [`configuration.md`](configuration.md) (complete TOML
reference) and [`architecture.md`](architecture.md) (how the pieces
fit).

---

## 1. Node catalog

Every node carries an `id` (unique within the workflow) and a typed
`type = "..."` discriminator. Variants group into five categories.

### 1.1 Input / context

Pull data into `ExecutionContext.node_outputs` so downstream nodes
can reach it via dotted paths.

#### `read_file`

Reads a UTF-8 file from disk.

```toml
[[nodes]]
id = "load"
type = "read_file"
path_from = "trigger.path"      # dotted path to a string in the context
```

Produces:
```json
{ "path": "/workspace/x.txt", "content": "...", "bytes": 1234 }
```

- Feature: `tools-fs`
- Policy: `Policy::check_fs_read(canonicalized_path)`
- Dry-run: returns `{ "path": "...", "dry_run": true }` without touching disk.

#### `read_env`

Reads an environment variable.

```toml
[[nodes]]
id = "token"
type = "read_env"
key = "GITHUB_TOKEN"            # literal, not a context path
```

Produces:
```json
{ "key": "GITHUB_TOKEN", "value": "..." }
// when unset:
{ "key": "GITHUB_TOKEN", "value": null, "missing": true }
```

- Feature: `tools-env`
- Policy: `Policy::check_env_read(key)`

#### `read_mcp_resource`

Reads an MCP resource by URI.

```toml
[[nodes]]
id = "page"
type = "read_mcp_resource"
resource_from = "trigger.resource_uri"
```

Produces:
```json
{
  "uri": "docs://pages/42",
  "contents": [ { "uri": "docs://pages/42", "text": "..." }, … ]
}
```

- Requires: `--mcp-stdio` / `AGENTD_MCP_STDIO` at runtime.
- Policy: `McpAllowlist::resource_allowed(uri)` before the client dials.
- Dry-run: `{ "uri": "...", "dry_run": true }`.

#### `parse_json`

Parses a context string as JSON.

```toml
[[nodes]]
id = "body"
type = "parse_json"
input_from = "read_body.content"
```

Produces:
```json
{ "parsed": <any JSON> }
```

- Feature: `tools-data`
- Pure — no side effects, no policy gate.

### 1.2 Transformation

Pure compute. No side effects, no policy.

#### `template_render`

`{{key}}` substitution. Unknown keys render the literal `{{key}}`
marker so authors notice the miss instead of a silent empty string.

```toml
[[nodes]]
id = "greet"
type = "template_render"
template = "Hi {{user.name}}, you are {{user.age}}."
input_from = "trigger"            # optional — default Null
```

Produces:
```json
{ "rendered": "Hi Ada, you are 36." }
```

- Feature: `tools-data`

#### `json_select`

Dotted-path walk into a JSON value. Separate from context
resolution because the input itself is a sub-object.

```toml
[[nodes]]
id = "username"
type = "json_select"
input_from = "body.parsed"
path = "user.name"
```

Produces:
```json
{ "value": "Ada", "found": true }
// or:
{ "value": null, "found": false }
```

- Feature: `tools-data`

#### `diff_compute`

Structural JSON diff between two context values.

```toml
[[nodes]]
id = "d"
type = "diff_compute"
left_from  = "fetch_old.parsed"
right_from = "fetch_new.parsed"
```

Output:

```json
{
  "added":    { "path.to.field": <new value>, … },
  "removed":  { "path.to.field": <old value>, … },
  "changed":  { "path.to.field": { "from": …, "to": … }, … },
  "unchanged": true | false
}
```

Paths use dot notation for objects (`config.timeout`) and bracket
notation for arrays (`items[2].name`). Arrays diff by index —
content-addressable diffs require pre-transforming into keyed
objects (e.g. via `json_select`). Leaf equality uses `Value ==
Value`; `1` ≠ `"1"` ≠ `true`. Workflow authors typically pair
this with a `condition` node on `diff.unchanged` to skip
downstream side-effects when nothing changed.

- Feature: `tools-data`

### 1.21 Policy-as-code (Rego)

On top of the static allowlist, workflows can declare a Rego
policy module that runs as an additional AND condition on every
tool decision. Feature: `policy-rego` (pulls `regorus`, pure-Rust
OPA-compatible evaluator).

```toml
[policy]
fs   = { read = ["/data/**"] }    # static allowlist still applies
http = { urls = ["https://*.internal/**"] }

[policy.rego]
file = "/etc/agentd/policy.rego"   # OR inline = "..."
# Extra data merged at the root of `data`; access as `data.<key>`.
data = { region = "eu-west-1", tenant = "acme" }
# Default query is `data.agent.allow`; operators rarely override.
# query = "data.agent.allow"
```

Rego policy contract:

```rego
package agent

default allow = false

# Input shape:
#   { tool: "fs.read" | "fs.write" | "fs.delete" | "fs.list"
#         | "env.read" | "http.request" | "shell.run",
#     args: { /* tool-specific */ } }

allow if {
    input.tool == "fs.read"
    startswith(input.args.path, "/data/safe/")
}

allow if {
    input.tool == "http.request"
    input.args.method == "POST"
    startswith(input.args.url, "https://api.internal/")
}
```

Semantics:

- **AND with static allowlist.** If static says deny, Rego never
  runs. If static allows, Rego must return `true` for the check
  to pass overall.
- **Compile at startup.** Bad Rego (syntax error, missing
  `package agent`, etc.) fails `agentd` at spawn, not at first
  request. No silent degradation.
- **Thread-local engines.** `regorus::Engine` is `!Send` (uses
  `Rc`); each thread lazily compiles its own engine from the
  shared spec on first check, reuses thereafter.
- **Parameterisable via `data`.** Workflows import a shared
  `.rego` module; per-deploy differences go in the `data` block
  so one policy file fits many agents.

### 1.25 Scheduled + event triggers (beyond HTTP)

Two more trigger shapes land workflows without touching HTTP:

```toml
# Fire every 5 minutes (local TZ).
[[triggers]]
type = "cron"
schedule = "0 */5 * * * *"     # 5-field cron: m h dom mon dow
start_node = "poll"

# Or a simpler interval (no TZ concerns).
[[triggers]]
type = "interval"
every = "30s"                   # s / m / h / d
start_node = "heartbeat"

# Fire on filesystem events.
[[triggers]]
type = "fs_watch"
path = "/var/incoming"
recursive = true
events = ["create", "modify"]   # empty = all 4 (create/modify/remove/rename)
debounce_ms = 500               # coalesce rapid bursts
start_node = "on_file"
```

- Features: `trigger-cron`, `trigger-fs-watch`.
- A workflow with any long-lived trigger auto-infers **serve mode**
  (no `[[http_routes]]` needed).
- Per-trigger fires are **serial** — an in-flight run holds the
  schedule; overlapping ticks drop rather than queue.
- Trigger payloads: cron/interval carry `kind`, `schedule`/`every_ms`,
  `fired_at_unix_ms`, `tick`. fs_watch carries `kind`, `path`, `event`,
  `fired_at_unix_ms`, `tick`.
- Audit events: `cron.fire`, `cron.completed`, `cron.error`,
  `fs_watch.started`, `fs_watch.fire`, `fs_watch.completed`,
  `fs_watch.error`.

### 1.3 Intelligence

#### `llm_infer`

One bounded reasoning call. Prompt template is rendered from the
optional `input_from` context value via the same `{{key}}` engine
as `template_render`. Dispatched through the registered
`IntelligenceClient` (Unix socket default; mock in tests).

```toml
[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"              # currently the only named backend
prompt = "Classify sentiment of: {{text}}"
input_from = "trigger"           # optional; default Null
output_schema = "schemas/out.json"  # optional; current behaviour = "must be JSON"
```

Produces:
```json
{
  "content": "positive",
  "parsed": null,                // or the parsed JSON when output_schema is set
  "usage": { "prompt_tokens": 12, "completion_tokens": 1 }
}
```

- Requires one of:
  - `--intel-unix PATH` / `AGENTD_INTEL_UNIX` — Unix socket provider
    speaking length-framed JSON-RPC 2.0 (`intel-unix` feature,
    always on; works with any
    length-framed JSON-RPC server speaking the same shape).
  - `--intel-http URL` / `AGENTD_INTEL_HTTP` — plain-HTTP provider at
    `http://host:port/path` (`intel-http` Cargo feature). Optional
    bearer auth via `--intel-http-bearer-file PATH` or
    `AGENTD_INTEL_HTTP_BEARER`. v1 is plain-HTTP only; for HTTPS
    upstreams terminate TLS at a sidecar and point at the localhost
    port. Same JSON-RPC 2.0 envelope as the Unix transport so one
    intel-server can front both.
- `output_schema` today only enforces "must be valid JSON" — full
  JSON-Schema validation is a future extension (see maturity).
- Dry-run: returns `{ "content": "<dry-run>", "dry_run": true }`
  without calling the backend.
- Unknown `backend` → `Error::Intelligence("backend ... is not
  configured")`. Multi-backend support is future work.

### 1.4 Action (side-effectful)

Every action goes through the policy layer and honours dry-run.

#### `write_file`

Writes a UTF-8 string (or serialises any other JSON value) to a
path. Parents created with `mkdir -p`.

```toml
[[nodes]]
id = "emit"
type = "write_file"
path_from = "trigger.output_path"
content_from = "classify.content"
```

- Feature: `tools-fs`
- Policy: `Policy::check_fs_write(path)`

#### `create_dir`

Idempotent `mkdir -p`.

```toml
[[nodes]]
id = "outdir"
type = "create_dir"
path_from = "trigger.dir"
```

- Feature: `tools-fs`
- Policy: `Policy::check_fs_write(path)`

#### `http_request`

Outbound HTTP/1.1 request. Plain HTTP only in the current build;
HTTPS is a future `tools-http-tls` feature (see maturity).

```toml
[[nodes]]
id = "post"
type = "http_request"
method = "POST"                  # literal
url_from = "trigger.webhook_url"
body_from = "classify"           # optional; JSON-serialised when non-string
```

Produces:
```json
{
  "status": 200,
  "headers": { "content-type": "application/json", … },
  "body": "...",
  "bytes": 123
}
```

- Feature: `tools-http`
- Policy: `Policy::check_http_request(method, url)`
- 1 MiB caps on request and response bodies.
- Non-2xx status sets `branch = "error"` — wire a `when = "error"`
  edge to route failures cleanly.
- HTTPS URL → `Error::Tool` with a clear "rebuild with
  tools-http-tls" hint.

#### `call_mcp_tool`

Invokes `tools/call` on the registered MCP server.

```toml
[[nodes]]
id = "post_comment"
type = "call_mcp_tool"
tool = "comment_on_page"
args_from = "classify.comment_payload"   # optional
```

Produces:
```json
{
  "tool": "comment_on_page",
  "content": [...],                 // MCP content blocks
  "is_error": false,
  "structured": null                // structured_content if returned
}
```

- Feature: always compiled (part of the MCP module).
- Requires: `--mcp-stdio` to have spawned a server.
- Policy: `McpAllowlist::tool_allowed(tool_name)`.
- `is_error: true` → `branch = "error"`.

#### `shell_run`

Spawn a local binary with argv-style args. **No shell
interpolation, no PATH lookup.**

```toml
[[nodes]]
id = "run"
type = "shell_run"
command = "/usr/bin/git"         # literal, absolute path only
args_from = "trigger.git_args"   # optional; resolves to a JSON array of strings
timeout_secs = 60                # optional; default 30
```

Produces:
```json
{
  "command": "/usr/bin/git",
  "args": ["log", "-1"],
  "exit_code": 0,
  "signal": null,
  "stdout": "...",
  "stderr": "",
  "truncated": false,
  "timed_out": false,
  "duration_ms": 42
}
```

- Feature: `tools-shell`
- Policy: `Policy::check_shell_run(canonical_path)`. Command
  `canonicalize`d before the check — symlink escape is caught.
- Non-zero exit or signal → `branch = "error"`.
- Env is cleared; only `PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`
  and `LANG=C.UTF-8` are set. No operator env leakage.
- Stdout and stderr capped at 64 KiB each; overflow sets
  `truncated: true`.
- Timeout kills the child with SIGKILL.

### 1.5 Control

Drive the traversal. No side effects. Always compiled.

#### `terminate`

Ends the run successfully.

```toml
[[nodes]]
id = "done"
type = "terminate"
```

→ `ExecutionOutcome::Completed { final_value: null, last_node: "done" }`.

#### `fail`

Ends the run with a declared reason.

```toml
[[nodes]]
id = "reject"
type = "fail"
reason = "input failed schema check"    # optional; default "workflow failed"
```

→ `ExecutionOutcome::Failed { reason, last_node: "reject" }`.
Exit code 5 in one-shot mode; HTTP 422 in serve mode.

#### `merge`

Pass-through. Multiple edges fan into a merge; one edge fans out.

```toml
[[nodes]]
id = "join"
type = "merge"
```

#### `condition`

Boolean branch via JSON truthiness.

```toml
[[nodes]]
id = "gate"
type = "condition"
expr = "trigger.flag"
```

Resolves `expr` as a dotted path in the context. Routes to
`when = "true"` or `when = "false"`.

Truthiness rules:
- `null`, `false`, `""`, `0`, `[]`, `{}` → false
- everything else → true

#### `switch`

Multi-way branch on a JSON value's string form.

```toml
[[nodes]]
id = "route"
type = "switch"
expr = "analyze.decision"

[[edges]]
from = "route"
to   = "post_comment"
when = "comment"

[[edges]]
from = "route"
to   = "done"
when = "ignore"
```

String values match verbatim; bool / number match their JSON text
(`"true"`, `"42"`); arrays / objects fall through to `"array"` /
`"object"` so mismatches against declared `when` labels fail loudly.

---

## 2. Edges

```toml
[[edges]]
from = "node_id"
to   = "other_node_id"
when = "label"        # optional; matches against the source's branch outcome
```

Rules enforced by the engine:

- Every `when`-absent edge fires only when the source node's
  handler emits `branch = None`.
- Every `when = "LABEL"` edge fires only when the source emits
  `branch = Some("LABEL")`.
- A node with **multiple unconditional out-edges** is a workflow
  error (`Error::Workflow { … matching out-edges }`).
- A node with **zero matching out-edges** is a dead-end;
  traversal ends with the current node's output as the final value
  (`ExecutionOutcome::Completed`).

The validator catches dangling `from` / `to`, cycles (Kahn's),
and unreachable nodes. It does NOT verify `when` labels match the
source kind's outcome grammar — that's a future extension.

---

## 3. Start nodes + triggers

### 3.1 `[[start_nodes]]`

```toml
[[start_nodes]]
name = "on_http"
source = "http"             # event | http | manual
entry_node = "analyze"      # optional; falls back to the single root node
```

`entry_node` points at a declared `nodes[].id`. If omitted, the
engine picks the unique in-degree-0 node; multiple roots without an
`entry_node` is a workflow error.

### 3.2 `[[triggers]]`

```toml
[[triggers]]
type = "mcp.resource.updated"   # or mcp.resource.created / internal.event
server = "docs"
resource = "docs://pages/*"
start_node = "on_resource_update"
```

Typed trigger declarations. The **listener side** for event-based
triggers is not wired in the current build — the harness accepts
these declarations (cross-referenced at validation time) but does
not subscribe to MCP notifications. Today's live triggers:

- **Manual** — one-shot CLI / env-driven invocation
- **HTTP** — `[[http_routes]]` → HTTP listener

### 3.3 `[[http_routes]]`

```toml
[[http_routes]]
method = "POST"                              # required
path   = "/webhooks/github"                  # required; routed on exact path
start_node = "on_push"                       # required; must exist in start_nodes
input_schema = "schemas/gh-push.json"        # optional; not enforced today (future)
auth = "hmac:github"                         # optional; none | bearer:name | hmac:name | mtls
[http_routes.rate_limit]                     # optional
capacity   = 10
per_second = 1.0
```

Per-route auth and rate-limit settings are validated at server
startup — misconfigured bindings fail the bind, not the first
request.

---

## 4. Policy

The `[policy]` block narrows what the **compiled-in** tools can do.
Every section defaults to **empty** — deny-by-default, fail-closed.

```toml
[policy.fs]
read   = ["/workspace/docs/**"]
write  = ["/tmp/agent-out/**"]
delete = []
list   = []                          # falls back to `read` when empty

[policy.env]
read_keys = ["DOCS_ROOT", "AGENTD_*"]

[policy.http]
urls    = ["http://api.internal.example/*"]
methods = ["GET", "POST"]            # optional; empty = any

[policy.shell]
commands = ["/usr/bin/git", "/usr/local/bin/mytool"]

[policy.mcp]
servers   = ["docs"]                 # informational for now
tools     = ["comment_on_page"]
resources = ["docs://pages/*"]
```

### Matcher semantics

Three patterns, deliberately narrow:

| Pattern | Matches |
|---|---|
| `"*"` | anything |
| `"prefix/**"` or `"prefix/*"` | `prefix` itself and anything under `prefix/…` |
| literal | exact equality |

No regex. No glob extensions beyond the above. An operator who
reads the manifest knows **exactly** what's reachable.

### Denial behaviour

On deny, the handler returns `Error::Policy("<tool> denied on <target>: <reason>")`:

1. Engine metrics increment `policy_denials`.
2. Tracing event `policy.denied` fires on the `agentd::audit` target.
3. The error propagates up — the workflow ends with
   `ExecutionOutcome::Failed` (or bubbles to HTTP 500 in serve mode).
4. No retry, even with `on = "any"` — `Error::Policy` is not
   transient by design.

### Absent `[policy]` block

If the workflow doesn't declare `[policy]`, the harness uses
`AllowAll`: every fs / env / http / shell / MCP check returns
`Decision::Allow`. This keeps the MVP path frictionless. Production
configs should always declare the block.

---

## 5. Auth (HTTP routes)

### 5.1 `[auth]` bindings

```toml
[auth.bearer.ops]
tokens_env = "OPS_TOKENS"            # newline-separated tokens in env
# tokens = ["literal"]                # tests only; discouraged

[auth.hmac.github]
secret_env = "GITHUB_WEBHOOK_SECRET"
header = "X-Hub-Signature-256"       # optional; default "X-Agent-Signature"
prefix = "sha256="                   # optional; default "sha256="
```

### 5.2 Route ref grammar

```toml
auth = "none"           # or omit entirely
auth = "bearer"         # → bearer:default
auth = "bearer:ops"
auth = "hmac"           # → hmac:default
auth = "hmac:github"
auth = "mtls"           # requires [server.tls.client_auth] mode = "required"
```

### 5.3 Verifier semantics

| Kind | What passes |
|---|---|
| Bearer | `Authorization: Bearer <token>` matches a token in the configured set (constant-time compare) |
| HMAC | `HMAC-SHA256(secret, body)` in hex equals the declared header's value after stripping the configured prefix (constant-time compare) |
| mTLS | A client certificate was presented and accepted by the TLS layer's `WebPkiClientVerifier`; principal name = `sha256:<64-hex>` of the DER bytes |

Denials emit `http.auth_denied` on the `agentd::audit` target and
return HTTP 401 with a `{"error": "unauthorized", "detail": "..."}`
body.

### 5.4 Principal injection

On successful auth, the runtime inserts into the trigger payload:

```json
"principal": { "kind": "bearer" | "hmac" | "mtls" | "anonymous", "name": "<binding name or fingerprint>" }
```

Workflow `condition` / `switch` nodes can route on
`trigger.principal.kind` or `.name` — e.g. different downstream
logic for `bearer:ops` vs `hmac:github`.

---

## 6. TLS + mTLS

### 6.1 `[server.tls]`

```toml
[server.tls]
cert_file = "/etc/ssl/server.pem"    # server cert chain (PEM, leaf first)
key_file  = "/etc/ssl/server.key"    # private key (PKCS8 / RSA / EC)

[server.tls.client_auth]             # omit for HTTPS-only
mode    = "required"                  # only `required` wired today
ca_file = "/etc/ssl/client-ca.pem"   # trust root for client certs
```

- Feature: `server-tls` (implies `auth`).
- Adds ~2 MB to the binary (rustls + aws-lc-rs + rustls-pemfile).
- Handshake failures are audited (`tls.handshake_failed`) and the
  connection is dropped — no HTTP-level reply is possible.
- `mode = "optional"` is parsed but rejected at build time ("only
  `required` is supported in this build"). Future work.

### 6.2 How mTLS composes with workflow policy

TLS-layer client-cert verification is the first line: rustls rejects
any unsigned / expired / CA-mismatched client cert before the HTTP
parser runs.

The workflow can then **further** pin acceptable clients by their
cert fingerprint:

```toml
[[nodes]]
id = "audit"
type = "condition"
expr = "trigger.principal.name"

[[edges]]
from = "audit" to = "allow"
when = "sha256:abc123...…"

[[edges]]
from = "audit" to = "deny"
# (any other fingerprint hits the ambiguous-out-edge error)
```

Fingerprints are SHA-256 of the peer cert's DER bytes. Operators
pre-compute these at deployment time.

---

## 7. Rate limiting

```toml
[[http_routes.rate_limit]]
capacity   = 10      # burst size
per_second = 1.0     # sustained refill rate (tokens / second, float)
```

Implementation: one `TokenBucket<SystemClock>` per `(method, path)`,
atomic `try_take()`. `capacity = 0` or `per_second <= 0` fails the
bind at startup.

Denied requests return `429 Too Many Requests` with a
`Retry-After: <seconds>` header and a body like:

```json
{ "error": "rate limited", "retry_after_ms": 1234 }
```

A `http.rate_limited` tracing event fires on `agentd::audit`.

The rate-limit check runs **before** auth — a flood of bad tokens
gets 429'd without burning HMAC cycles.

---

## 8. Per-node retry + backoff

```toml
[[nodes]]
id = "post"
type = "http_request"
method = "POST"
url_from = "trigger.url"
body_from = "analyze"

[nodes.retry]
max_attempts = 3              # total; must be ≥ 1
backoff_ms   = 500            # linear: attempt N waits N × backoff_ms
on = "transient"              # any | transient
```

### Retryable classes (`on = "transient"`)

- `Error::Tool`
- `Error::Intelligence`
- `Error::Mcp`

### Non-retryable (never retried regardless of `on`)

- `Error::Policy` — policy denial is deliberate, retry won't change it.
- `Error::Schema` — malformed LLM output doesn't fix itself.
- `Error::Timeout` — the engine deadline already fired.
- `Error::Config` / `Error::Workflow` / `Error::CapabilityUnavailable` —
  structural issues.

Deadline-aware: if the backoff would push past `ctx.deadline`, the
retry loop surfaces `Error::Timeout` instead of sleeping.

Every retry attempt emits a `node.retry` tracing event on the
`agentd::audit` target.

---

## 9. Triggers + mode inference

Mode auto-selects from workflow content:

| Workflow has | Default mode | Override |
|---|---|---|
| `[[http_routes]]` | `serve` | `--mode once` |
| No HTTP routes | `once` | `--mode serve` (errors without routes) |

One-shot mode:

```bash
agentd --config wf.toml --start main --input payload.json
```

- Reads `--input` as JSON, wraps as `TriggerMeta::manual(payload)`.
- Runs once; prints the outcome JSON to stdout; exits 0 (Completed)
  / 5 (Failed / TimedOut).

Serve mode:

```bash
agentd --config wf.toml --bind 127.0.0.1:8080
```

- Binds TCP (+ optional TLS); serves `[[http_routes]]`.
- Built-in `GET /healthz` — always live, returns `{"status":"ok","workflow":"..."}`.
- Shutdown: `SIGTERM` / `SIGINT` → stop accepting, wait up to
  `--drain-timeout-secs` (default 30) for in-flight, exit.

---

## 10. Logging

```toml
[logging]
level  = "info"                                # EnvFilter directive
format = "text"                                 # text | json
target = "stderr"                               # stderr | stdout | file:/path
enabled = true
```

Precedence: **CLI flags → `AGENTD_LOG_*` env → `[logging]` → default**.
`--quiet` / `AGENTD_QUIET=1` force `enabled = false`.

The subscriber installs **after** the workflow loads, so the first
instrumented event lands on the configured target. Pre-init errors
(bad config, malformed TOML) go to stderr as plain text.

File target:
- Parent dirs created automatically.
- Append mode (multi-invocation safe).
- Synchronous writes behind a `Mutex<File>` — fine for moderate
  rates. For high throughput, log to stderr and pipe into vector /
  filebeat.

See [`architecture.md`](architecture.md) §8 for the full event taxonomy.

---

## 11. Input resolution — the dotted path mechanism

`ExecutionContext::resolve_path("head.segment.segment")`:

1. First segment → node id (or the reserved `"trigger"` pseudo-node).
2. Each subsequent segment → JSON object key.
3. Any miss → the caller gets `None`.

Pre-populated:

- `trigger.kind` — always one of `"manual"`, `"http"`, `"event"`.
- `trigger.<field>` — top-level payload object fields hoisted in.
- `trigger.input` — non-object payloads wrapped here.
- `trigger.principal.{kind, name}` — present in HTTP mode after
  successful auth.

Every `*_from` / `expr` / `path_from` / `resource_from` / `url_from`
/ `body_from` / `content_from` / `args_from` / `input_from` field,
and every `{{key}}` template substitution, goes through this one
function.

---

## 12. Execution outcome

```
ExecutionOutcome =
    Completed { final_value: Value, last_node: Option<String> }
  | Failed    { reason: String,     last_node: Option<String> }
  | TimedOut  { elapsed: Duration,  last_node: Option<String> }
```

One-shot output (pretty-printed JSON on stdout):

```json
{
  "status": "completed",
  "final_value": null,
  "last_node": "done"
}
```

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Completed |
| `2` | Usage error (bad flags, missing config, unknown arg) |
| `5` | Semantic error — Failed / TimedOut / validation failed / policy denied |

HTTP status mapping in serve mode:

| Outcome | Status |
|---|---|
| Completed | 200 OK |
| Failed | 422 Unprocessable Entity |
| TimedOut | 504 Gateway Timeout |
| Invalid body JSON | 400 Bad Request |
| Unknown path | 404 Not Found |
| Wrong method (path known) | 405 Method Not Allowed |
| Body > 1 MiB | 413 Payload Too Large |
| Headers > 16 KiB | 431 Request Header Fields Too Large |
| Auth denial | 401 Unauthorized |
| Rate limit exceeded | 429 Too Many Requests |
| TLS handshake failed | connection dropped (no HTTP reply) |

---

## 13. Execution trace

```rust
pub struct ExecutionTrace {
    pub entries: Vec<TraceEntry>,
}

pub struct TraceEntry {
    pub node_id: String,
    pub kind: String,                   // e.g. "read_file" / "llm_infer"
    pub outcome: &'static str,          // "continue" / "terminate" / "fail"
    pub branch: Option<String>,         // branch label if any
}
```

`Engine::run_with_trace` returns `(ExecutionOutcome, ExecutionTrace)`.
The trace records the full ordered path through the DAG, including
the outcome flavour and any emitted branch label per node. Fixture
tests diff against expected traces (see §14).

---

## 14. Fixture-driven tests

Drop a directory under `tests/fixtures/<name>/` with two files:

### `workflow.toml`

Same shape as any workflow.

### `fixture.toml`

```toml
start = "main"
dry_run = false                 # optional
timeout_secs = 30               # optional

[trigger]
kind = "manual"                 # manual | http | event
payload = { text = "hello" }    # default: {}

[mocks]
intel = ["first response", "second"]
[mocks.mcp_tools]
say_hi = [{ content = [{ type = "text", text = "hi" }] }]
[mocks.mcp_resources]
"docs://pages/*" = [{ contents = [...] }]

[expected]
status = "completed"            # completed | failed | timed_out
last_node = "done"
reason_contains = "substring"   # Failed only
path = ["analyze", "done"]
path_exact = true               # default false = prefix match
```

### Running

```bash
# Auto-discovery suite (in-tree)
cargo test -p agentd --test fixture_suite

# Your own test
#[test]
fn my_workflow_works() {
    agentd::testing::run_fixture("tests/fixtures/my-flow").assert_pass();
}
```

The runner seeds mock `IntelligenceClient` + `McpClient` from the
fixture's `[mocks]`, runs the engine, and diffs against `[expected]`.

---

## 15. What is NOT supported (by design, today)

| Not supported | Why |
|---|---|
| Cycles | Acyclicity is the termination guarantee |
| Parallel branch execution | Sequential traversal keeps the model predictable; fork/join is a future extension |
| Arbitrary shell (`sh -c "..."`) | `shell_run` is argv-only — injection-safe by construction |
| Dynamic plugin loading | Compile-time-only capability surface |
| LLM-invented tool calls | Intelligence is a bounded reasoning step; it can't add edges or capabilities |
| Unrestricted network access | HTTP goes through `http_request` with policy; no raw sockets exposed |
| Schema validation beyond "must be JSON" | `output_schema` currently only enforces validity — full JSON-Schema is a future feature |
| Durable state across runs | Stateless by design — see maturity doc for the state-store roadmap |
| HTTP/2 | HTTP/1.1 only |
| HTTPS outbound in `http_request` | Plain HTTP only; `tools-http-tls` feature is a follow-up |
| MCP subscription trigger (live listener) | Declarations parse; the listener side needs `resources/subscribe` on the client |

---

## 16. File pointers

| Looking for… | Path |
|---|---|
| RFC / design rationale | `rfcs/0001-bounded-workflow-runtime.md` |
| Workflow types + TOML parse | `crates/agentd/src/workflow/model.rs` |
| DAG validator | `crates/agentd/src/workflow/validator.rs` |
| Engine | `crates/agentd/src/engine/runner.rs` |
| Control-node handlers | `crates/agentd/src/engine/handler.rs` |
| Tool handlers | `crates/agentd/src/tools/` |
| Intelligence client + handler | `crates/agentd/src/intelligence/` |
| MCP client + handlers | `crates/agentd/src/mcp/` |
| Policy manifest + matcher | `crates/agentd/src/policy.rs` |
| HTTP server | `crates/agentd/src/triggers/http.rs` |
| TLS / mTLS | `crates/agentd/src/triggers/http_tls.rs` |
| Auth (bearer / HMAC / mTLS) | `crates/agentd/src/auth/` |
| Rate limiter | `crates/agentd/src/ratelimit.rs` |
| Signals | `crates/agentd/src/signals.rs` |
| Observability | `crates/agentd/src/observability/` |
| Runtime dispatcher | `crates/agentd/src/runtime.rs` |
| Embedded config | `crates/agentd/src/embedded.rs` |
| Build-time validator | `crates/agentd/build.rs` |
| Fixture runner | `crates/agentd/src/testing/` |
| In-tree fixtures | `crates/agentd/tests/fixtures/` |
| CLI smoke tests | `crates/agentd/tests/cli_smoke.rs` |
