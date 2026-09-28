// SPDX-License-Identifier: AGPL-3.0-only
//! The identity registry: the names principals are known by, across
//! restarts.
//!
//! Two things spell `user:<x>`: a configured `user`-role rule whose `id` is
//! `x`, and a device session an operator approved as `x`. Ownership — of
//! tasks, runs, subagents, conversations — is persisted by principal id, so if
//! the two ever met in one id, whoever came second would inherit the first's
//! history: a person approved as `ci-bot` would read the CI rule's runs, or a
//! rule added later as `alice` would continue alice's conversations.
//!
//! Checking only what is configured NOW cannot stop that, because the other
//! half is history: the device whose sessions expired last week still owns
//! what it started, and the rule deleted from the config last month still
//! owns its runs. So every claim is recorded, durably, the first time it is
//! made — a rule id at listener spawn and at every principals reload, a device
//! name at approval — and a claim of the other kind is refused for as long as
//! the store persists:
//!
//! * an approval refuses a name whose `user:<name>` is registered as a rule;
//! * startup (exit 2) and a reload (the old rules stay) refuse a `user`-role
//!   rule id whose `user:<id>` is registered as a device.
//!
//! A memory store persists nothing across a restart — and neither does the
//! ownership this protects, so the registry lives exactly as long as what it
//! guards. There is deliberately no command that releases a name.
//!
//! Certificate-derived ids (`user:cn=…`, `user:san=…`) need no entry: the `=`
//! puts them outside both the declared-id and the approval-name charset.

use crate::config::v2::{A2a, Role};
use crate::state::{Durable, Kind, now_ms};
use serde_json::{Value, json};

/// Who first claimed a principal id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registered {
    /// A configured `a2a.principals` rule's `id`.
    Rule,
    /// A device session's approval name.
    Device,
}

impl Registered {
    pub fn as_str(self) -> &'static str {
        match self {
            Registered::Rule => "rule",
            Registered::Device => "device",
        }
    }

    fn parse(s: &str) -> Option<Registered> {
        match s {
            "rule" => Some(Registered::Rule),
            "device" => Some(Registered::Device),
            _ => None,
        }
    }
}

/// Why a claim was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// `user:<id>` is already an approved device's, and a `user`-role rule
    /// declares `id`.
    Collision { id: String },
    /// The registry could not be read or written. A claim nobody can check is
    /// refused rather than waved through.
    Store(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Collision { id } => write!(
                f,
                "a2a.principals: the user-role id {id:?} was approved as a device name on this \
                 agent, and user:{id} owns that device's history; give the rule another id"
            ),
            Refused::Store(e) => write!(f, "the identity registry: {e}"),
        }
    }
}

impl Refused {
    /// The `identity.collision` log line, for a collision.
    pub fn collision_line(&self) -> Option<Value> {
        match self {
            Refused::Collision { id } => Some(json!({
                "id": format!("user:{id}"),
                "registered": Registered::Device.as_str(),
                "declared": id,
            })),
            Refused::Store(_) => None,
        }
    }
}

/// The store key of a principal id: a hash, because a declared id may carry
/// `/` and `:`, which a file store would read as structure — of the id with
/// its ASCII case folded. Approval names are lowercase so that two spellings
/// never name two principals, but a declared id may carry capitals: keyed as
/// spelled, a rule `Alice` and a device `alice` would be `user:Alice` and
/// `user:alice` side by side, one person to a reader of the trail and two to
/// the ownership checks. Folded, each is the other's collision.
fn key(principal_id: &str) -> String {
    crate::sha::sha256_hex(principal_id.to_ascii_lowercase().as_bytes())
}

/// Who first claimed `principal_id`, if anybody has.
pub fn lookup(durable: &Durable, principal_id: &str) -> Result<Option<Registered>, Refused> {
    let env = durable
        .get(Kind::Identity, &key(principal_id))
        .map_err(|e| Refused::Store(e.to_string()))?;
    Ok(env.and_then(|e| e.state["kind"].as_str().and_then(Registered::parse)))
}

fn register(durable: &Durable, principal_id: &str, kind: Registered) -> Result<(), Refused> {
    durable
        .put(
            Kind::Identity,
            &key(principal_id),
            json!({"id": principal_id, "kind": kind.as_str(), "first_ms": now_ms()}),
            None,
        )
        .map(|_| ())
        .map_err(|e| Refused::Store(e.to_string()))
}

/// The principal ids the rules in `a2a` claim: every `user`- and `agent`-role
/// rule with an `id`. An operator rule is `operator` and an anonymous one
/// names nobody, so neither claims a name.
fn rule_ids(a2a: &A2a) -> impl Iterator<Item = (Role, &str)> {
    a2a.principals
        .iter()
        .filter(|p| matches!(p.role, Role::User | Role::Agent))
        .filter_map(|p| Some((p.role, p.id.as_deref()?)))
}

fn principal_id(role: Role, id: &str) -> String {
    match role {
        Role::Agent => format!("agent:{id}"),
        _ => format!("user:{id}"),
    }
}

/// Record every rule id `a2a` declares, refusing the whole set — before any
/// of it is written — when a `user`-role id is already a device's name.
///
/// Called at listener spawn and before a reload installs new rules, so a
/// refusal leaves the rules in force as they were. An `agent`-role id never
/// collides: it is `agent:<id>`, and a device is always `user:<name>`.
pub fn register_rules(durable: &Durable, a2a: &A2a) -> Result<(), Refused> {
    for (role, id) in rule_ids(a2a) {
        if role == Role::User
            && lookup(durable, &principal_id(role, id))? == Some(Registered::Device)
        {
            return Err(Refused::Collision { id: id.to_string() });
        }
    }
    for (role, id) in rule_ids(a2a) {
        let pid = principal_id(role, id);
        // First claim wins and keeps its `first_ms`; re-registering an id
        // every reload would rewrite when it was first seen.
        if lookup(durable, &pid)?.is_none() {
            register(durable, &pid, Registered::Rule)?;
        }
    }
    Ok(())
}

/// Record `user:<name>` as an approved device's. `Ok(true)` when it already
/// was — the approval hands this person everything that name owns, which the
/// operator is told. The caller has already refused a name a rule claimed.
pub fn register_device(durable: &Durable, name: &str) -> Result<bool, Refused> {
    let pid = format!("user:{name}");
    match lookup(durable, &pid)? {
        Some(Registered::Device) => Ok(true),
        Some(Registered::Rule) => Err(Refused::Store(format!(
            "{pid} is a configured principal's, not a device's"
        ))),
        None => register(durable, &pid, Registered::Device).map(|()| false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn durable() -> Durable {
        let store: crate::store::SharedStore =
            std::sync::Arc::new(crate::store::memory::MemoryStore::new());
        Durable::new(store, "agentd", "t", crate::state::Policy::default(), None)
    }

    fn a2a(doc: Value) -> A2a {
        serde_json::from_value(doc).unwrap()
    }

    /// A device name and a rule id never share a principal id, whichever
    /// claims it first — and an agent-role rule, which is `agent:<id>`, never
    /// collides with a device.
    #[test]
    fn a_device_name_and_a_user_rule_id_exclude_each_other() {
        let d = durable();
        assert!(!register_device(&d, "alice").unwrap());
        assert!(
            register_device(&d, "alice").unwrap(),
            "the second is existing"
        );
        assert_eq!(lookup(&d, "user:alice").unwrap(), Some(Registered::Device));

        let alice_rule = a2a(json!({"principals": [
            {"id": "alice", "match": {"bearer_ref": "{{secret:X}}"}, "role": "user"}
        ]}));
        assert_eq!(
            register_rules(&d, &alice_rule),
            Err(Refused::Collision { id: "alice".into() })
        );
        let agent_alice = a2a(json!({"principals": [
            {"id": "alice", "match": {"bearer_ref": "{{secret:X}}"}, "role": "agent"},
            {"id": "ci-bot", "match": {"bearer_ref": "{{secret:Y}}"}, "role": "user"}
        ]}));
        register_rules(&d, &agent_alice).unwrap();
        assert_eq!(lookup(&d, "agent:alice").unwrap(), Some(Registered::Rule));
        assert_eq!(lookup(&d, "user:ci-bot").unwrap(), Some(Registered::Rule));
        // A rule id is never taken over by a device.
        assert!(register_device(&d, "ci-bot").is_err());
        assert_eq!(lookup(&d, "user:ci-bot").unwrap(), Some(Registered::Rule));
    }

    /// A rule id and a device name that differ only in case are one name:
    /// whichever claims it first, the other is refused.
    #[test]
    fn a_claim_is_one_name_whatever_its_case() {
        let d = durable();
        let carol_rule = a2a(json!({"principals": [
            {"id": "Carol", "match": {"bearer_ref": "{{secret:X}}"}, "role": "user"}
        ]}));
        register_rules(&d, &carol_rule).unwrap();
        assert_eq!(lookup(&d, "user:carol").unwrap(), Some(Registered::Rule));
        assert!(register_device(&d, "carol").is_err());

        register_device(&d, "dave").unwrap();
        let dave_rule = a2a(json!({"principals": [
            {"id": "Dave", "match": {"bearer_ref": "{{secret:X}}"}, "role": "user"}
        ]}));
        assert_eq!(
            register_rules(&d, &dave_rule),
            Err(Refused::Collision { id: "Dave".into() })
        );
    }

    /// A refused set writes nothing: the collision is found before any of the
    /// set's ids is recorded.
    #[test]
    fn a_refused_rule_set_registers_none_of_it() {
        let d = durable();
        register_device(&d, "alice").unwrap();
        let set = a2a(json!({"principals": [
            {"id": "bob", "match": {"bearer_ref": "{{secret:X}}"}, "role": "user"},
            {"id": "alice", "match": {"bearer_ref": "{{secret:Y}}"}, "role": "user"}
        ]}));
        assert!(register_rules(&d, &set).is_err());
        assert_eq!(lookup(&d, "user:bob").unwrap(), None);
    }
}
