// SPDX-License-Identifier: AGPL-3.0-only
//! **Signing in at the IdP, on the listener origin** (RFC 0044 §5.4): the
//! IdP-federated display-client login (`a2a.login`), and every other
//! authorization-code sign-in a signed-in person starts — enrolling a
//! session, proving a session is the person a device name is bound to, a
//! step-up the IdP asked for.
//!
//! Every one goes the same way:
//!
//! 1. The client is handed a **start link** on the listener origin, single
//!    use and good for [`START_FOR`] — never the IdP's authorization URL.
//! 2. Opening it sets a `__Host-` cookie bound to the sign-in's `state`
//!    (`Secure`, `HttpOnly`, `SameSite=Lax`; a loopback `http` origin is a
//!    secure context for it) and redirects to the IdP with PKCE S256, the
//!    `state` and a `nonce`.
//! 3. The IdP returns the browser to [`CALLBACK_PATH`] on the same origin,
//!    which is taken only with that cookie: an IdP URL forwarded to someone
//!    else fails here, because their browser never opened the start link.
//! 4. The delegation supervisor ([`Federation`]) exchanges the code, checks
//!    RFC 9207's `iss`, verifies the ID token and does what the sign-in is
//!    for, off the listener's threads.
//!
//! For `a2a.login` the client asks at [`LOGIN_AUTHORIZATION_PATH`] for a
//! start link and a request code, and polls the token endpoint with the
//! request code and [`LOGIN_GRANT_TYPE`] until the session is issued: a
//! `source: oidc` session in RFC 0043's one store, `user:oidc:<iss>#<sub>`,
//! carrying the IdP proof it was issued on. The client never sees an IdP
//! token. Only hashes of the request code and the session token are kept.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use super::oauth::{
    Clock, Form, IdpProof, Mint, OAuthError, Reply, Session, SessionKind, Sessions, bucket,
    client_id_ok, hash, ms, take,
};
use super::principals::typed::PrincipalRef;
use crate::a2a::principals::SESSION_TOKEN_PREFIX;
use crate::a2a::serve::limits::SourceLimiter;
use crate::config::settings::Role;

/// Where a display client asks to sign in at the IdP (`a2a.login`).
pub const LOGIN_AUTHORIZATION_PATH: &str = "/oauth2/login_authorization";
/// Where a start link lives: `<START_PATH>/<id>`.
pub const START_PATH: &str = "/oauth2/start";
/// Where the IdP returns the browser.
pub const CALLBACK_PATH: &str = "/oauth2/callback";
/// The token endpoint's grant for a login's request code.
pub use crate::runtime::surface::auth::LOGIN_GRANT_TYPE;
/// The prefix of a login's request code.
pub const REQUEST_PREFIX: &str = "agentd_lq_";
/// The cookie a start link sets, one per sign-in: `<prefix><start id>`.
pub const COOKIE_PREFIX: &str = "__Host-agentd-signin-";
/// How long a start link stays good.
pub const START_FOR: Duration = Duration::from_secs(60);
/// How long a sign-in may take, end to end.
pub const SIGN_IN_FOR: Duration = Duration::from_secs(10 * 60);
/// How often a login's client may poll.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// What `slow_down` adds to a login client's interval.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(2);
/// Sign-ins in flight at once; at the bound the oldest gives way.
pub const PENDING_MAX: usize = 64;
/// Login requests per source: at most this many at once…
pub const AUTHORIZE_BURST: u32 = 5;
/// …and one more per interval after that.
pub const AUTHORIZE_REFILL: Duration = Duration::from_secs(12);

/// What a sign-in is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// A display client signing in (`a2a.login`): its session is issued
    /// once the IdP's answer is verified.
    Login { client_id: String },
    /// Session `sid` enrolling, or proving that it is the person its name
    /// is bound to: the proof lands on that session, and the outcome on the
    /// A2A task `task`.
    Enrol { sid: String, task: String },
    /// A step-up the IdP asked for before consent gate `gate`'s call: a
    /// fresh sign-in of the grant's own person, at least as recent as the
    /// `max_age` asked.
    StepUp { gate: String },
}

impl Purpose {
    fn as_str(&self) -> &'static str {
        match self {
            Purpose::Login { .. } => "login",
            Purpose::Enrol { .. } => "enrol",
            Purpose::StepUp { .. } => "step_up",
        }
    }
}

/// What the IdP's authorization URL is built from.
#[derive(Debug, Clone)]
pub struct Ask {
    /// The `identity.providers` entry.
    pub idp: String,
    pub redirect_uri: String,
    pub state: String,
    pub nonce: String,
    pub challenge: String,
    /// A step-up's `max_age` (sent with `prompt=login`).
    pub max_age: Option<u64>,
}

/// The IdP's answer at the callback, for [`Federation::complete`].
#[derive(Debug, Clone)]
pub struct Answer {
    pub purpose: Purpose,
    pub idp: String,
    pub code: String,
    pub verifier: String,
    pub redirect_uri: String,
    pub nonce: String,
    /// RFC 9207's `iss`, when the callback carried one.
    pub iss: Option<String>,
    pub max_age: Option<u64>,
}

/// A sign-in the delegation supervisor verified.
#[derive(Debug, Clone)]
pub struct Signed {
    /// Who the IdP said the person is.
    pub typed: PrincipalRef,
    pub proof: IdpProof,
    /// What the browser is told: agentd's own words.
    pub said: String,
}

/// The delegation supervisor's half of a sign-in: it reads the IdP's
/// metadata, exchanges the code, verifies the ID token and does what the
/// sign-in is for. Every call may wait on the network, so the listener
/// makes it off its own threads.
pub trait Federation: Send + Sync {
    /// The IdP's authorization URL for `ask`.
    fn authorize_url(&self, ask: &Ask) -> Result<String, String>;
    /// Complete a sign-in. `Err` is what the browser and the asker are told.
    fn complete(&self, answer: Answer) -> Result<Signed, String>;
}

/// The start link a sign-in is begun with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartLink {
    pub uri: String,
    pub expires_in: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Pending,
    /// The callback came; the supervisor is completing it.
    Completing,
    /// A login's session, issued and not yet collected by its client.
    Issued {
        token: String,
        expires_in: u64,
    },
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
struct Flow {
    purpose: Purpose,
    idp: String,
    state: String,
    verifier: String,
    nonce: String,
    cookie: String,
    created_ms: u64,
    start_until_ms: u64,
    until_ms: u64,
    started: bool,
    max_age: Option<u64>,
    /// A login's: the hash of its request code, and its polling.
    request_hash: Option<String>,
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    outcome: Outcome,
}

/// The `a2a.login` side: which IdP people sign in at, and how long their
/// sessions live.
#[derive(Debug, Clone)]
pub struct LoginCfg {
    pub idp: String,
    pub token_ttl: Duration,
}

/// The sign-ins in flight on a listener, by start-link id.
pub struct SignIns {
    clock: Clock,
    mint: Mint,
    login: Option<LoginCfg>,
    federation: OnceLock<Arc<dyn Federation>>,
    flows: Mutex<BTreeMap<String, Flow>>,
    limiter: SourceLimiter,
}

/// What the start link answers: a redirect with its cookie, or a refusal.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    /// The sign-in to send to the IdP: the [`Ask`] to build its URL from,
    /// and the `Set-Cookie` value.
    Go(Box<Ask>, String),
    Gone,
}

/// What the callback found.
#[derive(Debug)]
pub enum Callback {
    /// Hand to [`Federation::complete`]; then [`SignIns::finished`] with
    /// the start id.
    Complete(String, Box<Answer>),
    /// Refused, with the status and the page's text.
    Refused(u16, String),
}

impl PartialEq for Ask {
    fn eq(&self, o: &Ask) -> bool {
        self.state == o.state
    }
}
impl Eq for Ask {}

impl SignIns {
    pub fn new(login: Option<LoginCfg>, clock: Clock, mint: Mint) -> SignIns {
        SignIns {
            clock,
            mint,
            login,
            federation: OnceLock::new(),
            flows: Mutex::new(BTreeMap::new()),
            limiter: bucket(AUTHORIZE_BURST, AUTHORIZE_REFILL),
        }
    }

    /// Whether `a2a.login` is configured: the login endpoint and grant are
    /// served.
    pub fn login(&self) -> Option<&LoginCfg> {
        self.login.as_ref()
    }

    /// The delegation supervisor that completes sign-ins. Set once, when it
    /// is built; until then every sign-in is refused as unavailable.
    pub fn set_federation(&self, f: Arc<dyn Federation>) {
        let _ = self.federation.set(f);
    }

    fn federation(&self) -> Option<Arc<dyn Federation>> {
        self.federation.get().cloned()
    }

    fn table(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Flow>> {
        self.flows.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn random(&self, bytes: usize) -> Result<String, String> {
        (self.mint)(bytes).map_err(|e| format!("no OS randomness for the sign-in: {e}"))
    }

    /// A new sign-in for `purpose` at `idp`: its start-link id, and the
    /// request code a login's client polls with.
    fn new_flow(
        &self,
        purpose: Purpose,
        idp: &str,
        max_age: Option<u64>,
        login: bool,
    ) -> Result<(String, Option<String>), String> {
        if self.federation().is_none() {
            return Err("no IdP sign-in is served here".into());
        }
        let id = self.random(16)?;
        let verifier = crate::b64::url_nopad(self.random(32)?.as_bytes());
        let (state, nonce, cookie) = (self.random(16)?, self.random(16)?, self.random(16)?);
        let request = if login {
            Some(format!("{REQUEST_PREFIX}{}", self.random(32)?))
        } else {
            None
        };
        let now = (self.clock)();
        let mut t = self.table();
        t.retain(|_, f| now < f.until_ms.saturating_add(ms(SIGN_IN_FOR)));
        if t.len() >= PENDING_MAX
            && let Some(oldest) = t
                .iter()
                .filter(|(_, f)| f.outcome == Outcome::Pending)
                .min_by_key(|(_, f)| f.created_ms)
                .map(|(k, _)| k.clone())
        {
            t.remove(&oldest);
        }
        t.insert(
            id.clone(),
            Flow {
                purpose,
                idp: idp.to_string(),
                state,
                verifier,
                nonce,
                cookie,
                created_ms: now,
                start_until_ms: now.saturating_add(ms(START_FOR)),
                until_ms: now.saturating_add(ms(SIGN_IN_FOR)),
                started: false,
                max_age,
                request_hash: request.as_deref().map(hash),
                interval_ms: ms(POLL_INTERVAL),
                last_poll_ms: None,
                outcome: Outcome::Pending,
            },
        );
        Ok((id, request))
    }

    /// Begin a sign-in the runtime asks for — an enrolment, a step-up — at
    /// `idp`: the start link the person opens, on `issuer`.
    pub fn begin(
        &self,
        issuer: &str,
        purpose: Purpose,
        idp: &str,
        max_age: Option<u64>,
    ) -> Result<StartLink, String> {
        let (id, _) = self.new_flow(purpose, idp, max_age, false)?;
        Ok(StartLink {
            uri: crate::runtime::surface::auth::join(issuer, &format!("{START_PATH}/{id}")),
            expires_in: START_FOR.as_secs(),
        })
    }

    /// `POST /oauth2/login_authorization`: a display client asks to sign in
    /// at `a2a.login`'s IdP. Answered with the start link to open and the
    /// request code to poll the token endpoint with.
    pub fn login_authorization(
        &self,
        issuer: Option<&str>,
        content_type: Option<&str>,
        body: &[u8],
        peer: Option<IpAddr>,
    ) -> Reply {
        let Some(login) = &self.login else {
            return Reply::not_found();
        };
        if let Err(retry) = take(&self.limiter, peer) {
            return Reply::limited(retry, "too many sign-ins from this source; retry later");
        }
        let form = match super::oauth::parse_form(content_type, body) {
            Ok(f) => f,
            Err(e) => return Reply::error(e),
        };
        let Some(client_id) = form.get("client_id").filter(|c| client_id_ok(c)) else {
            return Reply::error(OAuthError::invalid_request(format!(
                "client_id is required: 1-{} characters of A-Z a-z 0-9 . _ -",
                super::oauth::CLIENT_ID_MAX
            )));
        };
        let Some(issuer) = issuer else {
            return Reply::error(OAuthError::unavailable("the listener is still starting"));
        };
        let purpose = Purpose::Login {
            client_id: client_id.to_string(),
        };
        let (id, request) = match self.new_flow(purpose, &login.idp, None, true) {
            Ok(x) => x,
            Err(e) => return Reply::error(OAuthError::unavailable(e)),
        };
        Reply::json(
            200,
            json!({
                "request_code": request,
                "start_uri": crate::runtime::surface::auth::join(issuer, &format!("{START_PATH}/{id}")),
                "start_expires_in": START_FOR.as_secs(),
                "expires_in": SIGN_IN_FOR.as_secs(),
                "interval": POLL_INTERVAL.as_secs(),
            }),
        )
    }

    /// `GET /oauth2/start/<id>`: once, within [`START_FOR`] — the cookie,
    /// and where the browser goes.
    pub fn start(&self, id: &str, issuer: &str) -> Start {
        let now = (self.clock)();
        let mut t = self.table();
        let Some(f) = t.get_mut(id) else {
            return Start::Gone;
        };
        if f.started || now >= f.start_until_ms || f.outcome != Outcome::Pending {
            return Start::Gone;
        }
        f.started = true;
        let challenge = crate::b64::url_nopad(&crate::sha::sha256(f.verifier.as_bytes()));
        let cookie = format!(
            "{COOKIE_PREFIX}{id}={}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={}",
            f.cookie,
            SIGN_IN_FOR.as_secs()
        );
        Start::Go(
            Box::new(Ask {
                idp: f.idp.clone(),
                redirect_uri: crate::runtime::surface::auth::join(issuer, CALLBACK_PATH),
                state: f.state.clone(),
                nonce: f.nonce.clone(),
                challenge,
                max_age: f.max_age,
            }),
            cookie,
        )
    }

    /// The IdP URL for a started sign-in, or why there is none: asked of
    /// the federation off the listener's threads.
    pub fn authorize_url(&self, ask: &Ask) -> Result<String, String> {
        self.federation()
            .ok_or_else(|| "no IdP sign-in is served here".to_string())?
            .authorize_url(ask)
    }

    /// What the federation makes of a callback's answer: asked off the
    /// listener's threads.
    pub fn complete(&self, answer: Answer) -> Result<Signed, String> {
        self.federation()
            .ok_or_else(|| "no IdP sign-in is served here".to_string())?
            .complete(answer)
    }

    /// `GET /oauth2/callback`: the browser back from the IdP, with the
    /// `Cookie` header it sent. Taken only with the cookie the start link
    /// set for the sign-in its `state` names; a callback without it fails
    /// that sign-in, so the forwarding attack gets nothing.
    pub fn callback(&self, query: &str, cookies: Option<&str>, issuer: &str) -> Callback {
        let q: BTreeMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| {
                (
                    k.to_string(),
                    super::oauth::decode(v.as_bytes()).unwrap_or_default(),
                )
            })
            .collect();
        let state = q.get("state").cloned().unwrap_or_default();
        let now = (self.clock)();
        let mut t = self.table();
        let Some((id, f)) = t.iter_mut().find(|(_, f)| {
            !state.is_empty() && crate::sha::ct_eq(f.state.as_bytes(), state.as_bytes())
        }) else {
            return Callback::Refused(400, "This sign-in is unknown or has ended.".into());
        };
        if f.outcome != Outcome::Pending || !f.started || now >= f.until_ms {
            return Callback::Refused(410, "This sign-in has ended.".into());
        }
        let name = format!("{COOKIE_PREFIX}{id}");
        let presented = cookies.and_then(|c| {
            c.split(';')
                .filter_map(|p| p.trim().split_once('='))
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        });
        if !presented.is_some_and(|v| crate::sha::ct_eq(v.as_bytes(), f.cookie.as_bytes())) {
            f.outcome = Outcome::Failed(
                "the sign-in came back to a browser that did not start it: a forwarded sign-in is refused".into(),
            );
            return Callback::Refused(
                403,
                "This sign-in did not come back to the browser that started it, so it is refused."
                    .into(),
            );
        }
        if let Some(e) = q.get("error") {
            f.outcome = Outcome::Failed(format!("the IdP refused the sign-in: {e}"));
            return Callback::Refused(
                200,
                "The sign-in was refused. You may close this window.".into(),
            );
        }
        let Some(code) = q.get("code").cloned() else {
            f.outcome = Outcome::Failed("the IdP's answer carried no code".into());
            return Callback::Refused(400, "The sign-in carried no code.".into());
        };
        f.outcome = Outcome::Completing;
        Callback::Complete(
            id.clone(),
            Box::new(Answer {
                purpose: f.purpose.clone(),
                idp: f.idp.clone(),
                code,
                verifier: f.verifier.clone(),
                redirect_uri: crate::runtime::surface::auth::join(issuer, CALLBACK_PATH),
                nonce: f.nonce.clone(),
                iss: q.get("iss").cloned(),
                max_age: f.max_age,
            }),
        )
    }

    /// Complete the sign-in `id` with what the federation made of it: a
    /// login's session issued into `sessions`. What the browser is told,
    /// and the session issued, if one was.
    pub fn finished(
        &self,
        id: &str,
        result: Result<Signed, String>,
        sessions: &Sessions,
    ) -> (String, Option<Session>) {
        let now = (self.clock)();
        let mut t = self.table();
        let Some(f) = t.get_mut(id) else {
            return ("This sign-in has ended.".into(), None);
        };
        match (result, &f.purpose) {
            (Err(e), _) => {
                f.outcome = Outcome::Failed(e.clone());
                (
                    format!("The sign-in was refused: {e}. You may close this window."),
                    None,
                )
            }
            (Ok(signed), Purpose::Login { client_id }) => {
                let Some(login) = &self.login else {
                    f.outcome = Outcome::Failed("a2a.login is not served".into());
                    return ("The sign-in was refused.".into(), None);
                };
                let minted = (|| {
                    let sid = format!(
                        "{}{}",
                        super::oauth::LOGIN_SID_PREFIX,
                        (self.mint)(super::oauth::SID_BYTES)?
                    );
                    let token = format!(
                        "{SESSION_TOKEN_PREFIX}{}",
                        (self.mint)(super::oauth::SECRET_BYTES)?
                    );
                    Ok::<_, std::io::Error>((sid, token))
                })();
                let Ok((sid, token)) = minted else {
                    f.outcome = Outcome::Failed("no session could be issued right now".into());
                    return (
                        "The sign-in could not be completed; sign in again.".into(),
                        None,
                    );
                };
                let session = Session {
                    sid,
                    kind: SessionKind::Login,
                    name: None,
                    role: Role::User,
                    principal: signed.typed.display(),
                    client_id: client_id.clone(),
                    created_ms: now,
                    expires_ms: Some(now.saturating_add(ms(login.token_ttl))),
                    approved_by: format!("idp:{}", f.idp),
                    approved_rule: None,
                    rate: None,
                    typed: signed.typed.clone(),
                    proof: Some(signed.proof.clone()),
                };
                sessions.insert(&token, session.clone());
                f.outcome = Outcome::Issued {
                    token,
                    expires_in: login.token_ttl.as_secs(),
                };
                (signed.said, Some(session))
            }
            (Ok(signed), _) => {
                f.outcome = Outcome::Done;
                (signed.said, None)
            }
        }
    }

    /// Whether `id` names a sign-in still being completed: a callback for
    /// it raced another.
    pub fn purpose_of(&self, id: &str) -> Option<&'static str> {
        self.table().get(id).map(|f| f.purpose.as_str())
    }

    /// The token endpoint's login grant: the session a client's sign-in
    /// was issued, once, to the client it was asked for.
    pub fn login_token(&self, form: &Form) -> Reply {
        let (Some(request), Some(client_id)) = (form.get("request_code"), form.get("client_id"))
        else {
            return Reply::error(OAuthError::invalid_request(
                "the login grant takes request_code and client_id",
            ));
        };
        let now = (self.clock)();
        let key = hash(request);
        let mut t = self.table();
        let Some((id, f)) = t.iter_mut().find(|(_, f)| {
            f.request_hash.as_deref() == Some(key.as_str())
                && matches!(&f.purpose, Purpose::Login { client_id: c } if c == client_id)
        }) else {
            return Reply::error(OAuthError::new(
                "invalid_grant",
                "the request code is unknown, used, or not this client's",
            ));
        };
        let id = id.clone();
        if now >= f.until_ms && f.outcome != Outcome::Completing {
            t.remove(&id);
            return Reply::error(OAuthError::new(
                "expired_token",
                "the sign-in was not completed in time; start again",
            ));
        }
        if let Some(last) = f.last_poll_ms
            && now.saturating_sub(last) < f.interval_ms
        {
            f.interval_ms = f.interval_ms.saturating_add(ms(SLOW_DOWN_STEP));
            f.last_poll_ms = Some(now);
            return Reply::error(OAuthError::new(
                "slow_down",
                format!("poll at most every {}s", f.interval_ms / 1000),
            ));
        }
        f.last_poll_ms = Some(now);
        match f.outcome.clone() {
            Outcome::Pending | Outcome::Completing => Reply::error(OAuthError::new(
                "authorization_pending",
                "open the start link and sign in at your organization's identity provider",
            )),
            Outcome::Failed(why) => {
                t.remove(&id);
                Reply::error(OAuthError::new("access_denied", why))
            }
            Outcome::Done => {
                t.remove(&id);
                Reply::error(OAuthError::new(
                    "invalid_grant",
                    "this sign-in issued no session",
                ))
            }
            Outcome::Issued { token, expires_in } => {
                t.remove(&id);
                Reply::json(
                    200,
                    json!({"access_token": token, "token_type": "Bearer",
                           "expires_in": expires_in, "scope": "user"}),
                )
            }
        }
    }

    /// What `auth.sessions`-style status shows of the sign-ins in flight:
    /// how many, by purpose. Never a code, a cookie or a state.
    pub fn waiting(&self) -> Value {
        let t = self.table();
        let mut by: BTreeMap<&'static str, usize> = BTreeMap::new();
        for f in t.values().filter(|f| f.outcome == Outcome::Pending) {
            *by.entry(f.purpose.as_str()).or_default() += 1;
        }
        json!(by)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fed;
    impl Federation for Fed {
        fn authorize_url(&self, ask: &Ask) -> Result<String, String> {
            Ok(format!("https://idp.example/authorize?state={}", ask.state))
        }
        fn complete(&self, a: Answer) -> Result<Signed, String> {
            Ok(Signed {
                typed: PrincipalRef {
                    issuer: "https://idp.example".into(),
                    ..PrincipalRef::local(super::super::principals::typed::Source::Oidc, "00u1")
                },
                proof: IdpProof {
                    idp: a.idp,
                    iss: "https://idp.example".into(),
                    sub: "00u1".into(),
                    at_ms: 1,
                },
                said: "Signed in.".into(),
            })
        }
    }

    fn signins(now: Arc<AtomicU64>) -> SignIns {
        let n = Arc::new(AtomicU64::new(0));
        let s = SignIns::new(
            Some(LoginCfg {
                idp: "okta".into(),
                token_ttl: Duration::from_secs(3600),
            }),
            Arc::new(move || now.load(Ordering::SeqCst)),
            Arc::new(move |b| {
                Ok(format!(
                    "{:0w$x}",
                    n.fetch_add(1, Ordering::SeqCst),
                    w = b * 2
                ))
            }),
        );
        s.set_federation(Arc::new(Fed));
        s
    }

    fn form(pairs: &str) -> Form {
        super::super::oauth::parse_form(Some("application/x-www-form-urlencoded"), pairs.as_bytes())
            .unwrap()
    }

    fn body(r: &Reply) -> Value {
        match &r.body {
            super::super::oauth::ReplyBody::Json(v) => v.clone(),
            _ => Value::Null,
        }
    }

    /// The whole login: a start link once, its cookie, the callback taken
    /// only with it, the session issued once to the client that asked — a
    /// `source: oidc` session carrying the IdP's proof.
    #[test]
    fn a_login_issues_an_oidc_session_once_through_the_start_links_cookie() {
        let now = Arc::new(AtomicU64::new(1_000));
        let s = signins(Arc::clone(&now));
        let sessions = Sessions::new(Arc::new(|| 1_000));
        let issuer = "http://127.0.0.1:9";
        let r = s.login_authorization(
            Some(issuer),
            Some("application/x-www-form-urlencoded"),
            b"client_id=tui",
            None,
        );
        assert_eq!(r.status, 200);
        let b = body(&r);
        let request = b["request_code"].as_str().unwrap().to_string();
        let start = b["start_uri"].as_str().unwrap().to_string();
        let id = start.rsplit('/').next().unwrap().to_string();
        let poll = |now_ms: u64| {
            now.store(now_ms, Ordering::SeqCst);
            body(&s.login_token(&form(&format!("request_code={request}&client_id=tui"))))
        };
        assert_eq!(poll(1_000)["error"], "authorization_pending");
        let Start::Go(ask, cookie) = s.start(&id, issuer) else {
            panic!("the start link goes once")
        };
        assert_eq!(s.start(&id, issuer), Start::Gone, "single use");
        assert!(
            cookie.starts_with(&format!("{COOKIE_PREFIX}{id}=")),
            "{cookie}"
        );
        assert!(
            cookie.contains("Secure; HttpOnly; SameSite=Lax"),
            "{cookie}"
        );
        assert_eq!(ask.redirect_uri, "http://127.0.0.1:9/oauth2/callback");
        let jar = cookie.split(';').next().unwrap().to_string();
        let query = format!("code=c1&state={}", ask.state);
        // Without the cookie — the forwarded IdP URL — it is refused, and
        // the sign-in is over.
        let Callback::Refused(403, _) = s.callback(&query, None, issuer) else {
            panic!("a callback with no cookie is refused")
        };
        assert_eq!(poll(5_000)["error"], "access_denied");
        // A second sign-in, completed in the browser that started it.
        let r = s.login_authorization(
            Some(issuer),
            Some("application/x-www-form-urlencoded"),
            b"client_id=tui",
            None,
        );
        let b = body(&r);
        let request = b["request_code"].as_str().unwrap().to_string();
        let id = b["start_uri"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        let Start::Go(ask, cookie) = s.start(&id, issuer) else {
            panic!()
        };
        let jar2 = cookie.split(';').next().unwrap().to_string();
        assert_ne!(jar, jar2);
        let query = format!("code=c2&state={}", ask.state);
        let Callback::Complete(fid, answer) =
            s.callback(&query, Some(&format!("other=1; {jar2}")), issuer)
        else {
            panic!("the callback with the cookie completes")
        };
        assert_eq!(answer.code, "c2");
        let (said, session) = s.finished(&fid, Fed.complete(*answer), &sessions);
        assert_eq!(said, "Signed in.");
        let session = session.expect("a session");
        assert_eq!(session.kind, SessionKind::Login);
        assert_eq!(session.principal, "user:oidc:https://idp.example#00u1");
        assert_eq!(session.proof.as_ref().map(|p| p.sub.as_str()), Some("00u1"));
        now.store(9_000, Ordering::SeqCst);
        let got = body(&s.login_token(&form(&format!("request_code={request}&client_id=other"))));
        assert_eq!(
            got["error"], "invalid_grant",
            "another client's code: {got}"
        );
        let got = body(&s.login_token(&form(&format!("request_code={request}&client_id=tui"))));
        let token = got["access_token"].as_str().expect("the token").to_string();
        assert!(token.starts_with(SESSION_TOKEN_PREFIX));
        assert_eq!(got["scope"], "user");
        let again = body(&s.login_token(&form(&format!("request_code={request}&client_id=tui"))));
        assert_eq!(again["error"], "invalid_grant", "collected once");
        match crate::a2a::principals::SessionVerifier::verify(&sessions, &token) {
            crate::a2a::principals::SessionCheck::Valid(p) => {
                assert_eq!(p.id, "user:oidc:https://idp.example#00u1");
                assert_eq!(
                    p.typed.map(|t| t.source),
                    Some(crate::a2a::principals::typed::Source::Oidc)
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A start link opened after its minute is gone, and an unknown one is
    /// too; a sign-in begun for an enrolment issues no session.
    #[test]
    fn a_start_link_lives_a_minute_and_an_enrolment_issues_no_session() {
        let now = Arc::new(AtomicU64::new(0));
        let s = signins(Arc::clone(&now));
        let sessions = Sessions::new(Arc::new(|| 0));
        let link = s
            .begin(
                "https://a.example",
                Purpose::Enrol {
                    sid: "ds_1".into(),
                    task: "t".into(),
                },
                "okta",
                None,
            )
            .unwrap();
        let id = link.uri.rsplit('/').next().unwrap().to_string();
        assert!(link.uri.starts_with("https://a.example/oauth2/start/"));
        now.store(ms(START_FOR) + 1, Ordering::SeqCst);
        assert_eq!(s.start(&id, "https://a.example"), Start::Gone);
        assert_eq!(s.start("nope", "https://a.example"), Start::Gone);
        let link = s
            .begin(
                "https://a.example",
                Purpose::Enrol {
                    sid: "ds_1".into(),
                    task: "t".into(),
                },
                "okta",
                None,
            )
            .unwrap();
        let id = link.uri.rsplit('/').next().unwrap().to_string();
        let Start::Go(ask, cookie) = s.start(&id, "https://a.example") else {
            panic!()
        };
        let jar = cookie.split(';').next().unwrap().to_string();
        let Callback::Complete(fid, a) = s.callback(
            &format!("code=x&state={}", ask.state),
            Some(&jar),
            "https://a.example",
        ) else {
            panic!()
        };
        let (_, session) = s.finished(&fid, Fed.complete(*a), &sessions);
        assert!(session.is_none(), "an enrolment issues no session");
        assert!(sessions.list().is_empty());
    }
}
