// SPDX-License-Identifier: AGPL-3.0-only
//! The A2A listener as an OAuth resource server (RFC 0044 §5.9, P3):
//! `a2a.resource`, the resource identifier inbound access tokens are issued
//! for, and `a2a.inbound_tokens`, the enterprise authorization servers whose
//! JWT access tokens (RFC 9068) name a caller.
//!
//! Each issuer is pinned: its keys come only from the key set its own
//! metadata names (or one pinned beside it, on its origin), its algorithms
//! from an asymmetric allow-list, its tokens' `typ` from its profile, and the
//! clients that may call from a list. A token that passes names a principal
//! `user:oidc:<iss>#<subject>`; a token issued to a client alone
//! (`client_credentials`) never names a user.
//!
//! Also here: `webhooks.*.auth.jwt`, a webhook start a person's token signs,
//! verified by the same issuers with the webhook's own audience, a short
//! life and a `jti` that is never accepted twice.
//!
//! Parsed whatever the build; without the `a2a` and `oauth` features it is
//! refused by name.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer};

use super::{Diagnostics, Dur, Role, Settings};

/// How old an inbound token may be, by its `iat`, when the issuer names no
/// `max_token_age`.
pub const MAX_TOKEN_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// The longest `max_token_age` may be.
pub const MAX_TOKEN_AGE_CAP: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// How deep a token's `act` chain may nest, by default; a deeper one is
/// refused, never cut.
pub const MAX_CHAIN_DEPTH: u32 = 4;
/// The deepest `max_chain_depth` may say.
pub const MAX_CHAIN_DEPTH_CAP: u32 = 16;
/// How long a webhook's token may live, from `iat` to `exp`, by default.
pub const WEBHOOK_LIFETIME: std::time::Duration = std::time::Duration::from_secs(5 * 60);
/// The longest `max_lifetime` may say.
pub const WEBHOOK_LIFETIME_CAP: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// The asymmetric algorithms an issuer may sign with.
pub const ALGS: &[&str] = &["RS256", "ES256", "EdDSA"];
/// The scheme names the card already gives to other mechanisms.
const RESERVED_SCHEMES: &[&str] = &["bearer", "mtls", "device_code"];

/// `a2a.inbound_tokens[]`: one enterprise authorization server.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InboundIssuer {
    /// The issuer, pinned: its metadata's `issuer` must equal it byte for
    /// byte, and every token's `iss` too.
    pub issuer: String,
    /// The card's scheme name; unset, `enterprise` for the first issuer and
    /// `enterprise-<n>` for the n-th.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub profile: InboundProfile,
    /// The `aud` values a token may carry; unset, `a2a.resource` alone.
    #[serde(default)]
    pub audiences: Vec<String>,
    /// The key set, pinned on the issuer's origin; unset, the metadata's
    /// `jwks_uri`.
    #[serde(default)]
    pub jwks_uri: Option<String>,
    /// The algorithms a token may be signed with; unset, `[RS256, ES256]`.
    #[serde(default)]
    pub algs: Vec<String>,
    /// The JOSE `typ` a token may carry; unset, the profile's.
    #[serde(default)]
    pub typ: Option<TypRule>,
    /// The claim that names the person; unset, the profile's (`sub`, `uid`
    /// under `okta`).
    #[serde(default)]
    pub subject_claim: Option<String>,
    /// The claim that names a multi-tenant issuer's tenant.
    #[serde(default)]
    pub tenant_claim: Option<String>,
    /// The front-end clients (`azp`, `cid` or `client_id`) that may call.
    #[serde(default)]
    pub clients: Vec<String>,
    /// Refuse a token issued to a client alone; default true.
    #[serde(default)]
    pub require_user_token: Option<bool>,
    /// How old a token may be by its `iat`; default 1h.
    #[serde(default)]
    pub max_token_age: Option<Dur>,
    /// How deep its `act` chain may nest; default 4.
    #[serde(default)]
    pub max_chain_depth: Option<u32>,
    /// The flow the card declares for getting a token here; default
    /// `authorization_code`.
    #[serde(default)]
    pub flow: Option<InboundFlow>,
    /// The skills each scope lets its holder use: `conversation`,
    /// `workflow:<name glob>`, `command:<op>`. Empty, the scopes narrow
    /// nothing.
    #[serde(default)]
    pub scopes_to_skills: BTreeMap<String, Vec<String>>,
}

/// An issuer's quirks pack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundProfile {
    /// RFC 9068: `typ: at+jwt`, `sub`, `client_id` and `scope`.
    #[default]
    Generic,
    /// An Okta custom authorization server: no `typ` (or `at+jwt`), the
    /// person in `uid`, the client in `cid`, the scopes in `scp`.
    Okta,
}

/// The card's flow for an issuer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundFlow {
    AuthorizationCode,
    DeviceCode,
}

/// `typ`: `none` — a token may carry no `typ`, or `at+jwt` — or the list
/// of values it may carry (`at+jwt` is always one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypRule {
    None,
    Any(Vec<String>),
}

impl<'de> Deserialize<'de> for TypRule {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            One(String),
            List(Vec<String>),
        }
        match Raw::deserialize(d)? {
            Raw::One(s) if s == "none" => Ok(TypRule::None),
            Raw::One(s) => Err(serde::de::Error::custom(format!(
                "typ {s:?}: `none`, or a list of the values a token may carry"
            ))),
            Raw::List(v) => Ok(TypRule::Any(v)),
        }
    }
}

impl InboundIssuer {
    /// The JOSE `typ` rule in force.
    pub fn typ_rule(&self) -> TypRule {
        self.typ.clone().unwrap_or(match self.profile {
            InboundProfile::Generic => TypRule::Any(Vec::new()),
            InboundProfile::Okta => TypRule::None,
        })
    }

    /// The claim that names the person.
    pub fn subject_claim(&self) -> &str {
        self.subject_claim.as_deref().unwrap_or(match self.profile {
            InboundProfile::Generic => "sub",
            InboundProfile::Okta => "uid",
        })
    }

    /// The claim that names the calling client.
    pub fn client_claim(&self) -> &'static str {
        match self.profile {
            InboundProfile::Generic => "client_id",
            InboundProfile::Okta => "cid",
        }
    }

    /// The claim that carries the scopes, and whether it is a list (Okta's
    /// `scp`) rather than a space-separated string (RFC 9068's `scope`).
    pub fn scope_claim(&self) -> (&'static str, bool) {
        match self.profile {
            InboundProfile::Generic => ("scope", false),
            InboundProfile::Okta => ("scp", true),
        }
    }

    pub fn algs(&self) -> Vec<String> {
        if self.algs.is_empty() {
            vec!["RS256".into(), "ES256".into()]
        } else {
            self.algs.clone()
        }
    }

    pub fn require_user_token(&self) -> bool {
        self.require_user_token.unwrap_or(true)
    }

    pub fn max_token_age(&self) -> std::time::Duration {
        self.max_token_age.map_or(MAX_TOKEN_AGE, |d| d.0)
    }

    pub fn max_chain_depth(&self) -> u32 {
        self.max_chain_depth.unwrap_or(MAX_CHAIN_DEPTH)
    }

    pub fn flow(&self) -> InboundFlow {
        self.flow.unwrap_or(InboundFlow::AuthorizationCode)
    }

    /// The `aud` values a token may carry.
    pub fn audiences(&self, resource: Option<&str>) -> Vec<String> {
        if self.audiences.is_empty() {
            resource.map(str::to_string).into_iter().collect()
        } else {
            self.audiences.clone()
        }
    }

    /// The key set pinned by the profile, when the metadata is not to be
    /// asked: an Okta custom authorization server (`…/oauth2/<id>`) serves
    /// it at `{issuer}/v1/keys`. Any other issuer's comes from its metadata.
    pub fn jwks_pin(&self) -> Option<String> {
        self.jwks_uri.clone().or_else(|| {
            (self.profile == InboundProfile::Okta && self.issuer.contains("/oauth2/"))
                .then(|| format!("{}/v1/keys", self.issuer.trim_end_matches('/')))
        })
    }
}

/// The card's scheme name of the `i`-th issuer.
pub fn scheme_name(i: usize, t: &InboundIssuer) -> String {
    t.name.clone().unwrap_or_else(|| {
        if i == 0 {
            "enterprise".to_string()
        } else {
            format!("enterprise-{}", i + 1)
        }
    })
}

/// `webhooks.default_auth.jwt`, or a webhook start node's `auth.jwt`: the
/// start is signed by a person's token from one of `a2a.inbound_tokens`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookJwt {
    /// An `a2a.inbound_tokens` issuer.
    pub issuer: String,
    /// The webhook's own resource identifier: the token's `aud`, never the
    /// A2A listener's.
    pub audience: String,
    /// How long a token may live, `iat` to `exp`; default 5m, at most 15m.
    #[serde(default)]
    pub max_lifetime: Option<Dur>,
}

impl WebhookJwt {
    pub fn max_lifetime(&self) -> std::time::Duration {
        self.max_lifetime.map_or(WEBHOOK_LIFETIME, |d| d.0)
    }
}

/// Whether `skill` is a `scopes_to_skills` entry: `conversation`,
/// `workflow:<name glob>` or `command:<op>`.
fn skill_ok(skill: &str) -> bool {
    skill == "conversation"
        || skill
            .strip_prefix("workflow:")
            .is_some_and(|n| !n.trim().is_empty())
        || skill
            .strip_prefix("command:")
            .is_some_and(|n| !n.trim().is_empty())
}

/// Whether `name` is a scheme name a card may carry.
fn scheme_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether the inbound-token surface is declared at all.
pub fn declared(s: &Settings) -> bool {
    !s.a2a.inbound_tokens.is_empty()
        || s.a2a.resource.is_some()
        || s.a2a
            .principals
            .iter()
            .any(|p| p.matcher.token_iss.is_some())
}

/// The webhook JWT blocks the configuration names, with where.
fn webhook_jwts(s: &Settings) -> Vec<(String, WebhookJwt)> {
    let mut out = Vec::new();
    if let Some(j) = s.webhooks.default_auth.as_ref().and_then(|a| a.jwt.clone()) {
        out.push(("webhooks.default_auth.jwt".to_string(), j));
    }
    for w in &s.workflows {
        let name = w
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        for (id, step) in w
            .get("steps")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flatten()
        {
            if step.get("kind").and_then(serde_json::Value::as_str) != Some("webhook") {
                continue;
            }
            // A malformed block is the workflow's own validation's to name.
            if let Some(j) = step.pointer("/auth/jwt")
                && let Ok(j) = serde_json::from_value::<WebhookJwt>(j.clone())
            {
                out.push((format!("workflows.{name}.steps.{id}.auth.jwt"), j));
            }
        }
    }
    out
}

/// Every rule above, collected.
pub(crate) fn validate(s: &Settings, d: &mut Diagnostics) {
    let jwts = webhook_jwts(s);
    if !declared(s) && jwts.is_empty() {
        return;
    }
    let mut err = |m: String| d.errors.push(m);
    if !cfg!(all(feature = "a2a", feature = "oauth")) {
        err("a2a.resource, a2a.inbound_tokens, a2a.principals match.token_iss and a webhook's auth.jwt need the `a2a` and `oauth` features (build with --features a2a,oauth)".into());
        return;
    }
    let a = &s.a2a;
    let unix = a
        .listen
        .as_deref()
        .is_some_and(|l| crate::config::unix_socket_path(l).is_some());
    if !a.inbound_tokens.is_empty() {
        if a.listen.is_none() || unix {
            err("a2a.inbound_tokens needs a TCP A2A listener: a token is presented over HTTP, and a unix socket's peer is named by the kernel".into());
        }
        if a.resource.is_none() {
            err("a2a.inbound_tokens needs a2a.resource: the resource identifier every token's aud names".into());
        }
    } else if a.resource.is_some() {
        err("a2a.resource names the resource inbound tokens are issued for, and a2a.inbound_tokens names no issuer".into());
    }
    if let Some(r) = &a.resource {
        if !super::delegation::https_or_loopback(r)
            || r.contains('#')
            || r.ends_with('/')
            || super::canonical_resource(r).as_deref() != Some(r.as_str())
        {
            err(format!(
                "a2a.resource {r:?}: an https:// URI in canonical form (lowercase scheme and host, no trailing slash, no fragment; plain http only to a loopback host)"
            ));
        }
        if let (Some(url), Some(origin)) = (
            crate::runtime::surface::auth::configured_url(s),
            crate::runtime::surface::auth::origin_of(r),
        ) && crate::runtime::surface::auth::origin_of(&url).as_deref() != Some(origin.as_str())
        {
            err(format!(
                "a2a.resource {r:?} is not on the listener's origin {url:?}: its protected-resource metadata is served there"
            ));
        }
    }
    let mut issuers = BTreeSet::new();
    let mut names = BTreeSet::new();
    let resource = a.resource.as_deref();
    for (i, t) in a.inbound_tokens.iter().enumerate() {
        let at = format!("a2a.inbound_tokens[{i}]");
        if !super::delegation::https_or_loopback(&t.issuer) {
            err(format!(
                "{at}.issuer {:?} is not an https:// URL (plain http only to a loopback host)",
                t.issuer
            ));
        }
        if !issuers.insert(t.issuer.as_str()) {
            err(format!("{at}.issuer {:?} is listed twice", t.issuer));
        }
        let name = scheme_name(i, t);
        if !scheme_ok(&name) || RESERVED_SCHEMES.contains(&name.as_str()) {
            err(format!(
                "{at}.name {name:?}: the card's scheme name, 1..=64 of A-Z a-z 0-9 . _ - and none of {RESERVED_SCHEMES:?}"
            ));
        }
        if !names.insert(name.clone()) {
            err(format!(
                "{at}.name {name:?} is another issuer's scheme name"
            ));
        }
        if t.clients.is_empty() || t.clients.iter().any(|c| c.trim().is_empty()) {
            err(format!(
                "{at}.clients: the front-end clients (azp, cid or client_id) that may call — at least one; a token from any other is refused"
            ));
        }
        for aud in t.audiences(resource) {
            if aud.trim().is_empty() {
                err(format!("{at}.audiences: an empty audience"));
            }
            if t.clients.contains(&aud) {
                err(format!(
                    "{at}.audiences: {aud:?} is a client id, and a token whose aud is a client is an ID token, never an access token for this agent"
                ));
            }
        }
        for alg in t.algs() {
            match alg.as_str() {
                a if ALGS.contains(&a) => {}
                "none" => err(format!(
                    "{at}.algs: none — an unsigned token proves nothing"
                )),
                a if a.starts_with("HS") => err(format!(
                    "{at}.algs: {a} — an HMAC algorithm would let the issuer's public key verify as a shared secret"
                )),
                a => err(format!("{at}.algs: {a:?} (want {})", ALGS.join(", "))),
            }
        }
        if let Some(TypRule::Any(list)) = &t.typ
            && list.iter().any(|v| v.trim().is_empty())
        {
            err(format!("{at}.typ: an empty value"));
        }
        for (field, v) in [
            ("subject_claim", &t.subject_claim),
            ("tenant_claim", &t.tenant_claim),
        ] {
            if v.as_deref().is_some_and(|c| c.trim().is_empty()) {
                err(format!("{at}.{field}: an empty claim name"));
            }
        }
        if let Some(j) = &t.jwks_uri
            && (!super::delegation::https_or_loopback(j)
                || crate::runtime::surface::auth::origin_of(j)
                    != crate::runtime::surface::auth::origin_of(&t.issuer))
        {
            err(format!(
                "{at}.jwks_uri {j:?}: an https:// URL on the issuer's own origin"
            ));
        }
        if let Some(m) = t.max_token_age
            && (m.0.is_zero() || m.0 > MAX_TOKEN_AGE_CAP)
        {
            err(format!("{at}.max_token_age: between 1s and 24h"));
        }
        if t.max_chain_depth.is_some_and(|n| n > MAX_CHAIN_DEPTH_CAP) {
            err(format!(
                "{at}.max_chain_depth: at most {MAX_CHAIN_DEPTH_CAP}"
            ));
        }
        for (scope, skills) in &t.scopes_to_skills {
            if scope.trim().is_empty() || scope.contains(char::is_whitespace) {
                err(format!("{at}.scopes_to_skills: {scope:?} is not one scope"));
            }
            for k in skills {
                if !skill_ok(k) {
                    err(format!(
                        "{at}.scopes_to_skills.{scope}: {k:?} — conversation, workflow:<name glob> or command:<op>"
                    ));
                }
            }
        }
    }
    // Rules naming a token's issuer.
    for (i, p) in a.principals.iter().enumerate() {
        let Some(iss) = &p.matcher.token_iss else {
            continue;
        };
        let at = format!("a2a.principals[{i}]");
        if !a.inbound_tokens.iter().any(|t| &t.issuer == iss) {
            err(format!(
                "{at}.match.token_iss {iss:?} is no a2a.inbound_tokens issuer"
            ));
        }
        let m = &p.matcher;
        if m.san.is_some() || m.spki.is_some() || m.bearer_ref.is_some() || m.uid.is_some() || m.any
        {
            err(format!(
                "{at}.match.token_iss names an inbound token, and san, spki, bearer_ref, uid and any are other evidence — say token_iss, with sub to match the issuer's subject"
            ));
        }
        if p.id.is_some() {
            err(format!(
                "{at}.id: a token's principal is the issuer's subject, user:oidc:<iss>#<subject> — an id would merge every person the rule matches into one"
            ));
        }
        if p.idp_subject.is_some() {
            err(format!(
                "{at}.idp_subject: a token's principal is already the issuer's subject; its grant is found through identity.providers.<idp>.equivalent_issuers"
            ));
        }
        if !matches!(p.role, Role::User | Role::Agent) {
            err(format!(
                "{at}.role: a token names a user, or with require_user_token false an agent"
            ));
        }
    }
    // Issuer equivalence: an inbound issuer whose subject is a provider's.
    let mut claimed: BTreeMap<&str, &str> = BTreeMap::new();
    for (name, p) in &s.identity.providers {
        for e in &p.equivalent_issuers {
            let at = format!("identity.providers.{name}.equivalent_issuers");
            if !a.inbound_tokens.iter().any(|t| &t.issuer == e) {
                err(format!("{at}: {e:?} is no a2a.inbound_tokens issuer"));
            }
            if e == &p.issuer {
                err(format!(
                    "{at}: {e:?} is the provider's own issuer, its subjects its own already"
                ));
            }
            if let Some(other) = claimed.insert(e.as_str(), name.as_str())
                && other != name
            {
                err(format!(
                    "{at}: {e:?} is also identity.providers.{other}'s — an inbound subject is one provider's at most"
                ));
            }
        }
    }
    // Webhooks a person's token signs.
    for (at, j) in &jwts {
        for e in webhook_jwt_refusals(a, at, &j.issuer, &j.audience, j.max_lifetime()) {
            err(e);
        }
    }
}

/// What refuses a webhook route a person's token signs — issuer `issuer`,
/// audience `audience`, tokens living at most `max_lifetime` — at `at`: an
/// issuer `a2a.inbound_tokens` does not name, an audience a listener token
/// is issued for, a lifetime outside 1s..15m. Asked of the configuration at
/// load and of every route as it is built, whatever defined it.
pub fn webhook_jwt_refusals(
    a: &super::A2a,
    at: &str,
    issuer: &str,
    audience: &str,
    max_lifetime: std::time::Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    let resource = a.resource.as_deref();
    if !a.inbound_tokens.iter().any(|t| t.issuer == issuer) {
        out.push(format!(
            "{at}.issuer {issuer:?} is no a2a.inbound_tokens issuer"
        ));
    }
    let listener_aud = a
        .inbound_tokens
        .iter()
        .flat_map(|t| t.audiences(resource))
        .any(|x| x == audience);
    if audience.trim().is_empty() || listener_aud || Some(audience) == resource {
        out.push(format!(
            "{at}.audience {audience:?}: the webhook's own resource identifier, never one an A2A token is issued for — a token for the listener would otherwise start runs here too"
        ));
    }
    if max_lifetime.is_zero() || max_lifetime > WEBHOOK_LIFETIME_CAP {
        out.push(format!("{at}.max_lifetime: between 1s and 15m"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `typ` is `none` or a list, and the profile decides the rest.
    #[test]
    fn the_profile_decides_typ_subject_client_and_scope_claims() {
        let t: InboundIssuer = serde_json::from_value(serde_json::json!({
            "issuer": "https://acme.okta.com/oauth2/ausA2A", "profile": "okta", "clients": ["c"]
        }))
        .unwrap();
        assert_eq!(t.typ_rule(), TypRule::None);
        assert_eq!(t.subject_claim(), "uid");
        assert_eq!(t.client_claim(), "cid");
        assert_eq!(t.scope_claim(), ("scp", true));
        assert_eq!(
            t.jwks_pin().as_deref(),
            Some("https://acme.okta.com/oauth2/ausA2A/v1/keys")
        );
        // Okta's org server keeps its keys elsewhere: its metadata says.
        let org: InboundIssuer = serde_json::from_value(serde_json::json!({
            "issuer": "https://acme.okta.com", "profile": "okta", "clients": ["c"]
        }))
        .unwrap();
        assert_eq!(org.jwks_pin(), None);
        let g: InboundIssuer = serde_json::from_value(serde_json::json!({
            "issuer": "https://as.example", "typ": ["JWT"], "clients": ["c"]
        }))
        .unwrap();
        assert_eq!(g.typ_rule(), TypRule::Any(vec!["JWT".into()]));
        assert_eq!((g.subject_claim(), g.client_claim()), ("sub", "client_id"));
        assert_eq!(g.jwks_pin(), None);
        assert_eq!(g.audiences(Some("https://a/a2a")), vec!["https://a/a2a"]);
        assert!(
            serde_json::from_value::<InboundIssuer>(serde_json::json!({
                "issuer": "https://as.example", "typ": "JWT"
            }))
            .is_err()
        );
        assert_eq!(scheme_name(0, &g), "enterprise");
        assert_eq!(scheme_name(2, &g), "enterprise-3");
        assert!(
            skill_ok("workflow:tri*") && skill_ok("command:status") && skill_ok("conversation")
        );
        assert!(!skill_ok("triage") && !skill_ok("workflow:"));
    }
}
