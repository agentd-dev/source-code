// SPDX-License-Identifier: AGPL-3.0-only
//! The `delegation.*` command ops (RFC 0044): what `agentd --login
//! idp:<name>`, `--delegation-list`, `--delegation-approve`,
//! `--delegation-deny`, `--delegation-revoke`, `--delegation-forget` and
//! `--delegation-audit` send over the unix A2A listener.
//!
//! Served only where an IdP or a delegated server is declared, in a build
//! that acts for an enrolled person (the op table's `Gate::Delegation`).
//! Enrolling, listing, approving and denying are open to any named caller
//! and decided here by who that caller is: a person agentd acts for — a
//! per-person operator the listener named `operator:<id>` by its peer's
//! uid, an `a2a.login` session, a device session — enrols themself, lists
//! their own grants and gates, and answers their own gates; no operator
//! signs in as them or answers for them unless the person's template lists
//! `approvers: [principal, operator]`, and an attempt is refused and
//! recorded. A person revokes their own grants; an operator lists
//! everything, revokes anyone's, and forgets and reads a person's consent
//! records — those two stay on the operator floor: an operator offboards,
//! and keeps the audit. An enrolment's task says where
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
        // Who is asking, as their session or the configuration names them —
        // a per-person operator over the unix listener, an `a2a.login`
        // session, a device session — or `None`: someone no grant is held by.
        let acting =
            crate::delegation::Acting::new(Some(&principal.id), principal.session.as_deref());
        let who = d.who(acting).ok();
        let not_theirs = |what: &str| {
            refusal(
                errors::PERMISSION_DENIED,
                reason::PERMISSION_DENIED,
                &format!(
                    "{op} {what}, and {} is no person agentd acts for: no operator acts as them",
                    principal.id
                ),
                &[("op", op)],
            )
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
            // An operator lists every grant and gate; anyone else their own.
            "delegation.list" => {
                let mine = (!principal.is_operator()).then(|| {
                    who.as_ref()
                        .map(|w| w.id.clone())
                        .unwrap_or_else(|| principal.id.clone())
                });
                Ok(DelegationAnswer::Doc(serde_json::json!({
                    "grants": d.list_of(mine.as_deref()),
                    "gates": rt.consent_list(who.as_ref().map_or(principal.id.as_str(), |w| w.id.as_str()), principal.is_operator(), mine.as_deref()),
                })))
            }
            "delegation.enrol" => {
                let Some(who) = who.clone() else {
                    return Err(not_theirs("signs a person in"));
                };
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
                let started = match (&who.session, grant) {
                    // A display session signs in through a start link on the
                    // listener origin, which its own browser opens.
                    (
                        Some(sid),
                        None
                        | Some(crate::config::settings::delegation::SignInGrant::AuthorizationCode),
                    ) => rt.begin_enrol(&d, &who, sid, &idp, &task),
                    _ => {
                        let tx = rt.events_tx.clone();
                        let t = task.clone();
                        crate::delegation::enrol::start(
                            std::sync::Arc::clone(&d),
                            who,
                            &idp,
                            grant,
                            std::sync::Arc::new(move |said| {
                                let _ = tx.send(crate::runtime::events::Event::DelegationSaid {
                                    task: t.clone(),
                                    said,
                                });
                            }),
                        )
                    }
                };
                if let Err(e) = started {
                    rt.a2a_task_fail(&task, &format!("{op}: {e}"));
                }
                rt.task_persist(&task);
                rt.task_sync(&task);
                Ok(DelegationAnswer::Task(rt.task_reply(&task)))
            }
            "delegation.approve" | "delegation.deny" => {
                // One gate, or several of one shape answered together (§5.10
                // rule 6), each bound to its own nonce and argument hash.
                let answers: Vec<Value> = match (args.get("gates"), args.get("gate")) {
                    (Some(Value::Array(g)), None) if !g.is_empty() && g.len() <= 50 => g.clone(),
                    (None, Some(_)) => vec![args.clone()],
                    _ => {
                        return Err(bad(format!(
                            "{op}: {{gate, nonce, args_hash}}, or {{gates: [...]}} of 1 to 50 — not both"
                        )));
                    }
                };
                let shapes: std::collections::BTreeSet<Option<String>> = answers
                    .iter()
                    .map(|a| rt.consent_shape(a["gate"].as_str().unwrap_or_default()))
                    .collect();
                if answers.len() > 1 && (shapes.len() != 1 || shapes.contains(&None)) {
                    return Err(bad(format!(
                        "{op}: gates answered together are open gates of one shape (delegation.list's `shape`): nothing was decided"
                    )));
                }
                // Who answers, and how (RFC 0044 §5.10 rule 1): the person
                // through a display session whose IdP sign-in is recent
                // enough, a per-person operator over the unix listener, a
                // certificate principal over its certificate, or — where the
                // person's template lists them — an operator. Every gate is
                // held to it before any is answered.
                let mut answerers = Vec::with_capacity(answers.len());
                for a in &answers {
                    let gate = a["gate"].as_str().unwrap_or_default();
                    match rt.consent_answerer(&d, principal, who.as_ref(), gate) {
                        Ok(by) => answerers.push(by),
                        Err(why) => {
                            rt.consent_refused(gate, &principal.id, why.0);
                            return Err(refusal(
                                errors::PERMISSION_DENIED,
                                reason::PERMISSION_DENIED,
                                &format!("{op}: {}", why.1),
                                &[("op", op)],
                            ));
                        }
                    }
                }
                let task = new_task(rt);
                let mut said = Vec::new();
                let mut failed = Vec::new();
                for (a, by) in answers.iter().zip(&answerers) {
                    let gate = a["gate"].as_str().unwrap_or_default();
                    match rt.consent_answer(
                        gate,
                        by,
                        a["nonce"].as_str(),
                        a["args_hash"].as_str(),
                        op == "delegation.approve",
                    ) {
                        Ok(text) => said.push(text),
                        Err(e) => failed.push(e),
                    }
                }
                if failed.is_empty() {
                    if let Some(t) = rt.tasks.get_mut(&task) {
                        t.transition(State::Completed, Some(said.join("\n")));
                    }
                } else {
                    said.extend(failed);
                    rt.a2a_task_fail(&task, &format!("{op}: {}", said.join("\n")));
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
                // An operator revokes the grants a target names — anyone's,
                // offboarding by IdP subject too; anyone else the grants
                // their own session holds alone (self-service, RFC 0044
                // §5.8): a device session under the same name that never
                // proved the grant's identity holds none, and revokes none.
                enum Whose {
                    Named(String),
                    Held(Box<crate::delegation::Who>),
                }
                let whose = match (args["target"].as_str(), principal.is_operator(), &who) {
                    (Some(""), ..) => return Err(bad(format!("{op}: target is required"))),
                    (Some(t), true, _) => Whose::Named(t.to_string()),
                    (None, _, Some(w)) => Whose::Held(Box::new(w.clone())),
                    (None, _, None) => {
                        return Err(bad(format!(
                            "{op}: target is required: {} holds no grant of its own",
                            principal.id
                        )));
                    }
                    (Some(t), false, Some(w)) if w.id == t => Whose::Held(Box::new(w.clone())),
                    (Some(_), false, _) => {
                        return Err(not_theirs("revokes another's grants"));
                    }
                };
                let task = new_task(rt);
                let tx = rt.events_tx.clone();
                let (t, by) = (task.clone(), principal.id.clone());
                // RFC 7009 is retried for seconds: off the loop.
                std::thread::Builder::new()
                    .name("delegation-revoke".into())
                    .spawn(move || {
                        let done = match &whose {
                            Whose::Named(target) => d.revoke(target, &by),
                            Whose::Held(w) => d.revoke_held(w, &by),
                        };
                        let end = done.map(|done| {
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
    /// Begin `who`'s enrolment at `idp` from their display session `sid`
    /// (RFC 0044 §5.4): eligible first — a template admits them, and a
    /// device name is pinned, bound or first-bindable — then a sign-in
    /// through a single-use start link on the listener origin, whose
    /// callback proves the identity on that very session. Task `task` says
    /// where to sign in, and the federation ends it.
    pub(crate) fn begin_enrol(
        &mut self,
        d: &std::sync::Arc<crate::delegation::Delegation>,
        who: &crate::delegation::Who,
        sid: &str,
        idp: &str,
        task: &str,
    ) -> Result<(), String> {
        use crate::a2a::tasks::State;
        d.eligible(who, idp).map_err(|e| e.to_string())?;
        let authority = self
            .a2a_serving
            .as_ref()
            .and_then(|s| s.authority.clone())
            .ok_or("this listener issues no sessions, so it has no sign-in to start")?;
        let (Some(signins), Some(issuer)) = (authority.signins(), authority.issuer()) else {
            return Err("this listener has no sign-in at the IdP to start".into());
        };
        let link = signins.begin(
            issuer,
            crate::a2a::login::Purpose::Enrol {
                sid: sid.to_string(),
                task: task.to_string(),
            },
            idp,
            None,
        )?;
        if let Some(t) = self.tasks.get_mut(task) {
            t.sign_in_link = Some(serde_json::json!({
                "idp": idp, "start_uri": link.uri,
                "expires_at": crate::state::now_ms() + link.expires_in * 1000,
            }));
            t.transition(
                State::Working,
                Some(format!(
                    "Open {} in your browser within {}s and sign in at {idp}: agentd binds {} to whoever signs in there",
                    link.uri, link.expires_in, who.id
                )),
            );
        }
        Ok(())
    }

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
        // An enrolment that ended has nothing more to sign in at.
        if matches!(said, Said::Done(_) | Said::Answered(..)) {
            t.sign_in_link = None;
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
