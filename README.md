# agentd

**A minimal, MCP-native, reactive agent runtime.** One small Rust binary runs
**one agent**: hand it an instruction and a single LLM endpoint, and it runs an
agentic loop — think, call a tool, observe, repeat — until the task reaches a
terminal status or a new event wakes it. Every tool comes from an **MCP server**
(agentd ships none of its own, save a gated `exec`); it reaches exactly **one**
LLM endpoint; and it reacts to the world through **MCP resource subscriptions**.
It is built to be a cloud-native unit of work — drop it into a `Job`, a
`CronJob`, or a long-lived reactive `Deployment`.

## What makes it different

1. **Minimalism as the moat.** A dependency-light, single static binary —
   no async runtime, no TLS, no C toolchain in the default build. It starts
   fast, idles cheaply, and drops into a container or a microVM. A tiny
   supervisor owns lifecycle and limits and **never talks to the LLM**, so it
   stays robust no matter how the model behaves.
2. **MCP as the universal interface.** agentd has no built-in `fs`/`http`/`shell`
   tool library. Every capability is an MCP server you declare with `--mcp`.
   One protocol in, one protocol out — tools and resources are all MCP.
3. **Reactivity via resource subscriptions.** Instead of polling, an agentd
   **idles at near-zero CPU and wakes when an MCP resource it subscribed to
   changes**. An upstream change is the trigger; an agentd can even subscribe
   itself mid-reasoning to schedule its own future wake.
4. **Composability by being an MCP server.** agentd can serve **its own MCP**
   (`--serve-mcp`), so one agentd is just another tool/resource surface another
   agentd connects to. agentd instances compose like Unix processes.
5. **Process-isolated subagents.** A parent spawns a child by re-exec'ing the
   same binary; agentd instances nest as an OS **process tree**. The reasoning lives in
   children the supervisor can `SIGKILL` — a runaway or crashing model is always
   contained, and a narrowed spawn payload keeps each child's context (and trust)
   scoped.

## Quickstart

```console
$ cargo build -p agentd --release
$ ./target/release/agentd --version
agentd 0.1.0

# one-shot run: instruction + one LLM endpoint + one MCP server, then exit
$ ./target/release/agentd \
    --instruction "Read /data/report.md and write a 3-bullet summary to /data/summary.md" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp
```

stdout carries the agentd's result; stderr carries JSON-lines telemetry; the exit
code maps the terminal status. Bad config exits `2` in milliseconds, before any
LLM round-trip. See [docs/getting-started.md](docs/getting-started.md).

## Three modes

```console
# once (default): run to a terminal status, then exit — Job / CLI shape
$ agentd --instruction "..." --intelligence https://gw.example/v1 --mode once

# loop: re-enter on a cadence until a bound or a drain signal
$ agentd --instruction "..." --intelligence https://gw.example/v1 \
    --mode loop --interval 5m --deadline 24h

# reactive: idle, wake on MCP resource changes (requires >=1 --subscribe)
$ agentd --instruction "..." --intelligence https://gw.example/v1 \
    --mode reactive --subscribe "file:///data/inbox"
```

(There is also a built-in `--mode schedule --interval <dur>`; for production cron
prefer an external scheduler firing `--mode once`.)

## Status

agentd is **implemented and shipped.** Config parse + validate, exit codes,
JSON-lines logging, signal handling, the supervisor reactor, the MCP client, the
intelligence client, the agentic loop, all four run modes, the reactive router,
subagents (sync + async/detach), and the served self-MCP all run today. Every
network surface is HTTPS (intelligence, the MCP client, the served self-MCP, A2A,
and operator control — mTLS/bearer auth, loopback `http://` for dev); agentd links
no unix/vsock transport. Agent-authored cyclic **run-graphs** ship under
`--features run-graph`. The default build holds a 3-dependency minimalism moat;
`serve-https`/`a2a`/`cron`/`metrics`/`otel`/`cluster`/`run-graph` are feature-gated.
See **[docs/design/00-target-vision-pivot.md](docs/design/00-target-vision-pivot.md)**.

## Links

- **[docs/README.md](docs/README.md)** — the documentation index (getting
  started, configuration, architecture, MCP, modes, subagents, security,
  observability, deployment).
- **[rfcs/README.md](rfcs/README.md)** — the normative specifications (RFC
  0001–0013).
- **[examples/SAMPLES.md](examples/SAMPLES.md)** — runnable samples and
  deployment manifests.
- **[LICENSE](LICENSE)** — license.
