// SPDX-License-Identifier: AGPL-3.0-only
//! The `company.*` command ops (RFC 0047 §5.2): the operator's view of the
//! Companies this instance acts at, and the reset of the User ID one of them
//! knows the User by.
//!
//! Both are on the operator floor, and served only where a Company is
//! declared, in a build that acts at one: the op table's `Gate::Companies`.
//! `company.status` is the `company.status` tool's document with why each
//! failed Company failed. `company.user_id.reset` answers open issue #11's
//! request that a User be able to start over at a Company: it forgets the
//! User ID, its secret and the `session_id`, and drops the Session, so the
//! Company sees a new User — and loses the cart and conversations it kept for
//! the old one. The Company's MCP connections were the old User's, sessions
//! and all, so each is dialled afresh before anything more is sent to it. It
//! is an audit record.

use super::commands::refusal;
use crate::a2a::Principal;
use crate::a2a::errors::{self, reason};
use crate::runtime::reactor::Runtime;
use serde_json::Value;

/// What a `company.*` op produced. A build without `companies` serves
/// neither op, so produces neither.
#[cfg_attr(not(feature = "companies"), allow(dead_code))]
pub(super) enum CompanyAnswer {
    /// A read's document (the `Message` row).
    Doc(Value),
    /// Work done at once: the task's text and result (the `Task` row).
    Done(String, Value),
}

/// Run a `company.*` op for `principal`, whom the listener and the command
/// dispatch have both already held to the operator floor.
pub(super) fn handle(
    rt: &mut Runtime,
    principal: &Principal,
    op: &str,
    args: &Value,
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
            let reset = sup.reset_user(&company);
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
        _ => Err(super::commands::unknown_op(op)),
    }
    #[cfg(not(feature = "companies"))]
    {
        let _ = (rt, principal, args, bad);
        Err(super::commands::unknown_op(op))
    }
}
