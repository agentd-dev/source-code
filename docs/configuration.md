# Configuration reference

`agentd` is configured from the **environment, the command line, and an optional
local config file** — no network config, ever (RFC 0011 §1). The whole
configuration is assembled and **validated before any side effect**: a bad flag,
a missing endpoint, or an unresolvable secret reference exits `2` in
milliseconds, not after an LLM round-trip or an MCP handshake.

> **Build status.** The runtime is implemented: config validation, the agentic
> loop, the supervisor + subagent tree, the MCP client, all five run modes, the
> reactive router + self-scheduling, the served self-MCP, the declarative config
> file (`--config`) with hot reload (SIGHUP + inotify), horizontal scaling
> (`--shard`/`--claim`/`--standby`, `cluster` feature), and multi-endpoint
> intelligence failover are all live (see [`docs/design/PLAN.md`](design/PLAN.md)).
> The flag/env surface below is derived verbatim from the binary's `--help`
> (`help_text()`) and the actual flag/env parsing in
> [`crates/agentd/src/config.rs`](../crates/agentd/src/config.rs). Where a flag
> needs a build feature, that is called out — a feature-gated flag that is set
> in a build without its feature exits `2`, never silently no-ops.

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
- **config file** — a local-only JSON file (`--config <path>` / `AGENT_CONFIG`)
  carrying verbose **structural** config (the MCP-server inventory, declared
  subscriptions, A2A peers, limits, model/log knobs, intelligence endpoint list +
  headers). **Live** (RFC 0017 §3). It slots between *default* and *env*, so env
  and flags still override it. **Repeatable list flags ADD to the file's lists**
  (`--mcp`/`--subscribe`/`--a2a-peer` append to what the file declares). Secrets
  are **never** stored in the file — only `{{secret:NAME}}` / `{{secret-file:PATH}}`
  *references* (§12). See §12 for the full file schema.
- **env var** — every setting that has an env equivalent (12-factor). Live.
- **CLI flag** — highest precedence; overrides env. Live.

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

`Config::validate()` runs **after** all layers merge and **before** the first
side effect — no MCP connect, no LLM call, no subagent spawn, no socket bind. It
is pure-CPU and sub-millisecond. On the first failure it prints
`agentd: <reason>` to stderr and exits **`2`** (`EXIT_USAGE`, a non-retriable
config error for a `podFailurePolicy`; RFC 0011 §5).

Validations enforced at startup (each is also collected by `--validate-config`,
§12):

| Check | Failure message (exit 2) |
|---|---|
| instruction present & non-blank | `missing instruction (INSTRUCTION env or --instruction)` |
| intelligence endpoint present | `missing intelligence endpoint (AGENT_INTELLIGENCE or --intelligence)` |
| every intelligence list element's scheme supported | `intelligence endpoint must be https://host/… (http:// is loopback-only) (got: …)` |
| every per-endpoint token *file* readable | (the secret-file read error) |
| every `--mcp` has a name and a valid endpoint | `mcp server '<name>' has empty name or endpoint` / `mcp server '<name>': endpoint must be https:// (loopback http:// for dev)` |
| `--max-steps` > 0 | `--max-steps must be > 0` |
| `--events-ring` > 0 | `--events-ring must be > 0` |
| `--mode reactive` has ≥1 `--subscribe`/`--continue` | `--mode reactive requires at least one --subscribe or --continue <uri>` |
| `--continue` only with `--mode reactive` | `--continue is only valid with --mode reactive` |
| `--mode schedule` has an interval or cron | `--mode schedule requires --interval <dur> or --cron <expr>` |
| `--cron` only with `--mode schedule` | `--cron is only valid with --mode schedule` |
| `--mode workflow` has a `--workflow <file>` (feature `workflow`) | `--mode workflow requires --workflow <file>` |
| `--workflow` only with `--mode workflow` | `--workflow is only valid with --mode workflow` |
| `--cgroup-memory-max`/`--cgroup-pids-max` need `--cgroup` | `--cgroup-memory-max/--cgroup-pids-max require --cgroup` |
| `--cgroup-*-max` not `0` | `--cgroup-pids-max must be > 0 … or 'max'` / `--cgroup-memory-max must be > 0 or 'max'` |
| `--serve-mcp` target scheme/port valid | `--serve-mcp: scheme unsupported …` |
| `--shard K/N` well-formed (`N>0`, `K<N`) | `--shard: K must be < N (got K/N)` / `--shard: N must be > 0` |
| `N>1` needs the `cluster` feature | `--shard requires the 'cluster' build feature` |
| `--claim` needs the `cluster` feature | `--claim requires the 'cluster' build feature` |
| each `--claim`/`--assign-from` server is a declared `--mcp` | `--claim route '<uri>' names coordination server '<srv>', which is not a declared --mcp server` |
| `--claim-renew-fraction` in `(0, 1)` | `--claim-renew-fraction must be in (0, 1) (got: …)` |
| `--standby`/`--assign-from` need the `cluster` feature | `--standby / --assign-from require the 'cluster' build feature` |
| `--standby`/`--assign-from` only with `--mode reactive` | `--standby / --assign-from are only valid with --mode reactive` |
| `--watch-config` needs the `config-watch` feature | `--watch-config requires the 'config-watch' build feature` |
| `--watch-config` needs a `--config` file | `--watch-config requires a config file (--config / AGENT_CONFIG)` |
| `--a2a-peer` needs the `a2a` feature; each endpoint scheme valid | `--a2a-peer requires the 'a2a' build feature` |
| no inline secret-shaped `intelligence_headers` value; refs resolve | `intelligence_headers['…'] looks like a credential but has an inline value …` |

`-h`/`--help`, `-V`/`--version`, `--capabilities`, `--config-schema`, and
`--validate-config` short-circuit before run-required validation and exit `0`
(`--validate-config` exits `0` on a valid config, `2` if it collected any
diagnostic). An unrecognized argument is a usage error: `unknown argument:
<arg>` → exit `2`.

```console
$ agentd --instruction 'x' --intelligence ftp://nope
agentd: intelligence endpoint must be https://host/… (http:// is loopback-only) (got: ftp://nope)
$ echo $?
2
```

---

## 3. The flag / env table

Every flag below is derived verbatim from the binary's `--help` (`help_text()`)
and the flag/env parsing in `Config::load`. **Only these flags and env vars
exist.** A blank **Env** cell means the setting is **flag-only** (no environment
equivalent is wired up); a blank in the other direction (`AGENT_*` with no flag
column entry) is noted inline. Flags that need a build feature say so — set
without the feature, they exit `2` (§2), never silently no-op.

### 3.1 Core / required

| Flag | Env | Default | Description |
|---|---|---|---|
| `--instruction <TEXT>` | `INSTRUCTION` (or `AGENT_INSTRUCTION`) | *(none; required)* | The task to run. Required for `once`/`loop`/`schedule` (and reactive, which reuses it per reaction). A prefixed spelling wins over the bare one. |
| `--instruction-file <PATH>` | — | — | Read the instruction from a local file (e.g. a ConfigMap/Secret projection). Sets `instruction`. |
| `--intelligence <LIST>` | `AGENT_INTELLIGENCE` (or bare `INTELLIGENCE`) | *(none; required)* | Ordered, comma-separated LLM endpoint **list** for failover (RFC 0018). Each element is `https://host/…` (or a loopback `http://` for a same-host dev gateway) — see §4. A prefixed spelling wins over the bare one. |
| `--config <PATH>` | `AGENT_CONFIG` | *(none)* | Load a declarative JSON config file (§12). |

### 3.2 Intelligence

| Flag | Env | Default | Description |
|---|---|---|---|
| `--intelligence-token <T>` | `AGENT_INTELLIGENCE_TOKEN` | *(none)* | Bearer/API key for endpoint 1. **Never logged**; redacted as `***`. |
| `--intelligence-token-file <PATH>` | `AGENT_INTELLIGENCE_TOKEN_FILE` | *(none)* | Read endpoint 1's token from a mounted file (rotation-friendly). Inline `--intelligence-token`/env wins over it. |
| — | `AGENT_INTELLIGENCE_TOKEN_<N>` / `…_<N>_FILE` | *(none)* | Per-endpoint credential for endpoint *N* (1-indexed; endpoint 1 uses the bare names above, endpoint 2 → `_2`/`_2_FILE`, etc.). A named-but-unreadable `…_FILE` is exit `2` at startup (fail fast before failover). Env-only. |
| `--model <NAME>` | `AGENT_MODEL` | *(none)* | Model id passed to the endpoint. **Reloadable** (§11). |
| `--model-swap <P>` | `AGENT_MODEL_SWAP` | `finish-on-old` | What an in-flight run does when a reload changes `model`: `finish-on-old` (the in-flight turn finishes on the old model, the next turn uses the new one) \| `restart-turn` (the in-flight turn is re-run on the new model from the same pre-turn state). An endpoint repoint with the model unchanged is always finish-on-old regardless (RFC 0018 §5). |
| `--tls-ca <PATH>` | `AGENT_TLS_CA` | *(none — bundled webpki roots only)* | Extra PEM CA certificate(s) trusted for **every outbound** `https://` dial (intelligence, MCP servers, A2A peers, OAuth token endpoints), **added to** the bundled webpki roots — the private/in-cluster PKI anchor. Public material (a CA cert path, never a key). Validated at startup (missing/unreadable/non-CA PEM is exit `2`); installed process-wide before the first dial and inherited by every subagent via the spawn payload. Set-once / restart-only. Needs the `tls` build feature. |

### 3.3 Tools / MCP / delegation

| Flag | Env | Default | Description |
|---|---|---|---|
| `--mcp name=<endpoint>` | — | *(none)* | Declare a remote MCP server, reached over **Streamable HTTP** — `name=https://host[:port][/path]` (or a loopback `http://` for dev). agentd spawns no local process. Repeatable. See §5. **Reloadable** (§11). |
| `--serve-mcp <TARGET>` | `AGENT_SERVE_MCP` | *(off)* | Serve agent's own MCP so agents compose: `https://host:port` (mTLS/bearer auth) or a loopback `http://host:port` (dev). Needs `--features serve-https`. |
| `--a2a-peer name=<ENDPOINT>` | `AGENT_A2A_PEER` | *(none)* | Declare a remote A2A delegation peer: `https://host[:port]` (or a loopback `http://`). Repeatable (the env channel declares one). Needs `--features a2a`. |
| `--workflow <FILE>` | `AGENT_WORKFLOW` | *(none)* | Path to a pinned workflow JSON, driven by `--mode workflow`. Needs `--features workflow`. See [workflows.md](workflows.md). |
| `--workflow-resume <REF>` | `AGENT_WORKFLOW_RESUME` | *(none)* | Resume a pinned workflow from a checkpoint (RFC 0021 §8.4): `<server>:<key>[@seq]` — `server` is a configured `--mcp` checkpointer, `@seq` pins a specific envelope (fork). Only with `--mode workflow`; validated pre-network (unknown server name is exit `2`). A workflow-hash mismatch at resume is a refusal (exit `5`). |
| `--workflow-resume-force` | — | `false` | Override the resume hash check (deliberate graph-edit-and-continue): loop guards reset, board + budget keep. Requires `--workflow-resume`. |
| `--mcp-tags name=tag,tag` | — | *(none)* | Capability tags for the Rule-of-Two check: `untrusted_input`\|`sensitive`\|`egress` (RFC 0012 §3.1). Attaches to a `--mcp` server (order-independent). Repeatable. |
| `--allow-trifecta` | `AGENT_ALLOW_TRIFECTA` | `false` | Permit all three lethal-trifecta legs in one agent instead of refusing at startup (RFC 0012 §3.2). |

### 3.4 Mode & triggers

| Flag | Env | Default | Description |
|---|---|---|---|
| `--mode once\|loop\|reactive\|schedule\|workflow` | `AGENT_MODE` | `once` | Selects the exit predicate (RFC 0008); `workflow` needs `--features workflow` + `--workflow <file>`. See §6. |
| `--subscribe <uri>` | — | *(none)* | Subscribe to an MCP resource (reactive mode); each event spawns a fresh run. Repeatable. |
| `--continue <uri>` | — | *(none)* | Like `--subscribe`, but route every event on the URI into **one warm session** (in order). Reactive only. Repeatable. |
| `--interval <dur>` | — | *(none)* | loop/schedule interval (duration syntax, §7). |
| `--cron <5-field>` | `AGENT_CRON` | *(none)* | UTC cron schedule for `--mode schedule` (needs `--features cron`; §6). |
| `--max-steps <N>` | `AGENT_MAX_STEPS` | `50` | Per-run step cap. Must be > 0. |
| `--max-tokens <N>` | `AGENT_MAX_TOKENS` | `200000` | Token budget for the run. **Reloadable** (§11). |
| `--deadline <dur>` | `AGENT_DEADLINE` | `600s` | Wall-clock deadline (duration syntax, §7). |
| `--max-depth <N>` | — | `4` | Subagent tree depth cap (RFC 0009). |

### 3.5 Sharding / work-claim / standby (`--features cluster`)

Horizontal scaling (RFC 0019). All of these are **restart-only** (§11) and
need the `cluster` build feature; set without it (with `N>1` for the shard),
each exits `2`. See §13 for the fleet model.

| Flag | Env | Default | Description |
|---|---|---|---|
| `--shard K/N` | `AGENT_SHARD` | `0/1` | Partition the URI/key space across a fleet: this replica owns shard `K` of `N` (FNV-1a hash gate). `0/1` is unsharded (the default; no feature needed). `N==0` or `K>=N` is exit `2`. agentctl injects `AGENT_SHARD` from a StatefulSet ordinal (§13). |
| — | `AGENT_SHARD_TIMER` | `shard0` | Timer-route behaviour for a sharded `schedule`/`loop` fleet: `shard0` (only shard 0 fires the fleet-wide ticker) \| `keyed` (every replica fires; a per-tick key gate is applied elsewhere). Env-only. |
| `--claim <uri>=<srv>[:style]` | — | *(none)* | Claim an item before a reactive worker processes it: lease it against coordination MCP server `<srv>` (a declared `--mcp` server), proceed only on a granted lease. The URI is also subscribed + routed. `style` is `tool` (default). `resource` parses but is a **CAS stub — not implemented**; use `tool` (see §13). Repeatable. |
| `--claim-ttl <dur>` | `AGENT_CLAIM_TTL` | `30s` | Requested lease TTL for `work.claim` (the server is the authority; this is the request). |
| `--claim-renew-fraction <F>` | `AGENT_CLAIM_RENEW_FRACTION` | `0.33` | Renew heartbeat at `ttl*F`, `F` in `(0, 1)`. In the synchronous-spawn v1 the renew is a documented no-op — the value is carried forward for the manifest. |
| `--standby` | `AGENT_STANDBY` | `false` | Run a warm, **assignment-driven** reactive worker that races `work.claim` on a shared pending resource (claim-pull) instead of its own content subscriptions. Reactive mode only. |
| `--assign-from <srv>:<uri>` | `AGENT_ASSIGN_FROM` | *(none)* | The shared assignment resource the standby pool claim-pulls from: server `<srv>` (a declared `--mcp` server) owns resource `<uri>`. Desugars into a claim route + a subscribe, so the pool reuses the claim machinery with no new code. Implies reactive mode. |
| — | `AGENT_WARM_INTEL` | `true` when `--standby`, else `false` | Keep the intelligence session warm in standby. **Forward-compat only** — there is no warm-child pool today, so this is accepted, stored, and reported but pre-warms nothing (see §13). Env-only. |

### 3.6 Runtime / observability / security

| Flag | Env | Default | Description |
|---|---|---|---|
| `--run-id <ID>` | `AGENT_RUN_ID` | *(auto)* | Idempotency key (§8). Default: a per-process id (time+pid). |
| `--log-level <L>` | `AGENT_LOG_LEVEL` | `info` | `trace`\|`debug`\|`info`\|`warn`\|`error`. **Reloadable** (§11). |
| `--log-content` | `AGENT_LOG_CONTENT` | `false` | Log tool args/results, not just lengths (RFC 0010 §2.9). Off by default (content-capture-off); propagates to children. |
| `--drain-timeout <dur>` | `AGENT_DRAIN_TIMEOUT` | `25s` | Graceful drain budget. Keep **< pod `terminationGracePeriodSeconds`** (RFC 0011 §3.3). |
| `--health-file <PATH>` | — | *(none)* | Liveness heartbeat file (exec-probe target; RFC 0010). |
| `--metrics-addr <ADDR>` | `AGENT_METRICS_ADDR` | *(off)* | Serve `/metrics`+`/healthz`+`/readyz` on a TCP addr — `host:port`, or `:port` for all IPv4 interfaces (read-only; restrict via firewall/NetworkPolicy if exposed). Needs `--features metrics`. |
| `--cgroup <auto\|PATH>` | `AGENT_CGROUP` | *(off)* | Per-run cgroup-v2 child for atomic `cgroup.kill` teardown: `auto` (derive `<own-cgroup>/agent`) or an absolute path under `/sys/fs/cgroup`. Best-effort — disabled if not writable (RFC 0010). |
| `--cgroup-memory-max <SIZE>` | `AGENT_CGROUP_MEMORY_MAX` | *(none)* | Per-run `memory.max`: `max` or a size (`512M`/`2G`/bytes). Needs `--cgroup` + a parent that can delegate the `memory` controller. `0` is rejected. |
| `--cgroup-pids-max <N>` | `AGENT_CGROUP_PIDS_MAX` | *(none)* | Per-run `pids.max`: `max` or a count. **Counts threads** — set it generously. Needs `--cgroup` + delegation. `0` is rejected. |
| `--traceparent <W3C>` | `AGENT_TRACEPARENT` | *(none)* | Continue an upstream W3C trace; else a trace id is minted from the run id (RFC 0010). |
| `--report-file <PATH>` | `AGENT_REPORT_FILE` | *(off)* | Write the run-outcome report at the terminal transition (atomic temp+rename). Inert for `--mode reactive` (warned at startup; a daemon has no single terminal outcome). RFC 0016. |
| `--events-ring <N>` | `AGENT_EVENTS_RING` | `1024` | Capacity of the in-memory `agent://events` live-tail ring. Must be > 0. Only consumed when the `events` surface is served (`--serve-mcp` + `--features events`). RFC 0016. |
| `--capabilities` | — | — | Print the capabilities manifest (JSON) and exit `0` — the side-effect-free admission probe; succeeds with no instruction. RFC 0015. |
| `-h`, `--help` | — | — | Print help and exit `0`. |
| `-V`, `--version` | — | — | Print version and exit `0`. |

### 3.7 Config file & hot reload (RFC 0017)

| Flag | Env | Default | Description |
|---|---|---|---|
| `--config <PATH>` | `AGENT_CONFIG` | *(none)* | Load a declarative JSON config file (§12). The lowest non-default precedence layer. |
| `--validate-config` | — | — | Load + validate (file + env + flags), print the admission verdict (one `config.valid` line, or one `config.invalid` line per diagnostic — **all** collected in one pass), exit `0`/`2`. Side-effect-free; needs no instruction to validate. |
| `--config-schema` | — | — | Print the config-file JSON Schema (Draft 2020-12) to stdout and exit `0`. Side-effect-free (short-circuits before the file is even read). |
| `--watch-config` | `AGENT_WATCH_CONFIG` | `false` | Watch the `--config` file's directory via `inotify` and reload on change (the same reload SIGHUP triggers). Needs `--features config-watch` **and** a `--config`/`AGENT_CONFIG` file (both validated, exit `2`). See §11. |

Hot reload itself (the `hot-reload` feature) is triggered by **SIGHUP** — there
is no flag for it (§9, §11).

> **Not wired.** RFC 0011 §3.2 once sketched a broader surface
> (`--log-format`/`AGENT_LOG_FORMAT`, `--health-addr`/`AGENT_HEALTH_ADDR` —
> `/healthz` is instead served by the `metrics` feature on `--metrics-addr`,
> `RUST_LOG`, env equivalents for `--interval`/`--subscribe`/`--max-depth`,
> `--mcp-config` — superseded by `--config`, `--pod-grace`/`AGENT_POD_GRACE_SECONDS`,
> a `--budget-exit-code`). **None of these exist in the binary today** — do not
> rely on them. Only the tables above are real.

---

## 4. Intelligence endpoints — schemes & failover

`--intelligence` is an **ordered, comma-separated endpoint list** (RFC 0018 §3.1).
A single element is the common case (and exactly the old single-endpoint
behaviour); multiple elements give sticky-primary **failover** — agentd prefers
the first healthy endpoint and falls back on a circuit-breaker trip. Each element
is selected by URI scheme (RFC 0006):

| Scheme | Form | Use |
|---|---|---|
| `https:` | `https://api.example.com/v1` | Remote HTTPS endpoint (the default; `tls` feature). Pair with a token. |
| `http:` | `http://127.0.0.1:8080` | **Loopback only** — a same-host dev gateway. Any other `http://` host is rejected. |

Every element's scheme is validated at startup; a non-`https`/non-loopback-`http`
scheme on **any** element (e.g. `ftp://…`, or `http://` to a remote host) is exit
`2`. An `https:` endpoint on a `--no-default-features` build (no `tls`) passes the
startup scheme check and is surfaced by the client as `Unsupported` at dial time —
so a `--validate-config`/`--capabilities` probe still passes.

**Per-endpoint credentials.** Endpoint 1 uses `--intelligence-token` /
`AGENT_INTELLIGENCE_TOKEN` (or `…_FILE`). Later endpoints are 1-indexed by env
only: endpoint 2 → `AGENT_INTELLIGENCE_TOKEN_2` (or `AGENT_INTELLIGENCE_TOKEN_2_FILE`),
endpoint 3 → `_3`, and so on. The inline value wins over the file; an absent
token is legal (a public/unauthenticated gateway). A named-but-unreadable token
*file* on any listed endpoint is exit `2` at startup — fail fast, not on failover.

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

All task tools come from MCP servers; agentd ships none of its own and never runs
local code. Declare each server with `--mcp`, repeatable — each names a **remote
MCP endpoint** reached over Streamable HTTP:

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
declared secret-free in the config file's `mcp_servers[].headers` and resolved at
connect time (§12), never inlined in the spec or logged.

An empty name or endpoint is a usage error: `--mcp '<spec>' has empty name or
endpoint`, a spec without `=` fails with `--mcp must be name=endpoint (got: …)`,
and a non-`https`/non-loopback-`http` endpoint is rejected at startup. All exit `2`.

---

## 6. Modes

`--mode` selects the exit predicate — one supervisor loop, four termination
policies (RFC 0008). The lifecycle, config, and signal machinery are identical
across modes.

| Mode | Behavior | Extra requirement |
|---|---|---|
| `once` *(default)* | Run the instruction once to a terminal status, then exit. | — |
| `loop` | Keep working until a bound (steps/deadline/token) or signal. | — |
| `reactive` | Idle; wake on MCP resource updates. Exits only on signal/fatal. | ≥1 `--subscribe <uri>` or `--continue <uri>` |
| `schedule` | Per-fire identical to `once`, driven by an internal timer. | `--interval <dur>` or `--cron <5-field>` (`cron` feature) |

`--continue <uri>` is a reactive variant of `--subscribe`: every event on the URI
re-enters **one warm session** in order (instead of a fresh spawn per event).
`--standby` + `--assign-from` (`cluster` feature) turn a reactive worker into an
assignment-driven member of a claim-pull pool — see §13. Both `--continue` and
`--standby`/`--assign-from` are reactive-only (exit `2` otherwise).

```console
# reactive: requires at least one subscription
$ agentd --instruction 'reconcile on change' \
    --intelligence https://gw.example/v1 \
    --mode reactive \
    --subscribe 'file:///data/desired.json' \
    --subscribe 'file:///data/observed.json'

# schedule: requires an interval
$ agentd --instruction 'emit hourly digest' \
    --intelligence https://gw.example/v1 \
    --mode schedule --interval 1h
```

> **Scope.** Reactivity rides the MCP servers' Streamable-HTTP subscriptions.
> Self-MCP serving (`--serve-mcp`) is over HTTP(S) with mTLS/bearer auth (loopback
> `http://` for dev). Subagent spawn defaults to sync; `{async}`/`{detach}` also
> ship. MCP tasks/sampling/roots are deferred (RFC 0013). For time-scheduling at
> scale, prefer an external `CronJob` firing `--mode once` per tick (RFC 0011 §9);
> the built-in `--interval` is a standalone convenience.

---

## 7. Duration syntax

`--interval`, `--deadline`, and `--drain-timeout` accept a number with an
optional unit suffix. A bare integer means **seconds**.

| Input | Meaning |
|---|---|
| `250ms` | 250 milliseconds |
| `600s` | 600 seconds |
| `5m` | 5 minutes (300 s) |
| `2h` | 2 hours (7200 s) |
| `30` | 30 seconds (bare = seconds) |

Recognized units: `ms`, `s`, `m`, `h`. An empty string, an unparsable number, or
an unknown unit is a usage error (exit `2`), e.g. `unknown duration unit 'd' in
2d` or `invalid duration: nope`.

---

## 8. Run ID & idempotency

`--run-id` / `AGENT_RUN_ID` is the idempotency key propagated into every
outbound MCP `tools/call` `_meta` so backing services can dedupe retries
(RFC 0011 §6).

- **Default** — when unset, agentd mints a per-process id (`time+pid`). It
  correlates logs/traces across the subagent tree but does **not** dedupe
  retries (each retry gets a fresh id).
- **For retry-dedupe** — the operator sets a **stable** key per logical unit of
  work (e.g. a K8s Job name or an input hash), so the same work reuses the same
  `run_id` across retries.

```console
$ agentd --instruction 'enqueue digest' \
    --intelligence https://gw.example/v1 \
    --mcp queue=https://mcp-queue.internal/mcp \
    --run-id "$JOB_NAME"
```

agentd introduces **no local non-idempotent side effects** — it has no built-in
durable tools, so all durable output is externalized through MCP, where the key
acts (RFC 0011 §6.4).

---

## 9. Drain timeout & signals

`--drain-timeout` (default `25s`) bounds the graceful drain on `SIGTERM`/`SIGINT`
(RFC 0011 §4). A clean drain exits **`0`, not `143`**. Keep the drain timeout
**strictly less than** the pod's `terminationGracePeriodSeconds` (recommended
`30`) so the supervisor's own ladder finishes before the kubelet's `SIGKILL`
lands.

```console
$ agentd --instruction 'serve reactions' \
    --intelligence https://gw.example/v1 \
    --mode reactive --subscribe 'file:///data/in.json' \
    --drain-timeout 20s
```

A **second** `SIGTERM`/`SIGINT` forces an immediate `SIGKILL` of all process
groups.

**`SIGHUP` reloads** in a `--features hot-reload` build (§11): it re-reads the
config file and applies the **reloadable subset** at a reactive quiesce boundary,
validate-first. In a build **without** `hot-reload`, `SIGHUP` keeps its default
disposition (terminates) — restart to reconfigure. Restart-only fields (mode,
`run_id`, `serve_mcp`, `drain_timeout`, shard/claim/standby routing, `continue`
topology) never reload (§11).

---

## 10. Observability of config

On startup agentd validates and emits structured
JSON-lines telemetry on stderr; the credential is always redacted. Example
shapes:

```json
{"level":"error","event":"config.invalid","reason":"missing intelligence endpoint (AGENT_INTELLIGENCE or --intelligence)"}
```

```json
{"level":"info","event":"config.loaded","max_steps":50,"max_tokens":200000,"deadline_ms":600000,"max_depth":4,"log_content":false,"serve_mcp":false,"intel_scheme":"https","instruction_len":42}
```

Content-capture stays **off**: `config.loaded` reports the instruction as a
length and the intelligence endpoint as a scheme only — never the instruction
body or the credential. The exact log schema is owned by RFC 0010 (see
[`observability.md`](observability.md)).

---

## 11. Hot reload & the reloadable/restart-only partition (RFC 0017 §5)

In a `--features hot-reload` build a running **reactive** daemon can apply a new
config without a process restart. Two triggers funnel into the **identical**
reload routine:

- **`SIGHUP`** — the portable, dependency-free default (always available when
  `hot-reload` is built).
- **`--watch-config`** (`--features config-watch`) — an `inotify` watch on the
  config file's *parent directory*, so a Kubernetes ConfigMap volume swap (an
  atomic directory-symlink rename) is seen and reloads in place. Needs a
  `--config`/`AGENT_CONFIG` file (else exit `2` — watching nothing is a usage
  error).

Reload is **validate-first**: the new file is re-read and re-merged through the
*same* `Config::load` validation pipeline (built-in < file < env < flag). An
invalid candidate is the same exit-2-class error startup would raise — the
**running config is kept**, nothing is half-applied. A coherence check (RFC 0017
§5.4) then rejects the reload if any **restart-only** field changed.

**Reloadable subset** (applied live at a quiesce boundary):

- `model`, `model_swap`, `max_tokens` — and the limits sub-object (`max_steps`,
  `max_depth`, `deadline`)
- `intelligence` (the endpoint **list**) — repointed via the runtime hot-swap
  primitive; in-flight turns follow `--model-swap` policy (RFC 0018 §5)
- `mcp_servers` — re-handshaked live (removed servers stop+reap, added servers
  spawn+handshake+subscribe with read-after-subscribe)
- `subscribe`, `log_level`, `intelligence_headers`

**Restart-only fields** (`RESTART_ONLY_FIELDS`) — a reload whose diff touches any
of these is **refused** with `reason="restart_required"` (agentctl rolls a pod
restart):

`mode`, `run_id`, `serve_mcp`, `drain_timeout`, `shard`, `claim_routes`,
`standby`, `assign_from`, `continue_subscribe`.

`--validate-config` runs the same coherence check (against no running config, so
it reports the reloadable-subset consistency errors an admission webhook needs).

> **Note.** None of the restart-only fields are file-settable in today's config
> schema (§12) — they are env/flag only — so a file edit can only ever touch the
> reloadable subset. The partition is enforced regardless, so a future widened
> schema stays safe.

---

## 12. The config file (`--config`, RFC 0017 §3)

`--config <PATH>` / `AGENT_CONFIG` loads a single **JSON** document (JSON, not
YAML — `serde_yaml` is a forbidden dependency; render a ConfigMap as JSON).
`//` and `/* */` comments are tolerated. It is the **lowest non-default
precedence layer**: env and flags override it, and repeatable list flags
(`--mcp`/`--subscribe`/`--a2a-peer`) **add to** the file's lists. An unknown key
is a hard error (`deny_unknown_fields` → exit `2`) — the most common config typo,
closed at parse time. Print the schema with `--config-schema` (Draft 2020-12,
exit `0`); validate a candidate with `--validate-config`.

**The file carries only structural config, never secrets.** Settable fields
(everything else stays env/flag):

| File field | Maps to | Notes |
|---|---|---|
| `config_version` | — | Optional; pins the schema major agentctl validated against. |
| `intelligence` | `--intelligence` | The endpoint **list** URI. Reloadable. Transport scheme only — never a credential. |
| `model_swap` | `--model-swap` | `finish-on-old`\|`restart-turn`. |
| `model` | `--model` | Reloadable. |
| `max_tokens` | `--max-tokens` | |
| `limits.max_steps` / `limits.max_depth` / `limits.deadline_secs` | `--max-steps` / `--max-depth` / `--deadline` | `deadline_secs` is whole seconds. |
| `mcp_servers[]` | `--mcp` + `--mcp-tags` | `{name, endpoint, headers{}, tags{glob:[…]}}`. `endpoint` is the `https://` (loopback `http://`) Streamable-HTTP URL. `headers` are secret-free auth/framing templates resolved at connect time (values may carry `{{secret:…}}` refs). `tags` is a glob→tag-list map flattened to the server's tag set. Seeds the list. |
| `subscribe[]` | `--subscribe` | Each string is one subscription URI. Seeds the list. |
| `a2a_peers[]` | `--a2a-peer` | `{name, endpoint}`. Seeds the list. |
| `log_level` | `--log-level` | Reloadable. |
| `intelligence_headers{}` | *(file-only)* | Declared intelligence HTTP headers (RFC 0006 §3). **No flag/env equivalent** — settable only here. Values may carry `{{secret:NAME}}` / `{{secret-file:PATH}}` refs; a credential-shaped header (e.g. `Authorization`) with an *inline* value is rejected (exit `2`). |

**Secret references.** A header (or any file value that needs one) carries a
secret by **reference**, never inline:

- `{{secret:NAME}}` — resolved from the environment variable `NAME`.
- `{{secret-file:PATH}}` — resolved by reading the mounted file at `PATH`.

Every referenced env var must be set and every referenced file readable at
startup, else exit `2`. The resolved value is never stored in `Config` or logged
(header NAMES only ever appear in `Debug`/`config.loaded`/`agent://config/effective`).
The intelligence **credential** itself is *not* a file field — use
`--intelligence-token` / `AGENT_INTELLIGENCE_TOKEN`, `--intelligence-token-file` /
`AGENT_INTELLIGENCE_TOKEN_FILE`, or the per-endpoint `_<N>` / `_<N>_FILE` env
vars (§4).

```jsonc
// /etc/agentd/config.json — structural config; secrets stay in env / mounted files
{
  "config_version": "1.0",
  "intelligence": "https://primary.internal/v1,https://fallback.internal/v1",
  "model": "my-model",
  "model_swap": "finish-on-old",
  "limits": { "max_steps": 80, "max_depth": 3, "deadline_secs": 300 },
  "mcp_servers": [
    { "name": "fs",    "endpoint": "https://mcp-fs.internal/mcp",
      "headers": { "authorization": "Bearer {{secret:FS_TOKEN}}" },
      "tags": { "*": ["sensitive"] } },
    { "name": "queue", "endpoint": "https://mcp-queue.internal/mcp" }
  ],
  "subscribe": ["tickets://queue/inbound"],
  "intelligence_headers": { "anthropic-version": "2023-06-01",
                            "authorization": "Bearer {{secret:LLM_KEY}}" }
}
```

```console
$ agentd --config /etc/agentd/config.json \
    --mode reactive \
    --instruction-file /etc/agentd/task.txt   # instruction + secrets via env/flag
```

For the reloadable-vs-restart-only partition of these fields, see §11.

---

## 13. Horizontal scaling — sharding, work-claim, standby (`--features cluster`)

Three `cluster`-gated surfaces let a fleet of identical agentd replicas process a
shared workload without duplicating it (RFC 0019). All are **restart-only** (§11).
Set without the `cluster` feature, each exits `2` rather than silently doing
nothing.

**Sharding — `--shard K/N` / `AGENT_SHARD`.** This replica owns shard `K` of `N`
(an FNV-1a hash over the URI/key decides ownership). `0/1` (the default) is
unsharded and needs no feature. `N==0` or `K>=N` is exit `2`. agentctl injects
`AGENT_SHARD=K/N` from a StatefulSet pod ordinal; a `--shard` flag overrides it.
`AGENT_SHARD_TIMER` (`shard0` default | `keyed`) controls timer-route behaviour
for a sharded `schedule`/`loop` fleet. Shard identity is immutable — restart-only.

**Work-claim — `--claim <uri>=<server>[:style]`.** Before a reactive worker
processes `<uri>`, it claims it against the coordination MCP server `<server>` (a
declared `--mcp` server advertising the `work.*` tools) and proceeds only on a
granted lease — so two replicas never process the same item. The claimed URI is
**also** subscribed and routed. Repeatable. Lease knobs: `--claim-ttl` (default
`30s`) requests the lease TTL; `--claim-renew-fraction` (default `0.33`, in
`(0,1)`) sets the renew heartbeat at `ttl*F`.

> **`:resource` is a stub.** The `style` suffix is `tool` (the default — calls
> the four `work.*` tools directly) or `resource`. `resource` **parses** but its
> CAS path is **not implemented** in v1 — use `tool`. The renew heartbeat is also
> a documented no-op in the synchronous-spawn v1 (the fraction is carried for the
> manifest and forward compatibility).

**Standby — `--standby` + `--assign-from <server>:<uri>`.** A standby worker is a
**reactive** worker held warm and driven by an *assignment channel* instead of
its own content subscriptions: on the shared pending resource's update, every
standby member races `work.claim` and processes only what it wins (claim-pull,
RFC 0019 §7.2). `--assign-from` desugars into a claim route + a subscribe, so the
pool reuses the existing claim machinery. Reactive-mode only; the named server
must be a declared `--mcp` server.

> **`AGENT_WARM_INTEL` is forward-compat only.** It defaults to `true` under
> `--standby` (else `false`) and is accepted, stored, and reported — but agentd's
> supervisor runs no LLM loop (each reaction re-execs and connects its own
> intelligence), so there is **no warm-child pool** to keep warm in v1. The flag
> exists so a future warm-child-pool build honours operator intent without a
> config break; today it pre-warms nothing.

```console
# A sharded reactive fleet of N replicas (the ordinal → AGENT_SHARD, §deployment.md)
$ AGENT_SHARD=2/8 agentd --mode reactive \
    --intelligence https://gw.example/v1 \
    --instruction-file /etc/agentd/task.txt \
    --subscribe 'tickets://queue/inbound'

# A work-claim worker leasing each item against a coordination server
$ agentd --mode reactive \
    --intelligence https://gw.example/v1 \
    --instruction-file /etc/agentd/task.txt \
    --mcp coord=https://mcp-coord.internal/mcp \
    --claim 'tickets://queue/inbound=coord' \
    --claim-ttl 45s
```

---

## 14. A complete example

```console
$ agentd \
    --instruction-file /etc/agentd/task.txt \
    --intelligence https://llm.internal/v1 \
    --intelligence-token "$LLM_KEY" \
    --model my-model \
    --mcp fs=https://mcp-fs.internal/mcp \
    --mcp queue=https://mcp-queue.internal/mcp \
    --mode once \
    --max-steps 80 --max-tokens 150000 --deadline 5m \
    --max-depth 3 \
    --run-id "$JOB_NAME" \
    --drain-timeout 20s \
    --log-level info \
    --health-file /run/agent/health
```

Equivalent settings via environment (for the env-backed keys), with flags only
where there is no env equivalent:

```console
$ export INSTRUCTION="$(cat /etc/agentd/task.txt)"
$ export AGENT_INTELLIGENCE=https://llm.internal/v1
$ export AGENT_INTELLIGENCE_TOKEN="$LLM_KEY"
$ export AGENT_MODEL=my-model
$ export AGENT_MODE=once
$ export AGENT_MAX_STEPS=80
$ export AGENT_MAX_TOKENS=150000
$ export AGENT_DEADLINE=5m
$ export AGENT_RUN_ID="$JOB_NAME"
$ export AGENT_DRAIN_TIMEOUT=20s
$ export AGENT_LOG_LEVEL=info
$ agentd \
    --mcp fs=https://mcp-fs.internal/mcp \
    --mcp queue=https://mcp-queue.internal/mcp \
    --max-depth 3 \
    --health-file /run/agent/health
```

(`--mcp`, `--max-depth`, and `--health-file` are flag-only — no env equivalent.)
