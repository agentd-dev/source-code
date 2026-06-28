# Design Note — agent as a Schedulable Cloud-Native Unit of Work

**Lens:** behavior of `agent` as a unit of work an external scheduler/operator
starts, stops, replicates, and reacts to. The orchestrator/operator is **out of
scope** (RFC §2). The deliverable here is the **contract** `agent` must honour to
be a perfect citizen for one: config, process lifecycle, signals, exit codes,
state, idempotency, health, and resource friendliness.

**Anchoring RFC sections:** §5 (triggers/modes), §10 (config surface), §11
(deployment shapes), §12 (dependency budget), §13 (security/limits), §14 Q3
(session durability — open).

**Prior art already on disk** (parts bin, not the target, but useful precedent):
`crates/agentd/src/signals.rs` (SIGTERM/SIGINT→`SHUTDOWN_REQUESTED`, SIGHUP→reload,
`SA_RESTART` deliberately off so blocking syscalls return `EINTR` and observe the
flag promptly) and `crates/agentd/src/runtime.rs` exit-code constants
(`EXIT_OK=0`, `EXIT_USAGE=2`, `EXIT_SEMANTIC=5`, `EXIT_PAUSED=7`). The new runtime
should **keep this convention and extend it**, not reinvent it.

---

## 0. The central tension and its resolution

The brief asks us to reconcile two seemingly opposed identities:

- **Unit of work** — ephemeral, run-once, produce a result, exit. Maps to a K8s
  `Job`/`CronJob`, a Knative "run to completion", a queue worker that processes
  one item.
- **Reactive daemon** — long-lived, idles cheaply, wakes on MCP resource updates,
  may keep warm sessions. Maps to a K8s `Deployment`.

**Resolution: these are not two binaries and not even two code paths — they are
the same supervisor loop under two *termination policies*.** The supervisor (RFC
§3, §4.1) is always the same: connect MCP, arm triggers, spawn/supervise subagent
processes, enforce limits, handle signals. What differs is the **exit predicate**:

| Mode | Exit predicate | Deploy shape |
|---|---|---|
| `once` | first root subagent reaches a terminal result | `Job`, CLI invocation |
| `loop` | a bound is hit (max iterations / deadline / cost ceiling) or signal | `Job` with deadline, or `Deployment` |
| `reactive` | **never** on its own; only on signal or fatal/limit | `Deployment` (long-lived) |
| `schedule` (time) | per-fire identical to `once`; process may persist between fires or be re-invoked by an external CronJob | `CronJob` (external) **or** internal interval |

So the "unit of work vs daemon" question collapses to: **does the supervisor's
event source ever go empty-and-final?** One-shot's source is "the instruction";
reactive's source is "an unbounded subscription stream." The lifecycle machinery,
config parsing, signal handling, and process-tree supervision are **byte-for-byte
identical**. This is the design's load-bearing simplification and it must be
preserved: *resist any feature that forks the daemon and the job into divergent
code.*

A direct consequence (and a recommendation): **time-scheduling should be
*external by default*.** A CronJob firing a fresh `--mode once` process per tick
is strictly more robust, more observable, and more 12-factor than an internal
scheduler thread that must survive restarts and clock skew. Ship an **optional**
internal interval (`--mode loop --interval`) for the standalone/no-orchestrator
case, but document that the production path is external CronJob → `once`.

---

## 1. Twelve-factor config contract (Factor III + others)

### 1.1 Precedence and sources

RFC §10 already states: **env first (containers), flags override (CLI)**. Lock
this precedence as a hard rule, top wins:

```
built-in default  <  config file  <  environment variable  <  CLI flag
```

Rationale: env is the container-native injection point (12-factor III); flags are
the human-at-a-shell override and the debugging escape hatch. A config file
(`AGENT_MCP_CONFIG`) exists only for the *verbose, structural* bits (MCP server
lists) that don't fit cleanly in env — never for anything that varies per
deployment environment, and **never for secrets**.

### 1.2 What is configurable (canonical table)

Restating RFC §10 with cloud-native additions. Every row MUST be env-settable.

| Concern | Env | Flag | Notes for scheduling |
|---|---|---|---|
| Instruction | `INSTRUCTION` | `--instruction "…"` / `@file` | `@file` allows ConfigMap/Secret projection |
| Intelligence transport | `AGENT_INTELLIGENCE` | `--intelligence vsock:│unix:│https:` | unix/vsock = no TCP in guest |
| Intelligence creds | `AGENT_INTELLIGENCE_TOKEN` | `--intelligence-token` | **Secret-mounted env or file; never logged** |
| Model / params | `AGENT_MODEL`, `AGENT_MAX_TOKENS` | `--model`, `--max-tokens` | |
| MCP servers | `AGENT_MCP_CONFIG` (file path) | `--mcp name=cmd`, `--mcp-config` | file = ConfigMap volume |
| Mode | `AGENT_MODE` | `--mode once│loop│reactive` | selects exit predicate (§0) |
| Interval | `AGENT_INTERVAL` | `--interval` | loop mode only |
| Subscriptions | `AGENT_SUBSCRIBE` (csv) | repeated `--subscribe URI` | reactive mode |
| Serve self-MCP | `AGENT_SERVE_MCP` | `--serve-mcp transport/addr` | opt-in; off for pure one-shot |
| Enable exec | `AGENT_ENABLE_EXEC` | `--enable-exec [allowlist]` | off by default (§13) |
| Limits | `AGENT_MAX_STEPS`, `AGENT_MAX_TOKENS`, `AGENT_DEADLINE`, `AGENT_MAX_DEPTH`, `AGENT_TREE_TOKEN_BUDGET` | `--max-steps`, `--max-tokens`, `--deadline`, `--max-depth` | bound model-owned loop |
| **Drain timeout** | `AGENT_DRAIN_TIMEOUT` | `--drain-timeout` | **new — must be < pod terminationGracePeriodSeconds** |
| **Log format** | `AGENT_LOG_FORMAT` (json│text) | `--log-format` | json default in container |
| **Log level** | `AGENT_LOG_LEVEL` / `RUST_LOG` | `--log-level` | |
| **Health addr** | `AGENT_HEALTH_ADDR` | `--health-addr` | optional; off ⇒ no listener |
| **Run ID** | `AGENT_RUN_ID` | `--run-id` | idempotency key (§5) |
| **Cgroup path** | `AGENT_CGROUP` (auto-detect default) | `--cgroup` | subagent placement (§7) |

### 1.3 Validation discipline (fail fast, fail at startup)

All config is parsed and **fully validated before any side effect** (before MCP
connects, before the first LLM call, before any subagent spawn). Invalid config
⇒ exit `2` (`EXIT_USAGE`) **immediately**. This is critical for a scheduler: a
config-broken pod should crash in milliseconds with a clear stderr message, not
after a 30s LLM round-trip, so `CrashLoopBackoff` is fast and the failure is
unambiguously "operator error, do not retry blindly."

**Never read config from the network at startup.** No remote config service, no
discovery. The full input is env + flags + a local file (12-factor III).

---

## 2. Process lifecycle

### 2.1 Fast startup (12-factor IX: disposability)

Target: **process is ready within ~hundreds of ms**, dominated only by MCP
handshakes. Concretely:

- No async runtime to spin up (RFC §12 — processes + threads, no tokio).
- Config parse + validate is pure-CPU, sub-millisecond.
- MCP server connections (stdio spawn + handshake) are the only meaningful
  startup latency. Do them **concurrently** (one thread per server handshake)
  and bound each with a connect timeout. A server that fails to handshake within
  timeout is a startup failure (exit `2`/`6`, see §3) — not a silent degrade,
  unless explicitly marked optional.
- **Readiness is declared only after all required MCP handshakes succeed and
  (reactive mode) all declared subscriptions are confirmed.** See §6.

### 2.2 Shutdown sequence — graceful drain then exit (12-factor IX)

On `SIGTERM` or `SIGINT`, run a **bounded, monotonic** drain. Reuse the
`signals.rs` one-way `SHUTDOWN_REQUESTED` flag pattern; the supervisor's main
select/poll loop observes it and transitions to DRAINING. One-way: once draining,
never return to running.

**Drain algorithm (supervisor):**

1. **Stop accepting new work.** Disarm triggers: stop the interval timer; stop
   routing new MCP `resources/updated` notifications into spawn/continue; if
   serving self-MCP, return "shutting down" to new `subagent.spawn` calls.
2. **Let in-flight subagents finish, bounded.** Signal each in-flight root
   subagent's loop to stop at its next safe turn boundary (inject a cooperative
   "wind down" control message; the agentic loop already checks for cancel each
   turn per RFC §6.1). Give them until `min(AGENT_DRAIN_TIMEOUT, deadline)`.
3. **Escalate.** Subagents still alive at the soft deadline get `SIGTERM`; those
   alive after a short grace get `SIGKILL`. Because subagents are real child
   processes (RFC §4.2), this is reliable OS-level termination, not cooperative
   unwinding.
4. **Flush & exit.** Flush logs/traces, close MCP client connections (which
   reaps stdio MCP server children — see §2.4), exit with the appropriate code
   (§3).

**The drain timeout MUST be operator-configurable and MUST be set safely below
the pod's `terminationGracePeriodSeconds`** (default 30s in K8s). Recommended
default `AGENT_DRAIN_TIMEOUT=25s` with K8s `terminationGracePeriodSeconds: 30`,
leaving headroom for SIGKILL escalation + log flush before the kubelet's own
SIGKILL lands. **If the kubelet SIGKILLs us first, we lose; so our internal
budget must always be the smaller number.** Document this coupling loudly — it is
the single most common cloud-native footgun.

### 2.3 SIGKILL safety (no graceful path runs)

We cannot run code on `SIGKILL`. Therefore safety on SIGKILL is a *design
property of state*, not a handler:

- **The supervisor holds no durable state that a SIGKILL can corrupt** (RFC §4.1
  — "holds no conversation state, makes no LLM calls"). Nothing to flush ⇒
  nothing to corrupt.
- **Subagent stdio children:** if the supervisor is SIGKILLed, its children are
  *not* automatically killed by the kernel (orphaned, reparented to init). Two
  mitigations, both recommended:
  - **cgroup-scoped tree (Linux):** place the whole subagent tree in a child
    cgroup (§7). On pod teardown the kubelet kills the *pod cgroup*, taking every
    descendant with it regardless of supervisor state. This is the real
    backstop and the reason §7 matters for correctness, not just limits.
  - **`PR_SET_PDEATHSIG` / `prctl(PDEATHSIG, SIGKILL)`** in each spawned
    subagent (Linux) so a child dies if its *immediate* parent dies. Cheap,
    `libc`-only, already within the dependency budget. Note PDEATHSIG only
    chains to the immediate parent, so cgroup teardown remains the tree-wide
    guarantee.
- **Idempotency covers the rest:** anything a SIGKILL interrupts mid-flight is
  recovered not by cleanup but by a *retried, idempotent re-run* (§5).

### 2.4 MCP server child reaping

stdio-transport MCP servers are themselves child processes (RFC §7.1). They must
be reaped on every exit path (clean, drain, panic). They go in the **same cgroup
scope** as subagents so SIGKILL/teardown reaps them too. Track them in the
supervisor's process table alongside subagents; the drain sequence closes their
stdin (polite EOF), waits briefly, then SIGTERM/SIGKILL.

---

## 3. Exit-code contract (the scheduler's primary signal)

Exit codes are how a K8s `podFailurePolicy` decides retriable vs terminal
(research: `onExitCodes` with actions Ignore/FailJob/Count). We therefore design
codes to be **machine-actionable**, partitioned into "do not retry" vs "retry may
help." Extend the existing `runtime.rs` convention:

| Code | Name | Meaning | Scheduler action |
|---|---|---|---|
| `0` | `EXIT_OK` | One-shot succeeded; loop hit a clean bound; daemon drained cleanly on SIGTERM | Success. Job → Complete |
| `1` | `EXIT_FAILURE` | Generic/unexpected error not otherwise classified | Retriable (treat as transient) |
| `2` | `EXIT_USAGE` | Bad config/flags/env; validation failed | **Non-retriable** — `FailJob`. Operator error |
| `3` | `EXIT_PARTIAL` | One-shot produced a *partial* result (some sub-tasks failed; deadline/budget hit mid-work but useful output emitted) | Policy-dependent; default retriable |
| `4` | `EXIT_INTELLIGENCE` | LLM/intelligence endpoint unreachable or erroring after retries | Retriable (often transient/upstream) |
| `5` | `EXIT_SEMANTIC` | Agent ran correctly but concluded the task *cannot* be done / refused / produced a defined failure | **Non-retriable** — deterministic, retry won't help |
| `6` | `EXIT_MCP` | A required MCP server failed to connect/handshake or died unrecoverably | Retriable (sidecar may be racing/coming up) |
| `7` | `EXIT_BUDGET` | Hit max-steps / max-tokens / deadline / tree budget before a result | Policy-dependent; usually non-retriable (raise budget, don't blind-retry) |
| `124` | `EXIT_TIMEOUT` | Hard wall-clock deadline (`--deadline`) tripped | Mirrors `timeout(1)` convention |
| `137` | (`128+SIGKILL`) | Killed (OOM, kubelet SIGKILL) | Reported by kernel; OOM ⇒ raise memory limit |
| `143` | (`128+SIGTERM`) | Exited *because* of SIGTERM without clean drain (escalated) | Distinguishes ungraceful from `0` |

**Rules:**

- **A graceful SIGTERM drain that completes cleanly returns `0`**, not `143`. We
  caught the signal, drained, and chose to exit successfully. `143` is reserved
  for "the runtime did not get to drain cleanly." This distinction matters: a
  reactive `Deployment` rolled by the operator should look like a *clean* exit,
  not a failure, in dashboards.
- **`once` mode maps the root subagent's terminal outcome to a code**: completed
  ⇒ `0`; refused/cannot ⇒ `5`; partial ⇒ `3`; budget ⇒ `7`. This mirrors the
  existing `runtime.rs` `Completed ⇒ EXIT_OK, _ ⇒ EXIT_SEMANTIC` logic but with
  finer partitioning so a `podFailurePolicy` can branch.
- **Loop/reactive daemons** essentially only ever exit `0` (clean drain), `143`
  (ungraceful), or a fatal class (`4`/`6`/`137`). They do not exit on individual
  task failure — a failed reaction is logged and the daemon keeps serving.
- Document the exact list in `--help` and in a `docs/` table so operators can
  write `podFailurePolicy` rules against it. **The exit-code contract is a public
  API; treat changes as breaking.**

---

## 4. Statelessness vs state (12-factor VI) — and reactive-restart survival

### 4.1 Classification

| State | Where it lives | On restart |
|---|---|---|
| Config | env/flags/file (external) | Re-read from environment — by definition |
| MCP connections | in-memory, reconstructable | **Re-established** (re-handshake) |
| Subscriptions (declared) | config (`--subscribe`) | **Re-subscribed** from config |
| Subscriptions (dynamic, via self-MCP `subscribe`) | in-memory | **Lost** unless checkpointed (see §4.3) |
| Warm/suspended reactive sessions | in-memory | **Lost** in v1 (RFC §14 Q3 bias) |
| Subagent process tree | OS processes | **Gone** (children died with the pod) |
| Final results / outputs | **externalized** to an MCP server (fs/db/queue) | Durable — that's the point |

### 4.2 The core principle

**The supervisor is stateless and share-nothing (12-factor VI).** All durable
output is written *through MCP tools* to backing services (a filesystem MCP, a db
MCP, a queue MCP) — never to supervisor-local memory or local disk as the source
of truth. This is exactly 12-factor "stateless processes; persist to a backing
service," and it falls out naturally because `agent` has **no built-in tools** —
the only way it can persist anything is by calling an MCP server, which is by
construction an external backing service.

### 4.3 How a reactive daemon survives a restart

This is RFC §14 Q3, and the brief asks us to specify it. Recommendation, in two
tiers:

**v1 — Rebuild, don't checkpoint (RFC's stated bias).** On restart the supervisor:

1. Re-reads config (env/flags/file).
2. Re-establishes all MCP connections.
3. **Re-issues `resources/subscribe` for every *declared* subscription** and,
   for each, does an immediate `resources/read` to get current state — because a
   resource may have changed *while the pod was down* and we'd otherwise miss the
   edge. This "read-after-subscribe reconciliation" replaces edge-triggering with
   level-triggering across the restart boundary, which is the correct
   cloud-native pattern (reconcile to desired state, don't rely on having seen
   every event). **This is the key reactive-restart correctness rule.**
4. Warm in-memory sessions and dynamic (self-arranged) subscriptions are **lost**.
   In-flight reactions are recovered by idempotent re-trigger (the resource that
   triggered them is re-read in step 3 and re-fires if still in the triggering
   state).

This makes a reactive daemon **restart-safe without any persistence layer**,
provided two invariants hold: (a) all meaningful work is externalized through MCP
(§4.2), and (b) reactions are idempotent (§5). Both are already required by the
architecture. **A daemon restart is therefore equivalent to a cold start that
reconciles** — exactly what a `Deployment` rollout needs.

**v2 (optional, later) — Checkpoint warm sessions.** If/when stateful
multi-turn reactive sessions become valuable enough to survive restarts, add an
*optional* checkpoint: serialize warm-session context to an MCP-backed store (not
local disk — local disk is not durable across pod reschedule) keyed by `RUN_ID`,
and on startup rehydrate. Keep it **off by default** and behind a flag; it adds
weight (serialization format, store contract) that violates the minimalism bar
for the common case. Edge-vs-level reconciliation (v1) should make this rarely
necessary.

**Explicitly reject:** local-disk session files as durable state. A pod can be
rescheduled to a new node; `emptyDir`/container FS is ephemeral. Durable = an
external backing service reached over MCP. (12-factor VI again.)

---

## 5. Idempotency of a one-shot run

A scheduler retries (research: `backoffLimit`, exponential backoff, at-least-once
semantics). Therefore **a one-shot run must be safe to execute more than once.**
`agent` cannot *make* an arbitrary instruction idempotent — but it must provide
the **mechanism** for the deployment to achieve it:

- **`RUN_ID` (idempotency key).** Accept `AGENT_RUN_ID` / `--run-id`. Propagate
  it into the agent's context and, crucially, **into every MCP tool call's
  metadata** so an MCP backing service that supports idempotency keys
  (e.g. a queue with dedupe, an HTTP API with `Idempotency-Key`) can dedupe
  retried side effects. Default: a per-process random ULID (so logs/traces
  correlate), but for retry-dedupe the operator should set a *stable* key per
  logical unit of work (K8s can inject the Job name / a hash).
- **Read-modify-write through MCP, not blind append.** Encourage (in docs and the
  default system prompt) the pattern: the agent checks current state via
  `resources/read` before mutating, so a re-run that finds work already done is a
  no-op. This is the level-triggered reconcile pattern again, applied to one-shot.
- **Deterministic terminal classification.** A re-run of an already-complete unit
  should detect "already done" (via the backing service) and exit `0`
  immediately, cheaply — not redo LLM work. This makes retries cheap and safe.
- **No hidden local side effects.** `agent` itself writes nothing durable
  locally except logs (stdout/stderr, which are append-only and harmless to
  duplicate). All side effects go through MCP, where the idempotency key can act.

**Honest scope statement:** true idempotency is a property of the *instruction +
the MCP tools it uses*, which `agent` does not own. Our contract is: (1) provide
and propagate a stable idempotency key, (2) never introduce *our own*
non-idempotent local side effects, (3) make "already done" cheap to detect and
exit on. Beyond that, idempotency is the operator's composition responsibility —
consistent with "composition is MCP, not a control plane we own" (RFC §2).

---

## 6. Readiness / liveness / startup semantics

K8s uses three probe types. Map each precisely; **keep them dependency-light** —
a tiny blocking HTTP/1.1 listener (already in the dependency budget, RFC §12) on
`AGENT_HEALTH_ADDR`, off entirely when unset (so a pure one-shot CLI run carries
no listener).

- **Startup probe → `/startup` (or readiness gating).** Returns `200` only once
  config is validated, all *required* MCP servers have handshaked, and (reactive)
  all declared subscriptions are confirmed + initially reconciled (§4.3). Until
  then `503`. This prevents the scheduler from sending work / counting the pod
  ready while MCP sidecars are still racing to come up. Gives slow MCP
  handshakes room without a generous liveness timeout.
- **Liveness → `/healthz`.** Returns `200` while the **supervisor loop is
  responsive** (it can answer the probe ⇒ its main thread isn't wedged). It MUST
  reflect *supervisor* health, **not** subagent or LLM health — a stuck/runaway
  subagent is a *contained, recoverable* condition the supervisor handles
  (kill + restart/abandon per §8); it must **not** cause a liveness failure that
  kills the whole pod and throws away every other healthy subagent and warm
  session. Tie liveness to a supervisor heartbeat (a counter the main loop bumps;
  the probe thread checks it advanced within N seconds). If the supervisor's own
  loop is wedged → liveness fails → kubelet restarts → clean rebuild (§4.3).
- **Readiness → `/readyz`.** For a reactive daemon: `200` when subscriptions are
  active and the supervisor can accept/route events. Flips to `503` the instant
  drain begins (§2.2) so the operator/endpoint controller stops routing to a pod
  that's shutting down — standard pre-stop drain choreography.
- **One-shot mode:** probes are largely irrelevant (the process is short-lived;
  the scheduler watches *exit code*, not probes). Health listener stays off
  unless explicitly enabled. Don't pay for what a `Job` doesn't use.

**Liveness philosophy (important):** prefer **few false-positive liveness
failures**. The cost of an over-eager liveness probe in an agent runtime is high —
it discards warm sessions and in-flight subagents. Bias toward the supervisor
self-healing internally (kill the bad subtree) and only fail liveness when the
*supervisor itself* is unrecoverable.

---

## 7. Resource requests/limits friendliness (cgroup-aware tree)

The subagent tree (RFC §6.3) is a process fan-out; under a memory/pids limit it
must not let one runaway branch OOM-kill the pod or exhaust PIDs.

- **Be cgroup-v2 aware (Linux).** On startup, detect the pod's memory ceiling by
  reading `memory.max` from the cgroup (and `memory.high` for soft pressure)
  rather than trusting host `/proc/meminfo` — a container that reads host RAM
  will mis-size everything. Use this to set sane defaults for tree-wide budgets
  and to refuse to spawn beyond what fits.
- **Place the subagent tree in a child cgroup** (`AGENT_CGROUP` or
  auto-detected sub-path). Benefits, all real:
  - **Tree teardown is a kernel op:** writing to `cgroup.kill` (or removing the
    cgroup) kills every descendant atomically — the reliable backstop for §2.3
    and §8, independent of supervisor liveness.
  - **`pids.max`** caps total processes so a recursive `subagent.spawn` storm
    can't fork-bomb the node (defense-in-depth alongside `--max-depth` and
    tree budget from RFC §13).
  - **`memory.max`** on the subtree contains a memory-hungry agent to a fraction
    of the pod, so the OOM killer hits the offending subtree, not the supervisor.
    The supervisor survives, observes the child's `137`, and reacts cleanly.
- **Respect, don't reimplement.** We do not build a sandbox (RFC §13). If cgroup
  control files aren't writable (non-Linux, restricted runtime, no delegation),
  degrade gracefully: fall back to `rlimit` (`RLIMIT_AS`, `RLIMIT_NPROC`) +
  `PDEATHSIG` + application-level budgets, and log that kernel-level tree
  teardown is unavailable. **Never hard-require cgroup write access** — many
  managed runtimes don't delegate it.
- **Memory ceiling for the supervisor itself** is tiny and bounded by design (no
  async runtime, no model state, no buffering of conversation). The memory story
  is: supervisor ~flat and small; cost is in subagents; subagents are
  individually capped and collectively cgroup-bounded. This makes
  `resources.requests`/`limits` easy for an operator to set: request for
  supervisor + headroom, limit sized to the expected subagent fan-out.
- **Memory-pressure backpressure:** when the subtree approaches `memory.high`,
  the supervisor should *stop spawning new subagents* (return "at capacity" to
  `subagent.spawn`) rather than push the pod into OOM. Cheap to implement
  (read one cgroup file before spawn), high value.

---

## 8. Detecting dead/stuck subagents and staying stable (req. 8)

Maps the brief's reliability requirement onto the lifecycle contract:

- **Dead (crashed):** child process exit is observed via `waitpid`/exit-status
  reaping in the supervisor's process table. Crash ⇒ classify (signal vs nonzero)
  ⇒ surface as that subagent's result; supervisor never dies with it (RFC §3 —
  intelligence isolated in a child).
- **Stuck (no progress):** the agentic loop streams events every turn (RFC §6.1).
  The supervisor tracks **last-event time per subagent**; if a subagent emits no
  event within a configurable *progress timeout* (distinct from total deadline),
  it's declared stuck → SIGTERM → SIGKILL. This catches a wedged LLM call or a
  hung MCP tool that the per-turn deadline alone wouldn't.
- **Runaway (making progress but exceeding budget):** caught by the existing
  budgets — max-steps/tokens/deadline per subagent, tree-wide token ceiling and
  max-depth for the whole tree (RFC §13).
- **Recover state:** because durable state is externalized (§4.2) and reactions
  are idempotent (§5), "recover" means *re-trigger from reconciled resource
  state*, not *resume an in-memory checkpoint*. A killed subtree's work is redone
  safely on the next (level-triggered) reaction.
- **Supervisor self-stability:** the supervisor has no model dependency, bounded
  memory, and a single poll/select loop guarded by the liveness heartbeat (§6).
  Its failure modes are narrow and externally recoverable (restart → rebuild).

---

## 9. The same binary, three deploy shapes — concrete config recipes

Demonstrating §0: identical binary, different env. (Operator manifests are out of
scope; these are the `agent`-side knobs each shape sets.)

**A. Standalone CLI one-shot**
```
agent --mode once \
  --instruction "summarize the open PRs and write a digest" \
  --intelligence https://gateway.local/v1 --intelligence-token $TOK \
  --mcp github=github-mcp --mcp fs=fs-mcp
# prints result, exits 0/3/5/7. No health listener, no self-MCP, no daemon.
```

**B. Long-lived reactive daemon (Deployment-shaped)**
```
AGENT_MODE=reactive
AGENT_SUBSCRIBE=db://query/inbound_tasks,file:///work/inbox
AGENT_MCP_CONFIG=/etc/agent/mcp.json
AGENT_INTELLIGENCE=unix:/var/run/intel.sock     # sidecar gateway, no TLS in-binary
AGENT_HEALTH_ADDR=:8080                          # liveness/readiness/startup
AGENT_SERVE_MCP=unix:/var/run/agent.sock        # composable by peers
AGENT_DRAIN_TIMEOUT=25s                           # < terminationGracePeriodSeconds:30
AGENT_LOG_FORMAT=json
# never exits on its own; SIGTERM → drain → exit 0.
```

**C. Subagent scheduled by a resource-list update OR by time**
- *By resource-list update:* this is just shape **B** reacting; the "schedule" is
  the subscription firing `spawn`. No extra config — reactivity *is* the
  scheduler here.
- *By time, external (recommended):* a CronJob fires shape **A** per tick with a
  stable `--run-id` derived from the schedule slot for retry-dedupe.
- *By time, internal (standalone fallback):* `--mode loop --interval 5m` with a
  total `--deadline`/iteration cap so it still terminates.

The point: **C is not a fourth code path.** "Scheduled by a resource-list update"
= reactive `spawn` (B's machinery). "Scheduled by time" = external CronJob → A,
or internal interval = a bounded loop. No new lifecycle, no new state model.

---

## 10. Observability contract (req. 6) — scheduler-facing slice

- **Logs to stdout/stderr only** (12-factor XI — treat logs as event streams;
  never write log *files* as the durable record). Structured JSON by default in
  container (`AGENT_LOG_FORMAT=json`), human text for CLI. Each line carries
  `run_id`, subagent id, tree path/depth, trigger cause. The existing
  `target: "agentd::audit"` event-name discipline from `signals.rs` is a good
  precedent — keep stable, greppable `event=` names.
- **Secrets never logged** (RFC §13) — intelligence tokens, MCP server env. Enforce
  at the logging boundary, not by convention.
- **Lifecycle events are first-class log records:** `startup.ready`,
  `subscribe.confirmed`, `trigger.fired`, `subagent.spawned/exited`,
  `drain.begin`, `drain.escalate.sigkill`, `exit{code}`. These let an operator
  build dashboards and alerts without scraping a metrics endpoint.
- **Health endpoint is the cheap, dependency-free signal** (§6). Heavier
  tracing/OTLP is **not** core (RFC §12 explicitly cuts OTLP); if a deployment
  wants distributed tracing it composes a collector — consistent with the moat
  being minimalism, not a built-in telemetry stack.

---

## 11. Summary of recommendations (the contract, distilled)

1. **One binary, one supervisor loop, different exit predicates.** `once`/`loop`/
   `reactive`/`schedule` are termination policies over identical machinery — never
   fork the daemon and the job into divergent code.
2. **Config: built-in < file < env < flag.** Everything env-settable; secrets
   env/flag only, never file, never logged. **Validate fully at startup; bad
   config ⇒ exit `2` in milliseconds.**
3. **Signals:** SIGTERM/SIGINT → bounded drain (disarm triggers → wind down
   subagents → SIGTERM/SIGKILL stragglers → flush → exit). `AGENT_DRAIN_TIMEOUT`
   **must be < pod `terminationGracePeriodSeconds`** (default 25s vs 30s).
4. **Exit codes are a public, machine-actionable API:** `0` ok, `2` non-retriable
   usage, `3` partial, `4` intelligence, `5` non-retriable semantic, `6` MCP,
   `7` budget, `124` timeout, plus kernel `137`/`143`. A clean SIGTERM drain
   returns `0`, not `143`. Designed for `podFailurePolicy` to branch on.
5. **Stateless supervisor; all durable output externalized through MCP backing
   services.** No local-disk source of truth.
6. **Reactive restart = rebuild + reconcile**, not checkpoint: re-subscribe from
   config and **read-after-subscribe** to convert edge-triggering into
   level-triggering across the restart boundary. Warm sessions in-memory in v1
   (lost on restart, recovered by idempotent re-trigger); optional MCP-backed
   checkpoint later.
7. **Idempotency:** propagate a stable `RUN_ID` into every MCP tool call; never
   introduce local non-idempotent side effects; make "already done" cheap to
   detect → exit `0`. True idempotency is composed via MCP, not owned.
8. **Health:** tiny HTTP listener, off unless `AGENT_HEALTH_ADDR` set.
   Liveness = supervisor heartbeat only (a stuck subagent must **not** fail
   liveness). Readyz flips `503` on drain. Startup gates on MCP handshakes +
   subscription reconcile.
9. **cgroup-v2-aware subagent tree:** read `memory.max`, place tree in a child
   cgroup for `cgroup.kill` teardown + `pids.max` + `memory.max` containment;
   `PDEATHSIG` + `rlimit` fallback where cgroup delegation is absent. Never
   hard-require cgroup write access. Backpressure on `memory.high`.
10. **Stuck-detection** via per-subagent last-event progress timeout, on top of
    budgets; killed subtrees recovered by reconciled re-trigger, keeping the
    supervisor stable and the pod long-lived.

---

## 12. Open questions surfaced for the broader design

- **RFC §14 Q3 (session durability):** this note recommends *rebuild+reconcile*
  for v1 and confirms in-memory warm sessions are acceptable **iff** read-after-
  subscribe reconciliation is implemented. Without that reconciliation step, a
  restart silently drops events that occurred while down — so reconciliation is
  **not optional** even in the in-memory tier.
- **Should `EXIT_PARTIAL`/`EXIT_BUDGET` default retriable or not?** Recommend
  making it operator-policy via a flag (`--budget-exit-code`), since "raise the
  budget" vs "retry" is deployment-specific.
- **Time-scheduling:** recommend external CronJob as the production path; internal
  interval is a standalone convenience, not the primary mechanism. Confirm we are
  comfortable *not* building a robust internal scheduler (clock skew, missed-tick
  catch-up, persistence) — the brief says the orchestrator is out of scope, which
  argues for leaning external.
- **Does `RUN_ID` propagation into MCP tool-call metadata require an MCP
  extension?** MCP `tools/call` has a `_meta` field; using it for an idempotency
  key is in-spec but depends on the backing server honouring it. Needs a small
  documented convention.
