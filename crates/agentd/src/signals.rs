//! Process-global signal plumbing.
//!
//! One flag, backed by an `AtomicBool`:
//!
//! - [`shutdown_requested`] — flips on `SIGTERM` / `SIGINT`. Long-
//!   running loops poll this and transition into graceful drain.
//!   One-way — once set, we never return to "running".
//!
//! Unix handler: `SA_RESTART` is intentionally NOT set — a blocked
//! `accept()` returns `EINTR` so the loop gets a chance to observe
//! the flag immediately instead of waiting for the next poll tick.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Observation: has any shutdown handler fired? Async-signal-safe read.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Reset the flag (tests only).
#[cfg(test)]
pub fn reset() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
}

/// Install handlers for `SIGTERM` and `SIGINT`.
/// Idempotent — calling twice is harmless; the second install
/// replaces the first with identical behaviour.
#[cfg(unix)]
#[allow(clippy::fn_to_numeric_cast_any, function_casts_as_integer)]
pub fn install_shutdown_handlers() {
    // SAFETY: `sigaction` is signal-safe; handlers only touch
    // `AtomicBool`s with SeqCst ordering, which is safe under
    // POSIX signal semantics on every platform we target.
    //
    // `sa_sigaction` is `libc::sighandler_t` (alias for `usize`)
    // because POSIX's sigaction union is represented as an opaque
    // word. Casting our `extern "C" fn` to `usize` is the canonical
    // bridge; the newer `function_casts_as_integer` lint flags it
    // but the operation is correct by construction here.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = shutdown_handler as usize;
        // Intentionally 0 (no SA_RESTART): we want syscalls in the
        // accept loop to return EINTR so the loop sees the flag
        // promptly.
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

/// Stub for any other target (wasm, unknown, …). Keeps compilation
/// green without pretending to handle signals.
#[cfg(not(unix))]
pub fn install_shutdown_handlers() {
    // no-op
}

#[cfg(unix)]
extern "C" fn shutdown_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_not_requested() {
        reset();
        assert!(!shutdown_requested());
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_handler_flips_flag() {
        reset();
        install_shutdown_handlers();
        shutdown_handler(libc::SIGTERM);
        assert!(shutdown_requested());
        reset();
    }
}
