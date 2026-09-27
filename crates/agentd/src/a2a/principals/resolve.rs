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
    /// The verified AAuth agent id, set only when an inbound AAuth verifier
    /// established one; an `aauth_agent` principal rule matches against it.
    pub aauth_agent: Option<String>,
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
        } else if let Some(agent) = &m.aauth_agent {
            id.aauth_agent.as_deref().is_some_and(|a| glob(agent, a))
        } else {
            false
        };
        if !hit {
            return None;
        }
        let pid = principal_id(self.role, id, m);
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

fn principal_id(role: Role, id: &CallerIdentity, m: &v2::PrincipalMatch) -> String {
    let sub = id
        .subject
        .clone()
        .or_else(|| id.sans.first().cloned())
        .or_else(|| id.bearer_ref.clone())
        .or_else(|| id.aauth_agent.clone())
        .or_else(|| m.sub.clone())
        .unwrap_or_else(|| "unknown".into());
    match role {
        Role::Operator => "operator".into(),
        Role::User => format!("user:{sub}"),
        Role::Agent => format!("agent:{sub}"),
        Role::Anonymous => "anonymous".into(),
    }
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
    } else if let Some(a) = &m.aauth_agent {
        json!({"aauth_agent": a})
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
                    {"match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent"},
                    {"match": {"any": true}, "role": "anonymous"}
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
        assert_eq!(user.id, "user:spiffe://team/alice");
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
}
