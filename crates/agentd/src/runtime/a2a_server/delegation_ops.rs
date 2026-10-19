// SPDX-License-Identifier: AGPL-3.0-only
//! The `delegation.*` command ops (RFC 0044 P1): what `agentd --login
//! idp:<name>`, `--delegation-list`, `--delegation-approve`,
//! `--delegation-deny` and `--delegation-revoke` send over the unix A2A
//! listener.
//!
//! All are on the operator floor, and served only where an IdP or a
//! delegated server is declared, in a build that acts for an enrolled person
//! (the op table's `Gate::Delegation`). Enrolling, approving and denying are
//! the per-person operator's alone — the caller the listener named
//! `operator:<id>` by its peer's uid: no other operator signs in as them or
//! answers for them. Any operator lists and revokes: an operator offboards.
//! An enrolment's task says where to sign in and how it ended; a
//! revocation's says what the IdP acknowledged.

#[cfg(feature = "xaa")]
use super::commands::refusal;
use crate::a2a::Principal;
#[cfg(feature = "xaa")]
use crate::a2a::errors::{self, reason};
use crate::runtime::reactor::Runtime;
use serde_json::Value;

/// What a `delegation.*` op produced.
#[cfg_attr(not(feature = "xaa"), allow(dead_code))]
pub(super) enum DelegationAnswer {
    /// A read's document (the `Message` row).
    Doc(Value),
    /// A task the op created (the `Task` row).
    Task(Value),
}

/// Run a `delegation.*` op for `principal`, already held to the operator
/// floor, in the conversation `ctx`, sent as `message`.
pub(super) fn handle(
    rt: &mut Runtime,
    principal: &Principal,
    op: &str,
    args: &Value,
    ctx: &str,
    message: &Value,
) -> Result<DelegationAnswer, Value> {
    #[cfg(feature = "xaa")]
    {
        served(rt, principal, op, args, ctx, message)
    }
    #[cfg(not(feature = "xaa"))]
    {
        let _ = (rt, principal, args, ctx, message);
        Err(super::commands::unknown_op(op))
    }
}

/// [`handle`] in a build that acts for an enrolled person.
#[cfg(feature = "xaa")]
fn served(
    rt: &mut Runtime,
    principal: &Principal,
    op: &str,
    args: &Value,
    ctx: &str,
    message: &Value,
) -> Result<DelegationAnswer, Value> {
    {
        use crate::a2a::tasks::{Link, State};
        use crate::delegation::enrol::Said;
        let bad = |msg: String| {
            refusal(
                errors::INVALID_PARAMS,
                reason::INVALID_COMMAND_ARGS,
                &msg,
                &[("op", op)],
            )
        };
        let Some(d) = rt.delegation.clone() else {
            return Err(super::commands::unknown_op(op));
        };
        let person = d.person.as_ref().map(|p| p.id.clone());
        let theirs = |what: &str| -> Result<(), Value> {
            if person.as_deref() == Some(principal.id.as_str()) {
                return Ok(());
            }
            Err(refusal(
                errors::PERMISSION_DENIED,
                reason::PERMISSION_DENIED,
                &format!(
                    "{op} {what}, and {} is not the per-person operator ({}): no operator acts as them",
                    principal.id,
                    person.as_deref().unwrap_or("none is declared")
                ),
                &[("op", op)],
            ))
        };
        let new_task = |rt: &mut Runtime| {
            let task = rt.task_create(
                ctx,
                principal,
                Link::Turn {
                    ctx: ctx.to_string(),
                },
                Some(message),
            );
            if let Some(t) = rt.tasks.get_mut(&task) {
                t.transition(State::Working, None);
            }
            task
        };
        match op {
            "delegation.list" => Ok(DelegationAnswer::Doc(serde_json::json!({
                "grants": d.list(),
                "gates": rt.consent_list(&principal.id),
            }))),
            "delegation.enrol" => {
                theirs("signs the per-person operator in")?;
                let idp = args["idp"].as_str().unwrap_or_default().to_string();
                let grant = match args["grant"].as_str() {
                    None => None,
                    Some("device") => {
                        Some(crate::config::settings::delegation::SignInGrant::Device)
                    }
                    Some("authorization_code") => {
                        Some(crate::config::settings::delegation::SignInGrant::AuthorizationCode)
                    }
                    Some(g) => {
                        return Err(bad(format!(
                            "{op}: grant {g:?} is not device or authorization_code"
                        )));
                    }
                };
                let task = new_task(rt);
                let tx = rt.events_tx.clone();
                let t = task.clone();
                let started = crate::delegation::enrol::start(
                    std::sync::Arc::clone(&d),
                    &idp,
                    grant,
                    std::sync::Arc::new(move |said| {
                        let _ = tx.send(crate::runtime::events::Event::DelegationSaid {
                            task: t.clone(),
                            said,
                        });
                    }),
                );
                if let Err(e) = started {
                    rt.a2a_task_fail(&task, &format!("{op}: {e}"));
                }
                rt.task_persist(&task);
                rt.task_sync(&task);
                Ok(DelegationAnswer::Task(rt.task_reply(&task)))
            }
            "delegation.approve" | "delegation.deny" => {
                theirs("answers for the person a consent gate asks")?;
                let gate = args["gate"].as_str().unwrap_or_default().to_string();
                let task = new_task(rt);
                let answered = rt.consent_answer(
                    &gate,
                    &principal.id,
                    args["nonce"].as_str(),
                    args["args_hash"].as_str(),
                    op == "delegation.approve",
                );
                match answered {
                    Ok(text) => {
                        if let Some(t) = rt.tasks.get_mut(&task) {
                            t.transition(State::Completed, Some(text));
                        }
                    }
                    Err(e) => rt.a2a_task_fail(&task, &format!("{op}: {e}")),
                }
                rt.task_persist(&task);
                rt.task_sync(&task);
                Ok(DelegationAnswer::Task(rt.task_reply(&task)))
            }
            "delegation.revoke" => {
                let target = args["target"].as_str().unwrap_or_default().to_string();
                if target.is_empty() {
                    return Err(bad(format!("{op}: target is required")));
                }
                let task = new_task(rt);
                let tx = rt.events_tx.clone();
                let (t, by) = (task.clone(), principal.id.clone());
                // RFC 7009 is retried for seconds: off the loop.
                std::thread::Builder::new()
                    .name("delegation-revoke".into())
                    .spawn(move || {
                        let end = d.revoke(&target, &by).map(|done| {
                            done.iter()
                                .map(|r| {
                                    format!(
                                        "{}: revoked{}",
                                        r["principal"].as_str().unwrap_or_default(),
                                        if r["revoked_at_idp"] == true {
                                            " at the IdP and here"
                                        } else {
                                            " here; the IdP did not acknowledge the revocation"
                                        }
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        });
                        let _ = tx.send(crate::runtime::events::Event::DelegationSaid {
                            task: t,
                            said: Said::Done(end),
                        });
                    })
                    .ok();
                rt.task_persist(&task);
                rt.task_sync(&task);
                Ok(DelegationAnswer::Task(rt.task_reply(&task)))
            }
            _ => Err(super::commands::unknown_op(op)),
        }
    }
}

#[cfg(feature = "xaa")]
impl Runtime {
    /// A delegation op's task moved on: what the sign-in says, or how it
    /// ended.
    pub(crate) fn delegation_said(&mut self, task: &str, said: crate::delegation::enrol::Said) {
        use crate::a2a::tasks::State;
        use crate::delegation::enrol::Said;
        let Some(t) = self.tasks.get_mut(task) else {
            return;
        };
        if t.state.is_terminal() {
            return;
        }
        match said {
            Said::Text(text) => t.transition(State::Working, Some(text)),
            Said::Done(Ok(text)) => t.transition(State::Completed, Some(text)),
            Said::Done(Err(e)) => t.transition(State::Failed, Some(e)),
        }
        self.task_persist(task);
        self.task_sync(task);
    }
}
