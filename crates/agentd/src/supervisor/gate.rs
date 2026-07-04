// SPDX-License-Identifier: Apache-2.0
//! The human-gate channel for one supervised run (RFC 0021 §7).
//!
//! A workflow child that reaches a `human` node suspends and reports the gate UP
//! the control channel (`AgentMsg::Gate`); the reply travels DOWN as
//! `ControlMsg::Inject`. The run's reactor owns the child processes, so — exactly
//! like [`super::swap::SwapChannel`] for hot-swaps and the shared `Arc<AtomicBool>`
//! for pause/cancel — this shared slot is how the served surface and the reactor
//! meet:
//!
//! - the **reactor** sets [`GateChannel::open`] on `AgentMsg::Gate` (recording
//!   WHICH child node opened it) and clears it on `AgentMsg::GateClosed`;
//! - the **served surface** reads the open gate (the A2A task projects
//!   `input-required`, the gate resource serves the payload) and posts the
//!   human's reply via [`GateChannel::post_reply`];
//! - the **reactor** fans the posted reply to the opener child as
//!   `ControlMsg::Inject`, exactly once per open gate.

use serde_json::Value;
use std::sync::{Arc, Mutex};

/// The shared gate slot for one run. Cloned: one handle lives in the served-
/// session registry, the other in the run's reactor.
#[derive(Clone, Default)]
pub struct GateChannel {
    inner: Arc<Mutex<GateSlot>>,
}

#[derive(Default)]
struct GateSlot {
    /// The open gate, if any: `(reactor NodeId ordinal, gate node id, payload)`.
    open: Option<OpenGate>,
    /// A posted, not-yet-fanned reply (the served surface writes; the reactor
    /// takes). At most one per open gate — a second post while one is pending
    /// is refused (the caller reports "reply already pending").
    reply: Option<String>,
}

#[derive(Clone)]
struct OpenGate {
    /// The reactor-tree node ordinal of the child that opened the gate (where
    /// the `Inject` reply must be sent).
    opener: u64,
    /// The workflow node id (`human` node) that opened it.
    node: String,
    /// The resolved gate payload (what the human is being asked to look at).
    payload: Value,
}

impl GateChannel {
    pub fn new() -> GateChannel {
        GateChannel::default()
    }

    /// The reactor records an opened gate (`AgentMsg::Gate` from child `opener`).
    pub fn open(&self, opener: u64, node: &str, payload: Value) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.open = Some(OpenGate {
            opener,
            node: node.to_string(),
            payload,
        });
        // A stale un-fanned reply from a PREVIOUS gate never leaks into a new
        // one — each gate starts with a clean reply slot.
        g.reply = None;
    }

    /// The reactor clears the gate (`AgentMsg::GateClosed`).
    pub fn close(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.open = None;
        g.reply = None;
    }

    /// The served surface: the open gate's `(node, payload)`, if any.
    pub fn snapshot(&self) -> Option<(String, Value)> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.open.as_ref().map(|o| (o.node.clone(), o.payload.clone()))
    }

    /// The served surface posts the human's reply. `Err` when no gate is open
    /// (nothing is waiting) or a reply is already pending (first one wins).
    pub fn post_reply(&self, reply: String) -> Result<(), &'static str> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.open.is_none() {
            return Err("no open gate (the task is not awaiting input)");
        }
        if g.reply.is_some() {
            return Err("a reply is already pending for this gate");
        }
        g.reply = Some(reply);
        Ok(())
    }

    /// The reactor takes a pending reply to fan as `ControlMsg::Inject` to the
    /// opener child; the slot empties (exactly-once fan).
    pub fn take_reply(&self) -> Option<(u64, String)> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let opener = g.open.as_ref()?.opener;
        g.reply.take().map(|r| (opener, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gate_lifecycle_open_reply_fan_close() {
        let ch = GateChannel::new();
        assert!(ch.snapshot().is_none());
        assert_eq!(
            ch.post_reply("early".into()),
            Err("no open gate (the task is not awaiting input)")
        );

        ch.open(3, "approve", json!({"q": "ship it?"}));
        let (node, payload) = ch.snapshot().expect("open gate");
        assert_eq!(node, "approve");
        assert_eq!(payload["q"], "ship it?");

        ch.post_reply("yes".into()).expect("first reply lands");
        assert_eq!(
            ch.post_reply("no".into()),
            Err("a reply is already pending for this gate")
        );
        assert_eq!(ch.take_reply(), Some((3, "yes".into())));
        assert_eq!(ch.take_reply(), None, "exactly-once fan");

        ch.close();
        assert!(ch.snapshot().is_none());
    }

    #[test]
    fn a_new_gate_never_inherits_a_stale_reply() {
        let ch = GateChannel::new();
        ch.open(1, "g1", json!(1));
        ch.post_reply("stale".into()).unwrap();
        ch.open(1, "g2", json!(2)); // resuspended before the fan
        assert_eq!(ch.take_reply(), None, "clean slot per gate");
    }
}
