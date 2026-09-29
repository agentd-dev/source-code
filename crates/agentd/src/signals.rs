// SPDX-License-Identifier: AGPL-3.0-only
//! Signal handling + the self-pipe wakeup.
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
    // Hot-reload request latch. The SIGHUP handler sets it and wakes the
    // reactor; the reactive supervisor consults `reload_requested()` on its next
    // tick (after `health::tick()`, like `draining()`) and runs the
    // validate-first/quiesce/apply choreography, then `clear_reload()`s it. A
    // SIGHUP while DRAINING is ignored (drain wins — checked at the consult site),
    // so this latch can be set-but-never-honoured during a drain, which is fine:
    // the process is exiting. The handler is registered ONLY under the
    // `hot-reload` feature; without it SIGHUP keeps its default disposition.
    static RELOAD: AtomicBool = AtomicBool::new(false);
    // Trigger-attribution latch for the `config.reload_requested` event, whose
    // `trigger` field is either "sighup" or "watch". The inotify file-watch thread
    // sets BOTH `RELOAD` and this flag via `request_reload_from_watch()`; the
    // reactive apply step reads-and-clears it with `take_reload_was_watch()` to
    // pick the trigger string, DEFAULTING to
    // "sighup" when unset (the SIGHUP handler / `request_reload()` never set it).
    // The watcher is a normal thread (not a signal handler), so a plain atomic
    // store is fine — no async-signal-safety constraint here.
    static RELOAD_FROM_WATCH: AtomicBool = AtomicBool::new(false);
    // Reload-in-progress guard: true only while the reactive supervisor is
    // APPLYING a validated reload's diff, so a reader can tell a mid-apply moment
    // (where config values may be half old, half new) from a settled one. Unlike
    // `DRAINING` it is transient — the apply step brackets itself and always
    // clears it, so a reader that refuses work while it is set cannot wedge.
    // Like PAUSED/LAME_DUCK it rides here rather than in a feature-gated module,
    // so any feature can read it without depending on another; only the
    // `hot-reload` apply step ever sets it.
    static RELOADING: AtomicBool = AtomicBool::new(false);
    // Lame-duck override: forces readiness toward NotReady without exiting —
    // set when a drain begins so a load balancer stops sending new work while the
    // tree winds down, and clearable again. NOT a signal. It rides here rather
    // than in a feature-gated module so both the `/readyz` probe (obs::serve,
    // `metrics`) and the A2A control surface (`a2a`) read one process-global
    // truth without either feature depending on the other.
    // Distinct from `DRAINING`: lame-duck never exits.
    static LAME_DUCK: AtomicBool = AtomicBool::new(false);
    // Tree-wide pause state. Like `LAME_DUCK`, it rides here rather than in a
    // feature-gated module so any feature can read one process-global truth
    // without depending on another. Distinct from DRAINING/LAME_DUCK: pause
    // freezes the agentic loops only — it never exits and never touches
    // readiness, so the supervisor reactor and the liveness heartbeat keep
    // running and a paused instance still answers probes.
    static PAUSED: AtomicBool = AtomicBool::new(false);
    // Intelligence all-endpoints-down latch. The model loop runs in a re-exec'd
    // CHILD process that owns its own intel client + circuit-breaker / failover
    // state; the supervisor has NO LLM and no live view of that breaker
    // state. The child therefore reports its reachability UPWARD (an edge-triggered
    // `AgentMsg::IntelHealth` at the breaker/failover seam — on entering all-down
    // and on recovering); the supervisor latches it HERE so the readiness probe
    // and the `agentd_intel_all_down` gauge read ONE truth without a feature
    // dependency (it rides here, not in a feature-gated module, exactly like
    // LAME_DUCK/PAUSED).
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

    /// Async-signal-safe SIGHUP handler: set the RELOAD latch + wake the reactor.
    /// Exactly the SIGTERM pattern (one atomic store + one self-pipe byte); the
    /// heavy lifting (re-load, validate, apply) runs on the reactor thread, never
    /// here — none of it is async-signal-safe. Registered only under the
    /// `hot-reload` feature.
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
        // SIGHUP -> hot reload, only when the feature is built. Without it
        // SIGHUP keeps its default disposition (terminate), so a build that
        // cannot reload never swallows the signal and looks wedged instead.
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

    /// Programmatically request a graceful drain — the SAME one-way latch
    /// SIGTERM sets, plus a wakeup so a blocked reactor begins the drain
    /// choreography promptly. Idempotent and monotonic: a request after drain has
    /// begun is a no-op that never escalates to FORCE. Only a *second* signal
    /// escalates, so a programmatic call can never cut a drain short.
    pub fn request_drain() {
        DRAINING.store(true, Ordering::SeqCst);
        // Reuse the signal-handler wakeup so the reactor leaves its blocking
        // select and runs the drain state machine.
        wake();
    }

    pub fn lame_duck() -> bool {
        LAME_DUCK.load(Ordering::SeqCst)
    }

    /// Set/clear the lame-duck readiness override. `true` forces `/readyz`
    /// NotReady while the supervisor keeps running; `false` clears the override
    /// (readiness then reflects the genuine computed state). No drain, no exit,
    /// reversible.
    pub fn set_lame_duck(on: bool) {
        LAME_DUCK.store(on, Ordering::SeqCst);
    }

    pub fn paused() -> bool {
        PAUSED.load(Ordering::SeqCst)
    }

    /// Set/clear the instance-wide pause state. Reporting-only: the per-session
    /// pause channels do the actual loop suspension, so clearing this flag alone
    /// does not resume anything. Reversible; never exits, never touches
    /// readiness.
    pub fn set_paused(on: bool) {
        PAUSED.store(on, Ordering::SeqCst);
    }

    pub fn intel_all_down() -> bool {
        INTEL_ALL_DOWN.load(Ordering::SeqCst)
    }

    /// Latch the intelligence all-endpoints-down state from a child's upward
    /// `AgentMsg::IntelHealth` report. Returns `true` iff the value
    /// TRANSITIONED (so the supervisor reacts exactly on a breaker enter/exit,
    /// not on every report).
    /// Eventually-consistent / last-child-experience — see the static's doc above.
    pub fn set_intel_all_down(on: bool) -> bool {
        INTEL_ALL_DOWN.swap(on, Ordering::SeqCst) != on
    }

    /// Take and clear the SIGCHLD flag — the reactor then runs the waitpid loop.
    pub fn take_child_exit() -> bool {
        CHILD_EXIT.swap(false, Ordering::SeqCst)
    }

    /// Has a hot reload been requested (SIGHUP)? Read by the reactive
    /// supervisor's tick; cleared with `clear_reload()` once the reload
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

    /// Request a hot reload attributed to the file-watch trigger: set the SAME
    /// RELOAD latch SIGHUP/`request_reload` do, PLUS the watch-attribution flag
    /// the apply step reads to emit `config.reload_requested{trigger:"watch"}`.
    /// Called by the inotify watcher thread; a reactor wakeup follows. Inert on a
    /// build without the reactive reload loop.
    pub fn request_reload_from_watch() {
        RELOAD_FROM_WATCH.store(true, Ordering::SeqCst);
        RELOAD.store(true, Ordering::SeqCst);
        wake();
    }

    /// Take-and-clear the watch-attribution flag: `true` if the pending reload was
    /// set by the file-watch trigger, `false` (the default) for SIGHUP / a
    /// programmatic `request_reload`. The apply step calls this once per reload to
    /// pick the `config.reload_requested` `trigger` string.
    pub fn take_reload_was_watch() -> bool {
        RELOAD_FROM_WATCH.swap(false, Ordering::SeqCst)
    }

    /// Is a validated reload mid-apply? True only between the apply step's
    /// `set_reloading(true)` and `(false)`, so a reader can refuse work that
    /// would otherwise observe a half-applied config.
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

    /// The fallback scan's reach when no listing of the descriptor table can
    /// be read: every number below it is tried. A soft limit of a million —
    /// common in containers — would cost a second of `fcntl` calls per spawn.
    const FD_SCAN_CAP: libc::c_int = 65_536;

    /// Where [`cloexec_by_range`] stops, and whether that is past every
    /// descriptor this process can open (the soft `RLIMIT_NOFILE` is at or
    /// under the cap). Read in the parent: `getrlimit` is not on the list of
    /// calls a child may make between fork and exec.
    pub fn fd_scan_bound() -> (libc::c_int, bool) {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
            return (FD_SCAN_CAP, false);
        }
        if lim.rlim_cur > FD_SCAN_CAP as libc::rlim_t {
            (FD_SCAN_CAP, false)
        } else {
            (lim.rlim_cur as libc::c_int, true)
        }
    }

    /// Set close-on-exec on `fd` if it is open and not yet marked. Two
    /// `fcntl` calls, so a child between fork and exec may make it.
    fn mark_cloexec(fd: libc::c_int) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }

    /// Test-only: behave as a kernel or seccomp profile that refuses
    /// `close_range` does, in this process and in every child it forks from
    /// now on, so the fallbacks are what a test exercises. An atomic, because
    /// a child between fork and exec reads it.
    #[cfg(test)]
    pub static REFUSE_CLOSE_RANGE: AtomicBool = AtomicBool::new(false);

    /// `close_range(first, u32::MAX, CLOSE_RANGE_CLOEXEC)`: true when the
    /// kernel took it. A raw syscall, so a child between fork and exec may
    /// make it.
    fn close_range_cloexec(first: libc::c_uint) -> bool {
        #[cfg(test)]
        if REFUSE_CLOSE_RANGE.load(Ordering::Relaxed) {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    first,
                    libc::c_uint::MAX,
                    libc::CLOSE_RANGE_CLOEXEC,
                )
            };
            rc == 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = first;
            false
        }
    }

    /// One syscall on Linux 5.11+: it sets the flag on every descriptor from
    /// 3 up and closes none of them. False on an older kernel, under a
    /// seccomp profile that refuses it, and off Linux.
    pub fn cloexec_by_close_range() -> bool {
        close_range_cloexec(3)
    }

    /// Whether [`cloexec_by_close_range`] would work here, asked without
    /// marking anything: a range that starts past every possible descriptor
    /// is a no-op the kernel still validates, flag included. A child inherits
    /// its parent's kernel and seccomp filter, so the answer holds for it.
    fn close_range_available() -> bool {
        close_range_cloexec(libc::c_uint::MAX)
    }

    /// The descriptors the kernel lists for this process, from 3 up; `None`
    /// when there is no listing to read — `/proc` unmounted, as it can be for
    /// agentd as PID 1; on Linux `/dev/fd` is a link into `/proc`, so it is
    /// no second path. The listing's own descriptor is in it, already
    /// close-on-exec and closed by the time anyone acts on it. It allocates,
    /// so it is for the process itself, never a child between fork and exec.
    fn listed_fds() -> Option<Vec<libc::c_int>> {
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let entries = std::fs::read_dir(dir).ok()?;
        Some(
            entries
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .filter(|fd| *fd >= 3)
                .collect(),
        )
    }

    /// Mark every descriptor the kernel lists for this process; false when
    /// there is no listing (see [`listed_fds`]), in which case `fcntl`
    /// refusing the listing's own closed descriptor is harmless.
    pub fn cloexec_by_walk() -> bool {
        let Some(fds) = listed_fds() else {
            return false;
        };
        for fd in fds {
            mark_cloexec(fd);
        }
        true
    }

    /// Try every number from 3 below `bound`, open or not. Needs no `/proc`
    /// and makes only `fcntl` calls, so it is the last resort of both the
    /// process and a child between fork and exec.
    pub fn cloexec_by_range(bound: libc::c_int) {
        for fd in 3..bound {
            mark_cloexec(fd);
        }
    }

    /// The one line saying a descriptor numbered at or above `bound` can
    /// still reach a child: what an operator needs to know when nothing could
    /// mark past the scan. The shape of every other log line, because it is
    /// written straight to stderr — at start no logger exists yet, and an
    /// embedder's children are spawned under whatever logger its host has.
    pub fn unmarked_warning(bound: libc::c_int, why: &str) -> serde_json::Value {
        serde_json::json!({
            "level": "warn",
            "event": "process.inherited_fds_unmarked",
            "scanned_below": bound,
            "msg": format!("{why}; a descriptor numbered at or above {bound} keeps no close-on-exec"),
        })
    }

    pub fn cloexec_inherited_fds() {
        if cloexec_by_close_range() || cloexec_by_walk() {
            return;
        }
        let (bound, complete) = fd_scan_bound();
        cloexec_by_range(bound);
        if !complete {
            eprintln!(
                "{}",
                unmarked_warning(
                    bound,
                    "close_range and /proc are unavailable and the descriptor limit exceeds the scan"
                )
            );
        }
    }

    /// How far a child's fallback scan must reach, decided in the parent —
    /// the child may neither read a directory nor ask for its limit — and
    /// whether that reach is past every descriptor it could inherit.
    ///
    /// The capped limit is enough when the limit is under the cap, and
    /// irrelevant when `close_range` works, since the child then marks
    /// everything in one call. Otherwise a descriptor the host numbered past
    /// the cap would reach the child unmarked, so the scan runs to just past
    /// the highest one the kernel lists instead. A descriptor another host
    /// thread opens inheritable between that listing and the fork can still
    /// slip past it: the listing is the best a parent can know. With no
    /// listing either, the reach stays capped and says so.
    pub fn child_scan_bound(
        (bound, complete): (libc::c_int, bool),
        close_range: impl FnOnce() -> bool,
        listed: impl FnOnce() -> Option<Vec<libc::c_int>>,
    ) -> (libc::c_int, bool) {
        if complete || close_range() {
            return (bound, true);
        }
        match listed() {
            Some(fds) => (
                fds.into_iter()
                    .map(|fd| fd.saturating_add(1))
                    .fold(bound, libc::c_int::max),
                true,
            ),
            None => (bound, false),
        }
    }

    /// One warning per process, however many children it spawns: the
    /// condition is the host's and does not change between them.
    static UNMARKED_WARNED: std::sync::Once = std::sync::Once::new();

    pub fn pass_only_stdio(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        let (bound, complete) =
            child_scan_bound(fd_scan_bound(), close_range_available, listed_fds);
        if !complete {
            // `cloexec_inherited_fds` reports the same gap only in the agentd
            // binary, at start; an embedder's host never runs it, so the
            // spawn site is the one place every process passes through.
            UNMARKED_WARNED.call_once(|| {
                eprintln!(
                    "{}",
                    unmarked_warning(
                        bound,
                        "a child is spawned where close_range and /proc are unavailable and the descriptor limit exceeds the scan"
                    )
                );
            });
        }
        // SAFETY: between fork and exec the closure makes only raw syscalls
        // (close_range, fcntl) and touches no heap. std has placed the
        // child's stdio on 0-2 before any closure runs, and its own
        // exec-error pipe is close-on-exec already, so marking — never
        // closing — leaves std's report of a failed exec intact.
        unsafe {
            cmd.pre_exec(move || {
                if !cloexec_by_close_range() {
                    cloexec_by_range(bound);
                }
                Ok(())
            });
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
    pub fn cloexec_inherited_fds() {}
    pub fn pass_only_stdio(_cmd: &mut std::process::Command) {}
}

/// Mark every descriptor this process inherited beyond its stdio
/// close-on-exec. Call first thing at process start, before anything is opened
/// that is meant to be handed on.
///
/// Whatever started agentd may have left descriptors open without the flag —
/// a CI runner's pipes, a shell's redirections, a supervisor's sockets — and
/// every process agentd spawns (the exec tool, subagents, instances, a
/// launched display client) would otherwise inherit them: a pipe held open by
/// a stranger never reaches end-of-file, and a descriptor is a capability the
/// child was never granted. agentd itself keeps using them; only a later
/// `exec` drops them. Nothing agentd hands a child arrives by inheritance
/// from its own parent: the launcher's fd 3 is one it creates and places
/// between fork and exec, so marking everything is the whole rule.
///
/// `close_range` does it in one call on Linux 5.11+; elsewhere the kernel's
/// listing of the table is walked; with neither (an old kernel or a seccomp
/// refusal, and no `/proc`) every number below the descriptor limit is tried,
/// capped at 65536 — and past the cap one `process.inherited_fds_unmarked`
/// warning on stderr says a higher descriptor was left as it was.
///
/// This covers the agentd binary's own processes. A child spawned through
/// the library is covered however its host started — see
/// [`pass_only_stdio`].
pub fn cloexec_inherited_fds() {
    imp::cloexec_inherited_fds();
}

/// Arrange for `cmd`'s child to keep, across its `exec`, its stdio and no
/// other descriptor it would inherit: every descriptor from 3 up is marked
/// close-on-exec in the child, between fork and exec.
///
/// [`cloexec_inherited_fds`] runs only where the agentd binary's `main` does;
/// an embedder of this crate spawns the exec tool, subagents and instances
/// through its own `main`, and a descriptor its host left inheritable would
/// reach each of them. Marking in the child covers every spawn site whoever
/// started the process, and leaves the host's own descriptors alone. A site
/// that hands the child a descriptor beyond its stdio places it in a
/// `pre_exec` registered after this one: closures run in order, and a
/// `dup2` copy carries no close-on-exec.
///
/// The child uses `close_range` where the kernel allows it; otherwise it
/// tries each number below a bound the parent chose — the descriptor limit
/// capped at 65536, or past the highest descriptor `/proc/self/fd` lists
/// when the limit is higher. With neither `close_range` nor `/proc` and a
/// limit over the cap, the first such spawn writes one
/// `process.inherited_fds_unmarked` warning on stderr.
pub fn pass_only_stdio(cmd: &mut std::process::Command) {
    imp::pass_only_stdio(cmd);
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

/// Request a graceful drain programmatically — the same one-way `DRAINING` latch
/// SIGTERM sets, plus a reactor wakeup. Idempotent and monotonic; never
/// escalates to FORCE (only a second signal does), so a caller cannot cut an
/// in-flight drain short.
pub fn request_drain() {
    imp::request_drain()
}

/// Is the lame-duck readiness override active? When true,
/// `/readyz` reports NotReady even though the supervisor keeps running.
pub fn lame_duck() -> bool {
    imp::lame_duck()
}

/// Set or clear the lame-duck readiness override. `true` overrides readiness
/// toward NotReady; `false` clears it. The reactor sets it when a drain begins,
/// so traffic stops arriving while the tree winds down.
pub fn set_lame_duck(on: bool) {
    imp::set_lame_duck(on)
}

/// Is the instance-wide pause active? When true, the agentic
/// loops are suspended at their turn boundaries; the supervisor and readiness
/// are unaffected.
pub fn paused() -> bool {
    imp::paused()
}

/// Set or clear the instance-wide pause state. Reporting-only — the per-session
/// pause channels perform the actual suspension.
pub fn set_paused(on: bool) {
    imp::set_paused(on)
}

/// Is the intelligence channel all-endpoints-down? The latched,
/// EVENTUALLY-CONSISTENT last-child-experience truth a child reports up via
/// `AgentMsg::IntelHealth` — read by `/readyz` (flips NotReady) and the
/// `agentd_intel_all_down` gauge. NOT a live supervisor-side probe (there is no model loop in the
/// supervisor): it reflects whichever child last exercised the endpoints.
pub fn intel_all_down() -> bool {
    imp::intel_all_down()
}

/// Latch the intelligence all-endpoints-down state from a child's `AgentMsg::
/// IntelHealth` report. Returns `true` iff the value TRANSITIONED,
/// so the supervisor can react exactly on a breaker enter/exit. Eventually-consistent / last-child-experience: a fresh
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

/// Has a hot reload been requested (SIGHUP)? The reactive supervisor consults
/// this each tick; a drain supersedes it (the caller checks
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

/// Request a hot reload attributed to the **file-watch** trigger: the same
/// RELOAD latch SIGHUP/`request_reload` set, plus the watch-attribution flag the
/// apply step reads to emit `config.reload_requested{trigger:"watch"}`. Called by
/// the inotify watcher thread (`config-watch`).
pub fn request_reload_from_watch() {
    imp::request_reload_from_watch()
}

/// Take-and-clear the watch-attribution flag — `true` if the pending reload came
/// from the file-watch trigger, `false` (the default) for SIGHUP or a
/// programmatic `request_reload`. The reactive apply step calls this once per
/// reload to label the `config.reload_requested` `trigger`.
pub fn take_reload_was_watch() -> bool {
    imp::take_reload_was_watch()
}

/// Is a validated reload mid-apply? True only while the reloadable diff is being
/// written into the live runtime, so a reader can refuse work that would
/// otherwise observe a half-applied config. Always `false` off the `hot-reload`
/// path (only the reactive apply step ever sets it).
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
// `INTEL_ALL_DOWN` are process-global, so tests that touch them race and poison
// each other when cargo runs them in parallel within one test binary (e.g. a
// drain test leaves `DRAINING` set, breaking every later readiness assertion).
// Every test that reads OR writes this state takes `test_guard()`: it serializes
// them on one mutex and resets the state to a clean slate for the test body.
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

/// A stray descriptor for the spawn-site tests: both ends of a pipe opened
/// without close-on-exec, as a careless host would leave one, and a way to
/// tell whether a child's `ls -l /proc/self/fd` shows it. Marking touches the
/// whole descriptor table, so every test that plants a stray or marks the
/// table holds [`stray_fd::lock`]: one test's marking would otherwise make
/// another's stray close-on-exec before its child was spawned.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod stray_fd {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    static FD_TABLE: Mutex<()> = Mutex::new(());

    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        FD_TABLE.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) struct StrayPipe {
        pub(crate) fds: [libc::c_int; 2],
        ino: u64,
    }

    impl StrayPipe {
        /// A fresh pipe from plain `pipe(2)`: both ends inheritable.
        pub(crate) fn open() -> StrayPipe {
            let mut fds = [0 as libc::c_int; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::fstat(fds[0], &mut st) }, 0, "fstat");
            let p = StrayPipe {
                fds,
                ino: st.st_ino,
            };
            assert!(
                fds.iter().all(|fd| !cloexec(*fd)),
                "the stray starts inheritable"
            );
            p
        }

        /// Whether an `ls -l /proc/self/fd` listing holds either end.
        pub(crate) fn held_in(&self, listing: &str) -> bool {
            listing.contains(&format!("pipe:[{}]", self.ino))
        }
    }

    impl Drop for StrayPipe {
        fn drop(&mut self) {
            for fd in self.fds {
                unsafe { libc::close(fd) };
            }
        }
    }

    pub(crate) fn cloexec(fd: libc::c_int) -> bool {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "fd {fd} is open");
        flags & libc::FD_CLOEXEC != 0
    }

    /// A script in a fresh directory that lists the descriptors it holds on
    /// its stdout and ignores its arguments, for a site whose command line
    /// the test does not choose.
    pub(crate) fn lister(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agentd-fds-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("ls-fds.sh");
        std::fs::write(&script, "#!/bin/sh\nexec ls -l /proc/self/fd\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Run `cmd` to completion. A script just written can refuse to exec
    /// while a concurrent test's fork still holds the writer's descriptor
    /// (`ETXTBSY`) until that child execs; that passes.
    pub(crate) fn run(cmd: &mut std::process::Command) -> std::process::Output {
        for _ in 0..100 {
            match cmd.output() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                r => return r.expect("run the lister"),
            }
        }
        panic!("the lister stayed busy");
    }

    /// Run `cmd` to completion and return its stdout.
    pub(crate) fn listing(cmd: &mut std::process::Command) -> String {
        String::from_utf8_lossy(&run(cmd).stdout).into()
    }

    pub(crate) fn cleanup(script: &Path) {
        if let Some(dir) = script.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod fd_tests {
    use super::stray_fd::{StrayPipe, cloexec, lock};
    use std::process::{Command, Stdio};

    /// The walk is what an older kernel, or a seccomp profile that refuses
    /// `close_range`, relies on; called directly because `close_range`
    /// succeeds on every kernel the tests run on.
    #[test]
    fn the_walk_marks_an_inheritable_pipe() {
        let _g = lock();
        let stray = StrayPipe::open();
        assert!(super::imp::cloexec_by_walk(), "/proc/self/fd is readable");
        for fd in stray.fds {
            assert!(cloexec(fd), "fd {fd} is close-on-exec after the walk");
        }
    }

    /// With neither `close_range` nor `/proc`, trying every number below
    /// the limit needs nothing but `fcntl`.
    #[test]
    fn the_range_fallback_marks_an_inheritable_pipe_without_proc() {
        let _g = lock();
        let stray = StrayPipe::open();
        let (bound, _) = super::imp::fd_scan_bound();
        assert!(
            stray.fds.iter().all(|fd| *fd < bound),
            "the scan reaches the pipe"
        );
        super::imp::cloexec_by_range(bound);
        for fd in stray.fds {
            assert!(cloexec(fd), "fd {fd} is close-on-exec after the range");
        }
    }

    #[test]
    fn the_scan_bound_is_the_soft_limit_capped() {
        let (bound, complete) = super::imp::fd_scan_bound();
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
        assert_eq!(bound as libc::rlim_t, lim.rlim_cur.min(65_536));
        assert_eq!(complete, lim.rlim_cur <= 65_536);
    }

    /// A child spawned with [`super::pass_only_stdio`] holds no stray the
    /// process never marked — while the same child without it does, so the
    /// stray was there to leak.
    #[test]
    fn a_child_spawned_with_pass_only_stdio_holds_no_stray() {
        let _g = lock();
        let stray = StrayPipe::open();
        let run = |hygiene: bool| {
            let mut cmd = Command::new("ls");
            cmd.args(["-l", "/proc/self/fd"]).stdin(Stdio::null());
            if hygiene {
                super::pass_only_stdio(&mut cmd);
            }
            super::stray_fd::listing(&mut cmd)
        };
        assert!(stray.held_in(&run(false)), "a plain spawn inherits it");
        let listing = run(true);
        assert!(!stray.held_in(&listing), "{listing}");
        for fd in stray.fds {
            assert!(!cloexec(fd), "the parent's own descriptors are untouched");
        }
    }

    /// Refuses `close_range` for as long as it lives, however the test ends.
    struct NoCloseRange;
    impl NoCloseRange {
        fn on() -> NoCloseRange {
            super::imp::REFUSE_CLOSE_RANGE.store(true, std::sync::atomic::Ordering::SeqCst);
            NoCloseRange
        }
    }
    impl Drop for NoCloseRange {
        fn drop(&mut self) {
            super::imp::REFUSE_CLOSE_RANGE.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// What an older kernel, or a seccomp profile that refuses `close_range`,
    /// leaves the child: its own scan between fork and exec. `close_range`
    /// succeeds on every kernel the tests run on, so it is refused here, or
    /// the scan would never be what the test exercises.
    #[test]
    fn a_child_spawned_where_close_range_is_refused_holds_no_stray() {
        let _g = lock();
        let stray = StrayPipe::open();
        let _refused = NoCloseRange::on();
        let mut cmd = Command::new("ls");
        cmd.args(["-l", "/proc/self/fd"]).stdin(Stdio::null());
        super::pass_only_stdio(&mut cmd);
        let listing = super::stray_fd::listing(&mut cmd);
        assert!(!stray.held_in(&listing), "{listing}");
    }

    /// The child's scan reaches past every descriptor the parent could hand
    /// it, whatever the limit: past the highest one the kernel lists when the
    /// limit is over the cap and `close_range` is refused — and it admits to
    /// falling short only when there is no listing either.
    #[test]
    fn a_childs_scan_reaches_past_every_listed_descriptor() {
        use super::imp::child_scan_bound;
        let never = || -> Option<Vec<libc::c_int>> { panic!("no listing is needed") };
        // A limit under the cap is the whole table.
        assert_eq!(
            child_scan_bound((1024, true), || false, never),
            (1024, true)
        );
        // `close_range` marks everything in the child; the bound is moot.
        assert_eq!(
            child_scan_bound((65_536, false), || true, never),
            (65_536, true)
        );
        // Over the cap without it: past the highest listed descriptor…
        assert_eq!(
            child_scan_bound((65_536, false), || false, || Some(vec![3, 9, 70_000])),
            (70_001, true)
        );
        // …never below the cap…
        assert_eq!(
            child_scan_bound((65_536, false), || false, || Some(vec![3, 9])),
            (65_536, true)
        );
        // …and with no listing, capped and incomplete: what the warning says.
        assert_eq!(
            child_scan_bound((65_536, false), || false, || None),
            (65_536, false)
        );
        let w = super::imp::unmarked_warning(65_536, "why");
        assert_eq!(w["event"], "process.inherited_fds_unmarked");
        assert_eq!(w["level"], "warn");
        assert_eq!(w["scanned_below"], 65_536);
    }
}
