//! cgroup v2 memory awareness (read-only). Assessment §4 M5.
//!
//! Best-effort, **never required**: a cloud-native unit reports the memory
//! budget its scheduler handed it so OOM risk is observable (logged at startup,
//! and exposed as a `/metrics` gauge). Reads the unified cgroup v2 interface
//! files directly under `/sys/fs/cgroup`; in a container with a cgroup
//! namespace (the target
//! shape) that path is the unit's *own* cgroup, so the direct read is correct.
//! On a bare host it reflects the root cgroup (whole-host) — still informative.
//! Any missing file / cgroup v1 / parse failure degrades to `None`.
//!
//! The active-enforcement pieces (a child cgroup + `cgroup.kill` for atomic
//! subtree teardown, `memory.high` backpressure) build on these reads later.

use std::path::Path;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A point-in-time view of the unit's cgroup v2 memory interface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemorySnapshot {
    /// Hard limit (`memory.max`); `None` = unlimited (`"max"`) or unavailable.
    pub max: Option<u64>,
    /// Current charged usage (`memory.current`), in bytes.
    pub current: Option<u64>,
    /// Soft throttle threshold (`memory.high`); `None` = unset or unavailable.
    pub high: Option<u64>,
}

impl MemorySnapshot {
    /// Whether any cgroup v2 memory file was readable (i.e. we are cgroup-aware).
    pub fn detected(&self) -> bool {
        self.max.is_some() || self.current.is_some() || self.high.is_some()
    }
}

/// Read the current cgroup v2 memory snapshot (best-effort; never fails).
pub fn snapshot() -> MemorySnapshot {
    MemorySnapshot { max: memory_max(), current: memory_current(), high: memory_high() }
}

/// `memory.max` — the hard limit; `None` when unlimited (`"max"`) or unreadable.
pub fn memory_max() -> Option<u64> {
    read_mem(&Path::new(CGROUP_ROOT).join("memory.max"))
}

/// `memory.current` — current charged usage in bytes.
pub fn memory_current() -> Option<u64> {
    read_mem(&Path::new(CGROUP_ROOT).join("memory.current"))
}

/// `memory.high` — the soft (throttling) limit; `None` when unset (`"max"`).
pub fn memory_high() -> Option<u64> {
    read_mem(&Path::new(CGROUP_ROOT).join("memory.high"))
}

/// Read + parse one cgroup memory file. `None` on any I/O error or `"max"`.
fn read_mem(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok().and_then(|s| parse_mem(&s))
}

/// Parse a cgroup v2 memory value: a byte count, or `"max"` (unlimited → `None`).
fn parse_mem(s: &str) -> Option<u64> {
    match s.trim() {
        "max" => None,
        t => t.parse::<u64>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parse_mem_handles_max_and_numbers() {
        assert_eq!(parse_mem("max\n"), None); // unlimited
        assert_eq!(parse_mem("max"), None);
        assert_eq!(parse_mem("1073741824\n"), Some(1_073_741_824));
        assert_eq!(parse_mem("0"), Some(0));
        assert_eq!(parse_mem("garbage"), None);
        assert_eq!(parse_mem(""), None);
    }

    #[test]
    fn read_mem_reads_a_fixture_or_degrades_to_none() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "536870912").unwrap();
        assert_eq!(read_mem(f.path()), Some(536_870_912));

        let mut unlimited = tempfile::NamedTempFile::new().unwrap();
        writeln!(unlimited, "max").unwrap();
        assert_eq!(read_mem(unlimited.path()), None);

        // missing file → None, never an error
        assert_eq!(read_mem(Path::new("/nonexistent/agentd/memory.max")), None);
    }

    #[test]
    fn snapshot_detected_reflects_any_readable_field() {
        assert!(!MemorySnapshot::default().detected());
        assert!(MemorySnapshot { max: Some(1), ..Default::default() }.detected());
    }
}
