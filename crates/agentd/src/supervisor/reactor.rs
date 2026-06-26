//! The supervisor reactor — the single loop that owns the process tree.
//! RFC 0002 §reactor, RFC 0003 §supervision.
//!
//! One thread owns the `Tree`, the `NodeId → Subagent` handle map, and a
//! per-child `Liveness`. It blocks on the merged event channel (every
//! subagent's reader thread forwards `(NodeId, AgentMsg)` here) with a short
//! `recv_timeout` tick that doubles as the timer; each tick it reaps exited
//! children (`reap::reap_pending` on the SIGCHLD flag), classifies liveness,
//! and — on a drain signal or a stuck/deadline/budget verdict — drives the
//! bounded `kill::Ladder` over `tree.deepest_first()`.
//!
//! M2 supervises a single **root** subagent (once-mode). Nested children
//! arrive via the self-MCP `subagent.spawn` tool (later in M2/M3); the loop is
//! already written to tear down a multi-node tree deepest-first.
//!
//! The signal self-pipe (`signals::wakeup_fd`) is built for sub-tick
//! promptness; v1's short tick already bounds signal/deadline latency, so the
//! reactor polls the flags each tick and just drains the pipe for hygiene.

use crate::agentloop::stop::Outcome;
use crate::obs::log::Logger;
use crate::signals;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::kill::{kill_group, term_group, Ladder, LadderAction};
use crate::supervisor::liveness::{Health, Liveness, LivenessConfig};
use crate::supervisor::reap;
use crate::supervisor::spawn::{spawn, Subagent};
use crate::supervisor::tree::{Caps, NodeId, NodeStatus, Tree};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Reactor tick. Bounds signal/deadline detection latency without a poll loop.
const TICK: Duration = Duration::from_millis(200);
/// How long to wait for a *completed* root's stragglers to exit before draining.
const FINISH_GRACE: Duration = Duration::from_secs(5);

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
    root: NodeId,
    root_terminal: Option<SuperviseResult>,
    finishing_since: Option<Instant>,
    drain: Option<Drain>,
    drain_timeout: Duration,
    liveness_cfg: LivenessConfig,
    log: Logger,
}

impl Supervisor {
    fn new(exe: PathBuf, drain_timeout: Duration, log: Logger) -> Supervisor {
        let (events_tx, events_rx) = mpsc::channel();
        Supervisor {
            exe,
            tree: Tree::new(Caps::default()),
            live: HashMap::new(),
            liveness: HashMap::new(),
            pid_to_node: HashMap::new(),
            events_tx,
            events_rx,
            root: NodeId(0),
            root_terminal: None,
            finishing_since: None,
            drain: None,
            drain_timeout,
            liveness_cfg: LivenessConfig::default(),
            log,
        }
    }

    fn spawn_root(&mut self, payload: &SpawnPayload) -> std::io::Result<()> {
        let node = self.tree.mint_root().expect("root mint on a fresh tree");
        self.root = node;
        let now = Instant::now();
        // Supervisor deadline is a backstop *behind* the child's own deadline
        // (Detector A): the child should hit its budget and wrap up first.
        let backstop =
            Duration::from_millis(payload.limits.deadline_ms).saturating_add(Duration::from_secs(60));
        self.liveness.insert(node, Liveness::new(now, now + backstop, self.liveness_cfg));
        let sub = spawn(&self.exe, payload, node, self.events_tx.clone())?;
        self.pid_to_node.insert(sub.pid(), node);
        self.live.insert(node, sub);
        self.log.info("subagent.spawn", json!({"node": node.0, "depth": payload.depth}));
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

            if signals::draining() && self.drain.is_none() {
                self.begin_drain(KillReason::Drain);
            }

            self.tick_liveness();

            if self.drain.is_some() && let Some(result) = self.drive_drain() {
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
                    self.log.warn("limit.exceeded", json!({"limit": "tree_tokens"}));
                    self.begin_drain(KillReason::TreeBudget);
                }
            }
            AgentMsg::Result { outcome } => {
                self.tree.set_status(node, NodeStatus::Done);
                self.log.info("subagent.result", json!({"node": node.0, "status": outcome.status.as_str()}));
                if node == self.root && self.root_terminal.is_none() {
                    self.root_terminal = Some(SuperviseResult::Completed(outcome));
                }
            }
            AgentMsg::Failed { error } => {
                self.tree.set_status(node, NodeStatus::Failed);
                self.log.error("subagent.failed", json!({"node": node.0, "err": error}));
                if node == self.root && self.root_terminal.is_none() {
                    self.root_terminal = Some(SuperviseResult::Failed(error));
                }
            }
        }
    }

    fn on_event(&mut self, node: NodeId, now: Instant) {
        if let Some(l) = self.liveness.get_mut(&node) {
            l.on_event(now);
        }
    }

    fn reap(&mut self) {
        // Reap every tick (cheap WNOHANG). The SIGCHLD flag/self-pipe is only a
        // promptness optimization; correctness must not depend on it — a nested
        // `supervise_once` runs inside a subagent that never installed the
        // SIGCHLD handler, yet still needs to reap its own children.
        signals::take_child_exit();
        for r in reap::reap_pending() {
            match self.pid_to_node.remove(&r.pid) {
                Some(node) => {
                    if let Some(mut h) = self.live.remove(&node) {
                        h.mark_reaped(); // suppress Drop signalling (PID reuse)
                    }
                    if let Some(l) = self.liveness.get_mut(&node) {
                        l.on_eof();
                    }
                    self.log.info("subagent.exit", json!({"node": node.0, "outcome": format!("{:?}", r.outcome)}));
                    if node == self.root && self.root_terminal.is_none() {
                        self.root_terminal =
                            Some(SuperviseResult::Failed("subagent exited without a result".into()));
                    }
                }
                None => self.log.debug(
                    "subagent.reap_unknown",
                    json!({"pid": r.pid, "outcome": format!("{:?}", r.outcome)}),
                ),
            }
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
            self.log.warn("subagent.teardown", json!({"node": node.0, "reason": format!("{reason:?}")}));
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
                let _ = h.send(&ControlMsg::Cancel { reason: format!("{reason:?}") });
            }
        }
        self.log.warn("subagent.drain", json!({"reason": format!("{reason:?}"), "live": self.live.len()}));
        self.drain =
            Some(Drain { reason, ladder: Ladder::with_defaults(now), deadline: now + self.drain_timeout, forced: false });
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
                self.log.warn("subagent.sigterm", json!({"live": self.live.len()}));
                None
            }
            LadderAction::Kill => {
                for h in self.live.values() {
                    kill_group(h.pgid());
                }
                self.log.warn("subagent.sigkill", json!({"live": self.live.len()}));
                None
            }
            LadderAction::Done => {
                let reason = self.drain.as_ref().map(|d| d.reason).unwrap_or(KillReason::Drain);
                // Prefer a real terminal the root produced; else the teardown reason.
                Some(self.root_terminal.take().unwrap_or(SuperviseResult::Killed(reason)))
            }
        }
    }
}

/// Serializes `supervise_once` *within a process*. A process may run several
/// supervisors over its lifetime — the daemon's mode loop AND served-MCP
/// `subagent.spawn` calls (RFC 0005) — but they must not run **concurrently**:
/// each reactor reaps via `waitpid(-1)` (which also collects `PR_SET_CHILD_
/// SUBREAPER` orphans, RFC 0003), so two concurrent reactors would steal each
/// other's children and hang the robbed one. Serializing keeps orphan reaping
/// intact without a process-wide reaper. The lock is per-process, so nested
/// supervise in a *separate* subagent process never contends. (A single-reaper
/// refactor that allows true concurrency is a throughput follow-up.)
static SUPERVISE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Supervise one root subagent to completion (once-mode entry). The handle
/// map's `Drop` backstops any leak on early return.
pub fn supervise_once(
    exe: PathBuf,
    payload: &SpawnPayload,
    drain_timeout: Duration,
    log: Logger,
) -> std::io::Result<SuperviseResult> {
    let _serialize = SUPERVISE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::obs::metrics::record_run_started();
    let mut sup = Supervisor::new(exe, drain_timeout, log);
    sup.spawn_root(payload)?;
    let result = sup.run();
    crate::obs::metrics::record_run(match &result {
        SuperviseResult::Completed(_) => crate::obs::metrics::RunOutcome::Completed,
        SuperviseResult::Failed(_) => crate::obs::metrics::RunOutcome::Failed,
        SuperviseResult::Killed(_) => crate::obs::metrics::RunOutcome::Killed,
    });
    Ok(result)
}
