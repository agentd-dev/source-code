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

- **Phase:** M3 (reactivity) underway — the reactive driver is wired end to end
  (router + MCP subscriptions + notify-then-read + spawn). A live update→react
  test needs a mock MCP server (next, also reused by the M7 observe-suite).
- **Last completed:** `triggers/mode.rs::run_reactive` + `--mode reactive` — the
  supervisor subscribes to the configured MCP resources and, on each
  `updated{uri}`, reads the current state and spawns a fresh root subagent
  templated from the event (standing instruction + changed-resource context),
  debounced via the `Router`; drains to exit 0 on SIGTERM. Wired in `main.rs`;
  `reactive_payload` templating is pure + unit-tested; CLI regression tests
  (reactive validation → exit 2, required-MCP-down → exit 6). (Prior:
  `triggers/router.rs` routing core.) 100 unit + 4 integration tests, clippy
  clean, default build still serde + libc only.
- **Next action:** build a **minimal mock MCP server** (test fixture: speaks
  stdio JSON-RPC — initialize w/ resources.subscribe, resources/list+read,
  subscribe, then emits `notifications/resources/updated` after a delay) so the
  live reactive path is testable by *observation* (assert agentd logs
  `subscribe`→`resource.updated`→`trigger.fired`→`subagent.spawn`). Reused by the
  M7 observe-suite. THEN finish M3: `read-after-subscribe` on connect (synthesize
  one "possibly changed" per URI — recovers missed updates), `list_changed`
  consumption, the `subscribe`/`unsubscribe`/`resource.read` **self-tools**
  (self-scheduling), warm `Continue` sessions, async subagents; then `loop`/
  `schedule` modes (M4). (M2 tail still open: `restart.rs`, `--serve-mcp`
  listener, live stuck/orphan tests.)
- **Active milestone:** M3 (reactivity).
- **Blockers:** none. (`net/tls.rs`/otel deferred. PDEATHSIG/setpgid/killpg/
  prctl/waitpid are Linux/Unix; agentd targets Linux for production.)

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
- [x] `net/http.rs` hand-rolled HTTP/1.1 over Read+Write + `net/unixsock.rs` (SSE + `net/tls.rs` deferred until https path/M6)
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
- [ ] `supervisor/restart.rs` backoff+jitter+breaker+crash-on-spawn
- [x] **`subagent.spawn` self-tool — the model self-orchestrates** (`agentloop/action.rs` `SelfHandler` + `subagent/orchestrator.rs`): builds a child payload (depth+1, narrowed MCP scope, inherited intel), enforces depth/breadth caps **refused as tool results**, and supervises the child synchronously via `supervise_once` (nested real processes). e2e test spawns a real child (`tests/orchestrator_spawn.rs`). `reactor::reap` made flag-independent (nested supervise works).
- [ ] self-MCP **server** listener (`mcp/server.rs`, `--serve-mcp unix:`) for peer composition + `subagent.send/cancel/status` (async) — deferred toward M3/M4
- [x] `sec/scope.rs` tool-scope grant logic (granted-MCP-subset, monotonic narrow, Rule-of-Two) — wiring into the chokepoint pending `spawn.rs`. (depth/breadth/rate caps already in `tree.rs`)
- **Acceptance:** parent spawns scoped child → child loop → distilled result up the channel; `kill -STOP` child → no-progress+missing-pongs → stuck → ladder to SIGKILL within budget; exited child reaped (no zombie); orphan grandchild reparents+reaped; killing supervisor collapses tree via PDEATHSIG; spawn past caps refused as tool result; crash-loop trips breaker.

### M3 — Reactivity: subscriptions, routing, warm sessions, async subagents
Modules: `triggers/{router,mode,timer}.rs`; extends `mcp/{client,server}.rs`, `supervisor/tree.rs`
- [x] **reactive driver** (`triggers/mode.rs::run_reactive` + `--mode reactive`): supervisor connects MCP, issues capability-gated `resources/subscribe` for `--subscribe` URIs (tracking owner server), loops draining `updated{uri}` notifications → `router.on_updated` → on `due` does **notify-then-read** (`resources/read`) → spawns a fresh root subagent templated from the event (standing instruction + changed-resource context). Drains to exit 0 on SIGTERM. CLI tests (validation→2, required-MCP-down→6). _Remaining: consume `list_changed`, **read-after-subscribe** on (re)connect, `unsubscribe` on shutdown._
- [x] `triggers/router.rs` reactive routing (pure, unit-tested): exact-beats-glob + longest-prefix exactly-one-owner match, `Disposition::Spawn`/`Continue` as a route property, debounce + newest-wins coalesce, `on_updated`/`due`/`next_deadline`, dropped-counter for no-match
- [ ] warm-session state in `tree.rs`
- [ ] `subscribe`/`unsubscribe` + `resource.read` self-tools; self-subscribe → auto continue-route (self-scheduling)
- [ ] async `subagent.spawn{async,detach}` + completion-as-self-resource
- [ ] rebuild+reconcile (read-after-subscribe) on (re)start
- **Acceptance:** `--mode reactive --subscribe file://…` idles near-zero CPU, wakes on `updated` then `resources/read`s; burst coalesces to one wake; no-route event dropped+counted; self-subscribing agent re-entered in same session; restart re-subscribes + read-after-subscribe re-fires missed change; async subagent returns handle, completion arrives as subscribable resource update.

### M4 — Composition, transports, exec, schedule
Modules: `net/vsock.rs sec/exec.rs`; extends `mcp/server.rs`, `triggers/{mode,timer}.rs`
- [ ] serve self-MCP over `unix:` (`--serve-mcp unix:…`)
- [ ] `net/vsock.rs` + vsock intelligence transport [vsock]
- [ ] `sec/exec.rs` gated `exec` self-tool folded into kill ladder + budgets + caps
- [ ] `triggers/timer.rs` internal `--interval` + optional `cron` feature (croner) as router event sources
- [ ] `--mode loop`/`schedule` drivers
- **Acceptance:** second agentd connects to served unix self-MCP, subscribes to `agentd://session/…`, reacts to first agent's progress; `--enable-exec` exposes exec only when binary exists, runs under deadline, killed+reaped by subtree ladder; `--mode loop --interval 5m` re-enters with idle backoff, terminates on global budget; vsock intelligence works in a microVM.

### M5 — Cloud-native hardening: drain, health, exit codes, idempotency
Modules: `obs/health.rs`; extends `signals.rs supervisor/{kill,reap}.rs config.rs`
- [ ] full drain choreography with `AGENTD_DRAIN_TIMEOUT` < grace
- [ ] `obs/health.rs` supervisor heartbeat + `--health-file`; opt-in `/healthz`+`/readyz`
- [ ] complete exit-code table in `exit.rs`
- [ ] RUN_ID propagation into MCP `_meta`
- [ ] cgroup-v2 awareness (read `memory.max`, optional child-cgroup + `cgroup.kill`, `memory.high` backpressure, never required)
- **Acceptance:** SIGTERM drains within budget → exit **0** (not 143); second SIGTERM forces kill; health file goes stale only when the *supervisor* wedges; each exit code matches the table; stable RUN_ID retried run detects "already done" via backing MCP → exit 0 cheaply; cgroup-writable host → tree reaped by `cgroup.kill`.

### M6 — Observability depth + security tags
Modules: `obs/{trace,metrics}.rs`; extends `obs/log.rs sec/scope.rs net/http.rs`
- [ ] W3C context propagation by default (`_meta`/HTTP header/spawn telemetry) in `obs/trace.rs`
- [ ] full closed event vocabulary emitted across supervisor + agent
- [ ] `--aggregate-logs` (mode B) + `--log-content` (redaction-aware)
- [ ] `sec/scope.rs` Rule-of-Two tag check (warn/refuse trifecta grants)
- [ ] SSRF guards in `net/http.rs`
- [ ] `metrics` feature (Prometheus text); `otel` feature (OTLP + GenAI semconv, HTTP exporter)
- **Acceptance:** upstream trace flows through agentd → MCP `_meta` + LLM header + child processes, reassembles by `run_id`+`agent_path`; trifecta grant refused without `--allow-trifecta`; HTTP client refuses RFC-1918/link-local by default; `--features metrics` serves valid Prometheus; `--features otel` exports `invoke_agent`/`chat`/`execute_tool` with `gen_ai.*`.

### M7 — Minimalism audit + conformance + release
Modules: fills `agentd-conformance/`; finalizes feature matrix
- [ ] `cargo tree -e normal` + `cargo audit`/`cargo deny` pass; cut unearned deps
- [ ] revisit hand-roll-vs-`minreq`, `thiserror`-vs-hand-rolled, miniserde go/no-go
- [ ] `agentd-conformance` MCP client+server conformance + supervisor behavior + record/replay tests
- [ ] **observe-to-validate E2E suite** (operator ask): drive real agent/subagent runs and assert on the *observed* JSON-lines telemetry stream + outcomes — reconstruct the agent tree by `run_id`+`agent_path`, verify each capability/assumption (delegation, caps refusal, stuck-kill, drain, reactivity, scope narrowing) is visible and auditable in the event log
- [ ] minimal container image (scratch/distroless, TLS-off default)
- [ ] docs: exit-code table, config table, event vocabulary, trifecta guidance, deployment recipes (CLI / reactive Deployment / external CronJob)
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
