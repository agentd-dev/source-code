// SPDX-License-Identifier: AGPL-3.0-only
//! **The authorization server on the listener origin**: the OAuth 2.0 device
//! authorization grant (RFC 8628), token revocation (RFC 7009) and the
//! server's metadata (RFC 8414), and the sessions they issue.
//!
//! A person signs a browser or a terminal in without the daemon's bearer ever
//! being copied into it: the client asks for a code, shows it, and polls; an
//! operator approves the code with `auth.device.approve {user_code, as}`; the
//! next poll is answered with a session token. The token is presented as a
//! Bearer like any other, and [`Sessions`] — the resolver's session verifier
//! — names its caller.
//!
//! What replaced pairing, and why it is shaped this way:
//!
//! * **Nothing is armed until a client asks.** A code exists only while a
//!   device waits on it, it is single-use, and it expires with `code_ttl`.
//!   The code a person reads is not the one a guesser needs: redeeming takes
//!   the 256-bit `device_code` the client holds, and the 8-character user
//!   code only names the request an operator is approving.
//! * **Every source is limited on its own.** A per-source bucket and a
//!   per-source cap on pending requests keep one address from locking out
//!   another, under a global cap that bounds the whole table.
//! * **A session is a name, not a code.** The approver chooses the name the
//!   device acts as (`user:<name>`), so the principal is somebody an operator
//!   decided on, several sessions of one name share what that name owns, and
//!   each session still has its own id to be revoked by.
//!
//! Only hashes are kept: the store holds the SHA-256 of every device code and
//! token, never the value, and neither is ever logged. Everything is held in
//! memory, so a restart revokes every session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::a2a::Principal;
use crate::a2a::principals::{SESSION_TOKEN_PREFIX, SessionCheck, SessionVerifier};
use crate::a2a::serve::limits::{SourceKey, SourceLimiter};
use crate::config::v2::{self, DeviceScope, Role};

// ---- the constants ----------------------------------------------------------

/// RFC 8628 §3.1: where a client asks for a code.
pub const DEVICE_AUTHORIZATION_PATH: &str = "/oauth2/device_authorization";
/// RFC 6749 §3.2: where a client redeems its device code.
pub const TOKEN_PATH: &str = "/oauth2/token";
/// RFC 7009: where a client ends its session.
pub const REVOKE_PATH: &str = "/oauth2/revoke";
/// The verification page a code's person is pointed at, unless
/// `a2a.device_grant.verification_uri` names another.
pub const VERIFICATION_PATH: &str = "/oauth2/device";
/// RFC 8414 §3: the metadata, at the issuer's root.
pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
/// RFC 8628 §3.4: the device grant's `grant_type`.
pub const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The user code's alphabet: consonants only, so no code spells a word and
/// none is misread (`0`/`O`, `1`/`I`), as RFC 8628 §6.1 suggests.
pub const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
/// The user code's length, before the display dash: 20^8 ≈ 2^34.6.
pub const USER_CODE_LEN: usize = 8;
/// Bytes of entropy in a device code and a session token: 256 bits.
pub const SECRET_BYTES: usize = 32;
/// Bytes of entropy in a session id. Not a secret — it is listed to the
/// operator and written to the audit trail — only a handle to revoke by.
pub const SID_BYTES: usize = 8;
/// The prefix of a device session's id.
pub const DEVICE_SID_PREFIX: &str = "ds_";

/// RFC 8628 §3.2: how often a client may poll.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 §3.5: what `slow_down` adds to a client's interval.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// The largest form body any endpoint reads. Every legal request is a few
/// hundred bytes; the cap is what a stranger may make the listener buffer.
pub const FORM_MAX: usize = 4096;
/// The longest `client_id` a device may give.
pub const CLIENT_ID_MAX: usize = 64;

/// Device authorizations per source: at most this many at once…
pub const AUTHORIZE_BURST: u32 = 5;
/// …and one more per interval after that.
pub const AUTHORIZE_REFILL: Duration = Duration::from_secs(12);
/// Token polls per source (the device grant only): a client polling every
/// 5 s uses a fraction of it, and a guesser of 256-bit codes gets nothing
/// from the rest.
pub const TOKEN_BURST: u32 = 30;
/// …refilled at one a second.
pub const TOKEN_REFILL: Duration = Duration::from_secs(1);
/// Codes one source may have waiting at once.
pub const PENDING_PER_SOURCE: usize = 4;
/// Codes waiting at once across every source — what bounds the operator's
/// `auth.device.pending` and this table, however many addresses ask.
pub const PENDING_GLOBAL: usize = 64;

/// The current time in epoch milliseconds. Injected so the tests can run a
/// ten-minute code lifetime, or a five-second poll interval, in no time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// `n` bytes of OS randomness as hex ([`crate::sec::random::hex_token`]).
/// Injected so a test can make the entropy source fail and watch every mint
/// refuse cleanly rather than issue something weaker — or abort the daemon.
pub type Mint = Arc<dyn Fn(usize) -> std::io::Result<String> + Send + Sync>;

/// The real clock.
pub fn system_clock() -> Clock {
    Arc::new(crate::state::now_ms)
}

/// The real entropy source.
pub fn os_mint() -> Mint {
    Arc::new(crate::sec::random::hex_token)
}

fn hash(secret: &str) -> String {
    crate::sha::sha256_hex(secret.as_bytes())
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ---- sessions ---------------------------------------------------------------

/// How a session was issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Through the device grant, approved by an operator.
    Device,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Device => "device",
        }
    }
}

/// One signed-in session.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The handle it is listed, audited and revoked by (`ds_<16 hex>`).
    pub sid: String,
    pub kind: SessionKind,
    /// The name it was approved as.
    pub name: Option<String>,
    pub role: Role,
    /// Who it acts as: `user:<name>`, or `operator`.
    pub principal: String,
    pub client_id: String,
    pub created_ms: u64,
    /// `None`: it ends only when revoked.
    pub expires_ms: Option<u64>,
    /// The principal id that approved it.
    pub approved_by: String,
    /// The rule that named the approver, when one with an id did.
    pub approved_rule: Option<String>,
    /// `a2a.device_grant.rate`: every session of one principal id shares its
    /// one bucket, because the bucket is keyed by the id.
    pub rate: Option<String>,
}

impl Session {
    fn expired(&self, now: u64) -> bool {
        self.expires_ms.is_some_and(|e| now >= e)
    }

    /// The principal a request presenting this session's token is.
    pub fn as_principal(&self) -> Principal {
        Principal {
            id: self.principal.clone(),
            role: self.role,
            // The role's defaults, and nothing beyond them: a session is who
            // the approver named, never a set of grants somebody typed.
            grants: if self.role == Role::Operator {
                vec!["*".into()]
            } else {
                Vec::new()
            },
            rate: self.rate.clone(),
            budget: None,
            labels: Default::default(),
            session: Some(self.sid.clone()),
        }
    }

    /// The row `auth.sessions` lists.
    pub fn view(&self) -> Value {
        let mut v = json!({
            "sid": self.sid,
            "kind": self.kind.as_str(),
            "principal": self.principal,
            "role": role_name(self.role),
            "client_id": self.client_id,
            "created_at": self.created_ms,
            "expires_at": self.expires_ms,
            "approved_by": self.approved_by,
        });
        if let Some(n) = &self.name {
            v["name"] = json!(n);
        }
        if let Some(r) = &self.approved_rule {
            v["approved_rule"] = json!(r);
        }
        v
    }

    /// The `auth.session.revoked` log line: the sid and the name, never the
    /// token.
    pub fn revoked_line(&self, reason: &str) -> Value {
        json!({"sid": self.sid, "name": self.name, "principal": self.principal, "reason": reason})
    }

    /// The feed's `auth` event for this session's end.
    pub fn revoked_event(&self) -> Value {
        json!({"event": "revoked", "sid": self.sid, "name": self.name, "principal": self.principal})
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Operator => "operator",
        Role::User => "user",
        Role::Agent => "agent",
        Role::Anonymous => "anonymous",
    }
}

/// Which sessions a revocation ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoke {
    /// Exactly one session — its siblings of the same name go on.
    Sid(String),
    /// Every session approved under a name.
    Name(String),
    All,
}

/// The sessions a listener has issued, by the SHA-256 of their tokens.
///
/// Exists on every TCP listener, device grant or not: `auth.sessions` lists
/// it and the resolver routes every `agentd_at_` bearer to it, and both need
/// it whether or not anything can currently issue into it.
pub struct Sessions {
    clock: Clock,
    table: Mutex<Table>,
}

#[derive(Debug, Default)]
struct Table {
    /// token hash → session.
    by_token: HashMap<String, Session>,
    /// sid → token hash.
    by_sid: HashMap<String, String>,
    /// approval name → its sids.
    by_name: BTreeMap<String, BTreeSet<String>>,
}

impl Table {
    fn remove_sid(&mut self, sid: &str) -> Option<Session> {
        let token = self.by_sid.remove(sid)?;
        let s = self.by_token.remove(&token)?;
        if let Some(name) = &s.name
            && let Some(sids) = self.by_name.get_mut(name)
        {
            sids.remove(sid);
            if sids.is_empty() {
                self.by_name.remove(name);
            }
        }
        Some(s)
    }

    fn prune(&mut self, now: u64) {
        let dead: Vec<String> = self
            .by_token
            .values()
            .filter(|s| s.expired(now))
            .map(|s| s.sid.clone())
            .collect();
        for sid in dead {
            self.remove_sid(&sid);
        }
    }
}

impl Sessions {
    pub fn new(clock: Clock) -> Sessions {
        Sessions {
            clock,
            table: Mutex::new(Table::default()),
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hold `session` under `token` (only the token's hash is kept).
    pub fn insert(&self, token: &str, session: Session) {
        let now = (self.clock)();
        let mut t = self.table();
        t.prune(now);
        let h = hash(token);
        t.by_sid.insert(session.sid.clone(), h.clone());
        if let Some(name) = &session.name {
            t.by_name
                .entry(name.clone())
                .or_default()
                .insert(session.sid.clone());
        }
        t.by_token.insert(h, session);
    }

    /// Whether session `sid` may still be served.
    pub fn alive(&self, sid: &str) -> bool {
        let now = (self.clock)();
        let t = self.table();
        t.by_sid
            .get(sid)
            .and_then(|h| t.by_token.get(h))
            .is_some_and(|s| !s.expired(now))
    }

    /// The live sessions, oldest first.
    pub fn list(&self) -> Vec<Session> {
        let now = (self.clock)();
        let mut t = self.table();
        t.prune(now);
        let mut out: Vec<Session> = t.by_token.values().cloned().collect();
        out.sort_by(|a, b| (a.created_ms, &a.sid).cmp(&(b.created_ms, &b.sid)));
        out
    }

    /// End the sessions `which` names; returns them.
    pub fn revoke(&self, which: &Revoke) -> Vec<Session> {
        let mut t = self.table();
        let sids: Vec<String> = match which {
            Revoke::Sid(sid) => vec![sid.clone()],
            Revoke::Name(name) => t
                .by_name
                .get(name)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            Revoke::All => t.by_sid.keys().cloned().collect(),
        };
        let mut out: Vec<Session> = sids.iter().filter_map(|s| t.remove_sid(s)).collect();
        out.sort_by(|a, b| a.sid.cmp(&b.sid));
        out
    }

    /// End the session `token` belongs to (RFC 7009), if any.
    pub fn revoke_token(&self, token: &str) -> Option<Session> {
        let mut t = self.table();
        let sid = t.by_token.get(&hash(token))?.sid.clone();
        t.remove_sid(&sid)
    }

    /// The listener's liveness hook: for a caller holding a session, the check
    /// that answers whether THAT session — its sid, not its principal — is
    /// still alive. Revoking one of two sessions approved as `alice` ends the
    /// streams of that one, and the other's go on.
    pub fn liveness(sessions: Arc<Sessions>) -> crate::a2a::serve::Liveness {
        Arc::new(move |p: &Principal| {
            let sid = p.session.clone()?;
            let sessions = Arc::clone(&sessions);
            Some(Arc::new(move || sessions.alive(&sid)) as crate::a2a::serve::LivenessCheck)
        })
    }
}

impl SessionVerifier for Sessions {
    fn verify(&self, token: &str) -> SessionCheck {
        let now = (self.clock)();
        let mut t = self.table();
        let h = hash(token);
        match t.by_token.get(&h) {
            Some(s) if !s.expired(now) => SessionCheck::Valid(s.as_principal()),
            Some(s) => {
                let sid = s.sid.clone();
                t.remove_sid(&sid);
                SessionCheck::Invalid
            }
            None => SessionCheck::Invalid,
        }
    }
}

// ---- the form rules ---------------------------------------------------------

/// An OAuth error answer (RFC 6749 §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub status: u16,
    pub error: &'static str,
    pub description: String,
}

impl OAuthError {
    pub fn new(error: &'static str, description: impl Into<String>) -> OAuthError {
        OAuthError {
            status: 400,
            error,
            description: description.into(),
        }
    }

    fn invalid_request(description: impl Into<String>) -> OAuthError {
        OAuthError::new("invalid_request", description)
    }

    /// The endpoint cannot issue right now: OS randomness failed, or the
    /// issuer is not yet known. RFC 6749 §4.1.2.1's `temporarily_unavailable`,
    /// at 503 — never a panic, which under `panic = "abort"` would end every
    /// session along with the request.
    fn unavailable(description: impl Into<String>) -> OAuthError {
        OAuthError {
            status: 503,
            error: "temporarily_unavailable",
            description: description.into(),
        }
    }

    fn body(&self) -> Value {
        json!({"error": self.error, "error_description": self.description})
    }
}

/// A form body, read by the rules every endpoint shares.
#[derive(Debug, Default)]
pub struct Form(HashMap<String, String>);

impl Form {
    /// A parameter's value. An empty value is absent (RFC 6749 §3.1: "Parameters
    /// sent without a value MUST be treated as if they were omitted").
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// Read a form: `application/x-www-form-urlencoded` only, at most
/// [`FORM_MAX`] bytes, percent-decoding to valid UTF-8. A parameter given
/// twice is refused (RFC 6749 §3.1 — which of two `client_id`s is meant is not
/// a guess to make), and one this server does not know is ignored, as §3.2
/// says it must be: a newer client's `resource` or `audience` is not a
/// malformed request.
pub fn parse_form(content_type: Option<&str>, body: &[u8]) -> Result<Form, OAuthError> {
    let form_type = content_type
        .and_then(|c| c.split(';').next())
        .is_some_and(|t| {
            t.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !form_type {
        return Err(OAuthError::invalid_request(
            "the body must be application/x-www-form-urlencoded",
        ));
    }
    if body.len() > FORM_MAX {
        return Err(OAuthError::invalid_request(format!(
            "the body is over {FORM_MAX} bytes"
        )));
    }
    let mut out = HashMap::new();
    let mut seen = BTreeSet::new();
    for pair in body.split(|b| *b == b'&').filter(|p| !p.is_empty()) {
        let (k, v) = match pair.iter().position(|b| *b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &[][..]),
        };
        let k = decode(k)?;
        let v = decode(v)?;
        if !seen.insert(k.clone()) {
            return Err(OAuthError::invalid_request(format!(
                "the parameter {k:?} is given more than once"
            )));
        }
        if !v.is_empty() {
            out.insert(k, v);
        }
    }
    Ok(Form(out))
}

/// One percent-encoded form component (`+` is a space).
fn decode(raw: &[u8]) -> Result<String, OAuthError> {
    let bad = || OAuthError::invalid_request("the body is not valid form encoding");
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = raw.get(i + 1..i + 3).ok_or_else(bad)?;
                let hex = std::str::from_utf8(hex).map_err(|_| bad())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| bad())
}

/// A `client_id`: 1–64 characters of `[A-Za-z0-9._-]`. It is shown to the
/// operator deciding on the code, so nothing in it can be a control character
/// or pass for other text.
fn client_id_ok(id: &str) -> bool {
    (1..=CLIENT_ID_MAX).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

// ---- the user code ----------------------------------------------------------

/// A user code as typed: case, spaces and the dash ignored. `None` when what
/// is left is not a code of this alphabet at all.
pub fn normalize_user_code(typed: &str) -> Option<String> {
    let code: String = typed
        .chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == USER_CODE_LEN && code.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)))
        .then_some(code)
}

/// A normalized code as a person reads it: `BCDF-GHJK`.
pub fn display_user_code(code: &str) -> String {
    let (a, b) = code.split_at(code.len().min(USER_CODE_LEN / 2));
    format!("{a}-{b}")
}

/// A fresh user code, from OS randomness by rejection sampling: a byte is
/// used only below 240 (12 × 20), so every letter is equally likely.
fn mint_user_code(mint: &Mint) -> std::io::Result<String> {
    let limit = (256 / USER_CODE_ALPHABET.len() * USER_CODE_ALPHABET.len()) as u8;
    let mut code = String::with_capacity(USER_CODE_LEN);
    // Each draw keeps about 94% of 32 bytes; needing a second is already
    // rare, and a third is a broken entropy source, not bad luck.
    for _ in 0..4 {
        let hex = mint(SECRET_BYTES)?;
        for pair in hex.as_bytes().chunks(2) {
            let Some(b) = std::str::from_utf8(pair)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            else {
                continue;
            };
            if b < limit {
                code.push(USER_CODE_ALPHABET[usize::from(b) % USER_CODE_ALPHABET.len()] as char);
                if code.len() == USER_CODE_LEN {
                    return Ok(code);
                }
            }
        }
    }
    Err(std::io::Error::other(
        "the entropy source returned too few usable bytes",
    ))
}

// ---- the device grant -------------------------------------------------------

/// A device authorization waiting on its approver, or approved and waiting to
/// be redeemed.
#[derive(Debug, Clone)]
struct Authorization {
    user_code: String,
    client_id: String,
    requested: DeviceScope,
    peer: Option<IpAddr>,
    source: Option<SourceKey>,
    requested_ms: u64,
    expires_ms: u64,
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    decision: Decision,
}

#[derive(Debug, Clone, PartialEq)]
enum Decision {
    Pending,
    Approved(Approval),
    Denied,
}

/// An operator's approval, recorded on the authorization until redeemed.
#[derive(Debug, Clone, PartialEq)]
pub struct Approval {
    /// The name the device acts as.
    pub name: String,
    pub scope: DeviceScope,
    /// The approver's principal id.
    pub approved_by: String,
    /// The rule that named the approver, when one with an id did.
    pub rule: Option<String>,
}

/// A pending authorization, as the approval op sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingView {
    pub user_code: String,
    pub client_id: String,
    pub requested: DeviceScope,
    pub peer: Option<IpAddr>,
    pub requested_ms: u64,
    pub expires_ms: u64,
}

impl PendingView {
    pub fn view(&self) -> Value {
        json!({
            "user_code": display_user_code(&self.user_code),
            "client_id": self.client_id,
            "scope": self.requested.as_str(),
            "peer": self.peer.map(|p| p.to_string()),
            "requested_at": self.requested_ms,
            "expires_at": self.expires_ms,
        })
    }
}

impl Authorization {
    fn expired(&self, now: u64) -> bool {
        now >= self.expires_ms
    }

    fn view(&self) -> PendingView {
        PendingView {
            user_code: self.user_code.clone(),
            client_id: self.client_id.clone(),
            requested: self.requested,
            peer: self.peer,
            requested_ms: self.requested_ms,
            expires_ms: self.expires_ms,
        }
    }

    /// Whether it holds one of the bounded slots: it is live and nobody has
    /// refused it. A denied request frees its slot at once, so denying a
    /// flood is what an operator does to make room.
    fn occupies(&self, now: u64) -> bool {
        !self.expired(now) && self.decision != Decision::Denied
    }
}

/// The device grant's state: the configured rules and the authorizations in
/// flight, keyed by the SHA-256 of their device codes.
pub struct DeviceGrant {
    scopes: Vec<DeviceScope>,
    token_ttl: Duration,
    code_ttl: Duration,
    rate: Option<String>,
    verification_uri: Option<String>,
    clock: Clock,
    mint: Mint,
    pending: Mutex<HashMap<String, Authorization>>,
}

impl DeviceGrant {
    pub fn new(cfg: &v2::DeviceGrant, clock: Clock, mint: Mint) -> DeviceGrant {
        DeviceGrant {
            scopes: cfg.scopes.clone(),
            token_ttl: cfg.token_ttl(),
            code_ttl: cfg.code_ttl(),
            rate: cfg.rate.clone(),
            verification_uri: cfg.verification_uri.clone(),
            clock,
            mint,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<String, Authorization>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop what has been expired long enough that no client is still asking
    /// about it: kept one more code lifetime so a late poll is told
    /// `expired_token` rather than that it never existed.
    fn prune(&self, table: &mut HashMap<String, Authorization>, now: u64) {
        let grace = ms(self.code_ttl);
        table.retain(|_, a| now < a.expires_ms.saturating_add(grace));
    }

    /// The configured scopes.
    pub fn scopes(&self) -> &[DeviceScope] {
        &self.scopes
    }

    /// The scope an approval grants: the one requested unless the approver
    /// names another. Refused when it is not configured, when it is wider than
    /// what the client asked for, and when it is `operator` without the
    /// approver saying so — an operator session takes two explicit choices,
    /// the client's request and the approver's `scope: operator`.
    pub fn grant_scope(
        &self,
        requested: DeviceScope,
        asked: Option<&str>,
    ) -> Result<DeviceScope, String> {
        let scope = match asked {
            Some(s) => DeviceScope::parse(s)?,
            None if requested == DeviceScope::Operator => {
                return Err(
                    "this device asked for operator scope: approve it with scope: operator to \
                     grant that, or deny it"
                        .into(),
                );
            }
            None => requested,
        };
        if !self.scopes.contains(&scope) {
            return Err(format!(
                "scope {} is not one a2a.device_grant.scopes allows",
                scope.as_str()
            ));
        }
        if scope == DeviceScope::Operator && requested != DeviceScope::Operator {
            return Err(format!(
                "this device asked for {} scope; an approval may narrow a request, never widen it",
                requested.as_str()
            ));
        }
        Ok(scope)
    }

    /// The authorizations waiting on an operator, oldest first.
    pub fn pending(&self) -> Vec<PendingView> {
        let now = (self.clock)();
        let mut t = self.table();
        self.prune(&mut t, now);
        let mut out: Vec<PendingView> = t
            .values()
            .filter(|a| !a.expired(now) && a.decision == Decision::Pending)
            .map(Authorization::view)
            .collect();
        out.sort_by(|a, b| (a.requested_ms, &a.user_code).cmp(&(b.requested_ms, &b.user_code)));
        out
    }

    /// The pending authorization a typed user code names.
    pub fn find(&self, typed: &str) -> Result<PendingView, String> {
        let code =
            normalize_user_code(typed).ok_or_else(|| format!("{typed:?} is not a device code"))?;
        let now = (self.clock)();
        let t = self.table();
        t.values()
            .find(|a| a.user_code == code && !a.expired(now) && a.decision == Decision::Pending)
            .map(Authorization::view)
            .ok_or_else(|| {
                format!(
                    "no device sign-in is waiting on {}",
                    display_user_code(&code)
                )
            })
    }

    /// Record `approval` on the pending authorization `typed` names. Its next
    /// poll is answered with a session.
    pub fn approve(&self, typed: &str, approval: Approval) -> Result<PendingView, String> {
        let view = self.find(typed)?;
        let mut t = self.table();
        let a = t
            .values_mut()
            .find(|a| a.user_code == view.user_code && a.decision == Decision::Pending)
            .ok_or_else(|| "the device sign-in is no longer waiting".to_string())?;
        a.decision = Decision::Approved(approval);
        Ok(view)
    }

    /// Refuse the pending authorization `typed` names, or every one when
    /// `None`; returns the codes refused.
    pub fn deny(&self, typed: Option<&str>) -> Result<Vec<String>, String> {
        let code = match typed {
            Some(t) => {
                Some(normalize_user_code(t).ok_or_else(|| format!("{t:?} is not a device code"))?)
            }
            None => None,
        };
        let now = (self.clock)();
        let mut t = self.table();
        let mut out = Vec::new();
        for a in t.values_mut() {
            if a.decision == Decision::Pending
                && !a.expired(now)
                && code.as_deref().is_none_or(|c| c == a.user_code)
            {
                a.decision = Decision::Denied;
                out.push(a.user_code.clone());
            }
        }
        if let Some(c) = &code
            && out.is_empty()
        {
            return Err(format!(
                "no device sign-in is waiting on {}",
                display_user_code(c)
            ));
        }
        out.sort();
        Ok(out)
    }
}

// ---- the authority ----------------------------------------------------------

/// What an endpoint answered, before it is HTTP: the listener adds the CORS
/// grant and `Cache-Control: no-store`, and publishes the notices.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: ReplyBody,
    /// `Retry-After`, on a 429.
    pub retry_after: Option<u64>,
    /// What the operator should hear about: pushed to the feed as `auth`
    /// events, and a revocation logged.
    pub notices: Vec<Notice>,
}

#[derive(Debug, PartialEq)]
pub enum ReplyBody {
    Json(Value),
    /// RFC 7009 §2.2: a revocation answers 200 with no body.
    Empty,
    Text(&'static str),
}

/// Something that happened at an endpoint the operator should hear about.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// A device asked for a code: the feed's `auth` `pending` event.
    Pending(Value),
    /// A session ended at `/oauth2/revoke`.
    Revoked(Session),
}

impl Reply {
    fn json(status: u16, body: Value) -> Reply {
        Reply {
            status,
            body: ReplyBody::Json(body),
            retry_after: None,
            notices: Vec::new(),
        }
    }

    fn error(e: OAuthError) -> Reply {
        Reply::json(e.status, e.body())
    }

    fn limited(retry_after: u64, why: &str) -> Reply {
        let mut r = Reply::json(
            429,
            json!({"error": "temporarily_unavailable", "error_description": why}),
        );
        r.retry_after = Some(retry_after);
        r
    }
}

/// The authorization server: the device grant, its per-source limits, and
/// the sessions it issues into — which it borrows, because they exist on the
/// listener with or without it.
pub struct Authority {
    pub device: DeviceGrant,
    /// Device authorizations per source.
    limiter: SourceLimiter,
    /// Device-code polls per source.
    token_limiter: SourceLimiter,
    sessions: Arc<Sessions>,
    /// The listener origin, once the bind has settled it (a `:0` port is not
    /// known before).
    issuer: OnceLock<String>,
}

/// A [`SourceLimiter`] used as a bucket: every request counts, and one more
/// than `burst` puts the source over. The limiter's own rule is "over once
/// the count EXCEEDS the burst", which counts failures; admitting `burst`
/// requests means an allowance of `burst - 1` there.
fn bucket(burst: u32, refill: Duration) -> SourceLimiter {
    SourceLimiter::new(
        burst.saturating_sub(1),
        refill,
        crate::a2a::serve::limits::SOURCE_ENTRIES,
    )
}

fn take(limiter: &SourceLimiter, peer: Option<IpAddr>) -> Result<(), u64> {
    let Some(ip) = peer else {
        return Ok(());
    };
    if let Some(retry) = limiter.over(ip) {
        return Err(retry);
    }
    limiter.failed(ip);
    Ok(())
}

impl Authority {
    pub fn new(device: DeviceGrant, sessions: Arc<Sessions>) -> Authority {
        Authority {
            device,
            limiter: bucket(AUTHORIZE_BURST, AUTHORIZE_REFILL),
            token_limiter: bucket(TOKEN_BURST, TOKEN_REFILL),
            sessions,
            issuer: OnceLock::new(),
        }
    }

    /// Settle the issuer: the advertised origin, with no trailing slash.
    pub fn set_issuer(&self, origin: &str) {
        let _ = self.issuer.set(origin.trim_end_matches('/').to_string());
    }

    pub fn issuer(&self) -> Option<&str> {
        self.issuer.get().map(String::as_str)
    }

    pub fn sessions(&self) -> &Arc<Sessions> {
        &self.sessions
    }

    fn endpoint(&self, path: &str) -> Result<String, OAuthError> {
        self.issuer()
            .map(|i| crate::runtime::surface::auth::join(i, path))
            .ok_or_else(|| OAuthError::unavailable("the listener is still starting"))
    }

    /// `POST /oauth2/device_authorization` (RFC 8628 §3.1–3.2).
    pub fn device_authorization(
        &self,
        content_type: Option<&str>,
        body: &[u8],
        peer: Option<IpAddr>,
    ) -> Reply {
        // Every request is counted, before a byte of it means anything: a
        // flood of junk costs its source the same as a flood of real asks.
        if let Err(retry) = take(&self.limiter, peer) {
            return Reply::limited(
                retry,
                "too many device authorizations from this source; retry later",
            );
        }
        let form = match parse_form(content_type, body) {
            Ok(f) => f,
            Err(e) => return Reply::error(e),
        };
        let Some(client_id) = form.get("client_id").filter(|c| client_id_ok(c)) else {
            return Reply::error(OAuthError::invalid_request(format!(
                "client_id is required: 1-{CLIENT_ID_MAX} characters of A-Z a-z 0-9 . _ -"
            )));
        };
        let requested = match form.get("scope") {
            None => DeviceScope::User,
            Some(s) => match DeviceScope::parse(s) {
                Ok(scope) => scope,
                Err(e) => return Reply::error(OAuthError::new("invalid_scope", e)),
            },
        };
        if !self.device.scopes.contains(&requested) {
            return Reply::error(OAuthError::new(
                "invalid_scope",
                format!("scope {} is not offered here", requested.as_str()),
            ));
        }
        let verification_uri = match &self.device.verification_uri {
            Some(u) => u.clone(),
            None => match self.endpoint(VERIFICATION_PATH) {
                Ok(u) => u,
                Err(e) => return Reply::error(e),
            },
        };

        let now = (self.device.clock)();
        let source = peer.map(SourceKey::of);
        let mut t = self.device.table();
        self.device.prune(&mut t, now);
        let live = t.values().filter(|a| a.occupies(now));
        let (mine, all) = live.fold((0, 0), |(m, a), x| {
            (
                m + usize::from(source.is_some() && x.source == source),
                a + 1,
            )
        });
        if mine >= PENDING_PER_SOURCE {
            return Reply::limited(
                AUTHORIZE_REFILL.as_secs(),
                "this source already has as many sign-ins waiting as it may",
            );
        }
        if all >= PENDING_GLOBAL {
            return Reply::limited(
                AUTHORIZE_REFILL.as_secs(),
                "too many sign-ins are waiting; retry later",
            );
        }
        let minted = (|| {
            let device_code = (self.device.mint)(SECRET_BYTES)?;
            // A user code is a name for the request, unique while it waits.
            for _ in 0..8 {
                let user_code = mint_user_code(&self.device.mint)?;
                if !t.values().any(|a| a.user_code == user_code) {
                    return Ok((device_code, user_code));
                }
            }
            Err(std::io::Error::other("no free user code"))
        })();
        let (device_code, user_code) = match minted {
            Ok(m) => m,
            Err(_) => {
                return Reply::error(OAuthError::unavailable(
                    "no code could be issued right now; retry later",
                ));
            }
        };
        let expires_ms = now.saturating_add(ms(self.device.code_ttl));
        t.insert(
            hash(&device_code),
            Authorization {
                user_code: user_code.clone(),
                client_id: client_id.to_string(),
                requested,
                peer,
                source,
                requested_ms: now,
                expires_ms,
                interval_ms: ms(POLL_INTERVAL),
                last_poll_ms: None,
                decision: Decision::Pending,
            },
        );
        let shown = display_user_code(&user_code);
        let mut r = Reply::json(
            200,
            json!({
                "device_code": device_code,
                "user_code": shown,
                "verification_uri": verification_uri,
                "expires_in": self.device.code_ttl.as_secs(),
                "interval": POLL_INTERVAL.as_secs(),
            }),
        );
        r.notices.push(Notice::Pending(json!({
            "event": "pending",
            "user_code": shown,
            "client_id": client_id,
            "scope": requested.as_str(),
            "peer": peer.map(|p| p.to_string()),
        })));
        r
    }

    /// `POST /oauth2/token` with the device-code grant (RFC 8628 §3.4–3.5).
    ///
    /// The refusals come in a fixed order — `invalid_request`,
    /// `unsupported_grant_type`, `invalid_grant`, `expired_token`,
    /// `slow_down`, `authorization_pending`, `access_denied` — so a client
    /// polling too fast is told so whatever the approver has decided.
    pub fn token(&self, content_type: Option<&str>, body: &[u8], peer: Option<IpAddr>) -> Reply {
        let form = match parse_form(content_type, body) {
            Ok(f) => f,
            Err(e) => return Reply::error(e),
        };
        let Some(grant_type) = form.get("grant_type") else {
            return Reply::error(OAuthError::invalid_request("grant_type is required"));
        };
        if grant_type != DEVICE_CODE_GRANT {
            return Reply::error(OAuthError::new(
                "unsupported_grant_type",
                format!("this server issues tokens for {DEVICE_CODE_GRANT} only"),
            ));
        }
        let (Some(device_code), Some(client_id)) = (form.get("device_code"), form.get("client_id"))
        else {
            return Reply::error(OAuthError::invalid_request(
                "device_code and client_id are required",
            ));
        };
        if let Err(retry) = take(&self.token_limiter, peer) {
            return Reply::limited(retry, "too many token requests from this source");
        }

        let now = (self.device.clock)();
        let key = hash(device_code);
        let mut t = self.device.table();
        self.device.prune(&mut t, now);
        let Some(a) = t.get_mut(&key).filter(|a| a.client_id == client_id) else {
            return Reply::error(OAuthError::new(
                "invalid_grant",
                "the device code is unknown, already used, or not this client's",
            ));
        };
        if a.expired(now) {
            t.remove(&key);
            return Reply::error(OAuthError::new(
                "expired_token",
                "the device code expired before it was approved; start again",
            ));
        }
        if let Some(last) = a.last_poll_ms
            && now.saturating_sub(last) < a.interval_ms
        {
            a.interval_ms = a.interval_ms.saturating_add(ms(SLOW_DOWN_STEP));
            a.last_poll_ms = Some(now);
            return Reply::error(OAuthError::new(
                "slow_down",
                format!("poll at most every {}s", a.interval_ms / 1000),
            ));
        }
        let approval = match &a.decision {
            Decision::Pending => {
                a.last_poll_ms = Some(now);
                return Reply::error(OAuthError::new(
                    "authorization_pending",
                    "an operator has not approved this code yet",
                ));
            }
            Decision::Denied => {
                a.last_poll_ms = Some(now);
                return Reply::error(OAuthError::new(
                    "access_denied",
                    "an operator denied this sign-in",
                ));
            }
            Decision::Approved(approval) => approval.clone(),
        };
        // Mint before anything is recorded: on failure the approval stands
        // and the code is not burned, so the client's next poll can succeed.
        let minted = (|| {
            let sid = format!("{DEVICE_SID_PREFIX}{}", (self.device.mint)(SID_BYTES)?);
            let token = format!(
                "{SESSION_TOKEN_PREFIX}{}",
                (self.device.mint)(SECRET_BYTES)?
            );
            Ok::<_, std::io::Error>((sid, token))
        })();
        let Ok((sid, token)) = minted else {
            return Reply::error(OAuthError::unavailable(
                "no session could be issued right now; poll again",
            ));
        };
        let a = t.remove(&key).expect("held under the same lock");
        drop(t);
        let (role, principal) = match approval.scope {
            DeviceScope::Operator => (Role::Operator, "operator".to_string()),
            DeviceScope::User => (Role::User, format!("user:{}", approval.name)),
        };
        self.sessions.insert(
            &token,
            Session {
                sid,
                kind: SessionKind::Device,
                name: Some(approval.name),
                role,
                principal,
                client_id: a.client_id,
                created_ms: now,
                expires_ms: Some(now.saturating_add(ms(self.device.token_ttl))),
                approved_by: approval.approved_by,
                approved_rule: approval.rule,
                rate: self.device.rate.clone(),
            },
        );
        Reply::json(
            200,
            json!({
                "access_token": token,
                "token_type": "Bearer",
                "expires_in": self.device.token_ttl.as_secs(),
                "scope": approval.scope.as_str(),
            }),
        )
    }

    /// `POST /oauth2/revoke` (RFC 7009): end the session a token belongs to.
    /// Always 200 with no body for a well-formed request — a token that
    /// names no session is already as revoked as it can be, and saying which
    /// tokens exist would be an oracle.
    pub fn revoke(&self, content_type: Option<&str>, body: &[u8]) -> Reply {
        revoke_with(&self.sessions, content_type, body)
    }

    /// `GET /.well-known/oauth-authorization-server` (RFC 8414).
    pub fn metadata(&self) -> Reply {
        let Some(issuer) = self.issuer() else {
            return Reply::error(OAuthError::unavailable("the listener is still starting"));
        };
        let at = |p| crate::runtime::surface::auth::join(issuer, p);
        Reply::json(
            200,
            json!({
                "issuer": issuer,
                "device_authorization_endpoint": at(DEVICE_AUTHORIZATION_PATH),
                "token_endpoint": at(TOKEN_PATH),
                "revocation_endpoint": at(REVOKE_PATH),
                "grant_types_supported": [DEVICE_CODE_GRANT],
                "response_types_supported": [],
                "token_endpoint_auth_methods_supported": ["none"],
                "revocation_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": self.device.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            }),
        )
    }

    /// `GET /oauth2/device`: where a code's person is sent. Fixed text: the
    /// approval is an operator's op, not a page a stranger can submit.
    pub fn verification(&self) -> Reply {
        Reply {
            status: 200,
            body: ReplyBody::Text(VERIFICATION_TEXT),
            retry_after: None,
            notices: Vec::new(),
        }
    }
}

/// The verification page's text.
pub const VERIFICATION_TEXT: &str = "This agent signs devices in with the OAuth 2.0 device \
authorization grant. Give the code your device shows to an operator of this agent; they approve \
it with `auth.device.approve {user_code, as}`, and your device signs itself in.\n";

/// RFC 7009 against `sessions`.
fn revoke_with(sessions: &Sessions, content_type: Option<&str>, body: &[u8]) -> Reply {
    let form = match parse_form(content_type, body) {
        Ok(f) => f,
        Err(e) => return Reply::error(e),
    };
    // `token_type_hint` and `client_id` are accepted and ignored: there is one
    // kind of token here, and a public client's id proves nothing.
    let Some(token) = form.get("token") else {
        return Reply::error(OAuthError::invalid_request("token is required"));
    };
    let mut r = Reply {
        status: 200,
        body: ReplyBody::Empty,
        retry_after: None,
        notices: Vec::new(),
    };
    if let Some(s) = sessions.revoke_token(token) {
        r.notices.push(Notice::Revoked(s));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const FORM: Option<&str> = Some("application/x-www-form-urlencoded");

    struct Fixture {
        now: Arc<AtomicU64>,
        auth: Authority,
    }

    impl Fixture {
        fn new(cfg: serde_json::Value) -> Fixture {
            Fixture::with_mint(cfg, os_mint())
        }

        fn with_mint(cfg: serde_json::Value, mint: Mint) -> Fixture {
            let now = Arc::new(AtomicU64::new(1_000_000));
            let t = Arc::clone(&now);
            let clock: Clock = Arc::new(move || t.load(Ordering::SeqCst));
            let cfg: v2::DeviceGrant = serde_json::from_value(cfg).unwrap();
            let sessions = Arc::new(Sessions::new(Arc::clone(&clock)));
            let auth = Authority::new(DeviceGrant::new(&cfg, clock, mint), sessions);
            auth.set_issuer("https://agent.example:8443");
            Fixture { now, auth }
        }

        fn advance(&self, d: Duration) {
            self.now.fetch_add(ms(d), Ordering::SeqCst);
        }

        fn authorize_from(&self, peer: &str, body: &str) -> Reply {
            self.auth
                .device_authorization(FORM, body.as_bytes(), Some(peer.parse().unwrap()))
        }

        /// A code for `client`, from `peer`: `(device_code, user_code)`.
        fn code(&self, peer: &str, body: &str) -> (String, String) {
            let r = self.authorize_from(peer, body);
            let v = json_of(&r);
            assert_eq!(r.status, 200, "{v}");
            (
                v["device_code"].as_str().unwrap().to_string(),
                v["user_code"].as_str().unwrap().to_string(),
            )
        }

        fn poll(&self, device_code: &str, client: &str) -> Reply {
            let body = format!(
                "grant_type={}&device_code={device_code}&client_id={client}",
                DEVICE_CODE_GRANT.replace(':', "%3A")
            );
            self.auth
                .token(FORM, body.as_bytes(), Some("10.0.0.1".parse().unwrap()))
        }

        fn approve(&self, user_code: &str, name: &str, scope: Option<&str>) {
            let view = self.auth.device.find(user_code).unwrap();
            let scope = self.auth.device.grant_scope(view.requested, scope).unwrap();
            self.auth
                .device
                .approve(
                    user_code,
                    Approval {
                        name: name.into(),
                        scope,
                        approved_by: "operator".into(),
                        rule: None,
                    },
                )
                .unwrap();
        }

        /// Approve, wait out the interval and redeem: the session token.
        fn session(&self, peer: &str, name: &str) -> String {
            let (dc, uc) = self.code(peer, "client_id=cli");
            self.approve(&uc, name, None);
            let r = self.poll(&dc, "cli");
            let v = json_of(&r);
            assert_eq!(r.status, 200, "{v}");
            v["access_token"].as_str().unwrap().to_string()
        }
    }

    fn json_of(r: &Reply) -> Value {
        match &r.body {
            ReplyBody::Json(v) => v.clone(),
            other => panic!("not JSON: {other:?}"),
        }
    }

    fn error_of(r: &Reply) -> String {
        json_of(r)["error"].as_str().unwrap_or_default().to_string()
    }

    fn principal_of(auth: &Authority, token: &str) -> Principal {
        match auth.sessions().verify(token) {
            SessionCheck::Valid(p) => p,
            SessionCheck::Invalid => panic!("the token is not a live session"),
        }
    }

    #[test]
    fn user_code_is_8_chars_of_the_base20_alphabet_and_normalises_input() {
        let f = Fixture::new(json!({"enabled": true}));
        let mut seen = BTreeSet::new();
        for i in 0..40 {
            let (_, shown) = f.code(&format!("10.1.{i}.1"), "client_id=cli");
            assert_eq!(shown.len(), USER_CODE_LEN + 1, "{shown}");
            assert_eq!(&shown[4..5], "-", "{shown}");
            let bare = shown.replace('-', "");
            assert!(
                bare.bytes().all(|b| USER_CODE_ALPHABET.contains(&b)),
                "{shown}"
            );
            seen.insert(bare);
            // Four per source are all that may wait; start each on a new one.
            f.auth.device.deny(Some(&shown)).unwrap();
        }
        assert!(seen.len() > 35, "codes repeat: {seen:?}");
        assert_eq!(USER_CODE_ALPHABET.len(), 20);
        assert!(!USER_CODE_ALPHABET.iter().any(|c| b"AEIOUY01".contains(c)));
        // Typed any way a person types it, it is the same code.
        for typed in ["bcdf-ghjk", "BCDFGHJK", " bcdf ghjk ", "Bc-Df-Gh-Jk"] {
            assert_eq!(
                normalize_user_code(typed).as_deref(),
                Some("BCDFGHJK"),
                "{typed:?}"
            );
        }
        for bad in ["BCDF-GHJ", "BCDF-GHJKL", "ABCD-EFGH", "BCDF-GHJ1", ""] {
            assert_eq!(normalize_user_code(bad), None, "{bad:?}");
        }
        let (_, shown) = f.code("10.9.9.9", "client_id=cli");
        let lower = shown.to_ascii_lowercase().replace('-', " ");
        assert_eq!(
            f.auth.device.find(&lower).unwrap().user_code,
            shown.replace('-', "")
        );
    }

    #[test]
    fn device_codes_and_tokens_carry_256_bits_and_are_stored_hashed() {
        let f = Fixture::new(json!({"enabled": true}));
        let (dc, uc) = f.code("10.0.0.1", "client_id=cli");
        assert_eq!(dc.len(), 64);
        assert!(
            dc.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let pending = format!("{:?}", f.auth.device.table());
        assert!(
            !pending.contains(&dc),
            "the device code is kept in the clear"
        );
        assert!(pending.contains(&hash(&dc)));
        f.approve(&uc, "alice", None);
        let r = f.poll(&dc, "cli");
        let v = json_of(&r);
        let token = v["access_token"].as_str().unwrap();
        let hex = token
            .strip_prefix(SESSION_TOKEN_PREFIX)
            .expect("the prefix");
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(v["token_type"], "Bearer");
        assert!(v.get("refresh_token").is_none(), "no refresh tokens");
        let table = format!("{:?}", f.auth.sessions().table());
        assert!(!table.contains(token), "the token is kept in the clear");
        assert!(!table.contains(hex));
        let p = principal_of(&f.auth, token);
        let sid = p.session.unwrap();
        assert!(
            sid.starts_with(DEVICE_SID_PREFIX) && sid.len() == 3 + 16,
            "{sid}"
        );
    }

    #[test]
    fn device_code_is_single_use() {
        let f = Fixture::new(json!({"enabled": true}));
        let (dc, uc) = f.code("10.0.0.1", "client_id=cli");
        f.approve(&uc, "alice", None);
        assert_eq!(f.poll(&dc, "cli").status, 200);
        f.advance(Duration::from_secs(10));
        let r = f.poll(&dc, "cli");
        assert_eq!((r.status, error_of(&r)), (400, "invalid_grant".to_string()));
    }

    #[test]
    fn poll_states_map_to_rfc8628_errors() {
        let f = Fixture::new(json!({"enabled": true, "code_ttl": "2m"}));
        let (dc, uc) = f.code("10.0.0.1", "client_id=cli");
        let poll = |dc: &str, client: &str| {
            f.advance(POLL_INTERVAL);
            let r = f.poll(dc, client);
            (r.status, error_of(&r))
        };
        let e = |s: &str| (400, s.to_string());
        assert_eq!(poll(&dc, "cli"), e("authorization_pending"));
        assert_eq!(poll("0".repeat(64).as_str(), "cli"), e("invalid_grant"));
        assert_eq!(
            poll(&dc, "someone-else"),
            e("invalid_grant"),
            "not its client"
        );
        f.auth.device.deny(Some(&uc)).unwrap();
        assert_eq!(poll(&dc, "cli"), e("access_denied"));
        // Expired: told so, then forgotten.
        let (dc2, _) = f.code("10.0.0.2", "client_id=cli");
        f.advance(Duration::from_secs(121));
        assert_eq!(poll(&dc2, "cli"), e("expired_token"));
        assert_eq!(poll(&dc2, "cli"), e("invalid_grant"));
        // The form's own refusals come first.
        let token = |body: &str| {
            let r = f
                .auth
                .token(FORM, body.as_bytes(), Some("10.0.0.3".parse().unwrap()));
            (r.status, error_of(&r))
        };
        assert_eq!(token("device_code=x&client_id=cli"), e("invalid_request"));
        assert_eq!(
            token("grant_type=authorization_code&code=x"),
            e("unsupported_grant_type")
        );
        assert_eq!(
            token(&format!("grant_type={DEVICE_CODE_GRANT}&client_id=cli")),
            e("invalid_request")
        );
    }

    #[test]
    fn polling_faster_than_interval_is_slow_down_and_adds_5s() {
        let f = Fixture::new(json!({"enabled": true}));
        let (dc, _) = f.code("10.0.0.1", "client_id=cli");
        assert_eq!(error_of(&f.poll(&dc, "cli")), "authorization_pending");
        f.advance(Duration::from_secs(4));
        let r = f.poll(&dc, "cli");
        assert_eq!(error_of(&r), "slow_down");
        assert!(
            json_of(&r)["error_description"]
                .as_str()
                .unwrap()
                .contains("10s")
        );
        // The interval is now 10 s: five is still too fast…
        f.advance(Duration::from_secs(5));
        assert_eq!(error_of(&f.poll(&dc, "cli")), "slow_down");
        // …and after another slow_down it is 15.
        f.advance(Duration::from_secs(15));
        assert_eq!(error_of(&f.poll(&dc, "cli")), "authorization_pending");
    }

    #[test]
    fn default_scope_is_user_and_operator_needs_two_explicit_choices() {
        let both = Fixture::new(json!({"enabled": true, "scopes": ["user", "operator"]}));
        // No scope asked: user.
        let v = both
            .auth
            .device
            .find(&both.code("10.0.0.1", "client_id=cli").1)
            .unwrap();
        assert_eq!(v.requested, DeviceScope::User);
        // The device asked for operator; the approver must say so too.
        let (dc, uc) = both.code("10.0.0.2", "client_id=cli&scope=operator");
        let asked = both.auth.device.find(&uc).unwrap().requested;
        assert_eq!(asked, DeviceScope::Operator);
        assert!(both.auth.device.grant_scope(asked, None).is_err());
        // The approver may narrow it to user…
        assert_eq!(
            both.auth.device.grant_scope(asked, Some("user")),
            Ok(DeviceScope::User)
        );
        // …never widen a user request.
        assert!(
            both.auth
                .device
                .grant_scope(DeviceScope::User, Some("operator"))
                .is_err()
        );
        assert!(both.auth.device.grant_scope(asked, Some("agent")).is_err());
        both.approve(&uc, "carol", Some("operator"));
        let r = both.poll(&dc, "cli");
        assert_eq!(json_of(&r)["scope"], "operator");
        let p = principal_of(&both.auth, json_of(&r)["access_token"].as_str().unwrap());
        assert_eq!((p.id.as_str(), p.role), ("operator", Role::Operator));
        let listed = both.auth.sessions().list();
        assert_eq!(
            listed[0].name.as_deref(),
            Some("carol"),
            "the name is recorded"
        );

        // Operator is not configured: not requestable, not grantable.
        let user_only = Fixture::new(json!({"enabled": true}));
        let r = user_only.authorize_from("10.0.0.1", "client_id=cli&scope=operator");
        assert_eq!(error_of(&r), "invalid_scope");
        let r = user_only.authorize_from("10.0.0.1", "client_id=cli&scope=agent");
        assert_eq!(error_of(&r), "invalid_scope");
        assert!(
            user_only
                .auth
                .device
                .grant_scope(DeviceScope::Operator, Some("operator"))
                .is_err()
        );
    }

    #[test]
    fn per_source_limits_isolate_sources_below_the_global_cap() {
        let f = Fixture::new(json!({"enabled": true}));
        // Four codes may wait per source; the fifth request is within the
        // bucket but over the source's pending cap.
        for _ in 0..PENDING_PER_SOURCE {
            f.code("10.0.0.1", "client_id=cli");
        }
        let r = f.authorize_from("10.0.0.1", "client_id=cli");
        assert_eq!(r.status, 429, "{:?}", json_of(&r));
        assert_eq!(error_of(&r), "temporarily_unavailable");
        assert!(r.retry_after.is_some());
        // Now the bucket: five requests, the sixth refused, whatever they are.
        let r = f.authorize_from("10.0.0.1", "junk");
        assert_eq!(r.status, 429);
        assert!(r.retry_after.unwrap() >= 1);
        // Another source is untouched by all of it — and so is another /64
        // of v6, while the same /64 is one source.
        f.code("10.0.0.2", "client_id=cli");
        f.code("2001:db8:0:1::1", "client_id=cli");
        let r = f.authorize_from("10.0.0.1", "client_id=cli");
        assert_eq!(r.status, 429);
        // The bucket refills.
        let bucket = bucket(AUTHORIZE_BURST, Duration::from_millis(1));
        let ip = Some("10.7.7.7".parse().unwrap());
        for _ in 0..AUTHORIZE_BURST {
            assert!(take(&bucket, ip).is_ok());
        }
        std::thread::sleep(Duration::from_millis(20));
        assert!(take(&bucket, ip).is_ok(), "refilled");
    }

    #[test]
    fn the_bucket_admits_exactly_its_burst() {
        let b = bucket(AUTHORIZE_BURST, AUTHORIZE_REFILL);
        let ip = Some("10.0.0.9".parse().unwrap());
        for i in 0..AUTHORIZE_BURST {
            assert!(take(&b, ip).is_ok(), "request {i}");
        }
        assert_eq!(take(&b, ip), Err(AUTHORIZE_REFILL.as_secs()));
    }

    #[test]
    fn pending_authorizations_are_bounded() {
        let f = Fixture::new(json!({"enabled": true}));
        let sources = PENDING_GLOBAL / PENDING_PER_SOURCE;
        for s in 0..sources {
            for _ in 0..PENDING_PER_SOURCE {
                f.code(&format!("10.2.{s}.1"), "client_id=cli");
            }
        }
        assert_eq!(f.auth.device.pending().len(), PENDING_GLOBAL);
        let r = f.authorize_from("10.3.0.1", "client_id=cli");
        assert_eq!(r.status, 429, "a fresh source over the global cap");
        // Denying frees the slots; expiry does too.
        let first = f.auth.device.pending()[0].user_code.clone();
        f.auth.device.deny(Some(&first)).unwrap();
        f.code("10.3.0.1", "client_id=cli");
        f.advance(Duration::from_secs(11 * 60));
        assert!(f.auth.device.pending().is_empty());
        f.code("10.3.0.2", "client_id=cli");
    }

    #[test]
    fn sessions_of_one_name_are_one_principal_and_revoke_singly() {
        let f =
            Fixture::new(json!({"enabled": true, "rate": "2/60s", "scopes": ["user", "operator"]}));
        let one = f.session("10.0.0.1", "alice");
        f.advance(Duration::from_secs(1));
        let two = f.session("10.0.0.2", "alice");
        let (p1, p2) = (principal_of(&f.auth, &one), principal_of(&f.auth, &two));
        assert_eq!(
            (p1.id.as_str(), p2.id.as_str()),
            ("user:alice", "user:alice")
        );
        assert_ne!(p1.session, p2.session, "each has its own sid");
        assert_eq!(p1.role, Role::User);
        // Both carry the device rate: one bucket, keyed by the principal id.
        assert_eq!(p1.rate.as_deref(), Some("2/60s"));
        let rates = crate::a2a::serve::limits::PrincipalRates::default();
        assert!(rates.admit(&p1).is_ok() && rates.admit(&p2).is_ok());
        assert!(
            rates.admit(&p1).is_err(),
            "the sibling spent the shared bucket"
        );
        // Revoking one sid leaves the other.
        let gone = f
            .auth
            .sessions()
            .revoke(&Revoke::Sid(p1.session.clone().unwrap()));
        assert_eq!(gone.len(), 1);
        assert_eq!(f.auth.sessions().verify(&one), SessionCheck::Invalid);
        assert!(matches!(
            f.auth.sessions().verify(&two),
            SessionCheck::Valid(_)
        ));
        let alive = Sessions::liveness(Arc::clone(f.auth.sessions()));
        assert!(!alive(&p1).unwrap()(), "the revoked sid is dead");
        assert!(alive(&p2).unwrap()(), "its sibling lives");
        assert!(
            alive(&Principal::anonymous()).is_none(),
            "no session, no check"
        );
        // {name} ends every session of the name.
        let three = f.session("10.0.0.3", "alice");
        let gone = f.auth.sessions().revoke(&Revoke::Name("alice".into()));
        assert_eq!(gone.len(), 2);
        assert_eq!(f.auth.sessions().verify(&two), SessionCheck::Invalid);
        assert_eq!(f.auth.sessions().verify(&three), SessionCheck::Invalid);
        // An operator-scope approval as carol is `operator`, the name kept.
        let (dc, uc) = f.code("10.0.0.4", "client_id=cli&scope=operator");
        f.approve(&uc, "carol", Some("operator"));
        let t = json_of(&f.poll(&dc, "cli"))["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let p = principal_of(&f.auth, &t);
        assert_eq!(p.id, "operator");
        assert_eq!(f.auth.sessions().list()[0].name.as_deref(), Some("carol"));
        // Expiry ends a session like revocation does.
        f.advance(Duration::from_secs(9 * 3600));
        assert_eq!(f.auth.sessions().verify(&t), SessionCheck::Invalid);
        assert!(f.auth.sessions().list().is_empty());
    }

    #[test]
    fn form_rules() {
        let f = Fixture::new(json!({"enabled": true}));
        // Unknown parameters are ignored.
        let r = f.authorize_from("10.0.0.1", "client_id=cli&audience=x&resource=y");
        assert_eq!(r.status, 200, "{:?}", json_of(&r));
        // A duplicated parameter is refused, even with one value empty.
        for body in ["client_id=a&client_id=b", "client_id=a&client_id="] {
            let r = f.authorize_from("10.0.0.2", body);
            assert_eq!(error_of(&r), "invalid_request", "{body}");
        }
        // An empty value is absent.
        let r = f.authorize_from("10.0.0.3", "client_id=cli&scope=");
        assert_eq!(r.status, 200);
        let r = f.authorize_from("10.0.0.3", "client_id=");
        assert_eq!(error_of(&r), "invalid_request");
        // Only a form, and only a small one.
        for ct in [None, Some("application/json"), Some("text/plain")] {
            let r = f.auth.device_authorization(
                ct,
                b"client_id=cli",
                Some("10.0.0.4".parse().unwrap()),
            );
            assert_eq!(error_of(&r), "invalid_request", "{ct:?}");
        }
        let r = f.auth.device_authorization(
            Some("Application/X-WWW-Form-Urlencoded; charset=UTF-8"),
            b"client_id=cli",
            Some("10.0.0.5".parse().unwrap()),
        );
        assert_eq!(r.status, 200, "the type is read with its parameters");
        let big = format!("client_id=cli&pad={}", "x".repeat(FORM_MAX));
        assert_eq!(
            error_of(&f.authorize_from("10.0.0.6", &big)),
            "invalid_request"
        );
        // Bad encodings are refused.
        for body in ["client_id=%zz", "client_id=%c3%28", "client_id=%4"] {
            assert_eq!(
                error_of(&f.authorize_from("10.0.0.7", body)),
                "invalid_request",
                "{body}"
            );
        }
        let form = parse_form(FORM, b"a=x+y%21&b").unwrap();
        assert_eq!(form.get("a"), Some("x y!"));
        assert_eq!(form.get("b"), None);
        // A bad client id.
        for id in ["a b", &"x".repeat(65), "a%0Ab"] {
            let r = f.authorize_from("10.0.0.8", &format!("client_id={id}"));
            assert_eq!(error_of(&r), "invalid_request", "{id}");
        }
        // Revoke takes token_type_hint and client_id, and a token naming
        // nothing is still a 200.
        let token = f.session("10.0.0.9", "alice");
        let r = f.auth.revoke(
            FORM,
            format!("token={token}&token_type_hint=access_token&client_id=cli").as_bytes(),
        );
        assert_eq!((r.status, &r.body), (200, &ReplyBody::Empty));
        assert!(
            matches!(&r.notices[..], [Notice::Revoked(s)] if s.name.as_deref() == Some("alice"))
        );
        assert_eq!(f.auth.sessions().verify(&token), SessionCheck::Invalid);
        let r = f.auth.revoke(FORM, b"token=agentd_at_nothing");
        assert_eq!((r.status, r.notices.len()), (200, 0));
        assert_eq!(
            error_of(&f.auth.revoke(FORM, b"token_type_hint=x")),
            "invalid_request"
        );
        assert_eq!(
            error_of(&f.auth.revoke(Some("text/plain"), b"token=x")),
            "invalid_request"
        );
    }

    /// Every mint that fails — the device code, the user code's entropy, the
    /// sid, the token — is a 503 `temporarily_unavailable` that issues and
    /// records nothing, never a panic.
    #[test]
    fn a_failed_mint_is_503_and_issues_nothing() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let flag = Arc::clone(&fail);
        let mint: Mint = Arc::new(move |n| {
            if flag.load(Ordering::SeqCst) {
                Err(std::io::Error::other("no entropy"))
            } else {
                crate::sec::random::hex_token(n)
            }
        });
        let f = Fixture::with_mint(json!({"enabled": true}), mint);
        let r = f.authorize_from("10.0.0.1", "client_id=cli");
        assert_eq!(r.status, 503);
        assert_eq!(
            json_of(&r)["error"],
            "temporarily_unavailable",
            "{:?}",
            json_of(&r)
        );
        assert!(r.notices.is_empty());
        assert!(f.auth.device.pending().is_empty(), "nothing recorded");

        fail.store(false, Ordering::SeqCst);
        let (dc, uc) = f.code("10.0.0.2", "client_id=cli");
        f.approve(&uc, "alice", None);
        fail.store(true, Ordering::SeqCst);
        let r = f.poll(&dc, "cli");
        assert_eq!(
            (r.status, error_of(&r)),
            (503, "temporarily_unavailable".into())
        );
        assert!(f.auth.sessions().list().is_empty(), "no session issued");
        // The approval stands and the code is not burned: the next poll works.
        fail.store(false, Ordering::SeqCst);
        f.advance(POLL_INTERVAL);
        assert_eq!(f.poll(&dc, "cli").status, 200);
    }

    #[test]
    fn metadata_names_every_endpoint_under_the_issuer() {
        let f = Fixture::new(json!({"enabled": true, "scopes": ["user", "operator"]}));
        let v = json_of(&f.auth.metadata());
        assert_eq!(v["issuer"], "https://agent.example:8443");
        assert_eq!(
            v["token_endpoint"],
            "https://agent.example:8443/oauth2/token"
        );
        assert_eq!(v["grant_types_supported"], json!([DEVICE_CODE_GRANT]));
        assert_eq!(v["scopes_supported"], json!(["user", "operator"]));
        // The verification URI defaults to the listener's own page.
        let (_, _) = f.code("10.0.0.1", "client_id=cli");
        let r = f.authorize_from("10.0.0.1", "client_id=cli");
        assert_eq!(
            json_of(&r)["verification_uri"],
            "https://agent.example:8443/oauth2/device"
        );
        // An issuer not yet settled answers 503 rather than a wrong origin.
        let sessions = Arc::new(Sessions::new(system_clock()));
        let cfg: v2::DeviceGrant = serde_json::from_value(json!({"enabled": true})).unwrap();
        let unsettled = Authority::new(DeviceGrant::new(&cfg, system_clock(), os_mint()), sessions);
        assert_eq!(unsettled.metadata().status, 503);
    }
}
