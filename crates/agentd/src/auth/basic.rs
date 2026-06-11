//! HTTP basic-auth verification (RFC 7617).
//!
//! Reads `Authorization: Basic <base64(user:pass)>` and compares
//! against a configured credential set constant-time. Exists for
//! webhook callers that can't set custom headers but can put
//! credentials in the URL — Twilio voice webhooks being the canonical
//! case. Same trust model as bearer: the channel must be confidential
//! (TLS at a gateway or the in-process listener).
//!
//! The base64 decoder is hand-rolled (strict RFC 4648, padding
//! required) to keep the default `auth` build dependency-free — the
//! same posture as the hand-rolled HTTP server it sits behind.

use crate::auth::bearer::ct_eq;
use crate::auth::{AuthConfig, AuthDecision, AuthRequest, Principal};
use serde::{Deserialize, Serialize};

const AUTH_HEADER: &str = "authorization";
const BASIC_PREFIX: &str = "Basic ";

/// One `[auth.basic.<name>]` binding: a set of `user:pass` entries,
/// newline-separated in an env var (production) or literal (tests).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BasicDef {
    #[serde(default)]
    pub credentials_env: Option<String>,
    #[serde(default)]
    pub credentials: Vec<String>,
}

impl BasicDef {
    /// Materialise the current credential set (`user:pass` strings).
    pub fn credentials(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .credentials
            .iter()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .collect();
        if let Some(var) = &self.credentials_env
            && let Ok(raw) = crate::secrets::resolve(var)
        {
            out.extend(
                raw.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from),
            );
        }
        out
    }
}

pub fn verify(config: &AuthConfig, name: &str, req: &AuthRequest<'_>) -> AuthDecision {
    let Some(def) = config.basic.get(name) else {
        return AuthDecision::Deny {
            reason: format!("basic binding `{name}` is not defined"),
        };
    };
    let Some(header) = req.headers.get(AUTH_HEADER) else {
        return AuthDecision::Deny {
            reason: "missing Authorization header".into(),
        };
    };
    let Some(encoded) = header.strip_prefix(BASIC_PREFIX) else {
        return AuthDecision::Deny {
            reason: "Authorization header does not use Basic scheme".into(),
        };
    };
    let Some(decoded) = base64_decode(encoded.trim()) else {
        return AuthDecision::Deny {
            reason: "Basic credentials are not valid base64".into(),
        };
    };
    let Ok(presented) = String::from_utf8(decoded) else {
        return AuthDecision::Deny {
            reason: "Basic credentials decode to invalid UTF-8".into(),
        };
    };
    // RFC 7617: the user-id may not contain a colon; the password may.
    // Compare the whole `user:pass` string — entries are stored in the
    // same shape, so a colon-in-password just works.
    let creds = def.credentials();
    if creds.is_empty() {
        return AuthDecision::Deny {
            reason: format!("basic binding `{name}` has no credentials configured"),
        };
    }
    for c in &creds {
        if ct_eq(presented.as_bytes(), c.as_bytes()) {
            let user = presented.split(':').next().unwrap_or("").to_string();
            return AuthDecision::Allow {
                principal: Principal {
                    kind: "basic",
                    name: user,
                },
            };
        }
    }
    AuthDecision::Deny {
        reason: "basic credentials do not match".into(),
    }
}

/// Strict RFC 4648 base64 (standard alphabet, padding required).
/// Returns `None` on any irregularity — fail closed, never guess.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i == bytes.len() / 4 - 1;
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        // Padding only in the final chunk, only as the trailing 1-2.
        if (pad > 0 && !last) || pad > 2 {
            return None;
        }
        if pad > 0 && chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        let v0 = val(chunk[0])?;
        let v1 = val(chunk[1])?;
        let v2 = if pad >= 2 { 0 } else { val(chunk[2])? };
        let v3 = if pad >= 1 { 0 } else { val(chunk[3])? };
        let triple = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push((triple >> 16) as u8);
        if pad < 2 {
            out.push((triple >> 8) as u8);
        }
        if pad < 1 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config_with(name: &str, creds: Vec<&str>) -> AuthConfig {
        let mut cfg = AuthConfig::default();
        cfg.basic.insert(
            name.into(),
            BasicDef {
                credentials_env: None,
                credentials: creds.into_iter().map(String::from).collect(),
            },
        );
        cfg
    }

    fn request(headers: &HashMap<String, String>) -> AuthRequest<'_> {
        AuthRequest {
            headers,
            body: b"",
            peer_cert_fingerprint: None,
        }
    }

    fn hdrs(pairs: Vec<(&str, &str)>) -> HashMap<String, String> {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .collect()
    }

    #[test]
    fn allows_matching_credentials_and_extracts_user() {
        let cfg = config_with("twilio", vec!["acct:s3cret"]);
        // base64("acct:s3cret")
        let hs = hdrs(vec![("Authorization", "Basic YWNjdDpzM2NyZXQ=")]);
        match verify(&cfg, "twilio", &request(&hs)) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "basic");
                assert_eq!(principal.name, "acct");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn colon_in_password_works() {
        // base64("u:p:w") — password is "p:w".
        let cfg = config_with("x", vec!["u:p:w"]);
        let hs = hdrs(vec![("Authorization", "Basic dTpwOnc=")]);
        assert!(matches!(
            verify(&cfg, "x", &request(&hs)),
            AuthDecision::Allow { .. }
        ));
    }

    #[test]
    fn denies_wrong_credentials_missing_header_and_bad_base64() {
        let cfg = config_with("x", vec!["u:p"]);
        let hs = hdrs(vec![("Authorization", "Basic dTp3cm9uZw==")]); // u:wrong
        assert!(matches!(
            verify(&cfg, "x", &request(&hs)),
            AuthDecision::Deny { .. }
        ));
        let hs = hdrs(vec![]);
        assert!(matches!(
            verify(&cfg, "x", &request(&hs)),
            AuthDecision::Deny { .. }
        ));
        let hs = hdrs(vec![("Authorization", "Basic !!!!")]);
        match verify(&cfg, "x", &request(&hs)) {
            AuthDecision::Deny { reason } => assert!(reason.contains("base64"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn base64_decoder_is_strict() {
        assert_eq!(base64_decode("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(base64_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(base64_decode("YWI="), Some(b"ab".to_vec()));
        assert!(base64_decode("YWJ").is_none()); // bad length
        assert!(base64_decode("Y=Jj").is_none()); // padding mid-chunk
        assert!(base64_decode("YQ==YQ==").is_none()); // padding mid-stream
        assert!(base64_decode("Y!Jj").is_none()); // bad alphabet
        assert!(base64_decode("").is_none());
    }
}
