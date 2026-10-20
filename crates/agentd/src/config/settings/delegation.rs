// SPDX-License-Identifier: AGPL-3.0-only
//! Acting for one enrolled person at MCP servers through their enterprise
//! identity provider (RFC 0044 P1): `identity.providers`, the IdPs agentd is
//! a client of; `identity.delegation`, the seal key, the token caps and the
//! templates that say who may enrol for what; `security.experimental.id_jag`;
//! and the fields an `mcp.servers[].auth` block of `grant: id_jag` adds.
//! All are parsed whatever the build; without the `xaa` feature they are
//! refused by name.
//!
//! **Who may delegate in P1.** One per-person operator: an `a2a.principals`
//! rule matching the unix A2A listener's peer by `uid`, with an `id` and an
//! exact `idp_subject` pin. That caller is `operator:<id>`; its runs, and no
//! one else's, use the grant its `agentd --login idp:<name>` enrolled. A
//! certificate principal with a pin, a device session and an IdP-federated
//! login delegate from P2.
//!
//! **What validation refuses** (RFC 0044 §5.14, each by the code the RFC
//! gives it): a delegated server without its `idp`, `audience` and
//! `resource` pins (`id_jag_missing_pin`) or without a trifecta tag
//! (`id_jag_untagged`); a delegated server on a listener that does not
//! authenticate its callers (`delegation_requires_authenticated_listener`);
//! the same resource or authorization server under a delegated and an
//! agent-mode server (`mode_mixing`); the IdP's key or `kid` reused at a
//! resource's authorization server (`key_reuse`); a key under `identity.*` or
//! on a delegated server that would resolve from the process environment
//! (`delegation_secret_in_environment`); `exec` beside a delegated server;
//! a public client; an IdP whose issuer is the resource's authorization
//! server (ID-JAG §9.3); and, under `profile: okta`, an `/oauth2/<id>`
//! issuer, from which Okta issues no ID-JAG.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::Value;

use super::{
    AuthKind, ClientAuth, ClientAuthMethod, Diagnostics, Dur, OAuthGrant, Role, Secret, Settings,
};

/// How long an access token is used at most, whatever `expires_in` says.
pub const AT_CACHE_CAP: std::time::Duration = std::time::Duration::from_secs(60 * 60);
/// How often an active grant's refresh token is proved alive.
pub const LIVENESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15 * 60);
/// How long a consent gate waits for the person.
pub const CONSENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
/// The longest a grant lives, and its default.
pub const MAX_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);
/// Default mint budgets (RFC 0044 §5.5): twice one fully active day, 80% of
/// Okta's 250 a month, an 8-hour run.
pub const PER_DAY: u32 = 16;
pub const PER_MONTH: u32 = 200;
pub const PER_RUN: u32 = 12;

/// `identity.providers.<name>`: an enterprise IdP agentd is a client of.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// The IdP's issuer, pinned: never discovered from an MCP server.
    pub issuer: String,
    #[serde(default)]
    pub profile: Profile,
    /// agentd's client at the IdP (Okta: the AI agent's client ID).
    pub client_id: String,
    /// How agentd authenticates at the IdP's token endpoint; unset,
    /// `private_key_jwt` with no key, which validation refuses.
    #[serde(default)]
    pub client_auth: Option<ClientAuth>,
    /// The secret `client_secret_basic` and `client_secret_post` send.
    #[serde(default)]
    pub client_secret: Option<Secret>,
    #[serde(default)]
    pub sign_in: SignIn,
    /// What the token exchange presents as its subject; unset, the profile's.
    #[serde(default)]
    pub subject: Option<SubjectKind>,
    /// Whether the IdP's subjects are the same at every resource; unset,
    /// `public`.
    #[serde(default)]
    pub subjects: Option<Subjects>,
    /// Where the IdP's signing keys are, when not where its OpenID metadata
    /// says; under `profile: okta`, `{issuer}/oauth2/v1/keys`.
    #[serde(default)]
    pub jwks_uri: Option<String>,
}

/// A provider's quirks pack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Generic,
    /// Okta: ID-JAGs come only from the org authorization server, its keys
    /// are at `/oauth2/v1/keys`, and the exchange presents the refresh token.
    Okta,
}

/// How the person signs in at enrolment.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct SignIn {
    /// The grants agentd may start, in the order it tries them; default
    /// `[device]`.
    pub grants: Vec<SignInGrant>,
    /// Default `[openid, offline_access]`.
    pub scopes: Vec<String>,
}

/// An enrolment grant at the IdP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInGrant {
    /// RFC 8628, with `verification_uri` and `user_code` shown and never the
    /// complete URI: allowed because P1's person is pinned, so a code
    /// forwarded to someone else fails the binding.
    Device,
    /// Authorization code with PKCE, through a one-time start link on a
    /// loopback callback the supervisor holds.
    AuthorizationCode,
}

impl SignInGrant {
    pub fn as_str(self) -> &'static str {
        match self {
            SignInGrant::Device => "device",
            SignInGrant::AuthorizationCode => "authorization_code",
        }
    }
}

/// The token exchange's subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// The sealed refresh token itself.
    RefreshToken,
    /// A fresh ID token from a `refresh_token` grant with no `scope`.
    IdToken,
}

/// Whether an IdP's subject identifiers are global.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subjects {
    /// The ID-JAG's `sub` must equal the grant's.
    Public,
    /// It is recorded, not compared.
    Pairwise,
}

impl Provider {
    /// The subject the exchange presents.
    pub fn subject(&self) -> SubjectKind {
        self.subject.unwrap_or(match self.profile {
            Profile::Okta => SubjectKind::RefreshToken,
            Profile::Generic => SubjectKind::IdToken,
        })
    }

    pub fn subjects(&self) -> Subjects {
        self.subjects.unwrap_or(Subjects::Public)
    }

    /// The pinned key set, when there is one.
    pub fn jwks_pin(&self) -> Option<String> {
        self.jwks_uri.clone().or_else(|| {
            (self.profile == Profile::Okta)
                .then(|| format!("{}/oauth2/v1/keys", self.issuer.trim_end_matches('/')))
        })
    }

    pub fn grants(&self) -> Vec<SignInGrant> {
        if self.sign_in.grants.is_empty() {
            vec![SignInGrant::Device]
        } else {
            self.sign_in.grants.clone()
        }
    }

    pub fn scopes(&self) -> Vec<String> {
        if self.sign_in.scopes.is_empty() {
            vec!["openid".into(), "offline_access".into()]
        } else {
            self.sign_in.scopes.clone()
        }
    }
}

/// `identity.delegation`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Delegation {
    /// 32 random bytes as a secret reference: every grant's refresh token is
    /// sealed with it. `key_custody: local` only.
    pub seal_key: Option<Secret>,
    /// The key before a rotation: read, never written with.
    pub seal_key_previous: Option<Secret>,
    /// Who holds the seal key and every `private_key_jwt` key under
    /// `identity.providers` and on a delegated server: the supervisor, read
    /// from secret references (`local`, the default), or an external signer
    /// it asks (`external`, with `signer`).
    pub key_custody: KeyCustody,
    /// The external signer, under `key_custody: external`.
    pub signer: Option<SignerCfg>,
    /// The longest an access token is used, whatever the server says;
    /// default 60m.
    pub at_cache_cap: Option<Dur>,
    /// How often an active grant's refresh token is proved alive; an access
    /// token is used only within this of the last proof. Default 15m.
    pub liveness_interval: Option<Dur>,
    /// How long a consent gate waits before it denies; default 24h.
    pub consent_timeout: Option<Dur>,
    /// Who may enrol, for what. No template matching, no enrolment.
    pub templates: Vec<Template>,
}

/// `identity.delegation.key_custody`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCustody {
    /// The keys are secret references the supervisor reads.
    #[default]
    Local,
    /// An external signer holds them; agentd never does.
    External,
}

/// `identity.delegation.signer`: the external signer (RFC 0044 P2a,
/// `docs/external-signer.md`).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignerCfg {
    /// The signer's unix socket, an absolute path.
    pub socket: String,
    /// The uid the signer process runs as: the socket's peer must be it.
    pub uid: u32,
    /// The bound on one request; default 5s.
    #[serde(default)]
    pub timeout: Option<Dur>,
    /// The signer's key-encryption key each grant's data key is wrapped
    /// under.
    pub seal_key: String,
    /// The key-encryption key before a rotation: unwraps, never wraps.
    #[serde(default)]
    pub seal_key_previous: Option<String>,
}

impl SignerCfg {
    /// The signer client this names.
    #[cfg(feature = "oauth")]
    pub fn signer(&self) -> crate::sec::signer::Signer {
        crate::sec::signer::Signer::new(&self.socket, self.uid, self.timeout.map(|d| d.0))
    }
}

impl Delegation {
    /// The signer every key is asked of, under `key_custody: external`.
    pub fn external(&self) -> Option<&SignerCfg> {
        match self.key_custody {
            KeyCustody::External => self.signer.as_ref(),
            KeyCustody::Local => None,
        }
    }

    pub fn at_cache_cap(&self) -> std::time::Duration {
        self.at_cache_cap.map_or(AT_CACHE_CAP, |d| d.0)
    }
    pub fn liveness_interval(&self) -> std::time::Duration {
        self.liveness_interval.map_or(LIVENESS_INTERVAL, |d| d.0)
    }
    pub fn consent_timeout(&self) -> std::time::Duration {
        self.consent_timeout.map_or(CONSENT_TIMEOUT, |d| d.0)
    }
}

/// `identity.delegation.templates[]`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Template {
    pub name: String,
    pub principals: TemplateMatch,
    /// The one IdP its principals bind to.
    pub idp: String,
    /// Each delegated server it allows, with its scope ceiling.
    pub servers: BTreeMap<String, TemplateServer>,
    /// The grant's life; default and maximum 30d.
    #[serde(default)]
    pub ttl: Option<Dur>,
    #[serde(default)]
    pub budget: TemplateBudget,
}

impl Template {
    pub fn ttl(&self) -> std::time::Duration {
        self.ttl.map_or(MAX_TTL, |d| d.0.min(MAX_TTL))
    }
}

/// Which principals a template admits: every field it names must match.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TemplateMatch {
    /// The principal's source; P1 admits `operator` alone.
    pub source: Option<String>,
    pub role: Option<Role>,
    pub labels: BTreeMap<String, String>,
}

/// A template's ceiling on one server.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TemplateServer {
    pub scopes: Vec<String>,
}

/// A template's mint budgets.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct TemplateBudget {
    pub per_day: Option<u32>,
    pub per_month: Option<u32>,
    pub per_run: Option<u32>,
}

impl TemplateBudget {
    pub fn per_day(&self) -> u32 {
        self.per_day.unwrap_or(PER_DAY)
    }
    pub fn per_month(&self) -> u32 {
        self.per_month.unwrap_or(PER_MONTH)
    }
    pub fn per_run(&self) -> u32 {
        self.per_run.unwrap_or(PER_RUN)
    }
}

/// `security.experimental`.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Experimental {
    /// `grant: id_jag` servers, until ID-JAG reaches working-group last call.
    pub id_jag: bool,
}

/// An `idp_subject` pin: the one IdP identity a principal is.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdpSubject {
    pub iss: String,
    pub sub: String,
}

/// `auth.scope_mode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMode {
    /// One token per server, minted with the union of `scopes` and every
    /// `tool_scopes` entry.
    #[default]
    Server,
    /// One token per distinct scope set.
    PerTool,
}

/// `auth.tools_from`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolsFrom {
    /// Listed with the first delegated token, then kept.
    #[default]
    FirstGrant,
    /// `tools_file`, a `tools/list` result.
    File,
}

/// `auth.id_jag`: how the resource's authorization server is held to EMA.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdJagMode {
    /// Its metadata must list the ID-JAG profile and the JWT bearer grant.
    #[default]
    Discover,
    /// Its metadata omits the profile key; trusted to take it anyway.
    Assume,
}

/// `auth.id_jag_reuse`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdJagReuse {
    /// Present an unexpired ID-JAG again after a 401, until the server
    /// refuses one.
    #[default]
    Auto,
    Never,
}

/// `auth.on_revoked`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnRevoked {
    #[default]
    Fail,
    /// Suspend and ask the person to enrol again: P2.
    Ask,
}

/// `auth.attribution`: what `agent/acting_for` carries on a delegated call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    /// The ID-JAG's own `sub`.
    #[default]
    Sub,
    None,
}

/// A key algorithm for `private_key_jwt`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum KeyAlg {
    #[default]
    ES256,
    RS256,
}

impl KeyAlg {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyAlg::ES256 => "ES256",
            KeyAlg::RS256 => "RS256",
        }
    }
}

/// Whether `s` declares anything this module is about.
fn declared(s: &Settings) -> bool {
    !s.identity.providers.is_empty()
        || s.identity.delegation != Delegation::default()
        || s.security.experimental.id_jag
        || delegated_servers(s).next().is_some()
        || s.a2a
            .principals
            .iter()
            .any(|p| p.idp_subject.is_some() || p.matcher.uid.is_some())
}

/// The servers whose `auth` is `grant: id_jag`.
pub fn delegated_servers(s: &Settings) -> impl Iterator<Item = &super::McpServer> {
    s.mcp.servers.iter().filter(|m| is_delegated(m))
}

/// Whether `m` is a delegated server.
pub fn is_delegated(m: &super::McpServer) -> bool {
    m.auth
        .as_ref()
        .is_some_and(|a| a.grant == Some(OAuthGrant::IdJag))
}

/// The per-person operator rule, when there is one.
pub fn person(s: &Settings) -> Option<&super::Principal> {
    s.a2a
        .principals
        .iter()
        .find(|p| p.matcher.uid.is_some() && p.idp_subject.is_some())
}

/// Every secret reference under `identity.*` and on a delegated server: the
/// supervisor's alone, each as what it resolves to
/// ([`crate::sec::secret::canonical_ref`]). A document may reference none
/// of them, by any spelling, and no child is started with the variables
/// they name.
pub fn reserved_refs(s: &Settings) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    let mut add = |t: &str| {
        refs.extend(
            crate::sec::secret::refs_of(t)
                .iter()
                .map(|r| crate::sec::secret::canonical_ref(r)),
        );
    };
    for p in s.identity.providers.values() {
        if let Some(k) = p.client_auth.as_ref().and_then(|c| c.key.as_ref()) {
            add(&k.0);
        }
        if let Some(c) = &p.client_secret {
            add(&c.0);
        }
    }
    for k in [
        &s.identity.delegation.seal_key,
        &s.identity.delegation.seal_key_previous,
    ]
    .into_iter()
    .flatten()
    {
        add(&k.0);
    }
    for m in delegated_servers(s) {
        for v in m.headers.values() {
            add(v);
        }
        if let Some(a) = &m.auth {
            for sec in [&a.client_secret, &a.token, &a.value].into_iter().flatten() {
                add(&sec.0);
            }
            if let Some(k) = a.client_auth.as_ref().and_then(|c| c.key.as_ref()) {
                add(&k.0);
            }
        }
    }
    refs
}

/// The same, read from an operator's configuration document before it is
/// typed — what a served document's fragment is checked against at load.
pub fn reserved_refs_of_doc(doc: &Value) -> BTreeSet<String> {
    fn strings(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::String(s) => out.extend(
                crate::sec::secret::refs_of(s)
                    .iter()
                    .map(|r| crate::sec::secret::canonical_ref(r)),
            ),
            Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
            Value::Object(o) => o.values().for_each(|x| strings(x, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    if let Some(i) = doc.get("identity") {
        for k in ["providers", "delegation"] {
            if let Some(v) = i.get(k) {
                strings(v, &mut out);
            }
        }
    }
    for srv in doc
        .pointer("/mcp/servers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if srv.pointer("/auth/grant").and_then(Value::as_str) == Some("id_jag") {
            for k in ["auth", "headers"] {
                if let Some(v) = srv.get(k) {
                    strings(v, &mut out);
                }
            }
        }
    }
    out
}

/// The `mcp.servers[]` auth fields a served document may not write (RFC
/// 0044 §5.14.1 as it amends RFC 0042): the delegation pins and their
/// client authentication. `grant` is refused only when its value is
/// `id_jag` or `token_exchange`.
pub const SERVER_AUTH_OPERATOR_ONLY: &[&str] = &[
    "idp",
    "audience",
    "resource",
    "client_auth",
    "tool_scopes",
    "scope_mode",
    "tools_from",
    "tools_file",
    "id_jag",
    "id_jag_reuse",
    "on_revoked",
    "allow_unbound_at",
    "attribution",
    "max_at_age",
];

/// What a served document's configuration `fragment` sets that only the
/// operator may: a delegated server (`document_may_not_declare_delegation`),
/// a delegation field of any server's `auth`, and any reference to one of
/// the operator's `reserved` secrets — each named as the document wrote it.
/// A reference is compared by what it resolves to, and a file a `*_file`
/// field names (a `jwt_svid_file` is sent as a bearer token) as a
/// `{{secret-file:…}}` of that path, so no other spelling of a reserved
/// secret reaches it.
pub fn document_declared(
    fragment: &serde_json::Map<String, Value>,
    reserved: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (i, srv) in fragment
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(auth) = srv.get("auth").and_then(Value::as_object) else {
            continue;
        };
        if let Some(g) = auth.get("grant").and_then(Value::as_str)
            && matches!(g, "id_jag" | "token_exchange")
        {
            out.push(format!(
                "mcp.servers[{i}].auth.grant: {g} (document_may_not_declare_delegation)"
            ));
        }
        for f in SERVER_AUTH_OPERATOR_ONLY {
            if auth.contains_key(*f) {
                out.push(format!("mcp.servers[{i}].auth.{f}"));
            }
        }
    }
    fn walk(v: &Value, at: &str, reserved: &BTreeSet<String>, out: &mut Vec<String>) {
        use crate::sec::secret::{canonical_path, canonical_ref, refs_of};
        match v {
            Value::String(s) => {
                let named_file = at
                    .rsplit('.')
                    .next()
                    .is_some_and(|k| k.ends_with("_file"))
                    .then(|| format!("secret-file:{}", canonical_path(s)));
                if refs_of(s)
                    .iter()
                    .map(|r| canonical_ref(r))
                    .chain(named_file)
                    .any(|r| reserved.contains(&r))
                {
                    out.push(format!("{at} (a reserved supervisor-only secret)"));
                }
            }
            Value::Array(a) => {
                for (i, x) in a.iter().enumerate() {
                    walk(x, &format!("{at}[{i}]"), reserved, out);
                }
            }
            Value::Object(o) => {
                for (k, x) in o {
                    let p = if at.is_empty() {
                        k.clone()
                    } else {
                        format!("{at}.{k}")
                    };
                    walk(x, &p, reserved, out);
                }
            }
            _ => {}
        }
    }
    if !reserved.is_empty() {
        walk(&Value::Object(fragment.clone()), "", reserved, &mut out);
    }
    out
}

/// Whether `url` is HTTPS, or plain HTTP to a loopback host — a development
/// IdP or authorization server on this machine.
fn https_or_loopback(url: &str) -> bool {
    if let Some(rest) = url.strip_prefix("https://") {
        return !rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Refuse a `{{secret:NAME}}` that would resolve from the environment: the
/// environment block stays readable in `/proc/<pid>/environ` and is handed
/// to children. A `{{secret-file:…}}`, or a value entered at the startup
/// prompt (`--prompt-missing`), is what a delegation key is read from.
fn env_sourced(field: &str, sec: &Secret, err: &mut dyn FnMut(String)) {
    for name in crate::sec::secret::env_refs_of(&sec.0) {
        if crate::sec::secret::prompted_of(&name).is_none() && std::env::var_os(&name).is_some() {
            err(format!(
                "{field}: {{{{secret:{name}}}}} resolves from the process environment (delegation_secret_in_environment) — read it from a mounted file ({{{{secret-file:…}}}}) or enter it at the startup prompt (--prompt-missing)"
            ));
        }
    }
}

/// A client authentication's key or secret, as an inline value or an
/// environment-sourced one: both refused.
fn check_client_secrets(
    at: &str,
    client_auth: Option<&ClientAuth>,
    client_secret: Option<&Secret>,
    err: &mut dyn FnMut(String),
) {
    for (field, sec) in [
        ("client_auth.key", client_auth.and_then(|c| c.key.as_ref())),
        ("client_secret", client_secret),
    ] {
        let Some(sec) = sec else { continue };
        if !crate::sec::secret::has_secret_ref(&sec.0) {
            err(format!(
                "{at}.{field} carries an inline credential; use a {{{{secret-file:…}}}} reference"
            ));
        }
        env_sourced(&format!("{at}.{field}"), sec, err);
    }
}

/// Whether a client authentication proves anything: a key — one the
/// signer holds, under `key_custody: external` — or a secret.
fn confidential(
    client_auth: Option<&ClientAuth>,
    client_secret: Option<&Secret>,
    external: bool,
) -> bool {
    match client_auth.map(|c| c.method) {
        None => client_secret.is_some(),
        Some(ClientAuthMethod::PrivateKeyJwt) => {
            client_auth.is_some_and(|c| c.key.is_some() || (external && c.kid.is_some()))
        }
        Some(_) => client_secret.is_some(),
    }
}

/// A `private_key_jwt` key as its custody gives it: a `key` reference under
/// `local`; under `external`, no key at all, and a `kid` naming the
/// signer's key.
fn check_custody(at: &str, c: Option<&ClientAuth>, external: bool, err: &mut dyn FnMut(String)) {
    let Some(c) = c.filter(|c| c.method == ClientAuthMethod::PrivateKeyJwt) else {
        return;
    };
    match (external, c.key.is_some(), c.kid.is_some()) {
        (true, true, _) => err(format!(
            "{at}.client_auth.key: under identity.delegation.key_custody external the signer holds the key — name it by kid, and give no key"
        )),
        (true, false, false) => err(format!(
            "{at}.client_auth.kid is required under identity.delegation.key_custody external: the signer's key id, which the assertion names"
        )),
        (false, false, _) => err(format!(
            "{at}.client_auth: private_key_jwt needs `key` (a {{{{secret-file:…}}}} PKCS#8 key)"
        )),
        _ => {}
    }
}

/// Every rule above, collected.
pub(crate) fn validate(s: &Settings, d: &mut Diagnostics) {
    let mut err = |m: String| d.errors.push(m);
    if !declared(s) {
        return;
    }
    if !cfg!(feature = "xaa") {
        err("identity.providers, identity.delegation, security.experimental.id_jag, `grant: id_jag` and a2a.principals idp_subject/uid need the `xaa` feature (build with --features xaa)".into());
        return;
    }
    let mut warn = Vec::new();
    let dl = &s.identity.delegation;
    let external = dl.key_custody == KeyCustody::External;
    // The providers.
    for (name, p) in &s.identity.providers {
        let at = format!("identity.providers.{name}");
        if !https_or_loopback(&p.issuer) {
            err(format!(
                "{at}.issuer {:?} is not an https:// URL (plain http only to a loopback host)",
                p.issuer
            ));
        }
        if p.client_id.trim().is_empty() {
            err(format!(
                "{at}.client_id is required: agentd's client at the IdP"
            ));
        }
        if let Some(j) = &p.jwks_uri
            && !https_or_loopback(j)
        {
            err(format!("{at}.jwks_uri {j:?} is not an https:// URL"));
        }
        check_custody(&at, p.client_auth.as_ref(), external, &mut err);
        if let Some(c) = &p.client_auth {
            match c.method {
                ClientAuthMethod::PrivateKeyJwt if p.client_secret.is_some() => err(format!(
                    "{at}: client_secret beside a key-based client_auth — say one"
                )),
                ClientAuthMethod::ClientSecretBasic | ClientAuthMethod::ClientSecretPost
                    if p.client_secret.is_none() =>
                {
                    err(format!(
                        "{at}.client_auth: {:?} needs client_secret",
                        c.method
                    ))
                }
                _ => {}
            }
        }
        if !confidential(p.client_auth.as_ref(), p.client_secret.as_ref(), external) {
            err(format!(
                "{at}: a public client cannot exchange for an ID-JAG (ID-JAG §9.1) — give it client_auth with a key, or a client_secret"
            ));
        }
        check_client_secrets(
            &at,
            p.client_auth.as_ref(),
            p.client_secret.as_ref(),
            &mut err,
        );
        if !p.scopes().iter().any(|x| x == "openid") {
            err(format!(
                "{at}.sign_in.scopes needs openid: enrolment binds the ID token's iss and sub"
            ));
        }
        if !p.scopes().iter().any(|x| x == "offline_access") {
            warn.push(format!(
                "{at}.sign_in.scopes has no offline_access: an IdP that needs it (Okta does) issues no refresh token, and enrolment fails"
            ));
        }
        if p.profile == Profile::Okta && p.issuer.contains("/oauth2/") {
            err(format!(
                "{at}.issuer {:?}: Okta issues ID-JAGs only from the org authorization server (an issuer without /oauth2/<id>)",
                p.issuer
            ));
        }
    }
    // The delegation section.
    match (external, &dl.signer) {
        (true, None) => err(
            "identity.delegation.key_custody external needs identity.delegation.signer {socket, uid, seal_key}"
                .into(),
        ),
        (false, Some(_)) => err(
            "identity.delegation.signer is read under key_custody: external alone — say key_custody: external, or drop the signer"
                .into(),
        ),
        (true, Some(sg)) => {
            if !std::path::Path::new(&sg.socket).is_absolute() {
                err(format!(
                    "identity.delegation.signer.socket {:?} is not an absolute path to the signer's unix socket",
                    sg.socket
                ));
            }
            // SAFETY: geteuid has no failure mode.
            if sg.uid == unsafe { libc::geteuid() } {
                warn.push(format!(
                    "identity.delegation.signer.uid {} is agentd's own: the peer check cannot tell the signer from a process of agentd's uid, which can also read a same-uid signer's memory and key files — run the signer as another uid",
                    sg.uid
                ));
            }
            if let Some(t) = sg.timeout
                && (t.0.is_zero() || t.0 > std::time::Duration::from_secs(60))
            {
                err("identity.delegation.signer.timeout: between 1ms and 60s".into());
            }
            if sg.seal_key.trim().is_empty() || sg.seal_key_previous.as_deref() == Some(sg.seal_key.as_str()) {
                err("identity.delegation.signer.seal_key names the signer's key-encryption key, and seal_key_previous another one".into());
            }
            for (field, k) in [
                ("seal_key", &dl.seal_key),
                ("seal_key_previous", &dl.seal_key_previous),
            ] {
                if k.is_some() {
                    err(format!(
                        "identity.delegation.{field}: under key_custody external the signer holds the key-encryption key — name it as identity.delegation.signer.{field}, and give no key"
                    ));
                }
            }
        }
        (false, None) => {}
    }
    for (field, k) in [
        ("identity.delegation.seal_key", &dl.seal_key),
        (
            "identity.delegation.seal_key_previous",
            &dl.seal_key_previous,
        ),
    ] {
        let Some(k) = k else { continue };
        if !crate::sec::secret::has_secret_ref(&k.0) {
            err(format!(
                "{field} carries an inline key; use a {{{{secret-file:…}}}} reference"
            ));
        }
        env_sourced(field, k, &mut err);
    }
    for (field, v) in [
        ("at_cache_cap", dl.at_cache_cap),
        ("liveness_interval", dl.liveness_interval),
        ("consent_timeout", dl.consent_timeout),
    ] {
        if let Some(d) = v
            && d.0.is_zero()
        {
            err(format!(
                "identity.delegation.{field}: 0 would never let a token or a gate live"
            ));
        }
    }
    let mut names = BTreeSet::new();
    for (i, t) in dl.templates.iter().enumerate() {
        let at = format!("identity.delegation.templates[{i}]");
        if t.name.trim().is_empty() || !names.insert(t.name.as_str()) {
            err(format!("{at}.name {:?} is empty or used twice", t.name));
        }
        match t.principals.source.as_deref() {
            Some("operator") => {}
            Some(other) => err(format!(
                "{at}.principals.source {other:?}: P1 delegates for one per-person operator alone (source: operator); oidc, x509 and device principals delegate from P2"
            )),
            None => err(format!(
                "{at}.principals.source is required (operator): a template matching every principal would admit the bearer and placeholder principals that may never delegate"
            )),
        }
        if matches!(t.principals.role, Some(Role::Agent | Role::Anonymous)) {
            err(format!(
                "{at}.principals.role: an agent or anonymous principal never delegates"
            ));
        }
        if !s.identity.providers.contains_key(&t.idp) {
            err(format!(
                "{at}.idp {:?} names no identity.providers entry",
                t.idp
            ));
        }
        if t.servers.is_empty() {
            err(format!(
                "{at}.servers is empty: a template allows at least one delegated server"
            ));
        }
        for (srv, ceiling) in &t.servers {
            match s.mcp.servers.iter().find(|m| &m.name == srv) {
                Some(m) if is_delegated(m) => {
                    if m.auth.as_ref().and_then(|a| a.idp.as_deref()) != Some(t.idp.as_str()) {
                        err(format!(
                            "{at}.servers.{srv}: the server binds another IdP than the template's {:?}",
                            t.idp
                        ));
                    }
                }
                _ => err(format!(
                    "{at}.servers.{srv} names no mcp.servers entry of grant: id_jag"
                )),
            }
            if ceiling.scopes.is_empty() {
                err(format!(
                    "{at}.servers.{srv}.scopes is empty: the ceiling names the scopes it allows"
                ));
            }
        }
        if let Some(ttl) = t.ttl
            && (ttl.0 > MAX_TTL || ttl.0.is_zero())
        {
            err(format!("{at}.ttl: between 1s and 30d"));
        }
        for (field, v) in [
            ("per_day", t.budget.per_day),
            ("per_month", t.budget.per_month),
            ("per_run", t.budget.per_run),
        ] {
            if v == Some(0) {
                err(format!("{at}.budget.{field}: 0 would let no mint happen"));
            }
        }
    }
    // The per-person operator.
    let unix = s
        .a2a
        .listen
        .as_deref()
        .is_some_and(|l| crate::config::unix_socket_path(l).is_some());
    let mut persons = 0;
    for (i, p) in s.a2a.principals.iter().enumerate() {
        let at = format!("a2a.principals[{i}]");
        if p.matcher.uid.is_some() {
            let m = &p.matcher;
            if m.san.is_some() || m.sub.is_some() || m.bearer_ref.is_some() || m.any {
                err(format!(
                    "{at}.match.uid names a unix listener's peer, and san, sub, bearer_ref and any are evidence a TCP caller presents — say uid alone"
                ));
            }
            if !unix {
                err(format!(
                    "{at}.match.uid needs a unix A2A listener (a2a.listen: unix://…)"
                ));
            }
            if p.role != Role::Operator {
                err(format!(
                    "{at}.match.uid: the unix listener's peer is an operator (role: operator)"
                ));
            }
            if p.id.is_none() {
                err(format!(
                    "{at}.match.uid needs `id`: the per-person operator is operator:<id>"
                ));
            }
        }
        if let Some(pin) = &p.idp_subject {
            if p.matcher.uid.is_none() {
                err(format!(
                    "{at}.idp_subject: P1 pins the per-person operator on the unix listener (match.uid); a certificate principal delegates from P2"
                ));
            }
            if !s.identity.providers.values().any(|v| v.issuer == pin.iss) {
                err(format!(
                    "{at}.idp_subject.iss {:?} is no identity.providers entry's issuer",
                    pin.iss
                ));
            }
            if pin.sub.trim().is_empty() {
                err(format!("{at}.idp_subject.sub is empty: the pin is exact"));
            }
            // Autonomous work — a schedule, a webhook, a stream — runs as
            // `identity.autonomous_as`; named the person, every firing would
            // act with their grant with nobody there.
            if let Some(id) = &p.id
                && s.identity.autonomous_as.as_deref() == Some(format!("operator:{id}").as_str())
            {
                err(format!(
                    "identity.autonomous_as is operator:{id}, the per-person operator {at} pins: autonomous runs never act with an enrolled person's grant (RFC 0044 P1) — name another principal"
                ));
            }
            persons += 1;
        }
    }
    if persons > 1 {
        err("a2a.principals: P1 enrols one per-person operator; more than one rule carries idp_subject".into());
    }
    // The delegated servers.
    let delegated: Vec<&super::McpServer> = delegated_servers(s).collect();
    if !delegated.is_empty() {
        if !s.security.experimental.id_jag {
            err(
                "grant: id_jag needs security.experimental.id_jag: true — ID-JAG is an IETF draft"
                    .into(),
            );
        }
        if dl.seal_key.is_none() && !external {
            err("grant: id_jag needs identity.delegation.seal_key: the refresh tokens are sealed with it".into());
        }
        // Who may answer: a listener that authenticates its callers — a
        // unix socket, whose peer the kernel names, or one that declares a
        // verifier, so no loopback caller is the operator by default.
        let posture = crate::runtime::surface::auth::listener_auth_of(&s.a2a);
        if s.a2a.listen.is_none() || !(posture.unix || posture.declares_any()) {
            err("a delegated server needs an A2A listener that authenticates its callers — unix://, or TCP with a bearer, client certificates or the device grant (delegation_requires_authenticated_listener)".into());
        }
        if persons == 0 {
            warn.push("delegated servers are declared, and no a2a.principals rule with match.uid and idp_subject: nobody can enrol, and every delegated call fails no_delegation".into());
        }
    }
    // The keys are mounted whenever they are configured, a delegated server
    // or none: a sealed grant stays in the store, and the IdP key and the
    // seal key open it.
    if s.security.exec.enabled
        && (!delegated.is_empty()
            || !s.identity.providers.is_empty()
            || dl.seal_key.is_some()
            || dl.seal_key_previous.is_some())
    {
        err("security.exec.enabled beside identity.providers, identity.delegation's seal keys or a delegated server: a local command runs as the supervisor's uid and could read the mounted keys (RFC 0044 T4) — they are exclusive in P1".into());
    }
    let agent_mode: Vec<&super::McpServer> =
        s.mcp.servers.iter().filter(|m| !is_delegated(m)).collect();
    for m in &delegated {
        let at = format!("mcp.servers.{}", m.name);
        let Some(a) = &m.auth else { continue };
        if a.kind != AuthKind::Oauth2 {
            err(format!("{at}.auth: grant id_jag is for auth.kind oauth2"));
        }
        let provider = match a.idp.as_deref() {
            None => {
                err(format!(
                    "{at}.auth.idp is required: the identity.providers entry it delegates through (id_jag_missing_pin)"
                ));
                None
            }
            Some(n) => match s.identity.providers.get(n) {
                None => {
                    err(format!(
                        "{at}.auth.idp {n:?} names no identity.providers entry"
                    ));
                    None
                }
                p => p,
            },
        };
        match a.audience.as_deref() {
            None => err(format!(
                "{at}.auth.audience is required: the issuer of the server's authorization server, pinned (id_jag_missing_pin)"
            )),
            Some(aud) => {
                if !https_or_loopback(aud) {
                    err(format!(
                        "{at}.auth.audience {aud:?} is not an https:// issuer"
                    ));
                }
                if let Some(p) = provider
                    && p.issuer == aud
                {
                    err(format!(
                        "{at}: the IdP's issuer is the server's authorization server, where ID-JAG §9.3 rules the exchange out — this same-domain case is grant: token_exchange (P3)"
                    ));
                }
            }
        }
        match &a.resource {
            Some(super::ResourceParam::Uri(r)) => {
                if r.ends_with('/')
                    || r.contains('#')
                    || super::canonical_resource(r).as_deref() != Some(r.as_str())
                {
                    err(format!(
                        "{at}.auth.resource {r:?} is not in canonical form (lowercase scheme and host, no trailing slash, no fragment)"
                    ));
                }
            }
            _ => err(format!(
                "{at}.auth.resource is required as the server's URI: auto and omit are not pins (id_jag_missing_pin)"
            )),
        }
        if a.client_id.as_deref().is_none_or(|c| c.trim().is_empty()) {
            err(format!(
                "{at}.auth.client_id is required: agentd's client at the server's authorization server, which the ID-JAG names"
            ));
        }
        check_custody(
            &format!("{at}.auth"),
            a.client_auth.as_ref(),
            external,
            &mut err,
        );
        if !confidential(a.client_auth.as_ref(), a.client_secret.as_ref(), external) {
            err(format!(
                "{at}.auth: a public client cannot redeem an ID-JAG (ID-JAG §9.1) — client_auth with a key, or a client_secret"
            ));
        }
        check_client_secrets(
            &format!("{at}.auth"),
            a.client_auth.as_ref(),
            a.client_secret.as_ref(),
            &mut err,
        );
        for (k, v) in &m.headers {
            if crate::sec::secret::has_secret_ref(v) {
                env_sourced(&format!("{at}.headers.{k}"), &Secret(v.clone()), &mut err);
            }
        }
        match m.tag_set() {
            Ok(t) if t.is_empty() => err(format!(
                "{at}: a delegated server needs a trifecta tag (sensitive, egress, untrusted_input) — what a person's SaaS answers is attacker-writable (id_jag_untagged)"
            )),
            _ => {}
        }
        if m.oauth.is_some() || m.aauth == Some(true) {
            err(format!(
                "{at}: grant id_jag with `oauth:` or `aauth:` on the same server — one credential per server"
            ));
        }
        if a.tools_from == Some(ToolsFrom::File) && a.tools_file.is_none() {
            err(format!(
                "{at}.auth.tools_from file needs tools_file: the tools/list result"
            ));
        }
        if a.tools_file.is_some() && a.tools_from != Some(ToolsFrom::File) {
            err(format!(
                "{at}.auth.tools_file is read with tools_from: file"
            ));
        }
        if a.on_revoked == Some(OnRevoked::Ask) {
            err(format!(
                "{at}.auth.on_revoked: ask suspends the run on a re-enrolment the person starts in a display client, which arrives in P2; fail is P1's"
            ));
        }
        // Key reuse between the IdP and this authorization server (T21).
        if let (Some(p), Some(rc)) = (provider, a.client_auth.as_ref()) {
            let pk = p.client_auth.as_ref();
            let same_key = rc.key.is_some() && pk.and_then(|c| c.key.as_ref()) == rc.key.as_ref();
            let same_kid = rc.kid.is_some() && pk.and_then(|c| c.kid.as_ref()) == rc.kid.as_ref();
            if same_key || same_kid {
                err(format!(
                    "{at}.auth.client_auth: the same key or kid as identity.providers.{} (key_reuse) — a resource's authorization server must not be able to mint as agentd at the IdP",
                    a.idp.as_deref().unwrap_or_default()
                ));
            }
        }
        // An agent-mode route to the same resource or authorization server.
        let resource = a.resource_for(&m.endpoint, true);
        for o in &agent_mode {
            let o_res = o
                .auth
                .as_ref()
                .and_then(|x| x.resource_for(&o.endpoint, true))
                .or_else(|| super::canonical_resource(&o.endpoint));
            let o_iss = o.auth.as_ref().and_then(|x| x.issuer.clone());
            if (resource.is_some() && o_res == resource)
                || (a.audience.is_some() && o_iss == a.audience)
            {
                err(format!(
                    "{at} and mcp.servers.{}: the same resource or authorization server under a delegated and an agent-mode server (mode_mixing) — a steered model would reach agentd's own, broader authority",
                    o.name
                ));
            }
        }
    }
    // A grant field on a server that is not delegated, or `id_jag` off an
    // MCP server.
    for m in &agent_mode {
        if let Some(a) = &m.auth {
            for (f, set) in [
                ("idp", a.idp.is_some()),
                ("tool_scopes", !a.tool_scopes.is_empty()),
                ("scope_mode", a.scope_mode.is_some()),
                ("tools_from", a.tools_from.is_some()),
                ("tools_file", a.tools_file.is_some()),
                ("id_jag", a.id_jag.is_some()),
                ("id_jag_reuse", a.id_jag_reuse.is_some()),
                ("on_revoked", a.on_revoked.is_some()),
                ("allow_unbound_at", a.allow_unbound_at.is_some()),
                ("attribution", a.attribution.is_some()),
                ("max_at_age", a.max_at_age.is_some()),
            ] {
                if set {
                    err(format!(
                        "mcp.servers.{}.auth.{f} is for grant: id_jag",
                        m.name
                    ));
                }
            }
        }
    }
    d.warnings.extend(warn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_fragment_may_not_delegate_or_reach_a_reserved_secret() {
        let reserved: BTreeSet<String> = ["secret-file:/run/okta.pem".to_string()].into();
        // Every spelling of the reserved file and of a reserved name, and a
        // `*_file` field naming the file, is the reserved secret.
        let names: BTreeSet<String> = [
            crate::sec::secret::canonical_ref("secret-file:/run/okta.pem"),
            "secret:OKTA_KEY".to_string(),
        ]
        .into();
        for (spelling, at) in [
            ("{{secret-file: /run/okta.pem}}", "headers.X"),
            ("{{secret-file:/run//okta.pem}}", "headers.X"),
            ("{{secret-file:/run/./okta.pem}}", "headers.X"),
            ("{{secret-file:/run/x/../okta.pem}}", "headers.X"),
            ("{{secret: OKTA_KEY }}", "headers.X"),
            ("/run/./okta.pem", "auth.jwt_svid_file"),
        ] {
            let (field, key) = at.split_once('.').unwrap();
            let frag = json!({"mcp": {"servers": [{"name": "x", "endpoint": "https://x.example/mcp",
                                                    field: {key: spelling}}]}});
            let found = document_declared(frag.as_object().unwrap(), &names);
            assert_eq!(
                found,
                [format!(
                    "mcp.servers[0].{at} (a reserved supervisor-only secret)"
                )],
                "{spelling}"
            );
        }
        let other = json!({"mcp": {"servers": [{"name": "x", "endpoint": "https://x.example/mcp",
            "headers": {"X": "{{secret-file:/run/other.pem}} {{secret:OKTA_KEY2}}"},
            "auth": {"kind": "spiffe", "svid": "jwt", "jwt_svid_file": "/run/svid.jwt"}}]}});
        assert!(document_declared(other.as_object().unwrap(), &names).is_empty());
        let frag = json!({"mcp": {"servers": [
            {"name": "ok", "endpoint": "https://a.example/mcp"},
            {"name": "x", "endpoint": "https://b.example/mcp",
             "auth": {"kind": "oauth2", "grant": "id_jag", "idp": "okta"}},
            {"name": "y", "endpoint": "https://c.example/mcp",
             "headers": {"Authorization": "Bearer {{secret-file:/run/okta.pem}}"}}
        ]}});
        let found = document_declared(frag.as_object().unwrap(), &reserved);
        assert!(
            found
                .iter()
                .any(|f| f.contains("mcp.servers[1].auth.grant: id_jag")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|f| f == "mcp.servers[1].auth.idp"),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|f| f.starts_with("mcp.servers[2].headers.Authorization")),
            "{found:?}"
        );
        assert_eq!(found.len(), 3, "{found:?}");
        let clean = json!({"mcp": {"servers": [{"name": "ok", "endpoint": "https://a.example/mcp",
                                                 "auth": {"kind": "oauth2", "grant": "device", "client_id": "c"}}]}});
        assert!(document_declared(clean.as_object().unwrap(), &reserved).is_empty());
    }

    /// One delegated server with every pin and key in place, its files in a
    /// directory of its own (`__D__` in the text), as `edit` rewrites it: the
    /// load's error, or nothing when it loads.
    #[cfg(feature = "xaa")]
    fn load_error(edit: &dyn Fn(String) -> String) -> String {
        loaded(edit).err().unwrap_or_default()
    }

    #[cfg(feature = "xaa")]
    const BASE: &str = r#"store: {kind: memory}
a2a:
  listen: "unix://__D__/a.sock"
  principals:
    - {match: {uid: 1000}, role: operator, id: alice, idp_subject: {iss: "https://acme.okta.com", sub: 00u1alice}}
security:
  experimental: {id_jag: true}
identity:
  providers:
    okta:
      issuer: https://acme.okta.com
      profile: okta
      client_id: agentd-idp
      client_auth: {method: private_key_jwt, key: "{{secret-file:__D__/idp.pem}}", kid: idp-1}
  delegation:
    seal_key: "{{secret-file:__D__/seal}}"
    templates:
      - {name: eng, principals: {source: operator}, idp: okta, servers: {linear: {scopes: ["issues:read"]}}}
mcp:
  servers:
    - name: linear
      endpoint: https://mcp.linear.example/mcp
      tags: {"*": [sensitive]}
      auth:
        kind: oauth2
        grant: id_jag
        idp: okta
        audience: https://auth.linear.example
        resource: https://mcp.linear.example/mcp
        client_id: agentd-at-linear
        client_auth: {method: private_key_jwt, key: "{{secret-file:__D__/ras.pem}}", kid: ras-1}
"#;

    /// [`load_error`]'s configuration as `edit` rewrites it, loaded.
    #[cfg(feature = "xaa")]
    fn loaded(edit: &dyn Fn(String) -> String) -> Result<crate::config::settings::Loaded, String> {
        let dir = tempfile::tempdir().unwrap();
        for f in ["idp.pem", "ras.pem", "seal"] {
            std::fs::write(dir.path().join(f), "x").unwrap();
        }
        let yaml = edit(BASE.to_string()).replace("__D__", &dir.path().display().to_string());
        let path = dir.path().join("c.yaml");
        std::fs::write(&path, yaml).unwrap();
        crate::config::settings::load(&["--config".to_string(), path.display().to_string()], &[])
            .map(|(l, _)| l)
            .map_err(|e| e.to_string())
    }

    /// Under `key_custody: external` the signer holds every key: the
    /// configuration names them by `kid` and gives no key and no seal key,
    /// and the signer is a socket by absolute path; under `local`, a signer
    /// is refused as unread (RFC 0044 P2a).
    #[cfg(feature = "xaa")]
    #[test]
    fn the_keys_are_held_as_their_custody_says() {
        if !cfg!(feature = "a2a") {
            return;
        }
        let external = |y: String| {
            y.replace(
                "    seal_key: \"{{secret-file:__D__/seal}}\"\n",
                "    key_custody: external\n    signer: {socket: \"__D__/signer.sock\", uid: 990, seal_key: vault-1}\n",
            )
            .replace(", key: \"{{secret-file:__D__/idp.pem}}\"", "")
            .replace(", key: \"{{secret-file:__D__/ras.pem}}\"", "")
        };
        assert_eq!(load_error(&external), "", "the external custody loads");
        type Case = (&'static str, Box<dyn Fn(String) -> String>, &'static str);
        let cases: Vec<Case> = vec![
            (
                "a key beside the signer",
                Box::new(move |y| {
                    external(y).replace(
                        "kid: idp-1",
                        "key: \"{{secret-file:__D__/idp.pem}}\", kid: idp-1",
                    )
                }),
                "the signer holds the key",
            ),
            (
                "no kid",
                Box::new(move |y| external(y).replace(", kid: ras-1", "")),
                "kid is required under identity.delegation.key_custody external",
            ),
            (
                "a seal key beside the signer",
                Box::new(move |y| {
                    external(y).replace(
                        "    key_custody: external\n",
                        "    key_custody: external\n    seal_key: \"{{secret-file:__D__/seal}}\"\n",
                    )
                }),
                "name it as identity.delegation.signer.seal_key",
            ),
            (
                "no signer",
                Box::new(move |y| {
                    external(y).replace(
                        "    signer: {socket: \"__D__/signer.sock\", uid: 990, seal_key: vault-1}\n",
                        "",
                    )
                }),
                "needs identity.delegation.signer",
            ),
            (
                "a relative socket",
                Box::new(move |y| external(y).replace("\"__D__/signer.sock\"", "signer.sock")),
                "is not an absolute path",
            ),
            (
                "a signer under local custody",
                Box::new(|y| {
                    y.replace(
                        "  delegation:\n",
                        "  delegation:\n    signer: {socket: /run/s.sock, uid: 990, seal_key: vault-1}\n",
                    )
                }),
                "is read under key_custody: external alone",
            ),
        ];
        for (what, edit, says) in cases {
            let e = load_error(&edit);
            assert!(e.contains(says), "{what}: want {says:?} in:\n{e}");
        }
        // A signer of agentd's own uid loads, and is said; the signer's
        // socket is among what a child is denied.
        let same = |uid: u32| {
            let l = loaded(&|y| external(y).replace("uid: 990", &format!("uid: {uid}"))).unwrap();
            let warned = l.warnings.iter().any(|w| w.contains("is agentd's own"));
            let denied = l
                .settings
                .supervisor_only_paths()
                .iter()
                .any(|p| p.path.ends_with("signer.sock"));
            (warned, denied)
        };
        // SAFETY: geteuid has no failure mode.
        let me = unsafe { libc::geteuid() };
        assert_eq!(same(me), (true, true), "agentd's own uid");
        assert_eq!(same(me.wrapping_add(1)), (false, true), "another uid");
    }

    /// Every rule a delegated server is held to at load refuses what it
    /// says, by the code RFC 0044 gives it; the whole loads. A build
    /// without `a2a` has no listener for the person to enrol and answer
    /// over, so it loads no delegated server at all.
    #[cfg(feature = "xaa")]
    #[test]
    fn a_delegated_server_is_held_to_every_load_rule() {
        if !cfg!(feature = "a2a") {
            let e = load_error(&|y| y);
            assert!(e.contains("a2a build feature"), "{e}");
            return;
        }
        assert_eq!(load_error(&|y| y), "", "the base configuration loads");
        type Case = (&'static str, Box<dyn Fn(String) -> String>, &'static str);
        let cases: Vec<Case> = vec![
            (
                "no idp",
                Box::new(|y| y.replace("        idp: okta\n", "")),
                "id_jag_missing_pin",
            ),
            (
                "no audience",
                Box::new(|y| y.replace("        audience: https://auth.linear.example\n", "")),
                "id_jag_missing_pin",
            ),
            (
                "resource auto",
                Box::new(|y| {
                    y.replace("resource: https://mcp.linear.example/mcp", "resource: auto")
                }),
                "id_jag_missing_pin",
            ),
            (
                "a trailing slash",
                Box::new(|y| {
                    y.replace(
                        "resource: https://mcp.linear.example/mcp",
                        "resource: https://mcp.linear.example/mcp/",
                    )
                }),
                "canonical form",
            ),
            (
                "untagged",
                Box::new(|y| y.replace("      tags: {\"*\": [sensitive]}\n", "")),
                "id_jag_untagged",
            ),
            (
                "a public client",
                Box::new(|y| {
                    y.replace(
                        "        client_auth: {method: private_key_jwt, key: \"{{secret-file:__D__/ras.pem}}\", kid: ras-1}\n",
                        "",
                    )
                }),
                "a public client cannot redeem",
            ),
            (
                "the IdP as the authorization server",
                Box::new(|y| {
                    y.replace(
                        "audience: https://auth.linear.example",
                        "audience: https://acme.okta.com",
                    )
                }),
                "§9.3",
            ),
            (
                "an Okta custom authorization server",
                Box::new(|y| {
                    y.replace(
                        "issuer: https://acme.okta.com\n",
                        "issuer: https://acme.okta.com/oauth2/default\n",
                    )
                    .replace(
                        "iss: \"https://acme.okta.com\"",
                        "iss: \"https://acme.okta.com/oauth2/default\"",
                    )
                }),
                "org authorization server",
            ),
            (
                "exec beside it",
                Box::new(|y| {
                    y.replace(
                        "  experimental: {id_jag: true}\n",
                        "  experimental: {id_jag: true}\n  exec: {enabled: true}\n",
                    )
                }),
                "exclusive",
            ),
            (
                "exec beside the keys, with no delegated server",
                Box::new(|y| {
                    y.split("mcp:\n").next().unwrap_or_default().replace(
                        "  experimental: {id_jag: true}\n",
                        "  experimental: {id_jag: true}\n  exec: {enabled: true}\n",
                    )
                }),
                "exclusive",
            ),
            (
                "autonomous work as the person",
                Box::new(|y| {
                    y.replace(
                        "identity:\n  providers:\n",
                        "identity:\n  autonomous_as: \"operator:alice\"\n  providers:\n",
                    )
                }),
                "autonomous runs never act",
            ),
            (
                "no experimental switch",
                Box::new(|y| {
                    y.replace(
                        "experimental: {id_jag: true}",
                        "experimental: {id_jag: false}",
                    )
                }),
                "security.experimental.id_jag",
            ),
            (
                "no seal key",
                Box::new(|y| y.replace("    seal_key: \"{{secret-file:__D__/seal}}\"\n", "")),
                "identity.delegation.seal_key",
            ),
            (
                "a key from the environment",
                Box::new(|y| y.replace("{{secret-file:__D__/ras.pem}}", "{{secret:PATH}}")),
                "delegation_secret_in_environment",
            ),
            (
                "the IdP's kid at the authorization server",
                Box::new(|y| y.replace("kid: ras-1", "kid: idp-1")),
                "key_reuse",
            ),
            (
                "an agent-mode route to the same resource",
                Box::new(|y| {
                    format!(
                        "{y}    - {{name: bot, endpoint: https://mcp.linear.example/mcp, tags: {{\"*\": [egress]}}}}\n"
                    )
                }),
                "mode_mixing",
            ),
            (
                "a TCP listener with no verifier",
                Box::new(|y| {
                    y.replace(
                        "listen: \"unix://__D__/a.sock\"",
                        "listen: \"http://127.0.0.1:18998\"",
                    )
                }),
                "delegation_requires_authenticated_listener",
            ),
            (
                "uid beside san",
                Box::new(|y| y.replace("match: {uid: 1000}", "match: {uid: 1000, san: x}")),
                "say uid alone",
            ),
            (
                "a second pin",
                Box::new(|y| {
                    y.replace(
                        "    - {match: {uid: 1000}",
                        "    - {match: {uid: 1001}, role: operator, id: bob, idp_subject: {iss: \"https://acme.okta.com\", sub: 00u2bob}}\n    - {match: {uid: 1000}",
                    )
                }),
                "one per-person operator",
            ),
            (
                "a template naming no delegated server",
                Box::new(|y| y.replace("servers: {linear: {scopes:", "servers: {other: {scopes:")),
                "names no mcp.servers entry of grant: id_jag",
            ),
            (
                "a template for another source",
                Box::new(|y| {
                    y.replace(
                        "principals: {source: operator}",
                        "principals: {source: oidc}",
                    )
                }),
                "one per-person operator alone",
            ),
            (
                "on_revoked: ask",
                Box::new(|y| {
                    y.replace(
                        "        client_id: agentd-at-linear\n",
                        "        client_id: agentd-at-linear\n        on_revoked: ask\n",
                    )
                }),
                "arrives in P2",
            ),
            (
                "a delegation field on an agent-mode server",
                Box::new(|y| {
                    format!(
                        "{y}    - {{name: plain, endpoint: https://p.example/mcp, auth: {{kind: oauth2, grant: device, issuer: https://p.example, client_id: c, idp: okta}}}}\n"
                    )
                }),
                "is for grant: id_jag",
            ),
        ];
        for (what, edit, says) in cases {
            let e = load_error(&*edit);
            assert!(e.contains(says), "{what}: want {says:?} in:\n{e}");
        }
    }

    /// A key that would resolve from the environment is refused for a
    /// delegation secret: the environment is readable in `/proc` and handed
    /// to children.
    #[test]
    fn a_delegation_key_never_comes_from_the_environment() {
        let mut errs = Vec::new();
        env_sourced("k", &Secret("{{secret:PATH}}".into()), &mut |e| {
            errs.push(e)
        });
        assert!(
            errs.iter()
                .any(|e| e.contains("delegation_secret_in_environment")),
            "{errs:?}"
        );
        let mut none = Vec::new();
        env_sourced(
            "k",
            &Secret("{{secret-file:/run/k.pem}}".into()),
            &mut |e| none.push(e),
        );
        env_sourced(
            "k",
            &Secret("{{secret:AGENTD_SURELY_UNSET_9f3}}".into()),
            &mut |e| none.push(e),
        );
        assert!(none.is_empty(), "{none:?}");
    }

    /// Without the `xaa` feature, delegation configuration is refused by
    /// name rather than loaded and ignored.
    #[cfg(not(feature = "xaa"))]
    #[test]
    fn a_build_without_xaa_refuses_delegation_by_name() {
        let yaml = "store: {kind: memory}\nsecurity: {experimental: {id_jag: true}}\n";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(&path, yaml).unwrap();
        let e = crate::config::settings::load(
            &["--config".to_string(), path.display().to_string()],
            &[],
        )
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(e.contains("need the `xaa` feature"), "{e}");
    }

    /// Every secret under `identity.*` and on a delegated server is a
    /// reserved name: no child is started with the variable one names.
    #[test]
    fn the_delegation_secrets_are_the_supervisors_alone() {
        let doc = json!({
            "identity": {"providers": {"okta": {"issuer": "https://i", "client_id": "c",
                "client_auth": {"method": "private_key_jwt", "key": "{{secret:OKTA_KEY}}"}}},
                "delegation": {"seal_key": "{{secret-file:/run/seal}}"}},
            "mcp": {"servers": [
                {"name": "d", "endpoint": "https://d/mcp", "headers": {"X": "{{secret:D_HEADER}}"},
                 "auth": {"kind": "oauth2", "grant": "id_jag", "idp": "okta", "client_id": "c",
                          "client_auth": {"method": "private_key_jwt", "key": "{{secret:RAS_KEY}}"}}},
                {"name": "plain", "endpoint": "https://p/mcp", "headers": {"X": "{{secret:PLAIN}}"}}]}
        });
        let s = Settings::from_document(doc.clone(), "t").unwrap();
        let env = s.supervisor_only_env();
        for name in ["OKTA_KEY", "D_HEADER", "RAS_KEY"] {
            assert!(env.contains(name), "{name} must be reserved: {env:?}");
        }
        assert!(
            !env.contains("PLAIN"),
            "a plain server's secret is not the supervisor's alone"
        );
        let refs = reserved_refs_of_doc(&doc);
        assert!(
            refs.contains("secret-file:/run/seal") && refs.contains("secret:RAS_KEY"),
            "{refs:?}"
        );
        assert!(!refs.contains("secret:PLAIN"), "{refs:?}");
    }

    #[test]
    fn a_loopback_http_issuer_is_the_one_plain_http_admitted() {
        assert!(https_or_loopback("https://acme.okta.com"));
        assert!(https_or_loopback("http://127.0.0.1:8080/idp"));
        assert!(https_or_loopback("http://[::1]:9"));
        assert!(!https_or_loopback("http://acme.okta.com"));
        assert!(!https_or_loopback("http://10.0.0.1"));
        assert!(!https_or_loopback("ftp://127.0.0.1"));
    }
}
