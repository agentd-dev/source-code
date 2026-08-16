// SPDX-License-Identifier: Apache-2.0
//! The **token governor** (RFC 0026 §7, plan §3.17): windowed, durable token/
//! request budgets that pace how fast an instance burns intelligence, with the
//! tactics `wait | slow | degrade | refuse | fail` when a window is exhausted.
//!
//! - **Windows** — `intelligence.budget.windows[]`: `{per: second|minute|hour|
//!   day|week, tokens?, requests?, reset?}`. Every window is a **fixed window
//!   aligned to its unit** (a rolling `second|minute|hour` window is the
//!   current unit-aligned bucket; a calendar `day|week` window resets at
//!   `reset` `HH:MMZ`, default `00:00Z`, weeks on Monday). Counters
//!   `{index, tokens, requests}` are durable in the manifest (RFC 0025 §3.3):
//!   a restart never re-opens a spent daily budget.
//! - **Scopes** — the instance governor plus optional sub-budgets per run /
//!   conversation / principal ([`Governor::admit`] takes the applicable scoped
//!   budgets); the tightest applicable window wins.
//! - **Reservation** — `admit` reserves an estimate against every window; the
//!   reported usage `settle`s it (replacing the estimate).
//! - **Lifetime** — `lifetime_tokens` is the hard ceiling (always `fail`).
//!
//! Pure and clock-injected (`now_ms`) — the runtime feeds it, the manifest
//! stores it, `agent://budget` reads it.

use crate::config::v2::{Budget, BudgetTactic, BudgetWindow, WindowUnit};
use crate::wire::intel::Usage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The verdict of an admission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Proceed (optionally on a degraded model). Carries the reservation id.
    Ok {
        reservation: u64,
        model: Option<String>,
    },
    /// Not now: come back at `until_ms` (`wait` / `slow` pacing).
    Wait { until_ms: u64, reason: String },
    /// Declined (`refuse` tactic).
    Refuse { reason: String },
    /// Fail the unit (`fail` tactic / lifetime ceiling).
    Fail { reason: String },
}

/// One window's durable counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WindowState {
    pub index: u64,
    pub tokens: u64,
    pub requests: u64,
    #[serde(skip)]
    pub reserved: u64,
}

/// A configured window + its state.
#[derive(Debug, Clone)]
struct Window {
    cfg: BudgetWindow,
    /// Reset offset (ms after 00:00Z / Monday 00:00Z) for calendar windows.
    reset_offset_ms: u64,
    state: WindowState,
}

impl Window {
    fn len_ms(&self) -> u64 {
        self.cfg.per.duration().as_millis() as u64
    }
    /// The window index at `now` (aligned buckets; calendar windows offset).
    fn index_at(&self, now_ms: u64) -> u64 {
        match self.cfg.per {
            WindowUnit::Day | WindowUnit::Week => {
                now_ms.saturating_sub(self.reset_offset_ms + week_epoch_shift(self.cfg.per))
                    / self.len_ms()
            }
            _ => now_ms / self.len_ms(),
        }
    }
    fn start_ms(&self, now_ms: u64) -> u64 {
        let idx = self.index_at(now_ms);
        match self.cfg.per {
            WindowUnit::Day | WindowUnit::Week => {
                idx * self.len_ms() + self.reset_offset_ms + week_epoch_shift(self.cfg.per)
            }
            _ => idx * self.len_ms(),
        }
    }
    fn next_reset_ms(&self, now_ms: u64) -> u64 {
        self.start_ms(now_ms) + self.len_ms()
    }
    /// Roll to the current window (clearing counters when it moved).
    fn roll(&mut self, now_ms: u64) {
        let idx = self.index_at(now_ms);
        if idx != self.state.index {
            self.state = WindowState {
                index: idx,
                ..Default::default()
            };
        }
    }
    fn tokens_left(&self) -> Option<u64> {
        self.cfg
            .tokens
            .map(|cap| cap.saturating_sub(self.state.tokens + self.state.reserved))
    }
    fn requests_left(&self) -> Option<u64> {
        self.cfg
            .requests
            .map(|cap| cap.saturating_sub(self.state.requests))
    }
    fn label(&self) -> String {
        format!("{:?}", self.cfg.per).to_lowercase()
    }
}

/// Unix epoch (1970-01-01) was a Thursday; shift so week windows start Monday.
fn week_epoch_shift(unit: WindowUnit) -> u64 {
    match unit {
        WindowUnit::Week => 4 * 86_400_000, // Thursday → the previous Monday is 3 days back; +4 aligns Monday 00:00
        _ => 0,
    }
}

/// Parse `HH:MMZ` (or `HH:MM`) into ms after midnight.
pub fn parse_reset(s: &str) -> Option<u64> {
    let t = s.trim().trim_end_matches(['Z', 'z']);
    let (h, m) = t.split_once(':')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    (h < 24 && m < 60).then_some((h * 3600 + m * 60) * 1000)
}

/// One scope's governor (the instance, or a sub-budget).
#[derive(Debug, Clone)]
struct Scope {
    windows: Vec<Window>,
    lifetime_cap: u64,
    lifetime_used: u64,
    tactic: BudgetTactic,
    slow_factor: f64,
    degrade_model: Option<String>,
}

impl Scope {
    fn from_budget(b: &Budget) -> Scope {
        Scope {
            windows: b
                .windows
                .iter()
                .map(|w| Window {
                    cfg: w.clone(),
                    reset_offset_ms: w.reset.as_deref().and_then(parse_reset).unwrap_or(0),
                    state: WindowState::default(),
                })
                .collect(),
            lifetime_cap: b.lifetime_tokens.unwrap_or(0),
            lifetime_used: 0,
            tactic: b.on_exhausted,
            slow_factor: b.slow.factor.unwrap_or(0.5).clamp(0.01, 1.0),
            degrade_model: b.degrade.model.clone(),
        }
    }

    fn roll(&mut self, now_ms: u64) {
        for w in &mut self.windows {
            w.roll(now_ms);
        }
    }

    /// Check an estimate: `Ok(())` or the first exhaustion reason + when it opens.
    fn check(&self, estimate: u64, now_ms: u64) -> Result<(), Exhausted> {
        if self.lifetime_cap > 0 && self.lifetime_used + estimate > self.lifetime_cap {
            return Err(Exhausted {
                window: "lifetime".into(),
                until_ms: None,
                pacing: false,
            });
        }
        for w in &self.windows {
            if let Some(left) = w.tokens_left()
                && estimate > left
            {
                return Err(Exhausted {
                    window: w.label(),
                    until_ms: Some(w.next_reset_ms(now_ms)),
                    pacing: false,
                });
            }
            if let Some(left) = w.requests_left()
                && left == 0
            {
                return Err(Exhausted {
                    window: w.label(),
                    until_ms: Some(w.next_reset_ms(now_ms)),
                    pacing: false,
                });
            }
            // `slow`: pace admissions to slow.factor × the window rate.
            if self.tactic == BudgetTactic::Slow
                && let Some(cap) = w.cfg.tokens
            {
                let elapsed = now_ms.saturating_sub(w.start_ms(now_ms)) as f64;
                let len = w.len_ms() as f64;
                let allowed = self.slow_factor * cap as f64 * (elapsed / len).clamp(0.0, 1.0)
                    + self.slow_factor * cap as f64 * 0.05;
                let after = (w.state.tokens + w.state.reserved + estimate) as f64;
                if after > allowed && after <= cap as f64 {
                    // When will the pace allow `after`? t = after/(factor*cap) * len.
                    let t = (after / (self.slow_factor * cap as f64)) * len;
                    let until = w.start_ms(now_ms) + t.min(len) as u64;
                    return Err(Exhausted {
                        window: w.label(),
                        until_ms: Some(until.max(now_ms + 50)),
                        pacing: true,
                    });
                }
            }
        }
        Ok(())
    }

    fn reserve(&mut self, estimate: u64) {
        for w in &mut self.windows {
            w.state.reserved += estimate;
            w.state.requests += 1;
        }
    }

    fn settle(&mut self, reserved: u64, used: u64) {
        for w in &mut self.windows {
            w.state.reserved = w.state.reserved.saturating_sub(reserved);
            w.state.tokens += used;
        }
        self.lifetime_used += used;
    }

    fn release(&mut self, reserved: u64) {
        for w in &mut self.windows {
            w.state.reserved = w.state.reserved.saturating_sub(reserved);
            w.state.requests = w.state.requests.saturating_sub(1);
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "windows": self.windows.iter().map(|w| json!({"per": w.label(), "index": w.state.index, "tokens": w.state.tokens, "requests": w.state.requests})).collect::<Vec<_>>(),
            "lifetime_used": self.lifetime_used,
        })
    }

    fn adopt(&mut self, v: &Value) {
        if let Some(ws) = v.get("windows").and_then(Value::as_array) {
            for w in &mut self.windows {
                if let Some(saved) = ws.iter().find(|s| s["per"].as_str() == Some(&w.label())) {
                    w.state = WindowState {
                        index: saved["index"].as_u64().unwrap_or(0),
                        tokens: saved["tokens"].as_u64().unwrap_or(0),
                        requests: saved["requests"].as_u64().unwrap_or(0),
                        reserved: 0,
                    };
                }
            }
        }
        self.lifetime_used = v.get("lifetime_used").and_then(Value::as_u64).unwrap_or(0);
    }

    fn status(&self, now_ms: u64) -> Value {
        json!({
            "tactic": format!("{:?}", self.tactic).to_lowercase(),
            "lifetime": if self.lifetime_cap > 0 { json!({"cap": self.lifetime_cap, "used": self.lifetime_used, "remaining": self.lifetime_cap.saturating_sub(self.lifetime_used)}) } else { Value::Null },
            "windows": self.windows.iter().map(|w| json!({
                "per": w.label(), "tokens": {"cap": w.cfg.tokens, "used": w.state.tokens, "reserved": w.state.reserved, "remaining": w.tokens_left()},
                "requests": {"cap": w.cfg.requests, "used": w.state.requests, "remaining": w.requests_left()},
                "resets_at_ms": w.next_reset_ms(now_ms),
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Exhausted {
    window: String,
    until_ms: Option<u64>,
    pacing: bool,
}

/// An in-flight reservation.
#[derive(Debug, Clone)]
struct Reservation {
    estimate: u64,
    scopes: Vec<String>,
}

/// The governor: the instance scope + named sub-scopes.
#[derive(Debug, Clone)]
pub struct Governor {
    instance: Scope,
    scopes: BTreeMap<String, Scope>,
    reservations: BTreeMap<u64, Reservation>,
    next_reservation: u64,
    /// Units currently waiting on the budget (for the gauge / status).
    pub waiting: BTreeMap<String, u64>,
    events: u64,
}

impl Governor {
    pub fn new(budget: &Budget) -> Governor {
        Governor {
            instance: Scope::from_budget(budget),
            scopes: BTreeMap::new(),
            reservations: BTreeMap::new(),
            next_reservation: 1,
            waiting: BTreeMap::new(),
            events: 0,
        }
    }

    /// Whether any budget is configured at all (else admission is trivially ok).
    pub fn is_active(&self) -> bool {
        !self.instance.windows.is_empty()
            || self.instance.lifetime_cap > 0
            || !self.scopes.is_empty()
    }

    /// Ensure a sub-scope exists (e.g. `conversation:<id>`, `run:<id>`).
    pub fn ensure_scope(&mut self, key: &str, budget: &Budget) {
        self.scopes
            .entry(key.to_string())
            .or_insert_with(|| Scope::from_budget(budget));
    }
    pub fn drop_scope(&mut self, key: &str) {
        self.scopes.remove(key);
    }

    /// Ask to spend `estimate` tokens now, under the instance scope and the
    /// named sub-scopes (`scopes` must exist via `ensure_scope`). On `Ok` the
    /// estimate is reserved until [`Governor::settle`] / [`Governor::release`].
    pub fn admit(&mut self, estimate: u64, scopes: &[String], now_ms: u64) -> Admission {
        self.instance.roll(now_ms);
        for k in scopes {
            if let Some(s) = self.scopes.get_mut(k) {
                s.roll(now_ms);
            }
        }
        // The tightest applicable verdict: check the sub-scopes first (they
        // nest under the instance), then the instance.
        let mut verdict: Option<(Exhausted, String, BudgetTactic, Option<String>)> = None;
        for k in scopes {
            if let Some(s) = self.scopes.get(k)
                && let Err(ex) = s.check(estimate, now_ms)
            {
                verdict = Some((ex, k.clone(), s.tactic, s.degrade_model.clone()));
                break;
            }
        }
        if verdict.is_none()
            && let Err(ex) = self.instance.check(estimate, now_ms)
        {
            verdict = Some((
                ex,
                "instance".into(),
                self.instance.tactic,
                self.instance.degrade_model.clone(),
            ));
        }
        if let Some((ex, key, tactic, degrade_model)) = verdict {
            let reason = if ex.pacing {
                format!(
                    "budget pacing ({key} {} window): slowing admissions",
                    ex.window
                )
            } else {
                format!("budget exhausted ({key} {} window)", ex.window)
            };
            self.events += 1;
            if ex.window == "lifetime" {
                return Admission::Fail {
                    reason: format!("lifetime token budget exhausted ({key})"),
                };
            }
            return match tactic {
                BudgetTactic::Wait | BudgetTactic::Slow => Admission::Wait {
                    until_ms: ex.until_ms.unwrap_or(now_ms + 1000),
                    reason,
                },
                BudgetTactic::Degrade => match degrade_model {
                    Some(m) => {
                        let id = self.reserve_all(estimate, scopes);
                        Admission::Ok {
                            reservation: id,
                            model: Some(m),
                        }
                    }
                    None => Admission::Wait {
                        until_ms: ex.until_ms.unwrap_or(now_ms + 1000),
                        reason,
                    },
                },
                BudgetTactic::Refuse => Admission::Refuse {
                    reason: format!("refused: {reason}"),
                },
                BudgetTactic::Fail => Admission::Fail { reason },
            };
        }
        let id = self.reserve_all(estimate, scopes);
        Admission::Ok {
            reservation: id,
            model: None,
        }
    }

    fn reserve_all(&mut self, estimate: u64, scopes: &[String]) -> u64 {
        self.instance.reserve(estimate);
        for k in scopes {
            if let Some(s) = self.scopes.get_mut(k) {
                s.reserve(estimate);
            }
        }
        let id = self.next_reservation;
        self.next_reservation += 1;
        self.reservations.insert(
            id,
            Reservation {
                estimate,
                scopes: scopes.to_vec(),
            },
        );
        id
    }

    /// Settle a reservation with the reported usage.
    pub fn settle(&mut self, reservation: u64, usage: Usage) {
        let Some(r) = self.reservations.remove(&reservation) else {
            return;
        };
        let used = usage.total();
        self.instance.settle(r.estimate, used);
        for k in &r.scopes {
            if let Some(s) = self.scopes.get_mut(k) {
                s.settle(r.estimate, used);
            }
        }
    }

    /// Charge usage that had no reservation (a child reported more calls than
    /// admissions, or admission is off).
    pub fn charge(&mut self, usage: Usage, scopes: &[String]) {
        let used = usage.total();
        self.instance.settle(0, used);
        for k in scopes {
            if let Some(s) = self.scopes.get_mut(k) {
                s.settle(0, used);
            }
        }
    }

    /// Release a reservation without usage (the unit never ran).
    pub fn release(&mut self, reservation: u64) {
        let Some(r) = self.reservations.remove(&reservation) else {
            return;
        };
        self.instance.release(r.estimate);
        for k in &r.scopes {
            if let Some(s) = self.scopes.get_mut(k) {
                s.release(r.estimate);
            }
        }
    }

    /// The durable counters (manifest `budget`).
    pub fn to_value(&self) -> Value {
        json!({
            "instance": self.instance.to_value(),
            "scopes": self.scopes.iter().map(|(k, s)| (k.clone(), s.to_value())).collect::<BTreeMap<_, _>>(),
        })
    }

    /// Adopt restored counters (matching windows by unit; scopes by key when
    /// they exist — a scope restored before `ensure_scope` is kept aside).
    pub fn restore(&mut self, v: &Value, now_ms: u64) {
        if let Some(i) = v.get("instance") {
            self.instance.adopt(i);
        }
        if let Some(sc) = v.get("scopes").and_then(Value::as_object) {
            for (k, sv) in sc {
                if let Some(s) = self.scopes.get_mut(k) {
                    s.adopt(sv);
                }
            }
        }
        self.instance.roll(now_ms);
    }

    /// `agent://budget`.
    pub fn status(&self, now_ms: u64) -> Value {
        json!({
            "active": self.is_active(),
            "instance": self.instance.status(now_ms),
            "scopes": self.scopes.iter().map(|(k, s)| (k.clone(), s.status(now_ms))).collect::<BTreeMap<_, _>>(),
            "reservations": self.reservations.len(),
            "waiting": self.waiting,
            "events": self.events,
        })
    }

    /// The instance-scope lifetime usage.
    pub fn lifetime_used(&self) -> u64 {
        self.instance.lifetime_used
    }

    /// The earliest moment any exhausted instance window opens again.
    pub fn next_reset_ms(&self, now_ms: u64) -> Option<u64> {
        self.instance
            .windows
            .iter()
            .map(|w| w.next_reset_ms(now_ms))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(doc: Value) -> Budget {
        serde_json::from_value(doc).unwrap()
    }
    fn usage(n: u64) -> Usage {
        Usage {
            input_tokens: n,
            output_tokens: 0,
        }
    }

    #[test]
    fn windows_reserve_settle_and_roll_over() {
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "minute", "tokens": 1000, "requests": 3}], "on_exhausted": "wait"}),
        ));
        assert!(g.is_active());
        let t0 = 1_700_000_000_000u64; // some instant
        let t0 = t0 - t0 % 60_000; // minute-aligned for clarity
        let a = g.admit(400, &[], t0);
        let Admission::Ok {
            reservation: r1,
            model: None,
        } = a
        else {
            panic!("{a:?}")
        };
        // Reserved counts: 400 of 1000 → another 700 does not fit.
        assert!(
            matches!(g.admit(700, &[], t0 + 1000), Admission::Wait { until_ms, .. } if until_ms == t0 + 60_000)
        );
        g.settle(r1, usage(300)); // actual usage lower than the estimate
        let Admission::Ok {
            reservation: r2, ..
        } = g.admit(700, &[], t0 + 2000)
        else {
            panic!()
        };
        g.settle(r2, usage(700));
        // 1000 used: exhausted; requests 2/3 used.
        assert!(matches!(g.admit(1, &[], t0 + 3000), Admission::Wait { .. }));
        // The next minute: fresh counters.
        let Admission::Ok {
            reservation: r3, ..
        } = g.admit(900, &[], t0 + 60_000)
        else {
            panic!()
        };
        g.release(r3);
        // Request cap: 3 admissions in a window.
        for _ in 0..3 {
            let Admission::Ok { reservation, .. } = g.admit(1, &[], t0 + 61_000) else {
                panic!()
            };
            g.settle(reservation, usage(1));
        }
        assert!(
            matches!(g.admit(1, &[], t0 + 62_000), Admission::Wait { .. }),
            "requests exhausted"
        );
        // Durability: counters round-trip through the manifest value.
        let v = g.to_value();
        assert_eq!(v["instance"]["windows"][0]["tokens"], json!(3));
        let mut g2 = Governor::new(&budget(
            json!({"windows": [{"per": "minute", "tokens": 1000, "requests": 3}]}),
        ));
        g2.restore(&v, t0 + 63_000);
        assert!(
            matches!(g2.admit(1, &[], t0 + 63_000), Admission::Wait { .. }),
            "restored counters still exhausted"
        );
        g2.restore(&v, t0 + 120_000);
        assert!(
            matches!(g2.admit(1, &[], t0 + 120_000), Admission::Ok { .. }),
            "rolled to a new window"
        );
        let st = g2.status(t0 + 120_000);
        assert_eq!(st["instance"]["windows"][0]["per"], json!("minute"));
    }

    #[test]
    fn tactics_lifetime_and_calendar_windows() {
        let day = 86_400_000u64;
        let now = 1_700_000_000_000u64;
        // Calendar day window resetting at 06:00Z; index changes at the reset.
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "day", "tokens": 100, "reset": "06:00Z"}], "on_exhausted": "fail"}),
        ));
        let start_of_day = now - now % day;
        let before = start_of_day + 5 * 3_600_000; // 05:00Z
        let after = start_of_day + 7 * 3_600_000; // 07:00Z
        let Admission::Ok { reservation, .. } = g.admit(100, &[], before) else {
            panic!()
        };
        g.settle(reservation, usage(100));
        assert!(
            matches!(g.admit(1, &[], before + 60_000), Admission::Fail { .. }),
            "fail tactic"
        );
        assert!(
            matches!(g.admit(1, &[], after), Admission::Ok { .. }),
            "the 06:00Z reset opened a new day window"
        );
        // Refuse.
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "hour", "tokens": 10}], "on_exhausted": "refuse"}),
        ));
        assert!(matches!(g.admit(11, &[], now), Admission::Refuse { .. }));
        // Degrade: admitted on the cheaper model.
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "hour", "tokens": 10}], "on_exhausted": "degrade", "degrade": {"model": "cheap"}}),
        ));
        assert!(
            matches!(g.admit(11, &[], now), Admission::Ok { model: Some(m), .. } if m == "cheap")
        );
        // Slow: pacing waits proportional to the window position.
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "hour", "tokens": 3600}], "on_exhausted": "slow", "slow": {"factor": 0.5}}),
        ));
        let hour_start = now - now % 3_600_000;
        // At the start of the hour, only the 5% burst allowance (0.5×3600×0.05 = 90) fits.
        assert!(matches!(g.admit(80, &[], hour_start), Admission::Ok { .. }));
        assert!(
            matches!(g.admit(500, &[], hour_start + 1000), Admission::Wait { until_ms, .. } if until_ms > hour_start + 1000 && until_ms < hour_start + 3_600_000)
        );
        // Half an hour in, 0.5×3600×0.5 = 900 (+90) is allowed.
        assert!(matches!(
            g.admit(500, &[], hour_start + 1_800_000),
            Admission::Ok { .. }
        ));
        // Lifetime ceiling always fails.
        let mut g = Governor::new(&budget(
            json!({"lifetime_tokens": 50, "on_exhausted": "wait"}),
        ));
        let Admission::Ok { reservation, .. } = g.admit(30, &[], now) else {
            panic!()
        };
        g.settle(reservation, usage(30));
        assert!(
            matches!(g.admit(30, &[], now), Admission::Fail { reason } if reason.contains("lifetime"))
        );
        assert_eq!(g.lifetime_used(), 30);
        // No budget configured ⇒ always ok.
        let mut g = Governor::new(&budget(json!({})));
        assert!(!g.is_active());
        assert!(matches!(g.admit(1_000_000, &[], now), Admission::Ok { .. }));
    }

    #[test]
    fn sub_scopes_nest_under_the_instance() {
        let mut g = Governor::new(&budget(
            json!({"windows": [{"per": "hour", "tokens": 1000}], "on_exhausted": "wait"}),
        ));
        g.ensure_scope(
            "conversation:c1",
            &budget(json!({"windows": [{"per": "hour", "tokens": 100}], "on_exhausted": "refuse"})),
        );
        let now = 1_700_000_000_000u64;
        // The tighter conversation window refuses first.
        assert!(
            matches!(g.admit(150, &["conversation:c1".to_string()], now), Admission::Refuse { reason } if reason.contains("conversation:c1"))
        );
        // Under the conversation cap: ok, reserved in both scopes.
        let Admission::Ok { reservation, .. } = g.admit(50, &["conversation:c1".to_string()], now)
        else {
            panic!()
        };
        g.settle(reservation, usage(50));
        let v = g.to_value();
        assert_eq!(
            v["scopes"]["conversation:c1"]["windows"][0]["tokens"],
            json!(50)
        );
        assert_eq!(v["instance"]["windows"][0]["tokens"], json!(50));
        // Unscoped usage still counts against the instance.
        g.charge(usage(940), &[]);
        assert!(matches!(g.admit(20, &[], now), Admission::Wait { .. }));
        assert_eq!(parse_reset("06:30Z"), Some((6 * 3600 + 30 * 60) * 1000));
        assert_eq!(parse_reset("25:00Z"), None);
        g.drop_scope("conversation:c1");
        assert!(g.to_value()["scopes"].as_object().unwrap().is_empty());
    }
}
