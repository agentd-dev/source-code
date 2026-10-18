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
//! `url` (§4.3); it is used only there.
//!
//! Renewal is on demand and single-flight: a request that finds its token in
//! the last fifth of its lifetime, or past it, renews first, holding the
//! Session's lock, so concurrent requests wait for one renewal instead of
//! each sending its own. An idle Session is never renewed in the background.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

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
/// Company and where its User ID is kept.
pub struct Env<'a> {
    pub identity: &'a Identity,
    pub verified: &'a Verified,
    pub dialer: &'a Dialer,
    pub users: &'a UserIds,
    pub principal: &'a str,
    pub log: &'a Logger,
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
    /// Bearer tokens, by MCP server `url`.
    bearer: BTreeMap<String, Held>,
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

    fn ctx<'a>(&'a self, env: &'a Env<'a>, user_id: &'a str) -> token::Ctx<'a> {
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
        let mut rec = match st.record.take() {
            Some(r) => r,
            None => env.users.load_or_mint(env.principal, &self.issuer)?,
        };
        let company = &env.verified.company;
        let renewed = match rec.session_id.clone() {
            Some(sid) => match token::request(&self.ctx(env, &rec.user_id), Request::renew(&sid)) {
                Err(TokenError::Refused { code, .. }) if code == err::INVALID_SESSION => {
                    env.log.info(
                        "company.session.ended",
                        json!({"company": company, "reason": err::INVALID_SESSION}),
                    );
                    None
                }
                other => Some(other),
            },
            None => None,
        };
        let (got, started) = match renewed {
            Some(r) => (r, false),
            None => (
                token::request(&self.ctx(env, &rec.user_id), Request::start()),
                true,
            ),
        };
        let got = match got {
            Ok(g) => g,
            Err(e) => {
                st.record = Some(rec);
                return Err(format!("company {company}: {e}"));
            }
        };
        if rec.session_id.as_deref() != Some(got.session_id.as_str()) {
            rec.session_id = Some(got.session_id.clone());
            // Kept before the token is used: a restart renews this Session.
            if let Err(e) = env.users.save(env.principal, &self.issuer, &rec) {
                st.record = Some(rec);
                return Err(format!("company {company}: {e}"));
            }
            // Bearer tokens belong to the Session they were issued for.
            st.bearer.clear();
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
        st.signed_in = got.signed_in;
        st.session = Some(Held::of(&got));
        st.record = Some(rec);
        Ok(())
    }

    /// A Bearer token for the MCP server at `url`: the one held when it is
    /// fresh, else a new one for the Session — which is opened first when
    /// it has no `session_id`, and started again when the Company says it
    /// has ended.
    pub fn bearer(&self, env: &Env<'_>, url: &str) -> Result<String, String> {
        let mut st = self.lock();
        if let Some(h) = st.bearer.get(url).filter(|h| h.fresh()) {
            return Ok(h.token.clone());
        }
        if st
            .record
            .as_ref()
            .and_then(|r| r.session_id.as_ref())
            .is_none()
        {
            self.open_locked(env, &mut st)?;
        }
        let company = &env.verified.company;
        for attempt in 0..2 {
            let rec = st.record.clone().ok_or("no User record")?;
            let sid = rec.session_id.clone().unwrap_or_default();
            match token::request(&self.ctx(env, &rec.user_id), Request::bearer(&sid, url)) {
                Ok(got) => {
                    if got.session_id != sid {
                        return Err(format!(
                            "company {company}: a Bearer token came back for another Session"
                        ));
                    }
                    let token = got.access_token.clone();
                    st.bearer.insert(url.to_string(), Held::of(&got));
                    return Ok(token);
                }
                Err(TokenError::Refused { code, .. })
                    if code == err::INVALID_SESSION && attempt == 0 =>
                {
                    env.log.info(
                        "company.session.ended",
                        json!({"company": company, "reason": err::INVALID_SESSION}),
                    );
                    st.session = None;
                    if let Some(r) = st.record.as_mut() {
                        r.session_id = None;
                    }
                    self.open_locked(env, &mut st)?;
                }
                Err(e) => return Err(format!("company {company}: {e}")),
            }
        }
        Err(format!("company {company}: no Session took a Bearer token"))
    }

    /// The server at `url` refused `token`: it is not used again, so the
    /// next request gets a new one.
    pub fn refused(&self, url: &str, token: &str) {
        let mut st = self.lock();
        if st.bearer.get(url).is_some_and(|h| h.token == token) {
            st.bearer.remove(url);
        }
    }
}
