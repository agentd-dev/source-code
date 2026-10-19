// SPDX-License-Identifier: AGPL-3.0-only
//! **Typed principals** (RFC 0044 §5.2): who a caller is, as the tuple the
//! transport that authenticated it established — never as the display
//! string. A display string can collide: every device an operator approves
//! under one name is `user:<name>`, an operator-scope device session is
//! `operator:<name>`, and a certificate's CN can read like anybody. The tuple
//! cannot: its `source` says how the caller was authenticated, so two
//! callers that share a string are still two principals.
//!
//! Every key derived for a principal — [`PrincipalRef::grant_id`] — is a
//! SHA-256 over the tuple's fields, each length-prefixed, so a field holding
//! a separator character cannot make two tuples encode alike. The display
//! string is derived from the tuple ([`PrincipalRef::display`]) and is never
//! parsed back.

use serde::{Deserialize, Serialize};

/// How a principal was authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// An identity provider's verified token: its issuer and subject.
    Oidc,
    /// A verified client certificate: its CA and public key.
    X509,
    /// A bearer secret a principal rule names, or an `any` rule.
    Bearer,
    /// A device-grant session (RFC 0043), by the name its approver gave it.
    Device,
    /// An AAuth identity.
    Aauth,
    /// The operator: the unix listener's peer, the server bearer, a
    /// certificate no rule names, the loopback default. `stable_id` names a
    /// per-person operator, and is empty for the bare operator.
    Operator,
    /// Work nobody typed: `identity.autonomous_as`.
    Autonomous,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Oidc => "oidc",
            Source::X509 => "x509",
            Source::Bearer => "bearer",
            Source::Device => "device",
            Source::Aauth => "aauth",
            Source::Operator => "operator",
            Source::Autonomous => "autonomous",
        }
    }
}

/// The issuer every principal agentd itself authenticated is under.
pub const AGENTD: &str = "agentd";

/// A principal as the tuple its transport established.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrincipalRef {
    pub source: Source,
    /// The identity provider's issuer (`oidc`); the client CA (`x509`);
    /// `agentd` for every principal agentd authenticated itself.
    pub issuer: String,
    /// The tenant of a multi-tenant issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// `oidc`: the issuer's subject; `x509`: the SHA-256 of the
    /// certificate's SubjectPublicKeyInfo, hex; `bearer`: the rule's id;
    /// `device`: the name its approver gave it; `operator`: the per-person
    /// operator's id, empty for the bare operator; `autonomous`: the actor.
    pub stable_id: String,
    /// `oidc`: the client of the token that authenticated the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

impl PrincipalRef {
    /// A principal agentd authenticated itself, under [`AGENTD`].
    pub fn local(source: Source, stable_id: &str) -> PrincipalRef {
        PrincipalRef {
            source,
            issuer: AGENTD.to_string(),
            tenant: None,
            stable_id: stable_id.to_string(),
            client: None,
        }
    }

    /// The bare operator: no person named.
    pub fn operator() -> PrincipalRef {
        PrincipalRef::local(Source::Operator, "")
    }

    /// Whether this is the bare operator, which names no person.
    pub fn is_bare_operator(&self) -> bool {
        self.source == Source::Operator && self.stable_id.is_empty()
    }

    /// The key of this principal's delegation grant at the identity
    /// provider `idp_iss`: `sha256(enc("grant") ‖ enc(source) ‖ enc(issuer)
    /// ‖ enc(tenant) ‖ enc(stable_id) ‖ enc(idp_iss))`, hex, where `enc` is
    /// a four-byte big-endian length then the bytes (an absent field is
    /// empty). Every cache, pool and lock a grant has is keyed by it.
    pub fn grant_id(&self, idp_iss: &str) -> String {
        let parts: [&[u8]; 6] = [
            b"grant",
            self.source.as_str().as_bytes(),
            self.issuer.as_bytes(),
            self.tenant.as_deref().unwrap_or_default().as_bytes(),
            self.stable_id.as_bytes(),
            idp_iss.as_bytes(),
        ];
        let mut enc = Vec::new();
        for p in parts {
            enc.extend_from_slice(&(p.len() as u32).to_be_bytes());
            enc.extend_from_slice(p);
        }
        crate::sha::sha256_hex(&enc)
    }

    /// The display string, derived from the tuple and never parsed back: an
    /// identity provider's principal is `user:oidc:<iss>#<sub>`, a namespace
    /// no transport matcher produces.
    pub fn display(&self) -> String {
        match self.source {
            Source::Oidc => format!("user:oidc:{}#{}", self.issuer, self.stable_id),
            Source::X509 => format!("x509:{}", self.stable_id),
            Source::Bearer | Source::Device => format!("user:{}", self.stable_id),
            Source::Aauth => format!("aauth:{}", self.stable_id),
            Source::Operator if self.stable_id.is_empty() => "operator".to_string(),
            Source::Operator => format!("operator:{}", self.stable_id),
            Source::Autonomous => self.stable_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grant key is over the tuple, each field length-prefixed: two
    /// principals that share a display string, or whose fields would join
    /// alike, never share a key; one principal at two providers has two.
    #[test]
    fn a_grant_key_is_the_tuple_never_the_string() {
        let device = PrincipalRef::local(Source::Device, "alice");
        let rule = PrincipalRef::local(Source::Bearer, "alice");
        assert_eq!(device.display(), rule.display(), "one display string");
        let idp = "https://acme.okta.com";
        assert_ne!(device.grant_id(idp), rule.grant_id(idp));
        assert_ne!(
            device.grant_id(idp),
            device.grant_id("https://other.example")
        );
        // A separator inside a field does not move the boundary.
        let a = PrincipalRef {
            issuer: "ab".into(),
            ..PrincipalRef::local(Source::Oidc, "c")
        };
        let b = PrincipalRef {
            issuer: "a".into(),
            ..PrincipalRef::local(Source::Oidc, "bc")
        };
        assert_ne!(a.grant_id(idp), b.grant_id(idp));
        assert_eq!(a.grant_id(idp).len(), 64);
        assert_eq!(a.grant_id(idp), a.clone().grant_id(idp), "stable");
        // An IdP's principal lives where no matcher reaches.
        let o = PrincipalRef {
            issuer: "https://acme.okta.com".into(),
            ..PrincipalRef::local(Source::Oidc, "00u1alice")
        };
        assert_eq!(o.display(), "user:oidc:https://acme.okta.com#00u1alice");
        assert!(PrincipalRef::operator().is_bare_operator());
        assert!(!PrincipalRef::local(Source::Operator, "alice").is_bare_operator());
        assert_eq!(
            PrincipalRef::local(Source::Operator, "alice").display(),
            "operator:alice"
        );
    }
}
