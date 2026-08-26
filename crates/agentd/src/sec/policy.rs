// SPDX-License-Identifier: AGPL-3.0-only
//! **Tool-call policy**: an ordered list of operator verdicts on the call
//! itself — allow, deny, ask a person, or hold it and say so.
//!
//! Grants answer *may this caller reach this tool at all*, and they are name
//! patterns, so they cannot express "delete anything outside `/tmp`" or "a
//! person signs off before any egress-tagged call". `agent.approval` only
//! decides whether to honour a gate the MODEL asked for. This is the layer in
//! between: the arguments are already schema-validated one step earlier, so
//! judging them here costs nothing extra and is the only place it can happen.
//!
//! It composes with the machinery either side rather than duplicating it:
//! `tools.overrides` says WHERE a call goes, this says WHETHER, and an `ask`
//! verdict suspends on the same deferred-human path `ask_human` and the
//! `human` node already use.
//!
//! First match wins and no match is allow, so an empty list is exactly today's
//! behaviour and the common path pays one `is_empty` check.

use crate::config::v2::{Policy, PolicyAction, PolicyCaller};
use crate::sec::scope::TrifectaTag;
use serde_json::Value;

/// What a call looks like to the policy list.
pub struct Call<'a> {
    pub tool: &'a str,
    /// The trifecta tags the registry computed for this tool. Until now these
    /// were folded once at startup and then never consulted again.
    pub tags: &'a [TrifectaTag],
    pub caller: PolicyCaller,
    pub principal: Option<&'a str>,
    pub args: &'a Value,
}

/// The verdict, plus which rule produced it (for the log and the audit line —
/// "denied" without "by which rule" is not an answer an operator can act on).
pub struct Verdict {
    pub action: PolicyAction,
    pub rule: usize,
    pub question: Option<String>,
    pub on_timeout: PolicyAction,
    pub timeout_ms: Option<u64>,
}

/// The tag name an operator writes in `match: {tags: [...]}`.
fn tag_name(t: TrifectaTag) -> &'static str {
    match t {
        TrifectaTag::UntrustedInput => "untrusted_input",
        TrifectaTag::Sensitive => "sensitive",
        TrifectaTag::Egress => "egress",
    }
}

/// Whether a rule matches. Every present condition must hold — conditions are
/// ANDed, so adding one always narrows and never widens. That direction
/// matters: an operator adding `caller: [subagent]` to a `deny` rule expects
/// to be tightening their configuration, not loosening it.
fn matches(p: &Policy, call: &Call<'_>, cel_ok: &mut bool) -> bool {
    if let Some(pat) = &p.matcher.tool
        && !crate::registry::pattern_matches(pat, call.tool)
    {
        return false;
    }
    if !p.matcher.tags.is_empty() {
        let have: Vec<&str> = call.tags.iter().map(|t| tag_name(*t)).collect();
        if !p.matcher.tags.iter().all(|w| have.contains(&w.as_str())) {
            return false;
        }
    }
    if !p.matcher.caller.is_empty() && !p.matcher.caller.contains(&call.caller) {
        return false;
    }
    if let Some(pat) = &p.matcher.principal {
        match call.principal {
            None => return false,
            Some(id) if !crate::registry::pattern_matches(pat, id) => return false,
            Some(_) => {}
        }
    }
    if let Some(expr) = &p.matcher.args {
        let tool = Value::String(call.tool.to_string());
        let caller = Value::String(caller_name(call.caller).to_string());
        let vars: Vec<(&str, &Value)> =
            vec![("args", call.args), ("tool", &tool), ("caller", &caller)];
        match crate::cel::eval_bool(expr.trim().trim_start_matches("CEL:").trim(), &vars) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(_) => {
                // An argument guard that cannot be evaluated must not silently
                // become "no match" — that turns a `deny` into an allow at
                // exactly the moment it was supposed to bite. The caller
                // refuses the call outright.
                *cel_ok = false;
                return false;
            }
        }
    }
    true
}

pub fn caller_name(c: PolicyCaller) -> &'static str {
    match c {
        PolicyCaller::Root => "root",
        PolicyCaller::Workflow => "workflow",
        PolicyCaller::Subagent => "subagent",
    }
}

/// Evaluate the list. `Ok(None)` means no rule matched (allow);
/// `Err(rule)` means a rule's argument guard failed to evaluate, which is
/// fail-closed rather than a pass.
pub fn evaluate(policies: &[Policy], call: &Call<'_>) -> Result<Option<Verdict>, usize> {
    for (i, p) in policies.iter().enumerate() {
        let mut cel_ok = true;
        let hit = matches(p, call, &mut cel_ok);
        if !cel_ok {
            return Err(i);
        }
        if !hit {
            continue;
        }
        if p.action == PolicyAction::Allow {
            // An explicit allow stops the scan — that is what makes an
            // exception before a broad deny expressible at all.
            return Ok(Some(Verdict {
                action: PolicyAction::Allow,
                rule: i,
                question: None,
                on_timeout: PolicyAction::Deny,
                timeout_ms: None,
            }));
        }
        return Ok(Some(Verdict {
            action: p.action,
            rule: i,
            question: p.question.clone(),
            // A gate nobody answered has not been approved.
            on_timeout: p.on_timeout.unwrap_or(PolicyAction::Deny),
            timeout_ms: p.timeout.as_ref().map(|d| d.0.as_millis() as u64),
        }));
    }
    Ok(None)
}

/// Whether any rule could apply to this tool for this caller, ignoring the
/// argument guard (which needs the actual call).
///
/// Used to decide which tools a turn worker must round-trip for. A child
/// dials its MCP tools DIRECTLY from its route map and never reaches
/// `execute_tool`, so a policy that covered root turns but not subagent turns
/// would be worse than none: the operator would believe they were covered.
/// Anything a rule might touch is moved out of the child's route map and
/// served by the runtime instead, so gated tools pay one round-trip and
/// everything else keeps the fast path.
pub fn could_apply(
    policies: &[Policy],
    tool: &str,
    tags: &[TrifectaTag],
    caller: PolicyCaller,
) -> bool {
    policies.iter().any(|p| {
        if let Some(pat) = &p.matcher.tool
            && !crate::registry::pattern_matches(pat, tool)
        {
            return false;
        }
        if !p.matcher.tags.is_empty() {
            let have: Vec<&str> = tags.iter().map(|t| tag_name(*t)).collect();
            if !p.matcher.tags.iter().all(|w| have.contains(&w.as_str())) {
                return false;
            }
        }
        if !p.matcher.caller.is_empty() && !p.matcher.caller.contains(&caller) {
            return false;
        }
        // `principal` and `args` are call-time facts, so a rule carrying them
        // is treated as "might apply" — the conservative direction.
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::v2::PolicyMatch;

    fn pol(m: PolicyMatch, a: PolicyAction) -> Policy {
        Policy {
            matcher: m,
            action: a,
            ..Default::default()
        }
    }

    fn call<'a>(
        tool: &'a str,
        tags: &'a [TrifectaTag],
        caller: PolicyCaller,
        args: &'a Value,
    ) -> Call<'a> {
        Call {
            tool,
            tags,
            caller,
            principal: None,
            args,
        }
    }

    #[test]
    fn no_rules_is_allow_and_costs_nothing() {
        let args = Value::Null;
        let c = call("anything", &[], PolicyCaller::Root, &args);
        assert!(evaluate(&[], &c).unwrap().is_none());
    }

    #[test]
    fn first_match_wins_so_an_exception_can_precede_a_broad_deny() {
        let args = Value::Null;
        let rules = vec![
            pol(
                PolicyMatch {
                    tool: Some("fs.read".into()),
                    ..Default::default()
                },
                PolicyAction::Allow,
            ),
            pol(
                PolicyMatch {
                    tool: Some("fs.*".into()),
                    ..Default::default()
                },
                PolicyAction::Deny,
            ),
        ];
        let v = evaluate(&rules, &call("fs.read", &[], PolicyCaller::Root, &args))
            .unwrap()
            .expect("matched");
        assert_eq!(v.action, PolicyAction::Allow);
        let v = evaluate(&rules, &call("fs.delete", &[], PolicyCaller::Root, &args))
            .unwrap()
            .expect("matched");
        assert_eq!(v.action, PolicyAction::Deny);
    }

    /// Tags finally do something at runtime. Every listed tag must be present,
    /// so `tags: [sensitive, egress]` is the pair, not either one.
    #[test]
    fn tag_conditions_require_all_of_them() {
        let args = Value::Null;
        let rules = vec![pol(
            PolicyMatch {
                tags: vec!["sensitive".into(), "egress".into()],
                ..Default::default()
            },
            PolicyAction::Deny,
        )];
        let both = [TrifectaTag::Sensitive, TrifectaTag::Egress];
        let one = [TrifectaTag::Egress];
        assert!(
            evaluate(&rules, &call("t", &both, PolicyCaller::Root, &args))
                .unwrap()
                .is_some()
        );
        assert!(
            evaluate(&rules, &call("t", &one, PolicyCaller::Root, &args))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn caller_narrows_rather_than_widens() {
        let args = Value::Null;
        let rules = vec![pol(
            PolicyMatch {
                tool: Some("*".into()),
                caller: vec![PolicyCaller::Subagent],
                ..Default::default()
            },
            PolicyAction::Deny,
        )];
        assert!(
            evaluate(&rules, &call("t", &[], PolicyCaller::Subagent, &args))
                .unwrap()
                .is_some()
        );
        assert!(
            evaluate(&rules, &call("t", &[], PolicyCaller::Root, &args))
                .unwrap()
                .is_none()
        );
    }

    /// The conservative direction: a rule that might apply once the arguments
    /// are known must pull the tool out of the child's direct route map, or
    /// the gate silently misses every call the child makes.
    #[test]
    fn could_apply_is_conservative_about_call_time_facts() {
        let rules = vec![pol(
            PolicyMatch {
                tool: Some("fs.*".into()),
                args: Some("CEL: args.path != '/tmp'".into()),
                ..Default::default()
            },
            PolicyAction::Deny,
        )];
        assert!(could_apply(
            &rules,
            "fs.delete",
            &[],
            PolicyCaller::Subagent
        ));
        assert!(!could_apply(
            &rules,
            "http.get",
            &[],
            PolicyCaller::Subagent
        ));
    }

    /// An argument guard that will not evaluate is fail-closed. Returning "no
    /// match" would turn a deny into an allow at exactly the moment it was
    /// meant to bite.
    #[test]
    #[cfg(feature = "cel")]
    fn an_unevaluatable_argument_guard_fails_closed() {
        let args = serde_json::json!({"path": "/etc"});
        let rules = vec![pol(
            PolicyMatch {
                tool: Some("*".into()),
                args: Some("CEL: this is not an expression((".into()),
                ..Default::default()
            },
            PolicyAction::Deny,
        )];
        assert!(evaluate(&rules, &call("t", &[], PolicyCaller::Root, &args)).is_err());
    }
}
