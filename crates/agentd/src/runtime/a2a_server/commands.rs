// SPDX-License-Identifier: AGPL-3.0-only
//! The built-in command ops, reached as a command DataPart on `SendMessage`.

use super::redact::redact_settings;
use super::send::command_data;
use super::{FeedVis, TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::a2a::tasks::{Link, State, Task};
use crate::runtime::events::kinds;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};

impl Runtime {
    /// A command DataPart. The synchronous subset completes at once;
    /// `workflow.run` links its task to the run it starts. The `interface.*`
    /// and debug reads are **taskless** — pure reads that create no durable
    /// task, so a display client can poll them without filling the task store.
    pub(super) fn a2a_command(
        &mut self,
        principal: &Principal,
        op: &str,
        message: &Value,
    ) -> Value {
        if !principal.may_command(op) {
            return err_obj(
                -32003,
                &format!("command {op:?} not granted to {}", principal.id),
            );
        }
        let data = command_data(message).unwrap_or_else(|| json!({}));
        // The taskless interface reads and controls: answered inline, before
        // any task is created.
        match op {
            "interface.info" => return self.interface_info(),
            "conversation.get" => return self.interface_conversation_get(principal, &data),
            "run.get" => return self.interface_run_get(principal, &data),
            "subagent.get" => return self.interface_subagent_get(&data),
            "debug.events" => return self.interface_debug_events(&data),
            "pairing.code" => return self.interface_pairing_code(),
            "config.set" => return self.interface_config_set(&data),
            _ => {}
        }
        let ctx = message["contextId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("a2a"));
        // Surface MUTATING commands on the interface feed so every attached
        // display client sees what its peers asked for. Read ops
        // (`status`, `config`, `workflow.status`) stay off the feed — they are
        // the observation plumbing itself, and N clients polling them would
        // spam every transcript.
        if matches!(
            op,
            "workflow.run"
                | "workflow.cancel"
                | "workflow.signal"
                | "subagent.send"
                | "subagent.kill"
        ) {
            self.feed_push(
                "command",
                FeedVis::Owner(Some(principal.id.clone())),
                json!({"op": op, "principal": principal.id, "contextId": ctx}),
            );
        }
        // The admin family, reached the A2A way: `SendMessage` with a command
        // DataPart, like every other op. The result is a completed Task, which
        // is the protocol's model for work — a stock client can call these
        // without knowing a single agentd-specific method.
        if crate::a2a::principals::is_admin_op(op) {
            let body = self.a2a_admin(principal, op, &data);
            let text = body
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| op.to_string());
            return self.task_complete_now(
                &ctx,
                principal,
                Link::Turn { ctx: ctx.clone() },
                State::Completed,
                Some(text),
                Some(body),
            );
        }
        match op {
            "status" => {
                let s = self.status_value();
                let text = format!(
                    "{} runs, {} subagents, {} conversations; budget active: {}",
                    s["runs"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["subagents"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["conversations"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["budget"]["active"]
                );
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    Some(text),
                    Some(s),
                )
            }
            // The effective merged configuration (`agent://config/effective`) —
            // operator-only (via `may_command`). Redacted on the way out: the
            // merged doc carries env/flag-supplied credentials INLINE, and
            // operator-only is not the same as public (see `redact_settings`).
            // What survives is the `{{secret:…}}` reference, never a value.
            "config" => self.task_complete_now(
                &ctx,
                principal,
                Link::Turn { ctx: ctx.clone() },
                State::Completed,
                Some("effective configuration".into()),
                Some(json!({"config": redact_settings(&self.settings_doc)})),
            ),
            "workflow.run" => {
                let name = data["name"]
                    .as_str()
                    .or_else(|| data["workflow"].as_str())
                    .unwrap_or("")
                    .to_string();
                let Some(wf) = self.workflows.get(&name) else {
                    return err_obj(
                        ::mcp::rpc::INVALID_PARAMS,
                        &format!("no such workflow {name:?}"),
                    );
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
                let task_id = self.task_create(&ctx, principal, Link::Run { id: run_id.clone() });
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
            "workflow.status" => {
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
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    None,
                    Some(json!({"runs": view})),
                )
            }
            "workflow.cancel" => match data["run"].as_str() {
                Some(id) if self.runs.contains_key(id) => {
                    self.cancel_run(id, "cancelled over A2A");
                    self.task_complete_now(
                        &ctx,
                        principal,
                        Link::Run { id: id.to_string() },
                        State::Completed,
                        Some(format!("run {id} cancelled")),
                        None,
                    )
                }
                _ => err_obj(TASK_NOT_FOUND, "no such run"),
            },
            // ---- steering: redirect live work without restarting it ------
            "workflow.signal" => {
                let name = data["name"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    return err_obj(::mcp::rpc::INVALID_PARAMS, "workflow.signal needs a name");
                }
                let payload = data.get("payload").cloned().unwrap_or(Value::Null);
                let target = data["run"].as_str().map(str::to_string);
                let delivered =
                    self.deliver_signal(&name, payload, target.as_deref(), Some(&principal.id));
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    Some(format!("signal {name:?} delivered to {delivered}")),
                    Some(json!({"signal": name, "delivered": delivered})),
                )
            }
            "subagent.send" | "subagent.kill" | "subagent.status" => {
                // Reuse the internal tool implementations verbatim.
                let mut args = data.clone();
                if op == "subagent.send"
                    && args.get("message").is_none()
                    && let Some(t) = data["text"].as_str()
                {
                    args["message"] = json!(t);
                }
                let tool_caller = crate::runtime::tools::ToolCaller {
                    principal: Some(principal.id.clone()),
                    ..Default::default()
                };
                match self.subagent_tool(&tool_caller, op, args) {
                    crate::runtime::tools::ToolOutcome::Ready(v, false) => self.task_complete_now(
                        &ctx,
                        principal,
                        Link::Turn { ctx: ctx.clone() },
                        State::Completed,
                        None,
                        Some(v),
                    ),
                    crate::runtime::tools::ToolOutcome::Ready(v, true) => err_obj(
                        ::mcp::rpc::INVALID_PARAMS,
                        v.as_str().unwrap_or("subagent op failed"),
                    ),
                    _ => err_obj(rpc_internal(), "unexpected deferred subagent op"),
                }
            }
            "plan.get" => {
                let id = data["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::context::ROOT.to_string());
                match self.contexts.get(&id) {
                    Some(c)
                        if principal.is_operator()
                            || c.principal.as_deref() == Some(principal.id.as_str()) =>
                    {
                        self.task_complete_now(
                            &ctx,
                            principal,
                            Link::Turn { ctx: ctx.clone() },
                            State::Completed,
                            None,
                            Some(json!({"conversation": id, "plan": c.plan, "progress": c.plan.as_ref().map(|p| p.progress())})),
                        )
                    }
                    _ => err_obj(TASK_NOT_FOUND, "no such conversation"),
                }
            }
            other => err_obj(
                UNSUPPORTED_OPERATION,
                &format!(
                    "command {other:?} is not available over A2A yet; send a natural-language message instead"
                ),
            ),
        }
    }
}

/// A compact run view for `workflow.status`.
fn run_view(id: &str, r: &crate::engine::RunState) -> Value {
    json!({"run": id, "workflow": r.workflow, "status": r.status.as_str(), "output": r.output, "error": r.error})
}
