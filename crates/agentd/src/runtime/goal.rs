// SPDX-License-Identifier: Apache-2.0
//! The self-correcting **goal watchdog** (RFC 0026). A supervisor-level periodic
//! check — it never blocks the agent loop — of whether the configured `goal` is
//! achieved, or the agent is **stuck** (no progress across `stuck_after` checks):
//!
//!   * achievement is judged **CEL-condition first** (a cheap deterministic
//!     predicate over the live status), then an **LLM judge** for fuzzy goals
//!     (`check.via: agent|both`) — run asynchronously on an executor thread and
//!     folded back via [`Event::Background`], so it never blocks the loop;
//!   * **progress** is a monotonic activity signal (finished runs + turns); when
//!     it stalls for `stuck_after` cycles the watchdog **self-corrects**;
//!   * dispositions: `on_achieved` → `finish` (drain, exit 0) / `idle` (stop
//!     checking) / `{workflow}` (run it); `on_stuck` → `{workflow}` (a recovery /
//!     re-plan workflow — the concrete self-correction) / `replan` / `escalate` /
//!     `idle` / `finish`.
//!
//! The check runs on a durable `{"kind":"goal"}` timer, so its cadence survives a
//! restart.

use serde_json::{Value, json};

use crate::config::v2::{Goal, GoalAction};
use crate::intel::client::IntelClient;
use crate::runtime::events::Event;
use crate::state::{Kind, now_ms};
use crate::wire::intel::{Message, Request};

/// The default check cadence when `goal.check.every` is unset.
const DEFAULT_EVERY_MS: u64 = 300_000; // 5m
/// The durable key holding the watchdog's cross-check state.
const GOAL_STATE: &str = "_goal/state";
/// A judge older than this (ms) is treated as lost, so checks resume.
const JUDGE_STALE_MS: u64 = 120_000;

impl crate::runtime::reactor::Runtime {
    /// Arm the goal watchdog at startup/restore (a no-op without `goal`). Idempotent:
    /// a restart re-arms from the durable cadence.
    pub(crate) fn arm_goal(&mut self) {
        let Some(g) = self.settings.goal.clone() else {
            return;
        };
        let every = goal_every_ms(&g);
        let _ = self.timers.arm(
            &self.durable,
            now_ms() + every,
            json!({"kind": "goal"}),
            json!({}),
        );
        self.log.info(
            "goal.armed",
            json!({"every_ms": every, "statement": g.statement, "via": g.check.via.as_deref().unwrap_or("both"), "stuck_after": g.stuck_after.unwrap_or(3)}),
        );
    }

    /// A goal-check timer fired: evaluate achievement + progress, run the LLM judge
    /// if configured, and dispatch. Always re-arms the next cadence (a daemon
    /// keeps watching); `finish`/`idle` stop it via the drain / a parked note.
    pub(crate) fn on_goal_check(&mut self, _payload: &Value) {
        let Some(g) = self.settings.goal.clone() else {
            return;
        };
        let state = self.status_value();

        // 1. Achievement — the CEL condition (deterministic) decides first.
        let mut achieved = false;
        if let Some(cond) = &g.check.condition {
            let expr = cond.trim().trim_start_matches("CEL:").trim();
            achieved = crate::cel::eval_bool(expr, &[("state", &state)]).unwrap_or(false);
        }

        // 2. Progress / stuck — a monotonic activity signal (finished runs + turns).
        let progress = state["counters"]["runs_finished"].as_u64().unwrap_or(0)
            + state["counters"]["turns"].as_u64().unwrap_or(0);
        let prev = self
            .durable
            .get(Kind::Memory, GOAL_STATE)
            .ok()
            .flatten()
            .map(|e| e.state)
            .unwrap_or_else(|| json!({"no_progress": 0, "last_progress": 0}));
        let last = prev["last_progress"].as_u64().unwrap_or(0);
        let mut no_progress = prev["no_progress"].as_u64().unwrap_or(0);
        if achieved || progress > last {
            no_progress = 0;
        } else {
            no_progress += 1;
        }
        let stuck_after = g.stuck_after.unwrap_or(3) as u64;
        let stuck_det = !achieved && no_progress >= stuck_after;
        let _ = self.durable.put(
            Kind::Memory,
            GOAL_STATE,
            json!({"no_progress": if stuck_det { 0 } else { no_progress }, "last_progress": progress}),
            None,
        );

        // 3. The LLM judge (check.via = agent|both). Runs async and refines the
        //    disposition when its verdict arrives (on_goal_judge). Skipped if the
        //    CEL already achieved, or a judge is still in flight.
        let want_judge =
            matches!(g.check.via.as_deref(), Some("agent") | Some("both") | None) && !achieved;
        let judge_pending = self
            .goal_judge_at
            .is_some_and(|t| now_ms().saturating_sub(t) < JUDGE_STALE_MS);
        if want_judge && !judge_pending {
            self.spawn_goal_judge(&g, &state, no_progress, stuck_after);
        }

        // 4. Dispatch the DETERMINISTIC verdict now. When an LLM judge is running,
        //    defer the not-yet-achieved decision to it (it may find achieved/stuck);
        //    otherwise act on the CEL/counter verdict immediately.
        let mut rearm = true;
        if achieved {
            self.log.info(
                "goal.achieved",
                json!({"statement": g.statement, "via": "condition"}),
            );
            rearm = self.dispatch_goal(
                g.on_achieved.clone().unwrap_or(GoalAction::Finish),
                "achieved",
            );
        } else if stuck_det && !(want_judge && !judge_pending) {
            self.log.warn(
                "goal.stuck",
                json!({"via": "counter", "no_progress": no_progress, "stuck_after": stuck_after, "statement": g.statement}),
            );
            rearm = self.dispatch_goal(g.on_stuck.clone().unwrap_or(GoalAction::Replan), "stuck");
        } else {
            self.log.info(
                "goal.check",
                json!({"achieved": false, "no_progress": no_progress, "judge": want_judge && !judge_pending}),
            );
        }

        if rearm && !self.draining {
            let _ = self.timers.arm(
                &self.durable,
                now_ms() + goal_every_ms(&g),
                json!({"kind": "goal"}),
                json!({}),
            );
        }
    }

    /// An async goal LLM judge finished ([`Event::Background`] id `goal.judge`):
    /// fold its verdict (`achieved` / `stuck`, combined with the counter) into a
    /// disposition. Does not re-arm — `on_goal_check` already scheduled the cadence.
    pub(crate) fn on_goal_judge(&mut self, result: &Value) {
        self.goal_judge_at = None;
        let Some(g) = self.settings.goal.clone() else {
            return;
        };
        if let Some(err) = result.get("error").and_then(Value::as_str) {
            self.log.warn("goal.judge.error", json!({"error": err}));
            return;
        }
        let achieved = result["achieved"].as_bool().unwrap_or(false);
        let stuck = result["stuck"].as_bool().unwrap_or(false)
            || result["stuck_det"].as_bool().unwrap_or(false);
        let reason = result["reason"].as_str().unwrap_or("");
        if achieved {
            self.log.info(
                "goal.achieved",
                json!({"statement": g.statement, "via": "judge", "reason": reason}),
            );
            let _ = self.dispatch_goal(
                g.on_achieved.clone().unwrap_or(GoalAction::Finish),
                "achieved",
            );
        } else if stuck {
            self.log.warn(
                "goal.stuck",
                json!({"via": "judge", "statement": g.statement, "reason": reason}),
            );
            // Reset the counter so a corrected agent gets a fresh window.
            let progress = self.status_value()["counters"]["runs_finished"]
                .as_u64()
                .unwrap_or(0);
            let _ = self.durable.put(
                Kind::Memory,
                GOAL_STATE,
                json!({"no_progress": 0, "last_progress": progress}),
                None,
            );
            let _ = self.dispatch_goal(g.on_stuck.clone().unwrap_or(GoalAction::Replan), "stuck");
        } else {
            self.log
                .info("goal.judge", json!({"achieved": false, "reason": reason}));
        }
    }

    /// Spawn the LLM judge on an executor thread. It posts an [`Event::Background`]
    /// (`goal.judge`) with `{achieved, stuck, reason}` (+ the counter's `stuck_det`).
    fn spawn_goal_judge(&mut self, g: &Goal, state: &Value, no_progress: u64, stuck_after: u64) {
        self.goal_judge_at = Some(now_ms());
        let uri = self.intel_uri.clone();
        let token = self.current_intel_bearer();
        let headers = self.intel_headers.clone();
        let aws_auth = self.intel_aws_auth();
        let dialect = self.intel_dialect();
        let model = self.model.clone();
        let tx = self.events_tx.clone();
        let statement = g.statement.clone().unwrap_or_default();
        let stuck_det = no_progress >= stuck_after;
        // A compact snapshot — enough for the judge, not the whole status dump.
        let summary = json!({
            "inbox_pending": state["inbox_pending"],
            "runs": state["runs"],
            "counters": state["counters"],
            "conversations": state["conversations"],
            "uptime_ms": state["uptime_ms"],
            "no_progress_checks": no_progress,
        });
        self.log.info(
            "goal.judge.start",
            json!({"statement": statement, "no_progress": no_progress}),
        );
        std::thread::Builder::new()
            .name("goal-judge".into())
            .spawn(move || {
                let mut result = goal_judge_call(
                    &uri, token, &headers, aws_auth, dialect, &model, &statement, &summary,
                );
                if let Value::Object(m) = &mut result {
                    m.insert("stuck_det".into(), json!(stuck_det));
                }
                let _ = tx.send(Event::Background {
                    id: "goal.judge".into(),
                    result,
                });
            })
            .ok();
    }

    /// Apply a goal disposition. Returns whether the watchdog should re-arm
    /// (false = it stops: `finish` drains, `idle` parks).
    fn dispatch_goal(&mut self, action: GoalAction, why: &str) -> bool {
        match action {
            GoalAction::Finish => {
                self.begin_drain(&format!("goal watchdog: {why} → finish"));
                false
            }
            GoalAction::Idle => {
                self.log.info(
                    "goal.idle",
                    json!({"reason": why, "note": "watchdog parked"}),
                );
                false
            }
            GoalAction::Workflow(name) => {
                self.fire_goal_workflow(&name, why);
                true
            }
            GoalAction::Replan => {
                self.log.warn(
                    "goal.replan",
                    json!({"reason": why, "statement": self.goal_statement(), "note": "no progress; reconsider the approach"}),
                );
                true
            }
            GoalAction::Escalate => {
                self.log.warn(
                    "goal.escalate",
                    json!({"reason": why, "statement": self.goal_statement()}),
                );
                true
            }
        }
    }

    /// Fire a named workflow's start node (manual/once) as a goal disposition.
    fn fire_goal_workflow(&mut self, name: &str, why: &str) {
        let start = self.workflows.get(name).and_then(|w| {
            w.start_steps()
                .into_iter()
                .find(|s| s.kind == "manual" || s.kind == "once")
                .map(|s| (s.id.clone(), s.spec.clone()))
        });
        match start {
            Some((node, spec)) => {
                self.log
                    .info("goal.workflow", json!({"workflow": name, "reason": why}));
                let payload = json!({"goal_reason": why, "statement": self.goal_statement()});
                self.fire_start(name, &node, &spec, payload, "goal");
            }
            None => self.log.warn(
                "goal.workflow.missing",
                json!({"workflow": name, "note": "no manual/once start node to fire"}),
            ),
        }
    }

    fn goal_statement(&self) -> Option<String> {
        self.settings
            .goal
            .as_ref()
            .and_then(|g| g.statement.clone())
    }
}

fn goal_every_ms(g: &Goal) -> u64 {
    g.check
        .every
        .as_ref()
        .map(|d| d.0.as_millis() as u64)
        .filter(|&ms| ms > 0)
        .unwrap_or(DEFAULT_EVERY_MS)
}

/// The blocking LLM call (on an executor thread): ask the model to judge the goal.
#[allow(clippy::too_many_arguments)]
fn goal_judge_call(
    uri: &str,
    token: Option<String>,
    headers: &[(String, String)],
    aws_auth: Option<crate::config::AuthSpec>,
    dialect: Option<String>,
    model: &str,
    statement: &str,
    summary: &Value,
) -> Value {
    let client = match IntelClient::from_parts(uri, token) {
        Ok(c) => {
            #[allow(unused_mut)]
            let mut c = c
                .with_headers(headers.to_vec())
                // RFC 0031 §8: the goal-judge dial uses the same wire dialect.
                .with_dialect(dialect.as_deref());
            // RFC 0031: SigV4-sign the goal-judge LLM dial when aws auth is set.
            #[cfg(feature = "oauth")]
            if let Some(aws) = &aws_auth
                && let Ok(s) = crate::auth::aws::SigV4Signer::from_spec(aws, "intelligence")
            {
                c = c.with_signer(Some(s as std::sync::Arc<dyn ::mcp::http::RequestSigner>));
            }
            #[cfg(not(feature = "oauth"))]
            let _ = &aws_auth;
            c
        }
        Err(e) => return json!({"achieved": false, "stuck": false, "error": format!("intel: {e}")}),
    };
    let system = "You are the goal supervisor for an autonomous agent. Given the GOAL and the agent's current STATE, judge whether the goal is achieved and whether the agent is stuck (making no meaningful progress toward it). Reply with ONLY compact JSON: {\"achieved\": <bool>, \"stuck\": <bool>, \"reason\": \"<short>\"}.";
    let user = format!(
        "GOAL: {statement}\n\nSTATE:\n{}\n\nJudge now.",
        serde_json::to_string(summary).unwrap_or_default()
    );
    let req = Request {
        model: model.to_string(),
        messages: vec![Message::System(system.to_string()), Message::User(user)],
        tools: vec![],
        max_tokens: 300,
        temperature: Some(0.0),
    };
    match client.complete(&req) {
        Ok(resp) => parse_verdict(&resp.text.unwrap_or_default()),
        Err(e) => json!({"achieved": false, "stuck": false, "error": format!("intel: {e}")}),
    }
}

/// Extract `{achieved, stuck, reason}` from the model's reply (tolerant of prose
/// around the JSON object).
fn parse_verdict(text: &str) -> Value {
    let obj = extract_json_object(text).unwrap_or_else(|| json!({}));
    json!({
        "achieved": obj["achieved"].as_bool().unwrap_or(false),
        "stuck": obj["stuck"].as_bool().unwrap_or(false),
        "reason": obj["reason"].as_str().unwrap_or(""),
    })
}

/// The first balanced `{…}` JSON object in `text`, parsed (or `None`).
fn extract_json_object(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}
