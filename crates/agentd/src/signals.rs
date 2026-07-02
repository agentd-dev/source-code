// SPDX-License-Identifier: Apache-2.0
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
    // Hot-reload request latch (RFC 0017 §5.2). The SIGHUP handler sets it +
    // wakes the reactor; the reactive supervisor consults `reload_requested()`
    // on its next tick (after `health::tick()`, like `draining()`) and runs the
    // validate-first/quiesce/apply choreography, then `clear_reload()`s it. A
    // SIGHUP while DRAINING is ignored (drain wins — checked at the consult site),
    // so this latch can be set-but-never-honoured during a drain, which is fine:
    // the process is exiting. The handler is registered ONLY under the
    // `hot-reload` feature; without it SIGHUP keeps its default disposition.
    static RELOAD: AtomicBool = AtomicBool::new(false);
    // Trigger-attribution latch for the `config.reload_requested` event (RFC 0017
    // §5.6 — `{trigger:"sighup"|"watch"}`). The inotify file-watch thread
    // (RFC 0017 §5.2) sets BOTH `RELOAD` and this flag via
    // `request_reload_from_watch()`; the reactive apply step reads-and-clears it
    // with `take_reload_was_watch()` to pick the trigger string, DEFAULTING to
    // "sighup" when unset (the SIGHUP handler / `request_reload()` never set it).
    // The watcher is a normal thread (not a signal handler), so a plain atomic
    // store is fine — no async-signal-safety constraint here.
    static RELOAD_FROM_WATCH: AtomicBool = AtomicBool::new(false);
    // Reload-in-progress guard (RFC 0017 §5.3 step 3). Set while the reactive
    // supervisor is APPLYING a validated reload; the served `subagent.spawn`
    // chokepoint consults it and returns a transient "reload in progress" error to
    // NEW spawns (mirrors the `draining` guard, but transient — cleared in step 6).
    // Like PAUSED/LAME_DUCK it rides here (not a feature-gated module) so it is one
    // process-global truth the served surface reads without a feature dependency;
    // it is only ever SET by the `hot-reload` reactive apply step.
    static RELOADING: AtomicBool = AtomicBool::new(false);
    // Lame-duck override (RFC 0015 §4.2): a one-way-per-call readiness override
    // toward NotReady, flipped by the `a2a.LameDuck` admin method — NOT a signal.
    // It rides here (not in a feature-gated module) so it is one process-global
    // truth consulted by BOTH the `/readyz` probe (obs::serve, `metrics`) and the
    // served control surface (mcp::server, `a2a`), with neither feature depending
    // on the other. Distinct from `DRAINING`: lame-duck never exits.
    static LAME_DUCK: AtomicBool = AtomicBool::new(false);
    // Tree-wide pause state (RFC 0015 §4.3): set by the `a2a.Pause` admin method,
    // cleared by `a2a.Resume`. Like `LAME_DUCK`, it rides here (not a feature-gated
    // module) so it is one process-global truth read by BOTH the served operator
    // surface (`agentd://inventory`, `serve-mcp`) and the `agentd_paused` gauge
    // (`metrics`), with neither feature depending on the other. Distinct from
    // DRAINING/LAME_DUCK: pause freezes the agentic loops only — never exits, never
    // touches readiness (the supervisor reactor and liveness heartbeat run on).
    static PAUSED: AtomicBool = AtomicBool::new(false);
    // Intelligence all-endpoints-down latch (RFC 0018 §6). The model loop runs in
    // a re-exec'd CHILD process that owns its own intel client + circuit-breaker /
    // failover state; the supervisor has NO LLM and no live view of that breaker
    // state. The child therefore reports its reachability UPWARD (an edge-triggered
    // `AgentMsg::IntelHealth` at the breaker/failover seam — on entering all-down
    // and on recovering); the supervisor latches it HERE so the readiness probe,
    // the `agentd_intel_all_down` gauge, and the `agentd://intelligence`/`capacity`
    // bodies all read ONE truth without a feature dependency (it rides here, not in
    // a feature-gated module, exactly like LAME_DUCK/PAUSED).
    //
    // SEMANTICS (be honest): this is EVENTUALLY-CONSISTENT, last-child-experience.
    // A fresh subagent spawn starts with FRESH breakers (all CLOSED), so the latched
    // flag reflects the MOST RECENT child's intel reachability and persists between
    // reactions — it is the right "should the fleet route work to this pod" signal,
    // but it is NOT a continuous supervisor-side probe of the endpoints. There is no
    // model loop in the supervisor to probe with; the truth comes from whichever
    // child last exercised the endpoints. Distinct from DRAINING/LAME_DUCK (which an
    // operator/SIGTERM set): this is set by the data path (a child's failover).
    static INTEL_ALL_DOWN: AtomicBool = AtomicBool::new(false);
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

    /// Async-signal-safe SIGHUP handler (RFC 0017 §5.2): set the RELOAD latch +
    /// wake the reactor. Exactly the SIGTERM pattern (one atomic store + one
    /// self-pipe byte); the heavy lifting (re-load, validate, apply) runs on the
    /// reactor thread, never here. Registered only under the `hot-reload` feature.
    #[cfg(feature = "hot-reload")]
    extern "C" fn on_hup(_sig: libc::c_int) {
        RELOAD.store(true, Ordering::SeqCst);
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
        // SIGHUP → hot reload (RFC 0017 §5.2), only when the feature is built.
        // Without it SIGHUP keeps its default disposition (terminate) — exactly
        // the RFC 0011 §4.1 signal table (this is the *one* amendment, gated).
        #[cfg(feature = "hot-reload")]
        {
            let hup = on_hup as extern "C" fn(libc::c_int) as libc::sighandler_t;
            set_handler(libc::SIGHUP, hup, 0);
        }
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

    pub fn intel_all_down() -> bool {
        INTEL_ALL_DOWN.load(Ordering::SeqCst)
    }

    /// Latch the intelligence all-endpoints-down state from a child's upward
    /// `AgentMsg::IntelHealth` report (RFC 0018 §6). Returns `true` iff the value
    /// TRANSITIONED (so the supervisor fires the `agentd://intelligence`
    /// notify-then-read exactly on a breaker enter/exit, not on every report).
    /// Eventually-consistent / last-child-experience — see the static's doc above.
    pub fn set_intel_all_down(on: bool) -> bool {
        INTEL_ALL_DOWN.swap(on, Ordering::SeqCst) != on
    }

    /// Take and clear the SIGCHLD flag — the reactor then runs the waitpid loop.
    pub fn take_child_exit() -> bool {
        CHILD_EXIT.swap(false, Ordering::SeqCst)
    }

    /// Has a hot reload been requested (SIGHUP, RFC 0017 §5.2)? Read by the
    /// reactive supervisor's tick; cleared with `clear_reload()` once the reload
    /// routine has run (whether it applied or was rejected — both consume the
    /// request). Always readable, but only ever SET under the `hot-reload`
    /// feature (the handler is the only setter besides `request_reload`).
    pub fn reload_requested() -> bool {
        RELOAD.load(Ordering::SeqCst)
    }

    /// Clear the hot-reload latch (after the reload routine has run, or when a
    /// drain supersedes it). Idempotent.
    pub fn clear_reload() {
        RELOAD.store(false, Ordering::SeqCst);
    }

    /// Programmatically request a hot reload (parity with `request_drain` — for
    /// a future `reload` operator tool / tests), plus a reactor wakeup. Honoured
    /// only by a `hot-reload` build's reactive loop; a no-feature build never
    /// consults the latch, so this is inert there.
    pub fn request_reload() {
        RELOAD.store(true, Ordering::SeqCst);
        wake();
    }

    /// Request a hot reload attributed to the file-watch trigger (RFC 0017 §5.2):
    /// set the SAME RELOAD latch SIGHUP/`request_reload` do, PLUS the
    /// watch-attribution flag the apply step reads for the `config.reload_requested`
    /// `{trigger:"watch"}` event (§5.6). Called by the inotify watcher thread; a
    /// reactor wakeup follows. Inert on a build without the reactive reload loop.
    pub fn request_reload_from_watch() {
        RELOAD_FROM_WATCH.store(true, Ordering::SeqCst);
        RELOAD.store(true, Ordering::SeqCst);
        wake();
    }

    /// Take-and-clear the watch-attribution flag: `true` if the pending reload was
    /// set by the file-watch trigger (RFC 0017 §5.2), `false` (the default) for
    /// SIGHUP / a programmatic `request_reload`. The apply step calls this once per
    /// reload to pick the `config.reload_requested` `trigger` string (§5.6).
    pub fn take_reload_was_watch() -> bool {
        RELOAD_FROM_WATCH.swap(false, Ordering::SeqCst)
    }

    /// Is a validated reload mid-apply (RFC 0017 §5.3 step 3)? The served
    /// `subagent.spawn` chokepoint reads this and transiently refuses NEW spawns.
    pub fn reloading() -> bool {
        RELOADING.load(Ordering::SeqCst)
    }

    /// Set/clear the reload-in-progress guard (the reactive apply step brackets
    /// its reloadable-diff application with `set_reloading(true)`/`(false)`).
    pub fn set_reloading(on: bool) {
        RELOADING.store(on, Ordering::SeqCst);
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

    /// Test-only: clear the one-way `DRAINING`/`FORCE` latches (production has no
    /// clear — drain is monotonic for a process's life). The signals test guard
    /// uses this so a draining test cannot poison readiness for later tests that
    /// share this process (cargo runs tests multithreaded in one binary).
    #[cfg(test)]
    pub fn clear_drain_for_test() {
        DRAINING.store(false, Ordering::SeqCst);
        FORCE.store(false, Ordering::SeqCst);
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
    #[cfg(test)]
    pub fn clear_drain_for_test() {}
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
    pub fn intel_all_down() -> bool {
        false
    }
    pub fn set_intel_all_down(_on: bool) -> bool {
        false
    }
    pub fn take_child_exit() -> bool {
        false
    }
    pub fn reload_requested() -> bool {
        false
    }
    pub fn clear_reload() {}
    pub fn request_reload() {}
    pub fn request_reload_from_watch() {}
    pub fn take_reload_was_watch() -> bool {
        false
    }
    pub fn reloading() -> bool {
        false
    }
    pub fn set_reloading(_on: bool) {}
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

/// Is the intelligence channel all-endpoints-down (RFC 0018 §6)? The latched,
/// EVENTUALLY-CONSISTENT last-child-experience truth a child reports up via
/// `AgentMsg::IntelHealth` — read by `/readyz` (flips NotReady), the
/// `agentd_intel_all_down` gauge, and the `agentd://intelligence`/`capacity`
/// bodies. NOT a live supervisor-side probe (there is no model loop in the
/// supervisor): it reflects whichever child last exercised the endpoints.
pub fn intel_all_down() -> bool {
    imp::intel_all_down()
}

/// Latch the intelligence all-endpoints-down state from a child's `AgentMsg::
/// IntelHealth` report (RFC 0018 §6). Returns `true` iff the value TRANSITIONED,
/// so the supervisor can fire the `agentd://intelligence` notify exactly on a
/// breaker enter/exit. Eventually-consistent / last-child-experience: a fresh
/// spawn has fresh breakers, so this reflects the most recent child's reachability
/// and persists between reactions — the right "route work here?" signal, not a
/// continuous probe.
pub fn set_intel_all_down(on: bool) -> bool {
    imp::set_intel_all_down(on)
}

/// Take-and-clear the SIGCHLD flag — true if a child exited since last checked.
pub fn take_child_exit() -> bool {
    imp::take_child_exit()
}

/// Has a hot reload been requested (SIGHUP, RFC 0017 §5.2)? The reactive
/// supervisor consults this each tick; a drain supersedes it (the caller checks
/// `draining()` first). Always `false` on a build without the `hot-reload`
/// feature (the handler that sets it is feature-gated).
pub fn reload_requested() -> bool {
    imp::reload_requested()
}

/// Clear the hot-reload latch once the reload routine has run (applied or
/// rejected), or when a drain supersedes the request. Idempotent.
pub fn clear_reload() {
    imp::clear_reload()
}

/// Programmatically request a hot reload (the same RELOAD latch SIGHUP sets) +
/// a reactor wakeup. Parity with `request_drain`; honoured only by a
/// `hot-reload` build's reactive loop.
pub fn request_reload() {
    imp::request_reload()
}

/// Request a hot reload attributed to the **file-watch** trigger (RFC 0017 §5.2):
/// the same RELOAD latch SIGHUP/`request_reload` set, plus the watch-attribution
/// flag the apply step reads for the `config.reload_requested{trigger:"watch"}`
/// event (§5.6). Called by the inotify watcher thread (`config-watch`).
pub fn request_reload_from_watch() {
    imp::request_reload_from_watch()
}

/// Take-and-clear the watch-attribution flag — `true` if the pending reload came
/// from the file-watch trigger (RFC 0017 §5.2), `false` (the default) for SIGHUP
/// or a programmatic `request_reload`. The reactive apply step calls this once per
/// reload to label the `config.reload_requested` `trigger` (§5.6).
pub fn take_reload_was_watch() -> bool {
    imp::take_reload_was_watch()
}

/// Is a validated reload mid-apply (RFC 0017 §5.3 step 3)? The served
/// `subagent.spawn` chokepoint reads this to transiently refuse NEW spawns while
/// the reloadable diff is being applied. Always `false` off the `hot-reload` path
/// (only the reactive apply step ever sets it).
pub fn reloading() -> bool {
    imp::reloading()
}

/// Set or clear the reload-in-progress guard. The reactive apply step brackets
/// its reloadable-diff application with `set_reloading(true)` then `(false)`.
pub fn set_reloading(on: bool) {
    imp::set_reloading(on)
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

// ── Test isolation for the process-global signal state ──────────────────────
// `DRAINING` is a one-way latch and `PAUSED`/`LAME_DUCK`/`RELOADING`/
// `INTEL_ALL_DOWN` are process-global, so tests that touch them race + poison
// each other when cargo
// runs them in parallel within one test binary (e.g. a drain test leaves
// `DRAINING` set, breaking every later readiness assertion). Every test that
// reads OR writes this state takes `test_guard()`: it serializes them on one
// mutex and resets the state to a clean slate for the test body.
#[cfg(test)]
static SIGNALS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reset every process-global signal latch to its initial (unset) state.
/// Test-only; called under the [`test_guard`] lock.
#[cfg(test)]
pub fn reset_for_test() {
    imp::clear_drain_for_test();
    set_lame_duck(false);
    set_paused(false);
    set_reloading(false);
    clear_reload();
    // The intel all-down latch is process-global too (set by a child's IntelHealth
    // report); clear it so an all-down readiness/gauge test cannot poison a later
    // readiness test sharing this process.
    let _ = set_intel_all_down(false);
    // Clear the watch-attribution latch too (set by `request_reload_from_watch`),
    // so a watcher test cannot leak `trigger:"watch"` into a later reload test.
    let _ = take_reload_was_watch();
}

/// RAII guard from [`test_guard`]. Resets the signal state on BOTH acquire and
/// drop — the drop reset runs while the mutex is still held (the inner
/// `MutexGuard` field drops after this `Drop::drop`), so a test that latches
/// `DRAINING` cannot leak it to the next test between lock-release and the next
/// acquire's reset.
#[cfg(test)]
pub struct SignalsTestGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

#[cfg(test)]
impl Drop for SignalsTestGuard {
    fn drop(&mut self) {
        reset_for_test();
    }
}

/// Serialize + clean-slate a test that touches the process-global signal state.
/// `let _g = crate::signals::test_guard();` at the top of the test, held for the
/// whole body, so no other signals-touching test interleaves. State is reset on
/// entry AND on drop (under the lock), so nothing leaks across tests. Recovers a
/// poisoned lock (a panicking test should not wedge the rest of the suite).
#[cfg(test)]
pub fn test_guard() -> SignalsTestGuard {
    let g = SIGNALS_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    reset_for_test();
    SignalsTestGuard(g)
}
