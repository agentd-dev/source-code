// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd login <target>` — the interactive device-authorization flow
//! (RFC 0031 §12). Resolves a configured endpoint's `auth:` block, runs the
//! RFC 8628 device grant (print a URL + short code, poll until the human
//! authorizes), and writes the obtained token to the per-user file cache the
//! daemon reads at startup.
//!
//! `<target>` is `mcp:<name>` (an MCP server) or `intelligence`. The flow is
//! fail-closed: a token is only cached on success; nothing is printed but the
//! code + URL (never the token).

use std::time::{Duration, Instant};

use crate::auth::cache::{self, CachedCred};
use crate::auth::oauth2::{self, DeviceAuth, OAuth2Params, PollOutcome};
use crate::config::v2::{Auth, AuthKind, OAuthGrant, Settings};

/// The user-facing device prompt (RFC 8628 §3.2) handed to the printer.
pub struct DevicePrompt<'a> {
    pub verification_uri: &'a str,
    pub verification_uri_complete: Option<&'a str>,
    pub user_code: &'a str,
    pub expires_in: Option<u64>,
}

/// Find a target's `auth:` block in the settings. `mcp:<name>` → that server;
/// `intelligence` → the intelligence block.
pub fn resolve_auth<'a>(settings: &'a Settings, target: &str) -> Result<&'a Auth, String> {
    if target == "intelligence" {
        return settings
            .intelligence
            .auth
            .as_ref()
            .ok_or_else(|| "intelligence has no `auth:` block to log in with".to_string());
    }
    if let Some(name) = target.strip_prefix("mcp:") {
        let s = settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("no mcp server named '{name}' in the config"))?;
        return s
            .auth
            .as_ref()
            .ok_or_else(|| format!("mcp server '{name}' has no `auth:` block to log in with"));
    }
    // RFC 0037: a service-catalog entry's shared credential.
    if let Some(name) = target.strip_prefix("service:") {
        let e = settings
            .services
            .get(name)
            .ok_or_else(|| format!("no service named '{name}' in the services: catalog"))?;
        return e
            .auth
            .as_ref()
            .ok_or_else(|| format!("services.{name} has no `auth:` block to log in with"));
    }
    Err(format!(
        "unknown login target '{target}' (expected `intelligence`, `mcp:<name>` or `service:<name>`)"
    ))
}

pub use super::canonical_target;

/// Build the OAuth2 request params from a config `auth:` block, discovering the
/// token / device endpoints from the `issuer` when they are not pinned.
/// `resource` (the endpoint being logged into) enables RFC 9728 issuer discovery
/// when no `issuer` is configured: the server's `401 WWW-Authenticate` challenge
/// names its authorization server.
pub fn params_from_auth(
    auth: &Auth,
    resource: Option<&str>,
    timeout: Duration,
) -> Result<OAuth2Params, String> {
    if auth.kind != AuthKind::Oauth2 {
        return Err("login requires an `auth: { kind: oauth2 }` block".into());
    }
    if matches!(auth.grant, Some(OAuthGrant::ClientCredentials)) {
        return Err("client_credentials is non-interactive — no login needed".into());
    }
    let client_id = auth
        .client_id
        .clone()
        .ok_or("auth.client_id is required for oauth2")?;

    let mut token_url = auth.token_url.clone();
    let mut device_url = auth.device_authorization_url.clone();
    let mut auth_url = auth.authorization_url.clone();
    let mut issuer = auth.issuer.clone();
    // RFC 9728: with no issuer AND no pinned endpoints, ask the resource — its
    // 401 challenge (or well-known metadata) names the authorization server.
    if issuer.is_none()
        && token_url.is_none()
        && device_url.is_none()
        && let Some(res) = resource
        && let Some(found) = crate::auth::challenge::discover_issuer(res, timeout)
    {
        issuer = Some(found);
    }
    // Discover the endpoints from the issuer if any are missing.
    if (token_url.is_none() || device_url.is_none() || auth_url.is_none())
        && let Some(issuer) = &issuer
    {
        let d = oauth2::discover(issuer, timeout)?;
        token_url = token_url.or(d.token_endpoint);
        device_url = device_url.or(d.device_authorization_endpoint);
        auth_url = auth_url.or(d.authorization_endpoint);
    }

    Ok(OAuth2Params {
        token_url: token_url
            .ok_or("auth.token_url is required (or set auth.issuer for discovery)")?,
        device_authorization_url: device_url,
        authorization_url: auth_url,
        client_id,
        client_secret: auth.client_secret.as_ref().map(|s| s.0.clone()),
        scopes: auth.scopes.clone(),
        audience: auth.audience.clone(),
    })
}

/// Run the device-authorization flow: print the prompt, then poll to a token.
/// `sleep` is injected so tests drive the poll loop without real delay.
pub fn device_login(
    params: &OAuth2Params,
    timeout: Duration,
    on_prompt: impl FnOnce(&DevicePrompt),
    sleep: impl Fn(Duration),
) -> Result<CachedCred, String> {
    let da: DeviceAuth = oauth2::start_device(params, timeout)?;
    on_prompt(&DevicePrompt {
        verification_uri: &da.verification_uri,
        verification_uri_complete: da.verification_uri_complete.as_deref(),
        user_code: &da.user_code,
        expires_in: da.expires_in,
    });

    let mut interval = Duration::from_secs(da.interval.max(1));
    let deadline =
        Instant::now() + Duration::from_secs(da.expires_in.unwrap_or(900).clamp(30, 1800));
    loop {
        match oauth2::poll_device_once(params, &da.device_code, timeout)? {
            PollOutcome::Token(t) => return Ok(tokens_to_cred(&t)),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += Duration::from_secs(5),
        }
        if Instant::now() + interval >= deadline {
            return Err("device code expired before authorization".into());
        }
        sleep(interval);
    }
}

/// Turn a token response into a cache record, stamping the absolute expiry.
pub fn tokens_to_cred(t: &oauth2::Tokens) -> CachedCred {
    let expires_at_ms = t
        .expires_in
        .map(|s| cache::now_ms() + s.saturating_mul(1000))
        .unwrap_or(0);
    CachedCred {
        access_token: t.access_token.clone(),
        refresh_token: t.refresh_token.clone(),
        expires_at_ms,
        token_type: t.token_type.clone(),
        extra: Default::default(),
    }
}

/// The CLI entry point (`agentd --login <target>`): resolve, run the device flow
/// with a stderr prompt + real sleep, and cache the token. Returns a one-line
/// success message for stdout (never the token).
pub fn run_cli(settings: &Settings, target: &str, timeout: Duration) -> Result<String, String> {
    let target = &canonical_target(settings, target);
    let auth = resolve_auth(settings, target)?;
    // AWS IAM Identity Center (SSO): a distinct device flow → temporary AWS creds.
    if auth.kind == AuthKind::Aws {
        if auth.source.as_deref() != Some("sso") {
            return Err(
                "aws login is only for `source: sso` (env/static credentials need no login)".into(),
            );
        }
        let p = crate::auth::aws_sso::SsoParams {
            region: auth.region.clone().ok_or("aws sso: `region` is required")?,
            start_url: auth
                .sso_start_url
                .clone()
                .ok_or("aws sso: `sso_start_url` is required")?,
            account_id: auth
                .account_id
                .clone()
                .ok_or("aws sso: `account_id` is required")?,
            role_name: auth
                .role_name
                .clone()
                .ok_or("aws sso: `role_name` is required")?,
        };
        let cred = crate::auth::aws_sso::sso_login(
            &p,
            timeout,
            |pr| print_prompt(target, pr),
            std::thread::sleep,
        )?;
        let dir = cache::default_dir();
        cache::store_file(&dir, target, &cred)?;
        return Ok(format!(
            "logged in to {target} (AWS SSO) — temporary credentials cached in {}",
            dir.display()
        ));
    }
    // The resource being logged into (RFC 9728 issuer discovery when no issuer
    // is configured): the MCP server's endpoint, or the primary intel endpoint.
    let resource: Option<String> = if target == "intelligence" {
        settings.intelligence.endpoints.first().cloned()
    } else if let Some(name) = target.strip_prefix("mcp:") {
        settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.endpoint.clone())
    } else if let Some(name) = target.strip_prefix("service:") {
        settings.services.get(name).map(|e| e.endpoint.clone())
    } else {
        None
    };
    let params = params_from_auth(auth, resource.as_deref(), timeout)?;
    // `grant: authorization_code` → the browser + PKCE loopback flow; otherwise
    // the device grant (the default).
    let cred = if auth.grant == Some(OAuthGrant::AuthorizationCode) {
        crate::auth::browser::browser_login(
            &params,
            |url| print_browser_prompt(target, url),
            timeout,
        )?
    } else {
        device_login(
            &params,
            timeout,
            |p| print_prompt(target, p),
            std::thread::sleep,
        )?
    };
    let dir = cache::default_dir();
    cache::store_file(&dir, target, &cred)?;
    Ok(format!(
        "logged in to {target} — token cached in {}",
        dir.display()
    ))
}

/// Print the browser-flow prompt to stderr (the authorization URL to open).
fn print_browser_prompt(target: &str, url: &str) {
    eprintln!("┌─ authorize agentd ──────────────────────────────");
    eprintln!("│  target   {target}");
    eprintln!("│  open     {url}");
    eprintln!("│  waiting for the browser redirect… (Ctrl-C to cancel)");
    eprintln!("└──────────────────────────────────────────────────");
}

/// Print the device prompt to stderr (a box with the URL + code; never the
/// token). Stdout stays reserved for the machine-readable success line.
fn print_prompt(target: &str, p: &DevicePrompt) {
    let uri = p.verification_uri_complete.unwrap_or(p.verification_uri);
    eprintln!("┌─ authorize agentd ──────────────────────────────");
    eprintln!("│  target   {target}");
    eprintln!("│  visit    {uri}");
    eprintln!("│  code     {}", p.user_code);
    match p.expires_in {
        Some(s) => eprintln!("│  waiting… (expires in {}s · Ctrl-C to cancel)", s),
        None => eprintln!("│  waiting… (Ctrl-C to cancel)"),
    }
    eprintln!("└──────────────────────────────────────────────────");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::v2::Secret;

    fn oauth_block() -> Auth {
        Auth {
            kind: AuthKind::Oauth2,
            issuer: None,
            token_url: Some("https://as.example/token".into()),
            device_authorization_url: Some("https://as.example/device".into()),
            authorization_url: None,
            client_id: Some("agentd".into()),
            client_secret: None,
            grant: Some(OAuthGrant::Device),
            scopes: vec!["mcp:read".into()],
            audience: None,
            token: None,
            header: None,
            value: None,
            region: None,
            service: None,
            source: None,
            sso_start_url: None,
            account_id: None,
            role_name: None,
            svid: None,
            jwt_svid_file: None,
            svid_file: None,
            key_file: None,
        }
    }

    #[test]
    fn params_from_auth_maps_the_block() {
        let p = params_from_auth(&oauth_block(), None, Duration::from_secs(5)).unwrap();
        assert_eq!(p.token_url, "https://as.example/token");
        assert_eq!(
            p.device_authorization_url.as_deref(),
            Some("https://as.example/device")
        );
        assert_eq!(p.client_id, "agentd");
        assert_eq!(p.scopes, vec!["mcp:read".to_string()]);
    }

    #[test]
    fn params_from_auth_rejects_non_oauth_and_client_creds() {
        let mut a = oauth_block();
        a.kind = AuthKind::Static;
        assert!(params_from_auth(&a, None, Duration::from_secs(1)).is_err());
        let mut a = oauth_block();
        a.grant = Some(OAuthGrant::ClientCredentials);
        assert!(params_from_auth(&a, None, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn tokens_to_cred_stamps_absolute_expiry() {
        let t = oauth2::Tokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
            scope: None,
        };
        let before = cache::now_ms();
        let c = tokens_to_cred(&t);
        assert_eq!(c.access_token, "at");
        assert_eq!(c.refresh_token.as_deref(), Some("rt"));
        assert!(c.expires_at_ms >= before + 3_600_000);
        // A secret block carries a confidential client's secret only via the template.
        let _ = Secret("x".into());
    }
}
