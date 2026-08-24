// SPDX-License-Identifier: AGPL-3.0-only
//! AWS **IAM Identity Center (SSO)** interactive login — the `aws` provider's
//! `source: sso`. This is the "enterprise login for Bedrock" flow: an OIDC
//! **device authorization** grant against AWS SSO-OIDC yields an SSO access
//! token, which the SSO portal exchanges for **temporary AWS credentials**
//! (access key / secret / session token). Those are cached and SigV4-sign
//! requests (see [`super::aws`]).
//!
//! AWS SSO-OIDC speaks JSON (camelCase), not form-encoding, so this has its own
//! tiny HTTP helpers. Endpoints are the public `oidc.{region}.amazonaws.com` and
//! `portal.sso.{region}.amazonaws.com` (no SSRF carve-out needed). Dependency-
//! free: `net::http` + `serde_json`.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::auth::cache::{self, CachedCred};
use crate::auth::login::DevicePrompt;
use crate::net::http::{self, Url};

/// The `agentd login` disposition for an `aws source: sso` block.
pub struct SsoParams {
    pub region: String,
    pub start_url: String,
    pub account_id: String,
    pub role_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterClientResp {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartDeviceResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default)]
    expires_in: Option<u64>,
}
fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTokenResp {
    access_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleCredsResp {
    role_credentials: RoleCreds,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoleCreds {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
    /// Expiry in **milliseconds** since the epoch (the SSO portal returns ms).
    #[serde(default)]
    expiration: u64,
}

/// Run the SSO login: register a public client, drive the device flow (printing
/// the prompt), then exchange the SSO token for temporary AWS credentials. The
/// result is a [`CachedCred`] whose `extra` holds the AWS keys and whose
/// `expires_at_ms` is the credentials' expiry — the SSO access token itself is
/// deliberately not kept, since only the derived AWS keys sign requests. When
/// the portal reports no expiry, the record falls back to one hour so a stale
/// credential cannot be cached indefinitely.
pub fn sso_login(
    p: &SsoParams,
    timeout: Duration,
    on_prompt: impl FnOnce(&DevicePrompt),
    sleep: impl Fn(Duration),
) -> Result<CachedCred, String> {
    let oidc = oidc_base(&p.region);

    // 1. Register a public client — the AWS SSO-OIDC precursor to the RFC 8628
    //    device grant. AWS SSO-OIDC issues a short-lived client
    //    id/secret pair per login; there is no long-lived client to configure.
    let reg: RegisterClientResp = post_json(
        &format!("{oidc}/client/register"),
        &json!({"clientName": "agentd", "clientType": "public"}),
        timeout,
    )?;

    // 2. Start the device authorization.
    let dev: StartDeviceResp = post_json(
        &format!("{oidc}/device_authorization"),
        &json!({"clientId": reg.client_id, "clientSecret": reg.client_secret, "startUrl": p.start_url}),
        timeout,
    )?;
    on_prompt(&DevicePrompt {
        verification_uri: &dev.verification_uri,
        verification_uri_complete: dev.verification_uri_complete.as_deref(),
        user_code: &dev.user_code,
        expires_in: dev.expires_in,
    });

    // 3. Poll the token endpoint until the user authorizes.
    let mut interval = Duration::from_secs(dev.interval.max(1));
    let deadline =
        Instant::now() + Duration::from_secs(dev.expires_in.unwrap_or(600).clamp(30, 1800));
    let token_body = json!({
        "clientId": reg.client_id,
        "clientSecret": reg.client_secret,
        "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        "deviceCode": dev.device_code,
    });
    let access_token = loop {
        let (status, body) = post_raw(&format!("{oidc}/token"), &token_body, timeout)?;
        if (200..300).contains(&status) {
            let t: CreateTokenResp = serde_json::from_slice(&body)
                .map_err(|e| format!("aws sso: bad token response: {e}"))?;
            break t.access_token;
        }
        match oauth_error(&body).as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += Duration::from_secs(5),
            other => {
                return Err(format!(
                    "aws sso: token failed: {} (HTTP {status})",
                    other.unwrap_or("error")
                ));
            }
        }
        if Instant::now() + interval >= deadline {
            return Err("aws sso: device code expired before authorization".into());
        }
        sleep(interval);
    };

    // 4. Exchange the SSO token for temporary AWS credentials.
    let portal = format!(
        "{}/federation/credentials?role_name={}&account_id={}",
        portal_base(&p.region),
        pct(&p.role_name),
        pct(&p.account_id)
    );
    let creds: RoleCredsResp = get_json(
        &portal,
        &[("x-amz-sso_bearer_token", access_token.as_str())],
        timeout,
    )?;
    let rc = creds.role_credentials;

    let mut extra = serde_json::Map::new();
    extra.insert("aws_access_key_id".into(), json!(rc.access_key_id));
    extra.insert("aws_secret_access_key".into(), json!(rc.secret_access_key));
    extra.insert("aws_session_token".into(), json!(rc.session_token));
    Ok(CachedCred {
        access_token: String::new(),
        refresh_token: None,
        expires_at_ms: if rc.expiration > 0 {
            rc.expiration
        } else {
            cache::now_ms() + 3_600_000
        },
        token_type: Some("aws-sso".into()),
        extra,
    })
}

/// Load the cached temporary AWS credentials for `target`, written there by an
/// SSO login. The record's `expires_at_ms` is not consulted: an expired SSO
/// session yields keys that AWS rejects at the endpoint rather than an early
/// local failure, and the fix in both cases is to log in again.
pub fn cached_creds(target: &str) -> Option<super::aws::AwsCreds> {
    let c = cache::load_file(&cache::default_dir(), target)?;
    let ak = c.extra.get("aws_access_key_id")?.as_str()?.to_string();
    let sk = c.extra.get("aws_secret_access_key")?.as_str()?.to_string();
    let st = c
        .extra
        .get("aws_session_token")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(super::aws::AwsCreds {
        access_key: ak,
        secret_key: sk,
        session_token: st,
    })
}

// --- helpers ---------------------------------------------------------------

/// The SSO-OIDC base URL for `region` (`AGENTD_SSO_OIDC` overrides it for tests).
fn oidc_base(region: &str) -> String {
    std::env::var("AGENTD_SSO_OIDC")
        .unwrap_or_else(|_| format!("https://oidc.{region}.amazonaws.com"))
}

/// The SSO portal base URL for `region` (`AGENTD_SSO_PORTAL` overrides it).
fn portal_base(region: &str) -> String {
    std::env::var("AGENTD_SSO_PORTAL")
        .unwrap_or_else(|_| format!("https://portal.sso.{region}.amazonaws.com"))
}

fn post_json<T: serde::de::DeserializeOwned>(
    url: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<T, String> {
    let (status, resp) = post_raw(url, body, timeout)?;
    if !(200..300).contains(&status) {
        return Err(format!("aws sso: {url} returned HTTP {status}"));
    }
    serde_json::from_slice(&resp).map_err(|e| format!("aws sso: bad response from {url}: {e}"))
}

fn post_raw(
    url: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<(u16, Vec<u8>), String> {
    let u = Url::parse(url).map_err(|e| format!("aws sso: url {url}: {e}"))?;
    let mut s = connect(&u, timeout)?;
    let payload = serde_json::to_vec(body).unwrap_or_default();
    let resp = http::send(
        s.as_mut(),
        &u.host_header(),
        "POST",
        &u.path,
        &[("Content-Type", "application/json")],
        &payload,
    )
    .map_err(|e| format!("aws sso: request failed: {e}"))?;
    Ok((resp.status, resp.body))
}

fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<T, String> {
    let u = Url::parse(url).map_err(|e| format!("aws sso: url {url}: {e}"))?;
    let mut s = connect(&u, timeout)?;
    // `Url::path` already includes the query string.
    let mut hs = vec![("Accept", "application/json")];
    hs.extend_from_slice(headers);
    let resp = http::send(s.as_mut(), &u.host_header(), "GET", &u.path, &hs, &[])
        .map_err(|e| format!("aws sso: request failed: {e}"))?;
    if !resp.is_success() {
        return Err(format!("aws sso: {url} returned HTTP {}", resp.status));
    }
    serde_json::from_slice(&resp.body).map_err(|e| format!("aws sso: bad response: {e}"))
}

#[derive(Deserialize)]
struct OidcError {
    error: String,
}
fn oauth_error(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<OidcError>(body)
        .ok()
        .map(|e| e.error)
}

fn connect(url: &Url, timeout: Duration) -> Result<Box<dyn http::Stream>, String> {
    let tcp = http::connect_tcp(&url.host, url.port, timeout)
        .map_err(|e| format!("aws sso: connect {}: {e}", url.host))?;
    if url.is_tls() {
        #[cfg(feature = "tls")]
        {
            let s = crate::net::tls::connect(tcp, &url.host, None)
                .map_err(|e| format!("aws sso: tls {}: {e}", url.host))?;
            Ok(Box::new(s))
        }
        #[cfg(not(feature = "tls"))]
        {
            Err("aws sso: https requires building with --features tls".to_string())
        }
    } else {
        Ok(Box::new(tcp))
    }
}

fn pct(s: &str) -> String {
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
