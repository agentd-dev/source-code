// SPDX-License-Identifier: AGPL-3.0-only
//! The token endpoint (RFC 0047 §5.5.4, draft §4.2–§4.9): every grant a
//! Session needs, device authorization and revocation, the typed token
//! response and the typed error.
//!
//! | Purpose | Grant | Extra | DPoP |
//! |---|---|---|---|
//! | Start a Session | JWT bearer | `assertion` | a proof |
//! | Renew a signed-out one | the same | `session_id` | a proof |
//! | Use the Account Token: sign a Session in, renew it, or start one signed in | `refresh_token` | `refresh_token`, `session_id`?, a narrower `scope`? | a proof |
//! | A Bearer token for an MCP server | the Session's renew grant | `session_id`, `resource` = its `url` | none (§4.3: "without the `DPoP` header") |
//! | A DPoP token for an API or a conversation endpoint the Company lists with a `resource` | the Session's renew grant | `session_id`, `resource` | a proof |
//! | Device Sign-In's poll | `device_code` | `device_code`, `session_id` | a proof |
//! | Direct Sign-In's code | `authorization_code` | `code`, `redirect_uri`, `code_verifier`, `session_id` | a proof |
//!
//! Every request carries a client assertion (`private_key_jwt`), and the JWT
//! bearer grant a fresh Session assertion, both with `aud` the issuer as one
//! string; every retry signs fresh ones (Guides #both-retries). The device
//! authorization request (§4.6) and the revocation request (§4.9) are form
//! requests to their own endpoints, with the client assertion and — as §4.3
//! says of every request, though their examples show none — a proof. A
//! `use_dpop_nonce` is retried once with the nonce the Company sent (§4.3:
//! "Personal Agents MUST support nonces"). Each request is safe to repeat
//! (RFC 0047 §5.9), so one that got no answer or a `5xx` is sent again, at
//! most [`ATTEMPTS`] times in all, backing off between — never before the
//! Company's not-before instant, which refuses it unsent. A response is used
//! only with every member §4.2 requires; one missing any is an error. Bodies
//! are never logged: an error carries the code the Company gave, and nothing
//! it wrote.

use std::time::Duration;

use serde_json::Value;

use super::dpop::{DpopKey, Nonces};
use super::http::{ATTEMPTS, Dialer, backoff};
use super::identity::Identity;
use super::wire::{self, error as err, grant, param};
use crate::auth::client_assertion::ASSERTION_TYPE;

/// What a token request is made with.
pub struct Ctx<'a> {
    pub identity: &'a Identity,
    pub issuer: &'a str,
    pub token_endpoint: &'a str,
    pub dialer: &'a Dialer,
    pub user_id: &'a str,
    pub dpop: &'a DpopKey,
    pub nonces: &'a Nonces,
}

/// What a token request is granted on.
#[derive(Clone, Copy)]
pub enum Grant<'a> {
    /// The Personal Agent's Session assertion (§4.2): a signed-out token.
    Assertion,
    /// The Account Token (§4.8), and the narrower scopes it is used for.
    Refresh {
        token: &'a str,
        scope: Option<&'a str>,
    },
    /// Device Sign-In's device code (§4.6).
    DeviceCode { device_code: &'a str },
    /// Direct Sign-In's authorization code (§4.5).
    Code {
        code: &'a str,
        redirect_uri: &'a str,
        verifier: &'a str,
    },
}

impl std::fmt::Debug for Grant<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Grant::Assertion => "Assertion",
            Grant::Refresh { .. } => "Refresh",
            Grant::DeviceCode { .. } => "DeviceCode",
            Grant::Code { .. } => "Code",
        })
    }
}

/// One token request: what it is granted on, the Session it is for (none
/// starts one), the `resource` it is for, and whether it carries a proof.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub grant: Grant<'a>,
    pub session_id: Option<&'a str>,
    pub resource: Option<&'a str>,
    pub dpop: bool,
}

impl<'a> Request<'a> {
    /// Start a Session: a DPoP Session Token for a new one.
    pub fn start() -> Request<'a> {
        Request {
            grant: Grant::Assertion,
            session_id: None,
            resource: None,
            dpop: true,
        }
    }

    /// Renew `session_id`'s DPoP Session Token.
    pub fn renew(session_id: &'a str) -> Request<'a> {
        Request {
            session_id: Some(session_id),
            ..Request::start()
        }
    }

    /// A Bearer token for the MCP server at `url`, for `session_id`.
    pub fn bearer(session_id: &'a str, url: &'a str) -> Request<'a> {
        Request {
            resource: Some(url),
            dpop: false,
            ..Request::renew(session_id)
        }
    }

    /// A DPoP token issued for `resource` — an API or conversation endpoint
    /// the Company lists with one (§3.1, §4.3) — for `session_id`.
    pub fn resource(session_id: &'a str, resource: &'a str) -> Request<'a> {
        Request {
            resource: Some(resource),
            ..Request::renew(session_id)
        }
    }

    /// The same request granted on the Account Token `token` instead —
    /// narrowed to `scope` when it names fewer scopes than the token holds.
    pub fn with_account(self, token: &'a str, scope: Option<&'a str>) -> Request<'a> {
        Request {
            grant: Grant::Refresh { token, scope },
            ..self
        }
    }

    /// Device Sign-In's poll of `device_code`, for `session_id`.
    pub fn device(device_code: &'a str, session_id: &'a str) -> Request<'a> {
        Request {
            grant: Grant::DeviceCode { device_code },
            ..Request::renew(session_id)
        }
    }

    /// Direct Sign-In's exchange of `code`, for `session_id`.
    pub fn code(
        code: &'a str,
        redirect_uri: &'a str,
        verifier: &'a str,
        session_id: &'a str,
    ) -> Request<'a> {
        Request {
            grant: Grant::Code {
                code,
                redirect_uri,
                verifier,
            },
            ..Request::renew(session_id)
        }
    }

    /// What the request is for, as `agent_company_token_requests_total`
    /// names it.
    pub fn grant(&self) -> &'static str {
        match (self.grant, self.resource, self.session_id) {
            (_, Some(_), _) if !self.dpop => "bearer",
            (_, Some(_), _) => "resource",
            (Grant::Refresh { .. }, None, _) => "refresh",
            (Grant::DeviceCode { .. }, None, _) => "device_code",
            (Grant::Code { .. }, None, _) => "authorization_code",
            (Grant::Assertion, None, Some(_)) => "renew",
            (Grant::Assertion, None, None) => "start",
        }
    }
}

/// A token response (§4.2, §4.4).
#[derive(Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Duration,
    pub scope: Vec<String>,
    pub session_id: String,
    pub signed_in: bool,
    /// The Account Token a sign-in issued, or the one a rotation replaced
    /// the used one with (§4.8).
    pub refresh_token: Option<String>,
    pub refresh_token_expires_in: Option<Duration>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"***")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("signed_in", &self.signed_in)
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Why no token came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// The Company refused with an OAuth error (§4.2's table).
    Refused {
        status: u16,
        code: String,
        /// The extension an `extension_required` names (§3.3).
        extension: Option<String>,
        retry_after: Option<Duration>,
    },
    /// The request was not sent, or no answer came.
    Transport(String),
    /// A status other than success, with no OAuth error.
    Status(u16),
    /// Not sent: the Company asked for no request before an instant still
    /// ahead.
    Paced(String),
    /// An answer that is not a token response the draft describes.
    Malformed(String),
}

impl TokenError {
    pub fn code(&self) -> Option<&str> {
        match self {
            TokenError::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether the Company refused with `code`.
    pub fn is(&self, code: &str) -> bool {
        self.code() == Some(code)
    }

    /// Whether sending the request again could get another answer: no
    /// answer came, or the Company's server failed (`5xx`).
    pub fn repeatable(&self) -> bool {
        match self {
            TokenError::Transport(_) => true,
            TokenError::Status(s) | TokenError::Refused { status: s, .. } => *s >= 500,
            TokenError::Paced(_) | TokenError::Malformed(_) => false,
        }
    }
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Refused {
                status,
                code,
                extension,
                ..
            } => {
                write!(f, "the token endpoint refused: {code} (HTTP {status})")?;
                if let Some(e) = extension {
                    write!(f, ", extension {e}")?;
                }
                Ok(())
            }
            TokenError::Transport(e) => write!(f, "the token request failed: {e}"),
            TokenError::Status(s) => {
                write!(
                    f,
                    "the token endpoint answered HTTP {s} with no OAuth error"
                )
            }
            TokenError::Paced(e) => write!(f, "the token request was not sent: {e}"),
            TokenError::Malformed(e) => write!(f, "the token endpoint answered {e}"),
        }
    }
}

/// Send `req`: once more with the Company's nonce when it asks for one, and
/// again after a back-off when no answer came or its server failed, at most
/// [`ATTEMPTS`] times.
pub fn request(ctx: &Ctx<'_>, req: Request<'_>) -> Result<TokenResponse, TokenError> {
    let json = repeat(ctx, req.dpop, req.grant(), || {
        Ok((ctx.token_endpoint.to_string(), fields(ctx, req)?))
    })?;
    parse(&json, req)
}

/// The form a token request sends: the grant's own fields, then the client's.
fn fields(ctx: &Ctx<'_>, req: Request<'_>) -> Result<Vec<(&'static str, String)>, TokenError> {
    let mut f: Vec<(&'static str, String)> = Vec::new();
    match req.grant {
        Grant::Assertion => {
            f.push(("grant_type", grant::JWT_BEARER.into()));
            let assertion = ctx
                .identity
                .session_assertion(ctx.user_id, ctx.issuer)
                .map_err(TokenError::Transport)?;
            f.push(("assertion", assertion));
        }
        Grant::Refresh { token, scope } => {
            f.push(("grant_type", grant::REFRESH_TOKEN.into()));
            f.push(("refresh_token", token.into()));
            if let Some(s) = scope {
                f.push(("scope", s.into()));
            }
        }
        Grant::DeviceCode { device_code } => {
            f.push(("grant_type", grant::DEVICE_CODE.into()));
            f.push(("device_code", device_code.into()));
        }
        Grant::Code {
            code,
            redirect_uri,
            verifier,
        } => {
            f.push(("grant_type", grant::AUTHORIZATION_CODE.into()));
            f.push(("code", code.into()));
            f.push(("redirect_uri", redirect_uri.into()));
            f.push(("code_verifier", verifier.into()));
        }
    }
    if let Some(s) = req.session_id {
        f.push((param::SESSION_ID, s.into()));
    }
    if let Some(r) = req.resource {
        f.push((param::RESOURCE, r.into()));
    }
    Ok(f)
}

/// What a device authorization request answered (§4.6).
#[derive(Clone)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    /// How long the device code lives: `expires_in`, at most
    /// [`DEVICE_FOR`].
    pub expires_in: Duration,
    /// How long to wait between polls: `interval`, or RFC 8628's 5 s — no
    /// longer than `expires_in`.
    pub interval: Duration,
}

impl std::fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceAuthorization")
            .field("device_code", &"***")
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// RFC 8628 §3.5's poll interval when the Company names none.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
/// The longest agentd follows a Device Sign-In, whatever `expires_in` says.
pub const DEVICE_FOR: Duration = Duration::from_secs(30 * 60);

/// Ask `endpoint`, the device authorization endpoint, to start a Device
/// Sign-In for `scope` (§4.6).
pub fn device_authorization(
    ctx: &Ctx<'_>,
    endpoint: &str,
    scope: &str,
) -> Result<DeviceAuthorization, TokenError> {
    let v = repeat(ctx, true, "device_authorization", || {
        Ok((endpoint.to_string(), vec![("scope", scope.to_string())]))
    })?;
    let text = |m: &str| {
        v[m].as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                TokenError::Malformed(format!("a device authorization response without {m}"))
            })
    };
    let expires_in = v["expires_in"].as_u64().filter(|n| *n > 0).ok_or_else(|| {
        TokenError::Malformed("a device authorization response without expires_in".into())
    })?;
    // Both are the Company's to choose, and only ever added to a clock: held
    // to what agentd follows, an interval to no longer than the code lives.
    let expires_in = Duration::from_secs(expires_in).min(DEVICE_FOR);
    Ok(DeviceAuthorization {
        device_code: text("device_code")?,
        user_code: text("user_code")?,
        verification_uri: text("verification_uri")?,
        verification_uri_complete: v["verification_uri_complete"].as_str().map(str::to_string),
        expires_in,
        interval: v["interval"]
            .as_u64()
            .filter(|n| *n > 0)
            .map_or(DEFAULT_INTERVAL, Duration::from_secs)
            .min(expires_in),
    })
}

/// Revoke the Account Token `token` at `endpoint`, the revocation endpoint
/// (§4.9, RFC 7009): `token_type_hint=refresh_token`, with the client
/// assertion. RFC 7009 §2.2 answers `200` whether or not the token was
/// good.
pub fn revoke(ctx: &Ctx<'_>, endpoint: &str, token: &str) -> Result<(), TokenError> {
    repeat(ctx, true, "revoke", || {
        Ok((
            endpoint.to_string(),
            vec![
                ("token", token.to_string()),
                ("token_type_hint", "refresh_token".to_string()),
            ],
        ))
    })
    .map(|_| ())
}

/// Send the form request `build` makes, as [`request`] sends one: once more
/// with the Company's nonce when it asks for one, and after a back-off when
/// no answer came or its server failed. `grant` names it in the metrics.
fn repeat(
    ctx: &Ctx<'_>,
    dpop: bool,
    grant: &'static str,
    build: impl Fn() -> Result<(String, Vec<(&'static str, String)>), TokenError>,
) -> Result<Value, TokenError> {
    let (mut nonce, mut attempt) = (false, 1);
    loop {
        match once(ctx, dpop, grant, &build) {
            Err(TokenError::Refused { code, .. })
                if dpop && !nonce && code == err::USE_DPOP_NONCE =>
            {
                nonce = true;
            }
            Err(e) if e.repeatable() && attempt < ATTEMPTS => {
                std::thread::sleep(backoff(attempt));
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// One attempt, with fresh assertions and, when it carries one, a fresh
/// proof — counted by its outcome once it is sent.
fn once(
    ctx: &Ctx<'_>,
    dpop: bool,
    grant: &'static str,
    build: &impl Fn() -> Result<(String, Vec<(&'static str, String)>), TokenError>,
) -> Result<Value, TokenError> {
    ctx.dialer.pace.check().map_err(TokenError::Paced)?;
    let out = build().and_then(|(url, f)| attempt(ctx, &url, &f, dpop));
    let outcome = match &out {
        Ok(_) => "ok",
        Err(TokenError::Refused { .. }) => "refused",
        Err(TokenError::Paced(_)) => return out,
        Err(_) => "failed",
    };
    crate::obs::metrics::record_company_token_request(&ctx.dialer.company, grant, outcome);
    out
}

fn attempt(
    ctx: &Ctx<'_>,
    url: &str,
    fields: &[(&'static str, String)],
    dpop: bool,
) -> Result<Value, TokenError> {
    let mut form = crate::auth::oauth2::Form::new();
    for (k, v) in fields {
        form.field(k, v);
    }
    let client_assertion = ctx
        .identity
        .client_assertion(ctx.issuer)
        .map_err(TokenError::Transport)?;
    form.field("client_id", &ctx.identity.client_id);
    form.field("client_assertion_type", ASSERTION_TYPE);
    form.field("client_assertion", &client_assertion);
    let body = form.finish();
    // Signed at the last moment, once the URL is final. A proof to these
    // endpoints never has `ath`, even one carrying a credential in its body.
    let proof = if dpop {
        Some(
            ctx.dpop
                .proof("POST", url, None, ctx.nonces.get(url).as_deref())
                .map_err(TokenError::Transport)?,
        )
    } else {
        None
    };
    let mut headers = vec![
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Accept", "application/json"),
    ];
    if let Some(p) = &proof {
        headers.push((wire::DPOP_HEADER, p.as_str()));
    }
    let reply = ctx
        .dialer
        .send("POST", url, &headers, &body)
        .map_err(TokenError::Transport)?;
    if let Some(n) = reply.header(wire::DPOP_NONCE_HEADER) {
        ctx.nonces.learn(url, n);
    }
    let json: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    if !reply.is_success() {
        return Err(match json["error"].as_str() {
            Some(code) => TokenError::Refused {
                status: reply.status,
                code: code.to_string(),
                extension: json["extension"].as_str().map(str::to_string),
                retry_after: reply.retry_after(),
            },
            None => TokenError::Status(reply.status),
        });
    }
    Ok(json)
}

/// A token response, with every member §4.2 requires.
fn parse(v: &Value, req: Request<'_>) -> Result<TokenResponse, TokenError> {
    let missing = |m: &str| TokenError::Malformed(format!("a token response without {m}"));
    let access_token = v["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| missing("access_token"))?;
    let token_type = v["token_type"]
        .as_str()
        .ok_or_else(|| missing("token_type"))?;
    let want = if req.dpop {
        wire::TOKEN_TYPE_DPOP
    } else {
        wire::TOKEN_TYPE_BEARER
    };
    if !token_type.eq_ignore_ascii_case(want) {
        return Err(TokenError::Malformed(format!(
            "a {token_type} token to a request for a {want} one"
        )));
    }
    let expires_in = v["expires_in"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or_else(|| missing("expires_in greater than 0"))?;
    let scope = v["scope"].as_str().ok_or_else(|| missing("scope"))?;
    let session_id = v[wire::member::SESSION_ID]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| missing("session_id"))?;
    let signed_in = v[wire::member::SIGNED_IN]
        .as_bool()
        .ok_or_else(|| missing("signed_in"))?;
    Ok(TokenResponse {
        access_token: access_token.to_string(),
        token_type: token_type.to_string(),
        expires_in: Duration::from_secs(expires_in),
        scope: scope
            .split(' ')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        session_id: session_id.to_string(),
        signed_in,
        refresh_token: v["refresh_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        refresh_token_expires_in: v["refresh_token_expires_in"]
            .as_u64()
            .map(Duration::from_secs),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({"access_token": "t", "token_type": "DPoP", "expires_in": 60, "scope": "",
               "session_id": "ses_1", "signed_in": false})
    }

    /// A token request is sent again only when another answer could come:
    /// none came, or the Company's server failed. A refusal, a malformed
    /// answer and a wait the Company asked for are final.
    #[test]
    fn only_a_request_that_got_no_answer_or_a_5xx_is_repeated() {
        let refused = |status| TokenError::Refused {
            status,
            code: "invalid_grant".into(),
            extension: None,
            retry_after: None,
        };
        assert!(TokenError::Transport("reset".into()).repeatable());
        assert!(TokenError::Status(502).repeatable());
        assert!(refused(503).repeatable());
        assert!(!refused(400).repeatable());
        assert!(refused(400).is(err::INVALID_GRANT));
        assert!(!TokenError::Status(404).repeatable());
        assert!(!TokenError::Paced("rate limited".into()).repeatable());
        assert!(!TokenError::Malformed("x".into()).repeatable());
        assert_eq!(Request::start().grant(), "start");
        assert_eq!(Request::renew("s").grant(), "renew");
        assert_eq!(Request::bearer("s", "https://m/mcp").grant(), "bearer");
        assert_eq!(Request::resource("s", "https://api/v1").grant(), "resource");
        assert!(Request::resource("s", "https://api/v1").dpop);
        assert_eq!(
            Request::renew("s").with_account("pat", None).grant(),
            "refresh"
        );
        assert_eq!(
            Request::bearer("s", "https://m/mcp")
                .with_account("pat", None)
                .grant(),
            "bearer"
        );
        assert_eq!(Request::device("d", "s").grant(), "device_code");
        assert_eq!(
            Request::code("c", "https://a/cb", "v", "s").grant(),
            "authorization_code"
        );
    }

    /// A response missing any member §4.2 requires is an error, not a token.
    #[test]
    fn a_token_response_needs_every_member_the_draft_requires() {
        let ok = parse(&good(), Request::start()).unwrap();
        assert_eq!(ok.session_id, "ses_1");
        assert!(!ok.signed_in && ok.scope.is_empty());
        assert!(ok.refresh_token.is_none());
        assert!(!format!("{ok:?}").contains("\"t\""), "never printed");
        for m in [
            "access_token",
            "token_type",
            "expires_in",
            "scope",
            "session_id",
            "signed_in",
        ] {
            let mut v = good();
            v.as_object_mut().unwrap().remove(m);
            assert!(parse(&v, Request::start()).is_err(), "without {m}");
        }
        let mut zero = good();
        zero["expires_in"] = json!(0);
        assert!(parse(&zero, Request::start()).is_err());
        // A Bearer request takes a Bearer token only, and a DPoP one a DPoP.
        assert!(parse(&good(), Request::bearer("ses_1", "https://m/mcp")).is_err());
        let mut b = good();
        b["token_type"] = json!("Bearer");
        assert!(parse(&b, Request::bearer("ses_1", "https://m/mcp")).is_ok());
        // A sign-in's Account Token comes back with the Session Token, and
        // is never printed.
        let mut signed = good();
        signed["refresh_token"] = json!("pat_one");
        signed["refresh_token_expires_in"] = json!(2_592_000);
        signed["signed_in"] = json!(true);
        signed["scope"] = json!("poppy:read poppy:write");
        let s = parse(&signed, Request::device("dev", "ses_1")).unwrap();
        assert_eq!(s.refresh_token.as_deref(), Some("pat_one"));
        assert_eq!(
            s.refresh_token_expires_in,
            Some(Duration::from_secs(2_592_000))
        );
        assert_eq!(s.scope, ["poppy:read", "poppy:write"]);
        assert!(!format!("{s:?}").contains("pat_one"));
    }
}
