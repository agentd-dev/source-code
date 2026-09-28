// SPDX-License-Identifier: AGPL-3.0-only
//! The `auth.*` command ops: the operator's side of the device grant and the
//! session list.
//!
//! Every one of them is on the operator floor — the op table says so, and
//! [`Principal::may_command`] reads it before any grant — because approving a
//! sign-in is minting a credential: a user who could reach these could approve
//! a device of their own as the operator.
//!
//! An approval names who the device becomes. `as` is required and is the name
//! the session acts under (`user:<name>`), so the principal is somebody an
//! operator chose on purpose — never a default every sign-in would share —
//! and two sessions approved as one name are one principal: they share its
//! tasks, runs and conversations, and its rate bucket, so history survives a
//! re-login. The approval says when that is happening (`existing`), because
//! reusing a name for a different person hands them that name's history.

use super::commands::{refusal, unknown_op};
use super::{FeedVis, err_obj};
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::a2a::oauth::{Approval, Revoke, Session};
use crate::config::v2::DeviceScope;
use crate::runtime::audit::AuditEvent;
use crate::runtime::identities::{self, Registered};
use crate::runtime::reactor::Runtime;
use crate::runtime::surface::{RESERVED_APPROVAL_NAMES, approval_name_ok};
use serde_json::{Value, json};

/// What an `auth.*` op produced.
pub(super) enum AuthAnswer {
    /// A read's document (the `Message` rows).
    Doc(Value),
    /// Work done at once: the task's text and result (the `Task` rows).
    Done(String, Value),
}

/// Run an `auth.*` op for `principal`, whom the listener and the command
/// dispatch have both already held to the operator floor.
pub(super) fn handle(
    rt: &mut Runtime,
    principal: &Principal,
    op: &str,
    args: &Value,
) -> Result<AuthAnswer, Value> {
    let bad = |msg: String| {
        refusal(
            errors::INVALID_PARAMS,
            reason::INVALID_COMMAND_ARGS,
            &msg,
            &[("op", op)],
        )
    };
    match op {
        "auth.device.pending" => {
            let device = &authority(rt)?.device;
            let pending: Vec<Value> = device.pending().iter().map(|p| p.view()).collect();
            Ok(AuthAnswer::Doc(json!({"pending": pending})))
        }
        "auth.device.approve" => approve(rt, principal, args).map_err(|e| match e {
            Refusal::Args(msg) => bad(msg),
            Refusal::Reply(v) => v,
        }),
        "auth.device.deny" => {
            let target = match (args["user_code"].as_str(), args["all"].as_bool()) {
                (Some(code), None | Some(false)) => Some(code),
                (None, Some(true)) => None,
                _ => {
                    return Err(bad(
                        "auth.device.deny takes {user_code} or {all: true}".into()
                    ));
                }
            };
            let denied = authority(rt)?.device.deny(target).map_err(bad)?;
            for code in &denied {
                let shown = crate::a2a::oauth::display_user_code(code);
                push(rt, json!({"event": "denied", "user_code": shown}));
            }
            audit(rt, principal, op, json!({"denied": denied.len()}));
            Ok(AuthAnswer::Done(
                format!("denied {} device sign-in(s)", denied.len()),
                json!({"denied": denied.len()}),
            ))
        }
        "auth.sessions" => {
            let sessions: Vec<Value> = sessions(rt)?.list().iter().map(Session::view).collect();
            Ok(AuthAnswer::Doc(json!({"sessions": sessions})))
        }
        "auth.sessions.revoke" => {
            let which = match (
                args["sid"].as_str(),
                args["name"].as_str(),
                args["all"].as_bool(),
            ) {
                (Some(sid), None, None | Some(false)) => Revoke::Sid(sid.to_string()),
                (None, Some(name), None | Some(false)) => Revoke::Name(name.to_string()),
                (None, None, Some(true)) => Revoke::All,
                _ => {
                    return Err(bad(
                        "auth.sessions.revoke takes exactly one of {sid}, {name} or {all: true}"
                            .into(),
                    ));
                }
            };
            let ended = sessions(rt)?.revoke(&which);
            for s in &ended {
                rt.log.info(
                    "auth.session.revoked",
                    s.revoked_line("auth.sessions.revoke"),
                );
                push(rt, s.revoked_event());
            }
            let target = match &which {
                Revoke::Sid(sid) => json!({"sid": sid, "revoked": ended.len()}),
                Revoke::Name(name) => json!({"name": name, "revoked": ended.len()}),
                Revoke::All => json!({"all": true, "revoked": ended.len()}),
            };
            audit(rt, principal, op, target);
            Ok(AuthAnswer::Done(
                format!("revoked {} session(s)", ended.len()),
                json!({"revoked": ended.len()}),
            ))
        }
        _ => Err(unknown_op(op)),
    }
}

/// Why an approval was refused: its arguments (`-32602`), or a reply
/// already whole.
enum Refusal {
    Args(String),
    Reply(Value),
}

impl From<Value> for Refusal {
    fn from(v: Value) -> Refusal {
        Refusal::Reply(v)
    }
}

/// `auth.device.approve {user_code, as, scope?}`.
///
/// Nothing is approved and nothing registered until every refusal has had
/// its say: the name's shape, the reserved names, a name a configured rule
/// holds now or ever held, the code, and the scope.
fn approve(rt: &mut Runtime, principal: &Principal, args: &Value) -> Result<AuthAnswer, Refusal> {
    let Some(name) = args["as"].as_str() else {
        return Err(Refusal::Args(
            "auth.device.approve needs `as`: the name the device signs in as — every session \
             approved under one name is one principal and shares what it owns"
                .into(),
        ));
    };
    if !approval_name_ok(name) {
        return Err(Refusal::Args(format!(
            "`as` {name:?} is not a name: lowercase letters, digits, `.`, `_` and `-`, starting \
             with a letter or digit, at most 64 ({})",
            crate::runtime::surface::APPROVAL_NAME_PATTERN
        )));
    }
    if RESERVED_APPROVAL_NAMES.contains(&name) {
        return Err(Refusal::Args(format!(
            "`as` {name:?} is reserved: it already names someone in the audit trail"
        )));
    }
    // A name a rule declares is that rule's principal (`user:<id>`, or
    // `agent:<id>`, which a reader of the trail would take for the same
    // caller); a device approved under it would inherit what the rule owns.
    if let Some(rule) = rt
        .settings
        .a2a
        .principals
        .iter()
        .find(|p| p.id.as_deref() == Some(name))
    {
        let role = format!("{:?}", rule.role).to_lowercase();
        return Err(Refusal::Args(format!(
            "`as` {name:?} is the id of a configured {role} principal rule \
             (a2a.principals id {name}); approve the device as another name"
        )));
    }
    let pid = format!("user:{name}");
    let registered = identities::lookup(&rt.durable, &pid).map_err(|e| store_refusal(&e))?;
    if registered == Some(Registered::Rule) {
        return Err(Refusal::Args(format!(
            "the name {name} was a configured principal id on this agent and owns its history; \
             approve the device as another name"
        )));
    }
    let Some(code) = args["user_code"].as_str() else {
        return Err(Refusal::Args(
            "auth.device.approve needs the device's user_code".into(),
        ));
    };
    let asked = match args.get("scope") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Err(Refusal::Args("`scope` is a string".into())),
    };
    let authority = authority(rt)?;
    let device = &authority.device;
    let view = device.find(code).map_err(Refusal::Args)?;
    let scope = device
        .grant_scope(view.requested, asked)
        .map_err(Refusal::Args)?;
    // The claim is recorded before the approval: a name that could not be
    // recorded is not one the registry can later protect.
    let existing = match scope {
        DeviceScope::User => {
            identities::register_device(&rt.durable, name).map_err(|e| store_refusal(&e))?
        }
        DeviceScope::Operator => registered == Some(Registered::Device),
    };
    let rule = rt
        .a2a_serving
        .as_ref()
        .and_then(|s| s.bridge.resolver().rule_of(principal).map(str::to_string));
    let view = device
        .approve(
            code,
            Approval {
                name: name.to_string(),
                scope,
                approved_by: principal.id.clone(),
                rule,
            },
        )
        .map_err(Refusal::Args)?;
    let session_principal = match scope {
        DeviceScope::Operator => "operator".to_string(),
        DeviceScope::User => pid,
    };
    let shown = crate::a2a::oauth::display_user_code(&view.user_code);
    let approved = json!({
        "user_code": shown,
        "client_id": view.client_id,
        "scope": scope.as_str(),
        "peer": view.peer.map(|p| p.to_string()),
        "principal": session_principal,
        "existing": existing,
    });
    push(
        rt,
        json!({"event": "approved", "user_code": shown, "name": name,
            "scope": scope.as_str(), "client_id": view.client_id,
            "principal": session_principal}),
    );
    audit(
        rt,
        principal,
        "auth.device.approve",
        json!({"name": name, "principal": session_principal, "scope": scope.as_str(),
            "client_id": view.client_id, "existing": existing}),
    );
    Ok(AuthAnswer::Done(
        format!("approved {shown} as {session_principal}"),
        json!({"approved": approved}),
    ))
}

fn store_refusal(e: &identities::Refused) -> Refusal {
    Refusal::Reply(err_obj(errors::INTERNAL_ERROR, &e.to_string()))
}

/// The authorization server, which the op table's gate has already said is
/// configured; a listener that failed to build one answers as if it were not.
fn authority(rt: &Runtime) -> Result<std::sync::Arc<crate::a2a::oauth::Authority>, Value> {
    rt.a2a_serving
        .as_ref()
        .and_then(|s| s.authority.clone())
        .ok_or_else(|| {
            err_obj(
                super::UNSUPPORTED_OPERATION,
                "the device grant is not serving on this listener",
            )
        })
}

/// The listener's sessions.
fn sessions(rt: &Runtime) -> Result<std::sync::Arc<crate::a2a::oauth::Sessions>, Value> {
    rt.a2a_serving
        .as_ref()
        .and_then(|s| s.sessions.clone())
        .ok_or_else(|| {
            err_obj(
                super::UNSUPPORTED_OPERATION,
                "this listener issues no sessions",
            )
        })
}

/// An `auth` event on the feed, for operators.
fn push(rt: &Runtime, event: Value) {
    if let Some(feed) = &rt.a2a_feed {
        feed.push("auth", FeedVis::Operator, event);
    }
}

/// The op's own audit line — the `a2a.SendMessage` line names only the task —
/// with the name and principal it acted on, and the caller's sid when the
/// operator is itself a session. Codes and tokens are never in it.
fn audit(rt: &Runtime, principal: &Principal, action: &str, target: Value) {
    let role = format!("{:?}", principal.role).to_lowercase();
    rt.audit(AuditEvent {
        action,
        target,
        outcome: "ok",
        principal: Some(&principal.id),
        role: Some(&role),
        request_id: None,
        sid: principal.session.as_deref(),
    });
}
