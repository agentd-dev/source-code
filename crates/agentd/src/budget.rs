// SPDX-License-Identifier: Apache-2.0
//! RFC 0025 — the per-instance **lifetime token budget**: a process-global,
//! cumulative cap on LLM tokens across every run/reaction an agent *instance*
//! performs. Distinct from the per-run `--max-tokens` box (which the re-exec'd
//! child loop enforces): this bounds the whole long-lived instance so a
//! `reactive`/`loop` daemon on a metering-free path (agentctl RFC 0024 direct
//! dial, no gateway ledger) stays bounded.
//!
//! The counter lives in the **supervisor** process, fed where every child's
//! token usage converges ([`crate::supervisor`]'s reactor `AgentMsg::Usage`
//! handler — the same point that feeds `agent_tokens_total`). Enforcement
//! differs by mode, matching the RFC:
//! - a **bounded run** folds `min(max_tokens, cap)` into its per-run budget, so
//!   the existing `ExhaustedTokens → EXIT_BUDGET(7)` path carries it unchanged;
//! - a **reactive/loop** instance consults [`exhausted`] before accepting the
//!   next reaction, then drains cleanly (RFC 0025 §3.1 preferred) — or exits per
//!   the `--budget-exit-code` knob.
//!
//! Threshold-crossing is observable *before* exhaustion: the gauge
//! `agent_budget_tokens_remaining` tracks the balance continuously, and the
//! first charge past 90% returns [`Crossed::Threshold`] for a one-shot event.
//! Dependency-free, always compiled; inert (no cap) unless installed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The installed lifetime ledger — set once per process. Absent = unbounded
/// (the default), so every accessor is a cheap `None` and the feature is inert.
static LEDGER: OnceLock<Ledger> = OnceLock::new();

/// What a [`charge`] pushed the cumulative total across, for the caller to log
/// and meter (the reactor emits the matching `limit.*` event + metric).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Crossed {
    /// No boundary crossed by this charge.
    None,
    /// The cumulative total first crossed the pre-exhaustion threshold (90% of
    /// the cap) — fired at most once, the alerting/scaling hook.
    Threshold,
    /// The cumulative total first reached/exceeded the cap.
    Exhausted,
}

struct Ledger {
    cap: u64,
    spent: AtomicU64,
    /// One-shot latch so the pre-exhaustion threshold event fires at most once.
    threshold_fired: AtomicBool,
}

impl Ledger {
    fn new(cap: u64) -> Ledger {
        Ledger {
            cap,
            spent: AtomicU64::new(0),
            threshold_fired: AtomicBool::new(false),
        }
    }

    /// Add `tokens` to the cumulative total; report the boundary (if any) this
    /// charge crossed. `Exhausted` wins over `Threshold` on a single big charge.
    fn charge(&self, tokens: u64) -> Crossed {
        let prev = self.spent.fetch_add(tokens, Ordering::SeqCst);
        let now = prev.saturating_add(tokens);
        if prev < self.cap && now >= self.cap {
            // Also latch the threshold so a later (impossible) re-arm can't refire.
            self.threshold_fired.store(true, Ordering::SeqCst);
            return Crossed::Exhausted;
        }
        let threshold = self.cap.saturating_mul(9) / 10;
        if now >= threshold && now < self.cap && !self.threshold_fired.swap(true, Ordering::SeqCst)
        {
            return Crossed::Threshold;
        }
        Crossed::None
    }

    fn spent(&self) -> u64 {
        self.spent.load(Ordering::SeqCst)
    }

    fn remaining(&self) -> u64 {
        self.cap.saturating_sub(self.spent())
    }
}

/// Install the process lifetime budget (RFC 0025). `cap == 0` leaves the feature
/// inert (unbounded — today's behaviour); a positive cap arms the ledger and
/// seeds the `agent_budget_tokens_remaining` gauge. Idempotent: a second install
/// is ignored (the first cap wins), so a re-exec'd child that re-installs is safe.
pub fn install(cap: u64) {
    if cap > 0 && LEDGER.set(Ledger::new(cap)).is_ok() {
        crate::obs::metrics::set_budget_tokens_remaining(cap);
    }
}

/// Charge `tokens` against the lifetime budget and update the remaining-balance
/// gauge. No-op (`Crossed::None`) when no budget is installed. Called at the
/// single supervisor-side point where all child token usage converges.
pub fn charge(tokens: u64) -> Crossed {
    let Some(l) = LEDGER.get() else {
        return Crossed::None;
    };
    let crossed = l.charge(tokens);
    crate::obs::metrics::set_budget_tokens_remaining(l.remaining());
    crossed
}

/// Whether the lifetime budget is installed AND exhausted (cumulative ≥ cap).
/// The reactive/loop daemon consults this before accepting the next reaction.
pub fn exhausted() -> bool {
    LEDGER.get().is_some_and(|l| l.spent() >= l.cap)
}

/// The installed cap, if any (for folding `min(max_tokens, cap)` into a bounded
/// run's per-run budget).
pub fn cap() -> Option<u64> {
    LEDGER.get().map(|l| l.cap)
}

/// Tokens remaining before exhaustion, if a budget is installed.
pub fn remaining() -> Option<u64> {
    LEDGER.get().map(|l| l.remaining())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_crosses_threshold_once_then_exhausts() {
        let l = Ledger::new(1000);
        // Below threshold: no crossing.
        assert_eq!(l.charge(500), Crossed::None);
        assert_eq!(l.remaining(), 500);
        // First charge past 90% (900) fires Threshold exactly once.
        assert_eq!(l.charge(450), Crossed::Threshold); // now 950
        assert_eq!(l.charge(30), Crossed::None); // still in [900,1000), no refire
        // Crossing the cap reports Exhausted.
        assert_eq!(l.charge(100), Crossed::Exhausted); // now 1080 ≥ 1000
        assert_eq!(l.remaining(), 0);
        assert!(l.spent() >= l.cap);
    }

    #[test]
    fn a_single_big_charge_reports_exhausted_not_threshold() {
        let l = Ledger::new(1000);
        assert_eq!(l.charge(5000), Crossed::Exhausted);
        assert_eq!(l.remaining(), 0);
    }

    #[test]
    fn uninstalled_ledger_is_inert() {
        // No install() in this test → the global stays empty and every accessor
        // is the unbounded default. (Other tests must not install, per the
        // process-global OnceLock; this asserts the inert contract.)
        assert_eq!(charge(1_000_000), Crossed::None);
        assert!(!exhausted());
        assert_eq!(cap(), None);
        assert_eq!(remaining(), None);
    }
}
