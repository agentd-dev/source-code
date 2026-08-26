// SPDX-License-Identifier: AGPL-3.0-only
//! **Nested bodies**: `foreach`/`batch` (dynamic fan-out
//! over an array, batches with bounded parallelism and rate pacing, per-batch
//! durable progress, positional collection, `on_error: continue` slots),
//! `iterate` (a bounded structured loop with `while`/`until`/
//! `max_iterations`), `parallel` (static branches, fan-in object), `race`
//! (first branch to finish wins, the rest are cancelled) and `subgraph` (an
//! inline sub-DAG). Body steps are ordinary steps executed under a **scope**:
//! their run-record ids are `<parent>[<index>].<step>` (elements /
//! iterations), `<parent>{<branch>}.<step>` (branches) or `<parent>.<step>`
//! (subgraph); templates inside a body see `item`, `index`, `batch`,
//! `iteration`, `branch` and `steps.<sibling>` resolved within the scope.
//! The parent step's `wait` record carries the durable progress.

use super::reactor::Runtime;
use crate::engine::model::{Body, OnError, Step, Workflow};
use crate::engine::run::{RunState, StepStatus};
use crate::engine::template::Data;
use crate::state::now_ms;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The scope a nested step runs in.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// The scoped parent id (`each[3]`, `par{a}`, `loop[2]`, `sub`).
    pub parent: String,
    /// The parent step's definition (for on_error, siblings…).
    pub parent_step: Option<Step>,
    pub siblings: BTreeMap<String, Step>,
    pub item: Option<Value>,
    pub index: Option<usize>,
    pub batch: Option<usize>,
    pub iteration: Option<usize>,
    pub branch: Option<String>,
}

/// One segment of a scoped id: `name`, `name[i]`, `name{branch}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub name: String,
    pub index: Option<usize>,
    pub branch: Option<String>,
}

/// Parse `each[3].classify` → `[each[3], classify]`.
pub fn parse_scoped(id: &str) -> Vec<Segment> {
    id.split('.')
        .map(|seg| {
            if let Some((name, rest)) = seg.split_once('[') {
                Segment {
                    name: name.to_string(),
                    index: rest.trim_end_matches(']').parse().ok(),
                    branch: None,
                }
            } else if let Some((name, rest)) = seg.split_once('{') {
                Segment {
                    name: name.to_string(),
                    branch: Some(rest.trim_end_matches('}').to_string()),
                    index: None,
                }
            } else {
                Segment {
                    name: seg.to_string(),
                    index: None,
                    branch: None,
                }
            }
        })
        .collect()
}

/// The scoped id of a body step.
pub fn scoped_id(parent: &str, step: &str) -> String {
    format!("{parent}.{step}")
}

/// Whether an id is nested (has a scope).
pub fn is_scoped(id: &str) -> bool {
    id.contains('.')
}

/// The parent scope of a scoped id (`each[3].classify` → `each[3]`).
pub fn parent_of(id: &str) -> Option<&str> {
    id.rsplit_once('.').map(|(p, _)| p)
}

impl Runtime {
    /// Resolve a (possibly scoped) step id against the workflow: the step
    /// definition and its scope (`None` for a top-level step).
    pub(crate) fn resolve_step(
        &self,
        wf: &Workflow,
        run: &RunState,
        id: &str,
    ) -> Option<(Step, Option<Scope>)> {
        let segs = parse_scoped(id);
        if segs.len() == 1 && segs[0].index.is_none() && segs[0].branch.is_none() {
            return wf.step(id).cloned().map(|s| (s, None));
        }
        // Walk the chain: the first segment is a top-level step; each further
        // segment is a body/branch step of the previous.
        let mut current: Step = wf.step(&segs[0].name)?.clone();
        let mut scope = Scope::default();
        let mut parent_path = String::new();
        for (i, seg) in segs.iter().enumerate() {
            let is_last = i + 1 == segs.len();
            let this_path = if parent_path.is_empty() {
                seg_label(seg)
            } else {
                format!("{parent_path}.{}", seg_label(seg))
            };
            if is_last {
                break;
            }
            // Descend into `current`'s body/branch for the next segment.
            let body: &Body = match &seg.branch {
                Some(b) => current.branches.get(b)?,
                None => current.body.as_ref()?,
            };
            let next = &segs[i + 1];
            let child = body.steps.get(&next.name)?.clone();
            // The parent's progress lives under the parent's own (unsuffixed) id.
            let progress = run
                .steps
                .get(&strip_scope_suffix(&this_path))
                .and_then(|s| s.wait.clone())
                .unwrap_or(Value::Null);
            let item = seg.index.and_then(|ix| {
                element_item(&progress, ix).or_else(|| self.items_of(&progress).get(ix).cloned())
            });
            scope = Scope {
                parent: this_path.clone(),
                parent_step: Some(current.clone()),
                siblings: body.steps.clone(),
                item,
                index: seg.index,
                batch: seg.index.and_then(|ix| batch_of(&progress, ix)),
                iteration: if current.kind == "iterate" {
                    seg.index
                } else {
                    None
                },
                branch: seg.branch.clone(),
            };
            current = child;
            parent_path = this_path;
        }
        Some((current, Some(scope)))
    }

    /// The template data of a scoped step: the run data + scope variables +
    /// `steps.<sibling>` overlaid with the scope's instances.
    pub(crate) fn scoped_data(&mut self, run_id: &str, scope: &Scope) -> Data {
        let mut data = self.run_data(run_id);
        if let Some(it) = &scope.item {
            data.insert("item".into(), it.clone());
            if let Some(alias) = scope.parent_step.as_ref().and_then(|p| p.field_str("as"))
                && alias != "item"
            {
                data.insert(alias.to_string(), it.clone());
            }
        }
        if let Some(ix) = scope.index {
            data.insert("index".into(), json!(ix));
        }
        if let Some(b) = scope.batch {
            data.insert("batch".into(), json!(b));
        }
        if let Some(k) = scope.iteration {
            data.insert("iteration".into(), json!(k));
        }
        if let Some(b) = &scope.branch {
            data.insert("branch".into(), json!(b));
        }
        if let Some(run) = self.runs.get(run_id) {
            let mut steps = data
                .get("steps")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for sib in scope.siblings.keys() {
                let sid = scoped_id(&scope.parent, sib);
                if let Some(st) = run.steps.get(&sid) {
                    steps.insert(sib.clone(), json!({"status": st.status, "output": st.output, "error": st.error, "attempt": st.attempt}));
                }
            }
            data.insert("steps".into(), Value::Object(steps));
        }
        data
    }

    // ---- start ---------------------------------------------------------------

    /// Start a nested step (`foreach`/`batch`/`iterate`/`parallel`/`race`/`subgraph`).
    pub(crate) fn nested_start(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
    ) {
        let progress = match step.kind.as_str() {
            "foreach" | "batch" => {
                let over = spec.get("over").cloned().unwrap_or(Value::Null);
                let items = match over {
                    Value::Array(a) => a,
                    Value::Null => Vec::new(),
                    Value::Object(o) => o
                        .into_iter()
                        .map(|(k, v)| json!({"key": k, "value": v}))
                        .collect(),
                    other => {
                        self.finish_step_pub(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!(
                                "{}: over must be an array (got {})",
                                step.kind, other
                            )),
                            0,
                        );
                        return;
                    }
                };
                let batch = spec.get("batch").cloned().unwrap_or(json!({}));
                // The ceiling is operator policy (`limits.workflow.fan_out`);
                // a definition asking for more was already refused at load, so
                // this clamp only ever bounds the DEFAULT.
                let fan_out_cap = self
                    .settings
                    .limits
                    .workflow
                    .fan_out
                    .map(u64::from)
                    .unwrap_or(crate::engine::model::MAX_BATCH_PARALLEL);
                let size = spec
                    .get("size")
                    .and_then(Value::as_u64)
                    .or_else(|| batch.get("size").and_then(Value::as_u64))
                    .unwrap_or(if step.kind == "batch" { 10 } else { 1 })
                    .max(1) as usize;
                let parallel = spec
                    .get("parallel")
                    .and_then(Value::as_u64)
                    .or_else(|| batch.get("parallel").and_then(Value::as_u64))
                    // Default to real concurrency. A fan-out that runs one at a
                    // time is a loop with extra syntax, and it was the default
                    // purely because the ceiling was low — the ceiling now lives
                    // in `limits.workflow.fan_out` and over-asking is refused at
                    // load, so the default can be what people expect.
                    .unwrap_or(crate::engine::model::DEFAULT_FAN_OUT)
                    .clamp(1, fan_out_cap) as usize;
                let rate = spec
                    .get("rate")
                    .and_then(Value::as_str)
                    .or_else(|| batch.get("rate").and_then(Value::as_str))
                    .map(str::to_string);
                let group_by = spec.get("by").and_then(Value::as_str).map(str::to_string);
                // `batch.by`: group elements by a key into batches (order of first appearance).
                let items = match (&group_by, step.kind.as_str()) {
                    (Some(key), "batch") => {
                        let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
                        for it in items {
                            let k = crate::engine::data::path_of(&it, key).unwrap_or(Value::Null);
                            match groups.iter_mut().find(|(gk, _)| *gk == k) {
                                Some((_, g)) => g.push(it),
                                None => groups.push((k, vec![it])),
                            }
                        }
                        groups.into_iter().map(|(_, g)| Value::Array(g)).collect()
                    }
                    _ => items,
                };
                let total = items.len();
                let items_ref = self.store_items(run_id, step_id, items);
                json!({
                    "kind": step.kind, "total": total, "size": size, "parallel": parallel, "rate": rate,
                    "cursor": 0, "active": [], "results": {}, "done": 0, "batches_done": 0, "next_batch_at": 0,
                    "items": items_ref, "started_ms": now_ms(),
                    "collect": spec.get("collect").cloned().unwrap_or(Value::Null),
                    "as": spec.get("as").and_then(Value::as_str).unwrap_or("item"),
                })
            }
            "iterate" => {
                let max = spec
                    .get("max_iterations")
                    .and_then(Value::as_u64)
                    .unwrap_or(crate::engine::model::MAX_ITERATIONS)
                    .min(crate::engine::model::MAX_ITERATIONS);
                json!({"kind": "iterate", "iteration": 0, "max": max, "results": [], "collect": spec.get("collect").cloned().unwrap_or(Value::Null), "started_ms": now_ms()})
            }
            "parallel" | "race" => {
                let branches: Vec<String> = step.branches.keys().cloned().collect();
                // `timeout` is a COMMON_FIELD: `parse_step` lifts it into the
                // typed `step.timeout_ms` and never copies it into `spec`, so
                // reading it back out of `spec` left `timeout_ms` permanently
                // null and a `race` deadline silently never fired.
                json!({"kind": step.kind, "branches": branches, "results": {}, "errors": {}, "started_ms": now_ms(), "min_success": spec.get("min_success").and_then(Value::as_u64), "timeout_ms": step.timeout_ms})
            }
            "subgraph" => json!({"kind": "subgraph", "started_ms": now_ms()}),
            other => {
                self.finish_step_pub(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some(format!("{other} is not a nested kind")),
                    0,
                );
                return;
            }
        };
        if let Some(st) = self
            .runs
            .get_mut(run_id)
            .and_then(|r| r.steps.get_mut(step_id))
        {
            st.status = StepStatus::Running;
            st.wait = Some(progress);
        }
        if let Some(r) = self.runs.get_mut(run_id) {
            r.touch();
        }
        self.checkpoint(false);
        self.nested_advance(run_id, step_id);
    }

    /// Store a foreach's items inline or as an artifact (`limits.inline_max_bytes`).
    fn store_items(&mut self, run_id: &str, step_id: &str, items: Vec<Value>) -> Value {
        let v = Value::Array(items);
        let cap = self.settings.limits.inline_max_bytes.unwrap_or(65_536) as usize;
        if v.to_string().len() > cap {
            match self.artifacts.create(
                &self.durable,
                super::artifacts::NewArtifact {
                    name: format!("{run_id}/{step_id}/items.json").as_str(),
                    mime: Some("application/json"),
                    content: v.clone(),
                    created_by: Some("engine"),
                    sensitive: false,
                    owner: Some(run_id),
                },
            ) {
                Ok(meta) => return json!({"$artifact": meta["id"]}),
                Err(e) => self.log.warn(
                    "nested.items.artifact_fail",
                    json!({"run": run_id, "step": step_id, "err": e}),
                ),
            }
        }
        v
    }

    /// The items array of a foreach progress record (dereferencing an artifact).
    fn items_of(&self, progress: &Value) -> Vec<Value> {
        match &progress["items"] {
            Value::Array(a) => a.clone(),
            Value::Object(o) if o.get("$artifact").is_some() => o
                .get("$artifact")
                .and_then(Value::as_str)
                .and_then(|id| self.artifacts.get(id))
                .and_then(|a| a.content.as_array().cloned())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    // ---- advance -------------------------------------------------------------

    /// Drive a nested step: schedule its body steps, start new elements /
    /// iterations, detect completion. Idempotent; called after any scoped
    /// step finishes and every scheduling tick.
    pub(crate) fn nested_advance(&mut self, run_id: &str, parent_id: &str) {
        let Some(wf) = self.definition_for_run(run_id) else {
            return;
        };
        let Some(run) = self.runs.get(run_id) else {
            return;
        };
        if run.status.is_terminal() {
            return;
        }
        let Some((parent, _)) = self.resolve_step(&wf, run, parent_id) else {
            return;
        };
        let Some(progress) = run.steps.get(parent_id).and_then(|s| s.wait.clone()) else {
            return;
        };
        if run.steps.get(parent_id).map(|s| s.status) != Some(StepStatus::Running) {
            return;
        }
        match progress["kind"].as_str() {
            Some("foreach") | Some("batch") => {
                self.advance_foreach(run_id, parent_id, &parent, progress)
            }
            Some("iterate") => self.advance_iterate(run_id, parent_id, &parent, progress),
            Some("parallel") => self.advance_parallel(run_id, parent_id, &parent, progress, false),
            Some("race") => self.advance_parallel(run_id, parent_id, &parent, progress, true),
            Some("subgraph") => self.advance_subgraph(run_id, parent_id, &parent),
            _ => {}
        }
    }

    /// Schedule one body instance under `scope_id`: returns `(done, failed
    /// error)` — done when every body step is terminal.
    fn drive_body(
        &mut self,
        run_id: &str,
        scope_id: &str,
        body: &Body,
        parent: &Step,
    ) -> BodyState {
        let ids: Vec<String> = body.topo_order();
        // Ensure the instances exist.
        if let Some(run) = self.runs.get_mut(run_id) {
            for id in &ids {
                run.steps.entry(scoped_id(scope_id, id)).or_default();
            }
        }
        let mut all_terminal = true;
        let mut failed: Option<String> = None;
        let mut ready: Vec<String> = Vec::new();
        let mut in_flight = false;
        // A fixpoint over `when` skips.
        let mut changed = true;
        while changed {
            changed = false;
            let Some(snapshot) = self.runs.get(run_id).map(|r| r.steps.clone()) else {
                return BodyState::Waiting;
            };
            for id in &ids {
                let sid = scoped_id(scope_id, id);
                let st = snapshot.get(&sid).cloned().unwrap_or_default();
                match st.status {
                    StepStatus::Running | StepStatus::Suspended => {
                        in_flight = true;
                        all_terminal = false;
                        continue;
                    }
                    StepStatus::Failed | StepStatus::Timeout | StepStatus::Cancelled => {
                        if failed.is_none() {
                            failed = Some(
                                st.error
                                    .clone()
                                    .unwrap_or_else(|| format!("{id} {}", st.status.as_label())),
                            );
                        }
                        continue;
                    }
                    StepStatus::Done | StepStatus::Skipped | StepStatus::Pruned => continue,
                    StepStatus::Pending => {}
                }
                all_terminal = false;
                if ready.contains(&sid) {
                    continue;
                }
                let step = &body.steps[id];
                if st.forced {
                    ready.push(sid);
                    continue;
                }
                let deps_ok = step.depends_on.iter().all(|d| {
                    snapshot
                        .get(&scoped_id(scope_id, d))
                        .is_some_and(|s| s.status.is_satisfied())
                });
                let deps_failed = step.depends_on.iter().any(|d| {
                    snapshot.get(&scoped_id(scope_id, d)).is_some_and(|s| {
                        matches!(
                            s.status,
                            StepStatus::Failed | StepStatus::Cancelled | StepStatus::Timeout
                        )
                    })
                });
                if deps_failed {
                    // Cascade: an unrouted failed dependency cancels dependents.
                    continue;
                }
                if !deps_ok {
                    continue;
                }
                if let Some(w) = &step.when {
                    let scope = {
                        let wf = self
                            .definition_for_run(run_id)
                            .unwrap_or_else(unreachable_wf);
                        self.runs
                            .get(run_id)
                            .and_then(|r| self.resolve_step(&wf, r, &sid))
                            .and_then(|(_, s)| s)
                            .unwrap_or_default()
                    };
                    let data = self.scoped_data_view(run_id, &scope);
                    let expr = w.trim().trim_start_matches("CEL:").trim();
                    let vars: Vec<(&str, &Value)> =
                        data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    match crate::cel::eval_bool(expr, &vars) {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Some(r) = self.runs.get_mut(run_id) {
                                r.end_step(&sid, StepStatus::Skipped, None, None);
                            }
                            changed = true;
                            continue;
                        }
                        Err(e) => {
                            failed = Some(format!("{id}: when: {e}"));
                            continue;
                        }
                    }
                }
                ready.push(sid);
            }
        }
        if let Some(e) = failed {
            // A failed body step that was not routed: the instance failed —
            // unless something is still running (let it finish first).
            if !in_flight {
                return BodyState::Failed(e);
            }
            return BodyState::Waiting;
        }
        if all_terminal {
            return BodyState::Done;
        }
        for sid in ready {
            // Stop if the run or the parent step ended meanwhile (a sibling's
            // failure may have failed the parent).
            if !self.parent_running(run_id, &strip_scope_suffix(scope_id)) {
                break;
            }
            self.execute_step_pub(run_id, &sid);
        }
        let _ = parent;
        BodyState::Waiting
    }

    /// Whether the run is live and the parent step still running.
    fn parent_running(&self, run_id: &str, parent_id: &str) -> bool {
        self.runs.get(run_id).is_some_and(|r| {
            !r.status.is_terminal()
                && r.steps
                    .get(parent_id)
                    .is_some_and(|st| st.status == StepStatus::Running)
        })
    }

    /// Read-only scoped data (no memory read-through side effects needed here).
    fn scoped_data_view(&mut self, run_id: &str, scope: &Scope) -> Data {
        self.scoped_data(run_id, scope)
    }

    /// The result of a finished body instance: one sink ⇒ its output; many ⇒
    /// `{sink: output}`.
    fn body_result(&self, run_id: &str, scope_id: &str, body: &Body) -> Value {
        let Some(run) = self.runs.get(run_id) else {
            return Value::Null;
        };
        let sinks = body.sinks();
        if sinks.len() == 1 {
            return run
                .steps
                .get(&scoped_id(scope_id, &sinks[0]))
                .and_then(|s| s.output.clone())
                .unwrap_or(Value::Null);
        }
        let mut o = Map::new();
        for s in sinks {
            o.insert(
                s.clone(),
                run.steps
                    .get(&scoped_id(scope_id, &s))
                    .and_then(|st| st.output.clone())
                    .unwrap_or(Value::Null),
            );
        }
        Value::Object(o)
    }

    fn advance_foreach(
        &mut self,
        run_id: &str,
        parent_id: &str,
        parent: &Step,
        mut progress: Value,
    ) {
        let Some(body) = parent.body.clone() else {
            return;
        };
        let total = progress["total"].as_u64().unwrap_or(0) as usize;
        let size = progress["size"].as_u64().unwrap_or(1).max(1) as usize;
        let parallel = progress["parallel"].as_u64().unwrap_or(1).max(1) as usize;
        let items = self.items_of(&progress);
        let mut active: Vec<usize> = progress["active"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|x| x as usize)
                    .collect()
            })
            .unwrap_or_default();
        let mut cursor = progress["cursor"].as_u64().unwrap_or(0) as usize;
        let mut done = progress["done"].as_u64().unwrap_or(0) as usize;
        let mut batches_done = progress["batches_done"].as_u64().unwrap_or(0) as usize;
        let mut results = progress["results"].as_object().cloned().unwrap_or_default();
        let mut changed = false;
        // 1. Drive the active elements.
        for ix in active.clone() {
            if !self.parent_running(run_id, parent_id) {
                return;
            }
            let scope_id = format!("{parent_id}[{ix}]");
            match self.drive_body(run_id, &scope_id, &body, parent) {
                BodyState::Waiting => {}
                BodyState::Done => {
                    let out = self.body_result(run_id, &scope_id, &body);
                    results.insert(ix.to_string(), out);
                    active.retain(|a| *a != ix);
                    done += 1;
                    changed = true;
                }
                BodyState::Failed(e) => {
                    active.retain(|a| *a != ix);
                    done += 1;
                    changed = true;
                    match parent.on_error {
                        OnError::Continue | OnError::Goto(_) => {
                            results.insert(ix.to_string(), json!({"index": ix, "error": e}));
                        }
                        OnError::Fail => {
                            // The whole step fails; cancel the other active elements.
                            self.cancel_scoped_children(run_id, parent_id);
                            self.finish_step_pub(
                                run_id,
                                parent_id,
                                StepStatus::Failed,
                                Some(collect_results(&results, total)),
                                Some(format!("element {ix} failed: {e}")),
                                0,
                            );
                            return;
                        }
                    }
                }
            }
        }
        // 2. Batch bookkeeping: a batch is done when all its elements are.
        let batches_total = total.div_ceil(size).max(if total == 0 { 0 } else { 1 });
        while batches_done < batches_total {
            let (from, to) = (batches_done * size, ((batches_done + 1) * size).min(total));
            if (from..to).all(|i| results.contains_key(&i.to_string())) {
                batches_done += 1;
                changed = true;
                // A completed batch is a DURABILITY POINT ("a restart resumes
                // at the next batch"). Body steps are commonly pure and no
                // longer checkpoint per step, so the boundary persists here —
                // one write per batch instead of one per element.
                self.checkpoint(false);
                crate::state::kill_point("batch.k");
            } else {
                break;
            }
        }
        // 3. Start new batches (bounded by `parallel`, paced by `rate`).
        let rate = progress["rate"].as_str().map(super::subagents::parse_rate);
        let mut next_batch_at = progress["next_batch_at"].as_u64().unwrap_or(0);
        while cursor < total {
            let batches_in_flight = active
                .iter()
                .map(|i| i / size)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            if batches_in_flight >= parallel {
                break;
            }
            if rate.is_some() && now_ms() < next_batch_at {
                break;
            }
            let end = (cursor + size).min(total);
            for ix in cursor..end {
                active.push(ix);
            }
            if let Some((_, per_sec)) = rate {
                next_batch_at = now_ms() + (1000.0 / per_sec.max(0.001)) as u64;
            }
            cursor = end;
            changed = true;
        }
        // Save progress before executing (durable per batch).
        progress["active"] = json!(active);
        progress["cursor"] = json!(cursor);
        progress["done"] = json!(done);
        progress["batches_done"] = json!(batches_done);
        progress["results"] = Value::Object(results.clone());
        progress["next_batch_at"] = json!(next_batch_at);
        if let Some(st) = self
            .runs
            .get_mut(run_id)
            .and_then(|r| r.steps.get_mut(parent_id))
        {
            st.wait = Some(progress.clone());
        }
        if changed {
            if let Some(r) = self.runs.get_mut(run_id) {
                r.touch();
            }
            self.checkpoint(false);
        }
        // 4. Start the body of newly-active elements (their scoped states are fresh).
        for ix in active.clone() {
            if !self.parent_running(run_id, parent_id) {
                return;
            }
            let scope_id = format!("{parent_id}[{ix}]");
            let fresh = self.runs.get(run_id).is_some_and(|r| {
                !body
                    .steps
                    .keys()
                    .any(|k| r.steps.contains_key(&scoped_id(&scope_id, k)))
            });
            if fresh {
                let _ = items.get(ix); // the item is read through the progress record by resolve_step
                let _ = self.drive_body(run_id, &scope_id, &body, parent);
            }
        }
        // 5. Completion.
        if !self.parent_running(run_id, parent_id) {
            return;
        }
        if done >= total && cursor >= total {
            let out = collect_results(&results, total);
            self.apply_collect(run_id, &progress["collect"], &out);
            self.finish_step_pub(run_id, parent_id, StepStatus::Done, Some(out), None, 0);
        }
    }

    fn advance_iterate(
        &mut self,
        run_id: &str,
        parent_id: &str,
        parent: &Step,
        mut progress: Value,
    ) {
        let Some(body) = parent.body.clone() else {
            return;
        };
        let k = progress["iteration"].as_u64().unwrap_or(0) as usize;
        let max = progress["max"].as_u64().unwrap_or(1) as usize;
        let mut results: Vec<Value> = progress["results"].as_array().cloned().unwrap_or_default();
        let scope_id = format!("{parent_id}[{k}]");
        let started = self.runs.get(run_id).is_some_and(|r| {
            body.steps
                .keys()
                .any(|s| r.steps.contains_key(&scoped_id(&scope_id, s)))
        });
        if !started {
            // `while` guard before iteration k.
            if let Some(w) = parent.field_str("while") {
                let scope = Scope {
                    parent: scope_id.clone(),
                    parent_step: Some(parent.clone()),
                    siblings: body.steps.clone(),
                    iteration: Some(k),
                    ..Default::default()
                };
                let mut data = self.scoped_data(run_id, &scope);
                data.insert("results".into(), Value::Array(results.clone()));
                data.insert(
                    "last".into(),
                    results.last().cloned().unwrap_or(Value::Null),
                );
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                match crate::cel::eval_bool(w.trim().trim_start_matches("CEL:").trim(), &vars) {
                    Ok(true) => {}
                    Ok(false) => {
                        let out = iterate_output(&progress, &results);
                        self.apply_collect(run_id, &progress["collect"], &out);
                        self.finish_step_pub(
                            run_id,
                            parent_id,
                            StepStatus::Done,
                            Some(out),
                            None,
                            0,
                        );
                        return;
                    }
                    Err(e) => {
                        self.finish_step_pub(
                            run_id,
                            parent_id,
                            StepStatus::Failed,
                            None,
                            Some(format!("iterate.while: {e}")),
                            0,
                        );
                        return;
                    }
                }
            }
            if k >= max {
                let out = iterate_output(&progress, &results);
                self.apply_collect(run_id, &progress["collect"], &out);
                self.finish_step_pub(run_id, parent_id, StepStatus::Done, Some(out), None, 0);
                return;
            }
        }
        match self.drive_body(run_id, &scope_id, &body, parent) {
            BodyState::Waiting => {}
            BodyState::Failed(e) => {
                self.finish_step_pub(
                    run_id,
                    parent_id,
                    StepStatus::Failed,
                    Some(iterate_output(&progress, &results)),
                    Some(format!("iteration {k} failed: {e}")),
                    0,
                );
            }
            BodyState::Done => {
                let out = self.body_result(run_id, &scope_id, &body);
                results.push(out.clone());
                // `until` after iteration k.
                let mut stop = k + 1 >= max;
                if let Some(u) = parent.field_str("until")
                    && !stop
                {
                    let scope = Scope {
                        parent: scope_id.clone(),
                        parent_step: Some(parent.clone()),
                        siblings: body.steps.clone(),
                        iteration: Some(k),
                        ..Default::default()
                    };
                    let mut data = self.scoped_data(run_id, &scope);
                    data.insert("result".into(), out.clone());
                    data.insert("results".into(), Value::Array(results.clone()));
                    let vars: Vec<(&str, &Value)> =
                        data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                    match crate::cel::eval_bool(u.trim().trim_start_matches("CEL:").trim(), &vars) {
                        Ok(b) => stop = b,
                        Err(e) => {
                            self.finish_step_pub(
                                run_id,
                                parent_id,
                                StepStatus::Failed,
                                None,
                                Some(format!("iterate.until: {e}")),
                                0,
                            );
                            return;
                        }
                    }
                }
                progress["results"] = Value::Array(results.clone());
                progress["iteration"] = json!(k + 1);
                if let Some(st) = self
                    .runs
                    .get_mut(run_id)
                    .and_then(|r| r.steps.get_mut(parent_id))
                {
                    st.wait = Some(progress.clone());
                }
                if let Some(r) = self.runs.get_mut(run_id) {
                    r.touch();
                }
                self.checkpoint(false);
                if stop {
                    let out = iterate_output(&progress, &results);
                    self.apply_collect(run_id, &progress["collect"], &out);
                    self.finish_step_pub(run_id, parent_id, StepStatus::Done, Some(out), None, 0);
                } else {
                    // Next iteration on the next advance (this tick).
                    self.nested_advance(run_id, parent_id);
                }
            }
        }
    }

    fn advance_parallel(
        &mut self,
        run_id: &str,
        parent_id: &str,
        parent: &Step,
        mut progress: Value,
        race: bool,
    ) {
        let mut results = progress["results"].as_object().cloned().unwrap_or_default();
        let mut errors = progress["errors"].as_object().cloned().unwrap_or_default();
        let branches: Vec<String> = parent.branches.keys().cloned().collect();
        let mut changed = false;
        for b in &branches {
            if results.contains_key(b) || errors.contains_key(b) {
                continue;
            }
            if !self.parent_running(run_id, parent_id) {
                return;
            }
            let Some(body) = parent.branches.get(b).cloned() else {
                continue;
            };
            let scope_id = format!("{parent_id}{{{b}}}");
            match self.drive_body(run_id, &scope_id, &body, parent) {
                BodyState::Waiting => {}
                BodyState::Done => {
                    results.insert(b.clone(), self.body_result(run_id, &scope_id, &body));
                    changed = true;
                    if race {
                        // First finisher wins: cancel the others.
                        for other in &branches {
                            if other != b {
                                self.cancel_scoped_children(
                                    run_id,
                                    &format!("{parent_id}{{{other}}}"),
                                );
                            }
                        }
                        let winner = results.get(b).cloned().unwrap_or(Value::Null);
                        self.finish_step_pub(
                            run_id,
                            parent_id,
                            StepStatus::Done,
                            Some(json!({"winner": b, "output": winner})),
                            None,
                            0,
                        );
                        return;
                    }
                }
                BodyState::Failed(e) => {
                    errors.insert(b.clone(), Value::String(e.clone()));
                    changed = true;
                    if !race && parent.on_error == OnError::Fail {
                        self.cancel_scoped_children(run_id, parent_id);
                        self.finish_step_pub(
                            run_id,
                            parent_id,
                            StepStatus::Failed,
                            Some(Value::Object(results.clone())),
                            Some(format!("branch {b} failed: {e}")),
                            0,
                        );
                        return;
                    }
                }
            }
        }
        progress["results"] = Value::Object(results.clone());
        progress["errors"] = Value::Object(errors.clone());
        if let Some(st) = self
            .runs
            .get_mut(run_id)
            .and_then(|r| r.steps.get_mut(parent_id))
        {
            st.wait = Some(progress.clone());
        }
        if changed {
            if let Some(r) = self.runs.get_mut(run_id) {
                r.touch();
            }
            self.checkpoint(false);
        }
        // Race timeout.
        if race
            && let Some(t) = progress["timeout_ms"].as_u64()
            && now_ms() >= progress["started_ms"].as_u64().unwrap_or(0) + t
        {
            self.cancel_scoped_children(run_id, parent_id);
            self.finish_step_pub(
                run_id,
                parent_id,
                StepStatus::Timeout,
                None,
                Some("race: no branch finished in time".into()),
                0,
            );
            return;
        }
        if results.len() + errors.len() >= branches.len() {
            if race {
                // Every branch failed.
                self.finish_step_pub(
                    run_id,
                    parent_id,
                    StepStatus::Failed,
                    Some(Value::Object(results)),
                    Some(format!(
                        "race: every branch failed: {}",
                        Value::Object(errors)
                    )),
                    0,
                );
            } else {
                let min = progress["min_success"].as_u64().map(|m| m as usize);
                let ok = min.is_none_or(|m| results.len() >= m)
                    && (errors.is_empty() || parent.on_error != OnError::Fail);
                let mut out = Value::Object(results.clone());
                if !errors.is_empty() {
                    out["_errors"] = Value::Object(errors.clone());
                }
                if ok {
                    self.finish_step_pub(run_id, parent_id, StepStatus::Done, Some(out), None, 0);
                } else {
                    self.finish_step_pub(
                        run_id,
                        parent_id,
                        StepStatus::Failed,
                        Some(out),
                        Some("parallel: not enough branches succeeded".into()),
                        0,
                    );
                }
            }
        }
    }

    fn advance_subgraph(&mut self, run_id: &str, parent_id: &str, parent: &Step) {
        let Some(body) = parent.body.clone() else {
            return;
        };
        match self.drive_body(run_id, parent_id, &body, parent) {
            BodyState::Waiting => {}
            BodyState::Done => {
                let out = self.body_result(run_id, parent_id, &body);
                self.finish_step_pub(run_id, parent_id, StepStatus::Done, Some(out), None, 0);
            }
            BodyState::Failed(e) => self.finish_step_pub(
                run_id,
                parent_id,
                StepStatus::Failed,
                None,
                Some(format!("subgraph failed: {e}")),
                0,
            ),
        }
    }

    /// `collect: {into, mode}` — write the nested step's output into a var.
    fn apply_collect(&mut self, run_id: &str, collect: &Value, out: &Value) {
        if let Some(into) = collect.get("into").and_then(Value::as_str) {
            let mode = collect
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("overwrite")
                .to_string();
            if let Some(r) = self.runs.get_mut(run_id) {
                r.write_var(into, out.clone(), &mode);
            }
        }
    }

    /// Cancel every child (turn worker) and mark every non-terminal scoped step
    /// under `prefix` cancelled. `prefix` is either an instance scope
    /// (`par{a}` — the race winner cancelling its losers) or the parent step's
    /// own id (`each` — a failure path cancelling every instance it spawned).
    pub(crate) fn cancel_scoped_children(&mut self, run_id: &str, prefix: &str) {
        let nodes: Vec<_> = self
            .children
            .iter()
            .filter(|(_, c)| matches!(&c.kind, super::children::ChildKind::StepTurn { run, step, .. } if run == run_id && (under_scope(step, prefix) || step == prefix)))
            .map(|(n, _)| *n)
            .collect();
        for n in nodes {
            self.children.cancel(n, "nested step cancelled");
        }
        let timers = self.timers.owned_by(|o| {
            o["run"].as_str() == Some(run_id)
                && o["step"].as_str().is_some_and(|s| under_scope(s, prefix))
        });
        for t in timers {
            let _ = self.timers.disarm(&self.durable, &t);
        }
        // Pruning `pending` here is reentrant-safe: `poll_pending` collects by
        // target, not by index, precisely because this runs underneath it.
        self.pending.retain(|p| !matches!(&p.target, super::reactor::Target::Step(r, s) if r == run_id && under_scope(s, prefix)));
        if let Some(run) = self.runs.get_mut(run_id) {
            for (id, st) in run.steps.iter_mut() {
                if under_scope(id, prefix) && !st.status.is_terminal() {
                    st.status = StepStatus::Cancelled;
                    st.finished = Some(now_ms());
                }
            }
            run.touch();
        }
    }

    /// A scoped step finished: advance its parent (and, if the parent's own
    /// completion cascades, its grandparent — `nested_advance` recurses through
    /// `finish_step`).
    pub(crate) fn on_scoped_step_done(&mut self, run_id: &str, scoped: &str) {
        if let Some(parent) = parent_of(scoped).map(str::to_string) {
            // Strip the element/branch suffix to find the parent's own id.
            let parent_step_id = strip_scope_suffix(&parent);
            self.nested_advance(run_id, &parent_step_id);
        }
    }
}

/// `each[3]` → `each`; `par{a}` → `par`; `sub` → `sub`; nested `x[1].y[2]` → `x[1].y`.
pub fn strip_scope_suffix(scoped_parent: &str) -> String {
    let (head, last) = match scoped_parent.rsplit_once('.') {
        Some((h, l)) => (Some(h), l),
        None => (None, scoped_parent),
    };
    let base = last.split(['[', '{']).next().unwrap_or(last);
    match head {
        Some(h) => format!("{h}.{base}"),
        None => base.to_string(),
    }
}

/// Whether the scoped step id `id` lives *under* the scope `prefix`, at any
/// depth: a subgraph body step (`prefix.step`), or a step of one of `prefix`'s
/// element/branch instances (`prefix[3].step`, `prefix{a}.step`). The
/// delimiter test is what makes it a scope test rather than a string test —
/// `eachother.x` is not under `each`. A plain `starts_with("{prefix}.")` was
/// only ever true for the subgraph form, so the `foreach`/`parallel` failure
/// paths and the `race` timeout — which all pass the parent's own id, whose
/// children are the indexed/branch forms — cancelled nothing at all.
fn under_scope(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with(['.', '[', '{']))
}

fn seg_label(seg: &Segment) -> String {
    match (&seg.index, &seg.branch) {
        (Some(i), _) => format!("{}[{i}]", seg.name),
        (_, Some(b)) => format!("{}{{{b}}}", seg.name),
        _ => seg.name.clone(),
    }
}

/// The element `item` at `ix` from a foreach progress record. Items may be
/// artifact-backed (dereferenced by the runtime through `items_of`); here the
/// inline case is served and the artifact case falls back to `null` — the
/// runtime pre-resolves the item into `scoped_data` for artifact-backed loops.
fn element_item(progress: &Value, ix: usize) -> Option<Value> {
    progress["items"]
        .as_array()
        .and_then(|a| a.get(ix).cloned())
}

fn batch_of(progress: &Value, ix: usize) -> Option<usize> {
    let size = progress["size"].as_u64()? as usize;
    Some(ix / size.max(1))
}

fn collect_results(results: &Map<String, Value>, total: usize) -> Value {
    Value::Array(
        (0..total)
            .map(|i| results.get(&i.to_string()).cloned().unwrap_or(Value::Null))
            .collect(),
    )
}

fn iterate_output(progress: &Value, results: &[Value]) -> Value {
    if progress["collect"].is_null() && !results.is_empty() {
        results.last().cloned().unwrap_or(Value::Null)
    } else {
        Value::Array(results.to_vec())
    }
}

fn unreachable_wf() -> std::sync::Arc<Workflow> {
    // Only reached if the definition vanished mid-scheduling; an empty
    // workflow makes `resolve_step` return None and the guard skips.
    std::sync::Arc::new(Workflow {
        // A synthetic body workflow is never triggered, so it is about nothing.
        key: None,
        state: Default::default(),
        name: String::new(),
        version: 3,
        priority: Default::default(),
        unload: Default::default(),
        durable: None,
        description: None,
        armed: false,
        inputs_schema: None,
        concurrency: Default::default(),
        limits: Default::default(),
        outputs_schema: None,
        steps: BTreeMap::new(),
        hash: String::new(),
        definition: Value::Null,
    })
}

/// The state of one body instance.
pub enum BodyState {
    Waiting,
    Done,
    Failed(String),
}

pub(crate) trait StatusLabel {
    fn as_label(&self) -> &'static str;
}
impl StatusLabel for StepStatus {
    fn as_label(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
            StepStatus::Pruned => "pruned",
            StepStatus::Cancelled => "cancelled",
            StepStatus::Timeout => "timeout",
            StepStatus::Suspended => "suspended",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_ids_parse_and_strip() {
        let segs = parse_scoped("each[3].classify");
        assert_eq!(segs.len(), 2);
        assert_eq!(
            segs[0],
            Segment {
                name: "each".into(),
                index: Some(3),
                branch: None
            }
        );
        assert_eq!(
            segs[1],
            Segment {
                name: "classify".into(),
                index: None,
                branch: None
            }
        );
        let segs = parse_scoped("par{a}.inner[2].leaf");
        assert_eq!(segs[0].branch.as_deref(), Some("a"));
        assert_eq!(segs[1].index, Some(2));
        assert_eq!(scoped_id("each[3]", "classify"), "each[3].classify");
        assert_eq!(parent_of("each[3].classify"), Some("each[3]"));
        assert_eq!(strip_scope_suffix("each[3]"), "each");
        assert_eq!(strip_scope_suffix("par{a}"), "par");
        assert_eq!(strip_scope_suffix("x[1].y[2]"), "x[1].y");
        assert!(is_scoped("a.b") && !is_scoped("a"));
        // Cancellation scope: the element/branch forms are children of the
        // parent's own id too, and a longer name that merely shares the
        // prefix is not.
        assert!(under_scope("each[0].work", "each"));
        assert!(under_scope("par{a}.work", "par"));
        assert!(under_scope("sub.work", "sub"));
        assert!(under_scope("each[0].inner{b}.leaf", "each[0].inner"));
        assert!(!under_scope("eachother.work", "each"));
        assert!(!under_scope("each", "each"));
        assert_eq!(
            collect_results(&[("0".to_string(), json!(1))].into_iter().collect(), 2),
            json!([1, null])
        );
    }
}
