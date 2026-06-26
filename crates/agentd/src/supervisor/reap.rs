//! Child reaping + orphan discipline. RFC 0003 §pid1-orphan.
//!
//! Two responsibilities:
//! 1. **Subreaper / PID-1 discipline.** `PR_SET_CHILD_SUBREAPER` makes
//!    grandchildren orphaned by a dying subagent reparent to *us*, not host
//!    init — so the whole tree stays in agentd's reaping domain. When agentd
//!    *is* PID 1 (a bare container), the same applies natively.
//! 2. **Reaping.** Each active `Supervisor` tick drains the process-global
//!    reaper (`reaper::reap_and_dispatch`), whose `reap_pending` here is the
//!    single `waitpid(-1, WNOHANG)` **loop** — looped because SIGCHLD does not
//!    queue, so one signal may cover several exits. The SIGCHLD self-pipe
//!    (`signals.rs`) is only a promptness hint; correctness does not depend on
//!    it.
//!
//! The exit-status decode ([`classify_status`]) is pure and unit-tested; the
//! `waitpid` loop itself is a thin libc wrapper exercised by the reactor.

/// How a child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Normal exit with this code (0 = clean).
    Exited(i32),
    /// Killed by this signal number (e.g. 9 = SIGKILL, 15 = SIGTERM).
    Signaled(i32),
}

impl WaitOutcome {
    pub fn is_clean(self) -> bool {
        matches!(self, WaitOutcome::Exited(0))
    }
}

/// A reaped child: its pid and how it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    pub pid: i32,
    pub outcome: WaitOutcome,
}

#[cfg(unix)]
mod imp {
    use super::{Reaped, WaitOutcome};

    /// Decode a raw `waitpid` status into a [`WaitOutcome`]. Pure — uses the
    /// libc `WIF*` macros so the encoding stays correct per platform.
    pub fn classify_status(status: i32) -> WaitOutcome {
        if libc::WIFSIGNALED(status) {
            WaitOutcome::Signaled(libc::WTERMSIG(status))
        } else {
            // WIFEXITED (or stopped/continued, which WNOHANG won't surface
            // without WUNTRACED/WCONTINUED — we request neither).
            WaitOutcome::Exited(libc::WEXITSTATUS(status))
        }
    }

    /// Reap every child that has exited, without blocking. Drains in a loop
    /// because SIGCHLD does not queue. Returns each reaped pid + outcome
    /// (including unknown pids — orphaned grandchildren we adopted).
    pub fn reap_pending() -> Vec<Reaped> {
        let mut reaped = Vec::new();
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid > 0 {
                reaped.push(Reaped { pid, outcome: classify_status(status) });
            } else {
                // 0 = children exist but none have exited; -1 = ECHILD/error.
                break;
            }
        }
        reaped
    }

    /// Become a subreaper so orphaned grandchildren reparent to us. Returns
    /// true on success (Linux ≥ 3.4). Best-effort: a failure just means
    /// orphans go to init, which `PDEATHSIG` already guards against.
    pub fn set_child_subreaper() -> bool {
        #[cfg(target_os = "linux")]
        {
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0 }
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Are we PID 1 (running as a bare container's init)? Then we must reap
    /// *all* orphans, and our own death takes the tree with us.
    pub fn is_init() -> bool {
        unsafe { libc::getpid() == 1 }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Reaped, WaitOutcome};
    pub fn classify_status(status: i32) -> WaitOutcome {
        WaitOutcome::Exited(status)
    }
    pub fn reap_pending() -> Vec<Reaped> {
        Vec::new()
    }
    pub fn set_child_subreaper() -> bool {
        false
    }
    pub fn is_init() -> bool {
        false
    }
}

pub use imp::{classify_status, is_init, set_child_subreaper};
// `reap_pending` is the one process-global `waitpid(-1)`; it must be called ONLY
// from `reaper::reap_and_dispatch` (under the routes lock), so a stray caller
// can't reopen the reap-before-register race or steal another reactor's child.
pub(in crate::supervisor) use imp::reap_pending;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Construct raw statuses in the Linux/glibc encoding the libc macros
    // decode: a normal exit puts the code in the high byte; a signal death
    // puts the signal in the low 7 bits.
    fn exited(code: i32) -> i32 {
        code << 8
    }
    fn signaled(sig: i32) -> i32 {
        sig
    }

    #[test]
    fn classify_exit_code() {
        assert_eq!(classify_status(exited(0)), WaitOutcome::Exited(0));
        assert_eq!(classify_status(exited(7)), WaitOutcome::Exited(7));
        assert!(classify_status(exited(0)).is_clean());
        assert!(!classify_status(exited(5)).is_clean());
    }

    #[test]
    fn classify_signal_death() {
        assert_eq!(classify_status(signaled(libc::SIGKILL)), WaitOutcome::Signaled(9));
        assert_eq!(classify_status(signaled(libc::SIGTERM)), WaitOutcome::Signaled(15));
        assert!(!classify_status(signaled(libc::SIGKILL)).is_clean());
    }

    // NOTE: `reap_pending()` is intentionally NOT unit-tested here. It calls
    // `waitpid(-1, WNOHANG)`, which in a multi-threaded test process would reap
    // *other* tests' child processes (e.g. the `exec` tests' /bin/echo) before
    // their own `Child::wait`, causing spurious ECHILD failures. In production
    // it only runs inside the supervisor process, whose reaping domain is its
    // own; it's covered end-to-end by the spawn/reactive integration tests,
    // which run agentd in separate processes.
}
