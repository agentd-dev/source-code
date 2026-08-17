# agentd

**A minimal, MCP-native, cloud-native AI agent runtime.** One small static Rust
binary runs **one agent**: hand it an instruction and one LLM endpoint, and it
runs the agentic loop — think, call a tool, observe, repeat — until the task
reaches a terminal status or a new event wakes it. Every tool comes from a
**remote MCP server** over HTTPS (agentd ships none of its own; local execution
is off unless you compile *and* enable the guarded `exec` runner), it reacts to
the world through **MCP resource subscriptions**, speaks **A2A** to other
agents, and can drive **durable DAG workflows**. It is built to be a
cloud-native unit of work — drop it into a `Job`, a `CronJob`, or a long-lived
A2A `Deployment` — and when you want to *work with* it, attach a terminal or a
browser: `agentd tui -c agent.yaml`.

```
binary 3.0 MiB static (musl, FROM scratch) · image ~1.2 MiB pull · cold start <1 ms
idle daemon ~2 MiB RSS · 3 direct external deps · HTTPS everywhere · AGPL-3.0
```

- [Why agentd](#why-agentd)
- [How it works](#how-it-works)
- [Install](#install)
- [Quickstart](#quickstart)
- [Talk to it — TUI & web UI](#talk-to-it--tui--web-ui)
- [Lifecycle & triggers](#lifecycle--triggers)
- [Workflows](#workflows)
- [Embedding — the engine in your app](#embedding--the-engine-in-your-app)
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
3. **Reactivity via resource subscriptions.** Instead of polling, an agentd with
   a `subscribe` start node **idles at near-zero CPU and wakes when an MCP
   resource it subscribed to changes** (notify-then-read). An upstream change is
   the trigger; a workflow can also schedule its own future wakes (`loop`,
   `schedule`).
4. **Two loops, strictly separated.** A tiny **supervisor** owns lifecycle,
   triggers, limits, and the kill ladder — and **never talks to the LLM**. The
   reasoning lives in **subagent child processes** (the same binary, re-exec'd)
   the supervisor can always `SIGKILL`. A runaway or crashing model is contained
   by construction; limits are enforced by a process that cannot be prompted.
5. **Composability, three ways.** An agentd **serves an A2A endpoint**
   (`a2a.listen`, RFC 0029), so one agent is just another agent a second one
   sends messages/commands to. It **delegates over A2A** (`a2a.peers`) to remote
   agents as spec-conformant Tasks. And it **nests subagents** as an OS process tree
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

**Installer** — detects your architecture, verifies the release `SHA256SUMS`,
installs to `/usr/local/bin` (or `~/.local/bin`), and never invokes sudo:

```console
$ curl -fsSL https://agentd.dev/install.sh | sh
```

**Release binaries** (static musl, amd64 + arm64) if you would rather do it by
hand — `install.sh --help` lists the pinning and directory options:

```console
$ TAG=$(curl -fsSL https://api.github.com/repos/agentd-dev/source-code/releases/latest | grep -m1 tag_name | cut -d'"' -f4)
$ curl -LO https://github.com/agentd-dev/source-code/releases/download/$TAG/agentd-$TAG-x86_64-unknown-linux-musl.tar.gz
$ tar xzf agentd-$TAG-x86_64-unknown-linux-musl.tar.gz && ./agentd --version
```

**Container image** (multi-arch, cosign-signed, single layer, ~1.2 MiB pull):

```console
$ docker run --rm ghcr.io/agentd-dev/agentd:latest --capabilities
```

**From source** (Rust stable; no C toolchain needed). Features are compile-time,
so `--capabilities` tells you what a given binary can actually do:

```console
$ cargo build -p agentd-cli --release
$ cargo build -p agentd-cli --release \
    --features "a2a,metrics,cron,otel,hot-reload,config-watch,aauth,oauth"   # the shipped set
$ cargo build -p agentd-cli --release --features a2a,exec   # + the local command runner
```

`exec` and `cel` are deliberately absent from release binaries — the first
because [running local commands is opt-in twice over](docs/security.md), the
second because it is the one dependency-bearing feature.

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

## Talk to it — TUI & web UI

agentd hosts the state; the clients are thin. One command runs the daemon and a
terminal UI together:

```console
$ agentd tui --config agent.yaml     # or: agentd ui -c agent.yaml (browser)
```

```
agentd 2.0.0 · prod-agent                        chat  tasks  subagents  debug
you › Deploy api-gateway to staging
agent › Deploy checks passed. Rolling out v2.4.1 — 3 pods cycling, ETA 90s.
⣾ read_file · 3s · 1.2k tok
● live http://127.0.0.1:8420 · 1 turns · 33/17 tok
```

Because the daemon owns the session, **several surfaces watch the same one at
once** — a terminal at your desk, a browser on another screen, a colleague's
machine paired with a rotating code — and quitting a client leaves the agent
working. Approvals (`ask_human`) render as answerable rows in every attached
client and survive a restart; a debug mode exposes the live event feed,
per-step run detail and the log tail when you ask for it.

Both clients are separate Node projects under [`interface/`](interface) built
on a shared thin-client core, so a third client is a small program. See
**[docs/interface.md](docs/interface.md)**, and
**[docs/coding-agent.md](docs/coding-agent.md)** to set one up as a
pair-programming agent for a repository.

## Lifecycle & triggers

agentd 2.0 has **one durable runtime** — no modes. A run is either a one-shot
**job** or a long-lived **daemon** (`lifecycle.run_until`), and what *triggers*
runs is a workflow **start node**.

```console
# a job (the quickstart): the --instruction sugar expands to a
# `once → agent → finish` workflow; run one turn, map the outcome to an exit
# code, then exit.
$ agentd --instruction "…" --intelligence https://gw.example/v1
```

Recurring / reactive shapes are **workflow start nodes** in a
`config_version: "2"` document (see
[docs/modes-and-triggers.md](docs/modes-and-triggers.md)):

```yaml
config_version: "2"
intelligence: { endpoints: https://gw.example/v1, model: gpt-… }
store: { kind: mcp, mcp: { server: state } }         # a daemon needs a durable store
a2a:   { listen: https://0.0.0.0:8443, tls: { cert: …, key: … } }   # the external channel
workflows:
  - name: watch
    steps:
      s:  { kind: subscribe, server: queue, uri: "queue://inbox" }  # loop|schedule|subscribe|signal|event
      do: { kind: agent, depends_on: [s], instruction: "Handle the item." }
      f:  { kind: finish, depends_on: [do] }
lifecycle: { run_until: drained }                    # a daemon
```

The 1.x `--mode once|loop|reactive|schedule|workflow` flags (and the flat v1
schema) were removed; a 1.x configuration is rejected with a migration hint.
`--traceparent` still continues an upstream W3C trace.

## Workflows

agentd runs **durable DAG workflows** (RFC 0027, always compiled — no feature
flag): a declarative graph of `steps` in the `config_version: "2"` document,
driven by the same reactor over durable state, so a run survives a restart and
resumes exactly where it died. Deterministic steps (`assign` / `map` / `filter` /
`switch` / …) cost **zero model tokens**; `agent` / `think` steps run turn
workers:

```yaml
workflows:
  - name: process
    steps:
      s:     { kind: once }
      fetch: { kind: agent, depends_on: [s], instruction: "fetch the next item", writes: item }
      route: { kind: switch, depends_on: [fetch], on: "{{vars.item.status}}",
               cases: { pending: [work] }, default: [done] }
      work:  { kind: mcp.tool, depends_on: [route], server: fs, tool: process,
               args: { id: "{{vars.item.id}}" } }
      done:  { kind: finish, depends_on: [work] }
```

- **A rich node catalogue** (RFC 0027 §5): `agent` / `think` (turn workers),
  `mcp.tool` (direct MCP), data steps (`assign` / `map` / `filter` / `reduce` /
  `sort` / `parse`), `switch` routing, nested bodies (`foreach` / `batch` with
  bounded parallelism + `rate` pacing, `iterate`, `parallel`, `race`, `subgraph`),
  orchestration (`wait` on resource / condition / signal / run / subagent /
  message / deadline, `join`, child `workflow` runs, `subagent`, `human` gates
  that project A2A `input-required`, `a2a.delegate`), and `finish`.
- **Variables + templates** thread data between steps (`writes` /
  `{{vars.…}}` / `{{steps.x.output}}` / `CEL:` expressions); large step outputs
  spill to durable artifacts and dereference transparently.
- **Durable + crash-resumable:** every step's progress is a durable envelope in
  the remote store (RFC 0025) — a run restores and resumes exactly where it died
  (proven by the chaos-matrix e2e), with idempotency keys so at-least-once
  effects run once. No database is linked — the store is behind MCP tools or HTTP.
- **Triggers are start nodes** (`once` / `loop` / `schedule` / `subscribe` /
  `signal` / `event` / `manual` / `a2a`) — the recurring/reactive shapes,
  durable across restarts.
- **Bounded by construction:** step / token / deadline budgets, concurrency
  policies (`queue` / `drop` / `replace`), and per-node caps — each terminal
  with a distinct `status` / `reason`.
- **Optional CEL** (`--features cel`): `CEL:` step conditions, computed values,
  and data-step element expressions; a non-CEL build fails those closed.

See [docs/workflows.md](docs/workflows.md).

## Embedding — the engine in your app

agentd is also **a library**: the binary is a thin shell (`agentd-cli`) over
the published engine crate (`agentd-core`, lib name `agentd`). Any Rust app can
run agentic logic as a **function call**, with **native Rust tools** the model
calls alongside MCP tools:

```rust
// 1. Register your code as a tool (it joins the model's catalogue — and wins
//    name collisions with remote servers; first-party is unstealable).
agentd::tools::register(agentd::tools::CodeTool::new(
    "word_count", "Count the words in a text.",
    json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
    |args| Ok(json!({ "words": args["text"].as_str().unwrap_or("").split_whitespace().count() })),
))?;

// 2. One agentic run, as a call — Outcome + token Usage back as plain values.
let intel = IntelClient::from_parts("https://gw.example/v1", token)?;
let (outcome, usage) = run_loop(&intel, &mcp_servers, &LoopInput {
    instruction: "Count the words in this review, then summarize it.".into(),
    output_contract: Some("JSON: {words, summary}".into()),
    model: "my-model".into(), max_steps: 10, max_tokens: 20_000,
    deadline: Instant::now() + Duration::from_secs(120),
    seed: vec![], cancel: None,
}, &mut NoSelfTools, &log)?;
```

Workflows embed the same way — author a dialect-2 graph as data, `drive()` it
with your own executor, and code tools are addressable from `tool` nodes as the
reserved server **`code`**. A compile-guaranteed example ships in-tree:
[`embedded-agent.rs`](crates/agentd/examples/embedded-agent.rs) — the loop
called directly from a host app, with a code-registered tool the model can call
into mid-reasoning. The **stock CLI registers nothing**
— its no-local-code posture holds by construction. Reusable on their own:
`agentd-mcp` (MCP client/server + wire) and `agentd-net` (transports). Recipes,
the embedder obligations (the re-exec dispatch!), and the API-stability tiers:
[docs/embedding.md](docs/embedding.md) + RFC 0022.

## Composition: serving, subagents, A2A

**Serve your agent over A2A** (RFC 0029, `--features a2a`) — set `a2a.listen` and
peers call `SendMessage` (natural language → a conversation turn, or a command
DataPart → a registry action like `status` / `workflow.run` / `config`),
`GetTask` / `ListTasks` / `CancelTask`, and `SendStreamingMessage` (SSE) on the
listener, each resolved to a **principal** (mTLS / bearer → `operator` / `user` /
`agent` / `anonymous`) and authorized against a role matrix, and (optionally)
audited:

```yaml
a2a:
  listen: https://0.0.0.0:8443
  tls: { cert: tls.crt, key: tls.key, client_ca: clients.crt }   # and/or a bearer:
  bearer: "{{secret:A2A_BEARER}}"
  principals:
    - match: { san: "spiffe://team/*" }
      role: user
      grants: [knowledge.*]
```

**Delegate over A2A** — a workflow `a2a.delegate` step (or a subagent) calls a
declared peer as a spec-conformant Task:

```yaml
a2a:
  peers:
    - name: research
      endpoint: https://research-agent.internal:8443
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

### AAuth [draft] — signed agent identity

Calling an MCP server protected by **AAuth**? Build with `--features aauth` and
agentd gets an **Ed25519 identity**, an agent token from an **Agent Provider**,
and **signs every MCP request** (RFC 9421) — no shared API key, and the server
knows exactly which agent is calling:

```console
$ agentd --instruction "…" --intelligence https://gw.example/v1 \
    --mcp secure=https://mcp.secure.example/mcp \
    --aauth-provider https://apd.example --aauth-enroll-token '{{secret:ENROLL}}'
```

The token is fetched, cached, and refreshed automatically; the whole subagent
tree signs under one identity. Draft support (Case A end-to-end); ships
build-from-source, like CEL. See [docs/aauth.md](docs/aauth.md) + RFC 0023.

## Operating it

**Exit codes are the contract** (RFC 0011): `0` completed · `1` crash · `2`
config/usage (fails in ms, pre-LLM) · `3` stalled/partial · `4` intelligence
unavailable · `5` refused · `6` required MCP server down · `7` budget/deadline
exhausted · `124` supervisor hard-kill backstop · `137`/`143` external kills. A
clean drain is always `0`, never `143`. Policy codes (`3`/`7`) can be remapped with `--budget-exit-code` for
schedulers that treat nonzero as retry-forever.

**Telemetry:** JSON-lines on stderr (trace-correlated, `--log-level`),
optional `--report-file` run-outcome report (atomic write), Prometheus
`/metrics` + `/healthz` + `/readyz` via `--metrics-addr` (`--features
metrics`), OTLP spans with GenAI semconv via `--features otel`, a liveness
heartbeat file via `--health-file`, and the `agent://events` live ring when
serving (`--features events`).

**Discovery:** `agentd --capabilities` prints a machine-readable manifest
(`contract_version: "1.0"` + a `surfaces{}` block of exactly what's compiled
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
| `a2a` | the A2A v2 HTTPS listener + outbound delegation peers (RFC 0029) | — |
| `cel` | CEL step conditions / computed values / data-step expressions | `cel-interpreter` (the one exception) |
| `otel` | OTLP traces + logs export (hand-rolled JSON) | — |
| `metrics` | hand-written Prometheus text + health endpoints | — |
| `otel` | hand-rolled OTLP/HTTP span export, GenAI semconv | — |
| `cron` | 5-field UTC cron scheduling (hand-rolled parser) | — |
| `cluster` | sharding, claim/lease, standby pools, capacity signal | — |
| `oauth` | OAuth 2.1 client-credentials for remote endpoints | — |
| `hot-reload` / `config-watch` | SIGHUP / inotify restart-free reconfig | — |

Shipped release feature set:
`serve-https,a2a,events,metrics,cron,otel,cluster,hot-reload,config-watch,workflow`.

## Footprint (measured)

Measured on the v1.0.0 release build (x86_64, musl, stripped):

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
  [interface (TUI/web UI)](docs/interface.md) ·
  [coding agent](docs/coding-agent.md) ·
  [workflows](docs/workflows.md) · [subagents](docs/subagents.md) ·
  [intelligence](docs/intelligence.md) · [security](docs/security.md) ·
  [observability](docs/observability.md) · [operations](docs/operations.md) ·
  [deployment](docs/deployment.md) · [scaling](docs/scaling.md) ·
  [use cases](docs/use-cases.md)
- **[rfcs/README.md](rfcs/README.md)** — the normative specifications
  (RFC 0001–0022; RFC 0001 is the narrative front door).
- **[examples/SAMPLES.md](examples/SAMPLES.md)** — runnable samples: a coding
  agent (`coding-agent.yaml`), Docker Compose, Kubernetes
  `Job`/`CronJob`/`Deployment` manifests, a systemd unit.
- **[skills/](skills/README.md)** — an Agent Skill that teaches an AI coding
  assistant to install, configure and debug agentd (drop it in `~/.claude/skills/`).
- **[SECURITY.md](SECURITY.md)** — what counts as a vulnerability here, and how
  to report one privately.
- **[CONTRIBUTING.md](CONTRIBUTING.md)** — build, test and review expectations.
- **[CHANGELOG.md](CHANGELOG.md)** — release history.
- **Website:** [agentd.dev](https://agentd.dev) — rendered docs + RFCs.

## License

AGPL-3.0-only — see [LICENSE](LICENSE). agentd is fully open source; commercial licensing (for proprietary embedding or AGPL-free use) and commercial support are available — contact agent@agentd.dev.
