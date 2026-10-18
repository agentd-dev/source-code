// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 / OIDC flows for **interactive login** and token refresh: the
//! **device authorization grant** (RFC 8628) — the default interactive UX,
//! which works headless and over SSH with no browser and no open port — plus
//! refresh-token renewal and authorization-server metadata discovery
//! (RFC 8414, then OIDC `.well-known/openid-configuration`).
//!
//! Dependency-free by design: the hand-rolled HTTP client (`net::http` +
//! `net::tls`, through [`super::http`]), `serde_json`, and a tiny form
//! encoder — no `oauth2` / `url` / `reqwest`. A confidential client's
//! `client_secret` stays a `{{secret:…}}` template until the moment of the
//! token POST, where it is resolved, sent, and dropped — so it is never held
//! in a config value and never reaches a log line. A `private_key_jwt`
//! client's key is read the same way, and only a short-lived assertion
//! signed with it is sent ([`super::client_assertion`]).

use crate::auth::http::{self, Dial, Reply};
use serde::Deserialize;
use std::time::Duration;

/// Resolved OAuth endpoints + client identity for one target endpoint.
#[derive(Debug, Clone, Default)]
pub struct OAuth2Params {
    pub token_url: String,
    /// RFC 8628 device-authorization endpoint (required for the device grant).
    pub device_authorization_url: Option<String>,
    /// The user-facing authorization endpoint (authorization-code grant).
    pub authorization_url: Option<String>,
    /// The RFC 7009 revocation endpoint, when the server's metadata names one.
    pub revocation_url: Option<String>,
    /// The authorization server's issuer identifier: configured, or the one
    /// the endpoints were discovered from. What an authorization response's
    /// `iss` must equal (RFC 9207), and an assertion's `aud` when it names
    /// the issuer.
    pub issuer: Option<String>,
    /// The server's metadata declares `authorization_response_iss_parameter_supported`:
    /// an authorization response without `iss` is then refused.
    pub iss_supported: bool,
    pub client_id: String,
    /// A `{{secret:NAME}}` template for a confidential client, or `None` for a
    /// public client (the device/PKCE flows work without a secret).
    pub client_secret: Option<String>,
    /// How the client authenticates at the token and revocation endpoints.
    pub client_auth: ClientAuth,
    pub scopes: Vec<String>,
    pub audience: Option<String>,
}

/// How a client authenticates at the token and revocation endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ClientAuth {
    /// `client_secret_post`: the secret, when there is one, in the form
    /// body. A public client sends only its `client_id`.
    #[default]
    Post,
    /// `client_secret_basic`: the secret in an `Authorization: Basic` header
    /// (RFC 6749 §2.3.1).
    Basic,
    /// `private_key_jwt`: a JWT signed with the PKCS#8 key `key` (a secret
    /// reference), published under `kid` or its RFC 7638 thumbprint.
    PrivateKeyJwt {
        key: String,
        kid: Option<String>,
        aud: AssertionAud,
    },
}

impl ClientAuth {
    /// The method an `auth:` block's `client_auth` names; none is the
    /// default, a secret (if any) in the form body.
    pub fn of(spec: Option<&crate::config::ClientAuthSpec>) -> Result<ClientAuth, String> {
        let Some(spec) = spec else {
            return Ok(ClientAuth::Post);
        };
        match spec.method.as_str() {
            "client_secret_post" => Ok(ClientAuth::Post),
            "client_secret_basic" => Ok(ClientAuth::Basic),
            "private_key_jwt" => Ok(ClientAuth::PrivateKeyJwt {
                key: spec
                    .key
                    .clone()
                    .ok_or("auth.client_auth private_key_jwt needs `key`")?,
                kid: spec.kid.clone(),
                aud: match spec.assertion_aud.as_deref() {
                    None | Some("token_endpoint") => AssertionAud::TokenEndpoint,
                    Some("issuer") => AssertionAud::Issuer,
                    Some(other) => {
                        return Err(format!(
                            "auth.client_auth assertion_aud '{other}' (want token_endpoint|issuer)"
                        ));
                    }
                },
            }),
            other => Err(format!(
                "auth.client_auth method '{other}' (want private_key_jwt|client_secret_basic|client_secret_post)"
            )),
        }
    }
}

/// What a client assertion's `aud` names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssertionAud {
    /// The token endpoint URL (RFC 7523 §3; Okta requires it).
    #[default]
    TokenEndpoint,
    /// The issuer identifier (draft-ietf-oauth-rfc7523bis).
    Issuer,
}

impl OAuth2Params {
    fn scope_str(&self) -> Option<String> {
        if self.scopes.is_empty() {
            None
        } else {
            Some(self.scopes.join(" "))
        }
    }

    /// Add this client's authentication to a token or revocation request:
    /// form fields, and the headers to send with it.
    fn authenticate(&self, form: &mut Form) -> Result<Vec<(String, String)>, String> {
        let env = |k: &str| std::env::var(k).ok();
        let secret = || -> Result<Option<String>, String> {
            self.client_secret
                .as_deref()
                .map(|t| crate::sec::secret::resolve(t, &env))
                .transpose()
        };
        match &self.client_auth {
            ClientAuth::Post => {
                if let Some(s) = secret()? {
                    form.field("client_secret", &s);
                }
                Ok(Vec::new())
            }
            ClientAuth::Basic => {
                let s = secret()?.ok_or("oauth: client_secret_basic needs client_secret")?;
                let pair = format!("{}:{}", form_encode(&self.client_id), form_encode(&s));
                Ok(vec![(
                    "Authorization".to_string(),
                    format!("Basic {}", crate::b64::std_pad(pair.as_bytes())),
                )])
            }
            ClientAuth::PrivateKeyJwt { key, kid, aud } => {
                let pem = crate::sec::secret::resolve(key, &env)?;
                let mut signer = super::client_assertion::ClientKey::from_pem(&pem)?;
                if let Some(kid) = kid {
                    signer = signer.with_kid(kid);
                }
                let aud = match aud {
                    AssertionAud::TokenEndpoint => self.token_url.as_str(),
                    AssertionAud::Issuer => self
                        .issuer
                        .as_deref()
                        .ok_or("oauth: assertion_aud issuer needs auth.issuer")?,
                };
                let assertion = signer.client_assertion(&self.client_id, aud)?;
                form.field(
                    "client_assertion_type",
                    super::client_assertion::ASSERTION_TYPE,
                );
                form.field("client_assertion", &assertion);
                Ok(Vec::new())
            }
        }
    }
}

/// The authorization-server metadata we consume (RFC 8414 / OIDC discovery).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Discovered {
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
    #[serde(default)]
    pub authorization_endpoint: Option<String>,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    #[serde(default)]
    pub issuer: Option<String>,
    /// RFC 9207 §3: the server puts `iss` in every authorization response.
    #[serde(default)]
    pub authorization_response_iss_parameter_supported: bool,
}

/// The device-authorization response (RFC 8628 §3.2). `interval` defaults to the
/// RFC-recommended 5s when omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: Option<u64>,
}
fn default_interval() -> u64 {
    5
}

/// A token response (RFC 6749 §5.1 / RFC 8628 §3.5).
#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// The result of one device-token poll (RFC 8628 §3.5).
#[derive(Debug)]
pub enum PollOutcome {
    /// The user has not yet authorized — keep polling at the current interval.
    Pending,
    /// The server asked us to back off — add 5s to the interval and keep it
    /// there for the rest of the flow; polling faster risks being cut off.
    SlowDown,
    /// The server answered `429` or `503` with a `Retry-After`: poll again no
    /// sooner than this.
    Wait(Duration),
    /// Authorized — the tokens.
    Token(Box<Tokens>),
}

/// A token request the server refused: what to say, and how long the server
/// asked to be left alone when it said (`429`/`503` with `Retry-After`).
#[derive(Debug)]
pub struct TokenError {
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for TokenError {
    fn from(message: String) -> TokenError {
        TokenError {
            message,
            retry_after: None,
        }
    }
}

impl From<TokenError> for String {
    fn from(e: TokenError) -> String {
        e.message
    }
}

/// An OAuth error response body (RFC 6749 §5.2).
#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
}

/// The metadata URLs to try for `issuer`, in the order MCP's authorization
/// spec gives (2025-11-25 and 2026-07-28): RFC 8414 first, then OIDC; for an
/// issuer with a path, each with the well-known suffix inserted before the
/// path, and last OIDC's appended form.
pub fn discovery_urls(issuer: &str) -> Vec<String> {
    let asm = http::insert_well_known(issuer, "oauth-authorization-server");
    let oidc = http::insert_well_known(issuer, "openid-configuration");
    let has_path = http::split_origin(issuer).is_some_and(|(_, p)| {
        !p.split(['?', '#'])
            .next()
            .unwrap_or("")
            .trim_matches('/')
            .is_empty()
    });
    let mut out: Vec<String> = [asm, oidc].into_iter().flatten().collect();
    if has_path || out.is_empty() {
        out.push(format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        ));
    }
    out
}

/// Discover the authorization server's endpoints from its issuer identifier,
/// trying [`discovery_urls`] in order. The first document that answers is
/// the server's, and its `issuer` must be byte-for-byte the identifier the
/// URL was built from (RFC 8414 §3.3, OIDC Discovery §4.3): a document that
/// names another issuer, or none, is refused rather than used — metadata
/// that does not match was never safe to use, since it is how a mix-up
/// attack points a client at another server's endpoints. A caller may pin
/// the endpoints in config to skip this.
pub fn discover(issuer: &str, timeout: Duration) -> Result<Discovered, String> {
    let mut last = String::new();
    for url in discovery_urls(issuer) {
        let found = match get_json::<Discovered>(&url, timeout) {
            Ok(d) => d,
            Err(e) => {
                last = e;
                continue;
            }
        };
        return match found.issuer.as_deref() {
            Some(i) if i == issuer => Ok(found),
            Some(i) => Err(format!(
                "oauth: discovery: {url} names the issuer {i:?}, not {issuer:?}"
            )),
            None => Err(format!("oauth: discovery: {url} names no issuer")),
        };
    }
    Err(format!("oauth: discovery failed: {last}"))
}

/// Start the device-authorization grant (RFC 8628 §3.1): returns the code the
/// user enters and the verification URI to visit.
pub fn start_device(params: &OAuth2Params, timeout: Duration) -> Result<DeviceAuth, String> {
    let url = params
        .device_authorization_url
        .as_deref()
        .ok_or("oauth: no device_authorization endpoint (set it or enable discovery)")?;
    let mut form = Form::new();
    form.field("client_id", &params.client_id);
    if let Some(scope) = params.scope_str() {
        form.field("scope", &scope);
    }
    if let Some(aud) = &params.audience {
        form.field("audience", aud);
    }
    let reply = post_form(url, form, &[], timeout)?;
    if reply.is_success() {
        serde_json::from_slice(&reply.body)
            .map_err(|e| format!("oauth: bad device-auth response: {e}"))
    } else {
        Err(format!(
            "oauth: device authorization failed: {}",
            oauth_error(&reply)
        ))
    }
}

/// One poll of the token endpoint for a device grant (RFC 8628 §3.4). The caller
/// loops, honoring [`PollOutcome::Pending`]/[`PollOutcome::SlowDown`] and the
/// device code's expiry, so this stays unit-testable without real sleeps.
pub fn poll_device_once(
    params: &OAuth2Params,
    device_code: &str,
    timeout: Duration,
) -> Result<PollOutcome, String> {
    let mut form = Form::new();
    form.field("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
    form.field("device_code", device_code);
    form.field("client_id", &params.client_id);
    let headers = params.authenticate(&mut form)?;
    let reply = post_form(&params.token_url, form, &headers, timeout)?;
    if reply.is_success() {
        let t: Tokens = serde_json::from_slice(&reply.body)
            .map_err(|e| format!("oauth: bad token response: {e}"))?;
        return Ok(PollOutcome::Token(Box::new(t)));
    }
    if let Some(wait) = throttled(&reply) {
        return Ok(PollOutcome::Wait(wait));
    }
    // A 400 with a well-known OAuth error drives the poll loop.
    match parse_error(&reply.body).as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        _ => Err(format!(
            "oauth: device token failed: {}",
            oauth_error(&reply)
        )),
    }
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3 + PKCE RFC 7636).
pub fn exchange_code(
    params: &OAuth2Params,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    timeout: Duration,
) -> Result<Tokens, String> {
    let mut form = Form::new();
    form.field("grant_type", "authorization_code");
    form.field("code", code);
    form.field("redirect_uri", redirect_uri);
    form.field("client_id", &params.client_id);
    form.field("code_verifier", verifier);
    let headers = params.authenticate(&mut form)?;
    let reply = post_form(&params.token_url, form, &headers, timeout)?;
    if reply.is_success() {
        serde_json::from_slice(&reply.body).map_err(|e| format!("oauth: bad token response: {e}"))
    } else {
        Err(format!(
            "oauth: code exchange failed: {}",
            oauth_error(&reply)
        ))
    }
}

/// Renew an access token from a refresh token (RFC 6749 §6). The response's
/// `refresh_token`, when it has one, replaces the one presented; when it has
/// none, the one presented stays good (§6: the server MAY issue a new one).
pub fn refresh(
    params: &OAuth2Params,
    refresh_token: &str,
    timeout: Duration,
) -> Result<Tokens, TokenError> {
    let mut form = Form::new();
    form.field("grant_type", "refresh_token");
    form.field("refresh_token", refresh_token);
    form.field("client_id", &params.client_id);
    if let Some(scope) = params.scope_str() {
        form.field("scope", &scope);
    }
    let headers = params.authenticate(&mut form)?;
    let reply = post_form(&params.token_url, form, &headers, timeout)?;
    if reply.is_success() {
        serde_json::from_slice(&reply.body)
            .map_err(|e| format!("oauth: bad refresh response: {e}").into())
    } else {
        Err(TokenError {
            message: format!("oauth: refresh failed: {}", oauth_error(&reply)),
            retry_after: throttled(&reply),
        })
    }
}

/// Revoke `token` at the RFC 7009 endpoint (§2.1), with this client's
/// authentication. `hint` is `refresh_token` or `access_token`. A `200` is
/// success — also for a token the server no longer knows (§2.2).
pub fn revoke(
    params: &OAuth2Params,
    url: &str,
    token: &str,
    hint: &str,
    timeout: Duration,
) -> Result<(), String> {
    let mut form = Form::new();
    form.field("token", token);
    form.field("token_type_hint", hint);
    form.field("client_id", &params.client_id);
    let headers = params.authenticate(&mut form)?;
    let reply = post_form(url, form, &headers, timeout)?;
    if reply.status == 200 {
        Ok(())
    } else {
        Err(format!("oauth: revocation failed: {}", oauth_error(&reply)))
    }
}

// --- helpers ---------------------------------------------------------------

/// A `429` or `503`'s `Retry-After`: the server's own wait.
fn throttled(reply: &Reply) -> Option<Duration> {
    matches!(reply.status, 429 | 503)
        .then(|| reply.retry_after())
        .flatten()
}

/// Extract the `error` code from an OAuth error body, if present.
fn parse_error(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<OAuthError>(body)
        .ok()
        .map(|e| e.error)
}

/// A short, non-secret description of an OAuth failure for a log/error line —
/// the `error` code and status, and a `Retry-After` the server gave, never
/// the raw body (it can echo request params).
fn oauth_error(reply: &Reply) -> String {
    let mut out = match parse_error(&reply.body) {
        Some(code) => format!("{code} (HTTP {})", reply.status),
        None => format!("HTTP {}", reply.status),
    };
    if let Some(wait) = throttled(reply) {
        out.push_str(&format!(", retry after {}s", wait.as_secs()));
    }
    out
}

/// A minimal `application/x-www-form-urlencoded` builder.
pub(crate) struct Form {
    buf: String,
}
impl Form {
    pub(crate) fn new() -> Form {
        Form { buf: String::new() }
    }
    pub(crate) fn field(&mut self, key: &str, value: &str) {
        if !self.buf.is_empty() {
            self.buf.push('&');
        }
        self.buf.push_str(&form_encode(key));
        self.buf.push('=');
        self.buf.push_str(&form_encode(value));
    }
    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf.into_bytes()
    }
}

/// Percent-encode a form component (unreserved `A-Za-z0-9-._~` pass through).
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// POST a form to an operator-configured endpoint (or one its issuer's
/// metadata names), with `headers`.
fn post_form(
    url: &str,
    form: Form,
    headers: &[(String, String)],
    timeout: Duration,
) -> Result<Reply, String> {
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    http::post_form(url, &form.finish(), &headers, timeout, Dial::Named)
}

/// GET + parse JSON (discovery).
fn get_json<T: serde::de::DeserializeOwned>(url: &str, timeout: Duration) -> Result<T, String> {
    let reply = http::get(url, &[("Accept", "application/json")], timeout, Dial::Named)?;
    if !reply.is_success() {
        return Err(format!("HTTP {}", reply.status));
    }
    serde_json::from_slice(&reply.body).map_err(|e| format!("bad json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encode_matches_www_form_urlencoded() {
        assert_eq!(form_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(form_encode("keep-._~"), "keep-._~");
        let mut f = Form::new();
        f.field("grant_type", "refresh_token");
        f.field("scope", "a b");
        assert_eq!(
            String::from_utf8(f.finish()).unwrap(),
            "grant_type=refresh_token&scope=a%20b"
        );
    }

    #[test]
    fn parse_error_reads_the_oauth_code() {
        assert_eq!(
            parse_error(br#"{"error":"authorization_pending"}"#).as_deref(),
            Some("authorization_pending")
        );
        assert_eq!(parse_error(b"not json"), None);
    }

    /// RFC 8414 is asked first, then OIDC; an issuer with a path has the
    /// suffix inserted before it, and OIDC's appended form comes last.
    #[test]
    fn discovery_tries_rfc_8414_then_oidc_with_the_suffix_inserted() {
        assert_eq!(
            discovery_urls("https://as.example"),
            [
                "https://as.example/.well-known/oauth-authorization-server",
                "https://as.example/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            discovery_urls("https://as.example/"),
            [
                "https://as.example/.well-known/oauth-authorization-server",
                "https://as.example/.well-known/openid-configuration",
            ]
        );
        assert_eq!(
            discovery_urls("https://login.example/tenant1/v2.0"),
            [
                "https://login.example/.well-known/oauth-authorization-server/tenant1/v2.0",
                "https://login.example/.well-known/openid-configuration/tenant1/v2.0",
                "https://login.example/tenant1/v2.0/.well-known/openid-configuration",
            ]
        );
    }

    /// A loopback authorization server answering each path from `docs`
    /// (`{base}` in a body is its own URL) and 404 otherwise, recording the
    /// paths asked for.
    fn spawn_metadata(
        docs: Vec<(&'static str, &'static str)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (me, log) = (base.clone(), std::sync::Arc::clone(&asked));
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).ok();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let (status, body) = match docs.iter().find(|(p, _)| *p == path) {
                    Some((_, b)) => ("200 OK", b.replace("{base}", &me)),
                    None => ("404 Not Found", String::new()),
                };
                let mut w = c;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, asked)
    }

    /// Discovery asks RFC 8414 first, at the path-inserted URL, and takes
    /// OIDC only when that is not there; the document's `issuer` must be the
    /// issuer byte for byte, and one naming another, or none, is refused.
    #[test]
    fn discovery_asks_rfc_8414_first_and_refuses_another_issuer() {
        let t = Duration::from_secs(5);
        let (base, asked) = spawn_metadata(vec![
            (
                "/.well-known/oauth-authorization-server/tenant",
                r#"{"issuer":"{base}/tenant","token_endpoint":"{base}/as-token","revocation_endpoint":"{base}/revoke","authorization_response_iss_parameter_supported":true}"#,
            ),
            (
                "/.well-known/openid-configuration/tenant",
                r#"{"issuer":"{base}/tenant","token_endpoint":"{base}/oidc-token"}"#,
            ),
            (
                "/other/.well-known/openid-configuration",
                r#"{"issuer":"{base}/other","token_endpoint":"{base}/oidc-token"}"#,
            ),
            (
                "/.well-known/oauth-authorization-server/evil",
                r#"{"issuer":"https://evil.example","token_endpoint":"https://evil.example/t"}"#,
            ),
            (
                "/.well-known/oauth-authorization-server/bare",
                r#"{"token_endpoint":"{base}/t"}"#,
            ),
        ]);
        let d = discover(&format!("{base}/tenant"), t).unwrap();
        assert_eq!(d.token_endpoint, Some(format!("{base}/as-token")));
        assert_eq!(d.revocation_endpoint, Some(format!("{base}/revoke")));
        assert!(d.authorization_response_iss_parameter_supported);
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            ["/.well-known/oauth-authorization-server/tenant"]
        );
        // No RFC 8414 document: OIDC's, path-inserted, then appended.
        asked.lock().unwrap().clear();
        let d = discover(&format!("{base}/other"), t).unwrap();
        assert_eq!(d.token_endpoint, Some(format!("{base}/oidc-token")));
        assert_eq!(
            asked.lock().unwrap().as_slice(),
            [
                "/.well-known/oauth-authorization-server/other",
                "/.well-known/openid-configuration/other",
                "/other/.well-known/openid-configuration",
            ]
        );
        // Another issuer's metadata, or metadata naming none: refused, and
        // no later candidate is asked to stand in for it.
        asked.lock().unwrap().clear();
        let e = discover(&format!("{base}/evil"), t).unwrap_err();
        assert!(
            e.contains("names the issuer \"https://evil.example\""),
            "{e}"
        );
        assert_eq!(asked.lock().unwrap().len(), 1);
        let e = discover(&format!("{base}/bare"), t).unwrap_err();
        assert!(e.contains("names no issuer"), "{e}");
        // The match is exact: a trailing slash is another issuer.
        let e = discover(&format!("{base}/tenant/"), t).unwrap_err();
        assert!(e.contains("not"), "{e}");
    }

    /// A `private_key_jwt` client sends, in place of a secret, an ES256
    /// assertion that the server can verify with the public key alone,
    /// addressed to its token endpoint, fresh on every request.
    #[test]
    fn a_private_key_jwt_client_signs_each_token_request() {
        use std::io::{BufRead, BufReader, Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.pem");
        let key = crate::auth::client_assertion::ClientKey::load_or_create(&path).unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let token_url = format!("http://{}/token", l.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for c in l.incoming().take(2).flatten() {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).unwrap();
                tx.send(String::from_utf8(body).unwrap()).unwrap();
                let ok = r#"{"access_token":"at","expires_in":60}"#;
                let mut w = c;
                let _ = write!(
                    w,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok}",
                    ok.len()
                );
            }
        });
        let p = OAuth2Params {
            token_url: token_url.clone(),
            client_id: "agentd".into(),
            client_auth: ClientAuth::PrivateKeyJwt {
                key: format!("{{{{secret-file:{}}}}}", path.display()),
                kid: None,
                aud: AssertionAud::TokenEndpoint,
            },
            ..Default::default()
        };
        let mut jtis = Vec::new();
        for _ in 0..2 {
            refresh(&p, "rt", Duration::from_secs(5)).unwrap();
            let form = rx.recv().unwrap();
            let field = |k: &str| {
                form.split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .map(str::to_string)
                    .unwrap_or_default()
            };
            assert_eq!(
                field("client_assertion_type"),
                "urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer"
            );
            assert!(!form.contains("client_secret"), "{form}");
            let jwt = field("client_assertion");
            let (input, sig) = jwt.rsplit_once('.').unwrap();
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::ECDSA_P256_SHA256_FIXED,
                key.public_point(),
            )
            .verify(input.as_bytes(), &crate::b64::url_decode(sig).unwrap())
            .expect("verifies with the public key");
            let claims: serde_json::Value = serde_json::from_slice(
                &crate::b64::url_decode(input.split('.').nth(1).unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(claims["aud"], token_url.as_str());
            assert_eq!(
                (claims["iss"].as_str(), claims["sub"].as_str()),
                (Some("agentd"), Some("agentd"))
            );
            jtis.push(claims["jti"].clone());
        }
        assert_ne!(jtis[0], jtis[1], "a fresh assertion per request");
    }

    #[test]
    fn basic_client_auth_encodes_the_id_and_secret_first() {
        // SAFETY: a variable no other test reads.
        unsafe { std::env::set_var("OAUTH2_BASIC_TEST", "s:e/c") };
        let p = OAuth2Params {
            client_id: "a b".into(),
            client_secret: Some("{{secret:OAUTH2_BASIC_TEST}}".into()),
            client_auth: ClientAuth::Basic,
            ..Default::default()
        };
        let mut f = Form::new();
        let h = p.authenticate(&mut f).unwrap();
        // RFC 6749 §2.3.1: each half form-encoded, then base64.
        assert_eq!(
            h,
            [(
                "Authorization".to_string(),
                format!("Basic {}", crate::b64::std_pad(b"a%20b:s%3Ae%2Fc"))
            )]
        );
        assert!(f.finish().is_empty(), "the secret is not also in the body");
        unsafe { std::env::remove_var("OAUTH2_BASIC_TEST") };
    }
}
