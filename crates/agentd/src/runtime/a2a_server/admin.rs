// SPDX-License-Identifier: AGPL-3.0-only
//! The operator admin family: drain, pause, resume, cancel and set.

use super::commands::refusal;
use super::{FeedVis, TASK_NOT_FOUND, err_obj};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::runtime::reactor::Runtime;
use crate::runtime::surface::RUNTIME_SETTABLE;
use serde_json::{Value, json};

/// The admin ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AdminOp {
    Drain,
    Pause,
    Resume,
    Cancel,
    Set,
}

impl AdminOp {
    pub(super) fn of(op: &str) -> Option<AdminOp> {
        match op {
            "admin.drain" => Some(AdminOp::Drain),
            "admin.pause" => Some(AdminOp::Pause),
            "admin.resume" => Some(AdminOp::Resume),
            "admin.cancel" => Some(AdminOp::Cancel),
            "admin.set" => Some(AdminOp::Set),
            _ => None,
        }
    }
}

impl Runtime {
    /// Run an admin op. `Ok` is the completed task's `(text, result)`.
    pub(super) fn a2a_admin(
        &mut self,
        _principal: &Principal,
        op: AdminOp,
        params: &Value,
    ) -> Result<(String, Value), Value> {
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("operator request")
            .to_string();
        let body = match op {
            AdminOp::Drain => {
                self.begin_drain(&reason);
                json!({"ok": true, "state": "draining", "reason": reason})
            }
            AdminOp::Cancel => {
                if let Some(run) = params.get("run").and_then(Value::as_str) {
                    self.cancel_run(run, &reason);
                    json!({"ok": true, "cancelled": run})
                } else {
                    return Err(err_obj(errors::INVALID_PARAMS, "cancel needs a run id"));
                }
            }
            // Pause/resume: with a `run`, flip that run between
            // Paused and Running (the scheduler already skips Paused runs);
            // without one, hold the WHOLE instance — intake continues (inbox,
            // tasks), but no new turns dispatch and no steps schedule until
            // resume. Reversible, unlike drain.
            AdminOp::Pause => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status.is_terminal() => {
                        return Err(err_obj(
                            errors::INVALID_PARAMS,
                            "the run is already terminal",
                        ));
                    }
                    Some(r) => {
                        r.status = crate::engine::RunStatus::Paused;
                        r.touch();
                        self.log
                            .info("run.paused", json!({"run": run, "reason": reason}));
                        json!({"ok": true, "paused": run})
                    }
                    None => return Err(err_obj(TASK_NOT_FOUND, "no such run")),
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
            AdminOp::Resume => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status == crate::engine::RunStatus::Paused => {
                        r.status = crate::engine::RunStatus::Running;
                        r.touch();
                        self.log.info("run.resumed", json!({"run": run}));
                        json!({"ok": true, "resumed": run})
                    }
                    Some(_) => {
                        return Err(err_obj(errors::INVALID_PARAMS, "the run is not paused"));
                    }
                    None => return Err(err_obj(TASK_NOT_FOUND, "no such run")),
                },
                None => {
                    self.paused = false;
                    crate::obs::metrics::set_paused(false);
                    self.log.info("agent.resumed", json!({}));
                    self.feed_push("lifecycle", FeedVis::All, json!({"paused": false}));
                    json!({"ok": true, "state": "running"})
                }
            },
            AdminOp::Set => {
                let set = self.admin_set(params)?;
                let text = format!("{} = {}", set["path"].as_str().unwrap_or(""), set["value"]);
                return Ok((text, set));
            }
        };
        let text = body
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{op:?}").to_lowercase());
        Ok((text, body))
    }

    /// `admin.set {path, value}`: change one [`RUNTIME_SETTABLE`] path in the
    /// running instance. Nothing is written to disk: the operator's documents
    /// stay the one source of truth, and the next reload puts the file's
    /// value back.
    fn admin_set(&mut self, params: &Value) -> Result<Value, Value> {
        let path = params["path"].as_str().unwrap_or_default().to_string();
        let value = params.get("value").cloned().unwrap_or(Value::Null);
        let setting = parse_set(&path, value).map_err(|why| {
            refusal(
                errors::INVALID_PARAMS,
                reason::INVALID_COMMAND_ARGS,
                &format!("{why}; settable: {}", RUNTIME_SETTABLE.join(", ")),
                &[("op", "admin.set")],
            )
        })?;
        let applied = match setting {
            Setting::Approval(a) => {
                self.settings.agent.approval = a;
                json!(approval_name(a))
            }
            Setting::Introspection(on) => {
                self.settings.a2a.introspection.enabled = on;
                if let Some(feed) = &self.a2a_feed {
                    feed.set_introspection(on);
                }
                if on {
                    // The introspection reads tail the log ring. Idempotent, so
                    // a second `true` keeps what the first one captured.
                    self.arm_introspection_ring();
                }
                json!(on)
            }
        };
        self.log
            .info("admin.set", json!({"path": path, "value": applied}));
        self.feed_push(
            "config",
            FeedVis::All,
            json!({"paths": [path], "source": "admin.set"}),
        );
        Ok(json!({"path": path, "value": applied}))
    }
}

/// One parsed `admin.set`.
#[derive(Debug, PartialEq)]
enum Setting {
    Approval(crate::config::v2::Approval),
    Introspection(bool),
}

/// Read `value` for `path` with the configuration's OWN types, so `admin.set`
/// accepts exactly what the config file does for that path — a spelling the
/// file refuses is refused here too, and one it accepts cannot mean something
/// different at runtime.
fn parse_set(path: &str, value: Value) -> Result<Setting, String> {
    match path {
        "agent.approval" => serde_json::from_value(value)
            .map(Setting::Approval)
            .map_err(|e| format!("agent.approval: {e}")),
        "a2a.introspection.enabled" => serde_json::from_value(value)
            .map(Setting::Introspection)
            .map_err(|e| format!("a2a.introspection.enabled: {e}")),
        other => Err(format!(
            "{other:?} is not runtime-settable — everything else is the config file and a reload"
        )),
    }
}

/// The canonical spelling of an approval mode.
fn approval_name(a: crate::config::v2::Approval) -> &'static str {
    match a {
        crate::config::v2::Approval::Ask => "ask",
        crate::config::v2::Approval::Auto => "auto",
        crate::config::v2::Approval::Accept => "accept",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path the table calls settable is one `admin.set` can parse, with
    /// the config's own types — and nothing else is. The list and the parser
    /// are two places; this is what keeps them one promise.
    #[test]
    fn every_runtime_settable_path_parses_and_nothing_else_does() {
        let sample = |path: &str| match path {
            "agent.approval" => json!("accept"),
            _ => json!(true),
        };
        for path in RUNTIME_SETTABLE {
            assert!(
                parse_set(path, sample(path)).is_ok(),
                "{path} is settable but admin.set cannot parse it"
            );
        }
        assert_eq!(
            parse_set("agent.approval", json!("auto")),
            Ok(Setting::Approval(crate::config::v2::Approval::Auto))
        );
        // A value the config file refuses is refused here too.
        assert!(parse_set("agent.approval", json!("sometimes")).is_err());
        assert!(parse_set("a2a.introspection.enabled", json!("yes")).is_err());
        for path in ["agent.model", "a2a.events.enabled", "a2a.bearer", ""] {
            assert!(
                parse_set(path, json!(true)).is_err(),
                "{path} is not settable"
            );
        }
    }
}
