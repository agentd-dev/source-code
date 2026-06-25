//! Signal handling. RFC 0003 §signals, RFC 0011 §signals.
//!
//! M1 installs the minimum: `SIGTERM`/`SIGINT` set a one-way `DRAINING` flag
//! (a second one sets `FORCE`); `SIGPIPE` is ignored so the supervisor never
//! dies writing to a just-exited child. Handlers are async-signal-safe — they
//! only touch `AtomicBool`s. `SA_RESTART` is deliberately **off** so blocked
//! syscalls return `EINTR` and the reactor can re-check the flags.
//!
//! The self-pipe wakeup (so a blocked `recv_timeout`/`poll` returns promptly)
//! and `SIGCHLD` reaping land with the supervisor reactor in M2.

#[cfg(unix)]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    static DRAINING: AtomicBool = AtomicBool::new(false);
    static FORCE: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_term(_sig: libc::c_int) {
        // First TERM/INT requests a graceful drain; a second forces a kill.
        if DRAINING.swap(true, Ordering::SeqCst) {
            FORCE.store(true, Ordering::SeqCst);
        }
    }

    fn set_handler(sig: libc::c_int, handler: libc::sighandler_t) {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0; // no SA_RESTART: blocked syscalls return EINTR
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }

    pub fn install() {
        let h = on_term as extern "C" fn(libc::c_int) as libc::sighandler_t;
        set_handler(libc::SIGTERM, h);
        set_handler(libc::SIGINT, h);
        set_handler(libc::SIGPIPE, libc::SIG_IGN);
    }

    pub fn draining() -> bool {
        DRAINING.load(Ordering::SeqCst)
    }

    pub fn force() -> bool {
        FORCE.load(Ordering::SeqCst)
    }
}

#[cfg(not(unix))]
mod imp {
    // Non-Unix: agentd targets Linux; these keep the crate compiling on other
    // hosts for editor/CI convenience. Real shutdown handling is Unix-only.
    pub fn install() {}
    pub fn draining() -> bool {
        false
    }
    pub fn force() -> bool {
        false
    }
}

/// Install SIGTERM/SIGINT/SIGPIPE handlers. Idempotent enough to call once at
/// startup.
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
