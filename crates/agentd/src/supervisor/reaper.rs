// SPDX-License-Identifier: AGPL-3.0-only
//! The process-global child reaper.
//!
//! `waitpid(-1)` is process-global — it reaps *any* child (including
//! `PR_SET_CHILD_SUBREAPER` orphans), so two threads each calling it would steal
//! each other's children (the robbed one then waits forever for an exit another
//! thread already collected).
//!
//! The fix is **dispatch by pid**: exactly one place drains `waitpid(-1)` and
//! routes each reaped pid to the owning `Supervisor`'s channel, so any number of
//! Supervisors run **concurrently** without stealing from each other.
//!
//! Two operations, both under a global pid→route registry mutex:
//!  * [`spawn_tracked`] forks the child **under the lock**, then registers its
//!    pid → the owner's reap channel — so registration is atomic with the fork
//!    and the reaper can never `waitpid` a not-yet-registered child.
//!  * [`reap_and_dispatch`] (called from each Supervisor's tick) drains
//!    `waitpid(-1, WNOHANG)` and sends each reaped pid to its owner; an **unowned**
//!    pid (an adopted orphan, or a *foreign* child whose owner never needs its
//!    status) is simply dropped — already reaped, no owner.
//!
//! There is **no dedicated reaper thread**: reaping happens only while a
//! Supervisor is active (its 200 ms tick drives it). So a child's owner cannot
//! count on a reaper to collect its exit either — with no reactor (a
//! subagent's own loop, an embedder that runs none) nothing ever would —
//! which is why [`OwnedChild`] collects its own pid too.
//!
//! **What `waitpid(-1)` reaps (the real coexistence contract).** Because it is
//! process-global, `reap_and_dispatch` reaps *every* exited child in the process,
//! not just tracked ones — including a daemon's long-lived MCP-server children, a
//! warm session's subagent, and adopted orphans — and it does so from **whichever
//! supervisor happens to tick**, concurrently with any other thread (a workflow's
//! `tool:exec` thread, an embedder's own). A plain `Child::try_wait` there loses
//! the race whenever a tick lands between the child's exit and the poll: the
//! status is gone and the poll answers `ECHILD`. So a component whose child may
//! exit while a reactor ticks gets its exit status in one of two ways:
//!
//!  * **It needs the status** — the `exec` tool's command, the launcher's display
//!    client: the child is spawned through [`spawn_owned`] / [`spawn_tracked_pid`],
//!    so the pid is routed from the fork on, and the status arrives on the route
//!    whichever side collected it. [`OwnedChild`] also collects it itself, under
//!    the same lock, when no reactor ticks at all.
//!  * **It does not** — a subagent's `Subagent::kill`/`Drop`, and every component
//!    that detects its child's death through its own channel (the warm/async
//!    `AgentMsg` channel): the child is torn down and its status discarded, and
//!    the teardown's own `child.wait()` tolerates the `ECHILD` it answers when a
//!    reaper collected the pid first. There is nothing for the reaper to rob.
//!
//! A foreign `child.wait()` is `waitpid(specific_pid)` and so can never steal a
//! *tracked* child (a different, still-live pid).

use crate::supervisor::reap::{self, Reaped, WaitOutcome};
use crate::supervisor::spawn::Subagent;
use std::collections::HashMap;
use std::io;
use std::process::{ChildStderr, ChildStdin, ChildStdout};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// pid → the owning Supervisor's reap channel. Holds only LIVE (unreaped)
/// supervised pids; an entry leaves when its pid is reaped (dispatched) or when
/// its handle is dropped unreaped ([`deregister`]).
static ROUTES: LazyLock<Mutex<HashMap<i32, Sender<Reaped>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
/// serialize on each spawn. The hold is bounded by child startup (the child
/// drains its stdin pipe within a few ms of `exec`), never by the length of a
/// run, so spawn contention stays in the millisecond range.
pub fn spawn_tracked(
    reap_tx: &Sender<Reaped>,
    spawn_fn: impl FnOnce() -> io::Result<Subagent>,
) -> io::Result<Subagent> {
    let mut routes = routes();
    let sub = spawn_fn()?;
    routes.insert(sub.pid(), reap_tx.clone());
    Ok(sub)
}

/// [`spawn_tracked`] for a plain [`std::process::Child`] — an instance-tier
/// child, which is a full daemon with no control channel. Same contract: the
/// fork happens under the routes lock so the reaper can never `waitpid` the pid
/// before it is registered.
pub fn spawn_tracked_pid(
    reap_tx: &Sender<Reaped>,
    spawn_fn: impl FnOnce() -> io::Result<std::process::Child>,
) -> io::Result<std::process::Child> {
    let mut routes = routes();
    let child = spawn_fn()?;
    routes.insert(child.id() as i32, reap_tx.clone());
    Ok(child)
}

/// How often an [`OwnedChild`] with no dispatch looks for its exit itself. A
/// reactor's dispatch wakes the waiter at once; this bounds how long an exit
/// waits when no reactor ticks.
const OWNED_POLL: Duration = Duration::from_millis(15);

/// A child whose spawner needs its exit status — the `exec` tool's command.
///
/// Its pid is routed to its own channel from the fork on ([`spawn_owned`]), and
/// the status is taken in exactly one of two places, both under the routes
/// lock: [`reap_and_dispatch`] (which removes the route and sends the status
/// before it lets go of the lock), or [`OwnedChild`]'s own
/// `waitpid(pid, WNOHANG)`, which runs only while the route is still present
/// and removes it on success. So neither side can lose the status or reap it
/// twice, and a waiter that finds the route gone finds the status already on
/// its channel. The std [`Child`](std::process::Child) is kept only for its
/// pipes and never waited on: its `try_wait` is the unlocked
/// `waitpid(pid)` this type exists to replace.
pub struct OwnedChild {
    child: std::process::Child,
    pid: i32,
    exit: Receiver<Reaped>,
    outcome: Option<WaitOutcome>,
}

/// Spawn a child whose exit status its spawner reads ([`OwnedChild`]). As in
/// [`spawn_tracked`], `spawn_fn`'s fork happens under the routes lock, so no
/// reaper can `waitpid` the pid before it is routed. The parent only holds the
/// lock; the forked child never touches it, so whatever `pre_exec` work the
/// command carries (`signals::pass_only_stdio`'s marking) runs as before.
pub fn spawn_owned(
    spawn_fn: impl FnOnce() -> io::Result<std::process::Child>,
) -> io::Result<OwnedChild> {
    let (tx, exit) = mpsc::channel();
    let mut routes = routes();
    let child = spawn_fn()?;
    let pid = child.id() as i32;
    routes.insert(pid, tx);
    Ok(OwnedChild {
        child,
        pid,
        exit,
        outcome: None,
    })
}

impl OwnedChild {
    pub fn id(&self) -> i32 {
        self.pid
    }
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// The exit status if the child has exited, without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<WaitOutcome>> {
        if self.outcome.is_some() {
            return Ok(self.outcome);
        }
        let mut routes = routes();
        // The reaper sends under this lock, so a status it dispatched is on
        // the channel by now.
        if let Ok(r) = self.exit.try_recv() {
            self.outcome = Some(r.outcome);
            return Ok(self.outcome);
        }
        // Not dispatched: the route is still present and, while this lock is
        // held, nobody else reaps the pid.
        let mut status: libc::c_int = 0;
        match unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } {
            0 => Ok(None),
            p if p == self.pid => {
                routes.remove(&self.pid);
                self.outcome = Some(reap::classify_status(status));
                Ok(self.outcome)
            }
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    // Only a waitpid outside this registry takes a routed pid.
                    Err(e)
                }
            }
        }
    }

    /// Wait for the exit until `deadline` (`None`: for as long as it takes).
    /// `Ok(None)` means the deadline passed with the child still running.
    pub fn wait_until(&mut self, deadline: Option<Instant>) -> io::Result<Option<WaitOutcome>> {
        loop {
            if let Some(o) = self.try_wait()? {
                return Ok(Some(o));
            }
            let nap = match deadline {
                None => OWNED_POLL,
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Ok(None);
                    }
                    (d - now).min(OWNED_POLL)
                }
            };
            // Parked on the route: a reactor's dispatch ends the nap at once.
            // A disconnect (the route removed with nothing sent) ends it too,
            // and the next `try_wait` names it.
            if let Ok(r) = self.exit.recv_timeout(nap) {
                self.outcome = Some(r.outcome);
                return Ok(self.outcome);
            }
        }
    }

    /// SIGKILL the child if it has not been reaped. A pid still routed is
    /// this child's, live or a zombie, and cannot be reaped (and so reused)
    /// while the lock is held; once the route is gone the pid may already
    /// name another process, so it is not signalled.
    pub fn kill(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        let routes = routes();
        if routes.contains_key(&self.pid) {
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

impl Drop for OwnedChild {
    /// An owned child is never left running or unreaped: with no reactor,
    /// nothing else would collect it.
    fn drop(&mut self) {
        if self.outcome.is_none() {
            self.kill();
            if self.wait_until(None).is_err() {
                deregister(self.pid);
            }
        }
    }
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
            let _ = tx.send(Reaped {
                pid,
                outcome: WaitOutcome::Exited(0),
            });
        }
        let got = rx.try_recv().expect("the route received the reap");
        assert_eq!(got.pid, pid);
        assert!(got.outcome.is_clean());
        assert!(
            routes().get(&pid).is_none(),
            "the route is removed on dispatch"
        );
    }
}
