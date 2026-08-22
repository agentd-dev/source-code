// SPDX-License-Identifier: AGPL-3.0-only
//! The circuit breaker for remote-effect steps — `retry`'s cross-run sibling.
//!
//! `retry` remembers failures *within one step of one run*; when the remote is
//! genuinely down, every new run still walks into it, burns its retry budget,
//! and adds load to a dependency that needs the opposite. A breaker remembers
//! across runs: after `failures` consecutive failures the circuit OPENS and
//! further attempts fail immediately — no connection, no timeout wait — until
//! `cooldown` has passed, when exactly ONE attempt is let through as a probe.
//! The probe's outcome decides: success closes the circuit, failure re-opens
//! it for another cooldown.
//!
//! Declared per step (`breaker: {failures: 5, cooldown: 60s}`) on the
//! remote-effect kinds (`http`, `mcp.tool`, `a2a.send`, `a2a.delegate`).
//! State is durable — keyed by workflow + the step's UNSCOPED id, so every
//! fan-out iteration (`each[0].call`, `each[1].call`…) shares one breaker,
//! because they share one dependency — and per instance: two replicas keep
//! independent breakers, which is the honest scope for state that is really a
//! local observation about a remote.
//!
//! This file is the pure state machine; the reactor owns WHEN it is consulted
//! (single-writer, so there are no races to reason about). A fast-fail is
//! reported through the normal step-failure path with [`OPEN_ERR`] as its
//! error prefix — which composes: `retry` on the step turns into a bounded
//! poll of the breaker, and `on_error: continue` + a `switch` on the error
//! text is a fallback route.

use serde_json::{Value, json};

/// The error prefix a breaker fast-fail carries. The recorder skips errors
/// with this prefix — a refusal to call is not evidence about the remote.
pub const OPEN_ERR: &str = "breaker open";

/// Parsed `breaker:` declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Consecutive failures that open the circuit.
    pub failures: u32,
    pub cooldown_ms: u64,
}

impl Config {
    /// Parse the (already-validated) `breaker:` value; `None` when absent or
    /// malformed (validation refuses malformed at load — this is the backstop).
    pub fn of(b: Option<&Value>) -> Option<Config> {
        let b = b?;
        let failures = b.get("failures")?.as_u64()? as u32;
        let cooldown_ms = b
            .get("cooldown")
            .and_then(Value::as_str)
            .and_then(|d| crate::config::parse_duration(d).ok())
            .map(|d| d.as_millis() as u64)?;
        (failures >= 1).then_some(Config {
            failures,
            cooldown_ms,
        })
    }
}

/// What the gate decides before an attempt is dispatched.
#[derive(Debug, PartialEq)]
pub enum Gate {
    /// Closed (or this attempt is not guarded): dispatch the effect.
    Proceed,
    /// Cooldown elapsed and no live probe: dispatch — this attempt IS the
    /// probe, and the state has been marked so.
    Probe,
    /// Open (or a probe is already in flight): fail fast without dialling.
    /// Carries the milliseconds until the next probe becomes possible.
    FastFail { retry_in_ms: u64 },
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Consult (and update, for the probe claim) the breaker before an attempt.
pub fn gate(state: &mut Value, cfg: Config, now_ms: u64) -> Gate {
    if state.get("state").and_then(Value::as_str) != Some("open") {
        return Gate::Proceed;
    }
    let opened = u(state, "opened_ms");
    if now_ms < opened.saturating_add(cfg.cooldown_ms) {
        return Gate::FastFail {
            retry_in_ms: opened + cfg.cooldown_ms - now_ms,
        };
    }
    // Half-open. One probe at a time; a probe record older than a cooldown is
    // stale (its process died mid-dial, or its completion was lost) and a new
    // probe may replace it rather than wedging the circuit open forever.
    let probe = u(state, "probe_ms");
    if probe != 0 && now_ms.saturating_sub(probe) < cfg.cooldown_ms {
        return Gate::FastFail {
            retry_in_ms: probe + cfg.cooldown_ms - now_ms,
        };
    }
    state["probe_ms"] = json!(now_ms);
    Gate::Probe
}

/// A state transition worth one log line (transitions only — a breaker that
/// logged every guarded call would be its own kind of load).
#[derive(Debug, PartialEq)]
pub enum Transition {
    None,
    Opened { fails: u32 },
    Reopened,
    Closed,
}

/// Record an attempt's outcome. `ok` is the step's terminal disposition for
/// this attempt; fast-fails (the [`OPEN_ERR`] prefix) must not reach here.
pub fn record(state: &mut Value, cfg: Config, ok: bool, now_ms: u64) -> Transition {
    let was_open = state.get("state").and_then(Value::as_str) == Some("open");
    let probing = u(state, "probe_ms") != 0;
    if ok {
        let t = if was_open {
            Transition::Closed
        } else {
            Transition::None
        };
        *state = json!({"state": "closed", "fails": 0});
        return t;
    }
    if was_open && probing {
        // The probe failed: the remote is still down. Re-open for another
        // cooldown, measured from now.
        *state = json!({"state": "open", "fails": u(state, "fails"), "opened_ms": now_ms});
        return Transition::Reopened;
    }
    let fails = u(state, "fails") as u32 + 1;
    if fails >= cfg.failures && !was_open {
        *state = json!({"state": "open", "fails": fails, "opened_ms": now_ms});
        return Transition::Opened { fails };
    }
    state["fails"] = json!(fails);
    state["state"] = json!(if was_open { "open" } else { "closed" });
    Transition::None
}

/// The durable key: workflow + the UNSCOPED step id (`each[0].call` → `call`),
/// so fan-out iterations share the breaker of the dependency they share.
pub fn key(workflow: &str, step_id: &str) -> String {
    let base = step_id.rsplit("].").next().unwrap_or(step_id);
    format!("{workflow}/{base}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: Config = Config {
        failures: 3,
        cooldown_ms: 1_000,
    };

    #[test]
    fn opens_after_n_consecutive_failures_and_only_then() {
        let mut s = json!({});
        assert_eq!(record(&mut s, CFG, false, 10), Transition::None);
        assert_eq!(record(&mut s, CFG, false, 20), Transition::None);
        assert_eq!(gate(&mut s, CFG, 25), Gate::Proceed, "still closed at 2/3");
        assert_eq!(
            record(&mut s, CFG, false, 30),
            Transition::Opened { fails: 3 }
        );
        assert_eq!(gate(&mut s, CFG, 40), Gate::FastFail { retry_in_ms: 990 });
    }

    #[test]
    fn a_success_resets_the_consecutive_count() {
        let mut s = json!({});
        record(&mut s, CFG, false, 10);
        record(&mut s, CFG, false, 20);
        assert_eq!(record(&mut s, CFG, true, 30), Transition::None);
        record(&mut s, CFG, false, 40);
        record(&mut s, CFG, false, 50);
        assert_eq!(
            gate(&mut s, CFG, 60),
            Gate::Proceed,
            "consecutive means consecutive — 2+2 with a success between is not 4"
        );
    }

    #[test]
    fn one_probe_after_cooldown_success_closes() {
        let mut s = json!({});
        for t in [10, 20, 30] {
            record(&mut s, CFG, false, t);
        }
        // During cooldown: everyone fails fast.
        assert!(matches!(gate(&mut s, CFG, 500), Gate::FastFail { .. }));
        // Cooldown over: exactly one probe; a second caller still fails fast.
        assert_eq!(gate(&mut s, CFG, 1_100), Gate::Probe);
        assert!(matches!(gate(&mut s, CFG, 1_101), Gate::FastFail { .. }));
        assert_eq!(record(&mut s, CFG, true, 1_200), Transition::Closed);
        assert_eq!(gate(&mut s, CFG, 1_300), Gate::Proceed);
    }

    #[test]
    fn a_failed_probe_reopens_for_another_cooldown() {
        let mut s = json!({});
        for t in [10, 20, 30] {
            record(&mut s, CFG, false, t);
        }
        assert_eq!(gate(&mut s, CFG, 1_100), Gate::Probe);
        assert_eq!(record(&mut s, CFG, false, 1_150), Transition::Reopened);
        assert!(matches!(gate(&mut s, CFG, 1_200), Gate::FastFail { .. }));
        // …and the NEXT cooldown is measured from the probe's failure.
        assert_eq!(gate(&mut s, CFG, 2_200), Gate::Probe);
    }

    #[test]
    fn a_stale_probe_does_not_wedge_the_circuit() {
        let mut s = json!({});
        for t in [10, 20, 30] {
            record(&mut s, CFG, false, t);
        }
        assert_eq!(gate(&mut s, CFG, 1_100), Gate::Probe);
        // The probe's completion never arrives (process died). A full cooldown
        // later, a new probe may claim the slot.
        assert_eq!(gate(&mut s, CFG, 2_200), Gate::Probe);
    }

    #[test]
    fn scoped_fanout_ids_share_one_breaker_key() {
        assert_eq!(key("pay", "charge"), "pay/charge");
        assert_eq!(key("pay", "each[0].charge"), "pay/charge");
        assert_eq!(key("pay", "each[17].charge"), "pay/charge");
        assert_ne!(key("pay", "charge"), key("bill", "charge"));
    }

    #[test]
    fn config_parses_and_rejects_nonsense() {
        let ok = json!({"failures": 5, "cooldown": "60s"});
        assert_eq!(
            Config::of(Some(&ok)),
            Some(Config {
                failures: 5,
                cooldown_ms: 60_000
            })
        );
        assert_eq!(Config::of(None), None);
        for bad in [
            json!({"failures": 0, "cooldown": "60s"}),
            json!({"failures": 5}),
            json!({"cooldown": "60s"}),
            json!({"failures": 5, "cooldown": "soon"}),
        ] {
            assert_eq!(Config::of(Some(&bad)), None, "{bad}");
        }
    }
}
