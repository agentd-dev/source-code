// SPDX-License-Identifier: AGPL-3.0-only
//! The self-correcting **goal watchdog**. A supervisor-level periodic
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

use crate::obs::trace::TraceContext;
use serde_json::{Value, json};

use crate::config::settings::{Goal, GoalAction, ModelKind, ModelRole};
use crate::runtime::events::Event;
use crate::state::{Kind, now_ms};
use crate::subagent::protocol::IntelConfig;
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

    /// Charge what a judge on an executor thread spent — the goal check, the
    /// ask judge — to the instance: it ran outside every turn, and nothing
    /// else would.
    pub(crate) fn charge_judge(&mut self, role: &str, result: &Value) {
        let u = &result["usage"];
        let usage = crate::wire::intel::Usage {
            input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
        };
        if usage.total() > 0 {
            self.governor.charge(usage, &[], now_ms());
            self.counters.tokens_in += usage.input_tokens;
            self.counters.tokens_out += usage.output_tokens;
            crate::obs::metrics::record_tokens(usage.input_tokens, usage.output_tokens);
        }
        // What a decision model decided on the operator's behalf, or about
        // the goal, is on the record whatever came of it (RFC 0046 §8).
        if let Some(d) = result.get("decision") {
            let mut line = d.clone();
            line["role"] = json!(role);
            self.log.info("intel.decision", line);
        }
    }

    /// Spawn the LLM judge on an executor thread. It posts an [`Event::Background`]
    /// (`goal.judge`) with `{achieved, stuck, reason}` (+ the counter's `stuck_det`).
    fn spawn_goal_judge(&mut self, g: &Goal, state: &Value, no_progress: u64, stuck_after: u64) {
        self.goal_judge_at = Some(now_ms());
        // The goal check runs on its own role's model (`roles.goal`), on that
        // model's connection, with its fallbacks — and, on a decision model,
        // as a decision.
        let intel = self.role_config(ModelRole::Goal);
        let reference = self.settings.intelligence.reference(ModelRole::Goal);
        let decided = reference
            .as_deref()
            .is_some_and(|r| self.settings.intelligence.kind_of(r) == ModelKind::Decision);
        let escalate = self.escalate_config(reference.as_deref());
        let confidence = self.settings.intelligence.role_confidence(ModelRole::Goal);
        let tx = self.events_tx.clone();
        // The goal is the instance's, so its check is a span of the process
        // trace.
        let trace = self.trace.clone();
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
                let mut result = if decided {
                    goal_decision_call(
                        &intel,
                        escalate.as_ref(),
                        confidence,
                        &statement,
                        &summary,
                        &trace,
                    )
                } else {
                    goal_judge_call(&intel, &statement, &summary, &trace)
                };
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
    intel: &IntelConfig,
    statement: &str,
    summary: &Value,
    trace: &TraceContext,
) -> Value {
    // Built as a worker's client is — the dialect, the headers, the signer,
    // the fallbacks — so a working agent never has a broken watchdog.
    let mut client = match intel.client() {
        Ok(c) => c,
        Err(e) => return json!({"achieved": false, "stuck": false, "error": format!("intel: {e}")}),
    };
    client.set_trace(Some(trace.clone()));
    let system = "You are the goal supervisor for an autonomous agent. Given the GOAL and the agent's current STATE, judge whether the goal is achieved and whether the agent is stuck (making no meaningful progress toward it). Reply with ONLY compact JSON: {\"achieved\": <bool>, \"stuck\": <bool>, \"reason\": \"<short>\"}.";
    let user = format!(
        "GOAL: {statement}\n\nSTATE:\n{}\n\nJudge now.",
        serde_json::to_string(summary).unwrap_or_default()
    );
    let req = Request {
        model: intel.model.clone().unwrap_or_default(),
        messages: vec![Message::System(system.to_string()), Message::User(user)],
        tools: vec![],
        max_tokens: 300,
        temperature: Some(0.0),
        output_schema: Some(verdict_schema()),
        ..Default::default()
    };
    match client.complete_recorded(&req, Some(trace)) {
        Ok(resp) => {
            let mut v = parse_verdict(resp.text.as_deref().unwrap_or_default());
            v["usage"] = usage_value(resp.usage);
            v
        }
        Err(e) => json!({"achieved": false, "stuck": false, "error": format!("intel: {e}")}),
    }
}

/// A judge's token usage as its result carries it to the loop, which
/// charges it ([`Runtime::charge_judge`]).
pub(crate) fn usage_value(u: crate::wire::intel::Usage) -> Value {
    json!({"input_tokens": u.input_tokens, "output_tokens": u.output_tokens})
}

/// The goal check on a decision model (RFC 0046 §5.4): `achieved` and
/// `stuck` as two statements about the status, each true at 0.5 — and,
/// with the role's `confidence`, only when the answer is that sure; an
/// unsure answer asked once more on the `escalate` model when there is one,
/// else read as neither (§5.6).
pub(crate) fn goal_decision_call(
    intel: &IntelConfig,
    escalate: Option<&IntelConfig>,
    confidence: Option<f64>,
    statement: &str,
    summary: &Value,
    trace: &TraceContext,
) -> Value {
    use crate::wire::decision::{Decision, Question, unsure};
    let decision = Decision {
        state: json!({"goal": statement, "state": summary}),
        questions: std::collections::BTreeMap::from([
            (
                "achieved".to_string(),
                Question::Noul {
                    instructions: format!("The agent has achieved its goal: {statement}"),
                },
            ),
            (
                "stuck".to_string(),
                Question::Noul {
                    instructions:
                        "The agent is stuck: it is making no meaningful progress toward its goal."
                            .into(),
                },
            ),
        ]),
    };
    let mut usage = crate::wire::intel::Usage::default();
    let mut ask = |cfg: &IntelConfig| -> Result<(crate::wire::decision::Answers, String), String> {
        let mut client = cfg.client().map_err(|e| format!("intel: {e}"))?;
        client.set_trace(Some(trace.clone()));
        let model = cfg.model.clone().unwrap_or_default();
        let resp = client
            .complete_recorded(
                &Request {
                    model: model.clone(),
                    max_tokens: 384,
                    temperature: Some(0.0),
                    decision: Some(decision.clone()),
                    ..Default::default()
                },
                Some(trace),
            )
            .map_err(|e| format!("intel: {e}"))?;
        usage.input_tokens += resp.usage.input_tokens;
        usage.output_tokens += resp.usage.output_tokens;
        let served = resp.fallback.clone().unwrap_or(model);
        resp.decision
            .map(|a| (a, served))
            .ok_or_else(|| "intel: the goal check's answer is not a decision".to_string())
    };
    let first = ask(intel);
    let (mut answers, mut model) = match first {
        Ok(a) => a,
        Err(e) => {
            return json!({"achieved": false, "stuck": false, "error": e, "usage": usage_value(usage)});
        }
    };
    let mut escalated = false;
    if let (Some(c), Some(e)) = (confidence, escalate)
        && !unsure(&answers, c).is_empty()
    {
        match ask(e) {
            Ok((a, m)) => {
                answers = a;
                model = m;
                escalated = true;
            }
            Err(e) => {
                return json!({"achieved": false, "stuck": false, "error": e, "usage": usage_value(usage)});
            }
        }
    }
    let decision = json!({
        "model": model,
        "escalated": escalated,
        "answers": answers
            .iter()
            .map(|(id, a)| (id.clone(), a.to_output(decision.questions.get(id))))
            .collect::<serde_json::Map<String, Value>>(),
    });
    let read = |id: &str| -> (bool, f64) {
        let Some(a) = answers.get(id) else {
            return (false, 0.0);
        };
        let p = match a.value {
            crate::wire::decision::AnswerValue::Noul(p) => p,
            _ => 0.0,
        };
        let sure = escalated || confidence.is_none_or(|c| a.sureness().is_some_and(|s| s >= c));
        (p >= 0.5 && sure, p)
    };
    let ((achieved, pa), (stuck, ps)) = (read("achieved"), read("stuck"));
    json!({
        "achieved": achieved,
        "stuck": stuck,
        "reason": format!("decided by {}: achieved p={pa:.2}, stuck p={ps:.2}{}", decision["model"].as_str().unwrap_or_default(), if escalated { ", escalated" } else { "" }),
        "usage": usage_value(usage),
        "decision": decision,
    })
}

/// The goal judge's answer, which a connection that allows it holds the
/// model to (RFC 0046 §5.8); [`parse_verdict`] reads it, and any reply.
fn verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "achieved": {"type": "boolean"},
            "stuck": {"type": "boolean"},
            "reason": {"type": "string"}
        },
        "required": ["achieved", "stuck", "reason"],
        "additionalProperties": false
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The goal judge's answer is one a provider can hold the model to —
    /// strictly on OpenAI — and a reply in that shape is read as given.
    #[test]
    fn the_verdict_has_a_strict_schema_and_is_read_from_it() {
        let schema = verdict_schema();
        assert_eq!(
            crate::intel::structured::openai(&schema).map(|(_, strict)| strict),
            Some(true)
        );
        let v =
            parse_verdict(r#"{"achieved": true, "stuck": false, "reason": "the report is filed"}"#);
        assert_eq!(v["achieved"], true);
        assert_eq!(v["reason"], "the report is filed");
        crate::jsonschema::validate(&schema, &v).unwrap();
    }
}
