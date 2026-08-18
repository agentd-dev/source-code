// SPDX-License-Identifier: AGPL-3.0-only
//! The **durable timer wheel** (RFC 0025 §3.3 `timer`, RFC 0026 §3): absolute
//! deadlines owned by a step, a tool request, a start node or the lifecycle;
//! armed through the store, fired by the loop's tick (`fire`), re-armed from
//! the restored records at startup (past deadlines fire immediately).
//!
//! The durable row outlives the firing: `fire` hands the record to the loop and
//! only `settle` — at the head of the next tick's `fire`, i.e. after the tick
//! that ran the effect has checkpointed — deletes it. See [`Timers::fire`] for
//! why that ordering is the only crash-safe one.

use super::reactor::Runtime;
use crate::engine::run::StepStatus;
use crate::state::{Durable, TimerRecord, now_ms, ulid};
use crate::store::StoreError;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Timers {
    /// id → record (sorted by id; scanned by deadline on fire — the count is small).
    map: BTreeMap<String, TimerRecord>,
    /// Fired, effect in flight, row still in the store — drained by `settle`.
    settling: Vec<TimerRecord>,
}

impl Timers {
    pub fn new() -> Timers {
        Timers {
            map: BTreeMap::new(),
            settling: Vec::new(),
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

    /// Disarm (delete) a timer — armed or still settling (a cancelled run's
    /// timers arrive here through `owned_by`, which reports both, so a row that
    /// fired moments ago is deleted rather than left to re-fire after a restart).
    pub fn disarm(&mut self, d: &Durable, id: &str) -> Result<(), StoreError> {
        let armed = self.map.remove(id).is_some();
        let settling = self.settling.iter().any(|r| r.id == id);
        self.settling.retain(|r| r.id != id);
        if armed || settling {
            d.timer_disarm(id)?;
        }
        Ok(())
    }

    /// Fire every due timer: returns them, removed from the wheel but NOT yet
    /// from the store.
    ///
    /// The caller runs the effect (`on_timer`) after this returns, and that
    /// effect is only durable once the same tick reaches its checkpoint.
    /// Deleting the row first opens a window in which a crash loses BOTH the
    /// timer and its consequence: the suspended step the timer owned would have
    /// nothing left to wake it — `poll_waits` does not look at the timer-backed
    /// wait kinds — so the run wedges forever while the reactor keeps spinning
    /// at its 5 ms floor around a step that can never advance. Effects are
    /// at-least-once by design (RFC 0025 §7: every effect carries an
    /// idempotency key and a replay is expected), so the survivable direction
    /// is the other one — keep the row until the consequence is durable and let
    /// a crash inside the window re-fire the timer on restore.
    pub fn fire(&mut self, d: &Durable, now: u64) -> Vec<TimerRecord> {
        // The previous tick's effects are checkpointed by now (step 10 of the
        // loop runs between two `fire`s), so their rows can go.
        self.settle(d);
        let due: Vec<String> = self
            .map
            .iter()
            .filter(|(_, r)| r.deadline_ms <= now)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in due {
            if let Some(r) = self.map.remove(&id) {
                self.settling.push(r.clone());
                out.push(r);
            }
        }
        out
    }

    /// Delete the rows of timers whose effect has been checkpointed. Idempotent
    /// (a delete that is lost re-fires the timer once more, which is safe).
    pub fn settle(&mut self, d: &Durable) {
        for r in std::mem::take(&mut self.settling) {
            let _ = d.timer_disarm(&r.id);
        }
    }

    /// Whether `id` is still armed (a settling timer has already fired).
    pub fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
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
    /// Timers owned by something matching `pred` — armed *and* settling, so a
    /// cancelled run takes its just-fired rows with it.
    pub fn owned_by(&self, pred: impl Fn(&Value) -> bool) -> Vec<String> {
        self.map
            .values()
            .chain(self.settling.iter())
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

impl Runtime {
    /// **Restore-time repair** (RFC 0025 §6, step 3 — "re-arm timers"): a
    /// `Suspended` step whose durable timer did not come back is unreachable.
    /// `poll_waits` only resolves the wait kinds it can evaluate itself
    /// (`condition`, `run`, `join`, deadlines…); the timer-backed ones —
    /// `sleep`, `waiting_budget`, `retry_backoff` — are woken by `on_timer` and
    /// by nothing else, so a missing row means that run never moves again while
    /// the reactor keeps ticking around it.
    ///
    /// A timer can go missing legitimately: the store lost it, or the process
    /// died in the window between a firing and its checkpoint (which `fire`
    /// narrows but cannot close). Either way the repair is the same — re-arm at
    /// the recorded deadline, which fires immediately when that instant has
    /// passed. Re-running the effect is safe (RFC 0025 §7); leaving the step
    /// wedged is not. If the store refuses the re-arm, the step is failed
    /// explicitly so the operator sees a failure instead of a hang.
    pub(crate) fn repair_orphaned_timer_waits(&mut self) {
        let now = now_ms();
        // (run, step, wait kind, deadline) — collected first: arming mutates.
        let orphans: Vec<(String, String, String, u64)> = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .flat_map(|r| {
                r.steps
                    .iter()
                    .filter_map(|(sid, st)| {
                        if st.status != StepStatus::Suspended {
                            return None;
                        }
                        let w = st.wait.as_ref()?;
                        // Only a wait that NAMES a timer depends on one.
                        let id = w["timer"].as_str()?;
                        if self.timers.contains(id) {
                            return None;
                        }
                        Some((
                            r.id.clone(),
                            sid.clone(),
                            w["kind"].as_str().unwrap_or("").to_string(),
                            w["deadline_ms"]
                                .as_u64()
                                .or_else(|| w["until_ms"].as_u64())
                                .unwrap_or(now),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for (run, step, kind, deadline) in orphans {
            // The owner kind is what `on_timer` switches on: `step` finishes a
            // `sleep` done, `step_budget` returns the step to pending. The
            // original `sleep` payload (`slept_ms`) died with the row, so the
            // repaired firing reports the repair as the step's output instead.
            let (owner_kind, payload) = match kind.as_str() {
                "sleep" => ("step", json!({"repaired": true})),
                _ => ("step_budget", Value::Null),
            };
            match self.timers.arm(
                &self.durable,
                deadline,
                json!({"kind": owner_kind, "run": run, "step": step}),
                payload,
            ) {
                Ok(id) => {
                    if let Some(st) = self.runs.get_mut(&run).and_then(|r| r.steps.get_mut(&step))
                        && let Some(w) = st.wait.as_mut()
                    {
                        w["timer"] = json!(id);
                        w["repaired"] = json!(true);
                    }
                    if let Some(r) = self.runs.get_mut(&run) {
                        r.touch();
                    }
                    self.log.warn(
                        "restore.timer.repaired",
                        json!({"run": run, "step": step, "wait": kind, "deadline_ms": deadline, "timer": id}),
                    );
                }
                Err(e) => {
                    self.log.error(
                        "restore.timer.lost",
                        json!({"run": run, "step": step, "wait": kind, "err": e.to_string()}),
                    );
                    self.finish_step_pub(
                        &run,
                        &step,
                        StepStatus::Failed,
                        None,
                        Some(format!(
                            "suspended on a timer that is gone, and re-arming it failed: {e}"
                        )),
                        0,
                    );
                }
            }
        }
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
        // `b`'s effect has not been checkpointed yet, so its row is still in the
        // store: a crash here re-fires it rather than losing it (RFC 0025 §7).
        assert_eq!(d.restore().unwrap().timers().len(), 2);
        // The next tick settles it — one firing, one deletion.
        assert!(t.fire(&d, now).is_empty());
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
