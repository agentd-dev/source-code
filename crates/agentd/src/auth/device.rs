// SPDX-License-Identifier: AGPL-3.0-only
//! Daemon-side consumption of an `auth:` provider: build the transport
//! [`RequestSigner`](::mcp::http::RequestSigner) for an endpoint from its
//! [`AuthSpec`] plus the cached token.
//!
//! An **interactive** (device / authorization-code) token comes from the file
//! cache written by `agentd --login`; it is held in memory and refreshed with
//! the cached refresh token when it nears expiry — so a long-lived daemon keeps
//! a live bearer without re-prompting a human. A refresh is written back to
//! the cache before it is used, the rotated refresh token with it, so a
//! restart renews from the token the server last issued; it is made under
//! the cache's lock for the target, so the processes and connections that
//! share a login renew it once; and a target the cache no longer holds —
//! logged out — is not renewed. A **client-credentials**
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
use std::time::{Duration, Instant};

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
    /// A refresh memory holds that the login cache does not yet: its write
    /// failed. Every request retries the write before anything else, and
    /// fails while it does, so a rotated refresh token is never used before
    /// it is kept.
    unsaved: Mutex<bool>,
    /// The instant before which the token endpoint asked for no refresh (a
    /// `429` or `503` with `Retry-After`).
    not_before: Mutex<Option<Instant>>,
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
            unsaved: Mutex::new(false),
            not_before: Mutex::new(None),
        }
    }

    /// A currently-valid access token. One within [`REFRESH_SKEW_MS`] of
    /// expiry is refreshed with the refresh token the login cache holds,
    /// under the cache's lock for the target. When memory holds none that is
    /// usable the login cache is read again first, so a login made since the
    /// last call is picked up without a reload. A target the cache holds no
    /// record for — never logged in, or logged out since — fails the
    /// request: a logout reaches a running process at its next renewal.
    pub fn bearer(&self) -> Result<String, String> {
        self.keep_unsaved()?;
        if let Some(access) = self.usable(cache::now_ms()) {
            return Ok(access);
        }
        let login = format!("run `agentd --login {}`", self.target);
        if cache::load_file(&self.dir, &self.target).is_none() {
            return Err(format!("no cached credential — {login}"));
        }
        let _held = cache::lock_file(&self.dir, &self.target, self.timeout)?;
        // Another request, here or in another process, may have renewed it
        // while this one waited.
        if let Some(access) = self.usable(cache::now_ms()) {
            return Ok(access);
        }
        // The login cache's refresh token, never memory's: every refresh
        // writes its rotation back there under this lock before using it, so
        // the cache holds the newest, and one the cache no longer holds was
        // logged out of.
        let Some(file) = cache::load_file(&self.dir, &self.target) else {
            return Err(format!("no cached credential — {login}"));
        };
        let rt = file
            .refresh_token
            .ok_or_else(|| format!("cached token expired with no refresh token — {login}"))?;
        if let Some(at) = *self.not_before.lock().unwrap_or_else(|e| e.into_inner()) {
            let left = at.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                return Err(format!(
                    "the token endpoint asked for no request for {}s more; the token is renewed after that",
                    left.as_secs().max(1)
                ));
            }
        }
        let params = self.endpoints.params()?;
        let toks = match oauth2::refresh(&params, &rt, self.timeout) {
            Ok(t) => t,
            Err(e) => {
                if let Some(wait) = e.retry_after {
                    *self.not_before.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now() + wait.min(crate::net::http::RETRY_AFTER_MAX));
                }
                return Err(e.message);
            }
        };
        let cred = crate::auth::login::refreshed_cred(&toks, &rt);
        let access = cred.access_token.clone();
        // Kept before it is used: a rotated refresh token memory alone held
        // would die with the process, and the one the cache still names is
        // spent. A write that fails leaves the refresh in memory, unused,
        // until a later request keeps it.
        let kept = cache::store_file(&self.dir, &self.target, &cred);
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(cred);
        if let Err(e) = kept {
            *self.unsaved.lock().unwrap_or_else(|e| e.into_inner()) = true;
            return Err(format!(
                "the renewed token could not be kept in the login cache: {e}"
            ));
        }
        Ok(access)
    }

    /// The access token memory or the login cache holds, if it is good at
    /// `now` with [`REFRESH_SKEW_MS`] of headroom; a good one in the cache
    /// is adopted.
    fn usable(&self, now: u64) -> Option<String> {
        let mut held = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = held.as_ref().filter(|c| c.valid_at(now, REFRESH_SKEW_MS)) {
            return Some(c.access_token.clone());
        }
        let file = cache::load_file(&self.dir, &self.target)?;
        if !file.valid_at(now, REFRESH_SKEW_MS) {
            return None;
        }
        let access = file.access_token.clone();
        *held = Some(file);
        Some(access)
    }

    /// Write a refresh memory holds unkept to the login cache, or fail. One
    /// whose target the cache no longer holds is dropped, not written: a
    /// logout made since is not undone.
    fn keep_unsaved(&self) -> Result<(), String> {
        if !*self.unsaved.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(());
        }
        let _held = cache::lock_file(&self.dir, &self.target, self.timeout)?;
        let mut unsaved = self.unsaved.lock().unwrap_or_else(|e| e.into_inner());
        if !*unsaved {
            return Ok(());
        }
        let held = self.cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if cache::load_file(&self.dir, &self.target).is_none() {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
        } else if let Some(cred) = held {
            cache::store_file(&self.dir, &self.target, &cred).map_err(|e| {
                format!("the renewed token could not be kept in the login cache: {e}")
            })?;
        }
        *unsaved = false;
        Ok(())
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
    let mut revocation_url = None;
    if token_url.is_none()
        && let Some(issuer) = &auth.issuer
    {
        let d = oauth2::discover(issuer, timeout)?;
        token_url = token_url.or(d.token_endpoint);
        device_url = device_url.or(d.device_authorization_endpoint);
        revocation_url = d.revocation_endpoint;
    }
    Ok(OAuth2Params {
        token_url: token_url.ok_or("auth.token_url is required (or set auth.issuer)")?,
        device_authorization_url: device_url,
        authorization_url: auth.authorization_url.clone(),
        revocation_url,
        issuer: auth.issuer.clone(),
        iss_supported: false,
        client_id,
        client_secret: auth.client_secret.clone(),
        client_auth: oauth2::ClientAuth::of(auth.client_auth.as_ref())?,
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
            client_id: "c".into(),
            ..Default::default()
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
        let issuer = base.clone();
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
                        (
                            "200 OK",
                            format!(r#"{{"issuer":"{issuer}","token_endpoint":"{token_url}"}}"#),
                        )
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

    /// A token endpoint that answers every request `429` with
    /// `Retry-After: 30`, counting them.
    fn spawn_throttling() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", l.local_addr().unwrap());
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&n);
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                counted.fetch_add(1, Ordering::SeqCst);
                let mut r = BufReader::new(conn.try_clone().unwrap());
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let mut w = conn;
                let _ = write!(
                    w,
                    "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        (url, n)
    }

    /// A refresh the token endpoint answers `429` with `Retry-After` says the
    /// wait, and the requests that follow fail at once, naming it, without
    /// asking the endpoint again before it is over.
    #[test]
    fn a_throttled_refresh_is_not_tried_again_before_its_retry_after() {
        let (token_url, asked) = spawn_throttling();
        let dir = tempfile::tempdir().unwrap();
        let expired = CachedCred {
            access_token: "old".into(),
            refresh_token: Some("rt".into()),
            expires_at_ms: 1,
            ..Default::default()
        };
        cache::store_file(dir.path(), "mcp:x", &expired).unwrap();
        let src = TokenSource::new(
            Endpoints::pinned(OAuth2Params {
                token_url,
                client_id: "c".into(),
                ..Default::default()
            }),
            "mcp:x",
            dir.path().to_path_buf(),
            Duration::from_secs(5),
        );
        let e = src.bearer().unwrap_err();
        assert!(
            e.contains("HTTP 429") && e.contains("retry after 30s"),
            "{e}"
        );
        let e = src.bearer().unwrap_err();
        assert!(e.contains("asked for no request for"), "{e}");
        assert_eq!(asked.load(Ordering::SeqCst), 1, "not asked again");
    }

    /// A token endpoint that rotates refresh tokens — `rt-N` is answered
    /// with `at-N+1` and `rt-N+1`, good for an hour — and refuses a spent
    /// one with `invalid_grant`, after `delay`, serving requests at once. It
    /// records each refresh token presented.
    fn spawn_rotating(delay: Duration) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/token", l.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            let current = Arc::new(Mutex::new(1u32));
            for conn in l.incoming().flatten() {
                let (log, current) = (Arc::clone(&log), Arc::clone(&current));
                std::thread::spawn(move || {
                    let mut r = BufReader::new(conn.try_clone().unwrap());
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
                    let form = String::from_utf8_lossy(&body).into_owned();
                    let rt = form
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("refresh_token="))
                        .unwrap_or_default()
                        .to_string();
                    log.lock().unwrap().push(rt.clone());
                    std::thread::sleep(delay);
                    let mut n = current.lock().unwrap();
                    let (status, payload) = if rt == format!("rt-{n}") {
                        *n += 1;
                        (
                            "200 OK",
                            format!(
                                r#"{{"access_token":"at-{n}","refresh_token":"rt-{n}","expires_in":3600}}"#
                            ),
                        )
                    } else {
                        (
                            "400 Bad Request",
                            r#"{"error":"invalid_grant"}"#.to_string(),
                        )
                    };
                    let mut w = conn;
                    let _ = write!(
                        w,
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                });
            }
        });
        (url, seen)
    }

    /// A source for `mcp:x` over the login cache at `dir`, renewed at
    /// `token_url`; the cache is seeded with a due token and `rt-1`.
    fn due_source(dir: &std::path::Path, token_url: &str) -> TokenSource {
        let due = CachedCred {
            access_token: "at-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: 1,
            ..Default::default()
        };
        if cache::load_file(dir, "mcp:x").is_none() {
            cache::store_file(dir, "mcp:x", &due).unwrap();
        }
        TokenSource::new(
            Endpoints::pinned(OAuth2Params {
                token_url: token_url.to_string(),
                client_id: "c".into(),
                ..Default::default()
            }),
            "mcp:x",
            dir.to_path_buf(),
            Duration::from_secs(5),
        )
    }

    /// Two sources over one login — two processes of a tree, or two
    /// connections in one — that find the token due at once renew it once:
    /// the second waits on the cache's lock and takes the token the first
    /// wrote back. Each presenting `rt-1` would have had a server that
    /// rotates refuse the second, or, detecting the reuse, end the grant.
    #[test]
    fn two_sources_that_find_the_token_due_renew_it_once() {
        let (token_url, presented) = spawn_rotating(Duration::from_millis(300));
        let dir = tempfile::tempdir().unwrap();
        let sources = [
            due_source(dir.path(), &token_url),
            due_source(dir.path(), &token_url),
        ];
        let got: Vec<Result<String, String>> = std::thread::scope(|s| {
            let asks: Vec<_> = sources.iter().map(|src| s.spawn(|| src.bearer())).collect();
            asks.into_iter().map(|a| a.join().unwrap()).collect()
        });
        assert_eq!(got, [Ok("at-2".to_string()), Ok("at-2".to_string())]);
        assert_eq!(*presented.lock().unwrap(), ["rt-1"]);
        assert_eq!(
            cache::load_file(dir.path(), "mcp:x")
                .unwrap()
                .refresh_token
                .as_deref(),
            Some("rt-2")
        );
    }

    /// A logout reaches a running process at its next renewal: with its
    /// login gone from the cache, a source whose token is due fails the
    /// request, presents nothing and writes no record back, though memory
    /// still holds the refresh token it was seeded with — whether the
    /// logout came before the request or while it waited on the lock.
    #[test]
    fn a_login_removed_from_the_cache_is_not_renewed() {
        let (token_url, presented) = spawn_rotating(Duration::ZERO);
        let logged_out = "no cached credential — run `agentd --login mcp:x`";

        let dir = tempfile::tempdir().unwrap();
        let src = due_source(dir.path(), &token_url);
        assert!(
            cache::take_file(dir.path(), "mcp:x", Duration::from_secs(5))
                .unwrap()
                .is_some()
        );
        assert_eq!(src.bearer().unwrap_err(), logged_out);
        assert!(cache::load_file(dir.path(), "mcp:x").is_none());

        let dir = tempfile::tempdir().unwrap();
        let src = due_source(dir.path(), &token_url);
        let held = cache::lock_file(dir.path(), "mcp:x", Duration::from_secs(5)).unwrap();
        let e = std::thread::scope(|s| {
            let ask = s.spawn(|| src.bearer());
            std::thread::sleep(Duration::from_millis(100));
            cache::evict_file(dir.path(), "mcp:x").unwrap();
            drop(held);
            ask.join().unwrap()
        });
        assert_eq!(e.unwrap_err(), logged_out);
        assert!(cache::load_file(dir.path(), "mcp:x").is_none());
        assert!(presented.lock().unwrap().is_empty(), "nothing presented");
    }

    /// A renewal whose write-back fails is not used, and the next request
    /// keeps it before serving it — unless the login was removed from the
    /// cache meanwhile: then it is dropped, and the logout stands.
    #[test]
    fn an_unkept_renewal_is_kept_by_the_next_request_unless_logged_out() {
        for logout in [false, true] {
            let (token_url, presented) = spawn_rotating(Duration::from_millis(300));
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("creds");
            std::fs::create_dir(&dir).unwrap();
            let src = due_source(&dir, &token_url);
            let aside = root.path().join("aside");
            let first = std::thread::scope(|s| {
                let ask = s.spawn(|| src.bearer());
                while presented.lock().unwrap().is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // A file where the cache directory was, while the refresh is
                // in flight: its write-back fails.
                std::fs::rename(&dir, &aside).unwrap();
                std::fs::write(&dir, b"").unwrap();
                ask.join().unwrap()
            });
            let e = first.unwrap_err();
            assert!(e.contains("could not be kept in the login cache"), "{e}");
            std::fs::remove_file(&dir).unwrap();
            std::fs::rename(&aside, &dir).unwrap();
            if logout {
                cache::evict_file(&dir, "mcp:x").unwrap();
                assert_eq!(
                    src.bearer().unwrap_err(),
                    "no cached credential — run `agentd --login mcp:x`"
                );
                assert!(
                    cache::load_file(&dir, "mcp:x").is_none(),
                    "not written back"
                );
            } else {
                assert_eq!(src.bearer().unwrap(), "at-2");
                let kept = cache::load_file(&dir, "mcp:x").unwrap();
                assert_eq!(kept.refresh_token.as_deref(), Some("rt-2"));
            }
            assert_eq!(*presented.lock().unwrap(), ["rt-1"]);
        }
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
