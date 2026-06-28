// SPDX-License-Identifier: Apache-2.0
//! Spawning a subagent process. RFC 0009 §re-exec, RFC 0003 §process-group.
//!
//! A subagent is the **same binary re-exec'd** with `AGENTD_SUBAGENT` set, so
//! the one artifact is CLI, supervisor, and subagent. Each child is put in its
//! own **process group** (`setpgid` in `pre_exec`) so the kill ladder can
//! `killpg` a whole subtree (RFC 0003). The supervisor delivers the
//! [`SpawnPayload`] as the first control frame; the child's upward
//! [`AgentMsg`]s are read on a dedicated thread and forwarded — tagged with
//! the child's [`NodeId`] — onto the reactor's single **merged channel**
//! (RFC 0002 §reactor).
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
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

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
    _reader: JoinHandle<()>,
}

/// Spawn a subagent that re-execs `exe` (normally `std::env::current_exe()`),
/// delivering `payload`. Upward messages are forwarded to `events` tagged with
/// `node`.
pub fn spawn(
    exe: &Path,
    payload: &SpawnPayload,
    node: NodeId,
    events: Sender<(NodeId, AgentMsg)>,
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
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                // Own process group → the kill ladder can target the subtree.
                libc::setpgid(0, 0);
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn()?;
    let pgid = child.id() as i32;
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
                        if events.send((node, msg)).is_err() {
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
        _reader: reader,
    })
}

impl Subagent {
    pub fn pid(&self) -> i32 {
        self.child.id() as i32
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
