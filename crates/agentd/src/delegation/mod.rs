// SPDX-License-Identifier: AGPL-3.0-only
//! Acting for enrolled people at MCP servers through their enterprise IdP
//! (RFC 0044 P1, P2): the supervisor's half of `grant: id_jag`.
//!
//! **Who** ([`Who`]): three kinds of principal may hold a grant, each by the
//! typed principal its transport established, never a display string —
//!
//! - a **per-person operator**, `operator:<id>`: the unix A2A listener's
//!   peer the kernel names by uid, with an exact `idp_subject` pin
//!   (`config::settings::delegation`), enrolled with `agentd --login
//!   idp:<name>` ([`enrol`]);
//! - an **IdP-federated session** (`a2a.login`), `user:oidc:<iss>#<sub>`:
//!   whoever the IdP said signed in, enrolled by that same sign-in;
//! - an **RFC 0043 device session**, `user:<name>`: eligible only through a
//!   `device_pins` pin or, opted in, the first IdP enrolment of its name —
//!   and then only through a session that itself proved, by signing in at
//!   the IdP, the `iss` and `sub` the name is bound to. Another session of
//!   the same name shares the name's history but not the grant
//!   (`not_enrolled`) until it signs in as that person; signing in as
//!   anyone else is `binding_mismatch`, and the grant is untouched.
//!
//! A run or a turn records the session it acts through, and every call is
//! admitted against that session as it is now: ended — its `token_ttl`,
//! `auth.sessions.revoke`, `/oauth2/revoke`, a restart — the call fails
//! `revoked`. Each enrolment verifies the ID token with the IdP's keys,
//! binds its `iss` and `sub` as the principal's kind says, and seals the
//! refresh token in the [`vault`]. Autonomous runs, an operator who is not
//! the person, and every other principal get `no_delegation`, and nothing
//! ever falls back to agentd's own credential.
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
#[cfg(feature = "a2a")]
pub mod federation;
pub mod lint;
pub mod unattended;
pub mod vault;

#[cfg(feature = "a2a")]
pub mod cli;

#[cfg(any(feature = "internal-mocks", debug_assertions))]
pub mod mock;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::a2a::principals::typed::{IdpProof, PrincipalRef, Source};
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
    /// The caller's inbound token a `subject: inbound` exchange presents
    /// has expired (RFC 0044 §5.7, M1b); or a firing's CIBA approval has,
    /// or a restart lost it, and a firing asks once (§5.12 E).
    SubjectExpired,
    /// A `via: ciba` firing has not asked its person yet: the loop asks
    /// them, and the call waits (RFC 0044 §5.12 E).
    CibaRequired,
    /// The person denied a firing's CIBA request, or it expired unanswered.
    CibaDenied,
    /// The `act_for` workflow is not the definition the person consented
    /// to run for them: its firings act again once they sign in again.
    UnattendedSuspended,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::SubjectExpired => "subject_expired",
            Code::CibaRequired => "ciba_required",
            Code::CibaDenied => "ciba_denied",
            Code::UnattendedSuspended => "unattended_suspended",
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
    /// A delegated server asks the person whose grant its connection
    /// carries (`elicitation/create`).
    Elicit(Box<Elicitation>),
}

/// Where notes go.
pub type Notify = Arc<dyn Fn(Note) + Send + Sync>;

/// A delegated server's elicitation (RFC 0044 §5.13 item 10, T24): what it
/// asks goes to the person whose grant the connection carries, and to no
/// one else.
#[derive(Debug)]
pub struct Elicitation {
    /// The principal id the person acts as: the gate's owner, and the one
    /// who answers it.
    pub principal: String,
    /// The grant the connection was made with: whoever answers must hold
    /// it, as a consent gate's answerer does.
    pub grant: String,
    pub server: String,
    /// The server's words, which agentd does not vouch for.
    pub message: String,
    pub schema: Value,
    /// How long the server waits for the answer.
    pub timeout: Duration,
    /// The person's answer, or `None`: cancelled.
    pub reply: std::sync::mpsc::SyncSender<Option<Value>>,
}

/// The elicitation handler of a connection on a person's grant: a form the
/// server sends is put to that person alone, and nothing answers it for
/// them — unanswered within the server's wait, it is cancelled. A URL
/// elicitation is declined by the client before it reaches here: agentd
/// never opens a link a server sends.
struct ElicitPerson {
    notify: Option<Notify>,
    principal: String,
    grant: String,
    server: String,
    timeout: Duration,
}

impl ::mcp::inbound::Handler for ElicitPerson {
    fn handle(&self, req: ::mcp::inbound::Inbound) -> Option<::mcp::inbound::Answer> {
        use ::mcp::inbound::{Answer, Inbound};
        let Inbound::Elicit {
            message,
            requested_schema,
        } = req
        else {
            return None;
        };
        let Some(notify) = &self.notify else {
            return Some(Answer::Cancel);
        };
        let (reply, answered) = std::sync::mpsc::sync_channel(1);
        notify(Note::Elicit(Box::new(Elicitation {
            principal: self.principal.clone(),
            grant: self.grant.clone(),
            server: self.server.clone(),
            message,
            schema: requested_schema.clone(),
            timeout: self.timeout,
            reply,
        })));
        match answered.recv_timeout(self.timeout) {
            Ok(Some(a)) => Some(crate::mcp::elicit::shape_reply(&a, &requested_schema)),
            _ => Some(Answer::Cancel),
        }
    }
}

/// A person an `a2a.principals` rule pins: a per-person operator, by the
/// unix listener peer's uid, or a certificate principal, by the one client
/// certificate key its rule admits — each with an exact `idp_subject`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    /// The typed principal's display string: `operator:<id>`, or
    /// `x509:<SPKI SHA-256>` — what a grant and a gate name.
    pub id: String,
    /// The principal id the listener names the caller by, and a run's
    /// authority records: `operator:<id>`, or `<role>:<id>`.
    pub principal: String,
    /// Its rule's `id`, as an `act_for` names it.
    pub rule: String,
    pub typed: PrincipalRef,
    pub role: crate::config::settings::Role,
    /// `unix-uid:<n>`, or `x509:<SPKI SHA-256>`.
    pub fingerprint: String,
    pub iss: String,
    pub sub: String,
    pub labels: BTreeMap<String, String>,
}

impl Person {
    /// Every person `settings` pin.
    pub fn all(settings: &Settings) -> Vec<Person> {
        use crate::config::settings::Role;
        cfg::pinned(settings)
            .filter_map(|rule| {
                let id = rule.id.clone()?;
                let pin = rule.idp_subject.clone()?;
                let (principal, typed, fingerprint) = match (&rule.matcher.uid, &rule.matcher.spki)
                {
                    (Some(uid), _) => (
                        format!("operator:{id}"),
                        PrincipalRef::local(Source::Operator, &id),
                        format!("unix-uid:{uid}"),
                    ),
                    (None, Some(spki)) => (
                        format!(
                            "{}:{id}",
                            if rule.role == Role::Operator {
                                "operator"
                            } else {
                                "user"
                            }
                        ),
                        PrincipalRef::local(Source::X509, spki),
                        format!("x509:{spki}"),
                    ),
                    // The validation refuses a pin on any other rule.
                    (None, None) => return None,
                };
                Some(Person {
                    id: typed.display(),
                    principal,
                    rule: id,
                    typed,
                    role: rule.role,
                    fingerprint,
                    iss: pin.iss,
                    sub: pin.sub,
                    labels: rule.labels.clone(),
                })
            })
            .collect()
    }
}

/// Who a delegated call acts for, as the work it is made in recorded it:
/// its principal, and the RFC 0043 session it acts through, when it was
/// started from one (RFC 0044 §5.12 `session`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Acting<'a> {
    pub principal: Option<&'a str>,
    pub session: Option<&'a str>,
}

impl<'a> Acting<'a> {
    pub fn new(principal: Option<&'a str>, session: Option<&'a str>) -> Acting<'a> {
        Acting { principal, session }
    }
}

/// A principal that may hold a grant, as it stands now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    /// Its display id: `operator:<id>`, `user:oidc:<iss>#<sub>`,
    /// `user:<name>`. A consent gate is addressed to it.
    pub id: String,
    pub typed: PrincipalRef,
    pub role: crate::config::settings::Role,
    pub labels: BTreeMap<String, String>,
    pub kind: WhoKind,
    /// The session it acts through, when it does.
    pub session: Option<String>,
    /// The IdP identity that session itself proved, when it did: what a
    /// device name's grant is held to at every call, and what lets a
    /// session answer the person's consent gates.
    pub proof: Option<IdpProof>,
    /// The firing of an `act_for` workflow the work is, when it is one:
    /// nobody is there (RFC 0044 §5.12 E).
    pub firing: Option<unattended::Firing>,
}

/// What kind of principal a [`Who`] is, and what its grant is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhoKind {
    /// A caller an inbound access token named (RFC 0044 §5.9): the grant
    /// is held to the IdP's subject, as an `a2a.login` principal's is,
    /// through the provider whose issuer the token's is or lists as
    /// equivalent; the token's own issuer and client are kept, and whether
    /// it has expired.
    Inbound {
        issuer: String,
        client: String,
        expired: bool,
    },
    /// A per-person operator: the grant is held to its uid and its pin.
    Person {
        fingerprint: String,
        iss: String,
        sub: String,
    },
    /// An `a2a.login` session: the grant is held to the IdP's subject.
    Oidc,
    /// An RFC 0043 device session: the grant is held to the name's
    /// binding, which this session must itself have proved.
    Device { name: String },
}

impl Who {
    /// The credential fingerprint a grant enrolled as this principal
    /// carries (RFC 0044 §5.2): the uid of a per-person operator, the
    /// IdP's subject of an `a2a.login` principal, and a device name's
    /// binding — the subject its enrolling session proved.
    fn fingerprint(&self, iss: &str, sub: &str) -> String {
        match &self.kind {
            WhoKind::Person { fingerprint, .. } => fingerprint.clone(),
            WhoKind::Oidc | WhoKind::Inbound { .. } => format!("oidc:{iss}#{sub}"),
            WhoKind::Device { .. } => format!("idp:{iss}#{sub}"),
        }
    }

    /// The template `source` this principal is.
    fn source(&self) -> &'static str {
        match self.kind {
            WhoKind::Person { .. } if self.typed.source == Source::X509 => "x509",
            WhoKind::Person { .. } => "operator",
            WhoKind::Oidc | WhoKind::Inbound { .. } => "oidc",
            WhoKind::Device { .. } => "device",
        }
    }

    /// How this person answers their consent gates without a display
    /// session: a per-person operator over the unix listener, a certificate
    /// principal over the connection its certificate authenticates.
    pub fn via_self(&self) -> &'static str {
        if self.typed.source == Source::X509 {
            "certificate"
        } else {
            "operator_self"
        }
    }
}

/// What the delegation supervisor reads of an RFC 0043 session: who it is,
/// and the IdP proof it holds. The listener's session store answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The principal id requests presenting its token act as.
    pub principal: String,
    pub typed: PrincipalRef,
    pub role: crate::config::settings::Role,
    pub proof: Option<IdpProof>,
    /// For an inbound access token's session (RFC 0044 §5.9): the token's
    /// issuer and client, the labels its rule gave, and whether it has
    /// expired — kept a while after, so work reads it as ended.
    pub inbound: Option<InboundInfo>,
}

/// What an inbound token's session says of the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundInfo {
    pub issuer: String,
    pub client: String,
    pub labels: BTreeMap<String, String>,
    pub expired: bool,
}

/// The listener's session store, as the delegation supervisor reads and
/// writes it.
pub trait SessionBook: Send + Sync {
    /// Session `sid`, while it lives — an inbound token's too, expired or
    /// not, while it is kept.
    fn session(&self, sid: &str) -> Option<SessionInfo>;
    /// The token inbound session `sid` holds and when it expires, ms: held
    /// only for an exchange back at its issuer (`subject: inbound`).
    fn inbound_token(&self, _sid: &str) -> Option<(String, u64)> {
        None
    }
    /// Record the IdP identity session `sid` proved; `false` when it is
    /// gone.
    fn set_proof(&self, sid: &str, proof: IdpProof) -> bool;
    /// Drop the proof of every session of the device name `name`.
    fn drop_device_proofs(&self, name: &str) -> Vec<String>;
    /// Every live RFC 0043 session, oldest first, by sid.
    fn sessions(&self) -> Vec<(String, SessionInfo)> {
        Vec::new()
    }
}

#[cfg(feature = "a2a")]
impl SessionBook for crate::a2a::oauth::Sessions {
    fn session(&self, sid: &str) -> Option<SessionInfo> {
        if sid.starts_with(crate::a2a::INBOUND_SID_PREFIX) {
            let now = now_ms();
            return self.inbound(sid).map(|s| SessionInfo {
                principal: s.principal,
                typed: s.typed,
                role: s.role,
                proof: None,
                inbound: Some(InboundInfo {
                    issuer: s.issuer,
                    client: s.client,
                    labels: s.labels,
                    expired: now >= s.expires_ms,
                }),
            });
        }
        crate::a2a::oauth::Sessions::session(self, sid).map(|s| SessionInfo {
            principal: s.principal,
            typed: s.typed,
            role: s.role,
            proof: s.proof,
            inbound: None,
        })
    }
    fn inbound_token(&self, sid: &str) -> Option<(String, u64)> {
        self.inbound(sid)
            .and_then(|s| Some((s.token?, s.expires_ms)))
    }
    fn set_proof(&self, sid: &str, proof: IdpProof) -> bool {
        crate::a2a::oauth::Sessions::set_proof(self, sid, proof)
    }
    fn drop_device_proofs(&self, name: &str) -> Vec<String> {
        self.drop_proofs(|s| s.typed.source == Source::Device && s.typed.stable_id == name)
    }
    fn sessions(&self) -> Vec<(String, SessionInfo)> {
        self.list()
            .into_iter()
            .map(|s| {
                (
                    s.sid.clone(),
                    SessionInfo {
                        principal: s.principal,
                        typed: s.typed,
                        role: s.role,
                        proof: s.proof,
                        inbound: None,
                    },
                )
            })
            .collect()
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

/// How a delegated server is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    /// An ID-JAG from the person's IdP, redeemed at the server's
    /// authorization server (`grant: id_jag`).
    IdJag,
    /// An RFC 8693 on-behalf-of exchange at the server's own authorization
    /// server, which is the person's IdP (`grant: token_exchange`, M1b).
    Exchange(cfg::ExchangeSubject),
}

/// A delegated server.
#[derive(Clone)]
pub struct Server {
    pub name: String,
    pub kind: ServerKind,
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
    /// Whether its token is bound to agentd's DPoP key where its metadata
    /// and its authorization server's advertise it.
    pub dpop: cfg::DpopMode,
    /// What a workflow step's call does when the grant it needs is gone.
    pub on_revoked: cfg::OnRevoked,
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
        let kind = if cfg::is_exchange(m) {
            ServerKind::Exchange(a.subject.unwrap_or_default())
        } else {
            ServerKind::IdJag
        };
        Some(Server {
            name: m.name.clone(),
            kind,
            endpoint: m.endpoint.clone(),
            idp: a.idp.clone()?,
            audience: a.audience.clone()?,
            resource: a.resource_for(&m.endpoint, true)?,
            // An exchange authenticates as the provider's client: the
            // authorization server is the IdP.
            client_id: match kind {
                ServerKind::IdJag => a.client_id.clone()?,
                ServerKind::Exchange(_) => String::new(),
            },
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
            dpop: a.dpop.unwrap_or_default(),
            on_revoked: a.on_revoked.unwrap_or_default(),
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
    /// Bound to agentd's DPoP key: presented as `DPoP`, with a proof.
    dpop: bool,
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

/// The pool's signer: a read of the token minted before the request —
/// with, for a DPoP-bound one, a fresh proof of agentd's key for this very
/// request.
struct Signer {
    token: Arc<Mutex<Option<Held>>>,
    refused: Refused401,
    dpop: Option<SignerDpop>,
}

/// What a signer proves a DPoP-bound token with: the key, the nonces the
/// server issued, and the scheme of its endpoint (a proof names the URL).
struct SignerDpop {
    key: Arc<crate::auth::dpop::DpopKey>,
    nonces: Arc<crate::auth::dpop::Nonces>,
    scheme: String,
}

impl ::mcp::http::RequestSigner for Signer {
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        _body: &[u8],
    ) -> Result<Vec<(String, String)>, String> {
        let held = self
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or("no delegated token is held for this request")?;
        match (&self.dpop, held.dpop) {
            (Some(d), true) => {
                let url = format!("{}://{authority}{path}", d.scheme);
                let proof =
                    d.key
                        .proof(method, &url, Some(&held.at), d.nonces.get(&url).as_deref())?;
                Ok(vec![
                    ("Authorization".to_string(), format!("DPoP {}", held.at)),
                    ("DPoP".to_string(), proof),
                ])
            }
            (None, true) => Err("a DPoP-bound token is held, and no key to prove it with".into()),
            _ => Ok(vec![(
                "Authorization".to_string(),
                format!("Bearer {}", held.at),
            )]),
        }
    }

    fn on_response(&self, resp: &::mcp::http::AuthResponse, authority: &str) -> bool {
        // A nonce the server issued is used from the next proof on, and a
        // proof it refused for want of one is sent once more with it.
        if let (Some(d), Some(n)) = (&self.dpop, &resp.dpop_nonce) {
            let url = format!("{}://{authority}/", d.scheme);
            let fresh = d.nonces.get(&url).as_deref() != Some(n.as_str());
            d.nonces.learn(&url, n);
            if fresh
                && resp.status == 401
                && resp
                    .www_authenticate
                    .as_deref()
                    .is_some_and(|w| w.contains(idjag::wire::USE_DPOP_NONCE))
            {
                return true;
            }
        }
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
    /// The `max_age` an IdP asked a step-up for, until a sign-in meets it,
    /// and when it asked, seconds since the epoch.
    step_up: Option<u64>,
    step_up_at: u64,
    /// A fresh ID token from a step-up, its `auth_time` and `exp`, and the
    /// call it was signed in for: the subject of that call's mint and no
    /// other's (A2A §7.6.4).
    fresh_id_token: Option<FreshIdToken>,
    /// ID-JAGs, memory only, by `(ras, resource, scopes)`.
    id_jags: BTreeMap<String, (String, u64)>,
    /// A probe is running.
    probing: bool,
    /// The data key a rotated refresh token would be kept with, wrapped
    /// before the IdP is asked ([`vault::Ahead`]).
    ahead: Option<vault::Ahead>,
}

/// A step-up sign-in's ID token: its `auth_time`, its `exp`, and the call
/// it resumes (`None`: the next mint, for a sign-in with `agentd --login
/// idp:<name>`).
#[derive(Clone)]
struct FreshIdToken {
    token: String,
    auth_time: u64,
    exp: u64,
    call: Option<CallOf>,
}

impl FreshIdToken {
    /// Whether this is the subject of `call`'s mint at `now_s`: unexpired,
    /// signed in for that very call — its server, its unit of work and its
    /// step — or, from `--login`, for whichever mint is next; and at least
    /// as recent as the `max_age` a step-up asked.
    fn serves(&self, call: &CallOf, step_up: Option<u64>, now_s: u64) -> bool {
        self.exp > now_s + 30
            && self.call.as_ref().is_none_or(|k| k == call)
            && step_up.is_none_or(|m| m == 0 || self.auth_time + m >= now_s)
    }
}

/// How far a sign-in's `auth_time` may be ahead of the moment a step-up was
/// asked, the clocks of agentd and the IdP apart.
const AUTH_TIME_SKEW_S: u64 = 60;

/// Whether a sign-in authenticated at `auth_time` meets a step-up the IdP
/// asked at `asked_s` with `max_age` (RFC 0044 §5.10 L4): its ID token says
/// when the person authenticated (OIDC Core §3.1.2.1 requires `auth_time`
/// beside a `max_age`), within `max_age` of `now_s` — or, for an IdP that
/// named no `max_age`, no earlier than the ask: a session the IdP reused
/// from before raises no assurance. Why not, when not.
pub(crate) fn auth_time_meets(
    auth_time: Option<u64>,
    max_age: u64,
    asked_s: u64,
    now_s: u64,
) -> Result<(), String> {
    let Some(t) = auth_time else {
        return Err(
            "its ID token carries no auth_time, so it cannot show it is as recent as the IdP asked"
                .into(),
        );
    };
    if max_age > 0 && t + max_age < now_s {
        return Err(format!(
            "the person authenticated {}s ago, and the IdP asked for a sign-in within {max_age}s",
            now_s.saturating_sub(t)
        ));
    }
    if max_age == 0 && t + AUTH_TIME_SKEW_S < asked_s {
        return Err(format!(
            "the person authenticated {}s before the IdP asked them to sign in again",
            asked_s - t
        ));
    }
    Ok(())
}

/// A delegated call as a step-up sign-in is bound to it: its server, the
/// unit of work it is made in (`Call::unit`) and its step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallOf {
    pub server: String,
    pub unit: String,
    pub step: Option<String>,
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
    /// Who may enrol, for what: reloadable (RFC 0044 §5.14).
    templates: RwLock<Vec<Template>>,
    /// Device names pinned to one IdP identity each: reloadable.
    pins: RwLock<BTreeMap<String, cfg::IdpSubject>>,
    /// Whether an unpinned device name binds at its first enrolment.
    first_binding: AtomicBool,
    pub persons: Vec<Person>,
    /// Inbound token issuers, each to the issuer of the provider that lists
    /// it among its `equivalent_issuers` (RFC 0044 §5.9).
    equivalents: BTreeMap<String, String>,
    /// The listener's session store, once the listener is up.
    book: OnceLock<Arc<dyn SessionBook>>,
    /// The step-up sign-ins consent gates ask for: gate → the grant, and
    /// the call the gate holds.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    stepping: Mutex<BTreeMap<String, (String, CallOf)>>,
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
    /// When each `(grant, resource)` minted in the last minute.
    minted: Mutex<BTreeMap<String, std::collections::VecDeque<u64>>>,
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
    /// The `act_for` workflows installed: reloaded with them.
    acts: RwLock<Vec<unattended::ActForDecl>>,
    /// The firings acting now, by the session id their work acts through.
    firings: Mutex<BTreeMap<String, unattended::Running>>,
    /// Held while the refresh tokens CIBA approvals left in the store
    /// before this start are revoked; true once they are.
    approvals_swept: Mutex<bool>,
    /// When a reactor pass next revokes them, while that has not finished.
    approvals_due: Mutex<unattended::SweepDue>,
    /// The key DPoP-bound tokens are bound to, made at its first use and
    /// held in memory alone (RFC 0044 T23).
    dpop: OnceLock<Arc<crate::auth::dpop::DpopKey>>,
    /// The DPoP nonces servers issued, by origin.
    nonces: Arc<crate::auth::dpop::Nonces>,
    /// The servers whose tokens are bound to that key: their metadata and
    /// their authorization server's advertise ES256 proofs.
    bound_servers: Mutex<BTreeSet<String>>,
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
    /// The run's authority: who the work is for, and the session it acts
    /// through.
    pub acting: Acting<'a>,
    /// The run or conversation the call is made in, for its budget.
    pub unit: String,
    pub run: Option<String>,
    pub step: Option<String>,
}

/// What a delegated call needs before it is dispatched: who, and which
/// grant under which template, checked on the loop with no network.
pub struct Admitted {
    pub grant: Grant,
    pub who: Who,
    pub template: Template,
}

/// What a grant a device name's first enrolment bound records.
pub const BOUND_BY_FIRST: &str = "first_binding";
/// What a grant a device name's pin bound records.
pub const BOUND_BY_PIN: &str = "pin";

/// Whether template `t` admits `who` at `idp`: its IdP, its source, and —
/// where it names them — its role and every label; a caller an inbound
/// token named, only through a client the template lists, where it lists
/// any (RFC 0044 §5.9).
fn admits(t: &Template, who: &Who, idp: &str) -> bool {
    t.idp == idp
        && t.principals.source.as_deref() == Some(who.source())
        && t.principals.role.is_none_or(|r| r == who.role)
        && t.principals
            .labels
            .iter()
            .all(|(k, v)| who.labels.get(k) == Some(v))
        && match &who.kind {
            WhoKind::Inbound { client, .. } => t.clients.is_empty() || t.clients.contains(client),
            _ => true,
        }
}

/// How `who` enrols at `idp`, for a notice.
fn enrol_hint(who: &Who, idp: &str) -> String {
    match who.kind {
        WhoKind::Person { .. } if who.typed.source == Source::X509 => {
            format!("they enrol with delegation.enrol {{idp: {idp}}} over their certificate")
        }
        WhoKind::Person { .. } => format!("they enrol with `agentd --login idp:{idp}`"),
        WhoKind::Oidc => "they sign in at the IdP again from the display client".into(),
        WhoKind::Inbound { .. } => {
            "they sign in at the IdP through the link a workflow that needs it offers, or from a display client".into()
        }
        WhoKind::Device { .. } => {
            "they sign in at the IdP from the display client (delegation.enrol)".into()
        }
    }
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
            templates: RwLock::new(dl.templates.clone()),
            pins: RwLock::new(dl.device_pins.clone()),
            first_binding: AtomicBool::new(dl.device_first_binding),
            persons: Person::all(settings),
            equivalents: settings
                .identity
                .providers
                .values()
                .flat_map(|p| {
                    p.equivalent_issuers
                        .iter()
                        .map(|e| (e.clone(), p.issuer.clone()))
                })
                .collect(),
            book: OnceLock::new(),
            stepping: Mutex::new(BTreeMap::new()),
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
            minted: Mutex::new(BTreeMap::new()),
            handles: Mutex::new(BTreeMap::new()),
            listing: Mutex::new(BTreeSet::new()),
            confirmed: Mutex::new(BTreeSet::new()),
            personal: Mutex::new(BTreeMap::new()),
            personal_open: Mutex::new(()),
            acts: RwLock::new(Vec::new()),
            firings: Mutex::new(BTreeMap::new()),
            approvals_swept: Mutex::new(false),
            approvals_due: Mutex::new(Default::default()),
            dpop: OnceLock::new(),
            nonces: Arc::default(),
            bound_servers: Mutex::new(BTreeSet::new()),
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

    /// Where the listener's session store is: every call made through a
    /// session is admitted against it.
    pub fn sessions_to(&self, book: Arc<dyn SessionBook>) {
        let _ = self.book.set(book);
    }

    fn book(&self) -> Option<&Arc<dyn SessionBook>> {
        self.book.get()
    }

    /// Whether session `sid` is still live: none is once it was revoked,
    /// expired, or lost to a restart, and none is where no listener issues
    /// sessions.
    pub fn session_live(&self, sid: &str) -> bool {
        if sid.starts_with(crate::engine::run::UNATTENDED_SID_PREFIX) {
            return self.firing_live(sid);
        }
        self.book().is_some_and(|b| b.session(sid).is_some())
    }

    /// The newest live session of `principal` holding a proof of the very
    /// identity their grant for `server` is bound to: what a run whose
    /// session ended moves to (RFC 0044 §5.12 C).
    pub fn rebind(&self, principal: &str, server: &str) -> Option<String> {
        let s = self.servers.get(server)?;
        let issuer = &self.providers.get(&s.idp)?.cfg.issuer;
        let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
        self.book()?
            .sessions()
            .into_iter()
            .rev()
            .find(|(_, info)| {
                info.principal == principal
                    && info.proof.as_ref().is_some_and(|p| {
                        grants
                            .get(&info.typed.grant_id(issuer))
                            .is_some_and(|g| g.idp_iss == p.iss && g.sub == p.sub)
                    })
            })
            .map(|(sid, _)| sid)
    }

    /// The templates in force.
    pub fn templates(&self) -> Vec<Template> {
        self.templates
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The pin of device name `name`, when it has one.
    pub fn pin_of(&self, name: &str) -> Option<cfg::IdpSubject> {
        self.pins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    fn first_binding(&self) -> bool {
        self.first_binding.load(Ordering::SeqCst)
    }

    /// The pinned person the listener names `principal`.
    pub fn person(&self, principal: &str) -> Option<&Person> {
        self.persons.iter().find(|p| p.principal == principal)
    }

    /// Who `acting` is, as its session or the configuration says now: a
    /// call through a session that has ended is `revoked` — the session's
    /// delegated access ended with it — and one by a principal no grant
    /// can be held by is `no_delegation`.
    pub fn who(&self, acting: Acting<'_>) -> Result<Who, DelegationError> {
        use crate::config::settings::Role;
        let nobody = |why: String| refuse(Code::NoDelegation, why);
        // A firing of an `act_for` workflow acts for the person it names,
        // while its run lives (RFC 0044 §5.12 E).
        if let Some(sid) = acting
            .session
            .filter(|s| s.starts_with(crate::engine::run::UNATTENDED_SID_PREFIX))
        {
            return self.firing_who(sid);
        }
        if let Some(sid) = acting.session {
            let Some(s) = self.book().and_then(|b| b.session(sid)) else {
                return Err(refuse(
                    Code::Revoked,
                    format!(
                        "the session {sid} this work acts through has ended — revoked, expired or lost to a restart — and its delegated access with it"
                    ),
                ));
            };
            if acting.principal.is_some_and(|p| p != s.principal) {
                return Err(refuse(
                    Code::Revoked,
                    format!("the session {sid} is not this work's principal's"),
                ));
            }
            // A caller an inbound token named (RFC 0044 §5.9): a person
            // only, and by the provider whose issuer the token's is or lists
            // as equivalent — never across issuers that were not declared.
            if let Some(ib) = s.inbound {
                if s.role != Role::User {
                    return Err(refuse(
                        Code::PrincipalNotDelegable,
                        format!(
                            "{} is a client's own token, which names no person: it never acts with a grant",
                            s.principal
                        ),
                    ));
                }
                let mut typed = s.typed;
                typed.client = None;
                if let Some(iss) = self.equivalents.get(&typed.issuer) {
                    typed.issuer = iss.clone();
                }
                return Ok(Who {
                    id: s.principal,
                    typed,
                    role: Role::User,
                    labels: ib.labels,
                    kind: WhoKind::Inbound {
                        issuer: ib.issuer,
                        client: ib.client,
                        expired: ib.expired,
                    },
                    session: Some(sid.to_string()),
                    proof: None,
                    firing: None,
                });
            }
            let kind = match s.typed.source {
                Source::Oidc => WhoKind::Oidc,
                Source::Device => WhoKind::Device {
                    name: s.typed.stable_id.clone(),
                },
                _ => {
                    return Err(nobody(format!(
                        "{} acts through a session no grant is held by: delegated servers act with an enrolled person's grant, never agentd's own credential",
                        s.principal
                    )));
                }
            };
            return Ok(Who {
                id: s.typed.display(),
                typed: s.typed,
                role: if matches!(kind, WhoKind::Oidc) {
                    Role::User
                } else {
                    s.role
                },
                labels: BTreeMap::new(),
                kind,
                session: Some(sid.to_string()),
                proof: s.proof,
                firing: None,
            });
        }
        let Some(p) = acting.principal.and_then(|id| self.person(id)) else {
            return Err(nobody(format!(
                "this work is {}'s, and delegated servers act with an enrolled person's grant: it is never done with agentd's own credential",
                acting.principal.unwrap_or("nobody")
            )));
        };
        Ok(Who {
            id: p.id.clone(),
            typed: p.typed.clone(),
            role: p.role,
            labels: p.labels.clone(),
            kind: WhoKind::Person {
                fingerprint: p.fingerprint.clone(),
                iss: p.iss.clone(),
                sub: p.sub.clone(),
            },
            session: None,
            proof: None,
            firing: None,
        })
    }

    /// What a first use the person approved for work `acting` is for is
    /// bound to (RFC 0044 §5.10 rule 6): every grant they hold, with its
    /// epoch, and every template that admits them, as each is now — so a
    /// revoked or re-enrolled grant, or a changed template, makes it
    /// another binding and the approval no longer stands. With no network.
    pub fn acting_binding(&self, acting: Acting<'_>) -> Option<String> {
        let who = self.who(acting).ok()?;
        let mut parts = Vec::new();
        {
            let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            for idp in self.providers.values() {
                if let Some(g) = grants.get(&who.typed.grant_id(&idp.cfg.issuer)) {
                    parts.push(format!("grant {} {} {}", g.grant_id, g.epoch, g.template));
                }
            }
        }
        for t in self
            .templates
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|t| admits(t, &who, &t.idp))
        {
            parts.push(format!(
                "template {} {}",
                t.name,
                crate::sha::sha256_hex(format!("{t:?}").as_bytes())
            ));
        }
        parts.sort();
        Some(crate::sha::sha256_hex(parts.join("\n").as_bytes()))
    }

    /// Whether `acting` holds a usable grant for `server`, with no
    /// network: what decides whether its tools are offered at all.
    pub fn usable(&self, acting: Acting<'_>, server: &str) -> bool {
        self.admit(acting, server).is_ok()
    }

    /// The templates of the grants `acting` acts with now — each grant the
    /// person holds at any IdP that is theirs to use through the session the
    /// work acts through, unexpired, under the template that admits them —
    /// or `None` when it acts with nobody's: agentd's own authority (RFC
    /// 0044 §5.3). With no network.
    pub fn acting_templates(&self, acting: Acting<'_>) -> Option<Vec<Template>> {
        let who = self.who(acting).ok()?;
        let held: Vec<(String, Grant)> = {
            let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            self.providers
                .iter()
                .filter_map(|(name, idp)| {
                    let g = grants.get(&who.typed.grant_id(&idp.cfg.issuer))?;
                    Some((name.clone(), g.clone()))
                })
                .collect()
        };
        let templates = self.templates.read().unwrap_or_else(|e| e.into_inner());
        let out: Vec<Template> = held
            .iter()
            .filter(|(idp, g)| self.held_to(&who, g, idp).is_ok() && now_ms() < g.expires_ms)
            .filter_map(|(idp, g)| {
                templates
                    .iter()
                    .find(|t| t.name == g.template && admits(t, &who, idp))
                    .cloned()
            })
            .collect();
        (!out.is_empty()).then_some(out)
    }

    /// The checks a delegated call passes before anything is dispatched:
    /// who the work is for, through the session it acts through as that
    /// session is now; their grant is active, current, unexpired and
    /// theirs — for a device name, bound to the very identity this session
    /// proved — and allows `server`. The grant handed back carries the
    /// ceilings it was enrolled with as the template that allowed it
    /// narrows them now: a template narrowed takes effect at the next call,
    /// and widening one takes a new enrolment.
    pub fn admit(&self, acting: Acting<'_>, server: &str) -> Result<Admitted, DelegationError> {
        if !self.servers.contains_key(server) {
            return Err(refuse(
                Code::Config,
                format!("{server} is not a delegated server"),
            ));
        }
        self.admit_who(self.who(acting)?, server)
    }

    /// [`Self::admit`] for `who` as they stand.
    fn admit_who(&self, who: Who, server: &str) -> Result<Admitted, DelegationError> {
        let s = self
            .servers
            .get(server)
            .ok_or_else(|| refuse(Code::Config, format!("{server} is not a delegated server")))?;
        let exchanges_inbound = s.kind == ServerKind::Exchange(cfg::ExchangeSubject::Inbound);
        // An inbound token's work acts through it while it is good, and no
        // longer (RFC 0044 §5.9): the call is `revoked` — or, for an
        // exchange of the token itself, `subject_expired` (§5.7).
        if let WhoKind::Inbound { expired: true, .. } = &who.kind {
            return Err(if exchanges_inbound {
                refuse(
                    Code::SubjectExpired,
                    format!(
                        "the inbound token {server} would exchange for {} has expired: they call again with a fresh one",
                        who.id
                    ),
                )
            } else {
                refuse(
                    Code::Revoked,
                    format!(
                        "the inbound token this work acts through for {} has expired, and its delegated access with it",
                        who.id
                    ),
                )
            });
        }
        if exchanges_inbound {
            return self.admit_inbound(who, s);
        }
        let issuer = &self
            .providers
            .get(&s.idp)
            .ok_or_else(|| {
                refuse(
                    Code::Config,
                    format!("{server}'s IdP {} is not declared", s.idp),
                )
            })?
            .cfg
            .issuer;
        let gid = who.typed.grant_id(issuer);
        let held = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&gid)
            .cloned();
        let Some(grant) = held else {
            // A revocation is said as one for as long as the tombstone
            // register keeps its record, a restart included: the person
            // enrols again, and nothing is presented meanwhile.
            if self
                .vault
                .floors()
                .get(&gid)
                .is_some_and(|f| f.revoked_epoch.is_some())
            {
                return Err(refuse(
                    Code::Revoked,
                    format!(
                        "{}'s grant at {} was revoked: they enrol again to act through it",
                        who.id, s.idp
                    ),
                ));
            }
            return Err(refuse(
                Code::NotEnrolled,
                format!(
                    "{} has no grant at {}: {}",
                    who.id,
                    s.idp,
                    enrol_hint(&who, &s.idp)
                ),
            ));
        };
        self.held_to(&who, &grant, &s.idp)?;
        if now_ms() >= grant.expires_ms {
            return Err(refuse(
                Code::NeedsReenroll,
                format!(
                    "{}'s grant at {} has expired: it is never extended — enrol again",
                    who.id, s.idp
                ),
            ));
        }
        let templates = self.templates.read().unwrap_or_else(|e| e.into_inner());
        let Some(template) = templates
            .iter()
            .find(|t| t.name == grant.template && admits(t, &who, &s.idp))
        else {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{}'s grant was allowed by template {}, which no longer admits them at {}: enrol again under the template that does",
                    who.id, grant.template, s.idp
                ),
            ));
        };
        let mut grant = grant;
        grant.ceilings = narrowed(&grant.ceilings, template);
        let template = template.clone();
        drop(templates);
        // A firing acts only as the template allows it to, and only while
        // its workflow is the definition the person consented to.
        if let Some(f) = &who.firing {
            self.firing_allowed(&who.id, f, &grant, &template)?;
        }
        if !grant.ceilings.contains_key(server) {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{}'s grant (template {}) does not allow {server}",
                    who.id, grant.template
                ),
            ));
        }
        Ok(Admitted {
            grant,
            who,
            template,
        })
    }

    /// The admission of a `subject: inbound` exchange (RFC 0044 §5.7, M1b):
    /// the work acts through an inbound token issued by the server's own
    /// authorization server — that issuer exactly, never one declared
    /// equivalent, since the token goes back to it alone — for a person a
    /// template admits there. No grant is stored: the token is the
    /// subject, and what is handed back is the template's ceiling under
    /// the key the person's grant would have, alive while the token is.
    fn admit_inbound(&self, who: Who, s: &Server) -> Result<Admitted, DelegationError> {
        let idp = self.providers.get(&s.idp).ok_or_else(|| {
            refuse(
                Code::Config,
                format!("{}'s IdP {} is not declared", s.name, s.idp),
            )
        })?;
        let WhoKind::Inbound { issuer, .. } = &who.kind else {
            return Err(refuse(
                Code::NoDelegation,
                format!(
                    "{} exchanges the caller's own inbound token, and this work ({}) was not started by one",
                    s.name, who.id
                ),
            ));
        };
        if *issuer != idp.cfg.issuer {
            return Err(refuse(
                Code::NoDelegation,
                format!(
                    "{} exchanges a token at {}, and this work's token is {issuer}'s: it is sent back to its own issuer alone",
                    s.name, idp.cfg.issuer
                ),
            ));
        }
        let expires_ms = who
            .session
            .as_deref()
            .and_then(|sid| self.book()?.inbound_token(sid))
            .map(|(_, exp)| exp)
            .ok_or_else(|| {
                refuse(
                    Code::SubjectExpired,
                    format!(
                        "no inbound token of {} is held for {}: they call again",
                        who.id, s.name
                    ),
                )
            })?;
        let template = self.template_for(&who, &s.idp).ok_or_else(|| {
            refuse(
                Code::DelegationNotPermitted,
                format!(
                    "no identity.delegation.templates entry admits {} at {}",
                    who.id, s.idp
                ),
            )
        })?;
        let ceilings = ceilings_of(&template);
        if !ceilings.contains_key(&s.name) {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{}'s template {} does not allow {}",
                    who.id, template.name, s.name
                ),
            ));
        }
        let iss = idp.cfg.issuer.clone();
        let grant = Grant {
            grant_id: who.typed.grant_id(&iss),
            principal: who.typed.clone(),
            fingerprint: who.fingerprint(&iss, &who.typed.stable_id),
            idp: s.idp.clone(),
            idp_iss: iss,
            sub: who.typed.stable_id.clone(),
            email_hash: None,
            template: template.name.clone(),
            ceilings,
            created_ms: now_ms(),
            expires_ms,
            epoch: 0,
            rt_version: 0,
            quota: Default::default(),
            bound_by: None,
            unattended: BTreeMap::new(),
            ciba: None,
        };
        Ok(Admitted {
            grant,
            who,
            template,
        })
    }

    /// Whether `grant` is `who`'s to use as they stand now: a per-person
    /// operator's and an `a2a.login` principal's enrolled as the same
    /// identity; a device name's bound to the very `iss` + `sub` this
    /// session proved, to the name's pin if it has one, and — bound by the
    /// first enrolment — only while first binding is on.
    fn held_to(&self, who: &Who, grant: &Grant, idp: &str) -> Result<(), DelegationError> {
        let not = |why: String| refuse(Code::NotEnrolled, why);
        match &who.kind {
            WhoKind::Device { name } => {
                let Some(proof) = &who.proof else {
                    return Err(not(format!(
                        "{} is bound at {idp}, and this session has not signed in there: sign in at the IdP from the display client to use the grant (delegation.enrol)",
                        who.id
                    )));
                };
                if proof.iss != grant.idp_iss || proof.sub != grant.sub {
                    return Err(not(format!(
                        "this session proved another IdP identity than {} is bound to",
                        who.id
                    )));
                }
                if let Some(pin) = self.pin_of(name)
                    && (pin.iss != grant.idp_iss || pin.sub != grant.sub)
                {
                    return Err(not(format!(
                        "{}'s grant was bound to another identity than its pin names now: enrol again",
                        who.id
                    )));
                }
                if grant.bound_by.as_deref() == Some(BOUND_BY_FIRST) && !self.first_binding() {
                    return Err(not(format!(
                        "{}'s grant was bound by its first enrolment, and identity.delegation.device_first_binding is off",
                        who.id
                    )));
                }
                Ok(())
            }
            _ => {
                if grant.fingerprint != who.fingerprint(&grant.idp_iss, &grant.sub)
                    || matches!(&who.kind, WhoKind::Person { iss, sub, .. } if *iss != grant.idp_iss || *sub != grant.sub)
                {
                    return Err(not(format!(
                        "{}'s grant was enrolled as another identity than the configuration pins now: enrol again",
                        who.id
                    )));
                }
                Ok(())
            }
        }
    }

    /// Whether grant `grant_id` is `who`'s to act with as they stand now —
    /// theirs, and for a device name bound to the very identity their
    /// session proved ([`Self::held_to`]): what answering a question the
    /// grant's connection was asked takes (RFC 0044 T24).
    pub fn holds(&self, who: &Who, grant_id: &str) -> Result<(), DelegationError> {
        let held = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(grant_id)
            .cloned();
        let Some(grant) = held.filter(|g| g.principal == who.typed) else {
            return Err(refuse(
                Code::NotEnrolled,
                format!("{} holds no such grant", who.id),
            ));
        };
        self.held_to(who, &grant, &grant.idp)
    }

    /// The template that admits `who` at `idp`.
    pub fn template_for(&self, who: &Who, idp: &str) -> Option<Template> {
        self.templates
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| admits(t, who, idp))
            .cloned()
    }

    /// Whether `who` may begin an enrolment at `idp` now (RFC 0044 §5.4
    /// steps 1 and 4, before anyone is sent to the IdP): a template must
    /// admit them, and a device name must be pinned, bound already, or
    /// first-bindable.
    pub fn eligible(&self, who: &Who, idp: &str) -> Result<Template, DelegationError> {
        if !self.providers.contains_key(idp) {
            return Err(refuse(
                Code::Config,
                format!("{idp:?} is not an identity.providers entry"),
            ));
        }
        let t = self.template_for(who, idp).ok_or_else(|| {
            refuse(
                Code::DelegationNotPermitted,
                format!(
                    "no identity.delegation.templates entry admits {} at {idp}",
                    who.id
                ),
            )
        })?;
        if let WhoKind::Device { name, .. } = &who.kind
            && self.pin_of(name).is_none()
            && !self.first_binding()
            && self.bound(who, idp).is_none()
        {
            return Err(refuse(
                Code::PrincipalNotDelegable,
                format!(
                    "{} is an operator-named device, not an IdP identity: it delegates only with identity.delegation.device_pins.{name}, or with device_first_binding",
                    who.id
                ),
            ));
        }
        Ok(t)
    }

    /// The grant `who` holds at `idp` now, whoever enrolled it.
    fn bound(&self, who: &Who, idp: &str) -> Option<Grant> {
        let issuer = &self.providers.get(idp)?.cfg.issuer;
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&who.typed.grant_id(issuer))
            .cloned()
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
        // The authorization server it names: the pinned audience of an
        // ID-JAG's redemption, the provider itself of an exchange.
        let (asrv, what) = match s.kind {
            ServerKind::IdJag => (s.audience.clone(), "the pinned audience"),
            ServerKind::Exchange(_) => (
                self.providers
                    .get(&s.idp)
                    .map(|p| p.cfg.issuer.clone())
                    .unwrap_or_default(),
                "its IdP's issuer",
            ),
        };
        if !m.authorization_servers.contains(&asrv) {
            return Err(not(format!(
                "its protected-resource metadata names the authorization servers {:?}, not {what} {asrv}",
                m.authorization_servers
            )));
        }
        // DPoP on the authorization-server and MCP legs (T23): where the
        // server and its authorization server both take ES256 proofs, unless
        // `auth.dpop: off`; a server that takes bound tokens alone is
        // refused when its token cannot be bound.
        if s.kind == ServerKind::IdJag {
            let prm = m
                .dpop_signing_alg_values_supported
                .iter()
                .any(|a| a == crate::auth::dpop::ALG);
            let ras = self.ras(s)?;
            let on = s.dpop == cfg::DpopMode::Auto && prm && ras.dpop;
            if m.dpop_bound_access_tokens_required && !on {
                return Err(not(format!(
                    "its protected-resource metadata takes DPoP-bound tokens alone, and {}",
                    if s.dpop == cfg::DpopMode::Off {
                        "auth.dpop is off".to_string()
                    } else if !prm {
                        "lists no ES256 proofs".to_string()
                    } else {
                        format!("{} advertises no ES256 DPoP proofs", ras.issuer)
                    }
                )));
            }
            if on {
                self.dpop_key().map_err(not)?;
                self.bound_servers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(s.name.clone());
            }
        }
        self.confirmed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(s.name.clone());
        Ok(())
    }

    /// The key DPoP-bound tokens are bound to: made once a process, never
    /// written anywhere.
    fn dpop_key(&self) -> Result<Arc<crate::auth::dpop::DpopKey>, String> {
        if let Some(k) = self.dpop.get() {
            return Ok(Arc::clone(k));
        }
        let made = Arc::new(crate::auth::dpop::DpopKey::generate()?);
        Ok(Arc::clone(self.dpop.get_or_init(|| made)))
    }

    /// Whether server `s`'s tokens are DPoP-bound.
    fn dpop_bound(&self, s: &Server) -> bool {
        self.bound_servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&s.name)
    }

    /// Redeem `jag` at `s`'s authorization server `ras`: with a DPoP proof
    /// where its token is to be bound.
    fn redeem(
        &self,
        s: &Server,
        ras: &idjag::RasMeta,
        jag: &str,
    ) -> Result<idjag::Redeemed, Refusal> {
        let key = if self.dpop_bound(s) {
            Some(self.dpop_key().map_err(Refusal::Unavailable)?)
        } else {
            None
        };
        idjag::redeem(
            &s.params(&ras.token_endpoint),
            jag,
            &s.resource,
            key.as_deref().map(|k| (k, &*self.nonces)),
            self.timeout,
        )
    }

    /// The pinned authorization server's metadata, read once.
    fn ras(&self, s: &Server) -> Result<idjag::RasMeta, DelegationError> {
        if let Some(t) = &s.token_url {
            return Ok(idjag::RasMeta {
                issuer: s.audience.clone(),
                token_endpoint: t.clone(),
                dpop: false,
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

    /// The pool entry `(server, grant, scopes)` — a `via: ciba` firing's
    /// own, under `scope`.
    fn entry(
        &self,
        server: &str,
        grant: &str,
        scopes: &[String],
        scope: Option<&str>,
    ) -> (String, Arc<Entry>) {
        let mut key = format!("{server}|{grant}|{}", scopes.join(" "));
        if let Some(sid) = scope {
            key.push('|');
            key.push_str(sid);
        }
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

    /// The live secret values a call made for `acting` with `grant` is
    /// scrubbed of: the grant's, and the inbound token the work acts
    /// through, where one is held — a server's answer or error can quote
    /// the subject it was exchanged from.
    fn secrets_of(&self, grant: &str, acting: Acting<'_>) -> Vec<String> {
        let mut out = self.live_values(grant, None);
        if let Some((t, _)) = acting
            .session
            .and_then(|sid| self.book()?.inbound_token(sid))
        {
            out.push(t);
        }
        if let Some(sid) = self.ciba_scope(acting) {
            out.extend(self.firing_values(&sid));
        }
        out
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
            if let Some(f) = &l.fresh_id_token {
                out.push(f.token.clone());
            }
        });
        out
    }

    /// The token a request to `s` for `tool` presents: the pooled one while
    /// it may be used, a re-presented ID-JAG's, or a new mint. `fresh`
    /// excludes re-presentation after a 401.
    #[allow(clippy::too_many_arguments)]
    fn token(
        &self,
        s: &Server,
        grant: &Grant,
        scopes: &[String],
        entry: &Entry,
        c: &Call<'_>,
        firing: Option<&unattended::Firing>,
        allow_reuse: bool,
    ) -> Result<(), DelegationError> {
        if let ServerKind::Exchange(subject) = s.kind {
            return self.exchanged(s, subject, grant, scopes, entry, c, firing);
        }
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
        // A `via: ciba` firing's approval is its own: no other work
        // re-presents what was minted with it, and it re-presents nothing
        // minted without it. The firing is the one the call was admitted
        // as: one that ended meanwhile has no approval, and mints nothing.
        let scope = unattended::ciba_sid(firing);
        let approval = self.approval(scope, grant)?;
        let mut jag_key = format!("{}|{}|{}", ras.issuer, s.resource, scopes.join(" "));
        if let Some(sid) = scope {
            jag_key.push('|');
            jag_key.push_str(sid);
        }
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
            match self.redeem(s, &ras, &jag) {
                Ok(r) => {
                    let sub = idjag::decode_unverified(&jag)
                        .ok()
                        .and_then(|(_, cl)| cl["sub"].as_str().map(str::to_string));
                    self.hold(
                        s,
                        grant,
                        entry,
                        &r,
                        sub,
                        approval.as_ref().map(|(_, exp)| *exp),
                    );
                    self.audit(
                        "delegation.minted",
                        "reused",
                        Some(&grant.principal.display()),
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
        self.mint(s, grant, scopes, entry, c, firing, &ras, &jag_key)
    }

    /// Put a redeemed access token in the pool: used no longer than the
    /// caps allow, nor past `approval_exp`, the expiry of the approval a
    /// `via: ciba` firing minted it with.
    fn hold(
        &self,
        s: &Server,
        grant: &Grant,
        entry: &Entry,
        r: &idjag::Redeemed,
        sub: Option<String>,
        approval_exp: Option<u64>,
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
            .min(grant.expires_ms)
            .min(approval_exp.map_or(u64::MAX, |e| e * 1000));
        *entry.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(Held {
            at: r.access_token.clone(),
            until_ms: until,
            fresh: true,
            sub,
            dpop: r.dpop,
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
        firing: Option<&unattended::Firing>,
        ras: &idjag::RasMeta,
        jag_key: &str,
    ) -> Result<(), DelegationError> {
        let approval = self.approval(unattended::ciba_sid(firing), grant)?;
        let via = firing.map(|f| f.via);
        let budget = self.paced(grant, s, c)?;
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
                let call = CallOf {
                    server: s.name.clone(),
                    unit: c.unit.clone(),
                    step: c.step.clone(),
                };
                let (subject, subject_type) = self.subject(
                    idp,
                    &params,
                    &mut g,
                    &mut rt,
                    &call,
                    approval.as_ref().map(|(t, _)| t.as_str()),
                )?;
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
                                Some(&g.principal.display()),
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
                        // A sign-in another call waits to resume with is
                        // that call's still.
                        self.with_live(&g.grant_id, |l| {
                            l.step_up = Some(max_age.unwrap_or(0));
                            l.step_up_at = now_ms() / 1000;
                            if l.fresh_id_token
                                .as_ref()
                                .is_some_and(|f| f.call.as_ref().is_none_or(|k| k == &call))
                            {
                                l.fresh_id_token = None;
                            }
                        });
                        self.audit(
                            "delegation.step_up",
                            "asked",
                            Some(&g.principal.display()),
                            json!({"grant_id": g.grant_id, "server": s.name, "tool": c.tool, "max_age": max_age}),
                        );
                        return Err(refuse(
                            Code::StepUp,
                            format!(
                                "the IdP wants a sign-in within {}s before it acts at {}: the person signs in again at {}, and the call is made again",
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
            Some(&grant.principal.display()),
            json!({"grant_id": grant.grant_id, "run": c.run, "step": c.step, "tool": c.tool,
                   "ras_iss": ras.issuer, "resource": s.resource, "scope_granted": x.scopes,
                   "narrowed": x.narrowed, "jti_sha256": crate::sha::sha256_hex(x.jti.as_bytes()),
                   "exp": x.exp, "auth_time": x.auth_time, "act": x.act,
                   "reused_id_jag": false, "month_to_date": month,
                   "unattended": via.is_some(), "via": via.map(|v| v.as_str())}),
        );
        match self.redeem(s, ras, &x.id_jag) {
            Ok(r) => {
                self.hold(
                    s,
                    grant,
                    entry,
                    &r,
                    Some(x.sub.clone()),
                    approval.map(|(_, exp)| exp),
                );
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

    /// The budget of `grant`'s template, once a mint for `c` at `s` is
    /// within it: the run's mints (`budget.per_run`), and the person's at
    /// the resource in the last minute (`budget.per_minute`, §5.13 item 7)
    /// — a steered loop over scope sets, or a server's string of refusals,
    /// spends minutes of mints, never the day's budget in seconds. Every
    /// mint asked for counts, whatever the IdP answers.
    fn paced(
        &self,
        grant: &Grant,
        s: &Server,
        c: &Call<'_>,
    ) -> Result<cfg::TemplateBudget, DelegationError> {
        let budget = self
            .templates
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| t.name == grant.template)
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
        let now = now_ms();
        let mut minted = self.minted.lock().unwrap_or_else(|e| e.into_inner());
        minted.retain(|_, q| q.back().is_some_and(|t| now.saturating_sub(*t) < 60_000));
        let q = minted
            .entry(pair(&grant.grant_id, &s.resource))
            .or_default();
        while q.front().is_some_and(|t| now.saturating_sub(*t) >= 60_000) {
            q.pop_front();
        }
        if q.len() >= budget.per_minute() as usize {
            return Err(refuse(
                Code::BudgetExhausted,
                format!(
                    "{} asked for {} mints at {} in the last minute (budget.per_minute {}): tried again once the minute has passed",
                    grant.principal.display(),
                    q.len(),
                    s.resource,
                    budget.per_minute()
                ),
            ));
        }
        q.push_back(now);
        Ok(budget)
    }

    /// The token a request to exchange server `s` presents (RFC 0044 §5.7,
    /// M1b): the pooled one while it may be used, else a new on-behalf-of
    /// exchange at the server's own authorization server — of an access
    /// token for agentd the grant's refresh token gets there (`subject:
    /// grant`), or, for a `via: ciba` firing, of its person's approval's ID
    /// token, which binds the result to that approval (§7.P3c); or of the
    /// caller's inbound token (`subject: inbound`), which is sent to its
    /// issuer and nowhere else. The result is held to the pinned audience
    /// and the person, and any refresh token in it is dropped: renewal is a
    /// new exchange.
    #[allow(clippy::too_many_arguments)]
    fn exchanged(
        &self,
        s: &Server,
        subject: cfg::ExchangeSubject,
        grant: &Grant,
        scopes: &[String],
        entry: &Entry,
        c: &Call<'_>,
        firing: Option<&unattended::Firing>,
    ) -> Result<(), DelegationError> {
        let now = now_ms();
        if subject == cfg::ExchangeSubject::Grant {
            let proved = self.with_live(&grant.grant_id, |l| l.proved_ms);
            if now.saturating_sub(proved) > self.liveness.as_millis() as u64 {
                self.probe(grant)?;
            }
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
        // A `via: ciba` firing exchanges only once its person approved it,
        // and its exchange binds to that approval: the subject is the
        // approval's ID token, never the grant's access token (§7.P3c).
        // Okta's on-behalf-of exchange takes an access token alone, so
        // there the call is refused before the person is asked for an
        // approval it could not use.
        let ciba = unattended::ciba_sid(firing);
        if ciba.is_some()
            && subject == cfg::ExchangeSubject::Grant
            && self
                .providers
                .get(&s.idp)
                .is_some_and(|i| i.cfg.profile == cfg::Profile::Okta)
        {
            return Err(refuse(
                Code::Config,
                format!(
                    "{} is exchanged by Okta's on-behalf-of exchange, which takes an access token alone as its subject, and a via: ciba firing exchanges its person's approval: {} serves no via: ciba firing (a via: standing one exchanges the grant's token)",
                    s.name, s.name
                ),
            ));
        }
        let approval = self.approval(ciba, grant)?;
        let via = firing.map(|f| f.via);
        let budget = self.paced(grant, s, c)?;
        let idp = self
            .providers
            .get(&s.idp)
            .ok_or_else(|| refuse(Code::Config, format!("no IdP {}", s.idp)))?;
        let meta = idp
            .meta(self.timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, format!("{}: {e}", s.idp)))?;
        let params = idp.params(&meta).map_err(|e| refuse(Code::Config, e))?;
        let subject_claim = match idp.cfg.profile {
            cfg::Profile::Okta => "uid",
            cfg::Profile::Generic => "sub",
        };
        let expect = idjag::ExpectAccess {
            subject_claim,
            sub: Some(grant.sub.as_str()),
        };
        let (redeemed, sub, month) = match subject {
            cfg::ExchangeSubject::Inbound => {
                let (token, exp) = c
                    .acting
                    .session
                    .and_then(|sid| self.book()?.inbound_token(sid))
                    .filter(|(_, exp)| now < *exp)
                    .ok_or_else(|| {
                        refuse(
                            Code::SubjectExpired,
                            format!(
                                "the caller's inbound token has expired, so {} has nothing to exchange: they call again with a fresh one",
                                s.name
                            ),
                        )
                    })?;
                let ex = idjag::AccessExchange {
                    subject_token: &token,
                    subject_token_type: idjag::wire::TOKEN_TYPE_ACCESS_TOKEN,
                    audience: &s.audience,
                    resource: &s.resource,
                    scopes,
                };
                match idjag::exchange_access(&params, &ex, &expect, self.timeout) {
                    Ok((r, sub)) => {
                        let mut r = r;
                        // Never past the token it was exchanged from.
                        let left = exp.saturating_sub(now) / 1000;
                        r.expires_in = Some(r.expires_in.map_or(left, |e| e.min(left)));
                        (r, sub, None)
                    }
                    Err(Refusal::Oauth { code, .. }) if code == idjag::wire::INVALID_GRANT => {
                        return Err(refuse(
                            Code::SubjectExpired,
                            format!(
                                "{} refused to exchange the caller's inbound token (invalid_grant): it is no longer good there",
                                s.idp
                            ),
                        ));
                    }
                    Err(r) => return Err(self.exchange_refusal(grant, s, r)),
                }
            }
            cfg::ExchangeSubject::Grant => {
                let lock = self.lock_of(&grant.grant_id);
                let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
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
                            "{} has made {} exchanges today and {} this month at {} (budget {}/day, {}/month)",
                            g.principal.display(),
                            q.day_count,
                            q.month_count,
                            s.resource,
                            budget.per_day(),
                            budget.per_month()
                        ),
                    ));
                }
                // The subject: a `via: ciba` firing's approval's ID token;
                // else an access token for agentd at this authorization
                // server, from the grant's refresh token with no `scope`:
                // the scopes the person granted stand, offline_access with
                // them.
                let (subject_token, subject_token_type) = match &approval {
                    Some((t, _)) => (t.clone(), idjag::wire::TOKEN_TYPE_ID_TOKEN),
                    None => {
                        let ahead = self.ahead(&g)?;
                        let t = match idjag::refresh(&params, &rt, self.timeout) {
                            Ok(t) => t,
                            Err(Refusal::Oauth { code, .. })
                                if code == idjag::wire::INVALID_GRANT =>
                            {
                                return Err(
                                    self.revoke_dead(&g, "the refresh token is no longer good")
                                );
                            }
                            Err(r) => {
                                self.keep_ahead(&g, ahead);
                                return Err(refuse(
                                    unavailable(&r),
                                    format!("{}: refresh: {r}", s.idp),
                                ));
                            }
                        };
                        match &t.refresh_token {
                            Some(new) => self.rotate(&mut g, &mut rt, new, ahead)?,
                            None => self.keep_ahead(&g, ahead),
                        }
                        self.with_live(&g.grant_id, |l| l.proved_ms = now_ms());
                        (t.access_token, idjag::wire::TOKEN_TYPE_ACCESS_TOKEN)
                    }
                };
                let ex = idjag::AccessExchange {
                    subject_token: &subject_token,
                    subject_token_type,
                    audience: &s.audience,
                    resource: &s.resource,
                    scopes,
                };
                let (r, sub) = match idjag::exchange_access(&params, &ex, &expect, self.timeout) {
                    Ok(x) => x,
                    Err(r) => {
                        drop(_held);
                        return Err(match approval {
                            Some(_) => self.approval_refusal(&g, s, r),
                            None => self.exchange_refusal(&g, s, r),
                        });
                    }
                };
                let mut q = q;
                q.day_count += 1;
                q.month_count += 1;
                g.quota.insert(s.resource.clone(), q.clone());
                if let Err(e) = self.vault.store(&g, &rt, Some(g.rt_version)) {
                    self.log.warn(
                        "delegation.quota.unsaved",
                        json!({"grant_id": g.grant_id, "err": e.to_string()}),
                    );
                }
                self.remember(&g);
                (r, sub, Some(q.month_count))
            }
        };
        *self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(c.unit.clone())
            .or_default() += 1;
        self.audit(
            "delegation.minted",
            "exchanged",
            Some(&grant.principal.display()),
            json!({"grant_id": grant.grant_id, "run": c.run, "step": c.step, "tool": c.tool,
                   "grant": "token_exchange",
                   "subject": match (subject, &approval) {
                       (cfg::ExchangeSubject::Grant, Some(_)) => "approval",
                       (cfg::ExchangeSubject::Grant, None) => "grant",
                       (cfg::ExchangeSubject::Inbound, _) => "inbound",
                   },
                   "as_iss": idp.cfg.issuer, "audience": s.audience, "resource": s.resource,
                   "scope_granted": redeemed.scope, "month_to_date": month,
                   "unattended": via.is_some(), "via": via.map(|v| v.as_str())}),
        );
        self.hold(
            s,
            grant,
            entry,
            &redeemed,
            sub.or_else(|| Some(grant.sub.clone())),
            approval.map(|(_, exp)| exp),
        );
        Ok(())
    }

    /// An exchange of a `via: ciba` firing's approval the authorization
    /// server refused: one that takes no ID token as the subject says so
    /// (`invalid_request`, RFC 8693 §2.2.2), which no later firing changes;
    /// one that no longer takes this approval ends the firing's use of it,
    /// for a firing asks once.
    fn approval_refusal(&self, g: &Grant, s: &Server, r: Refusal) -> DelegationError {
        match r {
            Refusal::Oauth { code, status, .. }
                if code == "invalid_request" || code == "unsupported_token_type" =>
            {
                refuse(
                    Code::Config,
                    format!(
                        "{} did not take the CIBA approval's ID token as the subject of the exchange for {} ({code}, HTTP {status}): a via: ciba firing's exchange presents its person's approval, so {} serves no via: ciba firing there",
                        s.idp, s.name, s.name
                    ),
                )
            }
            Refusal::Oauth { code, .. } if code == idjag::wire::INVALID_GRANT => refuse(
                Code::SubjectExpired,
                format!(
                    "{} no longer takes this firing's approval as the subject of the exchange for {} (invalid_grant), and a firing asks once",
                    s.idp, s.name
                ),
            ),
            r => self.exchange_refusal(g, s, r),
        }
    }

    /// An exchange the authorization server refused, by its code.
    fn exchange_refusal(&self, g: &Grant, s: &Server, r: Refusal) -> DelegationError {
        match r {
            Refusal::Oauth { code, .. } if code == idjag::wire::INVALID_GRANT => {
                self.after_invalid_grant(g, s)
            }
            Refusal::Oauth { code, max_age, .. }
                if code == idjag::wire::INSUFFICIENT_USER_AUTHENTICATION =>
            {
                refuse(
                    Code::StepUp,
                    format!(
                        "{} wants a sign-in within {}s before it exchanges for {}: the person signs in again, and the call is made again",
                        s.idp,
                        max_age.unwrap_or(0),
                        s.name
                    ),
                )
            }
            Refusal::Checked(k) => {
                self.fault(&g.grant_id, &s.name);
                refuse(
                    Code::Config,
                    format!("the token exchanged for {} failed a check: {k}", s.name),
                )
            }
            r => self.idp_refusal(g, s, r),
        }
    }

    /// The exchange's subject: a step-up's fresh ID token, once, for the
    /// call it was signed in for, while its `auth_time` meets the `max_age`
    /// asked — a sign-in through a step-up link authorizes the call it
    /// resumes and no other (A2A §7.6.4) — else the provider's.
    fn subject(
        &self,
        idp: &Idp,
        params: &OAuth2Params,
        g: &mut Grant,
        rt: &mut String,
        call: &CallOf,
        approval: Option<&str>,
    ) -> Result<(String, SubjectType), DelegationError> {
        // A `via: ciba` firing mints with its person's approval of it, and
        // nothing else (RFC 0044 §5.12 E).
        if let Some(t) = approval {
            return Ok((t.to_string(), SubjectType::IdToken));
        }
        let now_s = now_ms() / 1000;
        let fresh = self.with_live(&g.grant_id, |l| {
            let step_up = l.step_up;
            l.fresh_id_token.take_if(|f| f.serves(call, step_up, now_s))
        });
        if let Some(f) = fresh {
            return Ok((f.token, SubjectType::IdToken));
        }
        match idp.cfg.subject() {
            SubjectKind::RefreshToken => Ok((rt.clone(), SubjectType::RefreshToken)),
            SubjectKind::IdToken => {
                let id = self.fresh_id_token(idp, params, g, rt)?.ok_or_else(|| {
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

    /// A fresh ID token from a `refresh_token` grant with no `scope`, made
    /// under the grant's lock its caller holds: a rotated refresh token is
    /// kept before anything else, and the grant proved alive. `None` when
    /// the IdP returned no ID token.
    fn fresh_id_token(
        &self,
        idp: &Idp,
        params: &OAuth2Params,
        g: &mut Grant,
        rt: &mut String,
    ) -> Result<Option<String>, DelegationError> {
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
        Ok(t.id_token)
    }

    /// The approval a `via: ciba` firing mints with at `grant`, when
    /// `scope` is one: `Err` while it has none — `ciba_required` when it
    /// has not asked its person yet, a restart lost what it asked, or its
    /// request waits on their answer; else `subject_expired`, for a firing
    /// asks once, and once more after a restart.
    fn approval(
        &self,
        scope: Option<&str>,
        grant: &Grant,
    ) -> Result<Option<(String, u64)>, DelegationError> {
        let Some(sid) = scope else {
            return Ok(None);
        };
        let who = grant.principal.display();
        match self.firing_subject(sid, &grant.grant_id, now_ms() / 1000) {
            unattended::FiringSubject::Approved(t, exp) => Ok(Some((t, exp))),
            unattended::FiringSubject::Asked => Err(refuse(
                Code::SubjectExpired,
                format!(
                    "this firing's approval by {who} at {} has expired, was refused, or was lost to a restart after it had asked once more, and a firing asks its person once — once more after a restart",
                    grant.idp
                ),
            )),
            unattended::FiringSubject::Unasked => Err(refuse(
                Code::CibaRequired,
                format!(
                    "this firing has not asked {who} at {} yet: they are asked on their own device, and the call waits",
                    grant.idp
                ),
            )),
            unattended::FiringSubject::Asking => Err(refuse(
                Code::CibaRequired,
                format!(
                    "this firing's request to {who} at {} waits on their answer, and the call waits on it too",
                    grant.idp
                ),
            )),
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
        self.drop_hint(&g.grant_id);
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
            if let Some(gid) = self.grant_id_of(c.acting, c.server) {
                let mut v = Value::String(std::mem::take(&mut e.message));
                scrub(&mut v, &self.secrets_of(&gid, c.acting));
                e.message = v.as_str().unwrap_or_default().to_string();
            }
            self.denied(
                c.acting.principal,
                c.server,
                c.tool,
                e,
                c.run.clone(),
                c.step.clone(),
            );
        }
        result
    }

    /// The id of the grant `acting` would hold for `server`.
    fn grant_id_of(&self, acting: Acting<'_>, server: &str) -> Option<String> {
        let s = self.servers.get(server)?;
        let issuer = &self.providers.get(&s.idp)?.cfg.issuer;
        Some(self.who(acting).ok()?.typed.grant_id(issuer))
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
        let Admitted { grant, who, .. } = self.admit(c.acting, c.server)?;
        // The firing the call acts for is the one it was admitted as, to the
        // end: never looked up again by its session id, which a firing that
        // ends meanwhile no longer answers to.
        let firing = who.firing;
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
        let scope = unattended::ciba_sid(firing.as_ref());
        let (_, entry) = self.entry(c.server, &grant.grant_id, &scopes, scope);
        for _attempt in 0..2 {
            self.token(s, &grant, &scopes, &entry, c, firing.as_ref(), true)?;
            *entry.refused.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let client = match self.client(s, &entry, &grant) {
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
            // A firing that ended while its token was got acts for nobody.
            if let Some(f) = &firing
                && !self.firing_live(&f.sid)
            {
                return Err(refuse(
                    Code::Revoked,
                    format!(
                        "the firing this work acted for ({}) ended before the call was made, and its authority with it",
                        f.sid
                    ),
                ));
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
                    let secrets = self.secrets_of(&grant.grant_id, c.acting);
                    let mut v = serde_json::to_value(&r).unwrap_or(Value::Null);
                    scrub(&mut v, &secrets);
                    if let Ok(back) = serde_json::from_value(v) {
                        r = back;
                    }
                    self.audit(
                        "delegation.used",
                        if r.is_error() { "error" } else { "ok" },
                        Some(&grant.principal.display()),
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

    /// The principal id the listener names the holder of a grant for
    /// `typed` by: a pinned person's rule id, else the typed principal's
    /// display string.
    fn principal_of(&self, typed: &PrincipalRef) -> String {
        self.persons
            .iter()
            .find(|p| &p.typed == typed)
            .map_or_else(|| typed.display(), |p| p.principal.clone())
    }

    /// The connection of `entry`, on `grant`, made with the EMA extension
    /// declared and an elicitation handler that asks the grant's person.
    fn client(
        &self,
        s: &Server,
        entry: &Entry,
        grant: &Grant,
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
        let dpop = if self.dpop_bound(s) {
            Some(SignerDpop {
                key: self.dpop_key().map_err(|e| refuse(Code::Config, e))?,
                nonces: Arc::clone(&self.nonces),
                scheme: s
                    .endpoint
                    .split_once("://")
                    .map_or("https", |(scheme, _)| scheme)
                    .to_string(),
            })
        } else {
            None
        };
        let signer = Arc::new(Signer {
            token: Arc::clone(&entry.token),
            refused: Arc::clone(&entry.refused),
            dpop,
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
        .with_extension(idjag::wire::EMA_EXTENSION)
        .with_elicitation(Arc::new(ElicitPerson {
            notify: self
                .notify
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            principal: self.principal_of(&grant.principal),
            grant: grant.grant_id.clone(),
            server: s.name.clone(),
            timeout: s.timeout,
        }));
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

    /// List `server`'s tools with the delegated token of whoever `acting`
    /// is — one mint — and keep them: after an enrolment, for each
    /// templated server whose catalogue is empty.
    pub fn fill_catalogue(&self, server: &str, who: &Who) -> Result<usize, DelegationError> {
        if !self
            .listing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(server.to_string())
        {
            return Ok(0);
        }
        let out = self.list_with_grant(server, who);
        self.listing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(server);
        out
    }

    fn list_with_grant(&self, server: &str, who: &Who) -> Result<usize, DelegationError> {
        let Admitted {
            grant, who: as_now, ..
        } = self.admit_who(who.clone(), server)?;
        let firing = as_now.firing;
        let acting = Acting::new(Some(&who.id), who.session.as_deref());
        let s = self
            .servers
            .get(server)
            .ok_or_else(|| refuse(Code::Config, "no such server"))?;
        let ceiling = grant.ceilings.get(server).cloned().unwrap_or_default();
        let scopes = s
            .scope_set("", &ceiling)
            .map_err(|over| refuse(Code::ScopeBeyondCeiling, format!("{server} needs {over:?}")))?;
        self.confirm(s)?;
        let (_, entry) = self.entry(
            server,
            &grant.grant_id,
            &scopes,
            unattended::ciba_sid(firing.as_ref()),
        );
        let c = Call {
            server,
            tool: "tools/list",
            args: Value::Null,
            meta: json!({}),
            acting,
            unit: format!("catalogue:{server}"),
            run: None,
            step: None,
        };
        self.token(s, &grant, &scopes, &entry, &c, firing.as_ref(), true)?;
        let client = self.client(s, &entry, &grant)?;
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
        self.list_of(None)
    }

    /// The grants `principal` holds — every grant, for `None` — as
    /// [`Self::list`] shows them.
    pub fn list_of(&self, principal: Option<&str>) -> Vec<Value> {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|g| principal.is_none_or(|p| g.principal.display() == p))
            .map(|g| {
                let live = self.with_live(&g.grant_id, |l| (l.proved_ms, l.used_ms, l.step_up));
                json!({"grant_id": g.grant_id, "principal": g.principal.display(), "idp": g.idp,
                       "idp_iss": g.idp_iss, "sub": g.sub, "template": g.template,
                       "servers": g.ceilings, "created_ms": g.created_ms, "expires_ms": g.expires_ms,
                       "epoch": g.epoch, "rt_version": g.rt_version, "quota": g.quota,
                       "bound_by": g.bound_by, "unattended": g.unattended,
                       "ciba_today": g.ciba.as_ref().map(|q| q.at(now_ms()).day_count),
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
        let held: BTreeSet<String> = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|g| g.principal.display())
            .collect();
        for p in held {
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
        Ok(matching
            .iter()
            .map(|g| self.revoke_grant(g, by, None))
            .collect())
    }

    /// Revoke the grants `who` holds as they stand now (self-service, RFC
    /// 0044 §5.8): each of theirs that is theirs to use through the session
    /// they ask through — for a device name, one bound to the very identity
    /// that session proved — and no other. A device session under the same
    /// name that never signed in as that identity holds none, so revokes
    /// none.
    pub fn revoke_held(&self, who: &Who, by: &str) -> Result<Vec<Value>, String> {
        let held: Vec<Grant> = {
            let grants = self.grants.lock().unwrap_or_else(|e| e.into_inner());
            self.providers
                .iter()
                .filter_map(|(name, idp)| {
                    let g = grants.get(&who.typed.grant_id(&idp.cfg.issuer))?;
                    self.held_to(who, g, name).is_ok().then(|| g.clone())
                })
                .collect()
        };
        if held.is_empty() {
            return Err(format!(
                "{} holds no grant this session may use: a session revokes the grants it acts with, once it has signed in at the IdP as their holder",
                who.id
            ));
        }
        Ok(held
            .iter()
            .map(|g| self.revoke_grant(g, by, None))
            .collect())
    }

    /// Revoke grant `g`, `by`, for `why` when one is said: RFC 7009 at the
    /// IdP, retried until acknowledged or out of tries, then the tombstone
    /// — the epoch every later enrolment is one above — the record and the
    /// pool. What happened.
    pub fn revoke_grant(&self, g: &Grant, by: &str, why: Option<&str>) -> Value {
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
        self.drop_hint(&g.grant_id);
        self.audit(
            "delegation.revoked",
            "revoked",
            Some(by),
            json!({"grant_id": g.grant_id, "by": by, "why": why, "principal": g.principal.display(),
                   "sub": g.sub, "epoch": g.epoch, "at_idp": revoked_at_idp.is_ok(),
                   "at_idp_error": revoked_at_idp.as_ref().err(), "tombstoned": stored.is_ok()}),
        );
        json!({"grant_id": g.grant_id, "principal": g.principal.display(),
               "revoked_at_idp": revoked_at_idp.is_ok(),
               "error": revoked_at_idp.err().or(stored.err())})
    }

    /// Take a reload's templates, device pins and first binding (RFC 0044
    /// §5.14): narrowing a ceiling applies at the next mint, as every call
    /// is held to its template as it is now; what they no longer allow — a
    /// grant whose template is gone, or lost a server; a device name whose
    /// pin changed or went, or that first binding bound and first binding
    /// is off — is handed back with why, to be revoked off the loop, and
    /// the device names whose bindings that clears, whose sessions' proofs
    /// are dropped now.
    pub fn reload(&self, settings: &Settings) -> (Vec<(Grant, String)>, Vec<String>) {
        let dl = &settings.identity.delegation;
        let old_pins = self.pins.read().unwrap_or_else(|e| e.into_inner()).clone();
        *self.templates.write().unwrap_or_else(|e| e.into_inner()) = dl.templates.clone();
        *self.pins.write().unwrap_or_else(|e| e.into_inner()) = dl.device_pins.clone();
        let first_was = self
            .first_binding
            .swap(dl.device_first_binding, Ordering::SeqCst);
        let mut cleared: BTreeSet<String> = old_pins
            .iter()
            .filter(|(n, p)| dl.device_pins.get(*n) != Some(p))
            .map(|(n, _)| n.clone())
            .collect();
        let grants: Vec<Grant> = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut out = Vec::new();
        for g in grants {
            let device =
                (g.principal.source == Source::Device).then(|| g.principal.stable_id.clone());
            let why = match dl.templates.iter().find(|t| t.name == g.template) {
                None => Some(format!("its template {} was removed", g.template)),
                Some(t) => g
                    .ceilings
                    .keys()
                    .find(|srv| !t.servers.contains_key(*srv))
                    .map(|srv| format!("{srv} was removed from its template {}", g.template)),
            }
            .or_else(|| {
                let name = device.as_ref()?;
                match dl.device_pins.get(name) {
                    Some(pin) if pin.iss != g.idp_iss || pin.sub != g.sub => {
                        Some(format!("the pin of {name} names another identity now"))
                    }
                    None if g.bound_by.as_deref() == Some(BOUND_BY_PIN) => {
                        Some(format!("the pin that bound {name} was removed"))
                    }
                    None if g.bound_by.as_deref() == Some(BOUND_BY_FIRST)
                        && first_was
                        && !dl.device_first_binding =>
                    {
                        Some(format!(
                            "{name} was bound by its first enrolment, and device_first_binding is off"
                        ))
                    }
                    _ => None,
                }
            });
            if let Some(why) = why {
                if let Some(name) = device {
                    cleared.insert(name);
                }
                out.push((g, why));
            }
        }
        if let Some(book) = self.book() {
            for name in &cleared {
                book.drop_device_proofs(name);
            }
        }
        (out, cleared.into_iter().collect())
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

    /// The binding a verified sign-in as `claims` makes for `who` (RFC 0044
    /// §5.4 step 4): its `iss` + `sub` must be the per-person operator's
    /// pin, the `a2a.login` principal's own subject, or — for a device
    /// name — its pin, else the binding it already has; an unpinned,
    /// unbound name takes this one only under first binding. What bound it,
    /// for a device name.
    pub fn bind(
        &self,
        who: &Who,
        idp: &Idp,
        claims: &idjag::IdClaims,
    ) -> Result<Option<&'static str>, DelegationError> {
        let mismatch = |want: String| {
            refuse(
                Code::BindingMismatch,
                format!(
                    "the sign-in was {}#{}, and {} is {want}: nothing was kept",
                    claims.iss, claims.sub, who.id
                ),
            )
        };
        match &who.kind {
            WhoKind::Person { iss, sub, .. } => {
                if claims.iss != *iss || claims.sub != *sub {
                    return Err(mismatch(format!("pinned to {iss}#{sub}")));
                }
                Ok(None)
            }
            WhoKind::Oidc | WhoKind::Inbound { .. } => {
                if claims.iss != who.typed.issuer || claims.sub != who.typed.stable_id {
                    return Err(mismatch(format!(
                        "{}#{}",
                        who.typed.issuer, who.typed.stable_id
                    )));
                }
                Ok(None)
            }
            WhoKind::Device { name, .. } => {
                if let Some(pin) = self.pin_of(name) {
                    if claims.iss != pin.iss || claims.sub != pin.sub {
                        return Err(mismatch(format!("pinned to {}#{}", pin.iss, pin.sub)));
                    }
                    return Ok(Some(BOUND_BY_PIN));
                }
                if let Some(g) = self.bound(who, &idp.name) {
                    if claims.iss != g.idp_iss || claims.sub != g.sub {
                        return Err(mismatch(format!("bound to {}#{}", g.idp_iss, g.sub)));
                    }
                    return Ok(Some(match g.bound_by.as_deref() {
                        Some(BOUND_BY_PIN) => BOUND_BY_PIN,
                        _ => BOUND_BY_FIRST,
                    }));
                }
                if self.first_binding() {
                    return Ok(Some(BOUND_BY_FIRST));
                }
                Err(refuse(
                    Code::PrincipalNotDelegable,
                    format!(
                        "{} is unpinned and unbound, and identity.delegation.device_first_binding is off: an operator pins the name to a person first",
                        who.id
                    ),
                ))
            }
        }
    }

    /// Store a grant for `who` at `idp` after a verified sign-in that
    /// [`Self::bind`] bound.
    ///
    /// A sign-in for a grant that is still the same — the same identity,
    /// unexpired, under the template that admits them now with the
    /// ceilings it allows now — is a step-up or a renewal: the record keeps
    /// its metadata and takes the new refresh token one version up. Any
    /// other (expired, another `sub`, `iss` or uid, another template or
    /// ceilings) is replaced: the old epoch is tombstoned, its pooled tokens
    /// purged, and a new grant one epoch up is made from what is configured
    /// now. Either way the refresh token replaced is revoked at the IdP
    /// (RFC 7009), once the new one is kept.
    pub(crate) fn enrolled(
        &self,
        who: &Who,
        idp: &Idp,
        claims: &idjag::IdClaims,
        rt: &str,
        id_token: &str,
    ) -> Result<Grant, DelegationError> {
        let bound_by = self.bind(who, idp, claims)?;
        let template = self.template_for(who, &idp.name).ok_or_else(|| {
            refuse(
                Code::DelegationNotPermitted,
                format!(
                    "no identity.delegation.templates entry admits {} at {}",
                    who.id, idp.name
                ),
            )
        })?;
        let fingerprint = who.fingerprint(&claims.iss, &claims.sub);
        let gid = who.typed.grant_id(&idp.cfg.issuer);
        // Signing in is consenting to the `act_for` workflows that name the
        // person, as they are now (RFC 0044 §5.12 E).
        let consented = self.consented(who, idp, &template);
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
            g.fingerprint == fingerprint
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
                g.bound_by = bound_by.map(str::to_string);
                g.unattended = consented;
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
                        Some(&who.id),
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
                    principal: who.typed.clone(),
                    fingerprint,
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
                    bound_by: bound_by.map(str::to_string),
                    unattended: consented,
                    ciba: None,
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
        // The name the person signed in as, for a CIBA request whose IdP
        // returns no ID token to hint it with.
        self.keep_hint(&grant, claims.preferred_username.as_deref());
        let now_s = now / 1000;
        self.with_live(&gid, |l| {
            l.proved_ms = now;
            // A sign-in that meets the step-up asked is the next subject: as
            // recent as its `max_age`, its `auth_time` said.
            if let (Some(m), Some(t)) = (l.step_up, claims.auth_time)
                && auth_time_meets(Some(t), m, l.step_up_at, now_s).is_ok()
            {
                l.fresh_id_token = Some(FreshIdToken {
                    token: id_token.to_string(),
                    auth_time: t,
                    exp: claims.exp,
                    call: None,
                });
            }
        });
        Ok(grant)
    }

    /// Revoke a refresh token a sign-in handed agentd that it keeps no
    /// grant for — a login no template admits — at the IdP (RFC 7009).
    pub(crate) fn discard_refresh_token(&self, idp: &Idp, rt: &str) {
        let at_idp = self.revoke_at_idp(idp, rt);
        self.log.info(
            "delegation.discarded_token",
            json!({"idp": idp.name, "revoked_at_idp": at_idp.is_ok(), "at_idp_error": at_idp.err()}),
        );
    }

    /// The `max_age` a step-up of grant `grant` was asked for, while one is.
    pub fn step_up_asked(&self, grant: &str) -> Option<u64> {
        self.with_live(grant, |l| l.step_up)
    }

    /// When the IdP last asked grant `grant`'s person to sign in again, in
    /// seconds since the epoch: 0 in this life when it has not.
    pub fn step_up_at(&self, grant: &str) -> u64 {
        self.with_live(grant, |l| l.step_up_at)
    }

    /// Start the IdP's device grant at `idp`, for a step-up sign-in.
    pub fn start_device(&self, idp: &str) -> Result<oauth2::DeviceAuth, String> {
        let i = self
            .providers
            .get(idp)
            .ok_or_else(|| format!("no IdP {idp}"))?;
        let meta = i.meta(self.timeout)?;
        let params = i.params(&meta)?;
        oauth2::start_device(&params, self.timeout)
    }

    /// Poll step-up gate `gate`'s device grant at `idp` once: `None` once the
    /// person signed in — the sign-in verified, the grant's own, as recent
    /// as asked, and its fresh ID token the next mint's subject — else what
    /// the IdP said.
    #[cfg_attr(not(feature = "a2a"), allow(dead_code))]
    pub(crate) fn poll_step_up(
        &self,
        gate: &str,
        idp: &str,
        device_code: &str,
        (max_age, asked_s): (u64, u64),
    ) -> Result<Option<crate::runtime::stepup::Polled>, String> {
        use crate::runtime::stepup::Polled;
        let i = self
            .providers
            .get(idp)
            .ok_or_else(|| format!("no IdP {idp}"))?;
        let meta = i.meta(self.timeout)?;
        let params = i.params(&meta)?;
        match oauth2::poll_device_once(&params, device_code, self.timeout)? {
            oauth2::PollOutcome::Pending | oauth2::PollOutcome::Wait(_) => {
                Ok(Some(Polled::Pending))
            }
            oauth2::PollOutcome::SlowDown => Ok(Some(Polled::SlowDown)),
            oauth2::PollOutcome::Token(t) => {
                let signed = enrol::verify(self, idp, &t, None).map_err(|e| e.to_string())?;
                self.stepped_up_by(gate, signed, idp, Some(max_age), asked_s)
                    .map(|_| None)
            }
        }
    }

    /// [`Self::seal_personal`] off the loop: `principal`'s personal key is
    /// opened, or made, first — under an external signer that asks it.
    pub fn seal_personal_off_loop(
        &self,
        principal: &str,
        record: &str,
        content: &Value,
    ) -> Result<Value, String> {
        self.personal_key(principal, true)?;
        self.seal_personal(principal, record, content)
    }

    /// Clear a satisfied step-up once a mint used the fresh ID token.
    pub fn stepped_up(&self, grant: &str) -> bool {
        self.with_live(grant, |l| l.step_up.is_some())
    }

    /// Every grant used within the last `liveness_interval` whose last proof
    /// is older than it: probed off the loop, a dead one revoked; and the
    /// refresh tokens approvals left in the store before this start
    /// revoked, while a pass over them has not finished.
    pub fn tick(self: &Arc<Self>) {
        self.sweep_left_approvals();
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

#[cfg_attr(not(feature = "a2a"), allow(dead_code))]
impl Delegation {
    /// A step-up sign-in for gate `gate` at `idp` (RFC 0044 §5.10 L4): the
    /// grant the gate's call needs is held to the same `iss` + `sub`, and
    /// the sign-in must be as recent as the IdP asked — after `asked_s`,
    /// when the gate's record says the IdP asked, where it named no
    /// `max_age`, a restart included; its fresh ID token is then the next
    /// mint's subject. The gate's principal.
    pub(crate) fn stepped_up_by(
        &self,
        gate: &str,
        signed: enrol::SignedIn,
        idp: &str,
        max_age: Option<u64>,
        asked_s: u64,
    ) -> Result<PrincipalRef, String> {
        let (grant, call) = self
            .stepping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(gate)
            .cloned()
            .ok_or_else(|| format!("gate {gate} asks for no sign-in now"))?;
        let held = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&grant)
            .cloned()
            .ok_or_else(|| "the grant the call needs is gone".to_string())?;
        if signed.claims.iss != held.idp_iss || signed.claims.sub != held.sub {
            return Err(refuse(
                Code::BindingMismatch,
                format!(
                    "the sign-in was {}#{}, and the grant is {}#{}'s: nothing was kept",
                    signed.claims.iss, signed.claims.sub, held.idp_iss, held.sub
                ),
            )
            .to_string());
        }
        let now_s = now_ms() / 1000;
        let asked_s = asked_s.max(self.with_live(&grant, |l| l.step_up_at));
        if let Some(m) = max_age {
            auth_time_meets(signed.claims.auth_time, m, asked_s, now_s)
                .map_err(|e| format!("the sign-in at {idp}: {e}"))?;
        }
        let auth_time = signed.claims.auth_time.unwrap_or(now_s);
        // A refresh token the step-up sign-in handed back is the grant's next
        // one, kept one version up under the grant's lock; the one it
        // replaces is revoked at the IdP.
        if let Some(rt) = &signed.refresh_token {
            self.take_refresh_token(&grant, rt)
                .map_err(|e| e.to_string())?;
        }
        self.with_live(&grant, |l| {
            l.fresh_id_token = Some(FreshIdToken {
                token: signed.id_token.clone(),
                auth_time,
                exp: signed.claims.exp,
                call: Some(call),
            });
        });
        self.stepping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(gate);
        Ok(held.principal)
    }

    /// Keep `new`, a refresh token a sign-in for grant `grant_id` handed
    /// back, as its next one — compared and swapped one version up, under
    /// the grant's lock — and revoke the one it replaces at the IdP.
    fn take_refresh_token(&self, grant_id: &str, new: &str) -> Result<(), DelegationError> {
        let lock = self.lock_of(grant_id);
        let held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, mut rt) = self.load(grant_id)?;
        if rt == new {
            return Ok(());
        }
        let old = rt.clone();
        let ahead = self.ahead(&g)?;
        self.rotate(&mut g, &mut rt, new, ahead)?;
        drop(held);
        if let Some(idp) = self.providers.get(&g.idp) {
            let at_idp = self.revoke_at_idp(idp, &old);
            self.log.info(
                "delegation.replaced_token",
                serde_json::json!({"grant_id": grant_id, "revoked_at_idp": at_idp.is_ok(),
                                   "at_idp_error": at_idp.err()}),
            );
        }
        Ok(())
    }

    /// Gate `gate` asks for a step-up sign-in for grant `grant_id`, for
    /// `call`: the one call its fresh ID token is then the subject of.
    pub(crate) fn stepping_up(&self, gate: &str, grant_id: &str, call: CallOf) {
        self.stepping
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(gate.to_string(), (grant_id.to_string(), call));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step-up's fresh ID token is the subject of the call it was signed
    /// in for alone: another server's, another unit of work's or another
    /// step's mint does not take it, nor a mint once it is too old for the
    /// `max_age` asked; one from `--login` is the next mint's, whichever.
    #[test]
    fn a_fresh_id_token_serves_the_call_it_was_signed_in_for() {
        let call = |server: &str, unit: &str, step: Option<&str>| CallOf {
            server: server.into(),
            unit: unit.into(),
            step: step.map(str::to_string),
        };
        let gated = call("linear", "run-1", Some("write"));
        let fresh = |bound: Option<CallOf>| FreshIdToken {
            token: "id".into(),
            auth_time: 1_000,
            exp: 2_000,
            call: bound,
        };
        let f = fresh(Some(gated.clone()));
        assert!(f.serves(&gated, Some(60), 1_010));
        for other in [
            call("github", "run-1", Some("write")),
            call("linear", "ctx:c2", None),
            call("linear", "run-1", Some("read")),
        ] {
            assert!(!f.serves(&other, Some(60), 1_010), "{other:?}");
        }
        assert!(!f.serves(&gated, Some(60), 1_100), "older than max_age");
        assert!(!f.serves(&gated, None, 1_980), "about to expire");
        let login = fresh(None);
        assert!(login.serves(&call("github", "ctx:c2", None), None, 1_010));
    }

    fn server(mode: ScopeMode) -> Server {
        Server {
            name: "linear".into(),
            kind: ServerKind::IdJag,
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
            dpop: cfg::DpopMode::Auto,
            on_revoked: cfg::OnRevoked::Fail,
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

    const ISS: &str = "https://idp.example";

    /// The settings of one delegated server and `dl` as
    /// `identity.delegation`, its secrets in `dir`.
    fn people(dir: &std::path::Path, mut dl: Value) -> Settings {
        let seal = dir.join("seal");
        std::fs::write(&seal, "ab".repeat(32)).unwrap();
        let secret = format!("{{{{secret-file:{}}}}}", seal.display());
        dl["seal_key"] = json!(secret);
        Settings::from_document(
            json!({"agent": {"instruction": "x"},
                   "a2a": {"device_grant": {"enabled": true}},
                   "identity": {
                       "providers": {"okta": {"issuer": ISS, "client_id": "agentd",
                                              "subject": "refresh_token",
                                              "client_auth": {"method": "client_secret_basic"},
                                              "client_secret": secret}},
                       "delegation": dl},
                   "mcp": {"servers": [{"name": "linear", "endpoint": "https://mcp.linear.example/mcp",
                       "tags": {"*": ["sensitive"]},
                       "auth": {"kind": "oauth2", "grant": "id_jag", "idp": "okta",
                                "audience": "https://auth.linear.example",
                                "resource": "https://mcp.linear.example/mcp",
                                "client_id": "agentd-at-linear",
                                "client_auth": {"method": "client_secret_basic"},
                                "client_secret": secret}}]}}),
            "test",
        )
        .unwrap()
    }

    fn template(name: &str, source: &str, scopes: &[&str]) -> Value {
        json!({"name": name, "principals": {"source": source}, "idp": "okta",
               "servers": {"linear": {"scopes": scopes}}})
    }

    /// The delegation supervisor `settings` build, its session store `book`.
    fn supervisor(settings: &Settings, book: Arc<Book>) -> Arc<Delegation> {
        supervisor_on(
            settings,
            book,
            Arc::new(crate::store::memory::MemoryStore::new()),
        )
    }

    /// [`supervisor`] over `store`.
    fn supervisor_on(
        settings: &Settings,
        book: Arc<Book>,
        store: Arc<dyn crate::store::Store>,
    ) -> Arc<Delegation> {
        use crate::obs::log::{Comp, Level, LogCtx};
        let durable = Arc::new(Durable::new(
            store,
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
        let d = Delegation::build(settings, durable, None, &log)
            .unwrap()
            .expect("a delegated server is declared");
        d.sessions_to(book);
        d
    }

    /// A session store of fixed sessions.
    #[derive(Default)]
    struct Book(Mutex<BTreeMap<String, SessionInfo>>);

    impl Book {
        fn add(&self, sid: &str, typed: PrincipalRef, proof: Option<&str>) {
            let role = crate::config::settings::Role::User;
            self.0.lock().unwrap().insert(
                sid.to_string(),
                SessionInfo {
                    principal: typed.display(),
                    typed,
                    role,
                    inbound: None,
                    proof: proof.map(|sub| IdpProof {
                        idp: "okta".into(),
                        iss: ISS.into(),
                        sub: sub.into(),
                        at_ms: now_ms(),
                    }),
                },
            );
        }
    }

    impl SessionBook for Book {
        fn session(&self, sid: &str) -> Option<SessionInfo> {
            self.0.lock().unwrap().get(sid).cloned()
        }
        fn set_proof(&self, sid: &str, proof: IdpProof) -> bool {
            self.0
                .lock()
                .unwrap()
                .get_mut(sid)
                .map(|s| s.proof = Some(proof))
                .is_some()
        }
        fn drop_device_proofs(&self, name: &str) -> Vec<String> {
            let mut out = Vec::new();
            for (sid, s) in self.0.lock().unwrap().iter_mut() {
                if s.typed.source == Source::Device
                    && s.typed.stable_id == name
                    && s.proof.is_some()
                {
                    s.proof = None;
                    out.push(sid.clone());
                }
            }
            out
        }
    }

    fn device(name: &str) -> PrincipalRef {
        PrincipalRef::local(Source::Device, name)
    }

    fn oidc(sub: &str) -> PrincipalRef {
        PrincipalRef {
            issuer: ISS.into(),
            ..PrincipalRef::local(Source::Oidc, sub)
        }
    }

    /// Hold a grant for the person session `sid` is, as `sub`.
    fn hold(d: &Delegation, sid: &str, sub: &str, template: &str, bound_by: Option<&str>) -> Grant {
        let who = d.who(Acting::new(None, Some(sid))).unwrap();
        let g = Grant {
            grant_id: who.typed.grant_id(ISS),
            principal: who.typed.clone(),
            fingerprint: who.fingerprint(ISS, sub),
            idp: "okta".into(),
            idp_iss: ISS.into(),
            sub: sub.into(),
            email_hash: None,
            template: template.into(),
            ceilings: [(
                "linear".to_string(),
                vec!["issues:read".to_string(), "issues:write".to_string()],
            )]
            .into(),
            created_ms: now_ms(),
            expires_ms: now_ms() + 3_600_000,
            epoch: 1,
            rt_version: 1,
            quota: BTreeMap::new(),
            bound_by: bound_by.map(str::to_string),
            unattended: BTreeMap::new(),
            ciba: None,
        };
        d.grants
            .lock()
            .unwrap()
            .insert(g.grant_id.clone(), g.clone());
        g
    }

    fn claims(sub: &str) -> idjag::IdClaims {
        idjag::IdClaims {
            iss: ISS.into(),
            sub: sub.into(),
            exp: now_ms() / 1000 + 300,
            auth_time: None,
            email: None,
            preferred_username: None,
        }
    }

    fn code<T>(r: Result<T, DelegationError>) -> Option<Code> {
        r.err().map(|e| e.code)
    }

    /// A device name's grant is held through each session to the identity
    /// that session proved (RFC 0044 §5.2, §5.4): a second device under the
    /// same name with no proof, or another, is not enrolled; an ended
    /// session acts for nobody; a sign-in as someone else is
    /// binding_mismatch; first binding turned off, or a pin naming another,
    /// holds the grant back; and an unpinned, unbound name is not eligible
    /// without first binding.
    #[test]
    fn a_device_name_is_held_to_its_binding_through_each_session() {
        let dir = tempfile::tempdir().unwrap();
        let open = people(
            dir.path(),
            json!({"templates": [template("devices", "device", &["issues:read", "issues:write"])],
                   "device_first_binding": true}),
        );
        let book = Arc::new(Book::default());
        book.add("s1", device("laptop"), Some("00u1alice"));
        book.add("s2", device("laptop"), None);
        book.add("s3", device("laptop"), Some("00u2bob"));
        book.add("s4", device("phone"), None);
        let d = supervisor(&open, Arc::clone(&book));
        hold(&d, "s1", "00u1alice", "devices", Some(BOUND_BY_FIRST));
        let at = |sid: &str| d.admit(Acting::new(None, Some(sid)), "linear");
        assert!(at("s1").is_ok(), "the session that proved the binding");
        assert_eq!(code(at("s2")), Some(Code::NotEnrolled), "no proof");
        assert_eq!(code(at("s3")), Some(Code::NotEnrolled), "another identity");
        assert_eq!(code(at("gone")), Some(Code::Revoked), "an ended session");
        let who = |sid: &str| d.who(Acting::new(None, Some(sid))).unwrap();
        let idp = d.providers.get("okta").unwrap();
        assert_eq!(
            code(d.bind(&who("s3"), idp, &claims("00u2bob"))),
            Some(Code::BindingMismatch),
            "the name is bound to another"
        );
        assert_eq!(
            d.bind(&who("s2"), idp, &claims("00u1alice")).unwrap(),
            Some(BOUND_BY_FIRST)
        );
        assert!(d.eligible(&who("s4"), "okta").is_ok(), "first-bindable");
        // First binding off: what it bound is held back, and an unbound,
        // unpinned name is not eligible.
        let closed = people(
            dir.path(),
            json!({"templates": [template("devices", "device", &["issues:read", "issues:write"])]}),
        );
        d.reload(&closed);
        assert_eq!(code(at("s1")), Some(Code::NotEnrolled));
        // …whatever the session proves since: the binding is gone.
        book.add("s1", device("laptop"), Some("00u1alice"));
        assert_eq!(
            code(at("s1")),
            Some(Code::NotEnrolled),
            "first binding is off"
        );
        assert_eq!(
            code(d.eligible(&who("s4"), "okta")),
            Some(Code::PrincipalNotDelegable)
        );
        assert_eq!(
            code(d.bind(&who("s4"), idp, &claims("00u1alice"))),
            Some(Code::PrincipalNotDelegable)
        );
        // A pin binds only the identity it names.
        let pinned = people(
            dir.path(),
            json!({"templates": [template("devices", "device", &["issues:read"])],
                   "device_pins": {"phone": {"iss": ISS, "sub": "00u3carol"}}}),
        );
        d.reload(&pinned);
        assert_eq!(
            code(d.bind(&who("s4"), idp, &claims("00u1alice"))),
            Some(Code::BindingMismatch)
        );
        assert_eq!(
            d.bind(&who("s4"), idp, &claims("00u3carol")).unwrap(),
            Some(BOUND_BY_PIN)
        );
    }

    /// A certificate principal pinned by its key and its IdP identity is a
    /// person who delegates (RFC 0044 §5.2): named by the listener's id,
    /// held to the key its rule admits and the pin; work for them acts with
    /// their grant, and work for nobody with a grant does not; once their
    /// grant is revoked, every later call says `revoked` for as long as the
    /// register keeps the revocation (the user's decision of 2026-10-21).
    #[test]
    fn a_certificate_principal_delegates_and_a_revoked_grant_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let key = "ab".repeat(32);
        let mut settings = people(
            dir.path(),
            json!({"templates": [template("certs", "x509", &["issues:read", "issues:write"])]}),
        );
        settings.a2a.principals = serde_json::from_value(json!([
            {"match": {"spki": key}, "role": "user", "id": "alice",
             "idp_subject": {"iss": ISS, "sub": "00u1alice"}}
        ]))
        .unwrap();
        let book = Arc::new(Book::default());
        let d = supervisor(&settings, Arc::clone(&book));
        let alice = Acting::new(Some("user:alice"), None);
        let who = d.who(alice).unwrap();
        assert_eq!(who.id, format!("x509:{key}"));
        assert_eq!((who.source(), who.via_self()), ("x509", "certificate"));
        assert_eq!(
            code(d.admit(alice, "linear")),
            Some(Code::NotEnrolled),
            "no grant yet"
        );
        assert!(d.acting_templates(alice).is_none());
        let g = Grant {
            grant_id: who.typed.grant_id(ISS),
            principal: who.typed.clone(),
            fingerprint: who.fingerprint(ISS, "00u1alice"),
            idp: "okta".into(),
            idp_iss: ISS.into(),
            sub: "00u1alice".into(),
            email_hash: None,
            template: "certs".into(),
            ceilings: [("linear".to_string(), vec!["issues:read".to_string()])].into(),
            created_ms: now_ms(),
            expires_ms: now_ms() + 3_600_000,
            epoch: 1,
            rt_version: 1,
            quota: BTreeMap::new(),
            bound_by: None,
            unattended: BTreeMap::new(),
            ciba: None,
        };
        d.grants
            .lock()
            .unwrap()
            .insert(g.grant_id.clone(), g.clone());
        assert!(d.admit(alice, "linear").is_ok());
        assert_eq!(
            d.acting_templates(alice).map(|t| t[0].name.clone()),
            Some("certs".to_string())
        );
        assert!(d.who(Acting::new(Some("user:mallory"), None)).is_err());
        // The grant is held by its own principal alone: a device named
        // alice is `user:alice` to the listener too, signed in at the IdP
        // as the very same identity, and holds none of it — it answers none
        // of its questions and revokes nothing.
        let device = Who {
            id: "user:alice".into(),
            typed: PrincipalRef::local(Source::Device, "alice"),
            kind: WhoKind::Device {
                name: "alice".into(),
            },
            session: Some("ds_laptop".into()),
            proof: Some(crate::a2a::principals::typed::IdpProof {
                idp: "okta".into(),
                iss: ISS.into(),
                sub: "00u1alice".into(),
                at_ms: now_ms(),
            }),
            ..who.clone()
        };
        assert!(d.holds(&who, &g.grant_id).is_ok());
        assert!(d.holds(&device, &g.grant_id).is_err());
        assert!(d.holds(&who, "no-such-grant").is_err());
        assert!(d.revoke_held(&device, "user:alice").is_err());
        assert!(d.admit(alice, "linear").is_ok(), "untouched");
        // Revoked by its holder: said as one, and the work no longer acts
        // with a grant.
        let done = d.revoke_held(&who, "user:alice").unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(code(d.admit(alice, "linear")), Some(Code::Revoked));
        assert!(d.acting_templates(alice).is_none());
    }

    /// A step-up's sign-in is held to the `max_age` the IdP asked by its ID
    /// token's `auth_time` (RFC 0044 §5.10 L4): one without it shows
    /// nothing; one older than `max_age` is too old; with no `max_age` the
    /// person authenticated after the IdP asked — give or take the clocks.
    #[test]
    fn a_step_up_sign_in_is_as_recent_as_the_idp_asked() {
        assert!(
            auth_time_meets(None, 300, 1_000, 1_010).is_err(),
            "no auth_time"
        );
        assert!(auth_time_meets(Some(1_000), 300, 900, 1_200).is_ok());
        let old = auth_time_meets(Some(800), 300, 900, 1_200).unwrap_err();
        assert!(old.contains("400s ago"), "{old}");
        assert!(
            auth_time_meets(Some(950), 0, 1_000, 1_010).is_ok(),
            "within the skew"
        );
        let before = auth_time_meets(Some(500), 0, 1_000, 1_010).unwrap_err();
        assert!(before.contains("before the IdP asked"), "{before}");
    }

    /// A firing acts for its person only as their grant consents (RFC 0044
    /// §5.12 E): a sign-in consents to the `act_for` workflows naming them
    /// whose `via` the template allows, at their digests then; a firing of
    /// another digest is `unattended_suspended`, and of a `via` the template
    /// no longer allows `delegation_not_permitted`; a run a firing's work
    /// started, a firing of a workflow declaring no `act_for` for them, is
    /// `unattended_suspended` too; a turn it handed on by message acts for
    /// nobody. A firing's work acts as its person while the firing lives
    /// and for nobody once it ended; a `via: ciba` firing has no approval
    /// until it asks, its calls wait while its request does — one request
    /// waiting per person and workflow — and it asks once. A call admitted
    /// as a firing that ended meanwhile has no approval, never the grant's
    /// own subject.
    #[test]
    fn a_firing_acts_only_as_its_persons_consent_says() {
        use crate::config::settings::delegation::Via;
        use crate::engine::model::ActForPrincipal;
        let dir = tempfile::tempdir().unwrap();
        let with = |unattended: &str| {
            let mut t = template("people", "oidc", &["issues:read", "issues:write"]);
            t["unattended"] = json!(unattended);
            people(dir.path(), json!({"templates": [t]}))
        };
        let book = Arc::new(Book::default());
        book.add("a", oidc("00u1alice"), None);
        let d = supervisor(&with("ciba"), Arc::clone(&book));
        hold(&d, "a", "00u1alice", "people", None);
        let alice = ActForPrincipal {
            source: "oidc".into(),
            issuer: Some(ISS.into()),
            sub: Some("00u1alice".into()),
            id: None,
        };
        let decl = |digest: &str, via| unattended::ActForDecl {
            workflow: "nightly".into(),
            digest: digest.into(),
            via,
            principal: alice.clone(),
        };
        d.act_fors(vec![decl("d1", Via::Ciba), {
            let mut other = decl("d9", Via::Standing);
            other.workflow = "weekly".into();
            other
        }]);
        let who = d.who(Acting::new(None, Some("a"))).unwrap();
        let idp = d.providers.get("okta").unwrap();
        let t = d.template_for(&who, "okta").unwrap();
        let consented = d.consented(&who, idp, &t);
        assert_eq!(
            consented,
            [("nightly".to_string(), "d1".to_string())].into(),
            "standing is not ciba's to consent to"
        );
        assert_eq!(
            code(d.firing_ready(&decl("d1", Via::Ciba))),
            Some(Code::UnattendedSuspended),
            "nothing consented yet"
        );
        let gid = who.typed.grant_id(ISS);
        d.grants.lock().unwrap().get_mut(&gid).unwrap().unattended = consented;
        assert_eq!(
            d.firing_ready(&decl("d1", Via::Ciba)).unwrap(),
            who.typed.display()
        );
        assert_eq!(
            code(d.firing_ready(&decl("d2", Via::Ciba))),
            Some(Code::UnattendedSuspended),
            "a changed digest"
        );
        // The firing's work acts as the person, held to the same consent.
        let firing = |sid: &str, digest: &str| unattended::Firing {
            sid: sid.into(),
            run: format!("run-{sid}"),
            workflow: "nightly".into(),
            digest: digest.into(),
            via: Via::Ciba,
        };
        d.firing_started(firing("ua_1", "d1"), alice.clone(), &Default::default());
        d.firing_started(firing("ua_2", "d2"), alice.clone(), &Default::default());
        let at = |sid: &str| d.admit(Acting::new(None, Some(sid)), "linear");
        let ok = at("ua_1").unwrap();
        assert_eq!(ok.who.id, who.id);
        assert_eq!(
            ok.who.firing.as_ref().map(|f| f.run.as_str()),
            Some("run-ua_1")
        );
        assert_eq!(code(at("ua_2")), Some(Code::UnattendedSuspended));
        assert!(d.session_live("ua_1"));
        // A run its work started: a firing of a workflow with no act_for.
        d.firing_started(
            unattended::Firing {
                workflow: "body".into(),
                ..firing("ua_4", "b1")
            },
            alice.clone(),
            &Default::default(),
        );
        let e = at("ua_4").err().unwrap();
        assert_eq!(e.code, Code::UnattendedSuspended);
        assert!(e.message.contains("declares no act_for"), "{e}");
        assert_eq!(
            code(at(crate::engine::run::HANDED_ON_SID)),
            Some(Code::NoDelegation)
        );
        assert_eq!(
            crate::runtime::reactor::turn_session(Some("ua_1".into())).as_deref(),
            Some(crate::engine::run::HANDED_ON_SID),
            "a turn its work started by message"
        );
        assert_eq!(
            crate::runtime::reactor::turn_session(Some("s1".into())).as_deref(),
            Some("s1")
        );
        // No approval before asking; while the request waits the call waits
        // on it — and another firing of the workflow for her asks nothing
        // meanwhile; once the wait closed, none: a firing asks once.
        let g = d.grants.lock().unwrap().get(&gid).cloned().unwrap();
        assert_eq!(code(d.approval(Some("ua_1"), &g)), Some(Code::CibaRequired));
        d.ciba_asking("ua_1", &gid).unwrap();
        assert_eq!(code(d.approval(Some("ua_1"), &g)), Some(Code::CibaRequired));
        d.firing_started(firing("ua_3", "d1"), alice.clone(), &Default::default());
        assert_eq!(
            code(d.ciba_asking("ua_3", &gid)),
            Some(Code::BudgetExhausted)
        );
        assert_eq!(d.firings_asking(), vec![("ua_1".to_string(), gid.clone())]);
        d.not_asking("ua_1", &gid);
        assert_eq!(
            code(d.approval(Some("ua_1"), &g)),
            Some(Code::SubjectExpired)
        );
        d.ciba_asking("ua_3", &gid).unwrap();
        assert_eq!(
            d.ciba_scope(Acting::new(None, Some("ua_1"))).as_deref(),
            Some("ua_1")
        );
        // A call admitted as firing ua_3 keeps it: once the firing ended, it
        // is no firing's by its session id, and still has no approval.
        let admitted = at("ua_3").unwrap();
        d.firing_ended("ua_3");
        let kept = unattended::ciba_sid(admitted.who.firing.as_ref());
        assert_eq!(kept, Some("ua_3"));
        assert_eq!(d.ciba_scope(Acting::new(None, Some("ua_3"))), None);
        assert_eq!(code(d.approval(kept, &g)), Some(Code::SubjectExpired));
        // The template no longer allows it: refused as that.
        d.reload(&with("never"));
        assert_eq!(code(at("ua_1")), Some(Code::DelegationNotPermitted));
        // Ended, the firing acts for nobody.
        d.firing_ended("ua_1");
        assert_eq!(code(at("ua_1")), Some(Code::Revoked));
        assert!(!d.session_live("ua_1"));
    }

    /// A firing a restart found going asks its person once more at a grant
    /// its run's record says it asked once — its request, or the approval
    /// it gave, ended with the last life — and never after it asked once
    /// more or was refused; once it asks in this life, never again in it
    /// (RFC 0044 §7.P3c).
    #[test]
    fn a_restarted_firing_asks_once_more_and_no_more() {
        use crate::config::settings::delegation::Via;
        use crate::engine::model::ActForPrincipal;
        use crate::engine::run::Asked;
        let dir = tempfile::tempdir().unwrap();
        let mut t = template("people", "oidc", &["issues:read"]);
        t["unattended"] = json!("ciba");
        let book = Arc::new(Book::default());
        book.add("a", oidc("00u1alice"), None);
        let d = supervisor(&people(dir.path(), json!({"templates": [t]})), book);
        let g = hold(&d, "a", "00u1alice", "people", None);
        let alice = ActForPrincipal {
            source: "oidc".into(),
            issuer: Some(ISS.into()),
            sub: Some("00u1alice".into()),
            id: None,
        };
        let restored = |sid: &str, earlier: Option<Asked>| {
            d.firing_started(
                unattended::Firing {
                    sid: sid.into(),
                    run: format!("run-{sid}"),
                    workflow: "nightly".into(),
                    digest: "d1".into(),
                    via: Via::Ciba,
                },
                alice.clone(),
                &earlier
                    .map(|a| [(g.grant_id.clone(), a)].into())
                    .unwrap_or_default(),
            );
        };
        restored("ua_once", Some(Asked::Once));
        restored("ua_again", Some(Asked::Again));
        restored("ua_refused", Some(Asked::Refused));
        let approval = |sid: &str| code(d.approval(Some(sid), &g));
        assert_eq!(
            approval("ua_once"),
            Some(Code::CibaRequired),
            "a restart lost its request or approval: it asks once more"
        );
        assert!(!d.firing_asked("ua_once", &g.grant_id));
        assert_eq!(approval("ua_again"), Some(Code::SubjectExpired));
        assert_eq!(approval("ua_refused"), Some(Code::SubjectExpired));
        // Asked once more in this life, and closed unanswered: no more.
        d.ciba_asking("ua_once", &g.grant_id).unwrap();
        assert!(d.firing_asked("ua_once", &g.grant_id));
        d.not_asking("ua_once", &g.grant_id);
        assert_eq!(approval("ua_once"), Some(Code::SubjectExpired));
    }

    /// A store that fails its reads, or its listings, while told to.
    #[derive(Default)]
    struct Flaky {
        inner: crate::store::memory::MemoryStore,
        reads: AtomicBool,
        lists: AtomicBool,
    }

    impl crate::store::Store for Flaky {
        fn put(
            &self,
            key: &str,
            seq: u64,
            envelope: &Value,
        ) -> Result<crate::store::PutOutcome, crate::store::StoreError> {
            self.inner.put(key, seq, envelope)
        }
        fn get(
            &self,
            key: &str,
            seq: Option<u64>,
        ) -> Result<Option<Value>, crate::store::StoreError> {
            if self.reads.load(Ordering::Relaxed) {
                return Err(crate::store::StoreError::Io("down".into()));
            }
            self.inner.get(key, seq)
        }
        fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::store::KeySeq>, crate::store::StoreError> {
            if self.lists.load(Ordering::Relaxed) {
                return Err(crate::store::StoreError::Io("down".into()));
            }
            self.inner.list(prefix)
        }
        fn delete(&self, key: &str) -> Result<(), crate::store::StoreError> {
            self.inner.delete(key)
        }
        fn kind(&self) -> &'static str {
            "memory"
        }
    }

    /// An approval's refresh token the store cannot read or list now is
    /// kept for a later pass, and the start's pass is not done: only one
    /// that never opens is forgotten unrevoked; once the store answers,
    /// the pass forgets every one and is done (RFC 0044 §7.P3c).
    #[test]
    fn a_left_approval_the_store_cannot_read_now_waits_for_a_later_pass() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Flaky::default());
        let d = supervisor_on(
            &people(
                dir.path(),
                json!({"templates": [template("people", "oidc", &["issues:read"])]}),
            ),
            Arc::new(Book::default()),
            Arc::clone(&store) as Arc<dyn crate::store::Store>,
        );
        let a = vault::Approval {
            firing: "ua_1".into(),
            run: "run-1".into(),
            grant_id: "g1".into(),
            idp: "gone".into(),
        };
        d.vault.keep_approval(&a, "ciba-rt-1").unwrap();
        let kept = || {
            store.lists.store(false, Ordering::Relaxed);
            d.vault.approvals().unwrap()
        };
        store.lists.store(true, Ordering::Relaxed);
        assert!(d.revoke_left_approvals().is_err(), "unlisted, not done");
        assert_eq!(kept(), vec![a.name()]);
        store.reads.store(true, Ordering::Relaxed);
        let e = d.revoke_left_approvals().unwrap_err();
        assert_eq!(e.code, Code::IdpUnavailable, "{}", e.message);
        assert_eq!(kept(), vec![a.name()], "unread, kept for a later pass");
        store.reads.store(false, Ordering::Relaxed);
        // One that never opens is forgotten with the rest.
        d.durable
            .put(
                crate::state::Kind::Cred,
                "delegation/approval/ua_2.g1",
                json!({"not": "an approval"}),
                None,
            )
            .unwrap();
        d.revoke_left_approvals().unwrap();
        assert!(kept().is_empty());
        d.vault.keep_approval(&a, "ciba-rt-2").unwrap();
        d.revoke_left_approvals().unwrap();
        assert_eq!(
            kept(),
            vec![a.name()],
            "done once: this life's are its runs'"
        );
    }

    /// A step-up sign-in is held to when the IdP asked as its gate's record
    /// says (RFC 0044 §5.10 L4): where the IdP named no `max_age`, one of an
    /// authentication from before the ask is refused although this life
    /// never saw the ask — a device grant a restart resumed — and one after
    /// it is the call's.
    #[test]
    fn a_step_up_is_held_to_the_ask_its_gate_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let book = Arc::new(Book::default());
        book.add("a", oidc("00u1alice"), None);
        let d = supervisor(
            &people(
                dir.path(),
                json!({"templates": [template("people", "oidc", &["issues:read"])]}),
            ),
            Arc::clone(&book),
        );
        let g = hold(&d, "a", "00u1alice", "people", None);
        assert_eq!(d.step_up_at(&g.grant_id), 0, "a new life saw no ask");
        let now = now_ms() / 1000;
        let signed = |auth_time: u64| enrol::SignedIn {
            claims: idjag::IdClaims {
                auth_time: Some(auth_time),
                ..claims("00u1alice")
            },
            id_token: "it".into(),
            refresh_token: None,
        };
        d.stepping_up(
            "g1",
            &g.grant_id,
            CallOf {
                server: "linear".into(),
                unit: "r".into(),
                step: Some("s".into()),
            },
        );
        let old = d
            .stepped_up_by("g1", signed(now - 3_600), "okta", Some(0), now - 60)
            .unwrap_err();
        assert!(old.contains("before the IdP asked"), "{old}");
        assert!(
            d.stepped_up_by("g1", signed(now), "okta", Some(0), now - 60)
                .is_ok()
        );
    }

    /// A reload takes the templates, pins and first binding at once: a
    /// narrowed template narrows the grant at the next call and revokes
    /// nothing; a removed template, a pin naming another, and first binding
    /// turned off each hand back the grants they allowed, with why, and drop
    /// the proofs of those device names' sessions.
    #[test]
    fn a_reload_revokes_only_what_it_no_longer_allows() {
        let dir = tempfile::tempdir().unwrap();
        let both = |people_scopes: &[&str], pins: Value, first: bool| {
            json!({"templates": [template("people", "oidc", people_scopes),
                                 template("devices", "device", &["issues:read", "issues:write"])],
                   "device_pins": pins, "device_first_binding": first})
        };
        let start = people(
            dir.path(),
            both(
                &["issues:read", "issues:write"],
                json!({"phone": {"iss": ISS, "sub": "00u3carol"}}),
                true,
            ),
        );
        let book = Arc::new(Book::default());
        book.add("a", oidc("00u1alice"), None);
        book.add("l", device("laptop"), Some("00u1alice"));
        book.add("p", device("phone"), Some("00u3carol"));
        let d = supervisor(&start, Arc::clone(&book));
        hold(&d, "a", "00u1alice", "people", None);
        hold(&d, "l", "00u1alice", "devices", Some(BOUND_BY_FIRST));
        hold(&d, "p", "00u3carol", "devices", Some(BOUND_BY_PIN));
        // Narrowed: nothing revoked, the next call held to it.
        let (revoked, cleared) = d.reload(&people(
            dir.path(),
            both(
                &["issues:read"],
                json!({"phone": {"iss": ISS, "sub": "00u3carol"}}),
                true,
            ),
        ));
        assert!(
            revoked.is_empty() && cleared.is_empty(),
            "{revoked:?} {cleared:?}"
        );
        let a = d.admit(Acting::new(None, Some("a")), "linear").unwrap();
        assert_eq!(a.grant.ceilings["linear"], vec!["issues:read".to_string()]);
        // The pin moved and first binding off: both device grants go, and
        // their names' proofs with them; the person's stays.
        let (revoked, cleared) = d.reload(&people(
            dir.path(),
            both(
                &["issues:read"],
                json!({"phone": {"iss": ISS, "sub": "00u9dave"}}),
                false,
            ),
        ));
        let why: BTreeMap<String, String> = revoked
            .iter()
            .map(|(g, w)| (g.principal.stable_id.clone(), w.clone()))
            .collect();
        assert_eq!(why.len(), 2, "{why:?}");
        assert!(
            why["laptop"].contains("device_first_binding is off"),
            "{why:?}"
        );
        assert!(why["phone"].contains("names another identity"), "{why:?}");
        assert_eq!(cleared, vec!["laptop".to_string(), "phone".to_string()]);
        assert!(
            book.session("l").unwrap().proof.is_none()
                && book.session("p").unwrap().proof.is_none()
        );
        // Revoked off the loop, as the runtime does.
        for (g, _) in &revoked {
            d.grants.lock().unwrap().remove(&g.grant_id);
        }
        // The template removed: the person's grant goes, said why.
        let (revoked, _) = d.reload(&people(
            dir.path(),
            json!({"templates": [template("devices", "device", &["issues:read"])]}),
        ));
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].0.principal, oidc("00u1alice"));
        assert!(
            revoked[0].1.contains("template people was removed"),
            "{revoked:?}"
        );
    }
}
