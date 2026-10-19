// SPDX-License-Identifier: AGPL-3.0-only
//! Signing the User in at a Company, and out (RFC 0047 §5.7, draft
//! §4.4–§4.6, §4.9) — the supervisor's half. The gate the User is asked at,
//! and who is owed the outcome, are the runtime's (`runtime::company_sign_in`).
//!
//! **Which account.** A sign-in signs in to one of the User's accounts at
//! the Company, by the label the User's side gives it (`account`; RFC 0047
//! §9, P5b): the one named, else the only one held, else
//! [`DEFAULT_LABEL`] while none is — and with several held and none named it
//! is refused, naming them. The label is agentd's own word: it is in the
//! gate's text and never in anything sent to the Company. A label not held
//! yet is signed in to in a new Session of its own (§4.4: "A User with two
//! accounts at the same Company … uses separate Sessions for them"), which
//! takes the label only once the sign-in succeeds, and is refused once
//! [`MAX_ACCOUNTS`] are held.
//!
//! **First, the Account Token held** (§4.4: "Before starting a new sign-in,
//! the Personal Agent SHOULD check whether it already holds an Account
//! Token that covers the task"): one whose scopes cover what is asked signs
//! the account's Session in with the `refresh_token` grant and its
//! `session_id`, and no gate opens.
//!
//! **Otherwise a sign-in**, of the first type `services.<name>.sign_in` lists
//! that the Company offers, asking for what is needed now and what the held
//! token grants (§4.4: "requests all the scopes it needs"), within the
//! entry's `scopes` and the type's `scopes`. An empty request is refused,
//! saying why.
//!
//! - **Device** (§4.6). The device authorization request, with `scope` and
//!   the client assertion; its `verification_uri` and
//!   `verification_uri_complete` must be HTTPS URLs with a plain host on the
//!   entry's hosts, or the sign-in is refused before any gate opens
//!   (`company.endpoint.refused`, field `verification_uri`), saying so
//!   without the URL — the host rule covers what the documents name, and
//!   these arrive in a response, so without it a hostile Company could send
//!   the User to another Company's page with a code it started there. Then
//!   the gate, and the poll: every `interval` with `device_code` and
//!   `session_id`, 5 s more on `slow_down`, ending on `access_denied` or
//!   `expired_token`, or after `expires_in` — at most
//!   [`token::DEVICE_FOR`]. The `device_code` never leaves the supervisor.
//! - **Direct** (§4.5). The authorization URL — S256 PKCE, a random `state`
//!   from the kernel CSPRNG bound to this sign-in, the Company and the
//!   Session — is shown in the gate as a link; agentd never opens a browser.
//!   The served callback hands back `{code, state, iss}`: an unknown `state`
//!   ends nothing, a known one is spent at once, and an `iss` that is not the
//!   Company's `auth.issuer` byte for byte refuses the response (RFC 9207;
//!   §4.5: "MUST check that `iss` exactly matches the `auth.issuer`"). The
//!   code is exchanged with the same `redirect_uri`, the verifier and the
//!   `session_id`.
//!
//! A sign-in that succeeds keeps its Account Token sealed before using the
//! answer, takes the signed-in Session Token, drops every Bearer token,
//! reports the scopes granted beside those requested (§4.4: "the Personal
//! Agent checks `scope` before continuing"), and revokes the Account Token
//! it replaced (§4.4: "which the Personal Agent SHOULD revoke"); one whose
//! token cannot be kept revokes the new token instead. One with
//! `new_account` replaces the account the label names in a new Session
//! (§4.4), which takes the label only on success. `account_mismatch` issues
//! no tokens and leaves what is held as it was ("A pending or failed sign-in
//! leaves existing tokens unchanged").
//!
//! **A label stays with its account.** A sign-in to a label goes in the
//! label's own Session while that Session has been signed in to its account
//! — the token held, or lost to `invalid_grant` and gone on signed out — so
//! the Company signs that account in again and answers `account_mismatch`
//! for any other (§4.4, §4.9: "Such a Session can be signed in again only to
//! the same account"). One that goes in a new Session may be signed in to
//! any account: with `new_account`, or to a label whose account was given up
//! and whose Session is not held — signed out of, taken by a reset, or ended
//! at the Company ([`super::account::Accounts::given_up`]) — it suspends the
//! standing permissions covering the label until a restart
//! (`company.standing.suspended`), so they never cover an account they were
//! not declared for.
//!
//! **Signing out** (§4.9) of one account forgets its Account Token, revokes
//! it at the `revocation_endpoint` (RFC 7009, `token_type_hint=refresh_token`,
//! the client assertion), and stops using its Session — but the last
//! account's, which goes on signed out as the signed-out Session ("Usually
//! those Sessions continue signed out, so a cart … carries on"); every other
//! account the User holds there stays as it is. A sign-in and a sign-out
//! change the held token under its Session's lock, as a renewal that rotates
//! it does, so neither acts on a token a renewal in flight is replacing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::account::{Account, DEFAULT_LABEL, MAX_ACCOUNTS};
use super::token::{self, DeviceAuthorization, Request};
use super::wire::{self, error as err};
use super::{Company, Supervisor};
use crate::config::settings::companies::{SignIn, host_matches, host_of, is_https_url};

/// The longest a Company-chosen string a gate quotes may be.
const QUOTE_CAP: usize = 96;
/// The longest a verification URI or authorization URL shown in a gate may
/// be.
const URL_CAP: usize = 2048;
/// How long a Direct Sign-In waits for the User to come back.
pub const DIRECT_FOR: Duration = Duration::from_secs(10 * 60);
/// What a `slow_down` adds to the poll interval (RFC 8628 §3.5).
pub const SLOW_DOWN: Duration = Duration::from_secs(5);

/// What a sign-in asks for (`company.sign_in`).
#[derive(Debug, Clone)]
pub struct Ask {
    pub company: String,
    /// The scopes needed now; none asks for every scope the entry allows.
    pub scopes: Vec<String>,
    /// The label of the account it signs in to; none names the only one
    /// held, or [`DEFAULT_LABEL`] while none is.
    pub account: Option<String>,
    /// Sign in to another account than the one the label names, in a new
    /// Session.
    pub new_account: bool,
}

/// How a sign-in ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Signed in, with the scopes granted, beside those requested.
    SignedIn {
        scope: Vec<String>,
        requested: Vec<String>,
    },
    /// The User answered `cancel`, or the work that asked ended.
    Cancelled,
    /// The User declined at the Company (`access_denied`).
    Denied,
    /// The User did not finish in time (`expired_token`, or the deadline).
    Expired,
    /// The User signed in to another account than the Session's (§4.4).
    AccountMismatch,
    /// agentd restarted while it was pending.
    Interrupted,
    /// Anything else, in agentd's words.
    Failed(String),
}

impl Outcome {
    /// The word the tool's `outcome` and the log say.
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::SignedIn { .. } => "signed_in",
            Outcome::Cancelled => "cancelled",
            Outcome::Denied => "denied",
            Outcome::Expired => "expired",
            Outcome::AccountMismatch => "account_mismatch",
            Outcome::Interrupted => "interrupted",
            Outcome::Failed(_) => "failed",
        }
    }

    /// What the asker is told: the `company.sign_in` result, naming the
    /// account by its label once it is known, or for a failure an error
    /// text.
    pub fn result(&self, company: &str, account: Option<&str>) -> Result<Value, String> {
        match self {
            Outcome::Failed(why) => Err(format!("company {company}: the sign-in failed: {why}")),
            Outcome::SignedIn { scope, requested } => Ok(json!({
                "company": company, "account": account, "outcome": self.word(), "signed_in": true,
                "scope": scope, "requested": requested,
            })),
            _ => Ok(
                json!({"company": company, "account": account, "outcome": self.word(), "signed_in": false}),
            ),
        }
    }

    /// What the gate's task says when it ends; `account` is the label it
    /// signed in to, once known.
    pub fn said(&self, company: &str, account: Option<&str>) -> String {
        let at = match account {
            Some(l) => format!("{company}, as the account {l:?},"),
            None => company.to_string(),
        };
        match self {
            Outcome::SignedIn { scope, requested } if scope.len() < requested.len() => format!(
                "Signed in at {at} with fewer scopes than asked for: {} of {}.",
                list(scope),
                list(requested)
            ),
            Outcome::SignedIn { scope, .. } => {
                format!("Signed in at {at} for {}.", list(scope))
            }
            Outcome::Cancelled => format!("The sign-in at {company} was cancelled."),
            Outcome::Denied => format!("The sign-in at {company} was declined at the Company."),
            Outcome::Expired => format!("The sign-in at {company} was not finished in time."),
            Outcome::AccountMismatch => match account {
                Some(l) => format!(
                    "You signed in at {company} to a different account than the one agentd calls {l:?}; nothing changed. Ask to sign in with new_account to replace it, or under another label to hold both."
                ),
                None => format!(
                    "You signed in at {company} to a different account than this Session's before; nothing changed. Ask to sign in to a new account to use it."
                ),
            },
            Outcome::Interrupted => format!(
                "The sign-in at {company} was interrupted: agentd restarted before it finished. Ask again to sign in."
            ),
            Outcome::Failed(why) => format!("The sign-in at {company} failed: {why}"),
        }
    }
}

fn list(scopes: &[String]) -> String {
    if scopes.is_empty() {
        "no scopes".into()
    } else {
        scopes.join(" ")
    }
}

/// How a sign-in began.
pub enum Started {
    /// The held Account Token covers the scopes: the Session is signed in
    /// with it, and no gate opens.
    Held(Outcome),
    /// The gate's text, the scopes asked for, and the Device Sign-In to
    /// poll.
    Device {
        question: String,
        requested: Vec<String>,
        poll: Box<DevicePoll>,
    },
    /// The gate's text and the scopes asked for: the callback ends it.
    Direct {
        question: String,
        requested: Vec<String>,
    },
}

/// A Device Sign-In in progress. Its device code stays in the supervisor.
pub struct DevicePoll {
    company: Arc<Company>,
    /// The label of the account it signs in to.
    account: String,
    /// It may sign the label in to another account than the one it held.
    rebinds: bool,
    device: DeviceAuthorization,
    session_id: String,
    requested: Vec<String>,
    started: Instant,
}

impl DevicePoll {
    /// When the Company stops taking the device code.
    pub fn expires_in(&self) -> Duration {
        self.device.expires_in
    }
}

/// A Direct Sign-In waiting for its callback, by `state`.
pub struct Direct {
    /// The runtime's sign-in this one is.
    pub id: String,
    company: Arc<Company>,
    /// The label of the account it signs in to.
    account: String,
    /// It may sign the label in to another account than the one it held.
    rebinds: bool,
    verifier: String,
    redirect_uri: String,
    issuer: String,
    session_id: String,
    requested: Vec<String>,
    until: Instant,
}

/// A Direct Sign-In callback whose `state` named a sign-in, now spent: what
/// [`Supervisor::direct_finish`] checks and exchanges.
pub struct Returned {
    pending: Direct,
    query: BTreeMap<String, String>,
}

/// What a step of a sign-in hands the runtime.
#[derive(Debug, Clone)]
pub enum Step {
    /// It signs in to the account of this label.
    Account(String),
    /// Open the gate with `question`, of `kind`, asking for `requested`,
    /// for at most `for_ms`.
    Gate {
        question: String,
        kind: SignIn,
        requested: Vec<String>,
        for_ms: u64,
    },
    /// It ended.
    Done(Outcome),
}

impl Supervisor {
    /// The label of the account the sign-in `ask` signs in to at `company`:
    /// the one it names, else the only one held, else [`DEFAULT_LABEL`] while
    /// none is. Several held and none named, or a label not held once
    /// [`MAX_ACCOUNTS`] are, is refused before anything is sent.
    fn sign_in_label(&self, company: &Company, ask: &Ask) -> Result<String, String> {
        let name = &company.name;
        if let Some(l) = ask
            .account
            .as_deref()
            .filter(|l| !super::account::is_label(l))
        {
            return Err(format!(
                "company {name}: {}",
                super::account::label_refused(l)
            ));
        }
        let issuer = self
            .verified(company)
            .map_err(|e| format!("company {name} is unavailable: {e}"))?
            .issuer
            .clone();
        let held = match &self.accounts {
            Some(a) => a.labels(self.principal(), &issuer)?,
            None => Vec::new(),
        };
        let label = match (ask.account.as_deref(), held.as_slice()) {
            (Some(l), _) => l.to_string(),
            (None, []) => DEFAULT_LABEL.to_string(),
            (None, [only]) => only.clone(),
            (None, several) => {
                return Err(format!(
                    "company {name}: the User holds several accounts here ({}): name the one to sign in to as account, or a new label to sign in to another",
                    several.join(", ")
                ));
            }
        };
        if !held.contains(&label) && held.len() >= MAX_ACCOUNTS {
            return Err(format!("company {name}: {}", super::account::full(&held)));
        }
        Ok(label)
    }

    /// Begin the sign-in `ask` (RFC 0047 §5.7 steps 1–4): the label of the
    /// account it signs in to, and the held Account Token when it covers
    /// what is asked, else a Device or Direct Sign-In ready for its gate —
    /// `id` names it to the runtime. With `nobody` saying why nobody can be
    /// asked, only the held token can sign the User in, and nothing is
    /// started at the Company.
    pub fn begin_sign_in(
        &self,
        ask: &Ask,
        id: &str,
        nobody: Option<&str>,
    ) -> Result<(String, Started), String> {
        let company = self
            .company(&ask.company)
            .ok_or_else(|| format!("{:?} is not a company entry", ask.company))?;
        let name = company.name.clone();
        let entry = &company.discovery.entry;
        if entry.sign_in.is_empty() {
            return Err(format!(
                "company {name} lets agentd sign nobody in (services.{name}.sign_in lists no sign-in type)"
            ));
        }
        let wanted = if ask.scopes.is_empty() {
            entry.scopes.clone()
        } else {
            ask.scopes.clone()
        };
        let label = self.sign_in_label(&company, ask)?;
        let started = self.with_session(&company, Some(&label), |session, env| {
            let mut held = session.held(env)?;
            // 1. The Account Token held, when it covers what is asked.
            if !ask.new_account && held.as_ref().is_some_and(|a| a.covers(&wanted)) {
                let (signed_in, scope) = session.renew_now(env)?;
                if signed_in {
                    return Ok(Started::Held(Outcome::SignedIn {
                        scope,
                        requested: wanted.clone(),
                    }));
                }
                // The Company no longer takes it, so it was forgotten as the
                // Session was renewed: a sign-in is needed after all.
                held = session.held(env)?;
            }
            // 2. The type, and the scopes.
            let v = env.verified;
            let offered = |k: SignIn| match k {
                SignIn::Device => v
                    .device_authorization_endpoint
                    .as_ref()
                    .zip(v.device_scopes.as_ref()),
                SignIn::Direct => v
                    .authorization_endpoint
                    .as_ref()
                    .zip(v.direct_scopes.as_ref())
                    .filter(|_| self.identity.redirect_uri.is_some()),
                SignIn::Mediated => None,
            };
            let Some((kind, (endpoint, granted))) = entry
                .sign_in
                .iter()
                .find_map(|k| offered(*k).map(|o| (*k, o)))
            else {
                return Err(format!(
                    "company {name} offers none of the sign-in types services.{name}.sign_in lists ({})",
                    entry
                        .sign_in
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            let held_scopes = held
                .as_ref()
                .filter(|_| !ask.new_account)
                .map(|a| a.scope.clone())
                .unwrap_or_default();
            let requested = requested(&wanted, &held_scopes, &entry.scopes, granted);
            if requested.is_empty() {
                return Err(format!(
                    "company {name}: none of {} can be asked for — services.{name}.scopes allows {}, and {} sign-in grants {}",
                    list(&wanted),
                    list(&entry.scopes),
                    kind.as_str(),
                    list(granted)
                ));
            }
            if let Some(why) = nobody {
                return Err(format!(
                    "company {name}: signing in needs the User, and nobody can be asked ({why})"
                ));
            }
            // The label's own Session, while it has been signed in to its
            // account — held, or lost to the Company and gone on signed out:
            // the Company signs that account in again there and no other.
            // Else — and for another account under the label — a new
            // Session, which the Company may sign in to any account: one
            // under a label that held an account before suspends the standing
            // permissions covering it once it succeeds (RFC 0047 §5.8.1).
            let bound = if ask.new_account {
                None
            } else {
                session.bound_session_id(env)?
            };
            let rebinds = ask.new_account
                || (bound.is_none()
                    && match env.accounts {
                        Some(store) => store.given_up(env.principal, &session.issuer, &label)?,
                        None => false,
                    });
            let session_id = match bound {
                Some(sid) => sid,
                None => session.fresh_session_id(env)?,
            };
            let scope = requested.join(" ");
            let phrases = phrases(&requested, v);
            let named = company.named();
            let kept_as = format!("agentd calls this account {label:?}");
            match kind {
                SignIn::Device => {
                    let user_id = session.user_id(env)?;
                    let device = token::device_authorization(
                        &session.ctx(env, &user_id),
                        endpoint,
                        &scope,
                    )
                    .map_err(|e| format!("company {name}: the device authorization: {e}"))?;
                    // The host rule, over what the response names.
                    for (field, url) in [
                        ("verification_uri", Some(&device.verification_uri)),
                        (
                            "verification_uri_complete",
                            device.verification_uri_complete.as_ref(),
                        ),
                    ] {
                        let Some(url) = url else { continue };
                        // Said without the URL: its query may carry the
                        // code, which the model never sees.
                        if let Err(why) = admitted(url, &v.hosts) {
                            self.log().warn(
                                "company.endpoint.refused",
                                json!({"company": name, "field": field, "host": host_of(url)}),
                            );
                            return Err(format!(
                                "company {name}: the device authorization's {field} {why}, so it is not shown to the User; it must be an HTTPS URL on one of services.{name}.hosts"
                            ));
                        }
                    }
                    let code = super::quoted(&device.user_code, 32);
                    let question = match &device.verification_uri_complete {
                        Some(full) => format!(
                            "Sign in to {named} for: {phrases}; {kept_as}. Open {full} and check that the page shows the code {code}. This closes by itself once you have signed in there; answer cancel to stop."
                        ),
                        None => format!(
                            "Sign in to {named} for: {phrases}; {kept_as}. Open {} and enter the code {code}. This closes by itself once you have signed in there; answer cancel to stop.",
                            device.verification_uri
                        ),
                    };
                    Ok(Started::Device {
                        question,
                        requested: requested.clone(),
                        poll: Box::new(DevicePoll {
                            company: Arc::clone(&company),
                            account: label.clone(),
                            rebinds,
                            device,
                            session_id,
                            requested,
                            started: Instant::now(),
                        }),
                    })
                }
                // `offered` admits Direct as the only other type.
                SignIn::Direct | SignIn::Mediated => {
                    let random = |n| {
                        crate::sec::random::b64url_token(n)
                            .map_err(|e| format!("company {name}: no OS randomness: {e}"))
                    };
                    let verifier = random(32)?;
                    let state = random(16)?;
                    let challenge = crate::b64::url_nopad(&crate::sha::sha256(verifier.as_bytes()));
                    let redirect_uri = self.identity.redirect_uri.clone().unwrap_or_default();
                    let mut q = crate::auth::oauth2::Form::new();
                    for (k, val) in [
                        ("response_type", "code"),
                        ("client_id", self.identity.client_id.as_str()),
                        ("redirect_uri", redirect_uri.as_str()),
                        ("scope", scope.as_str()),
                        ("state", state.as_str()),
                        ("code_challenge", challenge.as_str()),
                        ("code_challenge_method", "S256"),
                    ] {
                        q.field(k, val);
                    }
                    let sep = if endpoint.contains('?') { '&' } else { '?' };
                    let url = format!(
                        "{endpoint}{sep}{}",
                        String::from_utf8(q.finish()).unwrap_or_default()
                    );
                    if url.len() > URL_CAP {
                        return Err(format!(
                            "company {name}: the authorization URL is over {URL_CAP} bytes"
                        ));
                    }
                    self.direct
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(
                            state,
                            Direct {
                                id: id.to_string(),
                                company: Arc::clone(&company),
                                account: label.clone(),
                                rebinds,
                                verifier,
                                redirect_uri,
                                issuer: v.issuer.clone(),
                                session_id,
                                requested: requested.clone(),
                                until: Instant::now() + DIRECT_FOR,
                            },
                        );
                    Ok(Started::Direct {
                        requested,
                        question: format!(
                            "Sign in to {named} for: {phrases}; {kept_as}. Open this link and sign in on the Company's own page: {url} — this closes by itself once you have signed in there; answer cancel to stop."
                        ),
                    })
                }
            }
        })?;
        Ok((label, started))
    }

    /// Poll the Device Sign-In `poll` to its end (§4.6): every `interval`,
    /// 5 s more on each `slow_down`, until the User approves, declines, or
    /// the code expires — or `cancel` is set.
    pub fn poll_device(&self, poll: Box<DevicePoll>, cancel: &AtomicBool) -> Outcome {
        let company = Arc::clone(&poll.company);
        let mut interval = poll.device.interval;
        // `expires_in` is held to `token::DEVICE_FOR`, and no wait runs past
        // it, so no Company's number overflows the clock.
        let until = poll.started + poll.device.expires_in;
        loop {
            // Wait out the interval in short slices, so a cancel is seen.
            let next =
                Instant::now() + interval.min(until.saturating_duration_since(Instant::now()));
            while Instant::now() < next {
                if cancel.load(Ordering::SeqCst) {
                    return Outcome::Cancelled;
                }
                std::thread::sleep(
                    Duration::from_millis(100).min(next.saturating_duration_since(Instant::now())),
                );
            }
            if cancel.load(Ordering::SeqCst) {
                return Outcome::Cancelled;
            }
            if Instant::now() >= until {
                break Outcome::Expired;
            }
            let answer = self.with_session(&company, Some(&poll.account), |session, env| {
                let user_id = session.user_id(env)?;
                Ok(token::request(
                    &session.ctx(env, &user_id),
                    Request::device(&poll.device.device_code, &poll.session_id),
                ))
            });
            let got = match answer {
                Err(e) => break Outcome::Failed(e),
                Ok(Ok(got)) => got,
                Ok(Err(e)) if e.is(err::AUTHORIZATION_PENDING) => continue,
                Ok(Err(e)) if e.is(err::SLOW_DOWN) => {
                    interval = interval.saturating_add(SLOW_DOWN);
                    continue;
                }
                Ok(Err(e)) if e.is(err::ACCESS_DENIED) => break Outcome::Denied,
                Ok(Err(e)) if e.is(err::EXPIRED_TOKEN) => break Outcome::Expired,
                Ok(Err(e)) if e.is(err::ACCOUNT_MISMATCH) => break Outcome::AccountMismatch,
                // No answer, a failed server or a wait the Company asked
                // for: the User may still be signing in, so the poll goes on.
                Ok(Err(e)) if e.repeatable() || matches!(e, token::TokenError::Paced(_)) => {
                    continue;
                }
                Ok(Err(e)) => break Outcome::Failed(e.to_string()),
            };
            break self.finish(
                &company,
                &poll.account,
                &got,
                SignIn::Device,
                &poll.requested,
                poll.rebinds,
            );
        }
    }

    /// The Direct Sign-In callback's `query` (§4.5): the sign-in its `state`
    /// names — spent at once, whatever comes of it — and how it ended. An
    /// unknown `state`, or one past its time, names none.
    pub fn direct_callback(&self, query: &str) -> Option<(String, Outcome)> {
        self.direct_returned(query).map(|r| self.direct_finish(r))
    }

    /// The first half of [`Self::direct_callback`], which sends nothing: the
    /// sign-in the callback's `state` names, spent now. One that names none
    /// is said — at most once a minute, with how many there have been.
    pub fn direct_returned(&self, query: &str) -> Option<Returned> {
        let q = form_of(query);
        let pending = q.get("state").and_then(|state| {
            let mut d = self.direct.lock().unwrap_or_else(|e| e.into_inner());
            d.retain(|_, p| p.until > Instant::now());
            // Compared in constant time, whichever is asked for.
            let key = d
                .keys()
                .find(|k| crate::sha::ct_eq(k.as_bytes(), state.as_bytes()))
                .cloned()?;
            d.remove(&key)
        });
        let Some(pending) = pending else {
            let say = self
                .callbacks_refused
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(false, Instant::now());
            if let crate::obs::quiet::Say::Failed { failures } = say {
                self.log().warn(
                    "company.sign_in.callback_refused",
                    json!({"reason": "no Direct Sign-In waits on this state (unknown, spent or past its time)",
                           "refused": failures}),
                );
            }
            return None;
        };
        Some(Returned { pending, query: q })
    }

    /// The second half of [`Self::direct_callback`]: the callback checked
    /// and its code exchanged — the sign-in it is, and how it ended.
    pub fn direct_finish(&self, returned: Returned) -> (String, Outcome) {
        let Returned { pending, query: q } = returned;
        let company = Arc::clone(&pending.company);
        let name = company.name.clone();
        let outcome = match (q.get("iss"), q.get("error"), q.get("code")) {
            // RFC 9207, and §4.5's MUST: the response is the issuer's this
            // sign-in went to, byte for byte, or it is refused.
            (iss, _, _) if iss.map(String::as_str) != Some(pending.issuer.as_str()) => {
                self.log().warn(
                    "company.sign_in.mixup",
                    json!({"company": name, "iss": iss.map(|i| super::quoted(i, QUOTE_CAP))}),
                );
                Outcome::Failed(format!(
                    "the response did not come from {}'s issuer ({}), so its code was not used",
                    name, pending.issuer
                ))
            }
            (_, Some(e), _) if e == err::ACCESS_DENIED => Outcome::Denied,
            (_, Some(e), _) => Outcome::Failed(format!(
                "the Company answered {}",
                super::quoted(e, QUOTE_CAP)
            )),
            (_, None, Some(code)) => {
                let exchanged =
                    self.with_session(&company, Some(&pending.account), |session, env| {
                        let user_id = session.user_id(env)?;
                        Ok(token::request(
                            &session.ctx(env, &user_id),
                            Request::code(
                                code,
                                &pending.redirect_uri,
                                &pending.verifier,
                                &pending.session_id,
                            ),
                        ))
                    });
                match exchanged {
                    Err(e) => Outcome::Failed(e),
                    Ok(Ok(got)) => self.finish(
                        &company,
                        &pending.account,
                        &got,
                        SignIn::Direct,
                        &pending.requested,
                        pending.rebinds,
                    ),
                    Ok(Err(e)) if e.is(err::ACCOUNT_MISMATCH) => Outcome::AccountMismatch,
                    Ok(Err(e)) => Outcome::Failed(e.to_string()),
                }
            }
            (_, None, None) => Outcome::Failed("the callback carried no code".into()),
        };
        (pending.id, outcome)
    }

    /// Forget the Direct Sign-In `id`, which ended another way: its callback
    /// is refused from now on.
    pub fn forget_direct(&self, id: &str) {
        self.direct
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, p| p.id != id);
    }

    /// A sign-in's answer `got` for the account `label`: its Account Token
    /// kept and its Session signed in, the token it replaced revoked, and
    /// the scopes granted beside those requested. A token that cannot be
    /// kept is revoked, so none is left good at the Company that agentd does
    /// not hold. One that `rebinds` the label — it may be to another account
    /// than the label held — suspends the standing permissions covering it
    /// until a restart: the operator declared them for the account it held.
    fn finish(
        &self,
        company: &Arc<Company>,
        label: &str,
        got: &token::TokenResponse,
        kind: SignIn,
        requested: &[String],
        rebinds: bool,
    ) -> Outcome {
        let Some(issued) = got.refresh_token.as_ref().filter(|_| got.signed_in) else {
            return Outcome::Failed(
                "the Company's answer is not a signed-in Session with an Account Token".into(),
            );
        };
        let replaced = self.with_session(company, Some(label), |session, env| {
            session.signed_in(env, got, kind.as_str())
        });
        let replaced = match replaced {
            Ok(r) => r,
            Err(e) => {
                let unkept = Account {
                    token: issued.clone(),
                    scope: got.scope.clone(),
                    expires_at_ms: None,
                    kind: kind.as_str().to_string(),
                    session_id: None,
                };
                let revoked = self.revoke_account(company, Some(label), &unkept);
                self.log().warn(
                    "company.account_token.unkept",
                    json!({"company": company.name, "account": label, "revoked": revoked.is_ok(),
                           "error": e}),
                );
                return Outcome::Failed(e);
            }
        };
        if let Some(old) = replaced {
            let revoked = self.revoke_account(company, Some(label), &old);
            self.log().info(
                "company.account_token.replaced",
                json!({"company": company.name, "account": label, "revoked": revoked.is_ok(),
                       "error": revoked.err()}),
            );
        }
        if rebinds {
            self.suspend_standing(&company.name, label);
        }
        Outcome::SignedIn {
            scope: got.scope.clone(),
            requested: requested.to_vec(),
        }
    }

    /// Revoke `account` at `company`'s revocation endpoint (§4.9), from the
    /// Session of the account `label` names, or the signed-out one.
    pub fn revoke_account(
        &self,
        company: &Company,
        label: Option<&str>,
        account: &Account,
    ) -> Result<(), String> {
        self.with_session(company, label, |session, env| {
            let user_id = session.user_id(env)?;
            token::revoke(
                &session.ctx(env, &user_id),
                &env.verified.revocation_endpoint,
                &account.token,
            )
            .map_err(|e| format!("company {}: the revocation: {e}", company.name))
        })
    }

    /// Sign the User out of one account at `company` (§4.9): the one
    /// `named`, else the only one held. Its Account Token is forgotten and
    /// its Session signed out — the token taken as a renewal in flight left
    /// it, so a rotated one is the one revoked — then revoked, and its
    /// Session forgotten; every other account stays as it is. `{company,
    /// account, signed_out, revoked}`; `signed_out` is false when the
    /// account was not held. Several held and none named is refused.
    pub fn sign_out(&self, company: &Company, named: Option<&str>) -> Result<Value, SignOutError> {
        let name = &company.name;
        let refused = SignOutError::Refused;
        if let Some(l) = named.filter(|l| !super::account::is_label(l)) {
            return Err(refused(format!(
                "company {name}: {}",
                super::account::label_refused(l)
            )));
        }
        let issuer = self
            .verified(company)
            .map_err(|e| refused(format!("company {name} is unavailable: {e}")))?
            .issuer
            .clone();
        let held = match &self.accounts {
            Some(a) => a.labels(self.principal(), &issuer).map_err(refused)?,
            None => Vec::new(),
        };
        let label = match (named, held.as_slice()) {
            (Some(l), _) => l.to_string(),
            (None, []) => {
                return Ok(
                    json!({"company": name, "account": null, "signed_out": false, "revoked": false}),
                );
            }
            (None, [only]) => only.clone(),
            (None, several) => {
                return Err(refused(format!(
                    "company {name}: the User holds several accounts here ({}): name the one to sign out of as account",
                    several.join(", ")
                )));
            }
        };
        let taken = self
            .with_session(company, Some(&label), |session, env| match env.accounts {
                Some(store) => session.take_account(store, env.principal, name),
                None => Ok(None),
            })
            .map_err(refused)?;
        let Some(account) = taken else {
            return Ok(
                json!({"company": name, "account": label, "signed_out": false, "revoked": false}),
            );
        };
        let revoked = self.revoke_account(company, Some(&label), &account);
        self.drop_session(company, &label);
        // The last account signed out of: work goes on in its Session,
        // signed out — a cart there carries on.
        if let Some(sid) = &account.session_id
            && held.iter().all(|l| *l == label)
            && let Err(e) = self.go_on_signed_out(&issuer, sid)
        {
            self.log().warn(
                "company.session.kept",
                json!({"company": name, "account": label, "error": e}),
            );
        }
        match revoked {
            Ok(()) => {
                Ok(json!({"company": name, "account": label, "signed_out": true, "revoked": true}))
            }
            Err(e) => Err(SignOutError::Unrevoked {
                error: format!(
                    "company {name}: signed out of the account {label:?} here, but {e}; its Account Token stays good at the Company until it expires"
                ),
                account: label,
            }),
        }
    }
}

/// Why a sign-out did not end as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignOutError {
    /// Nothing was taken: no account it could name, or the Company is not
    /// verified.
    Refused(String),
    /// The account's Account Token is forgotten here, and the Company did not
    /// take its revocation.
    Unrevoked { account: String, error: String },
}

impl std::fmt::Display for SignOutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignOutError::Refused(e) | SignOutError::Unrevoked { error: e, .. } => f.write_str(e),
        }
    }
}

/// How a gate names each of `scopes`: agentd's words for the draft's two,
/// the Company's own — quoted, from its document — for its others.
fn phrases(scopes: &[String], v: &super::discover::Verified) -> String {
    scopes
        .iter()
        .map(|s| match s.as_str() {
            wire::SCOPE_READ => "view your account information".to_string(),
            wire::SCOPE_WRITE => "make changes".to_string(),
            other => match v.custom_scopes.get(other) {
                Some(d) => format!(
                    "{} ({}'s words, from its published document)",
                    super::quoted(d, QUOTE_CAP),
                    v.domain
                ),
                None => super::quoted(other, QUOTE_CAP),
            },
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `scopes` ∪ `held`, each within `ceiling` and what the sign-in type
/// `granted`, in the order of `ceiling` — every scope needed now, and every
/// one the held token already grants (§4.4).
pub fn requested(
    scopes: &[String],
    held: &[String],
    ceiling: &[String],
    granted: &[String],
) -> Vec<String> {
    let asked: Vec<&String> = scopes
        .iter()
        .filter(|s| ceiling.contains(s) && granted.contains(s))
        .collect();
    if asked.is_empty() {
        return Vec::new();
    }
    ceiling
        .iter()
        .filter(|s| granted.contains(s) && (scopes.contains(s) || held.contains(s)))
        .cloned()
        .collect()
}

/// Whether `url` is an HTTPS URL with a plain host on one of `hosts`
/// ([`host_of`]), in printable ASCII — no space, control or bidirectional
/// character a gate could misprint, an IDNA name in its `xn--` form; else
/// what it is, in words that never quote it.
fn admitted(url: &str, hosts: &[String]) -> Result<(), String> {
    let Some(host) = host_of(url).filter(|_| is_https_url(url)) else {
        return Err("is not an HTTPS URL with a plain host".into());
    };
    if url.len() > URL_CAP || !url.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(format!(
            "is over {URL_CAP} bytes or holds a space, control or non-ASCII character"
        ));
    }
    if !hosts.iter().any(|p| host_matches(p, &host)) {
        return Err(format!(
            "names host {host}, which is not one of the entry's hosts"
        ));
    }
    Ok(())
}

/// A query string, decoded; a parameter given twice is none.
fn form_of(query: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut twice: Vec<String> = Vec::new();
    for kv in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let (k, v) = (
            crate::auth::browser::pct_decode(k),
            crate::auth::browser::pct_decode(v),
        );
        if out.insert(k.clone(), v).is_some() {
            twice.push(k);
        }
    }
    for k in twice {
        out.remove(&k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// A sign-in asks for what is needed and what the held token grants,
    /// each within the entry's scopes and the type's — and nothing when
    /// none of what is needed can be asked for.
    #[test]
    fn a_sign_in_asks_for_what_is_needed_within_both_ceilings() {
        let ceiling = s(&["poppy:read", "poppy:write", "addresses"]);
        let device = s(&["poppy:read", "poppy:write"]);
        assert_eq!(
            requested(&s(&["poppy:write"]), &s(&["poppy:read"]), &ceiling, &device),
            ["poppy:read", "poppy:write"],
            "the held scope is asked for again, not lost"
        );
        assert_eq!(
            requested(&s(&["addresses"]), &[], &ceiling, &device),
            Vec::<String>::new(),
            "the type cannot grant it"
        );
        assert_eq!(
            requested(
                &s(&["poppy:write", "orders"]),
                &[],
                &s(&["poppy:write"]),
                &device
            ),
            ["poppy:write"],
            "what the entry does not allow is not asked for"
        );
    }

    /// Only an HTTPS URL on the entry's hosts, with nothing in it a gate
    /// could misprint, is shown to the User.
    #[test]
    fn only_a_verification_uri_on_the_hosts_is_shown() {
        let hosts = s(&["example.com", "*.example.com"]);
        assert!(admitted("https://auth.example.com/device?user_code=X", &hosts).is_ok());
        assert!(admitted("https://auth.example.com:8443/device", &hosts).is_ok());
        let off = admitted("https://other.example/device?user_code=WDJB-MJHT", &hosts);
        assert_eq!(
            off,
            Err("names host other.example, which is not one of the entry's hosts".into())
        );
        assert!(admitted("http://auth.example.com/device", &hosts).is_err());
        assert!(admitted("https://auth.example.com/de vice", &hosts).is_err());
        assert!(admitted("https://auth.example.com/\u{202e}x", &hosts).is_err());
        // Nothing a browser could read another host from: it ends the
        // authority at `\` as at `/`, takes the host after an `@`, and
        // decodes a `%` in it.
        for hidden in [
            "https://other.example\\@auth.example.com/device?user_code=X",
            "https://other.example\\auth.example.com/device",
            "https://other.example@auth.example.com/device",
            "https://auth.example.com%2f@other.example/device",
            "https://other.example%2f.example.com/device",
        ] {
            assert_eq!(
                admitted(hidden, &hosts),
                Err("is not an HTTPS URL with a plain host".into()),
                "{hidden}"
            );
        }
    }

    /// A callback's query is decoded once per parameter, and one given
    /// twice is taken as given by neither.
    #[test]
    fn a_callback_names_each_parameter_once() {
        let q = form_of("code=a%2Bb&state=s1&iss=https%3A%2F%2Fas.example&x=");
        assert_eq!(q["code"], "a+b");
        assert_eq!(q["iss"], "https://as.example");
        let q = form_of("state=s1&state=s2&code=c");
        assert!(!q.contains_key("state"));
    }

    #[test]
    fn each_outcome_is_said_and_its_result_typed() {
        let ok = Outcome::SignedIn {
            scope: s(&["poppy:read"]),
            requested: s(&["poppy:read", "poppy:write"]),
        };
        let r = ok.result("shop", Some("work")).unwrap();
        assert_eq!(r["outcome"], "signed_in");
        assert_eq!(r["account"], "work");
        assert_eq!(r["scope"], json!(["poppy:read"]));
        assert_eq!(r["requested"], json!(["poppy:read", "poppy:write"]));
        assert_eq!(
            ok.said("shop", Some("work")),
            "Signed in at shop, as the account \"work\", with fewer scopes than asked for: poppy:read of poppy:read poppy:write."
        );
        assert_eq!(
            Outcome::Denied.result("shop", None).unwrap(),
            json!({"company": "shop", "account": null, "outcome": "denied", "signed_in": false})
        );
        assert!(Outcome::Failed("x".into()).result("shop", None).is_err());
        assert!(
            Outcome::Interrupted
                .said("shop", None)
                .contains("agentd restarted")
        );
        assert!(
            Outcome::AccountMismatch
                .said("shop", Some("home"))
                .contains("than the one agentd calls \"home\"; nothing changed")
        );
    }
}
