// SPDX-License-Identifier: Apache-2.0
//! OAuth 2.1 **client-credentials** (M2M) token acquisition for remote
//! intelligence / MCP endpoints. RFC 0006 §auth. [feature: oauth]
//!
//! For a service-to-service grant (no user, no browser), agentd exchanges a
//! `client_id` + `client_secret` at the provider's **token endpoint** for a
//! short-lived bearer access token, caches it, and refreshes shortly before
//! expiry. The token then rides the request as `Authorization: Bearer …` — so an
//! MCP server behind an OAuth gateway is reached with a rotating credential
//! rather than a static long-lived token.
//!
//! Secret-freedom (RFC 0012 §3.7): the `client_secret` is a `{{secret:…}}` /
//! `{{secret-file:…}}` template resolved at fetch time; it is form-posted to the
//! token endpoint and never logged. The access token lives only in memory.
//!
//! Dependency-free: the hand-rolled HTTP client (`net::http` + `net::tls`), the
//! `sec::secret` resolver, `serde_json`, and a tiny hand-rolled form encoder —
//! no `oauth2`/`url`/`form_urlencoded` crate (the minimalism moat, RFC 0002).

use crate::net::http::{self, Url};
use crate::sec::secret;
use serde::Deserialize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How early (before the advertised expiry) to proactively refresh, so an
/// in-flight request never rides a token that expires mid-flight.
const REFRESH_SKEW: Duration = Duration::from_secs(30);
/// Fallback lifetime when the token endpoint omits `expires_in`.
const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// A client-credentials configuration. `client_secret` is a secret-free template
/// (`{{secret:…}}`), resolved at fetch time.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub token_url: String,
    pub client_id: String,
    /// A `{{secret:NAME}}` / `{{secret-file:PATH}}` template (never an inline
    /// secret — the config/manifest stays secret-free).
    pub client_secret: String,
    /// Optional space-delimited `scope` request.
    pub scope: Option<String>,
}

/// A cached access token with the instant it must be refreshed by.
struct Cached {
    access_token: String,
    /// When the token stops being usable (already adjusted for [`REFRESH_SKEW`]).
    good_until: Instant,
}

/// A caching client-credentials token source. `bearer()` returns a live token,
/// fetching or refreshing under the hood. Cheap to share (one per endpoint).
pub struct OAuthClient {
    config: OAuthConfig,
    cached: Mutex<Option<Cached>>,
    timeout: Duration,
}

impl OAuthClient {
    pub fn new(config: OAuthConfig, timeout: Duration) -> OAuthClient {
        OAuthClient {
            config,
            cached: Mutex::new(None),
            timeout,
        }
    }

    /// Return a currently-valid access token, refreshing when the cached one is
    /// within [`REFRESH_SKEW`] of expiry (or absent). The value is the bare
    /// token — the caller frames it as `Authorization: Bearer …`.
    pub fn bearer(&self) -> Result<String, String> {
        let now = Instant::now();
        {
            let guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(c) = guard.as_ref()
                && now < c.good_until
            {
                return Ok(c.access_token.clone());
            }
        }
        // Miss/expired: fetch outside the lock, then store.
        let fresh = self.fetch()?;
        let token = fresh.access_token.clone();
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh);
        Ok(token)
    }

    /// One token-endpoint round-trip: POST the `client_credentials` grant as a
    /// form body and parse the access token + lifetime.
    fn fetch(&self) -> Result<Cached, String> {
        let env = |k: &str| std::env::var(k).ok();
        let client_secret = secret::resolve(&self.config.client_secret, &env)?;

        let mut form = String::new();
        form.push_str("grant_type=client_credentials");
        form.push_str("&client_id=");
        form.push_str(&form_encode(&self.config.client_id));
        form.push_str("&client_secret=");
        form.push_str(&form_encode(&client_secret));
        if let Some(scope) = &self.config.scope {
            form.push_str("&scope=");
            form.push_str(&form_encode(scope));
        }

        let body = self.post_form(&self.config.token_url, form.as_bytes())?;
        let parsed: TokenResponse =
            serde_json::from_slice(&body).map_err(|e| format!("oauth: bad token response: {e}"))?;
        let ttl = parsed
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TTL);
        // Never let the skew push the deadline into the past for a tiny ttl.
        let good_for = ttl.saturating_sub(REFRESH_SKEW).max(Duration::from_secs(1));
        Ok(Cached {
            access_token: parsed.access_token,
            good_until: Instant::now() + good_for,
        })
    }

    /// POST an `application/x-www-form-urlencoded` body to `url` and return the
    /// response body, mapping a non-2xx status to an error (the body may carry an
    /// OAuth `error`, but must not be logged — it can echo request params).
    fn post_form(&self, url: &str, form: &[u8]) -> Result<Vec<u8>, String> {
        let url = Url::parse(url).map_err(|e| format!("oauth: token_url: {e}"))?;
        let mut stream = connect(&url, self.timeout)?;
        let resp = http::send(
            stream.as_mut(),
            &url.host_header(),
            "POST",
            &url.path,
            &[("Content-Type", "application/x-www-form-urlencoded")],
            form,
        )
        .map_err(|e| format!("oauth: token request failed: {e}"))?;
        if !resp.is_success() {
            return Err(format!(
                "oauth: token endpoint returned HTTP {}",
                resp.status
            ));
        }
        Ok(resp.body)
    }
}

/// The subset of an RFC 6749 §5.1 token response we consume.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Connect to a token endpoint (`https://` over TLS, or `http://` plain TCP).
fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
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
            Err("oauth: https token_url requires building with --features tls".to_string())
        }
    } else {
        Ok(Box::new(tcp))
    }
}

/// Percent-encode a form value per `application/x-www-form-urlencoded`: the
/// unreserved set (`A-Za-z0-9-._~`) passes through; everything else becomes
/// `%XX`. (Space is encoded `%20`, not `+` — both are accepted by servers, and
/// `%20` avoids ambiguity for tokens containing `+`.) Hand-rolled: no `url` crate.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encode_escapes_reserved_and_keeps_unreserved() {
        assert_eq!(form_encode("abcXYZ0-9._~"), "abcXYZ0-9._~");
        assert_eq!(form_encode("a b"), "a%20b");
        assert_eq!(form_encode("s3cr3t/+=&"), "s3cr3t%2F%2B%3D%26");
    }

    #[test]
    fn token_response_parses_minimal_and_full() {
        let full: TokenResponse = serde_json::from_str(
            r#"{"access_token":"tok","token_type":"Bearer","expires_in":3600}"#,
        )
        .unwrap();
        assert_eq!(full.access_token, "tok");
        assert_eq!(full.expires_in, Some(3600));
        // expires_in is optional (falls back to DEFAULT_TTL at the call site).
        let minimal: TokenResponse = serde_json::from_str(r#"{"access_token":"tok2"}"#).unwrap();
        assert_eq!(minimal.access_token, "tok2");
        assert_eq!(minimal.expires_in, None);
    }
}
