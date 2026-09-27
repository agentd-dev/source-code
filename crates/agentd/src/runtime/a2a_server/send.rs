// SPDX-License-Identifier: AGPL-3.0-only
//! `SendMessage`: a command DataPart, a declared workflow command, an answer
//! to an open human gate, or a conversation turn.

use super::{FeedVis, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::tasks::{Link, State, Task};
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

/// The concatenated text of a message's text parts.
fn message_text(message: &Value) -> String {
    message["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

impl Runtime {
    /// `SendMessage`/`SendStreamingMessage`: a command DataPart routes to the
    /// registry; natural language becomes a conversation turn. Either way a
    /// durable task tracks it.
    pub(super) fn a2a_send(&mut self, principal: &Principal, params: &Value) -> Value {
        if self.draining {
            return err_obj(-32000, "the agent is draining");
        }
        let message = &params["message"];
        // An `a2a` START NODE registers its command. A workflow declaring
        // `{kind: a2a, command: "review.start"}` is what makes `review.start`
        // something a peer may ask for — otherwise the built-in list would be
        // the entire command surface and a start node could never be reached,
        // because an unknown op is refused before the message ever becomes an
        // inbox event. A registered command therefore skips command dispatch
        // and takes the ordinary message path: written ahead to the durable
        // inbox, then matched against the start nodes (roles included) by the
        // reactor. A built-in wins, so a workflow cannot shadow `status`.
        // `_instance.*` ops are the runtime's own children reporting home
        // (sync results, mirrored stream events). They take the inbox path
        // like a declared command — the REACTOR consumes them
        // before start matching; they never reach a model or a workflow.
        let internal_op = command_op(message).is_some_and(|op| op.starts_with("_instance."));
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
            && (internal_op
                || command_op(message).is_some_and(|op| self.workflow_declares_a2a_command(&op)));
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
        let text = message_text(message);
        // A command DataPart carries no text, and that is not an empty message.
        if text.trim().is_empty() && !declared {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "message has no text or command part",
            );
        }
        let message_id = message["messageId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("msg"));
        // Continue an existing task (answering an input-required gate) or start
        // a fresh conversation. An id for a task that does not exist yet is the
        // listener's reservation, and `task_create` takes it.
        let existing = message["taskId"].as_str().and_then(|tid| {
            self.tasks
                .get(tid)
                .map(|t| (tid.to_string(), t.context_id.clone(), t.principal.clone()))
        });
        // A LIVE human gate on the addressed task: the reply
        // resolves the suspended asker directly — the tool call returns the
        // text to the model, the `human` step completes with it — instead of
        // becoming a new conversation turn.
        if let Some((tid, ctx, owner)) = &existing
            && (owner.as_deref() == Some(principal.id.as_str()) || principal.is_operator())
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
            // Every attached client sees the answer (the cross-client transcript).
            self.feed_push(
                "message",
                FeedVis::Owner(Some(principal.id.clone())),
                json!({"contextId": ctx, "taskId": tid, "messageId": message_id, "principal": principal.id, "text": text}),
            );
            self.human_answer(i, &text, via, Some(&principal.id.clone()));
            return json!({"task": self.tasks.get(tid).map(Task::to_a2a).unwrap_or(Value::Null)});
        }
        let (task_id, ctx_id) = match existing {
            Some((tid, ctx, owner))
                if owner.as_deref() == Some(principal.id.as_str()) || principal.is_operator() =>
            {
                if let Some(t) = self.tasks.get_mut(&tid) {
                    t.transition(State::Working, None);
                }
                (tid, ctx)
            }
            _ => {
                let ctx = message["contextId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| self.next_id("a2a"));
                let tid = self.task_create(&ctx, principal, Link::Turn { ctx: ctx.clone() });
                (tid, ctx)
            }
        };
        // Write-ahead the message; the loop turns it into a conversation turn.
        let payload = json!({"context_id": ctx_id, "text": text, "parts": message["parts"],
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
                self.task_sync(&task_id);
                // Surface the prompt on the interface feed: this
                // is what lets a SECOND display client render the transcript a
                // first client is driving — the reply follows as the task's
                // terminal artifact on its `task` events.
                self.feed_push(
                    "message",
                    FeedVis::Owner(Some(principal.id.clone())),
                    json!({"contextId": ctx_id, "taskId": task_id, "messageId": message_id, "principal": principal.id, "text": text}),
                );
                json!({"task": self.tasks.get(&task_id).map(Task::to_a2a).unwrap_or(Value::Null)})
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
            "interface.info",
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

    #[test]
    fn command_and_text_extraction() {
        let m = json!({"parts": [{"text": "please"}, {"data": {"agentd": {"op": "workflow.run", "name": "x"}}}]});
        assert_eq!(command_op(&m), Some("workflow.run".to_string()));
        assert_eq!(command_data(&m).unwrap()["name"], "x");
        assert_eq!(
            message_text(&json!({"parts": [{"text": "a"}, {"text": "b"}]})),
            "a\nb"
        );
        assert_eq!(command_op(&json!({"parts": [{"text": "hi"}]})), None);
    }
}
