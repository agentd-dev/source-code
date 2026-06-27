//! The shard predicate — static partitioning of the URI/key space across a
//! fleet (RFC 0019 §4). [feature = "cluster"]
//!
//! An instance with shard `K` of `N` handles an item only if
//! `fnv1a64(shard_key(item)) % N == K`. The gate is applied at routing intake,
//! **before** any debounce/spawn (RFC 0019 §3.4/§4.1), so out-of-shard items are
//! dropped at near-zero cost. The hash MUST be **deterministic fleet-wide** —
//! stable across versions, languages, and architectures — so it is a hand-rolled
//! FNV-1a/64 (RFC 0019 §4.1): no `hashbrown`/`siphash`/`sha2` crate (the default
//! hasher is randomized, and those crates are deps).
//!
//! agentd does **not** discover its own shard — agentctl assigns `K/N` from the
//! StatefulSet ordinal (RFC 0019 §4.2); this module only reads, validates, and
//! applies it. Claim/lease (§3) and standby (§7) are DEFERRED (RFC 0019 §12), so
//! sharding here is the whole cross-instance-ownership story for this build: a
//! `K/N` fleet with no claim deterministically owns each item by one shard.

/// FNV-1a/64, hand-rolled exactly per RFC 0019 §4.1: offset basis
/// `0xcbf29ce484222325`, prime `0x00000100000001B3`, xor-then-multiply with
/// wrapping. Stable across versions/languages/architectures — the property a
/// shard hash needs (a fleet-wide deterministic partition).
///
/// `pub(crate)` so the work-claim key derivation (`cluster::claim`) reuses the
/// SAME hash (RFC 0019 §3.5 / RFC 0015 §5.6) — there is exactly one FNV in the
/// tree; a second would risk a fleet-wide divergence.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Timer-route shard behaviour (RFC 0019 §4.1, `AGENTD_SHARD_TIMER`). Timer
/// events carry no URI, so a sharded `schedule`/`loop` fleet needs a rule for
/// *which* replicas fire a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerShard {
    /// Only shard 0 fires (a single fleet-wide ticker) — the default, to avoid N
    /// replicas all firing the same cron tick.
    Shard0,
    /// Every replica fires; the per-tick key gate (sharding on the tick's target
    /// id) is applied elsewhere. The per-tick gate itself is DEFERRED, so this is
    /// a forward-compatible knob, not yet a live behaviour difference.
    Keyed,
}

impl TimerShard {
    /// Parse `AGENTD_SHARD_TIMER` — `shard0` (default) | `keyed`. `None` on an
    /// unknown value (the caller maps it to a `ConfigError::Usage`, exit 2).
    pub fn parse(s: &str) -> Option<TimerShard> {
        match s {
            "shard0" => Some(TimerShard::Shard0),
            "keyed" => Some(TimerShard::Keyed),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TimerShard::Shard0 => "shard0",
            TimerShard::Keyed => "keyed",
        }
    }
}

/// A shard identity: `K` of `N` (`--shard K/N`, `0 <= K < N`, `N >= 1`). The
/// `N == 1` (default, no `--shard`) case is a single logical shard whose
/// [`Shard::owns`] is always true — byte-for-byte RFC 0008 behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shard {
    pub k: u32,
    pub n: u32,
}

impl Default for Shard {
    /// The unsharded default: `0/1`, owns everything (RFC 0019 §4.1).
    fn default() -> Self {
        Shard { k: 0, n: 1 }
    }
}

impl Shard {
    /// Whether this shard owns `shard_key` (RFC 0019 §4.1). `N == 1` owns
    /// everything (the single-shard / unsharded case); otherwise the FNV-1a/64
    /// of the key modulo `N` must equal `K`.
    pub fn owns(&self, shard_key: &str) -> bool {
        self.n == 1 || (fnv1a64(shard_key.as_bytes()) % self.n as u64) == self.k as u64
    }

    /// Parse `--shard K/N` (`AGENTD_SHARD`). Rejects `N == 0`, `K >= N`, and any
    /// non-numeric / malformed form with an `Err(message)` the caller maps to a
    /// `ConfigError::Usage` (exit 2, before any side effect — RFC 0019 §4.4).
    pub fn parse(spec: &str) -> Result<Shard, String> {
        let (k_str, n_str) = spec
            .split_once('/')
            .ok_or_else(|| format!("--shard must be K/N (got: {spec})"))?;
        let k: u32 = k_str
            .trim()
            .parse()
            .map_err(|_| format!("--shard: invalid K '{k_str}' (want a number)"))?;
        let n: u32 = n_str
            .trim()
            .parse()
            .map_err(|_| format!("--shard: invalid N '{n_str}' (want a number)"))?;
        if n == 0 {
            return Err("--shard: N must be > 0".into());
        }
        if k >= n {
            return Err(format!("--shard: K must be < N (got {k}/{n})"));
        }
        Ok(Shard { k, n })
    }

    /// Whether this instance fires timer events for the given timer-shard mode
    /// (RFC 0019 §4.1). In `shard0` mode only `k == 0` (or `n == 1`) fires — a
    /// single fleet-wide ticker; in `keyed` mode every replica fires (the per-tick
    /// key gate is applied elsewhere / deferred). A non-sharded instance (`n == 1`)
    /// always fires regardless of the mode.
    pub fn fires_timers(&self, mode: TimerShard) -> bool {
        if self.n == 1 {
            return true;
        }
        match mode {
            TimerShard::Shard0 => self.k == 0,
            TimerShard::Keyed => true,
        }
    }

    /// The `"K/N"` identity string for the capabilities manifest / capacity
    /// resource (RFC 0019 §9). `None` for the unsharded `N == 1` case (the
    /// manifest reports `shard: null`).
    pub fn label(&self) -> Option<String> {
        if self.n == 1 {
            None
        } else {
            Some(format!("{}/{}", self.k, self.n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_known_vectors() {
        // The empty input hashes to the offset basis (xor/multiply never run) —
        // the canonical FNV-1a/64 anchor.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        // Canonical FNV-1a/64 test vectors (the published reference values) — a
        // pinned guard so the hash can never silently change fleet-wide.
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171_f73967e8);
        // A fixed URI pinned to its constant: if this value ever moves, every
        // deployed shard would re-partition — so it is frozen here on purpose.
        assert_eq!(fnv1a64(b"file:///inbox/42.json"), 0x5fae_e1ac_8a9c_5a03);
    }

    #[test]
    fn parse_valid_and_invalid() {
        assert_eq!(Shard::parse("3/8").unwrap(), Shard { k: 3, n: 8 });
        assert_eq!(Shard::parse("0/1").unwrap(), Shard { k: 0, n: 1 });
        assert_eq!(Shard::parse(" 2 / 5 ").unwrap(), Shard { k: 2, n: 5 });
        // N == 0, K >= N, non-numeric, malformed → Err (caller maps to exit 2).
        assert!(Shard::parse("0/0").is_err());
        assert!(Shard::parse("5/5").is_err());
        assert!(Shard::parse("8/3").is_err());
        assert!(Shard::parse("x/8").is_err());
        assert!(Shard::parse("3/y").is_err());
        assert!(Shard::parse("3").is_err());
        assert!(Shard::parse("").is_err());
    }

    #[test]
    fn n1_owns_everything() {
        let s = Shard { k: 0, n: 1 };
        assert!(s.owns("file:///a"));
        assert!(s.owns("anything-at-all"));
        assert!(s.owns(""));
        assert_eq!(Shard::default(), s);
    }

    #[test]
    fn owns_is_deterministic_for_a_key() {
        // Same key → same answer, every time (a partition must be stable).
        let s = Shard { k: 3, n: 8 };
        let key = "db://orders/42";
        let first = s.owns(key);
        for _ in 0..100 {
            assert_eq!(s.owns(key), first);
        }
    }

    #[test]
    fn each_uri_is_owned_by_exactly_one_shard() {
        // The partition is total + disjoint: every key is owned by exactly one of
        // the N shards (the modulo class), so a fleet sees no duplicate, no gap.
        let n = 8u32;
        for i in 0..1000u32 {
            let uri = format!("file:///inbox/{i}.json");
            let owners: Vec<u32> = (0..n).filter(|&k| Shard { k, n }.owns(&uri)).collect();
            assert_eq!(
                owners.len(),
                1,
                "uri {uri} owned by {owners:?}, want exactly one"
            );
        }
    }

    #[test]
    fn distribution_is_roughly_even() {
        // 1000 distinct URIs across N=8 → each shard gets a nonzero, roughly-even
        // share. FNV-1a spreads well, so the ideal is ~125/shard; assert a wide
        // [50,250] band so the test is a smoke check, not a brittle statistical one.
        let n = 8u32;
        let mut counts = [0usize; 8];
        for i in 0..1000u32 {
            let uri = format!("https://api.example/items/{i}");
            for k in 0..n {
                if (Shard { k, n }).owns(&uri) {
                    counts[k as usize] += 1;
                }
            }
        }
        for (k, &c) in counts.iter().enumerate() {
            assert!(
                (50..=250).contains(&c),
                "shard {k} got {c} items (want roughly-even ~125 in [50,250])"
            );
        }
        // Sanity: every item landed somewhere exactly once.
        assert_eq!(counts.iter().sum::<usize>(), 1000);
    }

    #[test]
    fn timer_shard_parse_and_fires() {
        assert_eq!(TimerShard::parse("shard0"), Some(TimerShard::Shard0));
        assert_eq!(TimerShard::parse("keyed"), Some(TimerShard::Keyed));
        assert_eq!(TimerShard::parse("nope"), None);

        // shard0: only k==0 of a real shard fires; an unsharded instance always does.
        assert!(Shard { k: 0, n: 4 }.fires_timers(TimerShard::Shard0));
        assert!(!Shard { k: 1, n: 4 }.fires_timers(TimerShard::Shard0));
        assert!(Shard { k: 0, n: 1 }.fires_timers(TimerShard::Shard0));
        // keyed: every replica fires (the per-key gate is applied elsewhere).
        assert!(Shard { k: 1, n: 4 }.fires_timers(TimerShard::Keyed));
    }

    #[test]
    fn label_is_none_when_unsharded() {
        assert_eq!(Shard { k: 0, n: 1 }.label(), None);
        assert_eq!(Shard { k: 3, n: 8 }.label(), Some("3/8".to_string()));
    }
}
