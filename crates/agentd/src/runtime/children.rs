// SPDX-License-Identifier: AGPL-3.0-only
//! The runtime's **flat child tree**: every turn worker and subagent is a
//! direct child of the supervisor — the tree is one level deep, so there is
//! exactly one process that can orphan work and exactly one place that reaps.
//! Children are spawned through `supervisor::spawn` (the reaper + PDEATHSIG +
//! process groups), tracked here with their purpose, liveness and
//! cancellation, and torn down by the kill ladder when a drain or a
//! `subagent.kill` stops them.

use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::kill::{Ladder, LadderAction, kill_group, term_group};
use crate::supervisor::liveness::{Health, Liveness, LivenessConfig};
use crate::supervisor::reap::Reaped;
use crate::supervisor::spawn::{Subagent, spawn};
use crate::supervisor::tree::NodeId;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Why a child exists.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildKind {
    /// A root/conversation turn for context `ctx`, triggered by inbox event `event`.
    RootTurn {
        ctx: String,
        event: Option<String>,
        reservation: Option<u64>,
        /// The message-hop depth this turn inherited, so anything it starts —
        /// a run, a nested message — continues the chain rather than resetting
        /// it. Lives exactly as long as the turn, which is the window that
        /// matters.
        msg_depth: u32,
        /// The RFC 0043 session the message that started this turn came
        /// through (RFC 0044 §5.12): the turn's delegated calls act through
        /// it, as it is when each is made.
        session: Option<String>,
    },
    /// A workflow step turn (`agent` / `think`).
    StepTurn {
        run: String,
        step: String,
        reservation: Option<u64>,
    },
    /// A structured think serving an internal request (compaction, `think` tool,
    /// preflight). `reply_to` = the requesting child + request id when a tool
    /// call is waiting on it.
    Think {
        purpose: String,
        ctx: Option<String>,
        reply_to: Option<(NodeId, u64)>,
        extra: Value,
        reservation: Option<u64>,
    },
    /// A subagent, addressed by its registry handle rather than its node id so
    /// callers can keep referring to it across a restore.
    Subagent { handle: String },
}

/// One live child.
pub struct Child {
    pub sub: Subagent,
    pub kind: ChildKind,
    pub started: Instant,
    pub liveness: Liveness,
    pub cancelled: bool,
    pub tokens: u64,
    /// The span the child was spawned under (its spawn payload's), which the
    /// supervisor's lines about it carry and whatever it asks for continues.
    pub trace: Option<crate::obs::trace::TraceContext>,
    /// Whether this child's unit has been settled — a terminal frame
    /// (`TurnDone` / `Failed`) was folded back into the durable state. The
    /// reap path needs the answer *after* the record has left the table (see
    /// [`Children::is_settled`]): the child's mere presence cannot give it,
    /// because a settled worker also stays in the table until it is reaped.
    pub settled: bool,
    /// The kill ladder a drain or a `subagent.kill` stopped this child with:
    /// `Cancel` when it was stopped, `SIGTERM` to its group after a grace,
    /// `SIGKILL` after another — or `SIGKILL` at once when the drain is
    /// forced. `None` while the child is let run.
    pub stop: Option<Ladder>,
    /// The drain stopped this child while it had work of its own, so what it
    /// owed — a step's result, a turn's reply, a subagent's outcome — is left
    /// for the next life, never failed. A child already cancelled for a reason
    /// of its own (a run's cancel, a liveness verdict) keeps that reason.
    pub parked: bool,
    /// For a turn worker, the delegated servers' tools its plan offered — a
    /// step's `tools:` and `servers:` narrowed; for a subagent, those of the
    /// delegated servers it declares that its `tools:` grant admits: the
    /// supervisor makes a delegated call for it of these alone (RFC 0044
    /// §5.7, per-`(step, tool)` use-authorisation). `None` for any other
    /// child.
    pub offered: Option<std::collections::BTreeSet<String>>,
}

/// The children registry.
pub struct Children {
    exe: PathBuf,
    events: crate::supervisor::spawn::FrameSink,
    reap_tx: Sender<Reaped>,
    map: HashMap<NodeId, Child>,
    pid_to_node: HashMap<i32, NodeId>,
    next: u64,
    liveness_cfg: LivenessConfig,
    last_ping: Instant,
    ping_seq: u64,
    /// The child reaped most recently, kept past its removal from `map` as
    /// `(node, kind, settled)`: the reap path asks "did this worker report a
    /// terminal frame?" — and, when it did not, needs the kind to route the
    /// failure and release the reservation the kind carries. One slot is
    /// enough because the reactor drains reaps one at a time and finishes with
    /// a node before taking the next.
    last_reaped: Option<(NodeId, ChildKind, bool)>,
    /// How many children have a kill ladder armed, so a tick with none
    /// climbs nothing.
    stopping: usize,
}

impl Children {
    pub fn new(
        exe: PathBuf,
        events: crate::supervisor::spawn::FrameSink,
        reap_tx: Sender<Reaped>,
    ) -> Children {
        Children {
            exe,
            events,
            reap_tx,
            map: HashMap::new(),
            pid_to_node: HashMap::new(),
            next: 1,
            liveness_cfg: LivenessConfig::from_env(),
            last_ping: Instant::now(),
            ping_seq: 0,
            last_reaped: None,
            stopping: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn get(&self, node: NodeId) -> Option<&Child> {
        self.map.get(&node)
    }
    pub fn get_mut(&mut self, node: NodeId) -> Option<&mut Child> {
        self.map.get_mut(&node)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &Child)> {
        self.map.iter()
    }
    pub fn count_kind(&self, f: impl Fn(&ChildKind) -> bool) -> usize {
        self.map.values().filter(|c| f(&c.kind)).count()
    }
    /// The OS pid of a live child (for logs; the reaper owns lifecycle).
    pub fn pid_of(&self, node: NodeId) -> Option<i32> {
        self.map.get(&node).map(|c| c.sub.pid())
    }
    /// Whether `pid` is a tracked child (a reap for an unknown pid needs no
    /// frame-ordering deferral).
    pub fn has_pid(&self, pid: i32) -> bool {
        self.pid_to_node.contains_key(&pid)
    }
    /// The reap channel — for supervised children spawned OUTSIDE this
    /// registry (instance-tier daemons, which have no control channel) whose
    /// exits must still route to this reactor.
    pub fn reap_sender(&self) -> Sender<Reaped> {
        self.reap_tx.clone()
    }
    /// Join `pid`'s stdout reader — after this, all its frames are queued.
    pub fn join_reader_of(&mut self, pid: i32) {
        if let Some(node) = self.pid_to_node.get(&pid)
            && let Some(c) = self.map.get_mut(node)
        {
            c.sub.join_reader();
        }
    }

    /// Spawn a child (tracked with the reaper). The payload's `telemetry`
    /// must already carry the correlation ids. `trace` is the span it is
    /// spawned under, as handed to it in `telemetry`: kept whole here, with
    /// the parent the `traceparent` cannot carry, for the lines about it.
    pub fn spawn(
        &mut self,
        payload: &SpawnPayload,
        kind: ChildKind,
        trace: Option<crate::obs::trace::TraceContext>,
        deadline: Duration,
    ) -> std::io::Result<NodeId> {
        let node = NodeId(self.next);
        self.next += 1;
        let exe = self.exe.clone();
        let events = self.events.clone();
        let sub = crate::supervisor::reaper::spawn_tracked(&self.reap_tx, || {
            spawn(&exe, payload, node, events)
        })?;
        let now = Instant::now();
        let child = Child {
            liveness: Liveness::new(
                now,
                now + deadline + Duration::from_secs(60),
                self.liveness_cfg,
            ),
            sub,
            kind,
            started: now,
            cancelled: false,
            tokens: 0,
            trace,
            settled: false,
            stop: None,
            parked: false,
            offered: None,
        };
        self.pid_to_node.insert(child.sub.pid(), node);
        self.map.insert(node, child);
        crate::obs::metrics::record_subagent_spawned();
        Ok(node)
    }

    /// A frame arrived from `node`: refresh liveness (returns whether known).
    pub fn on_frame(&mut self, node: NodeId, msg: &AgentMsg) -> bool {
        let Some(c) = self.map.get_mut(&node) else {
            return false;
        };
        let now = Instant::now();
        match msg {
            AgentMsg::Pong { .. } => c.liveness.on_pong(now),
            AgentMsg::Usage(u) => {
                c.tokens += u.total();
                c.liveness.on_event(now);
            }
            _ => c.liveness.on_event(now),
        }
        true
    }

    /// A terminal frame was folded back in for `node`: its unit is settled, so
    /// the reap path must not fail it a second time. Marks the just-reaped
    /// record too, so a failure routed *from* the reap path is not re-entered.
    pub fn mark_settled(&mut self, node: NodeId) {
        if let Some(c) = self.map.get_mut(&node) {
            c.settled = true;
        }
        if let Some((n, _, settled)) = self.last_reaped.as_mut()
            && *n == node
        {
            *settled = true;
        }
    }

    /// Whether `node`'s unit has been settled by a terminal frame — answerable
    /// after the child is gone, which is the only time the question is asked.
    /// A node we never knew counts as settled: there is nothing left to fail.
    pub fn is_settled(&self, node: NodeId) -> bool {
        if let Some(c) = self.map.get(&node) {
            return c.settled;
        }
        match &self.last_reaped {
            Some((n, _, settled)) if *n == node => *settled,
            _ => true,
        }
    }

    /// The kind of the most recently reaped child, so its failure can still be
    /// routed (and its reservation released) once `on_reaped` has removed it.
    pub fn reaped_kind(&self, node: NodeId) -> Option<ChildKind> {
        match &self.last_reaped {
            Some((n, kind, _)) if *n == node => Some(kind.clone()),
            _ => None,
        }
    }

    /// A child was reaped: forget it and return its record.
    pub fn on_reaped(&mut self, r: &Reaped) -> Option<(NodeId, Child)> {
        let node = self.pid_to_node.remove(&r.pid)?;
        let mut c = self.map.remove(&node)?;
        if c.stop.is_some() {
            self.stopping -= 1;
        }
        c.sub.mark_reaped();
        c.liveness.on_eof();
        self.last_reaped = Some((node, c.kind.clone(), c.settled));
        crate::obs::metrics::record_subagent_exited(match r.outcome {
            crate::supervisor::reap::WaitOutcome::Exited(0) => "completed",
            crate::supervisor::reap::WaitOutcome::Exited(_) => "crashed",
            crate::supervisor::reap::WaitOutcome::Signaled(_) => "cancelled",
        });
        Some((node, c))
    }

    /// Send a control frame to a child.
    pub fn send(&mut self, node: NodeId, msg: &ControlMsg) -> bool {
        self.map
            .get_mut(&node)
            .is_some_and(|c| c.sub.send(msg).is_ok())
    }

    /// Cancel a child gracefully.
    pub fn cancel(&mut self, node: NodeId, reason: &str) -> bool {
        let Some(c) = self.map.get_mut(&node) else {
            return false;
        };
        c.cancelled = true;
        c.sub
            .send(&ControlMsg::Cancel {
                reason: reason.to_string(),
            })
            .is_ok()
    }

    /// Kill a child now (its whole process group).
    /// Kill a child now. Its pid when it was still running: the kill reaps
    /// it, so the reaper never reports the exit and the caller settles it.
    pub fn kill(&mut self, node: NodeId) -> Option<i32> {
        let c = self.map.get_mut(&node)?;
        let pid = c.sub.pid();
        c.sub.kill().then_some(pid)
    }

    /// Periodic maintenance: pings + liveness. Returns the nodes that must be
    /// torn down (stuck / past deadline).
    pub fn tick(&mut self) -> Vec<(NodeId, Health)> {
        let now = Instant::now();
        if now.duration_since(self.last_ping) >= self.liveness_cfg.ping_interval {
            self.last_ping = now;
            self.ping_seq += 1;
            let seq = self.ping_seq;
            for c in self.map.values_mut() {
                let _ = c.sub.send(&ControlMsg::Ping { seq });
            }
        }
        self.map
            .iter()
            .filter_map(|(n, c)| {
                let h = c.liveness.classify(now);
                h.needs_teardown().then_some((*n, h))
            })
            .collect()
    }

    /// Stop `node` for the drain: `Cancel` it now and start its kill ladder
    /// (see [`Child::stop`]). Whether it was live and not already stopped.
    pub fn stop(&mut self, node: NodeId, reason: &str) -> bool {
        let Some(c) = self.map.get_mut(&node) else {
            return false;
        };
        if c.stop.is_some() {
            return false;
        }
        c.parked = !c.cancelled;
        c.cancelled = true;
        c.stop = Some(Ladder::with_defaults(Instant::now()));
        self.stopping += 1;
        let _ = c.sub.send(&ControlMsg::Cancel {
            reason: reason.to_string(),
        });
        true
    }

    /// Stop `node` for good, as `subagent.kill` does: `Cancel` it now and
    /// start its kill ladder, which the reactor climbs on every tick. Unlike
    /// the drain's [`stop`](Self::stop), nothing is parked: what a killed
    /// child owed is not left for the next life, even when the drain had
    /// stopped it first. A child already on a ladder keeps its clock, so a
    /// second kill does not put its signal off. Whether it was live.
    pub fn stop_for_good(&mut self, node: NodeId, reason: &str) -> bool {
        let Some(c) = self.map.get_mut(&node) else {
            return false;
        };
        c.parked = false;
        c.cancelled = true;
        if c.stop.is_none() {
            c.stop = Some(Ladder::with_defaults(Instant::now()));
            self.stopping += 1;
        }
        let _ = c.sub.send(&ControlMsg::Cancel {
            reason: reason.to_string(),
        });
        true
    }

    /// Climb each stopped child's ladder; `force` collapses every one to
    /// `SIGKILL`. A child let run is not touched. Returns each child a rung
    /// signalled, with the signal.
    pub fn drive_stops(&mut self, force: bool) -> Vec<(NodeId, &'static str)> {
        let mut signalled = Vec::new();
        if self.stopping == 0 {
            return signalled;
        }
        let now = Instant::now();
        for (node, c) in self.map.iter_mut() {
            let Some(ladder) = c.stop.as_mut() else {
                continue;
            };
            match ladder.poll(now, false, force) {
                LadderAction::Term => {
                    term_group(c.sub.pgid());
                    signalled.push((*node, "SIGTERM"));
                }
                LadderAction::Kill => {
                    kill_group(c.sub.pgid());
                    signalled.push((*node, "SIGKILL"));
                }
                LadderAction::Wait | LadderAction::Done => {}
            }
        }
        signalled
    }

    /// Kill and forget every remaining child: a drain past its abandon grace,
    /// or the process on its way out. Returns each one's node, kind, trace,
    /// whether its unit was settled and whether the drain parked it, so what
    /// it owed can still be left for the next life.
    pub fn abandon(
        &mut self,
    ) -> Vec<(
        NodeId,
        ChildKind,
        Option<crate::obs::trace::TraceContext>,
        bool,
        bool,
    )> {
        let gone = self
            .map
            .drain()
            .map(|(node, mut c)| {
                let _ = c.sub.kill();
                (node, c.kind, c.trace, c.settled, c.parked)
            })
            .collect();
        self.pid_to_node.clear();
        self.stopping = 0;
        gone
    }

    /// A status view.
    pub fn status(&self) -> Value {
        json!(
            self.map
                .iter()
                .map(|(n, c)| json!({"node": n.0, "pid": c.sub.pid(), "kind": kind_label(&c.kind), "age_ms": c.started.elapsed().as_millis() as u64, "tokens": c.tokens, "cancelled": c.cancelled}))
                .collect::<Vec<_>>()
        )
    }
}

pub fn kind_label(k: &ChildKind) -> String {
    match k {
        ChildKind::RootTurn { ctx, .. } => format!("turn:{ctx}"),
        ChildKind::StepTurn { run, step, .. } => format!("step:{run}/{step}"),
        ChildKind::Think { purpose, .. } => format!("think:{purpose}"),
        ChildKind::Subagent { handle } => format!("subagent:{handle}"),
    }
}
