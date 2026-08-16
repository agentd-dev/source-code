// SPDX-License-Identifier: AGPL-3.0-only
//! The **run record** and the **scheduler** (RFC 0027 §6, §7, §9): a run is a
//! durable transition log over a workflow's DAG — per-step `{status, attempt,
//! started, finished, output, error}`, `vars`, the start payload, budgets and
//! the terminal outcome. The scheduler is pure: given a workflow and a run it
//! names the steps that are ready (all `depends_on` terminal, `when` true),
//! applies step outcomes (`on_error` routing, `goto` recovery edges), and
//! decides the run's terminal state (`finish` reached, failed, cancelled, or
//! stalled — no ready step and no finish).

use super::model::{OnError, Step, Workflow};
use super::template::{self, Data};
use crate::state::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
    Cancelled,
    Timeout,
    /// Waiting on something durable (a timer, a gate, a signal, a budget).
    Suspended,
}

impl StepStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            StepStatus::Done
                | StepStatus::Failed
                | StepStatus::Skipped
                | StepStatus::Cancelled
                | StepStatus::Timeout
        )
    }
    /// Counts as satisfied for dependents (`done | skipped`).
    pub fn is_satisfied(self) -> bool {
        matches!(self, StepStatus::Done | StepStatus::Skipped)
    }
}

/// One step's durable state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StepState {
    #[serde(default)]
    pub status: StepStatus,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The suspension detail (`{kind, timer?, deadline_ms?, …}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<Value>,
    /// The turn worker / child handle executing this step (not durable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
    /// Scheduled explicitly (an `on_error: goto` target / a `switch` case):
    /// runs even if its dependencies are not terminal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Pending,
    Running,
    /// Every non-terminal step is suspended (timers/gates/budget).
    Suspended,
    Paused,
    Completed,
    Failed,
    Refused,
    Cancelled,
    Stalled,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed
                | RunStatus::Failed
                | RunStatus::Refused
                | RunStatus::Cancelled
                | RunStatus::Stalled
        )
    }
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Running => "running",
            RunStatus::Suspended => "suspended",
            RunStatus::Paused => "paused",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Refused => "refused",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Stalled => "stalled",
        }
    }
}

/// How the run started (RFC 0027 §3 `run.start`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Start {
    pub node: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub ts: u64,
}

/// The durable run record (RFC 0025 §3.3 `run`, RFC 0027 §9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub workflow: String,
    pub workflow_hash: String,
    #[serde(default)]
    pub inputs: Value,
    #[serde(default)]
    pub status: RunStatus,
    #[serde(default)]
    pub start: Start,
    #[serde(default)]
    pub steps: BTreeMap<String, StepState>,
    #[serde(default)]
    pub vars: Map<String, Value>,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub steps_run: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Value>,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(skip)]
    pub dirty: bool,
}

impl RunState {
    pub fn new(id: &str, wf: &Workflow, start: Start, inputs: Value) -> RunState {
        let now = now_ms();
        let mut steps = BTreeMap::new();
        for s in wf.steps.keys() {
            steps.insert(s.clone(), StepState::default());
        }
        // The fired start node is done with the payload as its output; sibling
        // start nodes are skipped.
        for s in wf.start_steps() {
            let st = steps.get_mut(&s.id).expect("present");
            if s.id == start.node {
                st.status = StepStatus::Done;
                st.output = Some(start.payload.clone());
                st.started = Some(now);
                st.finished = Some(now);
                st.attempt = 1;
            } else {
                st.status = StepStatus::Skipped;
            }
        }
        RunState {
            id: id.to_string(),
            workflow: wf.name.clone(),
            workflow_hash: wf.hash.clone(),
            inputs,
            status: RunStatus::Running,
            start,
            steps,
            vars: Map::new(),
            tokens: 0,
            steps_run: 0,
            output: None,
            error: None,
            task: None,
            principal: None,
            conversation: None,
            children: Vec::new(),
            parent: None,
            attempt: 1,
            created: now,
            updated: now,
            finished: None,
            deadline_ms: wf.limits.deadline_ms.map(|d| now + d),
            dirty: true,
        }
    }

    pub fn touch(&mut self) {
        self.updated = now_ms();
        self.dirty = true;
    }

    pub fn step(&self, id: &str) -> Option<&StepState> {
        self.steps.get(id)
    }

    /// The template data view of this run (RFC 0027 §3). `memory` and `env`
    /// are supplied by the caller (`env` curated + secret-free).
    pub fn data(&self, env: Value, memory: Value) -> Data {
        let mut d = Data::new();
        d.insert("inputs".into(), self.inputs.clone());
        d.insert(
            "run".into(),
            json!({"id": self.id, "workflow": self.workflow, "start": self.start, "principal": self.principal, "task": self.task, "attempt": self.attempt, "status": self.status}),
        );
        d.insert(
            "steps".into(),
            Value::Object(
                self.steps
                    .iter()
                    .map(|(k, s)| (k.clone(), json!({"status": s.status, "output": s.output, "error": s.error, "attempt": s.attempt})))
                    .collect(),
            ),
        );
        d.insert("vars".into(), Value::Object(self.vars.clone()));
        d.insert("env".into(), env);
        d.insert("memory".into(), memory);
        d
    }

    /// Mark a step running (attempt +1). Returns the attempt.
    pub fn begin_step(&mut self, id: &str) -> u32 {
        let attempt = {
            let st = self.steps.entry(id.to_string()).or_default();
            st.status = StepStatus::Running;
            st.attempt += 1;
            st.started = Some(now_ms());
            st.finished = None;
            st.error = None;
            st.wait = None;
            st.forced = false;
            st.attempt
        };
        if !self.status.is_terminal() {
            self.status = RunStatus::Running;
        }
        self.touch();
        attempt
    }

    /// Record a step's terminal outcome.
    pub fn end_step(
        &mut self,
        id: &str,
        status: StepStatus,
        output: Option<Value>,
        error: Option<String>,
    ) {
        let st = self.steps.entry(id.to_string()).or_default();
        st.status = status;
        st.finished = Some(now_ms());
        st.output = output;
        st.error = error;
        st.wait = None;
        st.worker = None;
        self.steps_run += 1;
        self.touch();
    }

    /// Record a suspension (timer/gate/budget).
    pub fn suspend_step(&mut self, id: &str, wait: Value) {
        let st = self.steps.entry(id.to_string()).or_default();
        st.status = StepStatus::Suspended;
        st.wait = Some(wait);
        self.touch();
    }

    /// Terminal transition of the run.
    pub fn finish(&mut self, status: RunStatus, output: Option<Value>, error: Option<String>) {
        self.status = status;
        self.output = output;
        self.error = error;
        self.finished = Some(now_ms());
        // Cancel every non-terminal step.
        for st in self.steps.values_mut() {
            if !st.status.is_terminal() {
                st.status = StepStatus::Cancelled;
                st.finished = Some(now_ms());
            }
        }
        self.touch();
    }

    /// Apply an `assign`/`transform` write with a reducer mode.
    pub fn write_var(&mut self, key: &str, value: Value, mode: &str) {
        let cur = self.vars.remove(key);
        let next = match (mode, cur) {
            ("append", Some(Value::Array(mut a))) => {
                match value {
                    Value::Array(more) => a.extend(more),
                    other => a.push(other),
                }
                Value::Array(a)
            }
            ("append", Some(other)) => json!([other, value]),
            ("append", None) => match value {
                Value::Array(a) => Value::Array(a),
                other => json!([other]),
            },
            ("merge", Some(Value::Object(mut o))) => {
                if let Value::Object(more) = value {
                    for (k, v) in more {
                        o.insert(k, v);
                    }
                }
                Value::Object(o)
            }
            ("union", Some(Value::Array(mut a))) => {
                if let Value::Array(more) = value {
                    for v in more {
                        if !a.contains(&v) {
                            a.push(v);
                        }
                    }
                } else if !a.contains(&value) {
                    a.push(value);
                }
                Value::Array(a)
            }
            (_, _) => value,
        };
        self.vars.insert(key.to_string(), next);
        self.touch();
    }

    /// The steps counted as terminal/pending — for status views.
    pub fn progress(&self) -> Value {
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for s in self.steps.values() {
            *counts
                .entry(match s.status {
                    StepStatus::Pending => "pending",
                    StepStatus::Running => "running",
                    StepStatus::Done => "done",
                    StepStatus::Failed => "failed",
                    StepStatus::Skipped => "skipped",
                    StepStatus::Cancelled => "cancelled",
                    StepStatus::Timeout => "timeout",
                    StepStatus::Suspended => "suspended",
                })
                .or_default() += 1;
        }
        json!(counts)
    }

    pub fn summary(&self) -> Value {
        json!({
            "id": self.id, "workflow": self.workflow, "status": self.status, "start": self.start.node,
            "steps": self.progress(), "tokens": self.tokens, "created": self.created, "updated": self.updated,
            "finished": self.finished, "output": self.output, "error": self.error, "task": self.task, "principal": self.principal,
        })
    }
}

/// What the scheduler wants done next.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Start these steps (all deps satisfied, `when` true).
    Ready(Vec<String>),
    /// Nothing ready but work is in flight / suspended.
    Waiting,
    /// The run is stalled: no ready step, nothing in flight, no finish reached.
    Stalled,
    /// The run is terminal already.
    Terminal,
}

/// Compute the ready steps. `when` guards are evaluated over `data`; a false
/// guard skips the step (durably) — hence `&mut RunState`.
pub fn schedule(wf: &Workflow, run: &mut RunState, data: &Data) -> Result<Next, String> {
    if run.status.is_terminal() {
        return Ok(Next::Terminal);
    }
    let mut ready = Vec::new();
    let mut in_flight = false;
    let mut changed = true;
    // Iterate to a fixpoint so a newly-skipped step lets its dependents proceed.
    while changed {
        changed = false;
        for id in wf.topo_order() {
            let step = &wf.steps[&id];
            let st = run.steps.get(&id).cloned().unwrap_or_default();
            match st.status {
                StepStatus::Running => {
                    in_flight = true;
                    continue;
                }
                StepStatus::Suspended => {
                    in_flight = true;
                    continue;
                }
                s if s.is_terminal() => continue,
                _ => {}
            }
            if ready.contains(&id) {
                continue;
            }
            if st.forced {
                ready.push(id.clone());
                continue;
            }
            let deps_ok = step
                .depends_on
                .iter()
                .all(|d| run.steps.get(d).is_some_and(|s| s.status.is_satisfied()));
            let deps_failed = step.depends_on.iter().any(|d| {
                run.steps.get(d).is_some_and(|s| {
                    matches!(
                        s.status,
                        StepStatus::Failed | StepStatus::Cancelled | StepStatus::Timeout
                    )
                })
            });
            if deps_failed {
                // A failed dependency that was not routed (on_error fail already
                // failed the run) — treat like cancelled downstream.
                continue;
            }
            if !deps_ok {
                continue;
            }
            if let Some(w) = &step.when {
                let expr = w.trim().trim_start_matches("CEL:").trim();
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                match crate::cel::eval_bool(expr, &vars) {
                    Ok(true) => {}
                    Ok(false) => {
                        run.end_step(&id, StepStatus::Skipped, None, None);
                        changed = true;
                        continue;
                    }
                    Err(e) => return Err(format!("step {id:?}: when: {e}")),
                }
            }
            ready.push(id.clone());
        }
    }
    if !ready.is_empty() {
        return Ok(Next::Ready(ready));
    }
    if in_flight {
        return Ok(Next::Waiting);
    }
    Ok(Next::Stalled)
}

/// Apply a failed step's `on_error` policy: returns the steps to schedule
/// next (a `goto` target) or `Err(reason)` when the run must fail.
pub fn route_failure(
    wf: &Workflow,
    run: &mut RunState,
    step: &Step,
    error: &str,
) -> Result<Vec<String>, String> {
    match &step.on_error {
        OnError::Fail => Err(format!("step {:?} failed: {error}", step.id)),
        OnError::Continue => {
            // Downstream sees the step as satisfied-with-error: mark it done
            // with an error output so `steps.<id>.error` is inspectable.
            let st = run.steps.entry(step.id.clone()).or_default();
            st.status = StepStatus::Done;
            st.error = Some(error.to_string());
            if st.output.is_none() {
                st.output = Some(json!({"error": error}));
            }
            run.touch();
            Ok(Vec::new())
        }
        OnError::Goto(target) => {
            if !wf.steps.contains_key(target) {
                return Err(format!(
                    "step {:?}: on_error goto {target:?} does not exist",
                    step.id
                ));
            }
            // The recovery target runs even if its deps are not terminal.
            let st = run.steps.entry(target.clone()).or_default();
            st.status = StepStatus::Pending;
            st.forced = true;
            run.touch();
            Ok(vec![target.clone()])
        }
    }
}

/// Whether the run's deadline passed.
pub fn deadline_passed(run: &RunState) -> bool {
    run.deadline_ms.is_some_and(|d| now_ms() >= d)
}

/// The `env` view (curated, secret-free) the templates see.
pub fn env_view(instance: &str, run_id: &str, instruction: Option<&str>) -> Value {
    json!({"instance": instance, "run": run_id, "ts": now_ms(), "instruction": instruction})
}

/// Render a step's spec against the run data (every field, recursively).
pub fn render_spec(step: &Step, data: &Data) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    for (k, v) in &step.spec {
        if super::model::is_raw_field(&step.kind, k) {
            out.insert(k.clone(), v.clone());
            continue;
        }
        out.insert(
            k.clone(),
            template::render(v, data).map_err(|e| format!("step {:?}: {k}: {e}", step.id))?,
        );
    }
    Ok(out)
}

// These tests drive workflows with `CEL:` when-clauses, so the whole module
// needs the `cel` feature (a default build evaluates CEL fail-closed).
#[cfg(all(test, feature = "cel"))]
mod tests {
    use super::*;
    use crate::engine::model::parse_workflow;

    fn wf() -> Workflow {
        parse_workflow(&json!({
            "name": "w", "steps": {
                "s": {"kind": "once"},
                "a": {"kind": "noop", "depends_on": ["s"]},
                "b": {"kind": "noop", "depends_on": ["s"], "when": "CEL: inputs.go == true"},
                "c": {"kind": "noop", "depends_on": ["a", "b"], "on_error": "goto:fix"},
                "fix": {"kind": "noop", "depends_on": ["c"]},
                "f": {"kind": "finish", "depends_on": ["c"], "status": "completed", "output": "{{vars.x | none}}"}
            }
        }))
        .unwrap()
    }

    #[cfg(feature = "cel")]
    #[test]
    fn scheduling_guards_failures_and_terminal_states() {
        let w = wf();
        let mut run = RunState::new(
            "r1",
            &w,
            Start {
                node: "s".into(),
                payload: json!({"p": 1}),
                ts: 0,
            },
            json!({"go": false}),
        );
        assert_eq!(run.steps["s"].status, StepStatus::Done);
        assert_eq!(run.steps["s"].output, Some(json!({"p": 1})));
        let data = run.data(env_view("i", "r1", None), json!({}));
        // a is ready; b's guard is false → skipped; c waits on a.
        assert_eq!(
            schedule(&w, &mut run, &data).unwrap(),
            Next::Ready(vec!["a".to_string()])
        );
        assert_eq!(run.steps["b"].status, StepStatus::Skipped);
        run.begin_step("a");
        let data = run.data(env_view("i", "r1", None), json!({}));
        assert_eq!(schedule(&w, &mut run, &data).unwrap(), Next::Waiting);
        run.end_step("a", StepStatus::Done, Some(json!("A")), None);
        let data = run.data(env_view("i", "r1", None), json!({}));
        assert_eq!(
            schedule(&w, &mut run, &data).unwrap(),
            Next::Ready(vec!["c".to_string()])
        );
        // c fails → goto fix.
        run.begin_step("c");
        run.end_step("c", StepStatus::Failed, None, Some("boom".into()));
        let next = route_failure(&w, &mut run, w.step("c").unwrap(), "boom").unwrap();
        assert_eq!(next, vec!["fix".to_string()]);
        run.begin_step("fix");
        run.end_step("fix", StepStatus::Done, None, None);
        // f depends on c which FAILED (not satisfied) → nothing ready, nothing in flight → stalled.
        let data = run.data(env_view("i", "r1", None), json!({}));
        assert_eq!(schedule(&w, &mut run, &data).unwrap(), Next::Stalled);
        run.finish(RunStatus::Stalled, None, Some("stalled".into()));
        assert!(run.status.is_terminal());
        let data = run.data(env_view("i", "r1", None), json!({}));
        assert_eq!(schedule(&w, &mut run, &data).unwrap(), Next::Terminal);
        // Continue policy marks done-with-error.
        let mut run2 = RunState::new(
            "r2",
            &w,
            Start {
                node: "s".into(),
                payload: json!({}),
                ts: 0,
            },
            json!({"go": true}),
        );
        let mut c = w.step("c").unwrap().clone();
        c.on_error = OnError::Continue;
        run2.begin_step("c");
        run2.end_step("c", StepStatus::Failed, None, Some("e".into()));
        assert!(route_failure(&w, &mut run2, &c, "e").unwrap().is_empty());
        assert_eq!(run2.steps["c"].status, StepStatus::Done);
        assert_eq!(run2.steps["c"].error.as_deref(), Some("e"));
        // Fail policy errors.
        let mut a = w.step("a").unwrap().clone();
        a.on_error = OnError::Fail;
        assert!(route_failure(&w, &mut run2, &a, "e").is_err());
    }

    #[test]
    fn vars_reducers_and_serialization() {
        let w = wf();
        let mut run = RunState::new("r", &w, Start::default(), json!({}));
        run.write_var("l", json!([1]), "overwrite");
        run.write_var("l", json!(2), "append");
        run.write_var("l", json!([3, 4]), "append");
        assert_eq!(run.vars["l"], json!([1, 2, 3, 4]));
        run.write_var("l", json!([4, 5]), "union");
        assert_eq!(run.vars["l"], json!([1, 2, 3, 4, 5]));
        run.write_var("o", json!({"a": 1}), "overwrite");
        run.write_var("o", json!({"b": 2}), "merge");
        assert_eq!(run.vars["o"], json!({"a": 1, "b": 2}));
        run.write_var("o", json!(7), "overwrite");
        assert_eq!(run.vars["o"], json!(7));
        let v = serde_json::to_value(&run).unwrap();
        let back: RunState = serde_json::from_value(v).unwrap();
        assert_eq!(back.vars, run.vars);
        assert!(!back.dirty);
        assert_eq!(back.summary()["workflow"], json!("w"));
        // A rendered spec.
        let data = run.data(env_view("inst", "r", Some("brief")), json!({"k": "v"}));
        let mut s = w.step("f").unwrap().clone();
        s.spec
            .insert("extra".into(), json!("{{env.instruction}}/{{memory.k}}"));
        let rendered = render_spec(&s, &data).unwrap();
        assert_eq!(rendered["output"], json!("none"));
        assert_eq!(rendered["extra"], json!("brief/v"));
        assert!(!deadline_passed(&run));
    }
}
