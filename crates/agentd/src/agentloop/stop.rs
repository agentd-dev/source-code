// SPDX-License-Identifier: AGPL-3.0-only
//! Terminal statuses — the stop-condition disjunction.
//!
//! [`TerminalStatus`] is the single authority for *why* a run ended: a run
//! stops for exactly one of these reasons, and [`crate::exit`] maps each to an
//! exit code. "partial" is **not** a status — it is a property of the result
//! body, so a run can `complete` while still carrying a partial answer; see
//! [`Outcome`]. The two fatal-infra aborts (intelligence unreachable, a
//! required MCP server down) are *aborts* rather than variants here: they never
//! reach a terminal status and short-circuit to exit codes 4 / 6 directly.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// The model emitted a final answer. Success is judged against the tool
    /// results the run actually produced, never against the model's own claim
    /// that it succeeded.
    Completed,
    /// The agent concluded the task cannot/should not be done (semantic).
    Refused,
    /// Hit the per-run step cap.
    ExhaustedSteps,
    /// Hit the token budget (per-node or tree ceiling).
    ExhaustedTokens,
    /// Hit the loop's own wall-clock deadline.
    Deadline,
    /// Output content-hash unchanged for N turns (default 3) — spinning.
    Stalled,
    /// A single tool repeated past the per-tool cap K (default 3).
    LoopDetected,
    /// Cancelled by the supervisor (drain, parent cancel, route teardown).
    Cancelled,
    /// The subagent process crashed / was killed before a final.
    Crashed,
}

impl TerminalStatus {
    pub fn as_str(self) -> &'static str {
        use TerminalStatus::*;
        match self {
            Completed => "completed",
            Refused => "refused",
            ExhaustedSteps => "exhausted_steps",
            ExhaustedTokens => "exhausted_tokens",
            Deadline => "deadline",
            Stalled => "stalled",
            LoopDetected => "loop_detected",
            Cancelled => "cancelled",
            Crashed => "crashed",
        }
    }

    /// Did the run reach a clean, intended conclusion?
    pub fn is_success(self) -> bool {
        matches!(self, TerminalStatus::Completed)
    }

    /// Was the run cut short by a budget bound (steps/tokens/deadline)?
    pub fn is_budget(self) -> bool {
        matches!(
            self,
            TerminalStatus::ExhaustedSteps
                | TerminalStatus::ExhaustedTokens
                | TerminalStatus::Deadline
        )
    }
}

/// A future wake-up an agent requested for itself via the `schedule` self-tool.
/// The reactive daemon arms it relative to now and re-invokes the agent with
/// `instruction` when it fires — the agent setting its own next tick. Honoured
/// only under a long-lived daemon: a one-shot run exits before any "later"
/// could arrive, so the request is carried on the outcome but never armed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleRequest {
    /// Delay from the run's completion before the wake fires.
    pub after_ms: u64,
    /// The instruction the woken reaction runs.
    pub instruction: String,
}

/// A resource (un)subscription an agent requested for itself via the
/// `subscribe`/`unsubscribe`/`await_resource` self-tools. The reactive daemon
/// applies it to its live subscriptions + router after the run, so an agent can
/// widen or narrow what wakes it. Honoured only under a daemon. Not `Eq`,
/// because a `condition` may carry a JSON number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionRequest {
    pub uri: String,
    pub action: SubscriptionAction,
    /// Optional content predicate (raw self-tool args, e.g.
    /// `{"pointer":"/status","op":"eq","value":"ready"}`) for a conditional
    /// `await_resource` subscribe — the route fires only when the resource content
    /// satisfies it. Validated at tool-call time, then re-parsed into a
    /// content-predicate condition when the daemon arms the route. `None` means
    /// fire on any update (plain `subscribe`). Skipped in the wire form when
    /// absent, so an `unsubscribe` / plain `subscribe` carries no extra key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionAction {
    Subscribe,
    Unsubscribe,
}

/// A finished run: its terminal status, whether the result body is partial, the
/// distilled result value, and any self-requested future wake-ups / resource
/// (un)subscriptions. This is what a run hands back to its caller — a parent
/// supervisor reads the same shape for a child it spawned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: TerminalStatus,
    /// True when the agent produced *some* usable output but did not fully
    /// satisfy the objective (drives exit code 3 in one-shot mode).
    pub partial: bool,
    pub result: serde_json::Value,
    /// Future wake-ups the agent scheduled for itself. Empty unless the model
    /// called `schedule`; acted on only by a daemon supervisor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled: Vec<ScheduleRequest>,
    /// Resource (un)subscriptions the agent requested for itself. Empty unless
    /// the model called `subscribe`/`unsubscribe`; daemon-applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<SubscriptionRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        let s = serde_json::to_string(&TerminalStatus::ExhaustedSteps).unwrap();
        assert_eq!(s, "\"exhausted_steps\"");
    }

    #[test]
    fn budget_classification() {
        assert!(TerminalStatus::Deadline.is_budget());
        assert!(!TerminalStatus::Completed.is_budget());
        assert!(TerminalStatus::Completed.is_success());
    }
}
