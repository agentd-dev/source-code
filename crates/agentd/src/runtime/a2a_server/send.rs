// SPDX-License-Identifier: AGPL-3.0-only
//! `SendMessage`: a command DataPart, a declared workflow command, an answer
//! to an open human gate, or a conversation turn — and the rules every one of
//! them is held to first: which task a message may name, what its parts may
//! carry, and whether the agent is still taking work.

use super::commands::refusal;
use super::{TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::tasks::{Link, State};
use crate::runtime::events::kinds;
use crate::runtime::reactor::{PendingKind, Runtime};
use crate::runtime::surface;
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

/// The refusal of a command that names a task, as a JSON-RPC error object.
///
/// A command starts its own task. One that names a task would be a command run
/// inside somebody's conversation, or a caller choosing the id of the task the
/// command creates — and a task id is the server's to mint. Shared by the
/// listener, which refuses it before anything else happens, and the runtime,
/// which refuses it again for whatever reaches it another way.
pub(crate) fn command_names_task(op: &str, task: &str) -> Value {
    json!({
        "code": errors::INVALID_PARAMS,
        "message": format!("command {op:?} starts its own task; it cannot name one (taskId {task:?})"),
        "data": [
            errors::bad_request(&[("message.taskId", "a command message carries no taskId")]),
            errors::error_info(errors::AGENTD_DOMAIN, reason::COMMAND_TASK_ID, &[("op", op)]),
        ],
    })
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
    ///   under the caller's id; a settled task takes no more (`-32004`); a
    ///   command names none (`-32602`);
    /// - a part that is no media type the card accepts refuses the message
    ///   (`-32005`);
    /// - a push config sent inline is checked like a registration, and
    ///   attached to the task only once the message is accepted.
    pub(super) fn a2a_send(&mut self, principal: &Principal, params: &Value) -> Value {
        if self.draining {
            return refusal(
                errors::INTERNAL_ERROR,
                reason::DRAINING,
                "the agent is draining",
                &[],
            );
        }
        let message = &params["message"];
        let named = named_task(params);
        if let (Some(op), Some(task)) = (command_op(message), named) {
            return json!({"_error": command_names_task(&op, task)});
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
        let out = self.a2a_message(principal, message, &text, named);
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
    /// and `named` the task it continues — already found, visible to the
    /// caller and still open.
    fn a2a_message(
        &mut self,
        principal: &Principal,
        message: &Value,
        text: &str,
        named: Option<&str>,
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
        // the entire command surface and a start node could never be reached,
        // because an unknown op is refused before the message ever becomes an
        // inbox event. A registered command therefore skips command dispatch
        // and takes the ordinary message path: written ahead to the durable
        // inbox, then matched against the start nodes (roles included) by the
        // reactor. A built-in wins, so a workflow cannot shadow `status` — and
        // the `_instance.*` reports a child sends home are built-ins too, so
        // they are consumed by their handler and never reach a model, a wait
        // or a start node.
        // A BUILT-IN always wins, which the paragraph above has always claimed
        // and the code did not do: `declared` was checked first, so a workflow
        // declaring `{kind: a2a, command: "status"}` took the inbox path and
        // skipped `a2a_command` — and with it `may_command`. Harmless for a
        // read; not harmless once `admin.*` joined the built-in surface, where
        // a declared collision would shadow an operator's drain control with a
        // workflow anyone the start node admits could trigger. Declaring one is
        // refused at validation; this is the second lock.
        let builtin = command_op(message).is_some_and(|op| surface::is_builtin_op(&op));
        let declared = !builtin
            && command_op(message).is_some_and(|op| self.workflow_declares_a2a_command(&op));
        // A declared command with a `schema:` is a CONTRACT: a payload that
        // does not match is refused HERE, synchronously, with the mismatch —
        // not accepted into the inbox to fail later where the caller cannot
        // see it. This is what makes cross-agent commands as typed as tool
        // calls.
        if declared
            && let Some(op) = command_op(message)
            && let Some(schema) = self.a2a_command_schema(&op)
        {
            let mut payload = command_data(message).unwrap_or_else(|| json!({}));
            if let Some(o) = payload.as_object_mut() {
                o.remove("op");
            }
            if let Err(errs) = crate::jsonschema::validate(&schema, &payload) {
                return err_obj(
                    ::mcp::rpc::INVALID_PARAMS,
                    &format!(
                        "command {op:?} payload does not match its declared schema: {}",
                        errs.join("; ")
                    ),
                );
            }
        }
        if !declared && let Some(op) = command_op(message) {
            return self.a2a_command(principal, &op, message);
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
        let wire_id = self.conversation_wire(&ctx_id);
        let payload = json!({"context_id": ctx_id, "wire_id": wire_id, "text": text, "parts": message["parts"],
        "task": task_id, "message_id": message_id,
        "role": match principal.role {
            crate::config::v2::Role::Operator => "operator",
            crate::config::v2::Role::User => "user",
            crate::config::v2::Role::Agent => "agent",
            crate::config::v2::Role::Anonymous => "anonymous",
        }});
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

    /// Whether any loaded workflow has an `a2a` start node declaring `op` as its
    /// command. This is what turns a start node into a registered part of the
    /// A2A command surface (see the call site in `a2a_send`).
    fn workflow_declares_a2a_command(&self, op: &str) -> bool {
        self.workflows.values().any(|w| {
            w.start_steps().into_iter().any(|s| {
                s.kind == "a2a" && s.spec.get("command").and_then(Value::as_str) == Some(op)
            })
        })
    }

    /// The declared `schema` of a registered command's `a2a` start, if any.
    fn a2a_command_schema(&self, op: &str) -> Option<Value> {
        self.workflows.values().find_map(|w| {
            w.start_steps().into_iter().find_map(|s| {
                (s.kind == "a2a" && s.spec.get("command").and_then(Value::as_str) == Some(op))
                    .then(|| s.spec.get("schema").cloned())
                    .flatten()
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "name": "shadow", "version": 3,
            "steps": {
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
            "name": "fine", "version": 3,
            "steps": {
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

    /// A command naming a task is refused in the shape the listener and the
    /// runtime share: the field, and the reason.
    #[test]
    fn a_command_naming_a_task_is_refused_with_its_field() {
        let e = command_names_task("workflow.run", "t-1");
        assert_eq!(e["code"], errors::INVALID_PARAMS);
        assert_eq!(
            e["data"][0]["fieldViolations"][0]["field"],
            "message.taskId"
        );
        assert_eq!(e["data"][1]["reason"], reason::COMMAND_TASK_ID);
        assert_eq!(e["data"][1]["domain"], errors::AGENTD_DOMAIN);
    }

    #[test]
    fn command_and_text_extraction() {
        let m = json!({"parts": [{"text": "please"}, {"data": {"agentd": {"op": "workflow.run", "name": "x"}}}]});
        assert_eq!(command_op(&m), Some("workflow.run".to_string()));
        assert_eq!(command_data(&m).unwrap()["name"], "x");
        assert_eq!(command_op(&json!({"parts": [{"text": "hi"}]})), None);
    }
}
