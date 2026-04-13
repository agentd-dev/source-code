//! HMAC-SHA256 webhook verification.
//!
//! Pattern lifted from GitHub / Stripe / Slack: signature in a
//! header, optional prefix, HMAC-SHA256 of the raw request body.
//! Computed digest is compared constant-time (via the `hmac` crate's
//! `verify_slice` which is constant-time internally).

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::auth::{AuthConfig, AuthDecision, AuthRequest, Principal};

type HmacSha256 = Hmac<Sha256>;

pub fn verify(config: &AuthConfig, name: &str, req: &AuthRequest<'_>) -> AuthDecision {
    let Some(def) = config.hmac.get(name) else {
        return AuthDecision::Deny {
            reason: format!("hmac binding `{name}` is not defined"),
        };
    };
    let Some(secret) = def.secret_bytes() else {
        return AuthDecision::Deny {
            reason: format!("hmac binding `{name}` has no secret configured"),
        };
    };

    let header_name = def.effective_header().to_ascii_lowercase();
    let Some(header_value) = req.headers.get(&header_name) else {
        return AuthDecision::Deny {
            reason: format!("missing signature header `{}`", def.effective_header()),
        };
    };

    let prefix = def.effective_prefix();
    let hex_value = match header_value.strip_prefix(prefix) {
        Some(h) => h,
        None if prefix.is_empty() => header_value.as_str(),
        None => {
            return AuthDecision::Deny {
                reason: format!(
                    "signature header `{}` missing expected prefix `{prefix}`",
                    def.effective_header()
                ),
            };
        }
    };
    let presented = match hex_decode(hex_value.trim()) {
        Ok(b) => b,
        Err(_) => {
            return AuthDecision::Deny {
                reason: format!(
                    "signature header `{}` is not valid hex",
                    def.effective_header()
                ),
            };
        }
    };

    let mut mac = match HmacSha256::new_from_slice(&secret) {
        Ok(m) => m,
        Err(e) => {
            return AuthDecision::Deny {
                reason: format!("hmac setup failed: {e}"),
            };
        }
    };
    mac.update(req.body);
    match mac.verify_slice(&presented) {
        Ok(()) => AuthDecision::Allow {
            principal: Principal {
                kind: "hmac",
                name: name.to_string(),
            },
        },
        Err(_) => AuthDecision::Deny {
            reason: "hmac signature does not match".into(),
        },
    }
}

/// Lowercase hex → bytes. Plenty of workflow tests use lowercase
/// per GitHub convention, and `.eq_ignore_ascii_case` in the
/// matcher would weaken constant-time semantics.
fn hex_decode(input: &str) -> std::result::Result<Vec<u8>, ()> {
    if input.len() % 2 != 0 {
        return Err(());
    }
    let mut out = Vec::with_capacity(input.len() / 2);
    for chunk in input.as_bytes().chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> std::result::Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(()),
    }
}

/// Public helper for tests + admin tooling: compute the hex digest
/// that a signed webhook would carry.
pub fn sign_hex(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac key is any length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::config::HmacDef;
    use std::collections::HashMap;

    fn cfg(name: &str, def: HmacDef) -> AuthConfig {
        let mut c = AuthConfig::default();
        c.hmac.insert(name.into(), def);
        c
    }

    fn headers(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect()
    }

    #[test]
    fn allows_matching_signature() {
        let def = HmacDef {
            secret: Some("s3cret".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        let body = br#"{"event":"push"}"#;
        let sig = sign_hex(b"s3cret", body);
        let hs = headers(&[("X-Agent-Signature", &format!("sha256={sig}"))]);
        let r = AuthRequest { headers: &hs, body };
        match verify(&c, "gh", &r) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "hmac");
                assert_eq!(principal.name, "gh");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn denies_wrong_signature() {
        let def = HmacDef {
            secret: Some("s3cret".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        // Deliberately wrong: 64 hex zeros.
        let hs = headers(&[(
            "X-Agent-Signature",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
        )]);
        let r = AuthRequest {
            headers: &hs,
            body: b"{}",
        };
        match verify(&c, "gh", &r) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("does not match"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn custom_header_and_prefix() {
        let def = HmacDef {
            secret: Some("k".into()),
            header: Some("X-Hub-Signature-256".into()),
            prefix: Some("sha256=".into()),
            ..HmacDef::default()
        };
        let c = cfg("hub", def);
        let body = b"payload";
        let sig = sign_hex(b"k", body);
        let hs = headers(&[("X-Hub-Signature-256", &format!("sha256={sig}"))]);
        let r = AuthRequest { headers: &hs, body };
        assert!(matches!(verify(&c, "hub", &r), AuthDecision::Allow { .. }));
    }

    #[test]
    fn denies_missing_signature_header() {
        let def = HmacDef {
            secret: Some("k".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        let hs = headers(&[]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
        };
        assert!(matches!(
            verify(&c, "gh", &r),
            AuthDecision::Deny { reason }
                if reason.contains("missing signature header")
        ));
    }

    #[test]
    fn denies_bad_hex() {
        let def = HmacDef {
            secret: Some("k".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        let hs = headers(&[("X-Agent-Signature", "sha256=not-hex")]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
        };
        assert!(matches!(
            verify(&c, "gh", &r),
            AuthDecision::Deny { reason }
                if reason.contains("not valid hex")
        ));
    }

    #[test]
    fn denies_missing_prefix() {
        let def = HmacDef {
            secret: Some("k".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        let hs = headers(&[("X-Agent-Signature", "deadbeef")]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
        };
        assert!(matches!(
            verify(&c, "gh", &r),
            AuthDecision::Deny { reason }
                if reason.contains("missing expected prefix")
        ));
    }

    #[test]
    fn empty_prefix_is_honoured() {
        let def = HmacDef {
            secret: Some("k".into()),
            prefix: Some("".into()),
            ..HmacDef::default()
        };
        let c = cfg("gh", def);
        let body = b"x";
        let sig = sign_hex(b"k", body);
        let hs = headers(&[("X-Agent-Signature", &sig)]);
        let r = AuthRequest { headers: &hs, body };
        assert!(matches!(verify(&c, "gh", &r), AuthDecision::Allow { .. }));
    }

    #[test]
    fn no_secret_is_deny() {
        let def = HmacDef::default();
        let c = cfg("gh", def);
        let hs = headers(&[]);
        let r = AuthRequest {
            headers: &hs,
            body: b"",
        };
        assert!(matches!(
            verify(&c, "gh", &r),
            AuthDecision::Deny { reason }
                if reason.contains("no secret configured")
        ));
    }

    #[test]
    fn sign_hex_round_trip() {
        let sig = sign_hex(b"key", b"msg");
        assert_eq!(sig.len(), 64);
        assert!(
            sig.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn hex_decode_variants() {
        assert_eq!(
            hex_decode("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            hex_decode("DEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(hex_decode("odd").is_err());
        assert!(hex_decode("zz").is_err());
    }
}
