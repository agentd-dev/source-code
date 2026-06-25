//! The public exit-code contract. RFC 0011 §5 — this is a stable,
//! machine-actionable API (e.g. for a Kubernetes `podFailurePolicy`); treat
//! changes as breaking.
//!
//! | Code | Meaning                                             | Scheduler hint |
//! |------|-----------------------------------------------------|----------------|
//! | 0    | success (one-shot completed / clean SIGTERM drain)  | Complete       |
//! | 1    | generic/unspecified failure                         | retriable      |
//! | 2    | config / usage error (validation)                   | non-retriable  |
//! | 3    | partial result                                      | policy         |
//! | 4    | intelligence unreachable / auth after retries       | retriable      |
//! | 5    | semantic — task cannot be done / refused            | non-retriable  |
//! | 6    | required MCP server failed to connect/handshake/die | retriable      |
//! | 7    | budget exceeded (steps/tokens/deadline/tree)        | policy         |
//! | 124  | hard wall-clock deadline (mnemonic to `timeout(1)`) | —              |
//! | 137  | killed by SIGKILL (128+9, OS-set) — often OOM       | raise memory   |
//! | 143  | killed by SIGTERM (128+15, OS-set) — ungraceful     | —              |
//!
//! A clean SIGTERM drain returns **0, not 143** (RFC 0011 §5.1). 137/143 are
//! set by the OS when the kernel kills us; we never `exit(137)` ourselves.

use crate::agentloop::stop::TerminalStatus;

pub const SUCCESS: i32 = 0;
pub const GENERIC: i32 = 1;
pub const USAGE: i32 = 2;
pub const PARTIAL: i32 = 3;
pub const INTEL_UNAVAILABLE: i32 = 4;
pub const REFUSED: i32 = 5;
pub const MCP_REQUIRED_DOWN: i32 = 6;
pub const BUDGET: i32 = 7;
pub const DEADLINE: i32 = 124;

/// Map a one-shot root subagent's outcome to an exit code (RFC 0011 §5.2).
/// `partial` is the result-body property, not a status: a `Completed` run
/// that only partially satisfied the objective exits `3`. A budget-bounded
/// run that nonetheless produced usable output is still reported under its
/// budget code (`7`) with the partial flag carried in the result JSON.
pub fn once_exit(status: TerminalStatus, partial: bool) -> i32 {
    use TerminalStatus::*;
    match status {
        Completed => {
            if partial {
                PARTIAL
            } else {
                SUCCESS
            }
        }
        Refused => REFUSED,
        ExhaustedSteps | ExhaustedTokens | Deadline => BUDGET,
        Stalled | LoopDetected => PARTIAL,
        Cancelled => GENERIC,
        Crashed => GENERIC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentloop::stop::TerminalStatus::*;

    #[test]
    fn mapping_matches_table() {
        assert_eq!(once_exit(Completed, false), SUCCESS);
        assert_eq!(once_exit(Completed, true), PARTIAL);
        assert_eq!(once_exit(Refused, false), REFUSED);
        assert_eq!(once_exit(ExhaustedSteps, false), BUDGET);
        assert_eq!(once_exit(Deadline, false), BUDGET);
        assert_eq!(once_exit(Stalled, false), PARTIAL);
    }
}
