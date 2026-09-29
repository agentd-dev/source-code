// SPDX-License-Identifier: AGPL-3.0-only
//! Resolving a caller's evidence to a principal, under the configured
//! `a2a.principals` and the listener's posture.
//!
//! The steps run in a fixed order, and each kind of evidence decides only for
//! itself:
//!
//! 0. a unix-socket peer (same uid, enforced at accept) is the operator;
//! 1. a presented bearer is checked — a session token against the sessions, any
//!    other against the server bearer and then the `bearer_ref` rules — and one
//!    that matches nothing is a FAILED credential, never a fallback to what
//!    else the request carried;
//! 2. a verified certificate matches the `san`/`sub`/`any` rules, and is the
//!    operator only while no rule exists at all;
//! 3. an `any` rule names every remaining caller;
//! 4. on a loopback-bound listener with no mechanism, a caller that is local
//!    and is not a browser is the implicit operator;
//! 5. anybody else is unauthenticated.
//!
//! Each kind of evidence used to be folded into one "management" flag, which
//! is how a certificate that matched no rule became the operator on a listener
//! whose operator credential was its bearer: the flag was true, and the bearer
//! was never asked for.

use super::{Principal, glob};
use crate::config::settings::{self, Role};
use crate::runtime::surface::auth::{ListenerAuth, listener_auth_of};
use crate::sec::secret;
use serde_json::{Value, json};

/// The prefix of every session token the listener issues. A bearer carrying it
/// is checked against the sessions and nothing else, so no configured secret
/// may start with it: [`Resolver::build`] refuses one that does.
pub const SESSION_TOKEN_PREFIX: &str = "agentd_at_";

/// A verified client certificate's identity.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CertId {
    /// The subject CN.
    pub subject: Option<String>,
    /// The SANs (a SPIFFE X.509-SVID's `spiffe://…` arrives as a URI SAN).
    pub sans: Vec<String>,
}

/// What the transport learned about one request.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    /// The `Authorization: Bearer` token, when one was presented.
    pub bearer: Option<String>,
    /// The verified client certificate, when the listener verifies one.
    pub cert: Option<CertId>,
    /// The peer is on this host (a loopback address).
    pub local: bool,
    /// The request carried an `Origin` header of ANY value — `null` included,
    /// which is what a `file://` page, a sandboxed iframe or a `data:` URL
    /// sends. It came from a browser.
    pub origin: bool,
}

/// Which evidence named the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The server bearer or a `bearer_ref` rule.
    Bearer,
    /// A session token the listener issued.
    Session,
    /// A verified client certificate.
    Cert,
    /// An `any` rule, with nothing presented.
    AnyRule,
    /// A unix-socket peer of the daemon's own uid.
    Unix,
    /// The implicit operator of a loopback listener with no mechanism.
    Implicit,
}

/// Who a request is.
// Unboxed: one lives per request, on the stack, for as long as the request
// takes to authenticate — boxing the common answer to shrink the rare ones
// buys nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// A principal, and the evidence that named it.
    Named(Principal, Via),
    /// Nobody. `presented` when a credential was offered and failed — the
    /// difference between "sign in" and "that sign-in is no good".
    Unauthenticated { presented: bool },
    /// A verified certificate that no rule grants a role.
    NoRole,
}

/// The answer a session store gives for a token.
#[allow(clippy::large_enum_variant)] // as for `Resolution`: per request, unboxed
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCheck {
    Valid(Principal),
    Invalid,
}

/// Checks session tokens. Implemented by the listener's session store; the
/// resolver only routes `agentd_at_` bearers to it.
pub trait SessionVerifier {
    fn verify(&self, token: &str) -> SessionCheck;
}

/// Resolves a request's evidence to a principal: the compiled rules and the
/// posture they were built with, as ONE value.
///
/// The posture lives here rather than beside the listener so that a reload,
/// which swaps the resolver, swaps both at once. A listener that kept its own
/// copy would pair new rules with a stale posture — a daemon given its first
/// principal by SIGHUP would still treat every local caller as the operator.
pub struct Resolver {
    rules: Vec<Compiled>,
    /// `a2a.bearer`, resolved: presenting it is the operator.
    server_bearer: Option<String>,
    posture: ListenerAuth,
}

struct Compiled {
    /// The rule's declared `id`, which names its callers when set.
    id: Option<String>,
    matcher: settings::PrincipalMatch,
    role: Role,
    grants: Vec<String>,
    rate: Option<String>,
    budget: Option<settings::Budget>,
    labels: std::collections::BTreeMap<String, String>,
    bearer_secret: Option<String>,
}

impl Resolver {
    /// Build from settings, resolving the server bearer and every `bearer_ref`
    /// secret now, and computing the posture from the same section.
    ///
    /// Refused: a `bearer_ref` secret equal to `a2a.bearer` or to another
    /// rule's (one secret would mean two principals, and only the first could
    /// ever be reached), and any secret starting with the session-token
    /// prefix (it would be routed to the sessions and never match).
    pub fn build(
        a2a: &settings::A2a,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Resolver, String> {
        let server_bearer = match &a2a.bearer {
            Some(b) => {
                let s = secret::resolve(&b.0, env).map_err(|e| format!("a2a.bearer: {e}"))?;
                if s.starts_with(SESSION_TOKEN_PREFIX) {
                    return Err(format!(
                        "a2a.bearer: a secret may not start with {SESSION_TOKEN_PREFIX:?}, \
                         the prefix of the session tokens this listener issues"
                    ));
                }
                Some(s)
            }
            None => None,
        };
        let mut rules: Vec<Compiled> = Vec::new();
        for (i, p) in a2a.principals.iter().enumerate() {
            let bearer_secret = match &p.matcher.bearer_ref {
                Some(r) => {
                    let s = secret::resolve(r, env)
                        .map_err(|e| format!("a2a principal bearer_ref: {e}"))?;
                    let name = p.id.clone().unwrap_or_else(|| format!("#{i}"));
                    if s.starts_with(SESSION_TOKEN_PREFIX) {
                        return Err(format!(
                            "a2a.principals[{name}].match.bearer_ref: a secret may not start \
                             with {SESSION_TOKEN_PREFIX:?}, the prefix of the session tokens \
                             this listener issues"
                        ));
                    }
                    if server_bearer.as_deref() == Some(s.as_str()) {
                        return Err(format!(
                            "a2a.principals[{name}].match.bearer_ref: the secret is a2a.bearer's; \
                             one secret cannot mean both the operator and this rule"
                        ));
                    }
                    if rules
                        .iter()
                        .any(|c| c.bearer_secret.as_deref() == Some(s.as_str()))
                    {
                        return Err(format!(
                            "a2a.principals[{name}].match.bearer_ref: the secret is another \
                             rule's; one secret cannot mean two principals"
                        ));
                    }
                    Some(s)
                }
                None => None,
            };
            rules.push(Compiled {
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
            rules,
            server_bearer,
            posture: listener_auth_of(a2a),
        })
    }

    /// The listener posture these rules were built with.
    pub fn posture(&self) -> ListenerAuth {
        self.posture
    }

    /// Resolve one request. `unix` is whether it arrived on a unix socket;
    /// `sessions` checks session tokens (none installed: every one fails).
    pub fn resolve(
        &self,
        ev: &Evidence,
        unix: bool,
        sessions: Option<&dyn SessionVerifier>,
    ) -> Resolution {
        // 0. The kernel already named this peer: the daemon's own uid.
        if unix {
            return Resolution::Named(operator(), Via::Unix);
        }
        // 1. A bearer decides whenever it is presented to a listener that can
        // check one. A failed bearer is a failed request: falling through to
        // the certificate or the loopback default would make "present any
        // junk" as good as presenting nothing.
        if let Some(token) = ev.bearer.as_deref() {
            if token.starts_with(SESSION_TOKEN_PREFIX) {
                return match sessions.map(|s| s.verify(token)) {
                    Some(SessionCheck::Valid(p)) => Resolution::Named(p, Via::Session),
                    _ => Resolution::Unauthenticated { presented: true },
                };
            }
            if self.posture.bearer || self.posture.device {
                if let Some(server) = &self.server_bearer
                    && crate::sha::ct_eq(server.as_bytes(), token.as_bytes())
                {
                    return Resolution::Named(operator(), Via::Bearer);
                }
                for c in &self.rules {
                    if let Some(secret) = &c.bearer_secret
                        && crate::sha::ct_eq(secret.as_bytes(), token.as_bytes())
                    {
                        return match c.principal(None) {
                            Some(p) if p.role != Role::Anonymous => {
                                Resolution::Named(p, Via::Bearer)
                            }
                            _ => Resolution::Unauthenticated { presented: true },
                        };
                    }
                }
                return Resolution::Unauthenticated { presented: true };
            }
            // A listener with no bearer mechanism has nothing to check it
            // against; it is ignored, as any unasked-for header is.
        }
        // 2. A verified certificate: the rules decide, and with none written
        // any certificate the CA signed is the operator. Once a rule exists
        // the allowlist is on, and an unmatched certificate is nobody.
        if let Some(cert) = &ev.cert {
            if self.rules.is_empty() {
                return Resolution::Named(operator(), Via::Cert);
            }
            for c in &self.rules {
                if c.matches_cert(cert)
                    && let Some(p) = c.principal(Some(cert))
                {
                    if p.role == Role::Anonymous {
                        return Resolution::NoRole;
                    }
                    return Resolution::Named(p, Via::Cert);
                }
            }
            return Resolution::NoRole;
        }
        // 3. An `any` rule names whoever is left.
        if let Some(c) = self.rules.iter().find(|c| c.matcher.any) {
            return match c.principal(None) {
                Some(p) if p.role != Role::Anonymous => Resolution::Named(p, Via::AnyRule),
                _ => Resolution::Unauthenticated { presented: false },
            };
        }
        // 4. The implicit operator: a local caller, on a loopback bind with no
        // mechanism — and never a browser. A page any site served can make
        // the operator's browser POST here, and CORS admission of its origin
        // is not trust: a browser signs in like everyone else.
        if ev.local && self.posture.implicit_operator && !ev.origin {
            return Resolution::Named(operator(), Via::Implicit);
        }
        // 5.
        Resolution::Unauthenticated { presented: false }
    }

    /// The declared id of the rule a principal was named by, when a rule with
    /// an id named it — for the audit line, which must tell rules apart even
    /// when their callers share a role. Every operator is `operator`, so the id
    /// alone cannot say which rule named one; that is not a guess this makes.
    pub fn rule_of(&self, p: &Principal) -> Option<&str> {
        if p.is_operator() {
            return None;
        }
        self.rules.iter().find_map(|c| {
            let id = c.id.as_deref()?;
            (c.role == p.role && principal_id(c.role, Some(id), None).as_deref() == Some(&p.id))
                .then_some(id)
        })
    }

    /// A status view of the configured principals.
    pub fn status(&self) -> Value {
        json!({
            "principals": self.rules.iter().map(|c| json!({
                "id": c.id,
                "role": format!("{:?}", c.role).to_lowercase(),
                "match": matcher_desc(&c.matcher),
                "grants": c.grants,
            })).collect::<Vec<_>>(),
            "implicit_operator": self.posture.implicit_operator,
        })
    }
}

impl Compiled {
    /// Whether this rule's certificate matcher admits `cert`. `any` admits
    /// every certificate the CA signed; a `bearer_ref` rule admits none.
    fn matches_cert(&self, cert: &CertId) -> bool {
        let m = &self.matcher;
        if m.any {
            true
        } else if let Some(san) = &m.san {
            cert.sans.iter().any(|s| glob(san, s))
                || cert.subject.as_deref().is_some_and(|s| glob(san, s))
        } else if let Some(sub) = &m.sub {
            cert.subject.as_deref() == Some(sub.as_str())
        } else {
            false
        }
    }

    /// The principal this rule's caller acts as, or `None` when neither the
    /// rule nor the evidence names anybody.
    ///
    /// Evidence that names nobody, under a rule that names nobody, is not a
    /// caller this rule can own work for. Validation requires an `id` on
    /// every rule whose evidence is anonymous, so this is the backstop that
    /// keeps `user:unknown` — one principal every such caller merged into —
    /// impossible rather than merely unconfigured.
    fn principal(&self, cert: Option<&CertId>) -> Option<Principal> {
        Some(Principal {
            id: principal_id(self.role, self.id.as_deref(), cert)?,
            role: self.role,
            grants: self.grants.clone(),
            rate: self.rate.clone(),
            budget: self.budget.clone(),
            labels: self.labels.clone(),
            session: None,
        })
    }
}

/// Every principal the rules name before anybody presents anything: the
/// operator, and each rule with a declared `id`, with the role, grants and
/// quotas that rule gives it — keyed by principal id.
///
/// This is what the runtime acts with for work restored or in flight when no
/// request has named its owner since the rules last changed. Without it, a
/// reload that narrowed or removed a rule left the old principal acting
/// through the model until it happened to call again (a removed one never
/// does), and a restart left even the operator unknown. What it cannot know
/// is a caller whose id comes from its evidence — a certificate's CN, a
/// device session — and those are indexed when they are seen.
///
/// Needs no secret: an id is derived from the rule, never from its bearer.
/// When two rules name the same id, the first wins, as it does in `resolve`.
pub fn declared_principals(a2a: &settings::A2a) -> std::collections::BTreeMap<String, Principal> {
    let mut out = std::collections::BTreeMap::new();
    let op = operator();
    out.insert(op.id.clone(), op);
    for p in &a2a.principals {
        let Some(id) = principal_id(p.role, p.id.as_deref(), None) else {
            continue;
        };
        // An operator rule is `operator`, already present; an anonymous one
        // names nobody the runtime could act for.
        if matches!(p.role, Role::Operator | Role::Anonymous) || out.contains_key(&id) {
            continue;
        }
        out.insert(
            id.clone(),
            Principal {
                id,
                role: p.role,
                grants: p.grants.clone(),
                rate: p.quotas.as_ref().and_then(|q| q.rate.clone()),
                budget: p.quotas.as_ref().and_then(|q| q.budget.clone()),
                labels: p.labels.clone(),
                session: None,
            },
        );
    }
    out
}

fn operator() -> Principal {
    Principal {
        id: "operator".into(),
        role: Role::Operator,
        grants: vec!["*".into()],
        rate: None,
        budget: None,
        labels: Default::default(),
        session: None,
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
fn principal_id(role: Role, declared: Option<&str>, cert: Option<&CertId>) -> Option<String> {
    let role_name = match role {
        Role::Operator => return Some("operator".into()),
        Role::Anonymous => return Some("anonymous".into()),
        Role::User => "user",
        Role::Agent => "agent",
    };
    let name = match (declared, cert) {
        (Some(d), _) => d.to_string(),
        (None, Some(c)) => match (&c.subject, c.sans.first()) {
            (Some(cn), _) => format!("cn={cn}"),
            (None, Some(san)) => format!("san={san}"),
            (None, None) => return None,
        },
        (None, None) => return None,
    };
    Some(format!("{role_name}:{name}"))
}

fn matcher_desc(m: &settings::PrincipalMatch) -> Value {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn a2a(doc: Value) -> settings::A2a {
        serde_json::from_value(doc).unwrap()
    }

    fn secrets(k: &str) -> Option<String> {
        Some(
            match k {
                "B" => "server-bearer",
                "OPS" => "ops-bearer",
                "PEER" => "s3cr3t",
                "DEPLOY" => "d3pl0y",
                "A" => "token-a",
                "C" => "token-c",
                "ANON" => "anon-bearer",
                _ => return None,
            }
            .to_string(),
        )
    }

    fn cert(sans: &[&str], sub: Option<&str>) -> Option<CertId> {
        Some(CertId {
            sans: sans.iter().map(|s| s.to_string()).collect(),
            subject: sub.map(str::to_string),
        })
    }

    fn ev(bearer: Option<&str>, cert: Option<CertId>) -> Evidence {
        Evidence {
            bearer: bearer.map(str::to_string),
            cert,
            ..Default::default()
        }
    }

    fn named(r: &Resolution) -> (&str, Via) {
        match r {
            Resolution::Named(p, via) => (p.id.as_str(), *via),
            other => panic!("expected a principal, got {other:?}"),
        }
    }

    /// A session store holding one token.
    struct OneSession;
    impl SessionVerifier for OneSession {
        fn verify(&self, token: &str) -> SessionCheck {
            if token == "agentd_at_live" {
                SessionCheck::Valid(Principal {
                    id: "user:dev".into(),
                    role: Role::User,
                    ..Principal::anonymous()
                })
            } else {
                SessionCheck::Invalid
            }
        }
    }

    /// The principals known before anyone presents anything are the operator
    /// and the declared ids, each exactly as `resolve` would name it — so the
    /// runtime acting for a restored or in-flight owner acts with what a
    /// request from that owner would carry now.
    #[test]
    fn declared_principals_are_what_resolve_would_name() {
        let settings = a2a(json!({
            "listen": "https://0.0.0.0:8443",
            "url": "https://agent.example",
            "tls": {"cert": "c", "key": "k", "client_ca": "ca"},
            "principals": [
                {"match": {"san": "spiffe://corp/team/*"}, "role": "user"},
                {"id": "peer", "match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent",
                 "grants": ["workflow.run:triage*"], "labels": {"team": "ops"}},
                {"id": "ops", "match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
            ]
        }));
        let declared = declared_principals(&settings);
        assert_eq!(
            declared.keys().collect::<Vec<_>>(),
            ["agent:peer", "operator"],
            "{declared:?}"
        );
        assert!(declared["operator"].is_operator());
        let r = Resolver::build(&settings, &secrets).unwrap();
        let ev = Evidence {
            bearer: secrets("PEER"),
            ..Default::default()
        };
        match r.resolve(&ev, false, None) {
            Resolution::Named(p, _) => assert_eq!(declared["agent:peer"], p),
            other => panic!("the peer's bearer names it: {other:?}"),
        }
    }

    /// The evidence table on the listener F01 was found on: a client CA, a
    /// server bearer AND principal rules. A bearer, when presented, decides —
    /// success or failure — and the certificate is consulted only when no
    /// bearer was; an unmatched certificate is nobody, never the operator.
    #[test]
    fn a_bearer_decides_when_presented_and_a_cert_only_when_it_is_not() {
        let r = Resolver::build(
            &a2a(json!({
                "listen": "https://0.0.0.0:8443",
                "url": "https://agent.example",
                "tls": {"cert": "c", "key": "k", "client_ca": "ca"},
                "bearer": "{{secret:B}}",
                "principals": [
                    {"match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
                    {"match": {"san": "spiffe://corp/team/*"}, "role": "user", "grants": ["knowledge.*"]},
                    {"id": "peer", "match": {"bearer_ref": "{{secret:PEER}}"}, "role": "agent"},
                    {"match": {"sub": "nobody"}, "role": "anonymous"},
                ]
            })),
            &secrets,
        )
        .unwrap();
        let team = || cert(&["spiffe://corp/team/alice"], None);
        let other = || cert(&["spiffe://other/x"], None);
        let resolve = |e: Evidence| r.resolve(&e, false, Some(&OneSession));

        // The server bearer is the operator whatever certificate came with it…
        assert_eq!(
            named(&resolve(ev(Some("server-bearer"), other()))),
            ("operator", Via::Bearer)
        );
        assert_eq!(
            named(&resolve(ev(Some("server-bearer"), team()))),
            ("operator", Via::Bearer)
        );
        // …a rule's bearer is that rule…
        assert_eq!(
            named(&resolve(ev(Some("s3cr3t"), team()))),
            ("agent:peer", Via::Bearer)
        );
        // …and a bearer that matches nothing FAILS, rather than falling back
        // to the certificate it arrived with.
        assert_eq!(
            resolve(ev(Some("junk"), team())),
            Resolution::Unauthenticated { presented: true }
        );
        // A session token goes to the sessions and nowhere else.
        assert_eq!(
            named(&resolve(ev(Some("agentd_at_live"), other()))),
            ("user:dev", Via::Session)
        );
        assert_eq!(
            resolve(ev(Some("agentd_at_dead"), team())),
            Resolution::Unauthenticated { presented: true }
        );
        assert_eq!(
            r.resolve(&ev(Some("agentd_at_live"), None), false, None),
            Resolution::Unauthenticated { presented: true },
            "with no session store every session token fails"
        );
        // No bearer: the certificate decides, by the rules.
        let t = resolve(ev(None, team()));
        assert_eq!(named(&t), ("user:san=spiffe://corp/team/alice", Via::Cert));
        assert_eq!(
            named(&resolve(ev(None, cert(&["spiffe://corp/ops/root"], None)))),
            ("operator", Via::Cert)
        );
        // F01: a certificate the CA signed that matches no rule is NOT the
        // operator on a listener whose operator credential is the bearer.
        assert_eq!(resolve(ev(None, other())), Resolution::NoRole);
        // An anonymous-role rule grants no role.
        assert_eq!(
            resolve(ev(None, cert(&[], Some("nobody")))),
            Resolution::NoRole
        );
        // Nothing at all, even from loopback: this bind is not loopback.
        let local = Evidence {
            local: true,
            ..Default::default()
        };
        assert_eq!(
            resolve(local),
            Resolution::Unauthenticated { presented: false }
        );
        // And a unix peer is the operator, whatever it presents.
        assert_eq!(
            named(&r.resolve(&ev(Some("junk"), None), true, None)),
            ("operator", Via::Unix)
        );

        // With no rules at all, any verified certificate is the operator.
        let bare = Resolver::build(
            &a2a(
                json!({"listen": "https://0.0.0.0:8443", "url": "https://agent.example",
                "tls": {"cert": "c", "key": "k", "client_ca": "ca"}, "bearer": "{{secret:B}}"}),
            ),
            &secrets,
        )
        .unwrap();
        assert_eq!(
            named(&bare.resolve(&ev(None, other()), false, None)),
            ("operator", Via::Cert)
        );
        assert_eq!(
            bare.resolve(&ev(Some("junk"), other()), false, None),
            Resolution::Unauthenticated { presented: true }
        );
    }

    /// The implicit operator is a property of the BIND, and never of a
    /// browser. A same-host reverse proxy relays every request from loopback,
    /// so on a wildcard bind a loopback peer must buy nothing; and a page on
    /// any site can make a local browser POST, so a request carrying `Origin`
    /// is never the operator even on a no-auth loopback daemon.
    #[test]
    fn implicit_operator_needs_a_loopback_bind() {
        let with = |doc: Value, local: bool, origin: bool| {
            Resolver::build(&a2a(doc), &secrets).unwrap().resolve(
                &Evidence {
                    local,
                    origin,
                    ..Default::default()
                },
                false,
                None,
            )
        };
        let proxy = json!({
            "listen": "https://0.0.0.0:8443", "url": "https://agent.example",
            "tls": {"cert": "c", "key": "k"}, "bearer": "{{secret:B}}",
            "device_grant": {"enabled": true},
        });
        assert_eq!(
            with(proxy.clone(), true, false),
            Resolution::Unauthenticated { presented: false },
            "a relayed request from loopback"
        );
        assert_eq!(
            with(proxy, false, false),
            Resolution::Unauthenticated { presented: false }
        );
        let open = json!({"listen": "http://127.0.0.1:8080"});
        assert_eq!(
            named(&with(open.clone(), true, false)),
            ("operator", Via::Implicit)
        );
        assert_eq!(
            with(
                json!({"listen": "http://127.0.0.1:8080", "bearer": "{{secret:B}}"}),
                true,
                false
            ),
            Resolution::Unauthenticated { presented: false },
            "a loopback listener with a bearer asks for it"
        );
        assert_eq!(
            with(open, true, true),
            Resolution::Unauthenticated { presented: false },
            "a browser is never the implicit operator"
        );
    }

    #[test]
    fn bearer_principals_get_their_declared_distinct_ids() {
        let r = Resolver::build(
            &a2a(json!({
                "listen": "http://127.0.0.1:8080",
                "principals": [
                    {"id": "token-a", "match": {"bearer_ref": "{{secret:A}}"}, "role": "user"},
                    {"id": "token-c", "match": {"bearer_ref": "{{secret:C}}"}, "role": "user"},
                    {"id": "ops", "match": {"bearer_ref": "{{secret:OPS}}"}, "role": "operator"},
                    {"id": "gone", "match": {"bearer_ref": "{{secret:ANON}}"}, "role": "anonymous"},
                ]
            })),
            &secrets,
        )
        .unwrap();
        let a = r.resolve(&ev(Some("token-a"), None), false, None);
        let c = r.resolve(&ev(Some("token-c"), None), false, None);
        assert_eq!(named(&a), ("user:token-a", Via::Bearer));
        assert_eq!(named(&c), ("user:token-c", Via::Bearer));
        let Resolution::Named(pa, _) = &a else {
            unreachable!()
        };
        assert_eq!(r.rule_of(pa), Some("token-a"));
        let ops = r.resolve(&ev(Some("ops-bearer"), None), false, None);
        assert_eq!(named(&ops), ("operator", Via::Bearer));
        // An anonymous-role bearer rule names nobody.
        assert_eq!(
            r.resolve(&ev(Some("anon-bearer"), None), false, None),
            Resolution::Unauthenticated { presented: true }
        );
        let status = r.status();
        assert_eq!(status["principals"][0]["id"], "token-a", "{status}");
        assert_eq!(status["implicit_operator"], false);
    }

    #[test]
    fn one_secret_cannot_mean_two_principals() {
        let build = |doc: Value| Resolver::build(&a2a(doc), &secrets).map(|_| ());
        let err = build(json!({"bearer": "{{secret:A}}", "principals": [
            {"id": "a", "match": {"bearer_ref": "{{secret:A}}"}, "role": "user"}
        ]}))
        .unwrap_err();
        assert!(err.contains("a2a.bearer"), "{err}");
        let err = build(json!({"principals": [
            {"id": "a", "match": {"bearer_ref": "{{secret:A}}"}, "role": "user"},
            {"id": "b", "match": {"bearer_ref": "{{secret:A}}"}, "role": "agent"}
        ]}))
        .unwrap_err();
        assert!(err.contains("another rule"), "{err}");
        let reserved = |k: &str| (k == "X").then(|| "agentd_at_looks_like_a_session".to_string());
        let err = Resolver::build(
            &a2a(json!({"principals": [
                {"id": "a", "match": {"bearer_ref": "{{secret:X}}"}, "role": "user"}
            ]})),
            &reserved,
        )
        .map(|_| ())
        .unwrap_err();
        assert!(err.contains("agentd_at_"), "{err}");
        let err = Resolver::build(&a2a(json!({"bearer": "{{secret:X}}"})), &reserved)
            .map(|_| ())
            .unwrap_err();
        assert!(err.contains("agentd_at_"), "{err}");
        // Distinct secrets are fine.
        build(json!({"bearer": "{{secret:B}}", "principals": [
            {"id": "a", "match": {"bearer_ref": "{{secret:A}}"}, "role": "user"},
            {"id": "c", "match": {"bearer_ref": "{{secret:C}}"}, "role": "user"}
        ]}))
        .unwrap();
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
            &secrets,
        )
        .unwrap();
        let declared = r.resolve(&ev(Some("d3pl0y"), None), false, None);
        assert_eq!(named(&declared).0, "user:deploy-bot");
        // CN `deploy-bot`, admitted by the san rule through a SAN it matches.
        let by_cn = r.resolve(
            &ev(None, cert(&["x.corp"], Some("deploy-bot"))),
            false,
            None,
        );
        assert_eq!(named(&by_cn).0, "user:cn=deploy-bot");
        // No CN; the first SAN is `deploy-bot` and a later one matches the rule.
        let by_san = r.resolve(
            &ev(None, cert(&["deploy-bot", "x.corp"], None)),
            false,
            None,
        );
        assert_eq!(named(&by_san).0, "user:san=deploy-bot");
        let Resolution::Named(p, _) = &by_cn else {
            unreachable!()
        };
        assert_eq!(r.rule_of(p), None, "no rule declared the certificate's id");
    }

    /// The backstop behind the validation that requires `id` on bearer_ref
    /// and any rules: a rule that names nobody, matched by evidence that names
    /// nobody, does not name anybody. Built past validation on purpose — this
    /// is the line that holds when validation is not what built the rules.
    #[test]
    fn evidence_that_names_nobody_never_becomes_user_unknown() {
        let r = Resolver::build(
            &a2a(json!({"principals": [{"match": {"any": true}, "role": "user"}]})),
            &secrets,
        )
        .unwrap();
        assert_eq!(
            r.resolve(&Evidence::default(), false, None),
            Resolution::Unauthenticated { presented: false }
        );
        // The same rule still names a certificate holder by its CN.
        let holder = r.resolve(&ev(None, cert(&[], Some("alice"))), false, None);
        assert_eq!(named(&holder), ("user:cn=alice", Via::Cert));
    }

    #[test]
    fn glob_matches_a_single_star() {
        assert!(glob("a*c", "abc") && glob("*", "x") && !glob("a*c", "abx"));
    }
}
