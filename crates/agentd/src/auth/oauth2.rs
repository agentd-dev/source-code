// SPDX-License-Identifier: AGPL-3.0-only
//! OAuth 2.1 / OIDC flows for **interactive login** and token refresh
//! (RFC 0031 §7): the **device authorization grant** (RFC 8628) — the default
//! interactive UX, working headless / over SSH with no browser or open port —
//! plus refresh-token renewal and authorization-server metadata discovery
//! (RFC 8414 / OIDC `.well-known/openid-configuration`).
//!
//! Dependency-free (the minimalism moat, RFC 0002): the hand-rolled HTTP client
//! (`net::http` + `net::tls`), `serde_json`, and a tiny form encoder — no
//! `oauth2` / `url` / `reqwest`. Secret-freedom (RFC 0012 §3.7): a confidential
//! client's `client_secret` is a `{{secret:…}}` template resolved only at the
//! moment of the token POST, form-posted, never logged.

use crate::net::http::{self, Url};
use serde::Deserialize;
use std::time::Duration;

/// Resolved OAuth endpoints + client identity for one target endpoint.
#[derive(Debug, Clone)]
pub struct OAuth2Params {
    pub token_url: String,
    /// RFC 8628 device-authorization endpoint (required for the device grant).
    pub device_authorization_url: Option<String>,
    /// The user-facing authorization endpoint (authorization-code grant; P5).
    pub authorization_url: Option<String>,
    pub client_id: String,
    /// A `{{secret:NAME}}` template for a confidential client, or `None` for a
    /// public client (the device/PKCE flows work without a secret).
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub audience: Option<String>,
}

impl OAuth2Params {
    fn scope_str(&self) -> Option<String> {
        if self.scopes.is_empty() {
            None
        } else {
            Some(self.scopes.join(" "))
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
    pub issuer: Option<String>,
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
    /// The server asked us to back off — increase the interval by 5s (RFC 8628).
    SlowDown,
    /// Authorized — the tokens.
    Token(Box<Tokens>),
}

/// An OAuth error response body (RFC 6749 §5.2).
#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
    #[serde(default)]
    #[allow(dead_code)]
    error_description: Option<String>,
}

/// Discover the authorization server's endpoints from an issuer URL, trying the
/// OIDC document then the OAuth (RFC 8414) document. Best-effort — a caller may
/// pin the endpoints in config to skip this.
pub fn discover(issuer: &str, timeout: Duration) -> Result<Discovered, String> {
    let base = issuer.trim_end_matches('/');
    let candidates = [
        format!("{base}/.well-known/openid-configuration"),
        format!("{base}/.well-known/oauth-authorization-server"),
    ];
    let mut last = String::new();
    for url in candidates {
        match get_json::<Discovered>(&url, timeout) {
            Ok(d) => return Ok(d),
            Err(e) => last = e,
        }
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
    let (status, body) = post_form(url, &form.finish(), timeout)?;
    if (200..300).contains(&status) {
        serde_json::from_slice(&body).map_err(|e| format!("oauth: bad device-auth response: {e}"))
    } else {
        Err(format!(
            "oauth: device authorization failed: {}",
            oauth_error(&body, status)
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
    add_client_secret(&mut form, params)?;
    let (status, body) = post_form(&params.token_url, &form.finish(), timeout)?;
    if (200..300).contains(&status) {
        let t: Tokens =
            serde_json::from_slice(&body).map_err(|e| format!("oauth: bad token response: {e}"))?;
        return Ok(PollOutcome::Token(Box::new(t)));
    }
    // A 400 with a well-known OAuth error drives the poll loop.
    match parse_error(&body).as_deref() {
        Some("authorization_pending") => Ok(PollOutcome::Pending),
        Some("slow_down") => Ok(PollOutcome::SlowDown),
        _ => Err(format!(
            "oauth: device token failed: {}",
            oauth_error(&body, status)
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
    add_client_secret(&mut form, params)?;
    let (status, body) = post_form(&params.token_url, &form.finish(), timeout)?;
    if (200..300).contains(&status) {
        serde_json::from_slice(&body).map_err(|e| format!("oauth: bad token response: {e}"))
    } else {
        Err(format!(
            "oauth: code exchange failed: {}",
            oauth_error(&body, status)
        ))
    }
}

/// Renew an access token from a refresh token (RFC 6749 §6).
pub fn refresh(
    params: &OAuth2Params,
    refresh_token: &str,
    timeout: Duration,
) -> Result<Tokens, String> {
    let mut form = Form::new();
    form.field("grant_type", "refresh_token");
    form.field("refresh_token", refresh_token);
    form.field("client_id", &params.client_id);
    if let Some(scope) = params.scope_str() {
        form.field("scope", &scope);
    }
    add_client_secret(&mut form, params)?;
    let (status, body) = post_form(&params.token_url, &form.finish(), timeout)?;
    if (200..300).contains(&status) {
        serde_json::from_slice(&body).map_err(|e| format!("oauth: bad refresh response: {e}"))
    } else {
        Err(format!(
            "oauth: refresh failed: {}",
            oauth_error(&body, status)
        ))
    }
}

// --- helpers ---------------------------------------------------------------

/// Resolve and add the confidential client's `client_secret` (a `{{secret:…}}`
/// template). A public client (no secret) adds nothing (device/PKCE flows).
fn add_client_secret(form: &mut Form, params: &OAuth2Params) -> Result<(), String> {
    if let Some(tmpl) = &params.client_secret {
        let env = |k: &str| std::env::var(k).ok();
        let secret = crate::sec::secret::resolve(tmpl, &env)?;
        form.field("client_secret", &secret);
    }
    Ok(())
}

/// Extract the `error` code from an OAuth error body, if present.
fn parse_error(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<OAuthError>(body)
        .ok()
        .map(|e| e.error)
}

/// A short, non-secret description of an OAuth failure for a log/error line —
/// the `error` code and status, never the raw body (it can echo request params).
fn oauth_error(body: &[u8], status: u16) -> String {
    match parse_error(body) {
        Some(code) => format!("{code} (HTTP {status})"),
        None => format!("HTTP {status}"),
    }
}

/// A minimal `application/x-www-form-urlencoded` builder.
struct Form {
    buf: String,
}
impl Form {
    fn new() -> Form {
        Form { buf: String::new() }
    }
    fn field(&mut self, key: &str, value: &str) {
        if !self.buf.is_empty() {
            self.buf.push('&');
        }
        self.buf.push_str(&form_encode(key));
        self.buf.push('=');
        self.buf.push_str(&form_encode(value));
    }
    fn finish(self) -> Vec<u8> {
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

/// POST an `application/x-www-form-urlencoded` body; return `(status, body)`
/// (a non-2xx is NOT an error here — device polling relies on reading the OAuth
/// error body of a 400).
fn post_form(url: &str, form: &[u8], timeout: Duration) -> Result<(u16, Vec<u8>), String> {
    let url = Url::parse(url).map_err(|e| format!("oauth: url {url}: {e}"))?;
    let mut stream = connect(&url, timeout)?;
    let resp = http::send(
        stream.as_mut(),
        &url.host_header(),
        "POST",
        &url.path,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        form,
    )
    .map_err(|e| format!("oauth: request failed: {e}"))?;
    Ok((resp.status, resp.body))
}

/// GET + parse JSON (discovery).
fn get_json<T: serde::de::DeserializeOwned>(url: &str, timeout: Duration) -> Result<T, String> {
    let url = Url::parse(url).map_err(|e| format!("oauth: url {url}: {e}"))?;
    let mut stream = connect(&url, timeout)?;
    let resp = http::send(
        stream.as_mut(),
        &url.host_header(),
        "GET",
        &url.path,
        &[("Accept", "application/json")],
        &[],
    )
    .map_err(|e| format!("oauth: request failed: {e}"))?;
    if !resp.is_success() {
        return Err(format!("HTTP {}", resp.status));
    }
    serde_json::from_slice(&resp.body).map_err(|e| format!("bad json: {e}"))
}

/// Connect to an OAuth endpoint (`https://` over TLS, `http://` plain — loopback
/// only in practice, guarded by the endpoint being an operator-declared IdP).
/// Shared with the RFC 9728 challenge probe ([`super::challenge`]).
pub(super) fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout)
        .map_err(|e| format!("oauth: connect {}: {e}", url.host))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| format!("oauth: tls {}: {e}", url.host))?;
            Ok(Box::new(s))
        }
        #[cfg(not(feature = "tls"))]
        {
            Err("oauth: https requires building with --features tls".to_string())
        }
    } else {
        Ok(Box::new(tcp))
    }
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
}
