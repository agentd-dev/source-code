// SPDX-License-Identifier: AGPL-3.0-only
//! The `delegation.*` command ops (RFC 0044): what `agentd --login
//! idp:<name>`, `--delegation-list`, `--delegation-approve`,
//! `--delegation-deny`, `--delegation-revoke`, `--delegation-forget` and
//! `--delegation-audit` send over the unix A2A listener.
//!
//! All are on the operator floor, and served only where an IdP or a
//! delegated server is declared, in a build that acts for an enrolled person
//! (the op table's `Gate::Delegation`). Enrolling, approving and denying are
//! the per-person operator's alone — the caller the listener named
//! `operator:<id>` by its peer's uid: no other operator signs in as them or
//! answers for them, and an attempt is refused and recorded. Any operator
//! lists, revokes, forgets and reads a person's consent records: an
//! operator offboards, and keeps the audit. An enrolment's task says where
//! to sign in and how it ended; a revocation's and an erasure's say what
//! was done; a read of the consent records answers with them as its
//! result, read off the loop.

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
                let gate = args["gate"].as_str().unwrap_or_default().to_string();
                // No operator answers for the person (T7): refused, and the
                // attempt recorded.
                if let Err(e) = theirs("answers for the person a consent gate asks") {
                    rt.consent_refused(&gate, &principal.id, "not_the_person");
                    return Err(e);
                }
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
            "delegation.audit" => {
                let who = args["principal"]
                    .as_str()
                    .unwrap_or(principal.id.as_str())
                    .to_string();
                let limit = args["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
                let Some(chain) = rt.audit_chain.clone() else {
                    return Err(bad(format!(
                        "{op}: observability.audit.chain is not set, so no consent record is kept to read"
                    )));
                };
                let task = new_task(rt);
                let tx = rt.events_tx.clone();
                let t = task.clone();
                // Every kept segment is read, and each record opened under
                // the person's key — through the external signer when it
                // holds them: off the loop.
                std::thread::Builder::new()
                    .name("delegation-audit".into())
                    .spawn(move || {
                        let said = match consent_records(&chain, &d, &who, limit) {
                            Ok(doc) => Said::Answered(
                                format!(
                                    "{}: {} consent record(s)",
                                    who,
                                    doc["records"].as_array().map_or(0, Vec::len)
                                ),
                                doc,
                            ),
                            Err(e) => Said::Done(Err(e)),
                        };
                        let _ = tx
                            .send(crate::runtime::events::Event::DelegationSaid { task: t, said });
                    })
                    .ok();
                rt.task_persist(&task);
                rt.task_sync(&task);
                Ok(DelegationAnswer::Task(rt.task_reply(&task)))
            }
            "delegation.forget" => {
                let target = args["principal"].as_str().unwrap_or_default().to_string();
                if target.is_empty() {
                    return Err(bad(format!("{op}: principal is required")));
                }
                // Their open gates close first, recorded under the key the
                // erasure destroys.
                rt.forget_consents(&target);
                let task = new_task(rt);
                let tx = rt.events_tx.clone();
                let (t, by) = (task.clone(), principal.id.clone());
                // RFC 7009 is retried for seconds: off the loop.
                std::thread::Builder::new()
                    .name("delegation-forget".into())
                    .spawn(move || {
                        let end = d.forget(&target, &by).map(|done| {
                            format!(
                                "{target}: {} grant(s) revoked; {}",
                                done["revoked"].as_array().map_or(0, Vec::len),
                                if done["key"].is_null() {
                                    "no personal key was kept".to_string()
                                } else {
                                    "their personal key was destroyed: what it sealed is unreadable"
                                        .to_string()
                                }
                            )
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

/// `who`'s last `limit` consent records in `chain`, the latest first,
/// each with what its gate showed opened under their personal key —
/// `unreadable` once that key is gone, `unsealed` where none was kept.
#[cfg(feature = "xaa")]
fn consent_records(
    chain: &crate::obs::chain::Chain,
    d: &crate::delegation::Delegation,
    who: &str,
    limit: usize,
) -> Result<Value, String> {
    let mut records = chain.records(
        |r| r["action"] == "delegation.consent" && r["principal"] == who,
        limit,
    )?;
    records.reverse();
    for r in &mut records {
        let gate = r["target"]["gate"].as_str().unwrap_or_default().to_string();
        let sealed = r["target"]["personal"].clone();
        let shown = if sealed.get("sealed").is_none() {
            serde_json::json!("unsealed")
        } else {
            match d.open_personal(who, &format!("consent:{gate}"), &sealed) {
                Ok(Some(content)) => content,
                Ok(None) => serde_json::json!("unreadable"),
                Err(e) => serde_json::json!({"error": e}),
            }
        };
        r["target"]["personal"] = shown;
    }
    Ok(serde_json::json!({"principal": who, "records": records}))
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
            Said::Answered(text, doc) => {
                t.set_result(doc);
                t.transition(State::Completed, Some(text));
            }
            Said::Done(Err(e)) => t.transition(State::Failed, Some(e)),
        }
        self.task_persist(task);
        self.task_sync(task);
    }
}
