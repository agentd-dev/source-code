//! Route authentication (RFC §13.2, §21.7).
//!
//! Three auth mechanisms, each feature-complete independently:
//!
//! - **Bearer token** — `Authorization: Bearer <token>` compared
//!   constant-time against a set sourced from an env var
//!   (newline-separated) or literal (discouraged, tests-only).
//! - **HMAC-signed webhook** — HMAC-SHA256 of the raw body, hex
//!   digest compared constant-time against a configurable header
//!   (default `X-Agent-Signature`) optionally stripped of a prefix
//!   (default `sha256=`).
//! - **mTLS** — placeholder. The `"mtls"` ref value is parsed and
//!   routed to a stub that errors with a clean "wire TLS first"
//!   message. R3b adds the real rustls-backed verification.
//!
//! The auth block is parsed out of the workflow TOML:
//!
//! ```toml
//! [auth.bearer.ops]
//! tokens_env = "OPS_TOKENS"
//!
//! [auth.hmac.github]
//! secret_env = "GITHUB_WEBHOOK_SECRET"
//! header = "X-Hub-Signature-256"
//! prefix = "sha256="
//! ```
//!
//! Route refs:
//!
//! ```toml
//! [[http_routes]]
//! auth = "bearer:ops"        # or "hmac:github" / "mtls" / "none"
//! ```

pub mod bearer;
pub mod config;
pub mod hmac;
pub mod mtls;
pub mod oidc;

use std::collections::HashMap;

use crate::error::{Error, Result};

pub use config::AuthConfig;

/// Spawn-time-prepared auth state. Holds the parsed
/// [`AuthConfig`] plus per-binding state that requires up-front
/// work — today that's the OIDC bindings' JWKS parse. Built once
/// via [`AuthConfig::prepare`]; shared across request handlers
/// via `Arc`.
#[derive(Debug)]
pub struct PreparedAuth {
    pub config: AuthConfig,
    /// OIDC binding name → prepared JWKS + validation settings.
    pub oidc: HashMap<String, oidc::PreparedOidc>,
}

impl PreparedAuth {
    /// Prepare from a config, surfacing JWKS / algorithm errors at
    /// spawn time rather than the first request that needs them.
    pub fn from_config(cfg: &AuthConfig) -> Result<Self> {
        let mut oidc_map = HashMap::with_capacity(cfg.oidc.len());
        for (name, def) in &cfg.oidc {
            let prepared =
                oidc::prepare(def).map_err(|e| Error::Config(format!("auth.oidc.{name}: {e}")))?;
            oidc_map.insert(name.clone(), prepared);
        }
        Ok(Self {
            config: cfg.clone(),
            oidc: oidc_map,
        })
    }
}

/// The parsed reference on a route's `auth` field. Independently
/// of whether the named binding is configured — that gets checked at
/// startup by [`AuthConfig::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRef {
    None,
    Bearer {
        name: String,
    },
    Hmac {
        name: String,
    },
    MTls,
    /// OIDC / JWT bearer — validated against `[auth.oidc.<name>]`.
    Oidc {
        name: String,
    },
}

impl AuthRef {
    /// Parse a route's `auth` string into a typed ref.
    ///
    /// Shapes accepted:
    ///
    /// - `"none"` / `""` / absent → [`AuthRef::None`]
    /// - `"bearer"` → `Bearer { name: "default" }`
    /// - `"bearer:NAME"` → `Bearer { name: "NAME" }`
    /// - `"hmac"` → `Hmac { name: "default" }`
    /// - `"hmac:NAME"` → `Hmac { name: "NAME" }`
    /// - `"mtls"` → `MTls`
    ///
    /// Anything else errors.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(raw) = raw else {
            return Ok(AuthRef::None);
        };
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
            return Ok(AuthRef::None);
        }
        let (kind, name) = match raw.split_once(':') {
            Some((k, n)) => (k.trim(), n.trim().to_string()),
            None => (raw, "default".to_string()),
        };
        match kind {
            "bearer" => Ok(AuthRef::Bearer { name }),
            "hmac" => Ok(AuthRef::Hmac { name }),
            "oidc" => Ok(AuthRef::Oidc { name }),
            "mtls" => {
                if !name.is_empty() && name != "default" {
                    return Err(Error::Config(format!(
                        "auth ref `{raw}`: mtls takes no name"
                    )));
                }
                Ok(AuthRef::MTls)
            }
            other => Err(Error::Config(format!(
                "unknown auth kind `{other}` (expected bearer / hmac / oidc / mtls / none)"
            ))),
        }
    }
}

/// Decision for a single request.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthDecision {
    /// Request authenticated; the [`Principal`] captures who.
    Allow { principal: Principal },
    /// Request rejected; the runtime maps this to HTTP 401.
    Deny { reason: String },
}

/// Identity extracted from a successful auth check. Exposed to the
/// workflow context as `trigger.principal` so nodes can react to
/// who called them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub kind: &'static str, // "bearer" / "hmac" / "mtls" / "oidc"
    pub name: String,       // the binding name, or subject (oidc)
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            kind: "anonymous",
            name: String::new(),
        }
    }
}

/// What the HTTP handler passes to the auth layer per request.
pub struct AuthRequest<'a> {
    pub headers: &'a HashMap<String, String>,
    pub body: &'a [u8],
    /// SHA-256 hex digest of the peer's client cert DER (if any).
    /// Only populated when the server is configured for mTLS
    /// (`server-tls` feature + `[server.tls.client_auth]`).
    pub peer_cert_fingerprint: Option<&'a str>,
}

/// Evaluate a route's auth requirement against an incoming request.
pub fn evaluate(
    auth_ref: &AuthRef,
    prepared: &PreparedAuth,
    req: &AuthRequest<'_>,
) -> AuthDecision {
    match auth_ref {
        AuthRef::None => AuthDecision::Allow {
            principal: Principal::anonymous(),
        },
        AuthRef::Bearer { name } => bearer::verify(&prepared.config, name, req),
        AuthRef::Hmac { name } => hmac::verify(&prepared.config, name, req),
        AuthRef::MTls => mtls::verify(req),
        AuthRef::Oidc { name } => match prepared.oidc.get(name) {
            Some(prep) => match oidc::extract_bearer(req) {
                Some(token) => oidc::verify(prep, token),
                None => AuthDecision::Deny {
                    reason: "oidc.missing-bearer".into(),
                },
            },
            None => AuthDecision::Deny {
                reason: format!("oidc.unknown-binding:{name}"),
            },
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_none_variants() {
        assert_eq!(AuthRef::parse(None).unwrap(), AuthRef::None);
        assert_eq!(AuthRef::parse(Some("")).unwrap(), AuthRef::None);
        assert_eq!(AuthRef::parse(Some("none")).unwrap(), AuthRef::None);
        assert_eq!(AuthRef::parse(Some("NONE")).unwrap(), AuthRef::None);
    }

    #[test]
    fn parse_bearer_shapes() {
        assert_eq!(
            AuthRef::parse(Some("bearer")).unwrap(),
            AuthRef::Bearer {
                name: "default".into()
            }
        );
        assert_eq!(
            AuthRef::parse(Some("bearer:ops")).unwrap(),
            AuthRef::Bearer { name: "ops".into() }
        );
    }

    #[test]
    fn parse_hmac_shapes() {
        assert_eq!(
            AuthRef::parse(Some("hmac")).unwrap(),
            AuthRef::Hmac {
                name: "default".into()
            }
        );
        assert_eq!(
            AuthRef::parse(Some("hmac:github")).unwrap(),
            AuthRef::Hmac {
                name: "github".into()
            }
        );
    }

    #[test]
    fn parse_mtls_rejects_name() {
        assert_eq!(AuthRef::parse(Some("mtls")).unwrap(), AuthRef::MTls);
        assert!(AuthRef::parse(Some("mtls:anything")).is_err());
    }

    #[test]
    fn parse_unknown_kind_errors() {
        assert!(AuthRef::parse(Some("oauth:foo")).is_err());
        assert!(AuthRef::parse(Some("garbage")).is_err());
    }

    #[test]
    fn mtls_denies_when_no_client_cert() {
        let cfg = AuthConfig::default();
        let prepared = PreparedAuth::from_config(&cfg).unwrap();
        let headers = HashMap::new();
        let req = AuthRequest {
            headers: &headers,
            body: b"",
            peer_cert_fingerprint: None,
        };
        match evaluate(&AuthRef::MTls, &prepared, &req) {
            AuthDecision::Deny { reason } => {
                assert!(reason.contains("mtls"), "reason: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn mtls_allows_when_client_cert_fingerprint_present() {
        let cfg = AuthConfig::default();
        let prepared = PreparedAuth::from_config(&cfg).unwrap();
        let headers = HashMap::new();
        let req = AuthRequest {
            headers: &headers,
            body: b"",
            peer_cert_fingerprint: Some("sha256:deadbeef"),
        };
        match evaluate(&AuthRef::MTls, &prepared, &req) {
            AuthDecision::Allow { principal } => {
                assert_eq!(principal.kind, "mtls");
                assert_eq!(principal.name, "sha256:deadbeef");
            }
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn none_always_allows() {
        let cfg = AuthConfig::default();
        let prepared = PreparedAuth::from_config(&cfg).unwrap();
        let headers = HashMap::new();
        let req = AuthRequest {
            headers: &headers,
            body: b"",
            peer_cert_fingerprint: None,
        };
        let decision = evaluate(&AuthRef::None, &prepared, &req);
        assert!(matches!(decision, AuthDecision::Allow { .. }));
    }
}
