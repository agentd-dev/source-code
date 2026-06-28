// SPDX-License-Identifier: Apache-2.0
//! The supervisor reactor — the single loop that owns the process tree.
//! RFC 0002 §reactor, RFC 0003 §supervision.
//!
//! One thread owns the `Tree`, the `NodeId → Subagent` handle map, and a
//! per-child `Liveness`. It blocks on the merged event channel (every
//! subagent's reader thread forwards `(NodeId, AgentMsg)` here) with a short
//! `recv_timeout` tick that doubles as the timer; each tick it drains the
//! process-global reaper (`reaper::reap_and_dispatch`, which routes each reaped
//! pid to its owning supervisor), classifies liveness, and — on a drain signal
//! or a stuck/deadline/budget verdict — drives the bounded `kill::Ladder` over
//! `tree.deepest_first()`.
//!
//! This reactor supervises a single **root** subagent to completion
//! (once-mode). Nested children are spawned by the running subagent via the
//! self-MCP `subagent.spawn` tool (`subagent/orchestrator.rs`), each supervised
//! by its own recursively-spawned `Supervisor`; this loop's deepest-first
//! teardown over a multi-node `Tree` is exercised when a tree is built locally.
//!
//! The signal self-pipe (`signals::wakeup_fd`) is built for sub-tick
//! promptness; the short tick already bounds signal/deadline latency, so the
//! reactor polls the flags each tick and just drains the pipe for hygiene.

use crate::agentloop::stop::Outcome;
use crate::obs::log::Logger;
use crate::signals;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::cgroup::CgroupGuard;
use crate::supervisor::kill::{Ladder, LadderAction, kill_group, term_group};
use crate::supervisor::liveness::{Health, Liveness, LivenessConfig};
use crate::supervisor::reap::{Reaped, WaitOutcome};
use crate::supervisor::reaper;
use crate::supervisor::spawn::{Subagent, spawn};
use crate::supervisor::swap::SwapChannel;
use crate::supervisor::tree::{Caps, NodeId, NodeStatus, Tree};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Reactor tick. Bounds signal/deadline detection latency without a poll loop.
const TICK: Duration = Duration::from_millis(200);
/// How long to wait for a *completed* root's stragglers to exit before draining.
const FINISH_GRACE: Duration = Duration::from_secs(5);
/// Grace past the drain deadline after which a still-unreaped subtree is
/// *abandoned* (its handles' `Drop` is suppressed and the teardown returns), so
/// `drive_drain` can never spin forever on a reap that doesn't arrive.
const ABANDON_GRACE: Duration = Duration::from_secs(3);
/// After the root process is reaped *without* having reported a terminal on the
/// events channel, wait this long for the channel to flush before concluding it
/// "exited without a result". The reap (`waitpid`) and the final `Result`/`Failed`
/// frame travel on independent channels, so the reap can win the race even though
/// the child wrote its result to stdout just before exiting; without this grace a
/// real reason (e.g. an intel-unavailable `Failed` → exit 4) would be masked as a
/// generic exit 1. Only affects the rare genuinely-no-result path (e.g. a SIGKILL
/// or segfault), which then takes this much longer to conclude.
const RESULT_GRACE: Duration = Duration::from_millis(500);

/// Why a subtree was torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    Drain,
    Deadline,
    Stuck,
    TreeBudget,
}

/// The outcome of supervising a run.
#[derive(Debug)]
pub enum SuperviseResult {
    /// The root reached a terminal status and returned its distilled result.
    Completed(Outcome),
    /// The root hit a fatal infrastructure failure (intel/mcp unreachable).
    Failed(String),
    /// The root (and its subtree) was torn down by the supervisor.
    Killed(KillReason),
}

struct Drain {
    reason: KillReason,
    ladder: Ladder,
    deadline: Instant,
    /// Latched once the drain budget is exceeded, so `drain.timeout` is
    /// announced exactly once (the forced-teardown boundary).
    forced: bool,
}

pub struct Supervisor {
    exe: PathBuf,
    tree: Tree,
    live: HashMap<NodeId, Subagent>,
    liveness: HashMap<NodeId, Liveness>,
    pid_to_node: HashMap<i32, NodeId>,
    events_tx: Sender<(NodeId, AgentMsg)>,
    events_rx: Receiver<(NodeId, AgentMsg)>,
    /// Child-exit events for THIS supervisor's pids, dispatched by the global
    /// reaper ([`crate::supervisor::reaper`]) — replaces a private `waitpid(-1)`,
    /// so supervisors run concurrently without stealing each other's children.
    reap_tx: Sender<Reaped>,
    reap_rx: Receiver<Reaped>,
    root: NodeId,
    root_terminal: Option<SuperviseResult>,
    /// When the root was reaped without a terminal — starts the `RESULT_GRACE`
    /// window for the events channel to deliver its trailing `Result`/`Failed`.
    root_exited_at: Option<Instant>,
    finishing_since: Option<Instant>,
    drain: Option<Drain>,
    drain_timeout: Duration,
    liveness_cfg: LivenessConfig,
    last_ping: Instant,
    ping_seq: u64,
    /// Optional per-run cancel flag (a served async run can be cancelled by
    /// handle). Set → the run drains gracefully, like a SIGTERM but scoped to
    /// this one supervisor. `None` for runs that only honour process SIGTERM.
    cancel: Option<Arc<AtomicBool>>,
    /// Optional per-run pause flag (a served async run can be paused by the
    /// instance-wide `pause` operator tool, RFC 0015 §4.3). Set → the reactor
    /// forwards `ctrl/pause` to every live child so each suspends at its next
    /// turn boundary; cleared → it forwards `ctrl/resume`. `None` for runs with
    /// no pause channel. This is the async-session parallel of `cancel`: a shared
    /// atomic the reactor reads and translates to a `ControlMsg`, exactly as
    /// `cancel` becomes `ControlMsg::Cancel`. `paused_sent` debounces the
    /// forward so an edge fans exactly once, not every tick.
    paused: Option<Arc<AtomicBool>>,
    paused_sent: bool,
    /// Optional per-run intelligence hot-swap channel (RFC 0018 §5.2). A hot
    /// reload that touches `intelligence`/`model` publishes the new config here;
    /// the reactor reads it each tick and fans `ctrl/swap_intel` to every live
    /// child once per published swap — the async-run parallel of `paused`. `None`
    /// for runs with no swap channel. `swap_sent_gen` debounces the fan so each
    /// distinct published swap is forwarded exactly once (the parallel of
    /// `paused_sent`).
    swap: Option<SwapChannel>,
    swap_sent_gen: u64,
    /// The per-run child cgroup (opt-in via `--cgroup`; `None` when off / not
    /// writable). The root subagent is placed here so its whole subtree can be
    /// torn down atomically with `cgroup.kill` — the backstop beyond killpg +
    /// PDEATHSIG. RAII: dropping the supervisor kills + removes the cgroup.
    cgroup: Option<CgroupGuard>,
    log: Logger,
}

impl Supervisor {
    fn new(exe: PathBuf, drain_timeout: Duration, log: Logger) -> Supervisor {
        let (events_tx, events_rx) = mpsc::channel();
        let (reap_tx, reap_rx) = mpsc::channel();
        Supervisor {
            exe,
            tree: Tree::new(Caps::default()),
            live: HashMap::new(),
            liveness: HashMap::new(),
            pid_to_node: HashMap::new(),
            events_tx,
            events_rx,
            reap_tx,
            reap_rx,
            root: NodeId(0),
            root_terminal: None,
            root_exited_at: None,
            finishing_since: None,
            drain: None,
            drain_timeout,
            // Liveness timeouts are tunable via env (a niche knob); the ping
            // cadence is derived so Detector C (ping/pong) is actually active.
            liveness_cfg: LivenessConfig::from_env(),
            last_ping: Instant::now(),
            ping_seq: 0,
            cancel: None,
            paused: None,
            paused_sent: false,
            swap: None,
            swap_sent_gen: 0,
            cgroup: None,
            log,
        }
    }

    fn spawn_root(&mut self, payload: &SpawnPayload) -> std::io::Result<()> {
        let node = self.tree.mint_root().expect("root mint on a fresh tree");
        self.root = node;
        let now = Instant::now();
        // Supervisor deadline is a backstop *behind* the child's own deadline
        // (Detector A): the child should hit its budget and wrap up first.
        let backstop = Duration::from_millis(payload.limits.deadline_ms)
            .saturating_add(Duration::from_secs(60));
        self.liveness
            .insert(node, Liveness::new(now, now + backstop, self.liveness_cfg));
        // Register the pid → this supervisor's reap channel ATOMICALLY with the
        // fork (under the global reaper lock), so the reaper can't waitpid the
        // child before it is tracked.
        let sub = reaper::spawn_tracked(&self.reap_tx, || {
            spawn(&self.exe, payload, node, self.events_tx.clone())
        })?;
        // Place the root in the per-run cgroup; descendants it forks AFTER this
        // inherit membership, so the subtree becomes `cgroup.kill`-able at once.
        // (A grandchild forked in the sub-ms window before this write inherits the
        // parent cgroup and falls back to killpg + PDEATHSIG — i.e. the pre-cgroup
        // baseline, never worse. In practice the root reads its payload + makes an
        // LLM round-trip long before it could nest, so the window is empty.)
        if let Some(cg) = &self.cgroup {
            let placed = cg.place(sub.pid());
            let body = json!({"node": node.0, "pid": sub.pid(), "cgroup": cg.path().display().to_string(), "ok": placed});
            if placed {
                self.log.info("cgroup.placed", body);
            } else {
                // Placement failed (e.g. pids.max=0 refuses the move): the root
                // runs in the PARENT cgroup, so this run silently loses both the
                // limits AND the cgroup.kill teardown backstop — make it auditable.
                self.log.warn("cgroup.placed", body);
            }
        }
        self.pid_to_node.insert(sub.pid(), node);
        self.live.insert(node, sub);
        self.log.info(
            "subagent.spawn",
            json!({"node": node.0, "depth": payload.depth}),
        );
        // Frozen §4.3 `agentd_subagents_spawned_total` — wired here because the
        // spawn happens in THIS (supervisor) process, so the bump reaches the
        // supervisor's `/metrics` scrape (cross-process rollup is a v1 non-goal).
        crate::obs::metrics::record_subagent_spawned();
        Ok(())
    }

    /// Run until the root terminates and its subtree is reaped.
    fn run(mut self) -> SuperviseResult {
        loop {
            // Prove the supervisor loop is making progress (RFC 0010 §health):
            // this bumps liveness even while a subagent is busy, so a slow run
            // doesn't read as a wedged supervisor.
            crate::obs::health::tick();
            while let Ok((node, msg)) = self.events_rx.try_recv() {
                self.handle_event(node, msg);
            }
            self.reap();
            self.maybe_finalize_root_exit();

            // Terminal: the root reported a result/failure. Wait for the whole
            // subtree to exit; escalate only if stragglers linger past a grace.
            if self.root_terminal.is_some() {
                if self.live.is_empty() && self.drain.is_none() {
                    return self.root_terminal.take().unwrap();
                }
                let since = *self.finishing_since.get_or_insert_with(Instant::now);
                if since.elapsed() > FINISH_GRACE && self.drain.is_none() {
                    self.begin_drain(KillReason::Drain);
                }
            }

            // Tear down on a process SIGTERM *or* this run's own cancel flag (a
            // served async run cancelled by handle). Both drain gracefully.
            let cancelled = self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed));
            if (signals::draining() || cancelled) && self.drain.is_none() {
                self.begin_drain(KillReason::Drain);
            }

            // Forward an instance-wide pause/resume edge to every live child
            // (RFC 0015 §4.3). Skipped during a drain — the kill ladder owns the
            // children then, and a paused tree must still drain.
            self.forward_pause();

            // Forward a published intelligence hot-swap to every live child (RFC
            // 0018 §5.2) — the async-run reach of a reload that repoints the
            // endpoint list / changes the model. Same drain exclusion as pause.
            self.forward_swap();

            self.maybe_send_pings(Instant::now());
            self.tick_liveness();

            if self.drain.is_some()
                && let Some(result) = self.drive_drain()
            {
                return result;
            }

            signals::drain_wakeup(); // hygiene; the tick is our real timer

            match self.events_rx.recv_timeout(TICK) {
                Ok((node, msg)) => self.handle_event(node, msg),
                Err(RecvTimeoutError::Timeout) => {}
                // We hold events_tx, so the channel never truly disconnects.
                Err(RecvTimeoutError::Disconnected) => {}
            }
        }
    }

    fn handle_event(&mut self, node: NodeId, msg: AgentMsg) {
        let now = Instant::now();
        match msg {
            AgentMsg::Ready => {
                self.tree.set_status(node, NodeStatus::Running);
                self.on_event(node, now);
                self.log.info("subagent.ready", json!({"node": node.0}));
            }
            AgentMsg::Pong { .. } => {
                if let Some(l) = self.liveness.get_mut(&node) {
                    l.on_pong(now);
                }
            }
            AgentMsg::Event { .. } => self.on_event(node, now),
            AgentMsg::Usage(u) => {
                self.on_event(node, now);
                crate::obs::metrics::record_tokens(u.input_tokens, u.output_tokens);
                if self.tree.charge_tokens(node, u.total()) && self.drain.is_none() {
                    self.log
                        .warn("limit.exceeded", json!({"limit": "tree_tokens"}));
                    // Frozen §4.3 `agentd_limit_exceeded_total{limit}` — the
                    // `tree_tokens` leg is the SUPERVISOR's own bound (the tree
                    // ceiling is charged here), so it reaches the scrape. The other
                    // legs (steps/tokens/deadline/depth) trip inside the re-exec'd
                    // child loop; those are process-local (see metrics.rs caveat).
                    crate::obs::metrics::record_limit_exceeded("tree_tokens");
                    self.begin_drain(KillReason::TreeBudget);
                }
            }
            AgentMsg::Turn { outcome } => {
                // A warm session's per-turn completion. The blocking reactor
                // (supervise_once) is not the warm-session driver — the reactive
                // daemon is — so here a turn is just progress: reset liveness and
                // log it, never terminal. RFC 0008 §spawn-vs-continue.
                self.on_event(node, now);
                self.log.info(
                    "subagent.turn",
                    json!({"node": node.0, "status": outcome.status.as_str()}),
                );
            }
            AgentMsg::Result { outcome } => {
                self.tree.set_status(node, NodeStatus::Done);
                self.log.info(
                    "subagent.result",
                    json!({"node": node.0, "status": outcome.status.as_str()}),
                );
                if node == self.root && self.root_terminal.is_none() {
                    self.root_terminal = Some(SuperviseResult::Completed(outcome));
                }
            }
            AgentMsg::Failed { error } => {
                self.tree.set_status(node, NodeStatus::Failed);
                self.log
                    .error("subagent.failed", json!({"node": node.0, "err": error}));
                if node == self.root && self.root_terminal.is_none() {
                    self.root_terminal = Some(SuperviseResult::Failed(error));
                }
            }
            AgentMsg::IntelHealth { all_down, active } => {
                // RFC 0018 §6: the child reports its intel reachability (the
                // supervisor has no LLM of its own). Latch it into the process-global
                // `/readyz`, `agentd_intel_all_down`, and `agentd://intelligence`/
                // `capacity` read — the ONE eventually-consistent truth. Just
                // progress for liveness; never terminal. The notify-then-read on a
                // transition is fired by the served drain path (which holds the subs
                // registry); the blocking `supervise_once` reactor only latches.
                self.on_event(node, now);
                let transitioned = signals::set_intel_all_down(all_down);
                crate::obs::metrics::set_intel_all_down(all_down);
                if transitioned {
                    let mut f = json!({"node": node.0, "all_down": all_down});
                    if let Some(a) = &active {
                        f["active_index"] = json!(a.index);
                        f["active_transport"] = json!(a.transport);
                    }
                    self.log.info("intel.health", f);
                }
            }
        }
    }

    fn on_event(&mut self, node: NodeId, now: Instant) {
        if let Some(l) = self.liveness.get_mut(&node) {
            l.on_event(now);
        }
    }

    /// Ping every live child at the configured cadence so Detector C (ping/pong)
    /// is actually exercised. A *responsive* child answers from its control
    /// thread regardless of what its loop is doing (so a long model call reads
    /// `Busy`, not `Stuck`); a frozen/wedged child cannot, and after
    /// `pong_timeout` of silence it crosses into `Stuck`. Suspended during a
    /// drain (the kill ladder owns the child then).
    fn maybe_send_pings(&mut self, now: Instant) {
        if self.drain.is_some()
            || now.duration_since(self.last_ping) < self.liveness_cfg.ping_interval
        {
            return;
        }
        self.last_ping = now;
        self.ping_seq += 1;
        let seq = self.ping_seq;
        for h in self.live.values_mut() {
            let _ = h.send(&ControlMsg::Ping { seq });
        }
    }

    /// Translate the per-run `paused` atomic into `ctrl/pause`/`ctrl/resume`
    /// frames to every live child on each edge (RFC 0015 §4.3) — the async-run
    /// parallel of how `cancel` becomes `ctrl/cancel` in `begin_drain`. Debounced
    /// via `paused_sent` so an edge fans exactly once. Suspended during a drain:
    /// the kill ladder owns the children then, and a paused tree must still drain.
    /// The supervisor loop (and its `health::tick` liveness heartbeat) keeps
    /// running — only the children's agentic loops suspend (RFC 0015 §4.3).
    fn forward_pause(&mut self) {
        if self.drain.is_some() {
            return;
        }
        let want = self
            .paused
            .as_ref()
            .is_some_and(|p| p.load(Ordering::Relaxed));
        if want == self.paused_sent {
            return; // no edge
        }
        let msg = if want {
            ControlMsg::Pause
        } else {
            ControlMsg::Resume
        };
        for h in self.live.values_mut() {
            let _ = h.send(&msg);
        }
        self.log.info(
            "subagent.pause",
            json!({"paused": want, "live": self.live.len()}),
        );
        self.paused_sent = want;
    }

    /// Forward a published intelligence hot-swap (RFC 0018 §5.2) to every live
    /// child as a `ctrl/swap_intel` frame — the async-run parallel of how
    /// `forward_pause` translates the `paused` edge. A hot reload that touches
    /// `intelligence`/`model` publishes the new config into the run's
    /// [`SwapChannel`]; this reads each tick and fans the LATEST config once per
    /// published generation (debounced via `swap_sent_gen`). Each child drains it
    /// at its next turn boundary — we never tear an in-flight `complete_once`.
    /// Suspended during a drain: the kill ladder owns the children then. The
    /// token rides the frame (like the spawn payload) but is NEVER logged — only
    /// the bounded fan count + (non-secret) policy.
    fn forward_swap(&mut self) {
        if self.drain.is_some() {
            return;
        }
        let Some(ch) = self.swap.as_ref() else {
            return;
        };
        let Some((swap, generation)) = ch.take_newer(self.swap_sent_gen) else {
            return; // no newer swap published since the last fan
        };
        let policy = swap.policy.as_str();
        let msg = ControlMsg::SwapIntel(Box::new(swap));
        for h in self.live.values_mut() {
            let _ = h.send(&msg);
        }
        self.log.info(
            "subagent.swap_intel",
            json!({"live": self.live.len(), "policy": policy}),
        );
        self.swap_sent_gen = generation;
    }

    fn reap(&mut self) {
        // Reap every tick (cheap WNOHANG). The SIGCHLD flag/self-pipe is only a
        // promptness optimization; correctness must not depend on it — a nested
        // `supervise_once` runs inside a subagent that never installed the
        // SIGCHLD handler, yet still needs to reap its own children.
        signals::take_child_exit();
        // The global reaper drains `waitpid(-1)` and routes each reaped pid to
        // the owning supervisor; we then drain OUR pids' exits. Any tick of any
        // live supervisor drives reaping for the whole process — no private
        // `waitpid(-1)`, so concurrent supervisors never steal each other's
        // children.
        reaper::reap_and_dispatch();
        while let Ok(r) = self.reap_rx.try_recv() {
            match self.pid_to_node.remove(&r.pid) {
                Some(node) => {
                    if let Some(mut h) = self.live.remove(&node) {
                        h.mark_reaped(); // suppress Drop signalling (PID reuse)
                    }
                    if let Some(l) = self.liveness.get_mut(&node) {
                        l.on_eof();
                    }
                    self.log.info(
                        "subagent.exit",
                        json!({"node": node.0, "outcome": format!("{:?}", r.outcome)}),
                    );
                    // Frozen §4.3 `agentd_subagents_exited_total{status}` — wired
                    // here (supervisor process → reaches the scrape). The reap site
                    // carries only the OS `WaitOutcome`, not the RFC 0007 §3.4
                    // terminal status the child reported, so this is a COARSE
                    // projection onto the closed status domain (clean exit →
                    // `completed`, any signal → `cancelled` [torn down], a non-zero
                    // exit → `crashed`); the precise per-status breakdown lives in
                    // the child's `subagent.result`/`subagent.failed` log frames.
                    crate::obs::metrics::record_subagent_exited(exit_status_label(r.outcome));
                    // The root exited without reporting a result. Only treat that
                    // as an unexpected failure when we are NOT tearing the tree
                    // down: during a drain/stuck/deadline/budget teardown the root
                    // dying is the *expected* consequence, so the teardown reason
                    // (Killed(..)) must stand — not a synthetic "exited without a
                    // result" that would mask a stuck-kill as a generic failure.
                    //
                    // Don't synthesize the failure *here*: the child may have
                    // written its real `Result`/`Failed` frame to stdout just
                    // before exiting, and that frame races this reap on an
                    // independent channel. Start the `RESULT_GRACE` window instead
                    // and let `maybe_finalize_root_exit` conclude only if nothing
                    // arrives — otherwise the real reason (e.g. intel-unavailable →
                    // exit 4) would be masked as a generic exit 1.
                    if node == self.root && self.root_terminal.is_none() && self.drain.is_none() {
                        // A SIGKILL with a non-zero cgroup OOM count means
                        // `memory.max` killed the root — surface that plainly
                        // instead of letting RESULT_GRACE conclude a generic
                        // "exited without a result" that hides the operator's limit.
                        let oom = matches!(r.outcome, WaitOutcome::Signaled(s) if s == libc::SIGKILL)
                            && self
                                .cgroup
                                .as_ref()
                                .and_then(|c| c.oom_kills())
                                .is_some_and(|n| n > 0);
                        if oom {
                            self.log.warn(
                                "cgroup.oom_kill",
                                json!({"node": node.0, "cgroup": self.cgroup.as_ref().map(|c| c.path().display().to_string())}),
                            );
                            self.root_terminal = Some(SuperviseResult::Failed(
                                "subagent killed by cgroup memory limit (OOM)".into(),
                            ));
                        } else {
                            self.root_exited_at.get_or_insert_with(Instant::now);
                        }
                    }
                }
                None => self.log.debug(
                    "subagent.reap_unknown",
                    json!({"pid": r.pid, "outcome": format!("{:?}", r.outcome)}),
                ),
            }
        }
    }

    /// Once the root has been reaped without a terminal (`root_exited_at` set),
    /// grant `RESULT_GRACE` for its trailing `Result`/`Failed` frame to arrive on
    /// the events channel before concluding it "exited without a result". If the
    /// frame lands first, `handle_event` sets `root_terminal` and this never fires.
    fn maybe_finalize_root_exit(&mut self) {
        if self.root_terminal.is_some() || self.drain.is_some() {
            return;
        }
        if self
            .root_exited_at
            .is_some_and(|t| t.elapsed() > RESULT_GRACE)
        {
            self.root_terminal = Some(SuperviseResult::Failed(
                "subagent exited without a result".into(),
            ));
        }
    }

    fn tick_liveness(&mut self) {
        if self.drain.is_some() {
            return; // already tearing down
        }
        let now = Instant::now();
        let mut verdict: Option<(NodeId, KillReason)> = None;
        for (node, l) in &self.liveness {
            if !self.live.contains_key(node) {
                continue; // already exited
            }
            match l.classify(now) {
                Health::Stuck => verdict = Some((*node, KillReason::Stuck)),
                Health::DeadlineExceeded => verdict = Some((*node, KillReason::Deadline)),
                _ => {}
            }
        }
        if let Some((node, reason)) = verdict {
            // A distinct, queryable signal that liveness classification (not a
            // deadline) condemned the child (RFC 0010 §2.9 `subagent.stuck`).
            if reason == KillReason::Stuck {
                self.log.warn("subagent.stuck", json!({"node": node.0}));
            }
            self.log.warn(
                "subagent.teardown",
                json!({"node": node.0, "reason": format!("{reason:?}")}),
            );
            self.begin_drain(reason);
        }
    }

    fn begin_drain(&mut self, reason: KillReason) {
        if self.drain.is_some() {
            return;
        }
        let now = Instant::now();
        self.tree.set_draining(); // refuse new spawns mid-teardown
        // Graceful first: ask every live child to wind down, deepest-first.
        for node in self.tree.deepest_first() {
            if let Some(h) = self.live.get_mut(&node) {
                let _ = h.send(&ControlMsg::Cancel {
                    reason: format!("{reason:?}"),
                });
            }
        }
        self.log.warn(
            "subagent.drain",
            json!({"reason": format!("{reason:?}"), "live": self.live.len()}),
        );
        // Frozen §4.3 `agentd_drains_total{phase="started"}` — the reactor's own
        // teardown begins in THIS (supervisor) process, so it reaches the scrape.
        // `completed` is bumped when the ladder finishes (`LadderAction::Done`),
        // `forced` when the drain budget is blown (`drain.timeout`).
        crate::obs::metrics::record_drain("started");
        self.drain = Some(Drain {
            reason,
            ladder: Ladder::with_defaults(now),
            deadline: now + self.drain_timeout,
            forced: false,
        });
    }

    /// Advance the active teardown ladder. Returns the final result once the
    /// subtree is fully reaped (or the budget is blown and we force-kill).
    fn drive_drain(&mut self) -> Option<SuperviseResult> {
        let now = Instant::now();
        let all_reaped = self.live.is_empty();
        let budget_blown = self.drain.as_ref().is_some_and(|d| now >= d.deadline);
        let force = signals::force() || budget_blown;
        // One-shot: the drain budget was exceeded, so we force the kill ladder
        // rather than wait. Ops should keep AGENTD_DRAIN_TIMEOUT below the pod's
        // termination grace so this fires before the kubelet SIGKILLs us.
        if budget_blown && self.drain.as_ref().is_some_and(|d| !d.forced) {
            if let Some(d) = self.drain.as_mut() {
                d.forced = true;
            }
            self.log.warn(
                "drain.timeout",
                json!({"live": self.live.len(), "drain_ms": self.drain_timeout.as_millis() as u64}),
            );
            // Frozen §4.3 `agentd_drains_total{phase="forced"}` — the budget was
            // exceeded, so the kill ladder is being forced (distinguishes a clean
            // drain from one that overran). Latched once via the `forced` flag.
            crate::obs::metrics::record_drain("forced");
        }
        // Hard escape: completion is otherwise gated solely on `live` emptying,
        // which depends on every child's exit being reaped + dispatched. If the
        // budget is exceeded by a further grace and the subtree STILL isn't reaped
        // (a wedged-uninterruptible child, or — defensively — a reap that never
        // arrived), give up rather than spin: mark the stragglers reaped so their
        // `Drop` won't block, log the leak, and return the teardown reason. This
        // guarantees `drive_drain` always terminates.
        if !all_reaped
            && self
                .drain
                .as_ref()
                .is_some_and(|d| now >= d.deadline + ABANDON_GRACE)
        {
            let reason = self
                .drain
                .as_ref()
                .map(|d| d.reason)
                .unwrap_or(KillReason::Drain);
            // Last word before abandoning: `cgroup.kill` SIGKILLs the entire
            // subtree atomically — including any process that escaped the group
            // and would survive the killpg above (the whole point of the cgroup).
            if let Some(cg) = &self.cgroup {
                cg.kill_all();
            }
            self.log.warn(
                "drain.abandon",
                json!({"live": self.live.len(), "reason": format!("{reason:?}")}),
            );
            for h in self.live.values_mut() {
                h.mark_reaped(); // already SIGKILL'd; don't let Drop block waiting on it
            }
            return Some(
                self.root_terminal
                    .take()
                    .unwrap_or(SuperviseResult::Killed(reason)),
            );
        }
        let action = match self.drain.as_mut() {
            Some(d) => d.ladder.poll(now, all_reaped, force),
            None => return None,
        };
        match action {
            LadderAction::Wait => None,
            LadderAction::Term => {
                for h in self.live.values() {
                    term_group(h.pgid());
                }
                self.log
                    .warn("subagent.sigterm", json!({"live": self.live.len()}));
                // Frozen §4.3 `agentd_subagent_stuck_kills_total{signal="term"}` —
                // the wedged-subtree kill ladder runs in THIS (supervisor) process.
                crate::obs::metrics::record_subagent_stuck_kill("term");
                None
            }
            LadderAction::Kill => {
                for h in self.live.values() {
                    kill_group(h.pgid());
                }
                // Atomic subtree backstop: catches any process that left its
                // group (setsid) and so slipped past killpg. No-op when the
                // cgroup feature is off.
                if let Some(cg) = &self.cgroup {
                    cg.kill_all();
                }
                self.log
                    .warn("subagent.sigkill", json!({"live": self.live.len()}));
                // Frozen §4.3 `agentd_subagent_stuck_kills_total{signal="kill"}`.
                crate::obs::metrics::record_subagent_stuck_kill("kill");
                None
            }
            LadderAction::Done => {
                let reason = self
                    .drain
                    .as_ref()
                    .map(|d| d.reason)
                    .unwrap_or(KillReason::Drain);
                // Frozen §4.3 `agentd_drains_total{phase="completed"}` — the ladder
                // finished and the whole subtree is reaped (supervisor process).
                crate::obs::metrics::record_drain("completed");
                // Prefer a real terminal the root produced; else the teardown reason.
                Some(
                    self.root_terminal
                        .take()
                        .unwrap_or(SuperviseResult::Killed(reason)),
                )
            }
        }
    }
}

/// Coarse projection of an OS `WaitOutcome` onto the frozen RFC 0007 §3.4
/// terminal-status label domain for `agentd_subagents_exited_total{status}`. The
/// reap site only sees how the process *ended* (exit code / signal), not the
/// child's reported terminal status, so this maps clean→`completed`, signalled→
/// `cancelled` (the subtree was torn down), and a non-zero exit→`crashed`. The
/// exact per-status counts come from the child's own `subagent.result` frames.
fn exit_status_label(outcome: WaitOutcome) -> &'static str {
    match outcome {
        WaitOutcome::Exited(0) => "completed",
        WaitOutcome::Exited(_) => "crashed",
        WaitOutcome::Signaled(_) => "cancelled",
    }
}

/// Supervise one root subagent to completion (once-mode entry). The handle
/// map's `Drop` backstops any leak on early return.
///
/// **Concurrency:** multiple supervisors may now run **concurrently** in one
/// process (the daemon's mode loop, served-MCP `subagent.spawn` runs, nested
/// orchestration). They no longer serialize on a lock — the process-global
/// [`reaper`] owns the single `waitpid(-1)` and dispatches each reaped pid to its
/// owning supervisor by pid, so concurrent reactors never steal each other's
/// children (RFC 0003, RFC 0005 §3.2).
pub fn supervise_once(
    exe: PathBuf,
    payload: &SpawnPayload,
    drain_timeout: Duration,
    log: Logger,
) -> std::io::Result<SuperviseResult> {
    supervise_cancellable(exe, payload, drain_timeout, log, None)
}

/// Like [`supervise_once`] but with an optional per-run `cancel` flag: setting it
/// drains this run's subtree gracefully (a served async run cancelled by handle),
/// independent of process SIGTERM. RFC 0005 §3.2.
pub fn supervise_cancellable(
    exe: PathBuf,
    payload: &SpawnPayload,
    drain_timeout: Duration,
    log: Logger,
    cancel: Option<Arc<AtomicBool>>,
) -> std::io::Result<SuperviseResult> {
    supervise_pausable(exe, payload, drain_timeout, log, cancel, None)
}

/// Like [`supervise_cancellable`] but also with an optional per-run `paused` flag
/// (RFC 0015 §4.3): setting it forwards `ctrl/pause` to every live child so each
/// suspends at its next turn boundary; clearing it forwards `ctrl/resume`. The
/// async-session pause channel, the parallel of `cancel`. The supervisor loop
/// (and its liveness heartbeat) is never gated by `paused` — only the children's
/// agentic loops suspend.
pub fn supervise_pausable(
    exe: PathBuf,
    payload: &SpawnPayload,
    drain_timeout: Duration,
    log: Logger,
    cancel: Option<Arc<AtomicBool>>,
    paused: Option<Arc<AtomicBool>>,
) -> std::io::Result<SuperviseResult> {
    supervise_swappable(exe, payload, drain_timeout, log, cancel, paused, None)
}

/// Like [`supervise_pausable`] but also with an optional per-run intelligence
/// hot-swap channel (RFC 0018 §5.2): a hot reload that touches `intelligence`/
/// `model` publishes the new config into the [`SwapChannel`], and this run's
/// reactor fans `ctrl/swap_intel` to every live child at the next tick (each
/// applies it at its turn boundary). The async-session swap channel, the parallel
/// of `paused`. `None` for runs with no swap channel (a blocking once-mode run,
/// or a build without hot reload) — the no-swap path is unchanged.
#[allow(clippy::too_many_arguments)]
pub fn supervise_swappable(
    exe: PathBuf,
    payload: &SpawnPayload,
    drain_timeout: Duration,
    log: Logger,
    cancel: Option<Arc<AtomicBool>>,
    paused: Option<Arc<AtomicBool>>,
    swap: Option<SwapChannel>,
) -> std::io::Result<SuperviseResult> {
    crate::obs::metrics::record_run_started();
    let mut sup = Supervisor::new(exe, drain_timeout, log);
    sup.cancel = cancel;
    sup.paused = paused;
    sup.swap = swap;
    // Per-run child cgroup (opt-in, best-effort): the root + its whole subtree
    // land here so teardown can `cgroup.kill` them atomically. `None` when the
    // feature is off or the tree isn't writable — the run then relies on
    // PDEATHSIG + the kill ladder exactly as before.
    sup.cgroup = CgroupGuard::for_run();
    sup.spawn_root(payload)?;
    let result = sup.run();
    crate::obs::metrics::record_run(match &result {
        SuperviseResult::Completed(_) => crate::obs::metrics::RunOutcome::Completed,
        SuperviseResult::Failed(_) => crate::obs::metrics::RunOutcome::Failed,
        SuperviseResult::Killed(_) => crate::obs::metrics::RunOutcome::Killed,
    });
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_status_label_projects_wait_outcome_onto_closed_domain() {
        // The coarse OS-outcome → frozen RFC 0007 §3.4 status projection used to
        // drive `agentd_subagents_exited_total{status}` from the reap site.
        assert_eq!(exit_status_label(WaitOutcome::Exited(0)), "completed");
        // a non-zero exit (e.g. exit 7 budget / exit 1 generic) → crashed bucket
        assert_eq!(exit_status_label(WaitOutcome::Exited(1)), "crashed");
        assert_eq!(exit_status_label(WaitOutcome::Exited(7)), "crashed");
        // any signal death (SIGTERM/SIGKILL from the teardown ladder) → cancelled
        assert_eq!(exit_status_label(WaitOutcome::Signaled(15)), "cancelled");
        assert_eq!(exit_status_label(WaitOutcome::Signaled(9)), "cancelled");
        // every projected label is in the frozen subagents-exited status domain.
        for o in [
            WaitOutcome::Exited(0),
            WaitOutcome::Exited(1),
            WaitOutcome::Signaled(9),
        ] {
            let label = exit_status_label(o);
            assert!(
                ["completed", "crashed", "cancelled"].contains(&label),
                "projected status {label} is outside the closed domain"
            );
        }
    }
}
