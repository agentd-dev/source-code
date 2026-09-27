// SPDX-License-Identifier: AGPL-3.0-only
//! Gate addressees: who must answer an `ask_human`/`human` gate.

use super::{Principal, glob};
use crate::config::v2::Role;
use serde_json::Value;

/// **Who must answer a gate.** An `ask_human`/`human` `to:` declaration.
///
/// An addressee is what makes a gate's record TRUE: "the finance lead
/// approved this refund" is only worth writing down if someone else could not
/// have satisfied it. Every declared condition must hold, so adding one always
/// narrows — the same direction as `security.policies` matching, and for the
/// same reason: an operator tightening a gate should never widen it by
/// accident.
///
/// Declared either as a bare principal-id glob:
///
/// ```yaml
/// to: "*@finance.acme.example"
/// ```
///
/// or structurally, when identity is better described than enumerated:
///
/// ```yaml
/// to: {role: user, labels: {team: finance}}
/// ```
///
/// Labels are the durable form — people change, teams do not — and they come
/// from `a2a.principals[].labels`, which is operator-declared and closed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Addressee {
    /// Principal-id glob (`*` suffix, or exact).
    pub id: Option<String>,
    pub role: Option<Role>,
    /// Every pair must be present on the principal with that value.
    pub labels: std::collections::BTreeMap<String, String>,
}

impl Addressee {
    /// Parse a `to:` declaration. A string is an id glob; an object names any
    /// of `id`, `role`, `labels`.
    pub fn parse(v: &Value) -> Result<Addressee, String> {
        match v {
            Value::String(s) if !s.trim().is_empty() => Ok(Addressee {
                id: Some(s.trim().to_string()),
                ..Default::default()
            }),
            Value::String(_) => Err("`to` must not be empty".into()),
            Value::Object(o) => {
                for k in o.keys() {
                    if !["id", "role", "labels"].contains(&k.as_str()) {
                        return Err(format!("unknown `to` field {k:?} (want id|role|labels)"));
                    }
                }
                let role = match o.get("role").and_then(Value::as_str) {
                    None => None,
                    Some("operator") => Some(Role::Operator),
                    Some("user") => Some(Role::User),
                    Some("agent") => Some(Role::Agent),
                    // Addressing anonymous is not a mistake worth allowing: an
                    // anonymous caller is precisely the one whose identity
                    // nothing vouches for, so a gate "answered by anonymous"
                    // records nothing at all.
                    Some("anonymous") => {
                        return Err("`to.role: anonymous` names nobody — a gate answered by an                                     unidentified caller records nothing"
                            .into());
                    }
                    Some(other) => {
                        return Err(format!(
                            "unknown `to.role` {other:?} (want operator|user|agent)"
                        ));
                    }
                };
                let mut labels = std::collections::BTreeMap::new();
                if let Some(m) = o.get("labels") {
                    let Some(m) = m.as_object() else {
                        return Err("`to.labels` must be an object of string values".into());
                    };
                    for (k, v) in m {
                        let Some(v) = v.as_str() else {
                            return Err(format!("`to.labels.{k}` must be a string"));
                        };
                        labels.insert(k.clone(), v.to_string());
                    }
                }
                let id = o
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let a = Addressee { id, role, labels };
                if a.is_empty() {
                    // An addressee matching everyone is not an addressee, and
                    // silently accepting one would make a gate look routed
                    // when it is not.
                    return Err("`to` names nobody — give an id, a role or labels".into());
                }
                Ok(a)
            }
            _ => Err("`to` must be a principal-id glob or {id, role, labels}".into()),
        }
    }

    fn is_empty(&self) -> bool {
        self.id.is_none() && self.role.is_none() && self.labels.is_empty()
    }

    /// Whether this principal is who the gate is waiting for.
    pub fn matches(&self, p: &Principal) -> bool {
        // This module's own `glob`, not the registry's tool-name matcher: a
        // principal id is domain-shaped (`user:alice@acme.example`), so the
        // useful pattern is a SUFFIX (`*@finance.example`) — which the tool
        // matcher cannot express, since it only strips a trailing star.
        // Widening that one to suit this would widen every tool grant with it.
        if let Some(pat) = &self.id
            && !glob(pat, &p.id)
        {
            return false;
        }
        if let Some(r) = self.role
            && p.role != r
        {
            return false;
        }
        self.labels
            .iter()
            .all(|(k, v)| p.labels.get(k).map(String::as_str) == Some(v.as_str()))
    }

    /// A human-readable rendering, for the question, the task and the refusal
    /// — a gate that will not accept your answer has to say whose it wants.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(id) = &self.id {
            parts.push(id.clone());
        }
        if let Some(r) = self.role {
            parts.push(format!("role {}", format!("{r:?}").to_lowercase()));
        }
        for (k, v) in &self.labels {
            parts.push(format!("{k}={v}"));
        }
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn person(id: &str, role: Role, labels: &[(&str, &str)]) -> Principal {
        Principal {
            id: id.into(),
            role,
            grants: Vec::new(),
            rate: None,
            budget: None,
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }

    /// The whole point of an addressee: someone else cannot satisfy it. Every
    /// declared condition must hold, so adding one narrows.
    #[test]
    fn an_addressee_admits_only_who_it_names() {
        let by_id = Addressee::parse(&json!("*@finance.example")).unwrap();
        assert!(by_id.matches(&person("lead@finance.example", Role::User, &[])));
        assert!(!by_id.matches(&person("dev@eng.example", Role::User, &[])));

        // Labels are the durable form — people change, teams do not.
        let by_label =
            Addressee::parse(&json!({"role": "user", "labels": {"team": "finance"}})).unwrap();
        assert!(by_label.matches(&person("anyone", Role::User, &[("team", "finance")])));
        assert!(
            !by_label.matches(&person("anyone", Role::User, &[("team", "eng")])),
            "a different team is a different decider"
        );
        assert!(
            !by_label.matches(&person("anyone", Role::Agent, &[("team", "finance")])),
            "conditions AND: the role must hold too"
        );
        assert!(
            !by_label.matches(&person("anyone", Role::User, &[])),
            "a principal with no labels matches no label condition"
        );
    }

    /// Three declarations are refused rather than accepted-and-ignored,
    /// because each would produce a gate that LOOKS routed and is not.
    #[test]
    fn an_addressee_that_names_nobody_is_refused() {
        // Matches everyone.
        assert!(Addressee::parse(&json!({})).is_err());
        assert!(Addressee::parse(&json!("")).is_err());
        // Anonymous is precisely the identity nothing vouches for, so a gate
        // "answered by anonymous" records nothing at all.
        assert!(Addressee::parse(&json!({"role": "anonymous"})).is_err());
        // And a typo must not silently widen the gate.
        assert!(Addressee::parse(&json!({"rolle": "user"})).is_err());
        assert!(Addressee::parse(&json!({"role": "auditor"})).is_err());
        assert!(Addressee::parse(&json!({"labels": {"team": 1}})).is_err());
    }

    /// A gate that will not take your answer has to say whose it wants.
    #[test]
    fn an_addressee_describes_itself() {
        let a = Addressee::parse(&json!({"id": "u:*", "role": "user", "labels": {"team": "fin"}}))
            .unwrap();
        let d = a.describe();
        assert!(
            d.contains("u:*") && d.contains("user") && d.contains("team=fin"),
            "{d}"
        );
    }
}
