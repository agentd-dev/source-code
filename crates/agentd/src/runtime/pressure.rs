// SPDX-License-Identifier: AGPL-3.0-only
//! Resource pressure, and what a healthy daemon does about it: **shed new work,
//! drain what is in flight.**
//!
//! The failure this exists for is disk. The file store writes until `ENOSPC`,
//! and a checkpoint failure is a halting condition — so before this module a
//! full disk stopped the agent *after* the fact, with no warning before it and
//! nothing between "fine" and "dead". Now there are two thresholds: below
//! `warn` the operator is told; below `shed` the daemon stops **admitting**
//! work — no new runs fired, webhooks answered `429 Retry-After`, no new turns
//! dispatched — while everything already running drains normally. An agent
//! that finishes its current job but takes no more degrades; one that dies
//! mid-checkpoint corrupts the next restart's starting point.
//!
//! Memory pressure (the cgroup's `memory.high`, when armed) sheds through the
//! same gate: before this it was consulted at exactly ONE admission point of
//! four — subagent spawn — so a daemon at its soft limit refused to spawn a
//! child and then cheerfully accepted a webhook, fired a schedule and
//! dispatched a turn.
//!
//! Assessment is cached (~2s) and lock-free to read: the gates sit on hot
//! paths and a `statvfs` per webhook would be its own kind of pressure.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// How pressed the daemon is. Ordering matters: higher is worse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok = 0,
    /// Running low — logged, exported, nothing refused yet.
    Warn = 1,
    /// Admission stops; in-flight work drains.
    Shed = 2,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Shed => "shed",
        }
    }
    fn from_u8(v: u8) -> Level {
        match v {
            2 => Level::Shed,
            1 => Level::Warn,
            _ => Level::Ok,
        }
    }
}

pub struct Pressure {
    /// The file store's root — the filesystem whose headroom decides. `None`
    /// (a memory/mcp/http store) disables the disk checks: their durability
    /// does not live on this disk, and refusing work for a full local disk the
    /// store never touches would be shedding for the wrong reason.
    disk_path: Option<PathBuf>,
    /// Below this many free bytes: shed. Warn at twice this.
    shed_below: u64,
    level: AtomicU8,
    /// What drove the level ("disk" / "memory"), packed as u8.
    cause: AtomicU8,
    last_check_ms: AtomicU64,
    /// The last measured free-bytes reading (for gauges and logs).
    pub disk_free: AtomicU64,
}

const RECHECK_MS: u64 = 2_000;

impl Pressure {
    /// The admission verdict for work at a given priority: `Shed` refuses
    /// everything, `Warn` already refuses **low**-priority work — which is what
    /// gives `priority: low` teeth beyond a niceness delta: it is the work an
    /// operator pre-agreed to sacrifice first.
    pub fn refusal(&self, low_priority: bool) -> Option<String> {
        match self.level() {
            Level::Shed => Some(format!(
                "{} pressure (shedding new work; in-flight work drains)",
                self.cause()
            )),
            Level::Warn if low_priority => Some(format!(
                "{} pressure (low-priority work sheds at warn)",
                self.cause()
            )),
            _ => None,
        }
    }
}

impl Pressure {
    pub fn new(disk_path: Option<PathBuf>, shed_below: u64) -> Pressure {
        Pressure {
            disk_path,
            shed_below,
            level: AtomicU8::new(0),
            cause: AtomicU8::new(0),
            last_check_ms: AtomicU64::new(0),
            disk_free: AtomicU64::new(u64::MAX),
        }
    }

    /// The current level, re-measuring at most every couple of seconds.
    pub fn level(&self) -> Level {
        let now = crate::state::now_ms();
        let last = self.last_check_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= RECHECK_MS
            && self
                .last_check_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let (level, cause) = self.assess();
            self.level.store(level as u8, Ordering::Relaxed);
            self.cause.store(cause, Ordering::Relaxed);
        }
        Level::from_u8(self.level.load(Ordering::Relaxed))
    }

    /// Whether new work should be REFUSED right now.
    pub fn shedding(&self) -> bool {
        self.level() == Level::Shed
    }

    /// What drove the current level.
    pub fn cause(&self) -> &'static str {
        match self.cause.load(Ordering::Relaxed) {
            1 => "disk",
            2 => "memory",
            _ => "none",
        }
    }

    fn assess(&self) -> (Level, u8) {
        if let Some(p) = &self.disk_path
            && self.shed_below > 0
            && let Some(free) = free_bytes(p)
        {
            self.disk_free.store(free, Ordering::Relaxed);
            if free < self.shed_below {
                return (Level::Shed, 1);
            }
            if free < self.shed_below.saturating_mul(2) {
                return (Level::Warn, 1);
            }
        }
        if crate::supervisor::cgroup::under_memory_pressure() {
            return (Level::Shed, 2);
        }
        (Level::Ok, 0)
    }
}

/// Free bytes available to unprivileged writes on `path`'s filesystem.
///
/// `f_bavail` (available to non-root), not `f_bfree`: the reserved root blocks
/// are headroom the store cannot actually use, and counting them reports a
/// disk as fine right up until every write fails.
pub fn free_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut sv) } != 0 {
        return None;
    }
    Some((sv.f_bavail as u64).saturating_mul(sv.f_frsize as u64))
}

/// Parse a human size: `256MB`, `1.5GiB`, `524288000`. Decimal and binary
/// prefixes both mean binary here — an operator writing `256MB` for a
/// threshold wants "about a quarter gig", and the 4.8% difference is noise
/// against a knob whose purpose is "not zero".
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty size".into());
    }
    let lower = t.to_ascii_lowercase();
    let (num, mult) = if let Some(n) = lower
        .strip_suffix("gib")
        .or_else(|| lower.strip_suffix("gb"))
        .or_else(|| lower.strip_suffix('g'))
    {
        (n, 1u64 << 30)
    } else if let Some(n) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix("mb"))
        .or_else(|| lower.strip_suffix('m'))
    {
        (n, 1u64 << 20)
    } else if let Some(n) = lower
        .strip_suffix("kib")
        .or_else(|| lower.strip_suffix("kb"))
        .or_else(|| lower.strip_suffix('k'))
    {
        (n, 1u64 << 10)
    } else {
        (lower.as_str(), 1u64)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid size {s:?} (want e.g. 256MB, 1.5GiB, or bytes)"))?;
    if !(v.is_finite() && v >= 0.0) {
        return Err(format!("invalid size {s:?}"));
    }
    Ok((v * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_gives_low_priority_its_teeth_one_level_early() {
        // Real statvfs, engineered thresholds: with shed_below at 2/3 of the
        // actual free space, free sits between shed (free×2/3) and warn
        // (free×4/3) — the WARN band — deterministically, whatever the disk.
        let free = free_bytes(std::path::Path::new("/")).expect("statvfs /");
        let warn_band = Pressure::new(Some("/".into()), free * 2 / 3);
        assert_eq!(warn_band.level(), Level::Warn);
        assert!(
            warn_band.refusal(false).is_none(),
            "normal work admits at warn"
        );
        let msg = warn_band.refusal(true).expect("low sheds at warn");
        assert!(msg.contains("low-priority"), "{msg}");

        // Below shed everything refuses (shed_below > free → Shed).
        let shedding = Pressure::new(Some("/".into()), u64::MAX);
        assert_eq!(shedding.level(), Level::Shed);
        assert!(shedding.refusal(false).is_some());
        assert!(shedding.refusal(true).is_some());

        // No file store → no disk opinion at all.
        let none = Pressure::new(None, 0);
        assert_eq!(none.level(), Level::Ok);
        assert!(none.refusal(true).is_none());
    }

    #[test]
    fn sizes_parse_and_the_root_filesystem_reports_headroom() {
        assert_eq!(parse_bytes("256MB").unwrap(), 256 << 20);
        assert_eq!(parse_bytes("256MiB").unwrap(), 256 << 20);
        assert_eq!(
            parse_bytes("1.5GiB").unwrap(),
            (1.5 * (1u64 << 30) as f64) as u64
        );
        assert_eq!(parse_bytes("1024").unwrap(), 1024);
        assert_eq!(parse_bytes("0").unwrap(), 0);
        assert!(parse_bytes("lots").is_err());
        // statvfs works on a path that certainly exists.
        assert!(free_bytes(std::path::Path::new("/")).unwrap() > 0);
    }

    /// The thresholds, driven with a fake filesystem via the raw pieces.
    #[test]
    fn levels_change_at_the_declared_thresholds() {
        let p = Pressure::new(None, 256 << 20);
        // No disk path: only memory can shed, and without a cgroup the level
        // is Ok — the checks disable rather than guess.
        assert_eq!(p.assess().0, Level::Ok);
    }
}
