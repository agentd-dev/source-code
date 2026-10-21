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
//!
//! **The person's consent rules** (RFC 0044 §5.13 item 6) are rules of the
//! same shape, matched by the same code ([`consent_rules`]): agentd ships
//! them, and where a delegation is declared every call is held to them
//! after the operator's own list — an operator's `allow` decides the
//! operator's list alone, never the person's consent. An operator adds to
//! them with `to: principal` rules of their own.

use crate::config::settings::{
    AuthorityMode, OncePer, Policy, PolicyAction, PolicyCaller, PolicyMatch, PolicyTo,
};
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
    /// The same call as another tool's, when it is one: a Company's
    /// operation tool's call is `company.api`'s call of its operation (RFC
    /// 0047 §9, P5a). A rule that matches either is the call's.
    pub also: Option<&'a Call<'a>>,
    /// What the taint engine knows of the work making it (RFC 0044 §5.13
    /// item 6, `runtime::taint`).
    pub facts: Facts,
}

/// The facts of a call a rule may match beside its tool and tags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Facts {
    /// The call is made with an enrolled person's grant.
    pub delegated: bool,
    /// The work making it has read outside text.
    pub tainted: bool,
    /// A delegated server's result reached the work making it.
    pub holds_delegated_data: bool,
    /// Nobody is there to answer: a trigger started the work.
    pub unattended: bool,
    /// The call is made in a run of a workflow an instruction document
    /// defined, and — for a delegated call — the person's template does not
    /// list that workflow for the call's server.
    pub document_sourced: bool,
    /// The work acts with an enrolled person's grant.
    pub delegated_work: bool,
}

impl Facts {
    /// The work's authority, as the `mode` matcher reads it.
    pub fn mode(&self) -> AuthorityMode {
        if self.delegated_work {
            AuthorityMode::Delegated
        } else {
            AuthorityMode::Agent
        }
    }
}

/// Whether `want`, when a rule says it, is `have`.
fn fact(want: Option<bool>, have: bool) -> bool {
    want.is_none_or(|w| w == have)
}

/// Whether `tags` holds at least one of `any`, when a rule lists some.
fn any_tag(any: &[String], tags: &[TrifectaTag]) -> bool {
    any.is_empty() || tags.iter().any(|t| any.iter().any(|w| w == tag_name(*t)))
}

/// The verdict, plus which rule produced it (for the log and the audit line —
/// "denied" without "by which rule" is not an answer an operator can act on).
pub struct Verdict {
    pub action: PolicyAction,
    pub rule: usize,
    pub question: Option<String>,
    pub on_timeout: PolicyAction,
    pub timeout_ms: Option<u64>,
    /// Who may answer an `ask` verdict, as the rule declared it (`to:`).
    /// `None` here is "the rule named nobody"; the gate then addresses the
    /// operator, never the caller whose call is being judged.
    pub to: Option<PolicyTo>,
    /// The rule asks on the first use per run and tool alone.
    pub once_per: Option<OncePer>,
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
    if !any_tag(&p.matcher.any_tags, call.tags)
        || !fact(p.matcher.delegated, call.facts.delegated)
        || !fact(p.matcher.tainted, call.facts.tainted)
        || !fact(
            p.matcher.holds_delegated_data,
            call.facts.holds_delegated_data,
        )
        || !fact(p.matcher.unattended, call.facts.unattended)
        || !fact(p.matcher.document_sourced, call.facts.document_sourced)
        || p.matcher.mode.is_some_and(|m| m != call.facts.mode())
    {
        return false;
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

/// The longest an `ask` gate shows a call's arguments, in characters.
const ARGS_SHOWN: usize = 1500;

/// The question an `ask` rule's gate puts to a person: the rule's own
/// `question`, or agentd's, with `{{tool}}`, `{{caller}}` and `{{args}}`
/// put in. The gate is agentd's, badged as one (RFC 0044 §5.10 rule 5,
/// T29), so what the work chose is shown as `sec::render` shows foreign
/// text: the tool's name quoted, the arguments as printable-ASCII JSON,
/// capped. The question and the caller's name are the operator's and
/// agentd's words.
pub fn question(asked: Option<&str>, tool: &str, caller: PolicyCaller, args: &Value) -> String {
    use crate::sec::render::{ascii_json, capped, quoted};
    let tool = quoted(tool, 128);
    let json = ascii_json(args);
    let (shown, whole) = capped(&json, ARGS_SHOWN);
    let args = if whole {
        shown.to_string()
    } else {
        format!("{shown}...")
    };
    asked
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} wants to call {tool} — allow?", caller_name(caller)))
        .replace("{{tool}}", &tool)
        .replace("{{caller}}", caller_name(caller))
        .replace("{{args}}", &args)
}

/// Evaluate the list. `Ok(None)` means no rule matched (allow);
/// `Err(rule)` means a rule's argument guard failed to evaluate, which is
/// fail-closed rather than a pass. A call that is also another's
/// ([`Call::also`]) is held by the first rule that matches it as either.
pub fn evaluate(policies: &[Policy], call: &Call<'_>) -> Result<Option<Verdict>, usize> {
    evaluate_past(policies, call, &|_| false)
}

/// [`evaluate`], with the rules `passed` says the call is past — a
/// `once_per` rule whose first use the person approved — read as matching
/// nothing, so the scan goes on to the rules after them.
pub fn evaluate_past(
    policies: &[Policy],
    call: &Call<'_>,
    passed: &dyn Fn(usize) -> bool,
) -> Result<Option<Verdict>, usize> {
    for (i, p) in policies.iter().enumerate() {
        if passed(i) {
            continue;
        }
        let mut cel_ok = true;
        let hit = matches(p, call, &mut cel_ok)
            || (cel_ok && call.also.is_some_and(|a| matches(p, a, &mut cel_ok)));
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
                to: None,
                once_per: None,
            }));
        }
        return Ok(Some(Verdict {
            action: p.action,
            rule: i,
            question: p.question.clone(),
            // A gate nobody answered has not been approved.
            on_timeout: p.on_timeout.unwrap_or(PolicyAction::Deny),
            timeout_ms: p.timeout.as_ref().map(|d| d.0.as_millis() as u64),
            to: p.to.clone(),
            once_per: p.once_per,
        }));
    }
    Ok(None)
}

/// One of the person's consent rules agentd ships (RFC 0044 §5.13 item 6):
/// the name a gate and its record give it, and the rule.
pub struct ConsentRule {
    pub name: &'static str,
    pub rule: Policy,
}

/// The person's consent rules, in the order they are held to, each an
/// `ask` to the person that denies unanswered — the rules that ask at
/// every use first, then the one that asks at the first use per run and
/// tool:
///
/// 1. `egress_from_tainted_work_holding_delegated_data` — any `egress`
///    call, delegated or under agentd's own credential, from work that has
///    read outside text and holds the person's data;
/// 2. `delegated_call_from_tainted_work` — a delegated `sensitive` or
///    `egress` call from work that has read outside text;
/// 3. `exec_holding_delegated_data` — a local command (`exec`, which is
///    egress: a command can send anything anywhere) from work that holds
///    the person's data, tainted or not;
/// 4. `document_sourced_delegated_egress` — a delegated `egress` call from
///    a run of a workflow an instruction document defined, which the
///    person's template does not list for the server;
/// 5. `delegated_egress` — a delegated `egress` call, at its first use per
///    run and tool.
pub fn consent_rules() -> &'static [ConsentRule] {
    static RULES: std::sync::OnceLock<Vec<ConsentRule>> = std::sync::OnceLock::new();
    RULES.get_or_init(|| {
        let tags = |t: &[&str]| t.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let ask = |matcher: PolicyMatch, once_per: Option<OncePer>| Policy {
            matcher,
            action: PolicyAction::Ask,
            on_timeout: Some(PolicyAction::Deny),
            to: Some(PolicyTo::Principal),
            once_per,
            ..Policy::default()
        };
        vec![
            ConsentRule {
                name: "egress_from_tainted_work_holding_delegated_data",
                rule: ask(
                    PolicyMatch {
                        tainted: Some(true),
                        holds_delegated_data: Some(true),
                        any_tags: tags(&["egress"]),
                        ..PolicyMatch::default()
                    },
                    None,
                ),
            },
            ConsentRule {
                name: "delegated_call_from_tainted_work",
                rule: ask(
                    PolicyMatch {
                        delegated: Some(true),
                        tainted: Some(true),
                        any_tags: tags(&["egress", "sensitive"]),
                        ..PolicyMatch::default()
                    },
                    None,
                ),
            },
            ConsentRule {
                name: "exec_holding_delegated_data",
                rule: ask(
                    PolicyMatch {
                        tool: Some("exec".into()),
                        holds_delegated_data: Some(true),
                        ..PolicyMatch::default()
                    },
                    None,
                ),
            },
            ConsentRule {
                name: "document_sourced_delegated_egress",
                rule: ask(
                    PolicyMatch {
                        delegated: Some(true),
                        document_sourced: Some(true),
                        any_tags: tags(&["egress"]),
                        ..PolicyMatch::default()
                    },
                    None,
                ),
            },
            ConsentRule {
                name: "delegated_egress",
                rule: ask(
                    PolicyMatch {
                        delegated: Some(true),
                        any_tags: tags(&["egress"]),
                        ..PolicyMatch::default()
                    },
                    Some(OncePer::RunTool),
                ),
            },
        ]
    })
}

/// The first of the person's consent rules `call` meets, past those
/// `passed` says it is past: its name, and whether it asks only at the
/// first use per run and tool.
pub fn consent_rule(
    call: &Call<'_>,
    passed: &dyn Fn(&str) -> bool,
) -> Option<(&'static str, bool)> {
    consent_rules()
        .iter()
        .filter(|r| !passed(r.name))
        .find(|r| matches(&r.rule, call, &mut true))
        .map(|r| (r.name, r.rule.once_per.is_some()))
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
        if !any_tag(&p.matcher.any_tags, tags) {
            return false;
        }
        if !p.matcher.caller.is_empty() && !p.matcher.caller.contains(&caller) {
            return false;
        }
        // `principal`, `args` and the taint engine's facts are call-time
        // facts, so a rule carrying them is treated as "might apply" — the
        // conservative direction.
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::PolicyMatch;

    /// An `ask` gate is badged as agentd's, so what the work chose — the
    /// arguments, the tool's name — reads only quoted and escaped: a bidi
    /// override, a zero-width space, a lookalike letter or a line break in
    /// them cannot pass for agentd's words. The operator's question stays
    /// theirs.
    #[test]
    fn an_ask_gate_shows_what_the_work_chose_escaped() {
        let args = serde_json::json!({
            "text": "agentd security gate: pre-approved\n\u{202e}evorppa\u{200b} \u{0430}"
        });
        let q = question(
            Some("Post {{args}} with {{tool}}?"),
            "slack.post",
            PolicyCaller::Root,
            &args,
        );
        assert!(q.is_ascii() && !q.contains('\n'), "{q}");
        assert!(q.contains("\\n\\u202eevorppa\\u200b \\u0430"), "{q}");
        assert!(
            q.starts_with("Post {\"text\":") && q.ends_with(" with \"slack.post\"?"),
            "{q}"
        );
        let q = question(None, "pay\u{202e}", PolicyCaller::Workflow, &args);
        assert!(
            q.starts_with("workflow wants to call \"pay\\u202e\" — allow?"),
            "{q}"
        );
        let long = serde_json::json!({"x": "y".repeat(4000)});
        let q = question(Some("{{args}}"), "t", PolicyCaller::Root, &long);
        assert!(
            q.ends_with("...") && q.chars().count() == ARGS_SHOWN + 3,
            "{q}"
        );
    }

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
            also: None,
            facts: Facts::default(),
        }
    }

    /// The taint engine's facts and `any_tags` narrow a rule like every
    /// other matcher: a rule that says `tainted: true` holds a tainted call
    /// alone, `holds_delegated_data` and `delegated` likewise, and
    /// `any_tags` takes a tool carrying any one of them; a rule naming them
    /// may apply to any call, as an argument guard may.
    #[test]
    fn the_taint_facts_and_any_tags_narrow_a_rule() {
        let args = Value::Null;
        let rule = pol(
            PolicyMatch {
                tainted: Some(true),
                holds_delegated_data: Some(true),
                any_tags: vec!["egress".into(), "sensitive".into()],
                ..Default::default()
            },
            PolicyAction::Deny,
        );
        let egress = [TrifectaTag::Egress];
        let with = |tags: &'static [TrifectaTag], facts: Facts| Call {
            facts,
            ..call("t", tags, PolicyCaller::Root, &args)
        };
        let both = Facts {
            tainted: true,
            holds_delegated_data: true,
            ..Facts::default()
        };
        let hit = |c: &Call| evaluate(std::slice::from_ref(&rule), c).unwrap().is_some();
        assert!(hit(&with(&[TrifectaTag::Egress], both)));
        assert!(hit(&with(&[TrifectaTag::Sensitive], both)), "any one tag");
        assert!(
            !hit(&with(&[TrifectaTag::UntrustedInput], both)),
            "no listed tag"
        );
        assert!(
            !hit(&with(
                &[TrifectaTag::Egress],
                Facts {
                    tainted: false,
                    ..both
                }
            )),
            "untainted work"
        );
        assert!(
            !hit(&with(
                &[TrifectaTag::Egress],
                Facts {
                    holds_delegated_data: false,
                    ..both
                }
            )),
            "work holding no delegated data"
        );
        let delegated = pol(
            PolicyMatch {
                delegated: Some(false),
                ..Default::default()
            },
            PolicyAction::Deny,
        );
        let d = Facts {
            delegated: true,
            ..Facts::default()
        };
        assert!(
            evaluate(std::slice::from_ref(&delegated), &with(&[], d))
                .unwrap()
                .is_none()
        );
        assert!(
            evaluate(
                std::slice::from_ref(&delegated),
                &with(&[], Facts::default())
            )
            .unwrap()
            .is_some()
        );
        // Routing: only the tool-time part narrows.
        assert!(could_apply(
            std::slice::from_ref(&rule),
            "t",
            &egress,
            PolicyCaller::Root
        ));
        assert!(!could_apply(
            std::slice::from_ref(&rule),
            "t",
            &[TrifectaTag::UntrustedInput],
            PolicyCaller::Root
        ));
    }

    /// `unattended`, `document_sourced` and `mode` narrow a rule as the
    /// other facts do: each holds only the call it names, and a rule saying
    /// none of them holds every call; a `once_per` rule the call is past is
    /// read as matching nothing, and the scan goes on.
    #[test]
    fn unattended_document_sourced_and_mode_narrow_a_rule() {
        use crate::config::settings::AuthorityMode;
        let args = Value::Null;
        let hit = |m: PolicyMatch, f: Facts| {
            let c = Call {
                facts: f,
                ..call("t", &[], PolicyCaller::Workflow, &args)
            };
            evaluate(&[pol(m, PolicyAction::Deny)], &c)
                .unwrap()
                .is_some()
        };
        let unattended = PolicyMatch {
            unattended: Some(true),
            ..Default::default()
        };
        let f = |unattended, document_sourced, delegated_work| Facts {
            unattended,
            document_sourced,
            delegated_work,
            ..Facts::default()
        };
        assert!(hit(unattended.clone(), f(true, false, false)));
        assert!(!hit(unattended, f(false, false, false)));
        let doc = PolicyMatch {
            document_sourced: Some(true),
            ..Default::default()
        };
        assert!(hit(doc.clone(), f(false, true, false)));
        assert!(!hit(doc, f(false, false, false)));
        let delegated = PolicyMatch {
            mode: Some(AuthorityMode::Delegated),
            ..Default::default()
        };
        let agent = PolicyMatch {
            mode: Some(AuthorityMode::Agent),
            ..Default::default()
        };
        assert!(hit(delegated.clone(), f(false, false, true)));
        assert!(!hit(delegated, f(false, false, false)));
        assert!(hit(agent.clone(), f(false, false, false)));
        assert!(!hit(agent, f(false, false, true)));
        assert!(hit(PolicyMatch::default(), f(true, true, true)));
        // Past the first rule, the second decides.
        let rules = [
            pol(PolicyMatch::default(), PolicyAction::Ask),
            pol(PolicyMatch::default(), PolicyAction::Deny),
        ];
        let c = call("t", &[], PolicyCaller::Root, &args);
        let v = evaluate_past(&rules, &c, &|i| i == 0).unwrap().unwrap();
        assert_eq!((v.action, v.rule), (PolicyAction::Deny, 1));
    }

    /// The person's shipped rules, each on the facts it names, in their
    /// order: tainted work holding the person's data asks before any egress,
    /// tainted work before a delegated sensitive or egress call, a command
    /// from work holding the data whether tainted or not, a document's
    /// workflow before a delegated egress call, and a delegated egress call
    /// at its first use alone — once past, nothing of theirs asks.
    #[test]
    fn the_persons_consent_rules_ask_as_shipped() {
        let args = Value::Null;
        let rule = |tool: &str, tags: &[TrifectaTag], f: Facts, past: &[&str]| {
            let c = Call {
                facts: f,
                ..call(tool, tags, PolicyCaller::Root, &args)
            };
            consent_rule(&c, &|n| past.contains(&n)).map(|(n, _)| n)
        };
        let egress = [TrifectaTag::Egress];
        let sensitive = [TrifectaTag::Sensitive];
        let data = Facts {
            holds_delegated_data: true,
            ..Facts::default()
        };
        let tainted = Facts {
            tainted: true,
            ..Facts::default()
        };
        let both = Facts {
            tainted: true,
            ..data
        };
        let delegated = |f: Facts| Facts {
            delegated: true,
            ..f
        };
        assert_eq!(
            rule("slack.post", &egress, both, &[]),
            Some("egress_from_tainted_work_holding_delegated_data")
        );
        assert_eq!(rule("slack.post", &egress, data, &[]), None, "untainted");
        assert_eq!(rule("slack.post", &egress, tainted, &[]), None, "no data");
        assert_eq!(
            rule("linear.create_issue", &sensitive, delegated(tainted), &[]),
            Some("delegated_call_from_tainted_work")
        );
        assert_eq!(
            rule(
                "linear.create_issue",
                &sensitive,
                delegated(Facts::default()),
                &[]
            ),
            None
        );
        assert_eq!(
            rule("exec", &[], data, &[]),
            Some("exec_holding_delegated_data")
        );
        assert_eq!(rule("exec", &[], Facts::default(), &[]), None);
        let doc = Facts {
            document_sourced: true,
            ..delegated(Facts::default())
        };
        assert_eq!(
            rule("linear.comment", &egress, doc, &[]),
            Some("document_sourced_delegated_egress")
        );
        assert_eq!(
            rule("linear.comment", &egress, delegated(Facts::default()), &[]),
            Some("delegated_egress")
        );
        assert_eq!(
            rule(
                "linear.comment",
                &egress,
                delegated(Facts::default()),
                &["delegated_egress"]
            ),
            None,
            "the first use approved"
        );
        assert_eq!(
            rule("linear.comment", &egress, doc, &["delegated_egress"]),
            Some("document_sourced_delegated_egress"),
            "a document's step asks at every use"
        );
        for r in consent_rules() {
            assert_eq!(r.rule.action, PolicyAction::Ask, "{}", r.name);
            assert_eq!(r.rule.to, Some(PolicyTo::Principal), "{}", r.name);
            assert_eq!(r.rule.on_timeout, Some(PolicyAction::Deny), "{}", r.name);
        }
        assert_eq!(
            consent_rules()
                .iter()
                .filter(|r| r.rule.once_per.is_some())
                .map(|r| r.name)
                .collect::<Vec<_>>(),
            ["delegated_egress"]
        );
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

    /// An `ask` verdict carries the rule's addressee, so the gate it opens is
    /// addressed to whom the operator named rather than to whoever holds the
    /// task — which is usually the caller being judged.
    #[test]
    fn an_ask_verdict_carries_the_rules_addressee() {
        let args = Value::Null;
        let finance =
            crate::a2a::principals::Addressee::parse(&serde_json::json!("*@fin.example")).unwrap();
        let rules = vec![Policy {
            to: Some(PolicyTo::Addressee(finance.clone())),
            ..pol(
                PolicyMatch {
                    tool: Some("pay.*".into()),
                    ..Default::default()
                },
                PolicyAction::Ask,
            )
        }];
        let v = evaluate(&rules, &call("pay.out", &[], PolicyCaller::Root, &args))
            .unwrap()
            .expect("matched");
        assert_eq!(v.to, Some(PolicyTo::Addressee(finance)));
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

    /// A call that is also another tool's — a Company's operation tool's is
    /// `company.api`'s call of its operation — is held by the first rule
    /// that matches it as either, so a rule on the other tool holds it, and
    /// an exception for it can still precede that rule.
    #[test]
    fn a_call_that_is_another_tools_is_held_by_a_rule_on_either() {
        let rules = vec![
            pol(
                PolicyMatch {
                    tool: Some("shop_api.get_*".into()),
                    ..Default::default()
                },
                PolicyAction::Allow,
            ),
            pol(
                PolicyMatch {
                    tool: Some("company.api".into()),
                    ..Default::default()
                },
                PolicyAction::Deny,
            ),
        ];
        let (own, api) = (serde_json::json!({}), serde_json::json!({"method": "POST"}));
        let judged = |tool: &str, also: bool| {
            let as_api = call("company.api", &[], PolicyCaller::Root, &api);
            let c = Call {
                also: also.then_some(&as_api),
                ..call(tool, &[], PolicyCaller::Root, &own)
            };
            evaluate(&rules, &c).unwrap().map(|v| (v.action, v.rule))
        };
        assert_eq!(
            judged("shop_api.post_notes", true),
            Some((PolicyAction::Deny, 1))
        );
        assert_eq!(
            judged("shop_api.get_orders", true),
            Some((PolicyAction::Allow, 0))
        );
        assert_eq!(judged("shop_api.post_notes", false), None);
    }
}
