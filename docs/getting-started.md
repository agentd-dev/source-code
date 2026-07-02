# Getting started with agentd

`agentd` is a small, dependency-light Rust binary that runs **one agent**. You
give it an instruction and a way to reach an LLM, and it runs an agentic loop —
think, call tools, observe, repeat — until the job is done or a new event wakes
it. Every task tool it can call comes from an **MCP server**; agentd ships none of its
own and runs no local code. It reaches exactly **one** LLM endpoint, the
*intelligence*. And it reacts to the world through **MCP resource
subscriptions** — a resource changing upstream is what triggers a run.

This page gets you from a checkout to a first end-to-end run, then shows the same
instruction in `loop` and `reactive` modes. For the full knob list see
[configuration.md](configuration.md); for how triggers and modes work in depth
see [modes-and-triggers.md](modes-and-triggers.md). The architecture is in
[RFC 0001](../rfcs/0001-mcp-native-agent-runtime.md).

> **Build status.** The agent runtime is fully implemented — config
> validation, the agentic loop, the supervisor + subagent process tree, the
> MCP client, the intelligence client, and all four run modes. The examples on
> this page run as written.

---

## Install / build

agentd is a single Cargo crate in a workspace. The default build is
dependency-light: no async runtime, no TLS, no C/C++ toolchain.

```console
$ git clone <repo> agent && cd agent
$ cargo build -p agentd --release
   Compiling agentd v2.0.0
    Finished `release` profile [optimized] target(s)
$ ./target/release/agentd --version
agentd 2.0.0
```

The result is **one static binary** that starts fast, idles cheaply, and drops
into a container or a VM. The same binary is also the subagent: when a parent
spawns a child, it re-execs `argv[0]` in subagent mode — there is no second
artifact to ship.

### Optional features

The default build links `tls` (the `https://` transport with bundled roots) — the
only transport agentd uses. Turn on the rest only when you need them (each is gated
so it never weighs down a minimal build):

```console
$ cargo build -p agentd --release                                 # default: tls (https)
$ cargo build -p agentd --release --features serve-https,a2a      # served self-MCP + A2A
$ cargo build -p agentd --release --features serve-https,cluster,run-graph
```

To keep TLS out of the binary entirely, terminate it at a same-host sidecar and
point agentd at it over a **loopback `http://`** endpoint (`--no-default-features`).

### Minimal container

The binary needs nothing but libc (or build fully static for `FROM scratch`).
A minimal image is just the binary — MCP servers are **remote HTTP endpoints**, so
they are not bundled into the agentd image:

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build -p agentd --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/agentd /usr/local/bin/agentd
ENTRYPOINT ["agentd"]
```

All configuration is env-settable (12-factor), so the container takes its
instruction, intelligence endpoint, and MCP servers entirely from the
environment — see [configuration.md](configuration.md).

---

## The 60-second mental model

Two loops, deliberately separated:

```
  agentd (main process) = SUPERVISOR       ── never talks to the LLM
    • parse + validate config (exits 2 on bad config, before any side effect)
    • connect declared MCP servers (as a CLIENT) ── this is where ALL tools come from
    • arm the trigger: once | loop | reactive | schedule
    • subscribe to MCP resources; idle in recv_timeout until something happens
    • spawn + supervise subagent child processes; reap, kill, restart
        │ spawn (OS process tree)
        ▼
  subagent (child process) = the AGENTIC LOOP   ── where intelligence lives
    think → call MCP tool → observe → … → terminal status, return a result
    (may spawn its own children → agents nest as a process tree)
```

Three facts are the whole design:

1. **The supervisor never reasons.** It owns lifecycle, triggers, the process
   tree, and limits. It has no LLM dependency, so it stays tiny and robust; a
   runaway or crashing model is always isolated in a child the supervisor can
   `SIGKILL`.
2. **MCP is the only tool source.** agentd ships no `fs`/`http`/`shell` tool
   library and runs no local code. Want a capability? Connect an MCP server with
   `--mcp`. Its only built-in tools are its self/control primitives (spawn a
   subagent, subscribe, run a graph).
3. **One intelligence endpoint.** A single LLM endpoint named by a URI in
   `--intelligence` — `https://` (or a loopback `http://` for a same-host dev
   gateway). This is the LLM wire, not MCP; the two are different channels.

Output discipline: **stdout carries the agent's result; stderr carries
JSON-lines telemetry.** This holds for a one-shot run and is the convention every
example below relies on.

---

## A first one-shot run, end to end

The default mode is `once`: run the instruction to a terminal status, print the
result on stdout, exit. Here we give the agent a filesystem MCP server and ask it
to do something with a file.

```console
$ agentd \
    --instruction "Read /data/report.md and write a 3-bullet summary to /data/summary.md" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp
```

Three things are wired here:

- **`--instruction`** — the task. (Use `--instruction-file <path>` to read it
  from a file, or set the `INSTRUCTION` env var.)
- **`--intelligence https://gw.example/v1`** — the LLM endpoint. A direct provider
  is `--intelligence https://api.openai.com/v1/...` with `--intelligence-token`; a
  same-host gateway sidecar is `--intelligence http://127.0.0.1:4000/v1` (loopback
  `http://` is the only plaintext allowed). Any other scheme — or a non-loopback
  `http://` — is rejected at startup with exit 2.
- **`--mcp fs=https://mcp-fs.internal/mcp`** — declare an MCP server named `fs`. The
  value is `name=<endpoint>`; agentd connects to that **remote Streamable-HTTP MCP
  endpoint** (it spawns no process) and discovers its tools via `tools/list`. Repeat
  `--mcp` for more servers; declare per-server auth headers in the config file.

### Read the telemetry (stderr) and the result (stdout)

On stderr you get one JSON object per line. The run threads a
`proc.start`, the loop's tool calls, and a terminal `proc.exit` — all stamped
with the same `run_id`, `agent_id`, `agent_path`, and `comp` correlation tuple:

```jsonc
{"ts":"2026-06-25T11:18:02.796Z","level":"info","event":"proc.start","run_id":"19efe80512c1a9184","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":1741188,"version":"2.0.0","mode":"once","mcp_servers":1,"subscribe":0}
{"ts":"...","level":"info","event":"mcp.connect","run_id":"19efe80512c1a9184","agent_id":"sup","agent_path":"0","comp":"mcp","server":"fs"}
{"ts":"...","level":"info","event":"tool.call","run_id":"19efe80512c1a9184","agent_id":"a1","agent_path":"0.1","comp":"agent","server":"fs","tool":"read_file"}
{"ts":"...","level":"info","event":"tool.call","run_id":"19efe80512c1a9184","agent_id":"a1","agent_path":"0.1","comp":"agent","server":"fs","tool":"write_file"}
{"ts":"...","level":"info","event":"proc.exit","run_id":"19efe80512c1a9184","agent_id":"sup","agent_path":"0","comp":"supervisor","status":"completed","code":0}
```

`agent_path` is the cheap subtree-query trick: it is the agent's position in the
process tree (`0` = supervisor, `0.1` = first child), so filtering logs by an
`agent_path` prefix selects a whole subtree with no backend join. Secrets never
appear — the intelligence token prints as `***` and is kept out of every log line
and the model transcript.

On **stdout** you get just the distilled result:

```console
Wrote /data/summary.md (3 bullets). Source: /data/report.md (1,840 words).
```

The exit code is the agent's terminal status mapped to a number, so a script or
an external scheduler can branch on it:

| Terminal status | Exit code |
|---|---|
| `completed` | 0 |
| partial result usable | 3 |
| intelligence unreachable / auth failed | 4 |
| `refused` | 5 |
| a required MCP server is down | 6 |
| budget hit (`exhausted_steps`/`exhausted_tokens`/`deadline`) | 7 |
| supervisor hard-kill backstop (a child that won't self-terminate) | 124 |
| bad config (validation) | 2 |

Every run is bounded by limits you can tune — `--max-steps` (default 50),
`--max-tokens` (default 200000), and `--deadline` (default 600s) — so a confused
or runaway loop can never burn unbounded cost. See
[configuration.md](configuration.md) for the full list.

### Status: what runs today

The runtime is fully implemented and runs the command above end to end:
`--help` and `--version` exit 0; invalid config exits **2** in milliseconds with
an `agentd: …` message on stderr; valid config parses, logs `proc.start`, runs
the agentic loop, and exits on the agent's terminal status (see the exit-code
table above).

---

## The same instruction in `loop` mode

`loop` re-enters the agent on a timer or after each completion — the shape for a
polling or continuously-working agent. It is the *same* supervisor and *same*
inner loop as `once`; only the exit predicate differs. It stops on a bound (max
iterations / wall-clock deadline / tree-wide token ceiling) or a `SIGTERM`.

```console
$ agentd \
    --instruction "Check /data/inbox for new files; process each into /data/done" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp \
    --mode loop \
    --interval 5m \
    --deadline 24h
```

- **`--interval 5m`** sets the re-entry cadence: re-run every 5 minutes.
  `--interval 0` re-enters immediately on completion (work-until-done) instead of
  polling.
- **`--deadline 24h`** caps the daemon's lifetime; the token ceiling
  (`--max-tokens`) and a `SIGTERM` are the other ways it stops.

A healthy idle loop (nothing to do) backs off exponentially rather than spinning
hot. This is a `Deployment`-shaped or `Job-with-deadline`-shaped workload.

---

## The same instruction in `reactive` mode

`reactive` is the signature mode: the agentd **idles at near-zero CPU and wakes
when an MCP resource it subscribed to changes**. Instead of polling on a timer,
you subscribe to concrete resource URIs; an upstream change is the trigger.

```console
$ agentd \
    --instruction "When a file appears in the inbox, process it into /data/done" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp \
    --mode reactive \
    --subscribe "file:///data/inbox"
```

- **`--mode reactive` requires at least one `--subscribe`** (validated at
  startup; omitting it exits 2). `--subscribe` is repeatable, one concrete
  resource URI each.
- The supervisor issues MCP `resources/subscribe` for each URI (gated on the
  server advertising `resources.subscribe`), then idles in `recv_timeout`. When
  the server emits `notifications/resources/updated{uri}`, the reactive router
  maps it to exactly one action — spawn a fresh subagent for the event, or
  continue a warm session — and the agentd wakes, re-reads current state, and
  works.

Two facts worth knowing up front, both detailed in
[modes-and-triggers.md](modes-and-triggers.md):

- **Notify-then-read.** The update notification carries only the `{uri}` — no
  diff, no payload. The agentd re-reads the resource on wake to learn what
  changed. Bursts are debounced and coalesced (newest-wins) per route.
- **You can only subscribe to concrete URIs, not templates.** To react to "any
  new row," enumerate concrete URIs via `resources/list` and subscribe per-URI.

An agentd can even subscribe **itself** to a resource mid-reasoning (via the
`subscribe` self-tool) to schedule its own future wake — the capability the
runtime is built around.

> **Scope notes.** Reactivity rides the MCP servers' Streamable-HTTP subscriptions;
> serving agentd's own MCP (`--serve-mcp`) is over HTTP(S) with mTLS/bearer auth
> (loopback `http://` for dev). Subagent spawning defaults to **synchronous**;
> `{async}`/`{detach}` dispositions also ship. Agent-authored cyclic **run-graphs**
> ship under `--features run-graph` ([run-graphs.md](run-graphs.md)). MCP
> tasks/sampling/roots are deferred ([RFC 0013](../rfcs/0013-deferred-v2-surface.md)).

---

## Where to go next

- **[configuration.md](configuration.md)** — every flag and env var, precedence
  (`default < config file < env < flag`), limits, secrets, exit codes.
- **[modes-and-triggers.md](modes-and-triggers.md)** — the four modes as exit
  predicates, reactive routing (exactly-one-owner, spawn-vs-continue,
  debounce/coalesce), self-subscribe, and internal `schedule`/cron.
- **[RFC 0001](../rfcs/0001-mcp-native-agent-runtime.md)** — the architecture
  front door; sub-RFCs 0002–0013 cover each mechanism in depth.
- **[docs/design/PLAN.md](design/PLAN.md)** — the design plan and milestone
  history for the loop, MCP client, and intelligence client.
