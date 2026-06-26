//! Per-run budget: step / token / wall-clock bounds. RFC 0007 §budgets.
//!
//! This is the *per-run* budget enforced inside one agent loop (step / token /
//! wall-clock). The *hierarchical* tree-token accounting — each subagent's
//! usage rolled up to the tree root and bounded by a tree-wide ceiling — lives
//! separately in `tree.rs` (`charge_tokens`) and is driven by the reactor
//! (`KillReason::TreeBudget`); this type is the per-run primitive that records
//! usage and answers "is a bound hit?".

use crate::agentloop::stop::TerminalStatus;
use crate::wire::intel::Usage;
use std::time::Instant;

#[derive(Debug)]
pub struct Budget {
    max_steps: u32,
    max_tokens: u64,
    deadline: Instant,
    steps: u32,
    tokens: u64,
}

impl Budget {
    pub fn new(max_steps: u32, max_tokens: u64, deadline: Instant) -> Budget {
        Budget { max_steps, max_tokens, deadline, steps: 0, tokens: 0 }
    }

    /// Count one completed loop turn.
    pub fn record_step(&mut self) {
        self.steps += 1;
    }

    /// Add a model call's token usage to the running total.
    pub fn record_usage(&mut self, usage: Usage) {
        self.tokens = self.tokens.saturating_add(usage.total());
    }

    pub fn tokens(&self) -> u64 {
        self.tokens
    }
    pub fn steps(&self) -> u32 {
        self.steps
    }
    /// The configured step ceiling (for an informational `loop.start` field).
    pub fn max_steps(&self) -> u32 {
        self.max_steps
    }

    /// The terminal status for whichever bound is hit, if any. Checked at the
    /// top of each turn so the loop stops *before* spending more. Deadline is
    /// checked first (it's the hardest bound).
    pub fn exceeded(&self) -> Option<TerminalStatus> {
        if Instant::now() >= self.deadline {
            Some(TerminalStatus::Deadline)
        } else if self.steps >= self.max_steps {
            Some(TerminalStatus::ExhaustedSteps)
        } else if self.tokens >= self.max_tokens {
            Some(TerminalStatus::ExhaustedTokens)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn step_bound() {
        let mut b = Budget::new(2, 1000, far());
        assert!(b.exceeded().is_none());
        b.record_step();
        b.record_step();
        assert_eq!(b.exceeded(), Some(TerminalStatus::ExhaustedSteps));
    }

    #[test]
    fn token_bound() {
        let mut b = Budget::new(100, 50, far());
        b.record_usage(Usage { input_tokens: 40, output_tokens: 20 });
        assert_eq!(b.exceeded(), Some(TerminalStatus::ExhaustedTokens));
    }

    #[test]
    fn deadline_bound() {
        let b = Budget::new(100, 1000, Instant::now() - Duration::from_secs(1));
        assert_eq!(b.exceeded(), Some(TerminalStatus::Deadline));
    }
}
