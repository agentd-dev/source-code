# RFC 0001: MCP-native agent runtime — core architecture

> **⚠ AMENDED (target-vision pivot, 2026-07-02).** The `exec` self-tool described
> here was **removed** — agentd runs no local code (no exec/shell tool). Every
> transport is HTTPS (no unix/vsock). See [`../docs/design/00-target-vision-pivot.md`](../docs/design/00-target-vision-pivot.md).

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

**Supersedes:** all prior RFCs (the bounded-workflow-DAG design is retired).

---

## 0. One paragraph

`agentd` is a small, dependency-light Rust binary that runs **one agent**. You
give it an `INSTRUCTION` and a way to reach an LLM (**intelligence**), and it
runs an agentic loop: think, call tools, observe, repeat — until the job is
done or a new event wakes it. It has **no built-in tools of its own**; every
capability comes from **MCP servers** it connects to. Its distinguishing trait
is that it **reacts to the world through MCP resource subscriptions** — a
resource changing upstream is what triggers a run, a continuation, or a fresh
iteration. The actual reasoning happens inside **subagent processes** that a
**supervisor** owns and watches, so agents nest: a parent spawns and controls
children as an OS process tree. And `agentd` **speaks MCP in both directions** —
it is an MCP *client* to the servers it uses, and an MCP *server* exposing
itself, so agents can be wired to each other with the same protocol they use
for everything else. It runs standalone from a shell, or inside a container that
an external scheduler (e.g. a Kubernetes operator — **not part of this project**)
starts, stops, and replicates.

This RFC is the **front door** of the agentd RFC set. It is the readable
narrative core: the thesis, the two-loop split, the components, the deployment
shapes, and the non-goals. Every mechanism it names is specified in depth in one
of the sub-RFCs (0002–0013); this document cross-references them by number rather
than restating their detail. The binding architecture-decision record is
`docs/design/00-architecture-assessment.md`; where this RFC and that document
ever diverge, **the assessment wins**.

---

## 1. Why rewrite

The previous `agentd` was a *bounded workflow runtime*: a predeclared, validated
TOML DAG where the model filled one node and control flow was structure, not
model output. That design optimised for auditability and refused model-owned
control. It was internally consistent and we are deliberately leaving it behind.

The thesis has changed. We are no longer building a workflow engine that happens
to call a model. We are building a **lean agent** whose value is:

1. **Minimalism.** Almost no dependencies. A single static binary that starts
   fast, idles cheaply, and is trivial to drop into a container or a VM. A
   default Linux build is single-digit first-party crates with no async runtime,
   no TLS, and no C/C++ toolchain (RFC 0002). The runtime is small enough to read
   in an afternoon.
2. **MCP as the universal interface.** Tools, triggers, and composition all flow
   through the Model Context Protocol. `agentd` does not ship a tool library, a
   policy DSL, a workflow language, or a plugin host. If you want a capability,
   you connect an MCP server. This collapses an enormous amount of surface area
   into one well-specified protocol.
3. **Reactivity through resource subscriptions.** Most agent runtimes are
   request/response or cron. `agentd` **subscribes to MCP resources** and treats
   their updates as triggers — a long-lived agent that sits idle and wakes when
   the world it watches changes. An agent can even subscribe *itself* to a
   resource mid-reasoning to arrange its own future wake-up. This — the reactive
   loop — is the single most novel thing in the design and is unbuilt elsewhere
   in the ecosystem; it is agentd's edge (RFC 0008).
4. **Composability by being MCP on both sides.** Because `agentd` is also an MCP
   server, one `agentd` can drive another, a parent can control its children, and
   a fleet of agents can be wired together — all with the same protocol, no
   bespoke clustering layer (RFC 0005).
5. **Process-isolated subagents.** Agentic work runs in child processes, not
   threads or async tasks. Isolation, crash containment, hard cancellation
   (`SIGKILL`), and natural nesting come from the OS, not from runtime machinery
   we build and audit (RFC 0003, RFC 0009). The cancellation argument is
   decisive: the only reliable way to stop runaway model work is to kill the
   process group — async future-drop cannot do it, which is the main reason
   tokio is rejected (RFC 0002).

The rewrite starts from scratch. This RFC is the target.

---

## 2. Non-goals

- **No workflow DAG / TOML graph engine.** Control flow is the model's, bounded
  by step/token/time/depth budgets, not by a declared graph.
- **No built-in tool catalogue.** No `fs`, `http`, `shell`, `data` tool families
  baked into the binary. The one exception (`exec`) is surfaced *as* an MCP tool
  served by agentd's own self-MCP, off by default (RFC 0005, RFC 0012).
- **No embedded policy engine, no Rego, no JSON-Schema validator, no signing
  subsystem** as core. The outer boundary (container, VM, the MCP subset you
  grant) is the security model; capability scoping is the *granted MCP subset*
  interpreted as a Rule-of-Two trust budget (RFC 0012). This is a conscious
  reversal of the prior design's "governance is the moat" stance — here
  minimalism and MCP-nativity are the moat.
- **No async runtime, no web framework, no ORM, no heavy TLS stack we can
  avoid.** Concurrency is OS processes plus a few threads, never an executor.
  Every dependency earns its place against the minimalism bar (RFC 0002).
- **No Kubernetes operator, CRDs, or controller in this repository.** External
  schedulers orchestrate `agentd`. We make `agentd` a *good citizen* to be
  scheduled — clean signals, a public exit-code contract, structured logs,
  health — and stop there (RFC 0011). Composition between agents is MCP, not a
  control plane we own.
- **No multi-tenant shared service.** One agent (and its subagent tree) per
  process tree. Scale by running more instances; an external scheduler does the
  multiplying.
- **No v1 reactivity over HTTP, no self-MCP over HTTP, no MCP tasks/sampling/
  roots.** These are real and tempting but deferred for honest minimalism
  reasons spelled out in §7.3 and RFC 0013.

---

## 3. Mental model

```
                    ┌──────────────────────────────────────────────┐
                    │              agentd (main process)           │
                    │                 = SUPERVISOR (a REACTOR)      │
                    │           no LLM dependency, never reasons    │
                    │                                               │
  INSTRUCTION ─────▶│  • parse + validate config (env/flags/file)  │
  intelligence ────▶│  • connect MCP servers (as CLIENT)           │──┐ MCP client
  MCP server defs ─▶│  • serve agentd's own MCP (as SERVER)       │◀─┘ MCP server
                    │  • arm triggers: once │ loop │ reactive │ sch │
                    │  • subscribe to MCP resources  ◀──────────── │── notifications/
                    │  • recv_timeout(merged mpsc): one blocking   │   resources/updated
                    │    wait over every reader thread + timers    │   (URI only → re-read)
                    │  • spawn + supervise subagent processes       │
                    │  • detect dead/stuck, reap, kill, restart     │
                    └───────────────┬───────────────────────────────┘
                                    │ spawn / control (process tree)
              ┌─────────────────────┼─────────────────────┐
              ▼                     ▼                     ▼
     ┌────────────────┐   ┌────────────────┐    ┌────────────────┐
     │  subagent A    │   │  subagent B    │    │  subagent C    │
     │  (process,     │   │  (process)     │    │  (process)     │
     │   own pgroup)  │   │                │    │  spawns its    │
     │  AGENTIC LOOP: │   │  AGENTIC LOOP  │    │  own children  │
     │  think→tool→   │   │                │    │   ▼   ▼        │
     │  observe→…     │   │                │    │  D    E        │
     │  + control thr │   │                │    │                │
     └───────┬────────┘   └────────────────┘    └────────────────┘
             │ tool calls
             ▼
   ┌───────────────────────────────────────────────────────┐
   │  MCP servers (external): filesystem, github, db, …     │
   │  + agentd's own self-MCP (subagent.*, subscribe, exec) │
   └───────────────────────────────────────────────────────┘
```

Two loops, deliberately separated:

- **The supervisor loop** (main process) is *dumb orchestration*: it owns
  lifecycle, triggers, subscriptions, the process tree, dead/stuck detection,
  reaping, and limits. It is a **reactor** — one thread that `recv_timeout`s a
  single merged `mpsc` fed by one reader thread per long-lived stream, with the
  timeout doubling as the timer tick. It **never talks to the model itself**
  (RFC 0002).
- **The agentic loop** (each subagent process) is where *intelligence lives*: it
  talks to the LLM, calls MCP tools, and reasons (RFC 0007). It may spawn
  children, which run their own agentic loops (RFC 0009).

This split is the heart of the architecture. The supervisor stays tiny and
robust because it has no model dependency; intelligence is always isolated in a
child that can crash, be killed, or run away without taking the supervisor down.

---

## 4. Components

### 4.1 The supervisor (main process)

Responsibilities, and nothing more:

1. **Configuration.** Read and **fully validate at startup** the `INSTRUCTION`,
   the intelligence transport + credentials, the MCP server definitions, the
   trigger/mode selection, and the limit budgets. Precedence is hard:
   `built-in default < config file < env var < CLI flag`. Bad config exits **2**
   in milliseconds, before any side effect or LLM round-trip (RFC 0011).
2. **MCP host.** Establish a connection to each declared MCP server, run the full
   `initialize` handshake **per connection**, negotiate the protocol version,
   and **store each server's negotiated capabilities**. Every later call is
   gated on what that server advertised, and every `*/list` follows pagination
   cursors. These connections are *shared infrastructure*; subagents are granted
   scoped subsets of them (RFC 0004, §6.3).
3. **Self MCP server.** Serve `agentd`'s own MCP endpoint over **stdio (when a
   parent/peer spawned this agentd as a subprocess) and/or a unix-socket on
   `--serve-mcp unix:…`** (RFC 0005). It exposes subagent-control tools, the
   subscription tools, the gated `exec` tool, and subscribable state resources.
   Note: serving the self-MCP on stdout and printing a `once`-mode result on
   stdout are **mutually exclusive per process** — the supervisor selects one by
   mode (RFC 0005 §3.6); a top-level `once` run keeps stdout for its result.
4. **Triggers.** Arm the configured trigger(s) and translate each fired trigger
   into a supervisor action — spawn a new root subagent or continue a warm
   session — via the reactive router (RFC 0008).
5. **Process supervision.** Spawn subagents, track the tree, relay control,
   detect dead-vs-stuck, reap exits (PID-1 subreaper + `waitpid` loop), enforce
   per-subagent and tree-wide limits, run the kill ladder, and govern restarts
   (RFC 0003).
6. **Lifecycle.** Handle `SIGTERM`/`SIGINT` (bounded drain → kill tree → exit),
   reap `SIGCHLD`, ignore `SIGPIPE`, report meaningful exit codes, and emit
   structured JSON-lines logs to stderr (RFC 0010, RFC 0011).

The supervisor holds **no conversation state and makes no LLM calls.** It is
itself stateless: the per-child spawn payload is the minimum recoverable unit,
and on its own restart it rebuilds and reconciles rather than persisting live
state (RFC 0003).

### 4.2 The subagent (process)

A subagent is the same `agentd` binary launched in **subagent mode** (re-exec of
`argv[0]`, not a separate artifact — keeps distribution to one binary). It
receives, in its **spawn payload** over the control channel (RFC 0009):

- an **instruction** plus an **output contract** (objective, required output
  format, tool/source guidance, boundaries — bare instruction strings reproduce
  the known vague-delegation failure mode),
- a **narrowed context seed** (only the slices the parent chooses, never the full
  transcript — context hygiene *and* an injection firewall),
- a **tool scope** (the subset of MCP endpoints/tools it may use — a subset of
  the parent's, narrowing monotonically down the tree),
- **limits** (max steps, max tokens, a **mandatory finite deadline**, depth) —
  where **depth is minted by the supervisor from the caller's handle, never
  trusted from the child's request**,
- a **telemetry block** for tree-correlated logging and W3C trace propagation
  (RFC 0010).

It runs the agentic loop (RFC 0007), streams events to its parent over a control
channel whose reader runs on a **dedicated thread decoupled from the agentic
loop** (so ping/pong liveness survives a long in-flight model/tool call), may
spawn children, and exits with a **distilled, structured result** (~1–2k tokens)
plus a terminal status and usage. Every subagent — and every `exec` child — early
in `main` sets `prctl(PR_SET_PDEATHSIG, SIGKILL)` so a supervisor crash collapses
the tree from the leaves up (RFC 0003).

Why the same binary re-exec'd rather than threads/in-process tasks: one artifact
to ship; a child can be `SIGKILL`'d instantly without unwinding shared state;
OS-level memory/CPU isolation; and the process tree *is* the agent tree,
observable with ordinary tools (`ps`, `pstree`).

---

## 5. Triggers — how a run starts (the novel part)

There is **one supervisor loop and one inner agentic loop**; the execution modes
are not divergent code paths but **one driver differing only by its EXIT
PREDICATE** (RFC 0008). This is the load-bearing cloud-native simplification — we
never fork the daemon and the job into separate engines.

| Mode | Exit predicate | Deploy shape |
|---|---|---|
| `once` | first root subagent reaches a terminal status | Job, CLI |
| `loop` | a bound hit (max iterations / global deadline / tree token ceiling) or signal | Job-with-deadline or Deployment |
| `reactive` | never on its own; only signal or fatal/limit | Deployment |
| `schedule` | per-fire identical to `once` | external CronJob (recommended) or internal interval/cron |

### 5.1 One-shot (`once`)

Run the instruction to completion, print the result on stdout, exit. CLI default
and the simplest container job. Maps onto a Kubernetes `Job`/`CronJob` an
external operator schedules. The root subagent's terminal status maps to the
exit code (completed→0, refused→5, partial→3, budget→7; RFC 0011).

### 5.2 Loop / interval (`loop`)

Re-enter the agent on a timer (`--interval D`, `D=0` = re-enter immediately) or
after each completion, for polling-style or continuously-working agents. Bounded
by global limits so an idle or confused agent cannot burn unbounded cost. Daemon
shape.

### 5.3 Reactive — MCP resource subscriptions

The signature mode. The supervisor issues MCP `resources/subscribe` for a set of
concrete resource URIs (declared in config, or established dynamically by a
running agent via the `subscribe` self-tool, §8), **gated on each server having
advertised `resources.subscribe`**. It then idles in `recv_timeout` at near-zero
CPU. When an MCP server emits a notification, the reactive router (RFC 0008) maps
it to exactly one action.

**Two protocol facts are load-bearing here — and were stated imprecisely in the
original draft; they are corrected once and for all:**

- **Notify-then-read.** `notifications/resources/updated` carries **only the
  `{uri}`** (optionally a `title`) — **no payload, no diff**. The supervisor (or
  the woken agentd) must issue a fresh `resources/read` on wake to learn what
  changed. The reactive loop is therefore **two round-trips and can race**
  (the resource may change again before the read), which makes per-route
  **debounce + coalesce** mandatory, not optional (RFC 0008).
- **Item subscriptions and list-changed are distinct mechanisms.** Per-URI
  `resources/subscribe` → `notifications/resources/updated{uri}` is **not** the
  same as the capability-implied `notifications/resources/list_changed{}` (no
  subscribe, no uri, gated on `resources.listChanged`). The original draft
  conflated them. They are two separate event sources and the trigger layer
  treats them as such (RFC 0004).
- **You cannot subscribe to a resource *template*.** Only concrete URIs are
  subscribable. The original draft's `db://query/...` example was wrong: to
  react to "any new row," enumerate concrete URIs via `resources/list` and
  subscribe per-URI, or react to the set via `list_changed` (RFC 0004).

On wake, the router resolves the event to one of:

- **Spawn** — start a fresh root subagent for the event (stateless reaction).
- **Continue** — deliver the event into an existing, warm session and re-enter
  its agentic loop in the same context (stateful reaction: the agentd "wakes,"
  re-reads current state, and keeps working).

Routing is **exactly-one-owner**: every `updated{uri}` matches exactly one route
by first-match in declared order (exact URI beats glob; longest-prefix first); no
fan-out; no match → log + drop + counter. Spawn-vs-continue is a **route
property, deterministic** — not a per-event guess. Delivery is **at-least-once,
made idempotent by re-reading current state** (we promise convergence, not
exactly-once). The full rule — debounce defaults, bounded queues, ordering,
backpressure, reconnect re-subscribe-and-read — is RFC 0008.

Examples: watch a `file://` resource for inbound work; enumerate and subscribe to
concrete `db://…` row URIs; watch *another agent's* exposed `agent://…`
resource (§8) to react to a sibling's progress.

Crucially, **an agentd can arrange its own triggers.** Mid-reasoning, a subagent
calls the `subscribe` self-tool; the supervisor auto-creates a
`continue(this_session)` route, the agentd ends its turn, and it is re-entered in
the same session when that resource updates. **Self-subscription as
self-scheduling** is the capability the runtime is built around (RFC 0008).

### 5.4 Time schedules (`schedule`)

An **external CronJob → `once`** is the **recommended production path** — robust
to clock skew, restarts, and 12-factor. Internal scheduling (`--interval`, and an
optional `cron`-feature 5-field cron) is a **standalone convenience**, implemented
as internal time events fed into the *same* reactive router ("a clock is just
another event source"). There is no second scheduling subsystem and no
calendar/DST/job-store in core; default TZ is UTC (RFC 0008).

---

## 6. The agentic loop, subagents, and nesting

### 6.1 The loop (inside a subagent)

```
build request  = system + instruction + output contract + context seed
                 + transcript + scoped tool catalogue (provider `tools` field)
                 + a compact resource CATALOGUE (URIs + descriptions, no bodies)
      │
      ▼
call intelligence  ───────────────────────────────►  (§7.2, RFC 0006)
      │  record usage → bump node + tree-root budgets (RFC 0003)
      ▼
response = text and/or tool calls
      │
      ├── tool calls?  ── scope-check → route to owning server → execute
      │                   → append result OR error as observation  ──────┐
      │                   (tool/exec results are the VERIFY ground truth) │
      │                                                                    │
      ├── final?       ── emit distilled result to parent, end turn        │
      │                                                                     ▼
      └──────────────────────────  loop  ◄─────────────────────────────────┘
          until a terminal status fires (the stop disjunction, RFC 0007)
```

The loop is intentionally ordinary ReAct/tool-use; the interesting choices are
*around* it. The original draft's notion of "final = the model stopped emitting
tool calls" is **replaced by an explicit terminal-status state machine** with a
named VERIFY phase grounded in tool/exec results, **never self-judgment**. Stopping
is a disjunction of cheap per-turn checks, each with a distinct terminal status
(RFC 0007):

`completed` · `exhausted_steps` · `exhausted_tokens` · `deadline` · `stalled`
(content-hash unchanged for N turns, default 3) · `loop_detected` (per-tool repeat
cap K, default 3) · `refused` · `cancelled` · `crashed`.

The global step/token/deadline cap is non-negotiable. At every budget the agentd
wraps up gracefully and returns partials; RLIMIT/`SIGKILL` are the backstop for
wedged children. **Error taxonomy:** tool-domain errors and malformed model output
become observations (recoverable, step-consuming); transient transport errors get
bounded retry with backoff+jitter; fatal infra (intelligence unreachable, auth,
hard budget) aborts with a matching terminal status. The `isError:true` inside a
successful tool result is an observation fed to the model; a JSON-RPC `error` is a
protocol/transport failure handled by retry/abort policy — **these are distinct and
must not be conflated** (RFC 0004, RFC 0007). Context is managed by lever-ordered
compaction (clear stale results → summarize at ~75% window → optional note file),
estimating tokens from the prior response's `usage` plus a chars/4 heuristic, with
no tokenizer dependency (RFC 0007).

### 6.2 Control channel (supervisor ↔ subagent)

Parent and child communicate over the child's stdio pipes using a **minimal
JSON-RPC sibling protocol — NOT literally MCP**. It shares the exact JSON codec
with the MCP layer but has **no MCP lifecycle** (no `initialize`/capabilities
handshake on a private pipe). It carries downward: the spawn payload and control
messages (pause/resume/cancel/inject/ping); upward: lifecycle and loop events,
usage, the final result, and pong. **Framing is length-prefixed (4-byte LE +
payload, cap 16 MiB), not NDJSON**, because control payloads may contain newlines;
MCP-over-stdio stays NDJSON per spec, and the two codecs share parse/serialize but
differ in framing. The control reader runs on a **dedicated thread decoupled from
the agentic loop** so ping/pong liveness survives a long in-flight call. The
*external-facing* "spawn a child and await its result" surface is **not** this
internal protocol leaked outward — it is exposed as self-MCP tools (RFC 0005). The
original draft's open question on whether this channel is "literally MCP" is
**resolved: minimal sibling, not MCP** (RFC 0005).

### 6.3 Tool scope and nesting

A subagent never has ambient access to all MCP servers. When the parent spawns
it, the parent passes a **tool scope**: the subset of MCP endpoints (and tools
within them) the child may use. A child is therefore strictly **less** capable
than its parent, never more — scope narrows monotonically down the tree. This is
the lightweight authority model — **scoping by granted MCP subset, interpreted as
a Rule-of-Two trust budget** (RFC 0012), not by a policy DSL. The supervisor/parent
can tag tools (`untrusted_input` / `sensitive` / `egress`) and **warn or refuse** a
grant that hands one subagent all three legs of the lethal trifecta without an
explicit override; process isolation plus the distilled structured return doubles
as a CaMeL-style injection firewall.

Nesting falls out naturally, **but only through the supervisor-owned
`subagent.spawn` self-tool — exactly one unforgeable chokepoint** for all caps. A
child creates children by calling back into the self-MCP; it cannot fork the
process tree any other way. Caps (finite, conservative defaults, enforced at the
chokepoint): `max_depth` (3–5), `max_children` per node, `max_total_subagents`
tree-wide, a spawn-rate token-bucket, and a tree-wide token ceiling. **A spawn
exceeding any cap is refused as a tool result** (the parent's model adapts), never
a crash (RFC 0009). Subagents are **sync-default**: `subagent.spawn` blocks the
parent's turn (simplest, deterministic; the parent is cheaply paused between
turns). Async (`{async:true}` handle / completion-as-self-resource) and detached
(`{detach:true}`) spawns **shipped in M3 alongside reactivity** (RFC 0009).

---

## 7. Intelligence and MCP — the two external dependencies

`agentd` reaches exactly two kinds of outside system: an **intelligence** endpoint
(the LLM) and **MCP servers** (everything else). These are **different wires** —
the intelligence transports carry the LLM wire, **not MCP** — and must not be
conflated.

### 7.1 MCP servers (the only tool source)

- **Roles.** `agentd` is an MCP **client/host** to N servers, an MCP **server** to
  whoever drives it (§8), and uses MCP **subscriptions** as triggers (§5.3). One
  protocol, three roles.
- **Target version.** MCP **2025-11-25**, interoperating down to 2024-11-05. Pin
  the version, negotiate at `initialize`, and implement the mismatch path
  (RFC 0004).
- **Transport (client).** **stdio** (spawn the server as a child, NDJSON JSON-RPC
  over pipes — the common, dependency-free case, with stderr captured and an
  ordered shutdown ladder: close-stdin → SIGTERM → SIGKILL). stdio is the default
  and the lightest, and over stdio server-initiated notifications simply arrive on
  the server's stdout (RFC 0004).
- **Capabilities used (v1, capability-gated, cursor-paginated).** `tools/list` +
  `tools/call` (parsing `content[]`, `isError`, `structuredContent`) +
  `notifications/tools/list_changed`; `resources/list` + `resources/read`
  (`contents[]` is an array; text or base64 `blob`); `resources/subscribe` /
  `unsubscribe` + `notifications/resources/updated` (URI-only → notify-then-read)
  + `notifications/resources/list_changed`; `ping` both ways;
  `notifications/cancelled` when abandoning an in-flight request;
  `notifications/progress` (reset request timeout with an absolute ceiling) and
  `notifications/message` (fold into logs). `agentd` **declares NO client
  capabilities** (no roots/sampling/elicitation/tasks); it answers `roots/list`
  with `{"roots":[]}` and rejects an unsolicited `sampling/createMessage`
  (RFC 0004).
- **Scoping.** Which servers exist is the supervisor's concern; which a given
  subagent may call is the parent's grant (§6.3).

There is no built-in tool that is not either an MCP tool from one of these servers
or one of `agentd`'s own self-MCP tools (§8). That invariant is the whole point.

### 7.2 Intelligence (the LLM transport)

A single, minimal abstraction with pluggable transports, selected by a URI in
`AGENT_INTELLIGENCE` (or `--intelligence`) — all driving the same
transport-agnostic hand-rolled HTTP/1.1+framed client over `Read + Write`
(RFC 0006):

- **`unix:/path`** — a local model gateway / sidecar over a Unix domain socket.
  The common same-pod case.
- **`https://…`** — a model provider or gateway over TLS (behind the `tls`
  feature; `rustls`/`ring`/`webpki-roots`). The standalone-CLI case. Most builds
  terminate TLS at a sidecar and link no TLS at all (RFC 0002).
- **`vsock:<cid>:<port>`** — for `agentd` inside a microVM / confidential enclave
  reaching a host gateway across the virtio socket (behind the `vsock` feature).
  Strong isolation; no TCP stack exposed inside the guest.

**Wire format (resolved; the original draft's open question is closed).** The
canonical in-binary shape is **OpenAI-compatible `/chat/completions` with native
tool-calling** (covers vLLM/Ollama/LM-Studio/most hosted gateways). Exactly **two
adapters ship in-binary — `openai-compatible` and `anthropic`**; the hard bias is
fewer adapters, thinner binary, push other provider quirks to the gateway. When a
gateway/model lacks native tool-calling, fall back to a JSON-action
`{"action":"tool"|"final"}` shape parsed prose-tolerantly. **Credentials** come
from env/flags only, via a `resolve(name)` front door, never logged, never
persisted, never in transcripts, with a build-time key probe for fast-fail
(RFC 0006, RFC 0012).

### 7.3 What MCP features v1 does NOT use (and why)

The assessment's MCP review surfaced three 2025-11-25 features that *look*
purpose-built for agentd. They are **acknowledged and deferred to v2 (RFC 0013)**,
not adopted, for honest minimalism:

- **Tasks** (durable/pollable/deferred-result requests) are the spec-native shape
  for the *external-facing* long-running surface. v1 falls back to
  request/response + `progress` + `cancel`.
- **Sampling** (`sampling/createMessage`) is the spec-correct way for a peer to
  "use agentd's intelligence" — but it is a **server→client** request where
  **sampling is a CLIENT capability**, so agentd would have to act as a
  *sampling-capable client*, the **opposite wiring** from "expose a server, peer
  connects." **v1 declares no client capabilities and implements sampling in
  neither direction.** (This corrects the original draft's implied directionality.)
- **Roots** is the idiomatic filesystem-scope signal, complementary to the
  granted-subset model. Deferred.

Reactivity-over-HTTP and self-MCP-over-HTTP are likewise deferred: receiving
notifications over HTTP needs a long-lived **SSE GET stream** (not "a tiny
blocking HTTP client"), and a real **Streamable HTTP** server needs POST+GET
endpoints, `MCP-Session-Id`, `MCP-Protocol-Version`, `Origin`→403, SSE upgrade,
and resumability. **v1 keeps reactivity on stdio only and serves the self-MCP over
stdio/unix-socket only.** The term is **"Streamable HTTP"**; the old
2024-11-05 HTTP+SSE two-endpoint transport is deprecated and **never implemented**
(RFC 0004, RFC 0005, RFC 0013).

---

## 8. agentd as an MCP server (self-wiring + the internal tools)

`agentd` exposes its **own MCP server**, served over **stdio (when spawned as a
subprocess by a parent/peer) and a unix socket when `--serve-mcp unix:…`**
(RFC 0005; stdio-serving and `once`-mode result-on-stdout are mutually exclusive
per process, RFC 0005 §3.6). This one decision delivers three
things at once: it gives the agentd its internal tools through the exact mechanism
it already uses for external tools; it lets a parent control children with that
same mechanism; and it lets *other* MCP clients (including another `agentd`) wire
to it — composing agents without any new protocol.

**Tools the self-MCP exposes** (v1; names illustrative):

- `subagent.spawn(instruction, output_contract, context_seed?, tool_scope?, limits?)`
  — create a child agent process; **sync (blocking) by default**; returns the distilled
  result. Async/detach shipped in M3.
- `subagent.send(handle, message)` / `subagent.cancel(handle)` /
  `subagent.status(handle)` — inject, control, introspect.
- `subscribe(resource_uri)` / `unsubscribe(resource_uri)` — register/clear interest
  in an MCP resource so its updates trigger this agentd (§5.3). Self-subscribe
  auto-creates a `continue(this_session)` route — the agentd schedules its own
  future wake.
- `resource.read(uri)` — pull a resource body on demand (the agentd reacts to
  *current state*, per notify-then-read).
- `exec(argv, …)` — run a local command. **Off by default** (RFC 0012), enabled
  only when config opts in *and* the binary exists; folded into the same OS-limit
  + kill-ladder + budget regime as subagents (RFC 0003).

It declares `tools:{listChanged:true}` and **emits
`notifications/tools/list_changed` when the gated set changes** (e.g. on runtime
scope narrowing) — the original draft implied dynamic scoping but never named this
(RFC 0004, RFC 0005).

**Resources the self-MCP exposes** — the agentd's own session/run/subagent state as
readable **and subscribable** resources, advertised as
`resources:{subscribe:true,listChanged:true}`, emitting
`notifications/resources/updated{uri}` on state transitions, under a custom
`agent://…` scheme (legal; only other agentd instances understand its semantics).
This is what makes **agent-to-agent reactivity** and **async subagent completion**
work: agent X subscribes to agent Y's `agent://session/…` resource and Y's state
change wakes X. Because notifications are payload-less, design the resource
granularity so a single `resources/read` on wake is cheap and meaningful (RFC 0005).

---

## 9. The `exec` / bash question (reconciled)

The requirement "no tools except those from MCP servers" and the allowance "may
run a command like bash" reconcile cleanly: **`exec` is itself an MCP tool**,
served by agentd's own self-MCP (§8). So the invariant holds — *every* tool the
model can call is an MCP tool — while a deliberate, gated local-execution escape
hatch exists. `exec` is **disabled by default**, **capability-checked** (the binary
must exist → absent, not a runtime error), and **isolated** under the same OS
limits, process-group kill ladder, deadline, and subtree budget/breadth/rate caps
as everything else. Having no control channel, only the deadline + kill detectors
apply to it (not ping/pong). It is the strongest leg of the lethal trifecta, so an
`exec`-scoped subagent should be the one least exposed to untrusted content
(RFC 0003, RFC 0012). We do not reimplement a sandbox — the deployment provides it.

---

## 10. Configuration surface

Everything is env-settable (12-factor III) and flag-overridable; precedence is
`built-in default < config file < env var < CLI flag`, **validated fully at
startup** (RFC 0011). The file (`AGENT_MCP_CONFIG`) is only for verbose structural
bits (MCP server lists), **never for secrets** (env/flag only).

| Concern | Env | Flag |
|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction "…"` / `--instruction @file` |
| Intelligence transport | `AGENT_INTELLIGENCE` | `--intelligence unix:…│https://…│vsock:…` |
| Intelligence credentials | `AGENT_INTELLIGENCE_TOKEN` (+ provider-specific) | `--intelligence-token …` |
| Model / params | `AGENT_MODEL`, … | `--model …`, `--max-tokens …`, … |
| MCP servers | `AGENT_MCP_CONFIG` (file) | repeated `--mcp name=cmd…`, `--mcp-config FILE` |
| Mode / triggers | `AGENT_MODE` | `--mode once│loop│reactive│schedule`, `--interval …` |
| Subscriptions | — | repeated `--subscribe <concrete-resource-uri>` |
| Serve self-MCP | `AGENT_SERVE_MCP` | `--serve-mcp stdio│unix:…` |
| Enable exec | `AGENT_ENABLE_EXEC` | `--enable-exec [allowlist]` |
| Run id (idempotency) | `AGENT_RUN_ID` | `--run-id …` |
| Drain budget | `AGENT_DRAIN_TIMEOUT` (**MUST be < pod grace**) | `--drain-timeout …` |
| Limits | `AGENT_MAX_STEPS`, `AGENT_MAX_TOKENS`, `AGENT_DEADLINE`, depth, tree budget | `--max-steps`, `--max-tokens`, `--deadline`, `--max-depth` |

Config is intentionally flat and small. No TOML workflow document, no graph — the
instruction plus these knobs is the whole input. Config is never read from the
network. `AGENT_RUN_ID` propagates into every MCP tool-call `_meta` so backing
services can dedupe retries (RFC 0011).

---

## 11. Deployment shapes

- **Standalone CLI.** `agentd --instruction "…" --intelligence https://… --mcp
  fs=… --mcp github=…`. One-shot by default; prints the result and exits. No
  daemon, no socket, no state.
- **Container.** The same binary; config via env. Intelligence via a unix-socket
  sidecar (TLS terminated there → no TLS in the binary) or a vsock to the host
  gateway; MCP servers either bundled in the image (stdio children) or reached as
  sidecars. Reactive or loop mode makes it a long-lived workload.
- **Scheduled by an external operator (e.g. Kubernetes).** The operator — **not in
  this repo** — decides when and how many `agentd` instances run: a `Job`/`CronJob`
  for one-shot work, a `Deployment` for a long-lived reactive agent, replicas for
  fan-out. `agentd`'s obligations are only to be a clean citizen: a bounded
  `SIGTERM` drain (**`AGENT_DRAIN_TIMEOUT` < `terminationGracePeriodSeconds`** —
  the top cloud-native footgun, validated at startup), a clean drain that returns
  **0 (not 143)**, the public exit-code contract, structured stderr logs, and a
  mode-aware health signal (supervisor heartbeat liveness — idle is healthy, a
  stuck subagent must not flip pod liveness). It is cgroup-v2-*aware* but never
  hard-requires cgroup write access (RFC 0010, RFC 0011). Composition *between*
  agents is MCP (§8), not a control plane we build.

---

## 12. Security and isolation posture

The model is deliberately thin, leaning on the deployment and on structural
isolation as the moat (RFC 0012):

- **Outer boundary** is the container / VM / enclave. `agentd` does not
  reimplement sandboxing.
- **Capability scoping** is by *granted MCP subset*, interpreted as a **Rule-of-Two
  trust budget**: a subagent narrows monotonically down the tree, and a grant that
  combines untrusted-input + sensitive + egress in one subagent is warned/refused
  without an explicit override. Process isolation + distilled structured returns
  are the injection firewall.
- **All MCP server content is untrusted — including tool descriptions, schemas,
  and annotations** (tool poisoning). Never build a launch command from
  model/server-controlled strings; surface/log tool descriptions for operator
  audit. stdio is the default transport (limits server reach to agentd only).
- **SSRF defenses** in the hand-rolled HTTP client: enforce HTTPS in prod, block
  RFC-1918 / loopback / link-local by default, validate redirects, with an
  explicit dev opt-out (RFC 0012).
- **`exec` is off by default** and capability-checked when on (§9).
- **Process isolation + budgets** contain crashes and runaway loops; the
  supervisor can `SIGKILL` any subtree; budgets (steps, tokens, deadline, depth,
  tree-wide token ceiling) bound a model-owned loop. Honest caveat: only the
  *token* ceiling is enforced in-binary — aggregate subtree *memory* needs cgroups
  (RFC 0003).
- **Secrets/credentials** come from env/flags via the `resolve()` front door, are
  kept out of logs and transcripts, never persisted, and print as `***` in
  `Debug`.

Heavier governance (signed instructions, policy-as-code, audited allowlists) is
**not core** — it arrives, if needed, as an MCP server in front of capabilities,
never as baseline binary weight.

---

## 13. Decisions map — where each area is specified

The original draft's §14 "Open questions" and §15 phased sketch are **retired**:
every question they raised is now resolved, and the build sequence (M1–M7) lives
in the architecture-decision document (`docs/design/00-architecture-assessment.md`
§4). Decisions are recorded per-area in the following RFCs; this document is the
narrative core and cross-references them rather than restating their mechanism
detail.

| RFC | Title | What it pins down |
|---|---|---|
| **0001** (this) | MCP-native agent runtime — core architecture | Thesis, two-loop split, components, modes, deployment shapes, non-goals; the front door. |
| **0002** | Supervisor reactor & concurrency model | Thread-per-fd + `mpsc` reactor, self-pipe signals, the abandon-don't-interrupt invariant, dependency budget, timer/deadline arming. |
| **0003** | Process supervision, dead/stuck detection & recovery | Three-detector model + EOF×pong classifier, PID-1 subreaper + `waitpid` loop, PDEATHSIG, bounded kill ladder, restart governor, rebuild+reconcile, hierarchical token accounting, cgroup-awareness. |
| **0004** | MCP client subset & wire codec | Target 2025-11-25, capability gating, pagination, tools/resources/subscribe, notify-then-read, item-vs-list, ping/cancel/progress, stdio transport + shutdown ladder, shared JSON-RPC codec, `isError` vs JSON-RPC error. |
| **0005** | Self-MCP server & control protocol | The self-MCP tool/resource surface, subscribable `agent://` state resources, stdio/unix serving; the length-framed JSON-RPC supervisor↔subagent control channel. |
| **0006** | Intelligence transport & wire format | unix/https(tls)/vsock transports, openai-compatible + anthropic adapters, native tool-calling + usage, JSON-action fallback, credential handling. |
| **0007** | Agentic loop & terminal-status state machine | ReAct turn, the stop disjunction with distinct statuses, VERIFY grounded in tool/exec, error taxonomy, context compaction levers, resource list-vs-read. |
| **0008** | Execution modes, triggers & reactive routing | once/loop/reactive/schedule as exit predicates; the routing rule (exactly-one-owner, spawn-vs-continue, debounce/coalesce, backpressure, ordering, self-subscribe); internal interval/cron as event sources. |
| **0009** | Subagent process model & nesting | Re-exec subagent mode, rich spawn payload + output contract, narrowed seed, distilled result, sync/async/detach, tool scope, depth/breadth/rate/tree-token caps, the single spawn chokepoint. |
| **0010** | Observability, health & telemetry | The JSON-lines logger, line schema + closed event vocabulary, correlation tuple + spawn telemetry block, W3C context propagation, mode-aware health, metrics-from-logs, gated `metrics`/`otel`. |
| **0011** | Cloud-native contract: config, signals, exit codes, idempotency | Config precedence + validate-at-startup, drain choreography + `AGENT_DRAIN_TIMEOUT` < grace, the exit-code table, RUN_ID idempotency, statelessness, cgroup friendliness. |
| **0012** | Security posture | Granted-MCP-subset as Rule-of-Two, untrusted-server-content stance, SSRF defenses, gated `exec`, self-MCP hardening, secrets. |
| **0013** (deferred) | v2 surface | MCP tasks, sampling (as client), roots, Streamable HTTP serving + SSE, MCP-backed session checkpointing. |

---

## 14. Summary

`agentd` is a **small, MCP-native, reactive agent**: an instruction, a model, MCP
servers for every tool, a model-owned loop running in isolated subagent processes
that nest into a supervised tree, triggered by — and emitting — MCP resource
updates, and exposing itself as an MCP server so agents compose with the same
protocol they use for everything else. A robust, model-free **supervisor reactor**
owns lifecycle and the process tree; the **agentic loop** lives only in children;
reactivity rides **notify-then-read** resource subscriptions on stdio with
exactly-one-owner routing; reliability rides PID-1 subreaping, PDEATHSIG, a
three-detector dead/stuck model, and a bounded kill ladder. It runs from a shell
or inside a container that someone else schedules. The bet is that **the smallest
agent that speaks MCP fluently in every direction** is more useful, more
composable, and more durable than a larger one that speaks a bespoke language of
its own. The mechanisms are specified in RFCs 0002–0013; this is the front door.
