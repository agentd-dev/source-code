//! Dead/stuck detection — the three-detector model + the EOF×pong classifier.
//! RFC 0003 §dead-stuck.
//!
//! This is the *pure* heart of supervision: given timestamps and flags, decide
//! whether a child is healthy, legitimately busy, stuck-alive, or dead. The
//! supervisor feeds it events (`on_event`/`on_pong`/`on_eof`) and asks
//! `classify(now)` on each reactor tick; the kill ladder (`kill.rs`) acts on a
//! teardown verdict. No processes or signals here — those are `spawn.rs`,
//! `reap.rs`, `kill.rs`.
//!
//! The three detectors:
//! - **A — hard deadline** (always on, no child cooperation).
//! - **B — no-progress watchdog**: substantive events (loop.step, tool.call,
//!   usage) stamp `last_event_at`; silence past `progress_timeout` is suspicious.
//! - **C — ping/pong**: pongs (answered by the child's *control thread*, which
//!   is separate from its agentic loop) stamp `last_pong_at`. Pongs continuing
//!   while events have stopped means "busy in a long legitimate tool call";
//!   pongs *also* stopping means the process is wedged.

use std::time::{Duration, Instant};

/// A child's liveness verdict on a given tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Substantive events are flowing — making progress.
    Healthy,
    /// No recent events, but pongs still arrive — a long legitimate tool/model
    /// call. Leave it alone.
    Busy,
    /// No events *and* no pongs past their timeouts — wedged. Tear down.
    Stuck,
    /// The control channel hit EOF — the child likely exited. Confirm via
    /// `waitpid` (`reap.rs`), then remove.
    Dead,
    /// The hard wall-clock deadline passed. Tear down (RFC 0011 → exit 124).
    DeadlineExceeded,
}

impl Health {
    /// Does this verdict require the kill ladder to run?
    pub fn needs_teardown(self) -> bool {
        matches!(self, Health::Stuck | Health::Dead | Health::DeadlineExceeded)
    }
}

/// Sensible default timeouts. `progress_timeout` is generous because a single
/// tool/model call can legitimately take a while; `pong_timeout` is tight
/// because the control thread answers a ping immediately regardless of what
/// the loop is doing.
#[derive(Debug, Clone, Copy)]
pub struct LivenessConfig {
    pub progress_timeout: Duration,
    pub pong_timeout: Duration,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        LivenessConfig {
            progress_timeout: Duration::from_secs(120),
            pong_timeout: Duration::from_secs(10),
        }
    }
}

/// Per-child liveness tracker. Construct at spawn with the child's absolute
/// deadline; feed it events as they arrive; `classify(now)` each tick.
#[derive(Debug)]
pub struct Liveness {
    deadline: Instant,
    cfg: LivenessConfig,
    last_event_at: Instant,
    last_pong_at: Instant,
    eof: bool,
}

impl Liveness {
    pub fn new(now: Instant, deadline: Instant, cfg: LivenessConfig) -> Liveness {
        Liveness { deadline, cfg, last_event_at: now, last_pong_at: now, eof: false }
    }

    /// A substantive progress frame arrived (Event/Usage/Result). Also counts
    /// as liveness, so it refreshes the pong clock too.
    pub fn on_event(&mut self, now: Instant) {
        self.last_event_at = now;
        self.last_pong_at = now;
    }

    /// A `Pong` arrived (liveness only — not progress).
    pub fn on_pong(&mut self, now: Instant) {
        self.last_pong_at = now;
    }

    /// The control channel closed.
    pub fn on_eof(&mut self) {
        self.eof = true;
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The 2×2 classifier (RFC 0003 §2.8). Order matters: EOF and the hard
    /// deadline dominate; otherwise recent events = Healthy, else recent pongs
    /// = Busy, else Stuck.
    pub fn classify(&self, now: Instant) -> Health {
        if self.eof {
            return Health::Dead;
        }
        if now >= self.deadline {
            return Health::DeadlineExceeded;
        }
        if now.duration_since(self.last_event_at) <= self.cfg.progress_timeout {
            Health::Healthy
        } else if now.duration_since(self.last_pong_at) <= self.cfg.pong_timeout {
            Health::Busy
        } else {
            Health::Stuck
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LivenessConfig {
        LivenessConfig { progress_timeout: Duration::from_secs(100), pong_timeout: Duration::from_secs(10) }
    }

    #[test]
    fn recent_events_are_healthy() {
        let t0 = Instant::now();
        let l = Liveness::new(t0, t0 + Duration::from_secs(3600), cfg());
        assert_eq!(l.classify(t0 + Duration::from_secs(50)), Health::Healthy);
    }

    #[test]
    fn no_events_but_pongs_is_busy() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, t0 + Duration::from_secs(3600), cfg());
        // 150s with no events (> progress_timeout) but a pong at 145s.
        l.on_pong(t0 + Duration::from_secs(145));
        assert_eq!(l.classify(t0 + Duration::from_secs(150)), Health::Busy);
    }

    #[test]
    fn no_events_no_pongs_is_stuck() {
        let t0 = Instant::now();
        let l = Liveness::new(t0, t0 + Duration::from_secs(3600), cfg());
        // 200s of silence: past both progress (100s) and pong (10s) timeouts.
        assert_eq!(l.classify(t0 + Duration::from_secs(200)), Health::Stuck);
        assert!(l.classify(t0 + Duration::from_secs(200)).needs_teardown());
    }

    #[test]
    fn eof_is_dead_and_dominates() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, t0 + Duration::from_secs(3600), cfg());
        l.on_event(t0 + Duration::from_secs(1)); // even with recent progress...
        l.on_eof();
        assert_eq!(l.classify(t0 + Duration::from_secs(2)), Health::Dead); // ...EOF wins
    }

    #[test]
    fn deadline_exceeded() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, t0 + Duration::from_secs(60), cfg());
        l.on_event(t0 + Duration::from_secs(59)); // busy right up to the wire
        assert_eq!(l.classify(t0 + Duration::from_secs(61)), Health::DeadlineExceeded);
    }

    #[test]
    fn on_event_refreshes_both_clocks() {
        let t0 = Instant::now();
        let mut l = Liveness::new(t0, t0 + Duration::from_secs(3600), cfg());
        l.on_event(t0 + Duration::from_secs(300));
        // right after an event → healthy again
        assert_eq!(l.classify(t0 + Duration::from_secs(301)), Health::Healthy);
    }
}
