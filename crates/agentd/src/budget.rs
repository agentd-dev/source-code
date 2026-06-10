//! Process-wide resource budgets.
//!
//! Agent is a micro-agent — one workflow per process — so "per
//! workflow" budgets are naturally process-wide. The module lands
//! three caps driven by the TOML's `[budget]` block:
//!
//! | Field              | Units | Enforcement                              |
//! |--------------------|-------|------------------------------------------|
//! | `max_memory_mb`    | MB    | `setrlimit(RLIMIT_AS)` — hard kill on OOM |
//! | `max_cpu_secs`     | sec   | `setrlimit(RLIMIT_CPU)` — SIGXCPU then SIGKILL |
//! | `max_run_time_secs`| sec   | Clamps `--timeout-secs` to this upper bound |
//! | `max_fs_write_mb`  | MB    | Cumulative-bytes counter in `write_file` |
//!
//! RLIMITs apply to **the whole process**, which is exactly the unit
//! we want under the 1-workflow-per-process model. Setting them
//! before engine startup means a runaway node — an infinite loop
//! inside a shell invocation, a memory-blown LLM response — gets
//! terminated by the kernel rather than continuing to consume
//! shared host resources.
//!
//! See [`docs/operations.md §Budgets`] for the operator view.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// `[budget]` block in the workflow TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BudgetConfig {
    /// Memory address-space cap in megabytes. Maps to
    /// `RLIMIT_AS` — the closest POSIX-defined cap on memory
    /// usage (RLIMIT_RSS is largely unenforced on modern Linux).
    /// On breach: SIGKILL.
    #[serde(default)]
    pub max_memory_mb: Option<u64>,

    /// CPU-time cap in seconds. Maps to `RLIMIT_CPU`. On breach:
    /// SIGXCPU at the soft limit (which our process doesn't
    /// handle → default terminate), then SIGKILL at the hard
    /// limit if somehow survived.
    #[serde(default)]
    pub max_cpu_secs: Option<u64>,

    /// Per-run wall-clock ceiling in seconds. If set, clamps the
    /// effective `--timeout-secs` to the lower of the two so the
    /// budget can't be silently enlarged by a CLI flag.
    #[serde(default)]
    pub max_run_time_secs: Option<u64>,

    /// Cumulative bytes the workflow is allowed to write via
    /// `write_file` over the process lifetime. In MB for
    /// readability. Enforced by [`BudgetTracker::check_fs_write`]
    /// in the fs-write handler.
    #[serde(default)]
    pub max_fs_write_mb: Option<u64>,
}

impl BudgetConfig {
    /// Effective timeout — the smaller of `--timeout-secs` and
    /// `max_run_time_secs`. `0` is treated as "no budget bound".
    pub fn clamp_run_time(&self, cli_timeout_secs: u64) -> u64 {
        match self.max_run_time_secs {
            Some(b) if b > 0 => cli_timeout_secs.min(b),
            _ => cli_timeout_secs,
        }
    }

    /// Cumulative fs-write cap in bytes, if set. `None` → unlimited.
    pub fn fs_write_cap_bytes(&self) -> Option<u64> {
        self.max_fs_write_mb
            .filter(|&m| m > 0)
            .map(|m| m.saturating_mul(1024 * 1024))
    }
}

// ---------------------------------------------------------------------------
// RLIMIT application
// ---------------------------------------------------------------------------

/// Apply the RLIMIT-backed caps at process startup. Called once
/// from the runtime before the engine builds. Failures emit a warn
/// audit event but don't abort — an operator who can't set a
/// budget (sandboxed container without CAP_SYS_RESOURCE, restricted
/// seccomp filter, etc.) should still be able to start the agent;
/// the budget just won't apply.
#[cfg(unix)]
pub fn apply_rlimits(cfg: &BudgetConfig) {
    if let Some(mb) = cfg.max_memory_mb {
        let bytes = mb.saturating_mul(1024 * 1024);
        set_rlimit(libc::RLIMIT_AS, bytes, "memory_mb", mb);
    }
    if let Some(secs) = cfg.max_cpu_secs {
        set_rlimit(libc::RLIMIT_CPU, secs, "cpu_secs", secs);
    }
}

/// Windows: apply memory + CPU-time budgets via a Job Object.
///
/// The current process is assigned to a freshly-created job with
/// the configured limits set via `SetInformationJobObject`. On
/// breach:
///   * memory: the kernel fails allocations / kills the process,
///     same effect as `RLIMIT_AS` on Linux.
///   * CPU time: the job is closed (equivalent to `RLIMIT_CPU`
///     terminating the process; Windows fires `EXCEPTION_CODE
///     0xC0000094` / the process just exits).
///
/// `KILL_ON_JOB_CLOSE` ensures the process dies if the job handle
/// is dropped — a defense against scripts that leak the handle.
///
/// `CREATE_BREAKAWAY_FROM_JOB`-related nesting is not used; Windows
/// 8+ allows nested jobs implicitly which is what we want inside
/// containers that may already have an outer job wrapping the
/// runtime.
///
/// Failures emit a warn audit event and continue — same posture as
/// Unix: an operator without the right privilege should still be
/// able to run the agent, just without the budget cap.
#[cfg(windows)]
pub fn apply_rlimits(cfg: &BudgetConfig) {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    const JOB_OBJECT_LIMIT_JOB_TIME: u32 = 0x0000_0004;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

    if cfg.max_memory_mb.is_none() && cfg.max_cpu_secs.is_none() {
        return; // nothing to do
    }

    // SAFETY: CreateJobObjectW with null attrs + null name returns a
    // process-local handle or NULL on failure. We check the return.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        let errno = std::io::Error::last_os_error();
        tracing::warn!(
            target: "agentd::audit",
            event = "budget.apply_failed",
            kind = "job_object_create",
            reason = %format!("{errno}"),
        );
        return;
    }

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    let mut basic: JOBOBJECT_BASIC_LIMIT_INFORMATION = unsafe { zeroed() };
    let mut flags: u32 = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    if let Some(mb) = cfg.max_memory_mb {
        flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = (mb as usize).saturating_mul(1024 * 1024);
    }
    if let Some(secs) = cfg.max_cpu_secs {
        flags |= JOB_OBJECT_LIMIT_JOB_TIME;
        // PerJobUserTimeLimit is 100-nanosecond ticks (same unit as
        // FILETIME). 1 second = 10_000_000 ticks. This is the
        // *cumulative* user-mode CPU time across every process in
        // the job, which matches RLIMIT_CPU's "user+system" on Linux
        // closely enough for budget purposes.
        basic.PerJobUserTimeLimit = (secs as i64).saturating_mul(10_000_000);
    }
    basic.LimitFlags = flags;
    info.BasicLimitInformation = basic;

    // SAFETY: raw pointer + byte-length of a stack-local struct.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        let errno = std::io::Error::last_os_error();
        tracing::warn!(
            target: "agentd::audit",
            event = "budget.apply_failed",
            kind = "job_object_set_info",
            reason = %format!("{errno}"),
        );
        unsafe { CloseHandle(job) };
        return;
    }

    // SAFETY: GetCurrentProcess returns a pseudo-handle, always valid.
    let process = unsafe { GetCurrentProcess() };
    let assigned = unsafe { AssignProcessToJobObject(job, process) };
    if assigned == FALSE {
        let errno = std::io::Error::last_os_error();
        tracing::warn!(
            target: "agentd::audit",
            event = "budget.apply_failed",
            kind = "job_object_assign",
            reason = %format!("{errno}"),
        );
        unsafe { CloseHandle(job) };
        return;
    }

    // Deliberately NOT calling CloseHandle(job). `HANDLE` is a raw
    // pointer with no Drop, so just letting it fall out of scope
    // leaves the job open for the lifetime of the process — which
    // is what we want. The kernel releases it + fires
    // `KILL_ON_JOB_CLOSE` semantics when the process exits.
    let _keep_open: windows_sys::Win32::Foundation::HANDLE = job;

    if let Some(mb) = cfg.max_memory_mb {
        tracing::info!(
            target: "agentd::audit",
            event = "budget.applied",
            kind = "memory_mb",
            value = mb,
        );
    }
    if let Some(secs) = cfg.max_cpu_secs {
        tracing::info!(
            target: "agentd::audit",
            event = "budget.applied",
            kind = "cpu_secs",
            value = secs,
        );
    }
}

#[cfg(not(any(unix, windows)))]
pub fn apply_rlimits(_cfg: &BudgetConfig) {
    // Exotic targets (wasm, etc.) — no process-wide budget primitive.
}

/// glibc spells the setrlimit resource type `__rlimit_resource_t`;
/// macOS / BSD libc uses a plain `c_int`. Alias per-OS so the call
/// compiles everywhere Unix.
#[cfg(all(unix, target_os = "linux"))]
type RlimitResource = libc::__rlimit_resource_t;
#[cfg(all(unix, not(target_os = "linux")))]
type RlimitResource = libc::c_int;

#[cfg(unix)]
fn set_rlimit(resource: RlimitResource, value: u64, label: &str, display_val: u64) {
    // `rlim_t` is `u64` on Linux and `RLIM_INFINITY == u64::MAX`,
    // so no clamp is needed; an operator who passes `u64::MAX`
    // gets exactly the "no effective limit" they asked for.
    let rl = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: setrlimit is a vanilla POSIX call; `rl` is a
    // stack-allocated struct we just initialised.
    let rc = unsafe { libc::setrlimit(resource, &rl) };
    if rc == 0 {
        tracing::info!(
            target: "agentd::audit",
            event = "budget.applied",
            kind = label,
            value = display_val,
        );
    } else {
        let errno = std::io::Error::last_os_error();
        tracing::warn!(
            target: "agentd::audit",
            event = "budget.apply_failed",
            kind = label,
            value = display_val,
            reason = %format!("{errno}"),
        );
    }
}

// ---------------------------------------------------------------------------
// Cumulative fs-write tracker
// ---------------------------------------------------------------------------

/// Shared per-process counter for `write_file` byte volume. All
/// fs-write handlers call [`check_fs_write`] before committing to
/// disk; the handler returns a policy-style deny when the cap is
/// exceeded.
#[derive(Debug)]
pub struct BudgetTracker {
    bytes_written: AtomicU64,
    cap_bytes: Option<u64>,
}

impl BudgetTracker {
    pub fn new(cfg: &BudgetConfig) -> Self {
        Self {
            bytes_written: AtomicU64::new(0),
            cap_bytes: cfg.fs_write_cap_bytes(),
        }
    }

    /// Reserve `n` bytes of fs-write budget. Atomically bumps the
    /// counter when the cap would not be crossed; returns `Err(msg)`
    /// when the cap is set AND would be exceeded. Returns `Ok(())`
    /// unconditionally when no cap is configured.
    pub fn check_fs_write(&self, n: u64) -> std::result::Result<(), String> {
        let Some(cap) = self.cap_bytes else {
            return Ok(());
        };
        // Compare-and-swap loop: the counter is shared with other
        // workflow threads when multiple triggers run concurrently
        // (cron + fs_watch both writing). Standard atomic
        // bump-if-under-cap pattern.
        let mut current = self.bytes_written.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(n);
            if next > cap {
                return Err(format!(
                    "budget.fs_write exceeded: tried to write {n} bytes but \
                     {current} / {cap} already consumed"
                ));
            }
            match self.bytes_written.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => {
                    current = observed;
                }
            }
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written.load(Ordering::Relaxed)
    }

    pub fn cap_bytes(&self) -> Option<u64> {
        self.cap_bytes
    }
}

/// Shareable handle for tool handlers.
pub type BudgetRef = Arc<BudgetTracker>;

/// Default tracker — no cap. Used when `[budget]` is absent from
/// the workflow.
pub fn unbounded() -> BudgetRef {
    Arc::new(BudgetTracker {
        bytes_written: AtomicU64::new(0),
        cap_bytes: None,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_run_time_takes_the_smaller() {
        let cfg = BudgetConfig {
            max_run_time_secs: Some(30),
            ..Default::default()
        };
        assert_eq!(cfg.clamp_run_time(120), 30);
        assert_eq!(cfg.clamp_run_time(10), 10);
    }

    #[test]
    fn clamp_run_time_respects_zero_meaning_unlimited() {
        let cfg = BudgetConfig {
            max_run_time_secs: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.clamp_run_time(120), 120);
    }

    #[test]
    fn clamp_run_time_without_budget_returns_cli_value() {
        let cfg = BudgetConfig::default();
        assert_eq!(cfg.clamp_run_time(120), 120);
    }

    #[test]
    fn fs_write_cap_converts_mb_to_bytes() {
        let cfg = BudgetConfig {
            max_fs_write_mb: Some(2),
            ..Default::default()
        };
        assert_eq!(cfg.fs_write_cap_bytes(), Some(2 * 1024 * 1024));
    }

    #[test]
    fn fs_write_cap_treats_zero_as_unbounded() {
        let cfg = BudgetConfig {
            max_fs_write_mb: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.fs_write_cap_bytes(), None);
    }

    #[test]
    fn tracker_with_no_cap_always_allows() {
        let t = BudgetTracker::new(&BudgetConfig::default());
        assert!(t.check_fs_write(1_000_000_000).is_ok());
        assert_eq!(t.bytes_written(), 0); // no tracking when unbounded
    }

    #[test]
    fn tracker_counts_writes_under_cap() {
        let t = BudgetTracker::new(&BudgetConfig {
            max_fs_write_mb: Some(1),
            ..Default::default()
        });
        assert!(t.check_fs_write(500_000).is_ok());
        assert!(t.check_fs_write(400_000).is_ok());
        assert_eq!(t.bytes_written(), 900_000);
    }

    #[test]
    fn tracker_denies_when_cap_crossed() {
        let t = BudgetTracker::new(&BudgetConfig {
            max_fs_write_mb: Some(1),
            ..Default::default()
        });
        assert!(t.check_fs_write(1_000_000).is_ok());
        let err = t.check_fs_write(200_000).unwrap_err();
        assert!(err.contains("fs_write exceeded"));
        // Counter does NOT advance on failed attempts.
        assert_eq!(t.bytes_written(), 1_000_000);
    }

    #[test]
    fn tracker_is_threadsafe() {
        use std::sync::Arc;
        use std::thread;
        let t: Arc<BudgetTracker> = Arc::new(BudgetTracker::new(&BudgetConfig {
            max_fs_write_mb: Some(1), // 1 MiB cap
            ..Default::default()
        }));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let t = t.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        let _ = t.check_fs_write(1024); // 1 KiB each
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Exactly 1 MiB consumed (8 threads * 100 calls * 1 KiB = 800 KiB;
        // we'd need more threads/calls to actually saturate, but the
        // counter should just reflect the total accepted writes).
        assert!(t.bytes_written() <= 1024 * 1024);
        assert!(t.bytes_written() >= 800 * 1024);
    }
}
