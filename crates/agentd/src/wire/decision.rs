// SPDX-License-Identifier: AGPL-3.0-only
//! The decide contract (RFC 0046 §5.5): a *state* and named typed *questions*
//! in, one typed answer per question out. One type serves every decision
//! point — a `decide` node, the `decide` tool, `classify`/`route`/`judge` on a
//! decision model — whether the model answers it natively (`dialect:
//! decisions`) or a chat model emulates it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The most questions one decision asks, and the most options or levels one
/// question offers — agentd's own bound, checked where a decision is
/// written; a vendor's tighter limit is the vendor's 4xx.
pub const MAX_QUESTIONS: usize = 16;
pub const MAX_OPTIONS: usize = 32;

/// What is decided about, and the questions asked of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    /// Text, a list of texts, or a JSON object.
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

/// One typed question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    /// One of named options, each with its criterion (may be empty).
    Choice {
        instructions: String,
        options: BTreeMap<String, String>,
    },
    /// A statement, answered with the probability it is true.
    Noul { instructions: String },
    /// Ordered levels, answered with a position on the scale.
    Score {
        instructions: String,
        levels: Vec<String>,
    },
}

/// One answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub value: AnswerValue,
    /// Over every option (choice) or level (score), when the model gives them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<BTreeMap<String, f64>>,
    /// How sure the model is, 0..1, when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// `false` when a chat model emulated the answer: its confidence is a
    /// self-report, not a calibrated probability.
    pub calibrated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerValue {
    Choice(String),
    Noul(f64),
    Score(f64),
}

/// The answers to one decision, by question id.
pub type Answers = BTreeMap<String, Answer>;

impl Answer {
    /// How sure the answer is, 0..1: the model's `confidence` for a choice or
    /// a score; for a noul, its distance from a coin flip, doubled
    /// (`|p − 0.5| × 2`). `None` when the model gave nothing to judge by.
    pub fn sureness(&self) -> Option<f64> {
        match self.value {
            AnswerValue::Noul(p) => Some(((p - 0.5).abs() * 2.0).clamp(0.0, 1.0)),
            _ => self.confidence,
        }
    }

    /// The answer as a workflow reads it: `{choice | noul | score (+ level),
    /// probabilities?, confidence?, calibrated}`.
    pub fn to_output(&self, question: Option<&Question>) -> Value {
        let mut o = Map::new();
        match &self.value {
            AnswerValue::Choice(c) => {
                o.insert("choice".into(), json!(c));
            }
            AnswerValue::Noul(p) => {
                o.insert("noul".into(), json!(p));
            }
            AnswerValue::Score(s) => {
                o.insert("score".into(), json!(s));
                if let Some(Question::Score { levels, .. }) = question
                    && !levels.is_empty()
                {
                    let at = (s.round().max(0.0) as usize).min(levels.len() - 1);
                    o.insert("level".into(), json!(levels[at]));
                }
            }
        }
        if let Some(p) = &self.probabilities {
            o.insert("probabilities".into(), json!(p));
        }
        if let Some(c) = self.sureness() {
            o.insert("confidence".into(), json!(c));
        }
        o.insert("calibrated".into(), json!(self.calibrated));
        Value::Object(o)
    }
}

/// How a decision's answers are handed back: per question (`decide`), or in
/// the output `classify`, `route` and `judge` have always had.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shape {
    #[default]
    Decide,
    /// One choice, `class`: `{class, confidence, probabilities?}`.
    Classify,
    /// One choice, `choice`: `{choice, confidence, probabilities?}`.
    Route,
    /// A noul, `verdict`, and with levels a score, `score`:
    /// `{verdict: pass|fail, probability, confidence, score?, level?}`.
    Judge,
}

/// The ids whose answer is less sure than `threshold` — or gives nothing to
/// judge by.
pub fn unsure(answers: &Answers, threshold: f64) -> Vec<String> {
    answers
        .iter()
        .filter(|(_, a)| a.sureness().is_none_or(|s| s < threshold))
        .map(|(id, _)| id.clone())
        .collect()
}

/// The answers as the node or tool that asked hands them on, with the model
/// that answered and whether the decision was escalated.
pub fn shaped(
    shape: Shape,
    d: &Decision,
    answers: &Answers,
    unsure: &[String],
    model: &str,
    escalated: bool,
) -> Value {
    let mut o = Map::new();
    let one = |id: &str| -> Map<String, Value> {
        answers
            .get(id)
            .map(|a| a.to_output(d.questions.get(id)))
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    };
    match shape {
        Shape::Decide => {
            for id in d.questions.keys() {
                let mut a = one(id);
                if unsure.contains(id) {
                    a.insert("unsure".into(), json!(true));
                }
                o.insert(id.clone(), Value::Object(a));
            }
        }
        Shape::Classify | Shape::Route => {
            let (id, key) = if shape == Shape::Classify {
                ("class", "class")
            } else {
                ("choice", "choice")
            };
            let mut a = one(id);
            if let Some(c) = a.remove("choice") {
                o.insert(key.into(), c);
            }
            o.extend(a);
            if !unsure.is_empty() {
                o.insert("unsure".into(), json!(true));
            }
        }
        Shape::Judge => {
            let mut verdict = one("verdict");
            if let Some(p) = verdict.remove("noul").and_then(|p| p.as_f64()) {
                o.insert(
                    "verdict".into(),
                    json!(if p >= 0.5 { "pass" } else { "fail" }),
                );
                o.insert("probability".into(), json!(p));
            }
            o.extend(verdict);
            let mut score = one("score");
            for k in ["score", "level"] {
                if let Some(v) = score.remove(k) {
                    o.insert(k.into(), v);
                }
            }
            // Without `levels`, the score a judge has always had — 0 to 10 —
            // is the verdict's probability on that scale.
            if !o.contains_key("score")
                && let Some(p) = o.get("probability").and_then(Value::as_f64)
            {
                o.insert("score".into(), json!((p * 100.0).round() / 10.0));
            }
            if let Some(p) = score.remove("probabilities") {
                o.insert("levels".into(), p);
            }
            if !unsure.is_empty() {
                o.insert("unsure".into(), json!(true));
            }
        }
    }
    o.insert("model".into(), json!(model));
    o.insert("escalated".into(), json!(escalated));
    Value::Object(o)
}

impl Decision {
    /// Parse a node's or a tool call's `questions`: each `{choice: {opt:
    /// criterion} | [opt…]}`, `{noul: "<statement>"}` or `{score: [level…]}`,
    /// with an optional `instructions`. Bounded by [`MAX_QUESTIONS`] and
    /// [`MAX_OPTIONS`].
    pub fn parse_questions(v: &Value) -> Result<BTreeMap<String, Question>, String> {
        let Some(qs) = v.as_object().filter(|o| !o.is_empty()) else {
            return Err("questions: name at least one question".into());
        };
        if qs.len() > MAX_QUESTIONS {
            return Err(format!(
                "questions: {} questions, at most {MAX_QUESTIONS}",
                qs.len()
            ));
        }
        let mut out = BTreeMap::new();
        for (id, q) in qs {
            out.insert(id.clone(), parse_question(id, q)?);
        }
        Ok(out)
    }
}

fn parse_question(id: &str, q: &Value) -> Result<Question, String> {
    let at = format!("questions.{id}");
    let Some(o) = q.as_object() else {
        return Err(format!("{at}: one of choice, noul or score"));
    };
    let instructions = match o.get("instructions") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(format!("{at}.instructions: a string")),
    };
    for k in o.keys() {
        if !matches!(k.as_str(), "choice" | "noul" | "score" | "instructions") {
            return Err(format!(
                "{at}: unknown field `{k}` (choice, noul, score, instructions)"
            ));
        }
    }
    let kinds: Vec<&str> = ["choice", "noul", "score"]
        .into_iter()
        .filter(|k| o.contains_key(*k))
        .collect();
    let [kind] = kinds.as_slice() else {
        return Err(format!("{at}: exactly one of choice, noul or score"));
    };
    match *kind {
        "choice" => {
            let options: BTreeMap<String, String> = match &o["choice"] {
                Value::Object(m) => m
                    .iter()
                    .map(|(k, v)| match v {
                        Value::String(s) => Ok((k.clone(), s.clone())),
                        Value::Null => Ok((k.clone(), String::new())),
                        _ => Err(format!("{at}.choice.{k}: a criterion string")),
                    })
                    .collect::<Result<_, _>>()?,
                Value::Array(a) => a
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(|s| (s.to_string(), String::new()))
                            .ok_or_else(|| format!("{at}.choice: option names are strings"))
                    })
                    .collect::<Result<_, _>>()?,
                _ => {
                    return Err(format!(
                        "{at}.choice: a map of option → criterion, or a list"
                    ));
                }
            };
            bounded(&at, "choice", options.len())?;
            Ok(Question::Choice {
                instructions: instructions.unwrap_or_else(|| id.to_string()),
                options,
            })
        }
        "noul" => {
            let Some(statement) = o["noul"].as_str().filter(|s| !s.trim().is_empty()) else {
                return Err(format!("{at}.noul: the statement to judge true or false"));
            };
            let instructions = match instructions {
                Some(i) => format!("{statement}\n{i}"),
                None => statement.to_string(),
            };
            Ok(Question::Noul { instructions })
        }
        _ => {
            let levels: Vec<String> = o["score"]
                .as_array()
                .ok_or_else(|| format!("{at}.score: the levels, lowest first"))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("{at}.score: level names are strings"))
                })
                .collect::<Result<_, _>>()?;
            bounded(&at, "score", levels.len())?;
            Ok(Question::Score {
                instructions: instructions.unwrap_or_else(|| id.to_string()),
                levels,
            })
        }
    }
}

fn bounded(at: &str, field: &str, n: usize) -> Result<(), String> {
    if n < 2 {
        return Err(format!("{at}.{field}: at least two"));
    }
    if n > MAX_OPTIONS {
        return Err(format!("{at}.{field}: {n}, at most {MAX_OPTIONS}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_parse_in_every_spelling_and_are_bounded() {
        let qs = Decision::parse_questions(&json!({
            "queue": {"choice": {"billing": "payment problems", "tech": "bugs"}},
            "lane": {"choice": ["fast", "slow"], "instructions": "Which lane?"},
            "urgent": {"noul": "The customer cannot work.", "instructions": "Only a full outage."},
            "mood": {"score": ["calm", "annoyed", "angry"]}
        }))
        .unwrap();
        assert!(
            matches!(&qs["queue"], Question::Choice { instructions, options }
            if instructions == "queue" && options["billing"] == "payment problems")
        );
        assert!(
            matches!(&qs["lane"], Question::Choice { instructions, .. } if instructions == "Which lane?")
        );
        assert!(matches!(&qs["urgent"], Question::Noul { instructions }
            if instructions.starts_with("The customer cannot work.") && instructions.contains("full outage")));
        assert!(matches!(&qs["mood"], Question::Score { levels, .. } if levels.len() == 3));
        for (bad, why) in [
            (json!({}), "at least one"),
            (json!({"q": {"choice": ["only"]}}), "at least two"),
            (
                json!({"q": {"noul": "x", "score": ["a", "b"]}}),
                "exactly one",
            ),
            (json!({"q": {"pick": ["a", "b"]}}), "unknown field"),
            (json!({"q": {"noul": ""}}), "statement"),
        ] {
            let e = Decision::parse_questions(&bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        let many: Map<String, Value> = (0..17)
            .map(|i| (format!("q{i}"), json!({"noul": "x"})))
            .collect();
        assert!(
            Decision::parse_questions(&Value::Object(many))
                .unwrap_err()
                .contains("at most 16")
        );
    }

    #[test]
    fn answers_are_handed_on_in_the_shape_the_node_has_always_had() {
        let d = Decision {
            state: json!("x"),
            questions: Decision::parse_questions(&json!({
                "verdict": {"noul": "It meets the rubric."},
                "score": {"score": ["poor", "fair", "good"]}
            }))
            .unwrap(),
        };
        let answers: Answers = [
            (
                "verdict".to_string(),
                Answer {
                    value: AnswerValue::Noul(0.3),
                    probabilities: None,
                    confidence: None,
                    calibrated: true,
                },
            ),
            (
                "score".to_string(),
                Answer {
                    value: AnswerValue::Score(2.0),
                    probabilities: None,
                    confidence: Some(0.9),
                    calibrated: true,
                },
            ),
        ]
        .into();
        let j = shaped(Shape::Judge, &d, &answers, &[], "jev", false);
        assert_eq!(j["verdict"], "fail");
        assert_eq!(j["probability"], 0.3);
        assert!((j["confidence"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(j["level"], "good");
        assert_eq!(j["model"], "jev");
        assert_eq!(unsure(&answers, 0.5), vec!["verdict".to_string()]);
        let bare = Decision {
            state: json!("x"),
            questions: Decision::parse_questions(&json!({"verdict": {"noul": "ok"}})).unwrap(),
        };
        let verdict_only: Answers = answers
            .iter()
            .filter(|(k, _)| *k == "verdict")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let j = shaped(Shape::Judge, &bare, &verdict_only, &[], "jev", false);
        assert_eq!(
            j["score"], 3.0,
            "the probability on the judge's 0..10 scale: {j}"
        );
        let per = shaped(
            Shape::Decide,
            &d,
            &answers,
            &["verdict".into()],
            "jev",
            true,
        );
        assert_eq!(per["verdict"]["unsure"], true);
        assert_eq!(per["score"]["level"], "good");
        assert_eq!(per["escalated"], true);
    }

    #[test]
    fn sureness_is_the_models_confidence_or_a_nouls_distance_from_a_coin_flip() {
        let noul = |p| Answer {
            value: AnswerValue::Noul(p),
            probabilities: None,
            confidence: None,
            calibrated: true,
        };
        assert_eq!(noul(0.5).sureness(), Some(0.0));
        assert!((noul(0.9).sureness().unwrap() - 0.8).abs() < 1e-9);
        assert!((noul(0.1).sureness().unwrap() - 0.8).abs() < 1e-9);
        let score = Answer {
            value: AnswerValue::Score(1.2),
            probabilities: None,
            confidence: Some(0.7),
            calibrated: false,
        };
        let q = Question::Score {
            instructions: "mood".into(),
            levels: vec!["calm".into(), "annoyed".into(), "angry".into()],
        };
        assert_eq!(score.sureness(), Some(0.7));
        let out = score.to_output(Some(&q));
        assert_eq!(out["level"], "annoyed");
        assert_eq!(out["calibrated"], false);
    }
}
