//! Bearer-token verification.
//!
//! Reads `Authorization: Bearer <token>`, compares against a set
//! of configured tokens constant-time. Missing / malformed header
//! → `Deny`. No hashing, no signing — the bearer model trusts that
//! the channel is confidential (TLS termination at a gateway, or
//! the R3b in-process TLS listener).

use crate::auth::{AuthConfig, AuthDecision, AuthRequest, Principal};

const AUTH_HEADER: &str = "authorization";
const BEARER_PREFIX: &str = "Bearer ";

pub fn verify(config: &AuthConfig, name: &str, req: &AuthRequest<'_>) -> AuthDecision {
    let Some(def) = config.bearer.get(name) else {
        // Caller should have validated at startup; fail closed.
        return AuthDecision::Deny {
            reason: format!("bearer binding `{name}` is not defined"),
        };
    };

    // Look up `Authorization: Bearer <token>`. Header keys are
    // stored lowercased by the HTTP parser.
    let Some(header) = req.headers.get(AUTH_HEADER) else {
        return AuthDecision::Deny {
            reason: "missing Authorization header".into(),
        };
    };
    let Some(presented) = header.strip_prefix(BEARER_PREFIX) else {
        return AuthDecision::Deny {
            reason: "Authorization header does not use Bearer scheme".into(),
        };
    };
    let presented = presented.trim();
    if presented.is_empty() {
        return AuthDecision::Deny {
            reason: "Bearer token is empty".into(),
        };
    }

    let tokens = def.tokens();
    if tokens.is_empty() {
        return AuthDecision::Deny {
            reason: format!("bearer binding `{name}` has no tokens configured"),
        };
    }
    for t in &tokens {
        if ct_eq(presented.as_bytes(), t.as_bytes()) {
            return AuthDecision::Allow {
                principal: Principal {
                    kind: "bearer",
                    name: name.to_string(),
                },
            };
        }
    }
    AuthDecision::Deny {
        reason: "bearer token does not match".into(),
    }
}

/// Constant-time byte-slice equality. Early-exits only on length
/// difference, which is public information (not the token itself).
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::config::BearerDef;
    use std::collections::HashMap;

    fn req(headers: Vec<(&str, &str)>) -> (HashMap<String, String>, AuthRequest<'static>) {
        // Dummy body; not used by bearer.
        let headers: HashMap<String, String> = headers
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect();
        let body: &'static [u8] = b"";
        let r = AuthRequest {
            headers: unsafe {
                // SAFETY: the tests always copy the HashMap out of
                // the tuple before inspecting the request; this is
                // just to satisfy the lifetime marker.
                &*(&headers as *const HashMap<String, String>)
            },
            body,
            peer_cert_fingerprint: None,
        };
        (headers, r)
    }

    fn config_with(name: &str, tokens: Vec<&str>) -> AuthConfig {
        let mut cfg = AuthConfig::default();
        cfg.bearer.insert(
            name.into(),
            BearerDef {
                tokens_env: None,
                tokens: tokens.into_iter().map(String::from).collect(),
            },
        );
        cfg
    }

    #[test]
    fn allows_matching_token() {
        let cfg = config_with("ops", vec!["s3cret"]);
        let (hs, r) = req(vec![("Authorization", "Bearer s3cret")]);
        let r = AuthRequest {
            headers: &hs,
            body: r.body,
            peer_cert_fingerprint: None,
        };
        match verify(&cfg, "ops", &r) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "bearer");
                assert_eq!(principal.name, "ops");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn denies_wrong_token() {
        let cfg = config_with("ops", vec!["s3cret"]);
        let (hs, _) = req(vec![("Authorization", "Bearer other")]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match verify(&cfg, "ops", &r) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("does not match"));
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn denies_missing_header() {
        let cfg = config_with("ops", vec!["s3cret"]);
        let (hs, _) = req(vec![]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match verify(&cfg, "ops", &r) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("missing Authorization"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn denies_non_bearer_scheme() {
        let cfg = config_with("ops", vec!["s3cret"]);
        let (hs, _) = req(vec![("Authorization", "Basic dXNlcjpwYXNz")]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match verify(&cfg, "ops", &r) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("Bearer scheme"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn denies_unknown_binding() {
        let cfg = AuthConfig::default();
        let (hs, _) = req(vec![("Authorization", "Bearer x")]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match verify(&cfg, "missing", &r) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("not defined"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ct_eq_semantics() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }
}
