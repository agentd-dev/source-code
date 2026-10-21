// SPDX-License-Identifier: AGPL-3.0-only
//! Acting for a person with nobody there (RFC 0044 M4, §5.12 E): a firing
//! of an `act_for` workflow — its `schedule`, `loop` or `once` start — acts
//! for the person the workflow names, with their grant, as their template
//! allows: `via: ciba`, each firing approved by them on their own device,
//! or `via: standing`, with their refresh token and nobody asked.
//!
//! **Consent is to content.** A person's sign-in — an enrolment, or signing
//! in again — keeps in their grant the definition digest of every
//! `act_for` workflow that names them and whose `via` their template
//! allows ([`Grant::unattended`]). A firing acts only while its workflow's
//! digest is the one they consented to: a reload that changes it — an
//! edit, or a `url:` or `dir:` body that changed — suspends its firings
//! (`unattended_suspended`) until they sign in again. A suspended firing
//! starts no run and asks nobody.
//!
//! **A firing is an authority of its own** ([`Firing`]). Its run acts
//! through a session id of the firing's
//! ([`crate::engine::run::UNATTENDED_SID_PREFIX`]), which every turn and
//! child of its work acts through too; it is registered here while the run
//! lives and gone when the run ends, so work that outlives it acts for
//! nobody. The authority is its run's alone: a run its work starts — a
//! `workflow` step, `workflow.run`, a signal or a message that starts one
//! — is a firing of its own, for the same person, held to their consent to
//! that run's own definition, which a workflow declaring no `act_for` for
//! them never has; and a conversation turn its work starts by message acts
//! for nobody's grant ([`crate::engine::run::HANDED_ON_SID`]). Its work is unattended — tainted, so the person's consent
//! rules ask them and, with nobody there, deny when they time out — and a
//! delegated tool tagged `egress` is refused unless the template lists it
//! in `unattended_tools`. No step-up is asked for: nobody is there to sign
//! in.
//!
//! **`via: ciba`** asks the person at the first delegated call of a firing
//! at each grant, by OpenID CIBA in poll mode ([`crate::auth::ciba`]): one
//! request per firing and grant, and once more after a restart that lost
//! it while it waited or once approved — never a third, and never after a
//! denial — at most one waiting per person and workflow, and at most
//! `ciba.per_day` a day per person, counted in their grant. The request's
//! hint is a fresh ID token from a `refresh_token` grant under the grant's
//! lock, discarded once sent; an IdP that answers none is sent the
//! `preferred_username` the person enrolled with, kept sealed under their
//! personal key (§6.4). The approval's ID token must name the grant's own
//! `iss` and `sub` — a hint can resolve to another account — or the firing
//! fails closed. It is then the subject of the firing's mints and
//! exchanges alone, in memory, until it expires (`subject_expired`); the
//! tokens minted with it are pooled for the firing alone; and a refresh
//! token the IdP issued with it anyway is kept sealed in the store while
//! the run lasts and revoked (RFC 7009) when it ends — or, after a crash,
//! at the next start's first pass, before any request is sent (§7.P3c).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::json;

use super::vault::{Approval, Unopened};
use super::{Acting, Code, Delegation, DelegationError, Idp, Who, WhoKind, refuse};
use crate::a2a::principals::typed::{PrincipalRef, Source};
use crate::auth::ciba::{self, Hint, Polled};
use crate::auth::idjag::Refusal;
use crate::config::settings::Role;
use crate::config::settings::delegation::{Profile, Template, Via};
use crate::engine::model::ActForPrincipal;
use crate::engine::run::{Asked, UNATTENDED_SID_PREFIX};
use crate::state::now_ms;

/// Where a grant's sealed login hint is kept, by `grant_id`.
const HINTS: &str = "delegation/hint/";

/// How long after a start's pass over the approvals' refresh tokens left
/// in the store could not finish the next is due, at first and at most.
const SWEEP_RETRY_MS: (u64, u64) = (5_000, 300_000);

/// When a reactor pass next runs the start's pass over the refresh tokens
/// approvals left in the store ([`Delegation::revoke_left_approvals`]).
#[derive(Default)]
pub(super) struct SweepDue {
    /// Due once now is past it: 0 at a start, `u64::MAX` while one runs or
    /// once one finished.
    at_ms: u64,
    /// How long after the last that could not finish.
    backoff_ms: u64,
}

/// An `act_for` workflow as the runtime installed it: what its firings act
/// with, and what a person's sign-in consents to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActForDecl {
    pub workflow: String,
    /// Its definition digest.
    pub digest: String,
    pub via: Via,
    pub principal: ActForPrincipal,
}

/// A firing, as the work it starts acts through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    /// The session id its work acts through.
    pub sid: String,
    pub run: String,
    pub workflow: String,
    /// The definition digest it runs.
    pub digest: String,
    pub via: Via,
}

/// A firing acting now: whom it is for, the grants it has asked its person
/// at in this life, how its run's record says it asked before this life
/// began, the grants it is asking at, and what their approvals gave it.
pub(super) struct Running {
    pub(super) firing: Firing,
    principal: ActForPrincipal,
    asked: BTreeSet<String>,
    earlier: BTreeMap<String, Asked>,
    asking: BTreeSet<String>,
    subjects: BTreeMap<String, Subject>,
}

/// What a CIBA approval gave a firing at one grant: the ID token in memory
/// alone, and a refresh token the IdP issued with it also kept sealed in
/// the store, as `kept`, until the run ends.
struct Subject {
    id_token: String,
    /// Seconds since the epoch.
    exp: u64,
    refresh_token: Option<String>,
    kept: Option<Approval>,
}

/// The subject a firing mints with at a grant.
pub(super) enum FiringSubject {
    /// Its approval's ID token.
    Approved(String, u64),
    /// Its request waits on the person's answer.
    Asking,
    /// It asked, and holds no approval: denied or expired in this life, or
    /// lost to a restart after it had asked once more.
    Asked,
    /// It has not asked yet — or a restart lost its request, or the
    /// approval it gave, and it asks once more.
    Unasked,
}

/// The session id of the `via: ciba` firing `firing` is, when it is one:
/// what its tokens are minted with and pooled for alone.
pub(super) fn ciba_sid(firing: Option<&Firing>) -> Option<&str> {
    firing
        .filter(|f| f.via == Via::Ciba)
        .map(|f| f.sid.as_str())
}

/// How a poll of a firing's CIBA request ended, for the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CibaOutcome {
    Pending,
    SlowDown,
    Approved,
    Refused { code: Code, why: String },
}

impl Delegation {
    /// Take the `act_for` workflows a load or a reload installed.
    pub fn act_fors(&self, decls: Vec<ActForDecl>) {
        *self.acts.write().unwrap_or_else(|e| e.into_inner()) = decls;
    }

    /// The person `p` names, as the configuration names them now, with no
    /// session.
    pub(crate) fn who_of(&self, p: &ActForPrincipal) -> Option<Who> {
        match p.source.as_str() {
            "operator" | "x509" => {
                let operator = p.source == "operator";
                let person = self.persons.iter().find(|x| {
                    Some(x.rule.as_str()) == p.id.as_deref()
                        && (x.typed.source == Source::Operator) == operator
                })?;
                Some(Who {
                    id: person.id.clone(),
                    typed: person.typed.clone(),
                    role: person.role,
                    labels: person.labels.clone(),
                    kind: WhoKind::Person {
                        fingerprint: person.fingerprint.clone(),
                        iss: person.iss.clone(),
                        sub: person.sub.clone(),
                    },
                    session: None,
                    proof: None,
                    firing: None,
                })
            }
            "oidc" => {
                let typed = PrincipalRef {
                    issuer: p.issuer.clone()?,
                    ..PrincipalRef::local(Source::Oidc, p.sub.as_deref()?)
                };
                Some(Who {
                    id: typed.display(),
                    typed,
                    role: Role::User,
                    labels: BTreeMap::new(),
                    kind: WhoKind::Oidc,
                    session: None,
                    proof: None,
                    firing: None,
                })
            }
            _ => None,
        }
    }

    /// The principal id a firing for `p` runs as: the listener's name for
    /// a pinned person, the display string of an IdP's subject.
    pub fn firing_principal(&self, p: &ActForPrincipal) -> Option<String> {
        let who = self.who_of(p)?;
        Some(self.principal_of(&who.typed))
    }

    /// The IdP `p` is a subject of: a pinned person's by their pin, an
    /// IdP subject's by its issuer.
    fn idp_of(&self, p: &ActForPrincipal) -> Option<&Idp> {
        let iss = match &self.who_of(p)?.kind {
            WhoKind::Person { iss, .. } => iss.clone(),
            _ => p.issuer.clone()?,
        };
        self.providers.values().find(|i| i.cfg.issuer == iss)
    }

    /// What `who`'s sign-in at `idp` under `template` consents to: every
    /// installed `act_for` workflow naming them at that IdP whose `via` the
    /// template allows, by name, at the digest it has now.
    pub(super) fn consented(
        &self,
        who: &Who,
        idp: &Idp,
        template: &Template,
    ) -> BTreeMap<String, String> {
        let gid = who.typed.grant_id(&idp.cfg.issuer);
        self.acts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|a| template.allows_unattended(a.via))
            .filter(|a| {
                self.idp_of(&a.principal)
                    .is_some_and(|i| i.name == idp.name)
            })
            .filter(|a| {
                self.who_of(&a.principal)
                    .is_some_and(|w| w.typed.grant_id(&idp.cfg.issuer) == gid)
            })
            .map(|a| (a.workflow.clone(), a.digest.clone()))
            .collect()
    }

    /// Whether a firing of `decl` may act for its person now, with no
    /// network: they hold an unexpired grant at the IdP they are a subject
    /// of, held to them as the configuration pins them, under a template
    /// that admits them, allows `via`, and keeps their consent to this
    /// very definition. The principal id its run runs as, or why not, as a
    /// delegated call would be refused.
    pub fn firing_ready(&self, decl: &ActForDecl) -> Result<String, DelegationError> {
        let who = self.who_of(&decl.principal).ok_or_else(|| {
            refuse(
                Code::NoDelegation,
                format!(
                    "workflow {}'s act_for names nobody the configuration pins now",
                    decl.workflow
                ),
            )
        })?;
        let idp = self.idp_of(&decl.principal).ok_or_else(|| {
            refuse(
                Code::Config,
                format!("{} is a subject of no declared IdP", who.id),
            )
        })?;
        let gid = who.typed.grant_id(&idp.cfg.issuer);
        let held = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&gid)
            .cloned();
        let Some(grant) = held else {
            let revoked = self
                .vault
                .floors()
                .get(&gid)
                .is_some_and(|f| f.revoked_epoch.is_some());
            return Err(refuse(
                if revoked {
                    Code::Revoked
                } else {
                    Code::NotEnrolled
                },
                format!(
                    "{} holds no grant at {}: their firings act for them once they enrol",
                    who.id, idp.name
                ),
            ));
        };
        self.held_to(&who, &grant, &idp.name)?;
        if now_ms() >= grant.expires_ms {
            return Err(refuse(
                Code::NeedsReenroll,
                format!("{}'s grant at {} has expired", who.id, idp.name),
            ));
        }
        let template = self
            .templates
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| t.name == grant.template && super::admits(t, &who, &idp.name))
            .cloned()
            .ok_or_else(|| {
                refuse(
                    Code::DelegationNotPermitted,
                    format!(
                        "{}'s grant was allowed by template {}, which no longer admits them",
                        who.id, grant.template
                    ),
                )
            })?;
        let firing = Firing {
            sid: String::new(),
            run: String::new(),
            workflow: decl.workflow.clone(),
            digest: decl.digest.clone(),
            via: decl.via,
        };
        self.firing_allowed(&who.id, &firing, &grant, &template)?;
        Ok(self.principal_of(&who.typed))
    }

    /// Whether `firing` may act with `grant` under `template`: the template
    /// allows its `via`, and the grant keeps the person's consent to the
    /// definition it runs.
    pub(super) fn firing_allowed(
        &self,
        who: &str,
        firing: &Firing,
        grant: &super::vault::Grant,
        template: &Template,
    ) -> Result<(), DelegationError> {
        if !template.allows_unattended(firing.via) {
            return Err(refuse(
                Code::DelegationNotPermitted,
                format!(
                    "{who}'s template {} says unattended: {}, which allows no via: {} firing of {}",
                    template.name,
                    template.unattended.as_str(),
                    firing.via.as_str(),
                    firing.workflow
                ),
            ));
        }
        match grant.unattended.get(&firing.workflow) {
            Some(d) if *d == firing.digest => Ok(()),
            Some(d) => Err(refuse(
                Code::UnattendedSuspended,
                format!(
                    "workflow {} is no longer the definition {who} consented to run for them (digest {}, consented {}): its firings act for them again once they sign in again",
                    firing.workflow,
                    &firing.digest[..firing.digest.len().min(12)],
                    &d[..d.len().min(12)]
                ),
            )),
            None => {
                // A run a firing's work started is a firing of its own
                // workflow, which may declare no act_for for them at all.
                let declared = self
                    .acts
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|a| a.workflow == firing.workflow);
                Err(refuse(
                    Code::UnattendedSuspended,
                    if declared {
                        format!(
                            "{who} has not consented to workflow {} running for them: its firings act for them once they sign in again",
                            firing.workflow
                        )
                    } else {
                        format!(
                            "workflow {} declares no act_for {who} consented to, and a run a firing's work starts acts for its person only as their consent to that run's own workflow says",
                            firing.workflow
                        )
                    },
                ))
            }
        }
    }

    /// A firing started — or, after a restart, resumes — acting through
    /// `firing.sid` for `principal`, having asked as its run's record says
    /// (`earlier`): what any request or approval gave it ended with the
    /// last life.
    pub fn firing_started(
        &self,
        firing: Firing,
        principal: ActForPrincipal,
        earlier: &BTreeMap<String, Asked>,
    ) {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                firing.sid.clone(),
                Running {
                    firing,
                    principal,
                    asked: BTreeSet::new(),
                    earlier: earlier.clone(),
                    asking: BTreeSet::new(),
                    subjects: BTreeMap::new(),
                },
            );
    }

    /// Whether firing `sid` has asked its person at `grant` in this life.
    pub fn firing_asked(&self, sid: &str, grant: &str) -> bool {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .is_some_and(|r| r.asked.contains(grant))
    }

    /// Whether firing `sid` is acting now.
    pub fn firing_live(&self, sid: &str) -> bool {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(sid)
    }

    /// The firing acting through `sid`, and whom it acts for, while it does.
    pub fn firing_at(&self, sid: &str) -> Option<(Firing, ActForPrincipal)> {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .map(|r| (r.firing.clone(), r.principal.clone()))
    }

    /// Who work acting through firing `sid` acts for: its person as they
    /// stand now, the firing named; `revoked` once its run has ended; and
    /// nobody for a turn a firing's work handed on by message.
    pub(super) fn firing_who(&self, sid: &str) -> Result<Who, DelegationError> {
        if sid == crate::engine::run::HANDED_ON_SID {
            return Err(refuse(
                Code::NoDelegation,
                "a conversation turn a firing's work started by message acts for nobody's grant: what it runs is no definition the person consented to",
            ));
        }
        let (firing, p) = {
            let f = self.firings.lock().unwrap_or_else(|e| e.into_inner());
            match f.get(sid) {
                Some(r) => (r.firing.clone(), r.principal.clone()),
                None => {
                    return Err(refuse(
                        Code::Revoked,
                        format!(
                            "the firing this work acted for ({sid}) has ended, and its authority with it"
                        ),
                    ));
                }
            }
        };
        let mut who = self.who_of(&p).ok_or_else(|| {
            refuse(
                Code::NoDelegation,
                format!(
                    "workflow {}'s act_for names nobody the configuration pins now",
                    firing.workflow
                ),
            )
        })?;
        who.firing = Some(firing);
        Ok(who)
    }

    /// The firing `acting` is, when it acts through one whose tokens are
    /// its own: a `via: ciba` firing's are minted with its approval, and
    /// pooled for it alone.
    pub(super) fn ciba_scope(&self, acting: Acting<'_>) -> Option<String> {
        let sid = acting
            .session
            .filter(|s| s.starts_with(UNATTENDED_SID_PREFIX))?;
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .filter(|r| r.firing.via == Via::Ciba)
            .map(|_| sid.to_string())
    }

    /// Firing `sid` asks its person at grant `grant` now — never again in
    /// this life — and waits on their answer: unless another firing of its
    /// workflow for the same person is waiting on theirs (one request waits
    /// at a time per person and workflow), decided with the mark made, so
    /// two firings asking at once never both wait or both refuse.
    pub fn ciba_asking(&self, sid: &str, grant: &str) -> Result<(), DelegationError> {
        let mut f = self.firings.lock().unwrap_or_else(|e| e.into_inner());
        let Some(me) = f.get(sid) else {
            return Err(refuse(Code::Revoked, "the firing has ended"));
        };
        let (workflow, principal) = (me.firing.workflow.clone(), me.principal.clone());
        if f.iter().any(|(other, r)| {
            other != sid
                && r.firing.workflow == workflow
                && r.principal == principal
                && !r.asking.is_empty()
        }) {
            return Err(refuse(
                Code::BudgetExhausted,
                format!(
                    "another firing of {workflow} is waiting on its person's approval: one request waits at a time per person and workflow"
                ),
            ));
        }
        if let Some(me) = f.get_mut(sid) {
            me.asked.insert(grant.to_string());
            me.asking.insert(grant.to_string());
        }
        Ok(())
    }

    /// Every (firing, grant) waiting on its person's answer now.
    pub fn firings_asking(&self) -> Vec<(String, String)> {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flat_map(|(sid, r)| r.asking.iter().map(move |g| (sid.clone(), g.clone())))
            .collect()
    }

    /// What firing `sid` mints with at grant `grant` at `now_s`.
    pub(super) fn firing_subject(&self, sid: &str, grant: &str, now_s: u64) -> FiringSubject {
        let f = self.firings.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = f.get(sid) else {
            return FiringSubject::Asked;
        };
        match r.subjects.get(grant) {
            Some(s) if s.exp > now_s + 30 => FiringSubject::Approved(s.id_token.clone(), s.exp),
            Some(_) => FiringSubject::Asked,
            None if r.asking.contains(grant) => FiringSubject::Asking,
            None if r.asked.contains(grant) => FiringSubject::Asked,
            // A restart lost the request or its approval: once more, unless
            // it had asked once more already, or was refused.
            None => match r.earlier.get(grant) {
                None | Some(Asked::Once) => FiringSubject::Unasked,
                Some(Asked::Again | Asked::Refused) => FiringSubject::Asked,
            },
        }
    }

    /// The live values a firing's calls are scrubbed of beside the grant's:
    /// its approvals' tokens.
    pub(super) fn firing_values(&self, sid: &str) -> Vec<String> {
        self.firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .map(|r| {
                r.subjects
                    .values()
                    .flat_map(|s| {
                        std::iter::once(s.id_token.clone()).chain(s.refresh_token.clone())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ask the person firing `sid` acts for to approve it, at grant
    /// `grant_id`, with `binding` as the message their device shows: off
    /// the loop, once [`Self::ciba_asking`] marked it, only while its wait
    /// is still open, and only once every refresh token an approval left
    /// in the store before this start is revoked
    /// ([`Self::revoke_left_approvals`]). `ciba.per_day` a day per person.
    pub fn start_ciba(
        &self,
        sid: &str,
        grant_id: &str,
        binding: &str,
    ) -> Result<ciba::Started, DelegationError> {
        let waiting = self
            .firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .is_some_and(|r| r.asking.contains(grant_id));
        if !waiting {
            return Err(refuse(
                Code::CibaDenied,
                "the wait for the person's approval ended before the request was sent",
            ));
        }
        let out = self
            .revoke_left_approvals()
            .and_then(|()| self.ask_ciba(sid, grant_id, binding));
        if out.is_err() {
            self.not_asking(sid, grant_id);
        }
        out
    }

    /// Firing `sid` waits on its person at grant `grant_id` no longer: its
    /// request was answered, refused, or its wait closed.
    pub fn not_asking(&self, sid: &str, grant_id: &str) {
        if let Some(r) = self
            .firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(sid)
        {
            r.asking.remove(grant_id);
        }
    }

    fn ask_ciba(
        &self,
        sid: &str,
        grant_id: &str,
        binding: &str,
    ) -> Result<ciba::Started, DelegationError> {
        let g = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(grant_id)
            .cloned()
            .ok_or_else(|| refuse(Code::NotEnrolled, "the grant is gone: enrol again"))?;
        let idp = self
            .providers
            .get(&g.idp)
            .ok_or_else(|| refuse(Code::Config, format!("no IdP {}", g.idp)))?;
        if !idp.cfg.ciba.enabled {
            return Err(refuse(
                Code::Config,
                format!("identity.providers.{}.ciba is not enabled", g.idp),
            ));
        }
        let meta = idp
            .meta(self.timeout)
            .map_err(|e| refuse(Code::IdpUnavailable, format!("{}: {e}", g.idp)))?;
        let endpoint = meta.ciba_endpoint.clone().ok_or_else(|| {
            refuse(
                Code::Config,
                format!(
                    "{}'s metadata offers no CIBA in poll mode ({} with {} listing poll)",
                    g.idp,
                    ciba::METADATA_ENDPOINT,
                    ciba::METADATA_MODES
                ),
            )
        })?;
        let params = idp.params(&meta).map_err(|e| refuse(Code::Config, e))?;
        let lock = self.lock_of(grant_id);
        let held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, mut rt) = self.load(grant_id)?;
        let mut q = g.ciba.clone().unwrap_or_default().at(now_ms());
        let per_day = idp.cfg.ciba.per_day();
        if q.day_count >= per_day {
            return Err(refuse(
                Code::BudgetExhausted,
                format!(
                    "{} has been sent {} CIBA requests today (ciba.per_day {per_day})",
                    g.principal.display(),
                    q.day_count
                ),
            ));
        }
        // The hint: a fresh ID token, sent once and dropped; else the name
        // the person enrolled with.
        let fresh = self.fresh_id_token(idp, &params, &mut g, &mut rt)?;
        let login = match &fresh {
            Some(_) => None,
            None => Some(self.login_hint(&g).ok_or_else(|| {
                refuse(
                    Code::Config,
                    format!(
                        "{} returned no ID token on refresh to hint the CIBA request with, and no login hint was kept when {} enrolled",
                        g.idp,
                        g.principal.display()
                    ),
                )
            })?),
        };
        let hint = match (&fresh, &login) {
            (Some(t), _) => Hint::IdToken(t),
            (None, Some(l)) => Hint::Login(l),
            (None, None) => unreachable!("a login hint stands in for a missing ID token"),
        };
        // Every request asked for counts, whatever the IdP answers.
        q.day_count += 1;
        g.ciba = Some(q.clone());
        if let Err(e) = self.vault.store(&g, &rt, Some(g.rt_version)) {
            self.log.warn(
                "delegation.quota.unsaved",
                json!({"grant_id": g.grant_id, "err": e.to_string()}),
            );
        }
        self.remember(&g);
        drop(held);
        let ask = ciba::Ask {
            hint,
            binding_message: binding,
            expiry_s: idp.cfg.ciba.request_expiry().as_secs(),
            expiry_param: match idp.cfg.profile {
                Profile::Okta => "request_expiry",
                Profile::Generic => "requested_expiry",
            },
        };
        let started =
            ciba::authorize(&params, &endpoint, &ask, self.timeout).map_err(|r| match r {
                Refusal::Oauth { code, status, .. } => refuse(
                    Code::CibaDenied,
                    format!("{} refused the CIBA request: {code} (HTTP {status})", g.idp),
                ),
                r => refuse(super::unavailable(&r), format!("{}: CIBA: {r}", g.idp)),
            })?;
        let run = self
            .firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sid)
            .map(|r| (r.firing.run.clone(), r.firing.workflow.clone()));
        self.audit(
            "delegation.ciba",
            "asked",
            Some(&g.principal.display()),
            json!({"grant_id": g.grant_id, "run": run.as_ref().map(|r| &r.0),
                   "workflow": run.as_ref().map(|r| &r.1), "binding_message": binding,
                   "hint": if fresh.is_some() { "id_token" } else { "login_hint" },
                   "expires_in": started.expires_in, "today": q.day_count, "per_day": per_day}),
        );
        Ok(started)
    }

    /// Poll the request firing `sid` sent its person at grant `grant_id`
    /// once: off the loop. An approval whose ID token names anyone but the
    /// grant's own `iss` and `sub` fails the firing closed.
    pub fn poll_ciba(&self, sid: &str, grant_id: &str, auth_req_id: &str) -> CibaOutcome {
        let refused = |code: Code, why: String| {
            self.not_asking(sid, grant_id);
            CibaOutcome::Refused { code, why }
        };
        let held = self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(grant_id)
            .cloned();
        let Some(g) = held else {
            return refused(
                Code::Revoked,
                "the grant the request was for is gone".into(),
            );
        };
        let Some(idp) = self.providers.get(&g.idp) else {
            return refused(Code::Config, format!("no IdP {}", g.idp));
        };
        let params = match idp.meta(self.timeout).and_then(|m| idp.params(&m)) {
            Ok(p) => p,
            // An IdP that does not answer now may by the next poll.
            Err(_) => return CibaOutcome::Pending,
        };
        let who = g.principal.display();
        match ciba::poll(&params, auth_req_id, self.timeout) {
            Ok(Polled::Pending) => CibaOutcome::Pending,
            Ok(Polled::SlowDown) => CibaOutcome::SlowDown,
            Ok(Polled::Denied) => {
                self.audit(
                    "delegation.ciba",
                    "denied",
                    Some(&who),
                    json!({"grant_id": g.grant_id, "sid": sid}),
                );
                refused(
                    Code::CibaDenied,
                    format!("{who} denied the request on their device"),
                )
            }
            Ok(Polled::Expired) => {
                self.audit(
                    "delegation.ciba",
                    "expired",
                    Some(&who),
                    json!({"grant_id": g.grant_id, "sid": sid}),
                );
                refused(
                    Code::CibaDenied,
                    format!("the request expired before {who} answered it"),
                )
            }
            Ok(Polled::Approved(t)) => {
                let signed = match super::enrol::verify(self, &g.idp, &t, None) {
                    Ok(s) => s,
                    Err(e) => {
                        if let Some(rt) = &t.refresh_token {
                            self.discard_refresh_token(idp, rt);
                        }
                        return refused(Code::CibaDenied, e.message);
                    }
                };
                if signed.claims.iss != g.idp_iss || signed.claims.sub != g.sub {
                    if let Some(rt) = &signed.refresh_token {
                        self.discard_refresh_token(idp, rt);
                    }
                    self.audit(
                        "delegation.denied",
                        "denied",
                        Some(&who),
                        json!({"code": Code::BindingMismatch.as_str(), "grant_id": g.grant_id,
                               "error": "a CIBA approval came from another identity than the grant's",
                               "approving_sub": signed.claims.sub}),
                    );
                    return refused(
                        Code::BindingMismatch,
                        format!(
                            "the approval came from {}#{}, and the grant is {}#{}'s: a hint can resolve to another account, so the firing fails closed",
                            signed.claims.iss, signed.claims.sub, g.idp_iss, g.sub
                        ),
                    );
                }
                let ended = |kept: Option<&Approval>| {
                    // The firing ended while its person approved: nothing
                    // is kept, and no refresh token is left alive.
                    if let Some(rt) = &signed.refresh_token {
                        self.discard_refresh_token(idp, rt);
                    }
                    if let Some(a) = kept {
                        let _ = self.vault.forget_approval(&a.name());
                    }
                    CibaOutcome::Refused {
                        code: Code::Revoked,
                        why: "the firing ended before its person approved it".into(),
                    }
                };
                let run = self
                    .firings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(sid)
                    .map(|r| r.firing.run.clone());
                let Some(run) = run else {
                    return ended(None);
                };
                // A refresh token the IdP issued anyway is kept sealed in
                // the store before the approval is used: a crash from here
                // on leaves none alive at the IdP that the next start does
                // not revoke. One that cannot be kept so is revoked now,
                // and the approval with it.
                let kept = match &signed.refresh_token {
                    None => None,
                    Some(rt) => {
                        let a = Approval {
                            firing: sid.to_string(),
                            run,
                            grant_id: grant_id.to_string(),
                            idp: g.idp.clone(),
                        };
                        match self.vault.keep_approval(&a, rt) {
                            Ok(()) => Some(a),
                            Err(e) => {
                                self.discard_refresh_token(idp, rt);
                                let e = super::not_stored(
                                    e,
                                    Code::IdpUnavailable,
                                    "the refresh token the IdP issued with the approval could not be kept sealed, so it was revoked and the approval with it",
                                );
                                return refused(e.code, e.message);
                            }
                        }
                    }
                };
                let held = {
                    let mut f = self.firings.lock().unwrap_or_else(|e| e.into_inner());
                    match f.get_mut(sid) {
                        Some(r) => {
                            r.asking.remove(grant_id);
                            r.subjects.insert(
                                grant_id.to_string(),
                                Subject {
                                    id_token: signed.id_token.clone(),
                                    exp: signed.claims.exp,
                                    refresh_token: signed.refresh_token.clone(),
                                    kept: kept.clone(),
                                },
                            );
                            true
                        }
                        None => false,
                    }
                };
                if !held {
                    return ended(kept.as_ref());
                }
                self.audit(
                    "delegation.ciba",
                    "approved",
                    Some(&who),
                    json!({"grant_id": g.grant_id, "sid": sid, "auth_time": signed.claims.auth_time,
                           "exp": signed.claims.exp,
                           "refresh_token": signed.refresh_token.is_some()}),
                );
                CibaOutcome::Approved
            }
            Err(Refusal::Unavailable(_)) => CibaOutcome::Pending,
            Err(r) => refused(Code::CibaDenied, format!("{}: CIBA: {r}", g.idp)),
        }
    }

    /// Firing `sid`'s run ended: its approvals are dropped, the tokens
    /// minted with them purged, and a refresh token an IdP issued with one
    /// anyway revoked at it (RFC 7009) and forgotten by the store — off the
    /// loop, since the IdP is asked.
    pub fn firing_ended(&self, sid: &str) {
        let Some(r) = self
            .firings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(sid)
        else {
            return;
        };
        let scoped = format!("|{sid}");
        self.pool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| !k.ends_with(&scoped));
        for grant in r.subjects.keys() {
            self.with_live(grant, |l| l.id_jags.retain(|k, _| !k.ends_with(&scoped)));
        }
        for (grant, s) in r.subjects {
            let Some(rt) = s.refresh_token else { continue };
            let idp = self
                .grants
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&grant)
                .and_then(|g| self.providers.get(&g.idp))
                .or_else(|| self.idp_of(&r.principal));
            let at_idp = match idp {
                Some(i) => self.revoke_at_idp(i, &rt),
                None => Err("the IdP is no longer declared".into()),
            };
            if let Some(a) = &s.kept
                && let Err(e) = self.vault.forget_approval(&a.name())
            {
                self.log.warn(
                    "delegation.ciba.unforgotten",
                    json!({"grant_id": grant, "run": r.firing.run, "err": e}),
                );
            }
            self.audit(
                "delegation.ciba",
                "revoked",
                None,
                json!({"grant_id": grant, "run": r.firing.run, "workflow": r.firing.workflow,
                       "revoked_at_idp": at_idp.is_ok(), "at_idp_error": at_idp.err()}),
            );
        }
    }

    /// Every refresh token an IdP issued with a CIBA approval that the
    /// store still keeps from before this start — a run the last life left
    /// unended, a crash — revoked at its IdP (RFC 7009) and forgotten:
    /// at the first pass, and before any request is sent, so a new
    /// approval is never revoked with an old one (RFC 0044 §7.P3c). Off the
    /// loop; done once. One the external signer cannot open now, or the
    /// store not read or list now, is kept for a later pass
    /// ([`Self::sweep_left_approvals`]), and every request meanwhile fails
    /// unsent; one that never opens is forgotten.
    pub fn revoke_left_approvals(&self) -> Result<(), DelegationError> {
        let mut swept = self
            .approvals_swept
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *swept {
            return Ok(());
        }
        let mut left = None;
        let names = self.vault.approvals().unwrap_or_else(|e| {
            left = Some((Code::IdpUnavailable, e));
            Vec::new()
        });
        for name in names {
            let (run, grant, at_idp) = match self.vault.open_approval(&name) {
                Ok(None) => continue,
                Ok(Some((a, rt))) => {
                    let at_idp = match self.providers.get(&a.idp) {
                        Some(i) => self.revoke_at_idp(i, &rt),
                        None => Err("the IdP is no longer declared".into()),
                    };
                    (Some(a.run), Some(a.grant_id), at_idp)
                }
                Err(Unopened::Signer(e)) => {
                    left = Some((Code::SignerUnavailable, e));
                    continue;
                }
                Err(Unopened::Store(e)) => {
                    left = Some((Code::IdpUnavailable, e));
                    continue;
                }
                // Nothing to revoke with: it is forgotten all the same.
                Err(Unopened::Never(e)) => (None, None, Err(format!("it does not open: {e}"))),
            };
            if let Err(e) = self.vault.forget_approval(&name) {
                self.log.warn(
                    "delegation.ciba.unforgotten",
                    json!({"grant_id": grant, "run": run, "err": e}),
                );
            }
            self.audit(
                "delegation.ciba",
                "revoked",
                None,
                json!({"grant_id": grant, "run": run, "left_by": "restart",
                       "revoked_at_idp": at_idp.is_ok(), "at_idp_error": at_idp.err()}),
            );
        }
        match left {
            None => {
                *swept = true;
                Ok(())
            }
            Some((code, e)) => {
                self.log
                    .warn("delegation.ciba.unrevoked", json!({"err": e.clone()}));
                Err(refuse(
                    code,
                    format!(
                        "a refresh token an approval left in the store before this start is not revoked yet, and no request is sent until it is: {e}"
                    ),
                ))
            }
        }
    }

    /// Run [`Self::revoke_left_approvals`] off the loop when it is due: at
    /// a start, and again after one the signer or the store kept from
    /// finishing — 5 s after, doubling to 5 min — until one finishes,
    /// whether or not a CIBA request comes meanwhile. At a start, and every
    /// reactor pass.
    pub fn sweep_left_approvals(self: &Arc<Self>) {
        {
            let mut due = self.approvals_due.lock().unwrap_or_else(|e| e.into_inner());
            if now_ms() < due.at_ms {
                return;
            }
            due.at_ms = u64::MAX;
        }
        let me = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("delegation-ciba-sweep".into())
            .spawn(move || {
                if me.revoke_left_approvals().is_err() {
                    let mut due = me.approvals_due.lock().unwrap_or_else(|e| e.into_inner());
                    let (first, most) = SWEEP_RETRY_MS;
                    due.backoff_ms = (due.backoff_ms * 2).clamp(first, most);
                    due.at_ms = now_ms() + due.backoff_ms;
                }
            });
        if spawned.is_err() {
            self.approvals_due
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .at_ms = 0;
        }
    }

    /// Keep `hint`, the name a person enrolled with at grant `g`'s IdP, for
    /// a CIBA request whose IdP returns no ID token on refresh: sealed under
    /// the person's personal key (§6.4), so forgetting them leaves it
    /// unreadable. Off the loop.
    pub(super) fn keep_hint(&self, g: &super::vault::Grant, hint: Option<&str>) {
        let id = format!("{HINTS}{}", g.grant_id);
        let Some(hint) = hint else {
            let _ = self.durable.delete(crate::state::Kind::Cred, &id);
            return;
        };
        let sealed = self.seal_personal_off_loop(
            &g.principal.display(),
            &format!("login_hint:{}", g.grant_id),
            &json!({"login_hint": hint}),
        );
        match sealed {
            Ok(v) => {
                if let Err(e) = self.durable.put(crate::state::Kind::Cred, &id, v, None) {
                    self.log.warn(
                        "delegation.hint.unkept",
                        json!({"grant_id": g.grant_id, "err": e.to_string()}),
                    );
                }
            }
            Err(e) => self.log.warn(
                "delegation.hint.unkept",
                json!({"grant_id": g.grant_id, "err": e}),
            ),
        }
    }

    /// Grant `g`'s kept login hint, opened.
    fn login_hint(&self, g: &super::vault::Grant) -> Option<String> {
        let sealed = self
            .durable
            .get(crate::state::Kind::Cred, &format!("{HINTS}{}", g.grant_id))
            .ok()
            .flatten()?
            .state;
        let opened = self
            .open_personal(
                &g.principal.display(),
                &format!("login_hint:{}", g.grant_id),
                &sealed,
            )
            .ok()
            .flatten()?;
        opened["login_hint"].as_str().map(str::to_string)
    }

    /// Forget grant `grant_id`'s login hint: it was revoked.
    pub(super) fn drop_hint(&self, grant_id: &str) {
        let _ = self
            .durable
            .delete(crate::state::Kind::Cred, &format!("{HINTS}{grant_id}"));
    }
}
