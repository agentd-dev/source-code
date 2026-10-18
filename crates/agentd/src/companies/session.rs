// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor's Session table (RFC 0047 §5.6): one Session per (User,
//! Company issuer), shared by every task and every interface, and the only
//! place a Session Token or a Bearer token is held — in memory, for its
//! lifetime, never written anywhere.
//!
//! A Session starts signed out with the JWT bearer grant and a DPoP proof
//! (draft §4.2). Its `session_id` is kept with the User ID, so after a
//! restart the same Session is renewed — with this process's new DPoP key,
//! which re-binds its Session Token (§4.3 lets a key change per Session) —
//! and only when the Company says it has ended (`invalid_session`) is a new
//! one started and kept instead. A Bearer token for an MCP server is the
//! Session's renew grant without a proof and with `resource` the server's
//! `url` (§4.3); it is used only there. The DPoP Session Token is what every
//! API and conversation request carries, with a proof of its own signed for
//! it ([`Session::proof`]); an endpoint the Company lists with a `resource`
//! gets the renew grant with a proof and that `resource` instead.
//!
//! While the User's Account Token is held ([`super::account`]) every token
//! is granted on it (§4.8): the Session is renewed, or signed in, with it and
//! its `session_id`, and a Session the Company ended is started again with it
//! and no `session_id` — "a new Session that is already signed in" — so no
//! renewal turns a signed-in Session into a signed-out one behind the User's
//! back. A token the entry's `scopes` admit none of is not used. A token the
//! Company rotates is kept before the response is used, under the Session's
//! lock, which a sign-in and a sign-out take to change the held token too.
//! Only `invalid_grant` for the Account Token says it is no longer good (a
//! disconnect, an expiry, a revocation): it is forgotten
//! (`company.account_token.dropped`) and the same request is made again
//! signed out — the draft's "The Session continues signed out" (§4.8).
//!
//! Renewal is on demand and single-flight: a request that finds its token in
//! the last fifth of its lifetime, or past it, renews first, holding the
//! Session's lock, so concurrent requests wait for one renewal instead of
//! each sending its own. An idle Session is never renewed in the background.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

use super::account::{Account, Accounts};
use super::discover::Verified;
use super::dpop::{DpopKey, Nonces};
use super::http::Dialer;
use super::identity::Identity;
use super::token::{self, Request, TokenError, TokenResponse};
use super::user_id::{UserIds, UserRecord};
use super::wire::error as err;
use crate::obs::log::Logger;

/// A token, when it was issued and for how long.
struct Held {
    token: String,
    issued: Instant,
    lifetime: Duration,
}

impl Held {
    fn of(r: &TokenResponse) -> Held {
        Held {
            token: r.access_token.clone(),
            issued: Instant::now(),
            lifetime: r.expires_in,
        }
    }

    /// Outside the last fifth of its lifetime.
    fn fresh(&self) -> bool {
        self.issued.elapsed() < self.lifetime.mul_f64(0.8)
    }
}

/// What a Session's requests are made with: the identity, the verified
/// Company, where its User ID and Account Token are kept, and the most
/// scopes agentd may use there.
pub struct Env<'a> {
    pub identity: &'a Identity,
    pub verified: &'a Verified,
    pub dialer: &'a Dialer,
    pub users: &'a UserIds,
    /// Where Account Tokens are kept; none when no Company may be signed in
    /// to.
    pub accounts: Option<&'a Accounts>,
    pub principal: &'a str,
    /// `services.<name>.scopes`: a held Account Token is used for no more,
    /// and not at all when they admit none of its scopes ([`narrowed`]).
    pub ceiling: &'a [String],
    pub log: &'a Logger,
}

impl Env<'_> {
    /// The User's Account Token here, if one is held.
    pub fn account(&self, issuer: &str) -> Result<Option<Account>, String> {
        match self.accounts {
            Some(a) => a.load(self.principal, issuer),
            None => Ok(None),
        }
    }
}

/// One Session.
pub struct Session {
    pub issuer: String,
    dpop: DpopKey,
    nonces: Nonces,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    record: Option<UserRecord>,
    session: Option<Held>,
    signed_in: bool,
    /// The account scopes the Session's latest token carries.
    scope: Vec<String>,
    /// Bearer tokens, by MCP server `url`.
    bearer: BTreeMap<String, Held>,
    /// DPoP tokens issued for a `resource` an API or conversation endpoint
    /// is listed with, by that `resource`.
    resource: BTreeMap<String, Held>,
}

impl State {
    /// Every token issued for one `resource`, Bearer or DPoP: each belongs
    /// to the Session as it was signed in, or out, when it was issued.
    fn drop_scoped(&mut self) {
        self.bearer.clear();
        self.resource.clear();
    }
}

impl Session {
    /// A Session at `issuer` with a new DPoP key.
    pub fn new(issuer: &str) -> Result<Session, String> {
        Ok(Session {
            issuer: issuer.to_string(),
            dpop: DpopKey::generate()?,
            nonces: Nonces::default(),
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The token request context, for the User `user_id`.
    pub fn ctx<'a>(&'a self, env: &'a Env<'a>, user_id: &'a str) -> token::Ctx<'a> {
        token::Ctx {
            identity: env.identity,
            issuer: &env.verified.issuer,
            token_endpoint: &env.verified.token_endpoint,
            dialer: env.dialer,
            user_id,
            dpop: &self.dpop,
            nonces: &self.nonces,
        }
    }

    /// The User's record here, read from the store or minted the first time.
    fn record(&self, env: &Env<'_>, st: &mut State) -> Result<UserRecord, String> {
        match st.record.clone() {
            Some(r) => Ok(r),
            None => {
                let r = env.users.load_or_mint(env.principal, &self.issuer)?;
                st.record = Some(r.clone());
                Ok(r)
            }
        }
    }

    /// One token request, `base`: granted on the Account Token while one is
    /// held, else on the assertion. A token the Company rotated is kept
    /// before the answer is used; an `invalid_grant` for the Account Token
    /// forgets it, and the request is made again signed out.
    fn request(
        &self,
        env: &Env<'_>,
        user_id: &str,
        base: Request<'_>,
    ) -> Result<TokenResponse, TokenError> {
        let company = &env.verified.company;
        let account = env.account(&self.issuer).map_err(TokenError::Transport)?;
        // A token the entry now admits none of is not used: the request is
        // made signed out, and the token kept for the User to sign out of.
        let usable = account.and_then(|a| narrowed(&a.scope, env.ceiling).map(|n| (a, n)));
        if let Some((a, narrower)) = usable {
            let req = base.with_account(&a.token, narrower.as_deref());
            match token::request(&self.ctx(env, user_id), req) {
                Ok(got) => {
                    if let Some(rt) = got.refresh_token.as_deref().filter(|rt| *rt != a.token) {
                        let next = Account {
                            token: rt.to_string(),
                            expires_at_ms: expires_at(&got).or(a.expires_at_ms),
                            ..a.clone()
                        };
                        // Kept before the new Session Token is used: the
                        // Company may have spent the old Account Token.
                        if let Some(store) = env.accounts {
                            store
                                .keep(env.principal, &self.issuer, &next)
                                .map_err(|e| {
                                    TokenError::Transport(format!(
                                        "the rotated Account Token could not be kept, so its answer is not used: {e}"
                                    ))
                                })?;
                        }
                        env.log
                            .info("company.account_token.rotated", json!({"company": company}));
                    }
                    return Ok(got);
                }
                Err(e) if e.is(err::INVALID_GRANT) => {
                    if let Some(store) = env.accounts {
                        store
                            .forget(env.principal, &self.issuer)
                            .map_err(TokenError::Transport)?;
                    }
                    env.log.info(
                        "company.account_token.dropped",
                        json!({"company": company, "reason": err::INVALID_GRANT}),
                    );
                    crate::obs::metrics::set_company_signed_in(company, false);
                }
                Err(e) => return Err(e),
            }
        }
        token::request(&self.ctx(env, user_id), base)
    }

    /// The Session open: a DPoP Session Token that is fresh, renewed or
    /// started as needed. Its `session_id`, when it changes, is kept with
    /// the User ID.
    pub fn open(&self, env: &Env<'_>) -> Result<(), String> {
        let mut st = self.lock();
        self.open_locked(env, &mut st)
    }

    fn open_locked(&self, env: &Env<'_>, st: &mut State) -> Result<(), String> {
        if st.session.as_ref().is_some_and(Held::fresh) {
            return Ok(());
        }
        let rec = self.record(env, st)?;
        let company = &env.verified.company;
        let renewed = match rec.session_id.as_deref() {
            Some(sid) => match self.request(env, &rec.user_id, Request::renew(sid)) {
                // Ended — or, with an Account Token, signed in to another
                // account than the token's (§4.4): either way a new Session.
                Err(e) if e.is(err::INVALID_SESSION) || e.is(err::ACCOUNT_MISMATCH) => {
                    env.log.info(
                        "company.session.ended",
                        json!({"company": company, "reason": e.code()}),
                    );
                    None
                }
                other => Some(other),
            },
            None => None,
        };
        let (got, started) = match renewed {
            Some(r) => (r, false),
            None => (self.request(env, &rec.user_id, Request::start()), true),
        };
        let got = got.map_err(|e| format!("company {company}: {e}"))?;
        self.adopt(env, st, &got, started)
    }

    /// Take `got`, a DPoP Session Token, as the Session's: its `session_id`
    /// kept with the User ID before it is used.
    fn adopt(
        &self,
        env: &Env<'_>,
        st: &mut State,
        got: &TokenResponse,
        started: bool,
    ) -> Result<(), String> {
        let company = &env.verified.company;
        let mut rec = self.record(env, st)?;
        if rec.session_id.as_deref() != Some(got.session_id.as_str()) {
            rec.session_id = Some(got.session_id.clone());
            // Kept before the token is used: a restart renews this Session.
            env.users
                .save(env.principal, &self.issuer, &rec)
                .map_err(|e| format!("company {company}: {e}"))?;
            st.record = Some(rec);
            // Tokens for one resource belong to the Session they were issued
            // for.
            st.drop_scoped();
        }
        let line = json!({"company": company, "signed_in": got.signed_in});
        if started {
            env.log.info("company.session.started", line);
        } else {
            env.log.debug("company.session.renewed", line);
        }
        env.log.debug(
            "company.session.id",
            json!({"company": company, "session_id": got.session_id}),
        );
        if st.signed_in != got.signed_in {
            // Tokens issued before reflect the Session as it was (§4.4).
            st.drop_scoped();
        }
        st.signed_in = got.signed_in;
        st.scope = got.scope.clone();
        crate::obs::metrics::set_company_signed_in(company, got.signed_in);
        st.session = Some(Held::of(got));
        Ok(())
    }

    /// A Bearer token for the MCP server at `url`: the one held when it is
    /// fresh, else a new one for the Session — which is opened first when
    /// it has no `session_id`, and started again when the Company says it
    /// has ended.
    pub fn bearer(&self, env: &Env<'_>, url: &str) -> Result<String, String> {
        let mut st = self.lock();
        self.scoped(env, &mut st, url, false)
    }

    /// The DPoP Session Token an API or conversation request carries (§4.3,
    /// §6): the Session's own, opened or renewed as needed — or, for an
    /// endpoint the Company lists with a `resource`, one issued for that
    /// `resource`.
    pub fn dpop_token(&self, env: &Env<'_>, resource: Option<&str>) -> Result<String, String> {
        let mut st = self.lock();
        match resource {
            Some(r) => self.scoped(env, &mut st, r, true),
            None => {
                self.open_locked(env, &mut st)?;
                st.session
                    .as_ref()
                    .map(|h| h.token.clone())
                    .ok_or_else(|| format!("company {}: no Session", env.verified.company))
            }
        }
    }

    /// A DPoP proof for `method` at `url`, with `ath` over `token` and the
    /// origin's nonce, signed now (§4.3).
    pub fn proof(&self, method: &str, url: &str, token: &str) -> Result<String, String> {
        self.dpop
            .proof(method, url, Some(token), self.nonces.get(url).as_deref())
    }

    /// Keep the nonce a response from `url`'s origin carried.
    pub fn learn_nonce(&self, url: &str, nonce: &str) {
        self.nonces.learn(url, nonce);
    }

    /// The token for `resource` — Bearer for an MCP server, or DPoP: the one
    /// held when it is fresh, else a new one for the Session, which is
    /// opened first when it has no `session_id`, and started again when the
    /// Company says it has ended.
    fn scoped(
        &self,
        env: &Env<'_>,
        st: &mut State,
        resource: &str,
        dpop: bool,
    ) -> Result<String, String> {
        let held = if dpop { &st.resource } else { &st.bearer };
        if let Some(h) = held.get(resource).filter(|h| h.fresh()) {
            return Ok(h.token.clone());
        }
        if st
            .record
            .as_ref()
            .and_then(|r| r.session_id.as_ref())
            .is_none()
        {
            self.open_locked(env, st)?;
        }
        let company = &env.verified.company;
        let what = if dpop { "A DPoP" } else { "A Bearer" };
        for attempt in 0..2 {
            let rec = st.record.clone().ok_or("no User record")?;
            let sid = rec.session_id.clone().unwrap_or_default();
            let req = if dpop {
                Request::resource(&sid, resource)
            } else {
                Request::bearer(&sid, resource)
            };
            match self.request(env, &rec.user_id, req) {
                Ok(got) => {
                    if got.session_id != sid {
                        return Err(format!(
                            "company {company}: {what} token for {resource} came back for another Session"
                        ));
                    }
                    if got.signed_in != st.signed_in {
                        // The Session was signed in, or out, since its DPoP
                        // token was issued: that token says otherwise now.
                        st.signed_in = got.signed_in;
                        st.scope = got.scope.clone();
                        st.session = None;
                        st.drop_scoped();
                        crate::obs::metrics::set_company_signed_in(company, got.signed_in);
                    }
                    let token = got.access_token.clone();
                    let into = if dpop {
                        &mut st.resource
                    } else {
                        &mut st.bearer
                    };
                    into.insert(resource.to_string(), Held::of(&got));
                    return Ok(token);
                }
                Err(e)
                    if (e.is(err::INVALID_SESSION) || e.is(err::ACCOUNT_MISMATCH))
                        && attempt == 0 =>
                {
                    env.log.info(
                        "company.session.ended",
                        json!({"company": company, "reason": e.code()}),
                    );
                    st.session = None;
                    if let Some(r) = st.record.as_mut() {
                        r.session_id = None;
                    }
                    self.open_locked(env, st)?;
                }
                Err(e) => return Err(format!("company {company}: {e}")),
            }
        }
        Err(format!(
            "company {company}: no Session took {} token for {resource}",
            what.to_ascii_lowercase()
        ))
    }

    /// The Session's `session_id`, opened first when it has none.
    pub fn session_id(&self, env: &Env<'_>) -> Result<String, String> {
        let mut st = self.lock();
        self.open_locked(env, &mut st)?;
        let rec = self.record(env, &mut st)?;
        rec.session_id
            .ok_or_else(|| format!("company {}: no Session", env.verified.company))
    }

    /// The User's ID here.
    pub fn user_id(&self, env: &Env<'_>) -> Result<String, String> {
        let mut st = self.lock();
        Ok(self.record(env, &mut st)?.user_id)
    }

    /// Renew the Session now with the held Account Token, which signs it
    /// in when it was signed out (§4.8: "With `session_id`, it gets a token
    /// for that Session, signing it in if it was signed out"): whether it is
    /// signed in, and with which scopes.
    pub fn renew_now(&self, env: &Env<'_>) -> Result<(bool, Vec<String>), String> {
        let mut st = self.lock();
        st.session = None;
        self.open_locked(env, &mut st)?;
        Ok((st.signed_in, st.scope.clone()))
    }

    /// A new signed-out Session, never the shared one: what a sign-in to
    /// another account than the held Account Token's starts in (§4.4, "The
    /// User then signs in again in a new Session"). Its `session_id`; the
    /// shared Session takes it only once the sign-in succeeds.
    pub fn fresh_session_id(&self, env: &Env<'_>) -> Result<String, String> {
        let user_id = self.user_id(env)?;
        token::request(&self.ctx(env, &user_id), Request::start())
            .map(|got| got.session_id)
            .map_err(|e| format!("company {}: {e}", env.verified.company))
    }

    /// A sign-in's answer, `got`, signed `got.session_id` in: its Account
    /// Token kept sealed — before anything else of the answer is used — and
    /// its Session Token taken as the Session's, every Bearer token dropped.
    /// The Account Token it replaced, if another was held.
    ///
    /// Under the Session's lock, as every request that may rotate the held
    /// token is: one in flight writes its rotated token back first, and that
    /// is the token replaced — never written over the new one after.
    pub fn signed_in(
        &self,
        env: &Env<'_>,
        got: &TokenResponse,
        kind: &str,
    ) -> Result<Option<Account>, String> {
        let company = &env.verified.company;
        let rt = got.refresh_token.clone().ok_or_else(|| {
            format!("company {company}: the sign-in's answer carries no Account Token")
        })?;
        let store = env.accounts.ok_or_else(|| {
            format!("company {company}: no seal key to keep an Account Token with")
        })?;
        let mut st = self.lock();
        let old = store.load(env.principal, &self.issuer)?;
        store.keep(
            env.principal,
            &self.issuer,
            &Account {
                token: rt.clone(),
                scope: got.scope.clone(),
                expires_at_ms: expires_at(got),
                kind: kind.to_string(),
            },
        )?;
        st.drop_scoped();
        self.adopt(env, &mut st, got, false)?;
        Ok(old.filter(|o| o.token != rt))
    }

    /// Give up the User's Account Token here: taken out of `store` and the
    /// Session signed out — every token held for it dropped, so the next
    /// request renews it signed out. The token it was, for the caller to
    /// revoke. Under the Session's lock, so a renewal in flight writes its
    /// rotated token back first and that is the one taken — none is left
    /// behind good at the Company.
    pub fn take_account(
        &self,
        store: &Accounts,
        principal: &str,
        company: &str,
    ) -> Result<Option<Account>, String> {
        let mut st = self.lock();
        let held = store.load(principal, &self.issuer)?;
        if held.is_some() {
            store.forget(principal, &self.issuer)?;
        }
        st.session = None;
        st.drop_scoped();
        st.signed_in = false;
        st.scope.clear();
        crate::obs::metrics::set_company_signed_in(company, false);
        Ok(held)
    }

    /// Whether the Session is signed in, once it has been opened.
    pub fn is_signed_in(&self) -> Option<bool> {
        let st = self.lock();
        st.session.as_ref().map(|_| st.signed_in)
    }

    /// The (User, Company) pair's own secret, from its record — read from
    /// the store, or minted, the first time. The privacy profile derives
    /// what a Company is sent in place of agentd's correlators from it
    /// (`super::privacy`).
    pub fn secret(&self, users: &UserIds, principal: &str) -> Result<String, String> {
        let mut st = self.lock();
        if st.record.is_none() {
            st.record = Some(users.load_or_mint(principal, &self.issuer)?);
        }
        Ok(st
            .record
            .as_ref()
            .map(|r| r.secret.clone())
            .unwrap_or_default())
    }

    /// The server at `url` refused `token`: it is not used again, so the
    /// next request gets a new one.
    pub fn refused(&self, url: &str, token: &str) {
        let mut st = self.lock();
        if st.bearer.get(url).is_some_and(|h| h.token == token) {
            st.bearer.remove(url);
        }
    }

    /// An API or conversation endpoint refused the DPoP token `token`
    /// (`invalid_token`), or needs the Session signed in with the held
    /// Account Token (`sign_in_required`): it is not used again, so the next
    /// request renews the Session — on the Account Token while one is held.
    pub fn refused_dpop(&self, token: &str) {
        let mut st = self.lock();
        if st.session.as_ref().is_some_and(|h| h.token == token) {
            st.session = None;
        }
        st.resource.retain(|_, h| h.token != token);
    }
}

/// When `got`'s Account Token stops being good, if the Company said — the
/// end of the clock for a lifetime past it.
fn expires_at(got: &TokenResponse) -> Option<u64> {
    got.refresh_token_expires_in.map(|d| {
        crate::auth::cache::now_ms()
            .saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    })
}

/// Whether a request may be made on an Account Token holding `held`, and
/// the `scope` it carries: the scopes of it the entry's `ceiling` admits,
/// when that is fewer (§4.8: "`scope` MAY ask for fewer scopes than the
/// Account Token has"), else none, which asks for every scope it holds. A
/// token the ceiling admits none of is not used at all — nor any, when the
/// entry may sign nobody in and its ceiling is empty.
pub fn narrowed(held: &[String], ceiling: &[String]) -> Option<Option<String>> {
    let kept: Vec<&str> = held
        .iter()
        .filter(|s| ceiling.contains(s))
        .map(String::as_str)
        .collect();
    if ceiling.is_empty() || (kept.is_empty() && !held.is_empty()) {
        return None;
    }
    Some((kept.len() < held.len()).then(|| kept.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held Account Token is used for no more than the entry's scopes: a
    /// token holding more asks for fewer, one within them asks for all it
    /// holds, and one the entry admits none of — or any, when the entry
    /// signs nobody in — is not used.
    #[test]
    fn an_account_token_is_narrowed_to_the_entrys_scopes() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            narrowed(&s(&["poppy:read", "poppy:write"]), &s(&["poppy:read"])),
            Some(Some("poppy:read".into()))
        );
        assert_eq!(
            narrowed(&s(&["poppy:read"]), &s(&["poppy:read", "poppy:write"])),
            Some(None)
        );
        assert_eq!(
            narrowed(&s(&["poppy:read", "poppy:write"]), &s(&["addresses"])),
            None,
            "a token the entry admits none of"
        );
        assert_eq!(narrowed(&s(&["poppy:read"]), &[]), None, "no sign-in");
        assert_eq!(narrowed(&[], &[]), None);
    }

    /// A lifetime past the clock's end ends with it.
    #[test]
    fn an_account_token_lifetime_never_overflows_the_clock() {
        let got = TokenResponse {
            access_token: "t".into(),
            token_type: "DPoP".into(),
            expires_in: Duration::from_secs(60),
            scope: Vec::new(),
            session_id: "s".into(),
            signed_in: true,
            refresh_token: Some("r".into()),
            refresh_token_expires_in: Some(Duration::from_secs(u64::MAX)),
        };
        assert_eq!(expires_at(&got), Some(u64::MAX));
    }
}
