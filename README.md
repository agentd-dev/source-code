# agentd

**A minimal, MCP-native, cloud-native AI agent runtime.** One small static Rust
binary runs **one agent**: hand it an instruction and one LLM endpoint, and it
runs the agentic loop — think, call a tool, observe, repeat — until the task
reaches a terminal status or a new event wakes it. Every tool comes from a
**remote MCP server** over HTTPS (agentd ships none of its own and **never runs
local code**), it reacts to the world through **MCP resource subscriptions**,
speaks **A2A** to other agents, and can drive **agent-authored workflows**. It
is built to be a cloud-native unit of work — drop it into a `Job`, a `CronJob`,
or a long-lived reactive `Deployment`.

```
binary 3.0 MiB static (musl, FROM scratch) · image ~1.2 MiB pull · cold start <1 ms
idle daemon ~2 MiB RSS · 3 direct external deps · HTTPS everywhere · Apache-2.0
```

- [Why agentd](#why-agentd)
- [How it works](#how-it-works)
- [Install](#install)
- [Quickstart](#quickstart)
- [Five modes](#five-modes)
- [Workflows](#workflows)
- [Composition: serving, subagents, A2A](#composition-serving-subagents-a2a)
- [Security model](#security-model)
- [Operating it](#operating-it)
- [Scaling out](#scaling-out)
- [Build features](#build-features)
- [Footprint (measured)](#footprint-measured)
- [Documentation map](#documentation-map)

## Why agentd

1. **Minimalism as the moat.** Three direct external dependencies (`serde`,
   `serde_json`, `libc`) — no async runtime, no framework, no C toolchain. The
   HTTP client/server, cron parser, Prometheus text, OTLP export, and inotify
   watch are all hand-rolled on `std` + `libc`. The result is a 3 MiB static
   binary that starts in under a millisecond, idles at ~2 MiB, and ships as a
   single-layer `FROM scratch` image with nothing to CVE-scan but agentd itself.
2. **MCP as the universal interface.** agentd has no built-in `fs`/`http`/`shell`
   tool library and executes nothing locally. Every capability is a **remote MCP
   server** you declare with `--mcp name=https://…`. One protocol in, one
   protocol out — tools and resources are all MCP, and agentd itself is
   addressable as an MCP server.
3. **Reactivity via resource subscriptions.** Instead of polling, a reactive
   agentd **idles at near-zero CPU and wakes when an MCP resource it subscribed
   to changes** (notify-then-read). An upstream change is the trigger; an agent
   can even subscribe itself mid-reasoning to schedule its own future wake.
4. **Two loops, strictly separated.** A tiny **supervisor** owns lifecycle,
   triggers, limits, and the kill ladder — and **never talks to the LLM**. The
   reasoning lives in **subagent child processes** (the same binary, re-exec'd)
   the supervisor can always `SIGKILL`. A runaway or crashing model is contained
   by construction; limits are enforced by a process that cannot be prompted.
5. **Composability, three ways.** An agentd **serves its own MCP**
   (`--serve-mcp`), so one agent is just another tool/resource surface a second
   agent connects to. It **delegates over A2A** (`--a2a-peer`) to remote agents
   as spec-conformant Tasks. And it **nests subagents** as an OS process tree
   with narrowed, per-child context and trust. Agents compose like Unix
   processes.

## How it works

```
              triggers: interval · cron · MCP resource change · A2A request
                                        │
       ┌────────────────────────────────▼─────────────────────────────────┐
       │  supervisor (never talks to the LLM)                             │
       │  config → validate → trifecta gate → mode driver → kill ladder   │
       │  limits: steps · tokens · deadline · depth · cgroup mem/pids     │
       └────────────┬─────────────────────────────────────┬───────────────┘
                    │ spawn (re-exec, narrowed payload)    │ serve (optional)
       ┌────────────▼────────────────┐        ┌────────────▼───────────────┐
       │  subagent (agentic loop)    │        │  self-MCP over HTTP(S)     │
       │  think → tool → observe …   │        │  tools · agent:// resources│
       │  or: workflow driver        │        │  A2A Tasks · operator ctl  │
       └──────┬───────────┬──────────┘        └────────────────────────────┘
              │ HTTPS     │ HTTPS
       ┌──────▼─────┐ ┌───▼──────────────┐
       │ intelligence│ │ MCP servers      │
       │ (one LLM    │ │ --mcp a=https://…│
       │  endpoint,  │ │ --mcp b=https://…│
       │  failover)  │ │  tools+resources │
       └────────────┘ └──────────────────┘
```

Every network edge is HTTP(S) — the LLM, the MCP servers, the served self-MCP,
A2A peers, and operator control — with mTLS and/or bearer auth (plaintext
`http://` is loopback-only, for dev). agentd links no unix/vsock/stdio
transport and spawns no tool processes.

## Install

**Release binaries** (static musl, amd64 + arm64, with `SHA256SUMS`):

```console
$ curl -LO https://github.com/agentd-dev/source-code/releases/download/v2.1.0/agentd-v2.1.0-x86_64-unknown-linux-musl.tar.gz
$ tar xzf agentd-v2.1.0-x86_64-unknown-linux-musl.tar.gz && ./agentd --version
agentd 2.1.0
```

**Container image** (multi-arch, cosign-signed, single layer, ~1.2 MiB pull):

```console
$ docker run --rm ghcr.io/agentd-dev/agentd:2.1.0 --capabilities
```

**From source** (Rust stable; no C toolchain needed):

```console
$ cargo build -p agentd --release
$ cargo build -p agentd --release \
    --features "serve-https,a2a,events,metrics,cron,otel,cluster,hot-reload,config-watch,workflow"   # the shipped set
```

## Quickstart

```console
# one-shot: instruction + one LLM endpoint + one MCP server, then exit
$ agentd \
    --instruction "Read /data/report.md and write a 3-bullet summary to /data/summary.md" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp
```

stdout carries the result; stderr carries JSON-lines telemetry (one structured
event per line, trace-correlated); the exit code maps the terminal status. Bad
config exits `2` in milliseconds, **before** any LLM round-trip — and
`--validate-config` checks a config without running anything. The intelligence
endpoint speaks the OpenAI-compatible wire with native tool-calling; a
comma-list of endpoints is a failover order. See
[docs/getting-started.md](docs/getting-started.md).

## Five modes

```console
# once (default): run to a terminal status, then exit — Job / CLI shape
$ agentd --instruction "…" --intelligence https://gw.example/v1 --mode once

# loop: re-enter on a cadence until a bound or a drain signal
$ agentd --instruction "…" --intelligence https://gw.example/v1 \
    --mode loop --interval 5m --deadline 24h

# reactive: idle, wake on MCP resource changes (requires ≥1 --subscribe)
$ agentd --instruction "…" --intelligence https://gw.example/v1 \
    --mcp queue=https://mcp-q.internal/mcp \
    --mode reactive --subscribe "queue://inbox"

# schedule: built-in interval/cron timer (--features cron for --cron)
$ agentd --instruction "…" --intelligence https://gw.example/v1 \
    --mode schedule --cron "0 8 * * *"

# workflow: drive a pinned workflow graph, supervised like any run
$ agentd --mode workflow --workflow ./pipeline.json \
    --intelligence https://gw.example/v1 --mcp fs=https://mcp-fs.internal/mcp
```

Reactive extras: `--continue <uri>` routes a subscription into one **warm
session** (context persists across wakes, tool lists refresh live);
`--traceparent` continues an upstream W3C trace. See
[docs/modes-and-triggers.md](docs/modes-and-triggers.md).

## Workflows

With `--features workflow`, agentd runs **agent-authored cyclic workflows**: a
serde-only graph the model authors for itself at runtime (`workflow.define` /
`workflow.run` self-tools) or an operator pins from a file (`--mode workflow`).
Deterministic steps cost **zero model tokens** — the graph walker measured at
~146k steps/sec:

```json
{
  "start": "fetch",
  "nodes": {
    "fetch":  { "kind": "agent", "instruction": "fetch the next item", "writes": "item",
                "edges": { "ok": "route", "error": "done" } },
    "route":  { "kind": "branch",
                "cases": [ { "when": {"op":"eq","key":"item","pointer":"/status","value":"pending"},
                            "goto": "work" } ],
                "default": "done" },
    "work":   { "kind": "tool", "server": "fs", "tool": "process",
                "args": { "id": { "$from": "item", "pointer": "/id" } },
                "writes": "item", "edges": { "ok": "fetch", "error": "done" } },
    "done":   { "kind": "halt", "status": "completed", "result_from": "item" }
  }
}
```

- **Ten node kinds:** `agent` (a full agentic sub-run), `tool` (direct MCP
  call), `assign` (pure data shaping), `infer` (schema-checked structured
  intelligence), `branch` (JSON-pointer predicates + one semantic tier),
  `foreach` (deterministic fan-out, ≤1024 items, ≤8 parallel lanes), `subgraph`
  (sync or `async: true` → spawned child), `join` (fan-in of async handles),
  `wait`, `halt`.
- **A blackboard** threads data between nodes (`writes` / `reads` /
  `{"$from": …}` JSON-pointer references; 1 MiB per-value clamp).
- **Cycles are legal, runaways are not:** layered termination — step budget,
  shared token pool, wall deadline, per-node visit caps, and a progress guard —
  each with a distinct `reason` in the result.
- **Optional CEL** (`--features cel` — the one gated dependency exception):
  `{"op":"cel"}` predicates, computed `assign.expr`, `infer.check` constraints,
  and reactive wake conditions. Hermetic, terminating, compile-checked at
  define time; a non-CEL build rejects CEL graphs fail-closed.
- **Reactive workflows:** `--mode reactive --workflow` suspends on a `wait`
  node and resumes on the trigger — a durable, event-driven pipeline.

See [docs/workflows.md](docs/workflows.md).

## Composition: serving, subagents, A2A

**Serve your agent as MCP** — tools `status` / `subagent.spawn` / `subagent.send`
/ `subagent.status` / `subagent.cancel`, plus live `agent://` resources
(`status`, `capabilities`, `inventory`, `intelligence`, `events`, `workflow`,
`run/<id>`, `config/effective`):

```console
$ agentd --mode reactive --subscribe "queue://inbox" --instruction "…" \
    --intelligence https://gw.example/v1 \
    --serve-mcp https://0.0.0.0:8443 --serve-cert tls.crt --serve-key tls.key \
    --serve-client-ca clients.crt          # and/or --serve-bearer <token>
```

**Delegate over A2A** (`--features a2a`) — a served run *is* a spec-conformant
A2A Task; peers call `SendMessage` / `GetTask` / `CancelTask` /
`SendStreamingMessage` (SSE) on the same listener, and an agent delegates
outward with `--a2a-peer name=https://…` (bearer and/or mTLS client identity):

```console
$ agentd --instruction "delegate the analysis to the research agent" \
    --intelligence https://gw.example/v1 \
    --a2a-peer research=https://research-agent.internal:8443
```

**Nest subagents** — a parent spawns a child by re-exec'ing the same binary
with a narrowed spawn payload (subset of servers, tighter limits, its own
cgroup). The tree is bounded by `--max-depth` and a spawn-rate token bucket;
every child is one `SIGKILL` from gone.

## Security model

- **No local execution.** There is no `exec`, no shell, no local tool — the
  attack surface of a tool call is the remote MCP server's, not the host's.
- **Rule-of-Two trifecta gate.** Tag servers with
  `--mcp-tags name=untrusted_input,sensitive,egress`; a config that wires all
  three legs into one agent is **refused at startup** unless you explicitly
  `--allow-trifecta`.
- **Authenticated everything.** Outbound: bearer/OAuth 2.1 client-credentials +
  bundled webpki roots (+ `--tls-ca` for private PKI). Inbound: mTLS client CA
  and/or constant-time bearer; **operator verbs require the Management
  identity** — unauthenticated peers can't even see them.
- **Hardened served surface.** Cross-origin requests are rejected (403);
  sessions get unique `Mcp-Session-Id`s; plaintext serving is loopback-only.
- **Secrets discipline.** Tokens come from env or mounted files
  (`--intelligence-token-file` rotates live) and are never logged; telemetry
  logs lengths, not contents, unless you opt in with `--log-content`.
- **Contained blast radius.** Reasoning runs in killable child processes under
  optional per-run cgroups (`--cgroup`, `--cgroup-memory-max`,
  `--cgroup-pids-max`) with atomic `cgroup.kill` teardown.

See [docs/security.md](docs/security.md) and [rfcs/0012](rfcs/0012-security-posture.md).

## Operating it

**Exit codes are the contract** (RFC 0011): `0` completed · `1` crash · `2`
config/usage (fails in ms, pre-LLM) · `3` stalled/partial · `4` intelligence
unavailable · `5` refused · `6` required MCP server down · `7` budget/deadline
exhausted · `137`/`143` external kills. A clean drain is always `0`, never
`143`. Policy codes (`3`/`7`) can be remapped with `--budget-exit-code` for
schedulers that treat nonzero as retry-forever.

**Telemetry:** JSON-lines on stderr (trace-correlated, `--log-level`),
optional `--report-file` run-outcome report (atomic write), Prometheus
`/metrics` + `/healthz` + `/readyz` via `--metrics-addr` (`--features
metrics`), OTLP spans with GenAI semconv via `--features otel`, a liveness
heartbeat file via `--health-file`, and the `agent://events` live ring when
serving (`--features events`).

**Discovery:** `agentd --capabilities` prints a machine-readable manifest
(`contract_version: "2.0"` + a `surfaces{}` block of exactly what's compiled
and configured in) and exits — feature-detect from this, not the version
string.

**Control plane:** a Management-authenticated peer drives the served endpoint
with the `a2a.Drain` / `a2a.LameDuck` / `a2a.Pause` / `a2a.Resume` /
`a2a.Cancel` admin methods. `SIGTERM` starts a graceful drain
(`--drain-timeout` < pod grace).

**Hot reload** (`--features hot-reload`): `SIGHUP` — or a ConfigMap volume
swap with `--watch-config` (`--features config-watch`) — revalidates and
reapplies the reloadable subset (model, limits, log level, subscriptions,
**live MCP server set**) at a quiesce boundary, restart-free.

See [docs/operations.md](docs/operations.md) and [docs/observability.md](docs/observability.md).

## Scaling out

With `--features cluster` (RFC 0019):

- `--shard K/N` — deterministic hash-partitioning of the URI/key space across a
  fleet of identical replicas (works for reactive and timer modes).
- `--claim <uri>=<server>` + `--claim-ttl` — claim/lease an item before
  processing it, so at-least-once event delivery becomes exactly-one-owner
  processing.
- `--standby --assign-from <server>:<uri>` — a warm worker pool that
  claim-pulls assignments.
- `agent://capacity` + Prometheus metrics feed autoscaling.

See [docs/scaling.md](docs/scaling.md).

## Build features

The default build is intentionally small; everything else is opt-in at compile
time. A flag whose feature is absent exits `2` loudly — never a silent no-op.

| Feature | What it adds | Extra deps |
|---|---|---|
| `tls` *(default)* | rustls + ring + bundled roots — direct `https://` everywhere | rustls stack |
| `serve-https` | serve agentd's own MCP over HTTP(S) with mTLS/bearer | — |
| `a2a` | A2A Task surface + outbound delegation peers | — |
| `workflow` | agent-authored cyclic workflows (all ten node kinds) | — |
| `cel` | CEL predicates/expressions in workflows + reactive conditions | `cel-interpreter` (the one exception) |
| `events` | the `agent://events` live ring resource | — |
| `metrics` | hand-written Prometheus text + health endpoints | — |
| `otel` | hand-rolled OTLP/HTTP span export, GenAI semconv | — |
| `cron` | 5-field UTC cron scheduling (hand-rolled parser) | — |
| `cluster` | sharding, claim/lease, standby pools, capacity signal | — |
| `oauth` | OAuth 2.1 client-credentials for remote endpoints | — |
| `hot-reload` / `config-watch` | SIGHUP / inotify restart-free reconfig | — |

Shipped release feature set:
`serve-https,a2a,events,metrics,cron,otel,cluster,hot-reload,config-watch,workflow`.

## Footprint (measured)

Measured on the v2.1.0 release build (x86_64, musl, stripped):

| Metric | Value |
|---|---|
| Binary (static-PIE, runs on `scratch`) | **3.0 MiB** (1.5 MiB gzipped) |
| Container image pull | **~1.2 MiB**, single layer |
| Cold start (`--version` / `--capabilities`) | **< 1 ms** |
| Idle serving daemon RSS | **~2 MiB**, flat under load |
| Served request overhead (`tools/call`, loopback, fresh conn) | **p50 0.26 ms** |
| Deterministic workflow steps | **~146k steps/sec** (single lane, 0 model tokens) |
| Direct external dependencies | **3** (`serde`, `serde_json`, `libc`) |

## Documentation map

- **[docs/README.md](docs/README.md)** — the task-oriented guide index:
  [getting started](docs/getting-started.md) ·
  [configuration](docs/configuration.md) ·
  [architecture](docs/architecture.md) · [mcp](docs/mcp.md) ·
  [modes & triggers](docs/modes-and-triggers.md) ·
  [workflows](docs/workflows.md) · [subagents](docs/subagents.md) ·
  [intelligence](docs/intelligence.md) · [security](docs/security.md) ·
  [observability](docs/observability.md) · [operations](docs/operations.md) ·
  [deployment](docs/deployment.md) · [scaling](docs/scaling.md) ·
  [use cases](docs/use-cases.md)
- **[rfcs/README.md](rfcs/README.md)** — the normative specifications
  (RFC 0001–0020; RFC 0001 is the narrative front door).
- **[examples/SAMPLES.md](examples/SAMPLES.md)** — runnable samples: shell
  one-liners, Docker Compose, Kubernetes `Job`/`CronJob`/`Deployment`
  manifests, a systemd unit.
- **[CHANGELOG.md](CHANGELOG.md)** — release history (v2.1.0 current).
- **Website:** [agentd.dev](https://agentd.dev) — rendered docs + RFCs.

## License

Apache-2.0 — see [LICENSE](LICENSE).
