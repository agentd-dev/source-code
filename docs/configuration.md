# Configuration reference

`agentd` is configured from the **environment, the command line, and an optional
local config file** — no network config, ever. The whole
configuration is assembled and **validated before any side effect**: a bad flag,
a malformed endpoint, a mistyped workflow step, or an unresolvable secret
reference exits `2` in milliseconds, not after an LLM round-trip or an MCP
handshake.

The configuration is one nested **`config_version: "1"`** document, with the
sections `agent`, `goal`, `intelligence`, `mcp`, `tools`, `store`, `memory`,
`context`, `knowledge`, `search`, `skills`, `workflows`, `webhooks`, `limits`,
`lifecycle`, `a2a`, `interface`, `observability`, `security`. Every
**path** in that schema is also an env var (`limits.max_runs` ⇒
`AGENTD_LIMITS_MAX_RUNS`) and a flag (`--limits.max-runs`); a set of short
spellings is wired up as **aliases** (`--instruction`, `--intelligence`,
`--model`, `--mcp`, `--config`, `--log-level`, …). The authoritative machine-readable schema
is **`agentd --config-schema`**; **`agentd --capabilities`** prints the
effective configured surface; **`agentd --validate-config`** validates without
side effects (exit `2` on error).

---

## 1. Precedence

Configuration is resolved in layers, each overriding the previous **key by key**
(a layer only touches the keys it actually sets — an unset env var never clobbers
a lower layer):

```
built-in default  <  config file  <  env var  <  CLI flag
   (lowest)        (--config; live)              (highest)
```

- **built-in default** — the compiled-in defaults (see the table below).
- **config file(s)** — local-only **YAML or JSON** files (`--config <path>`, or
  `-c`; the value may attach with `=`; repeatable;
  `AGENT_CONFIG=a.yaml:b.yaml`) carrying verbose **structural**
  config (the MCP-server inventory, workflow definitions, A2A peers and
  principals, the store, limits, model/log knobs, intelligence endpoint list +
  headers). **Live.** Several files compose into **one document,
  in order — a later file overrides the earlier ones** (§12.2). The merged
  document slots between *default* and *env*, so env and flags still override
  it. **Repeatable list flags ADD to the file's lists**
  (`--mcp`/`--a2a-peer`/`--workflow` append to what the files declare). Secrets
  are **never** stored in a file — only `{{secret:NAME}}` /
  `{{secret-file:PATH}}` *references* (§12). See §12 for the full file schema.
- **env var** — every setting that has an env equivalent (12-factor). Live.
  Every **config-file path** is an env var too, named after the path
  (`limits.run.steps` ⇒ `AGENTD_LIMITS_RUN_STEPS`; §1.1).
- **CLI flag** — highest precedence; overrides env. Live. Every config-file
  path is a flag too (`--limits.run.steps 5`; §1.1).

### 1.1 Config paths — one name, three sources

Every path in the config file's schema is settable from **all three** sources
with a name derived mechanically from the path, so nothing needs per-field
plumbing (`agentd --help` prints the full table under `CONFIG PATHS`):

| source | name for the path `limits.run.steps` |
|---|---|
| file (YAML or JSON) | `limits: { run: { steps: 5 } }` |
| env | `AGENTD_LIMITS_RUN_STEPS` › `AGENT_LIMITS_RUN_STEPS` › bare `LIMITS_RUN_STEPS` (first present wins) |
| flag | `--limits.run.steps 5` = `--limits.run-steps 5` = `--limits-run-steps 5` |

Values are **typed by the schema**: integers/numbers/booleans parse, enums are
checked against their set, a list takes a `[a, b]` literal or a comma-separated
`a, b`, an object takes a `{k: v}` (or JSON) literal, everything else is the
verbatim string. A value that does not type is exit `2` naming the source
(`invalid AGENTD_LIMITS_RUN_STEPS: expected an integer, got "many"`).

**Setting a path SETS its value.** From env or a `--<path>` flag, a list or map
path *replaces* what the files declared (`AGENTD_TOOLS_DISABLED=a,b` ⇒ exactly
`[a, b]`; `--mcp-servers '[{name: q, endpoint: https://…}]'` ⇒ exactly that
list). The **named repeatable flags** (`--mcp`, `--a2a-peer`, `--workflow`)
*add* one element. The named scalar aliases in §3 (`--max-steps`,
`AGENT_MAX_STEPS`, …) set the same path with a shorter spelling.

**Dotted flags reach into objects.** `--limits.run.steps 5` sets a nested
schema path; `--intelligence.headers.x-team ops` sets ONE entry of a free-form
map (the key keeps its exact spelling — `x-team` is not canonicalized) and
merges with the map's other entries; `--intelligence-headers '{k: v}'` sets the
whole map. Array elements are not addressable by path (`--mcp-servers.0.name`
is refused with a clear message): set the whole list, or use the named
repeatable flag.

Example — a flag beats the environment:

```console
$ INSTRUCTION='from-env' AGENT_INTELLIGENCE=https://gw.example/v1 \
    agentd --instruction 'from-flag'
# effective instruction: "from-flag"   (flag wins)
# effective intelligence: https://gw.example/v1  (env, no flag given)
```

**Secrets are env/flag only** — never inline in the config file. The
`--intelligence-token` value is redacted everywhere it could surface
(`Debug` output prints `***`, logs never carry it). The config file may carry
**references** to secrets (`{{secret:NAME}}` → an env var, `{{secret-file:PATH}}`
→ a mounted file) but never an inline credential — a credential-shaped header
with a literal value is rejected at validation (§12).

---

## 2. Validate-at-startup → exit 2

Validation runs **after** all layers merge and **before** the first side effect
— no MCP connect, no LLM call, no subagent spawn, no socket bind. It is pure-CPU
and sub-millisecond. Diagnostics are **collected in one pass**: every problem in
the document is reported, not just the first. Each one prints a
`{"event":"config.invalid","msg":…}` line to stderr and the process exits **`2`**
(`EXIT_USAGE`, a non-retriable config error for a `podFailurePolicy` — retrying
a bad document only reproduces it).

| Check | Example diagnostic (exit 2) |
|---|---|
| `config_version` is `"1"` | `config_version must be "1" (got "3")` |
| every `intelligence.endpoints` element is `https://` (loopback `http://` for dev) | `intelligence endpoint must be https://host[:port][/path] (got: ftp://nope)` / `plaintext http:// intelligence is allowed for loopback only (dev); use https://` |
| `intelligence.swap_policy` / `dialect` / `auth` are coherent | `intelligence.dialect: bedrock requires intelligence.auth.kind = aws (SigV4)` |
| every `mcp.servers[]` has a unique non-reserved name, a valid endpoint, and parseable tags | `mcp.servers[]: a server has an empty name` · `mcp.servers[]: duplicate server name 'fs'` · `mcp server 'a': mcp endpoint must be https://host[:port][/path] (got: ftp://x)` |
| every server reference resolves (`store.mcp.server`, `knowledge.server`, `search.server`, `skills.sources[].server`, `tools.overrides[].server`) | `store.mcp.server 'state' is not a declared MCP server` |
| the chosen `store.kind` carries its block | `store.kind is mcp but store.mcp is not set` · `store.http needs at least 'get' and 'put' operations` · `store.file.path is empty — set a directory, or omit the field to use $AGENTD_STATE_DIR / $XDG_STATE_HOME/agentd/state` |
| a **long-lived** instance (an `a2a.listen`/`webhooks.listen`, a `goal`, or a `loop`/`schedule`/`subscribe`/`signal`/`event`/`stream`/`a2a`/`webhook` start node) has a durable `store` — naming no store at all **defaults** to `kind: file` (§12.3), so this fires only when a config asks for `kind: none` outright | `store.kind is none but the instance is long-lived … — configure a durable store (store.kind: file \| mcp \| http), or drop store.kind to get the local file store by default` |
| every workflow is named, unique, and has exactly one of `file` \| `uri` \| `steps` | `workflows['w'] must have exactly one of file \| uri \| steps` |
| every inline workflow parses under the workflow node registry — the *same* parse the runtime runs at startup | `workflow "w" step "s": unknown field "every" for kind "loop" (allowed: interval, delay, until, max_iterations, backoff, inputs)` |
| an `a2a.listen: https://…` sets `a2a.tls.cert` + `a2a.tls.key`, and a non-loopback bind authenticates its clients | `a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set` · `a2a.listen on a non-loopback address needs client auth: a2a.tls.client_ca, a2a.bearer, and/or interface.pairing` |
| `interface.enabled` has a listener to ride, and pairing has an interface | `interface.enabled requires a2a.listen (the interface is served on the A2A listener)` |
| a `webhook` node has a listener | `a 'webhook' node (start or wait) is used but webhooks.listen is not set — configure webhooks.listen (https://host:port)` |
| a non-loopback `webhooks.listen` authenticates every route it serves — symmetric with `a2a.listen`, since both are inbound listeners that trigger work | `webhooks.listen on a non-loopback address needs auth: set webhooks.default_auth (hmac, bearer or header), or give every 'webhook' node its own auth (HMAC recommended) — unauthenticated: w/h` |
| every `a2a.peers[]` is uniquely named with an `http(s)://` endpoint; every `a2a.principals[]` match names a subject | `a2a peer 'p': endpoint must be http(s)://` · `a2a.principals[0]: match needs one of san \| sub \| bearer_ref \| aauth_agent \| any` |
| `lifecycle.exit_code_map` remaps only the policy codes | `lifecycle.exit_code_map: only the policy codes 3 and 7 are remappable (got key "5")` |
| `lifecycle.watch_config` has a file to watch | `lifecycle.watch_config requires a config file (--config / AGENTD_CONFIG)` |
| `observability.log_level` is a known level; an `audit.sink: store` has a store | `observability.audit.sink includes 'store' but store.kind is none` |
| the **file** layer carries no inline credential | `config file: intelligence.token carries an inline credential; use {{secret:NAME}} / {{secret-file:PATH}} (or set it from env/flag)` |
| no credential-shaped header (`intelligence.headers`, an MCP server's, a peer's, `store.http.headers`) has an inline value | `intelligence.headers['authorization'] looks like a credential but has an inline value; use {{secret:NAME}} / {{secret-file:PATH}}` |
| the root grant is not a lethal trifecta | `lethal-trifecta refused: the root grant wires untrusted_input + sensitive + egress into one agent; narrow the tags or set security.allow_trifecta (audited)` |

Non-fatal findings come back on the same channel as
`{"event":"config.warning","msg":…}` and do **not** change the exit code — a
`store.kind: none` one-shot (not durable: a crash re-runs it), a
`store.kind: memory` (state does not survive the process), a `store.file` block
sitting beside a `kind` that is not `file` (dead config, ignored), a non-loopback
`webhooks.listen` with no `webhooks.default_auth` *and no webhook routes yet*
(nothing is reachable, but the next node added would be — an unauthenticated
route is an error, see below), an `interface.debug` with the
interface off, and an unknown `interface.display` item.

`-h`/`--help`, `-V`/`--version`, `--capabilities`, `--config-schema`,
`--workflow-schema`, and `--validate-config` short-circuit and exit `0`
(`--validate-config` exits `0` on a valid config, `2` if it collected any
diagnostic). None of them need an instruction. An unrecognized argument is a
usage error: `unknown argument: <arg>` → exit `2`; so is an unknown key in a
config file (`deny_unknown_fields`, §12).

```console
$ agentd --instruction 'x' --intelligence ftp://nope --validate-config
{"event":"config.invalid","msg":"agentd: intelligence endpoint must be https://host[:port][/path] (got: ftp://nope)"}
$ echo $?
2
```

---

## 3. The flag / env table

The flags below are the **named aliases**: short spellings for the config paths
an operator reaches for most. They are derived verbatim from the binary's
`--help`, which also prints the complete `CONFIG PATHS` table — **every** schema
path as `--<path>` and `AGENTD_<PATH>`, whether or not it has an alias here.

The **Env** column names the alias's short env var. Each is read as
`AGENTD_<NAME>` › `AGENT_<NAME>` › bare `<NAME>` (first present wins) — so
`INSTRUCTION` below means `AGENTD_INSTRUCTION`, `AGENT_INSTRUCTION`, or a bare
`INSTRUCTION`. The one exception is `AGENT_CONFIG`, which is written out because
it takes only the two prefixed spellings. A blank cell means the setting has no
short env var — reach it by its path instead (`--health-file` ⇒
`AGENTD_OBSERVABILITY_HEALTH_FILE`). Every alias's target path is given so the
file spelling is never in doubt.

### 3.1 Core

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `--instruction <TEXT>` | `agent.instruction` | `INSTRUCTION` | *(none)* | The standing task/policy. May also be a single-token resource URI a declared MCP server serves (read + subscribed). |
| `--instruction-file <PATH>` | `agent.instruction` | — | — | Read the instruction from a local file (e.g. a ConfigMap/Secret projection). |
| `--prompt <TEXT>` | `agent.prompt` | `PROMPT` | *(none)* | A one-shot task: with no workflows configured, the generated run executes this while `instruction` stays the standing policy. |
| `--prompt-file <PATH>` | `agent.prompt` | — | — | Read the prompt from a local file. |
| `--intelligence <LIST>` | `intelligence.endpoints` | `INTELLIGENCE` | *(none)* | Ordered, comma-separated LLM endpoint **list** for failover. Each element is `https://host[:port][/path]` (or a loopback `http://` for a same-host dev gateway) — see §4. |
| `-c`, `--config <PATH>` | — | `AGENT_CONFIG` | *(none)* | Load a declarative config file — YAML or JSON (§12). Repeatable; the `=` form works too. |

### 3.2 Intelligence

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `--intelligence-token <T>` | `intelligence.token` | `INTELLIGENCE_TOKEN` | *(none)* | Bearer/API key for endpoint 1. **Never logged**; redacted as `***`. From a *file* it must be a `{{secret:…}}` reference (§12). |
| `--intelligence-token-file <PATH>` | `intelligence.token_file` | `INTELLIGENCE_TOKEN_FILE` | *(none)* | Read endpoint 1's token from a mounted file (rotation-friendly). An inline token wins over it (and setting both is a warning). |
| — | — | `AGENTD_INTELLIGENCE_TOKEN_<N>` / `…_<N>_FILE` | *(none)* | Per-endpoint credential for endpoint *N* (1-indexed; endpoint 1 uses the bare names above, endpoint 2 → `_2`/`_2_FILE`, etc.). Env-only. |
| `--model <NAME>` | `intelligence.model` | `MODEL` | *(none)* | Model id passed to the endpoint. **Reloadable** (§11). |
| `--model-swap <P>` | `intelligence.swap_policy` | `MODEL_SWAP` | `finish-on-old` | What an in-flight run does when a reload changes `model`: `finish-on-old` (the in-flight turn finishes on the old model, the next turn uses the new one) \| `restart-turn` (the in-flight turn is re-run on the new model from the same pre-turn state). An endpoint repoint with the model unchanged is always finish-on-old regardless — the conversation is identical on either endpoint, so there is nothing to re-run. |
| `--tls-ca <PATH>` | `security.tls_ca` | `TLS_CA` | *(none — bundled webpki roots only)* | Extra PEM CA certificate(s) trusted for **every outbound** `https://` dial (intelligence, MCP servers, A2A peers, OAuth token endpoints), **added to** the bundled webpki roots — the private/in-cluster PKI anchor. Public material (a CA cert path, never a key). Read at startup — a missing/unreadable/non-CA PEM is `security.tls_ca <path>: …` → exit `2` before the first dial (the path is *not* checked by `--validate-config`, which never touches the filesystem for credentials). Installed process-wide and inherited by every subagent via the spawn payload. Restart-only. Needs the `tls` build feature. |

The endpoint's `auth` block (`intelligence.auth.*` — OAuth 2.1, AWS SigV4,
SPIFFE) and `intelligence.dialect` / `intelligence.headers` /
`intelligence.budget` have no short aliases; set them by path or in the file. See
[`authentication.md`](authentication.md) and [`intelligence.md`](intelligence.md).

### 3.3 Tools / MCP / delegation

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `--mcp name=<endpoint>` | `mcp.servers` *(adds one)* | — | *(none)* | Declare a remote MCP server, reached over **Streamable HTTP** — `name=https://host[:port][/path]` (or a loopback `http://` for dev). agentd spawns no local process. Repeatable. See §5. **Reloadable** (§11). |
| `--mcp-tags name=tag,tag` | `mcp.servers[].tags` | — | *(none)* | Capability tags for the Rule-of-Two check: `untrusted_input`\|`sensitive`\|`egress`. Attaches to a declared server (order-independent); an unknown name is exit `2`. Repeatable. |
| `--listen <TARGET>` | `a2a.listen` | `SERVE_MCP` | *(off)* | Arm the A2A listener — the daemon's external channel and operator control: `https://host:port` (mTLS/bearer auth) or a loopback `http://host:port` (dev). `--serve-mcp` is the same alias. Needs `--features a2a`. |
| `--serve-cert` / `--serve-key` / `--serve-client-ca` | `a2a.tls.cert` / `.key` / `.client_ca` | — | *(none)* | The listener's server certificate, private key, and client-CA bundle for mTLS. An `https://` listen without cert+key is exit `2`. |
| `--serve-bearer <T>` | `a2a.bearer` | `SERVE_BEARER` | *(none)* | Static bearer token accepted by the listener. From a *file* it must be a `{{secret:…}}` reference. |
| `--a2a-peer name=<ENDPOINT>` | `a2a.peers` *(adds one)* | — | *(none)* | Declare a remote A2A delegation peer: `https://host[:port]` (or a loopback `http://`, or `unix:///path` for a co-located instance). Repeatable. Needs `--features a2a`. |
| `--workflow <FILE>` | `workflows` *(adds one)* | — | *(none)* | Append a workflow definition to `workflows:` as `{name: <file stem>, file: <path>}` — its start node is the trigger. Repeatable; the same as an inline `workflows:` entry. See [`workflows.md`](workflows.md). |
| `--allow-trifecta` | `security.allow_trifecta` | `ALLOW_TRIFECTA` | `false` | Permit all three lethal-trifecta legs in one agent: the startup refusal is downgraded to a loud, audited warning rather than dropped. |
| `--env <FILE>` | *(process env)* | — | *(none)* | Load a dotenv file into this process's environment before anything reads it — `${VAR}` expansion, `{{secret:NAME}}` resolution, subagent inheritance all see it. Repeatable: later files win; the **real environment always wins** over any file. `KEY=VALUE`, `export` prefix ok, `#` comments, `'…'` literal, `"…"` with `\n`-style escapes, no `$VAR` interpolation inside the file. A malformed line refuses startup naming file:line. |
| `--fresh` | *(process intent)* | — | *(none)* | Start a NEW durable-store generation instead of resuming; the previous generation stays on the store, so nothing is deleted by starting clean. |
| `--prompt-missing` | *(process intent)* | — | *(none)* | Ask interactively on `/dev/tty` (echo off) for each `{{secret:NAME}}` the startup preflight finds missing. Values live in process memory only; a restart re-asks. Refused without a controlling terminal (§12.3). |

Durable runs **resume automatically** from the store on restart; a workflow's
own `resume_policy` (`force` to always restart it) is the per-graph control.

### 3.4 Limits & budgets

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `--max-steps <N>` | `limits.run.steps` | `MAX_STEPS` | `500` | Per-run step cap. **Reloadable** (§11). |
| `--max-tokens <N>` | `limits.run.tokens` | `MAX_TOKENS` | `2000000` | Token budget for a single **run**. **Reloadable** (§11). |
| `--deadline <dur>` | `limits.run.deadline` | `DEADLINE` | `3600s` | Per-run wall-clock deadline (duration syntax, §7). **Reloadable** (§11). |
| `--max-depth <N>` | `limits.subagents.depth` | — | `3` | Subagent tree depth cap — how many levels of children a tree may nest. |
| `--budget-tokens-lifetime <N>` | `intelligence.budget.lifetime_tokens` | `BUDGET_TOKENS` | `0` (unbounded) | Per-**instance** cumulative token cap across **all** runs. See §3.4a. |
| `--budget-exit-code <N>` | `lifecycle.exit_code_map` | — | *(none)* | Remap the policy exit codes `3` and `7` to `N` (`0..=255`) — e.g. exit `0` so a budget stop is not a pod failure. |

`limits.max_runs` (concurrent runs, default `8`), `limits.step_timeout`,
`limits.inline_max_bytes`, and `limits.subagents.{breadth,total,rate}` have no
short alias; set them by path.

#### 3.4a The lifetime token budget (`--budget-tokens-lifetime`)

`--max-tokens` boxes a single run; `--budget-tokens-lifetime` bounds the **whole
instance** — the cumulative tokens across every run the process performs. It
exists so a long-lived agent on a path with **no metering gateway** (e.g. an
AAuth direct dial) still stays bounded. `0` (the default) is unbounded.

- **A job** is that single run, so the effective per-run cap is
  `min(limits.run.tokens, intelligence.budget.lifetime_tokens)`; exhaustion is
  the ordinary `EXIT_BUDGET(7)` path (remappable with `--budget-exit-code`).
- **A daemon** meters cumulative usage; once the cap is reached it **stops
  accepting new work** and **drains cleanly** (exit `0` by default — the
  preferred outcome — or `--budget-exit-code` to signal a policy stop). A
  `budget.exhausted` event marks the transition.
- **Observability**: the gauge `agent_budget_tokens_remaining` tracks the balance
  continuously, and a one-shot `limit.threshold` event fires the first time usage
  crosses 90% of the cap — the alerting/scaling hook, *before* exhaustion. On the
  fleet, the budget is per-member (each pod carries its own instance budget); an
  aggregate fleet cap remains a gateway concern.

The lifetime ceiling is the blunt end of `intelligence.budget`, which also takes
rolling `windows` (`{per: hour, tokens: 2000000}`), an `on_exhausted` tactic
(`wait`\|`slow`\|`degrade`\|`refuse`\|`fail`), a `reserve`, and a `scope`.
`agent.conversation_budget` is the same shape applied per conversation.

### 3.5 Runtime / observability / security

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `--run-id <ID>` | `lifecycle.run_id` | `RUN_ID` | *(auto)* | Idempotency key (§8). Default: a freshly minted ULID. |
| `--drain-timeout <dur>` | `lifecycle.drain_timeout` | `DRAIN_TIMEOUT` | `25s` | Graceful drain budget. Keep **< pod `terminationGracePeriodSeconds`**, or the kubelet's `SIGKILL` lands mid-drain. |
| `--log-level <L>` | `observability.log_level` | `LOG_LEVEL` | `info` | `trace`\|`debug`\|`info`\|`warn`\|`error`. **Reloadable** (§11). |
| `--log-content` | `observability.log_content` | `LOG_CONTENT` | `false` | Log tool args/results, not just lengths. Off by default (content-capture-off), because arguments and results routinely carry data the log is not the right home for; propagates to children. **Reloadable** (§11). |
| `--health-file <PATH>` | `observability.health_file` | — | *(none)* | Liveness heartbeat file, rewritten every 10s — the exec-probe target for images with no HTTP surface. |
| `--metrics-addr <ADDR>` | `observability.metrics_addr` | `METRICS_ADDR` | *(off)* | Serve `/metrics`+`/healthz`+`/readyz` on a TCP addr — `host:port`, or `:port` for all IPv4 interfaces (read-only; restrict via firewall/NetworkPolicy if exposed). Needs `--features metrics`. |
| `--traceparent <W3C>` | `observability.traceparent` | `TRACEPARENT` | *(none)* | Continue an upstream W3C trace; else a trace id is minted from the run id. |
| `--events-ring <N>` | `observability.events_ring` | — | `1024` | Capacity of the in-memory log ring the interface's debug feed tails. Installed only when `interface.enabled` **and** `interface.debug` are on, so an instance with no debug UI pays nothing for it. |
| `--report-file <PATH>` | `observability.report_file` | — | *(off)* | Path for a run-outcome report file. Accepted by the schema; the runtime does not write it — the terminal outcome is the `proc.exit` event and the A2A task artifact. |
| `--cgroup <auto\|PATH>` | `security.cgroup.spec` | — | *(off)* | cgroup-v2 parent for spawned children (turn workers + subagents), each placed in its own leaf for atomic `cgroup.kill` teardown: `auto` (derive `<own-cgroup>/agent`) or an absolute path under `/sys/fs/cgroup`. Best-effort — disabled if not writable. Linux only. |
| `--cgroup-memory-max <SIZE>` | `security.cgroup.memory_max` | — | *(none)* | Per-child `memory.max`: `max` or a size (`512M`/`2G`/bytes). Needs a parent that can delegate the `memory` controller. |
| `--cgroup-pids-max <N>` | `security.cgroup.pids_max` | — | *(none)* | Per-child `pids.max`: `max` or a count. **Counts threads** — set it generously. Needs delegation. |
| `--aauth-provider`, `--aauth-key-file`, `--aauth-enroll-token`, `--aauth-enroll-assertion-file`, `--aauth-person-server` | `security.aauth.*` | — | *(none)* | Agent-identity signing for AAuth-protected servers. Needs `--features aauth`; see [`aauth.md`](aauth.md). |
| `--login <target>` | — | — | — | Complete an interactive OAuth device login for an endpoint (e.g. `mcp:<name>`) and cache the token; exits. Needs `--features oauth`; see [`authentication.md`](authentication.md). |
| `--logout <target>` | — | — | — | Evict a cached credential; exits. |
| `--capabilities` | — | — | — | Print the capabilities manifest (JSON) and exit `0` — the side-effect-free admission probe. |
| `-h`, `--help` | — | — | — | Print help (including the full `CONFIG PATHS` table) and exit `0`. |
| `-V`, `--version` | — | — | — | Print version and exit `0`. |

`security.exec.*` (the guarded local command runner, default-OFF at both build
and run time) is documented in [`security.md`](security.md) §11.

### 3.6 Config file & hot reload

| Flag | Path | Env | Default | Description |
|---|---|---|---|---|
| `-c`, `--config <PATH>` | — | `AGENT_CONFIG` | *(none)* | Load a declarative config file — YAML (`.yaml`/`.yml`) or JSON (`.json`/`.jsonc`; other extensions are sniffed) (§12). The lowest non-default precedence layer. |
| `--validate-config` | — | — | — | Load + validate (files + env + flags), print the admission verdict (one `config.valid` line, or one `config.invalid` line per diagnostic — **all** collected in one pass), exit `0`/`2`. Side-effect-free. |
| `--config-schema` | — | — | — | Print the settings JSON Schema (Draft 2020-12) to stdout and exit `0`. Side-effect-free. |
| `--workflow-schema` | — | — | — | Print the workflow JSON Schema + node registry to stdout and exit `0`. |
| `--watch-config` | `lifecycle.watch_config` | `WATCH_CONFIG` | `false` | Watch each config file's parent directory via `inotify` and reload on change (the same reload SIGHUP triggers). Needs a `--config`/`AGENT_CONFIG` file (validated, exit `2`) and the `config-watch` build feature — without the feature the watch is simply not installed. See §11. |

Hot reload itself (the `hot-reload` feature) is triggered by **SIGHUP** — there
is no flag for it (§9, §11).

### 3.7 Subcommands

`agentd tui` and `agentd ui` run the daemon with a display client attached:
the terminal UI (`--inline` for in-place instead of fullscreen) or
the web UI opened in a browser. Both set `interface.enabled: true` for you, and
the client exits with the daemon. To attach detached instead, run `agentd -c …`
and point `agentd-tui --endpoint <url>` at it. See [`interface.md`](interface.md).

> **Not wired.** There is no `--log-format`/`AGENT_LOG_FORMAT` (the log surface
> is JSON lines, always), no `--health-addr`/`AGENT_HEALTH_ADDR` (`/healthz` is
> served by the `metrics` feature on `--metrics-addr`), no `RUST_LOG`, and no
> `--pod-grace`/`AGENT_POD_GRACE_SECONDS`. Only the tables above and the
> `CONFIG PATHS` table in `agentd --help` are real.

---

## 4. Intelligence endpoints — schemes & failover

`intelligence.endpoints` is an **ordered endpoint list** — a YAML sequence, or
the comma-separated string `--intelligence` takes. A single element is the
common case; multiple elements give sticky-primary **failover** — agentd
prefers the first healthy endpoint and falls back on a circuit-breaker trip.
Each element is selected by URI scheme:

| Scheme | Form | Use |
|---|---|---|
| `https:` | `https://api.example.com/v1` | Remote HTTPS endpoint (the default; `tls` feature). Pair with a token. |
| `http:` | `http://127.0.0.1:8080` | **Loopback only** — a same-host dev gateway. Any other `http://` host is rejected. |
| `mock:` | `mock:final`, `mock:file:play.json` | **Offline dev**: the built-in mock LLM, spawned in-process, dialled over loopback — a whole agent runs with no key, no network, no second terminal. Debug builds always carry it; a release binary needs `--features internal-mocks`. Scripts: `final` (answer immediately), `read`, `schedule`, `file:<playbook.json>` (scripted turns). |

Every element's scheme is validated at startup; a non-`https`/non-loopback-`http`
scheme on **any** element (e.g. `ftp://…`, or `http://` to a remote host) is exit
`2`. An `https:` endpoint on a `--no-default-features` build (no `tls`) passes the
startup scheme check and is surfaced by the client as `Unsupported` at dial time —
so a `--validate-config`/`--capabilities` probe still passes. An empty endpoint
list is *not* a config error: it fails at the first turn, with exit `4`
(intelligence unavailable).

**Per-endpoint credentials.** Endpoint 1 uses `--intelligence-token` /
`AGENT_INTELLIGENCE_TOKEN` (or `…_FILE`). Later endpoints are 1-indexed by env
only: endpoint 2 → `AGENT_INTELLIGENCE_TOKEN_2` (or `AGENT_INTELLIGENCE_TOKEN_2_FILE`),
endpoint 3 → `_3`, and so on. The inline value wins over the file; an absent
token is legal (a public/unauthenticated gateway). A per-endpoint token *file* is
read when that endpoint is resolved, so an unreadable path surfaces there, not at
config validation.

```console
# Single endpoint
$ agentd --instruction 'summarize the queue' \
    --intelligence https://api.example.com/v1 \
    --intelligence-token "$LLM_KEY" --model my-model

# Two endpoints with per-endpoint creds (primary + fallback)
$ AGENT_INTELLIGENCE_TOKEN="$PRIMARY_KEY" \
  AGENT_INTELLIGENCE_TOKEN_2_FILE=/var/run/secrets/fallback-token \
  agentd --instruction 'summarize the queue' \
    --intelligence 'https://primary.internal/v1,https://fallback.internal/v1' \
    --model my-model
```

The endpoint **list** and the `model`/`model-swap` knobs are file-settable and
**reloadable** — a ConfigMap repoint is a hot-swap, not a restart (§11, §12).

---

## 5. Declaring MCP servers — `--mcp name=<endpoint>`

Alongside its own internal tools (memory, artifacts, subagents, workflow control
— `agentd --capabilities` lists them), every *task* tool comes from an MCP
server. agentd runs no local code for them: declare each server with `--mcp`,
repeatable, and each names a **remote MCP endpoint** reached over Streamable
HTTP:

```
--mcp <name>=<endpoint>
```

The spec is split once on `=`: the left side is the server **name**, the right
side is the **endpoint** — `https://host[:port][/path]` (or a loopback `http://`
for dev). agentd spawns no subprocess; it dials the endpoint.

```console
$ agentd --instruction 'tidy /data' \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp \
    --mcp git=https://mcp-git.internal/mcp
```

Per-server auth/framing headers (e.g. `Authorization: Bearer {{secret:…}}`) are
declared secret-free in the config file's `mcp.servers[].headers` and resolved at
connect time (§12), never inlined in the spec or logged. Richer per-server
options — `auth`, `oauth`, `aauth`, `ns`, `timeout`, `tags` — are file/path only.

A spec without `=` fails with `--mcp: want name=endpoint (got: …)`; an empty name
is `mcp.servers[]: a server has an empty name`; a repeated name is
`mcp.servers[]: duplicate server name '<name>'`; the name `code` is reserved for
code-registered tools; and a non-`https`/non-loopback-`http` endpoint is rejected
at startup. All exit `2`.

### The system prompt — `context.template:`

The system prompt is **data plus a template**. The runtime exposes what it
knows — `instance`, `instruction`, `workflows`, `services`, `streams`,
`templates`, `skills`, `peers`, `signals`, `memory`, `tools.internal` — and a
small language renders it:

```text
{{ expr }}                                interpolate
{{#if expr}} … {{else}} … {{/if}}         emptiness counts as false
{{#each expr}} … {{/each}}                `this` is the element, `@index` its position
{{! comment }}
```

Expressions resolve as a **path first, CEL second**. `{{instance}}` and
`{{#each services}}` are bare lookups that work in any build; anything more
(`take(services, 16)`, `size(peers) > 0`) is CEL and needs `--features cel`,
refused at config load on a build without it. Two helpers exist because CEL
lacks them: `take(list, n)` (no slicing in CEL) and `join(list, sep)`.

The built-in default deliberately uses **bare paths only**, so it renders on
every build. That is why the data carries both a list and its joined text
(`tags` / `tags_text`, `params` / `params_text`) and caps lists at 16 (peers
at 24): the default needs no expressions, and CEL is there when you want
different caps or filters.

```yaml
context:
  template: |
    You are {{instance}}. {{#if egress_closed}}Egress is closed.{{/if}}
    ## Instruction
    {{instruction}}
    {{#if services}}
    ## Services
    {{#each take(services, 16)}}- {{this.name}}{{#if this.tags}} [{{join(this.tags, ", ")}}]{{/if}}
    {{/each}}{{/if}}
  templates:
    minimal: "You are {{instance}}: {{instruction}}"    # a node picks this
```

Start from the built-in rather than from scratch — `agentd
--context-template` prints it, and it is written in this same language.

**Order it stable-to-volatile.** Providers cache on the literal prefix of a
request, so a section that changes between turns invalidates the cache for
everything after it. The built-in default puts persona and instruction first,
then configuration-derived sections (workflows, services, streams, subagent
templates), then live state (peers, parked signals, memory keys). A template
that leads with `{{#each signals.waiting}}` works fine and quietly misses the
cache on most turns.

Malformed blocks, unknown block tags and references to names the runtime does
not export refuse startup. A template that never mentions `{{instruction}}`
is legal but warns on every boot, because an agent that silently lost its
standing policy still looks like a working agent. A step selects an
alternate with `context: {template: minimal, seed: [...]}`.

Compaction has the same treatment at its own scale: `context.summarize.prompt`
replaces the summarizer's guidance and `context.summarize.model` runs it on a
cheaper model. The summary's JSON schema is **not** overridable — it is parsed
back into the context, so a prompt asking for another shape produces a refusal
rather than a nicer summary.

### The service catalog — `services:`

For deployments past a handful of servers, the catalog names the external
services the deployment may use **once**, and `mcp.servers` entries reference
them:

```yaml
services:
  billing:
    endpoint: https://billing.internal/mcp
    auth: {kind: static, token: "{{secret:BILLING_MCP}}"}
    tags: {"*": [sensitive]}          # authoritative — a floor, never a suggestion
    allow: [charge_lookup, invoice_*] # the CEILING any consumer may get
    rate: "60/1m"                     # this instance's pacing toward the service
mcp:
  servers:
    - {name: money, service: billing, allow: [charge_lookup]}  # reference + narrow
security:
  egress: closed                      # only catalogued endpoints may be dialed
```

Three rules carry it. **Consumers reference, never restate** — `endpoint`,
`auth` and `headers` on a referencing entry are refused; the effective tool
surface is the intersection with the ceiling (a widening `allow` pattern is a
startup error), excludes union, and an absent consumer `allow` inherits the
ceiling itself. **Catalog tags are a floor, unconditionally** — any server
whose endpoint matches an entry (referencing *or* inline, `open` *or*
`closed`) gets the entry's tags unioned in before the trifecta gate runs, so
under-tagging cannot launder a sensitive endpoint. **`security.egress:
closed` makes the catalog enforceable** — a configured MCP server, the
machinery a subagent template brings with it, or a caller-registered A2A push
target whose URL matches no entry is refused; URL matching is scheme +
authority + path prefix
on segment boundaries, and prefix-comparable entries are themselves a
validation error. `--validate-config` prints each consumer's *effective*
endpoint, admission lists and tags, so review reads the outcome.

A referencing server's credential caches under `service:<entry>` — one
`agentd login service:billing` (or `login mcp:money`, which canonicalizes)
serves every consumer of the entry. Multi-instance fleets share one catalog
by merge order: `agentd -c services.yaml -c desk.yaml` —
[`examples/startup/services.yaml`](https://github.com/agentd-dev/source-code/tree/main/examples/startup)
is the reference deployment.

Entries carry a `kind:` — `mcp` (default), `intelligence`, `peer`, `http` —
and matching is kind-filtered, so one host may serve several kinds. `closed`
covers **all four surfaces**: MCP dials, `intelligence.endpoints` (`mock:`
excepted), `a2a.peers` (which take `service:` references exactly like MCP
servers, inheriting endpoint/auth/headers — also the way peers get `agentd
login`), the `http` step (literal URLs at load, templated at execution, plus
a `kind: http` entry's `methods: [GET, POST]` ceiling in either mode), the
HTTP store, workflow-reference URLs, and A2A push targets. The one surface
deliberately outside: `observability.otel.endpoint` (validation says so).
Two more per-entry knobs: `rate:` paces every consumer's calls **in each
process** — the reactor's steps and the worker/subagent processes' own
in-loop calls alike (a dry bucket is a tool/step failure a retry absorbs;
rate changes take a restart) — and `breaker: {failures, cooldown}` is the
default breaker POLICY for `mcp.tool` steps against the entry, while the
breaker's open/closed state stays per step.

---

## 6. Process shape — `lifecycle.run_until` and start nodes

Two independent settings decide whether the process is a one-shot **job** or a
long-lived **daemon**, and what wakes it:

- **`lifecycle.run_until`** — `idle` (a job: exit once no runs, turns, or pending
  inbox remain, after `lifecycle.idle_grace`, default `5s`), `drained` (a daemon:
  never exits on its own; a SIGTERM finishes in-flight work then exits `0`), or
  `auto` (the default: a job **unless** the instance has an `a2a.listen` or a
  long-lived start node — then a daemon).
- **a workflow start node** — the trigger that fires runs. One workflow may have
  several.

| start `kind` | fires a run… | key fields |
|---|---|---|
| `once` | once, at startup (unless a live run was restored) | `policy` |
| `manual` | only when explicitly triggered (`workflow.run`, or an A2A `workflow.run` command) | — |
| `loop` | repeatedly, on an interval, until a condition | `interval`, `delay`, `until`, `max_iterations`, `backoff` |
| `schedule` | on a clock | `cron: "0 2 * * *"` (needs `--features cron`), or `every: 1h`, or `at: "02:00Z"`; plus `tz`, `jitter`, `catch_up` |
| `subscribe` | when an MCP **resource** updates | `server`, `uri` (both required), `debounce_ms`, `coalesce`, `filter`, `deliver`, `on_no_listener`, `window` |
| `signal` | when a named signal arrives | `name` (required), `filter`, `deliver` |
| `event` | on a runtime event | `on` (required — e.g. `workflow_finished`), `filter` |
| `stream` | on each event of a declared stream | `stream` (required), `subject` (exact or `prefix.*`), `filter`, `from` (`new` \| `earliest`) |
| `webhook` | on an inbound HTTP request | `path` (required), `methods`, `auth`, `parallelism`, `on_overflow`, `rate`, `idempotency`, `respond` |

The **long-lived** kinds — `loop`, `schedule`, `subscribe`, `signal`, `event`,
`stream`, `webhook` — make the instance a daemon under `run_until: auto`, and a daemon is
**durable**: with no `store` section it gets `kind: file` on the local filesystem
(§12.3), and an explicit `kind: none` on a daemon is exit `2` (§2). A bare
`a2a.listen` does the same without any start node: an inbound A2A message
becomes a conversation turn directly. (There is also an `a2a` *start-node*
kind: declaring `command:` on it REGISTERS an A2A command the listener
accepts, and a matching inbound message fires a run instead of a
conversation turn — `examples/hiring/actions.yaml` and `examples/startup/`
are built on it.)

```yaml
workflows:
  - name: watch-queue
    steps:
      s: { kind: subscribe, server: queue, uri: "queue://inbox" }   # the trigger
      t: { kind: agent, depends_on: [s], instruction: "Triage the new item." }
      f: { kind: finish, depends_on: [t] }
```

Every workflow needs a start node and a `finish` step, and every non-start step
declares `depends_on`; `--validate-config` runs the same workflow parse the
runtime does, so a mistyped field is caught before the first side effect (§2).

The **job** shape needs no workflow at all: `agentd --instruction "…"
--intelligence https://…` expands to a `once → agent → finish` workflow, runs one
turn, and exits.

### 6.1 Where definitions come from

An entry in `workflows:` is any ONE of:

```yaml
workflows:
  - name: inline-one                # inline: the steps live in this file
    steps: { … }
  - name: from-file                 # a local file (YAML or JSON)
    file: ./workflows/triage.yaml
  - name: from-url                  # fetched at startup, fail-closed
    url: https://config.internal/workflows/triage.yaml
    headers: { authorization: "Bearer {{secret:WF_TOKEN}}" }
    timeout: 10s                    # default 30s
    allow_private: true             # the fetch rides the same SSRF guard as http nodes
  - dir: ./workflows                # every match becomes a workflow, named by file stem
    glob: "**/*.yaml"               # `*` within a segment, `**` crosses segments
```

A `url` fetch happens once, at startup, before validation — an unreachable URL
or a non-parsing body is exit `2`, not a daemon that silently runs without the
workflow. `headers` follows the same no-inline-credential rule as every other
header map (§3). A `dir` with zero matches is also exit `2`: an empty glob is
almost always a typo, and fail-open here means a reactive daemon with no
reactions. However a definition arrived, it is hashed and pinned identically —
a run started under one hash finishes under it.

**`security.workflows.immutable: true`** makes the loaded set read-only for the
*agent itself*: `workflow.create` / `workflow.update` / `workflow.delete` tool
calls are refused (logged as `workflow.locked`), so a model cannot rewrite its
own standing orders — the definitions are exactly what the operator deployed,
GitOps-style. Operators still change them by editing the source and restarting
(or hot-reloading).

> **Scope.** Reactivity rides the MCP servers' Streamable-HTTP subscriptions. The
> A2A listener (`a2a.listen`) is HTTP(S) with mTLS/bearer auth (loopback
> `http://` for dev). For time-scheduling at scale, prefer an external `CronJob`
> firing a job per tick; the built-in `loop`/`schedule` start nodes are the
> standalone convenience. A schedule that comes due while the process was down
> fires once on restart — missed occurrences collapse rather than replay.

See [`modes-and-triggers.md`](modes-and-triggers.md) for the lifecycle in depth
and [`workflows.md`](workflows.md) for the node catalogue —
`agentd --workflow-schema` prints the authoritative registry.

---


### 6.2 Directives — an instruction that carries its machinery

`agent.instruction` (inline, `--instruction-file`, or a config file) may embed
**colon-fence directives** — the `:::type{attrs}` … `:::` container syntax
MyST and ChatGPT readers already know:

```yaml
agent:
  instruction: |
    You watch the queue and keep things tidy.

    :::workflow
    name: triage
    steps:
      wake: { kind: subscribe, server: queue, uri: "queue://inbox" }
      act:  { kind: agent, depends_on: [wake], instruction: "triage the item" }
      done: { kind: finish, depends_on: [act] }
    :::

    :::skill{name=tidy description="how we tidy"}
    Always sweep before you mop.
    :::

    :::context{title="ops notes"}
    The queue drains overnight.
    :::
```

Four directives, fail-closed (an unknown name is exit `2` naming this set):

- **`:::workflow`** — the YAML body joins `workflows:` exactly as an inline
  entry: same `{{config.*}}` folding, validation, hashing, pinning, and
  retirement (§6.1, workflows doc §retirement). The model reads the *cleaned*
  instruction, where the block became a one-line note — prose and machinery
  never double-speak. An instruction that carries a workflow gets **no sugar
  `main` loop**: it declared its machinery explicitly.
- **`:::skill{name, description, when}`** — an **inline skill**: the body
  joins the skills catalogue with no MCP server involved, referenced as
  `@skill:<name>` like any discovered skill. Inline wins a name collision —
  the operator wrote it closer to this agent than any server did.
- **`:::context{title?}`** / **`:::example`** — model-facing: the fence goes,
  the body stays, wrapped in `<reference>` / `<example>` tags.

Editing the instruction and reloading (SIGHUP / `watch_config`) re-extracts:
an embedded workflow whose body changed is **replaced** (new runs on the new
hash, live runs finish pinned), one that disappeared is **retired** under its
`unload:` policy. Directives are parsed **only** from operator-authored
surfaces — never from conversation text; executing definitions out of less
trusted text would be prompt injection as a feature.

The full story — the precise grammar, the trust rule, retirement, and what
is deliberately out — is [`directives.md`](directives.md).

## 7. Duration syntax

Every duration-typed path — `limits.run.deadline` (`--deadline`),
`limits.step_timeout`, `lifecycle.drain_timeout` (`--drain-timeout`),
`lifecycle.idle_grace`, `intelligence.timeout`, `mcp.default_timeout`,
`a2a.conversation_ttl`, `security.exec.timeout`, and the `interval`/`every`/
`timeout` fields of workflow nodes — accepts a number with an optional unit
suffix. A bare integer means **seconds**.

| Input | Meaning |
|---|---|
| `250ms` | 250 milliseconds |
| `600s` | 600 seconds |
| `5m` | 5 minutes (300 s) |
| `2h` | 2 hours (7200 s) |
| `30d` | 30 days |
| `2w` | 2 weeks |
| `30` | 30 seconds (bare = seconds) |

Recognized units: `ms`, `s`, `m`, `h`, `d`, `w`. An empty string, an unparsable
number, or an unknown unit is a usage error (exit `2`), e.g.
`unknown duration unit 'x' in 2x` or `invalid duration: nope`. Rate windows
(`rate: "<burst>/<per>"`) accept the same units — `1/1d` is one per day.

---

## 8. Run ID & idempotency

`lifecycle.run_id` (`--run-id` / `AGENT_RUN_ID`) is the idempotency key
propagated into every outbound MCP `tools/call` `_meta` — alongside
`agent/instance` and a `traceparent` — so backing services can dedupe retries.

- **Default** — when unset, agentd mints a fresh ULID. It correlates logs/traces
  across the subagent tree but does **not** dedupe retries (each retry gets a
  fresh id).
- **For retry-dedupe** — the operator sets a **stable** key per logical unit of
  work (e.g. a K8s Job name or an input hash), so the same work reuses the same
  `run_id` across retries.

```console
$ agentd --instruction 'enqueue digest' \
    --intelligence https://gw.example/v1 \
    --mcp queue=https://mcp-queue.internal/mcp \
    --run-id "$JOB_NAME"
```

agentd introduces **no local non-idempotent side effects**: its own durable state
(runs, memory, artifacts) goes through the configured `store`, and every other
effect is externalized through MCP, which is where the key does its work.

---

## 9. Drain timeout & signals

`--drain-timeout` (default `25s`) bounds the graceful drain on
`SIGTERM`/`SIGINT`. A clean drain exits **`0`, not `143`**. Keep the drain timeout
**strictly less than** the pod's `terminationGracePeriodSeconds` (recommended
`30`) so the supervisor's own ladder finishes before the kubelet's `SIGKILL`
lands.

```console
# a daemon (an a2a.listen and/or a subscribe start node + a durable store — see §6),
# with a bounded graceful drain:
$ agentd --config daemon.yaml --drain-timeout 20s
```

A **second** `SIGTERM`/`SIGINT` forces an immediate `SIGKILL` of all process
groups.

**`SIGHUP` reloads** in a `--features hot-reload` build (§11): it re-reads the
config files and applies the **reloadable subset** at a quiesce boundary,
validate-first. In a build **without** `hot-reload`, `SIGHUP` keeps its default
disposition (terminates) — restart to reconfigure. Restart-only paths
(`store`, `lifecycle.run_until`/`run_id`/`drain_timeout`, `a2a.listen`/`a2a.tls`/
`a2a.bearer`, `security`, …) never reload (§11).

---

## 10. Observability of config

On startup agentd validates and emits structured JSON-lines telemetry on stderr;
the credential is always redacted. Example shapes:

```json
{"event":"config.warning","msg":"store.kind is none: this one-shot run is not durable (a crash re-runs it from scratch); set store.kind for durability"}
{"event":"config.invalid","msg":"a2a.listen is https:// but a2a.tls.cert / a2a.tls.key are not set"}
{"event":"store.file","path":"/var/lib/agentd","generation":3,"defaulted":true,"msg":"durable state is on the local filesystem; it survives a restart of this process but not a move to another host — use store.kind mcp|http for a fleet"}
```

Once the configuration is accepted the supervisor announces itself:

```json
{"level":"info","event":"proc.ready","comp":"supervisor","instance":"agentd","job_shape":true,"workflows":1,"runs":0,"inbox_pending":0,"run_id":"01M06…","trace_id":"a21f2d…","pid":924712,"ts":"…"}
```

Content-capture stays **off**: no startup line carries the instruction body, an
endpoint credential, or a header value — header *names* only. The full event
schema is in [`observability.md`](observability.md). A reload adds
`config.reloaded`, `config.reload.invalid`, and
`config.reload.restart_required` (§11).

---

## 11. Hot reload & the reloadable/restart-only partition

In a `--features hot-reload` build a running daemon applies a new config without
a process restart. Two triggers funnel into the **identical** reload routine:

- **`SIGHUP`** — the portable, dependency-free default (always available when
  `hot-reload` is built).
- **`lifecycle.watch_config`** / `--watch-config` (`--features config-watch`) —
  an `inotify` watch on each config file's *parent directory*, so a Kubernetes
  ConfigMap volume swap (an atomic directory-symlink rename) is seen and reloads
  in place. Needs a `--config`/`AGENT_CONFIG` file (else exit `2` — watching
  nothing is a usage error).

Reload is **validate-first**: the files are re-read and re-merged through the
*same* load + validation pipeline as startup (built-in < files < env < flags). An
invalid candidate is refused with `config.reload.invalid` — the **running config
is kept**, nothing is half-applied. A coherence check then refuses the reload
with `config.reload.restart_required` if any **restart-only** path changed,
naming the paths that differ.

**Reloadable** (applied live at a quiesce boundary; the flat tree does most of
the work — every turn worker is spawned fresh from the live settings, so the next
unit of work picks the new values up):

- `intelligence.endpoints` / `model` / `token` / `token_file` — repointed via the
  runtime hot-swap primitive; in-flight turns follow the `swap_policy` — and
  `intelligence.budget` (fresh windows, counters carried over)
- `agent.instruction` (a resource instruction re-subscribes) and the rest of
  `agent` — `preflight`, `wake_on`, `tools`, `max_parallel_turns`,
  `on_workflow_finished`, `conversation_budget`
- `mcp` — re-handshaked live: removed servers disconnect, added or changed
  servers connect + initialize; unchanged ones are left alone
- `tools`, `knowledge`, `search` — the tool registry is rebuilt (a registry that
  fails to build refuses the reload and keeps the old one)
- `skills` — the catalogue is re-discovered
- `workflows` — definitions reload and re-arm; **live runs stay pinned** to the
  definition hash they started with
- `limits`, `lifecycle.idle_grace`, `observability.log_level` /
  `log_content`, `memory`, `context`

**Restart-only paths** — a reload whose effective document differs under any of
these is **refused** with `restart_required` (roll the pod instead):

`config_version`, `agent.name`, `store.kind`, `store.prefix`, `store.mcp`,
`store.http`, `store.file`, `lifecycle.run_until`, `lifecycle.drain_timeout`,
`lifecycle.run_id`, `lifecycle.exit_code_map`, `lifecycle.watch_config`,
`a2a.listen`, `a2a.tls`, `a2a.bearer`, `observability.otel`,
`observability.metrics_addr`, `observability.health_file`,
`observability.events_ring`, `observability.traceparent`, `security`.

`store.file` is restart-only for the same reason as the rest of `store`: moving
the state directory under a running instance would strand every key it has
already written there.

Every applied reload logs `config.reloaded` with the changed groups, bumps
`lifecycle.config_generation` in the durable manifest, and is **audited** as a
`config.reload` action.

---

## 12. The config file (`--config`)

`--config <PATH>` (repeatable) / `AGENT_CONFIG` loads one or more documents in
**YAML or JSON** (§12.2 for how several compose). The extension picks the syntax
(`.yaml`/`.yml` ⇒ YAML, `.json`/`.jsonc` ⇒ JSON with `//`/`/* */` comments);
any other extension is sniffed (a document starting with `{`/`[` is JSON, else
YAML). YAML is read by agentd's own
dependency-free subset reader (mappings, sequences, flow collections, quoted /
plain / block `|` `>` scalars, comments, YAML 1.2 core typing — `yes`/`on` are
strings, not booleans); anchors/aliases, tags, merge keys and multi-document
streams are rejected with a line/column error, as are tab indentation and
duplicate keys. Both syntaxes yield the same document, validated identically.
It is the **lowest non-default precedence layer**: env and flags override it, and
repeatable list flags (`--mcp`/`--a2a-peer`/`--workflow`) **add to** the file's
lists. An unknown key is a hard error (`deny_unknown_fields` → exit `2`) naming
the file and listing the fields that *are* allowed — the most common config typo,
closed at parse time. Print the schema with `--config-schema` (Draft 2020-12,
exit `0`); validate a candidate with `--validate-config`.

### 12.1 `.agentd.yml` — the project's own config

When an invocation names **no** config — no `--config`, no `AGENT_CONFIG` —
agentd looks for `.agentd.yml` (or `.agentd.yaml`) in the working directory and
loads it, the way a linter or a formatter picks up its dotfile. So a checked-in
project config makes `agentd` work with no flags at all:

```console
$ cd ~/work/triage     # contains .agentd.yml
$ agentd --validate-config
{"event":"config.valid","files":["./.agentd.yml"],"schema":"2"}
```

Three rules keep it from being surprising:

- **It is only ever a fallback.** Naming a config — by flag or by env — means
  you have already decided; the dotfile is not consulted, not merged, not
  layered underneath. There is no way for a stray file to modify a run you
  spelled out.
- **Only the working directory.** No walk up to a parent, no `$HOME`, no
  `/etc`. Where it applies is exactly where you can see it.
- **Both spellings present is an error** (exit `2`), not a silent pick between
  them — whichever agentd chose, somebody would be editing the other and
  wondering why nothing changed.

`--help`, `--version`, `--config-schema` and `--workflow-schema` never discover
a config, so a malformed dotfile cannot stop you from reading the help.

Once discovered it is an ordinary file layer: env and flags still override it,
and `--watch-config` watches it like any other.

### 12.2 Several files — later overrides earlier

The files in play are, in order: every entry of `AGENT_CONFIG` (a `:`-separated,
PATH-style list), then every `--config <path>` in argument order. They compose
into **one document** with JSON-Merge-Patch semantics (RFC 7396): **objects
merge key by key (recursively), scalars and lists are replaced by the later
file, and an explicit `null` unsets a key**. Each file is type-checked on its
own (an unknown key names the file it is in), then the merged document is
applied as the file layer — env and flags still override it. `proc.start` lists
the merged files (`config_files`); with `--watch-config`, every file is watched
and a change to any of them re-merges the whole set on reload.

```console
$ AGENT_CONFIG=/etc/agentd/base.yaml \
    agentd --config /etc/agentd/site.yaml --config ./local-overrides.yml …
# base.yaml < site.yaml < local-overrides.yml < env < flags
```

```yaml
# base.yaml                     # site.yaml
intelligence:                   intelligence:
  model: default-model            model: site-model      # replaces
limits:                         limits:
  run: { steps: 50 }              subagents: { depth: 3 }  # merges: run.steps 50 kept
  subagents: { depth: 4 }
tools:                          tools:
  disabled: [a]                   disabled: [b]          # REPLACES: [b]
observability:                  observability:
  log_level: debug                log_level: null        # unsets → built-in default
```

### 12.3 What the file carries

The file carries the **whole** schema — every section named at the top of this
page, i.e. every path `agentd --config-schema` prints. There is no separate
"file-only" subset:
each path is equally reachable from env and flags (§1.1), so
`limits.run.steps` ⇒ `AGENTD_LIMITS_RUN_STEPS` / `--limits.run.steps`, and
`mcp.servers` ⇒ `AGENTD_MCP_SERVERS='[{name: fs, endpoint: https://…}]'`.

| Section | Carries |
|---|---|
| `config_version` | `"1"`. Optional, but pin it — any other value is exit `2`. |
| `vars` | Named values (any JSON type, nestable) referenced as `{{config.NAME}}` anywhere a string sits — see §12.4. |
| `agent` | `name`, `instruction`, `prompt`, `preflight`, `wake_on`, `tools` (`internal`/`mcp`/`code` allow-lists), `max_parallel_turns`, `conversation_budget`, `ask_human_fallback`, `on_workflow_finished`. |
| `intelligence` | `endpoints[]`, `model`, `dialect`, `swap_policy`, `timeout`, `headers{}`, `token`/`token_file`, `auth{}` (OAuth 2.1 / AWS SigV4 / SPIFFE), `budget{}`, `pricing`, `structured_output`. |
| `mcp` | `servers[]` — `{name, endpoint, headers{}, tags{glob:[…]}, ns, allow[], exclude[], timeout, auth{}, oauth{}, aauth}` — and `default_timeout`. `allow`/`exclude` gate the server's advertised tool names by glob (exclude beats allow; a gated-out tool never registers). |
| `tools` | `disabled[]`, `overrides{}` (retarget a tool at a declared server, optionally rewriting `args`/`result`). |
| `context` | `template` (the system-prompt template; unset = the built-in, printed by `agentd --context-template`), `templates{}` (named alternates a node picks with `context: {template: <name>}`), `summarize{prompt, model}` (the compaction guidance and a cheaper model to run it on), `compact_at`, `keep_last`, `model_window`, `plan{}`. |
| `store` | `kind` (`file`\|`mcp`\|`http`\|`memory`\|`none`), the matching `file{path, min_free}` / `mcp{}` / `http{}` block, `prefix`, `timeout`, `on_error`, `durability{a2a, steps, work}`, `checkpoint{}`, `audit`. Defaults per instance shape — see below. `durability.work: ephemeral` flips the deployment's durability CLASS: runs and subagent records are memory-only unless a workflow says `durable: true` (docs/workflows.md §durability) — the fast path when all work is recomputable. |
| `workflows` | Inline definitions, or `{name, file}` / `{name, uri}` / `{name, url, headers, timeout, allow_private}` references, or a `{dir, glob}` scan (§6). `security.workflows.immutable: true` locks the loaded set. |
| `streams` | Declared event streams: `streams: {orders: {retention: {max_events: 10000, max_age: 7d}}}`. An `emit` step or `stream` start naming an undeclared stream is exit `2`. Events are durable in the store; retention trims from the head (`max_events` defaults to 10000). |
| `goal` | The goal watchdog: `statement`, `check{via,condition,every}`, `stuck_after`, `on_achieved`, `on_stuck`. |
| `limits` | `max_message_depth` (chained `message` deliveries; default 8), `max_runs`, `run{steps,tokens,deadline}`, `step_timeout`, `inline_max_bytes`, `subagents{depth,breadth,total,rate}`. |
| `lifecycle` | `run_until`, `idle_grace`, `drain_timeout`, `run_id`, `exit_code_map`, `watch_config` (§6, §9). |
| `a2a` | `listen` (`https://host:port`, loopback `http://`, or `unix:///path` for co-located peers — kernel-authenticated, no TLS), `tls{cert,key,client_ca}`, `bearer`, `principals[]`, `peers[]` (endpoints may also be `unix:///path`), `conversation_ttl`. |
| `webhooks` | `listen`, `tls{}`, `default_auth{}` for `webhook` nodes. |
| `interface` | The TUI/web-UI surface served on the A2A listener: `enabled`, `origins[]`, `display{}`, `pairing{}`, `debug`. |
| `memory`, `context`, `knowledge`, `search`, `skills` | Working-memory caps, context window/compaction, and the MCP servers backing knowledge, search, and the skill catalogue. |
| `observability` | `log_level`, `log_content`, `metrics_addr`, `health_file`, `events_ring`, `traceparent`, `report_file`, `otel{}`, `audit{sink}`. |
| `security` | `allow_trifecta`, `tls_ca`, `cgroup{}`, `aauth{}`, `exec{}`, `egress`, `workflows{immutable}`, `policies[]` (ordered verdicts on a tool call — see [security.md](security.md#policies-a-verdict-on-the-call)). |
| `identity` | `autonomous_as` (who a schedule/webhook/stream firing is attributed to; default `system`) and `labels{}` carried with that work — see §15. |

**The `store` section, and the default each instance shape gets.** `store.kind`
picks the adapter: `mcp` (a coordination MCP server's `state.*` tools), `http` (a
plain HTTP key-value endpoint), `file` (this host's filesystem), `memory`
(in-process, lost on exit — dev only), `none` (no durable state). Set it and it
wins. Leave the whole section out and the **shape of the instance** decides:

| instance shape | default `store.kind` |
|---|---|
| one-shot (no long-lived start node, no listener, no `goal`) | `none` — a job that quietly began writing state to disk would surprise everyone who runs one, and a crash simply re-runs it. |
| long-lived (§6) | **`file`** — durability a laptop or a VM already satisfies, with no backend to stand up first. |

A shared backend (`mcp` or `http`) is what you graduate to when one instance
becomes a fleet, because the `file` adapter admits exactly one writer.

```yaml
store:
  kind: file
  file:
    path: /var/lib/agentd     # optional — the chain below applies when it is unset
  prefix: agentd              # as for every adapter (default `agentd`)
```

**Where the directory comes from**, first that applies — the same chain the
credential cache uses, so it is one chain to learn:

1. `store.file.path`
2. `$AGENTD_STATE_DIR`
3. `$XDG_STATE_HOME/agentd/state`
4. `$HOME/.local/state/agentd/state`
5. the OS temp dir — the last resort; the startup line names the path it landed
   on, and state under `/tmp` survives a restart of the process but not a reboot

Under that root the keys are the ordinary ones — `<prefix>/<instance>/<kind>/<id>`
— one JSON file per key, every path segment percent-encoded (so
an id of `../..` is a filename, never a directory hop). Nothing about the key
changes with the adapter, so an instance that outgrows `file` and moves to `mcp`
keeps the identity of everything it wrote.

Five things to know before relying on it:

- **One process per directory, enforced.** On open the adapter takes an
  exclusive `flock` on `<root>/.lock`; a second one fails at startup naming the
  holder — `… is locked by pid 4131 — another agentd is using this state
  directory; give this instance its own agent.name or store.file.path`. A
  directory has no compare-and-set that a second process would respect, so
  rather than pretend, it refuses. Two replicas need `mcp` or `http` (§13).
- **Identity is `agent.name`.** It is what the `<instance>` segment holds (with
  the usual fallback to the downward-API pod name, then `HOSTNAME`), so a
  restart finds its state again by being the same agent — not by a hash of the
  configuration, which would abandon in-flight work the first time somebody
  added an MCP server or fixed a typo. Renaming the agent is an unambiguous "this
  is a different instance".
- **Durability is the filesystem's, not agentd's.** The runtime says which
  directory it landed in, once, at startup (`{"event":"store.file",…}`, §10),
  including whether the path was chosen or defaulted; `--capabilities` carries
  the same as `store_file: {path, defaulted}`. On a container's writable layer
  the state survives a process restart and **not** a reschedule — mount a
  volume, or use `mcp`/`http` ([`deployment.md`](deployment.md)).
- **The disk is watched — `store.file.min_free`.** A checkpoint that hits
  `ENOSPC` halts the daemon, so the runtime measures the store filesystem's
  headroom (~every 2 s) and **sheds before that happens**: below `min_free`
  (default `256MB`; `1.5GiB`, plain bytes, `"0"` disables) no new work is
  admitted — schedules skip with a `start.shed` line, webhooks answer `429
  Retry-After`, queued turns stay queued — while everything in flight drains
  normally. Warn at twice the threshold. Transitions are logged once
  (`pressure.warn` / `pressure.shed` / `pressure.cleared`) and exported as
  `agent_pressure_level` / `agent_disk_free_bytes` (§10). See
  [`operations.md`](operations.md) for the full shed/drain story.
- **`0700` directories, `0600` files, no encryption at rest.** The state holds
  conversation content and tool results, and is protected exactly as the
  credential cache is: by the user the daemon runs as. If that is not enough,
  point `store.file.path` at an encrypted volume. No tool the model can call
  reaches it — there is no `fs` tool, and the store is the runtime's own ledger,
  not part of the agent's surface.

**Secrets stay out of the file.** Four paths are credential-bearing and are
**rejected outright** when a file supplies a literal: `intelligence.token`,
`a2a.bearer`, `security.aauth.enroll_token`, and
`mcp.servers[].oauth.client_secret`. So is any credential-shaped *header* key
(`Authorization`, …) with an inline value, in `intelligence.headers`, an MCP
server's `headers`, an A2A peer's, or `store.http.headers`. From env or a flag an
inline value is fine; from a file it must be a **reference**:

- `{{secret:NAME}}` — resolved from the environment variable `NAME`.
- `{{secret-file:PATH}}` — resolved by reading the mounted file at `PATH`.

References resolve **at startup**, not at `--validate-config` (which is
deliberately environment-independent): an unset env var or an unreadable file is
`agentd: intelligence.token: {{secret:LLM_KEY}} is not set in the environment` →
exit `2` before the first dial. The resolved value is never stored in the
settings or logged — header NAMES only ever reach the logs, and the operator-only
A2A `config` command returns the merged document with the `{{secret:…}}`
references still unresolved.

The startup preflight collects **every** unresolved reference across the config
and all loaded workflow definitions and reports them together — one restart
fixes the list, not one line per restart. Interactively, `--prompt-missing`
turns that list into prompts: each missing `{{secret:NAME}}` is asked for on
`/dev/tty` (echo off, one by one — the same experience as `agentd login`),
values live only in process memory, and a restart re-asks. Without a controlling
terminal the flag refuses and the normal aggregate error stands. Prompted values
resolve exactly like environment ones — including inside workflow steps — they
are just never persisted anywhere.

### 12.4 `vars` — named values for the config and its workflows

```yaml
vars:
  region: eu-1
  api_base: "https://api.eu-1.internal"
  batch: { size: 20, parallel: 4 }

intelligence:
  endpoints: ["{{config.api_base}}/v1"]

workflows:
  - name: sync
    steps:
      pull: { kind: http, url: "{{config.api_base}}/items?region={{config.region}}" }
      # exact-token references keep the var's TYPE:
      each: { kind: batch, over: "{{steps.pull.output.json}}", size: "{{config.batch.size}}" }
```

`{{config.NAME}}` (dotted paths reach into nested values) is substituted at
**load time** — before validation, before the definition hash — so a workflow
fetched from a URL and one written inline resolve identically, and the hash pins
the *resolved* definition. A string that is exactly one token takes the value
typed (`size` above is a number); embedded tokens stringify into place. The
namespace is deliberately `config.`, not `vars.` — `vars.` is the *run*
namespace `assign` writes at runtime; these are deployment constants. An
undefined reference is exit `2`, all misses reported together; there is no
escape syntax, because a URL still containing `{{config.region}}` at runtime is
a bug wherever it was headed. Values are plain data, not secrets — credentials
keep using `{{secret:…}}`, and the credential lint of §12.3 still applies to
them.

A YAML example (`/etc/agentd/config.yaml`):

```yaml
# structural config; secrets stay in env / mounted files
config_version: "1"

agent:
  name: triage
  instruction: You triage incoming items and escalate the risky ones.

intelligence:
  endpoints: [https://primary.internal/v1, https://fallback.internal/v1]
  model: my-model
  swap_policy: finish-on-old
  token: "{{secret:LLM_KEY}}"          # a reference, never inline
  headers:
    anthropic-version: "2023-06-01"

mcp:
  servers:
    - name: fs
      endpoint: https://mcp-fs.internal/mcp
      tags:
        "*": [sensitive]
    - name: web
      endpoint: https://mcp-web.internal/mcp
      headers:
        Authorization: "Bearer {{secret:WEB_TOKEN}}"
      tags:
        "*": [untrusted_input]

store:
  kind: mcp
  mcp: { server: fs }

limits:
  run: { steps: 80, tokens: 150000, deadline: 5m }
  subagents: { depth: 3 }

observability:
  log_level: info
```

And a JSON one:

```jsonc
// /etc/agentd/config.json — structural config; secrets stay in env / mounted files
{
  "config_version": "1",
  "agent": { "instruction": "Triage the inbound queue." },
  "intelligence": {
    "endpoints": ["https://primary.internal/v1", "https://fallback.internal/v1"],
    "model": "my-model",
    "token": "{{secret-file:/var/run/secrets/llm-key}}",
    "headers": { "anthropic-version": "2023-06-01" }
  },
  "mcp": {
    "servers": [
      { "name": "fs",    "endpoint": "https://mcp-fs.internal/mcp",
        "headers": { "authorization": "Bearer {{secret:FS_TOKEN}}" },
        "tags": { "*": ["sensitive"] } },
      { "name": "queue", "endpoint": "https://mcp-queue.internal/mcp" }
    ]
  },
  "store": { "kind": "mcp", "mcp": { "server": "fs" } },
  "limits": { "run": { "steps": 80, "deadline": "5m" }, "subagents": { "depth": 3 } }
}
```

```console
$ agentd --config /etc/agentd/config.json \
    --instruction-file /etc/agentd/task.txt   # instruction + secrets via env/flag
```

For the reloadable-vs-restart-only partition of these fields, see §11.

---

## 13. Running a fleet

There is no `cluster` section, no `--shard` flag, and no per-start `claim` or
`shard` option. agentd carries **no coordination protocol of its own**, because
coordination needs a shared source of truth and agentd already talks to two that
are better placed to own it: the MCP server the work comes from, and the store.

So a fleet partitions **upstream**: one queue subscription per replica, or a
coordination server that hands out work and takes it back when a lease expires.
Both are described, with working config, in [`scaling.md`](scaling.md).

One consequence for the store: a fleet needs `kind: mcp` or `kind: http`. The
`file` adapter (§12.3) is a single-writer store and says so at startup — the
second process to open the directory fails with the first one's pid rather than
interleaving writes into it.

---

## 14. A complete example

A **daemon** that serves A2A, watches a queue, and runs a durable workflow — the
whole configuration in one `config_version: "1"` file:

```yaml
# /etc/agentd/agentd.yaml
config_version: "1"

agent:
  name: triage
  instruction: You triage incoming items and escalate the risky ones.
  preflight: auto

intelligence:
  endpoints: [https://llm.internal/v1, https://llm-fallback.internal/v1]  # ordered failover
  model: my-model
  token: "{{secret:LLM_KEY}}"          # a reference, never the value
  budget:
    windows: [{ per: hour, tokens: 2000000 }]    # rate-limit the token burn

mcp:
  servers:
    - { name: fs,    endpoint: https://mcp-fs.internal/mcp }
    - { name: queue, endpoint: https://mcp-queue.internal/mcp }
    - { name: state, endpoint: https://mcp-state.internal/mcp }

store:                                  # a daemon must be durable
  kind: mcp
  mcp: { server: state }

a2a:                                    # the external channel
  listen: https://0.0.0.0:8443
  tls: { cert: /tls/cert.pem, key: /tls/key.pem, client_ca: /tls/clients.pem }
  principals:
    - { match: { san: "spiffe://ops/*" },  role: operator }
    - { match: { san: "spiffe://team/*" }, role: user, grants: [workflow.*] }

workflows:
  - name: watch-queue
    steps:
      s: { kind: subscribe, server: queue, uri: "queue://inbox" }   # the trigger
      t: { kind: agent, depends_on: [s], instruction: "Triage the new item." }
      f: { kind: finish, depends_on: [t] }

limits:    { max_runs: 8, run: { steps: 80, tokens: 150000, deadline: 5m }, subagents: { depth: 3 } }
lifecycle: { run_until: drained, drain_timeout: 20s }
observability: { log_level: info, health_file: /run/agent/health, metrics_addr: "127.0.0.1:9090", audit: { sink: [log, store] } }
security:  { cgroup: { spec: auto, memory_max: 2G } }
```

```console
$ agentd --config /etc/agentd/agentd.yaml
```

Any path is also an env var and a flag, so a container overrides at deploy time
without editing the file (`built-in < file < env < flag`):

```console
$ AGENTD_INTELLIGENCE_MODEL=my-other-model \
  agentd --config /etc/agentd/agentd.yaml --limits.run.steps 120
```

The **job** shape (a CLI one-shot) is just the `--instruction` sugar — it expands
to a `once → agent → finish` workflow, runs one turn, and exits:

```console
$ agentd --instruction "Summarise the incident." \
    --intelligence https://llm.internal/v1 --model my-model
```

## 15. Identity — who work is done for

A schedule, webhook, stream or `once` start carries no caller, so autonomous
work used to pass no principal at all: "every effect names the human or the
schedule that caused it" was false by construction, because the attribution
chain was dropped at its very first hop.

```yaml
identity:
  autonomous_as: "system:scheduler"       # default: system
  labels: {tenant: internal}

a2a:
  principals:
    - match: {sub: "*@acme.example"}
      role: user
      labels: {tenant: acme, cost_center: CC-42}
      quotas:
        rate: "30/1m"
        budget: {windows: [{per: day, tokens: 200000}]}
```

What travels: the acting id and its labels reach the run record (`run.start`
carries `acting_for`), the MCP `_meta` as `agent/acting_for` and
`agent/labels` — so a server can finally authorize or attribute per user — and
the audit line.

**The quotas now bite.** `quotas.budget` becomes a governor scope beside
`conversation:` and `run:`, and a run is charged to the principal it is *for*,
not only to itself, so a per-person ceiling covers work someone started rather
than only the turns they typed. `quotas.rate` is a real arrival limit, with
operators exempt — locking out the person who administers the daemon during an
incident is worse than the load they could generate. Both are also checked for
*shape* at startup, so a typo is exit 2 rather than a ceiling that silently
does nothing.

**Labels are a closed, operator-declared domain.** They become durable governor
scope keys and audit fields, and minting them from values arriving off the box
would be the same unbounded-cardinality hazard the metrics layer already bans
for labels, relocated into the manifest.

This is an audit field plus quota enforcement — deliberately **not**
multi-tenancy. agentd's answer to "a different caller needs a different
surface" remains a different process, which gives isolation a registry filter
sharing an address space with a prompt-injected turn cannot.
