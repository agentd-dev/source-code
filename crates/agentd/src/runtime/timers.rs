// SPDX-License-Identifier: Apache-2.0
//! The **durable timer wheel** (RFC 0025 §3.3 `timer`, RFC 0026 §3): absolute
//! deadlines owned by a step, a tool request, a start node or the lifecycle;
//! armed through the store, fired by the loop's tick (`fire`), re-armed from
//! the restored records at startup (past deadlines fire immediately).

use crate::state::{Durable, TimerRecord, now_ms, ulid};
use crate::store::StoreError;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Timers {
    /// id → record (sorted by id; scanned by deadline on fire — the count is small).
    map: BTreeMap<String, TimerRecord>,
}

impl Timers {
    pub fn new() -> Timers {
        Timers {
            map: BTreeMap::new(),
        }
    }

    /// Adopt restored records.
    pub fn restore(&mut self, records: Vec<TimerRecord>) {
        for r in records {
            self.map.insert(r.id.clone(), r);
        }
    }

    /// Arm a durable timer. `owner` names who to notify (`{"kind": "tool",
    /// "node": n, "req": id}` / `{"kind": "step", "run": r, "step": s}` / …).
    pub fn arm(
        &mut self,
        d: &Durable,
        deadline_ms: u64,
        owner: Value,
        payload: Value,
    ) -> Result<String, StoreError> {
        let id = ulid::new();
        let rec = TimerRecord {
            id: id.clone(),
            deadline_ms,
            owner,
            payload,
        };
        d.timer_arm(&rec)?;
        crate::state::kill_point("wait.armed");
        self.map.insert(id.clone(), rec);
        Ok(id)
    }

    /// Disarm (delete) a timer.
    pub fn disarm(&mut self, d: &Durable, id: &str) -> Result<(), StoreError> {
        if self.map.remove(id).is_some() {
            d.timer_disarm(id)?;
        }
        Ok(())
    }

    /// Fire every due timer: returns them (removed from the wheel + store).
    pub fn fire(&mut self, d: &Durable, now: u64) -> Vec<TimerRecord> {
        let due: Vec<String> = self
            .map
            .iter()
            .filter(|(_, r)| r.deadline_ms <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(r) = self.map.remove(&id) {
                let _ = d.timer_disarm(&id);
                out.push(r);
            }
        }
        out
    }

    /// The earliest deadline (for idle decisions).
    pub fn next_deadline(&self) -> Option<u64> {
        self.map.values().map(|r| r.deadline_ms).min()
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// Timers owned by something matching `pred`.
    pub fn owned_by(&self, pred: impl Fn(&Value) -> bool) -> Vec<String> {
        self.map
            .values()
            .filter(|r| pred(&r.owner))
            .map(|r| r.id.clone())
            .collect()
    }
    pub fn status(&self) -> Value {
        let now = now_ms();
        json!(self.map.values().map(|r| json!({"id": r.id, "in_ms": r.deadline_ms.saturating_sub(now), "owner": r.owner})).collect::<Vec<_>>())
    }
}

impl Default for Timers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Policy;
    use crate::store::memory::MemoryStore;
    use std::sync::Arc;

    #[test]
    fn timers_arm_fire_disarm_and_restore() {
        let d = Durable::new(
            Arc::new(MemoryStore::new()),
            "agentd",
            "i",
            Policy::default(),
            None,
        );
        let mut t = Timers::new();
        let now = now_ms();
        let a = t
            .arm(
                &d,
                now + 10_000,
                json!({"kind": "step", "run": "r"}),
                json!({}),
            )
            .unwrap();
        let b = t
            .arm(
                &d,
                now.saturating_sub(1),
                json!({"kind": "tool", "node": 1, "req": 2}),
                json!({"slept": 1}),
            )
            .unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.next_deadline(), Some(now.saturating_sub(1)));
        let fired = t.fire(&d, now);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].id, b);
        assert_eq!(t.owned_by(|o| o["kind"] == json!("step")), vec![a.clone()]);
        // Restore from the store: only `a` survives.
        let restored = d.restore().unwrap();
        let mut t2 = Timers::new();
        t2.restore(restored.timers());
        assert_eq!(t2.len(), 1);
        t2.disarm(&d, &a).unwrap();
        assert!(t2.is_empty());
        assert!(d.restore().unwrap().timers().is_empty());
    }
}
