// SPDX-License-Identifier: AGPL-3.0-only
//! Per-source and per-principal request limits.
//!
//! Two different questions, deliberately kept apart:
//!
//! * [`SourceLimiter`] counts FAILURES from one network source — a presented
//!   credential that did not authenticate, a role that was refused — and says
//!   when a source has failed too often. Only failures are counted, so what a
//!   web page can make a browser send — a refused origin, a request with no
//!   credential — never puts a source over. Once a source IS over, every
//!   request from it that presents a bearer is refused before the bearer is
//!   checked: a limiter that still checked each guess would slow nobody down,
//!   it would only change the status of a wrong guess from 401 to 429 while a
//!   right one sailed through. A request that presents nothing (the implicit
//!   operator, an `any` rule) is never refused by it.
//! * [`PrincipalRates`] is admission: one token per request from a named
//!   principal, from the rate its rule declares.
//! * [`DenialLog`] bounds what refusals cost the operator's log: a caller who
//!   is nobody pays nothing per request, so one log line per request would let
//!   anyone fill the disk at line rate.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::a2a::Principal;
use crate::supervisor::tree::{TokenBucket, parse_rate};

/// How many counted authentication failures a source may have in the window
/// before a further failing request is answered 429 instead.
pub const AUTH_FAILURE_BURST: u32 = 20;
/// How fast the failure count drains: one failure forgiven per interval.
pub const AUTH_FAILURE_REFILL: Duration = Duration::from_secs(3);
/// The most sources tracked at once; the least recently seen is dropped.
pub const SOURCE_ENTRIES: usize = 4096;
/// How often one source's refusals of one kind reach the log: the first in a
/// window is written, the rest are counted and reported with the next.
pub const DENIAL_LOG_WINDOW: Duration = Duration::from_secs(60);
/// Refusal lines across every source: at most this many at once…
pub const DENIAL_LOG_BURST: u32 = 20;
/// …and one more per interval after that, however many sources there are —
/// an IPv6 guesser has a /64 per address it controls, and a per-source window
/// alone would give each of them a line.
pub const DENIAL_LOG_REFILL: Duration = Duration::from_secs(1);

/// The unit a source is counted as: an IPv4 address, or an IPv6 /64 — one
/// host is routinely handed a whole /64, so counting single v6 addresses
/// would give every guesser 2^64 fresh budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKey {
    V4([u8; 4]),
    V6Prefix([u8; 8]),
}

/// The coarser unit a source belongs to: an IPv4 address still (one is
/// already scarce), an IPv6 /48 — the allocation a site, or one free tunnel
/// account, is routinely handed. The /64 sources under it are one party's to
/// spend, so a bound meant to hold against one party counts them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NetworkKey {
    V4([u8; 4]),
    V6Prefix([u8; 6]),
}

impl SourceKey {
    /// The network this source is part of.
    pub fn network(self) -> NetworkKey {
        match self {
            SourceKey::V4(o) => NetworkKey::V4(o),
            SourceKey::V6Prefix(p) => {
                let mut prefix = [0u8; 6];
                prefix.copy_from_slice(&p[..6]);
                NetworkKey::V6Prefix(prefix)
            }
        }
    }

    pub fn of(ip: IpAddr) -> SourceKey {
        match ip.to_canonical() {
            IpAddr::V4(v4) => SourceKey::V4(v4.octets()),
            IpAddr::V6(v6) => {
                let o = v6.octets();
                let mut prefix = [0u8; 8];
                prefix.copy_from_slice(&o[..8]);
                SourceKey::V6Prefix(prefix)
            }
        }
    }
}

/// A draining count of failures per source.
pub struct SourceLimiter {
    burst: f64,
    /// Failures forgiven per second.
    drain_per_sec: f64,
    capacity: usize,
    entries: Mutex<HashMap<SourceKey, Count>>,
}

#[derive(Debug, Clone, Copy)]
struct Count {
    level: f64,
    last: Instant,
}

impl Count {
    /// The level at `now`, having drained since `last`.
    fn at(&self, now: Instant, drain_per_sec: f64) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.level - elapsed * drain_per_sec).max(0.0)
    }
}

impl SourceLimiter {
    /// A limiter that answers "over" once a source has more than `burst`
    /// counted failures, forgiving one every `refill`.
    pub fn new(burst: u32, refill: Duration, capacity: usize) -> SourceLimiter {
        SourceLimiter {
            burst: f64::from(burst),
            drain_per_sec: 1.0 / refill.as_secs_f64().max(f64::MIN_POSITIVE),
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The listener's authentication-failure limiter.
    pub fn auth_failures() -> SourceLimiter {
        SourceLimiter::new(AUTH_FAILURE_BURST, AUTH_FAILURE_REFILL, SOURCE_ENTRIES)
    }

    /// `Some(retry_after_seconds)` when `source` has failed more than the
    /// burst allows. Asking counts nothing.
    pub fn over(&self, source: IpAddr) -> Option<u64> {
        self.over_at(source, Instant::now())
    }

    /// Count one failure against `source`.
    pub fn failed(&self, source: IpAddr) {
        self.failed_at(source, Instant::now())
    }

    pub(crate) fn over_at(&self, source: IpAddr, now: Instant) -> Option<u64> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let level = entries
            .get(&SourceKey::of(source))?
            .at(now, self.drain_per_sec);
        (level > self.burst)
            .then(|| ((level - self.burst) / self.drain_per_sec).ceil().max(1.0) as u64)
    }

    pub(crate) fn failed_at(&self, source: IpAddr, now: Instant) {
        let key = SourceKey::of(source);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            // Full: forget the source seen least recently. A scan, but only
            // when a NEW source arrives at a full table, over a bounded map.
            if let Some(oldest) = entries.iter().min_by_key(|(_, c)| c.last).map(|(k, _)| *k) {
                entries.remove(&oldest);
            }
        }
        let drain = self.drain_per_sec;
        let count = entries.entry(key).or_insert(Count {
            level: 0.0,
            last: now,
        });
        count.level = count.at(now, drain) + 1.0;
        count.last = now;
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

/// Which refusals are written to the log.
///
/// A refusal of a caller who is nobody — no credential, a bad one, a source
/// already over its failure limit — costs that caller nothing, so a line per
/// refusal is a line per request anyone can send. Each (source, reason) pair
/// is written once per [`DENIAL_LOG_WINDOW`], and the next line it gets says
/// how many were left out; a global drain caps the lines across all sources.
pub struct DenialLog {
    window: Duration,
    capacity: usize,
    burst: f64,
    drain_per_sec: f64,
    state: Mutex<DenialState>,
}

#[derive(Default)]
struct DenialState {
    seen: HashMap<(SourceKey, &'static str), Seen>,
    /// The global line budget, spent one per written line.
    spent: Option<Count>,
}

#[derive(Debug, Clone, Copy)]
struct Seen {
    /// When this pair last got a line; `None` while it never has.
    logged: Option<Instant>,
    /// When this pair was last refused, for eviction.
    last: Instant,
    /// Refusals since its last line that got none.
    suppressed: u64,
}

impl DenialLog {
    pub fn new(window: Duration, burst: u32, refill: Duration, capacity: usize) -> DenialLog {
        DenialLog {
            window,
            capacity: capacity.max(1),
            burst: f64::from(burst),
            drain_per_sec: 1.0 / refill.as_secs_f64().max(f64::MIN_POSITIVE),
            state: Mutex::new(DenialState::default()),
        }
    }

    /// The listener's refusal log.
    pub fn listener() -> DenialLog {
        DenialLog::new(
            DENIAL_LOG_WINDOW,
            DENIAL_LOG_BURST,
            DENIAL_LOG_REFILL,
            SOURCE_ENTRIES,
        )
    }

    /// `Some(suppressed)` when this refusal of `source` for `reason` should
    /// be written — `suppressed` being how many of the same were left out
    /// since the last line — and `None` when it is one of those.
    pub fn admit(&self, source: IpAddr, reason: &'static str) -> Option<u64> {
        self.admit_at(source, reason, Instant::now())
    }

    pub(crate) fn admit_at(
        &self,
        source: IpAddr,
        reason: &'static str,
        now: Instant,
    ) -> Option<u64> {
        let key = (SourceKey::of(source), reason);
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !st.seen.contains_key(&key)
            && st.seen.len() >= self.capacity
            && let Some(oldest) = st.seen.iter().min_by_key(|(_, s)| s.last).map(|(k, _)| *k)
        {
            st.seen.remove(&oldest);
        }
        let window = self.window;
        let seen = st.seen.entry(key).or_insert(Seen {
            logged: None,
            last: now,
            suppressed: 0,
        });
        seen.last = now;
        let due = seen
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= window);
        let seen = *seen;
        let budget = st.spent.map_or(0.0, |c| c.at(now, self.drain_per_sec));
        if due && budget + 1.0 <= self.burst {
            st.spent = Some(Count {
                level: budget + 1.0,
                last: now,
            });
            let s = st.seen.get_mut(&key).expect("inserted above");
            s.logged = Some(now);
            s.suppressed = 0;
            return Some(seen.suppressed);
        }
        if let Some(s) = st.seen.get_mut(&key) {
            s.suppressed += 1;
        }
        None
    }
}

/// Per-principal admission, from the rate each principal's rule declares.
///
/// Keyed by the principal id AND the spec, so a reload that changes a rule's
/// rate starts that principal on the new rate instead of draining the old
/// bucket; every session sharing a principal id shares its one bucket.
#[derive(Default)]
pub struct PrincipalRates {
    buckets: Mutex<HashMap<(String, String), TokenBucket>>,
}

impl PrincipalRates {
    /// Spend one token for `p`, or `Err(retry_after_seconds)` when it has
    /// none. Operators are exempt: locking the person who administers the
    /// daemon out of it during an incident is worse than the load they could
    /// generate. A principal with no rate is not limited.
    pub fn admit(&self, p: &Principal) -> Result<(), u64> {
        if p.is_operator() {
            return Ok(());
        }
        let Some(spec) = p.rate.as_deref() else {
            return Ok(());
        };
        // Validation parsed every declared rate at load; one that still does
        // not parse limits nothing rather than refusing everyone.
        let Ok((burst, per_sec)) = parse_rate(spec) else {
            return Ok(());
        };
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = buckets
            .entry((p.id.clone(), spec.to_string()))
            .or_insert_with(|| TokenBucket::new(burst, f64::from(burst) / per_sec));
        if bucket.try_take() {
            return Ok(());
        }
        // One token's worth of the window: the longest a caller waits for
        // the next one.
        Err((per_sec / f64::from(burst)).ceil().max(1.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::Role;

    #[test]
    fn a_source_is_over_only_after_more_than_the_burst_and_drains() {
        let l = SourceLimiter::new(20, Duration::from_secs(3), 16);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let t0 = Instant::now();
        assert_eq!(l.over_at(ip, t0), None, "an unseen source is not over");
        for _ in 0..20 {
            l.failed_at(ip, t0);
        }
        assert_eq!(
            l.over_at(ip, t0),
            None,
            "twenty failures are within the burst"
        );
        l.failed_at(ip, t0);
        let wait = l.over_at(ip, t0).expect("the twenty-first puts it over");
        assert!(wait >= 1, "{wait}");
        // Asking never counts.
        for _ in 0..100 {
            let _ = l.over_at(ip, t0);
        }
        assert_eq!(
            l.over_at(ip, t0 + Duration::from_secs(3)),
            None,
            "one drained"
        );
    }

    #[test]
    fn ipv6_counts_by_slash_64_and_v4_mapped_is_v4() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(SourceKey::of(a), SourceKey::of(b));
        assert_ne!(SourceKey::of(a), SourceKey::of(c));
        let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
        let v4: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(SourceKey::of(mapped), SourceKey::of(v4));
    }

    #[test]
    fn a_network_is_a_v6_slash_48_or_one_v4_address() {
        let net = |ip: &str| SourceKey::of(ip.parse().unwrap()).network();
        assert_eq!(net("2001:db8:1:2::1"), net("2001:db8:1:ff::1"));
        assert_ne!(net("2001:db8:1:2::1"), net("2001:db8:2:2::1"));
        assert_ne!(net("10.0.0.1"), net("10.0.0.2"));
    }

    #[test]
    fn a_full_table_evicts_the_least_recently_seen() {
        let l = SourceLimiter::new(1, Duration::from_secs(3600), 2);
        let t0 = Instant::now();
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        l.failed_at(ip("10.0.0.1"), t0);
        l.failed_at(ip("10.0.0.1"), t0);
        l.failed_at(ip("10.0.0.2"), t0 + Duration::from_secs(1));
        l.failed_at(ip("10.0.0.3"), t0 + Duration::from_secs(2));
        assert_eq!(l.len(), 2);
        assert_eq!(
            l.over_at(ip("10.0.0.1"), t0 + Duration::from_secs(2)),
            None,
            "the oldest source was forgotten"
        );
    }

    /// One line per source and reason per window, carrying the count of
    /// what it left out; a new window writes again; another reason or source
    /// has its own line.
    #[test]
    fn a_refusal_flood_writes_one_line_per_source_and_window() {
        let log = DenialLog::new(Duration::from_secs(60), 100, Duration::from_secs(1), 16);
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let t0 = Instant::now();
        assert_eq!(log.admit_at(ip, "unauthenticated", t0), Some(0));
        for _ in 0..500 {
            assert_eq!(log.admit_at(ip, "unauthenticated", t0), None);
        }
        assert_eq!(
            log.admit_at(ip, "invalid_credential", t0),
            Some(0),
            "another reason has its own line"
        );
        let other: IpAddr = "10.0.0.9".parse().unwrap();
        assert_eq!(log.admit_at(other, "unauthenticated", t0), Some(0));
        assert_eq!(
            log.admit_at(ip, "unauthenticated", t0 + Duration::from_secs(60)),
            Some(500),
            "the next window's line reports what the last one left out"
        );
        assert_eq!(
            log.admit_at(ip, "unauthenticated", t0 + Duration::from_secs(61)),
            None
        );
    }

    /// However many sources a flood comes from, the lines across all of them
    /// are bounded by the global drain.
    #[test]
    fn many_sources_share_one_line_budget() {
        let log = DenialLog::new(Duration::from_secs(60), 5, Duration::from_secs(1), 4096);
        let t0 = Instant::now();
        let written = (0..1000u32)
            .filter(|n| {
                let ip = IpAddr::from([10, 0, (n >> 8) as u8, *n as u8]);
                log.admit_at(ip, "unauthenticated", t0).is_some()
            })
            .count();
        assert_eq!(written, 5, "the burst, and no more");
        let ip = IpAddr::from([10, 9, 9, 9]);
        assert_eq!(
            log.admit_at(ip, "unauthenticated", t0 + Duration::from_secs(1)),
            Some(0),
            "the budget refills"
        );
        // A source left out for want of budget is still due: its line comes
        // with the next budget, carrying the refusals it missed.
        let starved = IpAddr::from([10, 0, 0, 7]);
        assert_eq!(
            log.admit_at(starved, "unauthenticated", t0 + Duration::from_secs(2)),
            Some(1)
        );
    }

    #[test]
    fn rates_exempt_operators_and_key_by_id() {
        let rates = PrincipalRates::default();
        let user = Principal {
            id: "user:ci".into(),
            role: Role::User,
            rate: Some("2/60s".into()),
            ..Principal::anonymous()
        };
        assert!(rates.admit(&user).is_ok() && rates.admit(&user).is_ok());
        assert_eq!(rates.admit(&user), Err(30));
        let other = Principal {
            id: "user:other".into(),
            ..user.clone()
        };
        assert!(rates.admit(&other).is_ok(), "another id has its own bucket");
        let op = Principal {
            id: "operator".into(),
            role: Role::Operator,
            ..user.clone()
        };
        for _ in 0..10 {
            assert!(rates.admit(&op).is_ok(), "operators are exempt");
        }
    }
}
