// SPDX-License-Identifier: Apache-2.0
//! Daemon-side consumption of a unified `auth:` provider (RFC 0031 §4/§11):
//! build the transport [`RequestSigner`](::mcp::http::RequestSigner) for an
//! endpoint from its [`AuthSpec`] plus the cached token.
//!
//! An **interactive** (device / authorization-code) token comes from the file
//! cache written by `agentd login`; it is held in memory and refreshed with the
//! cached refresh token when it nears expiry — so a long-lived daemon keeps a
//! live bearer without re-prompting. A **client-credentials** provider reuses the
//! M2M fetcher; a **static** provider resolves a `{{secret:…}}` per request.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::cache::{self, CachedCred};
use crate::auth::oauth2::{self, OAuth2Params};
use crate::config::AuthSpec;

/// Refresh a token this many ms before its advertised expiry.
const REFRESH_SKEW_MS: u64 = 60_000;

/// A refreshing access-token source, seeded from the file cache.
pub struct TokenSource {
    params: OAuth2Params,
    timeout: Duration,
    cache: Mutex<Option<CachedCred>>,
}

impl TokenSource {
    pub fn new(params: OAuth2Params, timeout: Duration, seed: Option<CachedCred>) -> TokenSource {
        TokenSource {
            params,
            timeout,
            cache: Mutex::new(seed),
        }
    }

    /// A currently-valid access token, refreshing (with the refresh token) when
    /// the cached one is within [`REFRESH_SKEW_MS`] of expiry.
    pub fn bearer(&self) -> Result<String, String> {
        let now = cache::now_ms();
        let refresh_token = {
            let g = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            match g.as_ref() {
                Some(c) if c.valid_at(now, REFRESH_SKEW_MS) => return Ok(c.access_token.clone()),
                Some(c) => c.refresh_token.clone(),
                None => return Err("no cached credential — run `agentd login`".into()),
            }
        };
        let rt = refresh_token
            .ok_or("cached token expired with no refresh token — run `agentd login`")?;
        let toks = oauth2::refresh(&self.params, &rt, self.timeout)?;
        let cred = crate::auth::login::tokens_to_cred(&toks);
        let access = cred.access_token.clone();
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(cred);
        Ok(access)
    }
}

/// A transport signer that injects a refreshing `Authorization: Bearer …`.
/// A token-source failure yields no header (the server answers `401`, surfacing
/// "not logged in" instead of hanging — fail-closed, RFC 0031 §2).
pub struct BearerSigner {
    source: Arc<TokenSource>,
}
impl ::mcp::http::RequestSigner for BearerSigner {
    fn sign(&self, _m: &str, _a: &str, _p: &str, _b: &[u8]) -> Vec<(String, String)> {
        match self.source.bearer() {
            Ok(tok) => vec![("Authorization".to_string(), format!("Bearer {tok}"))],
            Err(_) => Vec::new(),
        }
    }
}

/// A static-credential signer (`kind: static`): resolves a `{{secret:…}}` value
/// per request, so a rotated `{{secret-file:…}}` is picked up on the next call.
pub struct StaticSigner {
    header: String,
    value_template: String,
}
impl ::mcp::http::RequestSigner for StaticSigner {
    fn sign(&self, _m: &str, _a: &str, _p: &str, _b: &[u8]) -> Vec<(String, String)> {
        let env = |k: &str| std::env::var(k).ok();
        match crate::sec::secret::resolve(&self.value_template, &env) {
            Ok(v) => vec![(self.header.clone(), v)],
            Err(_) => Vec::new(),
        }
    }
}

/// Resolve the OAuth2 request params from a runtime `AuthSpec`, discovering the
/// endpoints from `issuer` when they are not pinned.
fn params_from_spec(auth: &AuthSpec, timeout: Duration) -> Result<OAuth2Params, String> {
    let client_id = auth
        .client_id
        .clone()
        .ok_or("auth.client_id is required for oauth2")?;
    let mut token_url = auth.token_url.clone();
    let mut device_url = auth.device_authorization_url.clone();
    if token_url.is_none()
        && let Some(issuer) = &auth.issuer
    {
        let d = oauth2::discover(issuer, timeout)?;
        token_url = token_url.or(d.token_endpoint);
        device_url = device_url.or(d.device_authorization_endpoint);
    }
    Ok(OAuth2Params {
        token_url: token_url.ok_or("auth.token_url is required (or set auth.issuer)")?,
        device_authorization_url: device_url,
        authorization_url: auth.authorization_url.clone(),
        client_id,
        client_secret: auth.client_secret.clone(),
        scopes: auth.scopes.clone(),
        audience: auth.audience.clone(),
    })
}

/// Build a refreshing [`TokenSource`] for an interactive (device /
/// authorization-code) oauth2 `auth:` block, seeded from the file cache under
/// `target`. Returns `Ok(None)` for a non-oauth2 or client-credentials block
/// (those aren't cache-backed refreshing sources). Used by the intelligence path
/// (RFC 0031), which consumes the bearer directly rather than via a signer.
pub fn token_source_for(
    auth: &AuthSpec,
    target: &str,
    timeout: Duration,
) -> Result<Option<Arc<TokenSource>>, String> {
    if auth.kind != "oauth2" || auth.grant.as_deref() == Some("client_credentials") {
        return Ok(None);
    }
    let params = params_from_spec(auth, timeout)?;
    let seed = cache::load_file(&cache::default_dir(), target);
    Ok(Some(Arc::new(TokenSource::new(params, timeout, seed))))
}

/// Build the request signer for an endpoint's `auth:` block. `target` is the
/// credential-cache key (e.g. `mcp:github`); `timeout` bounds token round-trips.
pub fn signer_for(
    auth: &AuthSpec,
    target: &str,
    timeout: Duration,
) -> Result<Option<Arc<dyn ::mcp::http::RequestSigner>>, String> {
    match auth.kind.as_str() {
        "static" => {
            if let Some(tok) = &auth.token {
                Ok(Some(Arc::new(StaticSigner {
                    header: "Authorization".to_string(),
                    value_template: format!("Bearer {tok}"),
                })))
            } else if let (Some(h), Some(v)) = (&auth.header, &auth.value) {
                Ok(Some(Arc::new(StaticSigner {
                    header: h.clone(),
                    value_template: v.clone(),
                })))
            } else {
                Err("auth.kind static needs `token` or `header`+`value`".into())
            }
        }
        "oauth2" => {
            let params = params_from_spec(auth, timeout)?;
            match auth.grant.as_deref() {
                Some("client_credentials") => {
                    let cs = auth
                        .client_secret
                        .clone()
                        .ok_or("client_credentials needs client_secret")?;
                    let spec = crate::config::McpOauthSpec {
                        token_url: params.token_url,
                        client_id: params.client_id,
                        client_secret: cs,
                        scope: (!params.scopes.is_empty()).then(|| params.scopes.join(" ")),
                    };
                    Ok(Some(Arc::new(crate::mcp::oauth::OAuthBearerSigner::new(
                        spec, timeout,
                    ))))
                }
                // device / authorization_code / unset → a cached, refreshing token.
                _ => {
                    let dir = cache::default_dir();
                    let seed = cache::load_file(&dir, target);
                    let source = Arc::new(TokenSource::new(params, timeout, seed));
                    Ok(Some(Arc::new(BearerSigner { source })))
                }
            }
        }
        "aws" => Ok(Some(crate::auth::aws::SigV4Signer::from_spec(
            auth, target,
        )?)),
        "spiffe" => match auth.svid.as_deref().unwrap_or("jwt") {
            // JWT-SVID: present the SPIRE-written token as a bearer, re-read per
            // request (SPIRE rotates it) via the `{{secret-file:…}}` resolver.
            "jwt" => {
                let path = auth
                    .jwt_svid_file
                    .clone()
                    .ok_or("spiffe: `jwt_svid_file` is required for svid jwt")?;
                Ok(Some(Arc::new(StaticSigner {
                    header: "Authorization".to_string(),
                    value_template: format!("Bearer {{{{secret-file:{path}}}}}"),
                })))
            }
            // X.509-SVID is a transport client identity (mTLS), set in
            // `mcp::from_spec` (needs `--features tls`), not a request signer.
            "x509" => Ok(None),
            other => Err(format!("spiffe: unknown svid '{other}' (want jwt|x509)")),
        },
        other => Err(format!("unknown auth.kind '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::mcp::http::RequestSigner;

    fn params() -> OAuth2Params {
        OAuth2Params {
            token_url: "https://as/token".into(),
            device_authorization_url: None,
            authorization_url: None,
            client_id: "c".into(),
            client_secret: None,
            scopes: vec![],
            audience: None,
        }
    }

    #[test]
    fn spiffe_jwt_svid_is_a_rotating_bearer() {
        // A `kind: spiffe, svid: jwt` block presents the SPIRE-written JWT-SVID as
        // a bearer, re-read per request via the `{{secret-file:…}}` resolver.
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "eyJ.jwt.svid").unwrap();
        let auth = AuthSpec {
            kind: "spiffe".into(),
            svid: Some("jwt".into()),
            jwt_svid_file: Some(f.path().to_str().unwrap().to_string()),
            ..Default::default()
        };
        let signer = signer_for(&auth, "mcp:x", Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let h = signer.sign("POST", "a", "/p", b"");
        assert_eq!(
            h,
            vec![(
                "Authorization".to_string(),
                "Bearer eyJ.jwt.svid".to_string()
            )]
        );
        // x509 SVID is an mTLS identity (set in mcp::from_spec), not a signer →
        // signer_for yields no request signer.
        let x509 = AuthSpec {
            kind: "spiffe".into(),
            svid: Some("x509".into()),
            svid_file: Some("/s".into()),
            key_file: Some("/k".into()),
            ..Default::default()
        };
        assert!(
            signer_for(&x509, "mcp:x", Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn token_source_serves_a_valid_cached_token_without_network() {
        let seed = CachedCred {
            access_token: "live".into(),
            refresh_token: Some("rt".into()),
            expires_at_ms: cache::now_ms() + 3_600_000,
            token_type: Some("Bearer".into()),
            extra: Default::default(),
        };
        let src = TokenSource::new(params(), Duration::from_secs(5), Some(seed));
        assert_eq!(src.bearer().unwrap(), "live");
        // And the signer wraps it as an Authorization header.
        let signer = BearerSigner {
            source: Arc::new(TokenSource::new(
                params(),
                Duration::from_secs(5),
                Some(CachedCred {
                    access_token: "live".into(),
                    expires_at_ms: cache::now_ms() + 3_600_000,
                    ..Default::default()
                }),
            )),
        };
        let h = signer.sign("POST", "a", "/p", b"");
        assert_eq!(
            h,
            vec![("Authorization".to_string(), "Bearer live".to_string())]
        );
    }

    #[test]
    fn token_source_without_a_seed_asks_for_login() {
        let src = TokenSource::new(params(), Duration::from_secs(5), None);
        assert!(src.bearer().unwrap_err().contains("agentd login"));
    }

    #[test]
    fn static_signer_resolves_a_secret_per_request() {
        // SAFETY: single-threaded test, unique var.
        unsafe { std::env::set_var("AUTH_DEV_STATIC_TOKEN", "s3cr3t") };
        let s = StaticSigner {
            header: "Authorization".into(),
            value_template: "Bearer {{secret:AUTH_DEV_STATIC_TOKEN}}".into(),
        };
        let h = s.sign("POST", "a", "/p", b"");
        assert_eq!(h[0].1, "Bearer s3cr3t");
        unsafe { std::env::remove_var("AUTH_DEV_STATIC_TOKEN") };
    }
}
