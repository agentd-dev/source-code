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
//! ## Active enforcement (best-effort, opt-in, never required)
//!
//! On top of the reads, when the operator opts in (`--cgroup auto|<path>` /
//! `AGENTD_CGROUP`) and the cgroup-v2 tree is writable, each supervised run is
//! placed in its own child cgroup so teardown can write **`cgroup.kill`** — the
//! kernel then SIGKILLs the *entire* subtree atomically, catching processes that
//! escaped the process group (`setsid`) which `killpg` + `PR_SET_PDEATHSIG`
//! would miss (assessment §2.3 risk #3, the worst leak). And [`under_memory_pressure`]
//! lets the spawn-admission gates backpressure when the unit is at its
//! `memory.high` soft limit. Every cgroup op is best-effort: if the tree isn't
//! writable (no delegation, cgroup-v1, off-cgroup) the feature silently disables
//! and the run falls back to the PDEATHSIG + kill-ladder path — agentd stays
//! cgroup-*aware*, never cgroup-*requiring*.
//!
//! Note: hard resource *limits* on the child (`memory.max`/`pids.max`) need the
//! parent to delegate controllers via `cgroup.subtree_control`, which fails
//! (`EBUSY`) whenever the parent cgroup holds processes directly — common for a
//! systemd unit without `Delegate=yes`. So limit-setting stays a deployment
//! concern (size the pod's `resources.limits`); agentd adds the atomic teardown
//! backstop and the soft-pressure backpressure, which need no delegation.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// Backpressure new spawns once usage reaches this percentage of `memory.high`.
const MEMORY_HIGH_BACKPRESSURE_PCT: u64 = 95;

/// The resolved parent cgroup directory under which per-run child cgroups are
/// created, set once at startup by [`configure`]. `Some(None)` = configured but
/// the tree isn't writable (feature disabled); unset / `Some(None)` both mean
/// [`CgroupGuard::for_run`] yields `None` and runs fall back to PDEATHSIG.
static PARENT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Per-run child-cgroup name counter (unique within this process).
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Hard resource limits to write onto each per-run leaf cgroup, set once at
/// startup by [`configure`]. Empty (the default) → leaves get no limits, only the
/// `cgroup.kill` teardown backstop. Applying limits needs the parent to delegate
/// the `memory`/`pids` controllers (see [`enable_controllers`]); where it can't
/// (e.g. `--cgroup auto` under a busy unit cgroup → `EBUSY`) the writes no-op and
/// the run still gets atomic teardown.
static LIMITS: OnceLock<Limits> = OnceLock::new();

/// Normalised hard limits for a per-run leaf cgroup. Each field, when set, is the
/// exact string written to the corresponding cgroup-v2 interface file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    /// Value for `memory.max` (`"max"` or a byte count).
    pub memory_max: Option<String>,
    /// Value for `pids.max` (`"max"` or a count).
    pub pids_max: Option<String>,
}

impl Limits {
    /// Build from raw `--cgroup-memory-max` / `--cgroup-pids-max` specs; an
    /// unparseable spec is dropped (the limit just isn't applied).
    pub fn from_specs(memory_max: Option<&str>, pids_max: Option<&str>) -> Limits {
        Limits {
            memory_max: memory_max.and_then(normalize_bytes),
            pids_max: pids_max.and_then(normalize_count),
        }
    }

    /// No limits requested → skip controller delegation + per-leaf writes entirely.
    pub fn is_empty(&self) -> bool {
        self.memory_max.is_none() && self.pids_max.is_none()
    }
}

/// Normalise a memory-size spec to the bytes string `memory.max` expects: `"max"`
/// (unlimited), a plain byte count, or a `K`/`M`/`G`-suffixed (1024-based) size.
/// `None` for anything unparseable.
fn normalize_bytes(s: &str) -> Option<String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("max") {
        return Some("max".to_string());
    }
    let (digits, mult): (&str, u64) = match s.chars().last() {
        Some(c) if c.is_ascii_digit() => (s, 1),
        Some('K' | 'k') => (&s[..s.len() - 1], 1024),
        Some('M' | 'm') => (&s[..s.len() - 1], 1024 * 1024),
        Some('G' | 'g') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => return None,
    };
    let n: u64 = digits.trim().parse().ok()?;
    n.checked_mul(mult).map(|b| b.to_string())
}

/// Normalise a pid-count spec for `pids.max`: `"max"` or a non-negative integer.
fn normalize_count(s: &str) -> Option<String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("max") {
        return Some("max".to_string());
    }
    s.parse::<u64>().ok().map(|n| n.to_string())
}

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
    MemorySnapshot {
        max: memory_max(),
        current: memory_current(),
        high: memory_high(),
    }
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
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| parse_mem(&s))
}

/// Parse a cgroup v2 memory value: a byte count, or `"max"` (unlimited → `None`).
fn parse_mem(s: &str) -> Option<u64> {
    match s.trim() {
        "max" => None,
        t => t.parse::<u64>().ok(),
    }
}

// ---------------------------------------------------------------------------
// Active enforcement: child-cgroup placement + `cgroup.kill` teardown backstop.
// ---------------------------------------------------------------------------

/// What [`configure`] settled on: the resolved parent dir (if armed) and whether
/// requested hard limits will actually be enforced (controllers delegated).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configured {
    /// The parent dir under which per-run leaves are created (feature is armed).
    pub parent: PathBuf,
    /// Limits that will be applied to each leaf (empty if none requested).
    pub limits: Limits,
    /// True when limits were requested but the controllers could **not** be
    /// delegated (so the writes will no-op) — the caller should warn.
    pub limits_unavailable: bool,
}

/// Resolve + probe the `--cgroup` spec ONCE at startup, arming per-run child
/// cgroups. `spec` is `"auto"` (derive `<own-cgroup>/agentd` from
/// `/proc/self/cgroup`) or an absolute path under `/sys/fs/cgroup`. Optional
/// `memory_max`/`pids_max` specs request hard limits on each run's leaf; this
/// best-effort delegates the controllers to the parent so the limits can take
/// effect. Returns `None` when off / not writable (the feature stays dormant).
/// Idempotent — the first call wins.
pub fn configure(
    spec: Option<&str>,
    memory_max: Option<&str>,
    pids_max: Option<&str>,
) -> Option<Configured> {
    let resolved = spec.and_then(resolve_parent).filter(|p| ensure_writable(p));
    let limits = Limits::from_specs(memory_max, pids_max);
    let mut limits_unavailable = false;
    if let Some(p) = &resolved {
        // Reclaim any `run-*` cgroups orphaned by prior crashed/abandoned runs (a
        // wedged D-state task can outlive its guard's Drop), so a long-lived
        // daemon can't slowly accumulate stale child cgroups across restarts.
        sweep_stale(p);
        // Delegate the controllers the requested limits need to the parent, so
        // the per-run leaves get enforceable `memory.max`/`pids.max`. Fails
        // (EBUSY) where the parent holds processes directly (e.g. `auto` under a
        // busy unit cgroup) — limits then no-op, but teardown still works.
        if !limits.is_empty() {
            limits_unavailable = !enable_controllers(p, &limits);
        }
    }
    // OnceLock::set fails only if already set (first call wins). Read back the
    // stored values so the caller's log never disagrees with what governs runs.
    let _ = PARENT.set(resolved);
    let _ = LIMITS.set(limits);
    let parent = PARENT.get().cloned().flatten()?;
    Some(Configured {
        parent,
        limits: LIMITS.get().cloned().unwrap_or_default(),
        limits_unavailable,
    })
}

/// Delegate the `memory`/`pids` controllers (only those the requested limits
/// need) to `parent` via its `cgroup.subtree_control`, so child leaves expose the
/// matching interface files. Each controller is delegated with its OWN write: a
/// `subtree_control` write is atomic, so a combined `+memory +pids` would fail
/// wholesale when the parent can delegate only one (e.g. a `Delegate=pids` unit),
/// needlessly dropping the achievable limit. Best-effort → returns whether
/// **every requested** controller was delegated.
fn enable_controllers(parent: &Path, limits: &Limits) -> bool {
    let file = parent.join("cgroup.subtree_control");
    let mut all_ok = true;
    if limits.memory_max.is_some() {
        all_ok &= write_cgroup(&file, "+memory");
    }
    if limits.pids_max.is_some() {
        all_ok &= write_cgroup(&file, "+pids");
    }
    all_ok
}

/// Best-effort reclaim of stale per-run child cgroups under `parent`. Removes a
/// `run-<pid>-*` cgroup only when its owning pid is **dead** (or is our own,
/// freshly-reused pid) — so a concurrent sibling agentd sharing this parent is
/// never torn down. `.probe-*` leftovers (from a crashed `ensure_writable`) are
/// always reclaimed.
fn sweep_stale(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        if stale_run(&entry.file_name().to_string_lossy(), me) {
            let dir = entry.path();
            let _ = std::fs::write(dir.join("cgroup.kill"), "1");
            let _ = std::fs::remove_dir(&dir);
        }
    }
}

/// Whether a child-cgroup dir name denotes a reclaimable stale run: a
/// `run-<pid>-*` whose pid is dead or our own (freshly-reused — we've made no run
/// cgroups yet) pid, or a `.probe-*` leftover from a crashed `ensure_writable`. A
/// live sibling's `run-*` (and any other name) is spared.
fn stale_run(name: &str, me: u32) -> bool {
    if let Some(rest) = name.strip_prefix("run-") {
        match rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) {
            Some(pid) => pid == me || !pid_alive(pid),
            None => false,
        }
    } else {
        name.starts_with(".probe-")
    }
}

/// Whether `pid` is a live process. `kill(pid, 0)` → `Ok`/`EPERM` = alive,
/// `ESRCH` = dead. Conservative: an unrelated process that reused the pid reads
/// as alive, so we skip (delay reclaim) rather than risk touching its cgroup.
fn pid_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Resolve a `--cgroup` spec to an absolute parent directory (no I/O beyond
/// reading `/proc/self/cgroup` for `auto`). `None` for an unusable spec.
fn resolve_parent(spec: &str) -> Option<PathBuf> {
    match spec.trim() {
        "" => None,
        "auto" => own_cgroup_dir().map(|d| d.join("agentd")),
        // An explicit path must sit under the cgroup-v2 mount, by path COMPONENT
        // (so `/sys/fs/cgroup-sibling` can't slip past a byte-prefix), with no
        // `..`. A guard-rail, not a security boundary — `--cgroup` is operator-
        // supplied and the operator already controls the process; a symlink
        // component could still redirect, which the trust model accepts.
        p if Path::new(p).is_absolute()
            && Path::new(p).starts_with(CGROUP_ROOT)
            && !p.contains("..") =>
        {
            Some(PathBuf::from(p))
        }
        _ => None,
    }
}

/// The unit's own cgroup directory, from the `0::<path>` line of
/// `/proc/self/cgroup` (cgroup-v2 unified hierarchy). `None` off cgroup-v2.
fn own_cgroup_dir() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = content.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(Path::new(CGROUP_ROOT).join(rel.trim_start_matches('/')))
}

/// Probe that we can actually create + remove a child cgroup under `parent`
/// (so the feature only arms where it works). Creates `parent` if needed.
fn ensure_writable(parent: &Path) -> bool {
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let probe = parent.join(format!(".probe-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Whether the unit is at/over the backpressure fraction of its `memory.high`
/// soft limit — a signal for the spawn-admission gates to refuse new subagents
/// rather than push the cgroup into reclaim/OOM. Reads live each call; `false`
/// when no cgroup / no `memory.high` set (can't tell → don't block).
pub fn under_memory_pressure() -> bool {
    // Read only the two files the predicate needs (not the full snapshot's three).
    over_threshold(memory_current(), memory_high())
}

/// Pure predicate behind [`under_memory_pressure`] (testable without a cgroup).
fn over_threshold(current: Option<u64>, high: Option<u64>) -> bool {
    match (current, high) {
        (Some(cur), Some(high)) if high > 0 => {
            cur.saturating_mul(100) >= high.saturating_mul(MEMORY_HIGH_BACKPRESSURE_PCT)
        }
        _ => false,
    }
}

/// A per-run child cgroup. Placing the root subagent here puts its whole subtree
/// in the cgroup (membership inherits across `fork`), so [`kill_all`] tears the
/// entire subtree down atomically. RAII: `Drop` kills + removes the cgroup.
///
/// [`kill_all`]: CgroupGuard::kill_all
pub struct CgroupGuard {
    dir: PathBuf,
}

impl CgroupGuard {
    /// Create the per-run child cgroup under the configured parent, applying the
    /// configured hard limits, or `None` when the feature is off / creation fails
    /// (best-effort, never an error).
    pub fn for_run() -> Option<CgroupGuard> {
        let parent = PARENT.get().and_then(|o| o.clone())?;
        let name = format!(
            "run-{}-{}",
            std::process::id(),
            RUN_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let guard = Self::create(&parent, &name)?;
        if let Some(limits) = LIMITS.get() {
            guard.apply_limits(limits);
        }
        Some(guard)
    }

    /// Create a child cgroup `parent/name` (best-effort). Shared by `for_run`
    /// and tests (which resolve a parent directly, bypassing the global).
    fn create(parent: &Path, name: &str) -> Option<CgroupGuard> {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).ok()?;
        Some(CgroupGuard { dir })
    }

    /// Write the configured hard limits onto this leaf's `memory.max`/`pids.max`.
    /// Best-effort: a write no-ops where the controller wasn't delegated (the
    /// interface file is absent / read-only), so the run keeps `cgroup.kill`
    /// teardown without the limit. Returns `(memory_ok, pids_ok)` for logging.
    pub fn apply_limits(&self, limits: &Limits) -> (bool, bool) {
        let memory_ok = match &limits.memory_max {
            Some(v) => write_cgroup(&self.dir.join("memory.max"), v),
            None => false,
        };
        let pids_ok = match &limits.pids_max {
            Some(v) => write_cgroup(&self.dir.join("pids.max"), v),
            None => false,
        };
        (memory_ok, pids_ok)
    }

    /// Move `pid` (and, by inheritance, its future descendants) into this
    /// cgroup by writing its `cgroup.procs`. Best-effort → returns success.
    pub fn place(&self, pid: i32) -> bool {
        write_cgroup(&self.dir.join("cgroup.procs"), &pid.to_string())
    }

    /// Atomically SIGKILL every process in the subtree via `cgroup.kill` — the
    /// backstop beyond `killpg`/PDEATHSIG. Best-effort → returns success.
    pub fn kill_all(&self) -> bool {
        write_cgroup(&self.dir.join("cgroup.kill"), "1")
    }

    /// The cgroup directory (for logging).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The `oom_kill` count from this leaf's `memory.events` — how many of its
    /// processes the kernel OOM-killed (i.e. hit `memory.max`). `None` when the
    /// memory controller isn't active (no limit / not delegated). Lets the
    /// supervisor report a `memory.max` kill plainly instead of as a generic exit.
    pub fn oom_kills(&self) -> Option<u64> {
        parse_oom_kills(&std::fs::read_to_string(self.dir.join("memory.events")).ok()?)
    }

    /// Remove the cgroup dir; succeeds only once every member process has been
    /// reaped (the kernel frees the cgroup then).
    fn try_remove(&self) -> bool {
        std::fs::remove_dir(&self.dir).is_ok()
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        // Final backstop: SIGKILL anything left, then remove the (now-freeing)
        // cgroup with a few short retries while the kernel reaps its members. A
        // lingering empty dir is harmless, so this never blocks for long.
        self.kill_all();
        for _ in 0..5 {
            if self.try_remove() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Write a value to a cgroup control file. Best-effort: any error (no such
/// controller, EACCES, EBUSY, off-cgroup) is swallowed → returns success.
fn write_cgroup(path: &Path, value: &str) -> bool {
    std::fs::write(path, value).is_ok()
}

/// Parse the `oom_kill` counter from a `memory.events` body (one `key value`
/// pair per line). `None` when the key is absent.
fn parse_oom_kills(events: &str) -> Option<u64> {
    events
        .lines()
        .find_map(|l| l.strip_prefix("oom_kill "))?
        .trim()
        .parse()
        .ok()
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
        assert!(
            MemorySnapshot {
                max: Some(1),
                ..Default::default()
            }
            .detected()
        );
    }

    #[test]
    fn over_threshold_backpressures_at_95_percent_of_high() {
        // No high set, or no current → can't tell → never backpressure.
        assert!(!over_threshold(Some(1_000), None));
        assert!(!over_threshold(None, Some(1_000)));
        assert!(!over_threshold(None, None));
        assert!(!over_threshold(Some(1_000), Some(0))); // high==0 → ignore
        // Below the fraction → allow; at/above → backpressure.
        assert!(!over_threshold(Some(900), Some(1_000))); // 90% < 95%
        assert!(over_threshold(Some(950), Some(1_000))); // exactly 95%
        assert!(over_threshold(Some(1_000), Some(1_000))); // at high
        assert!(over_threshold(Some(2_000), Some(1_000))); // over high
    }

    #[test]
    fn normalize_bytes_handles_suffixes_max_and_garbage() {
        assert_eq!(normalize_bytes("max").as_deref(), Some("max"));
        assert_eq!(normalize_bytes("MAX").as_deref(), Some("max"));
        assert_eq!(normalize_bytes("1048576").as_deref(), Some("1048576"));
        assert_eq!(
            normalize_bytes("512M").as_deref(),
            Some((512 * 1024 * 1024).to_string().as_str())
        );
        assert_eq!(
            normalize_bytes("2G").as_deref(),
            Some((2u64 * 1024 * 1024 * 1024).to_string().as_str())
        );
        assert_eq!(
            normalize_bytes("64k").as_deref(),
            Some((64 * 1024).to_string().as_str())
        );
        assert_eq!(normalize_bytes(""), None);
        assert_eq!(normalize_bytes("M"), None); // no digits
        assert_eq!(normalize_bytes("12T"), None); // unsupported suffix
        assert_eq!(normalize_bytes("abc"), None);
    }

    #[test]
    fn normalize_count_handles_max_and_integers() {
        assert_eq!(normalize_count("max").as_deref(), Some("max"));
        assert_eq!(normalize_count("128").as_deref(), Some("128"));
        assert_eq!(normalize_count("0").as_deref(), Some("0"));
        assert_eq!(normalize_count(""), None);
        assert_eq!(normalize_count("-1"), None);
        assert_eq!(normalize_count("lots"), None);
    }

    #[test]
    fn parse_oom_kills_reads_the_counter() {
        let events = "low 0\nhigh 0\nmax 3\noom 1\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(parse_oom_kills(events), Some(2));
        assert_eq!(parse_oom_kills("oom_kill 0\n"), Some(0));
        assert_eq!(parse_oom_kills("low 0\nhigh 0\n"), None); // key absent
        assert_eq!(parse_oom_kills(""), None);
    }

    #[test]
    fn limits_from_specs_drops_unparseable() {
        let l = Limits::from_specs(Some("256M"), Some("32"));
        assert_eq!(
            l.memory_max.as_deref(),
            Some((256 * 1024 * 1024).to_string().as_str())
        );
        assert_eq!(l.pids_max.as_deref(), Some("32"));
        assert!(!l.is_empty());
        assert!(Limits::from_specs(None, None).is_empty());
        assert!(Limits::from_specs(Some("nonsense"), None).is_empty()); // dropped
    }

    /// Live proof that hard limits are both applied and enforced: build a manager
    /// cgroup that delegates the pids controller, create a leaf via the real
    /// `CgroupGuard` path, set `pids.max=1`, and confirm a process inside the leaf
    /// cannot `fork` (kernel `EAGAIN`). Skips cleanly where controller delegation
    /// isn't available (limits are best-effort).
    #[test]
    fn limits_are_applied_and_pids_max_is_enforced() {
        let mgr = Path::new(CGROUP_ROOT).join(format!("agentd-test-limits-{}", std::process::id()));
        if std::fs::create_dir(&mgr).is_err() {
            eprintln!("skip: cannot create a cgroup under {CGROUP_ROOT}");
            return;
        }
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::write(self.0.join("cgroup.kill"), "1");
                let _ = std::fs::remove_dir(&self.0);
            }
        }
        let _mgr_cleanup = Cleanup(mgr.clone());

        let limits = Limits::from_specs(Some("32M"), Some("1"));
        if !enable_controllers(&mgr, &limits) {
            eprintln!("skip: parent cannot delegate memory/pids controllers");
            return;
        }
        let guard = CgroupGuard::create(&mgr, "leaf").expect("create leaf cgroup");
        let (mem_ok, pids_ok) = guard.apply_limits(&limits);
        assert!(pids_ok, "pids.max applied");
        assert_eq!(
            std::fs::read_to_string(guard.dir.join("pids.max"))
                .unwrap()
                .trim(),
            "1"
        );
        if mem_ok {
            assert_eq!(
                std::fs::read_to_string(guard.dir.join("memory.max"))
                    .unwrap()
                    .trim(),
                (32 * 1024 * 1024).to_string()
            );
        }

        // Functional enforcement: a process migrated INTO the leaf is the 1 task
        // pids.max=1 allows, so its own `fork` must be refused. Sync via a pipe so
        // the probe only forks after it has been placed in the cgroup.
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        let (rfd, wfd) = (fds[0], fds[1]);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork probe");
        if pid == 0 {
            // Child: async-signal-safe calls only.
            unsafe {
                libc::close(wfd);
                let mut b = [0u8; 1];
                libc::read(rfd, b.as_mut_ptr() as *mut libc::c_void, 1); // wait until placed
                let g = libc::fork();
                if g == 0 {
                    libc::_exit(0); // grandchild (only reached if enforcement failed)
                }
                if g < 0 {
                    libc::_exit(0); // EXPECTED: fork refused with EAGAIN
                }
                let mut s = 0;
                libc::waitpid(g, &mut s, 0);
                libc::_exit(1); // fork unexpectedly succeeded
            }
        }
        // SIGKILL + reap the probe even if an assertion below panics first, so a
        // failing run never leaks the blocked-on-read child / a busy leaf cgroup.
        struct ProbeGuard(Option<i32>);
        impl Drop for ProbeGuard {
            fn drop(&mut self) {
                if let Some(pid) = self.0 {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        let mut s = 0;
                        libc::waitpid(pid, &mut s, 0);
                    }
                }
            }
        }
        let mut probe = ProbeGuard(Some(pid));

        unsafe { libc::close(rfd) };
        assert!(guard.place(pid), "migrate the probe into the leaf");
        unsafe {
            libc::write(wfd, b"x".as_ptr() as *const libc::c_void, 1);
            libc::close(wfd);
        }
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, 0) },
            pid,
            "reap probe"
        );
        probe.0 = None; // reaped — disarm the guard (avoid waitpid on a reused pid)
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "a fork inside the pids.max=1 cgroup must be refused (status={status})"
        );
    }

    #[test]
    fn resolve_parent_accepts_auto_and_in_mount_paths_only() {
        assert_eq!(resolve_parent(""), None);
        assert_eq!(resolve_parent("relative/path"), None);
        assert_eq!(resolve_parent("/etc/passwd"), None); // outside the mount
        assert_eq!(resolve_parent("/sys/fs/cgroup/../etc"), None); // no `..` escape
        assert_eq!(resolve_parent("/sys/fs/cgroup-sibling/x"), None); // component check, not byte prefix
        assert_eq!(
            resolve_parent("/sys/fs/cgroup/foo/agentd"),
            Some(PathBuf::from("/sys/fs/cgroup/foo/agentd"))
        );
        // `auto` resolves iff this host exposes cgroup-v2 (`0::` line); either a
        // path under the mount or None — never a panic.
        if let Some(p) = resolve_parent("auto") {
            assert!(p.starts_with(CGROUP_ROOT));
            assert!(p.ends_with("agentd"));
        }
    }

    #[test]
    fn stale_run_targets_dead_and_own_pid_only() {
        let me = std::process::id();
        // A pid above any possible pid_max → never assigned → always dead (ESRCH).
        let dead = i32::MAX as u32;
        assert!(
            stale_run(&format!("run-{dead}-0"), me),
            "dead pid → reclaim"
        );
        assert!(
            stale_run(&format!("run-{me}-7"), me),
            "our reused pid → reclaim"
        );
        assert!(stale_run(".probe-123", me), "probe leftover → reclaim");
        assert!(!stale_run("run-1-0", me), "live sibling (pid 1) → spare");
        assert!(!stale_run("unrelated", me), "non-run dir → spare");
        assert!(!stale_run("run-notapid-0", me), "unparseable pid → spare");
    }

    /// Live backstop proof: a process that `setsid()`s out of our process group
    /// (so `killpg` would MISS it) is still SIGKILLed by `cgroup.kill` once placed
    /// in the cgroup. Skips cleanly where the cgroup-v2 tree isn't writable — the
    /// feature is never required, so its absence must not fail the suite.
    #[test]
    fn cgroup_kill_reaps_a_process_that_left_the_process_group() {
        let Some(parent) = resolve_parent("auto") else {
            eprintln!("skip: no cgroup-v2 on this host");
            return;
        };
        if !ensure_writable(&parent) {
            eprintln!("skip: cgroup-v2 tree not writable (no delegation)");
            return;
        }
        let cg = CgroupGuard::create(&parent, &format!("test-kill-{}", std::process::id()))
            .expect("create child cgroup");

        // Fork a child that leaves our process group then waits. Only
        // async-signal-safe libc calls between fork and exit (no allocations).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe {
                libc::setsid(); // new session/pgroup → killpg(our pgid) can't reach it
                libc::sleep(10); // cgroup.kill should end us well before this
                libc::_exit(0); // if we get here, we were NOT killed → test fails below
            }
        }

        assert!(cg.place(pid), "place the child pid into the cgroup");
        assert!(cg.kill_all(), "write cgroup.kill");

        let mut status = 0i32;
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(reaped, pid, "reaped the child");
        assert!(
            libc::WIFSIGNALED(status),
            "child was SIGKILLed by cgroup.kill, not a clean exit (status={status})"
        );
        assert_eq!(libc::WTERMSIG(status), libc::SIGKILL, "killed by SIGKILL");
        // `cg` Drop removes the now-empty cgroup dir.
    }
}
