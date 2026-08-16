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

> **Status — agentd 2.0.** The runtime is implemented: config validation, the
> agentic loop, the supervisor + subagent process tree, the MCP client, the v2
> **lifecycle** (`lifecycle.run_until` job/daemon) and **workflow triggers**
> (`once`/`loop`/`schedule`/`subscribe`/`signal`/`event`/`manual`/`a2a` start
> nodes), the self/control tools, and the **A2A v2** external channel (RFC 0029)
> all run today. Transport is **HTTPS everywhere** — intelligence, the MCP client,
> A2A, and operator control are all HTTP(S) with mTLS/bearer auth (loopback
> `http://` allowed for dev); agentd links no unix/vsock transport. The external
> channel and operator control are unified into the **A2A method family**
> (`a2a.listen`, `--features a2a`); the 1.x served self-MCP surface was removed in
> the mode cut-over. The durable **DAG workflow** engine (RFC 0027) is configured
> under `workflows:`. See [`design/01-durable-agent-plan.md`](design/01-durable-agent-plan.md)
> for the 2.0 build plan and [`design/00-target-vision-pivot.md`](design/00-target-vision-pivot.md)
> for the transport pivot.

## Pages

| Page | What it covers |
|---|---|
| [getting-started.md](getting-started.md) | Checkout to a first end-to-end run; the 60-second mental model; the same instruction in `once` / `loop` / `reactive` modes. |
| [use-cases.md](use-cases.md) | What agentd is *for*: worked end-to-end scenarios (jobs, reactive services, meshes of agents) with the flags and manifests that realize them. |
| [configuration.md](configuration.md) | Every flag and env var, precedence (`default < config file < env < flag`), validate-at-startup, intelligence URIs, durations, run-id, drain, exit codes. |
| [architecture.md](architecture.md) | The two-loop split (supervisor vs. agentic loop), components, the process tree, and how the pieces fit. |
| [mcp.md](mcp.md) | MCP as the universal interface: the client subset (tools/resources/subscribe, notify-then-read) and the Streamable HTTP transport. agentd's own external channel is now **A2A** (RFC 0029), not a served self-MCP. |
| [intelligence.md](intelligence.md) | The single LLM endpoint — the HTTPS transport (loopback `http://` for dev), the OpenAI-compatible wire, native tool-calling, and credential handling. |
| [modes-and-triggers.md](modes-and-triggers.md) | agentd 2.0's **lifecycle** (`lifecycle.run_until` job/daemon) and **triggers** — workflow start nodes (`once`/`loop`/`schedule`/`subscribe`/`signal`/`event`/`manual`/`a2a`), the A2A daemon channel, and a 1.x→2.0 migration table. |
| [workflows.md](workflows.md) | *(1.x — superseded.)* The v3 **durable DAG engine** (RFC 0027) is now configured under `workflows:` in the v2 document; see the [README](../README.md#workflows) and RFC 0027. This page describes the retired v1 cyclic-graph dialect. |
| [embedding.md](embedding.md) | Build your own CLI on the `agentd-core` library: the re-exec dispatch, **code-registered tools** (native Rust in the agent), the reserved `code` workflow server, and the API-stability tiers (RFC 0022). |
| [subagents.md](subagents.md) | The same-binary re-exec subagent model, the rich spawn payload + output contract, narrowed seeds, the spawn chokepoint, and depth/breadth/rate caps. |
| [observability.md](observability.md) | JSON-lines telemetry, the line schema + event vocabulary, the correlation tuple / `agent_path` subtree trick, health, and metrics-from-logs. |
| [interface.md](interface.md) | The **display clients** (RFC 0032): the terminal UI + web UI under `interface/`, the `agentd tui`/`agentd ui` passthrough, the `SubscribeToEvents` feed, debug mode, hosted-web origins. |
| [aauth.md](aauth.md) | **AAuth [DRAFT]** (`--features aauth`): agent identity for AAuth-protected MCP servers — an Ed25519 key + Agent-Provider token + RFC 9421 request signing (RFC 0023). |
| [security.md](security.md) | The granted-MCP-subset trust budget (Rule-of-Two), untrusted-content stance, SSRF defenses, the no-local-execution posture, and secrets handling. |
| [deployment.md](deployment.md) | Deployment shapes — standalone CLI job, Kubernetes Job/CronJob, long-lived A2A Deployment, systemd — drain choreography, and the exit-code contract. |
| [operations.md](operations.md) | *(largely 1.x — superseded.)* In 2.0 the control plane is **A2A** (`a2a.listen`, RFC 0029): the operator admin family (`a2a.drain`/`lameduck`/`cancel`), `--capabilities` / `--config-schema=2`, the durable-task read surface, and hot reload (SIGHUP + ConfigMap file-watch). |
| ~~scaling.md~~ | *(removed in 2.0.)* The 1.x `cluster` sharding / work-claim / standby feature was removed. In 2.0, scale with multiple daemon replicas coordinating through their **durable store** (CAS per work item) — see [deployment.md §4d](deployment.md). |

## See also

- **[`../rfcs/README.md`](../rfcs/README.md)** — the normative RFC set (0001–0020,
  including the agentctl control-plane track 0014–0020).
- **[design/](design/)** — the binding [architecture assessment](design/00-architecture-assessment.md),
  the [build plan](design/PLAN.md), and the supporting research/review notes.
- **[`../examples/SAMPLES.md`](../examples/SAMPLES.md)** — runnable samples for the
  three operational shapes (once / reactive / loop) plus manifests.
