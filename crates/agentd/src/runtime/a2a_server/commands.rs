// SPDX-License-Identifier: AGPL-3.0-only
//! The built-in command ops, reached as a command DataPart on `SendMessage`.
//!
//! One table ([`surface::OPS`]) says what each op is; this file routes an op
//! to its handler with ONE match on the row's [`Handler`], and turns what the
//! handler produced into the reply the row's [`Reply`] promises — a Message
//! for a read, a Task for work. A new op is a row plus its handler's arm.

use super::redact::redact_settings;
use super::{TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::principals::workflow_name_of;
use crate::a2a::tasks::{Link, State};
use crate::engine::model::{Step, Workflow};
use crate::runtime::events::kinds;
use crate::runtime::reactor::{Runtime, may_act_on};
use crate::runtime::surface::{self, Command, Gate, Handler, Reply};
use crate::runtime::waits::SignalSender;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

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
///
/// Where the dispatch holds the command, [`unknown_command`] names the part
/// it came in; this is for a handler that has only the op.
pub(super) fn unknown_op(op: &str) -> Value {
    json!({"_error": surface::unknown_op_error(op, "message.parts[*].data.agentd.op")})
}

/// [`unknown_op`] for the command `c`, naming the field it came in.
fn unknown_command(c: &Command) -> Value {
    let field = format!("message.parts[{}].data.agentd.op", c.index);
    json!({"_error": surface::unknown_op_error(&c.op, &field)})
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
    /// A command addressed to a built-in op — or to no op at all: whatever
    /// no workflow declares comes here to be refused. The command is already
    /// held to its envelope and the caller's reach (`a2a_send`); what is left
    /// is the switch that serves it, and its handler.
    pub(super) fn a2a_command(
        &mut self,
        principal: &Principal,
        command: &Command,
        message: &Value,
    ) -> Value {
        let op = command.op.as_str();
        let Some((spec, route)) = route(op) else {
            return unknown_command(command);
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
                _ => unknown_command(command),
            };
        }
        let data = &command.envelope;
        // The same namespace a conversational message is held to: a command
        // runs in a conversation its caller may address, claimed before any
        // work is done. An op that answers with a Message starts nothing in
        // any conversation — `status`, `workflow.status`, `subagent.status`,
        // the sign-in listings — so it binds nothing: asked of the row, not of
        // the handler family, because a poll with no `contextId` gets a fresh
        // one every time, and each would otherwise be a binding kept for good.
        // Its reply names the conversation the caller's way.
        let wire = super::send::context_wire(message);
        let unbound =
            !principal.is_operator() && self.conv_index.key_of(&principal.id, &wire).is_none();
        let ctx = if spec.reply == Reply::Message {
            wire.clone()
        } else {
            match self.resolve_context(principal, &wire) {
                Ok(key) => key,
                Err(e) => return e,
            }
        };
        let answer = match route {
            Route::Read(r) => self.read_op(principal, r, data),
            Route::Workflow(w) => self.workflow_op(principal, w, data, &ctx, message),
            Route::Subagent(s) => self.subagent_op(principal, op, s, data),
            Route::Admin(a) => match self.a2a_admin(principal, a, data) {
                Ok((text, result)) => Answer::Done {
                    link: None,
                    text: Some(text),
                    result: Some(result),
                },
                Err(e) => Answer::Reply(e),
            },
            Route::Introspection(i) => match self.introspection_op(principal, i, data) {
                Ok(doc) => Answer::Doc(doc),
                Err(e) => Answer::Reply(e),
            },
            Route::Auth => match super::auth_ops::handle(self, principal, op, data) {
                Ok(super::auth_ops::AuthAnswer::Doc(doc)) => Answer::Doc(doc),
                Ok(super::auth_ops::AuthAnswer::Done(text, result)) => Answer::Done {
                    link: None,
                    text: Some(text),
                    result: Some(result),
                },
                Err(e) => Answer::Reply(e),
            },
            Route::Instance(i) => match self.instance_op(principal, i, data) {
                Ok(text) => Answer::Done {
                    link: None,
                    text: Some(text),
                    result: None,
                },
                Err(e) => Answer::Reply(err_obj(errors::INVALID_PARAMS, &e)),
            },
        };
        // No feed frame of its own: the task this creates carries the command
        // message in its history, and its `task` frame is what a display
        // client sees.
        match answer {
            Answer::Doc(doc) => {
                debug_assert_eq!(spec.reply, Reply::Message, "{op} answered a document");
                crate::a2a::reply::read_reply(&wire, doc)
            }
            Answer::Done { link, text, result } => {
                debug_assert_eq!(spec.reply, Reply::Task, "{op} answered a task");
                self.task_complete_now(
                    &ctx,
                    principal,
                    link.unwrap_or(Link::Turn { ctx: ctx.clone() }),
                    message,
                    text,
                    result,
                )
            }
            Answer::Reply(v) => {
                // A task op its handler refused (an unknown workflow, a role,
                // shedding) created nothing that records the binding it was
                // given, so nothing would ever release it either: the claim is
                // dropped again when it was this request's own. One the task a
                // handler did create (`workflow.run`) keeps it.
                if unbound && spec.reply == Reply::Task {
                    self.release_unused_conversation(&ctx);
                }
                v
            }
        }
    }

    /// `status`, `config` and `plan.get`.
    fn read_op(&mut self, principal: &Principal, op: ReadOp, data: &Value) -> Answer {
        match op {
            // Granted to every named caller, so scoped to the caller: its own
            // work, and the instance's facts.
            ReadOp::Status => Answer::Doc(self.status_value_for(principal)),
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
                // A non-operator names a conversation the way it sent it, in
                // its own namespace; only an operator uses the runtime's keys.
                let key = self.conversation_named(principal, &id);
                match key.and_then(|k| self.contexts.get(&k)) {
                    Some(c) if may_act_on(principal, c.principal.as_deref()) => Answer::Doc(
                        json!({"conversation": id, "plan": c.plan, "progress": c.plan.as_ref().map(|p| p.progress())}),
                    ),
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
        message: &Value,
    ) -> Answer {
        match op {
            WorkflowOp::Run => Answer::Reply(self.workflow_run(principal, data, ctx, message)),
            WorkflowOp::Status => {
                let view: Vec<Value> = match data["run"].as_str() {
                    Some(id) => match self.owned_run(principal, id) {
                        Some(r) => vec![run_view(id, r)],
                        None => return no_such_run(),
                    },
                    None => self
                        .runs
                        .iter()
                        .filter(|(_, r)| may_act_on(principal, r.principal.as_deref()))
                        .map(|(id, r)| run_view(id, r))
                        .collect(),
                };
                Answer::Doc(json!({"runs": view}))
            }
            WorkflowOp::Cancel => match data["run"].as_str() {
                Some(id) if self.owned_run(principal, id).is_some() => {
                    self.cancel_run(id, "cancelled over A2A");
                    Answer::Done {
                        link: Some(Link::Run { id: id.to_string() }),
                        text: Some(format!("run {id} cancelled")),
                        result: None,
                    }
                }
                _ => no_such_run(),
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
                if let Some(id) = &target
                    && self.owned_run(principal, id).is_none()
                {
                    return no_such_run();
                }
                // Without a target the signal still reaches only what this
                // caller owns: `deliver_signal` scopes it to the sender.
                let sender = SignalSender::Principal {
                    id: principal.id.clone(),
                    operator: principal.is_operator(),
                };
                let delivered = self.deliver_signal(
                    &name,
                    payload,
                    target.as_deref(),
                    Some(&principal.id),
                    &sender,
                );
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
    fn workflow_run(
        &mut self,
        principal: &Principal,
        data: &Value,
        ctx: &str,
        message: &Value,
    ) -> Value {
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
        let task_id = self.task_create(
            ctx,
            principal,
            Link::Run { id: run_id.clone() },
            Some(message),
        );
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
                self.task_reply(&task_id)
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
        // Ownership first, before the tool can say "not running" or "not a
        // warm subagent" about a handle that is someone else's.
        let handle = data["handle"].as_str().unwrap_or("");
        if self.owned_subagent(principal, handle).is_none() {
            return Answer::Reply(err_obj(TASK_NOT_FOUND, "no such subagent"));
        }
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

impl Runtime {
    /// Whether `command` — an op no row of the table holds — is one a loaded
    /// workflow declares, held to that declaration, for a caller of `role`.
    ///
    /// A declared command with a `schema:` is a CONTRACT: a payload that does
    /// not match is refused HERE, synchronously, with every miss named as the
    /// field it is (`INVALID_COMMAND_ARGS`, as a built-in's arguments are) —
    /// not accepted into the inbox to fail later where the caller cannot see
    /// it. This is what makes cross-agent commands as typed as tool calls.
    /// `Ok(false)` is an op nobody declares, for [`Self::a2a_command`] to
    /// refuse as unknown.
    pub(super) fn check_declared(&self, command: &Command, role: &str) -> Result<bool, Value> {
        let Some(schema) = declared_schema(&self.workflows, &command.op, role) else {
            return Ok(false);
        };
        if let Some(schema) = schema {
            let mut payload = command.envelope.clone();
            if let Some(o) = payload.as_object_mut() {
                o.remove("op");
            }
            if let Err(errs) = crate::jsonschema::validate(schema, &payload) {
                return Err(surface::invalid_args(&command.op, command.index, &errs));
            }
        }
        Ok(true)
    }
}

/// The `a2a` start node a message fires: the first, in workflow order, whose
/// `command` is the message's `op` — or that names none, and so takes any
/// message — and whose `roles` admit a caller of `role`. The reactor fires
/// it and [`declared_schema`] holds the payload to its contract: one
/// selection, so the schema checked is the schema of the node that runs.
pub(crate) fn a2a_start_node<'a>(
    workflows: &'a BTreeMap<String, Arc<Workflow>>,
    op: Option<&str>,
    role: &str,
) -> Option<(&'a Workflow, &'a Step)> {
    workflows.values().find_map(|w| {
        w.start_steps()
            .into_iter()
            .find(|s| {
                s.kind == "a2a"
                    && s.spec
                        .get("command")
                        .and_then(Value::as_str)
                        .is_none_or(|want| Some(want) == op)
                    && s.spec
                        .get("roles")
                        .and_then(Value::as_array)
                        .is_none_or(|roles| {
                            roles.is_empty() || roles.iter().any(|r| r.as_str() == Some(role))
                        })
            })
            .map(|s| (w.as_ref(), s))
    })
}

/// The contract a declared command `op` from a caller of `role` is held to:
/// `None` when no `a2a` start node declares `op` (it is no command of any
/// workflow's), else `Some` of the schema its payload must match, if any.
///
/// The schema is the one of the node that will run the payload. Two
/// workflows may declare one command — say an operator-only path and an open
/// one — and checking whichever declared it first let a caller that fires the
/// second pass it a payload its own `schema:` refuses. A node that types
/// nothing itself — a catch-all, or a declaration without a schema — is held
/// to the first schema the command is declared with, as before: a command is
/// typed once, wherever it is typed.
fn declared_schema<'a>(
    workflows: &'a BTreeMap<String, Arc<Workflow>>,
    op: &str,
    role: &str,
) -> Option<Option<&'a Value>> {
    let declaring = || {
        workflows.values().flat_map(|w| {
            w.start_steps().into_iter().filter(move |s| {
                s.kind == "a2a" && s.spec.get("command").and_then(Value::as_str) == Some(op)
            })
        })
    };
    declaring().next()?;
    let fired = a2a_start_node(workflows, Some(op), role).and_then(|(_, s)| s.spec.get("schema"));
    Some(fired.or_else(|| declaring().find_map(|s| s.spec.get("schema"))))
}

/// The answer to a run the caller may not see — unknown or someone else's,
/// told apart for nobody.
fn no_such_run() -> Answer {
    Answer::Reply(err_obj(TASK_NOT_FOUND, "no such run"))
}

/// A compact run view for `workflow.status`.
fn run_view(id: &str, r: &crate::engine::RunState) -> Value {
    json!({"run": id, "workflow": r.workflow, "status": r.status.as_str(), "output": r.output, "error": r.error})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::surface::{INSTANCE_OPS, OPS, command_ops_of, is_builtin_op, op_spec};

    /// A declared command's payload is held to the schema of the start node
    /// that will run it — the one selection the reactor fires by — not to
    /// whichever workflow declared the command first. Two workflows may
    /// declare one command; checking the first let a caller who fires the
    /// second hand it a payload its own `schema:` refuses.
    #[test]
    fn a_declared_command_is_held_to_the_node_that_runs_it() {
        let wf = |name: &str, start: Value| {
            Arc::new(
                crate::engine::model::parse_workflow(&json!({"name": name, "steps": {
                    "s": start,
                    "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}}}))
                .unwrap(),
            )
        };
        let only = |field: &str| {
            json!({"type": "object", "required": [field], "additionalProperties": false,
                   "properties": {field: {"type": "integer"}}})
        };
        let (for_ops, for_all) = (only("n"), only("m"));
        let fired = |w: &BTreeMap<String, Arc<Workflow>>, role| {
            a2a_start_node(w, Some("x"), role).map(|(w, _)| w.name.clone())
        };

        // `a` sorts first and admits the operator only; `b` admits anyone.
        let mut workflows = BTreeMap::new();
        workflows.insert(
            "a".to_string(),
            wf(
                "a",
                json!({"kind": "a2a", "command": "x", "roles": ["operator"], "schema": for_ops}),
            ),
        );
        workflows.insert(
            "b".to_string(),
            wf(
                "b",
                json!({"kind": "a2a", "command": "x", "schema": for_all}),
            ),
        );
        assert_eq!(fired(&workflows, "user").as_deref(), Some("b"));
        assert_eq!(
            declared_schema(&workflows, "x", "user"),
            Some(Some(&for_all))
        );
        assert_eq!(fired(&workflows, "operator").as_deref(), Some("a"));
        assert_eq!(
            declared_schema(&workflows, "x", "operator"),
            Some(Some(&for_ops))
        );

        // The node that runs types nothing itself: the command is still
        // typed, by the schema it is declared with.
        workflows.insert(
            "a".to_string(),
            wf(
                "a",
                json!({"kind": "a2a", "command": "x", "roles": ["operator"]}),
            ),
        );
        assert_eq!(fired(&workflows, "operator").as_deref(), Some("a"));
        assert_eq!(
            declared_schema(&workflows, "x", "operator"),
            Some(Some(&for_all))
        );
        assert_eq!(
            declared_schema(&workflows, "x", "user"),
            Some(Some(&for_all))
        );

        // Nobody declares `y`; a declaration without a schema is untyped.
        assert_eq!(declared_schema(&workflows, "y", "user"), None);
        workflows.remove("b");
        assert_eq!(declared_schema(&workflows, "x", "operator"), Some(None));
        // A role the only declaring node refuses fires nothing — and the
        // command is still one a workflow declares.
        assert_eq!(fired(&workflows, "user"), None);
        assert_eq!(declared_schema(&workflows, "x", "user"), Some(None));
    }

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
        // of each prefix family — the members the table publishes, which are
        // exactly the ones the instance handler knows.
        let concrete = OPS
            .iter()
            .filter(|s| !s.name.ends_with('.'))
            .map(|s| s.name)
            .chain(INSTANCE_OPS.iter().map(|m| m.name));
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
