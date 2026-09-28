// SPDX-License-Identifier: AGPL-3.0-only
//! Resolving a caller to a principal from the configured `a2a.principals`.

use super::{Principal, glob};
use crate::config::v2::{self, Role};
use crate::sec::secret;
use serde_json::{Value, json};

/// What the transport learned about the caller.
#[derive(Debug, Clone, Default)]
pub struct CallerIdentity {
    /// The verified client-cert subject/SANs (mTLS).
    pub sans: Vec<String>,
    pub subject: Option<String>,
    /// A verified bearer subject (post token check), if the transport resolved it.
    pub bearer_ref: Option<String>,
    /// Whether the connection is loopback (dev operator default).
    pub loopback: bool,
    /// Whether the framework already authenticated the peer as management
    /// (a verified client cert / matched bearer).
    pub management: bool,
}

/// Resolves a caller to a principal from the configured `a2a.principals`.
pub struct Resolver {
    principals: Vec<Compiled>,
    /// A bearer whose match resolves to the operator, when `a2a.bearer` is set
    /// and no principal claims it (the loopback/single-operator default).
    default_operator_on_bearer: bool,
    /// Whether an unconfigured deployment treats a loopback caller as the
    /// operator. True only while `a2a.principals` is empty: once an operator
    /// has written any rule, the implicit local operator disappears rather
    /// than sitting behind their matrix as a way in.
    loopback_operator: bool,
}

struct Compiled {
    /// The rule's declared `id`, which names its callers when set.
    id: Option<String>,
    matcher: v2::PrincipalMatch,
    role: Role,
    grants: Vec<String>,
    rate: Option<String>,
    budget: Option<v2::Budget>,
    labels: std::collections::BTreeMap<String, String>,
    bearer_secret: Option<String>,
}

impl Resolver {
    /// Build from settings, resolving `bearer_ref` secrets at startup.
    pub fn build(a2a: &v2::A2a, env: &dyn Fn(&str) -> Option<String>) -> Result<Resolver, String> {
        let mut principals = Vec::new();
        for p in &a2a.principals {
            let bearer_secret = match &p.matcher.bearer_ref {
                Some(r) => Some(
                    secret::resolve(r, env)
                        .map_err(|e| format!("a2a principal bearer_ref: {e}"))?,
                ),
                None => None,
            };
            principals.push(Compiled {
                id: p.id.clone(),
                matcher: p.matcher.clone(),
                role: p.role,
                grants: p.grants.clone(),
                rate: p.quotas.as_ref().and_then(|q| q.rate.clone()),
                budget: p.quotas.as_ref().and_then(|q| q.budget.clone()),
                labels: p.labels.clone(),
                bearer_secret,
            });
        }
        Ok(Resolver {
            principals,
            default_operator_on_bearer: a2a.bearer.is_some(),
            loopback_operator: a2a.principals.is_empty(),
        })
    }

    /// Resolve a caller. Matching order: explicit principal rules (first match),
    /// then the operator defaults (verified management / loopback), then
    /// anonymous.
    pub fn resolve(&self, id: &CallerIdentity, presented_bearer: Option<&str>) -> Principal {
        for c in &self.principals {
            if let Some(p) = c.matches(id, presented_bearer) {
                return p;
            }
        }
        // A configured `a2a.bearer` (server bearer) that the transport matched
        // ⇒ operator, unless a principal already claimed the connection.
        if id.management && (self.default_operator_on_bearer || self.loopback_operator) {
            return operator();
        }
        if id.loopback && self.loopback_operator {
            return operator();
        }
        Principal::anonymous()
    }

    /// A status view of the configured principals.
    pub fn status(&self) -> Value {
        json!({
            "principals": self.principals.iter().map(|c| json!({"role": format!("{:?}", c.role).to_lowercase(), "match": matcher_desc(&c.matcher), "grants": c.grants})).collect::<Vec<_>>(),
            "loopback_operator": self.loopback_operator,
        })
    }
}

impl Compiled {
    fn matches(&self, id: &CallerIdentity, presented_bearer: Option<&str>) -> Option<Principal> {
        let m = &self.matcher;
        let hit = if m.any {
            true
        } else if let Some(san) = &m.san {
            id.sans.iter().any(|s| glob(san, s))
                || id.subject.as_deref().is_some_and(|s| glob(san, s))
        } else if let Some(sub) = &m.sub {
            id.subject.as_deref().is_some_and(|s| s == sub)
                || id.bearer_ref.as_deref().is_some_and(|b| b == sub)
        } else if m.bearer_ref.is_some() {
            match (&self.bearer_secret, presented_bearer) {
                (Some(secret), Some(got)) => ct_eq(secret.as_bytes(), got.as_bytes()),
                _ => false,
            }
        } else {
            false
        };
        if !hit {
            return None;
        }
        // Evidence that names nobody, under a rule that names nobody, is not a
        // caller this rule can own work for. Validation requires an `id` on
        // every rule whose evidence is anonymous, so this is the backstop that
        // keeps `user:unknown` — one principal every such caller merged into —
        // impossible rather than merely unconfigured.
        let pid = principal_id(self.role, self.id.as_deref(), id)?;
        Some(Principal {
            id: pid,
            role: self.role,
            grants: self.grants.clone(),
            rate: self.rate.clone(),
            budget: self.budget.clone(),
            labels: self.labels.clone(),
        })
    }
}

fn operator() -> Principal {
    Principal {
        id: "operator".into(),
        role: Role::Operator,
        grants: vec!["*".into()],
        rate: None,
        budget: None,
        labels: Default::default(),
    }
}

/// The principal id a matched rule's caller acts as.
///
/// A declared id wins: it is what the operator chose to own this caller's
/// tasks, conversations and rate bucket by. Without one, a certificate names
/// its holder — but as `cn=<CN>` or `san=<SAN>`, never bare. `=` is in neither
/// the declared-id charset nor a device-approval name's, so a certificate whose
/// CN happens to equal a declared id (`deploy-bot`) can never spell
/// `user:deploy-bot` and inherit that principal's work.
fn principal_id(role: Role, declared: Option<&str>, id: &CallerIdentity) -> Option<String> {
    let role_name = match role {
        Role::Operator => return Some("operator".into()),
        Role::Anonymous => return Some("anonymous".into()),
        Role::User => "user",
        Role::Agent => "agent",
    };
    let name = match declared {
        Some(d) => d.to_string(),
        None => match (&id.subject, id.sans.first()) {
            (Some(cn), _) => format!("cn={cn}"),
            (None, Some(san)) => format!("san={san}"),
            (None, None) => return None,
        },
    };
    Some(format!("{role_name}:{name}"))
}

fn matcher_desc(m: &v2::PrincipalMatch) -> Value {
    if m.any {
        json!({"any": true})
    } else if let Some(s) = &m.san {
        json!({"san": s})
    } else if let Some(s) = &m.sub {
        json!({"sub": s})
    } else if m.bearer_ref.is_some() {
        json!({"bearer_ref": "***"})
    } else {
        json!({})
    }
}

/// Constant-time byte compare.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    d == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a2a(doc: Value) -> v2::A2a {
        serde_json::from_value(doc).unwrap()
    }
    fn ident(sans: &[&str], sub: Option<&str>, mgmt: bool, loopback: bool) -> CallerIdentity {
        CallerIdentity {
            sans: sans.iter().map(|s| s.to_string()).collect(),
            subject: sub.map(str::to_string),
            management: mgmt,
            loopback,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_roles_and_enforces_the_matrix() {
        let r = Resolver::build(
            &a2a(json!({
                "principals": [
                    {"match": {"san": "spiffe://ops/*"}, "role": "operator"},
                    {"match": {"san": "spiffe://team/*"}, "role": "user", "grants": ["knowledge.*"]},
                    {"id": "peer", "match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent"},
                    {"id": "anyone", "match": {"any": true}, "role": "anonymous"}
                ]
            })),
            &|k| (k == "PEER").then(|| "s3cr3t".to_string()),
        )
        .unwrap();
        let op = r.resolve(&ident(&["spiffe://ops/admin"], None, true, false), None);
        assert!(op.is_operator());
        assert!(op.may("SendMessage", Some("a2a.Drain")) || op.may_command("workflow.delete"));
        let user = r.resolve(&ident(&["spiffe://team/alice"], None, true, false), None);
        assert_eq!(user.role, Role::User);
        assert_eq!(user.id, "user:san=spiffe://team/alice");
        assert!(user.may("SendMessage", None), "NL is allowed");
        assert!(
            user.may_command("status")
                && user.may_command("workflow.run")
                && user.may_command("knowledge.search")
        );
        assert!(
            !user.may_command("workflow.delete"),
            "not granted to a user"
        );
        assert!(!user.may("a2a.Drain", None), "admin is operator-only");
        let agent = r.resolve(&ident(&[], None, false, false), Some("s3cr3t"));
        assert_eq!(agent.role, Role::Agent);
        assert_eq!(
            agent.id, "agent:peer",
            "a bearer rule acts as its declared id"
        );
        assert!(agent.may_command("workflow.run") && !agent.may_command("subagent.send"));
        assert!(
            r.resolve(&ident(&[], None, false, false), Some("wrong"))
                .is_anonymous()
        );
        let anon = r.resolve(&ident(&["spiffe://other/x"], None, false, false), None);
        assert!(anon.is_anonymous());
        assert!(!anon.may("SendMessage", None) && !anon.may_command("status"));
    }

    #[test]
    fn loopback_and_bearer_defaults() {
        // No principals configured + loopback ⇒ operator.
        let r = Resolver::build(&a2a(json!({})), &|_| None).unwrap();
        assert!(r.resolve(&ident(&[], None, true, true), None).is_operator());
        assert!(
            r.resolve(&ident(&[], None, false, false), None)
                .is_anonymous(),
            "non-loopback without a match is anonymous"
        );
        // A server bearer the transport matched ⇒ operator.
        let r = Resolver::build(&a2a(json!({"bearer": "{{secret:B}}"})), &|k| {
            (k == "B").then(|| "t".to_string())
        })
        .unwrap();
        assert!(
            r.resolve(&ident(&[], None, true, false), None)
                .is_operator()
        );
        assert!(glob("a*c", "abc") && glob("*", "x") && !glob("a*c", "abx"));
    }
    /// A certificate can never spell a declared principal id.
    ///
    /// The rule `{id: deploy-bot, match: {bearer_ref}}` owns everything its
    /// bearer started. A certificate whose CN — or first SAN — is `deploy-bot`,
    /// admitted by an unrelated `san` rule without an id, used to resolve to
    /// `user:deploy-bot` as well, and so read and cancelled that principal's
    /// tasks and conversations and shared its rate bucket. The `cn=`/`san=`
    /// marker keeps derived ids in a namespace no declared id can enter.
    #[test]
    fn a_certificate_id_never_collides_with_a_declared_one() {
        let r = Resolver::build(
            &a2a(json!({
                "principals": [
                    {"id": "deploy-bot", "match": {"bearer_ref": "{{secret:DEPLOY}}"}, "role": "user"},
                    {"match": {"san": "*.corp"}, "role": "user"}
                ]
            })),
            &|k| (k == "DEPLOY").then(|| "d3pl0y".to_string()),
        )
        .unwrap();
        let declared = r.resolve(&ident(&[], None, false, false), Some("d3pl0y"));
        assert_eq!(declared.id, "user:deploy-bot");
        // CN `deploy-bot`, admitted by the san rule through a SAN it matches.
        let by_cn = r.resolve(&ident(&["x.corp"], Some("deploy-bot"), true, false), None);
        assert_eq!(by_cn.id, "user:cn=deploy-bot");
        // No CN; the first SAN is `deploy-bot` and a later one matches the rule.
        let by_san = r.resolve(&ident(&["deploy-bot", "x.corp"], None, true, false), None);
        assert_eq!(by_san.id, "user:san=deploy-bot");
        for p in [&by_cn, &by_san] {
            assert_ne!(p.id, declared.id, "a certificate inherited a declared id");
        }
    }

    /// The backstop behind the validation that requires `id` on bearer_ref
    /// and any rules: a rule that names nobody, matched by evidence that names
    /// nobody, does not match at all. Built past validation on purpose — this
    /// is the line that holds when validation is not what built the rules.
    #[test]
    fn evidence_that_names_nobody_never_becomes_user_unknown() {
        let r = Resolver::build(
            &a2a(json!({"principals": [{"match": {"any": true}, "role": "user"}]})),
            &|_| None,
        )
        .unwrap();
        let p = r.resolve(&ident(&[], None, false, false), None);
        assert_eq!(p.role, Role::Anonymous, "{}", p.id);
        assert_ne!(p.id, "user:unknown");
        // The same rule still names a certificate holder by its CN.
        let cert = r.resolve(&ident(&[], Some("alice"), false, false), None);
        assert_eq!(cert.id, "user:cn=alice");
    }
}
