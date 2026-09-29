// SPDX-License-Identifier: AGPL-3.0-only
//! `SendMessage`: a command DataPart, a declared workflow command, an answer
//! to an open human gate, or a conversation turn — and the rules every one of
//! them is held to first: which task a message may name, what its parts may
//! carry, and whether the agent is still taking work.

use super::commands::refusal;
use super::{err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::errors::{TASK_NOT_FOUND, UNSUPPORTED_OPERATION};
use crate::a2a::tasks::{Link, State};
use crate::runtime::events::kinds;
use crate::runtime::reactor::{PendingKind, Runtime};
use crate::runtime::surface::{self, Command, Ext};
use serde_json::{Value, json};

/// A command DataPart's op (`{"data": {"agentd": {"op": "<tool>", …}}}`).
pub fn command_op(message: &Value) -> Option<String> {
    message["parts"].as_array()?.iter().find_map(|p| {
        p.get("data")
            .and_then(|d| d.get("agentd"))
            .and_then(|a| a.get("op"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// The full command DataPart object (`{op, ...args}`).
pub(crate) fn command_data(message: &Value) -> Option<Value> {
    message["parts"]
        .as_array()?
        .iter()
        .find_map(|p| p.get("data").and_then(|d| d.get("agentd")).cloned())
}

/// The `contextId` a message names, as its sender knows it. a2a-rs stamps
/// one on every message it forwards, so only a path around it arrives
/// without; that one gets an id minted the way a2a-rs would have.
pub(super) fn context_wire(message: &Value) -> String {
    message["contextId"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| a2a_rs::domain::ContextId::generate().to_string())
}

/// The task a send names, when it names one.
///
/// Through the protocol layer `taskId` is always set — a2a-rs generates one
/// for a message that named none — so `newTask` says which it is (see
/// `ports::RequestScope::named_task`); anything but `true` is read as named,
/// so a malformed bridge request can never create a task under an id it did
/// not reserve. A read op the listener answers itself carries the message
/// exactly as the caller sent it.
fn named_task(params: &Value) -> Option<&str> {
    let named = match params.get("newTask") {
        Some(new) => (new.as_bool() != Some(true))
            .then(|| params["taskId"].as_str())
            .flatten(),
        None => params["message"]["taskId"].as_str(),
    };
    // Blank is unnamed, as the listener and a2a-rs both read it.
    named.filter(|t| !t.trim().is_empty())
}

/// A role as the inbox event records it and an `a2a` start's `roles:` names
/// it — the one spelling both sides of that match read.
fn role_name(role: crate::config::settings::Role) -> &'static str {
    use crate::config::settings::Role;
    match role {
        Role::Operator => "operator",
        Role::User => "user",
        Role::Agent => "agent",
        Role::Anonymous => "anonymous",
    }
}

/// The command a send carries, held to what the listener already held it
/// to: the command extension's envelope — activated, marked, one of it
/// ([`surface::check_command`]) — then the caller's reach
/// ([`Principal::authorize_command`]). `Ok(None)` is a message that carries
/// no command; `Err` is the reply that refuses it.
///
/// A pure function of the request, so the runtime's own lock can be shown
/// to hold without a listener in front of it — which is the only case it
/// exists for.
fn admitted_command(
    params: &Value,
    principal: &Principal,
    active: surface::Active,
) -> Result<Option<Command>, Value> {
    let activated = active.contains(Ext::Command);
    let command = surface::check_command(params, named_task(params), activated)
        .map_err(|e| json!({"_error": e}))?;
    if let Some(c) = &command
        && let Err(why) = principal.authorize_command(&c.op, &c.envelope)
    {
        return Err(refusal(
            errors::PERMISSION_DENIED,
            reason::PERMISSION_DENIED,
            &why,
            &[("op", &c.op)],
        ));
    }
    Ok(command)
}

impl Runtime {
    /// The key of the conversation `principal`'s `contextId` (`wire`, see
    /// [`context_wire`]) names.
    ///
    /// The one ingress rule, for a conversational message and a command
    /// alike (see `runtime::conversations`). An operator's `contextId` IS the
    /// key — root included. Anyone else's is bound in its own namespace, so it
    /// reaches only a conversation it started, and never errors on, reads or
    /// charges one it did not: another principal's id, the root's and one
    /// nobody has used all get the same fresh conversation.
    pub(super) fn resolve_context(
        &mut self,
        principal: &Principal,
        wire: &str,
    ) -> Result<String, Value> {
        if principal.is_operator() {
            return Ok(wire.to_string());
        }
        match self.conv_index.claim(&principal.id, wire) {
            Ok(key) => Ok(key),
            // No randomness, no key: refused, with nothing bound or written.
            Err(e) => {
                self.log.warn(
                    "a2a.context.unavailable",
                    json!({"principal": principal.id, "err": e.to_string()}),
                );
                Err(err_obj(
                    rpc_internal(),
                    "the conversation could not be opened; try again",
                ))
            }
        }
    }

    /// `SendMessage`/`SendStreamingMessage`: a command DataPart routes to the
    /// registry; natural language becomes a conversation turn. Either way a
    /// durable task tracks it.
    ///
    /// Before anything is created, the message is held to the rules a caller
    /// can be told about while it is still there:
    ///
    /// - a draining agent takes no new work (`-32603`, `DRAINING`) — the same
    ///   object on every path, which the listener's fidelity filter keeps
    ///   whole when a2a-rs answered;
    /// - a task id is the server's: a message that names a task continues it,
    ///   and one it cannot see is "not found" (`-32001`) rather than a new task
    ///   under the caller's id; a settled task takes no more (`-32004`);
    /// - a part that is no media type the card accepts refuses the message
    ///   (`-32005`);
    /// - a push config sent inline is checked like a registration, and
    ///   attached to the task only once the message is accepted.
    ///
    /// A command is held first to what the listener already held it to —
    /// the envelope ([`surface::check_command`]), then the caller's reach
    /// ([`Principal::authorize_command`]) — so a path into the runtime that
    /// skips the listener cannot skip either, and nothing a refused command
    /// asked for is created, queued or charged: an `_instance.*` report from
    /// anyone but the operator is a 403 here and now, never an inbox event
    /// the reactor drops later where its sender cannot see.
    pub(super) fn a2a_send(&mut self, principal: &Principal, params: &Value) -> Value {
        let message = &params["message"];
        let named = named_task(params);
        let command = match admitted_command(params, principal, self.a2a_active) {
            Ok(command) => command,
            Err(refused) => return refused,
        };
        if self.draining {
            return refusal(
                errors::INTERNAL_ERROR,
                reason::DRAINING,
                "the agent is draining",
                &[],
            );
        }
        let text = match crate::a2a::wire::message_input(message) {
            Ok(text) => text,
            Err(part) => {
                return refusal(
                    errors::CONTENT_TYPE_NOT_SUPPORTED,
                    reason::CONTENT_TYPE_NOT_SUPPORTED,
                    &part.to_string(),
                    &[],
                );
            }
        };
        if let Some(tid) = named {
            match self.tasks.get(tid) {
                Some(t) if t.is_visible_to(principal) => {
                    if t.state.is_terminal() {
                        return err_obj(
                            UNSUPPORTED_OPERATION,
                            &format!(
                                "task {tid} is {}; it accepts no further messages",
                                t.state.wire()
                            ),
                        );
                    }
                }
                // "Not yours" and "does not exist" answer alike, so no caller
                // can probe for another principal's task ids.
                _ => return err_obj(TASK_NOT_FOUND, "task not found"),
            }
        }
        let push = match params.get("push").filter(|p| !p.is_null()) {
            None => None,
            Some(config) => match self.push_enabled().and_then(|()| self.push_target(config)) {
                Ok(target) => Some(target),
                Err(e) => return e,
            },
        };
        let out = self.a2a_message(principal, message, &text, named, command.as_ref());
        // Attached at once, within the send that created (or continued) the
        // task: no other request runs on the loop between the two, and the
        // webhook is told the state the task is in now — which is where every
        // transition it would have heard about so far has led.
        if let Some(target) = push
            && out.get("_error").is_none()
            && let Some(tid) = out["task"]["id"].as_str().map(str::to_string)
        {
            self.attach_push(&tid, target, principal);
            self.push_now(&tid);
        }
        out
    }

    /// The message, accepted: routed to the registry, a gate, or a turn.
    /// `text` is what [`crate::a2a::wire::message_input`] made of its parts,
    /// `named` the task it continues — already found, visible to the caller
    /// and still open — and `command` the command it carries, already held
    /// to its envelope and the caller's reach.
    fn a2a_message(
        &mut self,
        principal: &Principal,
        message: &Value,
        text: &str,
        named: Option<&str>,
        command: Option<&Command>,
    ) -> Value {
        // The caller's message, with the id it is known by from here on: its
        // own `messageId`, or one minted for it. History records it under that
        // id, so a client finds its prompt again by the id it sent. An id in
        // the agent's namespace is minted over, so no caller can pose as one
        // of the agent's status messages in history.
        let mut message = message.clone();
        let message_id = message["messageId"]
            .as_str()
            .filter(|s| !s.is_empty() && !crate::a2a::wire::is_agent_message_id(s))
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("msg"));
        if let Some(o) = message.as_object_mut() {
            o.insert("messageId".into(), json!(message_id));
        }
        let message = &message;
        // An `a2a` START NODE registers its command. A workflow declaring
        // `{kind: a2a, command: "review.start"}` is what makes `review.start`
        // something a peer may ask for — otherwise the built-in list would be
        // the entire command surface and a start node could never be reached.
        // A registered command skips command dispatch and takes the ordinary
        // message path: its payload held to the start node's `schema` here,
        // synchronously, then written ahead to the durable inbox and matched
        // against the start nodes (roles included) by the reactor.
        //
        // A BUILT-IN always wins: only an op no row of the table holds is
        // looked up among the workflows (`spec` is `None`), so a workflow
        // declaring `status` or `admin.drain` — refused at validation — could
        // not shadow it here either, and the `_instance.*` reports a child
        // sends home are consumed by their handler, never by a model, a wait
        // or a start node.
        let declared = match command {
            Some(c) if c.spec.is_none() => {
                match self.check_declared(c, role_name(principal.role)) {
                    Ok(declared) => declared,
                    Err(e) => return json!({"_error": e}),
                }
            }
            _ => false,
        };
        if let Some(c) = command
            && !declared
        {
            return self.a2a_command(principal, c, message);
        }
        // A command DataPart carries no text, and that is not an empty message.
        if text.trim().is_empty() && !declared {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "message has no text or command part",
            );
        }
        // Continue the named task (answering an input-required gate) or start
        // a fresh conversation under the id this send reserved.
        let existing = named.and_then(|tid| {
            self.tasks
                .get(tid)
                .map(|t| (tid.to_string(), t.conversation.clone()))
        });
        // A LIVE human gate on the addressed task: the reply
        // resolves the suspended asker directly — the tool call returns the
        // text to the model, the `human` step completes with it — instead of
        // becoming a new conversation turn.
        if let Some((tid, _)) = &existing
            && let Some(i) = self
                .pending
                .iter()
                .position(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == tid))
        {
            // The ADDRESSEE, if the gate named one. Enforced here for the same
            // reason the answer schema is enforced: a gate that names a decider
            // and then accepts anyone records something that did not happen.
            //
            // An operator is not exempted silently — they are exempted VISIBLY.
            // Refusing them outright would be theatre, since an operator can
            // already rewrite the config, the store or the definition; what
            // actually matters is that the record names who really answered,
            // so an override is marked as one and audited as one.
            let addressee = match &self.pending[i].kind {
                PendingKind::Human { addressee, .. } => addressee.clone(),
                _ => None,
            };
            let mut via = "human";
            if let Some(a) = &addressee
                && !a.matches(principal)
            {
                if !principal.is_operator() {
                    let want = a.describe();
                    self.log.info(
                        "human.answer.not_addressed",
                        json!({"task": tid, "from": principal.id, "addressee": want}),
                    );
                    self.audit_a2a(
                        "SendMessage",
                        None,
                        principal,
                        "not_addressed",
                        json!({"task": tid, "addressee": want}),
                        None,
                    );
                    // The gate stays OPEN and the answerer is told why, rather
                    // than their reply vanishing into the conversation.
                    return err_obj(
                        ::mcp::rpc::INVALID_PARAMS,
                        &format!(
                            "this decision is for {want}; your answer was not recorded and the gate is still open"
                        ),
                    );
                }
                via = "operator_override";
                self.log.warn(
                    "human.answer.override",
                    json!({"task": tid, "by": principal.id, "addressee": a.describe()}),
                );
            }
            // The answer enters the gate task's history before the gate
            // settles, so the transition that follows publishes it: every
            // attached client sees who answered, and with what, on the task.
            if let Some(t) = self.tasks.get_mut(tid) {
                t.record_inbound(message);
            }
            self.human_answer(i, text, via, Some(&principal.id.clone()));
            return self.task_reply(tid);
        }
        let (task_id, ctx_id) = match existing {
            Some((tid, ctx)) => {
                if let Some(t) = self.tasks.get_mut(&tid) {
                    t.record_inbound(message);
                    t.transition(State::Working, None);
                }
                (tid, ctx)
            }
            None => {
                let ctx = match self.resolve_context(principal, &context_wire(message)) {
                    Ok(key) => key,
                    Err(e) => return e,
                };
                let tid = self.task_create(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    Some(message),
                );
                (tid, ctx)
            }
        };
        // Write-ahead the message; the loop turns it into a conversation turn.
        // `context_id` is the conversation's key; `wire_id` what the caller
        // called it, which is what a waiting step was written against.
        let wire_id = self.conversation_wire(&principal.id, &ctx_id);
        let payload = json!({"context_id": ctx_id, "wire_id": wire_id, "text": text, "parts": message["parts"],
        "task": task_id, "message_id": message_id,
        "role": role_name(principal.role)});
        match self.accept_event(kinds::A2A_MESSAGE, Some(principal.id.clone()), payload) {
            Ok(inbox_id) => {
                self.event_to_task.insert(inbox_id, task_id.clone());
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.transition(State::Working, None);
                }
                // The prompt is already in the task's history — durably, so a
                // continuation's message survives a restart with the task —
                // and this `task` event is what lets a SECOND display client
                // render the transcript a first client is driving. The reply
                // follows as the task's terminal artifact on its later `task`
                // events.
                self.task_persist(&task_id);
                self.task_sync(&task_id);
                self.task_reply(&task_id)
            }
            Err(e) => {
                self.a2a_task_fail(&task_id, &e);
                err_obj(rpc_internal(), &e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime's own lock on a command, with no listener in front of it:
    /// a command DataPart without the command extension activated is not run as one, and
    /// a child's `_instance.*` report from anyone but the operator is a 403
    /// now — whatever a grant says — rather than an inbox event dropped later
    /// where its sender cannot see. The listener refuses both first, so no
    /// request through it can show this lock missing.
    #[test]
    fn the_runtime_holds_a_command_itself() {
        use crate::config::settings::Role;
        let who = |role| Principal {
            role,
            grants: vec!["*".into()],
            ..Principal::anonymous()
        };
        let send = |envelope: Value| {
            json!({"newTask": true, "taskId": "t-1", "message": {
                "role": "ROLE_USER", "messageId": "m-1",
                "extensions": [surface::COMMAND_EXTENSION],
                "parts": [{"data": {"agentd": envelope}, "mediaType": "application/json"}],
            }})
        };
        // The code, and the ErrorInfo's reason wherever among the details
        // it sits.
        let reason_of = |e: &Value| {
            let reason = e["_error"]["data"]
                .as_array()
                .and_then(|d| d.iter().find_map(|x| x["reason"].as_str()))
                .unwrap_or_default()
                .to_string();
            (e["_error"]["code"].as_i64().unwrap_or_default(), reason)
        };
        let on = surface::Active::of(&[Ext::Command]);
        let status = send(json!({"op": "status"}));

        // Not activated: refused, for the operator too.
        let e = admitted_command(&status, &who(Role::Operator), surface::Active::NONE).unwrap_err();
        assert_eq!(
            reason_of(&e),
            (
                errors::INVALID_PARAMS,
                reason::EXTENSION_NOT_ACTIVATED.to_string()
            ),
            "{e}"
        );
        let c = admitted_command(&status, &who(Role::Operator), on).unwrap();
        assert_eq!(c.map(|c| c.op).as_deref(), Some("status"));

        // A child's report is the operator's alone, `*` grants or not.
        let report = send(
            json!({"op": "_instance.result", "handle": "c1", "status": "completed", "output": "ok"}),
        );
        for role in [Role::User, Role::Agent, Role::Anonymous] {
            let e = admitted_command(&report, &who(role), on).unwrap_err();
            assert_eq!(
                reason_of(&e),
                (
                    errors::PERMISSION_DENIED,
                    reason::PERMISSION_DENIED.to_string()
                ),
                "{role:?}: {e}"
            );
        }
        let c = admitted_command(&report, &who(Role::Operator), on).unwrap();
        assert_eq!(c.map(|c| c.op).as_deref(), Some("_instance.result"));

        // Plain conversation carries no command, activated or not.
        let text = json!({"newTask": true, "taskId": "t-1", "message": {
            "role": "ROLE_USER", "messageId": "m-2", "parts": [{"text": "hello"}]}});
        for active in [surface::Active::NONE, on] {
            assert!(
                admitted_command(&text, &who(Role::User), active)
                    .unwrap()
                    .is_none()
            );
        }
    }

    /// The built-in command surface is RESERVED, in both directions.
    ///
    /// A workflow's `a2a` start declaring one is refused at validation, and the
    /// listener dispatches a built-in to its own handler regardless — because a
    /// declared command takes the inbox path, where the per-op authorization
    /// the built-ins carry does not run. Before `admin.*` existed this could
    /// only shadow a read like `status`; now it could shadow an operator's
    /// drain control with a run that anyone the start node admits may trigger,
    /// and where the model may create workflows, the author is the model.
    #[test]
    fn a_workflow_cannot_claim_a_built_in_command_name() {
        use crate::runtime::surface::is_builtin_op;
        for op in [
            "status",
            "config",
            "workflow.run",
            "subagent.kill",
            "admin.drain",
            "admin.pause",
            "admin.set",
            "ask_human",
            "_instance.result",
        ] {
            assert!(is_builtin_op(op), "{op} is reserved");
        }
        for op in [
            "review.start",
            "order.paid",
            "admin",
            "admin.custom",
            "drain",
        ] {
            assert!(!is_builtin_op(op), "{op} is a workflow's to claim");
        }

        // The validator refuses the collision, naming it.
        let wf = serde_json::json!({
            "name": "shadow", "steps": {
                "s": {"kind": "a2a", "command": "admin.drain"},
                "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}
            }
        });
        let errs = crate::engine::model::parse_workflow(&wf)
            .err()
            .unwrap_or_default();
        assert!(
            errs.iter()
                .any(|e| e.contains("admin.drain") && e.contains("built-in")),
            "the refusal names the collision: {errs:?}"
        );

        // …and a name of the workflow's own still loads.
        let ok = serde_json::json!({
            "name": "fine", "steps": {
                "s": {"kind": "a2a", "command": "review.start"},
                "f": {"kind": "finish", "depends_on": ["s"], "status": "completed"}
            }
        });
        assert!(
            crate::engine::model::parse_workflow(&ok).is_ok(),
            "{:?}",
            crate::engine::model::parse_workflow(&ok).err()
        );
    }

    /// Which task a send names. Through the protocol layer `taskId` is always
    /// set, and only `newTask: true` makes it a reservation rather than a
    /// name — anything else continues a task that must exist, so a bridge
    /// request that says less than it should can never create a task under an
    /// id nobody reserved. A read the listener answers carries the caller's
    /// message as it was sent.
    #[test]
    fn the_task_a_send_names() {
        let msg = json!({"taskId": "t-msg"});
        let cases = [
            (
                json!({"message": msg, "taskId": "t-gen", "newTask": true}),
                None,
            ),
            (
                json!({"message": msg, "taskId": "t-named", "newTask": false}),
                Some("t-named"),
            ),
            (
                json!({"message": msg, "taskId": "t-odd", "newTask": "yes"}),
                Some("t-odd"),
            ),
            (json!({"message": msg}), Some("t-msg")),
            (json!({"message": {"taskId": ""}}), None),
            (json!({"message": {}}), None),
        ];
        for (params, want) in cases {
            assert_eq!(named_task(&params), want, "{params}");
        }
    }

    #[test]
    fn command_and_text_extraction() {
        let m = json!({"parts": [{"text": "please"}, {"data": {"agentd": {"op": "workflow.run", "name": "x"}}}]});
        assert_eq!(command_op(&m), Some("workflow.run".to_string()));
        assert_eq!(command_data(&m).unwrap()["name"], "x");
        assert_eq!(command_op(&json!({"parts": [{"text": "hi"}]})), None);
    }
}
