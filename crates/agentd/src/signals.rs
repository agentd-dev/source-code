//! Process-global signal plumbing.
//!
//! Two independent flags, both backed by `AtomicBool`:
//!
//! - [`shutdown_requested`] — flips on `SIGTERM` / `SIGINT` (Unix)
//!   or `Ctrl+C` / `Ctrl+Break` (Windows). The HTTP server's accept
//!   loop and `runtime::run_serve_mode`'s wait loop poll this and
//!   transition into graceful drain. One-way — once set, we never
//!   return to "running".
//! - [`reload_requested`] — flips on `SIGHUP` (Unix only). The
//!   serve loop polls this, calls the reload path, then clears the
//!   flag with [`clear_reload`]. Idempotent — spurious signals
//!   during reload collapse into one reload pass. Windows has no
//!   console equivalent; operators restart the process to reload.
//!
//! Unix handler: `SA_RESTART` is intentionally NOT set — a blocked
//! `accept()` returns `EINTR` so the loop gets a chance to observe
//! the flags immediately instead of waiting for the next poll tick.
//!
//! Windows handler: the `ctrlc` crate wraps `SetConsoleCtrlHandler`
//! and dispatches to a callback on Ctrl+C / Ctrl+Break / console
//! close. We set `SHUTDOWN_REQUESTED` and return — the drain loop
//! picks it up.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Observation: has any shutdown handler fired? Async-signal-safe read.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Observation: has `SIGHUP` fired since the last [`clear_reload`]?
pub fn reload_requested() -> bool {
    RELOAD_REQUESTED.load(Ordering::SeqCst)
}

/// Reset the reload flag — call after the serve loop has acted on it.
pub fn clear_reload() {
    RELOAD_REQUESTED.store(false, Ordering::SeqCst);
}

/// Reset both flags (tests only).
#[cfg(test)]
pub fn reset() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    RELOAD_REQUESTED.store(false, Ordering::SeqCst);
}

/// Install handlers for `SIGTERM`, `SIGINT`, and `SIGHUP`.
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
        // Shutdown signals.
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = shutdown_handler as usize;
        // Intentionally 0 (no SA_RESTART): we want syscalls in the
        // accept loop to return EINTR so the loop sees the flag
        // promptly.
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());

        // Reload signal (separate handler so the flags are decoupled).
        let mut sa_reload: libc::sigaction = std::mem::zeroed();
        sa_reload.sa_sigaction = reload_handler as usize;
        sa_reload.sa_flags = 0;
        libc::sigemptyset(&mut sa_reload.sa_mask);
        libc::sigaction(libc::SIGHUP, &sa_reload, std::ptr::null_mut());
    }
}

/// Windows handler: registers a console-control callback via the
/// `ctrlc` crate (which wraps `SetConsoleCtrlHandler`). Fires on
/// Ctrl+C, Ctrl+Break, and console close events. The callback flips
/// `SHUTDOWN_REQUESTED` and returns, letting the main drain loop
/// see the flag on its next poll tick.
///
/// No SIGHUP equivalent on Windows — operators wanting to rotate
/// TLS / auth / policy on Windows must restart the process. The
/// rest of the hot-reload plumbing is still wired in case
/// a future release adds a Windows-native reload channel (named
/// pipe, file-watch, or an HTTP admin endpoint).
#[cfg(windows)]
pub fn install_shutdown_handlers() {
    // `ctrlc::set_handler` returns Err on a second install — match
    // the Unix "idempotent" semantics by swallowing that.
    let _ = ctrlc::set_handler(|| {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    });
}

/// Stub for any other target (wasm, unknown, …). Keeps compilation
/// green without pretending to handle signals.
#[cfg(not(any(unix, windows)))]
pub fn install_shutdown_handlers() {
    // no-op
}

#[cfg(unix)]
extern "C" fn shutdown_handler(_sig: libc::c_int) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
extern "C" fn reload_handler(_sig: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// File-based reload trigger (cross-platform — primary Windows SIGHUP
// replacement; also usable on Unix as a second reload channel)
// ---------------------------------------------------------------------------

/// Spawn a background thread that polls `path`'s modification time
/// every 250 ms. Any change flips `RELOAD_REQUESTED` the same way
/// `SIGHUP` does on Unix. The file's **content is ignored** — operators
/// `touch` it, or let the orchestrator (k8s ConfigMap projection,
/// config-mgmt tool, file-deploy) bump it.
///
/// Thread runs until `shutdown` flips. The file is created on first
/// use if absent so operators don't have to pre-provision a dummy
/// file; a creation failure is logged and the thread exits cleanly
/// (the rest of the runtime keeps running).
pub fn spawn_reload_file_watcher(
    path: std::path::PathBuf,
    shutdown: std::sync::Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("agentd-reload-watch".into())
        .spawn(move || {
            if !path.exists()
                && let Err(e) = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&path)
            {
                tracing::warn!(
                    target: "agentd::audit",
                    event = "reload_file.create_failed",
                    path = %path.display(),
                    reason = %format!("{e}"),
                );
                return;
            }
            let mut last = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            tracing::info!(
                target: "agentd::audit",
                event = "reload_file.watching",
                path = %path.display(),
            );
            while !shutdown.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let now = std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                if now.is_some() && now != last {
                    last = now;
                    RELOAD_REQUESTED.store(true, Ordering::SeqCst);
                    tracing::info!(
                        target: "agentd::audit",
                        event = "reload_file.triggered",
                        path = %path.display(),
                    );
                }
            }
        })
        .expect("spawn agent-reload-watch thread")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// All signal tests mutate the same two process-global
    /// `AtomicBool`s; serialize them so parallel test threads don't
    /// stomp each other's `reset()` / assertion windows.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn default_state_is_not_requested() {
        let _g = serial();
        reset();
        assert!(!shutdown_requested());
        assert!(!reload_requested());
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_handler_flips_flag() {
        let _g = serial();
        reset();
        install_shutdown_handlers();
        shutdown_handler(libc::SIGTERM);
        assert!(shutdown_requested());
        assert!(!reload_requested());
        reset();
    }

    #[cfg(unix)]
    #[test]
    fn reload_handler_flips_flag() {
        let _g = serial();
        reset();
        install_shutdown_handlers();
        reload_handler(libc::SIGHUP);
        assert!(reload_requested());
        assert!(!shutdown_requested());
        reset();
    }

    #[cfg(unix)]
    #[test]
    fn clear_reload_clears_only_reload() {
        let _g = serial();
        reset();
        reload_handler(libc::SIGHUP);
        shutdown_handler(libc::SIGTERM);
        assert!(reload_requested());
        assert!(shutdown_requested());
        clear_reload();
        assert!(!reload_requested());
        assert!(shutdown_requested()); // untouched
        reset();
    }

    #[test]
    fn reset_returns_to_clear_state() {
        let _g = serial();
        reset();
        assert!(!shutdown_requested());
        assert!(!reload_requested());
    }

    #[test]
    fn reload_file_watcher_flips_flag_on_touch() {
        use std::sync::Arc;
        use std::time::Duration;
        let _g = serial();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reload.trigger");
        // Pre-create with one mtime, then bump it after starting the
        // watcher so the first poll cycle sees a changed mtime.
        std::fs::write(&path, b"seed").unwrap();

        reset();
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = spawn_reload_file_watcher(path.clone(), shutdown.clone());

        // Give the watcher one poll cycle to snapshot the initial
        // mtime, then touch and wait for it to observe the change.
        std::thread::sleep(Duration::from_millis(350));
        // Some filesystems (tmpfs on Linux) store mtime at
        // nanosecond resolution; others at second. Sleep past the
        // 1-second coarsest case so the post-write mtime differs.
        std::thread::sleep(Duration::from_millis(1_100));
        std::fs::write(&path, b"touch").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline && !reload_requested() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            reload_requested(),
            "reload-file watcher did not flip flag within 3s"
        );

        shutdown.store(true, Ordering::SeqCst);
        let _ = handle.join();
        reset();
    }
}
