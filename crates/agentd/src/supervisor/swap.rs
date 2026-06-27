//! The async-run intelligence hot-swap channel (RFC 0018 §5.2).
//!
//! A served async run is supervised by its own reactor (`reactor.rs`), which owns
//! the run's live children — the operator/reload fan-out cannot reach those
//! children directly (they are processes behind that reactor). So, exactly as the
//! `pause`/`resume` operator tools flip a shared `Arc<AtomicBool>` the reactor
//! reads and translates into `ControlMsg::Pause`/`Resume` to its children, a hot
//! reload that touches `intelligence`/`model` writes the new config into this
//! shared [`SwapChannel`]; the run's reactor reads it each tick and fans
//! `ControlMsg::SwapIntel` to its live children once per published swap.
//!
//! It is a **generation-tracked latest-value** slot: a swap that supersedes an
//! unfanned one simply overwrites it (the children only ever need the LATEST
//! config — last-write-wins), and the monotonic generation lets the reactor fan
//! each distinct published swap exactly once (the parallel of `paused_sent`).

use crate::subagent::protocol::SwapIntel;
use std::sync::{Arc, Mutex};

/// The shared latest-swap slot for one async run (RFC 0018 §5.2). Cloned: one
/// handle lives in the served-session registry (written by the reload fan-out),
/// the other in the run's reactor (read + fanned to children).
#[derive(Clone, Default)]
pub struct SwapChannel {
    inner: Arc<Mutex<SwapSlot>>,
}

#[derive(Default)]
struct SwapSlot {
    /// Monotonic publish counter; the reactor fans when it advances past its last.
    generation: u64,
    /// The latest published swap (last-write-wins). `None` until the first swap.
    latest: Option<SwapIntel>,
}

impl SwapChannel {
    pub fn new() -> SwapChannel {
        SwapChannel::default()
    }

    /// Publish a new swap (the reload fan-out side): overwrite the latest config
    /// and bump the generation so the reactor fans it once.
    pub fn publish(&self, swap: SwapIntel) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.generation += 1;
        g.latest = Some(swap);
    }

    /// Take the published swap IF its generation is newer than `since` (the
    /// reactor side). Returns the swap to fan plus its generation (to record as the
    /// new `since`); `None` when nothing new has been published. Clones the swap so
    /// the slot keeps it (a later-joining child of the same run still needs it —
    /// but that is a startup-payload concern, not this hot path; the clone is cheap
    /// relative to a process spawn).
    pub fn take_newer(&self, since: u64) -> Option<(SwapIntel, u64)> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.generation > since {
            g.latest.as_ref().map(|s| (s.clone(), g.generation))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SwapPolicy;

    fn swap(model: &str) -> SwapIntel {
        SwapIntel {
            uri: "unix:/a".into(),
            token: None,
            model: Some(model.into()),
            policy: SwapPolicy::FinishOnOld,
        }
    }

    #[test]
    fn nothing_published_yields_none() {
        let ch = SwapChannel::new();
        assert!(ch.take_newer(0).is_none());
    }

    #[test]
    fn publish_then_take_once_per_generation() {
        let ch = SwapChannel::new();
        ch.publish(swap("m1"));
        let (s, g1) = ch.take_newer(0).expect("a fresh swap is takeable");
        assert_eq!(s.model.as_deref(), Some("m1"));
        // Same generation is not re-taken.
        assert!(ch.take_newer(g1).is_none());
        // A newer publish supersedes (last-write-wins) and is takeable again.
        ch.publish(swap("m2"));
        let (s2, g2) = ch.take_newer(g1).expect("a newer swap is takeable");
        assert_eq!(s2.model.as_deref(), Some("m2"));
        assert!(g2 > g1);
    }
}
