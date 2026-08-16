// SPDX-License-Identifier: AGPL-3.0-only
//! The runtime's **flat child tree** (RFC 0026 §2, D3): every turn worker and
//! subagent is a direct child of the supervisor, spawned through the 1.x
//! machinery (`supervisor::spawn` + the reaper + PDEATHSIG + process groups),
//! tracked here with its purpose, liveness and cancellation, and torn down
//! by the kill ladder on drain.

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
    /// A subagent (RFC 0009 payload) with a registry handle.
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
}

/// The children registry.
pub struct Children {
    exe: PathBuf,
    events: Sender<(NodeId, AgentMsg)>,
    reap_tx: Sender<Reaped>,
    map: HashMap<NodeId, Child>,
    pid_to_node: HashMap<i32, NodeId>,
    next: u64,
    liveness_cfg: LivenessConfig,
    ladder: Option<Ladder>,
    last_ping: Instant,
    ping_seq: u64,
}

impl Children {
    pub fn new(
        exe: PathBuf,
        events: Sender<(NodeId, AgentMsg)>,
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
            ladder: None,
            last_ping: Instant::now(),
            ping_seq: 0,
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

    /// Spawn a child (tracked with the reaper). The payload's `telemetry`
    /// must already carry the correlation ids.
    pub fn spawn(
        &mut self,
        payload: &SpawnPayload,
        kind: ChildKind,
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

    /// A child was reaped: forget it and return its record.
    pub fn on_reaped(&mut self, r: &Reaped) -> Option<(NodeId, Child)> {
        let node = self.pid_to_node.remove(&r.pid)?;
        let mut c = self.map.remove(&node)?;
        c.sub.mark_reaped();
        c.liveness.on_eof();
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

    /// Cancel a child gracefully (the kill ladder escalates on drain).
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
    pub fn kill(&mut self, node: NodeId) {
        if let Some(c) = self.map.get_mut(&node) {
            c.sub.kill();
        }
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

    /// Begin the drain: cancel every child; the ladder escalates.
    pub fn begin_drain(&mut self, reason: &str) {
        for c in self.map.values_mut() {
            c.cancelled = true;
            let _ = c.sub.send(&ControlMsg::Cancel {
                reason: reason.to_string(),
            });
        }
        if self.ladder.is_none() {
            self.ladder = Some(Ladder::with_defaults(Instant::now()));
        }
    }

    /// Drive the ladder: `true` when every child is gone.
    pub fn drive_drain(&mut self, force: bool) -> bool {
        let all_exited = self.map.is_empty();
        let Some(ladder) = self.ladder.as_mut() else {
            return all_exited;
        };
        match ladder.poll(Instant::now(), all_exited, force) {
            LadderAction::Wait => false,
            LadderAction::Term => {
                for c in self.map.values() {
                    term_group(c.sub.pgid());
                }
                false
            }
            LadderAction::Kill => {
                for c in self.map.values() {
                    kill_group(c.sub.pgid());
                }
                false
            }
            LadderAction::Done => true,
        }
    }

    /// Forget every remaining child (after a forced kill at abandon).
    pub fn abandon(&mut self) {
        for (_, mut c) in self.map.drain() {
            c.sub.kill();
        }
        self.pid_to_node.clear();
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
