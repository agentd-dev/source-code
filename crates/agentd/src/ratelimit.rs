//! Token-bucket rate limiter (R4).
//!
//! One bucket per protected unit (HTTP route today; node-level
//! retries in a later phase could borrow the same primitive). The
//! clock is abstracted so tests can advance time virtually without
//! sleeping.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Configuration deserialised from a workflow's route table:
///
/// ```toml
/// [[http_routes.rate_limit]]
/// capacity   = 10      # max burst size
/// per_second = 1.0     # sustained refill rate
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    pub capacity: u32,
    pub per_second: f32,
}

impl RateLimitConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.capacity == 0 {
            return Err("rate_limit.capacity must be > 0".into());
        }
        if !(self.per_second.is_finite() && self.per_second > 0.0) {
            return Err("rate_limit.per_second must be a positive number".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Clock abstraction
// ---------------------------------------------------------------------------

/// Abstract clock so tests can advance time without sleeping.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Virtual clock for tests. Cheap `Mutex<Instant>` inside — not
/// performance-sensitive.
#[derive(Debug)]
pub struct FakeClock {
    current: Mutex<Instant>,
}

impl Default for FakeClock {
    fn default() -> Self {
        Self {
            current: Mutex::new(Instant::now()),
        }
    }
}

impl FakeClock {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn advance(&self, d: Duration) {
        let mut guard = self.current.lock().unwrap();
        *guard += d;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.current.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Token bucket
// ---------------------------------------------------------------------------

/// Thread-safe token-bucket. `try_take()` is constant-time and
/// allocation-free.
pub struct TokenBucket<C: Clock = SystemClock> {
    capacity: f64,
    per_second: f64,
    state: Mutex<State>,
    clock: C,
}

struct State {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket<SystemClock> {
    pub fn new(cfg: &RateLimitConfig) -> Self {
        Self::new_with_clock(cfg, SystemClock)
    }
}

impl<C: Clock> TokenBucket<C> {
    pub fn new_with_clock(cfg: &RateLimitConfig, clock: C) -> Self {
        let now = clock.now();
        Self {
            capacity: cfg.capacity as f64,
            per_second: cfg.per_second as f64,
            state: Mutex::new(State {
                tokens: cfg.capacity as f64,
                last_refill: now,
            }),
            clock,
        }
    }

    /// Try to consume one token. Returns `Ok(())` if allowed,
    /// `Err(retry_after)` with a suggested wait duration otherwise.
    pub fn try_take(&self) -> Result<(), Duration> {
        let mut state = self.state.lock().expect("poisoned rate-limit state");
        let now = self.clock.now();
        self.refill(&mut state, now);
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            Ok(())
        } else {
            // Budget fraction missing × inverse rate = wait time.
            let deficit = 1.0 - state.tokens;
            let seconds = deficit / self.per_second;
            Err(Duration::from_secs_f64(seconds.max(0.001)))
        }
    }

    fn refill(&self, state: &mut State, now: Instant) {
        // Monotonic: ignore impossible backwards time.
        let elapsed = now.saturating_duration_since(state.last_refill);
        let added = elapsed.as_secs_f64() * self.per_second;
        state.tokens = (state.tokens + added).min(self.capacity);
        state.last_refill = now;
    }

    #[cfg(test)]
    pub fn tokens(&self) -> f64 {
        let mut state = self.state.lock().unwrap();
        let now = self.clock.now();
        self.refill(&mut state, now);
        state.tokens
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_bad_inputs() {
        assert!(
            RateLimitConfig {
                capacity: 0,
                per_second: 1.0
            }
            .validate()
            .is_err()
        );
        assert!(
            RateLimitConfig {
                capacity: 1,
                per_second: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(
            RateLimitConfig {
                capacity: 1,
                per_second: f32::NAN
            }
            .validate()
            .is_err()
        );
        assert!(
            RateLimitConfig {
                capacity: 1,
                per_second: -1.0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn starts_full() {
        let bucket = TokenBucket::new_with_clock(
            &RateLimitConfig {
                capacity: 3,
                per_second: 1.0,
            },
            FakeClock::new(),
        );
        assert!(bucket.try_take().is_ok());
        assert!(bucket.try_take().is_ok());
        assert!(bucket.try_take().is_ok());
    }

    #[test]
    fn denies_when_empty() {
        let clock = FakeClock::new();
        let bucket = TokenBucket::new_with_clock(
            &RateLimitConfig {
                capacity: 1,
                per_second: 1.0,
            },
            clock,
        );
        assert!(bucket.try_take().is_ok());
        let err = bucket.try_take().unwrap_err();
        assert!(err > Duration::ZERO);
        assert!(err <= Duration::from_secs(2));
    }

    #[test]
    fn refills_over_time() {
        let bucket = TokenBucket::new_with_clock(
            &RateLimitConfig {
                capacity: 5,
                per_second: 10.0,
            },
            FakeClock::new(),
        );
        // Drain.
        for _ in 0..5 {
            bucket.try_take().unwrap();
        }
        assert!(bucket.try_take().is_err());

        // 0.5s at 10 tok/s = +5 tokens (capped at capacity).
        bucket.clock.advance(Duration::from_millis(500));
        for _ in 0..5 {
            assert!(bucket.try_take().is_ok());
        }
    }

    #[test]
    fn caps_at_capacity() {
        let bucket = TokenBucket::new_with_clock(
            &RateLimitConfig {
                capacity: 3,
                per_second: 1.0,
            },
            FakeClock::new(),
        );
        // Advance way more than enough to overfill, ensure cap holds.
        bucket.clock.advance(Duration::from_secs(3600));
        assert!((bucket.tokens() - 3.0).abs() < 1e-6);
    }

    #[test]
    fn serde_round_trip() {
        let toml = r#"
            capacity = 10
            per_second = 2.5
        "#;
        let parsed: RateLimitConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.capacity, 10);
        assert_eq!(parsed.per_second, 2.5);
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(
            toml::from_str::<RateLimitConfig>(
                r#"capacity = 1
               per_second = 1
               extra = 5"#
            )
            .is_err()
        );
    }
}
