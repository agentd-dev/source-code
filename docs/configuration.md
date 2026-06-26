# Configuration reference

`agentd` is configured entirely from the **environment and the command line** — no
network config, ever (RFC 0011 §1). The whole configuration is assembled and
**validated before any side effect**: a bad flag or a missing endpoint exits `2`
in milliseconds, not after an LLM round-trip or an MCP handshake.

> **Build status.** The runtime is implemented: config validation, the agentic
> loop, the supervisor + subagent tree, the MCP client, all four run modes, the
> reactive router + self-scheduling, and the served self-MCP are live (see
> [`docs/design/PLAN.md`](design/PLAN.md)). The flag/env surface below is the
> stable v1 surface, derived verbatim from the binary's `--help` + `Config`.

---

## 1. Precedence

Configuration is resolved in layers, each overriding the previous **key by key**
(a layer only touches the keys it actually sets — an unset env var never clobbers
a lower layer):

```
built-in default  <  config file  <  env var  <  CLI flag
   (lowest)           (roadmap)                    (highest)
```

- **built-in default** — the compiled-in defaults (see the table below).
- **config file** — *(roadmap)* a local-only file for verbose structural lists
  (MCP server argv arrays), never for secrets. Not yet implemented; the env+flag
  layers are live today. It will slot between *default* and *env* without
  changing any flag/env name (RFC 0011 §3.1).
- **env var** — every setting that has an env equivalent (12-factor). Live.
- **CLI flag** — highest precedence; overrides env. Live.

Example — a flag beats the environment:

```console
$ INSTRUCTION='from-env' AGENTD_INTELLIGENCE=unix:/run/intel.sock \
    agentd --instruction 'from-flag'
# effective instruction: "from-flag"   (flag wins)
# effective intelligence: unix:/run/intel.sock  (env, no flag given)
```

**Secrets are env/flag only** — never the (roadmap) config file. The
`--intelligence-token` value is redacted everywhere it could surface
(`Debug` output prints `***`, logs never carry it).

---

## 2. Validate-at-startup → exit 2

`Config::validate()` runs **after** all layers merge and **before** the first
side effect — no MCP connect, no LLM call, no subagent spawn, no socket bind. It
is pure-CPU and sub-millisecond. On the first failure it prints
`agentd: <reason>` to stderr and exits **`2`** (`EXIT_USAGE`, a non-retriable
config error for a `podFailurePolicy`; RFC 0011 §5).

Validations enforced at startup:

| Check | Failure message (exit 2) |
|---|---|
| instruction present & non-blank | `missing instruction (INSTRUCTION env or --instruction)` |
| intelligence endpoint present | `missing intelligence endpoint (AGENTD_INTELLIGENCE or --intelligence)` |
| intelligence URI scheme supported | `intelligence endpoint must be unix:/path, https://host/…, or vsock:cid:port (got: …)` |
| every `--mcp` has a name and command | `mcp server '<name>' has empty name or command` |
| `--max-steps` > 0 | `--max-steps must be > 0` |
| `--mode reactive` has ≥1 subscription | `--mode reactive requires at least one --subscribe <uri>` |
| `--mode schedule` has an interval | `--mode schedule requires --interval <dur>` |

`-h`/`--help` and `-V`/`--version` short-circuit before validation and exit `0`.
An unrecognized argument is a usage error: `unknown argument: <arg>` → exit `2`.

```console
$ agentd --instruction 'x' --intelligence ftp://nope
agentd: intelligence endpoint must be unix:/path, https://host/…, or vsock:cid:port (got: ftp://nope)
$ echo $?
2
```

---

## 3. The flag / env table

Every flag below is derived verbatim from the binary's `--help` and
`Config::load`. **Only these flags and env vars exist.** A blank **Env** cell
means the setting is **flag-only** in v1 (no environment equivalent is wired up).

| Flag | Env | Default | Description |
|---|---|---|---|
| `--instruction <TEXT>` | `INSTRUCTION` | *(none; required)* | The task to run. Required for `once`/`loop`/`schedule`. |
| `--instruction-file <PATH>` | — | — | Read the instruction from a local file (e.g. a ConfigMap/Secret projection). Sets `instruction`. |
| `--intelligence <URI>` | `AGENTD_INTELLIGENCE` | *(none; required)* | LLM endpoint. `unix:/path` \| `https://host/…` \| `vsock:cid:port` (see §4). |
| `--intelligence-token <T>` | `AGENTD_INTELLIGENCE_TOKEN` | *(none)* | Bearer/API key. **Never logged**; redacted as `***`. |
| `--model <NAME>` | `AGENTD_MODEL` | *(none)* | Model id passed to the endpoint. |
| `--mcp name=command` | — | *(none)* | Declare an MCP server (stdio). Repeatable. See §5. |
| `--serve-mcp <unix:/path>` | `AGENTD_SERVE_MCP` | *(off)* | Serve agentd's own MCP so agents compose. stdio/unix only in v1 (HTTP serving is roadmap). |
| `--enable-exec` | `AGENTD_ENABLE_EXEC` | `false` | Expose the gated `exec` tool (off by default; RFC 0012). Env accepts `1`/`true`/`yes`/`on`. |
| `--mcp-tags name=tag,tag` | — | *(none)* | Capability tags for the Rule-of-Two check: `untrusted_input`\|`sensitive`\|`egress` (RFC 0012 §3.1). Attaches to a `--mcp` server. Repeatable. |
| `--allow-trifecta` | `AGENTD_ALLOW_TRIFECTA` | `false` | Permit all three lethal-trifecta legs in one agent instead of refusing at startup (RFC 0012 §3.2). |
| `--mode once\|loop\|reactive\|schedule` | `AGENTD_MODE` | `once` | Selects the exit predicate (RFC 0008). See §6. |
| `--subscribe <uri>` | — | *(none)* | Subscribe to an MCP resource (reactive mode). Repeatable. |
| `--interval <dur>` | — | *(none)* | loop/schedule interval (duration syntax, §7). |
| `--cron <5-field>` | `AGENTD_CRON` | *(none)* | UTC cron schedule for `--mode schedule` (needs `--features cron`; §6). |
| `--max-steps <N>` | `AGENTD_MAX_STEPS` | `50` | Per-run step cap. Must be > 0. |
| `--max-tokens <N>` | `AGENTD_MAX_TOKENS` | `200000` | Token budget for the run. |
| `--deadline <dur>` | `AGENTD_DEADLINE` | `600s` | Wall-clock deadline (duration syntax, §7). |
| `--max-depth <N>` | — | `4` | Subagent tree depth cap (RFC 0009). |
| `--run-id <ID>` | `AGENTD_RUN_ID` | *(auto)* | Idempotency key (§8). Default: a per-process id (time+pid). |
| `--log-level <L>` | `AGENTD_LOG_LEVEL` | `info` | `trace`\|`debug`\|`info`\|`warn`\|`error`. |
| `--log-content` | `AGENTD_LOG_CONTENT` | `false` | Log tool args/results, not just lengths (RFC 0010 §2.9). Off by default (content-capture-off); propagates to children. |
| `--drain-timeout <dur>` | `AGENTD_DRAIN_TIMEOUT` | `25s` | Graceful drain budget. Keep **< pod `terminationGracePeriodSeconds`** (RFC 0011 §3.3). |
| `--health-file <PATH>` | — | *(none)* | Liveness heartbeat file (exec-probe target; RFC 0010). |
| `--metrics-addr <ADDR>` | `AGENTD_METRICS_ADDR` | *(off)* | Serve `/metrics`+`/healthz`+`/readyz` on a TCP addr (needs `--features metrics`). |
| `--traceparent <W3C>` | `AGENTD_TRACEPARENT` | *(none)* | Continue an upstream W3C trace; else a trace id is minted from the run id (RFC 0010). |
| `-h`, `--help` | — | — | Print help and exit `0`. |
| `-V`, `--version` | — | — | Print version and exit `0`. |

> **Not yet wired.** RFC 0011 §3.2 sketches a broader surface
> (`--log-format`/`AGENTD_LOG_FORMAT`, `--health-addr`/`AGENTD_HEALTH_ADDR` —
> `/healthz` is instead served by the `metrics` feature on `--metrics-addr`,
> `RUST_LOG`, env equivalents for `--interval`/`--subscribe`/`--max-depth`,
> `AGENTD_MCP_CONFIG`/`--mcp-config`, `--pod-grace`/`AGENTD_POD_GRACE_SECONDS`,
> a `--budget-exit-code`). **None of these exist in the binary today** — do not
> rely on them. Only the table above is real.

---

## 4. Intelligence URI schemes

The single LLM endpoint is selected by URI scheme (RFC 0006). The validator
accepts exactly these prefixes:

| Scheme | Form | Use |
|---|---|---|
| `unix:` | `unix:/run/intel.sock` | Local unix-domain socket (a sidecar/broker). |
| `https:` | `https://api.example.com/v1` | Remote HTTPS endpoint. Pair with `--intelligence-token`. |
| `vsock:` | `vsock:2:5000` | VM-to-host vsock (`cid:port`), e.g. a Firecracker/Kata guest. |
| `http:` | `http://127.0.0.1:8080` | **Dev only** — accepted, but the client warns (no TLS). |

Anything else (e.g. `ftp://…`) fails validation with exit `2`.

```console
$ agentd --instruction 'summarize the queue' \
    --intelligence https://api.example.com/v1 \
    --intelligence-token "$LLM_KEY" --model my-model
```

---

## 5. Declaring MCP servers — `--mcp name=command`

All tools come from MCP servers; agentd ships none of its own (except the gated
`exec`). Declare each server with `--mcp`, repeatable:

```
--mcp <name>=<command> [args…]
```

The spec is split once on `=`: the left side is the server **name**, the right
side is the **command**, whitespace-split into argv (an M1 simplification; the
roadmap config-file layer will carry argv arrays verbatim for commands with
spaces/quotes).

```console
$ agentd --instruction 'tidy /data' \
    --intelligence unix:/run/intel.sock \
    --mcp fs='mcp-server-fs --root /data' \
    --mcp git='mcp-server-git --repo /data/proj'
```

This parses to two servers:

- `fs` → argv `["mcp-server-fs", "--root", "/data"]`
- `git` → argv `["mcp-server-git", "--repo", "/data/proj"]`

An empty name or empty command is a usage error: `--mcp '<spec>' has empty name
or command`, and a spec without `=` fails with `--mcp must be name=command (got:
…)`. Both exit `2`.

Transport is **stdio** in v1.

---

## 6. Modes

`--mode` selects the exit predicate — one supervisor loop, four termination
policies (RFC 0008). The lifecycle, config, and signal machinery are identical
across modes.

| Mode | Behavior | Extra requirement |
|---|---|---|
| `once` *(default)* | Run the instruction once to a terminal status, then exit. | — |
| `loop` | Keep working until a bound (steps/deadline/token) or signal. | — |
| `reactive` | Idle; wake on MCP resource updates. Exits only on signal/fatal. | ≥1 `--subscribe <uri>` |
| `schedule` | Per-fire identical to `once`, driven by an internal timer. | `--interval <dur>` or `--cron <5-field>` (`cron` feature) |

```console
# reactive: requires at least one subscription (stdio-only in v1)
$ agentd --instruction 'reconcile on change' \
    --intelligence unix:/run/intel.sock \
    --mode reactive \
    --subscribe 'file:///data/desired.json' \
    --subscribe 'file:///data/observed.json'

# schedule: requires an interval
$ agentd --instruction 'emit hourly digest' \
    --intelligence unix:/run/intel.sock \
    --mode schedule --interval 1h
```

> **v1 scope.** Reactivity is **stdio-only** in v1 — reactive-over-HTTP is
> roadmap. Self-MCP serving (`--serve-mcp`) is **stdio/unix only**; HTTP serving
> is roadmap. Subagent spawn defaults to sync; `{async}`/`{detach}` also ship. MCP
> tasks/sampling/roots are deferred (RFC 0013). For time-scheduling at scale,
> prefer an external `CronJob` firing `--mode once` per tick (RFC 0011 §9); the
> built-in `--interval` is a standalone convenience.

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

`--run-id` / `AGENTD_RUN_ID` is the idempotency key propagated into every
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
    --intelligence unix:/run/intel.sock \
    --mcp queue='mcp-server-queue --addr /run/q.sock' \
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
    --intelligence unix:/run/intel.sock \
    --mode reactive --subscribe 'file:///data/in.json' \
    --drain-timeout 20s
```

A **second** `SIGTERM`/`SIGINT` forces an immediate `SIGKILL` of all process
groups. `SIGHUP`/reload is dropped — config is a frozen, validated snapshot;
restart to reconfigure (RFC 0011 §4.1).

---

## 10. Observability of config

On startup agentd validates and emits structured
JSON-lines telemetry on stderr; the credential is always redacted. Example
shapes:

```json
{"level":"error","event":"config.invalid","reason":"missing intelligence endpoint (AGENTD_INTELLIGENCE or --intelligence)"}
```

```json
{"level":"info","event":"config.loaded","max_steps":50,"max_tokens":200000,"deadline_ms":600000,"max_depth":4,"enable_exec":false,"log_content":false,"serve_mcp":false,"intel_scheme":"unix","instruction_len":42}
```

Content-capture stays **off**: `config.loaded` reports the instruction as a
length and the intelligence endpoint as a scheme only — never the instruction
body or the credential. The exact log schema is owned by RFC 0010 (see
[`observability.md`](observability.md)).

---

## 11. A complete example

```console
$ agentd \
    --instruction-file /etc/agentd/task.txt \
    --intelligence https://llm.internal/v1 \
    --intelligence-token "$LLM_KEY" \
    --model my-model \
    --mcp fs='mcp-server-fs --root /data' \
    --mcp queue='mcp-server-queue --addr /run/q.sock' \
    --mode once \
    --max-steps 80 --max-tokens 150000 --deadline 5m \
    --max-depth 3 \
    --run-id "$JOB_NAME" \
    --drain-timeout 20s \
    --log-level info \
    --health-file /run/agentd/health
```

Equivalent settings via environment (for the env-backed keys), with flags only
where there is no env equivalent:

```console
$ export INSTRUCTION="$(cat /etc/agentd/task.txt)"
$ export AGENTD_INTELLIGENCE=https://llm.internal/v1
$ export AGENTD_INTELLIGENCE_TOKEN="$LLM_KEY"
$ export AGENTD_MODEL=my-model
$ export AGENTD_MODE=once
$ export AGENTD_MAX_STEPS=80
$ export AGENTD_MAX_TOKENS=150000
$ export AGENTD_DEADLINE=5m
$ export AGENTD_RUN_ID="$JOB_NAME"
$ export AGENTD_DRAIN_TIMEOUT=20s
$ export AGENTD_LOG_LEVEL=info
$ agentd \
    --mcp fs='mcp-server-fs --root /data' \
    --mcp queue='mcp-server-queue --addr /run/q.sock' \
    --max-depth 3 \
    --health-file /run/agentd/health
```

(`--mcp`, `--max-depth`, and `--health-file` are flag-only in v1.)
