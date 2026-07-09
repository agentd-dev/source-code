# agentd documentation

`agentd` is a small, dependency-light Rust binary that runs **one agent**: you
give it an instruction and a single LLM endpoint (the *intelligence*), and it
runs an agentic loop — think, call a tool, observe, repeat — until the job
reaches a terminal status or a new event wakes it. Every task tool it can call
comes from an **MCP server** — agentd ships none of its own and never runs local
code; its only built-in tools are its *self/control* orchestration primitives
(spawn a subagent, subscribe to a resource, run a graph). It reacts to the world
through **MCP resource subscriptions**. A tiny supervisor owns lifecycle,
triggers, limits, and the process tree; the reasoning lives in isolated subagent
child processes it can always kill.

These pages are the task-oriented guide. The **normative specifications** live in
[`../rfcs/`](../rfcs/README.md) (RFC 0001 is the narrative front door; 0002–0013
specify each mechanism). The **architecture decision record + build plan** live
in [`design/`](design/) — [`00-architecture-assessment.md`](design/00-architecture-assessment.md)
is the binding decision record and [`PLAN.md`](design/PLAN.md) tracks build
status and the M1–M3 milestones.

> **Status.** The agent runtime is implemented: config validation, the agentic
> loop, the supervisor + subagent process tree, the MCP client, all five run
> modes, the reactive router, the self/control tools, and the served self-MCP all
> run today. Transport is **HTTPS everywhere** — intelligence, the MCP client, the
> served self-MCP, A2A, and operator control are all HTTP(S) with mTLS/bearer auth
> (loopback `http://` allowed for dev); agentd links no unix/vsock transport.
> Operator control is unified into the A2A method family. Agent-authored cyclic
> **workflows** ship under `--features workflow` (see
> [workflows.md](workflows.md)). See [`design/00-target-vision-pivot.md`](design/00-target-vision-pivot.md)
> for the transport pivot and [`design/PLAN.md`](design/PLAN.md) for the base build.

## Pages

| Page | What it covers |
|---|---|
| [getting-started.md](getting-started.md) | Checkout to a first end-to-end run; the 60-second mental model; the same instruction in `once` / `loop` / `reactive` modes. |
| [use-cases.md](use-cases.md) | What agentd is *for*: worked end-to-end scenarios (jobs, reactive services, meshes of agents) with the flags and manifests that realize them. |
| [configuration.md](configuration.md) | Every flag and env var, precedence (`default < config file < env < flag`), validate-at-startup, intelligence URIs, durations, run-id, drain, exit codes. |
| [architecture.md](architecture.md) | The two-loop split (supervisor vs. agentic loop), components, the process tree, and how the pieces fit. |
| [mcp.md](mcp.md) | MCP as the universal interface: the client subset (tools/resources/subscribe, notify-then-read), the Streamable HTTP transport, and agentd's own self-MCP server. |
| [intelligence.md](intelligence.md) | The single LLM endpoint — the HTTPS transport (loopback `http://` for dev), the OpenAI-compatible wire, native tool-calling, and credential handling. |
| [modes-and-triggers.md](modes-and-triggers.md) | The five modes as exit predicates; reactive routing (exactly-one-owner, spawn-vs-continue, debounce/coalesce), self-subscribe, condition predicates, in-turn wait, and internal schedule/cron. |
| [workflows.md](workflows.md) | Agent-authored cyclic graphs (`--features workflow`, dialect 2): the twelve node kinds, `writes_mode` reducers, two-tier conditions (+ CEL), `foreach`/`parallel` fan-out, **human gates over A2A** (`input-required`), the **MCP checkpointer** (crash-resume, fork/time-travel), termination layers, and the `--mode workflow` / `workflow.define`/`run`/`patch` entry points. |
| [embedding.md](embedding.md) | Build your own CLI on the `agentd-core` library: the re-exec dispatch, **code-registered tools** (native Rust in the agent), the reserved `code` workflow server, and the API-stability tiers (RFC 0022). |
| [subagents.md](subagents.md) | The same-binary re-exec subagent model, the rich spawn payload + output contract, narrowed seeds, the spawn chokepoint, and depth/breadth/rate caps. |
| [observability.md](observability.md) | JSON-lines telemetry, the line schema + event vocabulary, the correlation tuple / `agent_path` subtree trick, health, and metrics-from-logs. |
| [aauth.md](aauth.md) | **AAuth [DRAFT]** (`--features aauth`): agent identity for AAuth-protected MCP servers — an Ed25519 key + Agent-Provider token + RFC 9421 request signing (RFC 0023). |
| [security.md](security.md) | The granted-MCP-subset trust budget (Rule-of-Two), untrusted-content stance, SSRF defenses, the no-local-execution posture, and secrets handling. |
| [deployment.md](deployment.md) | Deployment shapes — standalone CLI, Kubernetes Job/CronJob, reactive Deployment, systemd — drain choreography, and the exit-code contract. |
| [operations.md](operations.md) | The control plane: the HTTPS management transport (mTLS/bearer auth), the operator control family (`a2a.Drain`/`LameDuck`/`Pause`/`Resume`/`Cancel`), the capabilities manifest + `surfaces{}`, and hot reload (SIGHUP + ConfigMap file-watch). |
| [scaling.md](scaling.md) | Horizontal scaling — `--shard K/N` partitioning, work-claim leases for cross-instance ownership, standby, and the autoscaling signals + `agent://capacity`. |

## See also

- **[`../rfcs/README.md`](../rfcs/README.md)** — the normative RFC set (0001–0020,
  including the agentctl control-plane track 0014–0020).
- **[design/](design/)** — the binding [architecture assessment](design/00-architecture-assessment.md),
  the [build plan](design/PLAN.md), and the supporting research/review notes.
- **[`../examples/SAMPLES.md`](../examples/SAMPLES.md)** — runnable samples for the
  three operational shapes (once / reactive / loop) plus manifests.
