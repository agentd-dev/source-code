// SPDX-License-Identifier: AGPL-3.0-only
//! The built-in command ops, reached as a command DataPart on `SendMessage`.
//!
//! One table ([`surface::OPS`]) says what each op is; this file routes an op
//! to its handler with ONE match on the row's [`Handler`], and turns what the
//! handler produced into the reply the row's [`Reply`] promises — a Message
//! for a read, a Task for work. A new op is a row plus its handler's arm.

use super::redact::redact_settings;
use super::send::command_data;
use super::{FeedVis, TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::principals::workflow_name_of;
use crate::a2a::tasks::{Link, State, Task};
use crate::runtime::events::kinds;
use crate::runtime::reactor::Runtime;
use crate::runtime::surface::{self, Gate, Handler, Reply};
use serde_json::{Value, json};

/// A refusal carrying agentd's `ErrorInfo`, so a client branches on the
/// reason rather than on the prose.
pub(super) fn refusal(code: i64, why: &str, msg: &str, meta: &[(&str, &str)]) -> Value {
    json!({"_error": {
        "code": code,
        "message": msg,
        "data": [errors::error_info(errors::domain_of(why), why, meta)],
    }})
}

/// The answer to an op that is no row of the table, or a row nothing serves.
/// A removed op is named with what replaced it.
pub(super) fn unknown_op(op: &str) -> Value {
    let msg = match surface::removed_op(op) {
        Some(hint) => format!(
            "command `{op}` was removed in agentd {}: {hint}",
            surface::OPS_REMOVED_IN
        ),
        None => format!("unknown command {op:?}"),
    };
    refusal(
        errors::INVALID_PARAMS,
        reason::UNKNOWN_OP,
        &msg,
        &[("op", op)],
    )
}

/// The `Read` handler's ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadOp {
    Status,
    Config,
    PlanGet,
}

/// The `Workflow` handler's ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowOp {
    Run,
    Status,
    Cancel,
    Signal,
}

/// The `Subagent` handler's ops. They are the internal subagent tools, run
/// verbatim, so the op name is the tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentOp {
    Send,
    Kill,
    Status,
}

/// Where an op goes: its handler, with the op already resolved to one the
/// handler implements. Resolving is separate from running so that "every
/// served op reaches a handler arm" is checkable without a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Read(ReadOp),
    Workflow(WorkflowOp),
    Subagent(SubagentOp),
    Admin(super::admin::AdminOp),
    Introspection(super::introspection::IntrospectionOp),
    Auth,
    Instance(crate::runtime::instances::InstanceOp),
}

/// The row and route of `op`, or `None` for an op nothing serves. The ONE
/// match on the table's handler column.
fn route(op: &str) -> Option<(&'static surface::OpSpec, Route)> {
    use super::admin::AdminOp;
    use super::introspection::IntrospectionOp;
    use crate::runtime::instances::InstanceOp;
    let spec = surface::op_spec(op)?;
    let route = match spec.handler {
        Handler::Read => match op {
            "status" => Some(ReadOp::Status),
            "config" => Some(ReadOp::Config),
            "plan.get" => Some(ReadOp::PlanGet),
            _ => None,
        }
        .map(Route::Read),
        Handler::Workflow => match op {
            "workflow.run" => Some(WorkflowOp::Run),
            "workflow.status" => Some(WorkflowOp::Status),
            "workflow.cancel" => Some(WorkflowOp::Cancel),
            "workflow.signal" => Some(WorkflowOp::Signal),
            _ => None,
        }
        .map(Route::Workflow),
        Handler::Subagent => match op {
            "subagent.send" => Some(SubagentOp::Send),
            "subagent.kill" => Some(SubagentOp::Kill),
            "subagent.status" => Some(SubagentOp::Status),
            _ => None,
        }
        .map(Route::Subagent),
        Handler::Admin => AdminOp::of(op).map(Route::Admin),
        Handler::Introspection => IntrospectionOp::of(op).map(Route::Introspection),
        Handler::Auth => Some(Route::Auth),
        Handler::Instance => InstanceOp::of(op).map(Route::Instance),
        Handler::Reserved => None,
    };
    route.map(|r| (spec, r))
}

/// What a handler produced, before it is shaped into a reply.
enum Answer {
    /// A read's document: answered as a Message, with no task.
    Doc(Value),
    /// Work that finished at once: a completed task carrying `text` and
    /// `result`, linked to `link` (the conversation turn when `None`).
    Done {
        link: Option<Link>,
        text: Option<String>,
        result: Option<Value>,
    },
    /// A reply already whole: a task the handler created, or a refusal.
    Reply(Value),
}

impl Runtime {
    /// A command DataPart addressed to a built-in op.
    pub(super) fn a2a_command(
        &mut self,
        principal: &Principal,
        op: &str,
        message: &Value,
    ) -> Value {
        let Some((spec, route)) = route(op) else {
            return unknown_op(op);
        };
        // The row's switch, asked here for every row: an op behind a closed
        // gate never reaches its handler, so no handler has to remember to
        // re-check it. Introspection says why, since `admin.set` can open it;
        // any other closed gate answers as the unknown op the card, which
        // does not list it, already told the caller it was.
        if !surface::gate_open(spec, &self.settings) {
            return match spec.gate {
                Gate::Introspection => refusal(
                    UNSUPPORTED_OPERATION,
                    reason::INTROSPECTION_DISABLED,
                    "introspection is disabled (set a2a.introspection.enabled: true, or admin.set it)",
                    &[("op", op)],
                ),
                _ => unknown_op(op),
            };
        }
        let data = command_data(message).unwrap_or_else(|| json!({}));
        // The listener authorized the op already; this is the second lock, so
        // a path into the runtime that skips the listener cannot skip the
        // floor. It also checks what the listener cannot — the workflow a
        // `workflow.run` names.
        if let Err(why) = principal.authorize_command(op, &data) {
            return refusal(
                errors::PERMISSION_DENIED,
                reason::PERMISSION_DENIED,
                &why,
                &[("op", op)],
            );
        }
        let ctx = message["contextId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("a2a"));
        let answer = match route {
            Route::Read(r) => self.read_op(principal, r, &data),
            Route::Workflow(w) => self.workflow_op(principal, w, &data, &ctx),
            Route::Subagent(s) => self.subagent_op(principal, op, s, &data),
            Route::Admin(a) => match self.a2a_admin(principal, a, &data) {
                Ok((text, result)) => Answer::Done {
                    link: None,
                    text: Some(text),
                    result: Some(result),
                },
                Err(e) => Answer::Reply(e),
            },
            Route::Introspection(i) => match self.introspection_op(principal, i, &data) {
                Ok(doc) => Answer::Doc(doc),
                Err(e) => Answer::Reply(e),
            },
            Route::Auth => Answer::Reply(super::auth_ops::handle(self, principal, op, &data)),
            Route::Instance(i) => match self.instance_op(principal, i, &data) {
                Ok(text) => Answer::Done {
                    link: None,
                    text: Some(text),
                    result: None,
                },
                Err(e) => Answer::Reply(err_obj(errors::INVALID_PARAMS, &e)),
            },
        };
        let reply = match answer {
            Answer::Doc(doc) => {
                debug_assert_eq!(spec.reply, Reply::Message, "{op} answered a document");
                crate::a2a::reply::read_reply(&ctx, doc)
            }
            Answer::Done { link, text, result } => {
                debug_assert_eq!(spec.reply, Reply::Task, "{op} answered a task");
                self.task_complete_now(
                    &ctx,
                    principal,
                    link.unwrap_or(Link::Turn { ctx: ctx.clone() }),
                    State::Completed,
                    text,
                    result,
                )
            }
            Answer::Reply(v) => v,
        };
        // Surface work a caller asked for on the feed, so every attached
        // display client sees what its peers did — once it ran. Announced
        // before the handler, a `workflow.run` the handler then refused was
        // shown to every client as a command the principal ran. Reads stay
        // off it: they are the observation plumbing itself, and N clients
        // polling them would spam every transcript.
        if spec.reply == Reply::Task
            && matches!(spec.handler, Handler::Workflow | Handler::Subagent)
            && reply.get("_error").is_none()
        {
            self.feed_push(
                "command",
                FeedVis::Owner(Some(principal.id.clone())),
                json!({"op": op, "principal": principal.id, "contextId": ctx}),
            );
        }
        reply
    }

    /// `status`, `config` and `plan.get`.
    fn read_op(&mut self, principal: &Principal, op: ReadOp, data: &Value) -> Answer {
        match op {
            ReadOp::Status => Answer::Doc(self.status_value()),
            // The effective merged configuration. Redacted on the way out: the
            // merged doc carries env/flag-supplied credentials INLINE, and
            // operator-only is not the same as public (see `redact_settings`).
            // What survives is the `{{secret:…}}` reference, never a value.
            ReadOp::Config => Answer::Doc(json!({"config": redact_settings(&self.settings_doc)})),
            ReadOp::PlanGet => {
                let id = data["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::context::ROOT.to_string());
                match self.contexts.get(&id) {
                    Some(c)
                        if principal.is_operator()
                            || c.principal.as_deref() == Some(principal.id.as_str()) =>
                    {
                        Answer::Doc(
                            json!({"conversation": id, "plan": c.plan, "progress": c.plan.as_ref().map(|p| p.progress())}),
                        )
                    }
                    _ => Answer::Reply(err_obj(TASK_NOT_FOUND, "no such conversation")),
                }
            }
        }
    }

    /// `workflow.run`, `workflow.status`, `workflow.cancel` and
    /// `workflow.signal`.
    fn workflow_op(
        &mut self,
        principal: &Principal,
        op: WorkflowOp,
        data: &Value,
        ctx: &str,
    ) -> Answer {
        match op {
            WorkflowOp::Run => Answer::Reply(self.workflow_run(principal, data, ctx)),
            WorkflowOp::Status => {
                let view: Vec<Value> = match data["run"].as_str() {
                    Some(id) => self
                        .runs
                        .get(id)
                        .map(|r| vec![run_view(id, r)])
                        .unwrap_or_default(),
                    None => self
                        .runs
                        .iter()
                        .filter(|(_, r)| {
                            principal.is_operator()
                                || r.principal.as_deref() == Some(principal.id.as_str())
                        })
                        .map(|(id, r)| run_view(id, r))
                        .collect(),
                };
                Answer::Doc(json!({"runs": view}))
            }
            WorkflowOp::Cancel => match data["run"].as_str() {
                Some(id) if self.runs.contains_key(id) => {
                    self.cancel_run(id, "cancelled over A2A");
                    Answer::Done {
                        link: Some(Link::Run { id: id.to_string() }),
                        text: Some(format!("run {id} cancelled")),
                        result: None,
                    }
                }
                _ => Answer::Reply(err_obj(TASK_NOT_FOUND, "no such run")),
            },
            // ---- steering: redirect live work without restarting it ------
            WorkflowOp::Signal => {
                let name = data["name"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    return Answer::Reply(err_obj(
                        errors::INVALID_PARAMS,
                        "workflow.signal needs a name",
                    ));
                }
                let payload = data.get("payload").cloned().unwrap_or(Value::Null);
                let target = data["run"].as_str().map(str::to_string);
                let delivered =
                    self.deliver_signal(&name, payload, target.as_deref(), Some(&principal.id));
                Answer::Done {
                    link: None,
                    text: Some(format!("signal {name:?} delivered to {delivered}")),
                    result: Some(json!({"signal": name, "delivered": delivered})),
                }
            }
        }
    }

    /// `workflow.run {workflow, inputs?}`: the task is linked to the run it
    /// starts, and nothing is created until every refusal has had its say.
    fn workflow_run(&mut self, principal: &Principal, data: &Value, ctx: &str) -> Value {
        let Some(name) = workflow_name_of(data).map(str::to_string) else {
            return err_obj(errors::INVALID_PARAMS, "workflow.run needs {workflow}");
        };
        // A caller who may not run a workflow cannot tell it from one that
        // does not exist: the extended card hides the workflows it may not
        // run, and two different refusals would name them anyway. Only the
        // operator, whose card lists every workflow, hears that a name is
        // unknown.
        let wf = match self.workflows.get(&name) {
            Some(wf) if Runtime::may_run(principal, wf) => wf,
            None if principal.is_operator() => {
                return err_obj(
                    errors::INVALID_PARAMS,
                    &format!("no such workflow {name:?}"),
                );
            }
            _ => {
                return refusal(
                    errors::PERMISSION_DENIED,
                    reason::PERMISSION_DENIED,
                    &format!("workflow {name:?} is not runnable by {}", principal.id),
                    &[("op", "workflow.run")],
                );
            }
        };
        // Same admission gate as every other way of starting work: a
        // durable run begins with checkpoint writes, which is exactly
        // what a full disk cannot absorb. Refuse before creating the
        // task so nothing half-born needs cleanup. `priority: low`
        // workflows shed one level earlier (at warn).
        if let Some(cause) = self
            .pressure
            .refusal(wf.priority == crate::engine::model::Priority::Low)
        {
            return err_obj(rpc_internal(), &format!("shedding: {cause}"));
        }
        let run_id = format!("{}-{}", name, crate::state::ulid::new());
        let task_id = self.task_create(ctx, principal, Link::Run { id: run_id.clone() });
        let payload = json!({
            "workflow": name,
            "run_id": run_id,
            "inputs": data.get("inputs").cloned().unwrap_or_else(|| json!({})),
            "payload": {"requested_by": principal.id},
            "task": task_id,
            "conversation": ctx,
        });
        match self.accept_event(kinds::WORKFLOW_RUN, Some(principal.id.clone()), payload) {
            Ok(_) => {
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.transition(State::Working, None);
                }
                self.task_sync(&task_id);
                json!({"task": self.tasks.get(&task_id).map(Task::to_a2a).unwrap_or(Value::Null)})
            }
            Err(e) => err_obj(rpc_internal(), &e),
        }
    }

    /// `subagent.send {handle, message}`, `subagent.kill {handle}` and
    /// `subagent.status {handle?}` — the internal tool implementations, run
    /// verbatim.
    fn subagent_op(
        &mut self,
        principal: &Principal,
        op: &str,
        s: SubagentOp,
        data: &Value,
    ) -> Answer {
        let tool_caller = crate::runtime::tools::ToolCaller {
            principal: Some(principal.id.clone()),
            ..Default::default()
        };
        match self.subagent_tool(&tool_caller, op, data.clone()) {
            crate::runtime::tools::ToolOutcome::Ready(v, false) => match s {
                SubagentOp::Status => Answer::Doc(v),
                SubagentOp::Send | SubagentOp::Kill => Answer::Done {
                    link: None,
                    text: None,
                    result: Some(v),
                },
            },
            crate::runtime::tools::ToolOutcome::Ready(v, true) => Answer::Reply(err_obj(
                errors::INVALID_PARAMS,
                v.as_str().unwrap_or("subagent op failed"),
            )),
            _ => Answer::Reply(err_obj(rpc_internal(), "unexpected deferred subagent op")),
        }
    }
}

/// A compact run view for `workflow.status`.
fn run_view(id: &str, r: &crate::engine::RunState) -> Value {
    json!({"run": id, "workflow": r.workflow, "status": r.status.as_str(), "output": r.output, "error": r.error})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::surface::{OPS, command_ops_of, is_builtin_op, op_spec};

    /// The op table is complete and the dispatch agrees with it: every row
    /// is described, every op an instance serves reaches a handler arm, and
    /// nothing outside the table does. A row added without its handler — or
    /// a handler arm left behind by a deleted row — fails here rather than
    /// answering "unknown command" to a caller the card told to ask.
    #[test]
    fn every_builtin_op_is_classified_described_and_dispatched() {
        let mut names: Vec<&str> = OPS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(names.len(), n, "op names are unique");
        for spec in OPS {
            assert!(
                !spec.description.trim().is_empty(),
                "{} has no description",
                spec.name
            );
        }

        for events in [false, true] {
            for introspection in [false, true] {
                let mut s = crate::config::v2::Settings::default();
                s.a2a.events.enabled = events;
                s.a2a.introspection.enabled = introspection;
                for op in command_ops_of(&s) {
                    assert!(is_builtin_op(op), "{op} is served but no row of OPS");
                    assert!(route(op).is_some(), "{op} is served but reaches no handler");
                }
            }
        }

        // The ops a row names: the listable ones, plus the concrete members
        // of each prefix family.
        let concrete = OPS
            .iter()
            .filter(|s| !s.name.ends_with('.'))
            .map(|s| s.name)
            .chain(["_instance.result", "_instance.emit"]);
        for op in concrete {
            let spec = op_spec(op).expect("a row");
            match spec.handler {
                // Held, never served: a caller hears the op is unknown.
                Handler::Reserved => assert!(route(op).is_none(), "{op} is reserved"),
                _ => assert!(route(op).is_some(), "{op} reaches no handler arm"),
            }
        }

        // Outside the table, nothing is routed — the removed names included.
        for op in [
            "interface.info",
            "config.set",
            "pairing.code",
            "admin.lameduck",
            "review.start",
            "admin",
            "_instance.nope",
            "",
        ] {
            assert!(route(op).is_none(), "{op:?} must be an unknown op");
        }
    }
}
