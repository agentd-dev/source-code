// SPDX-License-Identifier: AGPL-3.0-only
//! OpenID CIBA Core 1.0 in poll mode on the wire (RFC 0044 §2.8, §5.10):
//! the backchannel authentication request a firing of an `act_for`
//! workflow sends its person's IdP, and the token-endpoint poll that learns
//! how they answered on their own device.
//!
//! - **The request** (CIBA §7.1) asks for `scope=openid` alone, so no
//!   refresh token is wanted; names the person by exactly one hint — an ID
//!   token, or a login hint; carries agentd's own `binding_message`, written
//!   from identifiers and never from a payload ([`binding_message`]); and
//!   says how long it waits, as CIBA Core's `requested_expiry` or Okta's
//!   `request_expiry`. It is authenticated as agentd's client at the IdP,
//!   its assertion's `aud` the token endpoint, as every request there is.
//! - **The poll** (§10.1) is the `urn:openid:params:grant-type:ciba` grant
//!   with the `auth_req_id`: `authorization_pending` and `slow_down` (§11)
//!   say to wait — `slow_down` 5 s longer every time from then on —
//!   `access_denied` that the person said no, `expired_token` that the
//!   request ran out, and a token response that they approved.

use std::time::Duration;

use serde_json::Value;

use crate::auth::idjag::Refusal;
use crate::auth::oauth2::{Form, OAuth2Params, Tokens, post_form};

/// The poll's `grant_type` (CIBA §10.1).
pub const GRANT_CIBA: &str = "urn:openid:params:grant-type:ciba";
/// The token endpoint's answers while the person has not approved (§11).
pub const AUTHORIZATION_PENDING: &str = "authorization_pending";
pub const SLOW_DOWN: &str = "slow_down";
pub const ACCESS_DENIED: &str = "access_denied";
pub const EXPIRED_TOKEN: &str = "expired_token";
/// An IdP's metadata members: where requests go, and how tokens are
/// delivered (§4).
pub const METADATA_ENDPOINT: &str = "backchannel_authentication_endpoint";
pub const METADATA_MODES: &str = "backchannel_token_delivery_modes_supported";
/// The one delivery mode agentd speaks.
pub const MODE_POLL: &str = "poll";
/// How often to poll when the IdP names no `interval` (§7.3).
pub const DEFAULT_INTERVAL: u64 = 5;
/// The longest binding message agentd writes.
const BINDING_MAX: usize = 96;

/// Whom a request names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint<'a> {
    /// An ID token the IdP issued for the person (`id_token_hint`).
    IdToken(&'a str),
    /// A name the IdP knows them by (`login_hint`).
    Login(&'a str),
}

/// One backchannel authentication request.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    pub hint: Hint<'a>,
    pub binding_message: &'a str,
    /// How long it waits for the person, seconds.
    pub expiry_s: u64,
    /// The parameter the wait is sent as: `requested_expiry` (CIBA Core),
    /// or `request_expiry` (Okta).
    pub expiry_param: &'static str,
}

/// What the IdP answered a request with (§7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub auth_req_id: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// What one poll found.
#[derive(Debug)]
pub enum Polled {
    Pending,
    SlowDown,
    /// The person said no.
    Denied,
    /// The request ran out before they answered.
    Expired,
    /// They approved: the token response.
    Approved(Box<Tokens>),
}

/// A binding message from `parts` — identifiers agentd chose, never text
/// a payload or a model wrote: each reduced to letters, digits and `.`,
/// `_`, `-`, `:` and `/`, joined by spaces, and the whole cut to 96
/// characters, as CIBA §7.1 asks of a message the person compares on two
/// screens.
pub fn binding_message(parts: &[&str]) -> String {
    let words: Vec<String> = parts
        .iter()
        .map(|p| {
            p.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/'))
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = words.join(" ");
    out.truncate(BINDING_MAX);
    out
}

/// The request's form, before client authentication (§7.1).
pub(crate) fn request_form(client_id: &str, ask: &Ask<'_>) -> Form {
    let mut form = Form::new();
    form.field("scope", "openid");
    match ask.hint {
        Hint::IdToken(t) => form.field("id_token_hint", t),
        Hint::Login(l) => form.field("login_hint", l),
    }
    if !ask.binding_message.is_empty() {
        form.field("binding_message", ask.binding_message);
    }
    form.field(ask.expiry_param, &ask.expiry_s.to_string());
    form.field("client_id", client_id);
    form
}

/// The answer to a request: the `auth_req_id` and its `expires_in`, both
/// required (§7.3), and the `interval` to poll at.
pub fn started_of(body: &Value) -> Result<Started, Refusal> {
    let auth_req_id = body["auth_req_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Refusal::Unavailable("no auth_req_id in the CIBA answer".into()))?;
    let expires_in = body["expires_in"]
        .as_u64()
        .filter(|e| *e > 0)
        .ok_or_else(|| Refusal::Unavailable("no expires_in in the CIBA answer".into()))?;
    Ok(Started {
        auth_req_id: auth_req_id.to_string(),
        expires_in,
        interval: body["interval"].as_u64().unwrap_or(DEFAULT_INTERVAL).max(1),
    })
}

/// Send `ask` to `endpoint`, the IdP's backchannel authentication endpoint,
/// authenticated as `idp` says.
pub fn authorize(
    idp: &OAuth2Params,
    endpoint: &str,
    ask: &Ask<'_>,
    timeout: Duration,
) -> Result<Started, Refusal> {
    let body = request(idp, endpoint, request_form(&idp.client_id, ask), timeout)?;
    started_of(&body)
}

/// What a poll's answer says: a token response, or one of §11's errors.
pub fn polled_of(answer: Result<Value, Refusal>) -> Result<Polled, Refusal> {
    match answer {
        Ok(v) => serde_json::from_value(v)
            .map(|t| Polled::Approved(Box::new(t)))
            .map_err(|e| Refusal::Unavailable(format!("not a token response: {e}"))),
        Err(Refusal::Oauth { code, .. }) if code == AUTHORIZATION_PENDING => Ok(Polled::Pending),
        Err(Refusal::Oauth { code, .. }) if code == SLOW_DOWN => Ok(Polled::SlowDown),
        Err(Refusal::Oauth { code, .. }) if code == ACCESS_DENIED => Ok(Polled::Denied),
        Err(Refusal::Oauth { code, .. }) if code == EXPIRED_TOKEN => Ok(Polled::Expired),
        Err(r) => Err(r),
    }
}

/// Poll the IdP's token endpoint once for `auth_req_id`.
pub fn poll(idp: &OAuth2Params, auth_req_id: &str, timeout: Duration) -> Result<Polled, Refusal> {
    let mut form = Form::new();
    form.field("grant_type", GRANT_CIBA);
    form.field("auth_req_id", auth_req_id);
    form.field("client_id", &idp.client_id);
    polled_of(request(idp, &idp.token_url, form, timeout))
}

/// POST `form` to `url` with `idp`'s client authentication: a JSON answer,
/// or what refused it.
fn request(
    idp: &OAuth2Params,
    url: &str,
    mut form: Form,
    timeout: Duration,
) -> Result<Value, Refusal> {
    let headers = idp.authenticate(&mut form).map_err(|e| {
        if crate::auth::oauth2::signer_failed(&e) {
            Refusal::Signer(e)
        } else {
            Refusal::Unavailable(e)
        }
    })?;
    let reply = post_form(url, form, &headers, timeout, idp.dial).map_err(Refusal::Unavailable)?;
    if !reply.is_success() {
        return Err(crate::auth::idjag::refusal_of(&reply));
    }
    serde_json::from_slice(&reply.body)
        .map_err(|e| Refusal::Unavailable(format!("not a JSON answer: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(f: Form) -> Vec<(String, String)> {
        String::from_utf8(f.finish())
            .unwrap()
            .split('&')
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap();
                (k.to_string(), crate::auth::browser::pct_decode(v))
            })
            .collect()
    }

    /// A request asks for `openid` alone, names the person by one hint,
    /// carries the binding message and the wait under the parameter the
    /// IdP reads it as — CIBA Core's, or Okta's.
    #[test]
    fn a_request_names_one_hint_and_its_wait() {
        let ask = Ask {
            hint: Hint::IdToken("eyJ.a.b"),
            binding_message: "agentd prod: nightly",
            expiry_s: 300,
            expiry_param: "requested_expiry",
        };
        let f = fields(request_form("agentd-idp", &ask));
        assert_eq!(
            f,
            [
                ("scope", "openid"),
                ("id_token_hint", "eyJ.a.b"),
                ("binding_message", "agentd prod: nightly"),
                ("requested_expiry", "300"),
                ("client_id", "agentd-idp"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string()))
        );
        let okta = Ask {
            hint: Hint::Login("alice@acme.example"),
            expiry_param: "request_expiry",
            ..ask
        };
        let f = fields(request_form("agentd-idp", &okta));
        assert!(f.contains(&("login_hint".into(), "alice@acme.example".into())));
        assert!(f.contains(&("request_expiry".into(), "300".into())));
        assert!(
            !f.iter()
                .any(|(k, _)| k == "id_token_hint" || k == "requested_expiry")
        );
    }

    /// The answer needs its `auth_req_id` and `expires_in`; the interval is
    /// 5 s when the IdP names none.
    #[test]
    fn an_answer_needs_its_id_and_expiry() {
        assert_eq!(
            started_of(&json!({"auth_req_id": "r1", "expires_in": 120})).unwrap(),
            Started {
                auth_req_id: "r1".into(),
                expires_in: 120,
                interval: 5
            }
        );
        assert_eq!(
            started_of(&json!({"auth_req_id": "r1", "expires_in": 120, "interval": 2}))
                .unwrap()
                .interval,
            2
        );
        assert!(started_of(&json!({"expires_in": 120})).is_err());
        assert!(started_of(&json!({"auth_req_id": "r1"})).is_err());
    }

    /// The poll's four states, and a token response: pending and
    /// `slow_down` wait, `access_denied` and `expired_token` end it, any
    /// other error is an error.
    #[test]
    fn a_poll_reads_the_four_states_and_an_approval() {
        let oauth = |code: &str| {
            Err(Refusal::Oauth {
                status: 400,
                code: code.into(),
                max_age: None,
            })
        };
        assert!(matches!(
            polled_of(oauth("authorization_pending")),
            Ok(Polled::Pending)
        ));
        assert!(matches!(
            polled_of(oauth("slow_down")),
            Ok(Polled::SlowDown)
        ));
        assert!(matches!(
            polled_of(oauth("access_denied")),
            Ok(Polled::Denied)
        ));
        assert!(matches!(
            polled_of(oauth("expired_token")),
            Ok(Polled::Expired)
        ));
        assert!(polled_of(oauth("invalid_grant")).is_err());
        let ok = polled_of(Ok(json!({"access_token": "a", "token_type": "Bearer",
                                     "id_token": "i", "expires_in": 3600})));
        assert!(matches!(ok, Ok(Polled::Approved(t)) if t.id_token.as_deref() == Some("i")));
    }

    /// A binding message is agentd's identifiers alone: whatever else a
    /// value holds is dropped, and the whole is cut short.
    #[test]
    fn a_binding_message_holds_identifiers_alone() {
        assert_eq!(
            binding_message(&["agentd", "prod:", "nightly-report", "run", "01J8"]),
            "agentd prod: nightly-report run 01J8"
        );
        assert_eq!(
            binding_message(&["agentd", "a\nb<script>", "\u{202e}x"]),
            "agentd abscript x"
        );
        assert_eq!(binding_message(&["x".repeat(200).as_str()]).len(), 96);
    }
}
