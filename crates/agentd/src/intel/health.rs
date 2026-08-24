// SPDX-License-Identifier: AGPL-3.0-only
//! Per-endpoint health record and circuit breaker.
//!
//! Always compiled and dependency-free. The failover policy consults these
//! records to skip a dead endpoint and to snap back to the primary. All state is
//! plain integers and atomics — no histogram library, no SDK, no background
//! timer thread. The breaker is decided **synchronously** against the wall clock
//! at the moment an endpoint is consulted, so there is no async runtime and no
//! prober thread: an idle agent runs no code here at all, and a breaker can
//! never be reopened by a timer racing a live request.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Three-state circuit breaker. Stored as a `u8` in the health record so the
/// whole record stays lock-free and shareable behind `&self`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Normal — in rotation.
    Closed = 0,
    /// Removed from rotation for a cooldown after N consecutive failures.
    Open = 1,
    /// Eligible for exactly one probe; success re-closes, failure re-opens.
    HalfOpen = 2,
}

impl BreakerState {
    fn from_u8(v: u8) -> BreakerState {
        match v {
            1 => BreakerState::Open,
            2 => BreakerState::HalfOpen,
            _ => BreakerState::Closed,
        }
    }
    /// The wire spelling published in the `agentd://intelligence` resource body.
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half-open",
        }
    }
}

/// The last-observed failure class for an endpoint. A small bounded enum rather
/// than a message string, so the resource body and the emitted events can name
/// the failure without allocating and without ever echoing provider text into an
/// observable surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    None = 0,
    Refused = 1,
    Reset = 2,
    Timeout = 3,
    Http5xx = 4,
    Http429 = 5,
    Probe = 6,
}

impl ErrKind {
    fn from_u8(v: u8) -> ErrKind {
        match v {
            1 => ErrKind::Refused,
            2 => ErrKind::Reset,
            3 => ErrKind::Timeout,
            4 => ErrKind::Http5xx,
            5 => ErrKind::Http429,
            6 => ErrKind::Probe,
            _ => ErrKind::None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::None => "none",
            ErrKind::Refused => "refused",
            ErrKind::Reset => "reset",
            ErrKind::Timeout => "timeout",
            ErrKind::Http5xx => "5xx",
            ErrKind::Http429 => "429",
            ErrKind::Probe => "probe",
        }
    }
}

/// Breaker tuning. Every list is constructed with [`BreakerConfig::default`],
/// so these defaults are the operative values and the tuning is not currently
/// exposed to operators.
#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    /// Consecutive failover-class failures that open the breaker.
    pub open_threshold: u32,
    /// Initial cooldown after the breaker opens.
    pub cooldown: Duration,
    /// Cooldown cap (cooldown doubles each consecutive open up to this).
    pub cooldown_max: Duration,
}

impl Default for BreakerConfig {
    fn default() -> BreakerConfig {
        BreakerConfig {
            open_threshold: 3,
            cooldown: Duration::from_secs(5),
            cooldown_max: Duration::from_secs(60),
        }
    }
}

/// Per-endpoint health and breaker state. Every field is an atomic so the
/// record can be updated through a shared `&self` on the dial path without a
/// lock, which is what lets `EndpointList::iter` hand out `&Endpoint` while a
/// call in flight records its outcome.
#[derive(Debug)]
pub struct HealthRecord {
    state: AtomicU8,        // BreakerState
    consec_fail: AtomicU32, // resets to 0 on success
    total_calls: AtomicU64,
    total_fail: AtomicU64, // failover-class failures
    ewma_latency_us: AtomicU64,
    last_ok_unix_ms: AtomicU64,
    last_err_unix_ms: AtomicU64,
    last_err_kind: AtomicU8, // ErrKind
    opened_unix_ms: AtomicU64,
    /// How many times the breaker has consecutively opened (the cooldown
    /// backoff multiplier; reset on a re-close).
    open_count: AtomicU32,
}

impl Default for HealthRecord {
    fn default() -> HealthRecord {
        HealthRecord::new()
    }
}

impl HealthRecord {
    pub const fn new() -> HealthRecord {
        HealthRecord {
            state: AtomicU8::new(BreakerState::Closed as u8),
            consec_fail: AtomicU32::new(0),
            total_calls: AtomicU64::new(0),
            total_fail: AtomicU64::new(0),
            ewma_latency_us: AtomicU64::new(0),
            last_ok_unix_ms: AtomicU64::new(0),
            last_err_unix_ms: AtomicU64::new(0),
            last_err_kind: AtomicU8::new(ErrKind::None as u8),
            opened_unix_ms: AtomicU64::new(0),
            open_count: AtomicU32::new(0),
        }
    }

    pub fn state(&self) -> BreakerState {
        BreakerState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn consec_fail(&self) -> u32 {
        self.consec_fail.load(Ordering::Relaxed)
    }

    pub fn total_calls(&self) -> u64 {
        self.total_calls.load(Ordering::Relaxed)
    }

    pub fn total_fail(&self) -> u64 {
        self.total_fail.load(Ordering::Relaxed)
    }

    /// Process-lifetime error rate, `total_fail / total_calls`. It is a lifetime
    /// figure and never decays, so a long-lived agent's value lags a recent
    /// recovery; a windowed rate is the collector's job, derived from the
    /// scraped counters rather than kept here.
    pub fn error_rate(&self) -> f64 {
        let calls = self.total_calls();
        if calls == 0 {
            0.0
        } else {
            self.total_fail() as f64 / calls as f64
        }
    }

    pub fn ewma_latency_ms(&self) -> u64 {
        self.ewma_latency_us.load(Ordering::Relaxed) / 1000
    }

    pub fn last_err_kind(&self) -> ErrKind {
        ErrKind::from_u8(self.last_err_kind.load(Ordering::Relaxed))
    }

    pub fn last_ok_ms_ago(&self) -> Option<u64> {
        let t = self.last_ok_unix_ms.load(Ordering::Relaxed);
        if t == 0 {
            None
        } else {
            Some(now_unix_ms().saturating_sub(t))
        }
    }

    pub fn opened_ms_ago(&self) -> Option<u64> {
        let t = self.opened_unix_ms.load(Ordering::Relaxed);
        if t == 0 {
            None
        } else {
            Some(now_unix_ms().saturating_sub(t))
        }
    }

    /// The current cooldown for this endpoint: the initial cooldown doubled once
    /// per consecutive open, capped at `cooldown_max`. A repeatedly failing
    /// endpoint is therefore probed less and less often, while one that closes
    /// again resets to the initial cooldown.
    pub fn cooldown(&self, cfg: &BreakerConfig) -> Duration {
        let n = self.open_count.load(Ordering::Relaxed).saturating_sub(1);
        let shift = n.min(20); // avoid overflow; 2^20 already past the cap
        let scaled = cfg.cooldown.saturating_mul(1u32 << shift);
        scaled.min(cfg.cooldown_max)
    }

    /// True if this endpoint is currently usable, i.e. not OPEN and still
    /// cooling. Called by `attempt_order()`. **This is not a pure read**: an
    /// endpoint whose cooldown has elapsed is promoted to HALF-OPEN here, so the
    /// next call probes it. Use [`is_up`](Self::is_up) where a side-effect-free
    /// answer is required.
    pub fn available(&self, cfg: &BreakerConfig) -> bool {
        match self.state() {
            BreakerState::Closed | BreakerState::HalfOpen => true,
            BreakerState::Open => {
                // Promote to HALF-OPEN once the cooldown has elapsed. There is
                // no timer thread, so the consult itself is the promotion —
                // which also means an endpoint nobody consults never recovers,
                // and never needs to.
                if self.opened_ms_ago().unwrap_or(0) >= self.cooldown(cfg).as_millis() as u64 {
                    self.state
                        .store(BreakerState::HalfOpen as u8, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }
        }
    }

    /// True if the breaker is not OPEN, i.e. the endpoint is in rotation. This
    /// is the meaning of the `agentd_intel_endpoint_up` gauge. Unlike
    /// [`available`](Self::available) it is a pure read and never promotes a
    /// cooled-down breaker, so observing an endpoint cannot change its state.
    pub fn is_up(&self) -> bool {
        self.state() != BreakerState::Open
    }

    /// Record a successful round-trip: reset the consecutive-failure run,
    /// re-close the breaker, and fold the latency into the EWMA (alpha = 1/8).
    /// Returns the breaker transition if one happened, which the caller emits as
    /// an event and reflects in the served resource body; `None` means the
    /// breaker was already CLOSED and nothing is worth reporting.
    pub fn record_success(&self, latency: Duration) -> Option<BreakerTransition> {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        self.consec_fail.store(0, Ordering::Relaxed);
        self.last_ok_unix_ms.store(now_unix_ms(), Ordering::Relaxed);
        self.update_ewma(latency);
        let prev = self.state();
        if prev != BreakerState::Closed {
            self.state
                .store(BreakerState::Closed as u8, Ordering::Relaxed);
            self.open_count.store(0, Ordering::Relaxed);
            self.opened_unix_ms.store(0, Ordering::Relaxed);
            return Some(BreakerTransition::Closed);
        }
        None
    }

    /// Record a failover-class failure: bump the consecutive-failure run, stamp
    /// the error kind, and open the breaker if the run crossed the threshold or
    /// a HALF-OPEN probe failed. Only failover-class failures belong here — an
    /// auth or bad-request error is identical on every endpoint, so counting it
    /// would open breakers across a perfectly healthy list. Returns the breaker
    /// transition if one happened, for the caller to emit.
    pub fn record_failure(&self, kind: ErrKind, cfg: &BreakerConfig) -> Option<BreakerTransition> {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        self.total_fail.fetch_add(1, Ordering::Relaxed);
        let run = self.consec_fail.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_err_unix_ms
            .store(now_unix_ms(), Ordering::Relaxed);
        self.last_err_kind.store(kind as u8, Ordering::Relaxed);
        let prev = self.state();
        // A HALF-OPEN probe failure, or crossing the threshold from CLOSED,
        // opens the breaker and bumps the cooldown backoff. A single failed
        // probe is enough: the endpoint has already proved itself unhealthy.
        if prev == BreakerState::HalfOpen || run >= cfg.open_threshold {
            self.open_breaker();
            return Some(BreakerTransition::Opened);
        }
        None
    }

    fn open_breaker(&self) {
        self.state
            .store(BreakerState::Open as u8, Ordering::Relaxed);
        self.open_count.fetch_add(1, Ordering::Relaxed);
        self.opened_unix_ms.store(now_unix_ms(), Ordering::Relaxed);
    }

    fn update_ewma(&self, latency: Duration) {
        let sample = latency.as_micros() as u64;
        let prev = self.ewma_latency_us.load(Ordering::Relaxed);
        // EWMA alpha = 1/8: new = prev + (sample - prev)/8. Seed with the first
        // sample so a cold endpoint reports its real latency immediately.
        let next = if prev == 0 {
            sample
        } else if sample >= prev {
            prev + (sample - prev) / 8
        } else {
            prev - (prev - sample) / 8
        };
        self.ewma_latency_us.store(next, Ordering::Relaxed);
    }
}

/// A breaker state transition worth surfacing to an operator as an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerTransition {
    Opened,
    Closed,
}

/// Wall-clock now in unix milliseconds. Saturating, and `0` only on a pre-epoch
/// clock; every reader treats `0` as "unknown" rather than as a real timestamp,
/// so a broken clock degrades to no-information instead of a bogus age.
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_after_threshold_and_skips() {
        let cfg = BreakerConfig::default(); // threshold 3
        let h = HealthRecord::new();
        assert!(h.available(&cfg));
        assert_eq!(h.record_failure(ErrKind::Refused, &cfg), None);
        assert_eq!(h.record_failure(ErrKind::Refused, &cfg), None);
        // third consecutive failure opens
        assert_eq!(
            h.record_failure(ErrKind::Refused, &cfg),
            Some(BreakerTransition::Opened)
        );
        assert_eq!(h.state(), BreakerState::Open);
        assert!(!h.is_up());
        // a freshly-opened breaker is skipped (cooldown not elapsed)
        assert!(!h.available(&cfg));
    }

    #[test]
    fn breaker_half_opens_after_cooldown_then_closes_on_success() {
        // Tiny cooldown so the test doesn't sleep meaningfully.
        let cfg = BreakerConfig {
            open_threshold: 2,
            cooldown: Duration::from_millis(1),
            cooldown_max: Duration::from_millis(50),
        };
        let h = HealthRecord::new();
        h.record_failure(ErrKind::Timeout, &cfg);
        h.record_failure(ErrKind::Timeout, &cfg);
        assert_eq!(h.state(), BreakerState::Open);
        std::thread::sleep(Duration::from_millis(3));
        // consult promotes OPEN → HALF-OPEN once the cooldown elapsed
        assert!(h.available(&cfg));
        assert_eq!(h.state(), BreakerState::HalfOpen);
        // a successful probe closes it and resets the run
        assert_eq!(
            h.record_success(Duration::from_millis(10)),
            Some(BreakerTransition::Closed)
        );
        assert_eq!(h.state(), BreakerState::Closed);
        assert_eq!(h.consec_fail(), 0);
    }

    #[test]
    fn half_open_probe_failure_reopens_with_longer_cooldown() {
        let cfg = BreakerConfig {
            open_threshold: 1,
            cooldown: Duration::from_millis(1),
            cooldown_max: Duration::from_millis(1000),
        };
        let h = HealthRecord::new();
        // open #1
        h.record_failure(ErrKind::Refused, &cfg);
        let c1 = h.cooldown(&cfg);
        std::thread::sleep(Duration::from_millis(3));
        assert!(h.available(&cfg)); // → HALF-OPEN
        // probe fails → re-open, cooldown doubles
        assert_eq!(
            h.record_failure(ErrKind::Refused, &cfg),
            Some(BreakerTransition::Opened)
        );
        let c2 = h.cooldown(&cfg);
        assert!(c2 > c1, "cooldown backs off: {c1:?} -> {c2:?}");
    }

    #[test]
    fn ewma_tracks_latency_and_error_rate() {
        let h = HealthRecord::new();
        h.record_success(Duration::from_millis(40));
        assert_eq!(h.ewma_latency_ms(), 40);
        // a few successes keep it near 40ms
        h.record_success(Duration::from_millis(40));
        assert!((39..=41).contains(&h.ewma_latency_ms()));
        let cfg = BreakerConfig::default();
        h.record_failure(ErrKind::Http5xx, &cfg);
        // 1 failure / 3 calls
        assert!((h.error_rate() - 1.0 / 3.0).abs() < 1e-9);
    }
}
