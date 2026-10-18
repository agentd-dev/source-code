// SPDX-License-Identifier: AGPL-3.0-only
//! The token endpoint (RFC 0047 §5.5.4, draft §4.2–§4.3): the grants a
//! signed-out Session needs, the typed token response and the typed error.
//!
//! | Purpose | Grant | Extra | DPoP |
//! |---|---|---|---|
//! | Start a Session | JWT bearer | `assertion` | a proof |
//! | Renew it | the same | `session_id` | a proof |
//! | A Bearer token for an MCP server | the same | `session_id`, `resource` = its `url` | none (§4.3: "without the `DPoP` header") |
//!
//! Every request carries a client assertion (`private_key_jwt`) and a fresh
//! Session assertion, both with `aud` the issuer as one string; every retry
//! signs fresh ones (Guides #both-retries). A `use_dpop_nonce` is retried
//! once with the nonce the Company sent (§4.3: "Personal Agents MUST support
//! nonces"). A response is used only with every member §4.2 requires; one
//! missing any is an error. Bodies are never logged: an error carries the
//! code the Company gave, and nothing it wrote.

use std::time::Duration;

use serde_json::Value;

use super::dpop::{DpopKey, Nonces};
use super::http::Dialer;
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

/// One token request: the Session it renews (none starts one), the
/// `resource` it is for, and whether it carries a proof.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    pub session_id: Option<&'a str>,
    pub resource: Option<&'a str>,
    pub dpop: bool,
}

impl<'a> Request<'a> {
    /// Start a Session: a DPoP Session Token for a new one.
    pub fn start() -> Request<'a> {
        Request {
            session_id: None,
            resource: None,
            dpop: true,
        }
    }

    /// Renew `session_id`'s DPoP Session Token.
    pub fn renew(session_id: &'a str) -> Request<'a> {
        Request {
            session_id: Some(session_id),
            resource: None,
            dpop: true,
        }
    }

    /// A Bearer token for the MCP server at `url`, for `session_id`.
    pub fn bearer(session_id: &'a str, url: &'a str) -> Request<'a> {
        Request {
            session_id: Some(session_id),
            resource: Some(url),
            dpop: false,
        }
    }
}

/// A token response (§4.2).
#[derive(Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Duration,
    pub scope: Vec<String>,
    pub session_id: String,
    pub signed_in: bool,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"***")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("signed_in", &self.signed_in)
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
            TokenError::Malformed(e) => write!(f, "the token endpoint answered {e}"),
        }
    }
}

/// Send `req`, retrying once with the Company's nonce when it asks for one.
pub fn request(ctx: &Ctx<'_>, req: Request<'_>) -> Result<TokenResponse, TokenError> {
    match once(ctx, req) {
        Err(TokenError::Refused { code, .. }) if req.dpop && code == err::USE_DPOP_NONCE => {
            once(ctx, req)
        }
        other => other,
    }
}

/// One attempt, with fresh assertions and, when it carries one, a fresh
/// proof.
fn once(ctx: &Ctx<'_>, req: Request<'_>) -> Result<TokenResponse, TokenError> {
    let mut form = crate::auth::oauth2::Form::new();
    form.field("grant_type", grant::JWT_BEARER);
    let assertion = ctx
        .identity
        .session_assertion(ctx.user_id, ctx.issuer)
        .map_err(TokenError::Transport)?;
    form.field("assertion", &assertion);
    if let Some(s) = req.session_id {
        form.field(param::SESSION_ID, s);
    }
    if let Some(r) = req.resource {
        form.field(param::RESOURCE, r);
    }
    let client_assertion = ctx
        .identity
        .client_assertion(ctx.issuer)
        .map_err(TokenError::Transport)?;
    form.field("client_id", &ctx.identity.client_id);
    form.field("client_assertion_type", ASSERTION_TYPE);
    form.field("client_assertion", &client_assertion);
    let body = form.finish();
    // Signed at the last moment, once the URL is final. A token request
    // never has `ath`, even one carrying a credential in its body.
    let proof = if req.dpop {
        Some(
            ctx.dpop
                .proof(
                    "POST",
                    ctx.token_endpoint,
                    None,
                    ctx.nonces.get(ctx.token_endpoint).as_deref(),
                )
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
        .send("POST", ctx.token_endpoint, &headers, &body)
        .map_err(TokenError::Transport)?;
    if let Some(n) = reply.header(wire::DPOP_NONCE_HEADER) {
        ctx.nonces.learn(ctx.token_endpoint, n);
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
            None => TokenError::Malformed(format!("HTTP {} with no OAuth error", reply.status)),
        });
    }
    parse(&json, req)
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

    /// A response missing any member §4.2 requires is an error, not a token.
    #[test]
    fn a_token_response_needs_every_member_the_draft_requires() {
        let ok = parse(&good(), Request::start()).unwrap();
        assert_eq!(ok.session_id, "ses_1");
        assert!(!ok.signed_in && ok.scope.is_empty());
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
    }
}
