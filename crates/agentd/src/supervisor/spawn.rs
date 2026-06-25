//! Spawning a subagent process. RFC 0009 §re-exec, RFC 0003 §process-group.
//!
//! A subagent is the **same binary re-exec'd** with `AGENTD_SUBAGENT` set, so
//! the one artifact is CLI, supervisor, and subagent. Each child is put in its
//! own **process group** (`setpgid` in `pre_exec`) so the kill ladder can
//! `killpg` a whole subtree (RFC 0003). The supervisor delivers the
//! [`SpawnPayload`] as the first control frame and reads upward [`AgentMsg`]s
//! on a dedicated thread.
//!
//! This wake provides spawn + result-wait + immediate kill; the full
//! three-detector liveness model and the graceful SIGTERM→SIGKILL ladder land
//! in `liveness.rs`/`kill.rs`.

use crate::agentloop::stop::Outcome;
use crate::json::frame;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload, SUBAGENT_ENV};
use crate::supervisor::tree::NodeId;
use std::io;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How a subagent run ended, from the supervisor's view.
#[derive(Debug)]
pub enum Terminal {
    Result(Outcome),
    Failed(String),
    /// The deadline passed with no terminal message (the child is wedged or
    /// slow — the caller runs the kill ladder).
    Timeout,
}

/// A handle to a running subagent process and its control channel.
pub struct Subagent {
    pub node: NodeId,
    child: Child,
    writer: ChildStdin,
    events: Receiver<AgentMsg>,
    /// Process-group id for `killpg` (== child pid; the child is its own group
    /// leader after `setpgid(0, 0)`).
    pgid: i32,
    _reader: JoinHandle<()>,
}

/// Spawn a subagent that re-execs `exe` (normally `std::env::current_exe()`),
/// delivering `payload`. `node` is the supervisor-minted tree node.
pub fn spawn(exe: &Path, payload: &SpawnPayload, node: NodeId) -> io::Result<Subagent> {
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
    let mut writer = child.stdin.take().ok_or_else(|| io::Error::other("no child stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no child stdout"))?;

    // Deliver the spawn payload as the first control frame.
    frame::write_frame(&mut writer, &ControlMsg::Spawn(Box::new(payload.clone())))?;

    let (tx, events) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name(format!("subagent-events:{}", node.0))
        .spawn(move || {
            let mut r = io::BufReader::new(stdout);
            // Exits on Ok(None) (clean EOF) or Err (child closed stdout/exited).
            while let Ok(Some(bytes)) = frame::read_frame(&mut r) {
                match serde_json::from_slice::<AgentMsg>(&bytes) {
                    Ok(msg) => {
                        if tx.send(msg).is_err() {
                            break; // supervisor dropped the handle
                        }
                    }
                    Err(_) => { /* skip an unparseable frame */ }
                }
            }
        })?;

    Ok(Subagent { node, child, writer, events, pgid, _reader: reader })
}

impl Subagent {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// Send a control message down (Ping / Cancel / Inject).
    pub fn send(&mut self, msg: &ControlMsg) -> io::Result<()> {
        frame::write_frame(&mut self.writer, msg)
    }

    /// Receive the next upward message, if one arrives within `dur`.
    pub fn recv_timeout(&self, dur: Duration) -> Option<AgentMsg> {
        self.events.recv_timeout(dur).ok()
    }

    /// Block until the child reports a terminal status, the channel closes, or
    /// `deadline` passes. Intermediate messages (Ready/Pong/Event/Usage) are
    /// consumed; richer handling (no-progress watchdog, usage rollup) is the
    /// reactor's job in a later wake.
    pub fn wait_terminal(&self, deadline: Instant) -> Terminal {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Terminal::Timeout;
            }
            let slice = (deadline - now).min(Duration::from_millis(250));
            match self.events.recv_timeout(slice) {
                Ok(AgentMsg::Result { outcome }) => return Terminal::Result(outcome),
                Ok(AgentMsg::Failed { error }) => return Terminal::Failed(error),
                Ok(_) => {} // Ready / Pong / Event / Usage — keep waiting
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Terminal::Failed("subagent channel closed without a result".into());
                }
            }
        }
    }

    /// Immediate, unconditional teardown of the whole process group. The
    /// graceful ladder (cancel → SIGTERM → SIGKILL) lands in `kill.rs`.
    pub fn kill(&mut self) {
        kill_group(self.pgid);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Subagent {
    fn drop(&mut self) {
        kill_group(self.pgid);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn kill_group(pgid: i32) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pgid: i32) {}
