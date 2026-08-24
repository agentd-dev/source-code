// SPDX-License-Identifier: AGPL-3.0-only
//! Spawning a subagent process.
//!
//! A subagent is the **same binary re-exec'd** with `AGENT_SUBAGENT` set, so
//! the one artifact is CLI, supervisor, and subagent. Each child is put in its
//! own **process group** (`setpgid` in `pre_exec`) so the kill ladder can
//! `killpg` a whole subtree in one call, including grandchildren the subagent
//! forked itself. The supervisor delivers the [`SpawnPayload`] as the first
//! control frame; the child's upward [`AgentMsg`]s are read on a dedicated
//! thread and forwarded — tagged with the child's [`NodeId`] — onto the
//! reactor's single **merged channel**, which is what lets the reactor stay
//! single-threaded no matter how many children are live.
//!
//! Teardown is **reap-safe**: once the reactor has reaped a child via
//! `waitpid(-1)` it calls [`Subagent::mark_reaped`], so `Drop` will not signal
//! a possibly-reused pid.

use crate::json::frame;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SUBAGENT_ENV, SpawnPayload};
use crate::supervisor::kill::kill_group;
use crate::supervisor::tree::NodeId;
use std::io;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

/// A handle to a running subagent process and the down side of its control
/// channel. Upward messages arrive on the reactor's merged channel, not here.
pub struct Subagent {
    pub node: NodeId,
    child: Child,
    writer: ChildStdin,
    /// Process-group id for `killpg` (== child pid; the child is its own group
    /// leader after `setpgid(0, 0)`).
    pgid: i32,
    /// Set once the reactor has reaped this child — suppresses Drop signalling.
    reaped: bool,
    /// The stdout reader thread. Joinable: after the child exits its pipe EOFs
    /// and the reader finishes promptly — the reactor joins it before acting on
    /// a reap, so every frame the child wrote is in the event queue first.
    reader: Option<JoinHandle<()>>,
    /// The child's own cgroup leaf (`security.cgroup`), held for its lifetime —
    /// its Drop writes `cgroup.kill` + removes the leaf (the atomic teardown
    /// backstop). `None` when cgroups are not configured.
    _cgroup: Option<crate::supervisor::cgroup::CgroupGuard>,
}

/// Where a child's upward frames land. The reader thread calls this for every
/// frame, **directly into the queue the consumer drains** — no intermediate
/// hop, because ordering with other producers (the reap path) is established
/// by joining the reader, and a hop thread would break that happens-before.
/// Return `false` when the consumer is gone (stops the reader).
pub type FrameSink = std::sync::Arc<dyn Fn(NodeId, AgentMsg) -> bool + Send + Sync>;

/// Spawn a subagent that re-execs `exe` (normally `std::env::current_exe()`),
/// delivering `payload`. Upward messages are handed to `events` tagged with
/// `node`.
pub fn spawn(
    exe: &Path,
    payload: &SpawnPayload,
    node: NodeId,
    events: FrameSink,
) -> io::Result<Subagent> {
    let mut cmd = Command::new(exe);
    cmd.env(SUBAGENT_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Child telemetry (JSON to its stderr) is inherited into ours; the
        // control channel is stdout (binary frames).
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Copy the OS caps out of the payload: the pre_exec closure runs
        // between fork and exec and may only touch plain values.
        let mem = payload.limits.memory_bytes;
        let cpu = payload.limits.cpu_seconds;
        let nice = payload.limits.nice;
        // SAFETY: only async-signal-safe calls between fork and exec
        // (setpgid/setrlimit/setpriority all are).
        unsafe {
            cmd.pre_exec(move || {
                // Own process group → the kill ladder can target the subtree.
                libc::setpgid(0, 0);
                if let Some(bytes) = mem {
                    let lim = libc::rlimit {
                        rlim_cur: bytes as libc::rlim_t,
                        rlim_max: bytes as libc::rlim_t,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some(secs) = cpu {
                    // Soft cap = the declared budget (SIGXCPU); hard cap 5 s
                    // later (SIGKILL) so a child ignoring SIGXCPU still dies.
                    let lim = libc::rlimit {
                        rlim_cur: secs as libc::rlim_t,
                        rlim_max: secs.saturating_add(5) as libc::rlim_t,
                    };
                    if libc::setrlimit(libc::RLIMIT_CPU, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if let Some(n) = nice {
                    // Lowering priority always works; raising needs
                    // CAP_SYS_NICE — best-effort by design, never an error.
                    let _ = libc::setpriority(libc::PRIO_PROCESS, 0, n);
                }
                Ok(())
            });
        }
    }

    // Spawn, retrying a transient `EAGAIN` — the kernel refusing a `fork` under
    // process/memory pressure (a wide fan-out starting many subagents at once, or
    // a CPU-starved host). Bounded (~1s total, short backoff); a genuine error
    // (ENOENT, EMFILE-persisted, …) still surfaces. Real robustness, not just a
    // test artifact: a busy agent tree hits the same refusal.
    let mut child = {
        let mut attempt = 0u32;
        loop {
            match cmd.spawn() {
                Ok(c) => break c,
                Err(e)
                    if attempt < 10
                        && (e.raw_os_error() == Some(libc::EAGAIN)
                            || e.kind() == io::ErrorKind::WouldBlock) =>
                {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(u64::from(20 * attempt)));
                }
                Err(e) => return Err(e),
            }
        }
    };
    let pgid = child.id() as i32;
    // Place the child in its own cgroup leaf (best-effort; `None` unless
    // `security.cgroup` armed the parent). The guard is held on the Subagent so
    // teardown (`cgroup.kill` + rmdir) fires when the child is reaped.
    let cgroup = crate::supervisor::cgroup::CgroupGuard::for_run().inspect(|g| {
        g.place(pgid);
    });
    let mut writer = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("no child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("no child stdout"))?;

    // Deliver the spawn payload as the first control frame.
    frame::write_frame(&mut writer, &ControlMsg::Spawn(Box::new(payload.clone())))?;

    let reader = std::thread::Builder::new()
        .name(format!("subagent-events:{}", node.0))
        .spawn(move || {
            let mut r = io::BufReader::new(stdout);
            // Exits on Ok(None) (clean EOF) or Err (child closed stdout/exited).
            while let Ok(Some(bytes)) = frame::read_frame(&mut r) {
                match serde_json::from_slice::<AgentMsg>(&bytes) {
                    Ok(msg) => {
                        if !events(node, msg) {
                            break; // reactor dropped the channel
                        }
                    }
                    Err(_) => { /* skip an unparseable frame */ }
                }
            }
        })?;

    Ok(Subagent {
        node,
        child,
        writer,
        pgid,
        reaped: false,
        reader: Some(reader),
        _cgroup: cgroup,
    })
}

impl Subagent {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
    }
    /// Wait for the stdout reader to finish (bounded: the pipe has EOF'd once
    /// the child is reapable). After this, every frame the child ever wrote has
    /// been forwarded.
    pub fn join_reader(&mut self) {
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// Send a control message down (Ping / Cancel / Inject).
    pub fn send(&mut self, msg: &ControlMsg) -> io::Result<()> {
        frame::write_frame(&mut self.writer, msg)
    }

    /// Mark that the reactor already reaped this child (via `waitpid(-1)`), so
    /// teardown won't signal a possibly-reused pid.
    pub fn mark_reaped(&mut self) {
        self.reaped = true;
    }

    /// Immediate, unconditional teardown of the whole process group. The
    /// graceful ladder (cancel → SIGTERM → SIGKILL over time) is driven by the
    /// reactor via `kill.rs`; this is the backstop.
    pub fn kill(&mut self) {
        if !self.reaped {
            crate::supervisor::reaper::deregister(self.pid());
            kill_group(self.pgid);
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

impl Drop for Subagent {
    fn drop(&mut self) {
        if !self.reaped {
            // Drop a never-dispatched route (an abandoned run), then tear down +
            // reap the child ourselves. `child.wait()` tolerates ECHILD if the
            // global reaper collected it first; deregistering first means it sees
            // this pid as foreign rather than routing a stale exit.
            crate::supervisor::reaper::deregister(self.pid());
            kill_group(self.pgid);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
