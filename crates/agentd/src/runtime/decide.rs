// SPDX-License-Identifier: AGPL-3.0-only
//! Deciding nodes and the `decide` tool (RFC 0046 §5.5, §5.6, §6, §7).
//!
//! A `decide` node asks several typed questions about one state; `classify`,
//! `route` and `judge` ask theirs as one decision when their model is a
//! decision model (on a chat model they stay the presets they are, plus
//! `confidence`). Either way the decision runs in a turn worker — admitted,
//! degraded and cancelled like any other model call — on the node's model or
//! `roles.decide`, natively or emulated, with the node's `confidence`,
//! `on_unsure` and the model's `escalate` applied there.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

use super::reactor::Runtime;
use super::steps::StepWork;
use crate::config::settings::{ModelKind, ModelRole};
use crate::engine::model::Step;
use crate::engine::run::StepStatus;
use crate::engine::template::Data;
use crate::subagent::protocol::{DecisionJob, OnUnsure, Sureness};
use crate::wire::decision::{Decision, Question, Shape};

/// The decision a deciding node asks, from its (rendered) spec.
pub(crate) fn node_decision(kind: &str, spec: &Map<String, Value>) -> Result<Decision, String> {
    let guidance = spec
        .get("prompt")
        .and_then(Value::as_str)
        .map(|p| format!(" {p}"))
        .unwrap_or_default();
    let options = |field: &str| -> Result<BTreeMap<String, String>, String> {
        let wrapped = serde_json::json!({ field: {"choice": spec.get(field).cloned().unwrap_or(Value::Null)} });
        match Decision::parse_questions(&wrapped)?.remove(field) {
            Some(Question::Choice { options, .. }) => Ok(options),
            _ => Err(format!(
                "{field}: a list of names, or a map of name → criterion"
            )),
        }
    };
    let state = |field: &str| spec.get(field).cloned().unwrap_or(Value::Null);
    let questions = match kind {
        "decide" => Decision::parse_questions(spec.get("questions").unwrap_or(&Value::Null))?,
        "classify" => BTreeMap::from([(
            "class".to_string(),
            Question::Choice {
                instructions: format!("Which class does the input belong to?{guidance}"),
                options: options("classes")?,
            },
        )]),
        "route" => BTreeMap::from([(
            "choice".to_string(),
            Question::Choice {
                instructions: format!("Where should the input go?{guidance}"),
                options: options("choices")?,
            },
        )]),
        "judge" => {
            let rubric = match spec.get("rubric") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let mut q = BTreeMap::from([(
                "verdict".to_string(),
                Question::Noul {
                    instructions: format!("The input satisfies the rubric: {rubric}{guidance}"),
                },
            )]);
            if let Some(levels) = spec.get("levels").and_then(Value::as_array) {
                q.insert(
                    "score".to_string(),
                    Question::Score {
                        instructions: format!(
                            "How well does the input satisfy the rubric? {rubric}"
                        ),
                        levels: levels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    },
                );
            }
            q
        }
        other => return Err(format!("`{other}` does not decide")),
    };
    let state = if kind == "decide" {
        state("state")
    } else {
        state("input")
    };
    Ok(Decision { state, questions })
}

/// A deciding node's threshold: its `confidence` — else `roles.decide`'s —
/// and what an answer under
/// it does — `on_unsure` as written, else `escalate` where there is a model
/// to escalate to, else `otherwise`: `fail` for a node, so a threshold the
/// author set is never quietly ignored, and `continue` for a decision guard,
/// whose marked answer prunes the step it guards. `None` without
/// `confidence`: nothing is unsure.
pub(crate) fn node_sureness(
    spec: &Map<String, Value>,
    can_escalate: bool,
    otherwise: OnUnsure,
    role_confidence: Option<f64>,
) -> Option<Sureness> {
    let confidence = spec
        .get("confidence")
        .and_then(Value::as_f64)
        .or(role_confidence)?;
    let on_unsure = spec
        .get("on_unsure")
        .and_then(Value::as_str)
        .and_then(OnUnsure::parse)
        .unwrap_or(if can_escalate {
            OnUnsure::Escalate
        } else {
            otherwise
        });
    Some(Sureness {
        confidence,
        on_unsure,
    })
}

/// The output shape a deciding node hands its answers on in.
pub(crate) fn node_shape(kind: &str) -> Shape {
    match kind {
        "classify" => Shape::Classify,
        "route" => Shape::Route,
        "judge" => Shape::Judge,
        _ => Shape::Decide,
    }
}

impl Runtime {
    /// The model reference a deciding node runs on: its `model:`, else
    /// `roles.decide`.
    pub(crate) fn deciding_reference(&self, spec: &Map<String, Value>) -> Option<String> {
        spec.get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.settings.intelligence.reference(ModelRole::Decide))
    }

    /// Whether a deciding node's model is a decision model — what sends
    /// `classify`, `route` and `judge` down the native path.
    pub(crate) fn decides_natively(&self, spec: &Map<String, Value>) -> bool {
        self.deciding_reference(spec)
            .is_some_and(|r| self.settings.intelligence.kind_of(&r) == ModelKind::Decision)
    }

    /// The model an unsure decision on `reference` is asked again on,
    /// resolved.
    pub(crate) fn escalate_config(
        &self,
        reference: Option<&str>,
    ) -> Option<crate::subagent::protocol::IntelConfig> {
        let model = self
            .settings
            .intelligence
            .tier(reference?)?
            .escalate
            .clone()?;
        Some(self.intel_config(Some(&model)))
    }

    /// The work a deciding node hands its turn: the decision, its shape, the
    /// escalate model and the threshold. A decision guard (RFC 0046 §7) is
    /// escalate-else-`continue`, as the `decide` tool is: doubt prunes the
    /// guarded step rather than failing the run.
    pub(crate) fn decision_work(
        &self,
        step: &Step,
        spec: &Map<String, Value>,
    ) -> Result<StepWork, String> {
        let decision = node_decision(&step.kind, spec)?;
        let escalate = self.escalate_config(self.deciding_reference(spec).as_deref());
        let otherwise = if step.guarding.is_some() {
            OnUnsure::Continue
        } else {
            OnUnsure::Fail
        };
        let sure = node_sureness(
            spec,
            escalate.is_some(),
            otherwise,
            self.settings
                .intelligence
                .role_confidence(ModelRole::Decide),
        );
        Ok(StepWork {
            decision: Some(Box::new(DecisionJob {
                decision,
                shape: node_shape(&step.kind),
                escalate,
            })),
            sure,
        })
    }

    /// The work for a decision a ROLE asks (preflight, the goal check): on
    /// the role's model, with the role's `confidence` — an unsure answer
    /// escalated where the model names an `escalate` model, else
    /// `unsure_default`.
    pub(crate) fn decision_work_for_role(
        &self,
        role: ModelRole,
        decision: Decision,
        unsure_default: OnUnsure,
    ) -> StepWork {
        let intel = &self.settings.intelligence;
        let escalate = self.escalate_config(intel.reference(role).as_deref());
        let sure = intel.role_confidence(role).map(|confidence| Sureness {
            confidence,
            on_unsure: if escalate.is_some() {
                OnUnsure::Escalate
            } else {
                unsure_default
            },
        });
        StepWork {
            decision: Some(Box::new(DecisionJob {
                decision,
                shape: Shape::Decide,
                escalate,
            })),
            sure,
        }
    }

    /// A deciding step answered as a decision.
    pub(crate) fn step_decide(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &Data,
    ) {
        match self.decision_work(step, spec) {
            Ok(work) => {
                self.step_turn_pub(run_id, step_id, step, spec, data, ModelRole::Decide, work)
            }
            Err(e) => self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!("{}: {e}", step.kind)),
                0,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn each_deciding_node_asks_its_question() {
        let d = node_decision(
            "classify",
            &spec(json!({"input": "card charged twice", "classes": {"billing": "payment problems", "tech": "bugs"}})),
        )
        .unwrap();
        assert_eq!(d.state, json!("card charged twice"));
        assert!(
            matches!(&d.questions["class"], Question::Choice { options, .. } if options["billing"] == "payment problems")
        );
        let d =
            node_decision("route", &spec(json!({"input": "x", "choices": ["a", "b"]}))).unwrap();
        assert!(
            matches!(&d.questions["choice"], Question::Choice { options, .. } if options.len() == 2)
        );
        let d = node_decision(
            "judge",
            &spec(json!({"input": "x", "rubric": "is polite", "levels": ["poor", "ok", "great"]})),
        )
        .unwrap();
        assert!(
            matches!(&d.questions["verdict"], Question::Noul { instructions } if instructions.contains("is polite"))
        );
        assert!(
            matches!(&d.questions["score"], Question::Score { levels, .. } if levels.len() == 3)
        );
        let d = node_decision(
            "decide",
            &spec(
                json!({"state": {"ticket": 1}, "questions": {"urgent": {"noul": "It is urgent."}}}),
            ),
        )
        .unwrap();
        assert_eq!(d.state, json!({"ticket": 1}));
        assert!(
            node_decision(
                "classify",
                &spec(json!({"input": "x", "classes": ["only"]}))
            )
            .is_err()
        );
    }

    #[test]
    fn a_threshold_is_never_quietly_ignored() {
        let fail = OnUnsure::Fail;
        assert!(node_sureness(&spec(json!({})), true, fail, None).is_none());
        let s = node_sureness(&spec(json!({"confidence": 0.8})), true, fail, None).unwrap();
        assert_eq!(s.on_unsure, OnUnsure::Escalate);
        let s = node_sureness(&spec(json!({"confidence": 0.8})), false, fail, None).unwrap();
        assert_eq!(s.on_unsure, OnUnsure::Fail);
        let s = node_sureness(
            &spec(json!({"confidence": 0.8, "on_unsure": "continue"})),
            true,
            fail,
            None,
        )
        .unwrap();
        assert_eq!(s.on_unsure, OnUnsure::Continue);
        // `roles.decide`'s confidence is the default a node's own replaces.
        let s = node_sureness(&spec(json!({})), false, fail, Some(0.6)).unwrap();
        assert_eq!(s.confidence, 0.6);
        let s = node_sureness(&spec(json!({"confidence": 0.9})), false, fail, Some(0.6)).unwrap();
        assert_eq!(s.confidence, 0.9);
        // A decision guard escalates where it can and is otherwise handed on
        // marked: its doubt prunes the guarded step.
        let guard = OnUnsure::Continue;
        let s = node_sureness(&spec(json!({"confidence": 0.8})), true, guard, None).unwrap();
        assert_eq!(s.on_unsure, OnUnsure::Escalate);
        let s = node_sureness(&spec(json!({"confidence": 0.8})), false, guard, None).unwrap();
        assert_eq!(s.on_unsure, OnUnsure::Continue);
    }
}
