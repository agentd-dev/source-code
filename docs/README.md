# Overview

`agentd` is a small Rust binary that runs **one agent**. You give it an
instruction and one LLM endpoint, and it runs the agentic loop — think, call a
tool, observe, repeat — until the work reaches a terminal status or a new event
wakes it.

Three properties make it different from an agent framework:

- **It ships no tools.** Every capability an agent has comes from a remote
  **MCP server** you name. There is no built-in filesystem, shell or HTTP tool
  library, so the blast radius of a run is exactly the set of servers you wired.
  Local command execution exists, but it is off at two independent layers and
  fenced when on.
- **The reasoning runs where it can be killed.** A supervisor that never talks
  to a model owns lifecycle, limits and the process tree; the loop itself runs
  in child processes. A model that loops, overspends or is jailbroken is
  contained by a process that cannot be prompted.
- **It is a daemon, not a script.** State lives in a store, so a restart
  resumes rather than restarts. Terminals and browsers attach to a running
  agent as thin views; several can watch the same session at once.

## Where to start

| If you want to… | Read |
|---|---|
| get something running | [Getting started](getting-started.md) |
| see what people build with it | [Use cases](use-cases.md) |
| understand the design | [Architecture](architecture.md), then [The harness](harness.md) |
| build a coding agent | [Coding agent](coding-agent.md) |
| know every setting | [Configuration](configuration.md) |
| review the security posture | [Security](security.md) |

## The documentation

**How it works** — the concepts, in the order they build on each other.

| Page | What it covers |
|---|---|
| [architecture.md](architecture.md) | The two-loop split, the components, and how a run flows from config to result. |
| [harness.md](harness.md) | The supervisor: the process tree, the kill ladder, budgets, checkpoints, and recovery. |
| [agent-loop.md](agent-loop.md) | One turn end to end — context assembly, the round loop, tool dispatch, termination. |
| [subagents.md](subagents.md) | Delegation as a process tree: narrowed context, distilled returns, depth limits. |
| [workflows.md](workflows.md) | Durable DAGs: start nodes, the node catalogue, data flow, waits, resume. |
- [node-registry.md](node-registry.md) — every workflow node, what it requires, and the traps that bite first.
| [modes-and-triggers.md](modes-and-triggers.md) | Job or daemon, and the start nodes that decide when a run fires. |
| [mcp.md](mcp.md) | Where tools and events come from: the client subset and the Streamable HTTP transport. |
| [why-rust.md](why-rust.md) | The dependency moat, what is hand-rolled, and where the choice costs something. |

**Build & operate** — using it for real.

| Page | What it covers |
|---|---|
| [configuration.md](configuration.md) | Every setting, the three spellings, precedence, and validation. |
| [experience.md](experience.md) | Validate before anything runs; exit codes as an API; telemetry you can filter. |
| [interface.md](interface.md) | The terminal and web clients: one daemon, many synchronized surfaces. |
| [coding-agent.md](coding-agent.md) | A pair-programming agent on a repository — tools, approvals, budgets, practices. |
| [security.md](security.md) | Capability scoping, the Rule of Two, the exec fence, secrets, and the limits of all of it. |
| [authentication.md](authentication.md) | Authenticating outbound to model, MCP and A2A endpoints. |
| [observability.md](observability.md) | Structured telemetry, the correlation tuple, health and metrics. |
| [deployment.md](deployment.md) | Job, CronJob, long-lived Deployment, systemd — and drain choreography. |
| [operations.md](operations.md) | Driving a live daemon: the admin surface, capabilities, hot reload. |
| [scaling.md](scaling.md) | Many replicas over one queue: partitioning at the source, queue-side leases, idempotency. |

**Extend & embed**

| Page | What it covers |
|---|---|
| [embedding.md](embedding.md) | Build your own CLI on `agentd-core`, with native Rust tools registered in-process. |
| [intelligence.md](intelligence.md) | The model endpoint: the wire, failover, budgets, credentials. |
| [aauth.md](aauth.md) | Agent identity for AAuth-protected servers — an Ed25519 key and signed requests. |

## Design records

The **decision records** live in [`design/`](design/): they record why the
system is shaped this way, including options that were considered and rejected.
