//! The bounded teardown ladder. RFC 0003 §kill-ladder.
//!
//! When a subtree must die (SIGTERM-to-agentd, a deadline/stuck verdict, or a
//! tree-budget breach), the reactor tears it down **deepest-first** (children
//! before parents, via `tree.deepest_first()`, so a parent can't spawn
//! replacements mid-teardown — the `draining` flag also blocks new spawns) and
//! escalates per the ladder: graceful `Cancel` → `killpg(SIGTERM)` after a
//! grace → `killpg(SIGKILL)` after a kill-grace → reap. A second SIGTERM/SIGINT
//! (`force`) collapses straight to SIGKILL. The whole budget is bounded and
//! must stay **< the orchestrator's `terminationGracePeriodSeconds`** (RFC
//! 0011) — the reactor enforces that ceiling.
//!
//! The `killpg` calls are thin libc; the **escalation timing** is the pure,
//! unit-tested [`Ladder`] state machine.

use std::time::{Duration, Instant};

/// Default grace before SIGTERM (let the child wind down at a turn boundary).
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);
/// Default grace between SIGTERM and SIGKILL.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(2);

/// What the reactor should do on this tick of a teardown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderAction {
    /// Nothing yet — keep waiting for children to exit.
    Wait,
    /// `killpg(SIGTERM)` every still-live target.
    Term,
    /// `killpg(SIGKILL)` every still-live target.
    Kill,
    /// Everything has exited (or been killed and reaped) — teardown complete.
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Phase {
    Cancel,
    Term,
    Kill,
    Done,
}

/// The escalation timer for one teardown. Construct it, send `Cancel` to the
/// targets, then call [`Ladder::poll`] each reactor tick; perform whatever
/// action it returns on the still-live set.
#[derive(Debug)]
pub struct Ladder {
    started: Instant,
    grace: Duration,
    kill_grace: Duration,
    phase: Phase,
}

impl Ladder {
    /// Begin a teardown at `now`. The caller should immediately send a
    /// graceful `Cancel` to every target (the ladder assumes that happened).
    pub fn new(now: Instant, grace: Duration, kill_grace: Duration) -> Ladder {
        Ladder { started: now, grace, kill_grace, phase: Phase::Cancel }
    }

    pub fn with_defaults(now: Instant) -> Ladder {
        Ladder::new(now, DEFAULT_GRACE, DEFAULT_KILL_GRACE)
    }

    /// Decide the next action. `all_exited` is whether every target has been
    /// reaped; `force` collapses straight to SIGKILL (second signal).
    pub fn poll(&mut self, now: Instant, all_exited: bool, force: bool) -> LadderAction {
        if all_exited {
            self.phase = Phase::Done;
            return LadderAction::Done;
        }
        if force && self.phase < Phase::Kill {
            self.phase = Phase::Kill;
            return LadderAction::Kill;
        }
        let elapsed = now.saturating_duration_since(self.started);
        match self.phase {
            Phase::Cancel if elapsed >= self.grace => {
                self.phase = Phase::Term;
                LadderAction::Term
            }
            Phase::Term if elapsed >= self.grace + self.kill_grace => {
                self.phase = Phase::Kill;
                LadderAction::Kill
            }
            Phase::Done => LadderAction::Done,
            // Still within a grace window, or already SIGKILL'd and waiting to
            // reap (a process that ignores SIGKILL is in uninterruptible sleep;
            // the reactor's overall budget bounds the wait and logs a leak).
            _ => LadderAction::Wait,
        }
    }
}

/// `killpg(pgid, sig)`. Guards `pgid > 1` so we never signal pgid 0 (our own
/// group) or 1 (init).
#[cfg(unix)]
pub fn signal_group(pgid: i32, sig: i32) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid, sig);
        }
    }
}

#[cfg(unix)]
pub fn term_group(pgid: i32) {
    signal_group(pgid, libc::SIGTERM);
}

#[cfg(unix)]
pub fn kill_group(pgid: i32) {
    signal_group(pgid, libc::SIGKILL);
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: i32, _sig: i32) {}
#[cfg(not(unix))]
pub fn term_group(_pgid: i32) {}
#[cfg(not(unix))]
pub fn kill_group(_pgid: i32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder(now: Instant) -> Ladder {
        Ladder::new(now, Duration::from_secs(5), Duration::from_secs(2))
    }

    #[test]
    fn waits_during_grace() {
        let t0 = Instant::now();
        let mut l = ladder(t0);
        assert_eq!(l.poll(t0 + Duration::from_secs(1), false, false), LadderAction::Wait);
    }

    #[test]
    fn escalates_term_then_kill() {
        let t0 = Instant::now();
        let mut l = ladder(t0);
        assert_eq!(l.poll(t0 + Duration::from_secs(5), false, false), LadderAction::Term);
        // still waiting between term and kill
        assert_eq!(l.poll(t0 + Duration::from_secs(6), false, false), LadderAction::Wait);
        // after grace + kill_grace = 7s
        assert_eq!(l.poll(t0 + Duration::from_secs(7), false, false), LadderAction::Kill);
    }

    #[test]
    fn all_exited_is_done() {
        let t0 = Instant::now();
        let mut l = ladder(t0);
        assert_eq!(l.poll(t0 + Duration::from_secs(1), true, false), LadderAction::Done);
    }

    #[test]
    fn force_collapses_to_kill() {
        let t0 = Instant::now();
        let mut l = ladder(t0);
        // even at t0, force jumps straight to SIGKILL
        assert_eq!(l.poll(t0, false, true), LadderAction::Kill);
        // and won't re-issue Kill on the next poll (waits to reap)
        assert_eq!(l.poll(t0 + Duration::from_millis(1), false, true), LadderAction::Wait);
    }

    #[test]
    fn done_after_kill_when_reaped() {
        let t0 = Instant::now();
        let mut l = ladder(t0);
        l.poll(t0, false, true); // Kill
        assert_eq!(l.poll(t0 + Duration::from_secs(1), true, false), LadderAction::Done);
    }
}
