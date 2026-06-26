//! Restart governor — backoff + jitter + circuit breaker + crash-on-spawn.
//! RFC 0003 §3.7; assessment §2.8 / §4 M2 ("a crash-looping child trips the
//! breaker and is marked failed").
//!
//! The *temporal* control over restarts of a loop/reactive (or
//! session-backing) child — complementary to the *structural* fork-bomb caps
//! at the spawn chokepoint (RFC 0009). A one-shot root is **never** governed:
//! one-shot means one attempt (§3.7). This type is the pure decision core; the
//! reactor owns the clock, the spawn, and the wakeup timer (§3.2).
//!
//! Three behaviours, per §3.7:
//!   (a) **Exponential backoff + capped jitter** — respawn delay grows
//!       `BASE · 2^consecutive`, clamped to `CAP`, plus deterministic-RNG-free
//!       jitter in `0..=delay/4` (nanos & mask, no `rand`).
//!   (b) **Circuit breaker** — more than `BREAKER_THRESHOLD` failures inside a
//!       sliding `window` (or that many consecutive) opens the breaker: stop
//!       respawning, the session is failed (§3.7 surfaces it as a self-MCP
//!       resource — out of scope here).
//!   (c) **Crash-on-spawn fast-fail** — a run that dies faster than
//!       `SPAWN_READY` (before it could emit `ctrl/ready`, §3.6) is the
//!       fork-bomb early warning; it counts `SPAWN_FAIL_WEIGHT`× heavier,
//!       accelerating the breaker.
//!
//! Success (clean exit 0 + a received `final`, §3.7) is **not** a failure: it
//! resets the consecutive count and closes the breaker. Only the caller knows
//! the exit was clean; it passes `success` in.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Base backoff before the first respawn (§3.7 `RESTART_BASE`).
const RESTART_BASE: Duration = Duration::from_millis(500);
/// Backoff ceiling — exponential growth is clamped here (§3.7 `RESTART_CAP`).
const RESTART_CAP: Duration = Duration::from_secs(30);
/// Sliding-window length for the breaker's failure count (§3.7 `RESTART_WINDOW`).
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Weighted failures within `window` — or consecutive — that open the breaker
/// (§3.7 `BREAKER_THRESHOLD`). Conservative default; overridable per-handle via
/// `RestartConfig::threshold`.
const BREAKER_THRESHOLD: u32 = 5;
/// A run shorter than this never reached `ctrl/ready` (§3.6) — crash-on-spawn.
const SPAWN_READY: Duration = Duration::from_secs(2);
/// Crash-on-spawn counts this many failures (§3.7 `SPAWN_FAIL_WEIGHT`) — the
/// fork-bomb early warning trips the breaker faster.
const SPAWN_FAIL_WEIGHT: u32 = 3;

/// Tunables for one governed handle. `Default` is the RFC 0003 §3.7 profile;
/// tests construct narrower configs to exercise edges deterministically.
#[derive(Debug, Clone)]
pub struct RestartConfig {
    /// First-step backoff; doubles per consecutive failure up to `cap`.
    pub base: Duration,
    /// Maximum backoff before jitter.
    pub cap: Duration,
    /// Sliding window over which failures are counted for the breaker.
    pub window: Duration,
    /// Weighted failures within `window` (or consecutive) that trip the breaker.
    pub threshold: u32,
    /// A run shorter than this is a crash-on-spawn.
    pub spawn_ready: Duration,
    /// Failure weight charged to a crash-on-spawn.
    pub spawn_fail_weight: u32,
}

impl Default for RestartConfig {
    fn default() -> RestartConfig {
        RestartConfig {
            base: RESTART_BASE,
            cap: RESTART_CAP,
            window: RESTART_WINDOW,
            threshold: BREAKER_THRESHOLD,
            spawn_ready: SPAWN_READY,
            spawn_fail_weight: SPAWN_FAIL_WEIGHT,
        }
    }
}

/// What the supervisor should do after a governed child's run ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAction {
    /// Respawn after sleeping this long (backoff + jitter). The reactor arms a
    /// wakeup at this delay (§3.2) rather than busy-waiting.
    Backoff(Duration),
    /// The breaker is open: stop respawning, mark the session failed (§3.7).
    Tripped,
}

/// Per-handle restart state + decision logic. Pure: no clock of its own, no
/// I/O — `now` and the run's `ran_for` are supplied by the caller, so the
/// whole thing is unit-testable without sleeping.
#[derive(Debug)]
pub struct RestartGovernor {
    cfg: RestartConfig,
    /// Consecutive (unweighted) failures since the last success — drives the
    /// exponential backoff exponent.
    consecutive: u32,
    /// Weighted failure timestamps inside the window — drives the breaker. A
    /// crash-on-spawn pushes `spawn_fail_weight` copies (RFC 0003 weighting).
    failures: VecDeque<Instant>,
    /// Latched once the breaker opens; only a success (or `reset`) clears it.
    open: bool,
}

impl Default for RestartGovernor {
    fn default() -> RestartGovernor {
        RestartGovernor::new(RestartConfig::default())
    }
}

impl RestartGovernor {
    pub fn new(cfg: RestartConfig) -> RestartGovernor {
        RestartGovernor {
            cfg,
            consecutive: 0,
            failures: VecDeque::new(),
            open: false,
        }
    }

    /// Whether the breaker is currently open (session is failed). Lets the
    /// router drop events for a known-bad session (§3.7) without re-running
    /// `on_outcome`.
    pub fn is_tripped(&self) -> bool {
        self.open
    }

    /// Record one finished run and decide what to do next.
    ///
    /// `success` = clean exit 0 + a received `final` (§3.7); anything else
    /// (non-zero exit, signal death, stuck-kill) is a failure. `ran_for` is how
    /// long the run lived — a run shorter than `spawn_ready` is a crash-on-spawn
    /// and is weighted heavier. `now` is the caller's clock.
    ///
    /// On success: reset and close the breaker. On failure: grow the backoff,
    /// and if the windowed weighted failure count (or the consecutive count)
    /// reaches the threshold, trip.
    pub fn on_outcome(&mut self, success: bool, ran_for: Duration, now: Instant) -> RestartAction {
        if success {
            self.reset();
            return RestartAction::Backoff(self.cfg.base);
        }

        // A spawn that died before it could signal readiness is the fork-bomb
        // early warning (§3.7): charge it `spawn_fail_weight` failures.
        let crash_on_spawn = ran_for < self.cfg.spawn_ready;
        let weight = if crash_on_spawn {
            self.cfg.spawn_fail_weight
        } else {
            1
        };

        self.consecutive = self.consecutive.saturating_add(1);
        self.prune(now);
        for _ in 0..weight {
            self.failures.push_back(now);
        }

        // Breaker: open on weighted-in-window OR consecutive crossing threshold.
        // `>=` because a weight-3 burst can already meet the threshold — the
        // accelerated path the crash-loop test exercises.
        if self.windowed() >= self.cfg.threshold || self.consecutive >= self.cfg.threshold {
            self.open = true;
            return RestartAction::Tripped;
        }

        RestartAction::Backoff(self.backoff(now))
    }

    /// Clear all failure state (called on success, §3.7).
    pub fn reset(&mut self) {
        self.consecutive = 0;
        self.failures.clear();
        self.open = false;
    }

    /// Exponential backoff for the current consecutive count, capped, plus
    /// jitter in `0..=delay/4`. `consecutive` is `>= 1` here (we just counted a
    /// failure); the exponent is `consecutive - 1` so the first retry waits
    /// `base`.
    fn backoff(&self, now: Instant) -> Duration {
        // Clamp the shift so `1 << exp` cannot overflow; with the cap at 30s the
        // delay saturates long before exp reaches the clamp anyway.
        let exp = self.consecutive.saturating_sub(1).min(16);
        let grown = self.cfg.base.saturating_mul(1u32 << exp);
        let delay = grown.min(self.cfg.cap);
        delay + jitter_up_to(delay / 4, now)
    }

    /// Drop failure timestamps older than the window (sliding-window prune).
    fn prune(&mut self, now: Instant) {
        let cutoff = now.checked_sub(self.cfg.window);
        while let Some(&front) = self.failures.front() {
            match cutoff {
                Some(c) if front < c => {
                    self.failures.pop_front();
                }
                _ => break,
            }
        }
    }

    /// Weighted failure count currently inside the window.
    fn windowed(&self) -> u32 {
        self.failures.len() as u32
    }
}

/// A uniform-ish jitter in `0..=max`, with **no** `rand` crate: mix the wall
/// clock's nanoseconds (the only entropy we have) the way `obs/trace::
/// new_span_id` does, then reduce modulo `max + 1`. `now` (an `Instant`)
/// carries no extractable epoch, so the entropy is drawn from `SystemTime` —
/// its absolute value is irrelevant, only the low bits' churn matters.
/// Guarantees `0 <= jitter <= max`.
fn jitter_up_to(max: Duration, _now: Instant) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // FNV-1a-style mix (house style, obs/trace) so successive same-nanosecond
    // calls still spread; the process id breaks cross-process correlation.
    let seed = 0xcbf2_9ce4_8422_2325u64 ^ (std::process::id() as u64);
    let mixed = mix(nanos, seed);
    let span = (max.as_nanos() as u64).saturating_add(1); // inclusive upper bound
    Duration::from_nanos(mixed % span)
}

/// FNV-1a over `value`'s little-endian bytes, seeded — the dependency-free
/// hash the rest of the tree uses for ids (obs/trace::fnv1a).
fn mix(value: u64, seed: u64) -> u64 {
    let mut h = seed;
    for &b in &value.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with tight thresholds for deterministic edges. Jitter is
    /// bounded by `delay/4`, so backoff growth is asserted against
    /// `[delay, delay + delay/4]`.
    fn cfg() -> RestartConfig {
        RestartConfig {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
            window: Duration::from_secs(60),
            threshold: 5,
            spawn_ready: Duration::from_secs(2),
            spawn_fail_weight: 3,
        }
    }

    /// A run length past `spawn_ready` — avoids the crash-on-spawn path.
    fn healthy() -> Duration {
        Duration::from_secs(10)
    }

    fn backoff_of(a: RestartAction) -> Duration {
        match a {
            RestartAction::Backoff(d) => d,
            RestartAction::Tripped => panic!("expected Backoff, got Tripped"),
        }
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        let mut g = RestartGovernor::new(cfg());
        let t = Instant::now();
        // base 500ms, jitter <= delay/4. First failure → ~500ms.
        let d1 = backoff_of(g.on_outcome(false, healthy(), t));
        assert!(
            d1 >= Duration::from_millis(500) && d1 <= Duration::from_millis(625),
            "d1={d1:?}"
        );
        // Second → ~1s.
        let d2 = backoff_of(g.on_outcome(false, healthy(), t));
        assert!(
            d2 >= Duration::from_secs(1) && d2 <= Duration::from_millis(1250),
            "d2={d2:?}"
        );
        // Third → ~2s.
        let d3 = backoff_of(g.on_outcome(false, healthy(), t));
        assert!(
            d3 >= Duration::from_secs(2) && d3 <= Duration::from_millis(2500),
            "d3={d3:?}"
        );
        // Fourth → ~4s (still below the 5-failure breaker threshold).
        let d4 = backoff_of(g.on_outcome(false, healthy(), t));
        assert!(
            d4 >= Duration::from_secs(4) && d4 <= Duration::from_secs(5),
            "d4={d4:?}"
        );
    }

    #[test]
    fn backoff_caps_at_configured_ceiling() {
        // A high threshold so the breaker never trips while we watch the cap.
        let mut c = cfg();
        c.threshold = 1000;
        c.cap = Duration::from_secs(30);
        let mut g = RestartGovernor::new(c);
        let t = Instant::now();
        let mut last = Duration::ZERO;
        for _ in 0..40 {
            last = backoff_of(g.on_outcome(false, healthy(), t));
        }
        // After many doublings the delay is pinned to cap (+ up to cap/4 jitter).
        assert!(
            last >= Duration::from_secs(30),
            "should reach cap: {last:?}"
        );
        assert!(
            last <= Duration::from_secs(30) + Duration::from_secs(30) / 4,
            "over cap+jitter: {last:?}"
        );
    }

    #[test]
    fn breaker_trips_after_consecutive_threshold() {
        let mut g = RestartGovernor::new(cfg());
        let t = Instant::now();
        // 4 healthy-length failures stay Backoff; the 5th trips (consecutive==5).
        for i in 0..4 {
            assert!(
                matches!(g.on_outcome(false, healthy(), t), RestartAction::Backoff(_)),
                "i={i}"
            );
        }
        assert_eq!(g.on_outcome(false, healthy(), t), RestartAction::Tripped);
        assert!(g.is_tripped());
    }

    #[test]
    fn success_resets_and_closes_breaker() {
        let mut g = RestartGovernor::new(cfg());
        let t = Instant::now();
        for _ in 0..4 {
            g.on_outcome(false, healthy(), t);
        }
        // A clean run wipes the slate before the breaker would have tripped.
        assert!(matches!(
            g.on_outcome(true, healthy(), t),
            RestartAction::Backoff(_)
        ));
        assert!(!g.is_tripped());
        // And the next failure starts from base again (consecutive reset).
        let d = backoff_of(g.on_outcome(false, healthy(), t));
        assert!(
            d <= Duration::from_millis(625),
            "should restart from base: {d:?}"
        );
    }

    #[test]
    fn crash_on_spawn_accelerates_the_breaker() {
        let mut g = RestartGovernor::new(cfg());
        let t = Instant::now();
        // spawn_fail_weight=3, threshold=5: one fast crash charges 3, a second
        // charges 3 more → windowed weight 6 >= 5, so the breaker trips on the
        // SECOND crash-on-spawn — far faster than 5 healthy-length failures.
        let fast = Duration::from_millis(50); // < spawn_ready (2s)
        assert!(matches!(
            g.on_outcome(false, fast, t),
            RestartAction::Backoff(_)
        ));
        assert_eq!(g.on_outcome(false, fast, t), RestartAction::Tripped);
    }

    #[test]
    fn sliding_window_drops_stale_failures() {
        let mut c = cfg();
        c.window = Duration::from_secs(60);
        let mut g = RestartGovernor::new(c);
        let base = Instant::now();
        // 4 failures, each 100s apart — every prune drops the prior one, so the
        // windowed weight is always 1 and the windowed path never trips.
        for k in 0..4u32 {
            let t = base + Duration::from_secs(100 * k as u64);
            g.on_outcome(false, healthy(), t);
            assert_eq!(g.windowed(), 1, "stale failures should be pruned (k={k})");
        }
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let now = Instant::now();
        // Zero ceiling → exactly zero jitter (no panic, no overflow).
        assert_eq!(jitter_up_to(Duration::ZERO, now), Duration::ZERO);
        // For a range of ceilings, every draw lands in [0, max].
        for ms in [1u64, 7, 250, 1000, 7500] {
            let max = Duration::from_millis(ms);
            for _ in 0..1000 {
                let j = jitter_up_to(max, now);
                assert!(j <= max, "jitter {j:?} exceeded max {max:?}");
            }
        }
    }

    #[test]
    fn tripped_is_sticky_until_reset() {
        let mut g = RestartGovernor::new(cfg());
        let t = Instant::now();
        for _ in 0..5 {
            g.on_outcome(false, healthy(), t);
        }
        assert!(g.is_tripped());
        // Still tripped on a further failure.
        assert_eq!(g.on_outcome(false, healthy(), t), RestartAction::Tripped);
        // Only an explicit reset (or success) clears it.
        g.reset();
        assert!(!g.is_tripped());
    }
}
