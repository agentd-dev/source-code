# agentd architecture

A readable overview for anyone who will **operate** or **extend** agentd. It
explains the one idea everything else hangs off of — the **two-loop split** —
then the concurrency model, the module map, and how a single run flows from
arguments to result.

This document is the operator/contributor-facing companion to the RFCs. The
authoritative specs are in [`rfcs/`](../rfcs/) (0001 core, 0002 reactor, 0007
agentic loop, …) and the binding decisions in
[`docs/design/00-architecture-assessment.md`](design/00-architecture-assessment.md).
Where this overview simplifies, those win.

> **Build status (2026-06):** the agent runtime is implemented — config
> validation, the agentic ReAct loop, the supervisor + subagent process tree
> (spawn/reap/liveness/kill-ladder/restart-governor), the MCP client, all five
> run modes (once/loop/reactive/schedule/workflow), the reactive router, the
> self-scheduling/self-subscribe self-tools, and the served self-MCP
> (`--serve-mcp`), with the serve-https/a2a/cron/metrics/workflow surfaces
> feature-gated. Every network surface is HTTPS (the default `tls` build); agentd
> links no unix/vsock transport. The example runs below describe live behavior.

---

## 1. What agentd is

agentd is a small, dependency-light Rust binary that runs **one agent**. You
give it an `INSTRUCTION` and a way to reach an LLM (the **intelligence**
endpoint), and it runs an agentic loop — think, call a tool, observe, repeat —
until the job is done or a new event wakes it.

Three properties define it:

- **MCP is the only tool source.** agentd ships **no built-in tool library** and
  runs no local code. Every capability comes from an
  [MCP](https://modelcontextprotocol.io) server it connects to; its only built-in
  tools are its self/control primitives (spawn a subagent, subscribe, run a graph).
- **It reacts.** agentd subscribes to MCP **resources** and treats their
  updates as triggers. A long-lived agentd can sit idle at near-zero cost and
  wake when the world it watches changes. An agentd can even subscribe *itself*
  to a resource mid-reasoning to schedule its own future wake-up.
- **It composes.** agentd is an MCP **client** to the servers it uses *and* an
  MCP **server** exposing itself, so one agent can drive another with the same
  protocol it uses for everything else — no bespoke clustering layer.

It runs standalone from a shell, as a long-lived daemon, or inside a minimal
container that an **external** scheduler (e.g. a Kubernetes operator — *not part
of this project*) starts, stops, and replicates.

---

## 2. The two-loop split (the heart of the design)

agentd is built around a deliberate separation of two loops:

| | **Supervisor loop** | **Agentic loop** |
|---|---|---|
| Lives in | the **main process** | each **subagent** child process |
| Talks to the LLM? | **never** | **always** (it is the reasoning) |
| Owns | lifecycle, config, MCP connections, triggers, subscriptions, the process tree, dead/stuck detection, reaping, limits | think → tool → observe; calling MCP tools; spawning children |
| Shape | a **reactor**: one thread blocking on a merged channel | a straight-line ReAct state machine |
| Spec | RFC 0002, RFC 0003 | RFC 0007, RFC 0009 |

The supervisor is **dumb orchestration**. It owns *when* things run and *that*
they stay healthy, but it never reasons. The agentic loop, where intelligence
lives, runs **only** inside subagent child processes that the supervisor spawns,
watches, and can kill.

### Why split it this way

Three reasons, in priority order:

1. **Cancellation = `SIGKILL`.** The only reliable way to stop runaway model
   work is to kill the process group. Async future-drop cannot stop a model
   that is wedged in a long call or spinning in a loop. Because the agentic loop
   lives in a child process with its own process group, the supervisor can
   `killpg` it instantly. This is the decisive argument and the main reason
   **tokio is rejected** — its central selling point (cooperative cancellation)
   doesn't solve agentd's actual cancel problem.
2. **Crash isolation.** Intelligence is the volatile part: it can panic, OOM, or
   run away. Isolating it in a child means a crashing or runaway agent never
   takes the supervisor down. The supervisor stays tiny and robust precisely
   because it has *no* model dependency.
3. **Minimalism.** Process isolation, nesting, hard cancel, and free
   observability (`ps`, `pstree`) all fall out of the **OS**, not from runtime
   machinery we build and audit. The process tree literally *is* the agent tree.

Subagents are the **same binary re-exec'd** in subagent mode (`argv[0]`), not a
separate artifact — one thing to ship. Every child sets
`prctl(PR_SET_PDEATHSIG, SIGKILL)` early in `main`, so if the supervisor dies the
whole tree collapses from the leaves up rather than leaking orphans (RFC 0003).

### The picture

```
                  ┌──────────────────────────────────────────────┐
  INSTRUCTION ───▶│            agentd (main process)            │
  intelligence ──▶│            = SUPERVISOR  (a REACTOR)         │
  MCP defs ──────▶│            no LLM dependency, never reasons  │
                  │                                              │
                  │  • parse + validate config (exit 2 on bad)   │
                  │  • connect MCP servers .................. as CLIENT ──┐
                  │  • serve agentd's own MCP .............. as SERVER ◀─┘
                  │  • arm triggers: once│loop│reactive│schedule │
                  │  • subscribe MCP resources ◀──── notifications/resources/
                  │  • recv_timeout(merged mpsc): ONE blocking   │   updated {uri}
                  │    wait over every reader thread + timers    │   (uri only → re-read)
                  │  • spawn + supervise subagent processes      │
                  │  • detect dead/stuck, reap, kill, restart    │
                  └───────────────┬──────────────────────────────┘
                                  │ spawn / control (OS process tree)
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
     ┌────────────────┐  ┌────────────────┐  ┌────────────────┐
     │  subagent A    │  │  subagent B    │  │  subagent C    │
     │  (process,     │  │  (process)     │  │  spawns its    │
     │   own pgroup)  │  │                │  │  own children  │
     │  AGENTIC LOOP: │  │  AGENTIC LOOP  │  │   ▼      ▼      │
     │  think→tool→   │  │                │  │   D      E      │
     │  observe→…     │  │                │  │                │
     │  + control thr │  │                │  │                │
     └───────┬────────┘  └────────────────┘  └────────────────┘
             │ tool calls (scoped MCP subset)
             ▼
   ┌──────────────────────────────────────────────────────────┐
   │  MCP servers (external): filesystem, github, db, …        │
   │  + agent's own self-MCP (subagent.*, subscribe, graph.*)  │
   └──────────────────────────────────────────────────────────┘
```

Note the two channels into each subagent: tool calls go *out* to MCP servers,
while a private **control channel** (the child's stdio pipes) carries the spawn
payload down and lifecycle/result events up. That control channel's reader runs
on a **dedicated thread** inside each subagent, decoupled from the agentic loop,
so ping/pong liveness survives even while the loop is blocked in a long
model or tool call.

---

## 3. Concurrency: thread-per-fd + an mpsc reactor (no async runtime)

The supervisor multiplexes a handful of heterogeneous I/O sources while owning a
state machine, enforcing deadlines, and staying **idle at near-zero cost**. The
sources it juggles (RFC 0002):

| Source | Count | Liveness concern |
|---|---|---|
| MCP server connections (Streamable HTTP) | ~1–8 | server hangs mid-call; emits async `resources/updated` |
| Subagent control channels | 1–50 (bounded tree) | runaway loop, deadlock, crash |
| Intelligence connection | 1 | slow/streaming, can stall mid-token |
| Timers | a few | deadline / interval / backoff / ping cadence |
| OS signals | TERM/INT/CHLD/PIPE | must drain + reap the tree |

The key insight: **scale is small and bounded** (one agent unit per process
tree; you scale by running more instances). The hard problem is *liveness*, not
throughput — robustly supervising a few things, not efficiently juggling
thousands. So the model is deliberately old-fashioned and easy to read:

- **One reader thread per long-lived readable stream.** Each MCP-server stdout,
  each subagent control-channel stdout, the intelligence connection. Each reader
  parses frames and forwards **tagged** events onto **one merged
  `std::sync::mpsc`**.
- **The supervisor is a single thread that `recv_timeout`s that merged
  channel.** The timeout *is* the timer tick — deadlines, intervals, backoff,
  and ping cadence all ride the same loop. No separate timer thread, no
  busy-poll. At idle, the thread is parked on a futex.
- **Signals** flip `AtomicBool`s **and** write one byte to a **self-pipe**, so
  the reactor wakes promptly. `SA_RESTART` is deliberately off, so blocked
  syscalls return `EINTR`. `SIGPIPE` is ignored (a write to a dead child becomes
  an `EPIPE` we handle, not a process kill).
- **Writes** go behind a per-pipe `Mutex<ChildStdin>` set `O_NONBLOCK`, fronted
  by a bounded outbound queue — a full queue is itself a *stuck* signal.

### The load-bearing invariant: abandon, don't interrupt

> The supervisor **never blocks on an untrusted source.** It reaches every pipe
> only via the `mpsc` it `recv_timeout`s, and it unblocks a parked reader **only
> by closing/killing the producer**, never by interrupting the read.

Pipes have no read timeout, and that's fine: the deadline lives at the reactor's
`recv_timeout`, and a stuck source is dealt with by making the producer go away
(close its stdout / `SIGKILL` the child), which unblocks the parked reader into
clean EOF. A misbehaving source is, by construction, one thread blocked in one
`read` that the supervisor simply ignores. This is *exactly* why thread-per-fd
is safe here without async machinery.

The reactor loop, in essence (RFC 0002 §M3):

```rust
loop {
    let timeout = next_armed_timer().unwrap_or(IDLE_TICK);  // 1s cap
    match rx.recv_timeout(timeout) {              // ONE blocking wait
        Ok(ev) => st.dispatch(ev),
        Err(Timeout) => { /* fall through to timers */ }
        Err(Disconnected) => return st.fatal_no_sources(),
    }
    while let Ok(ev) = rx.try_recv() { st.dispatch(ev); }   // batch drain
    st.timers.fire_due(Instant::now(), &mut st);            // deadlines, intervals, pings
    st.last_loop_tick = Instant::now();                     // heartbeat (idle is healthy)
    if st.draining && st.tree_is_empty() { return st.drain_exit_code(); }
}
```

**Scale check:** worst case ≈ 8 MCP readers + 50 subagent readers + 1
intelligence + 1 signal reader ≈ **60–65 threads, ~130 fds** — three orders of
magnitude inside default Linux limits. At 50 subagents the dominant cost is the
50 *child processes*, not the reader threads.

The one sanctioned exception to thread-per-fd is the optional `serve-mcp`
listener, where many *idle* peer connections would waste a thread each; there a
`mio`/`libc::poll` loop is allowed behind the `serve-mcp` feature.
The core supervision path stays thread-per-fd unconditionally.

---

## 4. Module map

The single `agentd` binary plays three roles from one artifact: supervisor
(the normal path), subagent re-exec, and the early-exit `--help`/`--version`.
The crate layout (from assessment §4.0):

```
crates/agentd/src/
  main.rs            arg parse → mode dispatch (supervisor vs subagent re-exec)
  config.rs          precedence (default<file<env<flag), validate-at-startup, exit 2
  exit.rs            the public exit-code table + terminal-status→code mapping
  json/              shared JSON-RPC 2.0 codec — wire types in ONE module
    frame.rs           NDJSON (MCP stdio) + length-prefix (control channel) framing
  wire/
    mcp.rs             MCP 2025-11-25 request/result/notification types + capability map
    intel.rs           intelligence Request/Response/Usage (+ tool-calling fields)
  net/
    http.rs            hand-rolled HTTP/1.1 + SSE client over Read+Write (+ SSRF guards)
    tls.rs             rustls/ring wiring                            [feature: tls]
    unixsock.rs        UnixStream transport
    vsock.rs           VsockStream transport                         [feature: vsock]
  mcp/
    client.rs          reader-thread + pending-request map + notification dispatch
    registry.rs        name→server-handle map; resolve(); per-server caps cache
    server.rs          self-MCP request/dispatch (tools/resources) + HTTP serving
  intel/
    client.rs          HTTPS transport (loopback http for dev) + request timeout
    openai.rs          openai-compatible adapter (+ native tool-calls)
    anthropic.rs       anthropic adapter
  agentloop/          the ReAct loop (subagent side)
    agent.rs           the turn driver
    stop.rs            terminal-status disjunction + content-hash/no-progress/repeat-cap
    context.rs         transcript + compaction levers + resource catalogue
    action.rs          native tool-call dispatch + JSON-action fallback parser
  supervisor/
    reactor.rs         the single recv_timeout loop; merged mpsc; timers
    tree.rs            Child records, parent edges, depth, budgets
    spawn.rs           re-exec subagent spawn; setpgid; pre_exec rlimit+PDEATHSIG
    reap.rs            SIGCHLD waitpid loop; subreaper; classify exit
    liveness.rs        deadline + no-progress + ping/pong; EOF×pong classifier
    kill.rs            bounded depth-first SIGTERM→SIGKILL ladder; drain budget
    restart.rs         backoff + jitter + circuit breaker + crash-on-spawn
    budget.rs          hierarchical token/step accounting to the tree root
  triggers/
    mode.rs            once/loop/reactive/schedule drivers (exit predicates)
    router.rs          reactive routing: exactly-one-owner, debounce/coalesce, queues
    timer.rs           interval + cron event source             [cron: hand-rolled 5-field UTC parser, feature]
  subagent/
    control.rs         control-channel reader thread (decoupled from loop) + ping/pong
    protocol.rs        spawn payload, control messages, upward events, result
  obs/
    log.rs             hand-rolled JSON-lines logger + LogCtx + closed event vocabulary
    health.rs          heartbeat, --health-file, /healthz+/readyz   [http surface opt-in]
    trace.rs           W3C context propagation (default) + OTLP export [feature: otel]
    metrics.rs         atomic counters → Prometheus text          [feature: metrics]
  graph/             agent-authored workflow: model + validation + driver [feature: workflow]
  sec/
    secret.rs          resolve(name) env/file front door; Debug=***
    scope.rs           tool-scope grant + Rule-of-Two tag check
  signals.rs           sigaction (no SA_RESTART) + self-pipe; SIGTERM/INT/CHLD/PIPE
```

The transport primitives (`http`/`tls`/`unixsock`/`vsock` and the MCP wire/server
framing) now live in the reusable **`crates/net`** and **`crates/mcp`** — they
retain the unix/vsock capability for reuse, but **agentd itself uses only HTTP(S)**.
There is **no `exec` module** — agentd runs no local code. The default build links
`tls` (HTTPS is the transport); `serve-https`/`a2a`/`cron`/`metrics`/`otel`/`cluster`/
`workflow` are opt-in. The default Linux build is single-digit first-party crates —
no async runtime, no C toolchain.

Two boundaries are worth internalizing as a contributor:

- **`json/` is the one place wire types live**, so swapping the JSON
  implementation (the minimalism audit may revisit `serde_json`) stays
  mechanical. The MCP codec and the control-channel codec **share** parse/
  serialize but differ in framing — HTTP bodies (+ SSE) for the Streamable-HTTP
  MCP transport, length-prefixed (4-byte LE + payload) for the private control
  channel.
- **`supervisor/` never reasons; `agentloop/` never supervises.** That wall
  mirrors the two-loop split. RFC 0002 owns *how events arrive and writes
  leave*; RFC 0003 owns *what to conclude and do about a child*; RFC 0007 owns
  what happens *inside* a turn.

---

## 5. How a run flows

A single `once` invocation, end to end:

```
1. CONFIG     parse argv + env  →  apply precedence (default < file < env < flag)
              →  validate FULLY before any side effect.
              Bad/missing config → exit 2 in milliseconds (no LLM round-trip).

2. CONNECT    for each --mcp server: connect to its remote HTTP endpoint, run the
              MCP `initialize` handshake, negotiate the protocol version, and store
              its advertised capabilities. Every later call is gated on those
              caps; every */list follows pagination cursors.
              (Optionally serve agentd's own MCP over --serve-mcp https://… with mTLS/bearer auth.)

3. SPAWN      the supervisor spawns the ROOT subagent (re-exec of argv[0]) with:
                · instruction + output contract (objective, format, boundaries)
                · a narrowed context seed (only chosen slices, never a full transcript)
                · a tool scope (a subset of the MCP servers; narrows down the tree)
                · limits (max steps / tokens / a MANDATORY finite deadline / depth)
                · a telemetry block (run_id, trace ids, agent_path) for correlation
              The child sets PR_SET_PDEATHSIG and starts its control-reader thread.

4. LOOP       inside the subagent, one ReAct turn repeats:
                assemble request  = system + instruction + output contract
                                  + context seed + transcript
                                  + scoped tool catalogue (provider `tools` field)
                                  + a compact resource CATALOGUE (URIs, no bodies)
                          │
                          ▼
                call intelligence ───────────────►  (openai-compatible or anthropic)
                          │  record usage → bump node + tree-root budgets
                          ▼
                response = text and/or tool_calls
                          │
                          ├─ tool_calls? → scope-check → route to owning server
                          │                → append result OR error as observation
                          │                (tool/exec results are the VERIFY ground truth)
                          │
                          └─ final?       → VERIFY gate → emit distilled result
                          │
                          └──────────  loop  ◄──────────┘
                until a terminal status fires (the stop disjunction below)

5. RESULT     the subagent returns a DISTILLED structured result (~1–2k tokens)
              + terminal status + usage up the control channel. For `once`, the
              supervisor maps that status to a process exit code and exits.
```

### Stopping is explicit, not "the model went quiet"

RFC 0001's original "final = the model stopped emitting tool calls" is replaced
by an explicit **terminal-status state machine** (RFC 0007). Each turn evaluates
a disjunction of cheap checks, each mapping to a **distinct** status so the
parent — and the exit code — can tell *why* it stopped:

```
completed · refused · exhausted_steps · exhausted_tokens · deadline ·
stalled · loop_detected · cancelled · crashed
```

- The global **step / token / deadline cap is a hard safety system, not a
  preference** (the lesson of a $47K runaway loop). At every soft budget the
  agent wraps up gracefully and returns a labeled *partial*; `RLIMIT`/`SIGKILL`
  from the supervisor is the backstop for a child that won't wrap up.
- **VERIFY is grounded in tool/exec results and resource state — never the model
  judging itself.** Self-critique without external ground truth reinforces blind
  spots; agentd ships no LLM-as-judge in core. The VERIFY gate checks the
  *machine-checkable* parts of the output contract (required fields present,
  declared artifacts written) and otherwise accepts the final.
- **Errors split three ways.** Tool-domain errors and malformed model output
  become **observations** the model adapts to (step-consuming). Transient
  transport errors get **bounded retry** with backoff+jitter. Fatal
  infrastructure (intelligence unreachable, auth, hard budget) **aborts** with a
  matching status. A crucial distinction: `isError:true` *inside* a successful
  tool result is an observation fed to the model; a JSON-RPC `error` is a
  transport failure handled by retry/abort policy.

### Example run

```console
$ agentd \
    --instruction "Summarize the open TODOs under /work and write SUMMARY.md" \
    --intelligence https://gw.example/v1 \
    --mcp fs=https://mcp-fs.internal/mcp \
    --max-steps 40 --deadline 600s
```

- **stdout** carries only the agent's result (a `once` run keeps stdout for its
  result; serving the self-MCP on stdout is mutually exclusive with that).
- **stderr** carries structured JSON-lines telemetry, one event per line:

```json
{"ts":"2026-06-25T10:00:00Z","level":"info","event":"proc.start","run_id":"01J…","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4120,"mode":"once","mcp_servers":1}
{"ts":"2026-06-25T10:00:00Z","level":"info","event":"mcp.connect","run_id":"01J…","agent_id":"sup","agent_path":"0","comp":"mcp","pid":4120,"server":"fs"}
{"ts":"2026-06-25T10:00:01Z","level":"info","event":"subagent.spawn","run_id":"01J…","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4120,"child":"0"}
{"ts":"2026-06-25T10:00:01Z","level":"info","event":"loop.start","run_id":"01J…","agent_id":"root","agent_path":"0","comp":"agent","pid":4121}
{"ts":"2026-06-25T10:00:03Z","level":"info","event":"tool.call","run_id":"01J…","agent_id":"root","agent_path":"0","comp":"agent","pid":4121,"server":"fs","tool":"list_directory"}
{"ts":"2026-06-25T10:00:09Z","level":"info","event":"loop.final","run_id":"01J…","agent_id":"root","agent_path":"0","comp":"agent","pid":4121,"status":"completed","dur_ms":8200}
{"ts":"2026-06-25T10:00:09Z","level":"info","event":"proc.exit","run_id":"01J…","agent_id":"sup","agent_path":"0","comp":"supervisor","pid":4120,"code":0}
```

The correlation tuple — `run_id` + `agent_path` (+ `pid`) — is the cheap
superpower: a collector reassembles the whole tree by `run_id` and queries any
subtree by `agent_path` prefix (`0`, `0.2`, `0.2.1`) with no backend join, and
`pid` joins the log tree to the free OS `pstree` (RFC 0010).

All five steps above — CONFIG, CONNECT, SPAWN, LOOP, RESULT — are implemented.
See [`docs/design/PLAN.md`](design/PLAN.md).

---

## 6. Execution modes — one loop, four exit predicates

There is **one** supervisor loop and **one** inner agentic loop. The execution
modes are not divergent code paths; they differ **only by exit predicate**
(RFC 0008). This is the load-bearing cloud-native simplification — the daemon
and the one-shot job never fork into separate engines.

| Mode | Exit predicate | Deploy shape |
|---|---|---|
| `once` (default) | first root subagent reaches a terminal status | Job, CLI |
| `loop` | a bound hit (iterations / global deadline / tree-token ceiling) or signal | Job-with-deadline or Deployment |
| `reactive` | never on its own; only signal or fatal/limit | Deployment |
| `schedule` | per-fire identical to `once`, driven by an interval/cron | external CronJob (recommended) or internal |

**Reactive mode** is the signature capability. The supervisor issues MCP
`resources/subscribe` for concrete resource URIs (gated on each server
advertising `resources.subscribe`) and then idles in `recv_timeout`. When a
server emits `notifications/resources/updated`, the reactive router maps it to
exactly one action. Two protocol facts shape this:

- **Notify-then-read.** `resources/updated` carries **only the `{uri}`** — no
  payload, no diff. The woken agentd must issue a fresh `resources/read` to learn
  what changed, acting on **current state**. The loop is two round-trips and can
  race, which is why per-route **debounce + coalesce** is mandatory, and why
  redelivery is safe (agentd converges on current state).
- **Exactly-one-owner routing.** Every `updated{uri}` matches exactly one route
  by first-match in declared order; no fan-out. A route's disposition is a fixed
  property — **spawn** a fresh root subagent per event, or **continue** into a
  warm session — never a per-event guess.

**Self-subscription = self-scheduling:** a running agentd calls the `subscribe`
self-tool, the supervisor auto-creates a `continue(this_session)` route, the
agentd ends its turn, and it is re-entered in the same session when that resource
updates.

> **Scope boundaries:**
> - **Reactivity rides Streamable HTTP.** Subscriptions are `resources/subscribe`
>   against the owning MCP server; the client holds the SSE stream open and processes
>   pushed `notifications/resources/updated` (notify-then-read).
> - **Self-MCP serving is HTTP(S)** with mTLS/bearer auth (loopback `http://` for
>   dev) — a full Streamable-HTTP server (POST + `subscriptions/listen` SSE), framed
>   by the reusable `mcp` crate.
> - **`subagent.spawn` defaults to synchronous** — it blocks the parent's turn
>   and returns the distilled result. `{async}` / `{detach}` spawns and
>   completion-as-self-resource (`agent://subagent/<handle>`) also ship.
> - **MCP tasks, sampling, and roots are deferred** to a future major
>   (RFC 0013). The current runtime
>   declares no client capabilities; it answers `roots/list` with `{"roots":[]}`
>   and rejects an unsolicited `sampling/createMessage`.

---

## 7. The two external dependencies

agentd reaches exactly two kinds of outside system, on **different wires**:

- **Intelligence (the LLM).** One minimal abstraction over **HTTPS**, named by a
  URI in `AGENT_INTELLIGENCE` / `--intelligence`: `https://…` (the default `tls`
  build), or a **loopback** `http://` for a same-host dev gateway. The
  canonical in-binary wire is **OpenAI-compatible `/chat/completions` with
  native tool-calling**; exactly two adapters ship in-binary
  (`openai-compatible` + `anthropic`), with other provider quirks pushed to the
  gateway. Credentials come from env/flags only, are never logged or persisted,
  and print as `***`.
- **MCP servers (every tool).** agentd is a client to N **remote** servers over
  **Streamable HTTP(S)** (`--mcp name=https://…`); it spawns no server process
  and runs no local code. There is no built-in tool that isn't either an MCP
  tool from one of these servers or one of agentd's own self/control tools
  (`subagent.*`, `subscribe`, `resource.read`). That invariant is the whole
  point.

---

## 8. Reliability & lifecycle (operator notes)

The supervisor is **stateless**: the per-child spawn payload is the minimum
recoverable unit, and on its own restart it **rebuilds and reconciles** rather
than persisting live state. The mechanisms an operator should know about
(RFC 0003):

- **Dead vs stuck is detected by three detectors** — a mandatory finite
  **deadline**, a **no-progress watchdog** (stale `last_event_at`), and active
  **ping/pong** answered by the child's dedicated control thread — combined with
  an EOF×pong classifier. ping/pong is the only one that distinguishes "busy in
  a long legitimate tool call" from "process wedged."
- **The kill ladder is bounded and depth-first.** On SIGTERM or a stuck verdict:
  graceful `cancel` → `killpg(SIGTERM)` after grace → `killpg(SIGKILL)` after
  kill-grace → `waitpid` until reaped. Each subagent is in its own process
  group. A second SIGTERM/SIGINT forces immediate SIGKILL of all groups.
- **PID-1 / orphan discipline.** `PR_SET_CHILD_SUBREAPER` so orphaned
  grandchildren reparent to agentd (not host init); a `waitpid(-1, WNOHANG)`
  loop on `SIGCHLD` reaps any child including unknown PIDs; `PR_SET_PDEATHSIG` on
  every child so a supervisor crash collapses the tree.
- **Nesting goes through one chokepoint.** A child creates children **only** by
  calling back into the supervisor-owned `subagent.spawn` self-tool. That is the
  single unforgeable place where finite caps (`max_depth`, breadth — per-node
  `max_children` and tree-wide `max_total` — and the tree-wide token ceiling) are
  enforced; **depth is minted by the supervisor**,
  never trusted from the child. A spawn past any cap is **refused as a tool
  result**, never a crash.

### The cloud-native contract (RFC 0011)

agentd's only obligation to an external scheduler is to be a clean citizen:

- **Config precedence** is `built-in default < config file < env var < CLI
  flag`, **fully validated at startup** → exit 2 on bad config, before any side
  effect.
- **A clean SIGTERM drain returns 0, not 143**, within a bounded budget.
  `AGENT_DRAIN_TIMEOUT` (default 25s) **MUST be less than the pod
  `terminationGracePeriodSeconds`** (the top cloud-native footgun; validated at
  startup).
- **The exit-code table is a public, machine-actionable API** (for
  `podFailurePolicy`): `0` success, `2` config/usage (non-retriable), `3`
  partial, `4` intelligence unreachable/auth, `5` semantic refusal
  (non-retriable), `6` required MCP server failed, `7` budget exceeded
  (steps/tokens/deadline), `124` supervisor hard-kill backstop (a child that
  won't self-terminate), `137`/`143` SIGKILL/SIGTERM. A one-shot maps the root
  subagent's terminal status to a code (`completed`→0, `refused`→5,
  partial→3, budget→7).
- **Health is mode-aware.** One-shot = the exit code. loop/reactive =
  **supervisor heartbeat liveness only** (a monotonic tick the reactor bumps
  every wake, *including idle waits* — idle is healthy; a stuck subagent must
  **not** flip pod liveness) plus readiness = MCP-connected and subscriptions
  reconciled. The default surface is the exit code + an optional `--health-file`
  the supervisor writes each tick.

The actual CLI/env surface is in
[`crates/agentd/src/config.rs`](../crates/agentd/src/config.rs) (run
`agentd --help`). Only flags/env vars defined there exist.

---

## 9. Security posture (in one breath)

Minimalism plus **structural isolation** is the moat — no policy engine, no
signing, no auth as core (RFC 0012). The outer boundary (container/VM/enclave)
is the sandbox; agentd does not reimplement sandboxing. Capability scoping is
the **granted MCP subset**, interpreted as a Rule-of-Two trust budget that
narrows monotonically down the subagent tree — a grant handing one subagent all
three legs of the lethal trifecta (untrusted-input + sensitive + egress) is
warned or refused without an explicit override. **All MCP server content is
treated as untrusted**, including tool descriptions and schemas. The hand-rolled
HTTP client carries SSRF defenses (block RFC-1918 / loopback / link-local by
default). There is **no `exec` tool** — agentd runs no local code. Secrets come from env/flags via a single
`resolve()` front door, never logged, never persisted, `Debug` prints `***`.

---

## 10. Where to go next

- **The thesis and the front door:** [`rfcs/0001-mcp-native-agent-runtime.md`](../rfcs/0001-mcp-native-agent-runtime.md)
- **The reactor & concurrency model:** [`rfcs/0002-supervisor-reactor-and-concurrency.md`](../rfcs/0002-supervisor-reactor-and-concurrency.md)
- **Supervision, dead/stuck detection, recovery:** [`rfcs/0003-process-supervision-and-recovery.md`](../rfcs/0003-process-supervision-and-recovery.md)
- **The agentic loop & terminal statuses:** [`rfcs/0007-agentic-loop-and-terminal-status.md`](../rfcs/0007-agentic-loop-and-terminal-status.md)
- **All RFCs:** [`rfcs/`](../rfcs/) — 0004 MCP client, 0005 self-MCP + control, 0006 intelligence, 0008 modes/routing, 0009 subagents, 0010 observability, 0011 cloud-native, 0012 security, 0013 deferred v2.
- **Binding decisions:** [`docs/design/00-architecture-assessment.md`](design/00-architecture-assessment.md)
- **Build plan & current status:** [`docs/design/PLAN.md`](design/PLAN.md)
```
