//! The process-global child reaper. RFC 0003 §pid1-orphan.
//!
//! `waitpid(-1)` is process-global — it reaps *any* child (including
//! `PR_SET_CHILD_SUBREAPER` orphans), so two threads each calling it would steal
//! each other's children (the robbed one then waits forever for an exit another
//! thread already collected). The old design serialized every supervised run
//! behind a process-wide `SUPERVISE_LOCK` to keep a single `waitpid(-1)` caller.
//!
//! This module replaces that lock with **dispatch by pid**: one place drains
//! `waitpid(-1)` and routes each reaped pid to the owning `Supervisor`'s
//! channel, so any number of Supervisors run **concurrently** without stealing.
//!
//! Two operations, both under a global pid→route registry mutex:
//!  * [`spawn_tracked`] forks the child **under the lock**, then registers its
//!    pid → the owner's reap channel — so registration is atomic with the fork
//!    and the reaper can never `waitpid` a not-yet-registered child.
//!  * [`reap_and_dispatch`] (called from each Supervisor's tick) drains
//!    `waitpid(-1, WNOHANG)` and sends each reaped pid to its owner; an **unowned**
//!    pid (an adopted orphan, or a *foreign* child such as an MCP-server / `exec`
//!    process that reaps itself) is simply dropped — already reaped, no owner.
//!
//! There is **no dedicated reaper thread**: reaping happens only while a
//! Supervisor is active (its 200 ms tick drives it). That is deliberate — a
//! continuous `waitpid(-1)` would steal the children of components that
//! spawn-and-wait their own (the `exec` self-tool, the MCP client).
//!
//! **What `waitpid(-1)` reaps (the real coexistence contract).** Because it is
//! process-global, `reap_and_dispatch` reaps *every* exited child in the process,
//! not just tracked ones — including a daemon's long-lived MCP-server children, a
//! warm session's subagent, and adopted orphans — and it does so from **whichever
//! supervisor happens to tick**, possibly concurrently with the main thread. That
//! is safe **not** because these never overlap (in the daemon they do, once a
//! served async run is in flight) but because every such component detects its
//! child's death via its own channel (MCP stdout EOF, the warm/async `AgentMsg`
//! channel) and its `Drop` tolerates `ECHILD` — none of them needs the reaped
//! exit *status*, which is what `waitpid(-1)` consumes. The one component that
//! *does* consume a child's status, the `exec` tool, runs only on a subagent's
//! single agentic-loop thread where no reactor ticks concurrently. A foreign
//! `child.wait()` is `waitpid(specific_pid)` and so can never steal a *tracked*
//! supervised child (a different, still-live pid).

use crate::supervisor::reap::{self, Reaped};
use crate::supervisor::spawn::Subagent;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex, MutexGuard};

/// pid → the owning Supervisor's reap channel. Holds only LIVE (unreaped)
/// supervised pids; an entry leaves when its pid is reaped (dispatched) or when
/// its handle is dropped unreaped ([`deregister`]).
static ROUTES: LazyLock<Mutex<HashMap<i32, Sender<Reaped>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn routes() -> MutexGuard<'static, HashMap<i32, Sender<Reaped>>> {
    ROUTES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Spawn a supervised child and register its pid → `reap_tx` **atomically with
/// the fork** (both under the routes lock), so the reaper can never `waitpid` a
/// child before it is registered. `spawn_fn` does the fork and returns the
/// [`Subagent`] whose `pid()` is the registry key.
///
/// The lock is held across all of `spawn_fn` — the fork, the first-frame payload
/// write, and the reader-thread spawn — so concurrent supervisors briefly
/// serialize on each spawn (bounded by child startup; the child drains its stdin
/// pipe within a few ms of `exec`). This is far cheaper than the retired
/// `SUPERVISE_LOCK` (held for a whole *run*, possibly seconds); moving the payload
/// write outside the lock is a possible follow-on if spawn contention is ever
/// measured to matter.
pub fn spawn_tracked(
    reap_tx: &Sender<Reaped>,
    spawn_fn: impl FnOnce() -> io::Result<Subagent>,
) -> io::Result<Subagent> {
    let mut routes = routes();
    let sub = spawn_fn()?;
    routes.insert(sub.pid(), reap_tx.clone());
    Ok(sub)
}

/// Drain `waitpid(-1, WNOHANG)` and dispatch each reaped pid to its owning
/// Supervisor. Unowned pids (orphans / foreign self-reaping children) are
/// dropped. Called from each Supervisor's tick; the lock keeps the single
/// `waitpid(-1)` serialized across concurrent Supervisors.
pub fn reap_and_dispatch() {
    let mut routes = routes();
    for reaped in reap::reap_pending() {
        if let Some(tx) = routes.remove(&reaped.pid) {
            let _ = tx.send(reaped); // the owner may be gone — harmless
        }
        // else: an adopted orphan or a foreign self-reaping child (MCP server /
        // warm session / async child) — already reaped here, no route, no owner
        // that needs its exit status (each detects death via its own channel).
    }
}

/// Drop a pid's route without reaping it — for a [`Subagent`] handle dropped
/// before the reaper dispatched its exit (an abandoned run, which then reaps the
/// child itself). Harmless if the pid is absent (a foreign / already-reaped pid).
pub fn deregister(pid: i32) {
    routes().remove(&pid);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::reap::WaitOutcome;
    use std::sync::mpsc;

    #[test]
    fn deregister_of_an_unknown_pid_is_a_noop() {
        deregister(-12345); // must not panic / must tolerate a foreign pid
    }

    #[test]
    fn a_registered_route_receives_a_dispatched_reap() {
        // Drive the registry directly (no real fork): register a synthetic pid,
        // then simulate the reaper dispatching its exit.
        let (tx, rx) = mpsc::channel::<Reaped>();
        let pid = -98765; // a pid waitpid(-1) will never return — isolates this test
        routes().insert(pid, tx);
        // Simulate dispatch (what reap_and_dispatch does on a real exit).
        if let Some(tx) = routes().remove(&pid) {
            let _ = tx.send(Reaped { pid, outcome: WaitOutcome::Exited(0) });
        }
        let got = rx.try_recv().expect("the route received the reap");
        assert_eq!(got.pid, pid);
        assert!(got.outcome.is_clean());
        assert!(routes().get(&pid).is_none(), "the route is removed on dispatch");
    }
}
