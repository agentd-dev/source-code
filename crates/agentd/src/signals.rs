//! Signal handling + the self-pipe wakeup. RFC 0003 §signals, RFC 0011 §signals.
//!
//! Handlers are async-signal-safe — they only touch atomics and `write()` one
//! byte to a **self-pipe** so a blocked reactor wakes promptly (`SA_RESTART`
//! is deliberately off, so blocked syscalls also return `EINTR`). The reactor
//! selects on `wakeup_fd()` alongside its channels; on wake it checks the
//! flags and drains the pipe.
//!
//! - `SIGTERM`/`SIGINT` → one-way `DRAINING` (a second sets `FORCE`).
//! - `SIGCHLD` → set the child-exit flag (the reactor runs `reap::reap_pending`).
//! - `SIGPIPE` → ignored, so the supervisor never dies writing to a dead child.

#[cfg(unix)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    static DRAINING: AtomicBool = AtomicBool::new(false);
    static FORCE: AtomicBool = AtomicBool::new(false);
    static CHILD_EXIT: AtomicBool = AtomicBool::new(false);
    // Lame-duck override (RFC 0015 §4.2): a one-way-per-call readiness override
    // toward NotReady, flipped by the `lame-duck` operator tool — NOT a signal.
    // It rides here (not in a feature-gated module) so it is one process-global
    // truth consulted by BOTH the `/readyz` probe (obs::serve, `metrics`) and the
    // served operator tool (mcp::server, `serve-mcp`), with neither feature
    // depending on the other. Distinct from `DRAINING`: lame-duck never exits.
    static LAME_DUCK: AtomicBool = AtomicBool::new(false);
    // Tree-wide pause state (RFC 0015 §4.3): set by the `pause` operator tool,
    // cleared by `resume`. Like `LAME_DUCK`, it rides here (not a feature-gated
    // module) so it is one process-global truth read by BOTH the served operator
    // surface (`agentd://inventory`, `serve-mcp`) and the `agentd_paused` gauge
    // (`metrics`), with neither feature depending on the other. Distinct from
    // DRAINING/LAME_DUCK: pause freezes the agentic loops only — never exits, never
    // touches readiness (the supervisor reactor and liveness heartbeat run on).
    static PAUSED: AtomicBool = AtomicBool::new(false);
    // Self-pipe fds (-1 until install()). The write end is touched from signal
    // handlers; the read end is what the reactor waits on.
    static WAKE_R: AtomicI32 = AtomicI32::new(-1);
    static WAKE_W: AtomicI32 = AtomicI32::new(-1);

    /// Async-signal-safe: write one byte to the self-pipe. A full/again pipe is
    /// fine — the reactor only needs *a* readable byte to wake.
    fn wake() {
        let w = WAKE_W.load(Ordering::Relaxed);
        if w >= 0 {
            let b = [0u8; 1];
            unsafe {
                libc::write(w, b.as_ptr() as *const libc::c_void, 1);
            }
        }
    }

    extern "C" fn on_term(_sig: libc::c_int) {
        if DRAINING.swap(true, Ordering::SeqCst) {
            FORCE.store(true, Ordering::SeqCst);
        }
        wake();
    }

    extern "C" fn on_chld(_sig: libc::c_int) {
        CHILD_EXIT.store(true, Ordering::SeqCst);
        wake();
    }

    fn set_handler(sig: libc::c_int, handler: libc::sighandler_t, flags: libc::c_int) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = flags; // never SA_RESTART
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }

    fn make_self_pipe() {
        if WAKE_R.load(Ordering::SeqCst) >= 0 {
            return; // already created
        }
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return;
        }
        for &fd in &fds {
            unsafe {
                let fl = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                let fdfl = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, fdfl | libc::FD_CLOEXEC);
            }
        }
        WAKE_R.store(fds[0], Ordering::SeqCst);
        WAKE_W.store(fds[1], Ordering::SeqCst);
    }

    pub fn install() {
        make_self_pipe();
        let term = on_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let chld = on_chld as extern "C" fn(libc::c_int) as libc::sighandler_t;
        set_handler(libc::SIGTERM, term, 0);
        set_handler(libc::SIGINT, term, 0);
        // SA_NOCLDSTOP: only fire on child *termination*, not stop/continue.
        set_handler(libc::SIGCHLD, chld, libc::SA_NOCLDSTOP);
        set_handler(libc::SIGPIPE, libc::SIG_IGN, 0);
    }

    pub fn draining() -> bool {
        DRAINING.load(Ordering::SeqCst)
    }
    pub fn force() -> bool {
        FORCE.load(Ordering::SeqCst)
    }

    /// Programmatically request a graceful drain (the `drain` operator tool,
    /// RFC 0015 §4.1) — the SAME one-way latch SIGTERM sets, plus a wakeup so a
    /// blocked reactor begins the drain choreography promptly. Idempotent and
    /// monotonic: a request after drain has begun is a no-op that never escalates
    /// to FORCE (force remains the *second signal*, RFC 0011 §4.3).
    pub fn request_drain() {
        DRAINING.store(true, Ordering::SeqCst);
        // Reuse the signal-handler wakeup so the reactor leaves its blocking
        // select and runs the drain state machine (RFC 0011 §4.2).
        wake();
    }

    pub fn lame_duck() -> bool {
        LAME_DUCK.load(Ordering::SeqCst)
    }

    /// Set/clear the lame-duck readiness override (RFC 0015 §4.2). `true` forces
    /// `/readyz` NotReady while the supervisor keeps running; `false` clears the
    /// override (readiness then reflects the genuine computed state). No drain,
    /// no exit, reversible.
    pub fn set_lame_duck(on: bool) {
        LAME_DUCK.store(on, Ordering::SeqCst);
    }

    pub fn paused() -> bool {
        PAUSED.load(Ordering::SeqCst)
    }

    /// Set/clear the instance-wide pause state (the `pause`/`resume` operator
    /// tools, RFC 0015 §4.3). Reporting-only: the per-session pause channels do
    /// the actual loop suspension; this flag is the single truth `agentd://inventory`
    /// and `agentd_paused` read. Reversible; never exits, never touches readiness.
    pub fn set_paused(on: bool) {
        PAUSED.store(on, Ordering::SeqCst);
    }

    /// Take and clear the SIGCHLD flag — the reactor then runs the waitpid loop.
    pub fn take_child_exit() -> bool {
        CHILD_EXIT.swap(false, Ordering::SeqCst)
    }

    pub fn wakeup_fd() -> i32 {
        WAKE_R.load(Ordering::SeqCst)
    }

    /// Drain all pending wakeup bytes (the pipe is edge-ish; we level it).
    pub fn drain_wakeup() {
        let r = WAKE_R.load(Ordering::SeqCst);
        if r < 0 {
            return;
        }
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe { libc::read(r, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break; // EAGAIN (drained) or error
            }
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
    pub fn draining() -> bool {
        false
    }
    pub fn force() -> bool {
        false
    }
    pub fn request_drain() {}
    pub fn lame_duck() -> bool {
        false
    }
    pub fn set_lame_duck(_on: bool) {}
    pub fn paused() -> bool {
        false
    }
    pub fn set_paused(_on: bool) {}
    pub fn take_child_exit() -> bool {
        false
    }
    pub fn wakeup_fd() -> i32 {
        -1
    }
    pub fn drain_wakeup() {}
}

/// Install SIGTERM/SIGINT/SIGCHLD/SIGPIPE handlers + the self-pipe. Call once
/// at supervisor startup.
pub fn install() {
    imp::install();
}

/// Has a graceful drain been requested (first SIGTERM/SIGINT)?
pub fn draining() -> bool {
    imp::draining()
}

/// Has a forced shutdown been requested (second SIGTERM/SIGINT)?
pub fn force() -> bool {
    imp::force()
}

/// Request a graceful drain programmatically (the `drain` operator tool,
/// RFC 0015 §4.1) — the same one-way `DRAINING` latch SIGTERM sets, plus a
/// reactor wakeup. Idempotent/monotonic; never escalates to FORCE.
pub fn request_drain() {
    imp::request_drain()
}

/// Is the lame-duck readiness override active (RFC 0015 §4.2)? When true,
/// `/readyz` reports NotReady even though the supervisor keeps running.
pub fn lame_duck() -> bool {
    imp::lame_duck()
}

/// Set or clear the lame-duck readiness override (the `lame-duck` operator tool,
/// RFC 0015 §4.2). `true` overrides readiness toward NotReady; `false` clears it.
pub fn set_lame_duck(on: bool) {
    imp::set_lame_duck(on)
}

/// Is the instance-wide pause active (RFC 0015 §4.3)? When true, the agentic
/// loops are suspended at their turn boundaries; the supervisor and readiness
/// are unaffected.
pub fn paused() -> bool {
    imp::paused()
}

/// Set or clear the instance-wide pause state (the `pause`/`resume` operator
/// tools, RFC 0015 §4.3). Reporting truth for `agentd://inventory` + the
/// `agentd_paused` gauge; the per-session pause channels do the suspension.
pub fn set_paused(on: bool) {
    imp::set_paused(on)
}

/// Take-and-clear the SIGCHLD flag — true if a child exited since last checked.
pub fn take_child_exit() -> bool {
    imp::take_child_exit()
}

/// The read end of the self-pipe — the reactor waits on it for prompt wakeups.
/// Returns -1 before `install()` (or on non-Unix).
pub fn wakeup_fd() -> i32 {
    imp::wakeup_fd()
}

/// Drain pending wakeup bytes after a wake.
pub fn drain_wakeup() {
    imp::drain_wakeup()
}
