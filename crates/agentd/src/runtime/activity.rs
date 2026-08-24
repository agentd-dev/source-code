// SPDX-License-Identifier: AGPL-3.0-only
//! **Live activity**: what each working unit is doing *right now*, for the
//! display clients' working row.
//!
//! The turn worker reports coarse progress upward as [`AgentMsg::Event`]
//! frames — `turn.think`, `turn.round`, `turn.tool`. Here they fold into a
//! per-unit [`Activity`] record — phase, current tool, round, tokens so far,
//! start time — and publish as `activity` feed events.
//!
//! Deliberately **coarse**: an event is emitted only when something the
//! operator would notice CHANGES (phase, tool, round). Elapsed time is not
//! streamed — the record carries `started_ms` and clients tick their own
//! clock — so a long think emits nothing at all. That keeps the feed's replay
//! ring meaningful: a handful of activity events per turn rather than one per
//! second, so a client that reconnects can still see the whole turn in the
//! ring instead of a second's worth of noise.

use super::children::ChildKind;
use super::reactor::Runtime;
use crate::state::now_ms;
use crate::supervisor::tree::NodeId;
use serde_json::{Value, json};

/// What a unit is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// A model call is in flight.
    Thinking,
    /// A tool is executing.
    Tool,
    /// Parked on a deferred wait (timer, subagent, human gate).
    Waiting,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Thinking => "thinking",
            Phase::Tool => "tool",
            Phase::Waiting => "waiting",
        }
    }
}

/// One working unit's live activity.
#[derive(Debug, Clone)]
pub struct Activity {
    /// The A2A task this unit answers (the client keys on it), when it has one.
    pub task: Option<String>,
    /// The conversation, when the unit is a turn.
    pub ctx: Option<String>,
    pub phase: Phase,
    /// The tool executing right now (phase `tool`).
    pub tool: Option<String>,
    /// The model round this unit is on (1-based).
    pub round: u32,
    /// Tokens this unit has spent so far.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// When the unit started (clients tick elapsed from this).
    pub started_ms: u64,
    pub updated_ms: u64,
}

impl Activity {
    fn new(task: Option<String>, ctx: Option<String>) -> Activity {
        let now = now_ms();
        Activity {
            task,
            ctx,
            phase: Phase::Thinking,
            tool: None,
            round: 0,
            tokens_in: 0,
            tokens_out: 0,
            started_ms: now,
            updated_ms: now,
        }
    }

    /// Fold one progress frame in. Returns `true` when the change is worth
    /// telling clients about (phase / tool / round moved) — token-only and
    /// clock-only updates are silent.
    pub fn apply(&mut self, event: &str, fields: &Value) -> bool {
        let before = (self.phase, self.tool.clone(), self.round);
        match event {
            "turn.think" => {
                self.phase = Phase::Thinking;
                self.tool = None;
                if let Some(r) = fields["round"].as_u64() {
                    self.round = r as u32;
                }
            }
            "turn.round" => {
                if let Some(r) = fields["round"].as_u64() {
                    self.round = r as u32;
                }
                self.tokens_in += fields["tokens_in"].as_u64().unwrap_or(0);
                self.tokens_out += fields["tokens_out"].as_u64().unwrap_or(0);
                // The model answered; tools (if any) announce themselves next.
                self.phase = Phase::Thinking;
                self.tool = None;
            }
            "turn.tool" => {
                self.phase = Phase::Tool;
                self.tool = fields["tool"].as_str().map(str::to_string);
            }
            _ => return false,
        }
        self.updated_ms = now_ms();
        (self.phase, self.tool.clone(), self.round) != before
    }

    /// Park the unit (a deferred tool: sleep, subagent, human gate).
    pub fn park(&mut self, what: &str) -> bool {
        let changed = self.phase != Phase::Waiting || self.tool.as_deref() != Some(what);
        self.phase = Phase::Waiting;
        self.tool = Some(what.to_string());
        self.updated_ms = now_ms();
        changed
    }

    pub fn to_value(&self) -> Value {
        json!({
            "task": self.task,
            "ctx": self.ctx,
            "phase": self.phase.as_str(),
            "tool": self.tool,
            "round": self.round,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
            "started_ms": self.started_ms,
            "updated_ms": self.updated_ms,
        })
    }
}

impl Runtime {
    /// A child's progress frame (`AgentMsg::Event`) — fold it into the unit's
    /// activity and publish the change.
    pub(crate) fn on_child_progress(&mut self, node: NodeId, event: &str, fields: &Value) {
        let (task, ctx) = self.unit_of(node);
        let entry = self
            .activity
            .entry(node.0)
            .or_insert_with(|| Activity::new(task.clone(), ctx.clone()));
        // A restored/late binding (the task id is minted after the spawn).
        if entry.task.is_none() && task.is_some() {
            entry.task = task;
        }
        if entry.apply(event, fields) {
            let v = entry.to_value();
            self.publish_activity(node, v);
        }
    }

    /// Mark the unit parked on a deferred wait.
    pub(crate) fn activity_park(&mut self, node: NodeId, what: &str) {
        let Some(entry) = self.activity.get_mut(&node.0) else {
            return;
        };
        if entry.park(what) {
            let v = entry.to_value();
            self.publish_activity(node, v);
        }
    }

    /// The unit finished: drop the record and tell clients it is gone.
    pub(crate) fn activity_end(&mut self, node: NodeId) {
        if self.activity.remove(&node.0).is_some() {
            #[cfg(feature = "a2a")]
            self.feed_push(
                "activity.removed",
                crate::runtime::a2a_server::FeedVis::Operator,
                json!({"id": node.0.to_string()}),
            );
        }
    }

    /// The A2A task + conversation a child answers, when it has one. Without
    /// the `a2a` feature there are no tasks to bind to — the record still
    /// tracks the unit's phase for `status`.
    fn unit_of(&self, node: NodeId) -> (Option<String>, Option<String>) {
        match self.children.get(node).map(|c| c.kind.clone()) {
            Some(ChildKind::RootTurn { ctx, event, .. }) => {
                #[cfg(feature = "a2a")]
                let task = event.and_then(|e| self.event_to_task.get(&e).cloned());
                #[cfg(not(feature = "a2a"))]
                let task = {
                    let _ = event;
                    None
                };
                (task, Some(ctx))
            }
            Some(ChildKind::StepTurn { run, .. }) => {
                let task = self.runs.get(&run).and_then(|r| r.task.clone());
                (
                    task,
                    self.runs.get(&run).and_then(|r| r.conversation.clone()),
                )
            }
            _ => (None, None),
        }
    }

    /// Publish to the interface feed — a no-op without the `a2a` feature (no
    /// feed exists to publish to; `status.activity` still carries the record).
    #[allow(unused_mut, unused_variables)]
    fn publish_activity(&self, node: NodeId, mut v: Value) {
        #[cfg(feature = "a2a")]
        {
            v["id"] = json!(node.0.to_string());
            // Owner-scoped when the unit answers a task; else operator-only.
            let owner = v["task"]
                .as_str()
                .and_then(|t| self.tasks.get(t))
                .and_then(|t| t.principal.clone());
            let vis = match owner {
                Some(p) => crate::runtime::a2a_server::FeedVis::Owner(Some(p)),
                None => crate::runtime::a2a_server::FeedVis::Operator,
            };
            self.feed_push("activity", vis, v);
        }
    }

    /// The live activity view for `status` (the poll-fallback path sees the
    /// same information the feed carries).
    pub(crate) fn activity_value(&self) -> Value {
        json!(
            self.activity
                .iter()
                .map(|(node, a)| {
                    let mut v = a.to_value();
                    v["id"] = json!(node.to_string());
                    v
                })
                .collect::<Vec<_>>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_folds_progress_and_reports_only_notable_changes() {
        let mut a = Activity::new(Some("task-1".into()), Some("c1".into()));
        assert_eq!(a.phase, Phase::Thinking);
        // A think announcement on round 1: the round moved ⇒ notable.
        assert!(a.apply("turn.think", &json!({"round": 1})));
        assert_eq!(a.round, 1);
        // The same announcement again changes nothing ⇒ silent.
        assert!(!a.apply("turn.think", &json!({"round": 1})));
        // The round lands with usage: tokens accrue, phase/tool/round unchanged
        // ⇒ silent (tokens alone must not spam the feed).
        assert!(!a.apply(
            "turn.round",
            &json!({"round": 1, "tokens_in": 100, "tokens_out": 20})
        ));
        assert_eq!((a.tokens_in, a.tokens_out), (100, 20));
        // A tool starts ⇒ notable, and names itself.
        assert!(a.apply("turn.tool", &json!({"tool": "read_file"})));
        assert_eq!(a.phase, Phase::Tool);
        assert_eq!(a.tool.as_deref(), Some("read_file"));
        // A different tool ⇒ notable.
        assert!(a.apply("turn.tool", &json!({"tool": "memory.set"})));
        // Back to thinking on the next round ⇒ notable; tokens keep accruing.
        assert!(a.apply(
            "turn.round",
            &json!({"round": 2, "tokens_in": 50, "tokens_out": 10})
        ));
        assert_eq!(a.phase, Phase::Thinking);
        assert_eq!(a.tool, None);
        assert_eq!((a.tokens_in, a.tokens_out), (150, 30));
        // Parking is notable once.
        assert!(a.park("subagent"));
        assert!(!a.park("subagent"));
        assert_eq!(a.phase, Phase::Waiting);
        // Unknown events are ignored entirely.
        assert!(!a.apply("something.else", &json!({})));
        let v = a.to_value();
        assert_eq!(v["phase"], "waiting");
        assert_eq!(v["task"], "task-1");
        assert!(v["started_ms"].as_u64().is_some_and(|t| t > 0));
    }
}
