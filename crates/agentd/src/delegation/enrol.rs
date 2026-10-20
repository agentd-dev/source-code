// SPDX-License-Identifier: AGPL-3.0-only
//! Enrolment (RFC 0044 §5.4, F1): the per-person operator signs in at their
//! IdP, and the supervisor — never the CLI, so the refresh token exists in
//! no other process — verifies the ID token, binds its `iss` and `sub` to the
//! operator's exact `idp_subject` pin, and seals the refresh token.
//!
//! 1. **A template must admit the person at the IdP** before anything is
//!    sent: no template, `delegation_not_permitted`.
//! 2. **The sign-in**, by the first of the provider's `sign_in.grants` the
//!    IdP's metadata supports:
//!    - `device` — the IdP's device grant: agentd shows `verification_uri`
//!      and `user_code`, never `verification_uri_complete`. Allowed because
//!      the person is pinned, so a code forwarded to someone else fails the
//!      binding below.
//!    - `authorization_code` — PKCE (S256, refused without it), `state`,
//!      `nonce`, and RFC 9207's `iss` decided on the callback. The
//!      supervisor holds a loopback callback for the sign-in's life, and the
//!      client is given a one-time start link on it, valid 60 seconds:
//!      opening it sets a `__Host-` cookie bound to the `state` and
//!      redirects to the IdP, and the callback is taken only with that
//!      cookie. The IdP's authorization URL is never handed to a client, so
//!      one forwarded to someone else fails at the callback. A sign-in that
//!      answers a step-up asks for `max_age` and `prompt=login`.
//! 3. **The ID token** is verified with the IdP's keys: `iss` the configured
//!    issuer, `aud` agentd's client, `exp` and `iat`, and the `nonce` the
//!    code flow sent.
//! 4. **The binding**: its `iss` + `sub` must equal the pin, or nothing is
//!    kept (`binding_mismatch`). Globs and email binds do not exist.
//! 5. **The grant** is stored, sealed, and `delegation.enrolled` names the
//!    approving `sub`. Then each server the template allows whose tool
//!    catalogue is empty is listed with a delegated token — one mint each,
//!    the only mint that is not a call's.
//!
//! A sign-in for a grant that exists renews it: the same identity, a new
//! refresh token one version up — what a step-up does.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use super::{Code, Delegation, DelegationError, refuse};
use crate::auth::browser::{check_iss, pct, pct_decode};
use crate::auth::idjag::{self, IdpMeta};
use crate::auth::oauth2::{self, OAuth2Params, PollOutcome, Tokens};
use crate::config::settings::delegation::{SignInGrant, ToolsFrom};
use crate::state::now_ms;

/// How long a sign-in may take, end to end.
pub const SIGN_IN_FOR: Duration = Duration::from_secs(10 * 60);
/// How long a start link stays good.
const START_LINK_FOR: Duration = Duration::from_secs(60);
/// The cookie that binds a browser to the sign-in it started.
const COOKIE: &str = "__Host-agentd-enrol";

/// What a sign-in says while it runs, and how it ends — or how a read ends:
/// what it says, and the document it answers with.
#[derive(Debug)]
pub enum Said {
    Text(String),
    Done(Result<String, String>),
    Answered(String, serde_json::Value),
}

/// Where a sign-in says it.
pub type Say = Arc<dyn Fn(Said) + Send + Sync>;

/// Start the person's sign-in at `idp`, by `grant` or the provider's first
/// that the IdP supports, on a thread of its own.
pub fn start(
    d: Arc<Delegation>,
    idp: &str,
    grant: Option<SignInGrant>,
    say: Say,
) -> Result<(), String> {
    let person = d
        .person
        .clone()
        .ok_or("no a2a.principals rule with match.uid and idp_subject declares the per-person operator (principal_not_delegable)")?;
    if !d.providers.contains_key(idp) {
        return Err(format!("{idp:?} is not an identity.providers entry"));
    }
    if d.template_for(idp).is_none() {
        return Err(format!(
            "no identity.delegation.templates entry admits {} at {idp} (delegation_not_permitted)",
            person.id
        ));
    }
    let idp = idp.to_string();
    std::thread::Builder::new()
        .name(format!("delegation-enrol:{idp}"))
        .spawn(move || {
            let end = run(&d, &idp, grant, &say);
            say(Said::Done(end.map_err(|e| e.to_string())));
        })
        .map(|_| ())
        .map_err(|e| format!("no thread for the sign-in: {e}"))
}

fn run(
    d: &Arc<Delegation>,
    name: &str,
    grant: Option<SignInGrant>,
    say: &Say,
) -> Result<String, DelegationError> {
    let idp = d
        .providers
        .get(name)
        .ok_or_else(|| refuse(Code::Config, format!("no IdP {name}")))?;
    let meta = idp
        .meta(d.timeout)
        .map_err(|e| refuse(Code::IdpUnavailable, format!("{name}: {e}")))?;
    if meta.lists_id_jag == Some(false) {
        d.log.warn(
            "delegation.idp.no_id_jag",
            json!({"idp": name, "note": "its metadata lists identity-chaining token types without the ID-JAG"}),
        );
    }
    let params = idp.params(&meta).map_err(|e| refuse(Code::Config, e))?;
    let order = match grant {
        Some(g) => vec![g],
        None => idp.cfg.grants(),
    };
    let chosen = order
        .into_iter()
        .find(|g| match g {
            SignInGrant::Device => meta.device_authorization_endpoint.is_some(),
            SignInGrant::AuthorizationCode => meta.authorization_endpoint.is_some(),
        })
        .ok_or_else(|| {
            refuse(
                Code::Config,
                format!("{name}'s metadata offers none of the sign-in grants identity.providers.{name}.sign_in names"),
            )
        })?;
    let step_up = d
        .person
        .as_ref()
        .map(|p| p.typed.grant_id(&idp.cfg.issuer))
        .and_then(|g| d.with_live(&g, |l| l.step_up));
    let (tokens, nonce) = match chosen {
        SignInGrant::Device => (device(&params, say, d.timeout)?, None),
        SignInGrant::AuthorizationCode => {
            let (t, n) = code(&params, &meta, step_up, say, d.timeout)?;
            (t, Some(n))
        }
    };
    let id_token = tokens.id_token.clone().ok_or_else(|| {
        refuse(
            Code::Config,
            format!("{name} issued no ID token: sign_in.scopes needs openid"),
        )
    })?;
    let jwks_uri = meta.jwks_uri.clone().ok_or_else(|| {
        refuse(
            Code::Config,
            format!("{name} names no jwks_uri: set identity.providers.{name}.jwks_uri"),
        )
    })?;
    let jwks = idjag::fetch_jwks(&jwks_uri, d.timeout)
        .map_err(|e| refuse(Code::IdpUnavailable, format!("{name}: {e}")))?;
    let claims = idjag::verify_id_token(
        &id_token,
        &jwks,
        &idp.cfg.issuer,
        &idp.cfg.client_id,
        nonce.as_deref(),
        now_ms() / 1000,
    )
    .map_err(|e| refuse(Code::Config, format!("{name}: {e}")))?;
    let rt = tokens.refresh_token.clone().ok_or_else(|| {
        refuse(
            Code::Config,
            format!("{name} issued no refresh token: sign_in.scopes needs offline_access, and the app the refresh_token grant"),
        )
    })?;
    let g = d.enrolled(idp, &claims, &rt, &id_token).inspect_err(|e| {
        if e.code == Code::BindingMismatch {
            d.audit(
                "delegation.denied",
                "denied",
                d.person.as_ref().map(|p| p.id.as_str()),
                json!({"code": e.code.as_str(), "error": e.message, "idp": name, "approving_sub": claims.sub}),
            );
        }
    })?;
    d.audit(
        "delegation.enrolled",
        "enrolled",
        Some(&g.principal.display()),
        json!({"grant_id": g.grant_id, "principal": g.principal.display(), "idp": name,
               "approving_sub": claims.sub, "template": g.template, "servers": g.ceilings,
               "expires_ms": g.expires_ms, "grant": chosen.as_str(), "step_up": step_up}),
    );
    let mut listed = Vec::new();
    for server in g.ceilings.keys() {
        let empty = d
            .servers
            .get(server)
            .is_some_and(|s| s.tools_from == ToolsFrom::FirstGrant)
            && d.catalogue(server).is_none();
        if empty {
            match d.fill_catalogue(server) {
                Ok(n) => listed.push(format!("{server} ({n} tools)")),
                Err(e) => listed.push(format!("{server} (not listed: {e})")),
            }
        }
    }
    let until = crate::delegation::vault::days(g.expires_ms).0;
    let mut out = format!(
        "{} is enrolled at {name} as {} (template {}): agentd acts as them on {} until {until}",
        g.principal.display(),
        g.sub,
        g.template,
        g.ceilings.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    if !listed.is_empty() {
        out.push_str(&format!("; listed {}", listed.join(", ")));
    }
    Ok(out)
}

/// The IdP's device grant, polled until the person approves or it expires.
fn device(params: &OAuth2Params, say: &Say, timeout: Duration) -> Result<Tokens, DelegationError> {
    let start =
        oauth2::start_device(params, timeout).map_err(|e| refuse(Code::IdpUnavailable, e))?;
    let expires = Duration::from_secs(start.expires_in.unwrap_or(600)).min(SIGN_IN_FOR);
    say(Said::Text(format!(
        "Open {} and enter the code {} (it expires in {}s)",
        start.verification_uri,
        start.user_code,
        expires.as_secs()
    )));
    let deadline = Instant::now() + expires;
    let mut interval = Duration::from_secs(start.interval.max(1));
    loop {
        if Instant::now() >= deadline {
            return Err(refuse(
                Code::Config,
                "the device code expired before it was approved",
            ));
        }
        std::thread::sleep(interval.min(deadline.saturating_duration_since(Instant::now())));
        match oauth2::poll_device_once(params, &start.device_code, timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, e))?
        {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown => interval += Duration::from_secs(5),
            PollOutcome::Wait(w) => interval = interval.max(w),
            PollOutcome::Token(t) => return Ok(*t),
        }
    }
}

/// One request the loopback callback read.
struct Request {
    path: String,
    query: Vec<(String, String)>,
    cookie: Option<String>,
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut cookie = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("cookie")
        {
            cookie = v
                .split(';')
                .filter_map(|c| c.trim().split_once('='))
                .find(|(n, _)| *n == COOKIE)
                .map(|(_, v)| v.to_string());
        }
    }
    let target = line.split_whitespace().nth(1)?;
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let query = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), pct_decode(v)))
        .collect();
    Some(Request {
        path: path.to_string(),
        query,
        cookie,
    })
}

fn respond(mut stream: &TcpStream, status: &str, headers: &[(&str, String)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

/// The authorization-code sign-in through a loopback callback and a
/// one-time start link: the tokens, and the nonce the ID token must carry.
fn code(
    params: &OAuth2Params,
    meta: &IdpMeta,
    max_age: Option<u64>,
    say: &Say,
    timeout: Duration,
) -> Result<(Tokens, String), DelegationError> {
    if !meta.pkce_s256 {
        return Err(refuse(
            Code::Config,
            "the IdP's metadata does not list S256 in code_challenge_methods_supported: no authorization-code sign-in without PKCE",
        ));
    }
    let random = |n| {
        crate::sec::random::b64url_token(n).map_err(|e| {
            refuse(
                Code::Config,
                format!("no OS randomness for the sign-in: {e}"),
            )
        })
    };
    let (verifier, state, nonce, start, cookie) = (
        random(32)?,
        random(16)?,
        random(16)?,
        random(16)?,
        random(16)?,
    );
    let challenge = crate::b64::url_nopad(&crate::sha::sha256(verifier.as_bytes()));
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| refuse(Code::Config, format!("no loopback callback: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| refuse(Code::Config, e.to_string()))?
        .port();
    listener.set_nonblocking(true).ok();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let auth = meta.authorization_endpoint.as_deref().unwrap_or_default();
    let mut url = format!(
        "{auth}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        pct(&params.client_id),
        pct(&redirect),
        pct(&params.scopes.join(" ")),
        pct(&state),
        pct(&nonce),
        pct(&challenge),
    );
    if let Some(m) = max_age {
        url.push_str(&format!("&max_age={m}&prompt=login"));
    }
    say(Said::Text(format!(
        "Open http://127.0.0.1:{port}/start/{start} in a browser on this machine within {}s, and sign in",
        START_LINK_FOR.as_secs()
    )));
    let opened = Instant::now();
    let deadline = opened + SIGN_IN_FOR;
    let mut started = false;
    loop {
        if Instant::now() >= deadline {
            return Err(refuse(Code::Config, "the sign-in did not finish in time"));
        }
        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        stream.set_nonblocking(false).ok();
        let Some(req) = read_request(&stream) else {
            continue;
        };
        if req.path == format!("/start/{start}") {
            if started || opened.elapsed() > START_LINK_FOR {
                respond(
                    &stream,
                    "410 Gone",
                    &[],
                    "This sign-in link was used or has expired.",
                );
                continue;
            }
            started = true;
            respond(
                &stream,
                "302 Found",
                &[
                    ("Location", url.clone()),
                    (
                        "Set-Cookie",
                        format!("{COOKIE}={cookie}; Path=/; Secure; HttpOnly; SameSite=Lax"),
                    ),
                ],
                "",
            );
            continue;
        }
        if req.path != "/callback" {
            respond(&stream, "404 Not Found", &[], "");
            continue;
        }
        let get = |k: &str| {
            req.query
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        // Taken only by the browser the start link was opened in.
        if req.cookie.as_deref() != Some(cookie.as_str()) {
            respond(
                &stream,
                "403 Forbidden",
                &[],
                "This callback did not come from the browser that started the sign-in.",
            );
            return Err(refuse(
                Code::Config,
                "the callback came without the start link's cookie: a forwarded sign-in is refused",
            ));
        }
        if get("state").as_deref() != Some(state.as_str()) {
            respond(&stream, "400 Bad Request", &[], "state mismatch");
            return Err(refuse(
                Code::Config,
                "the callback's state is not the sign-in's",
            ));
        }
        if let Some(e) = get("error") {
            respond(
                &stream,
                "200 OK",
                &[],
                "The sign-in was refused. You may close this window.",
            );
            return Err(refuse(
                Code::Config,
                format!("the IdP refused the sign-in: {e}"),
            ));
        }
        if let Err(e) = check_iss(
            get("iss").as_deref(),
            params.issuer.as_deref(),
            meta.iss_supported,
        ) {
            respond(&stream, "400 Bad Request", &[], "issuer mismatch");
            return Err(refuse(Code::Config, e));
        }
        let Some(code) = get("code") else {
            respond(&stream, "400 Bad Request", &[], "no code");
            return Err(refuse(Code::Config, "the callback carried no code"));
        };
        respond(
            &stream,
            "200 OK",
            &[],
            "Signed in. You may close this window and return to the terminal.",
        );
        let tokens = oauth2::exchange_code(params, &code, &verifier, &redirect, timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, e))?;
        return Ok((tokens, nonce));
    }
}
