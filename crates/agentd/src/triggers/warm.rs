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

use crate::agentloop::stop::{Outcome, TerminalStatus};
use crate::obs::log::Logger;
use crate::subagent::protocol::{AgentMsg, ControlMsg, SpawnPayload, SwapIntel};
use crate::supervisor::spawn::{Subagent, spawn};
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

/// What one [`WarmRegistry::drain`] pass yields: the completed turns to apply
/// self-* effects for, and the sessions that ENDED this pass (with their
/// terminal disposition) so a continue-claim holder can ack/release the lease
/// keyed by the session (RFC 0019 §3.4).
pub struct WarmDrain {
    /// `(session_id, outcome)` for each completed turn (a warm session stays
    /// alive after a turn — self-schedule / self-subscribe effects are applied).
    pub turns: Vec<(String, Outcome)>,
    /// `(session_id, terminal)` for each session that ended this pass.
    /// `Some(status)` from a terminal `Result` (status drives ack vs release);
    /// `None` from a `Failed` / channel disconnect (no clean completion → the
    /// holder releases so the item is immediately re-claimable).
    pub ended: Vec<(String, Option<TerminalStatus>)>,
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
            w.sub.send(&ControlMsg::Inject {
                message: event.to_string(),
            })?;
            log.info(
                "warm.inject",
                json!({"session": session_id, "bytes": event.len()}),
            );
            return Ok(false);
        }
        payload.warm = true;
        let node = NodeId(self.next_node);
        self.next_node += 1;
        let (tx, rx) = mpsc::channel();
        let sub = spawn(exe, &payload, node, tx)?;
        self.sessions
            .insert(session_id.to_string(), Warm { sub, rx });
        log.info(
            "warm.spawned",
            json!({"session": session_id, "node": node.0}),
        );
        Ok(true)
    }

    /// Drain a tick's frames from every live session, returning a
    /// [`WarmDrain`]: each completed turn's `(session_id, outcome)` for the daemon
    /// to apply self-* effects, AND each ENDED session's `(session_id, terminal)`
    /// so a continue-claim holder can ack (terminal `completed`) or release
    /// (anything else) the lease keyed by that session (RFC 0019 §3.4). A session
    /// ends on a terminal `Result{outcome}` (its status is the terminal one), a
    /// `Failed{..}` (infra death — no clean completion → `None` → release), or a
    /// channel disconnect (the process died → `None` → release). Reaps every
    /// ended session.
    pub fn drain(&mut self, log: &Logger) -> WarmDrain {
        let mut turns = Vec::new();
        // (session_id, terminal-status-if-clean): Some(status) from a terminal
        // Result; None from a Failed / disconnect (no terminal outcome → release).
        let mut ended: Vec<(String, Option<TerminalStatus>)> = Vec::new();
        let mut dead = Vec::new();
        for (id, w) in self.sessions.iter_mut() {
            loop {
                match w.rx.try_recv() {
                    Ok((_, AgentMsg::Turn { outcome })) => {
                        log.info(
                            "warm.turn",
                            json!({"session": id, "status": outcome.status.as_str()}),
                        );
                        turns.push((id.clone(), outcome));
                    }
                    Ok((_, AgentMsg::Result { outcome })) => {
                        log.info(
                            "warm.ended",
                            json!({"session": id, "status": outcome.status.as_str()}),
                        );
                        ended.push((id.clone(), Some(outcome.status)));
                        dead.push(id.clone());
                        break;
                    }
                    Ok((_, AgentMsg::Failed { .. })) => {
                        log.info("warm.ended", json!({"session": id, "status": "failed"}));
                        ended.push((id.clone(), None)); // infra death → release
                        dead.push(id.clone());
                        break;
                    }
                    // RFC 0018 §6: a warm session reports its intel reachability;
                    // latch it into the process-global readiness/gauge/resource truth
                    // (eventually-consistent, last-child-experience). A warm-only
                    // daemon has no `supervise_once` reaction to latch this, so it must
                    // be latched here too. The notify-then-read fires from the served
                    // `LiveConfig` path; this daemon-held drain has no subs registry.
                    Ok((_, AgentMsg::IntelHealth { all_down, .. })) => {
                        if crate::signals::set_intel_all_down(all_down) {
                            crate::obs::metrics::set_intel_all_down(all_down);
                            log.info("intel.health", json!({"session": id, "all_down": all_down}));
                        }
                    }
                    // Per-turn token usage rolled up from the warm child (RFC 0003
                    // §hierarchical-accounting). A warm-only daemon has no
                    // `supervise_once` reactor to consume this, so — like IntelHealth
                    // above — it is recorded HERE into the frozen `agentd_tokens_total`
                    // (+ legacy token counters). One `AgentMsg::Usage` per emitted turn
                    // is a DELTA, and `record_tokens` fetch_adds, so no double-count.
                    Ok((_, AgentMsg::Usage(usage))) => {
                        crate::obs::metrics::record_tokens(usage.input_tokens, usage.output_tokens);
                    }
                    Ok(_) => {} // Ready / Pong / Event — progress, ignore here
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        log.warn("warm.died", json!({"session": id}));
                        ended.push((id.clone(), None)); // process died → release
                        dead.push(id.clone());
                        break;
                    }
                }
            }
        }
        for id in &dead {
            self.sessions.remove(id); // Subagent::Drop kills + reaps (ECHILD-tolerant)
        }
        WarmDrain { turns, ended }
    }

    /// Fan an intelligence hot-swap (RFC 0018 §5.2) to every live warm session —
    /// the reactive-daemon reach of a reload that repoints the endpoint list /
    /// changes the model. Each session's control thread parks it; the session's
    /// loop applies it at its next turn boundary (rebuild client + adopt model;
    /// transcript untouched, §5.3). The parallel of `cancel_all`'s fan, with a
    /// payload — `w.sub.send(ControlMsg::SwapIntel)`. Returns the count reached.
    /// The `token` rides the frame (like the spawn payload) but is NEVER logged.
    pub fn fan_swap_intel(&mut self, swap: &SwapIntel, log: &Logger) -> u64 {
        let mut reached = 0u64;
        let msg = ControlMsg::SwapIntel(Box::new(swap.clone()));
        for (id, w) in self.sessions.iter_mut() {
            if w.sub.send(&msg).is_ok() {
                reached += 1;
                log.info("warm.swap_intel", json!({"session": id}));
            }
        }
        reached
    }

    /// Begin graceful teardown: ask every session to wind down. The warm loop
    /// ends on the cancel and emits a terminal `Result`, then the process exits
    /// (collected by a subsequent [`WarmRegistry::drain`] or by [`Self::clear`]).
    pub fn cancel_all(&mut self, log: &Logger) {
        for (id, w) in self.sessions.iter_mut() {
            let _ = w.sub.send(&ControlMsg::Cancel {
                reason: "drain".into(),
            });
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

    #[test]
    fn drain_of_an_empty_registry_yields_no_turns_and_no_ended() {
        // The continue-claim settle pass keys off `ended`; an empty registry must
        // produce an empty `ended` (nothing to ack/release), not panic.
        let mut r = WarmRegistry::default();
        let d = r.drain(&test_logger());
        assert!(d.turns.is_empty());
        assert!(d.ended.is_empty());
    }

    fn test_logger() -> Logger {
        use crate::obs::log::{Comp, Level, LogCtx};
        Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 0,
                trace_id: None,
            },
            Level::Error,
        )
    }
}
