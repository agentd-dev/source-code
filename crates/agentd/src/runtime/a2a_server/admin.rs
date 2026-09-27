// SPDX-License-Identifier: AGPL-3.0-only
//! The operator admin family: drain, pause, resume and cancel.

use super::{FeedVis, TASK_NOT_FOUND, UNSUPPORTED_OPERATION, err_obj};
use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};

impl Runtime {
    pub(super) fn a2a_admin(
        &mut self,
        _principal: &Principal,
        method: &str,
        params: &Value,
    ) -> Value {
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("operator request")
            .to_string();
        match method.to_ascii_lowercase().as_str() {
            "admin.drain" | "admin.lameduck" => {
                self.begin_drain(&reason);
                json!({"ok": true, "state": "draining", "reason": reason})
            }
            "admin.cancel" => {
                if let Some(run) = params.get("run").and_then(Value::as_str) {
                    self.cancel_run(run, &reason);
                    json!({"ok": true, "cancelled": run})
                } else {
                    err_obj(::mcp::rpc::INVALID_PARAMS, "cancel needs a run id")
                }
            }
            // Pause/resume: with a `run`, flip that run between
            // Paused and Running (the scheduler already skips Paused runs);
            // without one, hold the WHOLE instance — intake continues (inbox,
            // tasks), but no new turns dispatch and no steps schedule until
            // resume. Reversible, unlike drain.
            "admin.pause" => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status.is_terminal() => {
                        err_obj(::mcp::rpc::INVALID_PARAMS, "the run is already terminal")
                    }
                    Some(r) => {
                        r.status = crate::engine::RunStatus::Paused;
                        r.touch();
                        self.log
                            .info("run.paused", json!({"run": run, "reason": reason}));
                        json!({"ok": true, "paused": run})
                    }
                    None => err_obj(TASK_NOT_FOUND, "no such run"),
                },
                None => {
                    self.paused = true;
                    crate::obs::metrics::set_paused(true);
                    self.log.info("agent.paused", json!({"reason": reason}));
                    self.feed_push(
                        "lifecycle",
                        FeedVis::All,
                        json!({"paused": true, "reason": reason}),
                    );
                    json!({"ok": true, "state": "paused", "reason": reason})
                }
            },
            "admin.resume" => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status == crate::engine::RunStatus::Paused => {
                        r.status = crate::engine::RunStatus::Running;
                        r.touch();
                        self.log.info("run.resumed", json!({"run": run}));
                        json!({"ok": true, "resumed": run})
                    }
                    Some(_) => err_obj(::mcp::rpc::INVALID_PARAMS, "the run is not paused"),
                    None => err_obj(TASK_NOT_FOUND, "no such run"),
                },
                None => {
                    self.paused = false;
                    crate::obs::metrics::set_paused(false);
                    self.log.info("agent.resumed", json!({}));
                    self.feed_push("lifecycle", FeedVis::All, json!({"paused": false}));
                    json!({"ok": true, "state": "running"})
                }
            },
            other => err_obj(
                UNSUPPORTED_OPERATION,
                &format!("unknown admin op {other:?}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    /// The admin family answers to the ROLE, never to a grant. Before these
    /// moved onto the command surface they were hard-denied for non-operators
    /// regardless of `grants:`; the move must not relax that.
    #[test]
    fn admin_ops_are_operator_only_whatever_the_grants_say() {
        use crate::a2a::principals::{Principal, is_admin_op};
        use crate::config::v2::Role;
        let with = |role: Role, grants: &[&str]| Principal {
            role,
            grants: grants.iter().map(|g| (*g).to_string()).collect(),
            ..Principal::anonymous()
        };
        for op in ["admin.drain", "admin.pause", "admin.resume", "admin.cancel"] {
            assert!(is_admin_op(op), "{op} is in the admin family");
            assert!(
                with(Role::Operator, &[]).may_command(op),
                "an operator may {op} with no grants at all"
            );
            for role in [Role::User, Role::Agent] {
                assert!(
                    !with(role, &["*"]).may_command(op),
                    "{role:?} with grants ['*'] must still not {op}"
                );
                assert!(!with(role, &[op]).may_command(op), "nor by naming it");
            }
        }
        // …while an ordinary command still answers to grants as before.
        assert!(with(Role::Agent, &["workflow.run"]).may_command("workflow.run"));
    }
}
