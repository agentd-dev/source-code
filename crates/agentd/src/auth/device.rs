// SPDX-License-Identifier: AGPL-3.0-only
//! Daemon-side consumption of an `auth:` provider: build the transport
//! [`RequestSigner`](::mcp::http::RequestSigner) for an endpoint from its
//! [`AuthSpec`] plus the cached token.
//!
//! An **interactive** (device / authorization-code) token comes from the file
//! cache written by `agentd --login`; it is held in memory and refreshed with
//! the cached refresh token when it nears expiry — so a long-lived daemon keeps
//! a live bearer without re-prompting a human. A **client-credentials**
//! provider reuses the machine-to-machine fetcher; a **static** provider
//! resolves a `{{secret:…}}` per request.
//!
//! Building a signer reads the block and the login cache and dials nothing:
//! an `issuer` is asked for its endpoints by the first request that needs
//! them. A credential the signer cannot produce — none cached, expired with no
//! refresh token, an issuer or a token endpoint that does not answer, a secret
//! that does not resolve — fails that request unsent, and the next request
//! tries again: a login made while the daemon runs is picked up then.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::cache::{self, CachedCred};
use crate::auth::oauth2::{self, OAuth2Params};
use crate::auth::report;
use crate::config::AuthSpec;

/// Refresh a token this many ms before its advertised expiry.
const REFRESH_SKEW_MS: u64 = 60_000;

/// The OAuth2 request params of one `auth:` block: pinned by the block, or
/// discovered from its `issuer` by the first request that needs them — and by
/// each one after, until discovery succeeds — so an issuer that is down when
/// the connection is built leaves its requests failing, not unauthenticated,
/// until it answers.
pub struct Endpoints {
    auth: AuthSpec,
    timeout: Duration,
    resolved: Mutex<Option<OAuth2Params>>,
}

impl Endpoints {
    /// The block's endpoints, without dialing: `client_id` and a `token_url`
    /// or an `issuer` are what it must name.
    pub fn new(auth: &AuthSpec, timeout: Duration) -> Result<Endpoints, String> {
        if auth.client_id.is_none() {
            return Err("auth.client_id is required for oauth2".into());
        }
        if auth.token_url.is_none() && auth.issuer.is_none() {
            return Err("auth.token_url is required (or set auth.issuer)".into());
        }
        Ok(Endpoints {
            auth: auth.clone(),
            timeout,
            resolved: Mutex::new(None),
        })
    }

    /// Endpoints already known: nothing is discovered.
    pub fn pinned(params: OAuth2Params) -> Endpoints {
        Endpoints {
            auth: AuthSpec::default(),
            timeout: Duration::ZERO,
            resolved: Mutex::new(Some(params)),
        }
    }

    /// The params, discovering them from the issuer when the block does not
    /// pin a `token_url` and no discovery has succeeded yet.
    pub fn params(&self) -> Result<OAuth2Params, String> {
        if let Some(p) = self
            .resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Ok(p.clone());
        }
        let p = params_from_spec(&self.auth, self.timeout)?;
        *self.resolved.lock().unwrap_or_else(|e| e.into_inner()) = Some(p.clone());
        Ok(p)
    }
}

/// A refreshing access-token source for one login target, seeded from the
/// file cache.
pub struct TokenSource {
    endpoints: Endpoints,
    target: String,
    dir: PathBuf,
    timeout: Duration,
    cache: Mutex<Option<CachedCred>>,
}

impl TokenSource {
    /// The credential `target` names in the login cache at `dir`, refreshed
    /// through `endpoints`.
    pub fn new(endpoints: Endpoints, target: &str, dir: PathBuf, timeout: Duration) -> TokenSource {
        let seed = cache::load_file(&dir, target);
        TokenSource {
            endpoints,
            target: target.to_string(),
            dir,
            timeout,
            cache: Mutex::new(seed),
        }
    }

    /// A currently-valid access token. One within [`REFRESH_SKEW_MS`] of
    /// expiry is refreshed with the refresh token. When memory holds none
    /// that is usable the login cache is read again first, so a login made
    /// since the last call is picked up without a reload.
    pub fn bearer(&self) -> Result<String, String> {
        let now = cache::now_ms();
        let held = self.cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(c) = &held
            && c.valid_at(now, REFRESH_SKEW_MS)
        {
            return Ok(c.access_token.clone());
        }
        let file = cache::load_file(&self.dir, &self.target);
        if let Some(c) = file.as_ref().filter(|c| c.valid_at(now, REFRESH_SKEW_MS)) {
            let access = c.access_token.clone();
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = file;
            return Ok(access);
        }
        let login = format!("run `agentd --login {}`", self.target);
        if held.is_none() && file.is_none() {
            return Err(format!("no cached credential — {login}"));
        }
        // Memory's refresh token first: a refresh made here may have rotated
        // it past the one the login cache holds.
        let rt = held
            .and_then(|c| c.refresh_token)
            .or_else(|| file.and_then(|c| c.refresh_token))
            .ok_or_else(|| format!("cached token expired with no refresh token — {login}"))?;
        let params = self.endpoints.params()?;
        let toks = oauth2::refresh(&params, &rt, self.timeout)?;
        let cred = crate::auth::login::tokens_to_cred(&toks);
        let access = cred.access_token.clone();
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(cred);
        Ok(access)
    }
}

/// A transport signer that injects a refreshing `Authorization: Bearer …`.
/// A token it cannot produce fails the request unsent, naming the target and
/// the login that would fix it, rather than letting it go out without one.
pub struct BearerSigner {
    source: Arc<TokenSource>,
}
impl ::mcp::http::RequestSigner for BearerSigner {
    fn sign(
        &self,
        _m: &str,
        _a: &str,
        _p: &str,
        _b: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        let tok = report::outcome(&self.source.target, self.source.bearer())?;
        Ok(vec![("Authorization".to_string(), format!("Bearer {tok}"))])
    }
}

/// The client-credentials grant behind an `auth:` block: the machine-to-machine
/// fetcher, made once the token endpoint is known — discovered from `issuer`
/// by the first request when the block does not pin it.
pub struct ClientCredentialsSigner {
    endpoints: Endpoints,
    target: String,
    client_secret: String,
    timeout: Duration,
    client: Mutex<Option<Arc<crate::mcp::oauth::OAuthClient>>>,
}

impl ClientCredentialsSigner {
    fn bearer(&self) -> Result<String, String> {
        let made = self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let client = match made {
            Some(c) => c,
            None => {
                let p = self.endpoints.params()?;
                let c = Arc::new(crate::mcp::oauth::OAuthClient::new(
                    crate::mcp::oauth::OAuthConfig {
                        token_url: p.token_url,
                        client_id: p.client_id,
                        client_secret: self.client_secret.clone(),
                        scope: (!p.scopes.is_empty()).then(|| p.scopes.join(" ")),
                    },
                    self.timeout,
                ));
                *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&c));
                c
            }
        };
        client.bearer()
    }
}

impl ::mcp::http::RequestSigner for ClientCredentialsSigner {
    fn sign(
        &self,
        _m: &str,
        _a: &str,
        _p: &str,
        _b: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        let tok = report::outcome(&self.target, self.bearer())?;
        Ok(vec![("Authorization".to_string(), format!("Bearer {tok}"))])
    }
}

/// A static-credential signer (`kind: static`): resolves a `{{secret:…}}` value
/// per request rather than once at build time, so a rotated `{{secret-file:…}}`
/// is picked up on the next call without restarting the daemon. A reference
/// that does not resolve fails the request unsent, naming the reference.
pub struct StaticSigner {
    target: String,
    header: String,
    value_template: String,
}
impl ::mcp::http::RequestSigner for StaticSigner {
    fn sign(
        &self,
        _m: &str,
        _a: &str,
        _p: &str,
        _b: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        let env = |k: &str| std::env::var(k).ok();
        let v = report::outcome(
            &self.target,
            crate::sec::secret::resolve(&self.value_template, &env),
        )?;
        Ok(vec![(self.header.clone(), v)])
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
/// `target`. Returns `Ok(None)` for a non-oauth2 or client-credentials block,
/// since neither is a cache-backed refreshing source.
pub fn token_source_for(
    auth: &AuthSpec,
    target: &str,
    timeout: Duration,
) -> Result<Option<Arc<TokenSource>>, String> {
    if auth.kind != "oauth2" || auth.grant.as_deref() == Some("client_credentials") {
        return Ok(None);
    }
    let endpoints = Endpoints::new(auth, timeout)?;
    Ok(Some(Arc::new(TokenSource::new(
        endpoints,
        target,
        cache::default_dir(),
        timeout,
    ))))
}

/// Build the request signer for an endpoint's `auth:` block. `target` is the
/// credential-cache key (e.g. `mcp:github`), and what a failure is reported
/// under; `timeout` bounds token round-trips. An `Err` is the block itself —
/// a field its kind needs is missing — and nothing is dialed to build it.
pub fn signer_for(
    auth: &AuthSpec,
    target: &str,
    timeout: Duration,
) -> Result<Option<Arc<dyn ::mcp::http::RequestSigner>>, String> {
    let static_signer = |header: &str, value_template: String| {
        Arc::new(StaticSigner {
            target: target.to_string(),
            header: header.to_string(),
            value_template,
        }) as Arc<dyn ::mcp::http::RequestSigner>
    };
    match auth.kind.as_str() {
        "static" => {
            if let Some(tok) = &auth.token {
                Ok(Some(static_signer(
                    "Authorization",
                    format!("Bearer {tok}"),
                )))
            } else if let (Some(h), Some(v)) = (&auth.header, &auth.value) {
                Ok(Some(static_signer(h, v.clone())))
            } else {
                Err("auth.kind static needs `token` or `header`+`value`".into())
            }
        }
        "oauth2" => {
            let endpoints = Endpoints::new(auth, timeout)?;
            match auth.grant.as_deref() {
                Some("client_credentials") => {
                    let client_secret = auth
                        .client_secret
                        .clone()
                        .ok_or("client_credentials needs client_secret")?;
                    Ok(Some(Arc::new(ClientCredentialsSigner {
                        endpoints,
                        target: target.to_string(),
                        client_secret,
                        timeout,
                        client: Mutex::new(None),
                    })))
                }
                // device / authorization_code / unset → a cached, refreshing token.
                _ => {
                    let source = Arc::new(TokenSource::new(
                        endpoints,
                        target,
                        cache::default_dir(),
                        timeout,
                    ));
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
                Ok(Some(static_signer(
                    "Authorization",
                    format!("Bearer {{{{secret-file:{path}}}}}"),
                )))
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
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};

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

    fn live(token: &str) -> CachedCred {
        CachedCred {
            access_token: token.into(),
            expires_at_ms: cache::now_ms() + 3_600_000,
            ..Default::default()
        }
    }

    /// A token source for `target` over the login cache at `dir`.
    fn source(dir: &std::path::Path, target: &str) -> TokenSource {
        TokenSource::new(
            Endpoints::pinned(params()),
            target,
            dir.to_path_buf(),
            Duration::from_secs(5),
        )
    }

    /// An authorization server on loopback: discovery answers `503` while
    /// `down` is set, and the token endpoint issues `tok-cc`.
    fn spawn_issuer(down: Arc<AtomicBool>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let token_url = format!("{base}/token");
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                let mut r = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).ok();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.trim().eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).ok();
                let (status, payload) = if path.starts_with("/.well-known/") {
                    if down.load(Ordering::SeqCst) {
                        ("503 Service Unavailable", String::new())
                    } else {
                        ("200 OK", format!(r#"{{"token_endpoint":"{token_url}"}}"#))
                    }
                } else {
                    (
                        "200 OK",
                        r#"{"access_token":"tok-cc","expires_in":3600}"#.to_string(),
                    )
                };
                let mut w = conn;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        base
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
        let h = signer.sign("POST", "a", "/p", b"").unwrap();
        assert_eq!(
            h,
            vec![(
                "Authorization".to_string(),
                "Bearer eyJ.jwt.svid".to_string()
            )]
        );
        // An SVID SPIRE has not written fails the request rather than sending
        // it with no bearer.
        let path = f.path().to_path_buf();
        drop(f);
        let e = signer.sign("POST", "a", "/p", b"").unwrap_err();
        assert!(
            e.starts_with("mcp:x: ") && e.contains(path.to_str().unwrap()),
            "{e}"
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
        let dir = tempfile::tempdir().unwrap();
        cache::store_file(dir.path(), "mcp:x", &live("live")).unwrap();
        assert_eq!(source(dir.path(), "mcp:x").bearer().unwrap(), "live");
        // And the signer wraps it as an Authorization header.
        let signer = BearerSigner {
            source: Arc::new(source(dir.path(), "mcp:x")),
        };
        let h = signer.sign("POST", "a", "/p", b"").unwrap();
        assert_eq!(
            h,
            vec![("Authorization".to_string(), "Bearer live".to_string())]
        );
    }

    /// No login: the request fails, naming the target and the flag that
    /// logs into it — and a login made afterwards is the next request's
    /// credential, with no rebuild.
    #[test]
    fn a_missing_login_fails_the_request_until_one_is_made() {
        let dir = tempfile::tempdir().unwrap();
        let signer = BearerSigner {
            source: Arc::new(source(dir.path(), "intelligence:dev-a")),
        };
        let e = signer.sign("POST", "a", "/p", b"").unwrap_err();
        assert_eq!(
            e,
            "intelligence:dev-a: no cached credential — run `agentd --login intelligence:dev-a`"
        );
        cache::store_file(dir.path(), "intelligence:dev-a", &live("fresh")).unwrap();
        assert_eq!(
            signer.sign("POST", "a", "/p", b"").unwrap(),
            vec![("Authorization".to_string(), "Bearer fresh".to_string())]
        );
    }

    #[test]
    fn an_expired_token_with_no_refresh_token_asks_for_login() {
        let dir = tempfile::tempdir().unwrap();
        let expired = CachedCred {
            access_token: "old".into(),
            expires_at_ms: 1,
            ..Default::default()
        };
        cache::store_file(dir.path(), "mcp:x", &expired).unwrap();
        let e = source(dir.path(), "mcp:x").bearer().unwrap_err();
        assert!(
            e.contains("expired with no refresh token") && e.contains("run `agentd --login mcp:x`"),
            "{e}"
        );
    }

    /// Building the signer dials nothing; a request made while the issuer
    /// does not answer fails, unsent, and the next one asks it again.
    #[test]
    fn an_issuer_that_does_not_answer_fails_the_request_and_is_asked_again() {
        let down = Arc::new(AtomicBool::new(true));
        let issuer = spawn_issuer(Arc::clone(&down));
        // SAFETY: a variable no other test reads.
        unsafe { std::env::set_var("AUTH_DEV_CC_SECRET", "cc") };
        let auth = AuthSpec {
            kind: "oauth2".into(),
            grant: Some("client_credentials".into()),
            issuer: Some(issuer),
            client_id: Some("c".into()),
            client_secret: Some("{{secret:AUTH_DEV_CC_SECRET}}".into()),
            ..Default::default()
        };
        let signer = signer_for(&auth, "mcp:cc", Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let e = signer.sign("POST", "a", "/p", b"").unwrap_err();
        assert!(e.starts_with("mcp:cc: ") && e.contains("discovery"), "{e}");
        down.store(false, Ordering::SeqCst);
        assert_eq!(
            signer.sign("POST", "a", "/p", b"").unwrap(),
            vec![("Authorization".to_string(), "Bearer tok-cc".to_string())]
        );
        unsafe { std::env::remove_var("AUTH_DEV_CC_SECRET") };
    }

    /// The block's own gaps are refused when the signer is built.
    #[test]
    fn an_oauth2_block_without_its_endpoints_is_refused_at_build() {
        let auth = AuthSpec {
            kind: "oauth2".into(),
            client_id: Some("c".into()),
            ..Default::default()
        };
        let e = signer_for(&auth, "mcp:x", Duration::from_secs(1))
            .err()
            .expect("no token_url and no issuer");
        assert!(e.contains("token_url"), "{e}");
    }

    #[test]
    fn static_signer_resolves_a_secret_per_request() {
        // SAFETY: single-threaded test, unique var.
        unsafe { std::env::set_var("AUTH_DEV_STATIC_TOKEN", "s3cr3t") };
        let s = StaticSigner {
            target: "mcp:x".into(),
            header: "Authorization".into(),
            value_template: "Bearer {{secret:AUTH_DEV_STATIC_TOKEN}}".into(),
        };
        let h = s.sign("POST", "a", "/p", b"").unwrap();
        assert_eq!(h[0].1, "Bearer s3cr3t");
        unsafe { std::env::remove_var("AUTH_DEV_STATIC_TOKEN") };
        // Gone, the reference fails the request, named, and nothing of a
        // value rides the error.
        let e = s.sign("POST", "a", "/p", b"").unwrap_err();
        assert_eq!(
            e,
            "mcp:x: {{secret:AUTH_DEV_STATIC_TOKEN}} is not set in the environment"
        );
    }
}
