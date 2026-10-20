// SPDX-License-Identifier: AGPL-3.0-only
//! Acting for one enrolled person at MCP servers through their enterprise
//! IdP (RFC 0044 P1): the supervisor's half of `grant: id_jag`.
//!
//! **Who.** One per-person operator, `operator:<id>`: the unix A2A
//! listener's peer the kernel names by uid, with an exact `idp_subject` pin
//! (`config::settings::delegation`). `agentd --login idp:<name>` enrols them
//! ([`enrol`]): the supervisor runs the sign-in, verifies the ID token with
//! the IdP's keys, binds its `iss` and `sub` to the pin, and seals the
//! refresh token in the [`vault`]. Only that operator's runs use the grant;
//! autonomous runs, other operators and every other principal get
//! `no_delegation`, and nothing ever falls back to agentd's own credential.
//!
//! **Where.** Delegated servers are the supervisor's alone: no worker is
//! handed one to dial (`mcp::from_spec` refuses a delegated spec), nothing is
//! dialled at boot, and every call comes back here as reactor work and runs
//! on a thread of its own — so a slow IdP holds up only the call that waits
//! on it. The pool is keyed `(server, grant_id, scope_set)`, and its signer
//! is a pure read of a token minted before the request was dispatched.
//!
//! **The renewal ladder** ([`Delegation::call`], RFC 0044 §5.8): an access
//! token is used until `exp − 60 s`, `at_cache_cap` (or the server's
//! `max_at_age`) or the grant's end, and only while the grant's refresh
//! token was proved alive within `liveness_interval` — a stale proof is
//! renewed first, and an IdP that does not answer then is `idp_unavailable`,
//! never a success on an old token. A 401 on a token that was not just
//! minted evicts it and re-presents the cached ID-JAG (no new mint) unless
//! the server's authorization server refused one before or `id_jag_reuse:
//! never`; otherwise it mints. A 401 right after a fresh mint is the
//! server's fault (`delegation_rejected_by_resource`), backed off.
//!
//! **A mint** takes the grant's lock — one IdP call per grant at a time, so
//! two servers minting at once never present one rotating refresh token
//! twice — presents the subject (`refresh_token`, or a fresh ID token from a
//! `refresh_token` grant with no `scope`), persists any rotation with compare
//! and swap before it is used, checks the ID-JAG ([`crate::auth::idjag`]) and
//! redeems it. `invalid_grant` from the exchange runs the liveness probe:
//! dead, the grant is revoked, tombstoned and purged; alive, the error is
//! that `(grant, server)`'s alone and negative-cached.
//! `insufficient_user_authentication` is `step_up`: the person signs in again
//! (`agentd --login idp:<name>`), and the next mint presents that fresh ID
//! token, whose `auth_time` meets the `max_age` asked.
//!
//! **Budgets** per grant and resource per day and month, and per run, are
//! counted in the grant's record, so a restart does not reset the month;
//! `delegation.quota_warn` fires at 80%. Every refusal is a typed
//! [`DelegationError`], said to the model as a tool error and recorded as
//! `delegation.denied`. Every live token value — refresh token, ID token,
//! ID-JAG, access token — is scrubbed from what a call returns, by value.

pub mod enrol;
pub mod lint;
pub mod vault;

#[cfg(feature = "a2a")]
pub mod cli;

#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::a2a::principals::typed::{PrincipalRef, Source};
use crate::auth::idjag::{self, CheckKind, Exchange, Expect, Refusal, SubjectType};
use crate::auth::oauth2::{self, OAuth2Params};
use crate::config::settings::Settings;
use crate::config::settings::delegation::{
    self as cfg, Attribution, IdJagMode, IdJagReuse, Provider, ScopeMode, SubjectKind, Subjects,
    Template, ToolsFrom,
};
use crate::obs::log::Logger;
use crate::state::{Durable, now_ms};
use vault::{Grant, Refused, Vault};

/// How long before `exp` an access token stops being used.
const EXP_SKEW_MS: u64 = 60_000;
/// How long a `(grant, server)` is left alone after a fault, at first; it
/// doubles to [`BACKOFF_MAX_MS`].
const BACKOFF_MIN_MS: u64 = 60_000;
const BACKOFF_MAX_MS: u64 = 15 * 60_000;
/// Faults in a row that open a server's breaker.
const BREAKER: u32 = 3;
/// The most pool entries; the least recently used goes first.
const POOL_CAP: usize = 256;
/// Where a delegated server's tool catalogue is kept between restarts.
const CATALOGUE: &str = "delegation/catalogue/";

/// A refusal's code (RFC 0044 §5.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    Revoked,
    PolicyDenied,
    Config,
    ResourceFault,
    StepUp,
    IdpUnavailable,
    /// The external signer did not sign, wrap or unwrap: nothing was sent
    /// and nothing opened (RFC 0044 P2a).
    SignerUnavailable,
    NeedsReenroll,
    BudgetExhausted,
    ResourceNotBound,
    IdpNoIdTokenOnRefresh,
    UnexpectedRefreshToken,
    ScopeBeyondCeiling,
    DelegationRejectedByResource,
    NotEnrolled,
    BindingMismatch,
    NoDelegation,
    DelegationNotPermitted,
    PrincipalNotDelegable,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Revoked => "revoked",
            Code::PolicyDenied => "policy_denied",
            Code::Config => "config",
            Code::ResourceFault => "resource_fault",
            Code::StepUp => "step_up",
            Code::IdpUnavailable => "idp_unavailable",
            Code::SignerUnavailable => "signer_unavailable",
            Code::NeedsReenroll => "needs_reenroll",
            Code::BudgetExhausted => "budget_exhausted",
            Code::ResourceNotBound => "resource_not_bound",
            Code::IdpNoIdTokenOnRefresh => "idp_no_id_token_on_refresh",
            Code::UnexpectedRefreshToken => "unexpected_refresh_token",
            Code::ScopeBeyondCeiling => "scope_beyond_ceiling",
            Code::DelegationRejectedByResource => "delegation_rejected_by_resource",
            Code::NotEnrolled => "not_enrolled",
            Code::BindingMismatch => "binding_mismatch",
            Code::NoDelegation => "no_delegation",
            Code::DelegationNotPermitted => "delegation_not_permitted",
            Code::PrincipalNotDelegable => "principal_not_delegable",
        }
    }
}

/// Every delegated refusal: typed, and never a fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationError {
    pub code: Code,
    pub message: String,
}

impl std::fmt::Display for DelegationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "delegation refused ({}): {}",
            self.code.as_str(),
            self.message
        )
    }
}

fn refuse(code: Code, message: impl Into<String>) -> DelegationError {
    DelegationError {
        code,
        message: message.into(),
    }
}

/// The code an IdP request that got no token is refused with: the signer's
/// when it made no client assertion, else the IdP's.
fn unavailable(r: &Refusal) -> Code {
    match r {
        Refusal::Signer(_) => Code::SignerUnavailable,
        _ => Code::IdpUnavailable,
    }
}

/// What a record's personal content is sealed under: the personal key's id
/// and the record it is in, so a sealed part moved to another record does
/// not open there.
fn personal_record_ad(key: &str, record: &str) -> Vec<u8> {
    crate::auth::seal::associated_data(&[b"delegation-personal", key.as_bytes(), record.as_bytes()])
}

/// A grant the vault did not store, as a refusal: the signer's when it
/// wrapped no data key, else `otherwise`.
fn not_stored(e: vault::NotStored, otherwise: Code, what: &str) -> DelegationError {
    match e {
        vault::NotStored::Signer(e) => refuse(Code::SignerUnavailable, format!("{what}: {e}")),
        vault::NotStored::Failed(e) => refuse(otherwise, format!("{what}: {e}")),
    }
}

/// Ask the external signer, before anything is served, for the public key
/// of every client key it is to sign with — each must exist, under the
/// algorithm configured — and to wrap and unwrap a probe under the seal
/// key. A signer that refuses is a configuration error and the start fails;
/// one that does not answer is said (`delegation.signer.unavailable`), and
/// every use fails `signer_unavailable` until it does.
fn probe_signer(
    signer: &crate::sec::signer::Signer,
    settings: &Settings,
    log: &Logger,
) -> Result<(), String> {
    use crate::config::settings::ClientAuthMethod;
    use crate::sec::signer::SignerError;
    let mut keys: Vec<(String, String)> = Vec::new();
    let auths = settings
        .identity
        .providers
        .values()
        .filter_map(|p| p.client_auth.as_ref())
        .chain(
            cfg::delegated_servers(settings)
                .filter_map(|m| m.auth.as_ref().and_then(|a| a.client_auth.as_ref())),
        );
    for c in auths.filter(|c| c.method == ClientAuthMethod::PrivateKeyJwt) {
        if let Some(kid) = &c.kid {
            keys.push((kid.clone(), c.alg.unwrap_or_default().as_str().to_string()));
        }
    }
    let seal = settings
        .identity
        .delegation
        .external()
        .map(|s| s.seal_key.clone())
        .unwrap_or_default();
    let checked = || -> Result<(), SignerError> {
        for (kid, alg) in &keys {
            let pk = signer.public_key(kid)?;
            if &pk.alg != alg {
                return Err(SignerError::Refused {
                    code: "wrong_alg".into(),
                    message: format!("{kid} is {} at the signer, and {alg} here", pk.alg),
                });
            }
        }
        let mut probe = [0u8; 32];
        crate::sec::random::fill(&mut probe)
            .map_err(|e| SignerError::Unavailable(format!("no OS randomness: {e}")))?;
        let ad = b"agentd signer probe";
        let back = signer.unwrap(&seal, &signer.wrap(&seal, &probe, ad)?, ad)?;
        if back != probe {
            return Err(SignerError::Refused {
                code: "unwrap_failed".into(),
                message: format!("{seal} unwrapped a probe to other bytes"),
            });
        }
        Ok(())
    };
    match checked() {
        Ok(()) => {
            log.info(
                "delegation.signer.ready",
                json!({"socket": signer.socket.display().to_string(), "keys": keys.len() + 1}),
            );
            Ok(())
        }
        Err(SignerError::Unavailable(e)) => {
            log.warn(
                "delegation.signer.unavailable",
                json!({"socket": signer.socket.display().to_string(), "err": e}),
            );
            Ok(())
        }
        Err(e) => Err(format!("identity.delegation.signer: {e}")),
    }
}

/// What the supervisor tells the loop.
#[derive(Debug)]
pub enum Note {
    /// An audit record: `delegation.enrolled`, `.minted`, `.used`, `.denied`,
    /// `.step_up`, `.revoked`, `.quota_warn`.
    Audit {
        action: &'static str,
        outcome: &'static str,
        principal: Option<String>,
        target: Value,
    },
    /// A delegated server's tool catalogue, listed with a delegated token.
    Catalogue {
        server: String,
        tools: Vec<::mcp::wire::Tool>,
    },
}

/// Where notes go.
pub type Notify = Arc<dyn Fn(Note) + Send + Sync>;

/// The per-person operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// `operator:<id>`: the principal id a run's authority names.
    pub id: String,
    pub typed: PrincipalRef,
    /// `unix-uid:<n>`.
    pub fingerprint: String,
    pub iss: String,
    pub sub: String,
    pub labels: BTreeMap<String, String>,
}

impl Person {
    /// The person `settings` declare, when there is one.
    pub fn of(settings: &Settings) -> Option<Person> {
        let rule = cfg::person(settings)?;
        let id = rule.id.clone()?;
        let pin = rule.idp_subject.clone()?;
        Some(Person {
            id: format!("operator:{id}"),
            typed: PrincipalRef::local(Source::Operator, &id),
            fingerprint: format!("unix-uid:{}", rule.matcher.uid?),
            iss: pin.iss,
            sub: pin.sub,
            labels: rule.labels.clone(),
        })
    }
}

/// An IdP, its metadata read once.
pub struct Idp {
    pub name: String,
    pub cfg: Provider,
    meta: Mutex<Option<idjag::IdpMeta>>,
    /// The external signer that holds its client key, if one does.
    signer: Option<crate::sec::signer::Signer>,
}

impl Idp {
    /// Its two documents, merged; read at the first use.
    pub fn meta(&self, timeout: Duration) -> Result<idjag::IdpMeta, String> {
        if let Some(m) = self.meta.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(m);
        }
        let m = idjag::idp_metadata(&self.cfg.issuer, self.cfg.jwks_pin().as_deref(), timeout)?;
        *self.meta.lock().unwrap_or_else(|e| e.into_inner()) = Some(m.clone());
        Ok(m)
    }

    /// agentd as a client at its token endpoint: its key and secret read from
    /// a file or the startup prompt alone.
    pub fn params(&self, meta: &idjag::IdpMeta) -> Result<OAuth2Params, String> {
        Ok(OAuth2Params {
            token_url: meta.token_endpoint.clone(),
            device_authorization_url: meta.device_authorization_endpoint.clone(),
            authorization_url: meta.authorization_endpoint.clone(),
            revocation_url: meta.revocation_endpoint.clone(),
            issuer: Some(self.cfg.issuer.clone()),
            iss_supported: meta.iss_supported,
            client_id: self.cfg.client_id.clone(),
            client_secret: self.cfg.client_secret.as_ref().map(|s| s.0.clone()),
            client_auth: client_auth_of(self.cfg.client_auth.as_ref(), self.signer.as_ref())?,
            scopes: self.cfg.scopes(),
            pkce_s256: Some(meta.pkce_s256),
            prompt_only: true,
            ..Default::default()
        })
    }
}

/// A configured client authentication, as the OAuth helpers send it: a
/// `private_key_jwt` key the external `signer` holds, under `key_custody:
/// external`, is asked of it by its `kid`.
fn client_auth_of(
    c: Option<&crate::config::settings::ClientAuth>,
    signer: Option<&crate::sec::signer::Signer>,
) -> Result<oauth2::ClientAuth, String> {
    use crate::config::settings::{AssertionAud, ClientAuthMethod};
    let Some(c) = c else {
        return Ok(oauth2::ClientAuth::Post);
    };
    let aud = match c.assertion_aud {
        AssertionAud::TokenEndpoint => oauth2::AssertionAud::TokenEndpoint,
        AssertionAud::Issuer => oauth2::AssertionAud::Issuer,
    };
    Ok(match (c.method, signer) {
        (ClientAuthMethod::ClientSecretPost, _) => oauth2::ClientAuth::Post,
        (ClientAuthMethod::ClientSecretBasic, _) => oauth2::ClientAuth::Basic,
        (ClientAuthMethod::PrivateKeyJwt, Some(signer)) => {
            oauth2::ClientAuth::PrivateKeyJwtSigner {
                signer: signer.clone(),
                key: c
                    .kid
                    .clone()
                    .ok_or("client_auth private_key_jwt under key_custody external needs `kid`")?,
                aud,
                alg: c.alg.unwrap_or_default().as_str().to_string(),
            }
        }
        (ClientAuthMethod::PrivateKeyJwt, None) => oauth2::ClientAuth::PrivateKeyJwt {
            key: c
                .key
                .as_ref()
                .map(|k| k.0.clone())
                .ok_or("client_auth private_key_jwt needs `key`")?,
            kid: c.kid.clone(),
            aud,
            alg: c.alg.map(|a| a.as_str().to_string()),
        },
    })
}

/// A delegated server.
#[derive(Clone)]
pub struct Server {
    pub name: String,
    pub endpoint: String,
    pub idp: String,
    pub audience: String,
    pub resource: String,
    /// agentd's client at the server's authorization server.
    pub client_id: String,
    client_auth: oauth2::ClientAuth,
    client_secret: Option<String>,
    token_url: Option<String>,
    pub scopes: Vec<String>,
    pub tool_scopes: BTreeMap<String, Vec<String>>,
    pub scope_mode: ScopeMode,
    pub tools_from: ToolsFrom,
    pub tools_file: Option<String>,
    assume: bool,
    pub reuse: IdJagReuse,
    pub allow_unbound: bool,
    pub attribution: Attribution,
    max_at_age: Option<Duration>,
    /// Whether nobody outside can write what it answers (`auth.trusted_output`):
    /// its results then hold the person's data and taint nothing.
    pub trusted_output: bool,
    headers: Vec<(String, String)>,
    timeout: Duration,
}

impl Server {
    fn of(
        m: &crate::config::settings::McpServer,
        timeout: Duration,
        signer: Option<&crate::sec::signer::Signer>,
    ) -> Option<Server> {
        let a = m.auth.as_ref()?;
        Some(Server {
            name: m.name.clone(),
            endpoint: m.endpoint.clone(),
            idp: a.idp.clone()?,
            audience: a.audience.clone()?,
            resource: a.resource_for(&m.endpoint, true)?,
            client_id: a.client_id.clone()?,
            client_auth: client_auth_of(a.client_auth.as_ref(), signer).ok()?,
            client_secret: a.client_secret.as_ref().map(|s| s.0.clone()),
            token_url: a.token_url.clone(),
            scopes: a.scopes.clone(),
            tool_scopes: a.tool_scopes.clone(),
            scope_mode: a.scope_mode.unwrap_or_default(),
            tools_from: a.tools_from.unwrap_or_default(),
            tools_file: a.tools_file.clone(),
            assume: a.id_jag == Some(IdJagMode::Assume),
            reuse: a.id_jag_reuse.unwrap_or_default(),
            allow_unbound: a.allow_unbound_at == Some(true),
            attribution: a.attribution.unwrap_or_default(),
            max_at_age: a.max_at_age.map(|d| d.0),
            trusted_output: a.trusted_output == Some(true),
            headers: m
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            timeout: m.timeout.map_or(timeout, |d| d.0),
        })
    }

    /// The scopes a token for `tool` carries: under `scope_mode: server`
    /// the union of `scopes` and every `tool_scopes` entry, else `scopes`
    /// and `tool`'s own — each within `ceiling`. `Err` names a scope `tool`
    /// needs that the ceiling does not allow.
    pub fn scope_set(&self, tool: &str, ceiling: &[String]) -> Result<Vec<String>, String> {
        let mut need: BTreeSet<String> = self.scopes.iter().cloned().collect();
        need.extend(self.tool_scopes.get(tool).into_iter().flatten().cloned());
        if let Some(over) = need.iter().find(|s| !ceiling.contains(s)) {
            return Err(over.clone());
        }
        let mut set = need;
        if self.scope_mode == ScopeMode::Server {
            set.extend(
                self.tool_scopes
                    .values()
                    .flatten()
                    .filter(|s| ceiling.contains(s))
                    .cloned(),
            );
        }
        Ok(set.into_iter().collect())
    }

    /// agentd as a client at the server's authorization server.
    fn params(&self, token_url: &str) -> OAuth2Params {
        OAuth2Params {
            token_url: token_url.to_string(),
            issuer: Some(self.audience.clone()),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.clone(),
            client_auth: self.client_auth.clone(),
            prompt_only: true,
            ..Default::default()
        }
    }
}

/// An access token in the pool.
#[derive(Clone)]
struct Held {
    at: String,
    /// When it stops being used, ms.
    until_ms: u64,
    /// Minted by this mint, and not yet answered by the server.
    fresh: bool,
    /// The ID-JAG's `sub`: whom the server's authorization server issued
    /// it for, as `agent/acting_for` names them.
    sub: Option<String>,
}

/// The last status a server refused a pooled token with, and its
/// `WWW-Authenticate`.
type Refused401 = Arc<Mutex<Option<(u16, Option<String>)>>>;

/// One pool entry: the token, and the connection that presents it.
struct Entry {
    token: Arc<Mutex<Option<Held>>>,
    /// The last status the server refused a request with.
    refused: Refused401,
    client: Mutex<Option<Arc<::mcp::client::McpClient>>>,
    used_ms: Mutex<u64>,
}

/// The pool's signer: a read of the token minted before the request.
struct Signer {
    token: Arc<Mutex<Option<Held>>>,
    refused: Refused401,
}

impl ::mcp::http::RequestSigner for Signer {
    fn sign(
        &self,
        _method: &str,
        _authority: &str,
        _path: &str,
        _body: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        match self
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(h) => Ok(vec![(
                "Authorization".to_string(),
                format!("Bearer {}", h.at),
            )]),
            None => Err("no delegated token is held for this request".into()),
        }
    }

    fn on_response(&self, resp: &::mcp::http::AuthResponse, _authority: &str) -> bool {
        if matches!(resp.status, 401 | 403) {
            *self.refused.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((resp.status, resp.www_authenticate.clone()));
        }
        // The loop runs the ladder and dispatches again: the transport never
        // retries a delegated request by itself.
        false
    }
}

/// A grant's runtime facts.
#[derive(Default)]
struct Live {
    /// The last time its refresh token was proved alive.
    proved_ms: u64,
    /// The last delegated call.
    used_ms: u64,
    /// The `max_age` an IdP asked a step-up for, until a sign-in meets it.
    step_up: Option<u64>,
    /// A fresh ID token from a step-up, its `auth_time` and `exp`.
    fresh_id_token: Option<(String, u64, u64)>,
    /// ID-JAGs, memory only, by `(ras, resource, scopes)`.
    id_jags: BTreeMap<String, (String, u64)>,
    /// A probe is running.
    probing: bool,
    /// The data key a rotated refresh token would be kept with, wrapped
    /// before the IdP is asked ([`vault::Ahead`]).
    ahead: Option<vault::Ahead>,
}

/// What `(grant, server)` faults hold back.
#[derive(Default, Clone, Copy)]
struct Fault {
    until_ms: u64,
    failures: u32,
}

/// The supervisor's delegation state.
pub struct Delegation {
    pub providers: BTreeMap<String, Idp>,
    pub servers: BTreeMap<String, Server>,
    pub templates: Vec<Template>,
    pub person: Option<Person>,
    at_cache_cap: Duration,
    liveness: Duration,
    vault: Vault,
    durable: Arc<Durable>,
    timeout: Duration,
    log: Logger,
    notify: Mutex<Option<Notify>>,
    /// The active grants' metadata, by `grant_id`.
    grants: Mutex<BTreeMap<String, Grant>>,
    live: Mutex<BTreeMap<String, Live>>,
    /// One IdP call per grant at a time.
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    pool: Mutex<BTreeMap<String, Arc<Entry>>>,
    /// Authorization servers' metadata, by issuer.
    ras: Mutex<BTreeMap<String, idjag::RasMeta>>,
    /// Authorization servers that refused a re-presented ID-JAG.
    no_reuse: Mutex<BTreeSet<String>>,
    faults: Mutex<BTreeMap<String, Fault>>,
    breakers: Mutex<BTreeMap<String, Fault>>,
    /// Mints per run (or conversation).
    runs: Mutex<BTreeMap<String, u32>>,
    /// The random handle each `(grant, server)` is named by to its server.
    handles: Mutex<BTreeMap<String, String>>,
    /// Servers whose catalogue is being listed.
    listing: Mutex<BTreeSet<String>>,
    /// Servers whose protected-resource metadata confirmed their pins.
    confirmed: Mutex<BTreeSet<String>>,
    /// Each principal's personal key once opened (RFC 0044 §6.4), by
    /// principal: its id and its bytes, wiped when forgotten or dropped.
    personal: Mutex<BTreeMap<String, PersonalKey>>,
    /// Held while a personal key is opened or made into `personal`, and
    /// while one is forgotten: a key opened as it is destroyed is never
    /// kept behind it.
    personal_open: Mutex<()>,
}

/// A personal key opened: its id and its bytes.
type PersonalKey = (String, Box<[u8; 32]>);

/// The scope ceilings `t` allows, per server.
fn ceilings_of(t: &Template) -> BTreeMap<String, Vec<String>> {
    t.servers
        .iter()
        .map(|(k, v)| (k.clone(), v.scopes.clone()))
        .collect()
}

/// `ceilings` within what `t` allows now: a server it no longer lists goes,
/// and so does a scope it no longer allows on one.
fn narrowed(
    ceilings: &BTreeMap<String, Vec<String>>,
    t: &Template,
) -> BTreeMap<String, Vec<String>> {
    ceilings
        .iter()
        .filter_map(|(server, scopes)| {
            let now = &t.servers.get(server)?.scopes;
            Some((
                server.clone(),
                scopes.iter().filter(|x| now.contains(x)).cloned().collect(),
            ))
        })
        .collect()
}

/// The `(grant, server)` key of the fault and handle tables.
fn pair(grant: &str, server: &str) -> String {
    format!("{grant}|{server}")
}

/// Exponential backoff after `failures` faults.
fn backoff(failures: u32) -> u64 {
    BACKOFF_MIN_MS
        .saturating_mul(1u64 << failures.saturating_sub(1).min(8))
        .min(BACKOFF_MAX_MS)
}

/// The seal key's 32 bytes from `text`: hex or base64.
fn key_bytes(text: &str, what: &str) -> Result<[u8; 32], String> {
    let bad = || format!("{what}: not 32 bytes in hex or base64");
    let t = text.trim();
    let bytes = if t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).map_err(|_| bad()))
            .collect::<Result<Vec<u8>, String>>()?
    } else {
        crate::b64::url_decode(&t.replace('+', "-").replace('/', "_").replace('=', ""))
            .map_err(|_| bad())?
    };
    bytes.try_into().map_err(|_| bad())
}

/// Resolve a delegation secret: a mounted file, or a value entered at the
/// startup prompt — never the environment.
pub(crate) fn resolve_secret(template: &str) -> Result<String, String> {
    crate::sec::secret::resolve(template, &|_| None)
}

/// Replace every live secret value inside `v` — itself, base64 and
/// percent-encoded, and a long prefix of it — with `[redacted]`.
pub fn scrub(v: &mut Value, secrets: &[String]) {
    let forms: Vec<String> = secrets
        .iter()
        .filter(|s| s.len() >= 8)
        .flat_map(|s| {
            let mut f = vec![
                s.clone(),
                crate::b64::std_pad(s.as_bytes()),
                crate::b64::url_nopad(s.as_bytes()),
                pct(s),
            ];
            if s.len() > 32 {
                f.push(s[..24].to_string());
            }
            f
        })
        .collect();
    fn walk(v: &mut Value, forms: &[String]) {
        match v {
            Value::String(s) => {
                for f in forms {
                    if s.contains(f.as_str()) {
                        *s = s.replace(f.as_str(), "[redacted]");
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, forms)),
            Value::Object(o) => o.values_mut().for_each(|x| walk(x, forms)),
            _ => {}
        }
    }
    walk(v, &forms);
}

/// `s` percent-encoded as a form value is.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// One delegated call.
pub struct Call<'a> {
    pub server: &'a str,
    pub tool: &'a str,
    pub args: Value,
    /// The request's `_meta`, attribution added here.
    pub meta: Value,
    /// The run's authority: who the work is for.
    pub authority: Option<&'a str>,
    /// The run or conversation the call is made in, for its budget.
    pub unit: String,
    pub run: Option<String>,
    pub step: Option<String>,
}

/// What a delegated call needs before it is dispatched: who, and which
/// grant, checked on the loop with no network.
pub struct Admitted {
    pub grant: Grant,
}

impl Delegation {
    /// The state `settings` declare, its grants loaded from `durable` and
    /// its tombstone register anchored in `chain`, when the audit chain is
    /// on; `None` when no delegated server is declared.
    pub fn build(
        settings: &Settings,
        durable: Arc<Durable>,
        chain: Option<Arc<crate::obs::chain::Chain>>,
        log: &Logger,
    ) -> Result<Option<Arc<Delegation>>, String> {
        let delegated: Vec<_> = cfg::delegated_servers(settings).collect();
        if delegated.is_empty() && settings.identity.providers.is_empty() {
            return Ok(None);
        }
        let dl = &settings.identity.delegation;
        let signer = dl.external().map(|s| s.signer());
        let custody = match (&signer, dl.external()) {
            (Some(signer), Some(sg)) => {
                probe_signer(signer, settings, log)?;
                vault::Custody::External {
                    signer: signer.clone(),
                    key: sg.seal_key.clone(),
                    previous: sg.seal_key_previous.clone(),
                }
            }
            _ => {
                let key = dl
                    .seal_key
                    .as_ref()
                    .ok_or("identity.delegation.seal_key is required to hold a grant")?;
                let key = key_bytes(&resolve_secret(&key.0)?, "identity.delegation.seal_key")?;
                let previous = dl
                    .seal_key_previous
                    .as_ref()
                    .map(|k| {
                        resolve_secret(&k.0)
                            .and_then(|t| key_bytes(&t, "identity.delegation.seal_key_previous"))
                    })
                    .transpose()?;
                vault::Custody::local(key, previous)?
            }
        };
        let timeout = settings
            .mcp
            .default_timeout
            .map_or(Duration::from_secs(30), |d| {
                d.0.min(Duration::from_secs(60))
            });
        let vault = Vault::new(Arc::clone(&durable), custody, chain, Some(log.clone()));
        let d = Delegation {
            providers: settings
                .identity
                .providers
                .iter()
                .map(|(n, p)| {
                    (
                        n.clone(),
                        Idp {
                            name: n.clone(),
                            cfg: p.clone(),
                            meta: Mutex::new(None),
                            signer: signer.clone(),
                        },
                    )
                })
                .collect(),
            servers: delegated
                .iter()
                .filter_map(|m| {
                    Server::of(m, timeout, signer.as_ref()).map(|s| (m.name.clone(), s))
                })
                .collect(),
            templates: dl.templates.clone(),
            person: Person::of(settings),
            at_cache_cap: dl.at_cache_cap(),
            liveness: dl.liveness_interval(),
            vault,
            durable,
            timeout,
            log: log.clone(),
            notify: Mutex::new(None),
            grants: Mutex::new(BTreeMap::new()),
            live: Mutex::new(BTreeMap::new()),
            locks: Mutex::new(BTreeMap::new()),
            pool: Mutex::new(BTreeMap::new()),
            ras: Mutex::new(BTreeMap::new()),
            no_reuse: Mutex::new(BTreeSet::new()),
            faults: Mutex::new(BTreeMap::new()),
            breakers: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(BTreeMap::new()),
            handles: Mutex::new(BTreeMap::new()),
            listing: Mutex::new(BTreeSet::new()),
            confirmed: Mutex::new(BTreeSet::new()),
            personal: Mutex::new(BTreeMap::new()),
            personal_open: Mutex::new(()),
        };
        d.load_grants();
        Ok(Some(Arc::new(d)))
    }

    /// Read every grant's metadata; a record that does not load is said.
    fn load_grants(&self) {
        let mut grants = BTreeMap::new();
        for id in self.vault.ids() {
            match self.vault.meta(&id) {
                Ok(Some(g)) => {
                    grants.insert(id, g);
                }
                Ok(None) => {}
                Err(e) => self.log.warn(
                    "delegation.grant.unreadable",
                    json!({"grant_id": id, "err": format!("{e:?}")}),
                ),
            }
        }
        *self.grants.lock().unwrap_or_else(|e| e.into_inner()) = grants;
    }

    /// Where notes go from now on.
    pub fn notify_to(&self, n: Notify) {
        *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    }

    fn note(&self, n: Note) {
        if let Some(f) = self
            .notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            f(n);
        }
    }

    fn audit(
        &self,
        action: &'static str,
        outcome: &'static str,
        principal: Option<&str>,
        target: Value,
    ) {
        self.log.info(action, target.clone());
        self.note(Note::Audit {
            action,
            outcome,
            principal: principal.map(str::to_string),
            target,
        });
    }

    /// Whether `server` is delegated.
    pub fn is_delegated(&self, server: &str) -> bool {
        self.servers.contains_key(server)
    }

    /// The person's grant at `idp`, by id.
    fn grant_id(&self, idp: &str) -> Option<String> {
        let p = self.person.as_ref()?;
        let issuer = &self.providers.get(idp)?.cfg.issuer;
        Some(p.typed.grant_id(issuer))
    }

    /// Whether `authority` holds a usable grant for `server`, with no
    /// network: what decides whether its tools are offered at all.
    pub fn usable(&self, authority: Option<&str>, server: &str) -> bool {
        self.admit(authority, server).is_ok()
    }

    /// The checks a delegated call passes before anything is dispatched:
    /// the run's authority is the enrolled person, and their grant is active,
    /// current, unexpired and theirs, and allows `server`. The grant handed
    /// back carries the ceilings it was enrolled with as the template that
    /// allowed it narrows them now: a template narrowed or removed takes
    /// effect at the next call, and widening one takes a new enrolment.
    pub fn admit(
        &self,
        authority: Option<&str>,
        server: &str,
    ) -> Result<Admitted, DelegationError> {
        let s = self
            .servers
            .get(server)
            .ok_or_else(|| refuse(Code::Config, format!("{server} is not a delegated server")))?;
        let Some(person) = self
            .person
            .as_ref()
            .filter(|p| authority == Some(p.id.as_str()))
        else {
            return Err(refuse(
                Code::NoDelegation,
                format!(
                    "{server} acts with an enrolled person's grant, and this work is {}'s: it is never done with agentd's own credential",
                    authority.unwrap_or("nobody")
                ),
            ));
        };
        let gid = self.grant_id(&s.idp).ok_or_else(|| {
            refuse(
                Code::Config,
                format!("{server}'s IdP {} is not declared", s.idp),
            )
        })?;
        let grant = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&gid)
            .cloned()
            .ok_or_else(|| {
                refuse(
                    Code::NotEnrolled,
                    format!(
                        "{} has no grant at {}: they enrol with `agentd --login idp:{}`",
                        person.id, s.idp, s.idp
                    ),
                )
            })?;
        if grant.fingerprint != person.fingerprint
            || grant.sub != person.sub
            || grant.idp_iss != person.iss
        {
            return Err(refuse(
                Code::NotEnrolled,
                format!(
                    "{}'s grant was enrolled as another identity than the configuration pins now: enrol again with `agentd --login idp:{}`",
                    person.id, s.idp
                ),
            ));
        }
        if now_ms() >= grant.expires_ms {
            return Err(refuse(
                Code::NeedsReenroll,
                format!(
                    "{}'s grant at {} has expired: it is never extended — enrol again",
                    person.id, s.idp
                ),
            ));
        }
        let Some(template) = self
            .templates
            .iter()
            .find(|t| t.name == grant.template && self.admits(t, &s.idp))
        else {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{}'s grant was allowed by template {}, which no longer admits them at {}: enrol again under the template that does",
                    person.id, grant.template, s.idp
                ),
            ));
        };
        let mut grant = grant;
        grant.ceilings = narrowed(&grant.ceilings, template);
        if !grant.ceilings.contains_key(server) {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{}'s grant (template {}) does not allow {server}",
                    person.id, grant.template
                ),
            ));
        }
        Ok(Admitted { grant })
    }

    /// The template that admits the person at `idp`.
    pub fn template_for(&self, idp: &str) -> Option<&Template> {
        self.templates.iter().find(|t| self.admits(t, idp))
    }

    /// Whether template `t` admits the person at `idp`.
    fn admits(&self, t: &Template, idp: &str) -> bool {
        self.person.as_ref().is_some_and(|p| {
            t.idp == idp
                && t.principals.source.as_deref() == Some("operator")
                && t.principals
                    .role
                    .is_none_or(|r| r == crate::config::settings::Role::Operator)
                && t.principals
                    .labels
                    .iter()
                    .all(|(k, v)| p.labels.get(k) == Some(v))
        })
    }

    fn lock_of(&self, grant: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(grant.to_string())
                .or_default(),
        )
    }

    fn with_live<T>(&self, grant: &str, f: impl FnOnce(&mut Live) -> T) -> T {
        f(self
            .live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(grant.to_string())
            .or_default())
    }

    /// RFC 0044 §5.6 step 3: the server's RFC 9728 metadata confirms its
    /// pins — its `resource` is the pinned one byte for byte (RFC 9728 §3.3)
    /// and its `authorization_servers` name the pinned audience — before
    /// anything is minted for it, so an endpoint that is not the resource
    /// pinned, or that now names another authorization server, is refused
    /// rather than handed a token. Read once a process: the pins change only
    /// with a restart.
    fn confirm(&self, s: &Server) -> Result<(), DelegationError> {
        if self
            .confirmed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&s.name)
        {
            return Ok(());
        }
        let not = |why: String| {
            refuse(
                Code::Config,
                format!(
                    "{}: {why}; its pins are not confirmed, and nothing is minted for it",
                    s.name
                ),
            )
        };
        let m =
            crate::auth::challenge::resource_metadata(&s.endpoint, self.timeout).map_err(not)?;
        if m.resource.as_deref() != Some(s.resource.as_str()) {
            return Err(not(format!(
                "its protected-resource metadata describes {:?}, not the pinned resource {} (RFC 9728 §3.3)",
                m.resource.as_deref().unwrap_or(""),
                s.resource
            )));
        }
        if !m.authorization_servers.contains(&s.audience) {
            return Err(not(format!(
                "its protected-resource metadata names the authorization servers {:?}, not the pinned audience {}",
                m.authorization_servers, s.audience
            )));
        }
        self.confirmed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(s.name.clone());
        Ok(())
    }

    /// The pinned authorization server's metadata, read once.
    fn ras(&self, s: &Server) -> Result<idjag::RasMeta, DelegationError> {
        if let Some(t) = &s.token_url {
            return Ok(idjag::RasMeta {
                issuer: s.audience.clone(),
                token_endpoint: t.clone(),
            });
        }
        if let Some(m) = self
            .ras
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&s.audience)
        {
            return Ok(m.clone());
        }
        let m = idjag::ras_metadata(&s.audience, s.assume, self.timeout)
            .map_err(|e| refuse(Code::Config, format!("{}: {e}", s.name)))?;
        self.ras
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(s.audience.clone(), m.clone());
        Ok(m)
    }

    /// Whether `(grant, server)` is held back, by its own faults or its
    /// server's breaker.
    fn held_back(&self, grant: &str, server: &str) -> Option<String> {
        let now = now_ms();
        let f = self
            .faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pair(grant, server))
            .copied();
        if let Some(f) = f
            && now < f.until_ms
        {
            return Some(format!(
                "{server} failed this grant's last request; tried again in {}s",
                (f.until_ms - now) / 1000
            ));
        }
        let b = self
            .breakers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server)
            .copied();
        if let Some(b) = b
            && b.failures >= BREAKER
            && now < b.until_ms
        {
            return Some(format!(
                "{server}'s breaker is open after {} faults in a row; tried again in {}s",
                b.failures,
                (b.until_ms - now) / 1000
            ));
        }
        None
    }

    /// Count a fault against `(grant, server)` and the server's breaker.
    fn fault(&self, grant: &str, server: &str) {
        let now = now_ms();
        let bump = |f: &mut Fault| {
            f.failures += 1;
            f.until_ms = now + backoff(f.failures);
        };
        bump(
            self.faults
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(pair(grant, server))
                .or_default(),
        );
        bump(
            self.breakers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(server.to_string())
                .or_default(),
        );
    }

    fn healthy(&self, grant: &str, server: &str) {
        self.faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pair(grant, server));
        self.breakers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server);
    }

    /// The random handle `(grant, server)` is named by to `server`.
    fn handle(&self, grant: &str, server: &str) -> String {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(pair(grant, server))
            .or_insert_with(|| crate::sec::random::b64url_token(12).unwrap_or_default())
            .clone()
    }

    /// The pool entry `(server, grant, scopes)`.
    fn entry(&self, server: &str, grant: &str, scopes: &[String]) -> (String, Arc<Entry>) {
        let key = format!("{server}|{grant}|{}", scopes.join(" "));
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if !pool.contains_key(&key) && pool.len() >= POOL_CAP {
            let oldest = pool
                .iter()
                .min_by_key(|(_, e)| *e.used_ms.lock().unwrap_or_else(|x| x.into_inner()))
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                pool.remove(&k);
            }
        }
        let e = pool
            .entry(key.clone())
            .or_insert_with(|| {
                Arc::new(Entry {
                    token: Arc::default(),
                    refused: Arc::default(),
                    client: Mutex::new(None),
                    used_ms: Mutex::new(0),
                })
            })
            .clone();
        *e.used_ms.lock().unwrap_or_else(|x| x.into_inner()) = now_ms();
        (key, e)
    }

    /// Drop every pooled token and connection of `grant`, and its ID-JAGs.
    pub fn purge(&self, grant: &str) {
        self.pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| k.split('|').nth(1) != Some(grant));
        self.with_live(grant, |l| {
            l.id_jags.clear();
            l.fresh_id_token = None;
            l.ahead = None;
        });
    }

    /// Drop every pooled connection to `server`: its configuration changed.
    pub fn purge_server(&self, server: &str) {
        self.pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| k.split('|').next() != Some(server));
    }

    /// The live secret values a call's answer is scrubbed of.
    fn live_values(&self, grant: &str, rt: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = rt.map(str::to_string).into_iter().collect();
        for (k, e) in self.pool.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if k.split('|').nth(1) == Some(grant)
                && let Some(h) = e.token.lock().unwrap_or_else(|x| x.into_inner()).as_ref()
            {
                out.push(h.at.clone());
            }
        }
        self.with_live(grant, |l| {
            out.extend(l.id_jags.values().map(|(j, _)| j.clone()));
            if let Some((t, _, _)) = &l.fresh_id_token {
                out.push(t.clone());
            }
        });
        out
    }

    /// The token a request to `s` for `tool` presents: the pooled one while
    /// it may be used, a re-presented ID-JAG's, or a new mint. `fresh`
    /// excludes re-presentation after a 401.
    fn token(
        &self,
        s: &Server,
        grant: &Grant,
        scopes: &[String],
        entry: &Entry,
        c: &Call<'_>,
        allow_reuse: bool,
    ) -> Result<(), DelegationError> {
        let now = now_ms();
        // Freshness first: no token is used for a grant whose refresh token
        // was not proved alive within `liveness_interval`.
        let proved = self.with_live(&grant.grant_id, |l| l.proved_ms);
        if now.saturating_sub(proved) > self.liveness.as_millis() as u64 {
            self.probe(grant)?;
        }
        if let Some(h) = entry
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && now < h.until_ms
        {
            return Ok(());
        }
        *entry.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let ras = self.ras(s)?;
        let jag_key = format!("{}|{}|{}", ras.issuer, s.resource, scopes.join(" "));
        // Re-present an unexpired ID-JAG (§4.4.3): no new mint.
        let cached = self.with_live(&grant.grant_id, |l| l.id_jags.get(&jag_key).cloned());
        let reuse_ok = allow_reuse
            && s.reuse == IdJagReuse::Auto
            && !self
                .no_reuse
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&ras.issuer);
        if let (true, Some((jag, exp))) = (reuse_ok, cached)
            && exp * 1000 > now + 5_000
        {
            match idjag::redeem(
                &s.params(&ras.token_endpoint),
                &jag,
                &s.resource,
                self.timeout,
            ) {
                Ok(r) => {
                    let sub = idjag::decode_unverified(&jag)
                        .ok()
                        .and_then(|(_, cl)| cl["sub"].as_str().map(str::to_string));
                    self.hold(s, grant, entry, &r, sub);
                    self.audit(
                        "delegation.minted",
                        "reused",
                        self.person.as_ref().map(|p| p.id.as_str()),
                        json!({"grant_id": grant.grant_id, "run": c.run, "step": c.step, "tool": c.tool,
                               "ras_iss": ras.issuer, "resource": s.resource, "reused_id_jag": true}),
                    );
                    return Ok(());
                }
                Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => {
                    // A single-use authorization server: never again, this life.
                    self.no_reuse
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(ras.issuer.clone());
                    self.with_live(&grant.grant_id, |l| l.id_jags.remove(&jag_key));
                }
                Err(_) => {}
            }
        }
        self.mint(s, grant, scopes, entry, c, &ras, &jag_key)
    }

    /// Put a redeemed access token in the pool.
    fn hold(
        &self,
        s: &Server,
        grant: &Grant,
        entry: &Entry,
        r: &idjag::Redeemed,
        sub: Option<String>,
    ) {
        let now = now_ms();
        let cap = s
            .max_at_age
            .map_or(self.at_cache_cap, |m| m.min(self.at_cache_cap));
        let by_exp = r
            .expires_in
            .map_or(u64::MAX, |e| (now + e * 1000).saturating_sub(EXP_SKEW_MS));
        let until = by_exp
            .min(now + cap.as_millis() as u64)
            .min(grant.expires_ms);
        *entry.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(Held {
            at: r.access_token.clone(),
            until_ms: until,
            fresh: true,
            sub,
        });
    }

    /// A new mint for `scopes` at `s`: budgets, the exchange under the
    /// grant's lock, the checks, the redemption.
    #[allow(clippy::too_many_arguments)]
    fn mint(
        &self,
        s: &Server,
        grant: &Grant,
        scopes: &[String],
        entry: &Entry,
        c: &Call<'_>,
        ras: &idjag::RasMeta,
        jag_key: &str,
    ) -> Result<(), DelegationError> {
        let template = self
            .templates
            .iter()
            .find(|t| t.name == grant.template)
            .cloned();
        let budget = template
            .as_ref()
            .map(|t| t.budget.clone())
            .unwrap_or_default();
        let per_run = self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&c.unit)
            .copied()
            .unwrap_or(0);
        if per_run >= budget.per_run() {
            return Err(refuse(
                Code::BudgetExhausted,
                format!(
                    "this run made its {} mints (budget.per_run)",
                    budget.per_run()
                ),
            ));
        }
        let idp = self
            .providers
            .get(&s.idp)
            .ok_or_else(|| refuse(Code::Config, format!("no IdP {}", s.idp)))?;
        let meta = idp
            .meta(self.timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, format!("{}: {e}", s.idp)))?;
        let params = idp.params(&meta).map_err(|e| refuse(Code::Config, e))?;
        let lock = self.lock_of(&grant.grant_id);
        let exchanged = {
            let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
            // Read inside the lock: a rotation another mint made is seen.
            let (mut g, mut rt) = self.load(&grant.grant_id)?;
            let q = g
                .quota
                .get(&s.resource)
                .cloned()
                .unwrap_or_default()
                .at(now_ms());
            if q.day_count >= budget.per_day() || q.month_count >= budget.per_month() {
                return Err(refuse(
                    Code::BudgetExhausted,
                    format!(
                        "{} has made {} mints today and {} this month at {} (budget {}/day, {}/month)",
                        g.principal.display(),
                        q.day_count,
                        q.month_count,
                        s.resource,
                        budget.per_day(),
                        budget.per_month()
                    ),
                ));
            }
            let mut retried = false;
            loop {
                let (subject, subject_type) = self.subject(idp, &params, &mut g, &mut rt)?;
                // Presenting the refresh token may rotate it.
                let ahead = match subject_type {
                    SubjectType::RefreshToken => self.ahead(&g)?,
                    SubjectType::IdToken => None,
                };
                let ex = Exchange {
                    subject_token: &subject,
                    subject_type,
                    audience: &s.audience,
                    resource: &s.resource,
                    scopes,
                };
                let expect = Expect {
                    iss: &idp.cfg.issuer,
                    client_id: &s.client_id,
                    sub: (idp.cfg.subjects() == Subjects::Public).then_some(g.sub.as_str()),
                    allow_unbound: s.allow_unbound,
                };
                let (rotated, answer) = idjag::exchange(&params, &ex, &expect, self.timeout);
                // A rotation of the subject is kept before anything else is
                // done with the answer — an ID-JAG a check refuses included:
                // the IdP has spent the token presented, and the next mint,
                // for any server, presents the new one.
                match &rotated {
                    Some(new) => self.rotate(&mut g, &mut rt, new, ahead)?,
                    None => self.keep_ahead(&g, ahead),
                }
                match answer {
                    Ok(x) => {
                        let mut q = q.clone();
                        q.day_count += 1;
                        q.month_count += 1;
                        let warn = q.month_count * 5 >= budget.per_month() * 4
                            || q.day_count * 5 >= budget.per_day() * 4;
                        g.quota.insert(s.resource.clone(), q.clone());
                        if let Err(e) = self.vault.store(&g, &rt, Some(g.rt_version)) {
                            self.log.warn(
                                "delegation.quota.unsaved",
                                json!({"grant_id": g.grant_id, "err": e.to_string()}),
                            );
                        }
                        self.remember(&g);
                        self.with_live(&g.grant_id, |l| l.proved_ms = now_ms());
                        if warn {
                            self.audit(
                                "delegation.quota_warn",
                                "warn",
                                self.person.as_ref().map(|p| p.id.as_str()),
                                json!({"grant_id": g.grant_id, "resource": s.resource,
                                       "day": q.day_count, "per_day": budget.per_day(),
                                       "month": q.month_count, "per_month": budget.per_month()}),
                            );
                        }
                        break (x, q.month_count);
                    }
                    Err(Refusal::Oauth { code, max_age, .. })
                        if code == idjag::wire::INSUFFICIENT_USER_AUTHENTICATION =>
                    {
                        self.with_live(&g.grant_id, |l| {
                            l.step_up = Some(max_age.unwrap_or(0));
                            l.fresh_id_token = None;
                        });
                        self.audit(
                            "delegation.step_up",
                            "asked",
                            self.person.as_ref().map(|p| p.id.as_str()),
                            json!({"grant_id": g.grant_id, "server": s.name, "tool": c.tool, "max_age": max_age}),
                        );
                        return Err(refuse(
                            Code::StepUp,
                            format!(
                                "the IdP wants a sign-in within {}s before it acts at {}: run `agentd --login idp:{}` and call again",
                                max_age.unwrap_or(0),
                                s.name,
                                s.idp
                            ),
                        ));
                    }
                    Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => {
                        // One retry when a concurrent rotation moved the
                        // token meanwhile; then the probe decides.
                        if !retried
                            && let Ok((fresh, fresh_rt)) = self.load(&g.grant_id)
                            && fresh.rt_version != g.rt_version
                        {
                            retried = true;
                            g = fresh;
                            rt = fresh_rt;
                            continue;
                        }
                        drop(_held);
                        return Err(self.after_invalid_grant(&g, s));
                    }
                    Err(r) => return Err(self.idp_refusal(&g, s, r)),
                }
            }
        };
        let (x, month) = exchanged;
        *self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(c.unit.clone())
            .or_default() += 1;
        if x.unbound {
            self.log.warn(
                "delegation.unbound",
                json!({"grant_id": grant.grant_id, "server": s.name, "note": "the ID-JAG names no resource; allow_unbound_at"}),
            );
        }
        self.with_live(&grant.grant_id, |l| {
            l.id_jags
                .insert(jag_key.to_string(), (x.id_jag.clone(), x.exp));
        });
        self.audit(
            "delegation.minted",
            "minted",
            self.person.as_ref().map(|p| p.id.as_str()),
            json!({"grant_id": grant.grant_id, "run": c.run, "step": c.step, "tool": c.tool,
                   "ras_iss": ras.issuer, "resource": s.resource, "scope_granted": x.scopes,
                   "narrowed": x.narrowed, "jti_sha256": crate::sha::sha256_hex(x.jti.as_bytes()),
                   "exp": x.exp, "auth_time": x.auth_time, "act": x.act,
                   "reused_id_jag": false, "month_to_date": month, "unattended": false}),
        );
        match idjag::redeem(
            &s.params(&ras.token_endpoint),
            &x.id_jag,
            &s.resource,
            self.timeout,
        ) {
            Ok(r) => {
                self.hold(s, grant, entry, &r, Some(x.sub.clone()));
                Ok(())
            }
            Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => {
                self.fault(&grant.grant_id, &s.name);
                Err(refuse(
                    Code::ResourceFault,
                    format!(
                        "{}'s authorization server refused a fresh ID-JAG (invalid_grant): it does not trust this IdP or agentd's client there (config)",
                        s.name
                    ),
                ))
            }
            Err(Refusal::Checked(k)) => {
                self.fault(&grant.grant_id, &s.name);
                Err(refuse(Code::Config, format!("{}: {k}", s.name)))
            }
            // The resource's client key at the signer: not the server's fault.
            Err(Refusal::Signer(e)) => Err(refuse(
                Code::SignerUnavailable,
                format!("{}'s authorization server: {e}", s.name),
            )),
            Err(r) => {
                self.fault(&grant.grant_id, &s.name);
                Err(refuse(
                    Code::ResourceFault,
                    format!("{}'s authorization server: {r}", s.name),
                ))
            }
        }
    }

    /// The exchange's subject: a step-up's fresh ID token while its
    /// `auth_time` meets the `max_age` asked, else the provider's.
    fn subject(
        &self,
        idp: &Idp,
        params: &OAuth2Params,
        g: &mut Grant,
        rt: &mut String,
    ) -> Result<(String, SubjectType), DelegationError> {
        let now_s = now_ms() / 1000;
        let fresh = self.with_live(&g.grant_id, |l| {
            l.fresh_id_token.clone().filter(|(_, auth_time, exp)| {
                *exp > now_s + 30 && l.step_up.is_none_or(|m| m == 0 || auth_time + m >= now_s)
            })
        });
        if let Some((t, _, _)) = fresh {
            return Ok((t, SubjectType::IdToken));
        }
        match idp.cfg.subject() {
            SubjectKind::RefreshToken => Ok((rt.clone(), SubjectType::RefreshToken)),
            SubjectKind::IdToken => {
                let ahead = self.ahead(g)?;
                let t = match idjag::refresh(params, rt, self.timeout) {
                    Ok(t) => t,
                    Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => {
                        return Err(self.revoke_dead(g, "the refresh token is no longer good"));
                    }
                    Err(r) => {
                        self.keep_ahead(g, ahead);
                        return Err(refuse(
                            unavailable(&r),
                            format!("{}: refresh: {r}", idp.name),
                        ));
                    }
                };
                match &t.refresh_token {
                    Some(new) => self.rotate(g, rt, new, ahead)?,
                    None => self.keep_ahead(g, ahead),
                }
                self.with_live(&g.grant_id, |l| l.proved_ms = now_ms());
                let id = t.id_token.ok_or_else(|| {
                    refuse(
                        Code::IdpNoIdTokenOnRefresh,
                        format!(
                            "{} returned no ID token on refresh: set identity.providers.{}.subject: refresh_token",
                            idp.name, idp.name
                        ),
                    )
                })?;
                Ok((id, SubjectType::IdToken))
            }
        }
    }

    /// The data key the record after `g`'s is sealed with when the IdP
    /// rotates the refresh token, wrapped before the IdP is asked
    /// ([`vault::Ahead`]): the one drawn for an earlier call while it is
    /// still for that record, else one drawn now. A signer that wraps none
    /// refuses the call before anything is sent.
    fn ahead(&self, g: &Grant) -> Result<Option<vault::Ahead>, DelegationError> {
        let mut next = g.clone();
        next.rt_version += 1;
        if let Some(a) = self
            .with_live(&g.grant_id, |l| l.ahead.take())
            .filter(|a| a.is_for(&next))
        {
            return Ok(Some(a));
        }
        self.vault.ahead(g).map_err(|e| {
            not_stored(
                e,
                Code::IdpUnavailable,
                "no data key to keep a rotated refresh token with",
            )
        })
    }

    /// Keep `a` for the next call: the IdP rotated nothing.
    fn keep_ahead(&self, g: &Grant, a: Option<vault::Ahead>) {
        if let Some(a) = a {
            self.with_live(&g.grant_id, |l| l.ahead = Some(a));
        }
    }

    /// Keep a rotated refresh token: one version up, compared and swapped,
    /// before it is used, sealed with the data key wrapped `ahead` — no
    /// signer is asked once the IdP has spent the old token.
    fn rotate(
        &self,
        g: &mut Grant,
        rt: &mut String,
        new: &str,
        ahead: Option<vault::Ahead>,
    ) -> Result<(), DelegationError> {
        let over = g.rt_version;
        let mut next = g.clone();
        next.rt_version += 1;
        self.vault
            .store_ahead(&next, new, Some(over), ahead)
            .map_err(|e| {
                not_stored(
                    e,
                    Code::IdpUnavailable,
                    "the rotated refresh token was not kept",
                )
            })?;
        *g = next;
        *rt = new.to_string();
        self.remember(g);
        Ok(())
    }

    fn remember(&self, g: &Grant) {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(g.grant_id.clone(), g.clone());
    }

    /// The grant and its refresh token, from the vault.
    fn load(&self, grant_id: &str) -> Result<(Grant, String), DelegationError> {
        match self.vault.load(grant_id) {
            Ok(Some(x)) => Ok(x),
            Ok(None) => Err(refuse(Code::NotEnrolled, "the grant is gone: enrol again")),
            Err(Refused::Revoked) => Err(refuse(Code::Revoked, "the grant was revoked")),
            Err(Refused::Stale) => Err(refuse(
                Code::NeedsReenroll,
                "the stored grant is older than its latest refresh token (a restored backup?): enrol again",
            )),
            Err(Refused::Unreadable(e)) => Err(refuse(
                Code::Config,
                format!("the grant does not open: {e}"),
            )),
            Err(Refused::SignerUnavailable(e)) => Err(refuse(
                Code::SignerUnavailable,
                format!("the grant was not opened: {e}"),
            )),
        }
    }

    /// `invalid_grant` from the exchange: the liveness probe decides whether
    /// the grant is dead, or the error is this server's alone.
    fn after_invalid_grant(&self, g: &Grant, s: &Server) -> DelegationError {
        match self.probe(g) {
            Err(e) => e,
            Ok(()) => {
                self.fault(&g.grant_id, &s.name);
                refuse(
                    Code::PolicyDenied,
                    format!(
                        "the IdP refused to issue an ID-JAG for {} (invalid_grant) while the grant is alive: its policy, or a wrong auth.audience pin — the IdP's Resource connection decides",
                        s.name
                    ),
                )
            }
        }
    }

    /// Any other refusal from the IdP, by its code.
    fn idp_refusal(&self, g: &Grant, s: &Server, r: Refusal) -> DelegationError {
        match r {
            Refusal::Unavailable(e) => refuse(Code::IdpUnavailable, format!("{}: {e}", s.idp)),
            Refusal::Signer(e) => refuse(Code::SignerUnavailable, format!("{}: {e}", s.idp)),
            Refusal::Checked(k) => {
                self.fault(&g.grant_id, &s.name);
                let code = match k.kind {
                    CheckKind::ResourceNotBound => Code::ResourceNotBound,
                    CheckKind::UnexpectedRefreshToken => Code::UnexpectedRefreshToken,
                    _ => Code::Config,
                };
                refuse(
                    code,
                    format!("the ID-JAG for {} failed a check: {k}", s.name),
                )
            }
            Refusal::Oauth { code, status, .. } => {
                self.fault(&g.grant_id, &s.name);
                let hint = match code.as_str() {
                    "invalid_scope" => {
                        " — Okta custom scopes must be granted on the Resource connection"
                    }
                    "invalid_target" => " — check auth.resource for a trailing slash",
                    _ => "",
                };
                let kind = if matches!(code.as_str(), "invalid_client" | "unauthorized_client") {
                    Code::Config
                } else {
                    Code::PolicyDenied
                };
                refuse(
                    kind,
                    format!(
                        "the IdP refused the exchange for {}: {code} (HTTP {status}){hint}",
                        s.name
                    ),
                )
            }
        }
    }

    /// The liveness probe: a plain `refresh_token` grant under the grant's
    /// lock, which mints no ID-JAG and costs no quota. A dead refresh token
    /// revokes the grant.
    pub fn probe(&self, g: &Grant) -> Result<(), DelegationError> {
        let idp = self
            .providers
            .get(&g.idp)
            .ok_or_else(|| refuse(Code::Config, format!("no IdP {}", g.idp)))?;
        let meta = idp
            .meta(self.timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, format!("{}: {e}", g.idp)))?;
        let params = idp.params(&meta).map_err(|e| refuse(Code::Config, e))?;
        let lock = self.lock_of(&g.grant_id);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, mut rt) = self.load(&g.grant_id)?;
        let ahead = self.ahead(&g)?;
        match idjag::refresh(&params, &rt, self.timeout) {
            Ok(t) => {
                match &t.refresh_token {
                    Some(new) => self.rotate(&mut g, &mut rt, new, ahead)?,
                    None => self.keep_ahead(&g, ahead),
                }
                self.with_live(&g.grant_id, |l| l.proved_ms = now_ms());
                Ok(())
            }
            Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => Err(self
                .revoke_dead(
                    &g,
                    "the liveness probe's refresh was refused (invalid_grant)",
                )),
            Err(r) => Err(refuse(
                unavailable(&r),
                format!(
                    "{} did not answer the liveness probe ({r}); no delegated token is used without one",
                    g.idp
                ),
            )),
        }
    }

    /// The IdP says the refresh token is dead: revoked by the IdP,
    /// tombstoned and purged.
    fn revoke_dead(&self, g: &Grant, why: &str) -> DelegationError {
        if let Err(e) = self.vault.tombstone(&g.grant_id, g.epoch) {
            self.log.error(
                "delegation.revoke.failed",
                json!({"grant_id": g.grant_id, "err": e}),
            );
        }
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&g.grant_id);
        self.purge(&g.grant_id);
        self.audit(
            "delegation.revoked",
            "revoked",
            Some(&g.principal.display()),
            json!({"grant_id": g.grant_id, "by": "idp", "why": why, "sub": g.sub}),
        );
        refuse(
            Code::Revoked,
            format!(
                "the IdP revoked {}'s grant: {why}; enrol again",
                g.principal.display()
            ),
        )
    }

    /// A delegated call, end to end, on the caller's thread. A refusal is
    /// scrubbed of every live token value as a result is — a server's error
    /// can quote the token it was presented — before the model, the log or
    /// the audit record reads it.
    pub fn call(&self, c: Call<'_>) -> Result<::mcp::wire::CallToolResult, DelegationError> {
        let mut result = self.call_inner(&c);
        if let Err(e) = &mut result {
            if let Some(gid) = self
                .servers
                .get(c.server)
                .and_then(|s| self.grant_id(&s.idp))
            {
                let mut v = Value::String(std::mem::take(&mut e.message));
                scrub(&mut v, &self.live_values(&gid, None));
                e.message = v.as_str().unwrap_or_default().to_string();
            }
            self.denied(
                c.authority,
                c.server,
                c.tool,
                e,
                c.run.clone(),
                c.step.clone(),
            );
        }
        result
    }

    /// Record a refusal: `delegation.denied`, the error verbatim.
    pub fn denied(
        &self,
        authority: Option<&str>,
        server: &str,
        tool: &str,
        e: &DelegationError,
        run: Option<String>,
        step: Option<String>,
    ) {
        self.audit(
            "delegation.denied",
            "denied",
            authority,
            json!({"code": e.code.as_str(), "error": e.message, "server": server,
                   "tool": tool, "run": run, "step": step}),
        );
    }

    fn call_inner(&self, c: &Call<'_>) -> Result<::mcp::wire::CallToolResult, DelegationError> {
        let Admitted { grant } = self.admit(c.authority, c.server)?;
        let s = self
            .servers
            .get(c.server)
            .ok_or_else(|| refuse(Code::Config, "no such server"))?;
        let ceiling = grant.ceilings.get(c.server).cloned().unwrap_or_default();
        let scopes = s.scope_set(c.tool, &ceiling).map_err(|over| {
            refuse(
                Code::ScopeBeyondCeiling,
                format!(
                    "{}.{} needs {over:?}, beyond the grant's ceiling on {}: widening is the person's, by a new enrolment under a template that allows it",
                    c.server, c.tool, c.server
                ),
            )
        })?;
        if let Some(why) = self.held_back(&grant.grant_id, c.server) {
            return Err(refuse(Code::ResourceFault, why));
        }
        self.confirm(s)?;
        let (_, entry) = self.entry(c.server, &grant.grant_id, &scopes);
        for _attempt in 0..2 {
            self.token(s, &grant, &scopes, &entry, c, true)?;
            *entry.refused.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let client = match self.client(s, &entry) {
                Ok(client) => client,
                // The server refused the token at the handshake: the same
                // ladder as a refused call.
                Err(e) => match self.refused_token(&grant, c.server, &entry) {
                    Some(Ok(())) => continue,
                    Some(Err(r)) => return Err(r),
                    None => return Err(e),
                },
            };
            let mut meta = c.meta.clone();
            if let Some(o) = meta.as_object_mut() {
                let sub = entry
                    .token
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|h| h.sub.clone());
                match (s.attribution, sub) {
                    (Attribution::Sub, Some(sub)) => {
                        o.insert("agent/acting_for".into(), json!(sub));
                    }
                    _ => {
                        o.remove("agent/acting_for");
                    }
                }
                o.remove("agent/labels");
                o.insert(
                    "agent/grant".into(),
                    json!(self.handle(&grant.grant_id, c.server)),
                );
            }
            let res = client.call_tool_reaching(c.tool, Some(c.args.clone()), meta);
            let refused = entry
                .refused
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if res.is_err()
                && matches!(refused, Some((401, _)))
                && let Some(next) = self.refused_token(&grant, c.server, &entry)
            {
                next?;
                continue;
            }
            match (res, refused) {
                (Ok(mut r), _) => {
                    if let Some(h) = entry
                        .token
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_mut()
                    {
                        h.fresh = false;
                    }
                    self.healthy(&grant.grant_id, c.server);
                    self.with_live(&grant.grant_id, |l| l.used_ms = now_ms());
                    let secrets = self.live_values(&grant.grant_id, None);
                    let mut v = serde_json::to_value(&r).unwrap_or(Value::Null);
                    scrub(&mut v, &secrets);
                    if let Ok(back) = serde_json::from_value(v) {
                        r = back;
                    }
                    self.audit(
                        "delegation.used",
                        if r.is_error() { "error" } else { "ok" },
                        c.authority,
                        json!({"grant_id": grant.grant_id, "server": c.server, "tool": c.tool,
                               "run": c.run, "step": c.step}),
                    );
                    return Ok(r);
                }
                (Err(_), Some((403, www))) => {
                    let wanted = www
                        .as_deref()
                        .and_then(|w| ::mcp::http::Challenge::parse(403, w))
                        .and_then(|ch| ch.scope)
                        .unwrap_or_default();
                    let beyond: Vec<&str> = wanted
                        .split_whitespace()
                        .filter(|x| !ceiling.iter().any(|c| c == x))
                        .collect();
                    if !beyond.is_empty() {
                        return Err(refuse(
                            Code::ScopeBeyondCeiling,
                            format!(
                                "{} asks for {} beyond the grant's ceiling",
                                c.server,
                                beyond.join(" ")
                            ),
                        ));
                    }
                    self.fault(&grant.grant_id, c.server);
                    return Err(refuse(
                        Code::ResourceFault,
                        format!(
                            "{} answered insufficient_scope for scopes the token carries ({wanted}): a server inconsistency",
                            c.server
                        ),
                    ));
                }
                (Err(f), _) => {
                    return Err(refuse(
                        Code::ResourceFault,
                        format!("{}: {}", c.server, f.error),
                    ));
                }
            }
        }
        Err(refuse(
            Code::DelegationRejectedByResource,
            format!("{} refused the delegated token twice", c.server),
        ))
    }

    /// The connection of `entry`, made with the EMA extension declared.
    fn client(
        &self,
        s: &Server,
        entry: &Entry,
    ) -> Result<Arc<::mcp::client::McpClient>, DelegationError> {
        if let Some(c) = entry
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(c);
        }
        let headers = s
            .headers
            .iter()
            .map(|(k, v)| Ok((k.clone(), resolve_secret(v)?)))
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| refuse(Code::Config, format!("{}: a header: {e}", s.name)))?;
        let signer = Arc::new(Signer {
            token: Arc::clone(&entry.token),
            refused: Arc::clone(&entry.refused),
        });
        let mut client = ::mcp::client::McpClient::connect_signed(
            &s.name,
            &s.endpoint,
            headers,
            s.timeout,
            Some(signer as Arc<dyn ::mcp::http::RequestSigner>),
        )
        .map_err(|e| refuse(Code::Config, e.to_string()))?
        .with_client_info(::mcp::wire::Implementation {
            name: "agentd".into(),
            version: crate::VERSION.into(),
            title: None,
        })
        .with_extension(idjag::wire::EMA_EXTENSION);
        client
            .initialize()
            .map_err(|e| refuse(Code::ResourceFault, format!("{}: {e}", s.name)))?;
        let client = Arc::new(client);
        *entry.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&client));
        Ok(client)
    }

    fn reset_client(&self, entry: &Entry) {
        *entry.client.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The server answered `401` to `entry`'s token: right after a mint the
    /// server's fault, backed off (`Some(Err)`); otherwise the token is
    /// evicted and the call goes again (`Some(Ok)`). `None` when no `401`
    /// was seen.
    fn refused_token(
        &self,
        grant: &Grant,
        server: &str,
        entry: &Entry,
    ) -> Option<Result<(), DelegationError>> {
        if !matches!(
            *entry.refused.lock().unwrap_or_else(|e| e.into_inner()),
            Some((401, _))
        ) {
            return None;
        }
        let fresh = entry
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some_and(|h| h.fresh);
        self.reset_client(entry);
        if fresh {
            self.fault(&grant.grant_id, server);
            return Some(Err(refuse(
                Code::DelegationRejectedByResource,
                format!(
                    "{server} refused a token minted for it a moment ago (401): its fault, not a reason to mint again"
                ),
            )));
        }
        Some(Ok(()))
    }

    /// `server`'s tool catalogue: `tools_file`, or what its first delegated
    /// connection listed, as kept.
    pub fn catalogue(&self, server: &str) -> Option<Vec<::mcp::wire::Tool>> {
        let s = self.servers.get(server)?;
        match s.tools_from {
            ToolsFrom::File => {
                let path = s.tools_file.as_ref()?;
                let text = std::fs::read_to_string(path).ok()?;
                let v: Value = serde_json::from_str(&text).ok()?;
                let list = v.get("tools").cloned().unwrap_or(v);
                serde_json::from_value(list).ok()
            }
            ToolsFrom::FirstGrant => self
                .durable
                .get(crate::state::Kind::Cred, &format!("{CATALOGUE}{server}"))
                .ok()
                .flatten()
                .and_then(|e| serde_json::from_value(e.state).ok()),
        }
    }

    /// List `server`'s tools with the person's delegated token — one mint —
    /// and keep them: after an enrolment, for each templated server whose
    /// catalogue is empty.
    pub fn fill_catalogue(&self, server: &str) -> Result<usize, DelegationError> {
        if !self
            .listing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server.to_string())
        {
            return Ok(0);
        }
        let out = self.list_with_grant(server);
        self.listing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server);
        out
    }

    fn list_with_grant(&self, server: &str) -> Result<usize, DelegationError> {
        let person = self
            .person
            .clone()
            .ok_or_else(|| refuse(Code::NoDelegation, "no per-person operator is declared"))?;
        let Admitted { grant } = self.admit(Some(&person.id), server)?;
        let s = self
            .servers
            .get(server)
            .ok_or_else(|| refuse(Code::Config, "no such server"))?;
        let ceiling = grant.ceilings.get(server).cloned().unwrap_or_default();
        let scopes = s
            .scope_set("", &ceiling)
            .map_err(|over| refuse(Code::ScopeBeyondCeiling, format!("{server} needs {over:?}")))?;
        self.confirm(s)?;
        let (_, entry) = self.entry(server, &grant.grant_id, &scopes);
        let c = Call {
            server,
            tool: "tools/list",
            args: Value::Null,
            meta: json!({}),
            authority: Some(&person.id),
            unit: format!("catalogue:{server}"),
            run: None,
            step: None,
        };
        self.token(s, &grant, &scopes, &entry, &c, true)?;
        let client = self.client(s, &entry)?;
        let tools = client
            .list_tools()
            .map_err(|e| refuse(Code::ResourceFault, format!("{server}: tools/list: {e}")))?;
        if let Some(h) = entry
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            h.fresh = false;
        }
        if s.tools_from == ToolsFrom::FirstGrant
            && let Ok(v) = serde_json::to_value(&tools)
        {
            let _ = self.durable.put(
                crate::state::Kind::Cred,
                &format!("{CATALOGUE}{server}"),
                v,
                None,
            );
        }
        let n = tools.len();
        self.note(Note::Catalogue {
            server: server.to_string(),
            tools,
        });
        Ok(n)
    }

    /// The grants an operator sees: metadata, never a secret.
    pub fn list(&self) -> Vec<Value> {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|g| {
                let live = self.with_live(&g.grant_id, |l| (l.proved_ms, l.used_ms, l.step_up));
                json!({"grant_id": g.grant_id, "principal": g.principal.display(), "idp": g.idp,
                       "idp_iss": g.idp_iss, "sub": g.sub, "template": g.template,
                       "servers": g.ceilings, "created_ms": g.created_ms, "expires_ms": g.expires_ms,
                       "epoch": g.epoch, "rt_version": g.rt_version, "quota": g.quota,
                       "proved_ms": live.0, "used_ms": live.1, "step_up_max_age": live.2})
            })
            .collect()
    }

    /// `principal`'s personal key when this process holds it open.
    fn cached_personal(&self, principal: &str) -> Option<(String, [u8; 32])> {
        self.personal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(principal)
            .map(|(id, k)| (id.clone(), **k))
    }

    /// Whether `principal` holds a grant here: a personal key is made only
    /// for a person agentd acts for, so none is made again for one
    /// forgotten, whose grants are gone.
    fn holds_grant(&self, principal: &str) -> bool {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|g| g.principal.display() == principal)
    }

    /// `principal`'s personal key, opened once a process and kept in memory
    /// (`create`: made when there is none and they hold a grant). Under an
    /// external signer this asks it: never on the loop.
    fn personal_key(
        &self,
        principal: &str,
        create: bool,
    ) -> Result<Option<(String, [u8; 32])>, String> {
        if let Some(k) = self.cached_personal(principal) {
            return Ok(Some(k));
        }
        let _open = self.personal_open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = self.cached_personal(principal) {
            return Ok(Some(k));
        }
        let create = create && self.holds_grant(principal);
        let found = self
            .vault
            .personal_key(principal, create)
            .map_err(|e| match e {
                vault::Refused::Revoked => format!("{principal}'s personal key was forgotten"),
                vault::Refused::SignerUnavailable(e) | vault::Refused::Unreadable(e) => e,
                vault::Refused::Stale => format!("{principal}'s personal key is stale"),
            })?;
        if let Some((id, k)) = &found {
            self.personal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(principal.to_string(), (id.clone(), Box::new(*k)));
        }
        Ok(found)
    }

    /// Under an external signer, open the enrolled person's personal key —
    /// made when they hold a grant and have none — off the loop, so a
    /// consent record then asks the signer nothing; a local key is opened
    /// where a record needs it.
    pub fn warm_personal(self: &Arc<Self>) {
        if let Some(p) = self.person.as_ref().map(|p| p.id.clone()) {
            self.warm(&p);
        }
    }

    /// Open `principal`'s personal key off the loop, as
    /// [`Self::warm_personal`] does, when an external signer holds it and
    /// this process does not have it open: a consent gate of theirs just
    /// opened, and its record is sealed on the loop.
    pub fn warm(self: &Arc<Self>, principal: &str) {
        if !self.vault.external() || self.cached_personal(principal).is_some() {
            return;
        }
        let (d, p) = (Arc::clone(self), principal.to_string());
        std::thread::Builder::new()
            .name("delegation-personal".into())
            .spawn(move || {
                if let Err(e) = d.personal_key(&p, true) {
                    d.log.warn(
                        "delegation.personal.unreadable",
                        json!({"principal": p, "err": e}),
                    );
                }
            })
            .ok();
    }

    /// The personal content of record `record` about `principal` — the
    /// arguments a person saw — sealed under their personal key (RFC 0044
    /// §6.4): `{key, sealed}`, the key's id and the AES-256-GCM ciphertext
    /// bound to it and to `record`. Kept in the audit chain in place of the
    /// content, so destroying the key makes it unreadable while the chain
    /// still verifies.
    ///
    /// Called on the loop: under an external signer only a key this
    /// process holds open seals ([`Self::warm`] opens it when the gate
    /// does), so the loop never waits on the signer; a local key is opened,
    /// or made, here.
    pub fn seal_personal(
        &self,
        principal: &str,
        record: &str,
        content: &Value,
    ) -> Result<Value, String> {
        let held = if self.vault.external() {
            self.cached_personal(principal).ok_or_else(|| {
                format!("{principal}'s personal key is not open yet: it is opened off the loop")
            })?
        } else {
            self.personal_key(principal, true)?
                .ok_or_else(|| format!("{principal} has no personal key"))?
        };
        let (id, mut key) = held;
        let sealed = crate::auth::seal::SealKey::new(&key).and_then(|k| {
            k.seal(
                &personal_record_ad(&id, record),
                content.to_string().as_bytes(),
            )
        });
        crate::sec::harden::wipe(&mut key);
        Ok(json!({"key": id, "sealed": crate::b64::url_nopad(&sealed?)}))
    }

    /// What `sealed` ([`Self::seal_personal`]) holds of record `record`
    /// about `principal`: `None` when the key that sealed it is gone —
    /// forgotten, or never this principal's.
    pub fn open_personal(
        &self,
        principal: &str,
        record: &str,
        sealed: &Value,
    ) -> Result<Option<Value>, String> {
        let Some((id, mut key)) = self.personal_key(principal, false)? else {
            return Ok(None);
        };
        if sealed["key"].as_str() != Some(id.as_str()) {
            crate::sec::harden::wipe(&mut key);
            return Ok(None);
        }
        let bytes = crate::b64::url_decode(sealed["sealed"].as_str().unwrap_or_default())
            .map_err(|_| "a sealed record is not base64url".to_string());
        let opened = bytes.and_then(|b| {
            crate::auth::seal::SealKey::new(&key)
                .and_then(|k| k.open(&personal_record_ad(&id, record), &b))
        });
        crate::sec::harden::wipe(&mut key);
        let plain = opened?;
        serde_json::from_slice(&plain)
            .map(Some)
            .map_err(|e| format!("a sealed record: {e}"))
    }

    /// Erase `principal` (RFC 0044 §6.4): their grants revoked as
    /// [`Self::revoke`] revokes them, then their personal key destroyed —
    /// what it sealed in the audit chain is unreadable from here on, and the
    /// chain still verifies. What was done.
    pub fn forget(&self, principal: &str, by: &str) -> Result<Value, String> {
        let revoked = match self.revoke(principal, by) {
            Ok(r) => r,
            Err(e) if e.starts_with("no grant names") => Vec::new(),
            Err(e) => return Err(e),
        };
        // Destroyed, then dropped from memory, with no key opened between.
        let key = {
            let _open = self.personal_open.lock().unwrap_or_else(|e| e.into_inner());
            let key = self.vault.forget_personal(principal);
            if let Some((_, mut k)) = self
                .personal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(principal)
            {
                crate::sec::harden::wipe(&mut k[..]);
            }
            key?
        };
        self.audit(
            "delegation.forgotten",
            "forgotten",
            Some(by),
            json!({"principal": principal, "by": by, "key": key,
                   "revoked": revoked.iter().map(|r| r["grant_id"].clone()).collect::<Vec<_>>()}),
        );
        Ok(json!({"principal": principal, "revoked": revoked, "key": key}))
    }

    /// Revoke the grants `target` names — a principal id, or
    /// `idp:<name>#<sub>` for every grant of that IdP subject — by `by`:
    /// RFC 7009 at the IdP, retried until acknowledged or out of tries, then
    /// the tombstone, the record and the pool. What happened, per grant.
    pub fn revoke(&self, target: &str, by: &str) -> Result<Vec<Value>, String> {
        let matching: Vec<Grant> = {
            let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            match target.strip_prefix("idp:") {
                Some(rest) => {
                    let (idp, sub) = rest
                        .split_once('#')
                        .ok_or("idp:<name>#<sub> names a subject after #")?;
                    grants
                        .values()
                        .filter(|g| g.idp == idp && g.sub == sub)
                        .cloned()
                        .collect()
                }
                None => grants
                    .values()
                    .filter(|g| g.principal.display() == target)
                    .cloned()
                    .collect(),
            }
        };
        if matching.is_empty() {
            return Err(format!("no grant names {target}"));
        }
        let mut out = Vec::new();
        for g in matching {
            let lock = self.lock_of(&g.grant_id);
            let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
            let rt = self
                .vault
                .load(&g.grant_id)
                .ok()
                .flatten()
                .map(|(_, rt)| rt);
            let mut revoked_at_idp = Err("the refresh token did not open".to_string());
            if let (Some(rt), Some(idp)) = (&rt, self.providers.get(&g.idp)) {
                revoked_at_idp = self.revoke_at_idp(idp, rt);
            }
            let stored = self.vault.tombstone(&g.grant_id, g.epoch);
            self.grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&g.grant_id);
            self.purge(&g.grant_id);
            self.audit(
                "delegation.revoked",
                "revoked",
                Some(by),
                json!({"grant_id": g.grant_id, "by": by, "principal": g.principal.display(),
                       "sub": g.sub, "at_idp": revoked_at_idp.is_ok(),
                       "at_idp_error": revoked_at_idp.as_ref().err(), "tombstoned": stored.is_ok()}),
            );
            out.push(
                json!({"grant_id": g.grant_id, "principal": g.principal.display(),
                            "revoked_at_idp": revoked_at_idp.is_ok(),
                            "error": revoked_at_idp.err().or(stored.err())}),
            );
        }
        Ok(out)
    }

    /// RFC 7009 at the IdP, three tries a second, two and four apart.
    fn revoke_at_idp(&self, idp: &Idp, rt: &str) -> Result<(), String> {
        let meta = idp.meta(self.timeout)?;
        let url = meta
            .revocation_endpoint
            .clone()
            .ok_or_else(|| format!("{} names no revocation_endpoint", idp.name))?;
        let params = idp.params(&meta)?;
        let mut last = String::new();
        for wait in [0u64, 1, 2, 4] {
            std::thread::sleep(Duration::from_secs(wait));
            match idjag::revoke(&params, &url, rt, self.timeout) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// Store a new grant for the person at `idp` after a verified sign-in.
    ///
    /// A sign-in for a grant that is still the same — the same identity,
    /// unexpired, under the template that admits the person now with the
    /// ceilings it allows now — is a step-up or a renewal: the record keeps
    /// its metadata and takes the new refresh token one version up. Any
    /// other (expired, another `sub`, `iss` or uid, another template or
    /// ceilings) is replaced: the old epoch is tombstoned, its pooled tokens
    /// purged, and a new grant one epoch up is made from what is configured
    /// now. Either way the refresh token replaced is revoked at the IdP
    /// (RFC 7009), once the new one is kept.
    pub(crate) fn enrolled(
        &self,
        idp: &Idp,
        claims: &idjag::IdClaims,
        rt: &str,
        id_token: &str,
    ) -> Result<Grant, DelegationError> {
        let person = self.person.clone().ok_or_else(|| {
            refuse(
                Code::PrincipalNotDelegable,
                "no per-person operator is declared",
            )
        })?;
        if claims.iss != person.iss || claims.sub != person.sub {
            return Err(refuse(
                Code::BindingMismatch,
                format!(
                    "the sign-in was {}#{}, and {} is pinned to {}#{}: nothing was kept",
                    claims.iss, claims.sub, person.id, person.iss, person.sub
                ),
            ));
        }
        let template = self.template_for(&idp.name).cloned().ok_or_else(|| {
            refuse(
                Code::DelegationNotPermitted,
                format!(
                    "no identity.delegation.templates entry admits {} at {}",
                    person.id, idp.name
                ),
            )
        })?;
        let gid = person.typed.grant_id(&idp.cfg.issuer);
        let lock = self.lock_of(&gid);
        let held = lock.lock().unwrap_or_else(|e| e.into_inner());
        // A grant the signer cannot open now is not taken for none: the
        // sign-in waits for it, rather than replacing a grant it never read.
        let existing = match self.vault.load(&gid) {
            Err(Refused::SignerUnavailable(e)) => {
                return Err(refuse(
                    Code::SignerUnavailable,
                    format!("the grant was not opened: {e}"),
                ));
            }
            r => r.ok().flatten(),
        };
        let now = now_ms();
        let ceilings = ceilings_of(&template);
        let same = |g: &Grant| {
            g.fingerprint == person.fingerprint
                && g.sub == claims.sub
                && g.idp_iss == claims.iss
                && now < g.expires_ms
                && g.template == template.name
                && g.ceilings == ceilings
        };
        let mut replaced = None;
        let grant = match existing {
            Some((mut g, old_rt)) if same(&g) => {
                let over = g.rt_version;
                g.rt_version += 1;
                self.vault
                    .store(&g, rt, Some(over))
                    .map_err(|e| not_stored(e, Code::Config, "the grant was not kept"))?;
                replaced = Some(old_rt);
                g
            }
            existing => {
                let mut epoch_floor = 0;
                if let Some((old, old_rt)) = existing {
                    self.vault
                        .tombstone(&gid, old.epoch)
                        .map_err(|e| refuse(Code::Config, e))?;
                    self.purge(&gid);
                    self.audit(
                        "delegation.revoked",
                        "revoked",
                        Some(&person.id),
                        json!({"grant_id": gid, "by": "enrolment", "epoch": old.epoch,
                               "why": "a sign-in replaced it: expired, another identity, or another template or ceilings",
                               "sub": old.sub}),
                    );
                    epoch_floor = old.epoch;
                    replaced = Some(old_rt);
                }
                let floor = self.vault.floors().get(&gid).copied().unwrap_or_default();
                let g = Grant {
                    grant_id: gid.clone(),
                    principal: person.typed.clone(),
                    fingerprint: person.fingerprint.clone(),
                    idp: idp.name.clone(),
                    idp_iss: claims.iss.clone(),
                    sub: claims.sub.clone(),
                    email_hash: claims
                        .email
                        .as_ref()
                        .map(|e| crate::sha::sha256_hex(e.as_bytes())),
                    template: template.name.clone(),
                    ceilings,
                    created_ms: now,
                    expires_ms: now + template.ttl().as_millis() as u64,
                    epoch: floor.revoked_epoch.unwrap_or(0).max(epoch_floor) + 1,
                    rt_version: floor.rt_version + 1,
                    quota: Default::default(),
                };
                self.vault
                    .store(&g, rt, None)
                    .map_err(|e| not_stored(e, Code::Config, "the grant was not kept"))?;
                g
            }
        };
        self.remember(&grant);
        drop(held);
        // The refresh token this sign-in replaced is agentd's no longer, so
        // it is revoked rather than left alive at the IdP — unless the IdP
        // handed the same one back.
        if let Some(old) = replaced.filter(|o| o != rt) {
            let at_idp = self.revoke_at_idp(idp, &old);
            self.log.info(
                "delegation.replaced_token",
                json!({"grant_id": gid, "revoked_at_idp": at_idp.is_ok(),
                       "at_idp_error": at_idp.err()}),
            );
        }
        let now_s = now / 1000;
        self.with_live(&gid, |l| {
            l.proved_ms = now;
            // A sign-in that meets the step-up asked is the next subject.
            if l.step_up.is_some() {
                l.fresh_id_token = Some((
                    id_token.to_string(),
                    claims.auth_time.unwrap_or(now_s),
                    claims.exp,
                ));
            }
        });
        Ok(grant)
    }

    /// Clear a satisfied step-up once a mint used the fresh ID token.
    pub fn stepped_up(&self, grant: &str) -> bool {
        self.with_live(grant, |l| l.step_up.is_some())
    }

    /// Every grant used within the last `liveness_interval` whose last proof
    /// is older than it: probed off the loop, a dead one revoked.
    pub fn tick(self: &Arc<Self>) {
        let now = now_ms();
        let window = self.liveness.as_millis() as u64;
        let due: Vec<Grant> = {
            let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            grants
                .values()
                .filter(|g| {
                    let l = live.entry(g.grant_id.clone()).or_default();
                    let due = !l.probing
                        && now.saturating_sub(l.used_ms) <= window
                        && l.used_ms > 0
                        && now.saturating_sub(l.proved_ms) > window;
                    if due {
                        l.probing = true;
                    }
                    due
                })
                .cloned()
                .collect()
        };
        for g in due {
            let me = Arc::clone(self);
            std::thread::Builder::new()
                .name("delegation-probe".into())
                .spawn(move || {
                    if let Err(e) = me.probe(&g) {
                        me.log.warn(
                            "delegation.probe.failed",
                            json!({"grant_id": g.grant_id, "code": e.code.as_str(), "err": e.message}),
                        );
                        // An IdP that does not answer leaves no token in use.
                        me.purge(&g.grant_id);
                    }
                    me.with_live(&g.grant_id, |l| l.probing = false);
                })
                .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(mode: ScopeMode) -> Server {
        Server {
            name: "linear".into(),
            endpoint: "https://mcp.linear.example/mcp".into(),
            idp: "okta".into(),
            audience: "https://auth.linear.example".into(),
            resource: "https://mcp.linear.example/mcp".into(),
            client_id: "agentd-at-linear".into(),
            client_auth: oauth2::ClientAuth::Post,
            client_secret: None,
            token_url: None,
            scopes: vec!["issues:read".into()],
            tool_scopes: [
                ("create_issue".to_string(), vec!["issues:write".to_string()]),
                ("delete_team".to_string(), vec!["admin".to_string()]),
            ]
            .into(),
            scope_mode: mode,
            tools_from: ToolsFrom::FirstGrant,
            tools_file: None,
            assume: false,
            reuse: IdJagReuse::Auto,
            allow_unbound: false,
            attribution: Attribution::Sub,
            max_at_age: None,
            trusted_output: false,
            headers: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }

    /// One token per server carries the configured union within the
    /// ceiling; per tool, each tool's own; a tool that needs a scope beyond
    /// the ceiling is refused by name.
    #[test]
    fn a_token_carries_the_configured_union_within_the_ceiling() {
        let ceiling = vec!["issues:read".to_string(), "issues:write".to_string()];
        let s = server(ScopeMode::Server);
        assert_eq!(
            s.scope_set("list_issues", &ceiling).unwrap(),
            ["issues:read", "issues:write"]
        );
        assert_eq!(
            s.scope_set("create_issue", &ceiling).unwrap(),
            ["issues:read", "issues:write"]
        );
        assert_eq!(s.scope_set("delete_team", &ceiling).unwrap_err(), "admin");
        let p = server(ScopeMode::PerTool);
        assert_eq!(
            p.scope_set("list_issues", &ceiling).unwrap(),
            ["issues:read"]
        );
        assert_eq!(
            p.scope_set("create_issue", &ceiling).unwrap(),
            ["issues:read", "issues:write"]
        );
    }

    /// A live token is scrubbed from an answer whole, in either base64
    /// alphabet, percent-encoded, and by a long prefix. The token's bytes
    /// put `/` and `+` in its standard base64, so neither form hides inside
    /// the other.
    #[test]
    fn a_live_token_never_leaves_in_an_answer() {
        let at = "at-0123456789abcdefghij???>>>klmnopqrstuvwxyz".to_string();
        let (b64, b64url) = (
            crate::b64::std_pad(at.as_bytes()),
            crate::b64::url_nopad(at.as_bytes()),
        );
        let mut v = json!({"text": format!("echo {at}"), "b64": b64, "b64url": b64url,
                           "url": format!("?t={}", pct(&at)), "cut": format!("{}…", &at[..30]),
                           "list": [at.clone()], "other": "fine"});
        scrub(&mut v, std::slice::from_ref(&at));
        let s = v.to_string();
        assert!(!s.contains(&at[..24]), "{s}");
        assert!(!s.contains(&b64) && !s.contains(&b64url), "{s}");
        assert!(!s.contains(&pct(&at)), "{s}");
        assert_eq!(v["other"], "fine");
        assert_eq!(v["list"][0], "[redacted]");
    }

    #[test]
    fn a_seal_key_is_hex_or_base64() {
        assert_eq!(key_bytes(&"ab".repeat(32), "k").unwrap(), [0xab; 32]);
        assert_eq!(
            key_bytes(&crate::b64::std_pad(&[7u8; 32]), "k").unwrap(),
            [7u8; 32]
        );
        assert!(key_bytes("short", "k").is_err());
    }

    /// A personal key is made only for a person who holds a grant here: a
    /// forgotten person, whose grants are gone, gets none again, so what a
    /// gate of theirs showed is never sealed readable after they were
    /// forgotten — nothing is sealed, and no key is kept.
    #[test]
    fn no_personal_key_is_made_for_a_principal_without_a_grant() {
        use crate::obs::log::{Comp, Level, LogCtx};
        let dir = tempfile::tempdir().unwrap();
        let seal = dir.path().join("seal");
        std::fs::write(&seal, "ab".repeat(32)).unwrap();
        let settings = Settings::from_document(
            json!({"agent": {"instruction": "x"},
                   "identity": {
                       "providers": {"okta": {"issuer": "https://idp.example",
                                              "client_id": "agentd", "subject": "refresh_token",
                                              "client_auth": {"method": "client_secret_basic"},
                                              "client_secret": format!("{{{{secret-file:{}}}}}", seal.display())}},
                       "delegation": {"seal_key": format!("{{{{secret-file:{}}}}}", seal.display())}}}),
            "test",
        )
        .unwrap();
        let durable = Arc::new(Durable::new(
            Arc::new(crate::store::memory::MemoryStore::new()),
            "t/",
            "t",
            crate::state::Policy::default(),
            None,
        ));
        let log = Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: std::process::id(),
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
            },
            Level::Error,
        );
        let d = Delegation::build(&settings, durable, None, &log)
            .unwrap()
            .expect("an IdP is declared");
        let who = "operator:alice";
        let e = d
            .seal_personal(who, "consent:g", &json!({"rendered": {}}))
            .unwrap_err();
        assert!(e.contains("has no personal key"), "{e}");
        assert!(
            matches!(d.vault.personal_key(who, false), Ok(None)),
            "none kept"
        );
        assert!(d.cached_personal(who).is_none());
    }
}
