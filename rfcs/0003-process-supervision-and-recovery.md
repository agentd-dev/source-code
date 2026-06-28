# RFC 0003: Process supervision, dead/stuck detection & recovery

**Status:** Accepted (shipped v1)
**Author:** Andrii Tsok
**Date:** 2026-06-25
**Part of:** the agentd rewrite — binding decisions in docs/design/00-architecture-assessment.md; core in RFC 0001

---

## 1. Problem / Context

RFC 0001 §3/§4.1 states supervision as a one-line responsibility — "spawn,
track, reap, enforce limits." That is a slogan. The reliability review found
twelve concrete gaps; the architecture-decision document elevates them to
binding mechanisms in §2.8. This RFC is the implementation-ready specification
for those mechanisms: how agentd, as the crash-resilient root of a process
tree, **detects** that a child is dead or stuck, **kills** a subtree without
killing itself, **recovers** after its own crash, and **accounts** for spend
across the whole tree.

The supervisor owns an OS process tree of subagent children (RFC 0009,
re-exec'd same binary) plus `exec` children (RFC 0012) plus stdio MCP server
children (RFC 0004). The agentic ReAct loop lives only inside subagent
processes (RFC 0007); the supervisor carries **no LLM dependency**. The
cancellation argument is decisive: the only reliable way to stop runaway model
work is `SIGKILL` of a process group — async future-drop cannot do it. So the
supervisor must be an actual init-class process manager for its tree.

The recurring invariant, stated once and assumed throughout: **the supervisor
is the sole, crash-resilient root of a reaping/kill/budget domain that no child
can escape, and liveness is actively probed, never assumed from PID
existence.** Everything below is `std` + raw `libc` — `poll`, `waitpid`,
`prctl`, `setpgid`, `killpg`, `setrlimit`, `signal`, `sigaction`, a self-pipe.
No async runtime, no `signal-hook`, no new dependency (assessment §2.2).

This RFC depends on the reactor and the per-child supervision record defined in
RFC 0002 (`recv_timeout` merged-`mpsc` loop, thread-per-fd, self-pipe signals,
abandon-don't-interrupt). It defines the behaviour layered onto that record;
RFC 0002 defines the loop it runs in.

---

## 2. Decision

Adopt, verbatim from assessment §2.8, the following and specify each to
build-ready depth:

1. **Three-detector dead/stuck model** — hard deadline (Detector A) +
   no-progress watchdog (Detector B) + active ping/pong on a control thread
   decoupled from the agentic loop (Detector C) — plus the **EOF×pong 2×2
   classifier**.
2. **Reaping** — `SIGCHLD` self-pipe → `waitpid(-1, WNOHANG)` in a loop;
   `PR_SET_CHILD_SUBREAPER` + PID-1 detection; reap any child including unknown
   reparented PIDs.
3. **Orphan discipline** — `PR_SET_PDEATHSIG = SIGKILL` in every child's early
   `main`; `signal(SIGPIPE, SIG_IGN)` at supervisor startup.
4. **Bounded kill ladder** — per-subtree, depth-first deepest-first,
   `ctrl:cancel` → `killpg(SIGTERM)` → `killpg(SIGKILL)` → `waitpid` to ECHILD,
   total drain budget `< terminationGracePeriodSeconds`; tree-draining flag;
   second-signal force.
5. **Restart governor** — backoff + jitter + circuit breaker + crash-on-spawn
   fast-fail; loop/reactive (and session-backing children) only, never a
   one-shot root.
6. **Stateless-supervisor rebuild + reconcile** on restart, with **mandatory
   read-after-subscribe**.
7. **Hierarchical token accounting** rolled to the tree root, O(1) per event;
   per-child `setrlimit`.
8. **cgroup-v2 awareness, not requirement** — read `memory.max`, place the tree
   in a child cgroup when writable, but always fall back to rlimit + PDEATHSIG.

Honest caveat carried forward from §2.8 and risk #12: only the **token** ceiling
is enforced in-binary. Aggregate subtree **memory** is a cgroups/deployment
concern; per-child `RLIMIT_AS` caps a single runaway, not the tree.

---

## 3. Mechanisms

### 3.0 The supervision record (recap from RFC 0002)

The fields this RFC reads and mutates. RFC 0002 owns the struct; reproduced here
for the signatures below.

```rust
pub struct Child {
    pid: libc::pid_t,
    pgid: libc::pid_t,               // == pid; child is a group leader (§3.4)
    handle: Handle,                  // opaque id used in self-MCP tools (RFC 0005)
    kind: ChildKind,                 // Subagent | Exec | McpServer
    parent: Option<Handle>,          // tree edge
    depth: u16,                      // minted by supervisor, never trusted (RFC 0009)
    state: ChildState,
    ctrl_out: RawFd,                 // child stdout (control frames in; None for Exec)
    ctrl_in: Option<Mutex<ChildStdin>>, // O_NONBLOCK; None for Exec
    spawned_at: Instant,
    ready_at: Option<Instant>,       // first `ready` frame (§3.6)
    last_event_at: Instant,          // Detector B
    last_ping_seq: u64,              // Detector C
    last_pong_seq: u64,
    last_ping_sent_at: Instant,
    deadline: Instant,               // Detector A — mandatory, finite
    restarts: RestartHistory,        // §3.7
    node_tokens: u64, node_steps: u32,
    grant_tokens: u64, grant_steps: u32,
}

pub enum ChildState {
    Spawning, Running, Idle,         // Idle: loop done, blocked reading next control msg
    Draining, Killing, Exited(ExitInfo),
}
```

The tree is a flat `HashMap<libc::pid_t, Child>` + `HashMap<Handle, pid_t>`;
parent edges make it a tree. No fancier structure is needed.

### 3.1 Startup invariants (run once, in supervisor `main`, before any spawn)

```rust
unsafe {
    // SIGPIPE: a write() to a just-dead child must not kill the supervisor.
    libc::signal(libc::SIGPIPE, libc::SIG_IGN);

    // Become subreaper so grandchildren orphaned by a dying subagent reparent
    // to us, not to host init — keeps the whole tree in our reaping domain.
    libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
}
let is_pid1 = (unsafe { libc::getpid() } == 1); // first-class container case
```

`is_pid1` changes nothing functionally — `PR_SET_CHILD_SUBREAPER` already routes
orphans to us — but it is logged (`proc.start`) and documented: **the
recommended container entrypoint is agentd itself; we are a tini-class init for
our tree and do not require an external `tini`.**

`SIGCHLD`, `SIGTERM`, `SIGINT` are installed via `sigaction` with `SA_RESTART`
**deliberately off** (assessment §2.1) so blocked syscalls return `EINTR`; each
handler is async-signal-safe — flip an `AtomicBool` and `write()` one byte to
the self-pipe (RFC 0002 owns the self-pipe; this RFC consumes its wake).

### 3.2 Detector A — hard deadline (always on, no child cooperation)

Every `Child` carries `deadline: Instant`, minted at spawn from `limits`
(RFC 0009). **A deadline is mandatory and finite** — never infinity. Default
`AGENT_CHILD_DEADLINE = 600s` for subagents; `exec` children get
`AGENT_EXEC_DEADLINE = 120s` (override per-spawn, but never to "none").

The reactor (RFC 0002) arms its `recv_timeout` to the minimum deadline across
all non-terminal children:

```rust
fn next_timer(&self) -> Duration {
    let now = Instant::now();
    self.children.values()
        .filter(|c| c.state.is_live())
        .map(|c| c.deadline)
        .chain(self.detector_c_next_ping()) // §3.4
        .chain(self.restart_backoff_wakeups()) // §3.7
        .min()
        .map(|t| t.saturating_duration_since(now))
        .unwrap_or(IDLE_TICK) // IDLE_TICK = 1s, also bumps the heartbeat (RFC 0010)
}
```

On expiry: classify the verdict as `deadline`, run the kill ladder (§3.5) on
that child's subtree, and (for one-shot roots) map to exit code 124
(assessment §2.10). Deadline expiry needs **no** cooperation from the child and
catches the "runs forever" case unconditionally — it is the floor under every
other detector.

### 3.3 Detector B — no-progress watchdog (liveness without cooperation)

Every control frame received from a child (RFC 0005 wire) stamps
`last_event_at = Instant::now()`. On each reactor wake, for every live child:

```rust
if now.duration_since(child.last_event_at) > self.progress_timeout(child) {
    self.declare_stuck(child.pid, StuckReason::NoProgress);
}
```

`progress_timeout` must exceed the longest legitimate single tool/model call, so
it is a **coarse** net: default `AGENT_PROGRESS_TIMEOUT = 120s` (≈ 2× the model
request timeout). It reuses the existing event stream — no new wire mechanism —
and fires even if the child's control thread is *also* wedged (Detector C would
go silent too, but B does not depend on the child answering). Detector C makes
the verdict precise; B is the cooperation-free backstop.

### 3.4 Detector C — active ping/pong on a decoupled control thread

This is the only detector that distinguishes *busy-in-a-long-legitimate-tool-call*
(pongs continue) from *process wedged* (pongs stop). **Hard design requirement
(assessment §2.3, §2.8):** inside each subagent the control-channel reader runs
on a **dedicated thread, decoupled from the agentic loop**, so ping/pong
liveness survives a long in-flight model/tool call.

Supervisor side — periodic ping on the control channel
(`AGENT_PING_INTERVAL = 5s`):

```rust
// length-framed JSON-RPC notification, downward (RFC 0005 codec)
{"jsonrpc":"2.0","method":"ctrl/ping","params":{"seq": <u64>}}
```

Child control thread replies **immediately**, never touching the loop:

```rust
{"jsonrpc":"2.0","method":"ctrl/pong","params":{"seq": <u64>}}
```

Verdict: after `AGENT_PING_MISS = 3` consecutive unanswered pings
(`last_ping_seq - last_pong_seq >= 3` with the oldest outstanding ping older
than `PING_INTERVAL`), the child's control thread is wedged or the process is in
uninterruptible `D` state → declare stuck (`StuckReason::PongTimeout`).

`exec` children have **no** control channel, so Detector C does not apply to
them — only A (deadline) + dead-detection (§3.6). This is stated explicitly so
`exec` is not silently un-watched.

### 3.5 The EOF×pong 2×2 classifier (write this explicitly into the supervisor)

EOF on `ctrl_out` (pipe `read()` returns 0) means the child closed stdout —
**necessary but not sufficient** for death (it may have closed stdout yet still
be alive or in `D`). Combine the two axes:

| | **pong flowing** | **pong stopped** |
|---|---|---|
| **no EOF, events flowing** | `Healthy` | (transient — wait for B/C) |
| **no EOF, no events** | `BusyHealthy` (long tool call) | `Stuck` (wedged / `D`-state) |
| **EOF** | `Exiting` → `waitpid` confirms | `Dead` → `waitpid` confirms |

```rust
enum Liveness { Healthy, BusyHealthy, Stuck, Exiting, Dead }

fn classify(c: &Child, now: Instant) -> Liveness {
    let eof = c.ctrl_eof;
    let pongs_ok = (c.last_ping_seq - c.last_pong_seq) < AGENT_PING_MISS;
    let recent_event = now.duration_since(c.last_event_at) <= c.progress_timeout;
    match (eof, pongs_ok, recent_event) {
        (true,  _,     _)     => Liveness::Exiting, // confirm via waitpid → Dead
        (false, true,  true)  => Liveness::Healthy,
        (false, true,  false) => Liveness::BusyHealthy,
        (false, false, _)     => Liveness::Stuck,
    }
}
```

`Exiting`/`Dead` always confirm with `waitpid` (§3.6) before mutating the tree —
EOF alone never declares death. `Stuck` runs the kill ladder (§3.5 below).
`BusyHealthy` is left alone (this prevents the false-positive kill of a child
doing legitimate slow work — the whole reason Detector C exists).

### 3.6 Dead detection & reaping

`SIGCHLD` does **not** queue, so the handler only wakes the reactor; reaping
loops until drained:

```rust
fn reap_loop(&mut self) {
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 { break; } // 0 = none ready; -1 = ECHILD (no children)
        match self.children.remove(&pid) {
            Some(child) => self.on_child_exit(child, ExitInfo::from(status)),
            None        => self.log_orphan_reaped(pid, status), // reparented grandchild
        }
    }
}
```

`waitpid(-1, …)` reaps **any** child including unknown reparented grandchildren
(the PID-1 trap): without it, orphaned grandchildren become zombies forever or
keep running untracked, holding MCP connections and burning tokens. Unknown PIDs
are logged (`subagent.exit` with `orphan:true`) and discarded.

`ExitInfo::from(status)` classifies via `WIFEXITED`/`WEXITSTATUS` vs
`WIFSIGNALED`/`WTERMSIG`:

```rust
enum ExitInfo {
    Clean(i32),        // WIFEXITED: exit code
    Signal(i32),       // WIFSIGNALED: 128 + signo for the exit-code table (RFC 0011)
}
```

Clean exit `0` + a received `final` frame = success (not a restart-governor
failure, §3.7). Non-zero exit, signal death, or a stuck-kill = failure.

`ready_at` is stamped on the first `ctrl/ready` frame the child emits in early
`main` (§3.9). A child that reaches `Exited` before `ready_at` within
`AGENT_SPAWN_READY = 2s` is a **crash-on-spawn** (§3.7) — the fork-bomb early
warning.

### 3.5 (kill) The bounded depth-first kill ladder

Triggered by: SIGTERM/SIGINT to agentd (drain — RFC 0011 owns the choreography,
this RFC owns the ladder), a Detector A/B/C verdict on one subtree, or a budget
breach (§3.8). Each subagent is its own **process group** (`setpgid(0,0)` in
`pre_exec`, §3.9) so a subtree is signalled atomically with `killpg`.

**Order: depth-first, deepest-first.** A parent must not spawn replacements
mid-teardown, so set a **tree-wide draining flag** first that makes
`subagent.spawn` (RFC 0005/0009) error, then tear down leaves before roots:

```rust
const DRAIN_GRACE: Duration = Duration::from_secs(5);   // AGENT_DRAIN_GRACE
const KILL_GRACE:  Duration = Duration::from_secs(2);   // AGENT_KILL_GRACE
// per-subtree budget = DRAIN_GRACE + KILL_GRACE = 7s nominal;
// AGENT_DRAIN_TIMEOUT = 25s caps the WHOLE tree and MUST be
// < terminationGracePeriodSeconds (rec 30s) — validated at startup (RFC 0011).

fn kill_subtree(&mut self, root: Handle, force: bool) {
    let order = self.postorder_handles(root); // deepest-first
    for h in &order {
        let pid = self.pid_of(*h);
        if force {
            unsafe { libc::killpg(self.pgid_of(pid), libc::SIGKILL); }
            continue;
        }
        self.send_ctrl_cancel(*h);              // t0: graceful, mark Draining
        self.children.get_mut(&pid).unwrap().state = ChildState::Draining;
    }
    if force { self.reap_until_clear(&order, Duration::ZERO); return; }

    // t0 + DRAIN_GRACE: SIGTERM the groups still alive
    self.arm_timer(DRAIN_GRACE, move |s| {
        for h in &order { unsafe { libc::killpg(s.pgid_of(s.pid_of(*h)), libc::SIGTERM); } }
        // t0 + DRAIN_GRACE + KILL_GRACE: SIGKILL the groups still alive
        s.arm_timer(KILL_GRACE, move |s| {
            for h in &order { unsafe { libc::killpg(s.pgid_of(s.pid_of(*h)), libc::SIGKILL); } }
            s.reap_until_clear(&order, KILL_GRACE);
        });
    });
}
```

`reap_until_clear` calls the §3.6 reap loop until each pid is reaped or
`waitpid` returns `ECHILD`; any pid that never reaps inside the budget (a
`D`-state leak — **`SIGKILL` cannot kill `D`-state**) is logged as a
`stuck-leak` metric and the supervisor exits with a distinct unclean-drain code
(143 for ungraceful drain, RFC 0011) so the orchestrator sees it. We **detect
and report** the un-killable case rather than hang on it (assessment risk #4).

**Two complementary reach mechanisms** (a child may create its own sub-groups,
so `killpg` of one group is not guaranteed to reach grandchildren): walk the
supervisor's own parent-edge table *and* `killpg` per node — this covers both
the cooperative case and orphans (which `PR_SET_CHILD_SUBREAPER` keeps in our
reaping domain regardless).

**Second-signal force.** First SIGTERM/SIGINT sets `draining = true`; a second
sets `force = true`. The ladder checks `force` and collapses to immediate
`killpg(SIGKILL)` of all groups, then exit (handles operator impatience /
orchestrator escalation).

### 3.6 (orphan) PDEATHSIG — collapse the tree on supervisor crash

If the supervisor dies, its children are orphaned; with the subreaper gone they
would reparent to host PID 1 and keep running untracked — **the worst leak in
the design** (assessment risk #3). Mandatory mitigation: every subagent, in its
**early re-exec'd `main`** (before any other work), runs

```rust
unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0); }
// caveat: PDEATHSIG fires on the death of the *spawning thread*, and is
// cleared across execve — set it AFTER the re-exec, in the child's own main,
// and recheck getppid() != supervisor_pid (passed via env) immediately after,
// to close the race where the parent died between fork and prctl.
let expected_ppid: i32 = env_supervisor_pid();
if unsafe { libc::getppid() } != expected_ppid {
    std::process::exit(137); // parent already gone — self-terminate
}
```

With PDEATHSIG, a supervisor crash collapses the tree from the leaves up
automatically — the safe default for a stateless/in-memory v1. Combined with the
child's own self-enforced deadline (§3.2 runs in the child too) as
belt-and-suspenders. **Without PDEATHSIG, "in-memory only" silently means
orphan leak** — this is non-optional.

### 3.7 The restart governor (loop/reactive + session-backing children only)

Never auto-restart a **one-shot root** — one-shot means one attempt
(assessment §2.6, §2.8). Restart policy applies only to loop/reactive modes and
to reactive-session-backing children (RFC 0008). Per-handle state:

```rust
struct RestartHistory {
    window: Duration,                 // AGENT_RESTART_WINDOW = 60s
    failures: VecDeque<Instant>,      // failure timestamps inside the window
    consecutive: u32,
    breaker_open_until: Option<Instant>,
}

const RESTART_BASE: Duration = Duration::from_millis(500);
const RESTART_CAP:  Duration = Duration::from_secs(30);
const BREAKER_THRESHOLD: usize = 5;   // failures within `window`
const SPAWN_FAIL_WEIGHT: u32 = 3;     // crash-on-spawn counts 3×

fn backoff(&self) -> Duration {
    let exp = RESTART_BASE.saturating_mul(1u32 << self.consecutive.min(16));
    let d = exp.min(RESTART_CAP);
    d + jitter(0..=d / 4)             // full-ish jitter, deterministic-RNG-free (nanos & mask)
}
```

- **Exponential backoff + jitter, capped.** The supervisor will not respawn a
  session-backing child before its backoff expires (wakeup armed on the reactor
  timer, §3.2).
- **Circuit breaker.** More than `BREAKER_THRESHOLD` failures inside `window` →
  open the breaker for that handle: stop respawning, mark the session **failed**,
  surface it as a self-MCP resource (`agent://session/<id>` state=failed,
  RFC 0005) so a watcher/operator sees it, and **drop routed reactive events**
  that would target the broken session (RFC 0008) — do not spawn into a known-bad
  loop.
- **Crash-on-spawn fast-fail.** A child that exits before its `ctrl/ready` frame
  within `AGENT_SPAWN_READY = 2s` (§3.6) is a spawn failure weighted
  `SPAWN_FAIL_WEIGHT` heavier — the fork-bomb early warning.
- **Success ≠ failure.** Clean exit 0 + a received `final` result does not count
  against the breaker; only non-zero exit, signal death, or stuck-kill does.

Fork-bomb breadth/depth/rate caps live at the `subagent.spawn` chokepoint
(RFC 0009 — depth minted by the supervisor, refused as a tool result); the
governor here is the *temporal* control over restarts, complementary to those
*structural* caps.

### 3.8 Hierarchical token accounting (rolled to the tree root, O(1))

Each subagent reports per-turn `usage{tokens, steps}` in its control events
(RFC 0005/0007). The **supervisor is the source of truth** — a child cannot
under-report past a cap it does not enforce.

```rust
fn on_usage(&mut self, pid: pid_t, u: Usage) {
    let c = self.children.get_mut(&pid).unwrap();
    c.node_tokens += u.tokens; c.node_steps += u.steps;
    let root = self.tree_root_of(pid);
    self.roots.get_mut(&root).unwrap().tokens += u.tokens; // single root counter

    if c.node_tokens > c.grant_tokens || c.node_steps > c.grant_steps {
        self.kill_subtree(c.handle, /*force=*/false); // cap the branch, spare siblings
    }
    if self.roots[&root].tokens > self.tree_token_ceiling {
        self.begin_drain(DrainReason::TreeBudget); // exit code 7 (RFC 0011)
    }
}
```

Rolling to the **root only** (not every ancestor) is sufficient for the
tree-wide ceiling and is O(1) per event; per-ancestor roll-up is optional and
only needed if per-branch sub-budgets are wanted (deferred). A node over its
grant cancels just that subtree; the root over the tree ceiling drains the whole
tree and exits with the budget code.

**Per-process resource limits** via `pre_exec` at spawn (§3.9): `RLIMIT_AS`
(address space) and `RLIMIT_CPU` (CPU seconds) cap a single runaway child
cheaply, no privilege required.

**Honest caveat (assessment §2.8, risk #12):** `setrlimit` is per-process and
inherited; it does **not** bound *aggregate subtree memory*. Only the **token**
ceiling is enforced in-binary. Operators must size `resources.limits` for the
whole pod and not assume a tree-wide memory cap — see §3.10.

### 3.9 Spawn-time setup (`pre_exec`)

All process-discipline syscalls run in the child between `fork` and `exec`, via
`std::os::unix::process::CommandExt::pre_exec` (RFC 0009 owns the spawn payload;
this RFC owns the syscalls):

```rust
unsafe {
    cmd.pre_exec(|| {
        // own process group → subtree addressable by killpg (§3.5)
        if libc::setpgid(0, 0) != 0 { return Err(io::Error::last_os_error()); }
        // per-child rlimits (§3.8)
        set_rlimit(libc::RLIMIT_AS,  as_bytes)?;
        set_rlimit(libc::RLIMIT_CPU, cpu_secs)?;
        Ok(())
    });
}
```

`PR_SET_PDEATHSIG` is **not** set in `pre_exec` (it is cleared across `execve`);
it is set in the re-exec'd child's early `main` instead (§3.6). The child stdin
is opened `O_NONBLOCK` with a bounded outbound queue (RFC 0002 invariant — a full
queue is itself a stuck signal). `ctrl/ready` is the first frame the child emits
after PDEATHSIG + getppid recheck succeed.

### 3.10 cgroup-v2 awareness (not requirement)

agentd is cgroup-v2-**aware** but **never hard-requires cgroup write access**
(assessment §2.8). At startup:

- **Read** `/sys/fs/cgroup/memory.max` and `memory.high` if present; log them
  (`config.loaded`) and use `memory.high` as a backpressure hint (slow new
  spawns when approaching it).
- **Place the tree in a child cgroup** only when the cgroup is writable: create
  `…/agent.<run_id>/`, write the supervisor PID to `cgroup.procs`; on drain,
  `echo 1 > cgroup.kill` reaps the entire subtree atomically (covers `D`-state
  and orphans the ladder might miss).
- **Fallback when not writable** (the common unprivileged case): rlimit
  (§3.8/§3.9) + PDEATHSIG (§3.6) only. No error, no degraded mode beyond the
  documented memory caveat (§3.8).

cgroup placement is a strict enhancement to the kill ladder, never a
precondition for correctness.

### 3.11 Stateless-supervisor rebuild + reconcile (on restart)

The supervisor is **stateless**; the minimum recoverable unit it retains for a
child's lifetime is its spawn payload (instruction + seed + scope + limits +
accumulated usage) so a bounded restart (§3.7) is possible. On supervisor
*process* restart (a new pod/process, in-memory state gone):

1. **Re-read config**, validate fully before any side effect (exit 2 on bad
   config — RFC 0011).
2. **Re-establish MCP** connections (RFC 0004), capability-gated.
3. **Re-issue every *declared* subscription** (from config — RFC 0008).
4. **read-after-subscribe each — MANDATORY, not optional.** Immediately after
   each `resources/subscribe`, issue `resources/read` on the same URI. This
   converts edge-triggering into level-triggering across the restart boundary:
   any change that happened while the supervisor was down is recovered, because
   the agentd acts on *current state*, not a missed delta (idempotent re-trigger,
   assessment §2.6 at-least-once-via-re-read-current-state).

```
for sub in declared_subscriptions {
    client.subscribe(sub.server, &sub.uri)?;        // re-arm
    let cur = client.read(sub.server, &sub.uri)?;   // MANDATORY synthesize-one-event
    router.deliver_synthetic_updated(&sub.uri, cur); // exactly-one-owner route (RFC 0008)
}
```

Warm sessions and dynamic (self-)subscriptions are **lost** in v1 — recovered by
idempotent re-trigger, not by resurrection. An optional MCP-backed checkpoint of
supervisor-owned facts (subscription set + handle map + routing table; atomic
write + `fsync` of file **and** dir) is a **deferred v2 extension**
(RFC 0013) — **never checkpoint live agentic context or live pipes.**

---

## 4. Interactions with other RFCs

- **RFC 0002 (Supervisor reactor & concurrency):** owns the `recv_timeout`
  reactor, the merged `mpsc`, the self-pipe, the `Child` struct, and the
  abandon-don't-interrupt invariant. This RFC's timers (§3.2), reap wakeups
  (§3.6), and kill-ladder `arm_timer` calls (§3.5) all run on that reactor. The
  supervisor self-heartbeat (a stuck *subagent* must not flip pod liveness; only
  a wedged *reactor* does) is specified in RFC 0010.
- **RFC 0004 (MCP client subset):** stdio MCP servers are children subject to
  reaping (§3.6) and the close-stdin→SIGTERM→SIGKILL shutdown ladder (a
  specialisation of §3.5). Re-establish + re-subscribe on rebuild (§3.11) uses
  the 0004 client.
- **RFC 0005 (Self-MCP server & control protocol):** defines the length-framed
  JSON-RPC control channel carrying `ctrl/ping`, `ctrl/pong`, `ctrl/cancel`,
  `ctrl/ready`, lifecycle and `usage` frames consumed here; and the
  `agent://session/<id>` resource the breaker surfaces (§3.7).
- **RFC 0007 (Agentic loop):** the loop runs in the subagent on a thread
  **separate** from the control reader (the §3.4 hard requirement); it emits the
  `usage` and progress events that drive Detectors B/C and §3.8.
- **RFC 0008 (Execution modes & reactive routing):** the restart breaker drops
  routed events for failed sessions (§3.7); rebuild+reconcile re-arms the routes
  and synthesizes coalesced events (§3.11). loop/reactive are the only modes the
  governor restarts.
- **RFC 0009 (Subagent process model & nesting):** owns the spawn payload, the
  single spawn chokepoint, depth-minting, and the depth/breadth/rate/tree-token
  caps refused as tool results. This RFC owns the `pre_exec` syscalls (§3.9),
  PDEATHSIG (§3.6), the kill ladder, and the temporal restart governor.
- **RFC 0010 (Observability):** every verdict here emits a closed-vocabulary
  event — `subagent.spawn/exit/signal/stuck/restart`, `limit.exceeded`; the
  stuck-leak and breaker-open counters; the supervisor heartbeat / health file.
- **RFC 0011 (Cloud-native contract):** owns the drain *choreography*, the
  `AGENT_DRAIN_TIMEOUT < terminationGracePeriodSeconds` startup validation, and
  the exit-code table this RFC's verdicts map to (0/7/124/137/143). This RFC owns
  the *ladder* the choreography invokes.
- **RFC 0012 (Security posture):** `exec` children are folded into the same
  regime — mandatory deadline (§3.2), process-group kill (§3.5), subtree budget +
  breadth/rate caps — but no control channel, so Detector C (§3.4) does not
  apply.
- **RFC 0013 (deferred v2):** MCP-backed session checkpointing (§3.11) and any
  durable warm-session recovery.

---

## 5. Non-goals / Deferred

- **Aggregate subtree memory enforcement in-binary** — needs cgroups v2
  (`memory.max`/`pids.max`/`cgroup.kill`), a deployment concern. We are
  cgroup-*aware* (§3.10), not cgroup-*requiring*. Only the token ceiling is
  in-binary (§3.8).
- **Session/state checkpointing** — supervisor is stateless in v1; rebuild +
  reconcile + idempotent re-trigger is the recovery story (§3.11). Durable
  warm-session checkpoint is RFC 0013.
- **Restarting one-shot roots** — out of scope by definition (§3.7).
- **Per-ancestor token roll-up / per-branch sub-budgets** — root-only roll-up is
  sufficient for v1 (§3.8); per-ancestor is an optional later extension.
- **Killing `D`-state processes** — physically impossible even with `SIGKILL`; we
  detect, report (stuck-leak metric), and exit uncleanly rather than hang
  (§3.5).
- **Non-Linux supervision** — `prctl(PR_SET_CHILD_SUBREAPER/PR_SET_PDEATHSIG)`,
  `killpg`, the self-pipe-over-`poll` reactor are Linux-shaped; v1 targets Linux
  (assessment baseline). Portability is not a v1 goal.

---

## 6. Open items

None that block implementation. Two values are tunable and may be revised by a
milestone acceptance test, not by redesign:

- The exact `AGENT_PROGRESS_TIMEOUT` (§3.3) default relative to the model
  request timeout — set conservatively at 120s; M2 chaos testing may tighten it.
- The `BREAKER_THRESHOLD` / `SPAWN_FAIL_WEIGHT` (§3.7) — chosen conservatively;
  M2 fork-bomb chaos test confirms they trip before resource exhaustion.
