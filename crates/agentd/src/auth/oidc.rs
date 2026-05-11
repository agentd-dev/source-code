//! OIDC / JWT bearer authentication.
//!
//! Validates `Authorization: Bearer <jwt>` against an
//! operator-supplied JWKS + a set of claim constraints (`iss`,
//! `aud`, `exp`, `nbf`, optional `sub` allowlist). Feature-gated on
//! `auth-oidc`; the module compiles to a stub when the feature is
//! off so other auth paths stay unaffected.
//!
//! ## Scope (v1)
//!
//! JWKS comes from disk or inline TOML — the operator rotates it
//! externally (cron, sidecar, config-mgmt). Live JWKS fetch over
//! HTTPS is deferred to v2; pulling a full HTTP/TLS client into
//! the auth path at runtime is a big dep expansion for the marginal
//! convenience of saved rotation. File/inline is the same pattern
//! the workflow-signing path uses for pinned keys.
//!
//! ## Supported algorithms
//!
//! RS256, RS384, RS512 (RSA PKCS#1 v1.5) and ES256, ES384 (ECDSA).
//! Per the JWT JOSE spec, `alg` from the token header is checked
//! against a per-binding allowlist — `none` and HS* are rejected
//! by default to prevent algorithm-confusion attacks.
//!
//! ## Audit events
//!
//! `oidc.verified` on success (fields: binding, subject, issuer).
//! `oidc.denied` on failure with a reason code. Errors are coarse
//! on purpose — leaking exact validation reasons gives attackers a
//! probe surface.

use serde::{Deserialize, Serialize};

use crate::auth::{AuthDecision, AuthRequest};
// `Principal` and `HashMap` are only used under `auth-oidc`; scoping
// them avoids unused-import errors on the feature-off build.
#[cfg(feature = "auth-oidc")]
use crate::auth::Principal;
#[cfg(feature = "auth-oidc")]
use std::collections::HashMap;

/// `[auth.oidc.<binding>]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OidcDef {
    /// Expected `iss` claim — required. Verification fails if the
    /// token's `iss` differs, even if the signature is valid.
    pub issuer: String,

    /// Expected `aud` claim — list form accepts multi-audience
    /// tokens. At least one of the list must match the token's
    /// audience.
    #[serde(default)]
    pub audience: Vec<String>,

    /// Inline JWKS JSON (`{"keys":[{kty,kid,n,e,…}, …]}`). Mutually
    /// exclusive with `jwks_file`.
    #[serde(default)]
    pub jwks_json: Option<String>,

    /// Filesystem path to a JWKS document. Rotated externally.
    #[serde(default)]
    pub jwks_file: Option<std::path::PathBuf>,

    /// If set, only tokens whose `sub` appears in this list are
    /// allowed through. Useful for service-account pinning.
    #[serde(default)]
    pub subject_allowlist: Vec<String>,

    /// Grace window for `exp` / `nbf` drift. Default 60s.
    #[serde(default = "default_clock_skew")]
    pub clock_skew_secs: u64,

    /// Accepted signing algorithms. Default: RS256.
    /// Operators add ES256 / RS384 / etc. as needed. `none` and
    /// HS* are always rejected regardless.
    #[serde(default = "default_algorithms")]
    pub algorithms: Vec<String>,
}

fn default_clock_skew() -> u64 {
    60
}

fn default_algorithms() -> Vec<String> {
    vec!["RS256".to_string()]
}

impl OidcDef {
    /// Load the raw JWKS JSON for this binding — either the inline
    /// string or the file path. Exactly one must be set.
    pub fn jwks_source(&self) -> Result<String, String> {
        match (&self.jwks_json, &self.jwks_file) {
            (Some(j), None) => Ok(j.clone()),
            (None, Some(p)) => std::fs::read_to_string(p)
                .map_err(|e| format!("open jwks_file {}: {e}", p.display())),
            (Some(_), Some(_)) => {
                Err("auth.oidc: jwks_json and jwks_file are mutually exclusive".into())
            }
            (None, None) => Err("auth.oidc: one of jwks_json / jwks_file must be set".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Verifier — feature-gated
// ---------------------------------------------------------------------------

#[cfg(feature = "auth-oidc")]
mod verifier {
    use super::*;
    use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};

    /// Pre-processed state built once at server spawn — parses the
    /// JWKS, validates algorithm allowlist, indexes decoding keys
    /// by `kid`. Loaded fresh on every config reload.
    //
    // `DecodingKey` doesn't implement `Debug`, so neither can we
    // derive. Hand-roll a no-secret-leak Debug impl instead.
    pub struct PreparedOidc {
        /// kid → (algorithm, DecodingKey). Missing `kid` in the
        /// token falls back to the first key when the JWKS has
        /// exactly one — common in small deployments.
        pub keys: HashMap<String, (Algorithm, DecodingKey)>,
        pub def: OidcDef,
        pub algorithms: Vec<Algorithm>,
    }

    impl std::fmt::Debug for PreparedOidc {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("PreparedOidc")
                .field("issuer", &self.def.issuer)
                .field("audience", &self.def.audience)
                .field("kids", &self.keys.keys().collect::<Vec<_>>())
                .field("algorithms", &self.algorithms)
                .finish()
        }
    }

    pub fn prepare(def: &OidcDef) -> Result<PreparedOidc, String> {
        // Parse the algorithm allowlist; reject anything unsafe
        // upfront so we never even try to verify an `alg: none`
        // token.
        let mut algorithms = Vec::with_capacity(def.algorithms.len());
        for raw in &def.algorithms {
            let alg = parse_algorithm(raw)?;
            algorithms.push(alg);
        }
        if algorithms.is_empty() {
            return Err("auth.oidc.algorithms must contain at least one entry".into());
        }

        // Load and parse the JWKS.
        let raw = def.jwks_source()?;
        let set: JwkSet =
            serde_json::from_str(&raw).map_err(|e| format!("parse jwks JSON: {e}"))?;
        if set.keys.is_empty() {
            return Err("jwks contains no keys".into());
        }
        let mut keys = HashMap::with_capacity(set.keys.len());
        for jwk in set.keys {
            let kid = jwk
                .common
                .key_id
                .clone()
                .unwrap_or_else(|| "default".into());
            let alg = jwk
                .common
                .key_algorithm
                .and_then(|a| a.to_string().parse::<Algorithm>().ok())
                .unwrap_or(Algorithm::RS256);
            let key = match jwk.algorithm {
                AlgorithmParameters::RSA(ref rsa) => {
                    DecodingKey::from_rsa_components(&rsa.n, &rsa.e)
                        .map_err(|e| format!("rsa jwk {kid}: {e}"))?
                }
                AlgorithmParameters::EllipticCurve(ref ec) => {
                    DecodingKey::from_ec_components(&ec.x, &ec.y)
                        .map_err(|e| format!("ec jwk {kid}: {e}"))?
                }
                AlgorithmParameters::OctetKey(_) => {
                    // Symmetric keys in a JWKS are almost always an
                    // error in an OIDC context (HS* is confusable
                    // with RS* at the JOSE header level). Refuse.
                    return Err(format!(
                        "jwk {kid}: symmetric keys are not supported in auth.oidc"
                    ));
                }
                _ => {
                    return Err(format!("jwk {kid}: unsupported key type"));
                }
            };
            keys.insert(kid, (alg, key));
        }

        Ok(PreparedOidc {
            keys,
            def: def.clone(),
            algorithms,
        })
    }

    /// Verify a bearer JWT under this prepared OIDC binding.
    pub fn verify(prep: &PreparedOidc, token: &str) -> AuthDecision {
        // Decode header first to pick the key.
        let header = match decode_header(token) {
            Ok(h) => h,
            Err(_) => return deny("malformed"),
        };

        // Algorithm gate: must be in the allowlist. `alg: none` /
        // HS* are implicitly rejected because they're not in
        // `prep.algorithms` (we reject them at prepare time too).
        if !prep.algorithms.contains(&header.alg) {
            return deny("algorithm-not-allowed");
        }

        // Key selection: by kid if the token has one; fall back to
        // the sole JWKS key when absent.
        let key_entry = match header.kid.as_deref() {
            Some(kid) => prep.keys.get(kid),
            None if prep.keys.len() == 1 => prep.keys.values().next(),
            None => None,
        };
        let Some((_jwk_alg, key)) = key_entry else {
            return deny("unknown-kid");
        };

        // Claim constraints.
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(std::slice::from_ref(&prep.def.issuer));
        if !prep.def.audience.is_empty() {
            validation.set_audience(&prep.def.audience);
        } else {
            validation.validate_aud = false;
        }
        validation.leeway = prep.def.clock_skew_secs;
        // Require `exp` present; `nbf` optional.
        validation.set_required_spec_claims(&["exp", "iss"]);

        let data = match decode::<TokenClaims>(token, key, &validation) {
            Ok(d) => d,
            Err(err) => {
                return match err.kind() {
                    jsonwebtoken::errors::ErrorKind::ExpiredSignature => deny("expired"),
                    jsonwebtoken::errors::ErrorKind::InvalidIssuer => deny("bad-issuer"),
                    jsonwebtoken::errors::ErrorKind::InvalidAudience => deny("bad-audience"),
                    jsonwebtoken::errors::ErrorKind::InvalidSignature => deny("bad-signature"),
                    jsonwebtoken::errors::ErrorKind::ImmatureSignature => deny("not-yet-valid"),
                    jsonwebtoken::errors::ErrorKind::InvalidAlgorithm
                    | jsonwebtoken::errors::ErrorKind::InvalidAlgorithmName => {
                        deny("bad-algorithm")
                    }
                    _ => deny("invalid"),
                };
            }
        };

        // Subject allowlist (optional).
        let subject = data.claims.sub.clone().unwrap_or_default();
        if !prep.def.subject_allowlist.is_empty() && !prep.def.subject_allowlist.contains(&subject)
        {
            return deny("subject-not-allowed");
        }

        tracing::info!(
            target: "agentd::audit",
            event = "oidc.verified",
            issuer = %prep.def.issuer,
            subject = %subject,
        );
        AuthDecision::Allow {
            principal: Principal {
                kind: "oidc",
                name: if subject.is_empty() {
                    prep.def.issuer.clone()
                } else {
                    subject
                },
            },
        }
    }

    fn deny(code: &str) -> AuthDecision {
        tracing::warn!(
            target: "agentd::audit",
            event = "oidc.denied",
            reason = code,
        );
        AuthDecision::Deny {
            reason: format!("oidc.{code}"),
        }
    }

    fn parse_algorithm(raw: &str) -> Result<Algorithm, String> {
        // Explicitly reject unsafe algs so the error surface
        // carries a hint rather than a raw parse failure.
        match raw.to_ascii_uppercase().as_str() {
            "NONE" => Err("auth.oidc.algorithms: `none` is rejected — signature bypass".into()),
            "HS256" | "HS384" | "HS512" => Err(format!(
                "auth.oidc.algorithms: `{raw}` (symmetric) is rejected — use RS*/ES*"
            )),
            other => other
                .parse::<Algorithm>()
                .map_err(|_| format!("auth.oidc.algorithms: unknown `{raw}`")),
        }
    }
}

#[cfg(feature = "auth-oidc")]
pub use verifier::{PreparedOidc, prepare, verify};

#[cfg(feature = "auth-oidc")]
#[derive(Debug, Clone, Deserialize)]
struct TokenClaims {
    #[serde(default)]
    sub: Option<String>,
    // iss / aud / exp / nbf are validated by jsonwebtoken's
    // Validation; we only surface `sub` here for the principal.
}

// ---------------------------------------------------------------------------
// Off-feature stub: keep the call surface uniform so runtime.rs can
// call into this module unconditionally.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "auth-oidc"))]
#[derive(Debug)]
pub struct PreparedOidc;

#[cfg(not(feature = "auth-oidc"))]
pub fn prepare(_def: &OidcDef) -> Result<PreparedOidc, String> {
    Err(
        "workflow declares [auth.oidc] but this build lacks the `auth-oidc` \
         Cargo feature; rebuild with --features auth-oidc"
            .into(),
    )
}

#[cfg(not(feature = "auth-oidc"))]
pub fn verify(_prep: &PreparedOidc, _token: &str) -> AuthDecision {
    AuthDecision::Deny {
        reason: "oidc.unsupported".into(),
    }
}

/// Extract the bearer token from an `Authorization` header, matching
/// the case-insensitive `Bearer ` prefix. Returns `None` when the
/// header is absent or malformed — callers treat both as "no
/// credential".
pub fn extract_bearer<'a>(req: &'a AuthRequest<'a>) -> Option<&'a str> {
    let header = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))?
        .1
        .as_str();
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "auth-oidc"))]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    // A small RSA-2048 keypair fixture. Generated once, committed as
    // base64 so tests don't need OpenSSL at build time. Production
    // deployments use real keys from their identity provider.
    // Real RSA-2048 keypair generated once at the time this test
    // was written (openssl genpkey -algorithm RSA). The public half
    // below is decomposed into the `n` / `e` JWK components that
    // jsonwebtoken expects. Deterministic + no runtime key gen, so
    // tests stay hermetic and fast.
    const RSA_PRIVATE_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDG8Np1pGHANamV
Ztvj70351c2weVZHCPpD+z/mId4DBqZHaul7XZRFN6/Fwa/NDD+qct7hNegu9WF1
rJfB59LqJbH52Y50xlMVs7glmemtYmJKtki7g81K/FlLsPU9mDlLd/sacSpbeaqr
9CPz+cpX8p9ADQsCeXrLmArvBXw3SOUlFXsv40aULlwEA33b4X8LqD0Zf19zfIOW
lhC+zqMOKq2ZfHaeeTsTCZiloea220ESCj1urolmreJxcPzotshBC2vYSxIBUkhK
6KsoWe3DhRjQTjCQEvJxIWqeTLw0c8dc1LwI2HHQ7kfzz+wMIZd2z9p/LGIorM2F
wE0QevytAgMBAAECggEAW0gyhR5K9/3ndTUAGmM4fTVcLuhN7UQySTUkybyqeOr5
KvXkcgWrPeNiVLQdrVE8eUJCAEZS5hETigIKlB+CCIwJUOJBWlWR0/hlu2MW9Maa
4TsovlmovgpyEqf8rymEyJsh7a0VSWnXJRVd1dm8vYQHDEWv0o/ZB0gZZDk5GMga
i3KzhTmQceVOfxXPuYqm4qq/9grywsonf1IDx6FSPYLMfgeHL8zMTEpWiVzq2+1h
80n1y62jivpP8USKStFPFNV4F8nzKt1i5Zk5NAn8F3zXsqoOtUUNfnTwfHmQzARW
Ohj2vqoicxnk+L3wYsI9KSDpyCIh/vVtHTL5zsBVpQKBgQD7JAVjCK6PHQmVHFh1
VbvO8rb3HYWyEFVK5CTV6Rv+Su9PIGPKcl+BIdg9ne35ZSgVUoWUGRXQI3v+DyqI
udogW+Xpa5mIbH/Wqawf9A6o5dttJl6dNpXzC7PZNq94o0iMC/yZUW8znpA3aIMi
dOEH65pe+IilFG+h+IzLLs0ISwKBgQDKykUZA3vfZ8VdyhCsgIIe02JkKCJLnZjR
qG0rDejW8wQeUbJ6el7wDRxSK5/m8a+lwN0yp7YIXsmQT62lWiemO9ttqKJtSPtc
OlHPt1X8aiXeKxDgQUDWQ8ExiMOjmW8dH9UOIE/FQaVLOFklxAJbBdc5rRGgLQ9P
GOkK3prj5wKBgQCzXs7aJOFIJh0p+szTQSCadpBnfxZ2T2Qb0Ubd4Vi1DyBNC306
ouXDfUDNAXduoOk4EXCGjkQeHLn6gyqF7Pf2FKzpQoit/5Bu6VCeodm2mDVYiAcb
klkW9kzF32EEcNrn68fGWXtrCt1GNcczXPc8iPIA0tIF1crFjJhCpnKacQKBgDls
eZCRufwTKIJce8g9Q5tzBEOUUdHTuLh11yP/9lUXz6y+OaoRCN00+TYTgF4nRjPL
n1d+wj8wiCdDSMqv8tZR0NsGi6giqHr/ULdfFQw7CqoUy9yU3cVOvmBGeA/VnO9E
WlJ7t9sFscbRF/1nubsItl9wsLMIz3L4fNVFH9s1AoGBANij7fURR/EmF84ADhR6
yLa34yFKaoWaiR1FS3GHjYTFSbLG6Mpa6WVCYBzlppYkJeYdJNGeEpZR+hcWs5b/
eycc4qc+mpDO5dzt39Wzf7He/81zp+2tQwOZDUjZhoWLNY8jq5bfx9NfeH7rGtg/
TDcVKJbvCLIdtYhX1ytTIv9p
-----END PRIVATE KEY-----
"#;

    const RSA_PUBLIC_JWK: &str = r#"{
  "keys": [{
    "kty": "RSA",
    "use": "sig",
    "kid": "test-kid",
    "alg": "RS256",
    "n": "xvDadaRhwDWplWbb4-9N-dXNsHlWRwj6Q_s_5iHeAwamR2rpe12URTevxcGvzQw_qnLe4TXoLvVhdayXwefS6iWx-dmOdMZTFbO4JZnprWJiSrZIu4PNSvxZS7D1PZg5S3f7GnEqW3mqq_Qj8_nKV_KfQA0LAnl6y5gK7wV8N0jlJRV7L-NGlC5cBAN92-F_C6g9GX9fc3yDlpYQvs6jDiqtmXx2nnk7EwmYpaHmtttBEgo9bq6JZq3icXD86LbIQQtr2EsSAVJISuirKFntw4UY0E4wkBLycSFqnky8NHPHXNS8CNhx0O5H88_sDCGXds_afyxiKKzNhcBNEHr8rQ",
    "e": "AQAB"
  }]
}"#;

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn make_token(claims: serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-kid".into());
        let key = EncodingKey::from_rsa_pem(RSA_PRIVATE_PEM.as_bytes()).unwrap();
        encode(&header, &claims, &key).unwrap()
    }

    // `make_request` was part of an earlier sketch that exercised
    // `evaluate` directly; the current tests call `verify` with a
    // naked token, so the helper is retained only as a reference
    // example for future tests that need a populated AuthRequest.
    #[allow(dead_code)]
    fn make_request(token: &str) -> AuthRequest<'static> {
        let mut headers = HashMap::new();
        headers.insert("authorization".into(), format!("Bearer {token}"));
        let headers: &'static _ = Box::leak(Box::new(headers));
        AuthRequest {
            headers,
            body: &[],
            peer_cert_fingerprint: None,
        }
    }

    fn prod_def() -> OidcDef {
        OidcDef {
            issuer: "https://issuer.example.com".into(),
            audience: vec!["svc-api".into()],
            jwks_json: Some(RSA_PUBLIC_JWK.into()),
            jwks_file: None,
            subject_allowlist: vec![],
            clock_skew_secs: 60,
            algorithms: vec!["RS256".into()],
        }
    }

    #[test]
    fn verifies_valid_token() {
        let prep = prepare(&prod_def()).unwrap();
        let token = make_token(serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "svc-api",
            "sub": "service-a",
            "exp": now() + 60,
        }));
        match verify(&prep, &token) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "oidc");
                assert_eq!(principal.name, "service-a");
            }
            AuthDecision::Deny { reason } => panic!("expected allow, got deny: {reason}"),
        }
    }

    #[test]
    fn rejects_expired() {
        let prep = prepare(&prod_def()).unwrap();
        let token = make_token(serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "svc-api",
            "sub": "s",
            "exp": now() - 3600,
        }));
        match verify(&prep, &token) {
            AuthDecision::Deny { reason } => assert!(reason.contains("expired")),
            _ => panic!("should deny"),
        }
    }

    #[test]
    fn rejects_wrong_issuer() {
        let prep = prepare(&prod_def()).unwrap();
        let token = make_token(serde_json::json!({
            "iss": "https://evil.example.com",
            "aud": "svc-api",
            "exp": now() + 60,
        }));
        match verify(&prep, &token) {
            AuthDecision::Deny { reason } => assert!(reason.contains("bad-issuer")),
            _ => panic!("should deny"),
        }
    }

    #[test]
    fn rejects_wrong_audience() {
        let prep = prepare(&prod_def()).unwrap();
        let token = make_token(serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "wrong-audience",
            "exp": now() + 60,
        }));
        match verify(&prep, &token) {
            AuthDecision::Deny { reason } => assert!(reason.contains("bad-audience")),
            _ => panic!("should deny"),
        }
    }

    #[test]
    fn rejects_tampered_signature() {
        let prep = prepare(&prod_def()).unwrap();
        let mut token = make_token(serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "svc-api",
            "exp": now() + 60,
        }));
        // Flip the last byte of the signature — base64 decode should
        // still succeed but verification should fail.
        let last_idx = token.len() - 1;
        let b = token.as_bytes()[last_idx];
        token.replace_range(last_idx.., &String::from(if b == b'A' { 'B' } else { 'A' }));
        match verify(&prep, &token) {
            AuthDecision::Deny { .. } => {}
            _ => panic!("should deny tampered sig"),
        }
    }

    #[test]
    fn subject_allowlist_enforced() {
        let mut def = prod_def();
        def.subject_allowlist = vec!["only-me".into()];
        let prep = prepare(&def).unwrap();
        let token = make_token(serde_json::json!({
            "iss": "https://issuer.example.com",
            "aud": "svc-api",
            "sub": "some-other-service",
            "exp": now() + 60,
        }));
        match verify(&prep, &token) {
            AuthDecision::Deny { reason } => assert!(reason.contains("subject-not-allowed")),
            _ => panic!("should deny"),
        }
    }

    #[test]
    fn rejects_hs256_config() {
        let def = OidcDef {
            algorithms: vec!["HS256".into()],
            ..prod_def()
        };
        let err = prepare(&def).unwrap_err();
        assert!(err.contains("HS256"));
    }

    #[test]
    fn rejects_none_config() {
        let def = OidcDef {
            algorithms: vec!["none".into()],
            ..prod_def()
        };
        let err = prepare(&def).unwrap_err();
        assert!(err.contains("none"));
    }

    #[test]
    fn jwks_source_exactly_one() {
        let both = OidcDef {
            jwks_json: Some("{}".into()),
            jwks_file: Some("/tmp/x".into()),
            ..prod_def()
        };
        assert!(
            both.jwks_source()
                .unwrap_err()
                .contains("mutually exclusive")
        );

        let neither = OidcDef {
            jwks_json: None,
            jwks_file: None,
            ..prod_def()
        };
        assert!(
            neither
                .jwks_source()
                .unwrap_err()
                .contains("one of jwks_json / jwks_file")
        );
    }

    #[test]
    fn extract_bearer_case_insensitive() {
        let mut h = HashMap::new();
        h.insert("authorization".into(), "Bearer abc.def.ghi".into());
        let req = AuthRequest {
            headers: &h,
            body: &[],
            peer_cert_fingerprint: None,
        };
        assert_eq!(extract_bearer(&req), Some("abc.def.ghi"));

        let mut h2 = HashMap::new();
        h2.insert("Authorization".into(), "bearer xyz".into());
        let req2 = AuthRequest {
            headers: &h2,
            body: &[],
            peer_cert_fingerprint: None,
        };
        assert_eq!(extract_bearer(&req2), Some("xyz"));
    }

    #[test]
    fn extract_bearer_returns_none_without_prefix() {
        let mut h = HashMap::new();
        h.insert("authorization".into(), "Basic xyz".into());
        let req = AuthRequest {
            headers: &h,
            body: &[],
            peer_cert_fingerprint: None,
        };
        assert_eq!(extract_bearer(&req), None);
    }

    #[test]
    fn missing_authorization_header_denies() {
        let prep = prepare(&prod_def()).unwrap();
        // Pass a clearly-invalid token so verify() fails fast.
        match verify(&prep, "") {
            AuthDecision::Deny { .. } => {}
            _ => panic!("should deny empty token"),
        }
    }
}
