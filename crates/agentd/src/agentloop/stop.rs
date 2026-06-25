//! Terminal statuses — the stop-condition disjunction.
//!
//! RFC 0007 §3.4 is the **single authority** for this enum; RFC 0011 §5.2
//! maps it to exit codes (see [`crate::exit`]). A run ends for exactly one of
//! these reasons. "partial" is **not** a status — it is a property of the
//! result body (a run can `complete` with a partial answer); see
//! [`Outcome`]. The two fatal-infra aborts (intelligence unreachable, a
//! required MCP server down) are *aborts*, not enum variants — they
//! short-circuit to exit codes 4 / 6 directly (RFC 0011 §5.2).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalStatus {
    /// The model emitted a final answer (verified against tool/exec results,
    /// never self-judgment — RFC 0007 §3.5).
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

/// A finished run: its terminal status, whether the result body is partial,
/// and the distilled result value. RFC 0007 / RFC 0009 §result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub status: TerminalStatus,
    /// True when the agent produced *some* usable output but did not fully
    /// satisfy the objective (drives exit code 3 in one-shot mode).
    pub partial: bool,
    pub result: serde_json::Value,
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
