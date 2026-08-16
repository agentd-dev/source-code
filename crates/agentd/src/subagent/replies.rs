// SPDX-License-Identifier: AGPL-3.0-only
//! The child-side **reply slots** for the agentd 2.0 round-trips (RFC 0026 §2):
//! a turn worker sends `ToolRequest`/`BudgetRequest` frames up and blocks until
//! the control-reader thread delivers the matching `ToolResult`/`BudgetGrant`
//! here (by `id`). One mutex + condvar; a closed channel or a cancel wakes
//! every waiter with `None`.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A delivered reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Tool {
        result: Value,
        is_error: bool,
    },
    Budget {
        ok: bool,
        wait_ms: u64,
        model: Option<String>,
        reason: Option<String>,
    },
}

#[derive(Default)]
pub struct Replies {
    slots: Mutex<HashMap<u64, Reply>>,
    cv: Condvar,
    closed: AtomicBool,
    next_id: AtomicU64,
}

impl Replies {
    pub fn new() -> Replies {
        Replies {
            next_id: AtomicU64::new(1),
            ..Default::default()
        }
    }

    /// Mint a fresh request id.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Deliver a reply (the control thread).
    pub fn deliver(&self, id: u64, reply: Reply) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, reply);
        self.cv.notify_all();
    }

    /// The channel closed (supervisor gone): wake everyone.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.cv.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Block until the reply for `id` arrives, the deadline passes, the
    /// channel closes, or `cancel` is set (polled every 100 ms).
    pub fn wait(&self, id: u64, deadline: Instant, cancel: &AtomicBool) -> Option<Reply> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = slots.remove(&id) {
                return Some(r);
            }
            if self.is_closed() || cancel.load(Ordering::Relaxed) {
                return None;
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let wait = (deadline - now).min(Duration::from_millis(100));
            let (guard, _) = self
                .cv
                .wait_timeout(slots, wait)
                .unwrap_or_else(|e| e.into_inner());
            slots = guard;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn replies_are_delivered_by_id_and_waiters_wake_on_close_or_cancel() {
        let r = Arc::new(Replies::new());
        let id = r.next_id();
        let r2 = r.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            r2.deliver(
                99,
                Reply::Tool {
                    result: json!("other"),
                    is_error: false,
                },
            );
            r2.deliver(
                id,
                Reply::Tool {
                    result: json!({"ok": true}),
                    is_error: false,
                },
            );
        });
        let cancel = AtomicBool::new(false);
        let got = r
            .wait(id, Instant::now() + Duration::from_secs(2), &cancel)
            .unwrap();
        assert_eq!(
            got,
            Reply::Tool {
                result: json!({"ok": true}),
                is_error: false
            }
        );
        t.join().unwrap();
        // Timeout.
        assert!(
            r.wait(12345, Instant::now() + Duration::from_millis(20), &cancel)
                .is_none()
        );
        // Cancel wakes.
        cancel.store(true, Ordering::Relaxed);
        assert!(
            r.wait(12345, Instant::now() + Duration::from_secs(5), &cancel)
                .is_none()
        );
        // Close wakes.
        let cancel2 = AtomicBool::new(false);
        let r3 = r.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            r3.close();
        });
        assert!(
            r.wait(777, Instant::now() + Duration::from_secs(5), &cancel2)
                .is_none()
        );
        t.join().unwrap();
        // A reply that arrived before the wait is picked up.
        r.deliver(
            5,
            Reply::Budget {
                ok: true,
                wait_ms: 0,
                model: None,
                reason: None,
            },
        );
        assert!(matches!(
            r.wait(5, Instant::now(), &cancel2),
            Some(Reply::Budget { ok: true, .. })
        ));
    }
}
