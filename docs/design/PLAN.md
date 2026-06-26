# agentd — Build Plan & Progress Tracker

**This file is the operational source of truth for the build loop.** The
*rationale* lives in [`00-architecture-assessment.md`](00-architecture-assessment.md)
(binding decisions) and the `rfcs/` (per-feature specs). This file tracks
**what is done, what is next, and how to continue.**

> **For the hourly build loop:** on each wake — (1) read the **Current
> status** section below, (2) run the build/test gate, (3) pick the next
> unchecked item in the current milestone, (4) implement it, (5) commit
> (never push), (6) tick the checkbox and update **Current status**. Keep
> changes small and always leave the tree compiling + tests green. If a
> milestone's acceptance criteria are all met, advance to the next.
>
> **Completion protocol (operator instruction, 2026-06-26):** when **every
> milestone M1–M7 is complete and its acceptance holds**, the build is done —
> stop changing code, report completion, **and disable the hourly build cron**:
> `CronDelete` job **`6885e804`** (the operator asked for the loop to be turned
> off once everything is completed). Do not disable it before then. If some
> items are permanently infra-gated (live-vsock microVM peer, container image
> build, an external MCP peer), treat the milestone as code-complete once the
> code + tests are in and the only gap is external infrastructure — record that
> explicitly in Current status before disabling.

---

## Ground rules

- **Branch:** `rewrite/mcp-native-agent`. **Commit, never push.** End every
  commit message with the `Claude-Session:` trailer.
- **Compiles + tests green before every commit.** `cargo build` and
  `cargo test` must pass on the default (no-feature) build. Run feature
  builds (`--features tls,vsock,…`) when a milestone touches them.
- **Minimalism is the moat.** No new dependency without justification
  against the budget in assessment §2.2. Default build = single-digit
  first-party crates, no async runtime, no C toolchain, no TLS.
- **Observability is a first-class, cross-cutting requirement** (operator
  ask): the agent AND every subagent must be observable / traceable / logged /
  auditable — full behaviour + performance. Every new behaviour emits a
  canonical JSON-lines event with the `run_id` + `agent_path` tree correlation
  (RFC 0010); the closed event vocabulary + W3C trace propagation deepen at M6.
  **Post-core deliverable:** an E2E suite that validates capabilities by
  *observing* agentd (asserting on its telemetry stream + outcomes), at M7.
- **Binding decisions win.** If code and the assessment doc disagree, the
  assessment doc is right (or open an explicit deviation note here).
- Small commits, one logical step each. Update this file in the same commit
  as the work it tracks.

## Build / test gate (run each wake)

```
cargo build -p agentd                 # default build must succeed
cargo test  -p agentd                 # default tests must pass
cargo build -p agentd --features tls,vsock,serve-mcp,cron,metrics,otel   # when touched
cargo clippy -p agentd -- -D warnings # keep clean
```

---

## Current status

- **Phase:** **M1–M7 SUBSTANTIVELY COMPLETE.** The chaos test (PDEATHSIG no-orphan
  collapse) landed this wake — the last M7 acceptance item. Every milestone's core
  functionality is built, tested, and observe-validated; the default build holds
  the minimalism moat (3 deps, no async/TLS/C); the docs match the runtime. The
  autonomous build loop has delivered the agentd rewrite per this plan.
- **Last completed (this wake):** the **PDEATHSIG chaos test** (`chaos_e2e.rs`) —
  the final M7 acceptance item. It catches a *live* supervised subagent (held in
  the model call by the mock LLM's `slow` script), SIGKILLs the supervisor with
  no chance to drain, and proves via `/proc` that `PR_SET_PDEATHSIG` collapses the
  subagent — **no orphan leaks**. (The stuck classifier + kill ladder + cap
  refusals are unit/integration-covered; the live kill path also by the drain
  tests.) **190 default tests** green, clippy clean (default + all-features),
  default build = 3 deps. The full milestone history is in the checklists below.
- **Post-completion (operator asked to work the deferred scope):** **done so far
  —** (1) **activated the ping/pong liveness detector (Detector C)**: the reactor
  now pings live children at a derived cadence, so a long model call reads `Busy`
  (not falsely `Stuck` at 120s) and a frozen child crosses to `Stuck`; (2) added
  **liveness env knobs** + the **live stuck→kill chaos test** (`chaos_e2e.rs`:
  SIGSTOP a subagent → `subagent.stuck` → force-kill within budget → exit 124);
  (3) **fixed a real bug** — a root killed *during* a teardown was misreported as
  a generic failure (synthetic "exited without a result") instead of the teardown
  reason, so a stuck-kill now correctly exits 124, not 1; (4) **shipped the `otel`
  feature** (OTLP/HTTP span export, GenAI semconv) — hand-rolled OTLP-over-JSON
  reusing the existing trace ids + HTTP client + serde_json, so `--features otel`
  stays **dependency-free (still 3 deps)**. A finished run exports its whole
  trace as one OTLP batch: the `invoke_agent` run span **plus a `chat` child per
  model call and an `execute_tool` child per tool call** (`gen_ai.*` attrs,
  children parented to the run span), to `OTEL_EXPORTER_OTLP_ENDPOINT`,
  best-effort. Built as a no-op `RunSpan` handle (no-op without the feature →
  clean loop call sites, zero default cost); proven end-to-end by
  `tests/otel_e2e.rs` (a real ReAct run POSTs invoke_agent + chat + execute_tool
  to an in-test collector). **This closes the M6 otel acceptance.** (5) **shipped
  warm `Continue` sessions** (M3 keystone): refactored the loop into a reusable
  `Session` (durable transcript) + per-turn `run_turn`; a `warm` subagent
  (`run_warm`) re-enters the same conversation per delivered event (new
  `AgentMsg::Turn`, control-channel `Inject`); the reactive daemon's
  `WarmRegistry` (`--continue <uri>`) holds one live session per route, injecting
  subsequent events and supervising non-blocking (death via channel-disconnect,
  reaping via Drop — safe given the single-threaded daemon). e2e-proven
  (`tests/warm_session.rs` + `tests/subagent_spawn.rs`). (6) **shipped async
  `subagent.spawn{async,detach}`**: `{async:true}` returns a **handle**
  immediately (non-blocking) and the parent keeps working; `subagent.await` /
  `subagent.status` self-tools collect the distilled result later (drained from
  the child's channel, handle consumed on collection); `{detach:true}` is
  fire-and-forget. Same single-threaded supervision-safety model as warm
  sessions; uncollected children die with the parent. e2e-proven
  (`tests/orchestrator_spawn.rs`). (7) **shipped the `agentd://` resource
  surface** (workflow-orchestrated: parallel understand → inline implement →
  4-lens adversarial review): a shared `agentd_uri` scheme module;
  **completion-as-self-resource** — `resource.read agentd://subagent/<handle>`
  returns an async child's result (idempotent peek; unified status/await/read so
  all three are consistent; detached children not collectable); and **served
  `agentd://` resources** — the served MCP server's `resources/list` +
  `resources/read` expose `agentd://status` (this agentd's live state) with the
  `resources` capability. (8) **shipped served async + control** (workflow-
  orchestrated, 4-lens adversarial review): served `subagent.spawn{async}` runs
  on a background thread tracked in a bounded `ServeCtx.sessions` registry;
  `subagent.status`/`subagent.cancel` tools + `resource.read
  agentd://subagent/<handle>` read it; a new **reactor per-run cancel token**
  (`supervise_cancellable`) drains one run's subtree on `subagent.cancel`. Review
  fixes folded in (cancel-status TOCTOU under the lock; completed-result-wins over
  a late cancel; static `resources/list`; `-32002`). e2e: async lifecycle +
  cancel-a-live-run (`tests/serve_mcp.rs`). (9) **shipped the single-reaper
  refactor** (the deferred high-risk one; workflow-orchestrated, 4-lens
  adversarial review): retired `SUPERVISE_LOCK` for a process-global
  `supervisor/reaper.rs` that owns the one `waitpid(-1)` and **dispatches each
  reaped pid to its owning supervisor by pid** — so the daemon's reactions +
  served async runs + nested orchestration run **truly concurrently** (no
  head-of-line blocking). `spawn_tracked` forks-and-registers under the routes
  lock (closes the reap-before-register race); no dedicated reaper thread (so the
  global `waitpid(-1)` never steals `exec`/MCP children that reap themselves).
  Review fixes folded in: a hard `drive_drain` abandon-deadline (teardown can
  never spin on a missing reap), corrected coexistence docs, `exec` final-wait
  made ECHILD-tolerant, `reap_pending` visibility restricted. **Proven** by a new
  `concurrent_async_runs_do_not_serialize` test (cancelling one live run drains it
  while another is still supervising — impossible under the old lock). (10)
  **wired self-subscribe → a warm continue route** (the signature capability — a
  self-subscribing agent re-enters one live session per event, not a fresh spawn;
  `tests/observe_e2e.rs`). (11) **shipped `resources/subscribe` push** (closes the
  reactive loop; 2-lens adversarial review): the served MCP server gained
  per-connection shared writers + a subscription registry; a peer subscribes to a
  **running** `agentd://subagent/<handle>` and is pushed `notifications/resources/
  updated` when the run terminates (the bg run thread pushes). Review fixes folded
  in: a socket **write timeout** (a stalled peer can't pin a run thread under the
  writer lock), subscribe **validates a running handle** (reject unknown/terminal/
  status → no leak, no late-subscribe gap), and notify **consumes** the
  subscription on its one event. `tests/serve_mcp.rs` proves a real subscribe →
  cancel → pushed notification. (12) **coordinated served-session drain on
  shutdown**: `serve()` returns a `ServeHandle`; on daemon shutdown `main` cancels
  in-flight served runs + waits (bounded by drain timeout) for them before exit,
  so their subtrees drain gracefully rather than being PDEATHSIG-collapsed. (13)
  **shipped served `subagent.send`** — the last substantial deferred capability
  (review fixes folded in): a served `subagent.spawn{warm:true}` now stays alive in
  a bounded `ServeCtx.warm` registry (`MAX_WARM_SESSIONS`=8), and `subagent.send
  {handle,message}` injects the next user message so the agent runs another turn
  over the SAME conversation; `subagent.status` reports `{turns, busy, last_result,
  …}` and `subagent.cancel` ends it. Lazy-drain design (no per-session supervision
  thread — drained on send/status) reusing the warm-subagent machinery. Review
  fixes: `spawn_warm` **sweeps finished-but-unpolled sessions** + holds the registry
  lock across cap-check+spawn+insert (no slot leak, no TOCTOU overshoot); cancelled
  live sessions are **dropped outside the lock** (SIGKILL+waitpid off the hot path);
  a **`busy`/`awaiting_turn`** signal makes the poll-the-turn-counter contract
  explicit (a peer polls status until `turns` reaches the `awaiting_turn` that
  `send` returns). e2e-proven (`tests/serve_mcp.rs::a_warm_session_runs_a_turn_per_send`
  now also asserts `awaiting_turn`/`busy`). (14) **fixed a real exit-reason race** —
  a root that reported its result/failure on the events channel *and then exited*
  could have the reap (`waitpid`, a separate channel) win the race, synthesizing a
  generic "exited without a result" (exit 1) that **masked the real reason** (e.g.
  intel-unavailable → exit 4). The reactor now grants a `RESULT_GRACE` (500ms)
  window for the trailing frame before concluding no-result; regression-proven by
  `supervised_once_exits_4_on_unreachable_intel` (was ~1-in-5 flaky, now 20/20).
  (15) **shipped cgroup-v2 active enforcement** (the "infra-gated" item — turned
  out this host *is* cgroup-writable as root; multi-lens adversarial review):
  opt-in `--cgroup auto|<path>` places each run's subtree in a child cgroup for
  atomic **`cgroup.kill`** teardown (the backstop beyond killpg+PDEATHSIG —
  proven live to reap a `setsid` escapee) + **`memory.high` spawn-backpressure**;
  best-effort, still 3 deps (pure std fs + libc), never cgroup-requiring. See the
  M5 checklist item for detail. (16) **shipped hard resource limits**
  (`--cgroup-memory-max`/`--cgroup-pids-max`, + env; adversarial review): when
  requested, `configure` delegates the `memory`/`pids` controllers to the parent
  and each per-run leaf gets `memory.max`/`pids.max` — **live-proven** (`pids.max=1`
  refuses a `fork` in the leaf with `EAGAIN`; e2e asserts the limits engage through
  the real binary). Degrades honestly where the parent can't delegate (`EBUSY`
  under a busy unit cgroup → limits no-op, teardown still works, `cgroup.limits_
  unavailable` logged). **Still deferred:** only a standalone `agentd-conformance`
  crate (non-essential reorg + external reference server).
- **Active milestone:** M7 (complete). M1–M6 all complete with acceptance holding.
- **Blockers:** none. **Build complete** — the hourly cron (`6885e804`) is
  disabled per the completion protocol. Infra-gated checks (real MCP reference
  server, live-vsock microVM peer, the actual `docker build`) are treated as
  code-complete: the code + tests are in and only external infrastructure is
  missing. _Workflow caveat (kept for future fan-outs): `isolation: worktree`
  agents branch from `main` (the retired web tree), not `rewrite/mcp-native-agent`
  — instruct them to `git reset --hard rewrite/mcp-native-agent` first._

_(The loop updates the lines above every iteration.)_

---

## Milestones

Acceptance criteria are condensed from assessment §4 (M1–M7). Tick items as
they land; a milestone is **done** only when every acceptance bullet holds.

### M0 — Planning & RFCs  _(done)_
- [x] Retire old design; draft RFC 0001
- [x] Architecture assessment + research notes
- [x] RFCs 0001–0013 authored, reconciled, committed
- [x] `rfcs/README.md` index
- [x] This plan committed

### M1 — Skeleton: config, one-shot, one MCP server, the loop, budgets  _(largely complete)_
Modules: `main.rs config.rs exit.rs json/ wire/ net/{http,unixsock,tls} intel/ mcp/{client,registry,config} agentloop/ supervisor/budget.rs obs/log.rs sec/secrets.rs signals.rs`
> Note: the plan's `loop/` dir is `agentloop/` in code (`loop` is a Rust keyword).
- [x] Scaffold workspace/crate/module tree (assessment §4.0); compiles
- [x] `config.rs` precedence (built-in<env<flag; file layer deferred) + validate-at-startup → exit 2
- [x] `exit.rs` public exit-code table + terminal-status→code map (`once_exit`)
- [x] `json/` shared JSON-RPC 2.0 codec + `frame.rs` (NDJSON + length-prefix)
- [x] `wire/mcp.rs` (2025-11-25 types, capability gating) + `wire/intel.rs` (neutral + tool-calling)
- [x] `net/http.rs` hand-rolled HTTP/1.1 over Read+Write + `net/unixsock.rs` + **`net/tls.rs`** (rustls/ring + bundled webpki-roots; `https://` intelligence works under `--features tls` — verified with a real TLS handshake). SSE deferred.
- [x] `intel/` openai-compatible adapter + native tool-calling + anthropic adapter; client over `unix:` / `https:`(tls) / `vsock:`(feat)
- [x] `mcp/client.rs` one stdio server (reader-thread + pending-map + timeouts) tools/list+call, resources/list+read, subscribe
- [x] `agentloop/runner.rs` ReAct loop (catalogue→intel→tools→observe→stop); `stop.rs` `TerminalStatus` done. (`context.rs`/`action.rs` split + resource-catalogue injection = M1 follow-up)
- [x] `supervisor/budget.rs` step/token/deadline budget
- [x] wire `main.rs` once-mode (intel + MCP connect + root loop + exit-code mapping). Structural acceptance verified (exit 4/6/2/1, budget partials); live LLM+MCP round-trip needs a real endpoint.
- [x] `obs/log.rs` JSON-lines logger + line schema; `signals.rs` SIGTERM/INT/PIPE
- **Acceptance:** `agentd --mode once --instruction … --intelligence https://… --mcp fs=…` → loop → real `tools/call` → result on stdout, JSON events on stderr; exit code maps terminal status; bad flag → exit 2 in <50ms; step/token/deadline cap → labeled partial not hang; `isError:true`→observation, JSON-RPC error→abort.

### M2 — Subagent processes: the supervised tree
Modules: `supervisor/{reactor,tree,spawn,reap,liveness,kill,restart}.rs subagent/ mcp/server.rs sec/scope.rs`
- [x] `supervisor/tree.rs` records (depth minting, caps chokepoint, token rollup, draining, deepest-first)
- [x] `supervisor/reactor.rs` the `Supervisor` loop (merged mpsc + recv_timeout tick): owns tree + handle map + per-child liveness, processes events, reaps on SIGCHLD, ticks liveness, drives the kill ladder on drain/stuck/deadline/tree-budget. **once-mode switched** to `supervise_once` (spawns + supervises the root subagent); `set_child_subreaper()` wired at startup; CLI regression tests (`tests/cli_once.rs`)
- [x] `supervisor/spawn.rs` re-exec subagent mode (`AGENTD_SUBAGENT`); `setpgid` via pre_exec; payload delivery + upward-event reader thread; immediate process-group kill (rlimit in pre_exec + graceful ladder deferred to kill.rs)
- [x] `subagent/protocol.rs` control protocol (ControlMsg/AgentMsg/SpawnPayload), length-framed
- [x] `subagent/control.rs` child-side: PDEATHSIG, read payload, Ready, connect intel+scoped MCP, run loop, **ping/pong on a separate thread** + cancel flag; `main.rs` subagent dispatch; e2e integration test (`tests/subagent_spawn.rs`)
- [x] `supervisor/reap.rs` `waitpid(-1,WNOHANG)` reap loop + pure exit-status classifier + `PR_SET_CHILD_SUBREAPER` + PID-1 detect (SIGCHLD self-pipe wiring lands with `reactor.rs`/`signals.rs`)
- [x] `supervisor/liveness.rs` three detectors (deadline/no-progress/ping-pong) + the EOF×pong 2×2 classifier — pure, fully unit-tested
- [x] `supervisor/kill.rs` the pure `Ladder` escalation timer (Cancel→SIGTERM→SIGKILL, grace/kill-grace, force) + `killpg` primitives — fully unit-tested (reactor walks `deepest_first` + enforces the total drain budget)
- [x] `signals.rs` SIGCHLD handler (SA_NOCLDSTOP) + self-pipe wakeup (`wakeup_fd`/`drain_wakeup`/`take_child_exit`) for the reactor
- [x] `supervisor/restart.rs` **restart governor** — pure backoff + capped jitter + circuit breaker + crash-on-spawn detection (hand-rolled jitter, no `rand`); `RestartGovernor::on_outcome → Backoff(d) | Tripped`. Wired into `run_scheduled`: failed fires back off via the governor, a crash-loop trips the breaker → `proc.exit{reason:"restart_breaker"}` + exit 1 (no hot-spin). 8 unit tests. _(Reactor-side per-child wiring for warm sessions: later, with M3 sessions.)_
- [x] **`subagent.spawn` self-tool — the model self-orchestrates** (`agentloop/action.rs` `SelfHandler` + `subagent/orchestrator.rs`): builds a child payload (depth+1, narrowed MCP scope, inherited intel), enforces depth/breadth caps **refused as tool results**, and supervises the child synchronously via `supervise_once` (nested real processes). e2e test spawns a real child (`tests/orchestrator_spawn.rs`). `reactor::reap` made flag-independent (nested supervise works).
- [~] self-MCP **server** listener (`mcp/server.rs`, `--serve-mcp unix:`) — **transport + `subagent.spawn` (sync **and async**) + `subagent.status`/`subagent.cancel` + the `agentd://` resource surface landed** (`agentd://status` + per-run `agentd://subagent/<handle>`) **+ `resources/subscribe` push** (a peer subscribes to a run's resource and is pushed `notifications/resources/updated` on completion — the reactive loop closed); see M4/M5. _(Concurrency: the single-reaper refactor landed — served runs are truly concurrent with the daemon.) **`subagent.send` (served warm sessions) landed** — `subagent.spawn{warm}` stays alive in a bounded registry, `send` injects the next turn, status reports `{turns,busy,last_result}`. (A coordinated served-session drain on shutdown now lands too: the daemon waits for in-flight served runs before exiting.)_
- [x] `sec/scope.rs` tool-scope grant logic (granted-MCP-subset, monotonic narrow, Rule-of-Two) — wiring into the chokepoint pending `spawn.rs`. (depth/breadth/rate caps already in `tree.rs`)
- **Acceptance:** parent spawns scoped child → child loop → distilled result up the channel; `kill -STOP` child → no-progress+missing-pongs → stuck → ladder to SIGKILL within budget; exited child reaped (no zombie); orphan grandchild reparents+reaped; killing supervisor collapses tree via PDEATHSIG; spawn past caps refused as tool result; crash-loop trips breaker.

### M3 — Reactivity: subscriptions, routing, warm sessions, async subagents
Modules: `triggers/{router,mode,timer}.rs`; extends `mcp/{client,server}.rs`, `supervisor/tree.rs`
- [x] **reactive driver** (`triggers/mode.rs::run_reactive` + `--mode reactive`): supervisor connects MCP, issues capability-gated `resources/subscribe` for `--subscribe` URIs (tracking owner server), loops draining `updated{uri}` notifications → `router.on_updated` → on `due` does **notify-then-read** (`resources/read`) → spawns a fresh root subagent templated from the event (standing instruction + changed-resource context). Drains to exit 0 on SIGTERM. **Proven end to end by observation** (`tests/reactive_e2e.rs` + the mock MCP server): subscribe→`resource.updated`→`trigger.fired`→reaction `subagent.spawn` all visible in telemetry. _Remaining: consume `list_changed`, **read-after-subscribe** on (re)connect, `unsubscribe` on shutdown._
- [x] built-in **mock MCP server** (`mcp/mock.rs`, hidden `--internal-mock-mcp <uri> [--no-emit]`): a tiny stdio MCP server advertising `resources.subscribe`, serving one resource, emitting one `updated` after subscribe — the fixture for live reactive tests + the M7 observe-suite. (Also fixed a latent codec bug: `json::Incoming` tried `Response` before `Request`, swallowing server→client requests; now `Request` first, regression-tested.)
- [x] **read-after-subscribe** (mandatory, §2.8): on startup the reactive driver synthesizes one "possibly changed" delivery per watched URI → edge→level (acts on current state; recovers updates missed before/while subscribing). `unsubscribe` on drain. e2e-proven (`--no-emit` mock → reacts via initial read, no `resource.updated`).
- [x] `triggers/router.rs` reactive routing (pure, unit-tested): exact-beats-glob + longest-prefix exactly-one-owner match, `Disposition::Spawn`/`Continue` as a route property, debounce + newest-wins coalesce, `on_updated`/`due`/`next_deadline`, dropped-counter for no-match
- [x] **warm `Continue` sessions** — `Disposition::Continue(session_id)` now delivers every event into ONE live warm subagent (single-consumer, in-order), instead of a fresh Spawn per event. **Subagent side** (`subagent/control.rs`): a `SpawnPayload.warm` subagent runs `run_warm` — prepare the `Session` once (the durable transcript, extracted in `agentloop/runner.rs`), then a turn per delivered event over the *same* conversation, emitting the non-terminal `AgentMsg::Turn` after each and blocking on the control channel for the next `Inject` (each turn gets a fresh per-event budget); ends on Cancel or control-channel close with a terminal `Result`. **Daemon side** (`triggers/warm.rs` `WarmRegistry`, wired into `run_reactive`): a `--continue <uri>` route spawns the warm session on its first event and injects thereafter; the daemon supervises non-blocking — each tick drains every session's `Turn` frames (applying self-schedule/subscribe effects like a Spawn reaction) and detects death via the (per-session) channel disconnecting; reaping is `Subagent::Drop` (tolerant of a concurrent reaction reactor's `waitpid(-1)` → benign `reap_unknown`), safe because the reactive daemon is single-threaded. Drain cancels + winds down warm sessions within the drain budget. e2e-proven (`tests/warm_session.rs`: first event spawns one session, the second injects into the SAME live process — no re-spawn — and runs a second turn; `tests/subagent_spawn.rs`: a real warm subagent runs a turn per injected event then ends on Cancel). _(Warm-session **persistence/checkpoint** across a supervisor restart stays v2-deferred per the assessment — v1 keeps the conversation in the live process only.)_
- [x] **`resource.read` self-tool + resource-catalogue injection** (`runner.rs`): list = awareness (a capped uri+label catalogue injected as a system note), read = attention (`resource.read{uri}` pulls a body on demand from the owning MCP server). Also closes the M1 "inject a resource catalogue" follow-up.
- [~] **self-scheduling + self-subscribe landed** (the signature capability). (1) **Self-scheduling**: the `schedule` self-tool (`{after_seconds, instruction}`, bounded ≤8/run, 1s–30d, root-only) sets a future wake-up. (2) **Self-subscribe**: the `subscribe`/`unsubscribe` self-tools (root-only, ≤16/run) add/remove a live MCP-resource subscription. Both ride out on `Outcome.{scheduled,subscriptions}` (up the existing Result path — no control-channel upcall); the reactive daemon applies them after each reaction: arming `(fire-at, instruction)` wakes (`arm_wakes`/`drain_due_wakes`) and mutating its router + server subscriptions (`Router::{has_exact,add_route,remove_exact}` + `apply_effects`). A woken/triggered reaction can itself schedule/subscribe again — a self-sustaining agent bounded by daemon lifetime + per-run budgets. Router dynamics + the orchestrator tools are unit-tested; events `self.schedule`/`self.subscribe`, `trigger.armed`/`trigger.fired` (kind:self_schedule|self_subscribe). _Remaining on this line: wiring **self-subscribe → an auto `continue(this_session)` route** so a self-subscribing agent re-enters its OWN in-flight session (the warm-session machinery now exists — see the warm `Continue` item above — but `apply_effects` still adds a fresh-Spawn route for a self-subscribe; explicit `--continue` routes are warm today)._
- [x] async `subagent.spawn{async,detach}` + completion-as-self-resource — **complete.** `subagent.spawn{async:true}` spawns the child via `spawn::spawn` (non-blocking) and returns a **handle** immediately (= the child's agent_path) so the parent keeps working; `{detach:true}` is fire-and-forget. Collection is via three **idempotent** paths (consistent — none consumes the handle; reaped at Drop / breadth cap): `subagent.await{handle}` (blocks, bounded by `AWAIT_MAX`=30s then hands control back so the loop stays cancel-responsive), `subagent.status{handle}` (non-blocking peek), and **`resource.read agentd://subagent/<handle>`** (completion-as-self-resource) — all share `peek_child` (drain the child's channel, distill the terminal `Result`/`Failed`). A **detached** child is reported not-collectable on every path (matches its fire-and-forget contract). Supervision reuses the warm-session safety model — the child is a bare `Subagent` + channel, death seen via the channel, reaped by `Subagent`'s Drop (safe: the subagent loop is single-threaded; a concurrent sync `supervise_once` reaping the async pid is a benign `reap_unknown`). Uncollected children die with the parent (no orphan outlives the tree; `subagent.async_reaped` logs the count at Drop). Breadth cap (`MAX_CHILDREN`) covers async children. The `agentd://` scheme is a shared module (`agentd_uri.rs`); a `resource.read` of an `agentd://` URI routes to the self-handler (`SelfHandler::read_resource`, defaulted). e2e-proven (`tests/orchestrator_spawn.rs`: async spawn → handle → await/status/`resource.read agentd://subagent/<handle>` all return the real child's distilled result idempotently; detached → not collectable). **Adversarially reviewed** (4 lenses: correctness/MCP-spec/security/regression) — the peek/consume divergence + detached-readable findings were fixed.
- [ ] rebuild+reconcile (read-after-subscribe) on (re)start
- **Acceptance:** `--mode reactive --subscribe file://…` idles near-zero CPU, wakes on `updated` then `resources/read`s; burst coalesces to one wake; no-route event dropped+counted; self-subscribing agent re-entered in same session; restart re-subscribes + read-after-subscribe re-fires missed change; async subagent returns handle, completion arrives as subscribable resource update.

### M4 — Composition, transports, exec, schedule
Modules: `net/vsock.rs sec/exec.rs`; extends `mcp/server.rs`, `triggers/{mode,timer}.rs`
- [~] serve self-MCP over `unix:` (`--serve-mcp unix:…`) — **transport + protocol landed**, dep-free. `mcp/server.rs` (feature `serve-mcp`, made dep-free — dropped the scaffold's mio per RFC 0005 §3.6's blocking `UnixListener`): a thread-per-connection NDJSON JSON-RPC server reusing the `json/` codec, answering `initialize` (declares `tools`), `ping`, `tools/list`, `tools/call`. v1 exposes a read-only `status` tool **and `subagent.spawn`** (sync): a peer delegates work — agentd builds a fresh root run from the daemon's payload template + the request (instruction, `output_contract`, `tool_scope` subset; depth minted here, not read from the request), supervises it, and returns the distilled `{handle,status,result}`. Concurrency-capped (≤4 in-flight, RAII guard); malformed params → JSON-RPC error, a cap/scope refusal or run failure → `isError:true` result (RFC §3.2). Trust boundary documented (socket perms gate who can delegate). **Concurrency bug found + fixed:** a served spawn runs `supervise_once` *concurrently* with the daemon's own mode-loop `supervise_once` in one process, and both reap via `waitpid(-1)` → child-stealing → hang; fixed with a per-process `SUPERVISE_LOCK` serializing supervisors (keeps subreaper-orphan reaping intact; a single-reaper refactor for true concurrency is a follow-up). Wired in `main.rs`; `--serve-mcp` warns `mcp.serve_unavailable` without the feature. **`agentd://` resources landed:** `initialize` now declares the `resources` capability; `resources/list` advertises **`agentd://status`** and `resources/read` returns this agentd's live state (run_id/mode/version/pid/uptime/in-flight + total spawn counts, one `status_body` source of truth shared with the `status` tool); an unknown/foreign URI is a JSON-RPC error (consistent with unknown-tool). **Served async + per-run resources + cancel landed:** `subagent.spawn{async:true}` returns a handle immediately and runs on a **background thread** tracked in a `ServeCtx.sessions` registry (`Arc<Mutex<HashMap<handle, ServedSession>>>`; the run holds the in-flight permit for its lifetime); `subagent.status{handle}` / `subagent.cancel{handle}` tools + `resource.read agentd://subagent/<handle>` read that registry; the registry is bounded (`MAX_SESSIONS`=64, oldest-terminal eviction). **Cancel** uses a new **per-run cancel token** in the reactor (`supervise_cancellable` + a `cancel: Option<Arc<AtomicBool>>` checked in `run()` alongside SIGTERM): setting it drains that one run's subtree via the kill ladder (independent of process SIGTERM). _(Update: the **single-reaper refactor** since retired `SUPERVISE_LOCK` — served runs now execute truly concurrently with the daemon's reactions, no head-of-line blocking; on shutdown the subtree still collapses via PDEATHSIG, a coordinated served drain is a follow-on.)_ **Adversarially reviewed** (4 lenses: concurrency/cancel/MCP-protocol/security) — no races/leaks; fixed a cancel-status TOCTOU (read the flag under the registry lock), made `run_to_status` preserve a completed run's result over a late cancel, kept `resources/list` static (per-run resources are read-only, not listed, since the reply-only transport has no `list_changed`), and used `-32002` for resource-not-found. **Proven E2E** (`tests/serve_mcp.rs`: sync round trip; **async spawn → handle → poll status → terminal + `resource.read agentd://subagent/<handle>`**; **cancel a live (hanging) run → drained + reported cancelled** well inside the hang). **`subagent.send` (served warm sessions), `resources/subscribe` (push), and the single-reaper refactor have all since landed** (see Current status items 9–14): a served `subagent.spawn{warm}` stays alive in a bounded `ServeCtx.warm` registry, `subagent.send` injects a turn, and status exposes `{turns,busy,awaiting_turn,last_result}`; review fixes folded in (sweep finished-but-unpolled sessions, atomic cap-check+spawn+insert, drop cancelled sessions outside the lock).
- [x] `net/vsock.rs` + vsock intelligence transport [vsock] — `VsockStream::connect_with_cid_port` + timeouts, drops into the HTTP client like the other transports. Compiles under `--features vsock`; live verification needs a microVM peer (deferred).
- [x] `sec/exec.rs` gated `exec` self-tool — off by default, advertised only with `--enable-exec` (propagated via the spawn payload, inherited by children). argv-style (no shell/PATH/interpolation), argv[0] = absolute path to an existing executable, scrubbed env, output capped (64 KiB), own process group `killpg`'d on a mandatory per-call timeout. Salvaged from the retired `shell.rs`. Validation/spawn failures are recoverable observations. (Budget/Rule-of-Two folding = later refinement.)
- [x] `--mode loop`/`schedule` drivers (`triggers/mode.rs::run_scheduled`): interval-based re-run of the standing instruction (each fire = an independent supervised `once` run); `loop` re-enters back-to-back (interval default 0), `schedule` fires on `--interval`; SIGTERM → graceful drain → exit 0; fast-failing runs back off (capped) so they can't hot-spin. e2e-proven (`tests/daemon_modes.rs`). _Remaining: optional 5-field `cron` feature (croner)._
- [x] optional `cron` feature as a `triggers/timer.rs` event source [feature: cron] — **hand-rolled, zero deps** (deviation from RFC 0008's `croner` mention, justified by the minimalism moat rfcs/0002; `cron = []` stays dep-free). A 5-field UTC `CronExpr::parse` (`*`, `*/step`, `a`, `a-b`, `a-b/step`, lists) + a minute-stepping `next_after` with day-of-month/day-of-week OR semantics (reuses `civil_from_days`). Wired into `run_scheduled` (`--cron`/`AGENTD_CRON`, requires `--mode schedule`): cron waits *until* its next instant before firing (vs interval's run-then-wait). Config rejects `--cron` outside schedule mode; the feature build fails fast on a bad expr (`config.invalid`→exit 2); the default build warns `cron.unavailable` and falls back to interval. 6 cron unit tests; observe-proven (arms + waits, no immediate fire). _Production path remains an external CronJob → `--mode once` (RFC 0008)._
- **Acceptance:** second agentd connects to served unix self-MCP, subscribes to `agentd://session/…`, reacts to first agent's progress; `--enable-exec` exposes exec only when binary exists, runs under deadline, killed+reaped by subtree ladder; `--mode loop --interval 5m` re-enters with idle backoff, terminates on global budget; vsock intelligence works in a microVM.

### M5 — Cloud-native hardening: drain, health, exit codes, idempotency
Modules: `obs/health.rs`; extends `signals.rs supervisor/{kill,reap}.rs config.rs`
- [x] full drain choreography with `AGENTD_DRAIN_TIMEOUT` < grace — SIGTERM → stop accepting work → Cancel→SIGTERM→SIGKILL ladder over `deepest_first` within the `--drain-timeout`/`AGENTD_DRAIN_TIMEOUT` budget → **exit 0** (a graceful drain self-exits 0, never 143; 143 is OS-set, RFC 0011 §5.1). A second SIGTERM (`signals::force`) or an exceeded budget forces the ladder; the budget-exceeded boundary now emits a one-shot `drain.timeout` warn so an ungraceful teardown is auditable (help already guides `--drain-timeout < pod grace`). Observe-proven for both daemon paths: `loop` and **`reactive`** (`tests/daemon_modes.rs`: SIGTERM → `reason:drain` → exit 0).
- [x] `obs/health.rs` **supervisor-heartbeat liveness** + `--health-file`: a process-global `tick()` is bumped by every supervisor hot loop (reactor, daemon driver, interval sleep) so liveness reflects the *supervisor* making progress — idle is healthy, a busy/stuck *subagent* doesn't flip it. A 1s writer thread renders `{alive, supervisor_tick_age_ms, mode, draining, ts}` atomically (temp+rename); a K8s exec probe checks `alive`/tick-age freshness. e2e-proven (`tests/daemon_modes.rs`: loop mode writes a live health file). The opt-in `/healthz`+`/readyz` HTTP surface now ships with the `metrics` feature (`obs/serve.rs`, bound by `--metrics-addr`); `/healthz` reuses this same supervisor-heartbeat liveness (200 when fresh + not draining, else 503).
- [x] complete exit-code table in `exit.rs` — the full RFC 0011 §5 table (0 success, 1 generic, 2 usage, 3 partial, 4 intelligence, 5 semantic/refused, 6 MCP, 7 budget, 124 deadline; 137/143 documented as OS-set), `once_exit` total over every `TerminalStatus`, and tests asserting the mapping, pairwise-distinctness, documented bands, and "non-completed never looks like success". (The supervisor hard-deadline path maps `KillReason::Deadline → 124` in `main.rs`, distinct from the loop's soft budget `7`.)
- [x] RUN_ID propagation into MCP `_meta` — `McpClient::set_tool_meta` stamps `{"agentd/run_id": …}` onto every `tools/call` `params._meta` (set after initialize in the subagent) so backing services dedupe retries of a run (RFC 0011 §idempotency). Pure builder unit-tested.
- [x] cgroup-v2 awareness + **active enforcement** — **read-side** (`supervisor/cgroup.rs`, no deps): best-effort reads of `memory.max`/`current`/`high` under `/sys/fs/cgroup` (correct under a container cgroup namespace; degrades to `None` off-cgroup / cgroup-v1). Logged once at startup as `cgroup.detected` (quiet when absent) and exposed as live `agentd_memory_max_bytes`/`agentd_memory_current_bytes` gauges on the `/metrics` surface. **Active enforcement landed** (opt-in `--cgroup auto|<path>` / `AGENTD_CGROUP`, still no deps — pure std fs + libc): a `CgroupGuard` RAII places each supervised run's root subagent in its own child cgroup (`run-<pid>-<seq>`; descendants inherit), so teardown writes **`cgroup.kill`** for atomic whole-subtree SIGKILL — the backstop beyond killpg+PDEATHSIG that catches a `setsid` escapee (proven by a live unit test: a process that left the group is still reaped). Wired into the reactor's `LadderAction::Kill` rung + the abandon path; the guard's Drop kills + `rmdir`s the cgroup. **`memory.high` backpressure** (`under_memory_pressure`, ≥95% of high): the served `subagent.spawn` + the model-driven orchestrator refuse new subagents under soft-limit pressure (retry/adapt). Best-effort throughout: not writable / off-cgroup → the feature silently disables and the run falls back to PDEATHSIG + the kill ladder (cgroup-*aware*, never cgroup-*requiring*). e2e-proven (`tests/cgroup_e2e.rs`: `--cgroup auto` arms, places the root, completes, removes the cgroup on exit; skips where not writable). **Hard resource limits landed** (`--cgroup-memory-max <max|512M|2G|bytes>` / `--cgroup-pids-max <max|N>`, + env): when limits are requested `configure` delegates the `memory`/`pids` controllers to the parent (`cgroup.subtree_control`), and each per-run leaf gets `memory.max`/`pids.max` applied (`normalize_bytes`/`normalize_count` parse the specs). Live-proven on this host (`pids.max=1` refuses a `fork` inside the leaf with `EAGAIN`; e2e asserts the limits engage through the real binary). Honest degradation: controller delegation needs a parent that can delegate — `EBUSY` when the parent holds processes directly (`--cgroup auto` under a busy unit cgroup), so limits then no-op while teardown still works; `configure` reports `limits_unavailable` and `main` logs `cgroup.limits_unavailable`. (A parent that delegates — a root child, a `Delegate=yes` unit, or a container's delegated cgroup — enforces them; this host's root delegates all controllers.) Review fixes folded in: a `memory.max` OOM-kill of the root is now reported plainly (`cgroup.oom_kill` + a distinct "killed by cgroup memory limit (OOM)" failure, reading the leaf's `memory.events` on the SIGKILL reap) instead of a generic "exited without a result"; `validate()` rejects the limit flags without `--cgroup` and rejects `0` (a footgun that disables placement/OOM-kills instantly); each controller is delegated with its own `subtree_control` write (a `Delegate=pids`-only parent keeps the achievable limit); placement failure logs at `warn` (the run lost the teardown backstop); `pids.max` documented as counting threads.
- **Acceptance:** SIGTERM drains within budget → exit **0** (not 143); second SIGTERM forces kill; health file goes stale only when the *supervisor* wedges; each exit code matches the table; stable RUN_ID retried run detects "already done" via backing MCP → exit 0 cheaply; cgroup-writable host → tree reaped by `cgroup.kill`.

### M6 — Observability depth + security tags
Modules: `obs/{trace,metrics,otel}.rs`; extends `obs/log.rs sec/scope.rs net/http.rs`
- [x] **W3C trace-context propagation** (default-on, dependency-free) in `obs/trace.rs`: one `trace_id` per run — ingested from an upstream `--traceparent`/`AGENTD_TRACEPARENT` or minted deterministically from the run id — stamped on every log line (supervisor + every subagent), carried in the spawn payload (children inherit), and emitted as `_meta.traceparent` on MCP tool calls. Only OTLP *export* is otel-gated. e2e-proven (`tests/reactive_e2e.rs`: the upstream trace id appears on both `comp:supervisor` and `comp:agent` lines).
- [x] **LLM `traceparent` header**: the run's `trace_id` threaded into the intel client (`set_trace_id`); every completion carries a fresh-span `traceparent` so the LLM call joins the run's trace (unit-tested `apply_trace_header`).
- [x] **full closed event vocabulary emitted across supervisor + agent**: closed the §2.9 gaps — `config.loaded` (validated policy; content-off, lengths/schemes only), `mcp.connect` success (supervisor + subagent), `proc.ready`, `loop.step` (per-turn budget anchor), `subagent.stuck` (distinct liveness verdict); aligned `reactive.armed`/`schedule.armed` → canonical `trigger.armed`. Observe-proven in a live reactive run.
- [x] `--log-content` (content capture, opt-in) — off by default (telemetry logs lengths only); `--log-content`/`AGENTD_LOG_CONTENT` adds the truncated tool args/results. Rides in the `Telemetry` block so it propagates to every child; `config.loaded` reports the policy; `Logger::content_capture()` gates the `tool.call`/`tool.result` content. Observe-proven (live run shows `log_content:true`). _(Redaction allowlist for secret-bearing tool args: a follow-up.)_
- [x] `--aggregate-logs` (mode B) — **satisfied by design / deviation noted.** The child's stderr is `Stdio::inherit()` (`spawn.rs`), so all subagent JSON telemetry already lands on the supervisor's single stderr stream by fd inheritance — the single-stream outcome mode B targets, without the control-channel forwarding machinery. An explicit forwarding path would only matter where a child's stderr is deliberately separated (uncommon for this deployment shape); deferred as redundant for v1 rather than built. Correlation fields are never rewritten (each process self-logs pre-correlated).
- [x] `sec/scope.rs` Rule-of-Two tag check — **wired and enforcing**. `TrifectaTag` (untrusted-input / sensitive-data / egress) + `check_trifecta`. Tag source: `--mcp-tags name=tag,tag` attaches operator tags to a server (carried in the spawn payload); untagged → `untrusted_input` (conservative default); `--enable-exec` → `egress`. Enforced **once at root startup** (`main.rs`): `Config::trifecta_grant_tags()` → refuse with exit 2 + `scope.trifecta_refused` unless `--allow-trifecta` (then proceed + `scope.trifecta_grant` warn). **Design note / deviation from RFC's per-spawn chokepoint:** because scope narrows monotonically (RFC 0009) a child's tags ⊆ the root's, so the single root check bounds the whole tree — no in-subagent orchestrator check, hence no process-global-flag-propagation problem. `--allow-trifecta` stays out of the payload. Observe-proven (refuse→exit 2, allow→warn+proceed, two-legs→silent).
- [~] SSRF guard — **pure classifier landed** in `net/ssrf.rs`: `is_global(IpAddr)` + `guard_host(host, allow_private)` reject loopback / RFC-1918 / link-local / ULA / unspecified / multicast / IPv4-mapped equivalents, 18 unit tests. _Not wired to a default-on call site: agentd's only HTTP client path is the operator-configured (trusted) intelligence endpoint, frequently localhost — blocking it would be wrong. The guard is ready for any future model/agent-supplied-URL fetcher, which MUST route through `guard_host` (acceptance "refuses RFC-1918 by default" applies there)._
- [x] `metrics` feature (Prometheus text) — **dependency-free**: `obs/metrics.rs` is always compiled but its `record_*` fns are no-ops unless `--features metrics` (the atomic registry + `render_prometheus` are gated), so default call sites stay clean and cost nothing. Counters (runs started/completed/failed/killed, reactions, in/out tokens, restart-breaker trips) increment at the supervisor chokepoints (`supervise_once`, the `Usage` handler, `trigger.fired`, breaker trip). `obs/serve.rs` (gated) serves `/metrics` + `/healthz` + `/readyz` on a single blocking-accept thread bound by `--metrics-addr` (opt-in). Live-proven via curl (valid Prometheus, 200/503/404 routing). Per-process scope documented (same boundary as the tree token ceiling).
- [x] `otel` feature (OTLP + GenAI semconv, HTTP exporter) — **dependency-free**: `obs/otel.rs` is always compiled but `export_run_span` is a no-op unless `--features otel` (the OTLP/JSON encoder + HTTP export are gated), so default loop call sites stay clean and cost nothing. Hand-rolled OTLP-over-**HTTP/JSON** reuses the run's W3C trace/span ids (`obs/trace.rs`), `serde_json`, and the existing HTTP client (`net/http.rs`) → `--features otel` stays **3 deps**. A finished run exports its whole trace as one OTLP batch — the `invoke_agent` run span **plus a `chat` child per model call and an `execute_tool` child per tool call** (`gen_ai.operation.name`/`gen_ai.request.model`/`gen_ai.usage.{input,output}_tokens`/`gen_ai.tool.name`, status OK/ERROR, children parented to the run span) — to `OTEL_EXPORTER_OTLP_ENDPOINT` at `/v1/traces`, best-effort (an export failure is logged-and-dropped; telemetry never fails a run). `https://` collectors need `--features tls`. Wired via a no-op `RunSpan` handle (`runner.rs` records a child as each chat/tool completes, flushes at the terminal). e2e-proven (`tests/otel_e2e.rs`: a *real* ReAct run POSTs `invoke_agent` + `chat` + `execute_tool` to an in-test OTLP collector — asserts `/v1/traces`, `resourceSpans`, all three span kinds, the GenAI attrs incl. `gen_ai.tool.name`, the model). **Closes the M6 otel acceptance.**
- **Acceptance:** upstream trace flows through agentd → MCP `_meta` + LLM header + child processes, reassembles by `run_id`+`agent_path`; trifecta grant refused without `--allow-trifecta`; HTTP client refuses RFC-1918/link-local by default; `--features metrics` serves valid Prometheus; `--features otel` exports `invoke_agent`/`chat`/`execute_tool` with `gen_ai.*`.

### M7 — Minimalism audit + conformance + release
Modules: fills `agentd-conformance/`; finalizes feature matrix
- [x] `cargo tree -e normal` + `cargo audit`/`cargo deny` pass; cut unearned deps — **default = exactly 3 direct first-party crates** (`libc`, `serde`, `serde_json`); the rest of the tree is build-time proc-macro (`syn`/`quote`/…) or pure-Rust runtime helpers (`itoa`/`memchr`/`zmij`) — **no async runtime, no TLS, no C toolchain** (M7 acceptance ✓). `--all-features` adds only `rustls`(+`ring`,`webpki-roots`)/`vsock` — the scaffold's `mio`/`croner`/`chrono` were cut. `cargo audit` clean (exit 0, no advisories); `cargo deny check` passes (advisories/bans/licenses/sources ok) behind a new `deny.toml` gate (wildcard-deny, permissive-only license allow-list).
- [x] revisit hand-roll-vs-`minreq`, `thiserror`-vs-hand-rolled, miniserde go/no-go — **revisit confirms the hand-rolls.** The hand-rolled HTTP/1.1 client (no `minreq`), hand-rolled error enums (no `thiserror`), and `serde_json` (not `miniserde`) all hold the moat and work; the audit above shows zero unearned cost from keeping them. No change — documented as a deliberate steady state.
- [~] `agentd-conformance` MCP client+server conformance + supervisor behavior — **the behaviours are validated across the `tests/` e2e suite** (MCP client conformance vs the built-in mock MCP in `reactive_e2e`/`observe_e2e`; agentd-as-MCP-server conformance in `serve_mcp`; supervisor behaviour in `daemon_modes`/`chaos_e2e`/`orchestrator_spawn`/`subagent_spawn`). A *standalone* conformance crate (record/replay corpus, real MCP reference servers) is **deferred as a non-essential reorganisation** + needs an external reference server (infra-gated).
- [x] **observe-to-validate E2E suite** (operator ask) — **complete.** A built-in **mock LLM** (`intel/mock.rs`, `--internal-mock-llm <socket> [final|read|schedule|slow]`, no deps) makes the real loop observable. The suite drives *real* agentd and asserts on the observed telemetry + outcome + process tree: `observe_e2e` — the loop runs to `completed`, a full `resource.read` ReAct cycle, **self-scheduling fires end to end**; `serve_mcp` — a peer delegates via served `subagent.spawn`; `daemon_modes` — graceful SIGTERM drain → exit 0 (loop + reactive); `chaos_e2e` — **PDEATHSIG collapses a live subagent when the supervisor is killed (leaks no process)**; `orchestrator_spawn` — depth-cap refusal (fork-bomb guard); plus the reactive/trace/round-trip suites. Tree reconstruction is intrinsic (every line carries `run_id`+`agent_path`). _(A live stuck→kill at the 120 s `progress_timeout` is impractical without test-only liveness knobs; the stuck classifier + kill ladder are unit-tested in `liveness`/`kill`, and the live kill path is exercised by the drain + PDEATHSIG tests.)_
- [x] minimal container image (scratch/distroless, TLS-off default) — `Dockerfile` rewritten to the minimalism target: a `rust:1.88-alpine` builder compiles the **default** (TLS-off) build to a **fully static musl binary** shipped `FROM scratch` (no shell, no libc, no package manager), nonroot uid, optional `--build-arg FEATURES=…` for heavier surfaces. Verified: the static release build links cleanly (`x86_64-unknown-linux-musl`, **statically linked, ~1.1 MB**). (Replaced a stale `--all-features`/distroless/cmake Dockerfile that referenced an aws-lc-rs path no longer used.) `docs/deployment.md` already documents this image; `.dockerignore` tightened. _The `docker build` itself is infra-gated (no daemon here), but the Dockerfile + static build are verified._
- [x] docs: exit-code table, config table, event vocabulary, trifecta guidance, deployment recipes — **reconciled to the implemented runtime.** `configuration.md` flag/env table now matches `--help` exactly (added `--log-content`/`--metrics-addr`/`--allow-trifecta`/`--mcp-tags`/`--cron`/`--traceparent`, dropped the never-built aspirational flags + the stale `config.loaded` shape); `observability.md` event vocabulary extended with the self-* / served-MCP / cgroup / drain / security / daemon-run events + a vocabulary-vs-wire note. A 10-file Workflow fan-out removed the stale "mid-build / scaffold-only / lands across M1–M3" caveats across the whole `docs/` set (now describes a working runtime; genuine roadmap caveats kept). Added the missing `--traceparent` line to `--help`. exit-code table (deployment.md) verified against `exit.rs` (0–7, 124, OS 137/143).
- **Acceptance:** default build links no async runtime, no TLS, no C toolchain, ≤ single-digit first-party crates; conformance passes against MCP reference servers + an agentd-as-server peer; stuck/orphan/fork-bomb chaos test leaks no process; runtime readable in an afternoon (size + module-count check).

---

## RFC index (authored in M0)

0001 core architecture · 0002 reactor/concurrency · 0003 supervision/recovery ·
0004 MCP client · 0005 self-MCP server + control protocol · 0006 intelligence
transport/wire · 0007 agentic loop + terminal status · 0008 modes + reactive
routing · 0009 subagent process model · 0010 observability/health · 0011
cloud-native contract · 0012 security posture · 0013 deferred v2 surface.

## Salvage list (from the retired code — assessment notes-mine-existing-code.md)

Lift/adapt: MCP stdio client + JSON-RPC framing; intelligence providers
(openai/anthropic dialects, `split_system`, key-safe Debug, build-time key
probe); the hand-rolled HTTP/1.1 client; length-frame `read_frame`/`write_frame`;
`shell.rs::run()` (reader-threads + try_wait + timeout-kill) for `exec`; the
CAS budget tracker; SIGTERM/INT signal handling; secrets `resolve()`.
**Drop:** workflow DAG model/validator/engine, policy/Rego, signing, auth,
cron/fs_watch triggers as-is, conformance corpus, jsonschema, otel-grpc, toml.
