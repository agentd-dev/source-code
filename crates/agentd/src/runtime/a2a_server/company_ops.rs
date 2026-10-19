// SPDX-License-Identifier: AGPL-3.0-only
//! The `company.*` command ops (RFC 0047 §5.2): the operator's view of the
//! Companies this instance acts at, the reset of the User ID one of them
//! knows the User by, and signing the User in and out there.
//!
//! All are on the operator floor, and served only where a Company is
//! declared, in a build that acts at one: the op table's `Gate::Companies`.
//! `company.status` is the `company.status` tool's document with why each
//! failed Company failed. `company.user_id.reset` answers open issue #11's
//! request that a User be able to start over at a Company: it revokes and
//! forgets the Account Token of every account the User holds there, forgets
//! the User ID, its secret and every `session_id`, and drops the Sessions,
//! so the Company sees a new User — and
//! loses the cart and conversations it kept for the old one. The Company's
//! MCP connections were the old User's, sessions and all, so each is dialled
//! afresh before anything more is sent to it. It is an audit record.
//!
//! `company.sign_in` and `company.sign_out` are what `agentd --login` and
//! `--logout company:<name>[/<label>]` send, and what a TUI or web UI the
//! User signs in to sends — each for one of the User's accounts there, by
//! its `account` label: only the configured User may, and their task is the
//! sign-in's gate and says how it ended (`runtime::company_sign_in`).

use super::commands::refusal;
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::runtime::reactor::Runtime;
use serde_json::Value;

/// What a `company.*` op produced. A build without `companies` serves
/// none of the ops, so produces none of these.
#[cfg_attr(not(feature = "companies"), allow(dead_code))]
pub(super) enum CompanyAnswer {
    /// A read's document (the `Message` row).
    Doc(Value),
    /// Work done at once: the task's text and result (the `Task` row).
    Done(String, Value),
    /// A task the op created, which its work drives on (the `Task` row).
    Task(Value),
}

/// Run a `company.*` op for `principal`, whom the listener and the command
/// dispatch have both already held to the operator floor, in the
/// conversation `ctx`, sent as `message`.
pub(super) fn handle(
    rt: &mut Runtime,
    principal: &Principal,
    op: &str,
    args: &Value,
    ctx: &str,
    message: &Value,
) -> Result<CompanyAnswer, Value> {
    let bad = |msg: String| {
        refusal(
            errors::INVALID_PARAMS,
            reason::INVALID_COMMAND_ARGS,
            &msg,
            &[("op", op)],
        )
    };
    #[cfg(feature = "companies")]
    match op {
        "company.status" => Ok(CompanyAnswer::Doc(rt.company_status(true))),
        "company.user_id.reset" => {
            let name = args["company"].as_str().unwrap_or_default();
            let (Some(sup), true) = (rt.companies.clone(), !name.is_empty()) else {
                return Err(bad(format!("{op}: no company {name:?} is declared")));
            };
            let Some(company) = sup.company(name) else {
                return Err(bad(format!(
                    "{op}: {name:?} is not a company entry (services.<name>.kind: company)"
                )));
            };
            // The old User's Account Tokens first — every account's: nothing
            // after the reset is signed in as them. Revoked off the loop —
            // whatever the reset comes to, since once taken they are no
            // longer held here.
            let taken = sup.take_accounts(&company);
            let reset = taken
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|_| sup.reset_user(&company));
            if let Ok(accounts) = taken
                && !accounts.is_empty()
            {
                let log = rt.log.clone();
                let (sup, company) = (std::sync::Arc::clone(&sup), std::sync::Arc::clone(&company));
                std::thread::Builder::new()
                    .name(format!("company-revoke:{name}"))
                    .spawn(move || {
                        for (label, account) in accounts {
                            // The Sessions are gone with the old User: revoked
                            // from the signed-out one.
                            let revoked = sup.revoke_account(&company, None, &account);
                            log.info(
                                "company.signed_out",
                                serde_json::json!({"company": company.name, "account": label,
                                                   "by": "company.user_id.reset",
                                                   "revoked": revoked.is_ok(), "error": revoked.err()}),
                            );
                        }
                    })
                    .ok();
            }
            rt.audit(crate::runtime::audit::AuditEvent {
                action: "company.user_id.reset",
                target: serde_json::json!({"company": name}),
                outcome: if reset.is_ok() { "reset" } else { "refused" },
                principal: Some(&principal.id),
                role: Some("operator"),
                request_id: None,
                sid: principal.session.as_deref(),
                trace: None,
                link: None,
            });
            let result = reset.map_err(|e| bad(format!("{op}: {e}")))?;
            rt.company_redial(name);
            Ok(CompanyAnswer::Done(
                format!(
                    "company {name}: the User ID is forgotten; the Company sees a new User from the next call on"
                ),
                result,
            ))
        }
        "company.sign_in" | "company.sign_out" => {
            // The User's alone: what they say here is presented to the
            // Company as theirs.
            let to = rt.company_user();
            if !to.matches(principal) {
                return Err(refusal(
                    errors::PERMISSION_DENIED,
                    reason::PERMISSION_DENIED,
                    &format!(
                        "{op} acts for {} at every Company, and {} is not them",
                        to.describe(),
                        principal.id
                    ),
                    &[("op", op)],
                ));
            }
            let task = rt.task_create(
                ctx,
                principal,
                crate::a2a::tasks::Link::Turn {
                    ctx: ctx.to_string(),
                },
                Some(message),
            );
            if let Some(t) = rt.tasks.get_mut(&task) {
                t.transition(crate::a2a::tasks::State::Working, None);
            }
            let started = if op == "company.sign_in" {
                rt.company_sign_in_op(&task, args, &principal.id)
            } else {
                rt.start_sign_out(
                    args,
                    crate::runtime::company_sign_in::Owed::Task(task.clone()),
                    Some(principal.id.clone()),
                )
            };
            if let Err(e) = started {
                rt.a2a_task_fail(&task, &format!("{op}: {e}"));
            }
            rt.task_persist(&task);
            rt.task_sync(&task);
            Ok(CompanyAnswer::Task(rt.task_reply(&task)))
        }
        _ => Err(super::commands::unknown_op(op)),
    }
    #[cfg(not(feature = "companies"))]
    {
        let _ = (rt, principal, args, ctx, message, bad);
        Err(super::commands::unknown_op(op))
    }
}
