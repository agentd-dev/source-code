// SPDX-License-Identifier: Apache-2.0
//! **Runs and steps** (RFC 0027 §6–§7, RFC 0026 §3): arming start nodes,
//! turning start events into durable runs, scheduling ready steps every tick,
//! executing the P3 step kinds (data steps in-loop, MCP calls on executor
//! threads, `agent`/`think` in turn workers, `sleep` on durable timers,
//! `finish` closing the run), retries + `on_error` routing, and the
//! `workflow.*` tools.

use super::children::ChildKind;
use super::events::kinds;
use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ToolCaller, ToolOutcome};
use crate::context::Msg;
use crate::engine::model::{OnError, Step, Workflow, parse_workflow};
use crate::engine::run::{
    self, Next, RunState, RunStatus, Start, StepStatus, env_view, render_spec,
};
use crate::engine::template;
use crate::governor::Admission;
use crate::registry::Caller;
use crate::state::{InboxEvent, Kind, now_ms, ulid};
use crate::subagent::protocol::{TurnKind, TurnResult, TurnSpec};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The memory key prefix runtime-created workflow definitions are stored under.
const WORKFLOW_DEF_PREFIX: &str = "_workflows/";

impl Runtime {
    // ---- definitions -----------------------------------------------------------

    /// Load the configured workflows (inline / file / uri) + runtime-created
    /// ones from the store. Errors are collected (a bad definition is refused).
    pub(crate) fn load_workflows(&mut self) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let docs = self.settings.workflows.clone();
        for doc in docs {
            let resolved = match (
                doc.get("file").and_then(Value::as_str),
                doc.get("uri").and_then(Value::as_str),
            ) {
                (Some(path), _) => match std::fs::read_to_string(path)
                    .map_err(|e| e.to_string())
                    .and_then(|t| {
                        crate::config::file::parse_document(
                            &t,
                            crate::config::file::Format::detect(
                                Some(std::path::Path::new(path)),
                                &t,
                            ),
                        )
                    }) {
                    Ok(mut d) => {
                        if d.get("name").is_none()
                            && let Some(n) = doc.get("name")
                        {
                            d["name"] = n.clone();
                        }
                        d
                    }
                    Err(e) => {
                        errs.push(format!("workflow file {path}: {e}"));
                        continue;
                    }
                },
                (None, Some(uri)) => match self.read_resource_any(uri) {
                    Ok(text) => match crate::config::file::parse_document(
                        &text,
                        crate::config::file::Format::detect(Some(std::path::Path::new(uri)), &text),
                    ) {
                        Ok(mut d) => {
                            if d.get("name").is_none()
                                && let Some(n) = doc.get("name")
                            {
                                d["name"] = n.clone();
                            }
                            d
                        }
                        Err(e) => {
                            errs.push(format!("workflow uri {uri}: {e}"));
                            continue;
                        }
                    },
                    Err(e) => {
                        errs.push(format!("workflow uri {uri}: {e}"));
                        continue;
                    }
                },
                _ => doc.clone(),
            };
            match parse_workflow(&resolved) {
                Ok(w) => {
                    self.log.info("workflow.loaded", json!({"name": w.name, "hash": &w.hash[..12], "steps": w.steps.len(), "starts": w.start_steps().iter().map(|s| s.kind.clone()).collect::<Vec<_>>()}));
                    self.workflows.insert(w.name.clone(), w);
                }
                Err(e) => errs.extend(e),
            }
        }
        // Runtime-created definitions (durable under memory/_workflows/<name>).
        if let Ok(list) = self.durable.list(Kind::Memory) {
            for ks in list {
                let Some((_, id)) = crate::store::parse_key(
                    self.durable.prefix(),
                    self.durable.instance(),
                    &ks.key,
                ) else {
                    continue;
                };
                if let Some(name) = id.strip_prefix(WORKFLOW_DEF_PREFIX)
                    && !self.workflows.contains_key(name)
                    && let Ok(Some(env)) = self.durable.get(Kind::Memory, id)
                    && let Some(def) = env.state.get("value")
                {
                    match parse_workflow(def) {
                        Ok(w) => {
                            self.log.info(
                                "workflow.loaded",
                                json!({"name": w.name, "source": "store"}),
                            );
                            self.workflows.insert(w.name.clone(), w);
                        }
                        Err(e) => self.log.warn(
                            "workflow.stored.invalid",
                            json!({"name": name, "errors": e}),
                        ),
                    }
                }
            }
        }
        // Validate tool/server references against the registry.
        for w in self.workflows.values() {
            for s in w.steps.values() {
                match s.kind.as_str() {
                    "tool" => {
                        if let Some(n) = s.field_str("name")
                            && !self.registry.allowed(&Caller::Workflow, n)
                        {
                            errs.push(format!("workflow {:?} step {:?}: tool {n:?} is unknown, disabled or not granted to workflows", w.name, s.id));
                        }
                    }
                    "mcp.tool" => {
                        if let Some(srv) = s.field_str("server")
                            && !self.mcp.contains_key(srv)
                        {
                            errs.push(format!(
                                "workflow {:?} step {:?}: mcp server {srv:?} is not connected",
                                w.name, s.id
                            ));
                        }
                    }
                    k if (k.starts_with("memory.")
                        || k.starts_with("artifact.")
                        || k.starts_with("knowledge.")
                        || k.starts_with("search."))
                        && !self.registry.allowed(&Caller::Workflow, k) =>
                    {
                        errs.push(format!("workflow {:?} step {:?}: {k} is unavailable (map it with tools.overrides or configure its server)", w.name, s.id));
                    }
                    _ => {}
                }
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// Read a resource URI (`mcp://<server>/<uri>` or a URI a connected server lists).
    pub(crate) fn read_resource_any(&self, uri: &str) -> Result<String, String> {
        if let Some(rest) = uri.strip_prefix("mcp://") {
            let (server, res) = rest
                .split_once('/')
                .ok_or("mcp:// uri needs <server>/<resource-uri>")?;
            let c = self
                .mcp
                .get(server)
                .ok_or_else(|| format!("mcp server {server:?} is not connected"))?;
            return c
                .read_resource(res)
                .map(|r| r.text())
                .map_err(|e| e.to_string());
        }
        let mut last = String::from("no connected server serves it");
        for c in self.mcp.values() {
            match c.read_resource(uri) {
                Ok(r) => return Ok(r.text()),
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }

    /// Arm start nodes (RFC 0027 §4): `once` fires now unless a live run of the
    /// workflow was restored (`policy: ensure`, default) — `always` fires anyway.
    pub(crate) fn arm_workflows(&mut self) {
        let names: Vec<String> = self.workflows.keys().cloned().collect();
        for name in names {
            let Some(w) = self.workflows.get(&name) else {
                continue;
            };
            if !w.armed {
                continue;
            }
            let starts: Vec<(String, String, Map<String, Value>)> = w
                .start_steps()
                .iter()
                .map(|s| (s.id.clone(), s.kind.clone(), s.spec.clone()))
                .collect();
            for (id, kind, spec) in starts {
                match kind.as_str() {
                    "once" => {
                        let policy = spec
                            .get("policy")
                            .and_then(Value::as_str)
                            .unwrap_or("ensure");
                        let live = self
                            .runs
                            .values()
                            .any(|r| r.workflow == name && !r.status.is_terminal());
                        let ever = self
                            .runs
                            .values()
                            .any(|r| r.workflow == name && r.start.node == id);
                        // A replayed (still pending) firing counts too — never fire twice.
                        let pending = self.inbox_queue.iter().any(|e| {
                            e.kind == kinds::START_FIRED
                                && e.payload["workflow"] == name.as_str()
                                && e.payload["node"] == id.as_str()
                        });
                        if policy == "ensure" && (live || ever || pending) {
                            self.log.info("start.once.skipped", json!({"workflow": name, "node": id, "live": live, "pending": pending}));
                            continue;
                        }
                        let inputs = spec.get("inputs").cloned().unwrap_or(json!({}));
                        let _ = self.accept_event(kinds::START_FIRED, None, json!({"workflow": name, "node": id, "payload": {"fired_at": now_ms()}, "inputs": inputs}));
                    }
                    "manual" => {}
                    // Long-lived starts are armed by arm_long_lived_starts.
                    _ => {}
                }
            }
        }
    }

    /// A start event → a run. Returns `true` when the event is consumed.
    pub(crate) fn on_start_event(&mut self, ev: &InboxEvent) -> bool {
        let name = ev.payload["workflow"].as_str().unwrap_or("").to_string();
        let Some(w) = self.workflows.get(&name).cloned() else {
            self.log.warn(
                "start.unknown_workflow",
                json!({"inbox_event": ev.id, "workflow": name}),
            );
            return true;
        };
        let node = ev.payload["node"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| default_start(&w).unwrap_or_default());
        // Concurrency (RFC 0027 §2).
        let live = self
            .runs
            .values()
            .filter(|r| r.workflow == name && !r.status.is_terminal())
            .count() as u32;
        let global_live = self
            .runs
            .values()
            .filter(|r| !r.status.is_terminal())
            .count() as u32;
        if live >= w.concurrency.max_runs
            || global_live >= self.settings.limits.max_runs.unwrap_or(8)
        {
            match w.concurrency.on_overflow {
                crate::engine::model::OnOverflow::Queue => {
                    // Keep the event pending; it is retried on a LATER tick.
                    // Nothing this tick can relieve the cap — only a live run
                    // reaching a terminal status in `schedule_runs` does, and
                    // that step has not run yet — so this must not be re-offered
                    // now. `process_inbox` drains a snapshot for exactly that
                    // reason: this push lands on the next tick's queue.
                    self.inbox_queue.push_back(ev.clone());
                    return false;
                }
                crate::engine::model::OnOverflow::Drop => {
                    self.log.warn(
                        "run.dropped",
                        json!({"workflow": name, "reason": "concurrency"}),
                    );
                    return true;
                }
                crate::engine::model::OnOverflow::Replace => {
                    if let Some(oldest) = self
                        .runs
                        .values()
                        .filter(|r| r.workflow == name && !r.status.is_terminal())
                        .min_by_key(|r| r.created)
                        .map(|r| r.id.clone())
                    {
                        self.cancel_run(&oldest, "replaced by a newer run");
                    }
                }
            }
        }
        // Inputs.
        let inputs = ev.payload.get("inputs").cloned().unwrap_or(json!({}));
        if let Some(schema) = &w.inputs_schema
            && let Err(e) = crate::jsonschema::validate(schema, &inputs)
        {
            self.log
                .warn("run.inputs.invalid", json!({"workflow": name, "errors": e}));
            return true;
        }
        // A2A pre-generates the run id so its task can link before the run starts.
        let run_id = ev
            .payload
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}-{}", name, ulid::new()));
        let mut run = RunState::new(
            &run_id,
            &w,
            Start {
                node: node.clone(),
                payload: ev.payload.get("payload").cloned().unwrap_or(Value::Null),
                ts: now_ms(),
            },
            inputs,
        );
        run.principal = ev.principal.clone();
        run.parent = ev.payload.get("parent").cloned().filter(|p| !p.is_null());
        run.conversation = ev
            .payload
            .get("conversation")
            .and_then(Value::as_str)
            .map(str::to_string);
        run.task = ev
            .payload
            .get("task")
            .and_then(Value::as_str)
            .map(str::to_string);
        // Durable before anything runs.
        if let Err(e) = self.durable.put(
            Kind::Run,
            &run_id,
            serde_json::to_value(&run).unwrap_or(Value::Null),
            Some(w.hash.clone()),
        ) {
            self.log.error(
                "run.create.fail",
                json!({"workflow": name, "err": e.to_string()}),
            );
            // The event stays pending in the DURABLE inbox (only a consumed
            // start is marked done), so dropping it from the in-memory queue
            // here would make the start silently vanish until a restart
            // replays it. Requeue it like an overflow: the next tick retries.
            self.inbox_queue.push_back(ev.clone());
            return false;
        }
        run.dirty = false;
        self.log.info(
            "run.start",
            json!({"run": run_id, "workflow": name, "node": node, "inbox_event": ev.id}),
        );
        self.counters.runs_started += 1;
        crate::obs::metrics::record_run_started();
        if node_kind(&w, &node) == Some("once") && self.job_shape {
            self.job_runs.push(run_id.clone());
        }
        // Answer a `workflow.run` waiter that asked for the id.
        if ev.kind == kinds::WORKFLOW_RUN
            && let Some(req) = ev.payload.get("request").and_then(Value::as_object)
        {
            let target = match (
                req.get("node").and_then(Value::as_u64),
                req.get("req").and_then(Value::as_u64),
                req.get("run").and_then(Value::as_str),
                req.get("step").and_then(Value::as_str),
            ) {
                (Some(n), Some(r), _, _) => {
                    Some(Target::Child(crate::supervisor::tree::NodeId(n), r))
                }
                (None, None, Some(r), Some(s)) => Some(Target::Step(r.to_string(), s.to_string())),
                _ => None,
            };
            if let Some(t) = target {
                if req.get("wait").and_then(Value::as_bool).unwrap_or(false) {
                    let deadline = now_ms()
                        + req
                            .get("timeout_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or(3_600_000);
                    self.pending.push(super::reactor::PendingTool {
                        target: t,
                        name: "workflow.run".into(),
                        kind: PendingKind::Run {
                            run: run_id.clone(),
                            deadline_ms: deadline,
                        },
                        started_ms: now_ms(),
                    });
                } else {
                    self.reply(
                        &t,
                        json!({"run": run_id, "status": "running", "workflow": name}),
                        false,
                    );
                }
            }
        }
        self.runs.insert(run_id, run);
        true
    }

    // ---- scheduling ------------------------------------------------------------

    /// Every tick: advance every live run.
    pub(crate) fn schedule_runs(&mut self) {
        if self.paused {
            return; // operator hold (a2a.pause) — steps park until resume
        }
        let ids: Vec<String> = self
            .runs
            .iter()
            .filter(|(_, r)| !r.status.is_terminal() && r.status != RunStatus::Paused)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.schedule_run(&id);
        }
    }

    /// The definition a run executes against: the one it started with (by
    /// hash) — the current definition when unchanged, else the pinned copy a
    /// reload kept. A restored run whose definition changed underneath it has
    /// no match (`resume_policy: refuse`, RFC 0027 §9).
    pub(crate) fn definition_for_run(&self, run_id: &str) -> Option<Workflow> {
        let run = self.runs.get(run_id)?;
        if let Some(w) = self.workflows.get(&run.workflow)
            && w.hash == run.workflow_hash
        {
            return Some(w.clone());
        }
        self.pinned.get(&run.workflow_hash).cloned()
    }

    fn schedule_run(&mut self, run_id: &str) {
        let Some(wf) = self.definition_for_run(run_id) else {
            // The definition vanished or changed (hash mismatch): refuse to
            // continue the run (`resume_policy: refuse`).
            let (name, hash) = self
                .runs
                .get(run_id)
                .map(|r| (r.workflow.clone(), r.workflow_hash.clone()))
                .unwrap_or_default();
            let reason = if self.workflows.contains_key(&name) {
                format!(
                    "workflow {name:?} definition changed (run pinned to hash {}); resume_policy refuse",
                    &hash[..hash.len().min(12)]
                )
            } else {
                format!("workflow {name:?} definition is gone")
            };
            self.log
                .warn("run.refused", json!({"run": run_id, "reason": reason}));
            if let Some(r) = self.runs.get_mut(run_id) {
                r.finish(RunStatus::Refused, None, Some(reason));
            }
            self.on_run_terminal(run_id);
            return;
        };
        if let Some(r) = self.runs.get(run_id)
            && run::deadline_passed(r)
        {
            self.log.warn("run.deadline", json!({"run": run_id}));
            self.cancel_children_of_run(run_id, "run deadline");
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some("deadline exceeded".into()),
            );
            self.on_run_terminal(run_id);
            return;
        }
        // A workflow that declares no budget inherits the instance's
        // `limits.run.*`. Previously a definition that was silent about limits
        // ran unbounded — the one shape where a single mistake can spend the
        // whole day's tokens, and the instance-wide knob that exists to prevent
        // it was documented as if it already applied.
        let step_cap = wf.limits.steps.or(self.settings.limits.run.steps);
        let token_cap = wf.limits.tokens.or(self.settings.limits.run.tokens);
        if let Some(cap) = step_cap
            && self.runs.get(run_id).is_some_and(|r| r.steps_run >= cap)
        {
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some(format!("exhausted steps: limit {cap}")),
            );
            self.on_run_terminal(run_id);
            return;
        }
        if let Some(cap) = token_cap
            && self.runs.get(run_id).is_some_and(|r| r.tokens >= cap)
        {
            self.runs.get_mut(run_id).expect("present").finish(
                RunStatus::Failed,
                None,
                Some(format!("exhausted tokens: limits.tokens = {cap}")),
            );
            self.on_run_terminal(run_id);
            return;
        }
        let data = self.run_data(run_id);
        let next = {
            let run = self.runs.get_mut(run_id).expect("present");
            run::schedule(&wf, run, &data)
        };
        // Nested parents in flight advance every tick (rate pacing, timeouts,
        // fresh iterations).
        let nested: Vec<String> = self
            .runs
            .get(run_id)
            .map(|r| {
                r.steps
                    .iter()
                    .filter(|(_, st)| {
                        st.status == StepStatus::Running
                            && st.wait.as_ref().is_some_and(|w| {
                                matches!(
                                    w["kind"].as_str(),
                                    Some("foreach")
                                        | Some("batch")
                                        | Some("iterate")
                                        | Some("parallel")
                                        | Some("race")
                                        | Some("subgraph")
                                )
                            })
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        for id in nested {
            self.nested_advance(run_id, &id);
        }
        match next {
            Ok(Next::Ready(steps)) => {
                for s in steps {
                    if self.draining {
                        return;
                    }
                    self.execute_step(run_id, &s);
                }
            }
            Ok(Next::Waiting) | Ok(Next::Terminal) => {}
            Ok(Next::Stalled) => {
                // "No ready step" is a symptom, not a diagnosis. Almost always
                // something upstream failed and its dependents could never
                // become ready — so name the first failed ancestor rather than
                // leaving whoever reads this to walk the graph themselves.
                let culprit = self.first_failed_step(run_id);
                let why = match &culprit {
                    Some((sid, err)) => format!(
                        "no ready step and no finish reached — step {sid:?} failed first: {err}"
                    ),
                    None => "no ready step and no finish reached".to_string(),
                };
                self.log.warn(
                    "run.stalled",
                    json!({"run": run_id, "blocked_by": culprit.as_ref().map(|(s, _)| s)}),
                );
                // A stall caused by a failure is a FAILED run, not a stalled
                // one: the distinction matters to an exit code and to a caller.
                let status = if culprit.is_some() {
                    RunStatus::Failed
                } else {
                    RunStatus::Stalled
                };
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .finish(status, None, Some(why));
                self.on_run_terminal(run_id);
            }
            Err(e) => {
                self.runs.get_mut(run_id).expect("present").finish(
                    RunStatus::Failed,
                    None,
                    Some(e),
                );
                self.on_run_terminal(run_id);
            }
        }
    }

    /// The earliest step of a run that failed, with its error.
    ///
    /// Used to explain a stall: a run with no ready step is usually a run whose
    /// dependency chain is blocked behind a failure that was routed away from
    /// `on_error: fail`, and the failure is what a person needs to see.
    fn first_failed_step(&self, run_id: &str) -> Option<(String, String)> {
        let run = self.runs.get(run_id)?;
        run.steps
            .iter()
            .filter(|(_, st)| {
                matches!(
                    st.status,
                    StepStatus::Failed | StepStatus::Timeout | StepStatus::Cancelled
                )
            })
            .min_by_key(|(_, st)| st.finished.unwrap_or(u64::MAX))
            .map(|(id, st)| {
                (
                    id.clone(),
                    st.error
                        .clone()
                        .unwrap_or_else(|| "no error recorded".into()),
                )
            })
    }

    /// The template data of a run (RFC 0027 §3): memory is a read-through map
    /// of the keys the workflow references would be nicer; here the whole
    /// (bounded) memory listing is offered lazily as `{key: value}`.
    pub(crate) fn run_data(&mut self, run_id: &str) -> template::Data {
        let env = env_view(
            &self.instance,
            run_id,
            Some(&self.instruction.text),
            self.settings.agent.prompt.as_deref(),
        );
        // Memory read-through: resolve every `memory.<key>` the definition names.
        let mut memory = Map::new();
        if let Some(wf) = self.definition_for_run(run_id) {
            let mut keys: Vec<String> = Vec::new();
            for s in wf.steps.values() {
                for (_, v) in &s.spec {
                    collect_memory_keys(v, &mut keys);
                }
                if let Some(w) = &s.when {
                    collect_memory_keys(&Value::String(w.clone()), &mut keys);
                }
            }
            for k in keys {
                if let Ok(v) = self.memory.get(&self.durable, &k)
                    && v["found"] == json!(true)
                {
                    memory.insert(k, v["value"].clone());
                }
            }
        }
        let mut data = self
            .runs
            .get(run_id)
            .map(|r| r.data(env, Value::Object(memory)))
            .unwrap_or_default();
        // Artifact-backed values (`{"$artifact": id}`) dereference transparently
        // for templates (RFC 0027 §3).
        for key in ["steps", "vars", "inputs"] {
            if let Some(v) = data.get_mut(key) {
                self.deref_artifacts(v);
            }
        }
        data
    }

    /// Replace `{"$artifact": id, …}` objects with the artifact's content.
    pub(crate) fn deref_artifacts(&self, v: &mut Value) {
        match v {
            Value::Object(o) => {
                if let Some(id) = o.get("$artifact").and_then(Value::as_str) {
                    if let Some(a) = self.artifacts.get(id) {
                        *v = a.content.clone();
                    }
                    return;
                }
                for x in o.values_mut() {
                    self.deref_artifacts(x);
                }
            }
            Value::Array(a) => {
                for x in a.iter_mut() {
                    self.deref_artifacts(x);
                }
            }
            _ => {}
        }
    }

    // ---- execution -------------------------------------------------------------

    /// Execute one ready step of a run.
    pub(crate) fn execute_step_pub(&mut self, run_id: &str, step_id: &str) {
        self.execute_step(run_id, step_id)
    }

    fn execute_step(&mut self, run_id: &str, step_id: &str) {
        if self.runs.get(run_id).is_none_or(|r| r.status.is_terminal()) {
            return;
        }
        let Some(wf) = self
            .runs
            .get(run_id)
            .and_then(|_| self.definition_for_run(run_id))
        else {
            return;
        };
        let Some((step, scope)) = self
            .runs
            .get(run_id)
            .and_then(|r| self.resolve_step(&wf, r, step_id))
        else {
            return;
        };
        let attempt = self
            .runs
            .get_mut(run_id)
            .expect("present")
            .begin_step(step_id);
        // A breakpoint set with `workflow.pause {before_step}` stops here — the
        // step has not begun, so the run can be inspected in the state it is in
        // rather than one effect later.
        if self
            .runs
            .get(run_id)
            .and_then(|r| r.break_before.as_deref())
            == Some(step_id)
        {
            if let Some(r) = self.runs.get_mut(run_id) {
                r.status = RunStatus::Paused;
                r.break_before = None;
                r.dirty = true;
                r.steps.entry(step_id.to_string()).or_default().status = StepStatus::Pending;
            }
            self.log
                .info("run.paused", json!({"run": run_id, "before_step": step_id}));
            self.checkpoint(true);
            return;
        }
        // Durable `running` BEFORE the effect (RFC 0025 §7).
        crate::state::kill_point("step.running");
        self.checkpoint(false);
        self.log.info(
            "step.start",
            json!({"run": run_id, "step": step_id, "kind": step.kind, "attempt": attempt}),
        );
        // The observation feed carried run-level counts only — "3 done, 1
        // running" — so a display client could see that a run was moving but
        // never WHAT was moving. One event per transition turns that into a
        // usable inner loop. Operator-scoped, because a step id and its kind
        // describe the workflow's internals.
        self.feed_push(
            "step",
            crate::runtime::a2a_server::FeedVis::Operator,
            json!({"run": run_id, "step": step_id, "kind": step.kind,
                   "phase": "start", "attempt": attempt}),
        );
        let data = match &scope {
            Some(sc) => self.scoped_data(run_id, sc),
            None => self.run_data(run_id),
        };
        let spec = match render_spec(&step, &data) {
            Ok(s) => s,
            Err(e) => {
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        };
        let step_caller = ToolCaller {
            run: Some(run_id.to_string()),
            step: Some(step_id.to_string()),
            req: attempt as u64,
            principal: self.runs.get(run_id).and_then(|r| r.principal.clone()),
            ctx: self.runs.get(run_id).and_then(|r| r.conversation.clone()),
            ..Default::default()
        };
        // `cache {key, ttl}`: a fresh memoized output skips the effect.
        let cache_key = match self.cache_lookup(&step, &spec, &data) {
            Some((_key, Some(hit))) => {
                self.log
                    .info("step.cache_hit", json!({"run": run_id, "step": step_id}));
                self.finish_step(run_id, step_id, StepStatus::Done, Some(hit), None, 0);
                return;
            }
            Some((key, None)) => Some(key),
            None => None,
        };
        if let Some(k) = cache_key
            && let Some(st) = self
                .runs
                .get_mut(run_id)
                .and_then(|r| r.steps.get_mut(step_id))
        {
            st.wait = Some(json!({"cache_key": k}));
        }
        match step.kind.as_str() {
            "checkpoint" => {
                // Documented as "force a durable checkpoint here rather than at
                // the next natural boundary", and implemented as an alias for
                // `noop` — so the one step whose entire purpose is to write did
                // not write. `true` forces the write rather than letting the
                // policy decide.
                self.checkpoint(true);
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    Some(Value::Null),
                    None,
                    0,
                );
            }
            "noop" => self.finish_step(
                run_id,
                step_id,
                StepStatus::Done,
                Some(Value::Null),
                None,
                0,
            ),
            "assign" | "transform" => {
                let value = spec.get("value").cloned().unwrap_or(Value::Null);
                let key = spec
                    .get("writes")
                    .and_then(Value::as_str)
                    .unwrap_or(step_id)
                    .to_string();
                // A declared `state` key carries a schema; a write that breaks
                // it fails the step where the bad value is produced, rather
                // than three steps later where a template reads a shape nobody
                // expected. This is the whole reason to declare state.
                if let Some(schema) = self
                    .definition_for_run(run_id)
                    .and_then(|wf| wf.state.get(&key).and_then(|d| d.schema.clone()))
                    && let Err(errs) = crate::jsonschema::validate(&schema, &value)
                {
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!(
                            "assign: value does not match the schema declared for state \
                             {key:?}: {}",
                            errs.join("; ")
                        )),
                        0,
                    );
                    return;
                }
                let mode = spec
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("overwrite")
                    .to_string();
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .write_var(&key, value.clone(), &mode);
                self.finish_step(run_id, step_id, StepStatus::Done, Some(value), None, 0);
            }
            "template" => {
                let out = spec
                    .get("text")
                    .cloned()
                    .or_else(|| spec.get("value").cloned())
                    .unwrap_or(Value::String(String::new()));
                self.finish_step(run_id, step_id, StepStatus::Done, Some(out), None, 0);
            }
            "validate" => {
                let value = spec.get("value").cloned().unwrap_or(Value::Null);
                let schema = spec.get("schema").cloned().unwrap_or(json!({}));
                match crate::jsonschema::validate(&schema, &value) {
                    Ok(()) => {
                        self.finish_step(run_id, step_id, StepStatus::Done, Some(value), None, 0)
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(value),
                        Some(format!(
                            "validation failed: {}",
                            crate::jsonschema::explain(&e)
                        )),
                        0,
                    ),
                }
            }
            "assert" => {
                let cond = step
                    .field_str("condition")
                    .unwrap_or("false")
                    .trim()
                    .trim_start_matches("CEL:")
                    .trim()
                    .to_string();
                let vars: Vec<(&str, &Value)> = data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                match crate::cel::eval_bool(&cond, &vars) {
                    Ok(true) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Done,
                        Some(json!(true)),
                        None,
                        0,
                    ),
                    Ok(false) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(json!(false)),
                        Some(
                            spec.get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("assertion failed: {cond}")),
                        ),
                        0,
                    ),
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("assert: {e}")),
                        0,
                    ),
                }
            }
            "fail" => {
                let msg = spec
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("deliberate failure")
                    .to_string();
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    spec.get("code").cloned(),
                    Some(msg),
                    0,
                );
            }
            "emit" => {
                if let Some(n) = spec.get("note").and_then(Value::as_str) {
                    let text = format!("run {run_id}: {n}");
                    self.note_root(text);
                }
                if let Some(a) = spec.get("audit") {
                    self.log.info(
                        "audit.emit",
                        json!({"run": run_id, "step": step_id, "audit": a}),
                    );
                }
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    spec.get("value").cloned().or(Some(Value::Null)),
                    None,
                    0,
                );
            }
            "finish" => {
                let status = match spec
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                {
                    "completed" => RunStatus::Completed,
                    "refused" => RunStatus::Refused,
                    "cancelled" => RunStatus::Cancelled,
                    _ => RunStatus::Failed,
                };
                let output = spec.get("output").cloned();
                // `outputs.schema` was checked for well-formedness at parse time
                // and then never applied — a workflow could declare the shape of
                // its result and return anything at all. Enforce it here, where
                // the result actually exists. A completed run whose output does
                // not match what it promised is a FAILED run: a caller reading
                // the declared shape is the whole reason to declare one.
                if matches!(status, RunStatus::Completed)
                    && let Some(schema) = self
                        .definition_for_run(run_id)
                        .and_then(|wf| wf.outputs_schema.clone())
                {
                    let value = output.clone().unwrap_or(Value::Null);
                    if let Err(errs) = crate::jsonschema::validate(&schema, &value) {
                        self.finish_step_pub(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!(
                                "finish: output does not match the workflow's declared \
                                 outputs.schema: {}",
                                errs.join("; ")
                            )),
                            0,
                        );
                        return;
                    }
                }
                let reason = spec
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.runs.get_mut(run_id).expect("present").end_step(
                    step_id,
                    StepStatus::Done,
                    output.clone(),
                    None,
                );
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .finish(status, output, reason);
                self.on_run_terminal(run_id);
            }
            "sleep" => {
                let ms = spec
                    .get("duration")
                    .map(crate::engine::model::duration_ms)
                    .unwrap_or(Ok(0))
                    .unwrap_or(0);
                match self.timers.arm(
                    &self.durable,
                    now_ms() + ms,
                    json!({"kind": "step", "run": run_id, "step": step_id}),
                    json!({"slept_ms": ms}),
                ) {
                    Ok(id) => {
                        self.runs.get_mut(run_id).expect("present").suspend_step(
                            step_id,
                            json!({"kind": "sleep", "timer": id, "deadline_ms": now_ms() + ms}),
                        );
                        self.checkpoint(false);
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("sleep: {e}")),
                        0,
                    ),
                }
            }
            "tool" => {
                let name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = spec.get("args").cloned().unwrap_or(json!({}));
                self.step_tool_call(run_id, step_id, &step_caller, &name, args);
            }
            "http" => self.step_http(run_id, step_id, &spec),
            k if k.starts_with("memory.")
                || k.starts_with("artifact.")
                || k.starts_with("knowledge.")
                || k.starts_with("search.") =>
            {
                let mut args = spec.clone();
                // `ttl` etc. pass through as-is; the contract validates.
                args.retain(|_, v| !v.is_null());
                self.step_tool_call(run_id, step_id, &step_caller, k, Value::Object(args));
            }
            "mcp.tool" => {
                let server = spec
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = spec
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = spec.get("args").cloned().unwrap_or(json!({}));
                let Some(client) = self.mcp.get(&server).cloned() else {
                    self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("mcp server {server:?} is not connected")),
                        0,
                    );
                    return;
                };
                let meta = json!({"agent/idempotency_key": format!("{}/{run_id}/{step_id}#{attempt}", self.instance), "agent/instance": self.instance, "agent/run": run_id});
                let timeout = step
                    .timeout_ms
                    .map(std::time::Duration::from_millis)
                    .unwrap_or(
                        self.settings
                            .limits
                            .step_timeout
                            .map(|d| d.0)
                            .unwrap_or(std::time::Duration::from_secs(600)),
                    );
                let tx = self.events_tx.clone();
                let (r, s) = (run_id.to_string(), step_id.to_string());
                self.executing
                    .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
                std::thread::Builder::new()
                    .name(format!("step:{server}.{tool}"))
                    .spawn(move || {
                        let (output, is_error, error) = match client.call_tool_with_meta_within(
                            &tool,
                            Some(args),
                            meta,
                            timeout,
                        ) {
                            Ok(res) => {
                                let v = super::worker::tool_result_value(&res);
                                if res.is_error() {
                                    (v.clone(), true, Some(res.text()))
                                } else {
                                    (v, false, None)
                                }
                            }
                            Err(e) => (Value::Null, true, Some(format!("transport error: {e}"))),
                        };
                        let _ = tx.send(super::events::Event::StepDone {
                            run: r,
                            step: s,
                            output,
                            is_error,
                            error,
                            tokens: 0,
                        });
                    })
                    .ok();
            }
            "agent" | "think" => self.step_turn(run_id, step_id, &step, &spec, &data),
            "foreach" | "batch" | "iterate" | "parallel" | "race" | "subgraph" => {
                self.nested_start(run_id, step_id, &step, &spec)
            }
            "wait" | "join" | "workflow" | "workflow.signal" | "workflow.wait"
            | "workflow.cancel" | "subagent" | "human" | "mcp.resource" | "a2a.delegate"
            | "a2a.send" | "a2a.wait" | "classify" | "extract" | "summarize" | "judge"
            | "route" => {
                self.execute_orchestration_step(run_id, step_id, &step, &spec, &data, &step_caller)
            }
            "switch" => {
                let on = spec.get("on").cloned().unwrap_or(Value::Null);
                let key = match &on {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                let cases = step
                    .field("cases")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let target = cases
                    .get(&key)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| step.field_str("default").map(str::to_string));
                match target {
                    Some(t) => {
                        // The chosen case runs even without its deps being terminal
                        // (an explicit routing edge); the other cases are skipped.
                        let scope_prefix = step_id
                            .rsplit_once('.')
                            .map(|(p, _)| format!("{p}."))
                            .unwrap_or_default();
                        let mut skipped = Vec::new();
                        // Every other target (cases + default) still pending is skipped;
                        // the chosen one is forced (runs even without its deps).
                        let mut others: Vec<String> = cases
                            .values()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        if let Some(d) = step.field_str("default") {
                            others.push(d.to_string());
                        }
                        if let Some(run) = self.runs.get_mut(run_id) {
                            for tid in others {
                                if tid == t {
                                    continue;
                                }
                                let sid = format!("{scope_prefix}{tid}");
                                if let Some(st) = run.steps.get_mut(&sid)
                                    && st.status == StepStatus::Pending
                                {
                                    // Pruned, not skipped: the case was not
                                    // chosen, so its whole tail is dead.
                                    st.status = StepStatus::Pruned;
                                    skipped.push(sid);
                                }
                            }
                            let sid = format!("{scope_prefix}{t}");
                            if let Some(st) = run.steps.get_mut(&sid) {
                                st.status = StepStatus::Pending;
                                st.forced = true;
                            }
                        }
                        self.finish_step(
                            run_id,
                            step_id,
                            StepStatus::Done,
                            Some(json!({"case": key, "goto": t, "skipped": skipped})),
                            None,
                            0,
                        );
                    }
                    None => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        Some(json!({"case": key})),
                        Some(format!("switch: no case for {key:?} and no default")),
                        0,
                    ),
                }
            }
            "map" | "filter" | "reduce" | "sort" | "dedupe" | "chunk" | "parse" => {
                let out = match step.kind.as_str() {
                    "map" => crate::engine::data::map(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        step.field_str("as").unwrap_or("item"),
                        &data,
                    ),
                    "filter" => crate::engine::data::filter(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        step.field_str("as").unwrap_or("item"),
                        &data,
                    ),
                    "reduce" => crate::engine::data::reduce(
                        spec.get("over").unwrap_or(&Value::Null),
                        step.field_str("expr").unwrap_or(""),
                        spec.get("initial").cloned().unwrap_or(Value::Null),
                        step.field_str("as").unwrap_or("item"),
                        step.field_str("acc").unwrap_or("acc"),
                        &data,
                    ),
                    "sort" => crate::engine::data::sort(
                        spec.get("over").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                        spec.get("order").and_then(Value::as_str),
                    ),
                    "dedupe" => crate::engine::data::dedupe(
                        spec.get("over").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                    ),
                    "chunk" => crate::engine::data::chunk(
                        spec.get("value").unwrap_or(&Value::Null),
                        spec.get("by").and_then(Value::as_str),
                        spec.get("size").and_then(Value::as_u64).unwrap_or(0) as usize,
                        spec.get("overlap").and_then(Value::as_u64).unwrap_or(0) as usize,
                    ),
                    _ => crate::engine::data::parse(
                        spec.get("text").and_then(Value::as_str).unwrap_or(""),
                        spec.get("format").and_then(Value::as_str),
                    ),
                };
                match out {
                    Ok(v) => self.finish_step(run_id, step_id, StepStatus::Done, Some(v), None, 0),
                    Err(e) => {
                        self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(e), 0)
                    }
                }
            }
            other => self.finish_step(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!(
                    "step kind {other:?} is not executable in this build (P4)"
                )),
                0,
            ),
        }
    }

    /// A step's internal tool call (in-loop or deferred/executor).
    fn step_tool_call(
        &mut self,
        run_id: &str,
        step_id: &str,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) {
        match self.execute_tool(caller, name, args) {
            ToolOutcome::Ready(v, is_error) => {
                let err = is_error.then(|| match &v {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                });
                self.finish_step(
                    run_id,
                    step_id,
                    if is_error {
                        StepStatus::Failed
                    } else {
                        StepStatus::Done
                    },
                    Some(v),
                    err,
                    0,
                );
            }
            ToolOutcome::Deferred(kind) => {
                let wait = match &kind {
                    PendingKind::Timer { id } => json!({"kind": "timer", "timer": id}),
                    PendingKind::Subagent { handle } => {
                        json!({"kind": "subagent", "handle": handle})
                    }
                    PendingKind::Think { .. } => json!({"kind": "think"}),
                    PendingKind::Run { run, .. } => json!({"kind": "run", "run": run}),
                    PendingKind::Await {
                        condition,
                        deadline_ms,
                    } => {
                        json!({"kind": "await", "condition": condition, "deadline_ms": deadline_ms})
                    }
                    PendingKind::Human {
                        task, deadline_ms, ..
                    } => {
                        json!({"kind": "human", "task": task, "deadline_ms": deadline_ms})
                    }
                };
                self.runs
                    .get_mut(run_id)
                    .expect("present")
                    .suspend_step(step_id, wait);
                if !matches!(kind, PendingKind::Timer { .. }) {
                    self.pending.push(super::reactor::PendingTool {
                        target: Target::Step(run_id.to_string(), step_id.to_string()),
                        name: name.to_string(),
                        kind,
                        started_ms: now_ms(),
                    });
                }
                self.checkpoint(false);
            }
            ToolOutcome::Executing => {
                self.executing
                    .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
            }
        }
    }

    pub(crate) fn step_turn_pub(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &template::Data,
    ) {
        self.step_turn(run_id, step_id, step, spec, data)
    }

    /// An `agent`/`think` step → a turn worker (budget-admitted).
    fn step_turn(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &template::Data,
    ) {
        let is_think = step.kind == "think";
        let prompt = if is_think {
            spec.get("prompt").and_then(Value::as_str).unwrap_or("")
        } else {
            spec.get("instruction")
                .and_then(Value::as_str)
                .unwrap_or("")
        }
        .to_string();
        let output_schema = spec.get("output_schema").cloned();
        let mut messages = Vec::new();
        // `reads`: fold named run data into the prompt.
        if let Some(reads) = spec.get("reads").and_then(Value::as_array) {
            for path in reads.iter().filter_map(Value::as_str) {
                if let Some(v) = template::lookup(path, data) {
                    messages.push(Msg::system(format!("{path} = {v}")));
                }
            }
        }
        // `context`: seed messages.
        if let Some(seed) = spec.get("context").and_then(Value::as_array) {
            for m in seed {
                match (m["role"].as_str(), m["content"].as_str()) {
                    (Some("system"), Some(c)) => messages.push(Msg::system(c)),
                    (Some("assistant"), Some(c)) => {
                        messages.push(Msg::assistant(Some(c.to_string()), vec![]))
                    }
                    (_, Some(c)) => messages.push(Msg::user(c, None)),
                    _ => {}
                }
            }
        }
        let mut user = prompt.clone();
        if let Some(c) = spec.get("output_contract").and_then(Value::as_str) {
            user.push_str(&format!("\n\nOutput contract:\n{c}"));
        }
        if let Some(s) = &output_schema {
            user.push_str(&format!(
                "\n\nReply with ONLY one JSON object matching this JSON Schema:\n{s}"
            ));
        }
        messages.push(Msg::user(user, None));
        // Skills for the step.
        let skill_bodies: Vec<String> = step
            .skills
            .iter()
            .chain(
                spec.get("skills")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                    .iter(),
            )
            .filter_map(|name| {
                let mcp = self.mcp.clone();
                let resolver = move |server: &str| -> Option<
                    std::sync::Arc<dyn crate::context::skills::SkillServer>,
                > {
                    mcp.get(server).map(|c| {
                        c.clone() as std::sync::Arc<dyn crate::context::skills::SkillServer>
                    })
                };
                self.skills
                    .load(name, None, &resolver)
                    .ok()
                    .map(|b| format!("### Skill: {}\n{}", b.name, b.body))
            })
            .collect();
        let extra = if skill_bodies.is_empty() {
            None
        } else {
            Some(format!(
                "Loaded skills — follow these instructions when relevant:\n{}",
                skill_bodies.join("\n\n")
            ))
        };
        let system = match spec.get("system").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None if is_think => format!(
                "You are the reasoning module of {}. Reply with {}. No tools are available.",
                self.instance,
                if output_schema.is_some() {
                    "ONLY one JSON object matching the schema"
                } else {
                    "your conclusion"
                }
            ),
            None => self.system_prompt(None, extra.as_deref()),
        };
        let (tools, internal, routes) = if is_think {
            (Vec::new(), Vec::new(), BTreeMap::new())
        } else {
            let allow: Option<Vec<String>> = spec.get("tools").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
            self.tool_plan(&Caller::Workflow, allow.as_deref())
        };
        let servers: Vec<String> = match spec.get("servers").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            None => routes
                .values()
                .map(|(s, _)| s.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect(),
        };
        // Budget admission (RFC 0026 §7): the run's scope + the instance.
        let est: u64 = messages.iter().map(Msg::est_tokens).sum::<u64>()
            + crate::context::tokens::estimate(&system)
            + 4096;
        let scopes = self.run_scopes(run_id);
        let reservation = match self.governor.admit(est, &scopes, now_ms()) {
            Admission::Ok { reservation, model } => {
                if let Some(m) = model {
                    self.log.info(
                        "budget.degraded",
                        json!({"run": run_id, "step": step_id, "model": m}),
                    );
                }
                Some(reservation)
            }
            Admission::Wait { until_ms, reason } => {
                self.log.info(
                    "budget.wait",
                    json!({"run": run_id, "step": step_id, "until_ms": until_ms, "reason": reason}),
                );
                crate::state::kill_point("budget.waiting");
                match self.timers.arm(
                    &self.durable,
                    until_ms,
                    json!({"kind": "step_budget", "run": run_id, "step": step_id}),
                    Value::Null,
                ) {
                    Ok(id) => {
                        self.runs.get_mut(run_id).expect("present").suspend_step(step_id, json!({"kind": "waiting_budget", "timer": id, "until_ms": until_ms, "reason": reason}));
                        self.checkpoint(false);
                    }
                    Err(e) => self.finish_step(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("budget wait: {e}")),
                        0,
                    ),
                }
                return;
            }
            Admission::Refuse { reason } | Admission::Fail { reason } => {
                self.finish_step(run_id, step_id, StepStatus::Failed, None, Some(reason), 0);
                return;
            }
        };
        let limits = spec.get("limits").cloned().unwrap_or(json!({}));
        let max_steps = limits
            .get("steps")
            .and_then(Value::as_u64)
            .map(|s| s as u32)
            .unwrap_or(self.settings.limits.run.steps());
        let max_tokens = step
            .budget
            .or_else(|| limits.get("tokens").and_then(Value::as_u64))
            .unwrap_or(self.settings.limits.run.tokens());
        let deadline_ms = step.timeout_ms.unwrap_or(
            self.settings
                .limits
                .step_timeout
                .map(|d| d.0.as_millis() as u64)
                .unwrap_or(600_000),
        );
        let spec = TurnSpec {
            kind: if is_think {
                TurnKind::Think
            } else {
                TurnKind::Agent
            },
            system,
            messages,
            tools,
            internal,
            mcp_routes: routes,
            output_schema,
            max_rounds: if is_think { 3 } else { 0 },
            budget_admission: self.governor.is_active(),
            idempotency_prefix: format!("{}/{run_id}/{step_id}", self.instance),
            tool_meta: Some(
                json!({"agent/run": run_id, "agent/step": step_id, "agent/instance": self.instance}),
            ),
            temperature: None,
            max_tokens_per_call: 0,
            turn_id: format!(
                "{run_id}/{step_id}#{}",
                self.runs
                    .get(run_id)
                    .and_then(|r| r.step(step_id))
                    .map(|s| s.attempt)
                    .unwrap_or(1)
            ),
        };
        let launch = super::turns::TurnLaunch {
            spec,
            kind: ChildKind::StepTurn {
                run: run_id.to_string(),
                step: step_id.to_string(),
                reservation,
            },
            servers,
            max_steps,
            max_tokens,
            deadline_ms,
            agent_path: format!("run/{run_id}/{step_id}"),
        };
        match self.spawn_turn(launch) {
            Ok(node) => {
                if let Some(st) = self
                    .runs
                    .get_mut(run_id)
                    .and_then(|r| r.steps.get_mut(step_id))
                {
                    st.worker = Some(node.0.to_string());
                }
                self.log.info(
                    "step.turn.spawn",
                    json!({"run": run_id, "step": step_id, "node": node.0}),
                );
            }
            Err(e) => {
                if let Some(r) = reservation {
                    self.governor.release(r);
                }
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some(format!("spawn: {e}")),
                    0,
                );
            }
        }
    }

    /// The governor scopes a run's turns are charged to (workflow budget → run scope).
    fn run_scopes(&mut self, run_id: &str) -> Vec<String> {
        let Some(wf) = self.definition_for_run(run_id) else {
            return Vec::new();
        };
        match wf
            .limits
            .budget
            .as_ref()
            .and_then(|b| serde_json::from_value::<crate::config::v2::Budget>(b.clone()).ok())
        {
            Some(b) => {
                let key = format!("run:{run_id}");
                self.governor.ensure_scope(&key, &b);
                vec![key]
            }
            None => Vec::new(),
        }
    }

    // ---- outcomes --------------------------------------------------------------

    /// An executor / deferred tool finished a step.
    pub(crate) fn on_step_done(
        &mut self,
        run_id: &str,
        step_id: &str,
        output: Value,
        is_error: bool,
        error: Option<String>,
        tokens: u64,
    ) {
        self.executing.remove(&format!("{run_id}/{step_id}"));
        self.finish_step(
            run_id,
            step_id,
            if is_error {
                StepStatus::Failed
            } else {
                StepStatus::Done
            },
            Some(output),
            error,
            tokens,
        );
    }

    /// A step's turn worker finished.
    pub(crate) fn on_step_turn_done(&mut self, run_id: &str, step_id: &str, turn: TurnResult) {
        let tokens = turn.usage.total();
        if turn.status == "completed" {
            let output = turn
                .value
                .clone()
                .or_else(|| turn.finish.as_ref().and_then(|f| f.get("output").cloned()))
                .or_else(|| turn.text.clone().map(Value::String))
                .unwrap_or(Value::Null);
            // A `finish {status: failed|refused}` from an agent step fails the step.
            let failed = turn
                .finish
                .as_ref()
                .and_then(|f| f.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| s != "completed");
            if failed {
                let reason = turn
                    .finish
                    .as_ref()
                    .and_then(|f| f.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("agent finished with a non-completed status")
                    .to_string();
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    Some(output),
                    Some(reason),
                    tokens,
                );
            } else {
                self.finish_step(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    Some(output),
                    None,
                    tokens,
                );
            }
        } else {
            let status = if turn.status == "deadline" {
                StepStatus::Timeout
            } else {
                StepStatus::Failed
            };
            self.finish_step(
                run_id,
                step_id,
                status,
                turn.value
                    .clone()
                    .or_else(|| turn.text.clone().map(Value::String)),
                Some(format!(
                    "turn {}{}",
                    turn.status,
                    turn.error
                        .as_deref()
                        .map(|e| format!(": {e}"))
                        .unwrap_or_default()
                )),
                tokens,
            );
        }
    }

    /// A step timer fired (`sleep` done, or a budget window opened).
    pub(crate) fn on_step_timer(
        &mut self,
        run_id: &str,
        step_id: &str,
        budget: bool,
        payload: &Value,
    ) {
        if budget {
            if let Some(st) = self
                .runs
                .get_mut(run_id)
                .and_then(|r| r.steps.get_mut(step_id))
            {
                st.status = StepStatus::Pending;
                st.wait = None;
            }
            if let Some(r) = self.runs.get_mut(run_id) {
                r.touch();
            }
            return;
        }
        self.finish_step(
            run_id,
            step_id,
            StepStatus::Done,
            Some(payload.clone()),
            None,
            0,
        );
    }

    /// Record a step's terminal outcome; retry / route failures; checkpoint.
    pub(crate) fn finish_step_pub(
        &mut self,
        run_id: &str,
        step_id: &str,
        status: StepStatus,
        output: Option<Value>,
        error: Option<String>,
        tokens: u64,
    ) {
        self.finish_step(run_id, step_id, status, output, error, tokens)
    }

    fn finish_step(
        &mut self,
        run_id: &str,
        step_id: &str,
        status: StepStatus,
        output: Option<Value>,
        error: Option<String>,
        tokens: u64,
    ) {
        let Some(wf) = self
            .runs
            .get(run_id)
            .and_then(|_| self.definition_for_run(run_id))
        else {
            return;
        };
        let Some((step, scope)) = self
            .runs
            .get(run_id)
            .and_then(|r| self.resolve_step(&wf, r, step_id))
        else {
            return;
        };
        {
            let run = self.runs.get_mut(run_id).expect("present");
            if run.status.is_terminal() {
                return; // a late result for a finished run
            }
            run.tokens += tokens;
        }
        crate::obs::metrics::record_step(match status {
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
            _ => "other",
        });
        // Large outputs become artifacts (`limits.inline_max_bytes`, RFC 0027 §3).
        let output = match output {
            Some(v) if !v.is_null() => {
                let cap = self.settings.limits.inline_max_bytes.unwrap_or(65_536) as usize;
                if v.to_string().len() > cap {
                    match self.artifacts.create(
                        &self.durable,
                        super::artifacts::NewArtifact {
                            name: &format!("{run_id}/{step_id}/output.json"),
                            mime: Some("application/json"),
                            content: v.clone(),
                            created_by: Some("engine"),
                            sensitive: false,
                            owner: Some(run_id),
                        },
                    ) {
                        Ok(meta) => {
                            self.log.info("step.output.artifact", json!({"run": run_id, "step": step_id, "artifact": meta["id"], "size": meta["size"]}));
                            Some(json!({"$artifact": meta["id"], "size": meta["size"]}))
                        }
                        Err(e) => {
                            self.log.warn(
                                "step.output.artifact_fail",
                                json!({"run": run_id, "step": step_id, "err": e}),
                            );
                            Some(v)
                        }
                    }
                } else {
                    Some(v)
                }
            }
            other => other,
        };
        // Output schema (RFC 0027 §6 structured I/O).
        let (status, error) = match (&status, &step.output_schema, &output) {
            (StepStatus::Done, Some(schema), Some(out)) => {
                match crate::jsonschema::validate(schema, out) {
                    Ok(()) => (status, error),
                    Err(e) => (
                        StepStatus::Failed,
                        Some(format!(
                            "output does not match output_schema: {}",
                            crate::jsonschema::explain(&e)
                        )),
                    ),
                }
            }
            _ => (status, error),
        };
        let attempt = self
            .runs
            .get(run_id)
            .and_then(|r| r.step(step_id))
            .map(|s| s.attempt)
            .unwrap_or(1);
        self.log.info("step.done", json!({"run": run_id, "step": step_id, "status": status, "attempt": attempt, "tokens": tokens, "err": error}));
        self.feed_push(
            "step",
            crate::runtime::a2a_server::FeedVis::Operator,
            json!({"run": run_id, "step": step_id, "phase": "done",
                   "status": crate::runtime::nested::StatusLabel::as_label(&status), "attempt": attempt, "tokens": tokens,
                   // Same 2 KiB cap the run drill-down applies: a step output can
                   // be an entire document, and a feed is not the place for it.
                   "err": error.as_deref().map(|e| {
                       e.chars().take(2048).collect::<String>()
                   })}),
        );
        if matches!(status, StepStatus::Failed | StepStatus::Timeout) {
            // Retry?
            if let Some(retry) = &step.retry
                && attempt <= retry.max
            {
                // Exponential, with jitter. Without it every step that failed
                // in the same wave — the usual case, since they usually failed
                // for the same upstream reason — retries in lockstep and
                // rebuilds the thundering herd the backoff exists to break up.
                // Deterministic per (run, step, attempt): no RNG, so a replay
                // reproduces the same schedule.
                let base = retry
                    .backoff_ms
                    .saturating_mul(1u64 << (attempt.saturating_sub(1)).min(10));
                let backoff = if base == 0 {
                    0
                } else {
                    let mut h: u64 = 1469598103934665603;
                    for b in run_id.bytes().chain(step_id.bytes()).chain([attempt as u8]) {
                        h ^= b as u64;
                        h = h.wrapping_mul(1099511628211);
                    }
                    // ±20% around the base.
                    let spread = (base / 5).max(1);
                    base.saturating_sub(spread) + (h % (spread * 2 + 1))
                };
                self.log.info("step.retry", json!({"run": run_id, "step": step_id, "attempt": attempt, "backoff_ms": backoff}));
                if backoff == 0 {
                    if let Some(st) = self
                        .runs
                        .get_mut(run_id)
                        .and_then(|r| r.steps.get_mut(step_id))
                    {
                        st.status = StepStatus::Pending;
                        st.error = error;
                    }
                } else {
                    match self.timers.arm(
                        &self.durable,
                        now_ms() + backoff,
                        json!({"kind": "step_budget", "run": run_id, "step": step_id}),
                        Value::Null,
                    ) {
                        Ok(id) => {
                            self.runs.get_mut(run_id).expect("present").suspend_step(
                                step_id,
                                json!({"kind": "retry_backoff", "timer": id, "error": error}),
                            );
                        }
                        Err(_) => {
                            if let Some(st) = self
                                .runs
                                .get_mut(run_id)
                                .and_then(|r| r.steps.get_mut(step_id))
                            {
                                st.status = StepStatus::Pending;
                            }
                        }
                    }
                }
                self.checkpoint(false);
                return;
            }
            let err_text = error.clone().unwrap_or_else(|| "failed".into());
            self.runs
                .get_mut(run_id)
                .expect("present")
                .end_step(step_id, status, output, error);
            if let Some(sc) = &scope {
                // Inside a body: `continue` marks done-with-error, `goto` re-arms
                // a sibling, `fail` leaves the step failed for the parent to judge.
                match &step.on_error {
                    OnError::Continue => {
                        if let Some(st) = self
                            .runs
                            .get_mut(run_id)
                            .and_then(|r| r.steps.get_mut(step_id))
                        {
                            st.status = StepStatus::Done;
                            st.error = Some(err_text.clone());
                            if st.output.is_none() {
                                st.output = Some(json!({"error": err_text}));
                            }
                        }
                    }
                    OnError::Goto(t) => {
                        let sid = super::nested::scoped_id(&sc.parent, t);
                        if let Some(st) = self
                            .runs
                            .get_mut(run_id)
                            .and_then(|r| r.steps.get_mut(&sid))
                        {
                            st.status = StepStatus::Pending;
                            st.forced = true;
                        }
                    }
                    OnError::Fail => {}
                }
                crate::state::kill_point("step.before_done");
                self.checkpoint(false);
                self.on_scoped_step_done(run_id, step_id);
                return;
            }
            let routed = run::route_failure(
                &wf,
                self.runs.get_mut(run_id).expect("present"),
                &step,
                &err_text,
            );
            match routed {
                Ok(next) => {
                    if !next.is_empty() {
                        self.log.info(
                            "step.goto",
                            json!({"run": run_id, "from": step_id, "to": next}),
                        );
                    }
                }
                Err(reason) => {
                    self.cancel_children_of_run(run_id, "run failed");
                    self.runs.get_mut(run_id).expect("present").finish(
                        RunStatus::Failed,
                        None,
                        Some(reason),
                    );
                    self.on_run_terminal(run_id);
                    return;
                }
            }
            if let OnError::Continue = step.on_error {
                // Already marked done-with-error by route_failure.
            }
        } else {
            // Memoize (`cache`).
            if step.cache.is_some()
                && status == StepStatus::Done
                && let Some(key) = self
                    .runs
                    .get(run_id)
                    .and_then(|r| r.steps.get(step_id))
                    .and_then(|st| st.wait.as_ref())
                    .and_then(|w| w.get("cache_key"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                && let Some(out) = &output
            {
                self.cache_store(&key, out);
            }
            self.runs
                .get_mut(run_id)
                .expect("present")
                .end_step(step_id, status, output, error);
        }
        crate::state::kill_point("step.before_done");
        self.checkpoint(false);
        if scope.is_some() {
            self.on_scoped_step_done(run_id, step_id);
        }
    }

    /// The run reached a terminal state: report, wake, plan bindings, counters.
    /// Evict terminal runs beyond `store.retention.runs`.
    ///
    /// Without this a long-lived instance keeps one durable record per run
    /// forever — on a laptop, the difference between an agent that runs for a
    /// month and one that fills a disk. Only TERMINAL runs are candidates:
    /// nothing in flight is ever dropped, whatever the policy says. Default is
    /// unbounded, so an operator who has not thought about it keeps today's
    /// behaviour.
    fn evict_terminal_runs(&mut self) {
        let policy = &self.settings.store.retention.runs;
        let keep_last = policy.keep_last;
        let ttl_ms = policy.ttl.as_ref().map(|d| d.0.as_millis() as u64);
        if keep_last.is_none() && ttl_ms.is_none() {
            return;
        }
        let now = now_ms();
        // Newest first, so "keep the last N" is a prefix.
        let mut terminal: Vec<(String, u64)> = self
            .runs
            .values()
            .filter(|r| r.status.is_terminal())
            .map(|r| (r.id.clone(), r.finished.unwrap_or(0)))
            .collect();
        terminal.sort_by_key(|(_, finished)| std::cmp::Reverse(*finished));

        let mut drop: Vec<String> = Vec::new();
        for (i, (id, finished)) in terminal.iter().enumerate() {
            let over_count = keep_last.is_some_and(|k| i >= k as usize);
            let over_age = ttl_ms.is_some_and(|t| now.saturating_sub(*finished) > t);
            if over_count || over_age {
                drop.push(id.clone());
            }
        }
        for id in drop {
            self.runs.remove(&id);
            if let Err(e) = self.durable.delete(crate::state::Kind::Run, &id) {
                self.log
                    .warn("run.evict.fail", json!({"run": id, "err": e.to_string()}));
                continue;
            }
            self.log.info("run.evicted", json!({"run": id}));
        }
    }

    pub(crate) fn on_run_terminal(&mut self, run_id: &str) {
        let Some(run) = self.runs.get(run_id) else {
            return;
        };
        let (status, output, error, workflow) = (
            run.status,
            run.output.clone(),
            run.error.clone(),
            run.workflow.clone(),
        );
        #[cfg(feature = "a2a")]
        let a2a_task = run.task.clone();
        // Eviction runs here because this is the only moment the candidate set
        // grows. Deferred to the end of the function so the run's own
        // completion handling (webhook reply, A2A task, feed) happens first —
        // evicting a record before its result was delivered would be a fine way
        // to lose an answer.
        let evict_after = true;
        // A `respond: sync` webhook awaiting this run gets its result now.
        #[cfg(feature = "a2a")]
        self.webhook_sync_reply(run_id);
        // A queued child run (`child_run` wait) resolves its parent step.
        if let Some(parent) = self.runs.get(run_id).and_then(|r| r.parent.clone())
            && let (Some(pr), Some(ps)) = (
                parent["run"].as_str().map(str::to_string),
                parent["step"].as_str().map(str::to_string),
            )
            && self
                .runs
                .get(&pr)
                .and_then(|r| r.steps.get(&ps))
                .is_some_and(|st| {
                    st.status == StepStatus::Suspended
                        && st.wait.as_ref().is_some_and(|w| w["kind"] == "child_run")
                })
        {
            self.finish_step_pub(
                &pr,
                &ps,
                if status == RunStatus::Completed {
                    StepStatus::Done
                } else {
                    StepStatus::Failed
                },
                Some(json!({"run": run_id, "status": status, "output": output, "error": error})),
                (status != RunStatus::Completed).then(|| {
                    error
                        .clone()
                        .unwrap_or_else(|| format!("child run {}", status.as_str()))
                }),
                0,
            );
        }
        self.counters.runs_finished += 1;
        crate::obs::metrics::record_run(match status {
            RunStatus::Completed => crate::obs::metrics::RunOutcome::Completed,
            RunStatus::Cancelled => crate::obs::metrics::RunOutcome::Killed,
            _ => crate::obs::metrics::RunOutcome::Failed,
        });
        crate::obs::metrics::record_run_status(status.as_str());
        self.log.info("run.done", json!({"run": run_id, "workflow": workflow, "status": status, "err": error, "output": if self.log.content_capture() { output.clone().unwrap_or(Value::Null) } else { Value::Null }}));
        self.governor.drop_scope(&format!("run:{run_id}"));
        // Answer waiters (workflow.wait / run sync).
        let waiting: Vec<Target> = self
            .pending
            .iter()
            .filter(|p| matches!(&p.kind, PendingKind::Run { run, .. } if run == run_id))
            .map(|p| p.target.clone())
            .collect();
        self.pending
            .retain(|p| !matches!(&p.kind, PendingKind::Run { run, .. } if run == run_id));
        for t in waiting {
            self.reply(
                &t,
                json!({"run": run_id, "status": status, "output": output, "error": error}),
                false,
            );
        }
        // Plan bindings + the root note (wake policy).
        let ok = status == RunStatus::Completed;
        let note = error
            .clone()
            .or_else(|| output.as_ref().map(|o| o.to_string()))
            .unwrap_or_default();
        self.settle_plan_bindings(
            &crate::context::plan::Binding::Run {
                id: run_id.to_string(),
            },
            ok,
            &note,
        );
        let wake = self.settings.agent.wake_on();
        let notify = match self.settings.agent.on_workflow_finished {
            crate::config::v2::OnWorkflowFinished::Ignore => false,
            _ => {
                ok && wake.contains(&crate::config::v2::WakeEvent::WorkflowFinished)
                    || !ok && wake.contains(&crate::config::v2::WakeEvent::WorkflowFailed)
            }
        };
        if notify && !self.job_shape {
            let short = if note.chars().count() > 400 {
                format!("{}…", note.chars().take(400).collect::<String>())
            } else {
                note.clone()
            };
            self.note_root(format!(
                "workflow {workflow} run {run_id} {}: {short}",
                status.as_str()
            ));
        }
        // A `loop` start re-arms the next iteration; `event` start nodes fire on
        // workflow.finished/failed.
        if let Some((wf, node, spec, kind)) = self.run_start_spec(run_id)
            && kind == "loop"
        {
            self.on_loop_run_finished(
                &wf,
                &node,
                &spec,
                ok,
                &output.clone().unwrap_or(Value::Null),
            );
        }
        if let Some(ev) = super::starts::run_event(status) {
            self.fire_event_starts(
                ev,
                &json!({"run": run_id, "workflow": workflow, "status": status.as_str()}),
            );
        }
        // A run started over A2A drives its task to the run's outcome.
        #[cfg(feature = "a2a")]
        if let Some(tid) = &a2a_task {
            self.a2a_task_for_run(tid, status.as_str(), output.as_ref(), error.as_deref());
        }
        self.checkpoint(false);
        if evict_after {
            self.evict_terminal_runs();
        }
    }

    /// Cancel a run: cancel its children, fail suspended waits, mark cancelled.
    pub(crate) fn cancel_run(&mut self, run_id: &str, reason: &str) {
        // Cascade to child runs started with `cascade: true` (RFC 0027 §6).
        let kids: Vec<String> = self
            .runs
            .values()
            .filter(|r| {
                !r.status.is_terminal()
                    && r.parent.as_ref().is_some_and(|p| {
                        p["run"].as_str() == Some(run_id) && p["cascade"].as_bool().unwrap_or(true)
                    })
            })
            .map(|r| r.id.clone())
            .collect();
        for k in kids {
            self.cancel_run(&k, "parent run cancelled");
        }
        self.cancel_children_of_run(run_id, reason);
        let timers = self.timers.owned_by(|o| o["run"].as_str() == Some(run_id));
        for t in timers {
            let _ = self.timers.disarm(&self.durable, &t);
        }
        self.pending
            .retain(|p| !matches!(&p.target, Target::Step(r, _) if r == run_id));
        if let Some(r) = self.runs.get_mut(run_id)
            && !r.status.is_terminal()
        {
            r.finish(RunStatus::Cancelled, None, Some(reason.to_string()));
            self.on_run_terminal(run_id);
        }
    }

    fn cancel_children_of_run(&mut self, run_id: &str, reason: &str) {
        let nodes: Vec<_> = self
            .children
            .iter()
            .filter(|(_, c)| matches!(&c.kind, ChildKind::StepTurn { run, .. } if run == run_id))
            .map(|(n, _)| *n)
            .collect();
        for n in nodes {
            self.children.cancel(n, reason);
        }
    }

    // ---- workflow.* tools ------------------------------------------------------

    pub(crate) fn workflow_tool(
        &mut self,
        caller: &ToolCaller,
        name: &str,
        args: Value,
    ) -> ToolOutcome {
        let err = |e: String| ToolOutcome::Ready(Value::String(e), true);
        match name {
            "workflow.run" => {
                let wname = args["name"].as_str().unwrap_or("").to_string();
                let Some(w) = self.workflows.get(&wname) else {
                    return err(format!("no such workflow {wname:?}"));
                };
                let start = match args.get("start").and_then(Value::as_str) {
                    Some(s) => match w.step(s) {
                        Some(st) if st.is_start() => s.to_string(),
                        _ => return err(format!("workflow {wname:?} has no start node {s:?}")),
                    },
                    None => match default_start(w) {
                        Some(s) => s,
                        None => return err(format!("workflow {wname:?} has no start node")),
                    },
                };
                let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
                let timeout_ms = args
                    .get("timeout")
                    .and_then(Value::as_str)
                    .and_then(|t| crate::config::parse_duration(t).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(3_600_000);
                let request = match (caller.node, &caller.run, &caller.step) {
                    (Some(n), _, _) => {
                        json!({"node": n.0, "req": caller.req, "wait": wait, "timeout_ms": timeout_ms})
                    }
                    (None, Some(r), Some(s)) => {
                        json!({"run": r, "step": s, "wait": wait, "timeout_ms": timeout_ms})
                    }
                    _ => Value::Null,
                };
                let payload = json!({"workflow": wname, "node": start, "payload": {"requested_by": caller.label_pub()}, "inputs": args.get("inputs").cloned().unwrap_or(json!({})), "request": request, "conversation": caller.ctx});
                match self.accept_event(kinds::WORKFLOW_RUN, caller.principal.clone(), payload) {
                    Ok(_) => {
                        // Process it right away so the caller learns the run id.
                        if let Some(ev) = self.inbox_queue.pop_back() {
                            let done = self.on_start_event(&ev);
                            if done {
                                self.inbox_done(&ev.id);
                            }
                        }
                        // The reply is (or will be) delivered through the request
                        // target: immediately with the run id, or when the run
                        // finishes for `wait: true` (registered by `on_start_event`).
                        ToolOutcome::Executing
                    }
                    Err(e) => err(e),
                }
            }
            "workflow.list" => ToolOutcome::Ready(
                json!({"workflows": self.workflows.values().map(|w| json!({
                    "name": w.name, "description": w.description, "armed": w.armed, "hash": w.hash,
                    "starts": w.start_steps().iter().map(|s| json!({"node": s.id, "kind": s.kind})).collect::<Vec<_>>(),
                    "runs": self.runs.values().filter(|r| r.workflow == w.name).map(|r| json!({"id": r.id, "status": r.status})).collect::<Vec<_>>(),
                })).collect::<Vec<_>>()}),
                false,
            ),
            "workflow.status" => {
                let runs: Vec<Value> = match (
                    args.get("run").and_then(Value::as_str),
                    args.get("name").and_then(Value::as_str),
                ) {
                    (Some(id), _) => self
                        .runs
                        .get(id)
                        .map(|r| vec![run_detail(r)])
                        .unwrap_or_default(),
                    (None, Some(n)) => self
                        .runs
                        .values()
                        .filter(|r| r.workflow == n)
                        .map(RunState::summary)
                        .collect(),
                    _ => self.runs.values().map(RunState::summary).collect(),
                };
                ToolOutcome::Ready(json!({"runs": runs}), false)
            }
            "workflow.cancel" => {
                let id = args["run"].as_str().unwrap_or("").to_string();
                if !self.runs.contains_key(&id) {
                    return err(format!("no such run {id:?}"));
                }
                let reason = args
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("cancelled by request")
                    .to_string();
                self.cancel_run(&id, &reason);
                ToolOutcome::Ready(
                    json!({"ok": true, "status": self.runs.get(&id).map(|r| r.status.as_str()).unwrap_or("cancelled")}),
                    false,
                )
            }
            "workflow.wait" => {
                let id = args["run"].as_str().unwrap_or("").to_string();
                let timeout_ms = args
                    .get("timeout")
                    .and_then(Value::as_str)
                    .and_then(|t| crate::config::parse_duration(t).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(3_600_000);
                match self.runs.get(&id) {
                    None => err(format!("no such run {id:?}")),
                    Some(r) if r.status.is_terminal() => ToolOutcome::Ready(
                        json!({"run": id, "status": r.status, "output": r.output, "error": r.error}),
                        false,
                    ),
                    Some(_) => ToolOutcome::Deferred(PendingKind::Run {
                        run: id,
                        deadline_ms: now_ms() + timeout_ms,
                    }),
                }
            }
            "workflow.pause" | "workflow.resume" => {
                let pause = name == "workflow.pause";
                // `before_step`: pause the run the moment a named step is about
                // to start, rather than immediately. This is a breakpoint —
                // "stop when you reach `notify`" — which is what you actually
                // want when debugging a graph, and it needs no new surface
                // because pause already exists and already survives a restart.
                if pause
                    && let Some(id) = args.get("run").and_then(Value::as_str)
                    && let Some(step) = args.get("before_step").and_then(Value::as_str)
                {
                    let known = self
                        .definition_for_run(id)
                        .is_some_and(|wf| wf.steps.contains_key(step));
                    if !known {
                        return err(format!(
                            "before_step {step:?} is not a step of this run's workflow"
                        ));
                    }
                    match self.runs.get_mut(id) {
                        None => return err(format!("no such run {id:?}")),
                        Some(r) => {
                            r.break_before = Some(step.to_string());
                            r.dirty = true;
                        }
                    }
                    self.log
                        .info("run.breakpoint", json!({"run": id, "before_step": step}));
                    return ToolOutcome::Ready(json!({"run": id, "break_before": step}), false);
                }
                if let Some(id) = args.get("run").and_then(Value::as_str) {
                    match self.runs.get_mut(id) {
                        None => return err(format!("no such run {id:?}")),
                        Some(r) if r.status.is_terminal() => {
                            return err(format!("run {id:?} is already {}", r.status.as_str()));
                        }
                        Some(r) => {
                            r.status = if pause {
                                RunStatus::Paused
                            } else {
                                RunStatus::Running
                            };
                            r.touch();
                        }
                    }
                    return ToolOutcome::Ready(json!({"ok": true}), false);
                }
                if let Some(n) = args.get("name").and_then(Value::as_str) {
                    match self.workflows.get_mut(n) {
                        None => return err(format!("no such workflow {n:?}")),
                        Some(w) => w.armed = !pause,
                    }
                    if !pause {
                        self.arm_workflows();
                    }
                    return ToolOutcome::Ready(json!({"ok": true}), false);
                }
                err(format!("{name}: give run or name"))
            }
            "workflow.create" | "workflow.update" => {
                let def = args["definition"].clone();
                match parse_workflow(&def) {
                    Err(e) => err(format!("{name}: {}", e.join("; "))),
                    Ok(w) => {
                        if name == "workflow.create" && self.workflows.contains_key(&w.name) {
                            return err(format!(
                                "workflow {:?} exists (use workflow.update)",
                                w.name
                            ));
                        }
                        if name == "workflow.update" && !self.workflows.contains_key(&w.name) {
                            return err(format!(
                                "workflow {:?} does not exist (use workflow.create)",
                                w.name
                            ));
                        }
                        let (wname, hash) = (w.name.clone(), w.hash.clone());
                        // Durable definition (memory/_workflows/<name>).
                        let rec = crate::context::memory::Record {
                            value: def,
                            ts: now_ms(),
                            ttl_ms: None,
                            by: Some(caller.label_pub()),
                        };
                        if let Err(e) = self.durable.put(
                            Kind::Memory,
                            &format!("{WORKFLOW_DEF_PREFIX}{wname}"),
                            serde_json::to_value(&rec).unwrap_or(Value::Null),
                            None,
                        ) {
                            return err(format!("{name}: store: {e}"));
                        }
                        let arm = args.get("arm").and_then(Value::as_bool).unwrap_or(true);
                        let mut w = w;
                        w.armed = arm;
                        self.workflows.insert(wname.clone(), w);
                        self.log.info(
                            "workflow.defined",
                            json!({"name": wname, "hash": &hash[..12], "op": name}),
                        );
                        if arm {
                            self.arm_workflows();
                        }
                        ToolOutcome::Ready(
                            json!({"name": wname, "hash": hash, "armed": arm}),
                            false,
                        )
                    }
                }
            }
            "workflow.delete" => {
                let wname = args["name"].as_str().unwrap_or("").to_string();
                if self.workflows.remove(&wname).is_none() {
                    return err(format!("no such workflow {wname:?}"));
                }
                let _ = self
                    .durable
                    .delete(Kind::Memory, &format!("{WORKFLOW_DEF_PREFIX}{wname}"));
                self.log.info("workflow.deleted", json!({"name": wname}));
                ToolOutcome::Ready(json!({"ok": true}), false)
            }
            "workflow.signal" => {
                let sname = args["name"].as_str().unwrap_or("").to_string();
                let _ = self.accept_event(kinds::SIGNAL, caller.principal.clone(), json!({"name": sname, "payload": args.get("payload").cloned().unwrap_or(Value::Null), "run": args.get("run"), "from": caller.label_pub()}));
                // Signal start nodes / waits land with P4; the signal is recorded.
                ToolOutcome::Ready(
                    json!({"delivered": 0, "note": "signal recorded; signal start nodes and waits land with the P4 engine"}),
                    false,
                )
            }
            _ => err(format!("unknown workflow tool {name}")),
        }
    }
}

impl ToolCaller {
    pub(crate) fn label_pub(&self) -> String {
        if let Some(s) = &self.subagent {
            return format!("subagent:{s}");
        }
        if let (Some(r), Some(s)) = (&self.run, &self.step) {
            return format!("step:{r}/{s}");
        }
        format!(
            "ctx:{}",
            self.ctx.as_deref().unwrap_or(crate::context::ROOT)
        )
    }
}

/// The start node `workflow.run` uses by default: `manual`, else the first.
fn default_start(w: &Workflow) -> Option<String> {
    let starts = w.start_steps();
    starts
        .iter()
        .find(|s| s.kind == "manual")
        .or_else(|| starts.first())
        .map(|s| s.id.clone())
}

fn node_kind<'a>(w: &'a Workflow, node: &str) -> Option<&'a str> {
    w.step(node).map(|s| s.kind.as_str())
}

fn run_detail(r: &RunState) -> Value {
    let mut v = r.summary();
    v["step_states"] = json!(r.steps);
    v["vars"] = Value::Object(r.vars.clone());
    v
}

/// The `memory.<key>` roots a value's templates reference.
fn collect_memory_keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => {
            let mut rest = s.as_str();
            while let Some(i) = rest.find("memory.") {
                let after = &rest[i + "memory.".len()..];
                let key: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | ':'))
                    .collect();
                if !key.is_empty() && !out.contains(&key) {
                    out.push(key.clone());
                }
                rest = &after[key.len().min(after.len())..];
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_memory_keys(x, out)),
        Value::Object(o) => o.values().for_each(|x| collect_memory_keys(x, out)),
        _ => {}
    }
}
