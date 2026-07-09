// SPDX-License-Identifier: Apache-2.0
//! The Person Server (PS) flow (RFC 0023 §Case C) — user-scoped identity. When
//! an MCP server answers `401 requirement=auth-token; resource-token="…"`, the
//! agent exchanges that resource token at the user's Person Server for a
//! **user-scoped auth token** (the human consents at the PS), then presents that
//! auth token instead of its agent token on subsequent calls.
//!
//! agentd drives the mechanical parts: parse the requirement, POST the resource
//! token to the PS `token_endpoint` (signed with the agent token, carrying a
//! human-readable `justification`), and — if the PS returns a pending
//! interaction — poll it to completion. The human's approve/deny happens at the
//! PS out of band (a consent screen / push / code).

use super::key::AgentKey;
use super::sig::{self, SigKey};
use crate::net::http::{self, Url};
use serde::Deserialize;
use std::time::{Duration, Instant};

/// Parse the `resource-token="…"` parameter out of an `AAuth-Requirement`
/// header value like `auth-token; resource-token="eyJ…"`.
pub fn resource_token(requirement: &str) -> Option<String> {
    let after = requirement.split("resource-token").nth(1)?;
    let after = after.trim_start_matches(['=', ' ']);
    let inner = after.trim_start_matches('"');
    Some(inner.split('"').next()?.to_string())
}

/// Whether a requirement asks for a user-scoped auth token (Case C).
pub fn wants_auth_token(requirement: &str) -> bool {
    requirement.trim_start().starts_with("auth-token")
}

#[derive(Deserialize)]
struct AuthTokenResp {
    #[serde(default)]
    auth_token: Option<String>,
    /// A pending interaction: poll this URL until the user consents.
    #[serde(default)]
    location: Option<String>,
}

#[derive(Deserialize)]
struct PollResp {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
}

/// Exchange `resource_token` at the Person Server for a user-scoped auth token,
/// signing with the current `agent_token` (the agent authenticates itself; the
/// PS then brings in the human). Polls a pending interaction up to `deadline`.
/// Returns the auth token on approval, or a clear error (denied / expired /
/// unreachable). RFC 0023 §6.
pub fn exchange(
    ps_url: &str,
    key: &AgentKey,
    agent_token: &str,
    resource_token: &str,
    justification: &str,
    timeout: Duration,
) -> Result<String, String> {
    let token_endpoint = format!("{}/token", ps_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "resource_token": resource_token,
        "justification": justification,
    });
    let resp: AuthTokenResp = signed_post(&token_endpoint, key, agent_token, &body, timeout)?;
    if let Some(tok) = resp.auth_token {
        return Ok(tok);
    }
    // Pending: the user is being asked to consent. Poll the interaction URL.
    let Some(poll_url) = resp.location else {
        return Err("aauth: PS returned neither an auth token nor an interaction URL".into());
    };
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            return Err("aauth: PS consent timed out".into());
        }
        std::thread::sleep(Duration::from_millis(500));
        let poll: PollResp = signed_get(&poll_url, key, agent_token, timeout)?;
        match poll.status.as_deref() {
            Some("approved") => {
                return poll
                    .auth_token
                    .ok_or_else(|| "aauth: PS approved without an auth token".into());
            }
            Some("denied") => {
                return Err("aauth: the user denied the request at the Person Server".into());
            }
            Some("expired") => return Err("aauth: the PS consent request expired".into()),
            // "pending" / "interacting" / None → keep polling.
            _ => continue,
        }
    }
}

fn signed_post<T: for<'de> Deserialize<'de>>(
    url: &str,
    key: &AgentKey,
    agent_token: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<T, String> {
    let u = Url::parse(url).map_err(|e| format!("aauth: PS url {url}: {e}"))?;
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let digest = sig::content_digest(&bytes);
    let hdrs = sig::sign_request(
        key,
        "POST",
        &u.host_header(),
        &u.path,
        SigKey::Jwt(agent_token),
        sig::now_secs(),
        Some(&digest),
    );
    request::<T>(&u, "POST", &hdrs, &bytes, timeout)
}

fn signed_get<T: for<'de> Deserialize<'de>>(
    url: &str,
    key: &AgentKey,
    agent_token: &str,
    timeout: Duration,
) -> Result<T, String> {
    let u = Url::parse(url).map_err(|e| format!("aauth: PS poll url {url}: {e}"))?;
    let hdrs = sig::sign_request(
        key,
        "GET",
        &u.host_header(),
        &u.path,
        SigKey::Jwt(agent_token),
        sig::now_secs(),
        None,
    );
    request::<T>(&u, "GET", &hdrs, &[], timeout)
}

fn request<T: for<'de> Deserialize<'de>>(
    u: &Url,
    method: &str,
    hdrs: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<T, String> {
    let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
    for (k, v) in hdrs {
        headers.push((k.as_str(), v.as_str()));
    }
    let mut stream = connect(u, timeout)?;
    let resp = http::send(
        stream.as_mut(),
        &u.host_header(),
        method,
        &u.path,
        &headers,
        body,
    )
    .map_err(|e| format!("aauth: PS {method} {}: {e}", u.path))?;
    if !resp.is_success() {
        return Err(format!("aauth: PS {method} returned HTTP {}", resp.status));
    }
    serde_json::from_slice(&resp.body).map_err(|e| format!("aauth: PS: bad response: {e}"))
}

fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout)
        .map_err(|e| format!("aauth: PS connect {}: {e}", url.host))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| format!("aauth: PS tls {}: {e}", url.host))?;
            return Ok(Box::new(s));
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err("aauth: https PS requires --features tls".to_string());
        }
    }
    Ok(Box::new(tcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resource_token_and_case() {
        assert!(wants_auth_token(" auth-token; resource-token=\"X\""));
        assert!(!wants_auth_token("agent-token"));
        assert_eq!(
            resource_token(r#"auth-token; resource-token="eyJabc.def""#).as_deref(),
            Some("eyJabc.def")
        );
        assert_eq!(resource_token("auth-token").as_deref(), None);
    }
}
