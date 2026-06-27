# Subagents & the supervised process tree

agentd is built from two kinds of process that never blur together:

- a **supervisor** — the long-lived root. It owns config, triggers, the MCP
  client connections, the process table, and every lifecycle decision. It does
  **not** reason: it carries no LLM dependency.
- one or more **subagents** — short-lived children that run the actual ReAct
  loop. A subagent is the *same binary* re-exec'd in subagent mode. The
  agentic loop, and the intelligence (LLM) calls it drives, live here and
  nowhere else.

The process tree *is* the agent tree. `pstree` shows you the real shape of a
run; `SIGKILL` works the instant a node exists; OS isolation between siblings
is free. This page covers how a subagent comes into existence, what it is
handed, what it returns, how nesting is gated, and how the supervisor decides a
child is dead or stuck and tears it down.

Authorities: **RFC 0009** (subagent process model & nesting) and **RFC 0003**
(supervision, dead/stuck detection & recovery). Where the two split: RFC 0009
owns the *spawn-boundary payload and grant-time policy*; RFC 0003 owns the
*runtime syscalls, detection, and teardown*.

> **Build status.** This is implemented. The runtime ships config validation,
> the agentic ReAct loop, the supervisor + subagent process tree
> (spawn/reap/liveness/kill-ladder/restart-governor), the MCP client, and all
> four run modes.

---

## 1. One binary, two modes

There is exactly one artifact. `main` dispatches on a non-spoofable marker
before doing any work:

```rust
fn main() -> ExitCode {
    match dispatch_mode() {
        Mode::Supervisor(cfg) => supervisor::run(cfg),  // owns the reactor (RFC 0002)
        Mode::Subagent        => subagent::run(),       // owns the ReAct loop (RFC 0007)
    }
}

// Subagent mode is selected by an env marker the SUPERVISOR sets when it
// re-execs itself — never by argv[0] string-matching alone, which a
// model-controlled instruction might try to influence.
fn dispatch_mode() -> Mode {
    if env::var_os("AGENTD_SUBAGENT").is_some() {
        Mode::Subagent
    } else {
        Mode::Supervisor(SupervisorConfig::load_and_validate()) // exit 2 on bad config
    }
}
```

When you run `agentd`, you start a **supervisor**. It re-execs itself
(`/proc/self/exe`) to create each subagent — never a bare instruction on the
command line (argv is world-readable via `ps`/`/proc`, and instructions/seeds
may carry untrusted or secrets-adjacent content). The spawn payload travels as
the **first control frame on the child's stdin**, not on argv.

Even a one-shot run is supervisor + one subagent. The supervisor spawns the
root subagent, blocks on its result, maps its terminal status to an exit code,
and exits.

```console
$ agentd \
    --instruction "summarize the open PRs and post a digest" \
    --intelligence unix:/run/intel.sock \
    --mcp github=mcp-server-github \
    --mode once
```

The loop and supervisor process tree are implemented.

---

## 2. The spawn: same-binary re-exec

The supervisor builds each child from `current_exe()` and, in order:

1. sets `AGENTD_SUBAGENT=1` (the mode marker);
2. wires three pipes — control-in on the child's **stdin**, control-out on its
   **stdout**, and **stderr** for the child's own telemetry;
3. applies `pre_exec` hooks (RFC 0003 §3.9): `setpgid(0,0)` (own process
   group, so the subtree is addressable by `killpg`), `setrlimit(RLIMIT_AS)`,
   `setrlimit(RLIMIT_CPU)`;
4. writes the **spawn payload** as the first length-prefixed JSON-RPC control
   frame on the child's stdin.

The re-exec'd child's **early `main`** — before any loop work — arms its
own orphan discipline. `PR_SET_PDEATHSIG` is cleared across `execve`, so it
must be re-set here, in the child, not in `pre_exec`:

```rust
// child's early main, before any loop work
unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0); }
// close the fork→prctl race: if the parent already died, self-terminate.
let expected_ppid: i32 = env_supervisor_pid();
if unsafe { libc::getppid() } != expected_ppid {
    std::process::exit(137);
}
// then emit ctrl/ready as the first upward frame
```

The child then emits a `ctrl/ready` frame. A child that exits *before* `ready`
within `AGENTD_SPAWN_READY` (2s) is a **crash-on-spawn** — the fork-bomb early
warning (see §7).

---

## 3. The spawn payload (rich, not minimal)

A bare instruction string is **rejected** at the chokepoint as malformed. Every
child is handed a structured payload: an output contract, a narrowed context
seed, a tool scope, limits, and a telemetry block.

```rust
struct SpawnPayload {
    // identity & tree position — ALL minted by the supervisor (§4)
    agent_id:   AgentId,        // e.g. "0.2.1"
    agent_path: String,         // dotted tree path
    depth:      u16,            // caller.depth + 1; any child-supplied value is ignored

    contract:  OutputContract,  // objective + required format + tool guidance + boundaries
    seed:      ContextSeed,     // ONLY chosen slices — never the parent's transcript
    scope:     ToolScope,       // a SUBSET of the parent's grant
    limits:    Limits,          // steps / tokens / deadline / breadth, clamped to tree budget
    telemetry: Telemetry,       // run_id, trace_id, parent_span_id, log level
}
```

### Output contract

```rust
struct OutputContract {
    objective:     String,         // the specific goal; EMPTY → spawn refused
    output_format: OutputFormat,   // Text | Json(schema) | Markdown
    tool_guidance: Option<String>, // which tools/sources to prefer, and how
    boundaries:    Option<String>, // what's in scope, what NOT to do, when to stop
}
```

A vague delegation ("go figure it out") reproduces a well-known failure: the
child duplicates work, misses requirements, or returns an unusable blob. The
contract forces an objective and a required shape. A payload with an empty
`objective` comes back to the parent as a tool-result error
(`spawn denied: missing objective/output-contract`) — not a silent accept.

### Narrowed context seed

```rust
struct ContextSeed { slices: Vec<SeedSlice> }
struct SeedSlice { label: String, content: SeedContent }
enum SeedContent { Text(String), ResourceRef(String) } // agentd:// or a server uri
```

The parent passes **only the slices it chooses** — a `file_path` here, an `id`
there, a `sub_goal`. There is deliberately **no field for the parent's full
transcript**. This is two things at once:

- **context hygiene** — the child's window stays clean, which is half the point
  of delegating;
- **an injection firewall** — a child that reads untrusted content receives only
  what it was told and returns a *distilled* summary, so raw untrusted bytes
  never cross back into a parent that holds sensitive or egress-capable tools.

### Tool scope (narrows monotonically)

```rust
struct ToolScope {
    allow: Vec<ToolRef>,  // (server, tool) pairs — a SUBSET of the parent's grant
    tags:  ToolTags,      // { untrusted_input, sensitive, egress } — Rule-of-Two (RFC 0012)
}
```

`scope.allow` must be a subset of the caller's own granted scope. The chokepoint
**intersects** — never unions — so capability can only narrow as you descend the
tree (§5). Any tool the parent does not itself hold is refused.

### Limits

```rust
struct Limits {
    max_steps:    u32,       // per-subagent step ceiling (default 30–50)
    max_tokens:   u64,       // token grant, carved from the tree budget
    deadline:     Duration,  // wall-clock — MANDATORY, never infinity
    max_children: u16,       // this node's breadth cap (§6)
}
```

`max_tokens` and `deadline` are **clamped at grant time** to whatever the tree
budget can still afford (RFC 0003 owns the accounting). A deadline of "none" is
not expressible.

### Telemetry

`run_id` (stable ULID across the whole tree), `trace_id` (W3C), optional
`parent_span_id`, log level, and `log_content`. Each child self-logs already
correlated by `run_id` + `agent_path` (RFC 0010), so a tree's log lines stitch
together without a collector.

---

## 4. Supervisor-minted depth & identity (the trust boundary)

`depth`, `agent_id`, `agent_path`, and the parent edge are **minted by the
supervisor from its own supervision record** — never read from the child's
request:

```rust
// inside the subagent.spawn handler, running IN the supervisor
fn handle_spawn(caller: Handle, req: SpawnRequest) -> ToolResult {
    let parent = self.tree.get(caller);   // authoritative record
    let depth  = parent.depth + 1;        // MINTED here
    // req.depth / req.agent_path / req.parent (if present) are DISCARDED.
    // ...
}
```

A child cannot lie about its depth because it never mints the value. This is
what makes the depth cap **unforgeable**: the only way to reach a higher depth
is to actually be that deep in a supervisor-tracked tree.

---

## 5. Nesting only through `subagent.spawn`

There is **exactly one** way to create a subagent: the supervisor-owned
`subagent.spawn` self-tool (the wire is RFC 0005). There is no fork from inside
the loop, no model-controlled `Command`, no spawning from a tool result. That
one chokepoint is where every cap is enforced, and adding a second spawn path is
a permanent non-goal.

Because `subagent.spawn` is served by the supervisor (it owns the process
table), the supervisor sees every request, mints the trusted fields, narrows the
scope by intersection, clamps the limits, and only then re-execs. The Rule-of-Two
trifecta is **not** re-evaluated here: it is enforced once, at startup, over the
root grant (RFC 0012; `--allow-trifecta` to override). Because scope only narrows
as you descend, a child's tag union can never exceed the root's, so that single
root check already bounds the whole tree.

`exec` (the gated shell tool, off by default) is folded into the **same
accounting path** — it counts against the same breadth/budget caps — even
though it has no control channel. It is not a fork-bomb bypass around the
chokepoint (§7).

---

## 6. Caps — refused as tool results, never crashes

The chokepoint enforces four caps. A violation comes back to the parent's model
as a normal MCP tool result with `isError: true` — an **observation it can
adapt to**, never a JSON-RPC protocol error and never a crash:

| Cap | Default | Scope | Refusal |
|---|---|---|---|
| `max_depth` | **4** (range 3–5) | tree | `spawn denied: max_depth N reached` |
| `max_children` | 8 | per node | `spawn denied: node child cap reached` |
| `max_total_subagents` | 64 | tree-wide | `spawn denied: tree subagent cap reached` |
| tree-token ceiling | from budget | tree-wide | refuse new spawns + new model calls; drain |

`max_depth` is the one you set directly on the CLI — `--max-depth N`
(or `AGENTD_MAX_DEPTH`-style via the env layer), default **4** per `config.rs`.
The others are RFC-level chokepoint defaults.

```rust
fn handle_spawn(caller: Handle, req: SpawnRequest) -> ToolResult {
    if self.tree.draining { return tool_err("spawn denied: tree draining"); }
    let parent = self.tree.get(caller);
    let depth = parent.depth + 1;
    if depth > self.caps.max_depth        { return tool_err("spawn denied: max_depth"); }
    if parent.children.len() as u16 >= parent.limits.max_children {
        return tool_err("spawn denied: node child cap");
    }
    if self.tree.total >= self.caps.max_total_subagents {
        return tool_err("spawn denied: tree subagent cap");
    }
    if self.tree.root_tokens >= self.caps.tree_token_ceiling {
        return tool_err("spawn denied: tree token ceiling");
    }
    // ... mint depth/identity, clamp limits, narrow scope, re-exec ...
}
```

A wedged or runaway child that keeps hammering `subagent.spawn` therefore just
keeps getting refusals — it **cannot** fork-bomb, because the only spawn path is
this one chokepoint and the absolute depth/breadth/total/token caps bound it. The
**tree-draining flag** makes `subagent.spawn` error during teardown, so a parent
cannot spawn replacements mid-kill-ladder.

---

## 7. The result: a distilled, structured value

The child runs to a terminal status (RFC 0007's closed enum) and returns a
**distillate** — roughly 1–2k tokens — as its final upward control frame:

```rust
struct SubagentResult {
    status:     TerminalStatus, // completed | refused | exhausted_steps | exhausted_tokens
                                // | deadline | stalled | loop_detected | cancelled | crashed
    distillate: ResultBody,     // what the parent appends to ITS context
    usage:      Usage,          // { tokens_in, tokens_out, steps }
}

enum ResultBody {
    Inline(serde_json::Value),               // small structured/text result
    Reference { uri: String, summary: String }, // store-and-reference for large output
}
```

The parent appends the **distillate, never the child's transcript**. This is
structural, not a convention: the control channel only ever carries a
`SubagentResult` upward, and the child's transcript never leaves the child
process. There is no protocol path for a raw transcript to flow up.

**Store-and-reference.** When the result would blow the ~1–2k budget, the child
writes the bulk to a resource (a scoped MCP tool, or an `agentd://`
self-resource) and returns `Reference { uri, summary }`. The parent appends the
`summary` and reads `uri` only if it actually needs the bulk — the coordinator's
window stays lean.

If a JSON `output_format` schema is declared and the distillate doesn't satisfy
it, the supervisor surfaces the validation failure *inside* the tool result (so
the parent's model can adapt), with `status` preserved. It does not crash the
parent.

---

## 8. Sync by default; async and detach also ship

`subagent.spawn` takes a disposition. **Sync is the default; async and detach
also ship.**

```rust
enum Disposition { Sync, Async, Detach } // default = Sync
```

- **`Sync` (default).** The tool call blocks the parent's *turn* until the
  child reaches a terminal status, then returns the `SubagentResult`. The
  parent's loop is between turns, so the parent process is cheaply paused — no
  orphan management, a deterministic mental model. (Implementation note: the
  reactor holds the pending JSON-RPC response; it does **not** block the reactor
  thread — RFC 0002.) If the child is killed instead of completing, the call
  returns whatever terminal status the kill produced (`deadline`, `cancelled`,
  `crashed`).

- **`Async`.** Returns the `handle` immediately; the parent keeps reasoning and
  later calls `subagent.await` (waits for it) or peeks with `subagent.status` /
  `resource.read agentd://subagent/{handle}` — the child's completion *is* an
  update on that `agentd://subagent/{handle}` resource (the URI is derived from
  the handle; there is no separate result resource). Bounded by `max_inflight`
  (default 4).

- **`Detach`.** Fire-and-forget. The child **still** counts
  against the tree budget, depth cap, and breadth cap, and is **still reaped**.
  Use sparingly.

Async and detach reuse the subscribe/notify machinery they share with
reactivity — the same machinery is built and live. Streaming a child's partial output into
the parent's reasoning is out of scope for v1: the child always streams loop
events up the control channel for *supervision and observability*, but those
partials never enter the parent's context — only the final distillate does.

---

## 9. The three-detector dead/stuck model

A live PID is not a live agent. The supervisor **actively probes** liveness; it
never assumes it from PID existence. Three detectors run against every child,
each catching a failure the others can't:

### Detector A — hard deadline (no child cooperation)

Every child carries a mandatory, finite `deadline: Instant`, minted at spawn
from its limits. Default `AGENTD_CHILD_DEADLINE = 600s` for subagents;
`exec` children get `AGENTD_EXEC_DEADLINE = 120s`. The reactor arms its
`recv_timeout` to the nearest deadline across all live children. On expiry: the
verdict is `deadline`, the kill ladder runs on that child's subtree. This is the
floor under everything else — it catches "runs forever" unconditionally.

### Detector B — no-progress watchdog (liveness without cooperation)

Every control frame stamps `last_event_at`. If a live child emits nothing for
longer than `AGENTD_PROGRESS_TIMEOUT` (default 120s, ≈ 2× the model request
timeout) it is declared stuck (`StuckReason::NoProgress`). It reuses the
existing event stream — no new wire — and fires even if the child's control
thread is *also* wedged.

### Detector C — active ping/pong on a decoupled thread

The only detector that tells *busy-in-a-long-legitimate-tool-call* apart from
*process wedged*. Inside each subagent the control reader runs on a **dedicated
thread, decoupled from the agentic loop** — so it can answer pings while a
model or tool call is in flight. The supervisor pings every
`AGENTD_PING_INTERVAL` (5s):

```jsonc
// downward, length-framed JSON-RPC (RFC 0005)
{"jsonrpc":"2.0","method":"ctrl/ping","params":{"seq": 42}}
// the child's control thread replies immediately, never touching the loop:
{"jsonrpc":"2.0","method":"ctrl/pong","params":{"seq": 42}}
```

After `AGENTD_PING_MISS = 3` consecutive unanswered pings, the child is declared
stuck (`StuckReason::PongTimeout`) — its control thread is wedged or the process
is in uninterruptible `D` state.

### The EOF × pong classifier

EOF on the child's stdout (`read()` returns 0) is **necessary but not
sufficient** for death — the child may have closed stdout yet still be alive or
in `D`. The supervisor combines the axes:

| | **pong flowing** | **pong stopped** |
|---|---|---|
| no EOF, events flowing | `Healthy` | (transient — wait for B/C) |
| no EOF, no events | `BusyHealthy` (long tool call) | `Stuck` (wedged / `D`-state) |
| EOF | `Exiting` → `waitpid` confirms | `Dead` → `waitpid` confirms |

`Exiting`/`Dead` always confirm with `waitpid` before mutating the tree — EOF
alone never declares death. `BusyHealthy` is **left alone** — that's the whole
reason Detector C exists: not to false-positive-kill a child doing legitimate
slow work.

`exec` children have **no** control channel, so only Detector A (deadline) and
dead-detection apply — no ping/pong, no no-progress stream. An `exec` child
cannot be distinguished "busy vs wedged"; it is bounded purely by its deadline
and the kill ladder.

---

## 10. Orphans, reaping, and the kill ladder

### PDEATHSIG + subreaper — keep the tree in our domain

Two mechanisms ensure no child ever escapes the supervisor's reaping/kill
domain:

- **`PR_SET_CHILD_SUBREAPER`** — the supervisor sets this at startup, so a
  grandchild orphaned by a dying subagent reparents to the supervisor, not to
  host PID 1. (If the supervisor is itself PID 1 — the recommended container
  entrypoint — this is moot; agentd is a tini-class init for its own tree and
  needs no external `tini`.)
- **`PR_SET_PDEATHSIG = SIGKILL`** in every child's early `main` (§2). If the
  supervisor dies, the kernel collapses the tree from the leaves up
  automatically. Without PDEATHSIG, "in-memory only" silently means *orphan
  leak* — so it is non-optional.

Reaping is a `SIGCHLD` self-pipe waking a `waitpid(-1, WNOHANG)` loop that drains
every ready child — **including unknown reparented grandchildren** (logged as
`subagent.exit` with `orphan:true`). `SIGPIPE` is ignored at startup so a
`write()` to a just-dead child can't kill the supervisor.

### The bounded kill ladder

Triggered by a drain signal, a Detector A/B/C verdict on a subtree, or a budget
breach. Each subagent is its own process group, so a subtree is signalled
atomically with `killpg`. Order is **depth-first, deepest-first** — leaves before
roots — and the tree-draining flag goes up first so no parent spawns
replacements mid-teardown:

```
ctrl/cancel  →  (DRAIN_GRACE 5s)  →  killpg(SIGTERM)  →  (KILL_GRACE 2s)  →  killpg(SIGKILL)  →  waitpid to ECHILD
```

The per-subtree budget is `DRAIN_GRACE + KILL_GRACE` (~7s nominal).
`--drain-timeout` (default **25s** per `config.rs`) caps the **whole tree** and
is validated at startup to be `< terminationGracePeriodSeconds` (RFC 0011). A
**second** SIGTERM/SIGINT sets `force` and collapses straight to
`killpg(SIGKILL)` of all groups — for operator impatience or orchestrator
escalation.

A `D`-state process cannot be killed even by `SIGKILL`. The supervisor does
**not** hang on it: any PID that never reaps inside the budget is logged as a
`stuck-leak` metric and the supervisor exits with a distinct unclean-drain code
(143). Detect and report, don't hang.

---

## 11. The restart governor

Restarts apply **only** to `loop` / `reactive` modes and reactive-session-backing
children — **never a one-shot root** (one-shot means one attempt). The governor
is the *temporal* control over restarts; the *structural* fork-bomb caps live at
the spawn chokepoint (§6). They are complementary.

- **Exponential backoff + jitter, capped** — base 500ms, cap 30s. A
  session-backing child is not respawned before its backoff expires.
- **Circuit breaker** — more than 5 failures inside a 60s window opens the
  breaker for that handle: stop respawning, mark the session **failed** (a
  watcher/operator sees it via `subagent.status` on the handle), and **drop
  routed reactive events** that would target the broken session. Don't spawn into
  a known-bad loop.
- **Crash-on-spawn fast-fail** — a child that exits before its `ctrl/ready`
  frame within 2s is weighted 3× heavier toward the breaker (the fork-bomb early
  warning).
- **Success ≠ failure** — clean exit 0 with a received `final` result does not
  count against the breaker; only non-zero exit, signal death, or a stuck-kill
  does.

---

## 12. Hierarchical token accounting & rebuild

### Tree-token ceiling (O(1) per event)

Each subagent reports per-turn `usage{tokens, steps}` in its control events. The
**supervisor is the source of truth** — a child can't under-report past a cap it
doesn't enforce. Per-event, the supervisor rolls usage into the node counter and
the **single tree-root counter**:

- a node over its own grant → kill just that subtree, spare its siblings;
- the root over the tree-token ceiling → drain the whole tree, exit code 7.

Per-process `RLIMIT_AS` / `RLIMIT_CPU` (set in `pre_exec`) cap a single runaway
cheaply. **Honest caveat:** `setrlimit` is per-process; it does **not** bound
*aggregate subtree memory*. Only the **token** ceiling is enforced in-binary.
Aggregate memory is a cgroups-v2 / deployment concern (agentd is cgroup-*aware*,
not cgroup-*requiring*) — size your pod's `resources.limits` for the whole tree,
not per child.

### Rebuild + reconcile (supervisor restart)

The supervisor is **stateless**. The minimum it retains for a child's lifetime
is the child's spawn payload (instruction + seed + scope + limits + accumulated
usage), enough for a bounded restart. On a *process* restart (new pod, in-memory
state gone) it: re-reads and validates config (exit 2 on bad config),
re-establishes MCP connections, re-issues every **declared** subscription, and —
**mandatory** — does a `resources/read` immediately after each
`resources/subscribe`:

```
for sub in declared_subscriptions {
    client.subscribe(sub.server, &sub.uri)?;       // re-arm
    let cur = client.read(sub.server, &sub.uri)?;  // MANDATORY: synthesize one event
    router.deliver_synthetic_updated(&sub.uri, cur);
}
```

This read-after-subscribe converts edge-triggering into level-triggering across
the restart boundary: any change that happened while the supervisor was down is
recovered, because the agent acts on *current state*, not a missed delta. Warm
sessions and dynamic self-subscriptions are **lost** in v1 — recovered by
idempotent re-trigger (`--run-id` / `AGENTD_RUN_ID`), not by resurrection.
Durable warm-session checkpointing is deferred to v2 (RFC 0013).

---

## 13. Quick reference

Knobs that exist on the CLI/env surface today (`config.rs`):

| Flag | Env | Default | Effect |
|---|---|---|---|
| `--max-depth N` | (env layer) | 4 | subagent tree depth cap |
| `--max-steps N` | `AGENTD_MAX_STEPS` | 50 | per-run step cap |
| `--max-tokens N` | `AGENTD_MAX_TOKENS` | 200000 | token budget |
| `--deadline <dur>` | `AGENTD_DEADLINE` | 600s | wall-clock deadline |
| `--drain-timeout <dur>` | `AGENTD_DRAIN_TIMEOUT` | 25s | whole-tree drain budget (`< pod grace`) |
| `--mode <m>` | `AGENTD_MODE` | once | `once` / `loop` / `reactive` / `schedule` |
| `--run-id <id>` | `AGENTD_RUN_ID` | generated | idempotency key for re-trigger |

RFC-level chokepoint and detector defaults (not CLI flags in v1):
`max_children` 8, `max_total_subagents` 64, `tree_token_ceiling` 2,000,000,
`AGENTD_CHILD_DEADLINE` 600s, `AGENTD_EXEC_DEADLINE` 120s,
`AGENTD_PROGRESS_TIMEOUT` 120s, `AGENTD_PING_INTERVAL` 5s, `AGENTD_PING_MISS` 3,
`AGENTD_SPAWN_READY` 2s, `DRAIN_GRACE` 5s, `KILL_GRACE` 2s.

See RFC 0009 and RFC 0003 for the binding specifications, and
`docs/design/PLAN.md` for what ships in which milestone.
