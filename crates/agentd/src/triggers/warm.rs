//! Daemon-held **warm continue-sessions** (RFC 0008 §spawn-vs-continue).
//!
//! A `Disposition::Continue(session_id)` route delivers every event on its URI
//! into ONE live warm subagent, in order (single-consumer FIFO) — the agent
//! re-enters the same conversation each event instead of a fresh spawn per
//! event. The reactive daemon owns these handles and supervises them
//! **non-blocking**: it spawns a warm session on the first event (the event baked
//! into the payload), forwards each subsequent event as an [`ControlMsg::Inject`],
//! and every tick drains each session's upward [`AgentMsg::Turn`] frames.
//!
//! **Lifecycle / reaping.** Each session has its own upward channel, so a
//! session's death is detected by that channel **disconnecting** (its reader
//! thread saw EOF) or by a terminal `Result`/`Failed` — no `waitpid` in the
//! daemon to conflict with a reaction's reactor. Reaping is via [`Subagent`]'s
//! `Drop` when the session is removed; if a concurrent reaction's reactor already
//! reaped the (now-orphaned) pid via `waitpid(-1)`, that is a benign
//! `reap_unknown` and `Drop`'s `wait` simply no-ops. This is safe precisely
//! because the reactive daemon is single-threaded (events are processed
//! serially), so the daemon never races itself.

use crate::agentloop::stop::Outcome;
use crate::obs::log::Logger;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload};
use crate::supervisor::spawn::{spawn, Subagent};
use crate::supervisor::tree::NodeId;
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// One live warm session: the subagent handle plus its dedicated upward channel.
struct Warm {
    sub: Subagent,
    rx: Receiver<(NodeId, AgentMsg)>,
}

/// The set of live warm continue-sessions, keyed by route `session_id`.
#[derive(Default)]
pub struct WarmRegistry {
    sessions: HashMap<String, Warm>,
    next_node: u64,
}

impl WarmRegistry {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Deliver an event into the `session_id` session: spawn a fresh warm
    /// session (the event already baked into `payload`) if none is live, else
    /// inject `event` into the existing one (in-order). Returns `true` if a new
    /// session was spawned.
    pub fn deliver(
        &mut self,
        exe: &Path,
        session_id: &str,
        mut payload: SpawnPayload,
        event: &str,
        log: &Logger,
    ) -> io::Result<bool> {
        if let Some(w) = self.sessions.get_mut(session_id) {
            w.sub.send(&ControlMsg::Inject { message: event.to_string() })?;
            log.info("warm.inject", json!({"session": session_id, "bytes": event.len()}));
            return Ok(false);
        }
        payload.warm = true;
        let node = NodeId(self.next_node);
        self.next_node += 1;
        let (tx, rx) = mpsc::channel();
        let sub = spawn(exe, &payload, node, tx)?;
        self.sessions.insert(session_id.to_string(), Warm { sub, rx });
        log.info("warm.spawned", json!({"session": session_id, "node": node.0}));
        Ok(true)
    }

    /// Drain a tick's frames from every live session: return each completed
    /// turn's `(session_id, outcome)` for the daemon to apply self-* effects,
    /// and reap any session that ended (terminal `Result`/`Failed`, or its
    /// channel disconnected = the process died).
    pub fn drain(&mut self, log: &Logger) -> Vec<(String, Outcome)> {
        let mut turns = Vec::new();
        let mut dead = Vec::new();
        for (id, w) in self.sessions.iter_mut() {
            loop {
                match w.rx.try_recv() {
                    Ok((_, AgentMsg::Turn { outcome })) => {
                        log.info("warm.turn", json!({"session": id, "status": outcome.status.as_str()}));
                        turns.push((id.clone(), outcome));
                    }
                    Ok((_, AgentMsg::Result { .. })) | Ok((_, AgentMsg::Failed { .. })) => {
                        log.info("warm.ended", json!({"session": id}));
                        dead.push(id.clone());
                        break;
                    }
                    Ok(_) => {} // Ready / Pong / Event / Usage — progress, ignore here
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        log.warn("warm.died", json!({"session": id}));
                        dead.push(id.clone());
                        break;
                    }
                }
            }
        }
        for id in &dead {
            self.sessions.remove(id); // Subagent::Drop kills + reaps (ECHILD-tolerant)
        }
        turns
    }

    /// Begin graceful teardown: ask every session to wind down. The warm loop
    /// ends on the cancel and emits a terminal `Result`, then the process exits
    /// (collected by a subsequent [`WarmRegistry::drain`] or by [`Self::clear`]).
    pub fn cancel_all(&mut self, log: &Logger) {
        for (id, w) in self.sessions.iter_mut() {
            let _ = w.sub.send(&ControlMsg::Cancel { reason: "drain".into() });
            log.info("warm.cancel", json!({"session": id}));
        }
    }

    /// Drop every session handle — kills + reaps any group still live.
    pub fn clear(&mut self) {
        self.sessions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_sessions() {
        let r = WarmRegistry::default();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }
}
