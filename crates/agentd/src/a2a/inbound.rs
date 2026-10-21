// SPDX-License-Identifier: AGPL-3.0-only
//! **Inbound access tokens** (RFC 0044 §5.9, T19): the listener as an OAuth
//! resource server for the enterprise authorization servers
//! `a2a.inbound_tokens` names.
//!
//! agentd has no JOSE library: a token is checked here, on `ring`, rule by
//! rule, and every rule refuses rather than warns.
//!
//! - **Issuers** are the configured ones alone, matched by `iss` byte for
//!   byte. **Keys** come only from the key set the issuer's own metadata
//!   names (or one pinned on its origin): `jku`, `x5u`, `jwk` and `x5c` in
//!   a token's header are never read, and `kid` only picks among the pinned
//!   keys. A key whose `use` is not `sig`, or whose `alg` is another, is
//!   not used.
//! - **Algorithms** come from the issuer's allow-list of asymmetric ones
//!   (RS256, ES256, EdDSA): `none` and every HMAC algorithm are refused, so
//!   a public key can never verify as a shared secret. A `crit` header is
//!   refused: no extension is understood.
//! - **`typ`** is held to the issuer's profile: `at+jwt` (RFC 9068) by
//!   default; under `okta`, none or `at+jwt`; or the values configured.
//! - **Not an ID token**, whatever `typ` says: `aud` must be one of the
//!   issuer's audiences and name no client; the token must carry its
//!   profile's client and scope claims; a `nonce` or an `at_hash` refuses
//!   it.
//! - **`exp`, `nbf`, `iat`** with a minute's skew, and `iat` within
//!   `max_token_age`.
//! - **The client** (`client_id`, `cid`, or `azp`) must be listed; an `azp`
//!   naming another client refuses the token.
//! - **A user**: a `uid`, a `sub` that is not the client, or `sub_profile:
//!   user`. A token issued to a client alone never names a user — it is
//!   refused, or with `require_user_token: false` names an agent.
//! - **`act`**, nested, is recorded as attribution and refused, not cut,
//!   deeper than `max_chain_depth`.
//!
//! Keys and metadata are read with the one OAuth HTTP helper, never
//! following a redirect, cached, and read again when a token names a `kid`
//! the cached set lacks — at most once every [`REFETCH_EVERY`] per issuer.
//! An issuer whose metadata and keys are loaded is **ready**: its scheme is
//! on the card, and its tokens are checked. One that is not answers every
//! token of its with a 401 until it is.
//!
//! A webhook's token is checked the same way, for the webhook's own
//! audience, with a lifetime from `iat` to `exp` no longer than the
//! webhook's `max_lifetime` and a `jti` the replay cache has never seen.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use super::principals::typed::{PrincipalRef, Source};
use crate::auth::http::{self, Dial};
use crate::b64;
use crate::config::settings::Settings;
use crate::config::settings::inbound::{InboundFlow, InboundIssuer, TypRule};
use crate::obs::log::Logger;

/// How far a token's times may be off this clock.
const SKEW_S: u64 = 60;
/// The longest token read at all.
const MAX_TOKEN: usize = 16 * 1024;
/// How long a key set is used before it is read again.
const JWKS_TTL_MS: u64 = 60 * 60 * 1000;
/// How often an issuer's metadata or keys may be read again on demand.
pub const REFETCH_EVERY: Duration = Duration::from_secs(30);
/// The most webhook `jti`s remembered at once; past it the oldest expired
/// ones go first, and with none expired the token is refused.
const JTI_CAP: usize = 65_536;
pub use super::INBOUND_SID_PREFIX;

/// Why a token named nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Not a compact JWS, or none of the configured issuers': the bearer
    /// is no inbound token at all.
    NotOurs(String),
    /// The issuer's metadata or keys are not loaded, or no key has the
    /// token's `kid`: reading them again may help.
    Keys { issuer: String, why: String },
    /// Checked, and refused.
    Invalid { issuer: String, why: String },
}

impl Refused {
    /// What the log says.
    pub fn why(&self) -> &str {
        match self {
            Refused::NotOurs(w) => w,
            Refused::Keys { why, .. } | Refused::Invalid { why, .. } => why,
        }
    }
    pub fn issuer(&self) -> Option<&str> {
        match self {
            Refused::NotOurs(_) => None,
            Refused::Keys { issuer, .. } | Refused::Invalid { issuer, .. } => Some(issuer),
        }
    }
}

/// A token every rule passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub issuer: String,
    /// The card's scheme name of the issuer.
    pub scheme: String,
    /// The person, by the issuer's subject claim — or, for a token issued
    /// to a client alone, the client.
    pub subject: String,
    /// The client that called: `client_id`, `cid` or `azp`.
    pub client: String,
    pub tenant: Option<String>,
    /// Whether the token names a person rather than a client.
    pub user: bool,
    pub scopes: Vec<String>,
    /// The `act` chain, outermost first: who acts for whom.
    pub act: Vec<String>,
    pub exp_ms: u64,
    pub jti: Option<String>,
}

impl Verified {
    /// The typed principal the token names.
    pub fn typed(&self) -> PrincipalRef {
        PrincipalRef {
            source: Source::Oidc,
            issuer: self.issuer.clone(),
            tenant: self.tenant.clone(),
            stable_id: self.subject.clone(),
            client: Some(self.client.clone()),
        }
    }

    /// The principal id: `user:oidc:<iss>#<subject>` — under a tenant claim
    /// `user:oidc:<iss>#<tenant>/<subject>` — or for a client's own token
    /// `agent:oidc:<iss>#<client>`.
    pub fn principal_id(&self) -> String {
        if self.user {
            self.typed().display()
        } else {
            format!("agent:oidc:{}#{}", self.issuer, self.subject)
        }
    }

    /// The handle the caller's inbound session is kept under: the same for
    /// every token of one person through one client, so work they started
    /// with one token goes on with the next.
    pub fn sid(&self) -> String {
        self.sid_for(b"inbound")
    }

    /// The handle the person's webhook deliveries act through: theirs and
    /// their client's like [`Self::sid`], and apart from it, so a delivery
    /// never replaces what the session their A2A work acts through holds.
    pub fn webhook_sid(&self) -> String {
        self.sid_for(b"webhook")
    }

    fn sid_for(&self, purpose: &[u8]) -> String {
        let t = self.typed();
        let parts: [&[u8]; 5] = [
            purpose,
            t.issuer.as_bytes(),
            t.tenant.as_deref().unwrap_or_default().as_bytes(),
            t.stable_id.as_bytes(),
            self.client.as_bytes(),
        ];
        let mut enc = Vec::new();
        for p in parts {
            enc.extend_from_slice(&(p.len() as u32).to_be_bytes());
            enc.extend_from_slice(p);
        }
        format!(
            "{INBOUND_SID_PREFIX}{}",
            &crate::sha::sha256_hex(&enc)[..16]
        )
    }
}

/// What a token is checked as: an A2A caller's, for the listener's
/// audiences, or a webhook's, for its own audience and lifetime.
#[derive(Debug, Clone, Copy)]
pub enum For<'a> {
    Listener,
    Webhook {
        audience: &'a str,
        max_lifetime: Duration,
    },
}

/// An issuer's metadata, as far as this module reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    /// Where the document was read: the card's `oauth2MetadataUrl`.
    pub url: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub device_authorization_endpoint: Option<String>,
    pub jwks_uri: Option<String>,
    pub pkce_s256: bool,
}

#[derive(Default)]
struct State {
    meta: Option<Meta>,
    jwks: Option<Value>,
    fetched_ms: u64,
    tried_ms: Option<u64>,
    error: Option<String>,
}

impl State {
    /// The key set, while it is fresh enough to check a token with: what
    /// the card's scheme and the verifier both read, so the card declares
    /// an issuer exactly while its tokens can pass.
    fn keys(&self) -> Option<&Value> {
        self.jwks
            .as_ref()
            .filter(|_| now_ms().saturating_sub(self.fetched_ms) < JWKS_TTL_MS)
    }
}

struct Issuer {
    cfg: InboundIssuer,
    scheme: String,
    audiences: Vec<String>,
    state: Mutex<State>,
}

/// What the card declares of a ready issuer.
#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub name: String,
    pub metadata_url: String,
    pub flow: InboundFlow,
    pub authorization_url: Option<String>,
    pub token_url: Option<String>,
    pub device_authorization_url: Option<String>,
    /// Each scope, and what it lets its holder use.
    pub scopes: BTreeMap<String, String>,
}

/// The listener's inbound-token verifier: one per process, shared by the
/// listener and the webhook surface.
pub struct Inbound {
    issuers: Vec<Issuer>,
    resource: Option<String>,
    timeout: Duration,
    log: Logger,
    /// Webhook `jti`s seen, by issuer and value, with their `exp`.
    jtis: Mutex<HashMap<String, u64>>,
    /// The issuers whose tokens are kept for an exchange back at them
    /// (`grant: token_exchange` with `subject: inbound`, RFC 0044 §5.7).
    kept: Vec<String>,
}

fn now_ms() -> u64 {
    crate::state::now_ms()
}

impl Inbound {
    /// The verifier `settings` declare, or `None` with no issuer.
    pub fn build(settings: &Settings, log: &Logger) -> Option<std::sync::Arc<Inbound>> {
        let a = &settings.a2a;
        if a.inbound_tokens.is_empty() {
            return None;
        }
        let resource = a.resource.clone();
        let kept = crate::config::settings::delegation::delegated_servers(settings)
            .filter(|m| {
                m.auth.as_ref().is_some_and(|x| {
                    x.subject == Some(crate::config::settings::delegation::ExchangeSubject::Inbound)
                })
            })
            .filter_map(|m| {
                let idp = m.auth.as_ref()?.idp.as_deref()?;
                Some(settings.identity.providers.get(idp)?.issuer.clone())
            })
            .collect();
        Some(std::sync::Arc::new(Inbound {
            issuers: a
                .inbound_tokens
                .iter()
                .enumerate()
                .map(|(i, t)| Issuer {
                    scheme: crate::config::settings::inbound::scheme_name(i, t),
                    audiences: t.audiences(resource.as_deref()),
                    cfg: t.clone(),
                    state: Mutex::new(State::default()),
                })
                .collect(),
            resource,
            timeout: settings
                .mcp
                .default_timeout
                .map_or(Duration::from_secs(10), |d| {
                    d.0.min(Duration::from_secs(30))
                }),
            log: log.clone(),
            jtis: Mutex::new(HashMap::new()),
            kept,
        }))
    }

    /// The resource identifier tokens are issued for.
    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }

    /// The configuration of issuer `iss`.
    pub fn issuer(&self, iss: &str) -> Option<&InboundIssuer> {
        self.issuers
            .iter()
            .find(|i| i.cfg.issuer == iss)
            .map(|i| &i.cfg)
    }

    /// Whether a token of `iss` is kept for an exchange back at it.
    pub fn keeps(&self, iss: &str) -> bool {
        self.kept.iter().any(|k| k == iss)
    }

    /// Read every issuer's metadata and keys: at start, off the listener's
    /// threads.
    pub fn warm(&self) {
        for i in &self.issuers {
            let _ = self.fetch(i, true);
        }
    }

    /// Read `i`'s metadata (once) and keys (again): `force` ignores the
    /// refetch interval. What failed is kept and logged.
    fn fetch(&self, i: &Issuer, force: bool) -> Result<(), String> {
        {
            let mut st = i.state.lock().unwrap_or_else(|e| e.into_inner());
            let now = now_ms();
            if !force
                && st
                    .tried_ms
                    .is_some_and(|t| now.saturating_sub(t) < REFETCH_EVERY.as_millis() as u64)
            {
                return Err(st
                    .error
                    .clone()
                    .unwrap_or_else(|| "read again too recently".into()));
            }
            st.tried_ms = Some(now);
        }
        let meta = {
            let have = i
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .meta
                .clone();
            match have {
                Some(m) => Ok(m),
                None => self.read_meta(&i.cfg),
            }
        };
        let result = meta.and_then(|m| {
            let uri = i
                .cfg
                .jwks_pin()
                .or_else(|| m.jwks_uri.clone())
                .ok_or_else(|| format!("{}'s metadata names no jwks_uri", i.cfg.issuer))?;
            if crate::runtime::surface::auth::origin_of(&uri)
                != crate::runtime::surface::auth::origin_of(&i.cfg.issuer)
            {
                return Err(format!(
                    "{}'s key set {uri} is off its own origin",
                    i.cfg.issuer
                ));
            }
            let jwks = get_json(&uri, self.timeout)?;
            if !jwks["keys"].is_array() {
                return Err(format!("{uri} is no key set"));
            }
            Ok((m, jwks))
        });
        let mut st = i.state.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok((m, jwks)) => {
                let first = st.jwks.is_none();
                st.meta = Some(m);
                st.jwks = Some(jwks);
                st.fetched_ms = now_ms();
                st.error = None;
                if first {
                    self.log.info(
                        "a2a.inbound.ready",
                        json!({"issuer": i.cfg.issuer, "scheme": i.scheme}),
                    );
                }
                Ok(())
            }
            Err(e) => {
                self.log.warn(
                    "a2a.inbound.unavailable",
                    json!({"issuer": i.cfg.issuer, "err": e}),
                );
                st.error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The issuer's metadata: the first document of the RFC 8414 and OpenID
    /// discovery URLs that answers, its `issuer` the configured one byte
    /// for byte, every endpoint it names on the issuer's origin.
    fn read_meta(&self, cfg: &InboundIssuer) -> Result<Meta, String> {
        let mut last = format!("{} publishes no metadata", cfg.issuer);
        let mut found: Option<Meta> = None;
        for url in crate::auth::oauth2::discovery_urls(&cfg.issuer) {
            let doc = match get_json(&url, self.timeout) {
                Ok(d) => d,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            if doc["issuer"].as_str() != Some(cfg.issuer.as_str()) {
                return Err(format!(
                    "{url} names the issuer {}, not {:?}",
                    doc["issuer"], cfg.issuer
                ));
            }
            let s = |k: &str| doc[k].as_str().map(str::to_string);
            let origin = crate::runtime::surface::auth::origin_of(&cfg.issuer);
            for k in [
                "authorization_endpoint",
                "token_endpoint",
                "device_authorization_endpoint",
                "jwks_uri",
            ] {
                if let Some(u) = s(k)
                    && crate::runtime::surface::auth::origin_of(&u) != origin
                {
                    return Err(format!("{url}: {k} {u} is off the issuer's origin"));
                }
            }
            let pkce_s256 = doc["code_challenge_methods_supported"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m == "S256"));
            // The first document that answers is the issuer's; a later one
            // fills what it leaves out — Okta's org server names its key
            // set only in the OpenID document.
            match &mut found {
                None => {
                    found = Some(Meta {
                        url,
                        authorization_endpoint: s("authorization_endpoint"),
                        token_endpoint: s("token_endpoint"),
                        device_authorization_endpoint: s("device_authorization_endpoint"),
                        jwks_uri: s("jwks_uri"),
                        pkce_s256,
                    });
                }
                Some(m) => {
                    m.authorization_endpoint = m
                        .authorization_endpoint
                        .take()
                        .or(s("authorization_endpoint"));
                    m.token_endpoint = m.token_endpoint.take().or(s("token_endpoint"));
                    m.device_authorization_endpoint = m
                        .device_authorization_endpoint
                        .take()
                        .or(s("device_authorization_endpoint"));
                    m.jwks_uri = m.jwks_uri.take().or(s("jwks_uri"));
                    m.pkce_s256 |= pkce_s256;
                }
            }
            if found.as_ref().is_some_and(|m| m.jwks_uri.is_some()) {
                break;
            }
        }
        found.ok_or(last)
    }

    /// Check `token` with what is loaded, reading nothing: a refusal of
    /// kind [`Refused::Keys`] may pass once [`Self::verify_fetching`] has
    /// read the issuer again.
    pub fn verify(&self, token: &str, purpose: For<'_>) -> Result<Verified, Refused> {
        let iss = issuer_of(token)?;
        let Some(i) = self.issuers.iter().find(|i| i.cfg.issuer == iss) else {
            return Err(Refused::NotOurs(format!(
                "{iss:?} is no a2a.inbound_tokens issuer"
            )));
        };
        let jwks = {
            let st = i.state.lock().unwrap_or_else(|e| e.into_inner());
            match st.keys() {
                Some(k) => k.clone(),
                None => {
                    return Err(Refused::Keys {
                        issuer: iss,
                        why: st
                            .error
                            .clone()
                            .unwrap_or_else(|| "the issuer's keys are not loaded".into()),
                    });
                }
            }
        };
        let audiences = match purpose {
            For::Listener => i.audiences.clone(),
            For::Webhook { audience, .. } => vec![audience.to_string()],
        };
        let mut v = check(&i.cfg, &audiences, &jwks, token, now_ms() / 1000)?;
        v.scheme = i.scheme.clone();
        if let For::Webhook { max_lifetime, .. } = purpose {
            self.webhook_rules(&i.cfg.issuer, token, &v, max_lifetime)?;
        }
        Ok(v)
    }

    /// [`Self::verify`], reading the issuer's metadata and keys again first
    /// when they are not loaded or lack the token's key — at most once every
    /// [`REFETCH_EVERY`]. May wait on the network.
    pub fn verify_fetching(&self, token: &str, purpose: For<'_>) -> Result<Verified, Refused> {
        match self.verify(token, purpose) {
            Err(Refused::Keys { issuer, .. }) => {
                if let Some(i) = self.issuers.iter().find(|i| i.cfg.issuer == issuer) {
                    let _ = self.fetch(i, false);
                }
                self.verify(token, purpose)
            }
            r => r,
        }
    }

    /// A webhook's own rules: a `jti`, a lifetime no longer than allowed,
    /// and a `jti` never seen before — remembered until the token's `exp`.
    fn webhook_rules(
        &self,
        iss: &str,
        token: &str,
        v: &Verified,
        max_lifetime: Duration,
    ) -> Result<(), Refused> {
        let invalid = |why: String| Refused::Invalid {
            issuer: iss.to_string(),
            why,
        };
        let (_, claims) = parts_of(token).map_err(invalid)?;
        let jti = v
            .jti
            .as_deref()
            .filter(|j| !j.is_empty())
            .ok_or_else(|| invalid("a webhook's token must carry a jti".into()))?;
        let iat = claims["iat"].as_u64().unwrap_or(0);
        let exp = v.exp_ms / 1000;
        if exp.saturating_sub(iat) > max_lifetime.as_secs() {
            return Err(invalid(format!(
                "the token lives {}s, and a webhook's at most {}s",
                exp.saturating_sub(iat),
                max_lifetime.as_secs()
            )));
        }
        let key = format!("{iss}\u{0}{jti}");
        let now = now_ms();
        let mut seen = self.jtis.lock().unwrap_or_else(|e| e.into_inner());
        if seen.contains_key(&key) {
            return Err(invalid(format!("the jti {jti:?} was presented before")));
        }
        if seen.len() >= JTI_CAP {
            seen.retain(|_, exp| *exp + SKEW_S * 1000 > now);
            if seen.len() >= JTI_CAP {
                return Err(invalid(
                    "the replay cache is full of unexpired tokens".into(),
                ));
            }
        }
        seen.insert(key, v.exp_ms);
        Ok(())
    }

    /// The ready issuers' schemes, for the card: each whose metadata and
    /// keys are loaded — the keys as fresh as a token's check needs them —
    /// and whose metadata names its flow's endpoints.
    pub fn schemes(&self) -> Vec<Scheme> {
        let mut out = Vec::new();
        for i in &self.issuers {
            let st = i.state.lock().unwrap_or_else(|e| e.into_inner());
            let (Some(m), Some(_)) = (&st.meta, st.keys()) else {
                continue;
            };
            let flow = i.cfg.flow();
            let ok = match flow {
                InboundFlow::AuthorizationCode => {
                    m.authorization_endpoint.is_some() && m.token_endpoint.is_some()
                }
                InboundFlow::DeviceCode => {
                    m.device_authorization_endpoint.is_some() && m.token_endpoint.is_some()
                }
            };
            if !ok {
                continue;
            }
            out.push(Scheme {
                name: i.scheme.clone(),
                metadata_url: m.url.clone(),
                flow,
                authorization_url: m.authorization_endpoint.clone(),
                token_url: m.token_endpoint.clone(),
                device_authorization_url: m.device_authorization_endpoint.clone(),
                scopes: i
                    .cfg
                    .scopes_to_skills
                    .iter()
                    .map(|(s, k)| (s.clone(), format!("Use {}", k.join(", "))))
                    .collect(),
            });
        }
        out
    }

    /// The listener's RFC 9728 protected-resource metadata.
    pub fn resource_metadata(&self, name: &str) -> Value {
        let mut scopes: Vec<&String> = self
            .issuers
            .iter()
            .flat_map(|i| i.cfg.scopes_to_skills.keys())
            .collect();
        scopes.sort();
        scopes.dedup();
        let mut v = json!({
            "resource": self.resource,
            "authorization_servers": self.issuers.iter().map(|i| &i.cfg.issuer).collect::<Vec<_>>(),
            "bearer_methods_supported": ["header"],
            "resource_name": name,
        });
        if !scopes.is_empty() {
            v["scopes_supported"] = json!(scopes);
        }
        v
    }

    /// The scopes of issuer `iss` that would let a token use `skill`.
    pub fn scopes_for(&self, iss: &str, skill: &str) -> Vec<String> {
        self.issuer(iss)
            .map(|c| {
                c.scopes_to_skills
                    .iter()
                    .filter(|(_, ks)| ks.iter().any(|k| skill_matches(k, skill)))
                    .map(|(s, _)| s.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `v`'s scopes let its holder use `skill`: always, where the
    /// issuer maps no scope to a skill.
    pub fn covers(&self, v: &Verified, skill: &str) -> bool {
        self.scopes_cover(&v.issuer, &v.scopes, skill)
    }

    /// Whether `scopes`, a token of `iss`'s, let their holder use `skill`.
    pub fn scopes_cover(&self, iss: &str, scopes: &[String], skill: &str) -> bool {
        let Some(c) = self.issuer(iss) else {
            return false;
        };
        c.scopes_to_skills.is_empty()
            || scopes.iter().any(|s| {
                c.scopes_to_skills
                    .get(s)
                    .is_some_and(|ks| ks.iter().any(|k| skill_matches(k, skill)))
            })
    }
}

/// Whether the `scopes_to_skills` entry `entry` names `skill`: the same
/// word, or a `workflow:` entry whose name glob matches.
pub fn skill_matches(entry: &str, skill: &str) -> bool {
    match (
        entry.strip_prefix("workflow:"),
        skill.strip_prefix("workflow:"),
    ) {
        (Some(pat), Some(name)) => super::principals::glob(pat, name),
        _ => entry == skill,
    }
}

/// GET `url` as JSON, by the one OAuth HTTP helper: no redirect followed.
fn get_json(url: &str, timeout: Duration) -> Result<Value, String> {
    let r = http::get(url, &[("Accept", "application/json")], timeout, Dial::Named)?;
    if !r.is_success() {
        return Err(format!("{url}: HTTP {}", r.status));
    }
    serde_json::from_slice(&r.body).map_err(|e| format!("{url}: not JSON: {e}"))
}

/// Whether `s` is a JWS segment: base64url, no padding, not empty.
fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A compact JWS's header and claims, undecided.
fn parts_of(token: &str) -> Result<(Value, Value), String> {
    if token.len() > MAX_TOKEN {
        return Err("the token is too long".into());
    }
    let mut it = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (it.next(), it.next(), it.next(), it.next()) else {
        return Err("not a compact JWS".into());
    };
    if !segment_ok(h) || !segment_ok(c) || !(s.is_empty() || segment_ok(s)) {
        return Err("not a compact JWS".into());
    }
    let json = |seg: &str| -> Result<Value, String> {
        let bytes = b64::url_decode(seg).map_err(|e| format!("not base64url: {e}"))?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| "a JWS part is not a JSON object".to_string())
    };
    Ok((json(h)?, json(c)?))
}

/// The `iss` a token claims, read before any check to choose its issuer.
fn issuer_of(token: &str) -> Result<String, Refused> {
    let (_, c) = parts_of(token).map_err(Refused::NotOurs)?;
    c["iss"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Refused::NotOurs("the token names no iss".into()))
}

/// The key type an algorithm needs.
fn key_fits(alg: &str, k: &Value) -> bool {
    match alg {
        "RS256" => k["kty"] == "RSA",
        "ES256" => k["kty"] == "EC" && k["crv"] == "P-256",
        "EdDSA" => k["kty"] == "OKP" && k["crv"] == "Ed25519",
        _ => false,
    }
}

/// Verify `sig` over `input` with JWK `k` under `alg`.
fn verify_with(alg: &str, k: &Value, input: &[u8], sig: &[u8]) -> bool {
    let member = |m: &str| b64::url_decode(k[m].as_str().unwrap_or_default()).ok();
    use ring::signature::{
        ECDSA_P256_SHA256_FIXED, ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
        UnparsedPublicKey,
    };
    match alg {
        "RS256" => match (member("n"), member("e")) {
            (Some(n), Some(e)) if !n.is_empty() && !e.is_empty() => RsaPublicKeyComponents { n, e }
                .verify(&RSA_PKCS1_2048_8192_SHA256, input, sig)
                .is_ok(),
            _ => false,
        },
        "ES256" => match (member("x"), member("y")) {
            (Some(x), Some(y)) if x.len() == 32 && y.len() == 32 => {
                let mut point = vec![0x04];
                point.extend(x);
                point.extend(y);
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
                    .verify(input, sig)
                    .is_ok()
            }
            _ => false,
        },
        "EdDSA" => match member("x") {
            Some(x) if x.len() == 32 => UnparsedPublicKey::new(&ED25519, x)
                .verify(input, sig)
                .is_ok(),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `aud` (a string or an array) names one of `audiences`, and the
/// values it names.
fn aud_values(aud: &Value) -> Vec<String> {
    match aud {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Every rule of §5.9 a token of `cfg`'s issuer is held to, with its key
/// set `jwks` and the audiences it may name, at `now_s`. The scheme name is
/// the caller's to fill in.
pub fn check(
    cfg: &InboundIssuer,
    audiences: &[String],
    jwks: &Value,
    token: &str,
    now_s: u64,
) -> Result<Verified, Refused> {
    let issuer = cfg.issuer.clone();
    let invalid = |why: String| Refused::Invalid {
        issuer: issuer.clone(),
        why,
    };
    let (h, c) = parts_of(token).map_err(invalid)?;
    // The algorithm, before any key is looked at.
    let alg = h["alg"].as_str().unwrap_or_default();
    if alg.eq_ignore_ascii_case("none") {
        return Err(invalid("alg none: an unsigned token proves nothing".into()));
    }
    if alg.to_ascii_uppercase().starts_with("HS") {
        return Err(invalid(format!(
            "alg {alg}: an HMAC algorithm is never accepted — a public key is no shared secret"
        )));
    }
    if !cfg.algs().iter().any(|a| a == alg) {
        return Err(invalid(format!(
            "alg {alg:?} is not among the issuer's algs {:?}",
            cfg.algs()
        )));
    }
    if h.get("crit").is_some() {
        return Err(invalid(
            "a crit header names extensions agentd does not understand".into(),
        ));
    }
    // The type, by the issuer's profile.
    let typ = h["typ"]
        .as_str()
        .map(|t| t.trim_start_matches("application/").to_ascii_lowercase());
    let typ_ok = match (cfg.typ_rule(), typ.as_deref()) {
        (_, Some("at+jwt")) => true,
        (TypRule::None, None) => true,
        (TypRule::None, Some(_)) => false,
        (TypRule::Any(list), Some(t)) => list.iter().any(|v| v.eq_ignore_ascii_case(t)),
        (TypRule::Any(_), None) => false,
    };
    if !typ_ok {
        return Err(invalid(format!(
            "typ {:?} is not an access token's under the issuer's profile",
            h["typ"]
        )));
    }
    if c["iss"].as_str() != Some(cfg.issuer.as_str()) {
        return Err(invalid(format!("iss {} is not {:?}", c["iss"], cfg.issuer)));
    }
    // The signature, by a pinned key alone.
    let (input, sig) = token
        .rsplit_once('.')
        .ok_or_else(|| invalid("not a compact JWS".into()))?;
    let sig = b64::url_decode(sig).map_err(|e| invalid(format!("signature: {e}")))?;
    let keys = jwks["keys"].as_array().cloned().unwrap_or_default();
    let kid = h["kid"].as_str();
    let candidates: Vec<&Value> = keys
        .iter()
        .filter(|k| kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)))
        .filter(|k| key_fits(alg, k))
        .filter(|k| k.get("use").is_none_or(|u| u == "sig"))
        .filter(|k| k.get("alg").is_none_or(|a| a == alg))
        .collect();
    if candidates.is_empty() {
        return Err(Refused::Keys {
            issuer: cfg.issuer.clone(),
            why: format!("no key of the issuer's set verifies {alg} with kid {kid:?}"),
        });
    }
    if !candidates
        .iter()
        .any(|k| verify_with(alg, k, input.as_bytes(), &sig))
    {
        return Err(invalid(
            "the signature does not verify with the issuer's keys".into(),
        ));
    }
    // The times.
    let exp = c["exp"].as_u64().ok_or_else(|| invalid("no exp".into()))?;
    if exp + SKEW_S <= now_s {
        return Err(invalid("the token has expired".into()));
    }
    if c["nbf"].as_u64().is_some_and(|nbf| nbf > now_s + SKEW_S) {
        return Err(invalid("the token is not valid yet (nbf)".into()));
    }
    let iat = c["iat"].as_u64().ok_or_else(|| invalid("no iat".into()))?;
    if iat > now_s + SKEW_S {
        return Err(invalid("the token was issued in the future".into()));
    }
    if now_s.saturating_sub(iat) > cfg.max_token_age().as_secs() {
        return Err(invalid(format!(
            "the token is older than max_token_age ({}s)",
            cfg.max_token_age().as_secs()
        )));
    }
    // An access token for this agent, never an ID token.
    if c.get("nonce").is_some() || c.get("at_hash").is_some() {
        return Err(invalid(
            "a nonce or an at_hash marks an ID token, which is no access token".into(),
        ));
    }
    let client = c[cfg.client_claim()]
        .as_str()
        .or_else(|| c["azp"].as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid(format!(
                "no {} claim: an access token names its client",
                cfg.client_claim()
            ))
        })?
        .to_string();
    if c["azp"].as_str().is_some_and(|azp| azp != client) {
        return Err(invalid(format!(
            "azp {} is another client than {client:?}",
            c["azp"]
        )));
    }
    let aud = aud_values(&c["aud"]);
    if aud.iter().any(|a| *a == client || cfg.clients.contains(a)) {
        return Err(invalid(
            "aud names a client: an ID token's audience, never an access token's for this agent"
                .into(),
        ));
    }
    if !aud.iter().any(|a| audiences.contains(a)) {
        return Err(invalid(format!("aud {aud:?} names none of {audiences:?}")));
    }
    if !cfg.clients.contains(&client) {
        return Err(invalid(format!(
            "client {client:?} is not among the issuer's clients"
        )));
    }
    let (scope_claim, list) = cfg.scope_claim();
    let scopes: Vec<String> = match (&c[scope_claim], list) {
        (Value::Array(a), true) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        (Value::String(s), false) => s.split_whitespace().map(str::to_string).collect(),
        _ => {
            return Err(invalid(format!(
                "no {scope_claim} claim of the profile's shape: an access token names its scopes"
            )));
        }
    };
    // A person, or a client alone.
    let sub = c["sub"].as_str();
    let profile_user = match &c["sub_profile"] {
        Value::String(s) => s.split_whitespace().any(|p| p == "user"),
        Value::Array(a) => a.iter().any(|p| p == "user"),
        _ => false,
    };
    let user = c["uid"].as_str().is_some_and(|u| !u.is_empty())
        || sub.is_some_and(|s| !s.is_empty() && s != client)
        || profile_user;
    let subject = if user {
        c[cfg.subject_claim()]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid(format!("no {} claim names the person", cfg.subject_claim())))?
            .to_string()
    } else if cfg.require_user_token() {
        return Err(invalid(
            "the token was issued to a client alone, and names no user (require_user_token)".into(),
        ));
    } else {
        client.clone()
    };
    let tenant = match &cfg.tenant_claim {
        Some(k) => Some(
            c[k.as_str()]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid(format!("no {k} claim names the tenant")))?
                .to_string(),
        ),
        None => None,
    };
    // The actor chain, recorded, bounded, never cut.
    let mut act = Vec::new();
    let mut cur = c.get("act");
    while let Some(a) = cur {
        let s = a["sub"]
            .as_str()
            .ok_or_else(|| invalid("an act claim names no sub".into()))?;
        act.push(s.to_string());
        if act.len() as u32 > cfg.max_chain_depth() {
            return Err(invalid(format!(
                "the act chain is deeper than max_chain_depth ({})",
                cfg.max_chain_depth()
            )));
        }
        cur = a.get("act");
    }
    Ok(Verified {
        issuer: cfg.issuer.clone(),
        scheme: String::new(),
        subject,
        client,
        tenant,
        user,
        scopes,
        act,
        exp_ms: exp * 1000,
        jti: c["jti"].as_str().map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client_assertion::{ClientKey, RsaClientKey};

    const ISS: &str = "https://acme.okta.com/oauth2/ausA2A";
    const AUD: &str = "https://agentd.acme.example/a2a";
    const NOW: u64 = 1_800_000_000;

    fn okta() -> InboundIssuer {
        serde_json::from_value(json!({"issuer": ISS, "profile": "okta",
            "clients": ["0oaChat"], "max_chain_depth": 2}))
        .unwrap()
    }

    fn generic() -> InboundIssuer {
        serde_json::from_value(json!({"issuer": ISS, "clients": ["0oaChat"],
            "algs": ["ES256", "RS256", "EdDSA"]}))
        .unwrap()
    }

    fn es() -> ClientKey {
        ClientKey::from_pkcs8(&ClientKey::generate_pkcs8().unwrap())
            .unwrap()
            .with_kid("k1")
    }

    fn okta_claims() -> Value {
        json!({"iss": ISS, "aud": AUD, "sub": "alice@acme.example", "uid": "00u1alice",
               "cid": "0oaChat", "scp": ["agentd:triage"], "iat": NOW - 10, "exp": NOW + 600,
               "jti": "j1"})
    }

    fn generic_claims() -> Value {
        json!({"iss": ISS, "aud": [AUD], "sub": "00u1alice", "client_id": "0oaChat",
               "scope": "agentd:triage openid", "iat": NOW - 10, "exp": NOW + 600})
    }

    fn keys(k: &ClientKey) -> Value {
        let mut jwk = k.public_jwk();
        jwk["kid"] = json!("k1");
        json!({"keys": [jwk]})
    }

    fn go(cfg: &InboundIssuer, jwks: &Value, token: &str) -> Result<Verified, Refused> {
        check(cfg, &[AUD.to_string()], jwks, token, NOW)
    }

    fn signed(k: &ClientKey, header: Value, claims: &Value) -> String {
        k.sign_jwt(header, claims).unwrap()
    }

    fn why(r: Result<Verified, Refused>) -> String {
        match r {
            Ok(v) => panic!("accepted: {v:?}"),
            Err(e) => e.why().to_string(),
        }
    }

    /// An Okta custom-AS token — no `typ`, the person in `uid`, `cid` and
    /// `scp` — and an RFC 9068 one both pass, naming the person.
    #[test]
    fn a_token_of_either_profile_names_its_person() {
        let k = es();
        let v = go(&okta(), &keys(&k), &signed(&k, json!({}), &okta_claims())).unwrap();
        assert_eq!(
            (v.subject.as_str(), v.client.as_str(), v.user),
            ("00u1alice", "0oaChat", true)
        );
        assert_eq!(v.scopes, vec!["agentd:triage"]);
        assert_eq!(v.principal_id(), format!("user:oidc:{ISS}#00u1alice"));
        assert!(v.sid().starts_with(INBOUND_SID_PREFIX));
        let g = go(
            &generic(),
            &keys(&k),
            &signed(&k, json!({"typ": "at+jwt"}), &generic_claims()),
        )
        .unwrap();
        assert_eq!(g.subject, "00u1alice");
        assert_eq!(g.scopes, vec!["agentd:triage", "openid"]);
        // One handle per person and client, whatever token.
        let mut other = generic_claims();
        other["jti"] = json!("j2");
        let g2 = go(
            &generic(),
            &keys(&k),
            &signed(&k, json!({"typ": "application/at+jwt"}), &other),
        )
        .unwrap();
        assert_eq!(g.sid(), g2.sid());
    }

    /// Every algorithm the RFC lists verifies with its own key type: an RSA
    /// key, an Ed25519 key, a P-256 key — and none verifies another's.
    #[test]
    fn rs256_es256_and_eddsa_verify_with_their_own_keys() {
        let rsa = RsaClientKey::from_pem(include_str!("../../tests/fixtures/rsa2048-test-key.pem"))
            .unwrap()
            .with_kid("r1");
        let mut rjwk = rsa.public_jwk();
        rjwk["kid"] = json!("r1");
        let rs = rsa
            .sign_jwt(json!({"typ": "at+jwt"}), &generic_claims())
            .unwrap();
        assert!(go(&generic(), &json!({"keys": [rjwk]}), &rs).is_ok());
        // Ed25519: signed here with ring, its key an OKP JWK.
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let rng = ring::rand::SystemRandom::new();
        let doc = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let ed = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let header = b64::url_nopad(br#"{"alg":"EdDSA","typ":"at+jwt","kid":"e1"}"#);
        let body = b64::url_nopad(generic_claims().to_string().as_bytes());
        let input = format!("{header}.{body}");
        let tok = format!(
            "{input}.{}",
            b64::url_nopad(ed.sign(input.as_bytes()).as_ref())
        );
        let ejwk = json!({"kty": "OKP", "crv": "Ed25519", "kid": "e1",
                          "x": b64::url_nopad(ed.public_key().as_ref())});
        assert!(go(&generic(), &json!({"keys": [ejwk.clone()]}), &tok).is_ok());
        // EdDSA is not in an issuer's default list.
        let default: InboundIssuer =
            serde_json::from_value(json!({"issuer": ISS, "clients": ["0oaChat"]})).unwrap();
        assert!(why(go(&default, &json!({"keys": [ejwk]}), &tok)).contains("not among"));
        // An ES256 key does not verify the RSA token.
        let k = es();
        let mut swapped = keys(&k);
        swapped["keys"][0]["kid"] = json!("r1");
        assert!(matches!(
            go(&generic(), &swapped, &rs),
            Err(Refused::Keys { .. })
        ));
    }

    /// The algorithm and key rules (T19): none and HMAC are refused before
    /// a key is looked at; a key the issuer did not publish, a `jku`/`jwk`
    /// header, a key of another `use` or `alg`, and an unknown `kid` verify
    /// nothing.
    #[test]
    fn none_hmac_and_foreign_keys_are_refused() {
        let k = es();
        let set = keys(&k);
        let claims = okta_claims();
        let body = b64::url_nopad(claims.to_string().as_bytes());
        let unsigned = format!("{}.{body}.", b64::url_nopad(br#"{"alg":"none"}"#));
        assert!(why(go(&okta(), &set, &unsigned)).contains("alg none"));
        let hs_in = format!(
            "{}.{body}",
            b64::url_nopad(br#"{"alg":"HS256","kid":"k1"}"#)
        );
        let mac = crate::sha::hmac_sha256(set.to_string().as_bytes(), hs_in.as_bytes());
        let hs = format!("{hs_in}.{}", b64::url_nopad(&mac));
        assert!(why(go(&okta(), &set, &hs)).contains("HMAC"));
        // Signed by a key the issuer never published, naming the issuer's
        // kid, its own public half riding in the header: the header is not
        // read, and the pinned key does not verify it.
        let evil = es();
        let header = json!({"jwk": evil.public_jwk(), "jku": "https://evil.example/keys",
                            "x5u": "https://evil.example/cert"});
        let forged = signed(&evil, header.clone(), &claims);
        assert!(why(go(&okta(), &set, &forged)).contains("does not verify"));
        // Under a kid the set lacks, nothing verifies.
        let stranger = es().with_kid("nope");
        assert!(matches!(
            go(&okta(), &set, &signed(&stranger, header, &claims)),
            Err(Refused::Keys { .. })
        ));
        let mut enc = set.clone();
        enc["keys"][0]["use"] = json!("enc");
        assert!(matches!(
            go(&okta(), &enc, &signed(&k, json!({}), &claims)),
            Err(Refused::Keys { .. })
        ));
        let mut other_alg = set.clone();
        other_alg["keys"][0]["alg"] = json!("RS256");
        assert!(matches!(
            go(&okta(), &other_alg, &signed(&k, json!({}), &claims)),
            Err(Refused::Keys { .. })
        ));
        let crit = signed(&k, json!({"crit": ["exp"]}), &claims);
        assert!(why(go(&okta(), &set, &crit)).contains("crit"));
        assert!(matches!(
            go(&okta(), &set, "a.b"),
            Err(Refused::Invalid { .. })
        ));
    }

    /// `typ` by profile: generic wants `at+jwt`; Okta takes none or
    /// `at+jwt`; neither takes another kind of JWT; a list takes what it
    /// names.
    #[test]
    fn typ_is_held_to_the_issuers_profile() {
        let k = es();
        let set = keys(&k);
        for typ in ["JWT", "oauth-id-jag+jwt", "logout+jwt", "dpop+jwt"] {
            let t = signed(&k, json!({"typ": typ}), &okta_claims());
            assert!(why(go(&okta(), &set, &t)).contains("typ"), "{typ}");
        }
        assert!(
            go(
                &okta(),
                &set,
                &signed(&k, json!({"typ": "at+jwt"}), &okta_claims())
            )
            .is_ok()
        );
        assert!(
            why(go(
                &generic(),
                &set,
                &signed(&k, json!({}), &generic_claims())
            ))
            .contains("typ")
        );
        let mut listed = generic();
        listed.typ = Some(TypRule::Any(vec!["JWT".into()]));
        assert!(
            go(
                &listed,
                &set,
                &signed(&k, json!({"typ": "JWT"}), &generic_claims())
            )
            .is_ok()
        );
        assert!(go(&listed, &set, &signed(&k, json!({}), &generic_claims())).is_err());
    }

    /// `iss`, `aud`, `exp`, `nbf`, `iat` and `max_token_age`.
    #[test]
    fn issuer_audience_and_times_are_checked() {
        let k = es();
        let set = keys(&k);
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = okta_claims();
            f(&mut c);
            go(&okta(), &set, &signed(&k, json!({}), &c))
        };
        assert!(why(with(&|c| c["iss"] = json!("https://acme.okta.com"))).contains("iss"));
        assert!(why(with(&|c| c["aud"] = json!("https://elsewhere.example"))).contains("aud"));
        assert!(why(with(&|c| c["exp"] = json!(NOW - 61))).contains("expired"));
        assert!(with(&|c| c["exp"] = json!(NOW - 30)).is_ok(), "within skew");
        assert!(
            why(with(&|c| {
                c.as_object_mut().unwrap().remove("exp");
            }))
            .contains("no exp")
        );
        assert!(why(with(&|c| c["nbf"] = json!(NOW + 300))).contains("nbf"));
        assert!(why(with(&|c| c["iat"] = json!(NOW + 300))).contains("future"));
        assert!(why(with(&|c| c["iat"] = json!(NOW - 7200))).contains("max_token_age"));
    }

    /// An ID token presented as an access token is refused with and without
    /// `typ`: its `aud` is a client, it carries a `nonce` or an `at_hash`,
    /// or it lacks the profile's client and scope claims.
    #[test]
    fn an_id_token_is_no_access_token() {
        let k = es();
        let set = keys(&k);
        let id_token = json!({"iss": ISS, "aud": "0oaChat", "sub": "00u1alice",
            "iat": NOW - 10, "exp": NOW + 600, "nonce": "n", "at_hash": "h"});
        for header in [json!({}), json!({"typ": "JWT"}), json!({"typ": "at+jwt"})] {
            assert!(
                go(&okta(), &set, &signed(&k, header.clone(), &id_token)).is_err(),
                "{header}"
            );
        }
        let mut c = okta_claims();
        c["aud"] = json!([AUD, "0oaChat"]);
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("aud names a client"));
        let mut c = okta_claims();
        c["nonce"] = json!("n");
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("ID token"));
        let mut c = okta_claims();
        c.as_object_mut().unwrap().remove("scp");
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("scp"));
        let mut c = okta_claims();
        c.as_object_mut().unwrap().remove("cid");
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("cid"));
    }

    /// A client not listed, and an `azp` naming another, are refused; a
    /// `client_credentials` token never names a user — refused, or an
    /// agent with `require_user_token: false`.
    #[test]
    fn clients_are_listed_and_a_clients_own_token_is_no_user() {
        let k = es();
        let set = keys(&k);
        let mut c = okta_claims();
        c["cid"] = json!("0oaOther");
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("not among"));
        let mut c = okta_claims();
        c["azp"] = json!("0oaOther");
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &c))).contains("azp"));
        // Okta's client_credentials: sub is the client, no uid.
        let cc = json!({"iss": ISS, "aud": AUD, "sub": "0oaChat", "cid": "0oaChat",
            "scp": ["agentd:triage"], "iat": NOW - 10, "exp": NOW + 600});
        assert!(why(go(&okta(), &set, &signed(&k, json!({}), &cc))).contains("client alone"));
        let mut agents = okta();
        agents.require_user_token = Some(false);
        let v = go(&agents, &set, &signed(&k, json!({}), &cc)).unwrap();
        assert!(!v.user);
        assert_eq!(v.principal_id(), format!("agent:oidc:{ISS}#0oaChat"));
        // sub_profile says so.
        let mut sp = cc.clone();
        sp["sub_profile"] = json!("user");
        sp["uid"] = json!("00u1alice");
        assert!(go(&okta(), &set, &signed(&k, json!({}), &sp)).unwrap().user);
    }

    /// The tenant claim, and an `act` chain recorded within its depth and
    /// refused beyond it.
    #[test]
    fn the_tenant_and_the_act_chain() {
        let k = es();
        let set = keys(&k);
        let mut t = okta();
        t.tenant_claim = Some("tid".into());
        let mut c = okta_claims();
        assert!(why(go(&t, &set, &signed(&k, json!({}), &c))).contains("tid"));
        c["tid"] = json!("acme");
        c["act"] = json!({"sub": "agent-a", "act": {"sub": "agent-b"}});
        let v = go(&t, &set, &signed(&k, json!({}), &c)).unwrap();
        assert_eq!(v.tenant.as_deref(), Some("acme"));
        assert_eq!(v.act, vec!["agent-a", "agent-b"]);
        // One subject in two tenants is two people: their principals, and
        // so their tasks and conversations, are apart.
        assert_eq!(v.principal_id(), format!("user:oidc:{ISS}#acme/00u1alice"));
        c["tid"] = json!("globex");
        let other = go(&t, &set, &signed(&k, json!({}), &c)).unwrap();
        assert_eq!(other.subject, v.subject);
        assert_ne!(other.principal_id(), v.principal_id());
        assert_ne!(other.sid(), v.sid());
        c["act"] = json!({"sub": "a", "act": {"sub": "b", "act": {"sub": "c"}}});
        assert!(why(go(&t, &set, &signed(&k, json!({}), &c))).contains("max_chain_depth"));
    }

    /// The card declares an issuer exactly while its tokens can pass: once
    /// its key set is older than a check takes, the scheme goes with it.
    #[test]
    fn the_card_names_an_issuer_while_its_keys_check_tokens() {
        use crate::obs::log::{Comp, Level, LogCtx, Logger};
        let settings: Settings = serde_json::from_value(json!({"a2a": {
            "resource": AUD, "inbound_tokens": [{"issuer": ISS, "profile": "okta", "clients": ["0oaChat"]}]}}))
        .unwrap();
        let log = Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 1,
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
            },
            Level::Error,
        );
        let inbound = Inbound::build(&settings, &log).unwrap();
        assert!(inbound.schemes().is_empty(), "nothing loaded yet");
        let k = es();
        let load = |at: u64| {
            let mut st = inbound.issuers[0].state.lock().unwrap();
            st.meta = Some(Meta {
                url: format!("{ISS}/.well-known/oauth-authorization-server"),
                authorization_endpoint: Some(format!("{ISS}/v1/authorize")),
                token_endpoint: Some(format!("{ISS}/v1/token")),
                device_authorization_endpoint: None,
                jwks_uri: Some(format!("{ISS}/v1/keys")),
                pkce_s256: true,
            });
            st.jwks = Some(keys(&k));
            st.fetched_ms = at;
        };
        load(now_ms());
        assert_eq!(inbound.schemes().len(), 1);
        load(now_ms() - JWKS_TTL_MS - 1);
        assert!(inbound.schemes().is_empty(), "stale keys check nothing");
        let token = signed(&k, json!({}), &okta_claims());
        assert!(matches!(
            inbound.verify(&token, For::Listener),
            Err(Refused::Keys { .. })
        ));
    }

    /// A scope covers a skill its entry names; with no mapping every skill
    /// is covered.
    #[test]
    fn scopes_cover_the_skills_they_name() {
        assert!(skill_matches("workflow:tri*", "workflow:triage"));
        assert!(!skill_matches("workflow:tri*", "workflow:deploy"));
        assert!(skill_matches("conversation", "conversation"));
        assert!(!skill_matches("command:status", "command:admin.drain"));
    }
}
