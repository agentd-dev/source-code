# `agentd` — configuration reference

Complete TOML + CLI + environment-variable reference. This doc is
the authoritative shape of the workflow config file and every
operator-facing override.

Paired with [`capabilities.md`](capabilities.md) (what each
capability does) and [`architecture.md`](architecture.md) (how it's
wired together).

---

## 1. File layout

A workflow file is a TOML document. Top-level sections (every one
optional except `name`):

```toml
name        = "..."             # required
description = "..."             # optional, human-readable

[[start_nodes]]                 # 1 or more entry points
[[triggers]]                    # 0+ event bindings
[[http_routes]]                 # 0+ HTTP endpoints
[[nodes]]                       # the DAG
[[edges]]

[policy.*]                      # optional; empty = AllowAll
[auth.*]                        # optional; HTTP auth bindings
[server.tls]                    # optional; HTTPS / mTLS
[logging]                       # optional; base logging config
```

An alternative wrapped form is also accepted:

```toml
[[workflows]]
name = "..."
# ... same body ...
```

The wrapped form allows one workflow per file. The
`WorkflowDoc::from_toml` parser accepts both.

Every section uses `#[serde(deny_unknown_fields)]` — a typo fails
the parse with a clear line + column from the `toml` crate.

---

## 2. Top-level fields

### `name` (required, string)

Workflow identifier. Surfaces in tracing (`workflow_id`), metrics
spans, and the `/healthz` response.

### `description` (optional, string)

Human-readable blurb. Not used by the runtime.

---

## 3. `[[start_nodes]]`

```toml
[[start_nodes]]
name       = "on_http_request"        # required; unique per workflow
source     = "http"                   # required; manual | http | event
entry_node = "load_resource"          # optional; must reference a nodes[].id
```

Multiple start nodes may share an `entry_node` — the graph body is
reached from any entry point.

If `entry_node` is omitted, the engine picks the unique node with
zero incoming edges at run time. Ambiguity (multiple root nodes
and no explicit `entry_node`) fails with
`Error::Workflow::ambiguous_start_entry`.

---

## 4. `[[triggers]]`

Typed external-event bindings.

### `mcp.resource.updated` / `mcp.resource.created`

```toml
[[triggers]]
type     = "mcp.resource.updated"     # or mcp.resource.created
server   = "docs"                     # logical server name (informational for now)
resource = "docs://pages/*"           # URI pattern
start_node = "on_resource_update"     # must match a [[start_nodes]].name
```

### `internal.event`

```toml
[[triggers]]
type = "internal.event"
name = "retry-requested"
start_node = "on_retry"
```

**Status:** Triggers are cross-referenced at validation time
(start-node must exist), but the listener side for MCP subscriptions
is not wired yet. Declared triggers serve as forward-compat
documentation; the live surface is HTTP routes + manual invocation.

---

## 5. `[[http_routes]]`

Each route maps an HTTP verb + path to a named start node.

```toml
[[http_routes]]
method       = "POST"                       # required; case-insensitive at runtime
path         = "/webhooks/github"           # required; exact-match routing
start_node   = "on_push"                    # required; must exist in start_nodes
input_schema = "schemas/gh-push.json"       # optional; NOT enforced yet (future)
auth         = "hmac:github"                # optional; see §8 for grammar

[http_routes.rate_limit]                    # optional
capacity   = 10                             # required if block is present; > 0
per_second = 1.0                            # required if block is present; > 0 and finite
```

Routes are validated at `HttpServer::spawn`:

- Duplicate `(method, path)` pairs are caught by `deny_unknown_fields`
  on the outer slice during mode resolution (runtime server-side
  check).
- `auth` must reference a configured binding (§8).
- `rate_limit` numbers must pass `RateLimitConfig::validate`.

---

## 6. `[[nodes]]` + `[[edges]]`

### Node shape

```toml
[[nodes]]
id   = "analyze"                            # required; unique per workflow
type = "llm_infer"                          # required; variant discriminator
# ...variant-specific fields...

# Optional retry policy:
[nodes.retry]
max_attempts = 3                            # ≥ 1
backoff_ms   = 500                          # default 100
on           = "transient"                  # any | transient; default any
```

The discriminator `type` is one of:

```
read_file, read_env, read_mcp_resource, parse_json,
template_render, json_select, diff_compute,
llm_infer,
write_file, create_dir, http_request, call_mcp_tool, shell_run,
condition, switch, merge, fail, terminate
```

See [`capabilities.md` §1](capabilities.md#1-node-catalog) for the
fields each variant requires.

### Edge shape

```toml
[[edges]]
from = "analyze"                            # required; must reference a node id
to   = "decision"                           # required; must reference a node id
when = "comment"                            # optional; matches the source node's branch label
```

The validator catches:

- `from` or `to` not declared (`dangling_edge`).
- Cycles (Kahn's algorithm).
- Unreachable nodes (BFS from every start node).
- Duplicate `(from, to, when)` triples.

The engine catches at runtime:

- A node emitting `branch = None` with multiple unconditional
  out-edges (`matching out-edges`).
- A node emitting `branch = Some(label)` with no matching
  `when = "label"` out-edge → dead-end, treated as successful
  completion.

---

## 7. `[policy]`

Fail-closed allowlist. Absent block → AllowAll (Phase-3 back-compat).

```toml
[policy.fs]
read   = ["/workspace/docs/**"]
write  = ["/tmp/agent-out/**"]
delete = []
list   = []                            # falls back to `read` when empty

[policy.env]
read_keys = ["DOCS_ROOT", "AGENTD_*"]

[policy.http]
urls    = ["http://api.internal.example/*"]
methods = ["GET", "POST"]              # optional; empty list = any method

[policy.shell]
commands = ["/usr/bin/git", "/usr/local/bin/mytool"]

[policy.mcp]
servers   = ["docs"]                   # informational for now
tools     = ["comment_on_page"]
resources = ["docs://pages/*"]
```

### Matcher grammar

| Pattern | Matches |
|---|---|
| `"*"` | anything |
| `"prefix/**"` | the exact path `prefix` OR anything starting with `prefix/` |
| `"prefix/*"` | same as `prefix/**` (both are accepted for ergonomics) |
| `"prefix*"` | any string that begins with `prefix` |
| literal (`"/usr/bin/git"`) | exact equality |

### Empty-section semantics

Every sub-section defaults to `[]`. An **empty list** means **deny
everything in this category**. Declaring `[policy]` with only
`[policy.fs]` populated means fs reads/writes within allowlist, but
env / http / shell / MCP are fully denied.

To allow-all a category, set `"*"`:

```toml
[policy.http]
urls = ["*"]
```

### Command canonicalisation (shell)

`shell_run` runs `std::fs::canonicalize` on the command path before
passing it to `Policy::check_shell_run`. A symlink at
`/bin/foo → /usr/local/bin/forbidden` matches against `/usr/local/bin/forbidden`
— symlink escape is caught.

---

## 8. `[auth]`

HTTP-route auth bindings. Each binding has an operator-facing name
referenced from `[[http_routes]].auth`.

### Bearer

```toml
[auth.bearer.ops]
tokens_env = "OPS_TOKENS"               # newline-separated in the env var
# tokens = ["literal-token"]             # tests only; discouraged in prose
```

Both `tokens_env` and `tokens` flatten into the same token set at
verification time — both sources contribute.

### HMAC

```toml
[auth.hmac.github]
secret_env = "GITHUB_WEBHOOK_SECRET"
# secret = "literal"                     # tests only
header = "X-Hub-Signature-256"          # default "X-Agent-Signature"
prefix = "sha256="                      # default "sha256="
```

- `secret_env` takes precedence over `secret` when both are set.
- Empty `prefix` (empty string) is honoured — the header value is
  used verbatim.
- Hex digest compare is constant-time via `Hmac::verify_slice`.

### Route reference grammar

```toml
[[http_routes]]
auth = "none"             # or omit entirely
auth = "bearer"           # → bearer:default
auth = "bearer:ops"
auth = "hmac"             # → hmac:default
auth = "hmac:github"
auth = "mtls"             # requires [server.tls.client_auth]
```

### Startup validation

Every `[[http_routes]].auth` ref is parsed + looked up in `[auth.*]`
at `HttpServer::spawn`. A missing binding fails the bind with a
message like:

```
agent: workflow `foo`: auth ref `bearer:missing` is not defined in [auth.bearer]
```

---

## 9. `[server.tls]`

Behind the `server-tls` Cargo feature.

```toml
[server.tls]
cert_file = "/etc/ssl/server.pem"       # PEM, leaf-first cert chain
key_file  = "/etc/ssl/server.key"       # PEM PKCS8 / RSA / EC

[server.tls.client_auth]                # omit for HTTPS-only
mode    = "required"                    # only `required` wired today
ca_file = "/etc/ssl/client-ca.pem"     # trust root for client certs
```

Build without `server-tls` + a `[server.tls]` block in config →
bind fails:

```
agent: workflow `foo`: workflow declares [server.tls] but this build
lacks the `server-tls` Cargo feature; rebuild with --features server-tls
```

`mode = "optional"` is parsed and deserialises successfully, but is
rejected at config-build time with:

```
tls.client_auth.mode: only `required` is supported in this build
```

Optional mode is future work.

### Artefact requirements

- `cert_file` must contain at least one PEM `CERTIFICATE` block.
  Intermediates should follow the leaf, in order.
- `key_file` must be a single PEM private key in PKCS8, RSA, or EC
  format (rustls-pemfile handles all three).
- `ca_file` must contain one or more PEM certificates — each is
  added to the `RootCertStore` used by the client-cert verifier.

---

## 10. `[logging]`

```toml
[logging]
level   = "info"                    # EnvFilter directive (see below)
format  = "text"                    # text | json
target  = "stderr"                  # stderr | stdout | file:/var/log/agent.log
enabled = true                      # default true; --quiet forces false
```

### `level` — EnvFilter directives

Accepts any `tracing_subscriber::EnvFilter` directive:

- `"warn"` — all targets at warn+
- `"info"` — all at info+
- `"debug"` — all at debug+
- `"agent=debug,hyper=off"` — per-target control
- `"agentd::audit=info,warn"` — split audit from regular logs

An invalid string silently falls through to `"info"` (behaviour of
`EnvFilter::try_new(...).unwrap_or_else(|_| EnvFilter::new("info"))`).

### `target` parse grammar

| String | Target |
|---|---|
| `"stderr"` (case-insensitive) | `LogTarget::Stderr` |
| `"stdout"` | `LogTarget::Stdout` |
| `"file:<path>"` | `LogTarget::File(<path>)`; parent dirs auto-created |

File writes are synchronous under a `Mutex<File>`. For high-throughput
workloads log to stderr and pipe into vector / filebeat.

### Precedence chain

```
CLI flag  >  AGENTD_LOG_* env  >  [logging] block  >  built-in default (warn / text / stderr / enabled)
```

`--quiet` and `AGENTD_QUIET=1` short-circuit: both force `enabled = false`
regardless of config.

---

## 11. CLI flags + environment variables

### Global flags (consumed before subcommand dispatch)

| Flag | Env twin | Default | Purpose |
|---|---|---|---|
| `--log-level LEVEL` | `AGENTD_LOG` | `warn` | EnvFilter directive |
| `--log-format text\|json` | `AGENTD_LOG_FORMAT` | `text` | Tracing output format |
| `--log-target TARGET` | `AGENTD_LOG_TARGET` | `stderr` | `stderr` / `stdout` / `file:PATH` |
| `--quiet` | `AGENTD_QUIET=1` | off | Disable tracing entirely |

### Run flags

| Flag | Env twin | Default | Purpose |
|---|---|---|---|
| `--config FILE` / `-c FILE` | `AGENTD_CONFIG` | — | Workflow file (required unless embedded) |
| `--input FILE` / `-i FILE` | `AGENTD_INPUT` | — | One-shot trigger payload JSON |
| `--start NAME` / `-s NAME` | `AGENTD_START` | auto | Start-node name |
| `--mode once\|serve` | `AGENTD_MODE` | inferred | Override auto mode-selection |
| `--bind HOST:PORT` / `-b HOST:PORT` | `AGENTD_HTTP_BIND` | `127.0.0.1:8080` | Serve-mode bind |
| `--timeout-secs N` | `AGENTD_TIMEOUT_SECS` | `120` | Per-run engine deadline |
| `--drain-timeout-secs N` | `AGENTD_DRAIN_TIMEOUT_SECS` | `30` | Graceful shutdown grace |
| `--intel-unix PATH` | `AGENTD_INTEL_UNIX` | — | Intelligence backend Unix socket |
| `--mcp-stdio "CMD ARGS"` | `AGENTD_MCP_STDIO` | — | MCP server to spawn as a stdio child |
| `--dry-run` | `AGENTD_DRY_RUN=1` | off | Skip every side effect |
| `--validate-only` | `AGENTD_VALIDATE_ONLY=1` | off | Run validator and exit |
| `--version` / `-V` | — | — | Print version; exit 0 |
| `--help` / `-h` | — | — | Print usage; exit 0 |

### No CLI override

These deliberately have **no** CLI flag:

- `[auth.*]` secrets — always environment-sourced (CLI history leak).
- `[server.tls]` paths — workflow-config-sourced only. Symlink the
  right paths at deploy time if they vary per environment.

---

## 12. Build modes (Cargo features)

```
tools-fs                fs handlers (read_file, write_file, create_dir)     [default]
tools-env               read_env                                             [default]
tools-data              parse_json, json_select, template_render             [default]
tools-http              http_request (outbound plain HTTP/1.1 client)
tools-http-tls          https:// in http_request + the agent_loop http tool
                        (ureq, rustls — same stack as intel-remote; implies
                        tools-http; redirects never followed, so the policy
                        allowlist decision stays exact)
tools-shell             shell_run
tools-mcp               (pre-declared; MCP is currently always compiled when used)

trigger-http            HTTP listener (agent serve HTTP)                    [default]
trigger-mcp             (declared; subscription listener not wired)

intel-unix              (declared; Unix intelligence client always compiled)
intel-http              (declared; HTTP intel client not wired)

auth                    bearer + HMAC HTTP auth (pulls sha2 + hmac crates)  [default]
server-tls              in-process TLS termination + mTLS (rustls)          [implies auth]

legacy-plan-act         (removed in the R1 cleanup pass; no longer exists)
```

### Compile-time artefact patterns

```bash
# Default — batteries-included dev build
cargo build -p agentd

# Sealed, minimal surface — read/compute only
cargo build --release -p agentd \
    --no-default-features \
    --features "tools-fs tools-data"

# Production HTTPS service (in-process TLS + HTTPS outbound)
cargo build --release -p agentd \
    --features "auth server-tls tools-http-tls tools-shell"

# Baked config (Mode B — RFC §11.2)
AGENTD_EMBED_CONFIG=./my-workflow.toml cargo build --release -p agentd
```

`build.rs` validates the embedded config at compile time. Typos,
dangling edges, duplicate IDs, and unknown-binding auth refs fail
the build with a clear error — they never land in the binary.

---

## 13. Canonical example — a hardened HTTPS webhook handler

```toml
name        = "notify_router"
description = "Routes webhook events to downstream MCP tools"

# --- Logging -------------------------------------------------------

[logging]
level   = "agent=info,agentd::audit=warn,hyper=off"
format  = "json"
target  = "file:/var/log/agent.log"

# --- TLS + mTLS -----------------------------------------------------

[server.tls]
cert_file = "/etc/ssl/notify-router.pem"
key_file  = "/etc/ssl/notify-router.key"

[server.tls.client_auth]
mode    = "required"
ca_file = "/etc/ssl/internal-ca.pem"

# --- Auth bindings --------------------------------------------------

[auth.bearer.ops]
tokens_env = "OPS_TOKENS"

[auth.hmac.github]
secret_env = "GITHUB_WEBHOOK_SECRET"
header     = "X-Hub-Signature-256"

# --- Policy ---------------------------------------------------------

[policy.http]
urls    = ["http://api.internal.example/*"]
methods = ["POST"]

[policy.shell]
commands = ["/usr/bin/git"]

[policy.mcp]
tools     = ["page_oncall", "post_to_slack", "open_jira"]
resources = ["internal://events/*"]

# --- Entry points ---------------------------------------------------

[[start_nodes]]
name       = "on_github"
source     = "http"
entry_node = "classify"

[[start_nodes]]
name       = "on_ops"
source     = "http"
entry_node = "classify"

# --- HTTP routes ----------------------------------------------------

[[http_routes]]
method     = "POST"
path       = "/webhooks/github"
start_node = "on_github"
auth       = "hmac:github"
[http_routes.rate_limit]
capacity   = 30
per_second = 5.0

[[http_routes]]
method     = "POST"
path       = "/ops/page"
start_node = "on_ops"
auth       = "bearer:ops"
[http_routes.rate_limit]
capacity   = 5
per_second = 0.5

# --- DAG ------------------------------------------------------------

[[nodes]]
id = "classify"
type = "llm_infer"
backend = "default"
prompt  = "Classify this event. JSON: {\"route\":\"pager\"|\"chat\"|\"ticket\"}.\n\n{{payload}}"
input_from    = "trigger"
output_schema = "schemas/route.json"
[nodes.retry]
max_attempts = 3
backoff_ms   = 300
on           = "transient"

[[nodes]]
id = "dispatch"
type = "switch"
expr = "classify.parsed.route"

[[nodes]]
id = "pager"
type = "call_mcp_tool"
tool = "page_oncall"
args_from = "trigger"

[[nodes]]
id = "chat"
type = "call_mcp_tool"
tool = "post_to_slack"
args_from = "trigger"

[[nodes]]
id = "ticket"
type = "call_mcp_tool"
tool = "open_jira"
args_from = "trigger"

[[nodes]]
id = "done"
type = "terminate"

# --- Edges ----------------------------------------------------------

[[edges]]
from = "classify" to = "dispatch"
[[edges]]
from = "dispatch" to = "pager"  when = "pager"
[[edges]]
from = "dispatch" to = "chat"   when = "chat"
[[edges]]
from = "dispatch" to = "ticket" when = "ticket"
[[edges]]
from = "pager"  to = "done"
[[edges]]
from = "chat"   to = "done"
[[edges]]
from = "ticket" to = "done"
```

Invocation:

```bash
export OPS_TOKENS="$(cat /etc/agentd/ops.tokens)"
export GITHUB_WEBHOOK_SECRET="$(cat /etc/agentd/github.secret)"

agentd --config /etc/agentd/notify-router.toml \
      --bind 0.0.0.0:8443 \
      --intel-unix /run/intelligence.sock \
      --mcp-stdio "/usr/local/bin/mcp-ops --root /var/ops" \
      --drain-timeout-secs 60
```

Shutdown: `systemctl stop agent` (or `kill -TERM $(pidof agent)`).
Any in-flight requests get up to 60 seconds to complete; the
process exits 0 on clean drain, 5 on forced.

---

## 14. Validation modes

Three points where the harness validates your config:

### Build time (`build.rs`, only with `AGENTD_EMBED_CONFIG`)

Runs a strict-subset validator. Catches structural errors that
typos produce:

- Missing `name`
- Duplicate node id
- Dangling edge (`from` / `to` doesn't reference a declared node)
- Duplicate start-node name
- `entry_node` references a missing node
- Trigger / HTTP-route `start_node` references a missing start node

### Workflow load (every start)

Runs the full `workflow::validate`. Adds to the build-time checks:

- Acyclicity (Kahn's algorithm)
- Reachability from each start node (BFS)
- Ambiguous start entry (multiple roots, no `entry_node`)

### Server spawn (serve mode)

Adds:

- Auth refs resolve to `[auth.*]` bindings.
- Rate-limit numbers are positive + finite.
- TLS cert + key + CA files load and produce a working rustls config.

Any failure at any layer fails fast with a structured JSON report
(build + load) or a printable error (server spawn). The binary
never runs with a config the runtime hasn't fully vetted.

---

## 15. Quick reference: every knob in one table

| Scope | Knob | Default | Notes |
|---|---|---|---|
| Workflow | `name` | — | Required |
| Workflow | `description` | — | Informational |
| Start node | `source` | — | `event` / `http` / `manual` |
| Start node | `entry_node` | auto | Falls back to sole in-degree-0 node |
| HTTP route | `method` | — | Case-insensitive |
| HTTP route | `auth` | `"none"` | See §8 |
| HTTP route | `input_schema` | — | Not enforced yet |
| HTTP route | `rate_limit.capacity` | — | > 0 |
| HTTP route | `rate_limit.per_second` | — | > 0, finite |
| Node | `retry.max_attempts` | 1 | ≥ 1 |
| Node | `retry.backoff_ms` | 100 | Linear ramp |
| Node | `retry.on` | `any` | `any` / `transient` |
| Policy fs | `read/write/delete/list` | `[]` (deny) | See §7 |
| Policy env | `read_keys` | `[]` (deny) | — |
| Policy http | `urls` | `[]` (deny) | — |
| Policy http | `methods` | `[]` (any) | Empty = any |
| Policy shell | `commands` | `[]` (deny) | Canonicalised paths |
| Policy mcp | `tools` | `[]` (deny) | — |
| Policy mcp | `resources` | `[]` (deny) | — |
| Auth bearer | `tokens_env` | — | Newline-separated |
| Auth hmac | `secret_env` | — | — |
| Auth hmac | `header` | `X-Agent-Signature` | Case-insensitive match |
| Auth hmac | `prefix` | `sha256=` | Empty string honoured |
| Server TLS | `cert_file` | — | PEM leaf-first chain |
| Server TLS | `key_file` | — | PEM PKCS8/RSA/EC |
| Client auth | `mode` | — | Only `required` wired |
| Client auth | `ca_file` | — | Trust root |
| Logging | `level` | `warn` | EnvFilter string |
| Logging | `format` | `text` | `text` / `json` |
| Logging | `target` | `stderr` | `stderr` / `stdout` / `file:PATH` |
| Logging | `enabled` | `true` | — |
| CLI / env | `timeout-secs` | `120` | Per-run engine deadline |
| CLI / env | `drain-timeout-secs` | `30` | Server-mode SIGTERM grace |
| CLI only | `--mode` | inferred | Override `once` / `serve` |
