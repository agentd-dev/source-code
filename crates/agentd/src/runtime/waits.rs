// SPDX-License-Identifier: AGPL-3.0-only
//! **Orchestration steps** — the integration, intelligence and control kinds:
//! `wait` (on a resource update, a CEL condition, a signal, a run, a subagent,
//! a conversation message, or a deadline), `join` (fan-in of runs/subagents),
//! `workflow` (a child run: `sync | async | detached`, `cascade`),
//! `workflow.signal` / `workflow.wait` / `workflow.cancel`, `subagent`,
//! `human` (through the `ask_human` contract), `mcp.resource`
//! (`read | list | prompt | complete`), `a2a.delegate` (the outbound A2A
//! client),
//! the `think` presets (`classify | extract | summarize | judge | route`), and
//! step `cache` (memoized outputs by input hash). Waits suspend the step
//! durably (`StepState.wait`) and are resolved by the loop's tick.

use super::events::kinds;
use super::reactor::{PendingKind, Runtime, Target};
use super::tools::{ToolCaller, ToolOutcome};
use crate::context::ROOT;
use crate::engine::model::Step;
use crate::engine::run::StepStatus;
use crate::engine::template::Data;
use crate::state::{Kind, now_ms};
use serde_json::{Map, Value, json};
#[cfg(feature = "a2a")]
use std::time::Duration;

/// A durable wait record kept in `StepState.wait`.
/// The message id an `idempotency:` declaration asks for — the step's derived
/// key, or the declared `value` (an application-level identity, which is
/// stronger: it also collides two different RUNS attempting the same
/// real-world operation). `None` when the step declared nothing, keeping
/// today's unique-per-send minting: a `goto` re-entry is a LOGICALLY new send,
/// and silently deduping it on a peer would lose notifications — so retry
///-safety here is opt-in, per node, like everywhere else.
#[cfg(feature = "a2a")]
fn idempotency_message_id(
    spec: &Map<String, Value>,
    run_id: &str,
    step_id: &str,
) -> Option<String> {
    let idem = spec.get("idempotency")?;
    if idem.as_bool() == Some(false) {
        return None;
    }
    Some(
        idem.get("value")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| crate::engine::run::idempotency_key(run_id, step_id)),
    )
}

pub(crate) fn wait_record(kind: &str, extra: Value, timeout_ms: Option<u64>) -> Value {
    let mut w = json!({"kind": kind, "since_ms": now_ms()});
    if let Some(t) = timeout_ms {
        w["deadline_ms"] = json!(now_ms() + t);
    }
    if let Value::Object(o) = extra {
        for (k, v) in o {
            w[k] = v;
        }
    }
    w
}

/// How many events one stream's waiters advance through per tick. Bounds the
/// single-writer loop's time on a busy stream; the anchor persists, so a
/// backlog is worked off across ticks rather than dropped.
const EVENT_SCAN: usize = 256;

/// The durable key of one event — mirrors the stream module's layout.
fn event_key(stream: &str, seq: u64) -> String {
    format!("{stream}/e{seq:020}")
}

impl Runtime {
    /// Execute one of the orchestration kinds (called from `execute_step`).
    pub(crate) fn execute_orchestration_step(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &Data,
        caller: &ToolCaller,
    ) {
        match step.kind.as_str() {
            "wait" => self.step_wait(run_id, step_id, step, spec, data),
            "join" => {
                let handles: Vec<String> = spec
                    .get("handles")
                    .map(|h| match h {
                        Value::Array(a) => a
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                        Value::String(s) => vec![s.clone()],
                        _ => Vec::new(),
                    })
                    .unwrap_or_default();
                let timeout = spec
                    .get("timeout")
                    .and_then(crate::engine::model::duration_ms_opt);
                let min = spec.get("min").and_then(Value::as_u64);
                self.suspend_wait(run_id, step_id, wait_record("join", json!({"handles": handles, "min": min, "partials": spec.get("partials").and_then(Value::as_bool).unwrap_or(false)}), timeout));
            }
            "workflow" => self.step_child_workflow(run_id, step_id, spec, caller),
            "message" => self.step_message(run_id, step_id, spec),
            "workflow.signal" => {
                let name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let payload = spec.get("payload").cloned().unwrap_or(Value::Null);
                let target_run = spec.get("run").and_then(Value::as_str).map(str::to_string);
                let delivered =
                    self.deliver_signal(&name, payload, target_run.as_deref(), Some(run_id));
                self.finish_step_pub(
                    run_id,
                    step_id,
                    StepStatus::Done,
                    Some(json!({"delivered": delivered})),
                    None,
                    0,
                );
            }
            "workflow.wait" => {
                let target = spec
                    .get("run")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let timeout = spec
                    .get("timeout")
                    .and_then(crate::engine::model::duration_ms_opt);
                self.suspend_wait(
                    run_id,
                    step_id,
                    wait_record("run", json!({"run": target}), timeout),
                );
            }
            "workflow.cancel" => {
                let target = spec
                    .get("run")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let reason = spec
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("cancelled by workflow.cancel")
                    .to_string();
                if self.runs.contains_key(&target) {
                    self.cancel_run(&target, &reason);
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Done,
                        Some(json!({"ok": true, "run": target})),
                        None,
                        0,
                    );
                } else {
                    self.finish_step_pub(
                        run_id,
                        step_id,
                        StepStatus::Failed,
                        None,
                        Some(format!("no such run {target:?}")),
                        0,
                    );
                }
            }
            "subagent" => {
                let mut args = json!({});
                for k in [
                    "instruction",
                    "template",
                    "params",
                    "mode",
                    "tools",
                    "servers",
                    "limits",
                    "priority",
                    "context",
                    "output_contract",
                    "output_schema",
                    "skills",
                    "durable",
                ] {
                    if let Some(v) = spec.get(k) {
                        args[k] = v.clone();
                    }
                }
                let mode = args["mode"].as_str().unwrap_or("sync").to_string();
                match self.subagent_tool(caller, "subagent.run", args) {
                    ToolOutcome::Ready(v, is_error) => {
                        let err = is_error.then(|| v.to_string());
                        // async/detached: the step's output is the handle record.
                        self.finish_step_pub(
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
                        let handle = match &kind {
                            PendingKind::Subagent { handle } => handle.clone(),
                            _ => String::new(),
                        };
                        self.suspend_wait(
                            run_id,
                            step_id,
                            wait_record(
                                "subagent",
                                json!({"handle": handle, "mode": mode}),
                                step.timeout_ms,
                            ),
                        );
                        self.push_pending(super::reactor::PendingTool {
                            target: Target::Step(run_id.to_string(), step_id.to_string()),
                            name: "subagent".into(),
                            kind,
                            started_ms: now_ms(),
                        });
                    }
                    ToolOutcome::Executing => {}
                }
            }
            "human" => {
                let mut args =
                    json!({"question": spec.get("question").cloned().unwrap_or(Value::Null)});
                for k in ["schema", "to", "timeout"] {
                    if let Some(v) = spec.get(k) {
                        args[k] = v.clone();
                    }
                }
                match self.execute_tool(caller, "ask_human", args) {
                    ToolOutcome::Ready(v, is_error) => {
                        let err = is_error.then(|| v.to_string());
                        self.finish_step_pub(
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
                        self.suspend_wait(
                            run_id,
                            step_id,
                            wait_record("human", json!({}), step.timeout_ms),
                        );
                        self.push_pending(super::reactor::PendingTool {
                            target: Target::Step(run_id.to_string(), step_id.to_string()),
                            name: "human".into(),
                            kind,
                            started_ms: now_ms(),
                        });
                    }
                    ToolOutcome::Executing => {
                        self.executing
                            .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
                    }
                }
            }
            "mcp.resource" => self.step_mcp_resource(run_id, step_id, spec),
            #[cfg(feature = "a2a")]
            "a2a.delegate" => self.step_a2a_delegate(run_id, step_id, spec),
            #[cfg(not(feature = "a2a"))]
            "a2a.delegate" => self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some("a2a.delegate requires the 'a2a' build feature".into()),
                0,
            ),
            #[cfg(feature = "a2a")]
            "a2a.send" => self.step_a2a_send(run_id, step_id, spec),
            #[cfg(not(feature = "a2a"))]
            "a2a.send" => self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some("a2a.send requires the 'a2a' build feature".into()),
                0,
            ),
            // `a2a.wait` suspends on a CONVERSATION, exactly as `wait {on:
            // message}` does — same durable wait record, so the same arrival
            // hook resolves both and a restart resumes either. The spelling
            // exists because a workflow that sent with `a2a.send` reads better
            // awaiting with `a2a.wait` than with a generic `wait`.
            "a2a.wait" => {
                let conv = spec
                    .get("conversation")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let timeout = spec
                    .get("timeout")
                    .and_then(crate::engine::model::duration_ms_opt);
                self.suspend_wait(
                    run_id,
                    step_id,
                    wait_record("message", json!({"conversation": conv}), timeout),
                );
            }
            "classify" | "extract" | "summarize" | "judge" | "route" => {
                self.step_preset(run_id, step_id, step, spec, data)
            }
            other => self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!(
                    "step kind {other:?} is not executable in this build"
                )),
                0,
            ),
        }
    }

    /// Suspend a step with a durable wait record.
    pub(crate) fn suspend_wait(&mut self, run_id: &str, step_id: &str, wait: Value) {
        if let Some(r) = self.runs.get_mut(run_id) {
            r.suspend_step(step_id, wait);
        }
        crate::state::kill_point("wait.armed");
        self.checkpoint(false);
    }

    /// `wait {on: resource|condition|signal|run|subagent|message, …, timeout}`.
    fn step_wait(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        _data: &Data,
    ) {
        let on = spec
            .get("on")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let timeout = spec
            .get("timeout")
            .and_then(crate::engine::model::duration_ms_opt)
            .or(step.timeout_ms);
        match on.as_str() {
            "resource" => {
                let server = spec.get("server").and_then(Value::as_str).unwrap_or("").to_string();
                let uri = spec.get("uri").and_then(Value::as_str).unwrap_or("").to_string();
                // Subscribe (notify-then-read on the loop's notification poll).
                match self.mcp.get(&server).cloned() {
                    Some(c) => {
                        if let Err(e) = c.subscribe(&uri) {
                            self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(format!("wait resource: subscribe {uri}: {e}")), 0);
                            return;
                        }
                        self.suspend_wait(run_id, step_id, wait_record("resource", json!({"server": server, "uri": uri}), timeout));
                    }
                    None => self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(format!("wait resource: server {server:?} is not connected")), 0),
                }
            }
            "condition" => {
                let cond = step.field_str("condition").unwrap_or("false").to_string();
                if let Err(e) = crate::cel::compile_check(cond.trim().trim_start_matches("CEL:").trim()) {
                    self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(format!("wait condition: {e}")), 0);
                    return;
                }
                self.suspend_wait(run_id, step_id, wait_record("condition", json!({"condition": cond}), timeout));
            }
            "signal" => {
                let name = spec.get("signal").and_then(Value::as_str).unwrap_or("").to_string();
                self.suspend_wait(run_id, step_id, wait_record("signal", json!({"signal": name}), timeout));
            }
            "run" => {
                let target = spec.get("run").and_then(Value::as_str).unwrap_or("").to_string();
                self.suspend_wait(run_id, step_id, wait_record("run", json!({"run": target}), timeout));
            }
            "subagent" => {
                let handle = spec.get("subagent").and_then(Value::as_str).unwrap_or("").to_string();
                self.suspend_wait(run_id, step_id, wait_record("subagent", json!({"handle": handle}), timeout));
            }
            "message" => {
                let conv = spec.get("conversation").and_then(Value::as_str).map(str::to_string).or_else(|| self.runs.get(run_id).and_then(|r| r.conversation.clone()));
                self.suspend_wait(run_id, step_id, wait_record("message", json!({"conversation": conv}), timeout));
            }
            // Park on the durable log. This is the one edge in the system that
            // is ordered, replayable and correlated, and until now it could
            // only START a run — so every pattern past "one run per event"
            // (sagas, absence, quorum) had to be two workflows plus hand-rolled
            // bookkeeping. The reason is narrow and specific: a `stream` start's
            // filter sees only `{event}`, and there was nowhere to match an
            // event against THIS run's inputs. `match` closes that.
            "event" => {
                let Some(stream) = spec.get("stream").and_then(Value::as_str) else {
                    self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some("wait on: event requires `stream`".into()), 0);
                    return;
                };
                if !self.settings.streams.contains_key(stream) {
                    self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(format!("stream {stream:?} is not declared (add it under `streams:`)")), 0);
                    return;
                }
                // Anchored at NOW, never earlier. A wait that could resolve on
                // an event predating the run would break the at-least-once
                // contract for everything downstream of it: the step would
                // "succeed" on work that happened before anyone asked, and its
                // idempotency key would cover a different world. There is
                // deliberately no `from: earliest` here — that belongs to
                // consumers, which own a durable offset.
                let anchor = self.durable.manifest().streams.get(stream).map(|m| m.seq).unwrap_or(0);
                let rec = json!({
                    "stream": stream,
                    "subject": spec.get("subject").and_then(Value::as_str),
                    "match": spec.get("match").and_then(Value::as_str),
                    "anchor": anchor,
                });
                self.suspend_wait(run_id, step_id, wait_record("event", rec, timeout));
            }
            #[cfg(feature = "a2a")]
            "webhook" => self.webhook_wait(run_id, step_id, spec, timeout),
            "deadline" | "" if timeout.is_some() => {
                self.suspend_wait(run_id, step_id, wait_record("deadline", json!({}), timeout));
            }
            other => self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(format!("wait: on must be resource|condition|signal|run|subagent|message|event|webhook (got {other:?})")), 0),
        }
    }

    /// Every tick: resolve `wait {on: event}` steps against the durable log.
    ///
    /// **Inverted on purpose.** The obvious shape — each parked wait scanning
    /// its own stream every tick — costs (waiters × events) durable reads on
    /// the single-writer thread, for waits that may sit for days. A hundred
    /// parked sagas would be a hundred independent walks per tick of a log the
    /// consumer loop is already walking. So this groups waiters BY STREAM and
    /// reads each event exactly once per tick no matter how many runs are
    /// parked on it, the same way `poll_stream_starts` batches.
    ///
    /// Each waiter's anchor advances past what it scanned, so a quiet stream
    /// costs one manifest lookup and a busy one never rescans. The advance is
    /// an ordinary run mutation, so it rides the tick's checkpoint rather than
    /// forcing a write of its own.
    pub(crate) fn poll_event_waits(&mut self) {
        // One waiter: where it is parked, what it is looking for, and the
        // run-local data its `match` expression is allowed to see.
        struct Waiter {
            run: String,
            step: String,
            anchor: u64,
            subject: Option<String>,
            expr: Option<String>,
            inputs: Value,
            vars: Value,
        }
        let mut by_stream: std::collections::BTreeMap<String, Vec<Waiter>> =
            std::collections::BTreeMap::new();
        for (rid, run) in &self.runs {
            if run.status.is_terminal() {
                continue;
            }
            for (sid, st) in &run.steps {
                if st.status != StepStatus::Suspended {
                    continue;
                }
                let Some(w) = &st.wait else { continue };
                if w["kind"].as_str() != Some("event") {
                    continue;
                }
                let Some(stream) = w["stream"].as_str() else {
                    continue;
                };
                by_stream
                    .entry(stream.to_string())
                    .or_default()
                    .push(Waiter {
                        run: rid.clone(),
                        step: sid.clone(),
                        anchor: w["anchor"].as_u64().unwrap_or(0),
                        subject: w["subject"].as_str().map(str::to_string),
                        expr: w["match"].as_str().map(str::to_string),
                        inputs: run.inputs.clone(),
                        vars: Value::Object(run.vars.clone()),
                    });
            }
        }
        if by_stream.is_empty() {
            return;
        }
        // (run, step, event) for the ones that matched, and (run, step, seq)
        // for every waiter's new anchor.
        let mut resolved: Vec<(String, String, Value)> = Vec::new();
        let mut advanced: Vec<(String, String, u64)> = Vec::new();
        for (stream, waiters) in &by_stream {
            let head = self
                .durable
                .manifest()
                .streams
                .get(stream)
                .map(|m| m.seq)
                .unwrap_or(0);
            let first = self
                .durable
                .manifest()
                .streams
                .get(stream)
                .map(|m| m.first)
                .unwrap_or(1);
            let Some(low) = waiters.iter().map(|w| w.anchor).min() else {
                continue;
            };
            if low >= head {
                continue; // nothing new on this stream since the earliest anchor
            }
            let mut pending: Vec<&Waiter> = waiters.iter().collect();
            let mut seq = low.max(first.saturating_sub(1));
            let mut scanned = 0usize;
            while seq < head && scanned < EVENT_SCAN && !pending.is_empty() {
                seq += 1;
                scanned += 1;
                // One read, shared by every waiter on this stream.
                let Some(env) = self
                    .durable
                    .get(Kind::Event, &event_key(stream, seq))
                    .ok()
                    .flatten()
                else {
                    continue; // trimmed underneath us
                };
                let event = env.state;
                let subject = event.get("subject").and_then(Value::as_str).unwrap_or("");
                pending.retain(|w| {
                    if w.anchor >= seq {
                        return true; // this waiter armed after this event
                    }
                    if let Some(pat) = &w.subject
                        && !super::streams::subject_matches(pat, subject)
                    {
                        return true;
                    }
                    if let Some(expr) = &w.expr {
                        // The point of the whole node: the expression sees this
                        // RUN's inputs beside the event, so "the shipment for
                        // the order this run is about" is expressible.
                        let vars: Vec<(&str, &Value)> =
                            vec![("event", &event), ("inputs", &w.inputs), ("vars", &w.vars)];
                        if crate::cel::eval_bool(
                            expr.trim().trim_start_matches("CEL:").trim(),
                            &vars,
                        ) != Ok(true)
                        {
                            return true;
                        }
                    }
                    resolved.push((w.run.clone(), w.step.clone(), event.clone()));
                    false
                });
            }
            // Everything still parked has now seen the log up to `seq`.
            for w in pending {
                if seq > w.anchor {
                    advanced.push((w.run.clone(), w.step.clone(), seq));
                }
            }
        }
        for (rid, sid, seq) in advanced {
            if let Some(w) = self
                .runs
                .get_mut(&rid)
                .and_then(|r| r.steps.get_mut(&sid))
                .and_then(|s| s.wait.as_mut())
            {
                w["anchor"] = json!(seq);
            }
        }
        for (rid, sid, event) in resolved {
            self.log.info(
                "wait.resolved",
                json!({"run": rid, "step": sid, "kind": "event",
                       "stream": event.get("stream"), "subject": event.get("subject")}),
            );
            self.finish_step_pub(&rid, &sid, StepStatus::Done, Some(event), None, 0);
        }
    }

    /// Every tick: resolve suspended waits (conditions, run/subagent completion,
    /// joins, deadlines, resource updates that arrived).
    pub(crate) fn poll_waits(&mut self) {
        let now = now_ms();
        let mut resolve: Vec<(String, String, StepStatus, Value, Option<String>)> = Vec::new();
        let await_data = self.await_data_view();
        for (rid, run) in &self.runs {
            if run.status.is_terminal() {
                continue;
            }
            for (sid, st) in &run.steps {
                if st.status != StepStatus::Suspended {
                    continue;
                }
                let Some(w) = &st.wait else { continue };
                let deadline = w["deadline_ms"].as_u64();
                let timed_out = deadline.is_some_and(|d| now >= d);
                match w["kind"].as_str() {
                    Some("condition") => {
                        let cond = w["condition"].as_str().unwrap_or("false").trim().trim_start_matches("CEL:").trim().to_string();
                        let vars: Vec<(&str, &Value)> = await_data.iter().map(|(k, v)| (k.as_str(), v)).collect();
                        match crate::cel::eval_bool(&cond, &vars) {
                            Ok(true) => resolve.push((rid.clone(), sid.clone(), StepStatus::Done, json!({"satisfied": true}), None)),
                            Ok(false) if timed_out => resolve.push((rid.clone(), sid.clone(), StepStatus::Timeout, json!({"satisfied": false, "timed_out": true}), Some("wait timed out".into()))),
                            Ok(false) => {}
                            Err(e) => resolve.push((rid.clone(), sid.clone(), StepStatus::Failed, Value::Null, Some(format!("wait condition: {e}")))),
                        }
                    }
                    Some("run") => {
                        let target = w["run"].as_str().unwrap_or("");
                        match self.runs.get(target) {
                            Some(t) if t.status.is_terminal() => resolve.push((rid.clone(), sid.clone(), StepStatus::Done, json!({"run": target, "status": t.status, "output": t.output, "error": t.error}), None)),
                            None => resolve.push((rid.clone(), sid.clone(), StepStatus::Failed, Value::Null, Some(format!("wait run: no such run {target:?}")))),
                            _ if timed_out => resolve.push((rid.clone(), sid.clone(), StepStatus::Timeout, json!({"run": target, "timed_out": true}), Some("wait timed out".into()))),
                            _ => {}
                        }
                    }
                    Some("subagent") => {
                        let handle = w["handle"].as_str().unwrap_or("");
                        match self.subagents.get(handle) {
                            Some(s) if super::reactor::is_terminal_status(&s.status) => resolve.push((rid.clone(), sid.clone(), if s.status == "completed" { StepStatus::Done } else { StepStatus::Failed }, json!({"handle": handle, "status": s.status, "result": s.result, "error": s.error}), (s.status != "completed").then(|| s.error.clone().unwrap_or_else(|| format!("subagent {}", s.status))))),
                            None if !handle.is_empty() => resolve.push((rid.clone(), sid.clone(), StepStatus::Failed, Value::Null, Some(format!("wait subagent: no such subagent {handle:?}")))),
                            _ if timed_out => resolve.push((rid.clone(), sid.clone(), StepStatus::Timeout, json!({"handle": handle, "timed_out": true}), Some("wait timed out".into()))),
                            _ => {}
                        }
                    }
                    Some("join") => {
                        let handles: Vec<String> = w["handles"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                        let min = w["min"].as_u64().map(|m| m as usize).unwrap_or(handles.len());
                        let mut results = Map::new();
                        let mut done = 0usize;
                        for h in &handles {
                            if let Some(t) = self.runs.get(h) {
                                if t.status.is_terminal() {
                                    done += 1;
                                    results.insert(h.clone(), json!({"kind": "run", "status": t.status, "output": t.output, "error": t.error}));
                                }
                            } else if let Some(s) = self.subagents.get(h) {
                                if super::reactor::is_terminal_status(&s.status) {
                                    done += 1;
                                    results.insert(h.clone(), json!({"kind": "subagent", "status": s.status, "result": s.result, "error": s.error}));
                                }
                            } else {
                                results.insert(h.clone(), json!({"error": "unknown handle"}));
                                done += 1;
                            }
                        }
                        if done >= handles.len() || done >= min {
                            resolve.push((rid.clone(), sid.clone(), StepStatus::Done, Value::Object(results), None));
                        } else if timed_out {
                            let partials = w["partials"].as_bool().unwrap_or(false);
                            if partials {
                                resolve.push((rid.clone(), sid.clone(), StepStatus::Done, json!({"partial": true, "results": results}), None));
                            } else {
                                resolve.push((rid.clone(), sid.clone(), StepStatus::Timeout, Value::Object(results), Some("join timed out".into())));
                            }
                        }
                    }
                    Some("deadline") if timed_out => resolve.push((rid.clone(), sid.clone(), StepStatus::Done, json!({"waited_ms": now.saturating_sub(w["since_ms"].as_u64().unwrap_or(now))}), None)),
                    // Absence IS the branch: an event that never arrives routes
                    // through `on_timeout` like any other deadline, which is
                    // what makes "alert fired, no recovery in ten minutes"
                    // expressible without a polling loop.
                    Some("resource") | Some("signal") | Some("message") | Some("human") | Some("child_run") | Some("event") if timed_out => {
                        resolve.push((rid.clone(), sid.clone(), StepStatus::Timeout, json!({"timed_out": true}), Some("wait timed out".into())));
                    }
                    _ => {}
                }
            }
        }
        for (rid, sid, status, out, err) in resolve {
            self.log.info(
                "wait.resolved",
                json!({"run": rid, "step": sid, "status": status}),
            );
            self.finish_step_pub(&rid, &sid, status, Some(out), err, 0);
        }
    }

    /// The variables an `await`/`wait condition` sees.
    pub(crate) fn await_data_view(&self) -> Data {
        let mut d = Data::new();
        d.insert(
            "runs".into(),
            Value::Object(
                self.runs
                    .iter()
                    .map(|(k, r)| (k.clone(), json!({"status": r.status, "output": r.output})))
                    .collect(),
            ),
        );
        d.insert(
            "subagents".into(),
            Value::Object(
                self.subagents
                    .iter()
                    .map(|(k, s)| (k.clone(), json!({"status": s.status, "result": s.result})))
                    .collect(),
            ),
        );
        d.insert("now_ms".into(), json!(now_ms()));
        d.insert(
            "signals".into(),
            Value::Object(
                self.recent_signals
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
        );
        d
    }

    /// A resource update arrived: resolve `wait resource` steps on it.
    ///
    /// The notify-then-read runs on an executor thread, never here. This is the
    /// single-writer loop: an MCP `resources/read` is a network round trip
    /// bounded only by the server's own patience, so reading inline hands a slow
    /// (or hostile) server the whole daemon for the length of one read — no
    /// timer fires, no checkpoint lands, the drain does not progress and SIGTERM
    /// is not observed. Subscriptions are agentd's reactivity story, so this is
    /// the hot path. The thread reports back through `events_tx` exactly like
    /// `step_mcp_resource` and every other asynchronous effect, and the loop
    /// resolves the waits when that event arrives.
    pub(crate) fn on_resource_updated(&mut self, server: &str, uri: &str) {
        // Each hit carries its own deadline: the loop keeps running while we
        // read, so a `wait … timeout` may resolve the step as Timeout first, and
        // a late read must not resurrect a step the loop has already finished.
        let mut hits: Vec<(String, String, Option<u64>)> = Vec::new();
        for (rid, run) in &self.runs {
            for (sid, st) in &run.steps {
                if st.status == StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "resource"
                    && w["server"] == server
                    && w["uri"] == uri
                {
                    hits.push((rid.clone(), sid.clone(), w["deadline_ms"].as_u64()));
                }
            }
        }
        if hits.is_empty() {
            return;
        }
        let Some(client) = self.mcp.get(server).cloned() else {
            return; // the server went away between the notification and here
        };
        let tx = self.events_tx.clone();
        let (srv, u) = (server.to_string(), uri.to_string());
        std::thread::Builder::new()
            .name(format!("mcp.updated:{server}"))
            .spawn(move || {
                // A failed read still resolves the wait, as the inline read did:
                // the update itself is the event, and `content: null` says the
                // follow-up read did not land.
                let content = client.read_resource(&u).ok().map(|r| {
                    let t = r.text();
                    serde_json::from_str::<Value>(&t).unwrap_or(Value::String(t))
                });
                for (run, step, deadline_ms) in hits {
                    if deadline_ms.is_some_and(|d| now_ms() > d) {
                        continue;
                    }
                    let _ = tx.send(super::events::Event::StepDone {
                        run,
                        step,
                        output: json!({"uri": u, "server": srv, "content": content}),
                        is_error: false,
                        error: None,
                        tokens: 0,
                    });
                }
            })
            .ok();
    }

    /// An A2A message arrived on a conversation: resolve every step suspended
    /// waiting for one. Returns how many were woken.
    ///
    /// This is what makes the asynchronous half of an A2A conversation
    /// expressible: `wait {on: message}` and `a2a.wait` both suspend on a
    /// `{kind: message, conversation}` record, and without a resolver they
    /// could only ever end by timing out — a workflow could send but never be
    /// woken by the reply.
    ///
    /// A wait with an EMPTY conversation matches any conversation, which is how
    /// a workflow awaits "the next thing anyone says" without knowing the id in
    /// advance.
    pub(crate) fn deliver_a2a_message(
        &mut self,
        conversation: &str,
        message: &Value,
        principal: Option<&str>,
    ) -> u64 {
        let mut hits: Vec<(String, String)> = Vec::new();
        for (rid, run) in &self.runs {
            for (sid, st) in &run.steps {
                if st.status == StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "message"
                {
                    let want = w["conversation"].as_str().unwrap_or("");
                    if want.is_empty() || want == conversation {
                        hits.push((rid.clone(), sid.clone()));
                    }
                }
            }
        }
        let mut delivered = 0u64;
        for (rid, sid) in hits {
            delivered += 1;
            self.finish_step_pub(
                &rid,
                &sid,
                StepStatus::Done,
                Some(json!({
                    "conversation": conversation,
                    "message": message,
                    "principal": principal,
                })),
                None,
                0,
            );
        }
        delivered
    }

    /// Deliver a named signal: to `wait signal` steps (any run, or only the
    /// `run` named as the target), to `signal` start nodes, and into the
    /// recent-signals view. Returns how many waiting steps were woken.
    pub(crate) fn deliver_signal(
        &mut self,
        name: &str,
        payload: Value,
        target_run: Option<&str>,
        from_run: Option<&str>,
    ) -> u64 {
        let mut delivered = 0u64;
        let mut hits: Vec<(String, String)> = Vec::new();
        for (rid, run) in &self.runs {
            if let Some(t) = target_run
                && rid != t
            {
                continue;
            }
            for (sid, st) in &run.steps {
                if st.status == StepStatus::Suspended
                    && let Some(w) = &st.wait
                    && w["kind"] == "signal"
                    && w["signal"] == name
                {
                    hits.push((rid.clone(), sid.clone()));
                }
            }
        }
        for (rid, sid) in hits {
            delivered += 1;
            self.finish_step_pub(
                &rid,
                &sid,
                StepStatus::Done,
                Some(json!({"signal": name, "payload": payload, "from": from_run})),
                None,
                0,
            );
        }
        self.recent_signals.insert(
            name.to_string(),
            json!({"payload": payload, "ts": now_ms(), "from": from_run}),
        );
        if self.recent_signals.len() > 64 {
            let first = self.recent_signals.keys().next().cloned();
            if let Some(k) = first {
                self.recent_signals.remove(&k);
            }
        }
        // Signal start nodes.
        delivered += self.fire_signal_starts(name, &payload, target_run.is_none());
        // When this signal is the configured `lifecycle.until_signal` it is the
        // retirement trigger: stop admitting, drain live runs, exit cleanly.
        // Delivery to whatever was parked on the signal happens first (above),
        // so an all-clear both completes the waiting run and retires the
        // instance.
        if self
            .settings
            .lifecycle
            .until_signal
            .as_deref()
            .is_some_and(|u| u == name)
        {
            self.log
                .info("lifecycle.until_signal", json!({"signal": name}));
            self.begin_drain("until_signal");
        }
        delivered
    }

    /// `message` step: deliver into one of this instance's own conversations,
    /// so a run can hand work to the agent instead of only the reverse.
    ///
    /// The delivery is an ordinary inbound message event — the same one the A2A
    /// listener produces — so it takes the same three readers in the same
    /// order (a step waiting on the conversation, then a matching `a2a` start,
    /// then a turn), and inherits write-ahead durability, crash replay, the
    /// per-context lock and pressure shedding without any of them being taught
    /// about this node.
    ///
    /// `wait: reply` parks on the answer using the `message` wait that already
    /// exists; without it the step completes as soon as the delivery is durable
    /// and the turn happens on its own schedule.
    fn step_message(&mut self, run_id: &str, step_id: &str, spec: &Map<String, Value>) {
        let to = spec
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or(ROOT)
            .trim();
        // `to: new` opens a fresh conversation, which is how a run gets a
        // clean transcript without borrowing the operator's.
        let ctx = if to.eq_ignore_ascii_case("new") {
            format!("run-{}", crate::state::ulid::new())
        } else if to.is_empty() {
            ROOT.to_string()
        } else {
            to.to_string()
        };
        let text = spec
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let parts = spec.get("parts").cloned();
        if text.trim().is_empty() && parts.is_none() {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some("message: one of text or parts is required".into()),
                0,
            );
            return;
        }
        // The hop guard. A run inherits the depth of the work that caused it,
        // so a chain that came back around to its own conversation is refused
        // HERE — before the delivery is durable — rather than being noticed
        // once the loop is already running.
        let depth = self.runs.get(run_id).map(|r| r.msg_depth).unwrap_or(0) + 1;
        let cap = self.settings.limits.message_depth();
        if depth > cap {
            self.log.warn(
                "message.too_deep",
                json!({"run": run_id, "step": step_id, "conversation": ctx,
                       "depth": depth, "max": cap}),
            );
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!(
                    "message refused: {depth} chained deliveries exceeds limits.max_message_depth ({cap}) — \
                     a message that causes a turn that causes this message is a loop, not a conversation"
                )),
                0,
            );
            return;
        }
        let mut payload = json!({"text": text, "context_id": ctx, "msg_depth": depth});
        if let Some(p) = parts {
            payload["parts"] = p;
        }
        let principal = self.runs.get(run_id).and_then(|r| r.principal.clone());
        if let Err(e) = self.accept_event(
            crate::runtime::events::kinds::A2A_MESSAGE,
            principal,
            payload,
        ) {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!("message: {e}")),
                0,
            );
            return;
        }
        let wants_reply = match spec.get("wait") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => {
                let s = s.trim();
                !s.is_empty() && !s.eq_ignore_ascii_case("none") && !s.eq_ignore_ascii_case("false")
            }
            Some(Value::Object(o)) => o
                .get("for")
                .and_then(Value::as_str)
                .is_some_and(|f| f.eq_ignore_ascii_case("reply")),
            _ => false,
        };
        if !wants_reply {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Done,
                Some(json!({"delivered": true, "conversation": ctx, "depth": depth})),
                None,
                0,
            );
            return;
        }
        // Park on the answer. `timeout` may sit on the node or inside `wait`,
        // and `on_timeout` routes an unanswered message the same way every
        // other suspending kind routes one.
        let timeout = spec
            .get("timeout")
            .or_else(|| spec.get("wait").and_then(|w| w.get("timeout")))
            .and_then(crate::engine::model::duration_ms_opt);
        self.suspend_wait(
            run_id,
            step_id,
            wait_record("message", json!({"conversation": ctx}), timeout),
        );
    }

    /// `workflow` step: a child run (`mode: sync|async|detached`, `cascade`).
    fn step_child_workflow(
        &mut self,
        run_id: &str,
        step_id: &str,
        spec: &Map<String, Value>,
        caller: &ToolCaller,
    ) {
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mode = spec
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("sync")
            .to_string();
        let inputs = spec.get("inputs").cloned().unwrap_or(json!({}));
        let start = spec
            .get("start")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(pin) = spec.get("version").and_then(Value::as_str)
            && let Some(w) = self.workflows.get(&name)
            && !w.hash.starts_with(pin)
        {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!(
                    "workflow {name:?} hash {} does not match the pinned version {pin}",
                    &w.hash[..12]
                )),
                0,
            );
            return;
        }
        let Some(w) = self.workflows.get(&name) else {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!("no such workflow {name:?}")),
                0,
            );
            return;
        };
        let start_node = match start {
            Some(s) => s,
            None => {
                let starts = w.start_steps();
                match starts
                    .iter()
                    .find(|s| s.kind == "manual")
                    .or_else(|| starts.first())
                {
                    Some(s) => s.id.clone(),
                    None => {
                        self.finish_step_pub(
                            run_id,
                            step_id,
                            StepStatus::Failed,
                            None,
                            Some(format!("workflow {name:?} has no start node")),
                            0,
                        );
                        return;
                    }
                }
            }
        };
        let cascade = spec.get("cascade").and_then(Value::as_bool).unwrap_or(true);
        let payload = json!({"workflow": name, "node": start_node, "payload": {"requested_by": caller.label_pub()}, "inputs": inputs, "parent": {"run": run_id, "step": step_id, "cascade": cascade}, "conversation": self.runs.get(run_id).and_then(|r| r.conversation.clone()), "task": self.runs.get(run_id).and_then(|r| r.task.clone()), "msg_depth": self.runs.get(run_id).map(|r| r.msg_depth).unwrap_or(0)});
        match self.accept_event(
            kinds::WORKFLOW_RUN,
            self.runs.get(run_id).and_then(|r| r.principal.clone()),
            payload,
        ) {
            Ok(_) => {}
            Err(e) => {
                self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        }
        // Process the event now so the child id is known.
        let child_id = if let Some(ev) = self.inbox_queue.pop_back() {
            let before: std::collections::BTreeSet<String> = self.runs.keys().cloned().collect();
            let done = self.on_start_event(&ev);
            if done {
                self.inbox_done(&ev.id);
            }
            self.runs.keys().find(|k| !before.contains(*k)).cloned()
        } else {
            None
        };
        let Some(child_id) = child_id else {
            // Queued (concurrency) — wait for it to appear: fall back to a run-wait
            // on the parent link. Simplest: suspend as a `child_run` wait resolved
            // when a run with our parent link finishes.
            self.suspend_wait(
                run_id,
                step_id,
                wait_record("child_run", json!({"workflow": name, "mode": mode}), None),
            );
            return;
        };
        if let Some(r) = self.runs.get_mut(run_id) {
            r.children.push(child_id.clone());
            r.touch();
        }
        match mode.as_str() {
            "sync" => self.suspend_wait(
                run_id,
                step_id,
                wait_record(
                    "run",
                    json!({"run": child_id, "child": true}),
                    spec.get("timeout")
                        .and_then(crate::engine::model::duration_ms_opt),
                ),
            ),
            _ => self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Done,
                Some(json!({"run": child_id, "workflow": name, "mode": mode})),
                None,
                0,
            ),
        }
    }

    /// `mcp.resource {server, op: read|list|prompt|complete, …}`.
    fn step_mcp_resource(&mut self, run_id: &str, step_id: &str, spec: &Map<String, Value>) {
        let server = spec
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let op = spec
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("read")
            .to_string();
        let Some(client) = self.mcp.get(&server).cloned() else {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some(format!("mcp.resource: server {server:?} is not connected")),
                0,
            );
            return;
        };
        let uri = spec
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let arguments = spec.get("arguments").cloned();
        let reference = spec.get("reference").cloned();
        let argument = spec.get("argument").cloned();
        let tx = self.events_tx.clone();
        let (r, s) = (run_id.to_string(), step_id.to_string());
        self.executing
            .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
        std::thread::Builder::new()
            .name(format!("mcp.resource:{server}"))
            .spawn(move || {
                let res: Result<Value, String> = match op.as_str() {
                    "read" => client.read_resource(&uri).map(|r| {
                        let t = r.text();
                        json!({"uri": uri, "text": t, "json": serde_json::from_str::<Value>(&t).ok(), "contents": r.contents})
                    }).map_err(|e| e.to_string()),
                    "list" => client.list_resources().map(|l| json!({"resources": l})).map_err(|e| e.to_string()),
                    "prompt" => client.get_prompt(&name, arguments).map(|p| json!({"description": p.description, "messages": p.messages, "text": crate::context::skills::prompt_messages_text(&p.messages)})).map_err(|e| e.to_string()),
                    "complete" => client.complete(reference.unwrap_or(Value::Null), argument.unwrap_or(Value::Null)).map(|c| serde_json::to_value(c).unwrap_or(Value::Null)).map_err(|e| e.to_string()),
                    "templates" => client.list_resource_templates().map(|l| json!({"templates": l})).map_err(|e| e.to_string()),
                    other => Err(format!("mcp.resource: op must be read|list|prompt|complete|templates (got {other:?})")),
                };
                let (output, is_error, error) = match res {
                    Ok(v) => (v, false, None),
                    Err(e) => (Value::Null, true, Some(e)),
                };
                let _ = tx.send(super::events::Event::StepDone { run: r, step: s, output, is_error, error, tokens: 0 });
            })
            .ok();
    }

    /// Resolve a configured A2A peer into a dialable endpoint plus its client
    /// auth (bearer headers, a per-request SigV4 signer, an mTLS identity).
    ///
    /// Shared by `a2a.delegate` and `a2a.send`, which differ only in what they
    /// do once connected — everything up to the socket is identical, and it is
    /// ~140 lines of credential plumbing that must not diverge between the two.
    #[cfg(feature = "a2a")]
    // `timeout` bounds only the interactive credential fetch, which lives behind
    // `oauth`; without that feature there is nothing to bound and the parameter
    // is genuinely unused. Keeping it in the signature keeps both callers
    // identical across feature sets.
    #[cfg_attr(not(feature = "oauth"), allow(unused_variables))]
    fn a2a_peer_conn(
        &self,
        peer_name: &str,
        timeout: Duration,
        what: &str,
    ) -> Result<(crate::config::A2aEndpoint, crate::mcp::a2a_client::PeerAuth), String> {
        let configured = self
            .settings
            .a2a
            .peers
            .iter()
            .find(|p| p.name == peer_name)
            .cloned();
        // Live instance children are dialable as peers too — by handle, or by
        // template name when that template has a single instance. A configured
        // peer wins the name, so an operator can always override the lookup.
        let peer = match configured {
            Some(p) => p,
            None => match self.instance_peer_endpoint(peer_name) {
                Some(endpoint) => crate::config::v2::A2aPeer {
                    name: peer_name.to_string(),
                    endpoint,
                    service: None,
                    headers: std::collections::BTreeMap::new(),
                    client_cert: None,
                    client_key: None,
                    auth: None,
                },
                None => {
                    return Err(format!(
                        "{what}: no such peer {peer_name:?} (a2a.peers or a live instance child)"
                    ));
                }
            },
        };
        let spec_v1 = crate::config::A2aPeerSpec {
            name: peer.name.clone(),
            endpoint: peer.endpoint.clone(),
            headers: peer
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            client_cert: peer.client_cert.clone(),
            client_key: peer.client_key.clone(),
        };
        let endpoint = spec_v1
            .endpoint_of()
            .map_err(|e| format!("{what}: peer endpoint: {e}"))?;
        #[allow(unused_mut)]
        let mut headers = crate::mcp::auth::resolve_headers(&spec_v1.headers)
            .map_err(|e| format!("{what}: peer headers: {e}"))?;
        // A peer `auth:` block resolves at dial time. A body-INDEPENDENT
        // bearer (static / oauth2 device-login / spiffe jwt) is baked into the
        // static headers; SigV4 (`kind: aws`) covers the exact body, so it rides
        // as a PER-REQUEST signer on `PeerAuth` (re-run on every POST).
        #[cfg(feature = "oauth")]
        let mut peer_signer: Option<std::sync::Arc<dyn ::mcp::http::RequestSigner>> = None;
        #[cfg(feature = "oauth")]
        if let Some(a) = &peer.auth {
            let aspec = a.to_spec();
            // A peer that names a catalog `service:` caches its credential
            // under `service:<entry>`, shared with every other consumer of that
            // entry, so one `agentd login service:<entry>` serves them all.
            // Without one the cache is private to this peer's name.
            let target = match &peer.service {
                Some(svc) => format!("service:{svc}"),
                None => format!("a2a:{}", peer.name),
            };
            if aspec.kind == "aws" {
                let s = crate::auth::aws::SigV4Signer::from_spec(&aspec, &target)
                    .map_err(|e| format!("{what}: peer aws auth: {e}"))?;
                peer_signer = Some(s as std::sync::Arc<dyn ::mcp::http::RequestSigner>);
            } else if let Some(signer) = crate::auth::device::signer_for(&aspec, &target, timeout)
                .map_err(|e| format!("{what}: peer auth: {e}"))?
            {
                for (k, v) in signer.sign("POST", &peer.endpoint, "/", &[]) {
                    headers.push((k, v));
                }
            }
        }
        #[allow(unused_mut)]
        let mut auth = crate::mcp::a2a_client::PeerAuth {
            headers,
            ..Default::default()
        };
        #[cfg(feature = "oauth")]
        {
            auth.signer = peer_signer;
        }
        #[cfg(feature = "tls")]
        if let (Some(cert), Some(key)) = (&spec_v1.client_cert, &spec_v1.client_key) {
            let id = std::fs::read(cert)
                .and_then(|c| std::fs::read(key).map(|k| (c, k)))
                .map_err(|e| e.to_string())
                .and_then(|(c, k)| {
                    crate::net::tls::ClientIdentity::from_pem(&c, &k).map_err(|e| e.to_string())
                })
                .map_err(|e| format!("{what}: peer mtls: {e}"))?;
            auth.identity = Some(id);
        }
        Ok((endpoint, auth))
    }

    /// `a2a.send {to, parts, context?, timeout?}` — notify a peer, do not wait.
    ///
    /// The step completes when the peer ACCEPTS the message, not when it has
    /// done anything about it. That is the difference from `a2a.delegate`, and
    /// it is what makes the asynchronous shape expressible: send, keep working,
    /// and pick the reply up later with `a2a.wait` on the same conversation.
    #[cfg(feature = "a2a")]
    fn step_a2a_send(&mut self, run_id: &str, step_id: &str, spec: &Map<String, Value>) {
        let peer_name = spec
            .get("to")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // `command` + `args` build the typed DataPart the peer's `a2a` start
        // nodes match on; `parts` text (if any) rides along for the humans
        // reading the transcript. Without `command`, `parts` goes as before.
        let parts = match spec.get("command").and_then(Value::as_str) {
            Some(cmd) => {
                let mut env = serde_json::Map::new();
                env.insert("op".into(), json!(cmd));
                if let Some(args) = spec.get("args") {
                    match args.as_object() {
                        Some(o) => {
                            for (k, v) in o {
                                env.insert(k.clone(), v.clone());
                            }
                        }
                        None => {
                            self.finish_step_pub(
                                run_id,
                                step_id,
                                StepStatus::Failed,
                                None,
                                Some("a2a.send: args must be a mapping".into()),
                                0,
                            );
                            return;
                        }
                    }
                }
                let mut arr = vec![json!({"data": {"agentd": Value::Object(env)}})];
                if let Some(t) = spec.get("parts").and_then(Value::as_str) {
                    arr.push(json!({"text": t}));
                }
                Value::Array(arr)
            }
            None => spec.get("parts").cloned().unwrap_or(Value::Null),
        };
        let context = spec
            .get("context")
            .and_then(Value::as_str)
            .map(str::to_string);
        let timeout = spec
            .get("timeout")
            .and_then(crate::engine::model::duration_ms_opt)
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(30));
        let (endpoint, auth) = match self.a2a_peer_conn(&peer_name, timeout, "a2a.send") {
            Ok(v) => v,
            Err(e) => {
                self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        };
        let message_id = idempotency_message_id(spec, run_id, step_id);
        let tx = self.events_tx.clone();
        let (r, st) = (run_id.to_string(), step_id.to_string());
        self.executing
            .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
        self.log.info(
            "a2a.send",
            json!({"run": run_id, "step": step_id, "to": peer_name}),
        );
        std::thread::Builder::new()
            .name(format!("a2a.send:{peer_name}"))
            .spawn(move || {
                let deadline = std::time::Instant::now() + timeout;
                let (output, is_error, error) = match crate::mcp::a2a_client::send(
                    &endpoint,
                    auth,
                    &parts,
                    context.as_deref(),
                    message_id.as_deref(),
                    deadline,
                ) {
                    Ok(v) => (v, false, None),
                    Err(e) => (Value::Null, true, Some(e)),
                };
                let _ = tx.send(super::events::Event::StepDone {
                    run: r,
                    step: st,
                    output,
                    is_error,
                    error,
                    tokens: 0,
                });
            })
            .ok();
    }

    /// `a2a.delegate {peer, objective, output_contract?, timeout?}`: hand a unit
    /// of work to a peer agent over A2A and suspend until it answers.
    #[cfg(feature = "a2a")]
    fn step_a2a_delegate(&mut self, run_id: &str, step_id: &str, spec: &Map<String, Value>) {
        let peer_name = spec
            .get("peer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let objective = spec
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // The typed form: `command` + `args` become the DataPart the peer's
        // `a2a` start matches; `objective` text (optional here) rides along.
        let command_env = match spec.get("command").and_then(Value::as_str) {
            Some(cmd) => {
                let mut env = serde_json::Map::new();
                env.insert("op".into(), json!(cmd));
                if let Some(args) = spec.get("args") {
                    match args.as_object() {
                        Some(o) => {
                            for (k, v) in o {
                                env.insert(k.clone(), v.clone());
                            }
                        }
                        None => {
                            self.finish_step_pub(
                                run_id,
                                step_id,
                                StepStatus::Failed,
                                None,
                                Some("a2a.delegate: args must be a mapping".into()),
                                0,
                            );
                            return;
                        }
                    }
                }
                Some(Value::Object(env))
            }
            None => None,
        };
        let contract = spec
            .get("output_contract")
            .and_then(Value::as_str)
            .map(str::to_string);
        let timeout = spec
            .get("timeout")
            .and_then(crate::engine::model::duration_ms_opt)
            .map(Duration::from_millis)
            .unwrap_or(Duration::from_secs(120));
        let (endpoint, auth) = match self.a2a_peer_conn(&peer_name, timeout, "a2a.delegate") {
            Ok(v) => v,
            Err(e) => {
                self.finish_step_pub(run_id, step_id, StepStatus::Failed, None, Some(e), 0);
                return;
            }
        };
        let message_id = idempotency_message_id(spec, run_id, step_id);
        let tx = self.events_tx.clone();
        let (r, s) = (run_id.to_string(), step_id.to_string());
        self.executing
            .insert(format!("{run_id}/{step_id}"), std::time::Instant::now());
        self.log.info(
            "a2a.delegate",
            json!({"run": run_id, "step": step_id, "peer": peer_name}),
        );
        std::thread::Builder::new()
            .name(format!("a2a.delegate:{peer_name}"))
            .spawn(move || {
                let deadline = std::time::Instant::now() + timeout;
                let (output, is_error, error) = match crate::mcp::a2a_client::delegate(
                    &endpoint,
                    auth,
                    &objective,
                    command_env.as_ref(),
                    contract.as_deref(),
                    message_id.as_deref(),
                    deadline,
                ) {
                    crate::mcp::a2a_client::DelegateOutcome::Distillate(text) => (
                        serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)),
                        false,
                        None,
                    ),
                    crate::mcp::a2a_client::DelegateOutcome::Error(e) => {
                        (Value::Null, true, Some(e))
                    }
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

    /// The `think` presets: sugar over `think` with a fixed prompt frame + schema.
    fn step_preset(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: &Step,
        spec: &Map<String, Value>,
        data: &Data,
    ) {
        let input = spec.get("input").cloned().unwrap_or(Value::Null);
        let input_text = match &input {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let extra = spec
            .get("prompt")
            .and_then(Value::as_str)
            .map(|p| format!("\n\nAdditional guidance: {p}"))
            .unwrap_or_default();
        let (prompt, schema) = match step.kind.as_str() {
            "classify" => {
                let classes: Vec<String> = spec
                    .get("classes")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                (
                    format!(
                        "Classify the input into exactly one of these classes: {}.{extra}\n\nInput:\n{input_text}\n\nReply with ONLY a JSON object {{\"class\": <one of the classes>, \"confidence\": <0..1>, \"reason\": <short>}}.",
                        classes.join(", ")
                    ),
                    json!({"type": "object", "properties": {"class": {"enum": classes}, "confidence": {"type": "number"}, "reason": {"type": "string"}}, "required": ["class"]}),
                )
            }
            "extract" => {
                let schema = spec
                    .get("output_schema")
                    .cloned()
                    .unwrap_or(json!({"type": "object"}));
                (
                    format!(
                        "Extract the structured data described by this JSON Schema from the input.{extra}\n\nSchema:\n{schema}\n\nInput:\n{input_text}\n\nReply with ONLY one JSON object matching the schema."
                    ),
                    schema,
                )
            }
            "summarize" => {
                let length = spec
                    .get("length")
                    .and_then(Value::as_str)
                    .unwrap_or("a short paragraph");
                (
                    format!(
                        "Summarize the input in {length}. Keep facts, names, numbers and identifiers verbatim.{extra}\n\nInput:\n{input_text}\n\nReply with ONLY a JSON object {{\"summary\": <text>}}."
                    ),
                    json!({"type": "object", "properties": {"summary": {"type": "string"}}, "required": ["summary"]}),
                )
            }
            "judge" => {
                let rubric = spec.get("rubric").cloned().unwrap_or(Value::Null);
                (
                    format!(
                        "Judge the input against the rubric.{extra}\n\nRubric:\n{rubric}\n\nInput:\n{input_text}\n\nReply with ONLY a JSON object {{\"verdict\": \"pass\"|\"fail\", \"score\": <0..10>, \"reasons\": [<short strings>]}}."
                    ),
                    json!({"type": "object", "properties": {"verdict": {"enum": ["pass", "fail"]}, "score": {"type": "number"}, "reasons": {"type": "array", "items": {"type": "string"}}}, "required": ["verdict"]}),
                )
            }
            _ => {
                let choices: Vec<String> = match spec.get("choices") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect(),
                    Some(Value::Object(o)) => o.keys().cloned().collect(),
                    _ => Vec::new(),
                };
                (
                    format!(
                        "Route the input to exactly one of these choices: {}.{extra}\n\nInput:\n{input_text}\n\nReply with ONLY a JSON object {{\"choice\": <one of the choices>, \"reason\": <short>}}.",
                        choices.join(", ")
                    ),
                    json!({"type": "object", "properties": {"choice": {"enum": choices}, "reason": {"type": "string"}}, "required": ["choice"]}),
                )
            }
        };
        // Delegate to the think machinery with a synthesized spec.
        let mut think = step.clone();
        think.kind = "think".into();
        let mut think_spec = Map::new();
        think_spec.insert("prompt".into(), Value::String(prompt));
        think_spec.insert("output_schema".into(), schema);
        if let Some(sk) = spec.get("skills") {
            think_spec.insert("skills".into(), sk.clone());
        }
        self.step_turn_pub(run_id, step_id, &think, &think_spec, data);
    }

    /// Step `cache {key, ttl}`: a memoized output by key (memory `_cache/<hash>`).
    pub(crate) fn cache_lookup(
        &mut self,
        step: &Step,
        spec: &Map<String, Value>,
        data: &Data,
    ) -> Option<(String, Option<Value>)> {
        let cache = step.cache.as_ref()?;
        let key_expr = cache.get("key").and_then(Value::as_str).unwrap_or("");
        let key_material = if key_expr.is_empty() {
            Value::Object(spec.clone()).to_string()
        } else {
            match crate::engine::template::render_str(key_expr, data) {
                Ok(v) => v.to_string(),
                Err(_) => return None,
            }
        };
        let hash = crate::sha::sha256_hex(
            format!("{}:{}:{}", step.kind, step.id, key_material).as_bytes(),
        );
        let ttl_ms = cache
            .get("ttl")
            .and_then(crate::engine::model::duration_ms_opt);
        let cache_key = format!("_cache/{hash}");
        let hit = self
            .durable
            .get(Kind::Memory, &cache_key)
            .ok()
            .flatten()
            .and_then(|env| {
                let ts = env.state.get("ts").and_then(Value::as_u64).unwrap_or(0);
                let fresh = ttl_ms.is_none_or(|t| now_ms() < ts + t);
                fresh.then(|| env.state.get("value").cloned()).flatten()
            });
        Some((cache_key, hit))
    }

    pub(crate) fn cache_store(&mut self, cache_key: &str, output: &Value) {
        let _ = self.durable.put(
            Kind::Memory,
            cache_key,
            json!({"value": output, "ts": now_ms()}),
            None,
        );
    }
}
