// SPDX-License-Identifier: AGPL-3.0-only
//! The display surface's taskless reads and controls: `interface.info`,
//! `config.set` and the introspection reads behind
//! `a2a.introspection.enabled`.

use super::{FEED_RING, FeedVis, TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj, rpc_internal};
use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};

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
    /// A gate error for an introspection read while introspection is off.
    ///
    /// Introspection alone decides: it is independent of the feed, so an
    /// operator can open transcripts on an instance that serves no feed.
    fn debug_gate(&self) -> Option<Value> {
        if !self.settings.a2a.introspection.enabled {
            return Some(err_obj(
                UNSUPPORTED_OPERATION,
                "introspection is disabled (set a2a.introspection.enabled: true)",
            ));
        }
        None
    }

    /// Arm the log ring the introspection reads tail. Idempotent, so every
    /// path that turns introspection on — startup, `config.set`, a reload —
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

    /// `interface.info` — what this instance's display surface serves. This is
    /// the client's first call: it learns whether the feed is on, whether the
    /// introspection panes may render and which ops exist, so a client never
    /// has to guess at a capability and offer an action the daemon would
    /// refuse. The chrome's layout is the client's own business, so nothing
    /// about it is reported here.
    pub(super) fn interface_info(&self) -> Value {
        if !self.settings.a2a.events.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the observation feed is disabled (set a2a.events.enabled: true)",
            );
        }
        // The same list the card renders as skills and `--capabilities`
        // reports, narrowed to this surface — not a fifth copy of it.
        let ops = crate::runtime::surface::interface_ops_of(&self.settings);
        json!({"interface": {
            "enabled": true,
            "debug": self.settings.a2a.introspection.enabled,
            "version": crate::VERSION,
            "instance": self.instance,
            "model": self.model,
            "protocol": 1,
            "feed": {"ring": FEED_RING, "method": "SubscribeToEvents"},
            "ops": ops,
        }})
    }

    /// `config.set {path, value}` (operator): runtime updates for a small
    /// WHITELIST of interface knobs. Everything else belongs to the config
    /// file plus a SIGHUP reload. This deliberately never writes files, so the
    /// operator's documents stay the single source of truth and a remote
    /// caller cannot make a change that outlives the process unnoticed.
    pub(super) fn interface_config_set(&mut self, data: &Value) -> Value {
        if !self.settings.a2a.events.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the observation feed is disabled (set a2a.events.enabled: true)",
            );
        }
        let path = data["path"].as_str().unwrap_or_default();
        let value = data.get("value").cloned().unwrap_or(Value::Null);
        let applied: Result<Value, String> = match path {
            "a2a.introspection.enabled" => match value.as_bool() {
                Some(on) => {
                    self.settings.a2a.introspection.enabled = on;
                    if let Some(feed) = &self.a2a_feed {
                        feed.set_debug(on);
                    }
                    if on {
                        // The introspection reads tail the log ring — make
                        // sure it runs.
                        self.arm_introspection_ring();
                    }
                    Ok(json!(on))
                }
                None => Err("a2a.introspection.enabled takes true|false".into()),
            },
            // How much the operator wants to be asked changes with what the
            // agent is doing — closely supervised somewhere unfamiliar, left
            // alone once it is doing something watched twenty times. That is a
            // decision made DURING a session, so it has to be settable in one.
            "agent.approval" => match value.as_str() {
                Some("ask") | Some("await") | Some("human") => {
                    self.settings.agent.approval = crate::config::v2::Approval::Ask;
                    Ok(json!("ask"))
                }
                Some("auto") => {
                    self.settings.agent.approval = crate::config::v2::Approval::Auto;
                    Ok(json!("auto"))
                }
                Some("accept") | Some("accept_all") | Some("yes") => {
                    self.settings.agent.approval = crate::config::v2::Approval::Accept;
                    Ok(json!("accept"))
                }
                _ => Err("agent.approval takes ask | auto | accept".into()),
            },
            other => Err(format!(
                "{other:?} is not runtime-settable; settable: a2a.introspection.enabled, agent.approval — everything else is the config file + SIGHUP (docs/configuration.md §11)"
            )),
        };
        match applied {
            Ok(v) => {
                self.log
                    .info("interface.config_set", json!({"path": path, "value": v}));
                self.feed_push(
                    "config",
                    FeedVis::Operator,
                    json!({"path": path, "value": v}),
                );
                json!({"set": {"path": path, "value": v}})
            }
            Err(e) => err_obj(::mcp::rpc::INVALID_PARAMS, &e),
        }
    }

    /// `subagent.get {handle}` (debug): one subagent's detail — instruction,
    /// status, attempts, result/error (truncated) — the drill-down view.
    pub(super) fn interface_subagent_get(&self, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let handle = data["handle"]
            .as_str()
            .or_else(|| data["id"].as_str())
            .unwrap_or("");
        let Some(s) = self.subagents.get(handle) else {
            return err_obj(TASK_NOT_FOUND, "no such subagent");
        };
        json!({"subagent": {
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
        }})
    }

    /// `conversation.get {id, limit?}` (debug): the conversation transcript —
    /// the one read that exposes message BODIES, which is why it rides the
    /// debug gate. Ownership: the owner or an operator.
    pub(super) fn interface_conversation_get(&self, principal: &Principal, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let id = data["id"].as_str().unwrap_or("");
        let limit = data["limit"].as_u64().unwrap_or(200).min(1000) as usize;
        let Some(c) = self.contexts.get(id) else {
            return err_obj(TASK_NOT_FOUND, "no such conversation");
        };
        let owner_ok =
            principal.is_operator() || c.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            // Don't disclose existence to a non-owner.
            return err_obj(TASK_NOT_FOUND, "no such conversation");
        }
        let skip = c.messages.len().saturating_sub(limit);
        let messages: Vec<Value> = c.messages[skip..]
            .iter()
            .map(|m| truncate_strings(serde_json::to_value(m).unwrap_or(Value::Null), 4096))
            .collect();
        json!({"conversation": {
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
        }})
    }

    /// `run.get {run}` (debug): a run with PER-STEP detail — status, attempts,
    /// timings, error, wait, truncated output — the projection a run-graph
    /// view renders (the plain `workflow.status` stays a histogram).
    pub(super) fn interface_run_get(&self, principal: &Principal, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let id = data["run"]
            .as_str()
            .or_else(|| data["id"].as_str())
            .unwrap_or("");
        let Some(r) = self.runs.get(id) else {
            return err_obj(TASK_NOT_FOUND, "no such run");
        };
        let owner_ok =
            principal.is_operator() || r.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            return err_obj(TASK_NOT_FOUND, "no such run");
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
        json!({"run": run})
    }

    /// `debug.events {after?, limit?, level?, prefix?}` (debug, operator): a
    /// cursor read of the live log ring — the TUI's log tail.
    pub(super) fn interface_debug_events(&self, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let after = data["after"].as_u64().unwrap_or(0);
        let limit = data["limit"].as_u64().unwrap_or(200).min(500) as usize;
        let level = data["level"].as_str();
        let prefixes: Vec<&str> = data["prefix"].as_str().map(|p| vec![p]).unwrap_or_default();
        match crate::obs::log::read_event_window(after, limit, level, &prefixes) {
            Some(w) => {
                json!({"events": w.events, "newest_seq": w.newest_seq, "oldest_seq": w.oldest_seq, "dropped": w.dropped})
            }
            None => err_obj(rpc_internal(), "the event ring is not installed"),
        }
    }
}
