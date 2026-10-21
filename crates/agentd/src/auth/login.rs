// SPDX-License-Identifier: AGPL-3.0-only
//! `agentd --login <target>` — the interactive login flow. Resolves a configured
//! endpoint's `auth:` block, runs the RFC 8628 device grant (print a URL and a
//! short code, poll until the human authorizes), and writes the obtained token
//! to the per-user file cache the daemon reads at startup.
//!
//! `<target>` is `mcp:<name>` (an MCP server), `service:<name>` (a catalog
//! entry shared by several servers), `intelligence`, or
//! `intelligence:<provider>` (a named intelligence connection). The flow is
//! fail-closed: a token is cached only on success, and nothing is printed but
//! the code and URL — never the token itself.

use std::time::{Duration, Instant};

use crate::auth::cache::{self, CachedCred};
use crate::auth::oauth2::{self, DeviceAuth, OAuth2Params, PollOutcome};
use crate::config::AuthSpec;
use crate::config::settings::{Auth, AuthKind, OAuthGrant, Settings};

/// The user-facing device prompt (RFC 8628 §3.2) handed to the printer.
pub struct DevicePrompt<'a> {
    pub verification_uri: &'a str,
    pub verification_uri_complete: Option<&'a str>,
    pub user_code: &'a str,
    pub expires_in: Option<u64>,
}

/// Find a target's `auth:` block in the settings. `mcp:<name>` → that server;
/// `intelligence` → the top-level intelligence connection;
/// `intelligence:<name>` → that provider.
pub fn resolve_auth<'a>(settings: &'a Settings, target: &str) -> Result<&'a Auth, String> {
    if let Some(c) = intelligence_connection(settings, target)? {
        return c
            .auth
            .ok_or_else(|| format!("{} has no `auth:` block to log in with", c.at()));
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
    // A service-catalog entry holds one credential shared by every server
    // that references it, so it is logged into once under its own name.
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
        "unknown login target '{target}' (expected `intelligence`, `intelligence:<provider>`, `mcp:<name>` or `service:<name>`)"
    ))
}

/// The intelligence connection a target names — `intelligence`, or
/// `intelligence:<provider>` — or `None` for a target of another kind.
fn intelligence_connection<'a>(
    settings: &'a Settings,
    target: &str,
) -> Result<Option<crate::config::settings::Connection<'a>>, String> {
    let provider = match target.strip_prefix("intelligence") {
        Some("") => None,
        Some(rest) => match rest.strip_prefix(':') {
            Some(name) => Some(name),
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    settings
        .intelligence
        .connection(provider)
        .map(Some)
        .ok_or_else(|| {
            format!(
                "no provider named '{}' under intelligence.providers",
                provider.unwrap_or_default()
            )
        })
}

pub use super::canonical_target;

/// Build the OAuth2 request params of an interactive login from an `auth:`
/// block's runtime spec ([`Settings::auth_spec_at`]), discovering whichever
/// of the token, device and authorization endpoints are not pinned from the
/// `issuer` — or, on an MCP server's block that names none, from the
/// authorization server the server's RFC 9728 metadata names. A spec whose
/// client is agentd's Client ID Metadata Document always reads the issuer's
/// metadata, and is refused where it does not advertise the document.
pub fn params_from_auth(auth: &AuthSpec, timeout: Duration) -> Result<OAuth2Params, String> {
    if auth.kind != "oauth2" {
        return Err("login requires an `auth: { kind: oauth2 }` block".into());
    }
    if auth.grant.as_deref() == Some("client_credentials") {
        return Err("client_credentials is non-interactive — no login needed".into());
    }
    resolve_params(auth, !pins_every_endpoint(auth), timeout)
}

/// The OAuth2 request params of `auth`, its authorization server's metadata
/// read when `discover` (an endpoint the caller needs is not pinned) or the
/// client is agentd's Client ID Metadata Document. The issuer is the block's,
/// or the one the MCP server's RFC 9728 metadata names when the block names
/// no `issuer` and no `token_url`: a server-chosen issuer is reached as
/// [`challenge::reach`] allows, and its metadata must keep its endpoints on
/// its own origin.
///
/// [`challenge::reach`]: crate::auth::challenge::reach
pub(crate) fn resolve_params(
    auth: &AuthSpec,
    discover: bool,
    timeout: Duration,
) -> Result<OAuth2Params, String> {
    let client_id = auth
        .client_id
        .clone()
        .ok_or("auth.client_id is required for oauth2")?;
    let mut token_url = auth.token_url.clone();
    let mut device_url = auth.device_authorization_url.clone();
    let mut auth_url = auth.authorization_url.clone();
    let mut issuer = auth.issuer.clone();
    let (mut revocation_url, mut iss_supported, mut pkce_s256) = (None, false, None);
    let mut dial = crate::auth::http::Dial::Named;
    let mut named = false;
    // RFC 9728: with no issuer and no token endpoint, ask the MCP server — its
    // 401 challenge (or well-known metadata) names the authorization server.
    if issuer.is_none()
        && token_url.is_none()
        && let Some(res) = &auth.protected_resource
        && let Some((found, how)) =
            crate::auth::challenge::discover_issuer(res, auth.catalog.as_deref(), timeout)?
    {
        issuer = Some(found);
        dial = how;
        named = true;
    }
    if discover || named || auth.client_id_metadata_document {
        match &issuer {
            Some(issuer) => {
                let d = oauth2::discover_by(issuer, timeout, dial)?;
                if named {
                    d.endpoints_on_origin(issuer)?;
                }
                if auth.client_id_metadata_document {
                    d.admits_document(issuer, &client_id)?;
                }
                pkce_s256 = Some(d.pkce_s256());
                token_url = token_url.or(d.token_endpoint);
                device_url = device_url.or(d.device_authorization_endpoint);
                auth_url = auth_url.or(d.authorization_endpoint);
                revocation_url = d.revocation_endpoint;
                iss_supported = d.authorization_response_iss_parameter_supported;
            }
            None if auth.client_id_metadata_document => {
                return Err(
                    "auth.issuer is required to use agentd's Client ID Metadata Document: its metadata says whether the authorization server fetches one".into(),
                );
            }
            None => {}
        }
    }
    Ok(OAuth2Params {
        token_url: token_url.ok_or(
            "auth.token_url is required (or set auth.issuer, or let the MCP server's RFC 9728 metadata name its authorization server)",
        )?,
        device_authorization_url: device_url,
        authorization_url: auth_url,
        revocation_url,
        issuer,
        iss_supported,
        client_id,
        client_id_metadata_document: auth.client_id_metadata_document,
        redirect_uri: None,
        client_secret: auth.client_secret.clone(),
        client_auth: oauth2::ClientAuth::of(auth.client_auth.as_ref())?,
        scopes: auth.scopes.clone(),
        audience: auth.audience.clone(),
        resource: auth.resource.clone(),
        pkce_s256,
        dial,
        prompt_only: false,
    })
}

/// Whether `auth` pins the token, device and authorization endpoints, so
/// [`params_from_auth`] asks no issuer for its metadata.
pub(crate) fn pins_every_endpoint(auth: &AuthSpec) -> bool {
    auth.token_url.is_some()
        && auth.device_authorization_url.is_some()
        && auth.authorization_url.is_some()
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
        // A `Retry-After` the token endpoint gave holds the next poll back at
        // least that long, for this poll only, and five minutes at most.
        let mut wait = interval;
        match oauth2::poll_device_once(params, &da.device_code, timeout)? {
            PollOutcome::Token(t) => return Ok(tokens_to_cred(&t)),
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => {
                interval = interval.saturating_add(Duration::from_secs(5));
                wait = interval;
            }
            PollOutcome::Wait(asked) => {
                wait = wait.max(asked.min(crate::net::http::RETRY_AFTER_MAX));
            }
        }
        // Compared, not added to an instant: a server's wait can be any size.
        if wait >= deadline.saturating_duration_since(Instant::now()) {
            return Err("device code expired before authorization".into());
        }
        sleep(wait);
    }
}

/// The cache record of a refresh: the response's tokens, and — when it
/// carries no `refresh_token` — the one `presented`, which stays good (RFC
/// 6749 §6). Without that, a server that does not rotate would leave the
/// record with nothing to renew from once its access token expired.
pub fn refreshed_cred(t: &oauth2::Tokens, presented: &str) -> CachedCred {
    let mut cred = tokens_to_cred(t);
    if cred.refresh_token.is_none() {
        cred.refresh_token = Some(presented.to_string());
    }
    cred
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
        authorization_server: None,
        scope: t.scope.clone(),
        step_up: Vec::new(),
        extra: Default::default(),
    }
}

/// The CLI entry point (`agentd --login <target>`): resolve, run the device flow
/// with a stderr prompt + real sleep, and cache the token. Returns a one-line
/// success message for stdout (never the token).
pub fn run_cli(settings: &Settings, target: &str, timeout: Duration) -> Result<String, String> {
    let target = &canonical_target(settings, target);
    let auth = resolve_auth(settings, target)?;
    // A delegated server holds no login of its own: it is reached with the
    // enrolled person's grant (RFC 0044), which the running daemon keeps.
    if matches!(
        auth.grant,
        Some(OAuthGrant::IdJag | OAuthGrant::TokenExchange)
    ) {
        return Err(format!(
            "{target} is reached with the enrolled person's grant, not a login of its own: run `agentd --login idp:{}`",
            auth.idp.as_deref().unwrap_or("<name>")
        ));
    }
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
        let _held = cache::lock_file(&dir, target, timeout)?;
        cache::store_file(&dir, target, &cred)?;
        return Ok(format!(
            "logged in to {target} (AWS SSO) — temporary credentials cached in {}",
            dir.display()
        ));
    }
    let spec = spec_of(settings, target, auth)?;
    let mut params = params_from_auth(&spec, timeout)?;
    let dir = cache::default_dir();
    // The scopes a server asked for since the last login, which its renewal
    // could not get (MCP's step-up flow): this login asks for them too.
    if let Some(held) = cache::load_file(&dir, target) {
        for s in held.step_up {
            if !params.scopes.contains(&s) {
                params.scopes.push(s);
            }
        }
    }
    // The document's client returns to the redirect URI it registers.
    if spec.client_id_metadata_document {
        params.redirect_uri = settings
            .security
            .client
            .as_ref()
            .and_then(|c| c.redirect_uris.first().cloned());
    }
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
    // Kept with the authorization server it came from: it is never
    // presented to another.
    let cred = CachedCred {
        authorization_server: Some(params.authorization_server().to_string()),
        ..cred
    };
    let _held = cache::lock_file(&dir, target, timeout)?;
    cache::store_file(&dir, target, &cred)?;
    Ok(format!(
        "logged in to {target} — token cached in {}",
        dir.display()
    ))
}

/// The runtime spec of `target`'s `auth:` block, as the daemon builds it
/// for the endpoint the target names ([`Settings::auth_spec_at`]): an MCP
/// server's, an MCP catalog entry's, a peer or intelligence entry's, or the
/// primary intelligence endpoint.
pub fn spec_of(settings: &Settings, target: &str, auth: &Auth) -> Result<AuthSpec, String> {
    let (endpoint, mcp) = if let Some(c) = intelligence_connection(settings, target)? {
        (c.endpoints.first().cloned().unwrap_or_default(), false)
    } else if let Some(name) = target.strip_prefix("mcp:") {
        let s = settings.mcp.servers.iter().find(|s| s.name == name);
        (s.map(|s| s.endpoint.clone()).unwrap_or_default(), true)
    } else if let Some(name) = target.strip_prefix("service:") {
        let e = settings.services.get(name);
        (
            e.map(|e| e.endpoint.clone()).unwrap_or_default(),
            e.is_some_and(|e| e.kind == crate::config::settings::ServiceKind::Mcp),
        )
    } else {
        (String::new(), false)
    };
    Ok(settings.auth_spec_at(auth, &endpoint, mcp))
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
    use crate::config::settings::Secret;

    fn oauth_block() -> Auth {
        Auth {
            kind: AuthKind::Oauth2,
            issuer: None,
            token_url: Some("https://as.example/token".into()),
            device_authorization_url: Some("https://as.example/device".into()),
            authorization_url: None,
            client_id: Some("agentd".into()),
            client_secret: None,
            client_auth: None,
            grant: Some(OAuthGrant::Device),
            scopes: vec!["mcp:read".into()],
            audience: None,
            resource: None,
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
            idp: None,
            tool_scopes: Default::default(),
            scope_mode: None,
            tools_from: None,
            tools_file: None,
            id_jag: None,
            id_jag_reuse: None,
            on_revoked: None,
            allow_unbound_at: None,
            attribution: None,
            max_at_age: None,
            trusted_output: None,
            subject: None,
        }
    }

    /// A device login whose device code asks for polls `interval` seconds
    /// apart, and whose first poll the token endpoint answers `429` with
    /// `Retry-After: retry_after`: the waits it slept between polls, or how
    /// it failed.
    fn device_login_told_to_wait(
        interval: &'static str,
        retry_after: &'static str,
    ) -> Result<Vec<Duration>, String> {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for (i, c) in l.incoming().flatten().enumerate() {
                let mut r = BufReader::new(c.try_clone().unwrap());
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let (status, extra, body) = match i {
                    0 => (
                        "200 OK",
                        "",
                        format!(
                            r#"{{"device_code":"d","user_code":"U","verification_uri":"https://as/device","interval":{interval}}}"#
                        ),
                    ),
                    1 => ("429 Too Many Requests", retry_after, String::new()),
                    _ => ("200 OK", "", r#"{"access_token":"at"}"#.to_string()),
                };
                let extra = match extra {
                    "" => String::new(),
                    v => format!("Retry-After: {v}\r\n"),
                };
                let mut w = c;
                let _ = write!(
                    w,
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let params = OAuth2Params {
            token_url: format!("{base}/token"),
            device_authorization_url: Some(format!("{base}/device")),
            client_id: "c".into(),
            ..Default::default()
        };
        let slept = std::sync::Mutex::new(Vec::new());
        let cred = device_login(
            &params,
            Duration::from_secs(5),
            |_| {},
            |d| slept.lock().unwrap().push(d),
        )?;
        assert_eq!(cred.access_token, "at");
        Ok(slept.into_inner().unwrap())
    }

    /// A device poll the token endpoint answers `429` with `Retry-After` is
    /// followed by the next poll no sooner than the wait it asked for, where
    /// the interval alone is a second — and no later than five minutes, the
    /// most any server's wait is taken for, however large the one it sent.
    /// An interval that outlasts the device code ends the login, as the
    /// code's expiry does.
    #[test]
    fn a_device_poll_waits_out_a_retry_after() {
        let max = crate::net::http::RETRY_AFTER_MAX;
        assert_eq!(
            device_login_told_to_wait("1", "7"),
            Ok(vec![Duration::from_secs(7)])
        );
        assert_eq!(device_login_told_to_wait("1", "600"), Ok(vec![max]));
        assert_eq!(
            device_login_told_to_wait("1", "99999999999999999999"),
            Ok(vec![max])
        );
        assert_eq!(
            device_login_told_to_wait("18446744073709551615", "1"),
            Err("device code expired before authorization".to_string())
        );
    }

    #[test]
    fn params_from_auth_maps_the_block() {
        let p = params_from_auth(&oauth_block().to_spec(), Duration::from_secs(5)).unwrap();
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
        assert!(params_from_auth(&a.to_spec(), Duration::from_secs(1)).is_err());
        let mut a = oauth_block();
        a.grant = Some(OAuthGrant::ClientCredentials);
        assert!(params_from_auth(&a.to_spec(), Duration::from_secs(1)).is_err());
    }

    /// A refresh that rotates replaces the refresh token; one that does not
    /// keeps the token it presented.
    #[test]
    fn a_refresh_without_a_refresh_token_keeps_the_one_presented() {
        let mut t = oauth2::Tokens {
            access_token: "at-2".into(),
            refresh_token: Some("rt-2".into()),
            expires_in: Some(60),
            token_type: None,
            scope: None,
            id_token: None,
        };
        assert_eq!(
            refreshed_cred(&t, "rt-1").refresh_token.as_deref(),
            Some("rt-2")
        );
        t.refresh_token = None;
        let c = refreshed_cred(&t, "rt-1");
        assert_eq!(
            (c.access_token.as_str(), c.refresh_token.as_deref()),
            ("at-2", Some("rt-1"))
        );
    }

    #[test]
    fn tokens_to_cred_stamps_absolute_expiry() {
        let t = oauth2::Tokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_in: Some(3600),
            token_type: Some("Bearer".into()),
            scope: None,
            id_token: None,
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
