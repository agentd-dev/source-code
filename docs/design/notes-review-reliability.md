# Reliability & Process-Supervision Review of RFC 0001

**Reviewer lens:** reliability + process supervision.
**Target:** `rfcs/0001-mcp-native-agent-runtime.md` (the rewrite target).
**Date:** 2026-06-25.
**Verdict:** The process model is the right shape (OS process tree, supervisor with no LLM
dependency, SIGKILL-able children), but RFC 0001 treats supervision as a one-line
responsibility ("spawn, track, reap, enforce limits") and **under-specifies every hard part**.
This document enumerates the concrete failure modes and gives a minimal, specific mechanism
for each. The bias throughout matches the RFC's: no async runtime, `std` + `libc` only,
data structures small enough to audit by reading.

The existing retired code (`crates/agentd/src/...`) is only a parts bin. Where it has prior
art it is **insufficient** for the new model:
- `mcp/client.rs` `Drop` does `child.kill(); child.wait()` — SIGKILL only, no group, no
  graceful term, blocking wait in Drop.
- `tools/shell.rs` busy-polls `try_wait()` every 10ms and `child.kill()`s on timeout —
  single child, no group, no SIGTERM→SIGKILL escalation.
- `signals.rs` flips an `AtomicBool` on SIGTERM — no drain orchestration, no SIGCHLD handling.
- `budget.rs` applies `setrlimit` to **the whole process** — wrong granularity for per-subtree
  accounting once there are many children.
- `engine/checkpoint.rs` writes per-run JSON — a usable base for session checkpoints but
  models the retired DAG, not warm reactive sessions.

None of these solve a *tree*. The sections below are what the RFC must add.

---

## 1. The supervisor must be a real reactor, not a busy-poll loop

RFC §3 describes "the supervisor loop" abstractly. With process children, stdio control
channels, MCP server pipes, MCP notifications, timers, and signals all needing attention
simultaneously, the supervisor needs **one blocking wait that wakes on any source**. The RFC
names no I/O multiplexing primitive. Without one you get either busy-polling (burns CPU while
"idling at near zero cost" — directly contradicts §5.3) or a thread-per-child sprawl with
lock contention and no single place to enforce tree-wide invariants.

**Mechanism (minimal, no async runtime):** a single supervisor thread blocked in
`poll(2)`/`epoll` (Linux) over a fixed fd set:
- each subagent's control-channel read fd (stdout pipe),
- each MCP client's notification fd (for `notifications/resources/updated`),
- a **self-pipe** written by the SIGCHLD/SIGTERM handlers (the classic self-pipe trick —
  async-signal-safe `write()` of one byte wakes the poll),
- a **timerfd** (Linux) or computed `poll` timeout for the nearest deadline/interval.

Signal handlers stay trivial (write one byte to the self-pipe; the existing `AtomicBool`
pattern in `signals.rs` is fine as a backstop but the self-pipe is what wakes the reactor
promptly). Everything else — reaping, deadline expiry, drain — is handled on the main thread
where it can mutate the tree without locks. This is the single most important structural
addition the RFC omits.

Writes to child stdin (control messages) can block if a child is wedged; never write from the
reactor thread without `O_NONBLOCK` on the child's stdin and a bounded outbound queue per
child (see §4, stuck detection). A full outbound queue is itself a stuck signal.

---

## 2. Crash isolation: what actually happens when a child dies

RFC §3/§4.2 assert "intelligence is always isolated in a child that can crash without taking
the supervisor down." True for a clean crash, but the RFC never specifies the failure
*surface* between supervisor and child. Concrete under-specified modes:

1. **SIGPIPE.** Supervisor writes a control message to a child that just died → `write()`
   raises `SIGPIPE`, default action terminates **the supervisor**. This is the classic way a
   "crash-isolated" supervisor dies with its child. **Fix:** `signal(SIGPIPE, SIG_IGN)` at
   startup (one line, `libc`), handle `EPIPE` as a normal "child gone" event.

2. **Partial / interleaved control frames.** A child can die mid-line, leaving a half-written
   JSON frame in the pipe. The reactor must treat a parse error on the control channel as a
   **protocol fault → terminate that child**, not as a recoverable skip. The old
   `mcp/client.rs` `rpc_call` loop instead loops forever waiting for a valid line; a wedged or
   garbage-emitting child would hang the caller. The control channel needs a **per-frame read
   timeout** and a hard cap on un-parseable bytes.

3. **EOF semantics.** Pipe `read()` returning 0 (EOF) means the child closed stdout — usually
   because it exited. EOF is necessary but **not sufficient** to declare death: the child may
   have closed stdout but still be alive (or stuck in `D` state). EOF must trigger a
   `waitpid` to get the real exit status, and a bounded wait before escalation (§4).

4. **Slow/blocked child stdout consumer.** If the supervisor stops reading a child's stdout
   (e.g. busy reaping another subtree), the child blocks on `write()` to a full pipe and
   *looks* stuck though it is healthy. The reactor's poll-driven design (§1) plus draining
   every readable fd each wake prevents this; document it as an invariant: **the supervisor
   must never let a child's stdout pipe fill.**

**Data the supervisor keeps per child** (the core supervision record):
```
struct Child {
    pid: Pid,
    pgid: Pid,                 // its own process group (see §5)
    handle: Handle,            // opaque id used in self-MCP tools
    parent: Option<Handle>,    // tree edge
    depth: u16,                // for recursion budget (§7)
    state: ChildState,         // Spawning|Running|Draining|Killing|Exited
    ctrl_in: Fd,  ctrl_out: Fd,
    last_event_at: Instant,    // for no-progress detection (§4)
    last_ping_seq: u64, last_pong_seq: u64, // liveness (§4)
    deadline: Instant,         // hard wall-clock kill time
    restarts: RestartHistory,  // for backoff/circuit-break (§6)
    budget: SubtreeBudget,     // tokens/steps/children, charged upward (§7,§8)
    outbound: VecDeque<Frame>, // bounded; full == stuck signal
}
```
A flat `HashMap<Pid, Child>` plus `HashMap<Handle, Pid>` is enough; the parent edges make it a
tree. No fancy structure required.

---

## 3. Reaping zombies — agent as PID 1 is a first-class scenario the RFC ignores

RFC §11 deploys agent in a container an operator starts. In a container agent is frequently
**PID 1**, and the RFC says nothing about it. Two distinct duties:

1. **Reap your own children.** A child that exits but is not `waitpid`'d becomes a zombie
   holding a PID and its slot in the supervision table. The RFC's "reap exits" must be wired
   to **SIGCHLD → self-pipe → reactor calls `waitpid(-1, WNOHANG)` in a loop** until it
   returns 0/`ECHILD`. Reaping in a loop is mandatory: multiple children can exit between two
   wakes and SIGCHLD does not queue.

2. **Reap *grandchildren* you did not spawn.** This is the PID-1 trap. A subagent (child)
   spawns its own children (grandchildren), then the subagent dies. Those grandchildren are
   **reparented to PID 1 = the supervisor**. If the supervisor only `waitpid`s for PIDs in its
   table, reparented grandchildren become un-reaped zombies forever, and any *orphaned but
   alive* grandchild keeps running untracked (a resource + safety leak — it may still hold MCP
   connections and burn tokens). Also: MCP servers spawned as stdio children (§7.1) are
   themselves grandchildren-spawning processes in the general case.

**Fix:**
- The supervisor's SIGCHLD handler **must `waitpid(-1, WNOHANG)` for *any* child**, not only
  known PIDs. Unknown reaped PIDs are logged and discarded.
- When agent detects it is PID 1 (`getpid() == 1`), it enables **subreaper-equivalent
  behavior by default**. On non-PID-1 deployments, call
  `prctl(PR_SET_CHILD_SUBREAPER, 1)` so that grandchildren orphaned by a dying subagent
  reparent to **agent**, not to the host's init — keeping the whole tree inside agent's
  reaping domain even when it is not PID 1. This is the single most important PID-discipline
  addition. (`prctl` is a `libc` call, no dependency.)
- Document that the recommended container entrypoint is agent itself (it *is* a tini-class
  init for its tree); we do **not** require an external `tini`, because we reap properly.

Without `PR_SET_CHILD_SUBREAPER`, killing a subtree (§5) cannot be guaranteed: orphaned
grandchildren escape the process group of the subagent if the subagent created new groups, and
even if not, the supervisor loses the parent edge needed to find them.

---

## 4. Dead vs. stuck — the detection the RFC completely omits

RFC §1(8) demands "detect dead/stuck subprocesses." RFC body provides **zero mechanism**.
This is the central reliability gap. "Dead" and "stuck" need different detectors:

### 4.1 Dead (process gone)
- Signal: SIGCHLD → `waitpid` returns the PID with a status. Authoritative. Combined with
  pipe EOF. This is easy and the RFC implicitly relies on it.
- Edge case: a child can be **alive but its loop is finished and it's blocked in `read()`**
  waiting for the next control message — that's *idle*, not dead. Distinguish by state
  machine, not by liveness.

### 4.2 Stuck (alive but not making progress) — three independent detectors, defense in depth

A subagent can hang in several ways the OS will never tell you about:
- spinning in a tool call to a wedged MCP server,
- blocked on a model call that never returns (gateway black-hole),
- deadlocked on its own internal lock,
- in uninterruptible `D` state on a stuck syscall (NFS, etc.) — note: **`D`-state cannot be
  killed even by SIGKILL**; only escalation to killing the blocking resource or the node
  helps. The supervisor must at least *detect and report* it rather than hang waiting.

**Detector A — hard deadline (cheapest, always on).** Every child carries an absolute
`deadline: Instant` (from its `limits`, RFC §4.2/§10). The reactor's timerfd is armed to the
nearest deadline across all children. Deadline expiry → graceful-kill that child's subtree
(§5). This catches the "runs forever" case unconditionally and needs no cooperation from the
child. **Make a deadline mandatory** — RFC §10 lists `--deadline` but does not require it;
a child with no deadline is an un-bounded liability. Default to a finite value, never infinity.

**Detector B — no-progress watchdog (liveness without cooperation).** Every loop turn already
streams events upward (RFC §6.1: thought/tool-call/tool-result/final). The supervisor stamps
`last_event_at` on every received frame. If `now - last_event_at > progress_timeout`
(e.g. 2× the model/tool timeout, configurable), the child is **stuck**. This reuses the
existing event stream — no new wire mechanism — and catches "alive but silent." The
`progress_timeout` must exceed the longest legitimate single tool/model call, so this is a
coarse net; Detector C makes it precise.

**Detector C — active liveness ping over the control channel (distinguishes stuck-in-tool
from stuck-in-supervisor-loop).** The supervisor periodically sends `ctrl: ping(seq)` on the
child's stdin; the child's control reader (which must run on a **separate thread from the
agentic loop** inside the subagent) replies `pong(seq)` immediately. Track
`last_ping_seq`/`last_pong_seq`. Missing N consecutive pongs (e.g. N=3, interval 5s) ⇒ the
child's *control thread* is wedged or the process is in `D`-state ⇒ stuck, escalate. This is
the only detector that works when the child is mid-`tools/call` and legitimately producing no
loop events for a long time but is otherwise healthy — pong still answers. **Design
requirement the RFC must state:** the subagent's control-channel handler runs on a dedicated
thread, decoupled from the agentic loop, precisely so liveness survives a stuck tool call.

**Why all three:** Deadline bounds total cost (policy). No-progress catches silent hangs with
zero child cooperation (works even if the control thread is also wedged). Ping/pong
distinguishes "busy in a long tool call but healthy" (pongs continue) from "process wedged"
(pongs stop) — preventing the false-positive kill of a child doing legitimate slow work.

EOF vs no-progress summary: **EOF = channel closed (likely dead, confirm with `waitpid`);
no pong + no EOF = stuck-alive; events flowing = healthy; nothing flowing but pongs flowing =
busy-healthy.** This 2×2 (EOF? × pong?) is the core liveness classifier and should be written
explicitly into the supervisor.

---

## 5. Graceful drain + bounded kill of a whole subtree (SIGTERM and deadline paths)

RFC §4.1/§6/§11 say "graceful drain → kill tree" and "can SIGKILL any subtree" but never say
**how a subtree is addressed or escalated**. With process children this is easy to get wrong.

### 5.1 Addressing the subtree: process groups
On spawn, put each subagent in its **own process group** via `setpgid` (or `pre_exec`
`setsid` for the root subagent). Then a subtree can be signalled atomically with
`killpg(pgid, SIG)`. **But** a child may create its own new groups for *its* children, so
`killpg` of one group is not guaranteed to reach grandchildren. Two complementary defenses:
- `PR_SET_CHILD_SUBREAPER` (§3) keeps orphans reparenting to agent so none escape.
- The supervisor walks its own tree (parent edges in the `Child` table) and signals each
  group depth-first. Combining "walk the table" with `killpg` per node covers both the
  cooperative and the orphaned case.

### 5.2 Escalation ladder (the bounded part the RFC omits)
On SIGTERM-to-agent, or a child deadline/stuck verdict, run a fixed, time-bounded ladder per
target subtree:
```
t0:            send ctrl:cancel (graceful) to the subagent; mark Draining
t0+grace:      killpg(pgid, SIGTERM)        // grace ~ 5s, configurable
t0+grace+kill: killpg(pgid, SIGKILL)        // kill ~ 2s after SIGTERM
then:          waitpid each pid until reaped or ECHILD; log any that
               never reaped (D-state) as a stuck-leak metric
```
The **total drain budget is bounded** (`grace + kill`, default ~10s, must be < the
orchestrator's `terminationGracePeriodSeconds`, RFC §11). The supervisor exits with a distinct
code if any process could not be reaped (so the orchestrator/operator sees an unclean drain).
This is the "bounded kill" the RFC names but does not define. **Order matters:** cancel
deepest children first (leaves before roots) so a parent doesn't spawn replacements while you
are tearing down — i.e. depth-first, deepest-first, and set a tree-wide "draining" flag that
makes `subagent.spawn` (§8) return an error during drain.

### 5.3 The double-fault case
What if the supervisor itself is mid-drain and receives a *second* SIGTERM/SIGINT (operator
impatience, or orchestrator escalating)? Second signal ⇒ **skip remaining grace, go straight
to SIGKILL of all groups, then exit.** Implement as: first signal sets `draining`, second
signal sets `force` — the ladder checks `force` and collapses to immediate SIGKILL.

---

## 6. Restart, backoff, circuit-breaking of flapping subagents

RFC §4.1 lists "enforce limits" but says nothing about **restart policy**. Two cases:

1. **Reactive warm sessions (RFC §5.3 "Continue").** If a stuck/crashed subagent backing a
   warm session dies, does the supervisor respawn it on the next resource update? Unbounded
   respawn of a child that crashes on startup = a flapping fork loop that looks like "reactive
   idle" but burns CPU and tokens.

2. **Subagents spawned by a parent via `subagent.spawn`.** A parent in an agentic loop can be
   told by the model to "retry" and spawn replacements forever.

**Mechanism — per-handle restart governor (a tiny struct, no dependency):**
```
struct RestartHistory {
    window: Duration,         // e.g. 60s
    failures: VecDeque<Instant>,
    consecutive: u32,
}
```
- **Exponential backoff with cap + jitter:** delay = min(base * 2^consecutive, cap), plus
  small jitter; the supervisor will not respawn a session-backing subagent before its backoff
  expires (tracked via the timerfd).
- **Circuit breaker:** if `failures` within `window` exceeds a threshold (e.g. 5), open the
  breaker for that handle/session: stop respawning, mark the session **failed**, and surface
  it as a self-MCP resource state (§8) so a watcher/operator can see it. Reactive updates that
  would route to a broken session are dropped (or routed to spawn-fresh, per the routing rule
  in §11).
- **Crash-on-spawn fast-fail:** a child that exits before emitting a single `ready`/`hello`
  control frame within a short window (e.g. 2s) counts as a *spawn failure*, weighted more
  heavily than a mid-run crash — this is the fork-bomb early-warning.
- **Distinguish clean completion from failure:** exit code 0 + `final` result = success, do
  not count against the breaker. Non-zero exit, signal death, or stuck-kill = failure.

The supervisor never auto-restarts a **one-shot** root (RFC §5.1) — one-shot means one
attempt; restart policy applies only to loop/reactive modes and to session-backing children.

---

## 7. Runaway recursion / fork-bomb prevention (depth + tree-wide budgets)

RFC §6.3 hand-waves: "bounded by a maximum tree depth and a tree-wide budget so recursion
can't explode." RFC §10/§14(7) note the knobs exist but pick no defaults and define no
enforcement point. This is a safety-critical gap: the model owns the loop (RFC §2), so nothing
but the supervisor stops `spawn → spawn → spawn`.

**Mechanisms:**
- **Depth cap (`max_depth`, default e.g. 4).** Each `Child` carries `depth`. `subagent.spawn`
  (§8) is served *by the supervisor* (it owns the process table), so the supervisor rejects a
  spawn whose `depth+1 > max_depth` with a tool error the parent's model sees. Depth is set by
  the supervisor from the *caller's* handle, **never trusted from the child's request** — a
  child cannot lie about its depth because it doesn't mint the value.
- **Breadth cap (`max_children` per node, and `max_total_subagents` tree-wide).** A global
  counter incremented on spawn, decremented on reap. Spawn refused past the ceiling.
- **Spawn-rate cap.** A token-bucket on spawns/sec tree-wide catches a fast fork loop that
  stays under the absolute count by churning. Cheap: one counter + timestamp.
- **The enforcement point is structural and unforgeable:** because every subagent process is
  created only through the supervisor-owned `subagent.spawn` tool (RFC §8 — the self-MCP is
  *the* spawn path), there is exactly one chokepoint. **This must be stated as an invariant:**
  a subagent has no other way to create an agent child than calling back into the
  supervisor's self-MCP. (The `exec` tool, §9, is a separate escape hatch — see §9 below.)

Defaults must be **finite and conservative** and shipped as such; "unbounded unless
configured" is the wrong default for a model-owned loop.

---

## 8. Resource accounting per subtree

RFC §13 lists "tree-wide token ceiling" as a budget but RFC `budget.rs` (parts bin) only does
**process-wide** `setrlimit` and a single flat token counter. With a tree of processes you
need **hierarchical accounting**: a token/step/cost spend charged to a child must roll up to
every ancestor and to the tree root, so a tree-wide ceiling is actually enforced and a runaway
*branch* can be capped without killing healthy siblings.

**Mechanism:**
- Each subagent reports token/step usage in its control-channel events (it already streams
  tool-result/final; add a per-turn `usage{tokens, steps}` field). The supervisor — not the
  child — is the source of truth for cumulative spend (a child cannot under-report past a cap
  it doesn't enforce).
- The supervisor keeps a **per-node counter and a single tree-root counter**. On each usage
  event: add to the node and to the root; if the node exceeds its grant ⇒ cancel that subtree
  (§5); if the root exceeds the tree-wide ceiling ⇒ drain the whole tree (§5) and exit with a
  budget-exceeded code. Rolling to the root only (not every ancestor) is sufficient for the
  tree ceiling and is O(1) per event; per-ancestor roll-up is optional and only needed if
  per-branch sub-budgets are wanted.
- **CPU/memory per subtree:** `setrlimit` is per-process and *inherited*, so set
  `RLIMIT_AS`/`RLIMIT_CPU` on each child at spawn via `pre_exec` (caps a single runaway child
  cheaply). For true *aggregate* subtree CPU/mem, the honest answer is **cgroups v2** (one
  cgroup per subtree) — but that requires privilege and is a deployment concern (RFC §13 says
  "typically applied by the deployment"). Recommendation: per-child `setrlimit` in-binary
  (no privilege, always available); aggregate cgroup accounting documented as an optional
  deployment-provided layer, not core. State this explicitly so operators don't assume the
  tree-wide *memory* ceiling is enforced in-binary (it is not; only the token ceiling is).

---

## 9. State recovery after supervisor or subagent crash

RFC §14(3) flags this as an open question and *biases to in-memory-only for v1*. That bias is
defensible for the **agentic context** (re-deriving a half-finished chain of thought is
costly but not safety-critical) but is **dangerous for supervision metadata** if not reasoned
about. Separate the two:

### 9.1 If a *subagent* crashes
- Its in-process agentic context (message history) is **lost** — acceptable per the RFC's
  bias. The supervisor still holds: the handle, the original instruction, context seed, tool
  scope, limits, and accumulated usage (because the supervisor minted all of these at spawn,
  §2 data structure). So the supervisor *can* respawn with the same seed if policy allows
  (§6), losing only mid-run progress. **Recommendation:** the spawn payload (instruction +
  seed + scope + limits) is cheap and must be retained by the supervisor for the child's
  lifetime precisely to make bounded restart possible. This is the minimum recoverable unit.

### 9.2 If the *supervisor* crashes
This is the load-bearing case the RFC under-thinks. If the supervisor dies:
- All children are orphaned. With `PR_SET_CHILD_SUBREAPER` *gone* (the supervisor was the
  subreaper), orphans reparent to host PID 1 and **keep running untracked** — burning tokens,
  holding MCP connections, possibly executing `exec` side effects. This is the worst leak in
  the whole design and the RFC does not mention it.
- **Mitigation (in-binary, minimal):** children must enforce their **own** hard deadline and a
  **parent-death trip-wire**: on Linux, each subagent calls
  `prctl(PR_SET_PDEATHSIG, SIGKILL)` at startup so the kernel kills it when its *immediate*
  parent dies. (Caveat: `PDEATHSIG` fires on the death of the thread that spawned it and is
  cleared across `execve` in some cases — set it in the re-exec'd child's early `main`, and
  combine with the self-enforced deadline as belt-and-suspenders.) With PDEATHSIG, a
  supervisor crash collapses the tree from the leaves up automatically, which is the safe
  default for a stateless/in-memory v1. **The RFC must require PDEATHSIG (or equivalent) on
  every spawned subagent** — without it, "in-memory only" silently means "orphan leak on
  supervisor crash."

### 9.3 Optional checkpoint (the deferred extension, done right)
If/when warm reactive sessions (RFC §5.3) get checkpointed so an external scheduler can restart
the pod (RFC §14(3)): checkpoint **only** the supervisor-owned, cheap, durable facts —
the subscription set, the handle→{instruction, seed, scope, limits, usage, restart-history}
map, and the spawn-vs-continue routing table (§11). Do **not** try to checkpoint live
agentic context or live pipes. Write atomically (write-temp + `rename`, as
`engine/checkpoint.rs` nearly does — but it should `fsync` the file and the dir, which the old
code omits). On restart, re-arm subscriptions and respawn session-backing subagents from their
seeds. This makes the pod restartable without resurrecting in-flight LLM state — a clean,
minimal recovery story consistent with the RFC's bias.

---

## 10. Health, liveness, and observability of the supervisor itself

RFC §11 promises "a trivial health signal," §1(6) demands "first-class healthcheck." The RFC
does not say **what unhealthy means** for a supervisor. A supervisor can be alive (PID up) yet
unhealthy: reactor thread wedged, all children stuck, MCP connections all dead, deadline
queue not advancing. A liveness probe that only checks "process exists" is a lie.

**Mechanism:**
- The reactor updates a `last_loop_tick: Instant` (monotonic heartbeat) every wake. The health
  signal (a tiny `unix:` socket or a file the probe reads, or exit-code of a `--health`
  subcommand) reports **unhealthy if `now - last_loop_tick > threshold`** — i.e. the reactor
  itself is wedged. This is the supervisor's own no-progress watchdog (the §4.2 idea turned
  inward).
- Report, in the health payload: tree size, count of children by state, count of stuck/
  draining children, count of un-reaped leaks, open-breaker sessions. These are the numbers an
  operator/orchestrator needs and they fall directly out of the §2 data structures.
- Keep it dependency-free: a `--health` exit-code probe (read the heartbeat file, exit 0/1)
  needs no HTTP server and suits Kubernetes `exec` probes, matching the minimalism bar.

---

## 11. Reactive routing under failure (touches reliability)

RFC §14(5) flags spawn-vs-continue routing as open. From a reliability angle the rule must be
**total and crash-safe**: every incoming `notifications/resources/updated` maps
deterministically to exactly one of {spawn-fresh, continue-session, drop}. Specifically:
- Maintain a `subscription_uri → session_handle` table (supervisor-owned, checkpointable per
  §9.3).
- If the target session's backing subagent is **healthy/warm** ⇒ continue.
- If it is **stuck/draining** ⇒ queue the event (bounded queue per session; overflow ⇒ drop
  oldest + increment a dropped-events metric) until drain completes, then spawn-fresh and
  replay if policy says so.
- If the breaker is **open** (§6) ⇒ drop + metric (do not spawn into a known-bad loop).
- **De-duplicate / coalesce** bursts: many `resources/updated` for the same URI while a session
  is mid-turn should coalesce to a single wake (level-triggered, not edge-triggered) — else a
  chatty resource becomes a self-inflicted spawn storm. This is both a correctness and a
  reliability property and the RFC must state it.

---

## 12. The `exec` escape hatch is a supervision hole (RFC §9)

RFC §9 gates `exec` but treats it as "the OS/container is the sandbox" and stops. From a
process-supervision view, `exec` spawns a child **outside the agent subagent protocol**:
- It has no control channel, so the §4 stuck-detectors (no-progress events, ping/pong) **do
  not apply** — only the hard deadline + kill does. The RFC must state that `exec` children get
  a **mandatory deadline and process-group kill**, reusing the §5 ladder, and are counted
  against the subtree budget (§8) and the breadth/rate caps (§7) — otherwise `exec` is a
  fork-bomb bypass around the §7 chokepoint.
- `exec` children must be put in the subagent's process group (or their own group tracked in
  the table) so subtree drain (§5) reaps them. The old `tools/shell.rs` `child.kill()` (no
  group, SIGKILL-only) is exactly the wrong pattern to carry over.

---

## 13. Concrete, prioritized list of fixes the RFC must absorb

1. **Reactor over `poll`/`epoll` + self-pipe + timerfd** (§1) — without it, "idle at near-zero
   cost" and unified supervision are impossible. *Foundational.*
2. **`signal(SIGPIPE, SIG_IGN)`** (§2.1) — one line; prevents the supervisor dying with a
   child. *Trivial, critical.*
3. **`PR_SET_CHILD_SUBREAPER` + `waitpid(-1, WNOHANG)` reap loop on SIGCHLD** (§3) — keeps the
   whole tree (incl. orphaned grandchildren) in agent's reaping/kill domain. *Foundational.*
4. **`PR_SET_PDEATHSIG` on every subagent + mandatory self-enforced deadline** (§9.2) — makes
   "in-memory only" safe against supervisor crash (no orphan leak). *Critical.*
5. **Three-detector stuck model (deadline + no-progress + ping/pong), control thread separate
   from agentic loop in the subagent** (§4) — the missing "dead vs stuck" requirement.
   *Core requirement, currently unspecified.*
6. **Bounded SIGTERM→SIGKILL escalation ladder over process groups, depth-first deepest-first,
   second-signal force path, drain budget < orchestrator grace** (§5) — defines the RFC's
   "graceful drain → bounded kill." *Core.*
7. **Restart governor: backoff + jitter + circuit breaker + crash-on-spawn fast-fail**
   (§6) — stops flapping/fork-loops; success≠failure distinction. *Core.*
8. **Depth/breadth/rate caps enforced at the single supervisor-owned `subagent.spawn`
   chokepoint, depth minted by supervisor not child** (§7) — fork-bomb prevention with an
   unforgeable enforcement point + finite defaults. *Core.*
9. **Hierarchical token accounting rolled to the tree root; per-child `setrlimit`; cgroups as
   optional deployment layer (state memory ceiling is NOT in-binary)** (§8). *Core, with an
   explicit honesty caveat.*
10. **Supervisor self-heartbeat health signal (`--health` exit-code probe) reporting tree
    state** (§10) — "trivial health signal" made real. *Important.*
11. **Total, crash-safe reactive routing with coalescing and bounded per-session queues**
    (§11). *Important.*
12. **`exec` children folded into the kill ladder + budgets + caps; mandatory deadline**
    (§12). *Important.*

Every mechanism above is `std` + `libc` (poll/epoll, timerfd, self-pipe, `prctl`, `setpgid`,
`killpg`, `waitpid`, `setrlimit`, `signal`) — no async runtime, no new dependency — consistent
with RFC §12's minimalism bar. The recurring theme: **the supervisor must be the sole, crash-
resilient root of a reaping/kill/budget domain that no child can escape**, and **liveness must
be actively probed, not assumed from PID existence.** RFC 0001 has the right architecture but
states supervision as a slogan; these are the specific, minimal mechanisms that make it true.
