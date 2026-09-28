// SPDX-License-Identifier: AGPL-3.0-only
//! The introspection reads behind `a2a.introspection.enabled`: transcripts,
//! per-step run detail, subagent detail and the log ring.

use super::{TASK_NOT_FOUND, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};

/// The introspection ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntrospectionOp {
    Conversation,
    Run,
    Subagent,
    Events,
}

impl IntrospectionOp {
    pub(super) fn of(op: &str) -> Option<IntrospectionOp> {
        match op {
            "conversation.get" => Some(IntrospectionOp::Conversation),
            "run.get" => Some(IntrospectionOp::Run),
            "subagent.get" => Some(IntrospectionOp::Subagent),
            "debug.events" => Some(IntrospectionOp::Events),
            _ => None,
        }
    }
}

/// Truncate every string in a JSON tree to `max` bytes (marking the cut) — the
/// debug reads bound their payloads with this so a huge tool result cannot
/// balloon an interface reply.
pub(super) fn truncate_strings(v: Value, max: usize) -> Value {
    match v {
        Value::String(s) if s.len() > max => {
            let mut cut = max;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            Value::String(format!("{}…(+{} bytes)", &s[..cut], s.len() - cut))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|x| truncate_strings(x, max)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k, truncate_strings(x, max)))
                .collect(),
        ),
        other => other,
    }
}

impl Runtime {
    /// Run an introspection read. `Ok` is the document the Message carries.
    ///
    /// Introspection alone gates these — not the feed, and not a display
    /// client — so an operator can open transcripts on an instance that
    /// serves no feed, and any A2A client may read them once it is on. The
    /// dispatch checks that gate, from the op table's own column, before it
    /// routes here.
    pub(super) fn introspection_op(
        &self,
        principal: &Principal,
        op: IntrospectionOp,
        data: &Value,
    ) -> Result<Value, Value> {
        debug_assert!(self.settings.a2a.introspection.enabled);
        match op {
            IntrospectionOp::Conversation => self.conversation_get(principal, data),
            IntrospectionOp::Run => self.run_get(principal, data),
            IntrospectionOp::Subagent => self.subagent_get(principal, data),
            IntrospectionOp::Events => self.debug_events(data),
        }
    }

    /// Arm the log ring the introspection reads tail. Idempotent, so every
    /// path that turns introspection on — startup, `admin.set`, a reload —
    /// can call it without clearing what an earlier one captured.
    pub(crate) fn arm_introspection_ring(&self) {
        let cap = self
            .settings
            .observability
            .events_ring
            .map(|n| n as usize)
            .unwrap_or(crate::obs::log::EVENTS_RING_DEFAULT);
        crate::obs::log::install_event_ring(cap);
    }

    /// `subagent.get {handle}`: one subagent's detail — instruction, status,
    /// attempts, result/error (truncated) — the drill-down view. Ownership:
    /// the owner or an operator, and a non-owner is told the handle does not
    /// exist, as `conversation.get` tells it.
    fn subagent_get(&self, principal: &Principal, data: &Value) -> Result<Value, Value> {
        let handle = data["handle"].as_str().unwrap_or("");
        let Some(s) = self.subagents.get(handle) else {
            return Err(err_obj(TASK_NOT_FOUND, "no such subagent"));
        };
        if !principal.is_operator() && self.subagent_owner(handle) != Some(principal.id.as_str()) {
            return Err(err_obj(TASK_NOT_FOUND, "no such subagent"));
        }
        Ok(json!({"subagent": {
            "handle": s.handle,
            "mode": s.mode,
            "status": s.status,
            "attempt": s.attempt,
            "tokens": s.tokens,
            "instruction": truncate_strings(json!(s.instruction), 4096),
            "result": s.result.clone().map(|r| truncate_strings(r, 4096)),
            "error": s.error,
            "requested_by": s.requested_by,
            "created": s.created,
            "updated": s.updated,
            "node": s.node.map(|n| n.0),
        }}))
    }

    /// Whose subagent `handle` is: the principal of the conversation or run
    /// that spawned it, or — for a subagent's own subagent — its parent's.
    /// The record names what spawned it, not who, so the owner is read off
    /// the spawner; a subagent whose spawner is gone is the operator's alone.
    fn subagent_owner(&self, handle: &str) -> Option<&str> {
        let mut h = handle;
        // Each hop is a distinct live record, so the chain ends within as
        // many hops as there are records — the bound only guards a cycle.
        for _ in 0..=self.subagents.len() {
            let by = self.subagents.get(h)?.requested_by.as_ref()?;
            let from_ctx = by["ctx"]
                .as_str()
                .and_then(|c| self.contexts.get(c))
                .and_then(|c| c.principal.as_deref());
            let from_run = by["run"]
                .as_str()
                .and_then(|r| self.runs.get(r))
                .and_then(|r| r.principal.as_deref());
            if let Some(p) = from_ctx.or(from_run) {
                return Some(p);
            }
            h = by["subagent"].as_str()?;
        }
        None
    }

    /// `conversation.get {id, limit?}`: the conversation transcript — the one
    /// read that exposes message BODIES, which is why it rides the
    /// introspection gate. Ownership: the owner or an operator.
    fn conversation_get(&self, principal: &Principal, data: &Value) -> Result<Value, Value> {
        let id = data["id"].as_str().unwrap_or("");
        let limit = data["limit"].as_u64().unwrap_or(200).min(1000) as usize;
        let Some(c) = self.contexts.get(id) else {
            return Err(err_obj(TASK_NOT_FOUND, "no such conversation"));
        };
        let owner_ok =
            principal.is_operator() || c.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            // Don't disclose existence to a non-owner.
            return Err(err_obj(TASK_NOT_FOUND, "no such conversation"));
        }
        let skip = c.messages.len().saturating_sub(limit);
        let messages: Vec<Value> = c.messages[skip..]
            .iter()
            .map(|m| truncate_strings(serde_json::to_value(m).unwrap_or(Value::Null), 4096))
            .collect();
        Ok(json!({"conversation": {
            "id": id,
            "kind": c.kind,
            "version": c.version,
            "turns": c.turns,
            "est_tokens": c.est_tokens,
            "principal": c.principal,
            "task": c.task,
            "skills": c.skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            "plan": c.plan,
            "summary": if c.summary.is_empty() { Value::Null } else { serde_json::to_value(&c.summary).unwrap_or(Value::Null) },
            "total_messages": c.messages.len(),
            "messages": messages,
            "updated": c.updated,
        }}))
    }

    /// `run.get {run}`: a run with PER-STEP detail — status, attempts,
    /// timings, error, wait, truncated output — the projection a run-graph
    /// view renders (the plain `workflow.status` stays a histogram).
    fn run_get(&self, principal: &Principal, data: &Value) -> Result<Value, Value> {
        let id = data["run"].as_str().unwrap_or("");
        let Some(r) = self.runs.get(id) else {
            return Err(err_obj(TASK_NOT_FOUND, "no such run"));
        };
        let owner_ok =
            principal.is_operator() || r.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            return Err(err_obj(TASK_NOT_FOUND, "no such run"));
        }
        let steps: serde_json::Map<String, Value> = r
            .steps
            .iter()
            .map(|(sid, st)| {
                (
                    sid.clone(),
                    json!({
                        "status": st.status,
                        "attempt": st.attempt,
                        "started": st.started,
                        "finished": st.finished,
                        "error": st.error,
                        "wait": st.wait,
                        "output": st.output.clone().map(|o| truncate_strings(o, 2048)),
                    }),
                )
            })
            .collect();
        let mut run = r.summary();
        run["steps"] = Value::Object(steps);
        run["vars"] = truncate_strings(Value::Object(r.vars.clone()), 2048);
        Ok(json!({"run": run}))
    }

    /// `debug.events {after?, limit?, level?, prefix?}` (operator): a cursor
    /// read of the live log ring — a display client's log tail.
    fn debug_events(&self, data: &Value) -> Result<Value, Value> {
        let after = data["after"].as_u64().unwrap_or(0);
        let limit = data["limit"].as_u64().unwrap_or(200).min(500) as usize;
        let level = data["level"].as_str();
        let prefixes: Vec<&str> = data["prefix"].as_str().map(|p| vec![p]).unwrap_or_default();
        match crate::obs::log::read_event_window(after, limit, level, &prefixes) {
            Some(w) => Ok(
                json!({"events": w.events, "newest_seq": w.newest_seq, "oldest_seq": w.oldest_seq, "dropped": w.dropped}),
            ),
            None => Err(err_obj(rpc_internal(), "the event ring is not installed")),
        }
    }
}
