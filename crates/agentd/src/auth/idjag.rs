// SPDX-License-Identifier: AGPL-3.0-only
//! The ID-JAG flow (draft-ietf-oauth-identity-assertion-authz-grant-04, RFC
//! 0044 §5.7) on the wire: the RFC 8693 exchange at the person's IdP for an
//! Identity Assertion JWT Authorization Grant, its redemption at the MCP
//! server's authorization server (RFC 7523), the `refresh_token` grant the
//! liveness probe and the `id_token` subject use, the two metadata documents
//! an IdP is read from, and the ID token check at enrolment.
//!
//! **Every answer is checked, and a failed check is a refusal, never a
//! warning** ([`check_exchange`], [`check_redeemed`]):
//!
//! - the exchange's `issued_token_type` is the ID-JAG's and its `token_type`
//!   `N_A`; a `refresh_token` in it is the IdP rotating the subject when the
//!   subject was the refresh token — handed back beside the verdict, whatever
//!   it is, since the IdP has spent the one presented ([`rotation_of`]) — and
//!   refused otherwise (§4.3.4);
//! - the ID-JAG, decoded without its signature (the authorization server
//!   verifies it), has `typ` `oauth-id-jag+jwt`, an `aud` that is the pinned
//!   authorization server (a string or a one-element array), the `client_id`
//!   agentd has there, the IdP's `iss`, an `exp` still ahead, the person's
//!   `sub` where subjects are public, and a `resource` that names the pinned
//!   resource — agentd asked for one, so one that does not is
//!   `resource_not_bound`;
//! - the scope granted is within the scope asked for; narrower is recorded;
//! - the redemption's `token_type` is `Bearer`, any `refresh_token` in it is
//!   dropped unread, and an access token that is a JWT names the pinned
//!   resource in its `aud`.
//!
//! Every identifier is in [`wire`]. Client authentication — `private_key_jwt`
//! with an ES256 or RS256 key, or a client secret — is `oauth2`'s, with its
//! key read from a mounted file or the startup prompt alone.

pub mod wire;

use std::time::Duration;

use serde_json::Value;

use crate::auth::http::{self, Dial, Reply};
use crate::auth::oauth2::{Form, OAuth2Params, Tokens, post_form};
use crate::b64;

/// What an exchange presents as its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectType {
    RefreshToken,
    IdToken,
}

impl SubjectType {
    pub fn token_type(self) -> &'static str {
        match self {
            SubjectType::RefreshToken => wire::TOKEN_TYPE_REFRESH_TOKEN,
            SubjectType::IdToken => wire::TOKEN_TYPE_ID_TOKEN,
        }
    }
}

/// Which check an answer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    IssuedTokenType,
    TokenType,
    UnexpectedRefreshToken,
    Malformed,
    Typ,
    Aud,
    ClientId,
    Iss,
    Sub,
    Expired,
    Scope,
    ResourceNotBound,
    /// The access token is a JWT for another resource.
    AccessTokenAudience,
}

/// A failed check, and what it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub kind: CheckKind,
    pub detail: String,
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

fn check(kind: CheckKind, detail: impl Into<String>) -> Check {
    Check {
        kind,
        detail: detail.into(),
    }
}

/// Why a request to the IdP or the authorization server got no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The server answered with an OAuth error: its `error`, the HTTP
    /// status, and the `max_age` a step-up asks for.
    Oauth {
        status: u16,
        code: String,
        max_age: Option<u64>,
    },
    /// No answer, a 5xx, or an answer that is no OAuth response at all.
    Unavailable(String),
    /// The answer failed a check.
    Checked(Check),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Oauth {
                status,
                code,
                max_age,
            } => {
                write!(f, "{code} (HTTP {status})")?;
                if let Some(m) = max_age {
                    write!(f, ", max_age {m}")?;
                }
                Ok(())
            }
            Refusal::Unavailable(e) => f.write_str(e),
            Refusal::Checked(c) => f.write_str(&c.detail),
        }
    }
}

impl Refusal {
    /// The OAuth error code, when the server answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Refusal::Oauth { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// An answer that is not a token: the OAuth error it names, or none.
fn refusal_of(reply: &Reply) -> Refusal {
    if reply.status >= 500 {
        return Refusal::Unavailable(format!("HTTP {}", reply.status));
    }
    let v: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    match v["error"].as_str() {
        Some(code) => Refusal::Oauth {
            status: reply.status,
            code: code.to_string(),
            max_age: v[wire::MAX_AGE].as_u64(),
        },
        None => Refusal::Unavailable(format!("HTTP {} with no OAuth error", reply.status)),
    }
}

/// POST `form` to `params`' token endpoint with its client authentication.
fn token_request(
    params: &OAuth2Params,
    mut form: Form,
    timeout: Duration,
) -> Result<Value, Refusal> {
    let headers = params
        .authenticate(&mut form)
        .map_err(Refusal::Unavailable)?;
    let reply = post_form(&params.token_url, form, &headers, timeout, params.dial)
        .map_err(Refusal::Unavailable)?;
    if !reply.is_success() {
        return Err(refusal_of(&reply));
    }
    serde_json::from_slice(&reply.body)
        .map_err(|e| Refusal::Unavailable(format!("not a JSON token response: {e}")))
}

/// The header and the claims of a compact JWS, unverified.
pub fn decode_unverified(jwt: &str) -> Result<(Value, Value), String> {
    let mut parts = jwt.split('.');
    let (Some(h), Some(c), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("not a compact JWS (three segments)".into());
    };
    let decode = |seg: &str| -> Result<Value, String> {
        let bytes = b64::url_decode(seg).map_err(|e| format!("not base64url: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("not JSON: {e}"))
    };
    Ok((decode(h)?, decode(c)?))
}

/// A space-separated scope string as a list.
fn scopes_of(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// One exchange: the subject, and where the ID-JAG is for.
pub struct Exchange<'a> {
    pub subject_token: &'a str,
    pub subject_type: SubjectType,
    /// The pinned authorization server's issuer: the RFC 8693 `audience`.
    pub audience: &'a str,
    /// The pinned MCP server.
    pub resource: &'a str,
    pub scopes: &'a [String],
}

/// What an ID-JAG must say.
pub struct Expect<'a> {
    /// The IdP's issuer.
    pub iss: &'a str,
    /// agentd's client at the authorization server.
    pub client_id: &'a str,
    /// The person's subject, where subjects are public; `None` records it.
    pub sub: Option<&'a str>,
    /// Accept an ID-JAG that names no resource (`allow_unbound_at`).
    pub allow_unbound: bool,
}

/// An ID-JAG, checked.
#[derive(Debug, Clone)]
pub struct Exchanged {
    pub id_jag: String,
    pub sub: String,
    pub jti: String,
    /// Seconds since the epoch.
    pub exp: u64,
    pub auth_time: Option<u64>,
    /// The scope the IdP granted.
    pub scopes: Vec<String>,
    /// Narrower than asked.
    pub narrowed: bool,
    /// The ID-JAG names no resource, under `allow_unbound_at`.
    pub unbound: bool,
    /// An `act` claim, kept verbatim for the audit record.
    pub act: Option<Value>,
}

/// Whether `v` — a string, or an array of strings — names `want`.
fn names(v: &Value, want: &str) -> bool {
    match v {
        Value::String(s) => s == want,
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some(want)),
        _ => false,
    }
}

/// Check an exchange's answer `body` for `ex` against `expect` at `now_s`.
pub fn check_exchange(
    body: &Value,
    ex: &Exchange<'_>,
    expect: &Expect<'_>,
    now_s: u64,
) -> Result<Exchanged, Check> {
    let issued = body["issued_token_type"].as_str().unwrap_or_default();
    if issued != wire::TOKEN_TYPE_ID_JAG {
        return Err(check(
            CheckKind::IssuedTokenType,
            format!("issued_token_type is {issued:?}, not the ID-JAG's"),
        ));
    }
    let token_type = body["token_type"].as_str().unwrap_or_default();
    if token_type != wire::TOKEN_TYPE_NA {
        return Err(check(
            CheckKind::TokenType,
            format!("token_type is {token_type:?}, not N_A"),
        ));
    }
    if body["refresh_token"].is_string() && ex.subject_type == SubjectType::IdToken {
        return Err(check(
            CheckKind::UnexpectedRefreshToken,
            "the exchange answered with a refresh_token, and its subject was an ID token: there is nothing for it to replace (ID-JAG §4.3.4)",
        ));
    }
    let id_jag = body["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| check(CheckKind::Malformed, "no access_token: no ID-JAG"))?;
    let (header, claims) = decode_unverified(id_jag)
        .map_err(|e| check(CheckKind::Malformed, format!("the ID-JAG is {e}")))?;
    if header["typ"].as_str() != Some(wire::TYP_ID_JAG) {
        return Err(check(
            CheckKind::Typ,
            format!(
                "the ID-JAG's typ is {}, not {}",
                header["typ"],
                wire::TYP_ID_JAG
            ),
        ));
    }
    let aud_ok = match &claims["aud"] {
        Value::String(s) => s == ex.audience,
        Value::Array(a) => a.len() == 1 && a[0].as_str() == Some(ex.audience),
        _ => false,
    };
    if !aud_ok {
        return Err(check(
            CheckKind::Aud,
            format!(
                "the ID-JAG's aud is {}, not the pinned {}",
                claims["aud"], ex.audience
            ),
        ));
    }
    if claims["client_id"].as_str() != Some(expect.client_id) {
        return Err(check(
            CheckKind::ClientId,
            format!(
                "the ID-JAG's client_id is {}, not agentd's {}",
                claims["client_id"], expect.client_id
            ),
        ));
    }
    if claims["iss"].as_str() != Some(expect.iss) {
        return Err(check(
            CheckKind::Iss,
            format!("the ID-JAG's iss is {}, not {}", claims["iss"], expect.iss),
        ));
    }
    let sub = claims["sub"].as_str().unwrap_or_default().to_string();
    if sub.is_empty() || expect.sub.is_some_and(|want| want != sub) {
        return Err(check(
            CheckKind::Sub,
            format!(
                "the ID-JAG's sub {sub:?} is not the enrolled person's {:?}",
                expect.sub.unwrap_or_default()
            ),
        ));
    }
    let exp = claims["exp"].as_u64().unwrap_or(0);
    if exp <= now_s {
        return Err(check(CheckKind::Expired, "the ID-JAG has expired"));
    }
    let unbound = claims.get("resource").is_none_or(Value::is_null);
    if unbound {
        if !expect.allow_unbound {
            return Err(check(
                CheckKind::ResourceNotBound,
                format!(
                    "the ID-JAG names no resource, and agentd asked for {} (resource_not_bound)",
                    ex.resource
                ),
            ));
        }
    } else if !names(&claims["resource"], ex.resource) {
        return Err(check(
            CheckKind::ResourceNotBound,
            format!(
                "the ID-JAG's resource is {}, which does not name the pinned {} (resource_not_bound)",
                claims["resource"], ex.resource
            ),
        ));
    }
    // The scope granted: the response's, else the claim's, else what was
    // asked (RFC 8693 §2.2.1: omitted when identical).
    let granted = body["scope"]
        .as_str()
        .or_else(|| claims["scope"].as_str())
        .map(scopes_of)
        .unwrap_or_else(|| ex.scopes.to_vec());
    if let Some(extra) = granted.iter().find(|s| !ex.scopes.contains(s)) {
        return Err(check(
            CheckKind::Scope,
            format!("the IdP granted {extra:?}, which was not asked for"),
        ));
    }
    Ok(Exchanged {
        id_jag: id_jag.to_string(),
        sub,
        jti: claims["jti"].as_str().unwrap_or_default().to_string(),
        exp,
        auth_time: claims["auth_time"].as_u64(),
        narrowed: granted.len() < ex.scopes.len(),
        scopes: granted,
        unbound,
        act: claims.get("act").cloned(),
    })
}

/// The refresh token an exchange's answer `body` replaced the subject with:
/// only a refresh-token subject's — beside an ID token one is refused by
/// [`check_exchange`] — and whatever the checks say of the ID-JAG.
pub fn rotation_of(body: &Value, subject_type: SubjectType) -> Option<String> {
    match subject_type {
        SubjectType::RefreshToken => body["refresh_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        SubjectType::IdToken => None,
    }
}

/// The exchange at the IdP (§4.3): `idp` carries its token endpoint and
/// agentd's client authentication there. The refresh token a rotating IdP
/// answered with comes back beside the checked ID-JAG, and also when a check
/// refuses it: the IdP spent the one presented, so the caller keeps the new
/// one before anything else (RFC 0044 §5.7).
pub fn exchange(
    idp: &OAuth2Params,
    ex: &Exchange<'_>,
    expect: &Expect<'_>,
    timeout: Duration,
) -> (Option<String>, Result<Exchanged, Refusal>) {
    let mut form = Form::new();
    form.field("grant_type", wire::GRANT_TOKEN_EXCHANGE);
    form.field("requested_token_type", wire::TOKEN_TYPE_ID_JAG);
    form.field("subject_token", ex.subject_token);
    form.field("subject_token_type", ex.subject_type.token_type());
    form.field("audience", ex.audience);
    form.field("resource", ex.resource);
    if !ex.scopes.is_empty() {
        form.field("scope", &ex.scopes.join(" "));
    }
    form.field("client_id", &idp.client_id);
    let body = match token_request(idp, form, timeout) {
        Ok(b) => b,
        Err(r) => return (None, Err(r)),
    };
    let now = crate::auth::cache::now_ms() / 1000;
    (
        rotation_of(&body, ex.subject_type),
        check_exchange(&body, ex, expect, now).map_err(Refusal::Checked),
    )
}

/// An access token from the authorization server, checked.
#[derive(Debug, Clone)]
pub struct Redeemed {
    pub access_token: String,
    pub expires_in: Option<u64>,
    pub scope: Option<String>,
}

/// Check a redemption's answer `body` for `resource`.
pub fn check_redeemed(body: &Value, resource: &str) -> Result<Redeemed, Check> {
    if !body["token_type"]
        .as_str()
        .is_some_and(|t| t.eq_ignore_ascii_case("bearer"))
    {
        return Err(check(
            CheckKind::TokenType,
            format!("token_type is {}, not Bearer", body["token_type"]),
        ));
    }
    let at = body["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| check(CheckKind::Malformed, "no access_token"))?;
    // An access token that is a JWT is held to the resource it is for; an
    // opaque one cannot be, and the authorization server's EMA obligation
    // is what protects it.
    if let Ok((_, claims)) = decode_unverified(at)
        && claims.get("aud").is_some()
        && !names(&claims["aud"], resource)
    {
        return Err(check(
            CheckKind::AccessTokenAudience,
            format!(
                "the access token's aud is {}, not the pinned {resource}",
                claims["aud"]
            ),
        ));
    }
    // Any `refresh_token` is dropped here, unread: renewal goes back through
    // the IdP, where a revocation lands.
    Ok(Redeemed {
        access_token: at.to_string(),
        expires_in: body["expires_in"].as_u64(),
        scope: body["scope"].as_str().map(str::to_string),
    })
}

/// The redemption at the resource's authorization server (RFC 7523 §2.1):
/// `ras` carries its token endpoint and agentd's client authentication.
pub fn redeem(
    ras: &OAuth2Params,
    id_jag: &str,
    resource: &str,
    timeout: Duration,
) -> Result<Redeemed, Refusal> {
    let mut form = Form::new();
    form.field("grant_type", wire::GRANT_JWT_BEARER);
    form.field("assertion", id_jag);
    form.field("client_id", &ras.client_id);
    let body = token_request(ras, form, timeout)?;
    check_redeemed(&body, resource).map_err(Refusal::Checked)
}

/// The `refresh_token` grant at the IdP with no `scope`, so the scopes
/// first granted apply — `openid` and `offline_access` among them, which a
/// narrower ask could lose. The liveness probe, and the `id_token`
/// subject's fresh ID token.
pub fn refresh(
    idp: &OAuth2Params,
    refresh_token: &str,
    timeout: Duration,
) -> Result<Tokens, Refusal> {
    let mut form = Form::new();
    form.field("grant_type", "refresh_token");
    form.field("refresh_token", refresh_token);
    form.field("client_id", &idp.client_id);
    let body = token_request(idp, form, timeout)?;
    serde_json::from_value(body)
        .map_err(|e| Refusal::Unavailable(format!("not a token response: {e}")))
}

/// RFC 7009 revocation of a refresh token at the IdP.
pub fn revoke(
    idp: &OAuth2Params,
    url: &str,
    refresh_token: &str,
    timeout: Duration,
) -> Result<(), String> {
    crate::auth::oauth2::revoke(idp, url, refresh_token, "refresh_token", timeout)
}

// --- the IdP's metadata ------------------------------------------------------

/// An IdP as its two metadata documents describe it, merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdpMeta {
    pub issuer: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub jwks_uri: Option<String>,
    /// `Some(true)` when `identity_chaining_requested_token_types_supported`
    /// lists the ID-JAG, `Some(false)` when it lists others, `None` when no
    /// document has the member (a SHOULD: a warning, not a refusal).
    pub lists_id_jag: Option<bool>,
    pub pkce_s256: bool,
    pub iss_supported: bool,
}

/// Merge the RFC 8414 document `asm` and the OpenID one `oidc` of `issuer`
/// (§5.6 step 5): `jwks_uri` and the OpenID endpoints from OpenID's, the
/// identity-chaining member and the grant types from RFC 8414's; each
/// present document must name `issuer` byte for byte, and two that name
/// different token endpoints are refused. `jwks_pin` wins over both.
pub fn merge_idp(
    issuer: &str,
    asm: Option<&Value>,
    oidc: Option<&Value>,
    jwks_pin: Option<&str>,
) -> Result<IdpMeta, String> {
    if asm.is_none() && oidc.is_none() {
        return Err(format!(
            "the IdP {issuer} publishes neither RFC 8414 nor OpenID metadata"
        ));
    }
    for (what, doc) in [("RFC 8414", asm), ("OpenID", oidc)] {
        if let Some(d) = doc
            && d["issuer"].as_str() != Some(issuer)
        {
            return Err(format!(
                "the IdP's {what} metadata names the issuer {}, not {issuer:?}",
                d["issuer"]
            ));
        }
    }
    let field = |k: &str, first: Option<&Value>, second: Option<&Value>| -> Option<String> {
        first
            .and_then(|d| d[k].as_str())
            .or_else(|| second.and_then(|d| d[k].as_str()))
            .map(str::to_string)
    };
    let tokens: Vec<&str> = [asm, oidc]
        .into_iter()
        .flatten()
        .filter_map(|d| d["token_endpoint"].as_str())
        .collect();
    if tokens.len() == 2 && tokens[0] != tokens[1] {
        return Err(format!(
            "the IdP's RFC 8414 and OpenID metadata name different token endpoints ({} and {})",
            tokens[0], tokens[1]
        ));
    }
    let token_endpoint = tokens
        .first()
        .map(|t| t.to_string())
        .ok_or_else(|| format!("the IdP {issuer}'s metadata names no token_endpoint"))?;
    let lists_id_jag = [asm, oidc]
        .into_iter()
        .flatten()
        .find_map(|d| d[wire::METADATA_IDENTITY_CHAINING].as_array())
        .map(|a| {
            a.iter()
                .any(|x| x.as_str() == Some(wire::TOKEN_TYPE_ID_JAG))
        });
    let lists = |k: &str, v: &str| {
        [asm, oidc].into_iter().flatten().any(|d| {
            d[k].as_array()
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(v)))
        })
    };
    Ok(IdpMeta {
        issuer: issuer.to_string(),
        token_endpoint,
        device_authorization_endpoint: field("device_authorization_endpoint", asm, oidc),
        authorization_endpoint: field("authorization_endpoint", oidc, asm),
        revocation_endpoint: field("revocation_endpoint", asm, oidc),
        jwks_uri: jwks_pin
            .map(str::to_string)
            .or_else(|| field("jwks_uri", oidc, asm)),
        lists_id_jag,
        pkce_s256: lists("code_challenge_methods_supported", "S256"),
        iss_supported: [asm, oidc].into_iter().flatten().any(|d| {
            d["authorization_response_iss_parameter_supported"]
                .as_bool()
                .unwrap_or(false)
        }),
    })
}

/// A JSON document at `url`, or `None` when the server has none there.
fn get_doc(url: &str, timeout: Duration) -> Result<Option<Value>, String> {
    let reply = http::get(url, &[("Accept", "application/json")], timeout, Dial::Named)?;
    match reply.status {
        200 => serde_json::from_slice(&reply.body)
            .map(Some)
            .map_err(|e| format!("{url}: not JSON: {e}")),
        404 | 410 => Ok(None),
        s => Err(format!("{url}: HTTP {s}")),
    }
}

/// Read the IdP `issuer`'s two documents and merge them ([`merge_idp`]).
pub fn idp_metadata(
    issuer: &str,
    jwks_pin: Option<&str>,
    timeout: Duration,
) -> Result<IdpMeta, String> {
    let asm_url = http::insert_well_known(issuer, "oauth-authorization-server")
        .ok_or_else(|| format!("{issuer:?} is not an issuer URL"))?;
    let asm = get_doc(&asm_url, timeout)?;
    let mut oidc = None;
    for url in [
        http::insert_well_known(issuer, "openid-configuration"),
        Some(format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        )),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(d) = get_doc(&url, timeout)? {
            oidc = Some(d);
            break;
        }
    }
    merge_idp(issuer, asm.as_ref(), oidc.as_ref(), jwks_pin)
}

/// The resource's authorization server as its metadata describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasMeta {
    pub issuer: String,
    pub token_endpoint: String,
}

/// Hold the authorization server's metadata `doc` to EMA (§5.6 step 4): a
/// byte-equal `issuer`, the JWT bearer grant among its grant types and the
/// ID-JAG profile among its grant profiles — unless `assume`, for a server
/// whose metadata omits the profile key.
pub fn ras_of(doc: &Value, audience: &str, assume: bool) -> Result<RasMeta, String> {
    if doc["issuer"].as_str() != Some(audience) {
        return Err(format!(
            "the authorization server's metadata names the issuer {}, not the pinned {audience:?}",
            doc["issuer"]
        ));
    }
    let lists = |k: &str, v: &str| {
        doc[k]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(v)))
    };
    if !assume {
        if !lists("grant_types_supported", wire::GRANT_JWT_BEARER) {
            return Err(format!(
                "{audience}'s metadata does not list the JWT bearer grant in grant_types_supported (set auth.id_jag: assume for a server that takes it unlisted)"
            ));
        }
        if !lists(wire::METADATA_GRANT_PROFILES, wire::PROFILE_ID_JAG) {
            return Err(format!(
                "{audience}'s metadata does not list {} in {} (set auth.id_jag: assume for a server that takes it unlisted)",
                wire::PROFILE_ID_JAG,
                wire::METADATA_GRANT_PROFILES
            ));
        }
    }
    let token_endpoint = doc["token_endpoint"]
        .as_str()
        .ok_or_else(|| format!("{audience}'s metadata names no token_endpoint"))?;
    Ok(RasMeta {
        issuer: audience.to_string(),
        token_endpoint: token_endpoint.to_string(),
    })
}

/// Read the pinned authorization server's metadata, in the order MCP's
/// authorization specification gives, and hold it to EMA ([`ras_of`]).
pub fn ras_metadata(audience: &str, assume: bool, timeout: Duration) -> Result<RasMeta, String> {
    let mut last = format!("{audience} publishes no authorization server metadata");
    for url in crate::auth::oauth2::discovery_urls(audience) {
        match get_doc(&url, timeout) {
            Ok(Some(doc)) => return ras_of(&doc, audience, assume),
            Ok(None) => {}
            Err(e) => last = e,
        }
    }
    Err(last)
}

// --- the ID token at enrolment ----------------------------------------------

/// What an enrolment keeps of a verified ID token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdClaims {
    pub iss: String,
    pub sub: String,
    pub exp: u64,
    pub auth_time: Option<u64>,
    pub email: Option<String>,
    pub preferred_username: Option<String>,
}

/// How far an ID token's `iat` may be ahead of this clock, and its `exp`
/// behind it.
const SKEW_S: u64 = 120;

/// Fetch the key set at `url`.
pub fn fetch_jwks(url: &str, timeout: Duration) -> Result<Value, String> {
    get_doc(url, timeout)?.ok_or_else(|| format!("{url}: no key set"))
}

/// Verify the JWS `jwt` against `jwks`: ES256 or RS256, by `kid` when the
/// header names one, else by the one key of the algorithm.
fn verify_jws(jwt: &str, jwks: &Value) -> Result<(Value, Value), String> {
    let (header, claims) = decode_unverified(jwt)?;
    let alg = header["alg"].as_str().unwrap_or_default();
    let keys = jwks["keys"].as_array().cloned().unwrap_or_default();
    let candidates: Vec<&Value> = keys
        .iter()
        .filter(|k| match header["kid"].as_str() {
            Some(kid) => k["kid"].as_str() == Some(kid),
            None => true,
        })
        .filter(|k| match alg {
            "ES256" => k["kty"] == "EC" && k["crv"] == "P-256",
            "RS256" => k["kty"] == "RSA",
            _ => false,
        })
        .collect();
    if candidates.is_empty() {
        return Err(format!(
            "no key in the IdP's key set verifies an {alg} token with kid {}",
            header["kid"]
        ));
    }
    let (input, sig) = jwt.rsplit_once('.').ok_or("not a compact JWS")?;
    let sig = b64::url_decode(sig).map_err(|e| format!("signature: {e}"))?;
    for k in candidates {
        let member = |m: &str| -> Result<Vec<u8>, String> {
            b64::url_decode(k[m].as_str().unwrap_or_default()).map_err(|e| format!("jwk {m}: {e}"))
        };
        let ok = match alg {
            "ES256" => {
                let mut point = vec![0x04];
                point.extend(member("x")?);
                point.extend(member("y")?);
                ring::signature::UnparsedPublicKey::new(
                    &ring::signature::ECDSA_P256_SHA256_FIXED,
                    &point,
                )
                .verify(input.as_bytes(), &sig)
                .is_ok()
            }
            _ => ring::signature::RsaPublicKeyComponents {
                n: member("n")?,
                e: member("e")?,
            }
            .verify(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                input.as_bytes(),
                &sig,
            )
            .is_ok(),
        };
        if ok {
            return Ok((header, claims));
        }
    }
    Err("the ID token's signature does not verify with the IdP's keys".into())
}

/// Verify an ID token at enrolment (§5.4 step 3): its signature with the
/// IdP's keys, `iss` the configured issuer, `aud` agentd's client, `exp`
/// and `iat` with skew, and the login's `nonce` when it sent one.
pub fn verify_id_token(
    id_token: &str,
    jwks: &Value,
    issuer: &str,
    client_id: &str,
    nonce: Option<&str>,
    now_s: u64,
) -> Result<IdClaims, String> {
    let (_, c) = verify_jws(id_token, jwks)?;
    if c["iss"].as_str() != Some(issuer) {
        return Err(format!(
            "the ID token's iss is {}, not {issuer:?}",
            c["iss"]
        ));
    }
    if !names(&c["aud"], client_id) {
        return Err(format!(
            "the ID token's aud is {}, not agentd's client {client_id:?}",
            c["aud"]
        ));
    }
    let exp = c["exp"].as_u64().unwrap_or(0);
    if exp + SKEW_S <= now_s {
        return Err("the ID token has expired".into());
    }
    if c["iat"].as_u64().is_some_and(|iat| iat > now_s + SKEW_S) {
        return Err("the ID token was issued in the future".into());
    }
    if let Some(want) = nonce
        && c["nonce"].as_str() != Some(want)
    {
        return Err("the ID token does not carry the login's nonce".into());
    }
    let sub = c["sub"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("the ID token names no sub")?;
    Ok(IdClaims {
        iss: issuer.to_string(),
        sub: sub.to_string(),
        exp,
        auth_time: c["auth_time"].as_u64(),
        email: c["email"].as_str().map(str::to_string),
        preferred_username: c["preferred_username"].as_str().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client_assertion::ClientKey;
    use serde_json::json;

    const IDP: &str = "https://acme.okta.com";
    const RAS: &str = "https://auth.linear.example";
    const RES: &str = "https://mcp.linear.example/mcp";

    fn key() -> ClientKey {
        ClientKey::from_pkcs8(&ClientKey::generate_pkcs8().unwrap()).unwrap()
    }

    fn claims() -> Value {
        json!({"iss": IDP, "sub": "00u1alice", "aud": RAS, "client_id": "agentd-at-linear",
               "jti": "j1", "iat": 1000, "exp": 1300, "resource": RES, "scope": "issues:read"})
    }

    fn answer(k: &ClientKey, header: Value, claims: &Value) -> Value {
        json!({"issued_token_type": wire::TOKEN_TYPE_ID_JAG, "token_type": "N_A",
               "access_token": k.sign_jwt(header, claims).unwrap(), "expires_in": 300})
    }

    fn ex(subject: SubjectType, scopes: &[String]) -> Exchange<'_> {
        Exchange {
            subject_token: "rt",
            subject_type: subject,
            audience: RAS,
            resource: RES,
            scopes,
        }
    }

    const EXPECT: Expect<'static> = Expect {
        iss: IDP,
        client_id: "agentd-at-linear",
        sub: Some("00u1alice"),
        allow_unbound: false,
    };

    /// Every check on an exchange's answer refuses what it should, by the
    /// kind it names.
    #[test]
    fn an_id_jag_is_held_to_every_pin() {
        let k = key();
        let typ = json!({"typ": wire::TYP_ID_JAG});
        let scopes = vec!["issues:read".to_string(), "issues:write".to_string()];
        let e = ex(SubjectType::IdToken, &scopes);
        let ok = check_exchange(&answer(&k, typ.clone(), &claims()), &e, &EXPECT, 1100).unwrap();
        assert_eq!(ok.sub, "00u1alice");
        assert_eq!(ok.scopes, ["issues:read"]);
        assert!(ok.narrowed, "narrower than asked is recorded");
        let kind = |body: &Value| check_exchange(body, &e, &EXPECT, 1100).unwrap_err().kind;
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims();
            f(&mut c);
            answer(&k, typ.clone(), &c)
        };
        assert_eq!(
            kind(&answer(&k, json!({"typ": "JWT"}), &claims())),
            CheckKind::Typ
        );
        assert_eq!(
            kind(&with(&|c| c["aud"] = json!("https://other.example"))),
            CheckKind::Aud
        );
        assert_eq!(
            kind(&with(&|c| c["aud"] = json!([RAS, "https://x"]))),
            CheckKind::Aud
        );
        assert_eq!(
            kind(&with(&|c| c["client_id"] = json!("someone"))),
            CheckKind::ClientId
        );
        assert_eq!(
            kind(&with(&|c| c["iss"] = json!("https://evil.example"))),
            CheckKind::Iss
        );
        assert_eq!(
            kind(&with(&|c| c["sub"] = json!("00u2bob"))),
            CheckKind::Sub
        );
        assert_eq!(kind(&with(&|c| c["exp"] = json!(1000))), CheckKind::Expired);
        assert_eq!(
            kind(&with(
                &|c| c["resource"] = json!("https://mcp.other.example/mcp")
            )),
            CheckKind::ResourceNotBound
        );
        assert_eq!(
            kind(&with(&|c| {
                c.as_object_mut().unwrap().remove("resource");
            })),
            CheckKind::ResourceNotBound
        );
        assert_eq!(
            kind(&with(&|c| c["scope"] = json!("issues:read admin"))),
            CheckKind::Scope
        );
        // A one-element array names the pinned audience too.
        check_exchange(&with(&|c| c["aud"] = json!([RAS])), &e, &EXPECT, 1100).unwrap();
        // The response's own members.
        let mut b = answer(&k, typ.clone(), &claims());
        b["issued_token_type"] = json!("urn:ietf:params:oauth:token-type:access_token");
        assert_eq!(kind(&b), CheckKind::IssuedTokenType);
        let mut b = answer(&k, typ.clone(), &claims());
        b["token_type"] = json!("Bearer");
        assert_eq!(kind(&b), CheckKind::TokenType);
        // A refresh token: a rotation of a refresh-token subject, refused
        // beside an ID token — and kept whatever the checks say of the
        // ID-JAG it came with.
        let mut b = answer(&k, typ.clone(), &claims());
        b["refresh_token"] = json!("rt-2");
        assert_eq!(kind(&b), CheckKind::UnexpectedRefreshToken);
        assert_eq!(rotation_of(&b, SubjectType::IdToken), None);
        let r = ex(SubjectType::RefreshToken, &scopes);
        check_exchange(&b, &r, &EXPECT, 1100).unwrap();
        assert_eq!(
            rotation_of(&b, SubjectType::RefreshToken).as_deref(),
            Some("rt-2")
        );
        let mut refused = answer(&k, json!({"typ": "JWT"}), &claims());
        refused["refresh_token"] = json!("rt-3");
        assert_eq!(
            check_exchange(&refused, &r, &EXPECT, 1100)
                .unwrap_err()
                .kind,
            CheckKind::Typ
        );
        assert_eq!(
            rotation_of(&refused, SubjectType::RefreshToken).as_deref(),
            Some("rt-3")
        );
        // Unbound, admitted only under allow_unbound_at.
        let unbound = with(&|c| {
            c.as_object_mut().unwrap().remove("resource");
        });
        let lax = Expect {
            allow_unbound: true,
            ..EXPECT
        };
        assert!(check_exchange(&unbound, &e, &lax, 1100).unwrap().unbound);
        // Pairwise subjects are recorded, not compared.
        let pairwise = Expect {
            sub: None,
            ..EXPECT
        };
        let other = with(&|c| c["sub"] = json!("pairwise-7"));
        assert_eq!(
            check_exchange(&other, &e, &pairwise, 1100).unwrap().sub,
            "pairwise-7"
        );
    }

    #[test]
    fn a_redemption_drops_its_refresh_token_and_holds_a_jwt_to_the_resource() {
        let ok = check_redeemed(
            &json!({"token_type": "Bearer", "access_token": "opaque", "expires_in": 3600, "refresh_token": "never-kept"}),
            RES,
        )
        .unwrap();
        assert_eq!(ok.access_token, "opaque");
        let k = key();
        let jwt = |aud: &str| {
            k.sign_jwt(json!({"typ": "at+jwt"}), &json!({"aud": aud}))
                .unwrap()
        };
        check_redeemed(
            &json!({"token_type": "bearer", "access_token": jwt(RES)}),
            RES,
        )
        .unwrap();
        assert_eq!(
            check_redeemed(
                &json!({"token_type": "Bearer", "access_token": jwt("https://x")}),
                RES
            )
            .unwrap_err()
            .kind,
            CheckKind::AccessTokenAudience
        );
        assert_eq!(
            check_redeemed(&json!({"token_type": "N_A", "access_token": "x"}), RES)
                .unwrap_err()
                .kind,
            CheckKind::TokenType
        );
    }

    /// Okta's org authorization server puts the key set in its OpenID
    /// document and the identity-chaining member in its RFC 8414 one; both
    /// are read.
    #[test]
    fn the_two_idp_documents_are_merged_as_okta_splits_them() {
        let asm = json!({"issuer": IDP, "token_endpoint": format!("{IDP}/oauth2/v1/token"),
            "device_authorization_endpoint": format!("{IDP}/oauth2/v1/device/authorize"),
            wire::METADATA_IDENTITY_CHAINING: [wire::TOKEN_TYPE_ID_JAG],
            "grant_types_supported": [wire::GRANT_TOKEN_EXCHANGE]});
        let oidc = json!({"issuer": IDP, "token_endpoint": format!("{IDP}/oauth2/v1/token"),
            "jwks_uri": format!("{IDP}/oauth2/v1/keys"),
            "authorization_endpoint": format!("{IDP}/oauth2/v1/authorize"),
            "code_challenge_methods_supported": ["S256"]});
        let m = merge_idp(IDP, Some(&asm), Some(&oidc), None).unwrap();
        assert_eq!(
            m.jwks_uri.as_deref(),
            Some("https://acme.okta.com/oauth2/v1/keys")
        );
        assert_eq!(m.lists_id_jag, Some(true));
        assert!(m.pkce_s256);
        assert_eq!(
            m.device_authorization_endpoint.as_deref(),
            Some("https://acme.okta.com/oauth2/v1/device/authorize")
        );
        // Either alone, the member is missing from what it does not hold.
        assert_eq!(
            merge_idp(IDP, None, Some(&oidc), None)
                .unwrap()
                .lists_id_jag,
            None
        );
        assert_eq!(
            merge_idp(IDP, Some(&asm), None, None).unwrap().jwks_uri,
            None
        );
        // A pin wins; a mismatched issuer, or two token endpoints, refuse.
        assert_eq!(
            merge_idp(IDP, Some(&asm), Some(&oidc), Some("https://pin/keys"))
                .unwrap()
                .jwks_uri
                .as_deref(),
            Some("https://pin/keys")
        );
        let mut bad = oidc.clone();
        bad["issuer"] = json!(format!("{IDP}/"));
        assert!(merge_idp(IDP, Some(&asm), Some(&bad), None).is_err());
        let mut other = oidc.clone();
        other["token_endpoint"] = json!("https://elsewhere.example/token");
        assert!(merge_idp(IDP, Some(&asm), Some(&other), None).is_err());
        assert!(merge_idp(IDP, None, None, None).is_err());
    }

    #[test]
    fn a_resource_authorization_server_must_take_the_id_jag_profile() {
        let doc = json!({"issuer": RAS, "token_endpoint": format!("{RAS}/token"),
            "grant_types_supported": [wire::GRANT_JWT_BEARER],
            wire::METADATA_GRANT_PROFILES: [wire::PROFILE_ID_JAG]});
        assert_eq!(
            ras_of(&doc, RAS, false).unwrap().token_endpoint,
            format!("{RAS}/token")
        );
        let mut no_profile = doc.clone();
        no_profile
            .as_object_mut()
            .unwrap()
            .remove(wire::METADATA_GRANT_PROFILES);
        assert!(ras_of(&no_profile, RAS, false).is_err());
        assert!(ras_of(&no_profile, RAS, true).is_ok(), "id_jag: assume");
        let mut no_grant = doc.clone();
        no_grant["grant_types_supported"] = json!(["client_credentials"]);
        assert!(ras_of(&no_grant, RAS, false).is_err());
        assert!(ras_of(&doc, "https://auth.linear.example/", false).is_err());
    }

    #[test]
    fn an_id_token_is_verified_with_the_idp_key_set() {
        let k = key();
        let mut jwk = k.public_jwk();
        jwk["kid"] = json!("k1");
        let k = k.with_kid("k1");
        let jwks = json!({"keys": [jwk]});
        let token = |c: Value| k.sign_jwt(json!({"typ": "JWT"}), &c).unwrap();
        let good = json!({"iss": IDP, "aud": "0oa-agent", "sub": "00u1alice", "iat": 1000,
                          "exp": 4000, "nonce": "n-1", "auth_time": 990});
        let c = verify_id_token(
            &token(good.clone()),
            &jwks,
            IDP,
            "0oa-agent",
            Some("n-1"),
            1100,
        )
        .unwrap();
        assert_eq!((c.sub.as_str(), c.auth_time), ("00u1alice", Some(990)));
        let fails = |f: &dyn Fn(&mut Value), nonce: Option<&str>| {
            let mut c = good.clone();
            f(&mut c);
            verify_id_token(&token(c), &jwks, IDP, "0oa-agent", nonce, 1100).is_err()
        };
        assert!(fails(&|c| c["iss"] = json!("https://evil"), None));
        assert!(fails(&|c| c["aud"] = json!("someone-else"), None));
        assert!(fails(&|c| c["exp"] = json!(900), None));
        assert!(fails(&|_| {}, Some("n-2")));
        // A token another key signed does not verify.
        let other = key().with_kid("k1");
        let forged = other.sign_jwt(json!({"typ": "JWT"}), &good).unwrap();
        assert!(verify_id_token(&forged, &jwks, IDP, "0oa-agent", None, 1100).is_err());
        // RS256, by an RSA key in the set.
        let rsa = crate::auth::client_assertion::RsaClientKey::from_pem(include_str!(
            "../../tests/fixtures/rsa2048-test-key.pem"
        ))
        .unwrap();
        let set = json!({"keys": [rsa.public_jwk()]});
        let t = rsa.sign_jwt(json!({"typ": "JWT"}), &good).unwrap();
        verify_id_token(&t, &set, IDP, "0oa-agent", None, 1100).unwrap();
    }
}
