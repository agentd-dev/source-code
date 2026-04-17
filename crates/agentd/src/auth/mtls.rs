//! mTLS verifier.
//!
//! The real cryptographic validation happens at the TLS layer —
//! `rustls`'s `WebPkiClientVerifier` rejects any connection whose
//! client cert doesn't chain to the configured CA before a byte
//! reaches the HTTP parser. This module only asserts that the
//! fingerprint was populated (i.e. the handshake produced a cert)
//! and surfaces it as the `Principal`.
//!
//! Builds without `server-tls` never set `peer_cert_fingerprint`,
//! so this path denies with a clear message.

use crate::auth::{AuthDecision, AuthRequest, Principal};

pub fn verify(req: &AuthRequest<'_>) -> AuthDecision {
    match req.peer_cert_fingerprint {
        Some(fp) => AuthDecision::Allow {
            principal: Principal {
                kind: "mtls",
                name: fp.to_string(),
            },
        },
        None => AuthDecision::Deny {
            reason: "mtls required but no client cert was presented at the TLS layer".into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn allow_when_fingerprint_present() {
        let headers = HashMap::new();
        let req = AuthRequest {
            headers: &headers,
            body: b"",
            peer_cert_fingerprint: Some("sha256:deadbeef"),
        };
        match verify(&req) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "mtls");
                assert_eq!(principal.name, "sha256:deadbeef");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn deny_when_no_fingerprint() {
        let headers = HashMap::new();
        let req = AuthRequest {
            headers: &headers,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match verify(&req) {
            AuthDecision::Deny { reason } => assert!(reason.contains("mtls")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }
}
