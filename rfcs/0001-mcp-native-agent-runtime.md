# RFC 0001: agentd — a minimal, MCP-native, reactive agent runtime

**Status:** Draft / foundational.
**Author:** Andrii Tsok
**Supersedes:** all prior RFCs (the bounded-workflow-DAG design is retired).
**Date:** 2026-06-25.

---

## 0. One paragraph

`agentd` is a small, dependency-light binary that runs **one agent**. You
give it an `INSTRUCTION` and a way to reach an LLM (**intelligence**), and
it runs an agentic loop: think, call tools, observe, repeat — until the
job is done or a new event wakes it. It has **no built-in tools of its
own**; every capability comes from **MCP servers** it connects to. Its
distinguishing trait is that it **reacts to the world through MCP resource
subscriptions** — a resource changing upstream is what triggers a run, a
continuation, or a fresh iteration. The actual reasoning happens inside
**subagent processes** that the main process supervises, so agents nest:
a parent spawns and controls children as an OS process tree. And `agentd`
**speaks MCP in both directions** — it is an MCP *client* to the servers it
uses, and an MCP *server* exposing itself, so agents can be wired to each
other and to other tools with the same protocol they use for everything
else. It runs standalone from a shell, or inside a container that an
external scheduler (e.g. a Kubernetes operator — **not part of this
project**) starts, stops, and replicates.

---

## 1. Why rewrite

The previous `agentd` was a *bounded workflow runtime*: a predeclared,
validated TOML DAG where the model filled one node and control flow was
structure, not model output. That design optimised for auditability and
refused model-owned control. It was internally consistent and we are
deliberately leaving it behind.

The thesis has changed. We are no longer building a workflow engine that
happens to call a model. We are building a **lean agent** whose value is:

1. **Minimalism.** Almost no dependencies. A single static binary that
   starts fast, idles cheaply, and is trivial to drop into a container or
   a VM. The runtime is small enough to read in an afternoon.
2. **MCP as the universal interface.** Tools, triggers, and composition
   all flow through the Model Context Protocol. `agentd` does not ship a
   tool library, a policy DSL, a workflow language, or a plugin host. If
   you want a capability, you connect an MCP server. This collapses an
   enormous amount of surface area into one well-specified protocol.
3. **Reactivity through resource subscriptions.** Most agent runtimes are
   request/response or cron. `agentd` can **subscribe to MCP resources**
   and treat their updates as triggers — a long-lived agent that sits
   idle and wakes when the world it watches changes. An agent can even
   subscribe *itself* to resources mid-reasoning to arrange its own future
   wake-ups. This is the single most novel thing in the design.
4. **Composability by being MCP on both sides.** Because `agentd` is also
   an MCP server, one `agentd` can drive another, a parent can control its
   children, and a fleet of agents can be wired together — all with the
   same protocol, no bespoke clustering layer.
5. **Process-isolated subagents.** Agentic work runs in child processes,
   not threads or async tasks. Isolation, crash containment, hard
   cancellation, and natural nesting come from the OS, not from runtime
   machinery we have to build and audit.

The rewrite is permitted to start from scratch. This RFC is the target.

---

## 2. Non-goals

- **No workflow DAG / TOML graph engine.** Control flow is the model's,
  bounded by step/token/time budgets, not by a declared graph.
- **No built-in tool catalogue.** No `fs`, `http`, `shell`, `data` tool
  families baked into the binary. The one exception (`exec`) is described
  in §9 and is itself surfaced *as* an MCP tool, off by default.
- **No embedded policy engine, no Rego, no JSON-Schema validator, no
  signing subsystem** as core. The outer boundary (container, VM, the
  MCP subset you grant) is the security model. Heavier governance can
  return later as an MCP server or an optional layer — not as core weight.
- **No Kubernetes operator, CRDs, or controller in this repository.**
  External schedulers orchestrate `agentd`. We make `agentd` a *good
  citizen* to be scheduled (clean signals, exit codes, logs, health), and
  stop there. Composition between agents is MCP, not a control plane we own.
- **No multi-tenant shared service.** One agent (and its subagent tree)
  per process tree. Scale by running more instances; an external scheduler
  does the multiplying.
- **No async runtime, no web framework, no ORM, no heavy TLS stack we can
  avoid.** Every dependency must earn its place against the minimalism bar
  (§12).

---

## 3. Mental model

```
                    ┌──────────────────────────────────────────────┐
                    │              agentd (main process)            │
                    │                 = SUPERVISOR                  │
                    │                                               │
  INSTRUCTION ─────▶│  • parse config (env / flags / file)          │
  intelligence ────▶│  • connect MCP servers (as CLIENT)            │──┐ MCP client
  MCP server defs ─▶│  • serve agentd's own MCP (as SERVER)         │◀─┘ MCP server
                    │  • set up triggers:                           │
                    │       one-shot │ loop/interval │ reactive     │
                    │  • subscribe to MCP resources  ◀───────────── │── notifications/
                    │  • spawn + supervise subagent processes       │   resources/updated
                    │  • route control, enforce limits, shut down   │
                    └───────────────┬───────────────────────────────┘
                                    │ spawn / control (process tree)
              ┌─────────────────────┼─────────────────────┐
              ▼                     ▼                     ▼
     ┌────────────────┐   ┌────────────────┐    ┌────────────────┐
     │  subagent A    │   │  subagent B    │    │  subagent C    │
     │  (process)     │   │  (process)     │    │  (process)     │
     │                │   │                │    │  spawns its    │
     │  AGENTIC LOOP: │   │  AGENTIC LOOP  │    │  own children  │
     │  think→tool→   │   │                │    │   ▼   ▼        │
     │  observe→…     │   │                │    │  D    E        │
     └───────┬────────┘   └────────────────┘    └────────────────┘
             │ tool calls
             ▼
   ┌───────────────────────────────────────────────────────┐
   │  MCP servers (external): filesystem, github, db, …     │
   │  + agentd's own MCP (subagent control, subscribe, exec)│
   └───────────────────────────────────────────────────────┘
```

Two loops, deliberately separated:

- **The supervisor loop** (main process) is *dumb orchestration*: it owns
  lifecycle, triggers, subscriptions, the process tree, and limits. It
  never talks to the model itself.
- **The agentic loop** (each subagent process) is where *intelligence
  lives*: it talks to the LLM, calls MCP tools, and reasons. It may spawn
  children, which run their own agentic loops.

This split is the heart of the architecture. The supervisor stays tiny and
robust because it has no model dependency; intelligence is always isolated
in a child that can crash, be killed, or run away without taking the
supervisor down.

---

## 4. Components

### 4.1 The supervisor (main process)

Responsibilities, and nothing more:

1. **Configuration.** Read `INSTRUCTION`, the intelligence transport +
   credentials, the MCP server definitions, the trigger/mode selection,
   and the limit budgets (§10). Sources: environment variables and CLI
   flags for standalone use; an optional config file for the more verbose
   bits (MCP server lists).
2. **MCP host.** Establish connections to each declared MCP server (§7.1),
   perform the MCP handshake, and hold the catalogue of available tools and
   resources. These connections are *shared infrastructure*; subagents are
   granted scoped subsets of them (§6.3).
3. **Self MCP server.** Start `agentd`'s own MCP endpoint (§8) exposing
   subagent-control tools, the subscription tool, the gated `exec` tool,
   and introspection resources.
4. **Triggers.** Arm the configured trigger(s): run-once, loop/interval,
   or reactive resource subscriptions (§5). Translate each fired trigger
   into a supervisor action: spawn a new root subagent, or continue an
   existing session.
5. **Process supervision.** Spawn subagents, track the tree, relay control
   (pause/resume/cancel/message), reap exits, and enforce per-subagent and
   tree-wide limits.
6. **Lifecycle.** Handle `SIGTERM`/`SIGINT` (graceful drain → kill tree →
   exit), report meaningful exit codes, and emit structured logs.

The supervisor holds **no conversation state and makes no LLM calls.**

### 4.2 The subagent (process)

A subagent is the same `agentd` binary launched in **subagent mode**
(re-exec of `argv[0]`, not a separate artifact — keeps distribution to one
binary). It receives, over its control channel (§6.2):

- an **instruction** (its task),
- a **context seed** (parent-provided messages / data),
- a **tool scope** (which MCP endpoints it may use — a subset of the
  supervisor's, §6.3),
- **limits** (max steps, max tokens, deadline).

It runs the agentic loop (§6.1), streams events to its parent, may spawn
children, and exits with a result. A subagent is the unit of crash
isolation, cancellation, and accounting.

Why the same binary re-exec'd rather than threads/in-process tasks: one
artifact to ship; a child can be `SIGKILL`'d instantly without unwinding
shared state; OS-level memory/CPU isolation; and the process tree *is* the
agent tree, observable with ordinary tools (`ps`, `pstree`).

---

## 5. Triggers — how a run starts (the novel part)

A run begins in one of three ways. The mode is chosen by config/flags and
may be implied or refined by the instruction.

### 5.1 One-shot

Run the instruction to completion, print the result, exit. This is the CLI
default and the simplest container job. Maps cleanly onto a Kubernetes
`Job` / `CronJob` that an external operator schedules.

### 5.2 Loop / interval

Re-enter the agent on a timer or immediately after completing, for
polling-style or continuously-working agents. Bounded by the global limits
so an idle or confused agent can't burn unbounded cost. Daemon shape.

### 5.3 Reactive — MCP resource subscriptions

The signature mode. The supervisor issues MCP `resources/subscribe` for a
set of resource URIs (declared in config, or established dynamically by a
running agent via the internal `subscribe` tool, §8). It then waits. When
an MCP server emits `notifications/resources/updated`, the supervisor maps
it to one of:

- **Spawn** — start a fresh root subagent for the event (stateless reaction).
- **Continue** — deliver the event into an existing, suspended session and
  re-enter its agentic loop (stateful reaction — the agent "wakes up,"
  reads what changed, and keeps working in the same context).

This makes `agentd` a **reactive, event-driven agent**: it can idle at near
zero cost, subscribed to the slices of the world it cares about, and act
only when they change. Examples: watch a `file://` resource for inbound
work; watch a `db://query/...` resource for new rows; watch *another
agentd's* exposed resource (§8) to react to a sibling agent's progress.

Crucially, **an agent can arrange its own triggers.** Mid-reasoning, a
subagent may call the `subscribe` tool to register interest in a resource,
then end its turn; the supervisor keeps the session warm and re-enters it
when that resource updates. The agent is, in effect, scheduling its own
future continuations. This — self-subscription as self-triggering — is the
capability we are building the runtime around.

---

## 6. The agentic loop, subagents, and nesting

### 6.1 The loop (inside a subagent)

```
build request  = system + instruction + accumulated context
                 + tool catalogue (scoped MCP tools + agentd self-tools)
      │
      ▼
call intelligence  ───────────────────────────────►  (§7.2)
      │
      ▼
response = text and/or tool calls
      │
      ├── tool calls?  ── for each: route to owning MCP server, execute,
      │                   append result to context  ──────┐
      │                                                    │
      ├── final?       ── emit result to parent, end turn  │
      │                                                     ▼
      └──────────────────────────  loop  ◄─────────────────┘
                       until: final │ step cap │ token budget │ deadline │ cancel
```

The loop is intentionally ordinary ReAct/tool-use. The interesting design
choices are *around* it (process isolation, MCP-only tools, reactive
continuation), not inside it. Every loop turn streams events
(thought/tool-call/tool-result/final) up the control channel for
observability and for the parent's supervision decisions.

### 6.2 Control channel (supervisor ↔ subagent)

Parent and child communicate over the child's stdio (pipes) using a small
JSON line protocol. It carries, downward: the spawn payload (instruction,
context seed, tool scope, limits) and control messages (pause, resume,
cancel, inject-message). Upward: lifecycle and loop events, and the final
result. We deliberately keep this protocol **MCP-flavoured** (JSON-RPC
shapes) so the control plane and the tool plane look the same and can share
code; whether it is literally MCP or a minimal sibling protocol is an open
question (§14).

### 6.3 Tool scope and nesting

A subagent never has ambient access to all MCP servers. When the parent
spawns it, the parent passes a **tool scope**: the subset of MCP endpoints
(and possibly the subset of tools within them) the child may use. A child
can therefore be strictly less capable than its parent, never more. This is
the lightweight authority model — **scoping by granted MCP subset**, not by
a policy DSL.

Nesting falls out naturally: a subagent that has been granted the `agentd`
self-MCP (§8) can call `subagent.spawn` to create its own children, each
with a further-narrowed scope. Parents control children (pause/cancel/
message) through the same self-MCP tools. The result is a supervised tree
of agents, each isolated in its own process, each scoped to a subset of its
parent's capabilities, all the way down — bounded by a maximum tree depth
and a tree-wide budget so recursion can't explode.

---

## 7. Intelligence and MCP — the two external dependencies

`agentd` reaches exactly two kinds of outside system: an **intelligence**
endpoint (the LLM) and **MCP servers** (everything else).

### 7.1 MCP servers (the only tool source)

- **Roles.** `agentd` is an MCP **client/host** to N servers, an MCP
  **server** to whoever drives it (§8), and uses MCP **subscriptions** as
  triggers (§5.3). One protocol, three roles.
- **Transports.** stdio (spawn the server as a child process, JSON-RPC over
  pipes — the common, dependency-free case) and, where needed, HTTP/SSE to
  a server reachable over the network or a sidecar. stdio is the default
  and the lightest.
- **Definition.** Servers are declared by name + launch command/endpoint
  (+ optional env) via repeated flags for simple cases and a config file
  for richer setups. No discovery magic; the operator declares what's
  available.
- **Capabilities used.** `tools/list` + `tools/call` (the action space),
  `resources/list` + `resources/read` + `resources/subscribe` +
  `notifications/resources/updated` (context and triggers). Tool results
  flow back into the agentic loop as observations.
- **Scoping.** Which servers exist is the supervisor's concern; which a
  given subagent may call is the parent's grant (§6.3).

There is no built-in tool that is not either an MCP tool from one of these
servers or one of `agentd`'s own MCP tools (§8). That invariant is the
whole point.

### 7.2 Intelligence (the LLM transport)

A single, minimal abstraction with pluggable backends, selected by a URI
in `AGENTD_INTELLIGENCE` (or `--intelligence`):

- **`vsock:<cid>:<port>` / a named `intelligence` vsock** — for `agentd`
  running inside a microVM / confidential enclave, reaching a model gateway
  on the host across the virtio socket boundary. Strong isolation; no TCP
  stack exposed inside the guest.
- **`unix:/path`** — a local model gateway / sidecar over a Unix domain
  socket. The common same-pod sidecar case.
- **`https://…`** — a model provider or gateway over TLS, with credentials
  supplied via env/flags. The standalone-CLI case.

Credentials come from environment variables or flags, never from a config
committed to disk, and are never logged.

**Wire format.** To stay minimal, `agentd` prefers to speak **one
normalised request/response shape to an intelligence gateway** and let the
gateway adapt to specific providers — so provider-specific dialects do not
accrete inside the binary. For the standalone case that has no gateway, a
single **OpenAI-compatible `/chat/completions`** adapter covers the large
majority of providers and local servers (vLLM, Ollama, LM Studio, most
hosted gateways). Which exact shape is canonical, and how many provider
adapters (if any) ship in-binary vs. live behind the gateway, is an open
question (§14) — but the bias is hard toward *fewer adapters, thinner
binary, push provider quirks to the gateway*.

---

## 8. agentd as an MCP server (self-wiring + the internal tools)

`agentd` exposes its **own MCP server**. This single decision delivers
three things at once: it gives the agent its internal tools through the
exact mechanism it already uses for external tools; it lets a parent
control children with that same mechanism; and it lets *other* MCP clients
(including another `agentd`) wire to it — composing agents into larger
systems without any new protocol.

**Tools the self-MCP exposes** (names illustrative):

- `subagent.spawn(instruction, context?, tool_scope?, limits?)` — create a
  child agent process; returns a handle.
- `subagent.send(handle, message)` — inject a message into a running child.
- `subagent.cancel(handle)` / `subagent.status(handle)` — control and
  introspect.
- `subscribe(resource_uri)` / `unsubscribe(resource_uri)` — register/clear
  interest in an MCP resource so its updates trigger this agent (§5.3). The
  mechanism by which an agent schedules its own continuations.
- `exec(argv, …)` — run a local command (e.g. bash). **Off by default**,
  enabled only when config/instruction opts in *and* the binary exists in
  the container/OS (§9).

**Resources the self-MCP exposes** — the agent's own session/run state,
subagent statuses, and outputs, as readable + **subscribable** resources.
This is what makes agent-to-agent reactivity work: agent X subscribes to
agent Y's exposed resource, and Y's state change wakes X. The runtime is
symmetric — it both subscribes to resources and emits them.

Exposing the self-MCP over a chosen transport (`unix:`/`vsock:`/HTTP) is
opt-in via flag, so a pure one-shot CLI run carries none of it.

---

## 9. The `exec` / bash question (reconciled)

The requirement "no tools except those from MCP servers" and the allowance
"may run a command like bash" are reconciled cleanly: **`exec` is itself an
MCP tool**, served by `agentd`'s own MCP server (§8). So the invariant
holds — *every* tool the model can call is an MCP tool — while a deliberate,
gated local-execution escape hatch exists.

`exec` is:

- **Disabled by default.** Present in the catalogue only when the
  instruction/config explicitly enables it.
- **Capability-checked.** The requested binary must exist and be executable
  in the container/OS; otherwise the tool is absent, not a runtime error.
- **Isolated.** Runs in a child process under the same OS limits as
  everything else; subject to the container/VM boundary. We do not
  reimplement a sandbox — the deployment provides it.

This keeps the binary honest (one tool model) and the OS/container as the
trust boundary.

---

## 10. Configuration surface

Everything is configurable from the environment (for containers) and from
flags (for standalone CLI), with flags taking precedence.

| Concern | Env | Flag |
|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction "…"` / `--instruction @file` |
| Intelligence transport | `AGENTD_INTELLIGENCE` | `--intelligence vsock:…│unix:…│https://…` |
| Intelligence credentials | `AGENTD_INTELLIGENCE_TOKEN` (+ provider-specific) | `--intelligence-token …` |
| Model / params | `AGENTD_MODEL`, … | `--model …`, `--max-tokens …`, … |
| MCP servers | `AGENTD_MCP_CONFIG` (file) | repeated `--mcp name=cmd…`, `--mcp-config FILE` |
| Mode / triggers | `AGENTD_MODE` | `--mode once│loop│reactive`, `--interval …` |
| Subscriptions | — | repeated `--subscribe <resource-uri>` |
| Serve self-MCP | `AGENTD_SERVE_MCP` | `--serve-mcp <transport/addr>` |
| Enable exec | `AGENTD_ENABLE_EXEC` | `--enable-exec [allowlist]` |
| Limits | `AGENTD_MAX_STEPS`, `AGENTD_MAX_TOKENS`, `AGENTD_DEADLINE`, depth, tree budget | `--max-steps`, `--max-tokens`, `--deadline`, `--max-depth` |

Config is intentionally flat and small. No TOML workflow document, no graph
— the instruction plus these knobs is the whole input.

---

## 11. Deployment shapes

- **Standalone CLI.** `agentd --instruction "…" --intelligence https://…
  --mcp fs=… --mcp github=…`. One-shot by default; prints the result and
  exits. No daemon, no socket, no state.
- **Container.** The same binary; config via env. Intelligence via a vsock
  to the host gateway or a unix-socket sidecar; MCP servers either bundled
  in the image (stdio children) or reached as sidecars (HTTP). Reactive or
  loop mode makes it a long-lived workload.
- **Scheduled by an external operator (e.g. Kubernetes).** The operator —
  **not in this repo** — decides when and how many `agentd` instances run:
  a `Job`/`CronJob` for one-shot work, a `Deployment` for a long-lived
  reactive agent, replicas for fan-out. `agentd`'s obligations are only to
  be a clean citizen: honour `SIGTERM` with a bounded drain, return
  meaningful exit codes, log structured events to stdout/stderr, and
  (optionally) expose a trivial health signal. Composition *between* agents
  is MCP (§8), not a control plane we build.

---

## 12. Dependency budget (the minimalism bar)

Every dependency must justify itself; the default answer is "no." Expected
to be in:

- **JSON** — unavoidable: MCP and LLM wire formats are JSON. A single small,
  trusted JSON library (or a hand-rolled minimal parser if even that is too
  much). This is the one non-negotiable dependency.
- **A tiny blocking HTTP/1.1 client** — for `https://` intelligence and
  HTTP-transport MCP. Hand-rolled where practical (we have prior art).
- **TLS** — only when `https://` is used and TLS is not terminated by a
  sidecar/gateway. Pure-Rust (`rustls`) behind a feature flag; the
  recommended container pattern terminates TLS at the sidecar and keeps
  `agentd` plaintext-to-localhost, so many builds carry no TLS at all.
- **vsock** — a thin crate or raw `libc` for the enclave transport, behind
  a feature flag.
- **OS primitives** — `std` process/pipe/signal handling; `libc` on Unix
  for signals and resource limits.

Explicitly *out*: any async runtime (tokio), web frameworks, the retired
policy/Rego/JSON-Schema/signing/OTLP stacks, ORMs, and provider SDKs.
Concurrency is OS processes plus a few threads, not an executor. The whole
runtime should remain small enough to audit by reading it.

---

## 13. Security and isolation posture

The model is deliberately thin, leaning on the deployment:

- **Outer boundary** is the container / VM / enclave. `agentd` does not
  reimplement sandboxing.
- **Capability scoping** is by *granted MCP subset*: a subagent can only do
  what its parent's tool scope allows, narrowing all the way down the tree.
- **`exec` is off by default** and capability-checked when on (§9).
- **Process isolation** contains crashes and runaway loops; the supervisor
  can `SIGKILL` any subtree. Optional OS resource limits (rlimit/cgroup,
  typically applied by the deployment) cap memory/CPU.
- **Budgets** (steps, tokens, deadline, tree depth, tree-wide token
  ceiling) bound cost and recursion for a model-owned loop.
- **Secrets/credentials** come from env/flags, are kept out of logs and
  tool/loop transcripts, and never persisted by the runtime.

Heavier governance (signed instructions, policy-as-code, audited
allowlists) is **not core** here. If a deployment needs it, it arrives as
an MCP server in front of capabilities, or as an optional layer — never as
baseline binary weight. This is a conscious reversal of the prior design's
"governance is the moat" stance; here, **minimalism and MCP-nativity are
the moat.**

---

## 14. Open questions (to resolve before/at implementation)

1. **Control protocol.** Is the supervisor↔subagent channel literally MCP,
   or a minimal JSON-RPC sibling that shares code with the MCP layer?
   (Bias: reuse MCP shapes to avoid a second protocol.)
2. **Intelligence wire.** What is the canonical normalised request/response
   to the gateway, and do we ship *any* provider adapter in-binary beyond
   the OpenAI-compatible one for the standalone case?
3. **Session durability.** Are warm/suspended reactive sessions in-memory
   only (lost on restart), or optionally checkpointed so an external
   scheduler can restart the pod without losing context? (Bias: in-memory
   for v1, checkpoint as a later extension.)
4. **Self-MCP surface for v1.** Minimum viable set:
   `subagent.spawn/send/cancel/status`, `subscribe/unsubscribe`, `exec`.
   What waits?
5. **Reactive routing.** When several subscriptions and several warm
   sessions exist, how does the supervisor decide spawn-vs-continue and
   which session an update belongs to? Needs a small, explicit routing rule.
6. **Subagent transport.** stdio pipes for the control channel — confirmed
   for v1? Any case needing socket-based control instead?
7. **Depth/breadth limits.** Default max tree depth and tree-wide token
   ceiling for nested agents.
8. **vsock specifics.** Exact addressing scheme for the named `intelligence`
   vsock and how it's discovered inside the guest.

---

## 15. Phased plan (sketch — not a commitment)

1. **Skeleton.** Single binary; config parsing (env/flags); one-shot mode;
   intelligence over `https://` (OpenAI-compatible) and `unix:`; the
   agentic loop with **one** stdio MCP server connected; budgets. Proves the
   core: instruction → loop → MCP tool calls → result.
2. **Subagent processes.** Re-exec subagent mode; control channel; the
   self-MCP with `subagent.spawn/send/cancel/status`; nesting + tool scope
   + depth/budget. Proves the process tree.
3. **Reactivity.** `resources/subscribe` + update notifications; the
   `subscribe` self-tool; spawn-vs-continue routing; warm sessions. Proves
   the signature mode.
4. **Composition + transports.** Serve the self-MCP for external/peer
   clients; `vsock` intelligence; HTTP-transport MCP; `exec` tool gated.
   Proves self-wiring and the enclave/container stories.
5. **Hardening.** Signals/drain/exit codes; structured logs; limits and
   failure paths; the minimalism audit (cut every dependency that didn't
   earn its place).

---

## 16. Summary

`agentd` becomes a **small, MCP-native, reactive agent**: an instruction, a
model, MCP servers for every tool, a model-owned loop running in isolated
subagent processes that nest into a supervised tree, triggered by — and
emitting — MCP resource updates, and exposing itself as an MCP server so
agents compose with the same protocol they use for everything else. It runs
from a shell or inside a container that someone else schedules. The bet is
that **the smallest agent that speaks MCP fluently in every direction** is
more useful, more composable, and more durable than a larger one that
speaks a bespoke language of its own.
