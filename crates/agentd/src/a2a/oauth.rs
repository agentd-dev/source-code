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
//! Why it is shaped this way:
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
//! The same origin also redeems the **launch grant** while `agentd tui` or
//! `agentd ui` has installed a [`LaunchSlot`] in this process: a single-use
//! code the launcher minted and handed its own client, or a request a browser
//! tab makes that the person at the launcher's terminal approves by typing
//! the code the tab shows. Nothing outside the launcher's process can mint a
//! code or approve a request — there is no op, route or key that does — and
//! both are redeemed only from a loopback peer. Either buys an operator
//! session: it acts for the person who started this daemon, from their own
//! configuration, on this host.
//!
//! Only hashes are kept: the store holds the SHA-256 of every device code,
//! launch code, request code and token, never the value, and none is ever
//! logged. Everything is held in memory, so a restart revokes every session.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use crate::a2a::Principal;
use crate::a2a::principals::{SESSION_TOKEN_PREFIX, SessionCheck, SessionVerifier};
use crate::a2a::serve::limits::{NetworkKey, SourceKey, SourceLimiter};
use crate::config::settings::{self, DeviceScope, Role};

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
/// Codes one network (an IPv6 /48; for IPv4, the source itself) may have
/// waiting at once: a quarter of the table, so the /64s of one allocation —
/// 65536 of them in a /48 — never fill it between them.
pub const PENDING_PER_NETWORK: usize = 16;
/// Codes waiting at once across every source — what bounds the operator's
/// `auth.device.pending` and this table, however many addresses ask. A
/// newcomer at the bound is not refused while some network holds more than
/// its share: that network's oldest waiting code gives way.
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
    /// Through the launch grant: a code the launcher minted in this process,
    /// or a request the person at the launcher's terminal approved.
    Launch,
}

impl SessionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionKind::Device => "device",
            SessionKind::Launch => "launch",
        }
    }
}

/// One signed-in session.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    /// The handle it is listed, audited and revoked by (`ds_<16 hex>`, or
    /// `ls_<16 hex>` for a launch session).
    pub sid: String,
    pub kind: SessionKind,
    /// The name it was approved as. A launch session has none: it is the
    /// person who started the daemon, not somebody an operator named.
    pub name: Option<String>,
    pub role: Role,
    /// Who it acts as: `user:<name>`, or `operator`.
    pub principal: String,
    pub client_id: String,
    pub created_ms: u64,
    /// `None`: it ends only when revoked.
    pub expires_ms: Option<u64>,
    /// The principal id that approved it — for a launch session,
    /// `launcher` (the code) or `launcher-terminal` (a typed request).
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

    /// The feed's `auth` event for this session's end, in the shape every
    /// `auth` event has ([`auth_event`]): the session's scope is its role.
    pub fn revoked_event(&self) -> Value {
        let mut v = auth_event("revoked", &self.client_id, role_name(self.role));
        v["sid"] = json!(self.sid);
        if let Some(n) = &self.name {
            v["name"] = json!(n);
        }
        v
    }
}

/// The skeleton of a feed `auth` event: what it is, and the `client_id` and
/// `scope` every one of them carries, so an operator's client can say which
/// device and what reach whatever happened. The optional fields
/// (`user_code`, `peer`, `sid`, `name`) are added only when there is a value
/// — never as `null` — and nothing else is: who a session acts as follows
/// from the scope and the name, and the feed's schema has no room for more.
pub fn auth_event(event: &str, client_id: &str, scope: &str) -> Value {
    json!({"event": event, "client_id": client_id, "scope": scope})
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
    /// The feed's `auth` event for this authorization: `pending` and
    /// `denied` carry the scope the device asked for.
    pub fn event(&self, event: &str) -> Value {
        let mut v = auth_event(event, &self.client_id, self.requested.as_str());
        v["user_code"] = json!(display_user_code(&self.user_code));
        if let Some(p) = self.peer {
            v["peer"] = json!(p.to_string());
        }
        v
    }

    /// The `approved` event: the scope granted, which may be narrower than
    /// the one asked for, and the name the device now acts under.
    pub fn approved_event(&self, name: &str, granted: DeviceScope) -> Value {
        let mut v = self.event("approved");
        v["scope"] = json!(granted.as_str());
        v["name"] = json!(name);
        v
    }

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

    fn network(&self) -> Option<NetworkKey> {
        self.source.map(SourceKey::network)
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
    pub fn new(cfg: &settings::DeviceGrant, clock: Clock, mint: Mint) -> DeviceGrant {
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

    /// At the global bound, free a slot for a newcomer from `network`, which
    /// holds `ours` of them: the network waiting on the most codes gives up
    /// its oldest one, if it holds more than the newcomer would after. So
    /// however many networks a flood comes from, a party asking for the first
    /// time is never refused on its account — the flood only ever displaces
    /// itself — while shares that are already even stay put.
    ///
    /// Only a code still waiting on an operator gives way; an approved one is
    /// the operator's decision. It is expired, not removed, so its device's
    /// next poll is told `expired_token` and may start again.
    fn make_room(
        t: &mut HashMap<String, Authorization>,
        now: u64,
        network: Option<NetworkKey>,
        ours: usize,
    ) -> bool {
        // Per network: how many codes it has waiting, and its oldest one (by
        // age, then code, so which one gives way among equals is fixed).
        struct Held<'t> {
            count: usize,
            oldest: (u64, &'t str),
            key: &'t str,
        }
        let mut held: HashMap<Option<NetworkKey>, Held<'_>> = HashMap::new();
        for (key, a) in t.iter() {
            if !(a.occupies(now) && a.decision == Decision::Pending) {
                continue;
            }
            let age = (a.requested_ms, a.user_code.as_str());
            let h = held.entry(a.network()).or_insert(Held {
                count: 0,
                oldest: age,
                key,
            });
            h.count += 1;
            if age < h.oldest {
                (h.oldest, h.key) = (age, key);
            }
        }
        let Some(biggest) = held
            .iter()
            .filter(|(n, _)| **n != network)
            .map(|(_, h)| h)
            .max_by(|a, b| a.count.cmp(&b.count).then(b.oldest.cmp(&a.oldest)))
        else {
            return false;
        };
        if biggest.count <= ours + 1 {
            return false;
        }
        let victim = biggest.key.to_string();
        if let Some(a) = t.get_mut(&victim) {
            a.expires_ms = now;
        }
        true
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
    /// `None`; returns what was refused, oldest first.
    pub fn deny(&self, typed: Option<&str>) -> Result<Vec<PendingView>, String> {
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
                out.push(a.view());
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
        out.sort_by(|a, b| (a.requested_ms, &a.user_code).cmp(&(b.requested_ms, &b.user_code)));
        Ok(out)
    }
}

// ---- the launch grant -------------------------------------------------------

/// Where a browser tab the launcher opened asks the person at the launcher's
/// terminal to sign it in. Served only while the slot carries a UI origin.
pub const LAUNCH_AUTHORIZATION_PATH: &str = "/oauth2/launch_authorization";
/// The prefix of a launch code.
pub const LAUNCH_CODE_PREFIX: &str = "agentd_lc_";
/// The prefix of a terminal-approved request's code.
pub const LAUNCH_REQUEST_PREFIX: &str = "agentd_lr_";
/// The prefix of a launch session's id.
pub const LAUNCH_SID_PREFIX: &str = "ls_";
/// How long a request waits for its code to be typed at the terminal.
pub const LAUNCH_REQUEST_TTL: Duration = Duration::from_secs(120);
/// How often a tab may poll its request…
pub const LAUNCH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// …and what `slow_down` adds to that.
pub const LAUNCH_SLOW_DOWN_STEP: Duration = Duration::from_secs(2);
/// Requests waiting on the terminal at once. A newer one displaces the
/// oldest, whose tab is told `expired_token` and asks again: the terminal is
/// the gate, so a flood can cost a tab a retry, never a sign-in.
pub const LAUNCH_PENDING_MAX: usize = 16;
/// Requests remembered at all, displaced and expired ones included, so a late
/// poll is told `expired_token` rather than that it never existed.
const LAUNCH_REQUESTS_KEPT: usize = 4 * LAUNCH_PENDING_MAX;
/// Refused launch presentations a source is answered before it gets 429…
pub const LAUNCH_FAILURE_BURST: u32 = 20;
/// …forgiven one every this long.
pub const LAUNCH_FAILURE_REFILL: Duration = Duration::from_secs(3);
/// `approved_by` of a session a launch code bought.
pub const LAUNCHER: &str = "launcher";
/// `approved_by` of a session the person at the launcher's terminal approved.
pub const LAUNCHER_TERMINAL: &str = "launcher-terminal";

/// What a launch code is bound to: the web origin of the UI the launcher
/// started, or no origin — a terminal client, which is no browser, so a
/// request that carries any `Origin` (`null` included) is not its.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchBind {
    Origin(String),
    NoOrigin,
}

impl LaunchBind {
    /// Whether a presentation carrying `origin` — the `Origin` header, when
    /// the request had one — is this bind's.
    fn admits(&self, origin: Option<&str>) -> bool {
        match (self, origin) {
            (LaunchBind::NoOrigin, None) => true,
            (LaunchBind::Origin(want), Some(got)) => same_origin(want, got),
            _ => false,
        }
    }

    /// How the exchange log names it: the origin, or `none`.
    fn label(&self) -> &str {
        match self {
            LaunchBind::Origin(o) => o,
            LaunchBind::NoOrigin => "none",
        }
    }
}

/// Two origins compared as origins (scheme, host, port with its default), so
/// `http://127.0.0.1:4555` is `HTTP://127.0.0.1:4555`, and `null` or anything
/// unparsable is nobody's.
fn same_origin(a: &str, b: &str) -> bool {
    matches!((settings::parse_origin(a), settings::parse_origin(b)), (Ok(a), Ok(b)) if a == b)
}

/// Whether a peer is on this host. The launch grant is redeemed only from
/// one: a code or request that leaked off the host is useless even on a
/// wildcard bind, while an SSH `-L` forward arrives from the remote end's own
/// loopback and works.
fn loopback(peer: Option<IpAddr>) -> bool {
    peer.is_some_and(|ip| ip.to_canonical().is_loopback())
}

/// A code waiting to be redeemed.
#[derive(Debug)]
struct LaunchCode {
    bind: LaunchBind,
    client_id: String,
    expires_ms: u64,
}

/// A browser tab's request, waiting for the terminal.
#[derive(Debug)]
struct LaunchRequest {
    user_code: String,
    client_id: String,
    /// The origin that asked, and the only one that may poll.
    origin: String,
    requested_ms: u64,
    expires_ms: u64,
    interval_ms: u64,
    last_poll_ms: Option<u64>,
    approved: bool,
}

impl LaunchRequest {
    fn expired(&self, now: u64) -> bool {
        now >= self.expires_ms
    }
}

/// The codes and requests in flight, keyed by the SHA-256 of their secrets.
#[derive(Debug, Default)]
struct LaunchTable {
    codes: HashMap<String, LaunchCode>,
    requests: HashMap<String, LaunchRequest>,
}

impl LaunchTable {
    /// Forget expired codes, and requests expired a lifetime ago; then, past
    /// what is kept at all, the oldest requests no tab can still redeem.
    fn prune(&mut self, now: u64) {
        self.codes.retain(|_, c| now < c.expires_ms);
        let grace = ms(LAUNCH_REQUEST_TTL);
        self.requests
            .retain(|_, r| now < r.expires_ms.saturating_add(grace));
        while self.requests.len() > LAUNCH_REQUESTS_KEPT {
            let Some(oldest) = self
                .requests
                .iter()
                .filter(|(_, r)| r.expired(now))
                .min_by_key(|(_, r)| (r.requested_ms, r.user_code.clone()))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.requests.remove(&oldest);
        }
    }
}

/// Something the launcher asked to hear about.
pub type LaunchHook = Arc<dyn Fn() + Send + Sync>;

/// **The launch slot**: what `agentd tui` / `agentd ui` install in the
/// daemon's own process ([`crate::runtime::RunOpts`]) so the client they
/// start can sign in without being handed a credential.
///
/// The launcher mints a code with [`LaunchSlot::issue`] and gives it to its
/// client — over an inherited pipe, or in a URL fragment — and the client
/// redeems it once at `/oauth2/token`. A browser that could not be given the
/// code (a sandboxed one, an SSH forward, a second tab) asks at
/// [`LAUNCH_AUTHORIZATION_PATH`] instead, shows a user code, and the person at
/// the launcher's terminal types it: [`LaunchSlot::approve_user_code`]. Only
/// the launcher holds the slot, so only it can do either.
pub struct LaunchSlot {
    /// The UI origin, for `agentd ui`: the one origin a code or request may
    /// be bound to, and the one the listener's CORS list gains.
    origin: Option<String>,
    clock: Clock,
    mint: Mint,
    table: Mutex<LaunchTable>,
    on_consume: Mutex<Option<LaunchHook>>,
    on_request: Mutex<Option<LaunchHook>>,
    /// The daemon's logger, attached when the listener starts.
    log: OnceLock<crate::obs::log::Logger>,
}

impl std::fmt::Debug for LaunchSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSlot")
            .field("origin", &self.origin)
            .field("table", &*self.table())
            .finish_non_exhaustive()
    }
}

impl LaunchSlot {
    /// A slot for a UI served at `origin` (`agentd ui`), or for a terminal
    /// client (`None`, `agentd tui`).
    pub fn new(origin: Option<&str>) -> Result<LaunchSlot, String> {
        LaunchSlot::with(origin, system_clock(), os_mint())
    }

    /// [`LaunchSlot::new`] on an injected clock and entropy source.
    pub fn with(origin: Option<&str>, clock: Clock, mint: Mint) -> Result<LaunchSlot, String> {
        if let Some(o) = origin {
            settings::parse_origin(o).map_err(|e| format!("the launched UI origin {o:?}: {e}"))?;
        }
        Ok(LaunchSlot {
            origin: origin.map(str::to_string),
            clock,
            mint,
            table: Mutex::new(LaunchTable::default()),
            on_consume: Mutex::new(None),
            on_request: Mutex::new(None),
            log: OnceLock::new(),
        })
    }

    fn table(&self) -> std::sync::MutexGuard<'_, LaunchTable> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The UI origin this slot was made for, if any.
    pub fn origin(&self) -> Option<&str> {
        self.origin.as_deref()
    }

    /// Mint a code for `bind` and `client_id`, redeemable once within
    /// [`crate::runtime::surface::launch::LAUNCH_CODE_TTL`]. A bind has one
    /// code at a time: issuing again makes the earlier one worthless, so a
    /// copy of a code the launcher replaced cannot be redeemed later.
    ///
    /// Only the code's hash is kept. `Err` when OS randomness failed: nothing
    /// was issued, and the earlier code, if any, still stands.
    pub fn issue(&self, bind: LaunchBind, client_id: &str) -> std::io::Result<String> {
        let code = format!("{LAUNCH_CODE_PREFIX}{}", (self.mint)(SECRET_BYTES)?);
        let now = (self.clock)();
        let ttl = ms(crate::runtime::surface::launch::LAUNCH_CODE_TTL);
        let mut t = self.table();
        t.prune(now);
        t.codes.retain(|_, c| c.bind != bind);
        t.codes.insert(
            hash(&code),
            LaunchCode {
                bind,
                client_id: client_id.to_string(),
                expires_ms: now.saturating_add(ttl),
            },
        );
        Ok(code)
    }

    /// Call `hook` whenever a code is consumed — by its redemption or by any
    /// presentation that burned it — so the launcher removes the file that
    /// carried it at once.
    pub fn on_consume(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_consume.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// Call `hook` whenever a browser tab asks to be signed in, so the
    /// launcher asks its terminal for the code the tab shows.
    pub fn on_request(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_request.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(hook));
    }

    /// Run a hook with no lock held: it may take its time, or call back in.
    fn fire(hook: &Mutex<Option<LaunchHook>>) {
        let hook = hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(h) = hook {
            h();
        }
    }

    /// Approve the one waiting request whose user code `typed` is — as a
    /// person types it: any case, dash optional. `false`, approving nothing,
    /// when no request is waiting on it.
    pub fn approve_user_code(&self, typed: &str) -> bool {
        let Some(code) = normalize_user_code(typed) else {
            return false;
        };
        let now = (self.clock)();
        let mut t = self.table();
        let Some(r) = t
            .requests
            .values_mut()
            .find(|r| r.user_code == code && !r.expired(now) && !r.approved)
        else {
            return false;
        };
        r.approved = true;
        let client_id = r.client_id.clone();
        drop(t);
        if let Some(log) = self.log.get() {
            log.info(
                "auth.launch.approved",
                serde_json::json!({"client_id": client_id}),
            );
        }
        true
    }

    /// How many browser tabs are waiting for the terminal right now: the
    /// launcher shows one prompt however many asked, and says how many more
    /// there are.
    pub fn waiting(&self) -> usize {
        let now = (self.clock)();
        self.table()
            .requests
            .values()
            .filter(|r| !r.expired(now) && !r.approved)
            .count()
    }

    /// Hand the slot the daemon's logger, once the listener is up.
    pub(crate) fn attach_log(&self, log: crate::obs::log::Logger) {
        let _ = self.log.set(log);
    }
}

/// The browser origins a listener admits: `a2a.cors.origins`, and the UI a
/// launcher started in this process. Every build of the live list goes
/// through here — at spawn and on every reload — so a reload that edits the
/// configured origins can never silently drop the launched UI. The launched
/// origin is not configuration: no settings dump or manifest count sees it.
pub fn admitted_origins(configured: &[String], launch: Option<&LaunchSlot>) -> Vec<String> {
    let mut out = configured.to_vec();
    if let Some(o) = launch.and_then(LaunchSlot::origin)
        && !out.iter().any(|c| same_origin(c, o))
    {
        out.push(o.to_string());
    }
    out
}

impl Session {
    /// The feed's `auth` event for a launch session: it acts as the
    /// operator, and it is named by its sid (it has no name).
    pub fn launched_event(&self) -> Value {
        let mut v = auth_event("launch", &self.client_id, role_name(self.role));
        v["sid"] = json!(self.sid);
        v
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
    /// A launch session was issued: logged as `auth.launch.exchanged`, and
    /// the feed's `auth` `launch` event.
    Launched {
        session: Session,
        /// The code's bind ([`LaunchBind`]): the origin, or `none`.
        bind: String,
        /// `code` or `terminal`.
        via: &'static str,
    },
    /// A browser tab asked the terminal to sign it in
    /// (`auth.launch.requested`), from `client_id`.
    LaunchRequested(String),
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

    /// A route this authority does not serve. The listener mounts none of
    /// them, so this is only a second lock.
    fn not_found() -> Reply {
        Reply {
            status: 404,
            body: ReplyBody::Empty,
            retry_after: None,
            notices: Vec::new(),
        }
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

/// The authorization server: the device grant and the launch grant — either
/// or both — their per-source limits, and the sessions they issue into, which
/// it borrows, because they exist on the listener with or without it.
pub struct Authority {
    /// The device grant, when `a2a.device_grant.enabled`.
    device: Option<DeviceGrant>,
    /// The launch grant, while a launcher has installed its slot.
    launch: Option<Arc<LaunchSlot>>,
    /// Device authorizations per source.
    limiter: SourceLimiter,
    /// Device-code polls per source.
    token_limiter: SourceLimiter,
    /// Refused launch presentations per source. Its own, and counting only
    /// refusals: a flood of junk from 127.0.0.1 must never delay a live code
    /// past its lifetime, so what is limited is failing, never redeeming.
    launch_failures: SourceLimiter,
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
    pub fn new(
        device: Option<DeviceGrant>,
        launch: Option<Arc<LaunchSlot>>,
        sessions: Arc<Sessions>,
    ) -> Authority {
        Authority {
            device,
            launch,
            limiter: bucket(AUTHORIZE_BURST, AUTHORIZE_REFILL),
            token_limiter: bucket(TOKEN_BURST, TOKEN_REFILL),
            launch_failures: bucket(LAUNCH_FAILURE_BURST, LAUNCH_FAILURE_REFILL),
            sessions,
            issuer: OnceLock::new(),
        }
    }

    /// The device grant, when it is enabled.
    pub fn device(&self) -> Option<&DeviceGrant> {
        self.device.as_ref()
    }

    /// The launcher's slot, when one is installed.
    pub fn launch(&self) -> Option<&Arc<LaunchSlot>> {
        self.launch.as_ref()
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
        let Some(device) = &self.device else {
            return Reply::not_found();
        };
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
        if !device.scopes.contains(&requested) {
            return Reply::error(OAuthError::new(
                "invalid_scope",
                format!("scope {} is not offered here", requested.as_str()),
            ));
        }
        let verification_uri = match &device.verification_uri {
            Some(u) => u.clone(),
            None => match self.endpoint(VERIFICATION_PATH) {
                Ok(u) => u,
                Err(e) => return Reply::error(e),
            },
        };

        let now = (device.clock)();
        let source = peer.map(SourceKey::of);
        let mut t = device.table();
        device.prune(&mut t, now);
        let network = source.map(SourceKey::network);
        let live = t.values().filter(|a| a.occupies(now));
        let (mine, ours, all) = live.fold((0, 0, 0), |(m, n, a), x| {
            (
                m + usize::from(source.is_some() && x.source == source),
                n + usize::from(network.is_some() && x.network() == network),
                a + 1,
            )
        });
        if mine >= PENDING_PER_SOURCE {
            return Reply::limited(
                AUTHORIZE_REFILL.as_secs(),
                "this source already has as many sign-ins waiting as it may",
            );
        }
        if ours >= PENDING_PER_NETWORK {
            return Reply::limited(
                AUTHORIZE_REFILL.as_secs(),
                "this network already has as many sign-ins waiting as it may",
            );
        }
        if all >= PENDING_GLOBAL && !DeviceGrant::make_room(&mut t, now, network, ours) {
            return Reply::limited(
                AUTHORIZE_REFILL.as_secs(),
                "too many sign-ins are waiting; retry later",
            );
        }
        let minted = (|| {
            let device_code = (device.mint)(SECRET_BYTES)?;
            // A user code is a name for the request, unique while it waits.
            for _ in 0..8 {
                let user_code = mint_user_code(&device.mint)?;
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
        let expires_ms = now.saturating_add(ms(device.code_ttl));
        let authorization = Authorization {
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
        };
        let pending = authorization.view();
        t.insert(hash(&device_code), authorization);
        let shown = display_user_code(&user_code);
        let mut r = Reply::json(
            200,
            json!({
                "device_code": device_code,
                "user_code": shown,
                "verification_uri": verification_uri,
                "expires_in": device.code_ttl.as_secs(),
                "interval": POLL_INTERVAL.as_secs(),
            }),
        );
        r.notices.push(Notice::Pending(pending.event("pending")));
        r
    }

    /// `POST /oauth2/token`: the device-code grant (RFC 8628 §3.4–3.5) while
    /// the device grant is enabled, and the launch grant while a launcher's
    /// slot is installed. `origin` is the request's `Origin` header, when it
    /// had one — the launch grant is bound to it.
    ///
    /// The refusals come in a fixed order — `invalid_request`,
    /// `unsupported_grant_type`, `invalid_grant`, `expired_token`,
    /// `slow_down`, `authorization_pending`, `access_denied` — so a client
    /// polling too fast is told so whatever the approver has decided.
    pub fn token(
        &self,
        content_type: Option<&str>,
        body: &[u8],
        peer: Option<IpAddr>,
        origin: Option<&str>,
    ) -> Reply {
        let form = match parse_form(content_type, body) {
            Ok(f) => f,
            Err(e) => return Reply::error(e),
        };
        let Some(grant_type) = form.get("grant_type") else {
            return Reply::error(OAuthError::invalid_request("grant_type is required"));
        };
        let launch_grant = crate::runtime::surface::launch::LAUNCH_GRANT_TYPE;
        match (&self.device, &self.launch) {
            (Some(device), _) if grant_type == DEVICE_CODE_GRANT => {
                self.device_token(device, &form, peer)
            }
            (_, Some(slot)) if grant_type == launch_grant => {
                self.launch_token(slot, &form, peer, origin)
            }
            _ => {
                let offered: Vec<&str> = self
                    .device
                    .as_ref()
                    .map(|_| DEVICE_CODE_GRANT)
                    .into_iter()
                    .chain(self.launch.as_ref().map(|_| launch_grant))
                    .collect();
                Reply::error(OAuthError::new(
                    "unsupported_grant_type",
                    format!(
                        "this server issues tokens for {} only",
                        offered.join(" and ")
                    ),
                ))
            }
        }
    }

    /// The device-code grant's redemption.
    fn device_token(&self, device: &DeviceGrant, form: &Form, peer: Option<IpAddr>) -> Reply {
        let (Some(device_code), Some(client_id)) = (form.get("device_code"), form.get("client_id"))
        else {
            return Reply::error(OAuthError::invalid_request(
                "device_code and client_id are required",
            ));
        };
        if let Err(retry) = take(&self.token_limiter, peer) {
            return Reply::limited(retry, "too many token requests from this source");
        }

        let now = (device.clock)();
        let key = hash(device_code);
        let mut t = device.table();
        device.prune(&mut t, now);
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
            let sid = format!("{DEVICE_SID_PREFIX}{}", (device.mint)(SID_BYTES)?);
            let token = format!("{SESSION_TOKEN_PREFIX}{}", (device.mint)(SECRET_BYTES)?);
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
                expires_ms: Some(now.saturating_add(ms(device.token_ttl))),
                approved_by: approval.approved_by,
                approved_rule: approval.rule,
                rate: device.rate.clone(),
            },
        );
        Reply::json(
            200,
            json!({
                "access_token": token,
                "token_type": "Bearer",
                "expires_in": device.token_ttl.as_secs(),
                "scope": approval.scope.as_str(),
            }),
        )
    }

    /// The launch grant's redemption: exactly one of `code` (the launcher's
    /// single-use code) or `request_code` (a terminal-approved request), and
    /// the `client_id` it was issued to, from a loopback peer, bound as it
    /// was issued.
    ///
    /// A code is consumed by ANY presentation of it that reaches here: a
    /// wrong origin, a wrong client, a remote peer all burn it along with the
    /// answer — the same `invalid_grant` an unknown code gets — so a stolen
    /// code buys its thief at most the denial of one sign-in. Only a failing
    /// mint leaves it be, because then nothing was presented wrongly.
    ///
    /// "Reaches here" is the CORS gate's to decide: every `/oauth2/*` route
    /// runs it first, so an `Origin` the listener does not admit — `null`
    /// included — is answered 403 and burns nothing, the code staying its
    /// own client's (a page on that origin could not read an answer anyway).
    /// The wrong origin that burns a code is an admitted one that is not the
    /// code's bind; `Origin: null` is no origin the gate can admit, so it
    /// never gets this far.
    fn launch_token(
        &self,
        slot: &LaunchSlot,
        form: &Form,
        peer: Option<IpAddr>,
        origin: Option<&str>,
    ) -> Reply {
        let client_id = form.get("client_id");
        let (code, request_code, Some(client_id)) =
            (form.get("code"), form.get("request_code"), client_id)
        else {
            return Reply::error(OAuthError::invalid_request(
                "the launch grant takes client_id and exactly one of code or request_code",
            ));
        };
        match (code, request_code) {
            (Some(code), None) => self.launch_code(slot, code, client_id, peer, origin),
            (None, Some(request)) => self.launch_request(slot, request, client_id, peer, origin),
            _ => Reply::error(OAuthError::invalid_request(
                "the launch grant takes client_id and exactly one of code or request_code",
            )),
        }
    }

    /// Redeem a launch code.
    fn launch_code(
        &self,
        slot: &LaunchSlot,
        code: &str,
        client_id: &str,
        peer: Option<IpAddr>,
        origin: Option<&str>,
    ) -> Reply {
        let now = (slot.clock)();
        let key = hash(code);
        let mut t = slot.table();
        t.prune(now);
        let Some(held) = t.codes.get(&key) else {
            drop(t);
            return self.launch_refused(peer);
        };
        if !(loopback(peer)
            && now < held.expires_ms
            && held.bind.admits(origin)
            && held.client_id == client_id)
        {
            t.codes.remove(&key);
            drop(t);
            LaunchSlot::fire(&slot.on_consume);
            return self.launch_refused(peer);
        }
        // Mint before the code is spent: a failing entropy source answers
        // 503 and the launcher's client may present the same code again.
        let Ok((sid, token)) = launch_secrets(slot) else {
            return Reply::error(OAuthError::unavailable(
                "no session could be issued right now; present the code again",
            ));
        };
        let held = t.codes.remove(&key).expect("held under the same lock");
        drop(t);
        LaunchSlot::fire(&slot.on_consume);
        self.launched(now, sid, token, held.client_id, &held.bind, "code")
    }

    /// Poll a terminal-approved request.
    fn launch_request(
        &self,
        slot: &LaunchSlot,
        request_code: &str,
        client_id: &str,
        peer: Option<IpAddr>,
        origin: Option<&str>,
    ) -> Reply {
        let now = (slot.clock)();
        let key = hash(request_code);
        let mut t = slot.table();
        t.prune(now);
        let Some(r) = t.requests.get_mut(&key).filter(|r| {
            loopback(peer)
                && r.client_id == client_id
                && origin.is_some_and(|o| same_origin(&r.origin, o))
        }) else {
            drop(t);
            return self.launch_refused(peer);
        };
        if r.expired(now) {
            return Reply::error(OAuthError::new(
                "expired_token",
                "this sign-in request expired or gave way to a newer one; start again",
            ));
        }
        if let Some(last) = r.last_poll_ms
            && now.saturating_sub(last) < r.interval_ms
        {
            r.interval_ms = r.interval_ms.saturating_add(ms(LAUNCH_SLOW_DOWN_STEP));
            r.last_poll_ms = Some(now);
            return Reply::error(OAuthError::new(
                "slow_down",
                format!("poll at most every {}s", r.interval_ms / 1000),
            ));
        }
        if !r.approved {
            r.last_poll_ms = Some(now);
            return Reply::error(OAuthError::new(
                "authorization_pending",
                "type the code this page shows at the terminal that ran agentd ui",
            ));
        }
        let Ok((sid, token)) = launch_secrets(slot) else {
            return Reply::error(OAuthError::unavailable(
                "no session could be issued right now; poll again",
            ));
        };
        let r = t.requests.remove(&key).expect("held under the same lock");
        drop(t);
        self.launched(
            now,
            sid,
            token,
            r.client_id,
            &LaunchBind::Origin(r.origin),
            "terminal",
        )
    }

    /// A launch presentation refused: `invalid_grant`, one answer for every
    /// reason, counted against its source — and past the limit, 429. Only a
    /// failure is ever counted or refused so: a live code or an approved
    /// request never reaches here, however much junk its source sent first.
    fn launch_refused(&self, peer: Option<IpAddr>) -> Reply {
        if let Some(ip) = peer {
            if let Some(retry) = self.launch_failures.over(ip) {
                return Reply::limited(retry, "too many refused launch sign-ins from this source");
            }
            self.launch_failures.failed(ip);
        }
        Reply::error(OAuthError::new(
            "invalid_grant",
            "the launch code or request is unknown, used, expired, or not this client's",
        ))
    }

    /// Issue the launch session a redemption bought: the operator, named by
    /// its sid, for eight hours when a browser holds it and until revoked or
    /// the launcher exits when a terminal client does — whose token lives only
    /// in that process's memory, so a long-running console is not cut off
    /// daily.
    fn launched(
        &self,
        now: u64,
        sid: String,
        token: String,
        client_id: String,
        bind: &LaunchBind,
        via: &'static str,
    ) -> Reply {
        let ttl = match bind {
            LaunchBind::Origin(_) => Some(crate::runtime::surface::launch::LAUNCH_SESSION_TTL),
            LaunchBind::NoOrigin => None,
        };
        let session = Session {
            sid,
            kind: SessionKind::Launch,
            name: None,
            role: Role::Operator,
            principal: "operator".into(),
            client_id,
            created_ms: now,
            expires_ms: ttl.map(|d| now.saturating_add(ms(d))),
            approved_by: match via {
                "code" => LAUNCHER,
                _ => LAUNCHER_TERMINAL,
            }
            .into(),
            approved_rule: None,
            // The operator is rate-exempt; a launch session is the operator.
            rate: None,
        };
        self.sessions.insert(&token, session.clone());
        let mut body = json!({
            "access_token": token,
            "token_type": "Bearer",
            "scope": "operator",
        });
        if let Some(d) = ttl {
            body["expires_in"] = json!(d.as_secs());
        }
        let mut r = Reply::json(200, body);
        r.notices.push(Notice::Launched {
            session,
            bind: bind.label().to_string(),
            via,
        });
        r
    }

    /// `POST /oauth2/launch_authorization`: a browser tab the launcher
    /// started, which could not be handed the code, asks the person at the
    /// launcher's terminal to sign it in. Only from the launched UI's origin
    /// and a loopback peer; answered with a request code to poll with and a
    /// user code to show. The terminal is the trust anchor: another web
    /// origin cannot start a request, and another local process can start one
    /// but cannot make the person at the terminal type its code.
    pub fn launch_authorization(
        &self,
        content_type: Option<&str>,
        body: &[u8],
        peer: Option<IpAddr>,
        origin: Option<&str>,
    ) -> Reply {
        let Some(slot) = self.launch.as_deref() else {
            return Reply::not_found();
        };
        let Some(slot_origin) = slot.origin() else {
            return Reply::not_found();
        };
        let form = match parse_form(content_type, body) {
            Ok(f) => f,
            Err(e) => return Reply::error(e),
        };
        let Some(client_id) = form.get("client_id").filter(|c| client_id_ok(c)) else {
            return Reply::error(OAuthError::invalid_request(format!(
                "client_id is required: 1-{CLIENT_ID_MAX} characters of A-Z a-z 0-9 . _ -"
            )));
        };
        if !origin.is_some_and(|o| same_origin(slot_origin, o)) {
            return Reply::error(OAuthError::invalid_request(
                "only the web UI agentd ui started may ask its terminal to sign it in",
            ));
        }
        if !loopback(peer) {
            return Reply::error(OAuthError::invalid_request(
                "a launch sign-in is asked for from this host only",
            ));
        }
        let now = (slot.clock)();
        let mut t = slot.table();
        t.prune(now);
        let minted = (|| {
            let request_code = format!("{LAUNCH_REQUEST_PREFIX}{}", (slot.mint)(SECRET_BYTES)?);
            for _ in 0..8 {
                let user_code = mint_user_code(&slot.mint)?;
                if !t.requests.values().any(|r| r.user_code == user_code) {
                    return Ok((request_code, user_code));
                }
            }
            Err(std::io::Error::other("no free user code"))
        })();
        let Ok((request_code, user_code)) = minted else {
            return Reply::error(OAuthError::unavailable(
                "no sign-in request could be issued right now; retry later",
            ));
        };
        // At the bound, the oldest request still waiting gives way. It is
        // expired, not forgotten, so its tab is told to start again.
        let waiting: Vec<(u64, String, String)> = t
            .requests
            .iter()
            .filter(|(_, r)| !r.expired(now) && !r.approved)
            .map(|(k, r)| (r.requested_ms, r.user_code.clone(), k.clone()))
            .collect();
        if waiting.len() >= LAUNCH_PENDING_MAX
            && let Some((_, _, oldest)) = waiting.into_iter().min()
            && let Some(r) = t.requests.get_mut(&oldest)
        {
            r.expires_ms = now;
        }
        t.requests.insert(
            hash(&request_code),
            LaunchRequest {
                user_code: user_code.clone(),
                client_id: client_id.to_string(),
                origin: slot_origin.to_string(),
                requested_ms: now,
                expires_ms: now.saturating_add(ms(LAUNCH_REQUEST_TTL)),
                interval_ms: ms(LAUNCH_POLL_INTERVAL),
                last_poll_ms: None,
                approved: false,
            },
        );
        drop(t);
        LaunchSlot::fire(&slot.on_request);
        let mut r = Reply::json(
            200,
            json!({
                "request_code": request_code,
                "user_code": display_user_code(&user_code),
                "expires_in": LAUNCH_REQUEST_TTL.as_secs(),
                "interval": LAUNCH_POLL_INTERVAL.as_secs(),
            }),
        );
        r.notices
            .push(Notice::LaunchRequested(client_id.to_string()));
        r
    }

    /// `POST /oauth2/revoke` (RFC 7009): end the session a token belongs to.
    /// Always 200 with no body for a well-formed request — a token that
    /// names no session is already as revoked as it can be, and saying which
    /// tokens exist would be an oracle.
    pub fn revoke(&self, content_type: Option<&str>, body: &[u8]) -> Reply {
        revoke_with(&self.sessions, content_type, body)
    }

    /// `GET /.well-known/oauth-authorization-server` (RFC 8414). Served with
    /// the device grant only; it names the launch grant too while a slot is
    /// installed, since the token endpoint then redeems it.
    pub fn metadata(&self) -> Reply {
        let Some(device) = &self.device else {
            return Reply::not_found();
        };
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
                "grant_types_supported": std::iter::once(DEVICE_CODE_GRANT)
                    .chain(self.launch.as_ref().map(|_| crate::runtime::surface::launch::LAUNCH_GRANT_TYPE))
                    .collect::<Vec<_>>(),
                "response_types_supported": [],
                "token_endpoint_auth_methods_supported": ["none"],
                "revocation_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": device.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            }),
        )
    }

    /// `GET /oauth2/device`: where a code's person is sent. Fixed text: the
    /// approval is an operator's op, not a page a stranger can submit.
    pub fn verification(&self) -> Reply {
        if self.device.is_none() {
            return Reply::not_found();
        }
        Reply {
            status: 200,
            body: ReplyBody::Text(VERIFICATION_TEXT),
            retry_after: None,
            notices: Vec::new(),
        }
    }
}

/// A launch session's sid and token, from the slot's entropy source.
fn launch_secrets(slot: &LaunchSlot) -> std::io::Result<(String, String)> {
    let sid = format!("{LAUNCH_SID_PREFIX}{}", (slot.mint)(SID_BYTES)?);
    let token = format!("{SESSION_TOKEN_PREFIX}{}", (slot.mint)(SECRET_BYTES)?);
    Ok((sid, token))
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
            let cfg: settings::DeviceGrant = serde_json::from_value(cfg).unwrap();
            let sessions = Arc::new(Sessions::new(Arc::clone(&clock)));
            let auth = Authority::new(Some(DeviceGrant::new(&cfg, clock, mint)), None, sessions);
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
            self.auth.token(
                FORM,
                body.as_bytes(),
                Some("10.0.0.1".parse().unwrap()),
                None,
            )
        }

        fn approve(&self, user_code: &str, name: &str, scope: Option<&str>) {
            let view = self.auth.device().unwrap().find(user_code).unwrap();
            let scope = self
                .auth
                .device()
                .unwrap()
                .grant_scope(view.requested, scope)
                .unwrap();
            self.auth
                .device()
                .unwrap()
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

    /// The feed's `auth` schema, as the FeedKind contract states it:
    /// `{event, client_id, scope, user_code?, peer?, sid?, name?}`, every
    /// value a string and nothing else — so the feed's own schema check can
    /// never trip on an event this module builds.
    fn meets_the_auth_contract(v: &Value) {
        let o = v
            .as_object()
            .unwrap_or_else(|| panic!("not an object: {v}"));
        assert!(
            ["pending", "approved", "denied", "revoked", "launch"]
                .contains(&o.get("event").and_then(Value::as_str).unwrap_or("")),
            "event: {v}"
        );
        for required in ["client_id", "scope"] {
            assert!(
                o.get(required).is_some_and(Value::is_string),
                "{required}: {v}"
            );
        }
        for (k, val) in o {
            assert!(
                [
                    "event",
                    "client_id",
                    "scope",
                    "user_code",
                    "peer",
                    "sid",
                    "name"
                ]
                .contains(&k.as_str()),
                "{k} is not in the contract: {v}"
            );
            assert!(val.is_string(), "{k} is a string when present: {v}");
        }
    }

    #[test]
    fn every_auth_event_meets_the_feed_contract() {
        let f = Fixture::new(json!({"enabled": true, "scopes": ["user", "operator"]}));
        // pending: from a peer, and (as a unix-socket-less listener never
        // is, but the builder must not care) from none.
        let r = f.authorize_from("10.0.0.7", "client_id=cli&scope=operator");
        let [Notice::Pending(pending)] = &r.notices[..] else {
            panic!("one pending notice: {:?}", r.notices);
        };
        meets_the_auth_contract(pending);
        assert_eq!(pending["scope"], "operator", "{pending}");
        assert_eq!(pending["peer"], "10.0.0.7", "{pending}");
        let mut quiet = f.auth.device().unwrap().pending()[0].clone();
        quiet.peer = None;
        meets_the_auth_contract(&quiet.event("pending"));

        // approved: the granted scope and the name.
        let view = f.auth.device().unwrap().pending()[0].clone();
        let approved = view.approved_event("alice", DeviceScope::User);
        meets_the_auth_contract(&approved);
        assert_eq!(
            (&approved["scope"], &approved["name"]),
            (&json!("user"), &json!("alice"))
        );

        // denied: every one refused, each with who asked and for what.
        f.code("10.0.0.8", "client_id=other");
        let denied = f.auth.device().unwrap().deny(None).unwrap();
        assert_eq!(denied.len(), 2);
        for d in &denied {
            let e = d.event("denied");
            meets_the_auth_contract(&e);
            assert_eq!(e["event"], "denied");
        }
        let mut who: Vec<Value> = denied
            .iter()
            .map(|d| d.event("denied")["client_id"].clone())
            .collect();
        who.sort_by_key(Value::to_string);
        assert_eq!(who, [json!("cli"), json!("other")]);

        // revoked: a named session, and one with no name (a launch's).
        f.session("10.0.0.9", "bob");
        let ended = f.auth.sessions().revoke(&Revoke::Name("bob".into()));
        let [s] = &ended[..] else {
            panic!("one session: {ended:?}")
        };
        let revoked = s.revoked_event();
        meets_the_auth_contract(&revoked);
        assert_eq!(
            (&revoked["sid"], &revoked["name"], &revoked["scope"]),
            (&json!(s.sid), &json!("bob"), &json!("user"))
        );
        let nameless = Session {
            name: None,
            ..s.clone()
        };
        meets_the_auth_contract(&nameless.revoked_event());
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
            f.auth.device().unwrap().deny(Some(&shown)).unwrap();
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
            f.auth.device().unwrap().find(&lower).unwrap().user_code,
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
        let pending = format!("{:?}", f.auth.device().unwrap().table());
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
        f.auth.device().unwrap().deny(Some(&uc)).unwrap();
        assert_eq!(poll(&dc, "cli"), e("access_denied"));
        // Expired: told so, then forgotten.
        let (dc2, _) = f.code("10.0.0.2", "client_id=cli");
        f.advance(Duration::from_secs(121));
        assert_eq!(poll(&dc2, "cli"), e("expired_token"));
        assert_eq!(poll(&dc2, "cli"), e("invalid_grant"));
        // The form's own refusals come first.
        let token = |body: &str| {
            let r = f.auth.token(
                FORM,
                body.as_bytes(),
                Some("10.0.0.3".parse().unwrap()),
                None,
            );
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
            .device()
            .unwrap()
            .find(&both.code("10.0.0.1", "client_id=cli").1)
            .unwrap();
        assert_eq!(v.requested, DeviceScope::User);
        // The device asked for operator; the approver must say so too.
        let (dc, uc) = both.code("10.0.0.2", "client_id=cli&scope=operator");
        let asked = both.auth.device().unwrap().find(&uc).unwrap().requested;
        assert_eq!(asked, DeviceScope::Operator);
        assert!(
            both.auth
                .device()
                .unwrap()
                .grant_scope(asked, None)
                .is_err()
        );
        // The approver may narrow it to user…
        assert_eq!(
            both.auth.device().unwrap().grant_scope(asked, Some("user")),
            Ok(DeviceScope::User)
        );
        // …never widen a user request.
        assert!(
            both.auth
                .device()
                .unwrap()
                .grant_scope(DeviceScope::User, Some("operator"))
                .is_err()
        );
        assert!(
            both.auth
                .device()
                .unwrap()
                .grant_scope(asked, Some("agent"))
                .is_err()
        );
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
                .device()
                .unwrap()
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
        assert_eq!(f.auth.device().unwrap().pending().len(), PENDING_GLOBAL);
        // Denying frees a slot.
        let first = f.auth.device().unwrap().pending()[0].user_code.clone();
        f.auth.device().unwrap().deny(Some(&first)).unwrap();
        f.code("10.3.0.1", "client_id=cli");
        // At the cap a source holding fewer takes from one holding the most,
        // but never past an even share: at 3 against 4 it would only swap.
        f.code("10.3.0.1", "client_id=cli");
        f.code("10.3.0.1", "client_id=cli");
        let r = f.authorize_from("10.3.0.1", "client_id=cli");
        assert_eq!(r.status, 429, "even shares over the global cap: {r:?}");
        assert_eq!(f.auth.device().unwrap().pending().len(), PENDING_GLOBAL);
        // Expiry frees every slot.
        f.advance(Duration::from_secs(11 * 60));
        assert!(f.auth.device().unwrap().pending().is_empty());
        f.code("10.3.0.2", "client_id=cli");
    }

    /// One IPv6 allocation is one party: the /64s under a /48 share a quarter
    /// of the table between them, however many of them ask.
    #[test]
    fn one_v6_allocation_cannot_fill_the_table() {
        let f = Fixture::new(json!({"enabled": true}));
        let mut admitted = 0;
        for s in 0..(PENDING_GLOBAL / PENDING_PER_SOURCE) {
            for _ in 0..PENDING_PER_SOURCE {
                let r = f.authorize_from(&format!("2001:db8:7:{s:x}::1"), "client_id=cli");
                admitted += usize::from(r.status == 200);
            }
        }
        assert_eq!(admitted, PENDING_PER_NETWORK);
        let r = f.authorize_from("2001:db8:7:ff::1", "client_id=cli");
        assert_eq!(r.status, 429, "a fresh /64 of the same /48: {r:?}");
        // Another allocation is untouched by it.
        f.code("2001:db8:8::1", "client_id=cli");
    }

    /// A flood from many networks fills the table; a party asking for the
    /// first time still gets a code, and the flood gives way — the oldest
    /// waiting code of the network holding the most, whose device is told its
    /// code expired. An approved code never gives way.
    #[test]
    fn a_newcomer_displaces_the_biggest_holder_at_the_global_cap() {
        let f = Fixture::new(json!({"enabled": true}));
        let networks = PENDING_GLOBAL / PENDING_PER_NETWORK;
        // Each network's first code, oldest network first.
        let mut firsts = Vec::new();
        for n in 0..networks {
            for s in 0..(PENDING_PER_NETWORK / PENDING_PER_SOURCE) {
                for i in 0..PENDING_PER_SOURCE {
                    let code = f.code(&format!("2001:db8:{n:x}:{s:x}::1"), "client_id=cli");
                    if s == 0 && i == 0 {
                        firsts.push(code);
                    }
                    f.advance(Duration::from_millis(1));
                }
            }
        }
        assert_eq!(f.auth.device().unwrap().pending().len(), PENDING_GLOBAL);
        // The very oldest is approved: the operator decided, so it stays, and
        // its network now holds fewer waiting codes than the others.
        let (approved_dc, approved_uc) = firsts[0].clone();
        f.approve(&approved_uc, "alice", None);
        f.code("198.51.100.7", "client_id=newcomer");
        assert_eq!(
            f.auth.device().unwrap().pending().len() + 1,
            PENDING_GLOBAL,
            "one waiting code gave way, and the approved one still holds its slot"
        );
        // The flood only displaces itself: asking again from the network that
        // just gave way takes nothing from anyone.
        let r = f.authorize_from("2001:db8:1:0::1", "client_id=cli");
        assert_eq!(r.status, 429, "{r:?}");
        f.advance(POLL_INTERVAL);
        assert_eq!(f.poll(&approved_dc, "cli").status, 200, "approved survives");
        // The one that gave way: the oldest code of the networks holding the
        // most — network 1's first — and its device is told it expired.
        let (displaced_dc, _) = &firsts[1];
        let r = f.poll(displaced_dc, "cli");
        assert_eq!(error_of(&r), "expired_token", "{r:?}");
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
        assert!(
            f.auth.device().unwrap().pending().is_empty(),
            "nothing recorded"
        );

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
        let cfg: settings::DeviceGrant = serde_json::from_value(json!({"enabled": true})).unwrap();
        let unsettled = Authority::new(
            Some(DeviceGrant::new(&cfg, system_clock(), os_mint())),
            None,
            sessions,
        );
        assert_eq!(unsettled.metadata().status, 503);
    }

    // ---- the launch grant ------------------------------------------------

    use crate::runtime::surface::launch::{LAUNCH_CODE_TTL, LAUNCH_GRANT_TYPE, LAUNCH_SESSION_TTL};

    const UI: &str = "http://127.0.0.1:4555";
    const OTHER_UI: &str = "http://127.0.0.1:4556";
    const LOCAL: &str = "127.0.0.1";

    /// A listener's authority with a launcher's slot, on one injected clock
    /// and entropy source, and — when `device` — the device grant beside it.
    struct Launch {
        now: Arc<AtomicU64>,
        slot: Arc<LaunchSlot>,
        auth: Authority,
    }

    impl Launch {
        fn new(origin: Option<&str>, device: bool) -> Launch {
            Launch::with_mint(origin, device, os_mint())
        }

        fn with_mint(origin: Option<&str>, device: bool, mint: Mint) -> Launch {
            let now = Arc::new(AtomicU64::new(1_000_000));
            let t = Arc::clone(&now);
            let clock: Clock = Arc::new(move || t.load(Ordering::SeqCst));
            let slot =
                Arc::new(LaunchSlot::with(origin, Arc::clone(&clock), Arc::clone(&mint)).unwrap());
            let cfg: settings::DeviceGrant =
                serde_json::from_value(json!({"enabled": true})).unwrap();
            let auth = Authority::new(
                device.then(|| DeviceGrant::new(&cfg, Arc::clone(&clock), mint)),
                Some(Arc::clone(&slot)),
                Arc::new(Sessions::new(clock)),
            );
            auth.set_issuer("http://127.0.0.1:8420");
            Launch { now, slot, auth }
        }

        fn advance(&self, d: Duration) {
            self.now.fetch_add(ms(d), Ordering::SeqCst);
        }

        /// Present a code as `client`, from `peer`, with `origin`.
        fn exchange_from(
            &self,
            peer: &str,
            code: &str,
            client: &str,
            origin: Option<&str>,
        ) -> Reply {
            let body = format!(
                "grant_type={}&code={code}&client_id={client}",
                enc(LAUNCH_GRANT_TYPE)
            );
            self.auth
                .token(FORM, body.as_bytes(), Some(peer.parse().unwrap()), origin)
        }

        fn exchange(&self, code: &str, client: &str, origin: Option<&str>) -> Reply {
            self.exchange_from(LOCAL, code, client, origin)
        }

        /// Ask the terminal to sign a tab in, from `origin`.
        fn request(&self, origin: Option<&str>) -> Reply {
            self.auth.launch_authorization(
                FORM,
                b"client_id=agentd-ui",
                Some(LOCAL.parse().unwrap()),
                origin,
            )
        }

        /// A request from the launched UI: `(request_code, user_code)`.
        fn requested(&self) -> (String, String) {
            let r = self.request(Some(UI));
            let v = json_of(&r);
            assert_eq!(r.status, 200, "{v}");
            (
                v["request_code"].as_str().unwrap().to_string(),
                v["user_code"].as_str().unwrap().to_string(),
            )
        }

        fn poll_request(&self, request_code: &str) -> Reply {
            self.poll_request_from(LOCAL, request_code)
        }

        /// Poll a request from `peer`, as the launched UI.
        fn poll_request_from(&self, peer: &str, request_code: &str) -> Reply {
            let body = format!(
                "grant_type={}&request_code={request_code}&client_id=agentd-ui",
                enc(LAUNCH_GRANT_TYPE)
            );
            self.auth
                .token(FORM, body.as_bytes(), Some(peer.parse().unwrap()), Some(UI))
        }
    }

    fn enc(v: &str) -> String {
        v.replace(':', "%3A").replace('/', "%2F")
    }

    fn token_of(r: &Reply) -> String {
        let v = json_of(r);
        assert_eq!(r.status, 200, "{v}");
        v["access_token"].as_str().unwrap().to_string()
    }

    fn refused(r: &Reply) {
        assert_eq!(
            (r.status, error_of(r)),
            (400, "invalid_grant".to_string()),
            "{:?}",
            r.body
        );
    }

    #[test]
    fn launch_codes_are_single_use_short_lived_and_bound() {
        let f = Launch::new(Some(UI), false);
        let bind = || LaunchBind::Origin(UI.into());
        // Exchanged once with its origin and client…
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        token_of(&f.exchange(&code, "agentd-ui", Some(UI)));
        // …and never again.
        refused(&f.exchange(&code, "agentd-ui", Some(UI)));
        // Past its lifetime it is worthless.
        let late = f.slot.issue(bind(), "agentd-ui").unwrap();
        f.advance(LAUNCH_CODE_TTL + Duration::from_secs(1));
        refused(&f.exchange(&late, "agentd-ui", Some(UI)));
        // Another origin is refused — and the code is burned by asking, so
        // the right presentation that follows is refused too.
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        refused(&f.exchange(&code, "agentd-ui", Some(OTHER_UI)));
        refused(&f.exchange(&code, "agentd-ui", Some(UI)));
        // No origin at all, for an origin-bound code; another client.
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        refused(&f.exchange(&code, "agentd-ui", None));
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        refused(&f.exchange(&code, "someone-else", Some(UI)));
        // The origin is compared as an origin, not as text.
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        token_of(&f.exchange(&code, "agentd-ui", Some("HTTP://127.0.0.1:4555")));
        // A code for a terminal client is no browser's: any Origin — `null`
        // included — is refused.
        for origin in [UI, "null"] {
            let code = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
            refused(&f.exchange(&code, "agentd-tui", Some(origin)));
        }
        let code = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        token_of(&f.exchange(&code, "agentd-tui", None));
        // Issuing again for a bind invalidates its earlier code, and only its.
        let tui = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        let first = f.slot.issue(bind(), "agentd-ui").unwrap();
        let second = f.slot.issue(bind(), "agentd-ui").unwrap();
        refused(&f.exchange(&first, "agentd-ui", Some(UI)));
        token_of(&f.exchange(&second, "agentd-ui", Some(UI)));
        token_of(&f.exchange(&tui, "agentd-tui", None));
        // 256 bits, and only the hash is kept.
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        let hex = code.strip_prefix(LAUNCH_CODE_PREFIX).expect("the prefix");
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let table = format!("{:?}", f.slot);
        assert!(!table.contains(hex), "the code is kept in the clear");
        assert!(table.contains(&hash(&code)));
    }

    #[test]
    fn launch_sessions_are_operator_and_revocable() {
        let f = Launch::new(Some(UI), false);
        // A web UI's session: the operator, for eight hours.
        let code = f
            .slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let r = f.exchange(&code, "agentd-ui", Some(UI));
        let v = json_of(&r);
        assert_eq!(v["expires_in"], LAUNCH_SESSION_TTL.as_secs(), "{v}");
        assert_eq!(v["expires_in"], 28800);
        assert_eq!(
            (&v["scope"], &v["token_type"]),
            (&json!("operator"), &json!("Bearer"))
        );
        assert!(v.get("refresh_token").is_none());
        let ui = token_of(&r);
        assert!(ui.starts_with(SESSION_TOKEN_PREFIX));
        let p = principal_of(&f.auth, &ui);
        assert_eq!((p.id.as_str(), p.role), ("operator", Role::Operator));
        assert_eq!(p.grants, ["*"]);
        assert_eq!(p.rate, None, "the operator is rate-exempt");
        let sid = p.session.clone().unwrap();
        assert!(
            sid.starts_with(LAUNCH_SID_PREFIX) && sid.len() == 3 + 16,
            "{sid}"
        );
        // What the exchange tells the operator: the sid, never the code.
        let [Notice::Launched { session, bind, via }] = &r.notices[..] else {
            panic!("one launch notice: {:?}", r.notices);
        };
        assert_eq!((bind.as_str(), *via), (UI, "code"));
        let event = session.launched_event();
        meets_the_auth_contract(&event);
        assert_eq!(
            event,
            json!({"event": "launch", "sid": sid, "client_id": "agentd-ui", "scope": "operator"})
        );
        // A terminal client's session has no expiry of its own.
        let code = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        let r = f.exchange(&code, "agentd-tui", None);
        assert!(json_of(&r).get("expires_in").is_none(), "{:?}", r.body);
        let tui = token_of(&r);
        f.advance(Duration::from_secs(30 * 24 * 3600));
        assert!(matches!(
            f.auth.sessions().verify(&tui),
            SessionCheck::Valid(_)
        ));
        assert_eq!(
            f.auth.sessions().verify(&ui),
            SessionCheck::Invalid,
            "the web UI's session ended after eight hours"
        );
        // Listed as a launch, approved by the launcher, with no name.
        let listed = f.auth.sessions().list();
        let [s] = &listed[..] else {
            panic!("the terminal session alone: {listed:?}")
        };
        let row = s.view();
        assert_eq!(
            (
                &row["kind"],
                &row["approved_by"],
                &row["principal"],
                &row["role"]
            ),
            (
                &json!("launch"),
                &json!("launcher"),
                &json!("operator"),
                &json!("operator")
            )
        );
        assert!(row.get("name").is_none(), "{row}");
        assert_eq!(row["expires_at"], Value::Null);
        // auth.sessions.revoke {sid} ends it.
        let tui_sid = s.sid.clone();
        assert_eq!(f.auth.sessions().revoke(&Revoke::Sid(tui_sid)).len(), 1);
        assert_eq!(f.auth.sessions().verify(&tui), SessionCheck::Invalid);
        // …and so does /oauth2/revoke, the page's disconnect.
        let code = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        let token = token_of(&f.exchange(&code, "agentd-tui", None));
        let r = f.auth.revoke(FORM, format!("token={token}").as_bytes());
        assert!(matches!(&r.notices[..], [Notice::Revoked(s)] if s.kind == SessionKind::Launch));
        assert_eq!(f.auth.sessions().verify(&token), SessionCheck::Invalid);
    }

    #[test]
    fn terminal_approved_requests() {
        let f = Launch::new(Some(UI), false);
        let (request, user_code) = f.requested();
        assert!(request.starts_with(LAUNCH_REQUEST_PREFIX) && request.len() == 10 + 64);
        assert_eq!(user_code.len(), USER_CODE_LEN + 1, "{user_code}");
        assert!(normalize_user_code(&user_code).is_some(), "{user_code}");
        let r = f.request(Some(UI));
        let v = json_of(&r);
        assert_eq!((&v["expires_in"], &v["interval"]), (&json!(120), &json!(2)));
        assert_eq!(
            r.notices,
            [Notice::LaunchRequested("agentd-ui".into())],
            "the request is logged by its client, never its codes"
        );
        // Waiting on the terminal; polled too fast, told to slow down.
        assert_eq!(error_of(&f.poll_request(&request)), "authorization_pending");
        f.advance(Duration::from_secs(1));
        let r = f.poll_request(&request);
        assert_eq!(error_of(&r), "slow_down");
        assert!(
            json_of(&r)["error_description"]
                .as_str()
                .unwrap()
                .contains("4s")
        );
        // A code nobody's tab shows approves nothing.
        let bare = normalize_user_code(&user_code).unwrap();
        let other: String = std::iter::once(if bare.starts_with('B') { 'C' } else { 'B' })
            .chain(bare.chars().skip(1))
            .collect();
        assert!(!f.slot.approve_user_code(&other));
        assert!(!f.slot.approve_user_code("not a code"));
        assert!(!f.slot.approve_user_code(""));
        f.advance(Duration::from_secs(4));
        assert_eq!(
            error_of(&f.poll_request(&request)),
            "authorization_pending",
            "a wrong code approved the request"
        );
        // Typed lower-case, without the dash: exactly that request.
        let typed = bare.to_ascii_lowercase();
        assert!(f.slot.approve_user_code(&typed));
        assert!(!f.slot.approve_user_code(&typed), "approved once");
        f.advance(Duration::from_secs(4));
        let r = f.poll_request(&request);
        let token = token_of(&r);
        assert!(
            json_of(&r).get("expires_in").is_some(),
            "a browser's session expires"
        );
        let p = principal_of(&f.auth, &token);
        assert_eq!((p.id.as_str(), p.role), ("operator", Role::Operator));
        let [Notice::Launched { session, via, .. }] = &r.notices[..] else {
            panic!("one launch notice: {:?}", r.notices)
        };
        assert_eq!(
            (session.approved_by.as_str(), *via),
            ("launcher-terminal", "terminal")
        );
        // Redeemed once.
        f.advance(Duration::from_secs(4));
        refused(&f.poll_request(&request));
        // Only the launched UI may ask, and only it may poll.
        let r = f.request(Some(OTHER_UI));
        assert_eq!(error_of(&r), "invalid_request", "{:?}", r.body);
        let r = f.request(None);
        assert_eq!(error_of(&r), "invalid_request");
        let (req, _) = f.requested();
        let body = format!(
            "grant_type={}&request_code={req}&client_id=agentd-ui",
            enc(LAUNCH_GRANT_TYPE)
        );
        let from = |origin| {
            f.auth
                .token(FORM, body.as_bytes(), Some(LOCAL.parse().unwrap()), origin)
        };
        refused(&from(Some(OTHER_UI)));
        refused(&from(None));
        // The seventeenth waiting request displaces the oldest, whose tab is
        // told it expired.
        let g = Launch::new(Some(UI), false);
        let (oldest, _) = g.requested();
        for _ in 1..LAUNCH_PENDING_MAX {
            g.advance(Duration::from_millis(1));
            g.requested();
        }
        assert_eq!(error_of(&g.poll_request(&oldest)), "authorization_pending");
        g.advance(Duration::from_millis(1));
        let (newest, _) = g.requested();
        assert_eq!(error_of(&g.poll_request(&oldest)), "expired_token");
        assert_eq!(error_of(&g.poll_request(&newest)), "authorization_pending");
        // And a request left waiting expires on its own.
        g.advance(LAUNCH_REQUEST_TTL);
        assert_eq!(error_of(&g.poll_request(&newest)), "expired_token");
        // A terminal client's slot takes no requests at all.
        let tui = Launch::new(None, false);
        assert_eq!(tui.request(Some(UI)).status, 404);
    }

    /// The limiter throttles failing, never redeeming: a flood of junk and
    /// refused asks from 127.0.0.1 — enough to be answered 429 — leaves a
    /// live code and an approved request redeemable at once, and the device
    /// grant's own bucket untouched.
    #[test]
    fn launch_failures_never_block_a_valid_exchange() {
        let f = Launch::new(Some(UI), true);
        let code = f
            .slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        let (request, user_code) = f.requested();
        let mut limited = 0;
        for i in 0..200 {
            let r = f.exchange(
                &format!("{LAUNCH_CODE_PREFIX}{i:064x}"),
                "agentd-ui",
                Some(UI),
            );
            assert!(matches!(r.status, 400 | 429), "{:?}", r.body);
            limited += usize::from(r.status == 429);
            let r = f.request(Some(OTHER_UI));
            assert_eq!(error_of(&r), "invalid_request");
        }
        assert!(limited > 100, "junk was never limited: {limited}");
        assert!(f.slot.approve_user_code(&user_code));
        token_of(&f.exchange(&code, "agentd-ui", Some(UI)));
        token_of(&f.poll_request(&request));
        // The device grant's bucket is the device grant's alone.
        let body = format!(
            "grant_type={}&device_code=x&client_id=cli",
            enc(DEVICE_CODE_GRANT)
        );
        let r = f
            .auth
            .token(FORM, body.as_bytes(), Some(LOCAL.parse().unwrap()), None);
        assert_eq!(error_of(&r), "invalid_grant", "{:?}", r.body);
    }

    #[test]
    fn consuming_a_code_fires_on_consume() {
        let f = Launch::new(Some(UI), false);
        let fired = Arc::new(AtomicU64::new(0));
        let n = Arc::clone(&fired);
        f.slot.on_consume(move || {
            n.fetch_add(1, Ordering::SeqCst);
        });
        let count = || fired.load(Ordering::SeqCst);
        let bind = || LaunchBind::Origin(UI.into());
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        token_of(&f.exchange(&code, "agentd-ui", Some(UI)));
        assert_eq!(count(), 1, "a redemption");
        let code = f.slot.issue(bind(), "agentd-ui").unwrap();
        refused(&f.exchange(&code, "agentd-ui", Some(OTHER_UI)));
        assert_eq!(count(), 2, "a burned presentation");
        // Nothing left to consume: an unknown or spent code fires nothing.
        refused(&f.exchange(&code, "agentd-ui", Some(UI)));
        refused(&f.exchange("agentd_lc_unknown", "agentd-ui", Some(UI)));
        assert_eq!(count(), 2);
        // A request tells the launcher to ask its terminal.
        let asked = Arc::new(AtomicU64::new(0));
        let a = Arc::clone(&asked);
        f.slot.on_request(move || {
            a.fetch_add(1, Ordering::SeqCst);
        });
        f.requested();
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// What the launcher's one prompt counts: requests still waiting — not
    /// one approved, expired or displaced.
    #[test]
    fn waiting_counts_the_requests_the_terminal_can_still_approve() {
        let f = Launch::new(Some(UI), false);
        assert_eq!(f.slot.waiting(), 0);
        let (_, first) = f.requested();
        f.requested();
        assert_eq!(f.slot.waiting(), 2);
        assert!(f.slot.approve_user_code(&first));
        assert_eq!(f.slot.waiting(), 1, "an approved request waits no more");
        for _ in 0..2 * LAUNCH_PENDING_MAX {
            f.requested();
        }
        assert_eq!(
            f.slot.waiting(),
            LAUNCH_PENDING_MAX,
            "displaced ones do not count"
        );
        f.advance(LAUNCH_REQUEST_TTL);
        assert_eq!(f.slot.waiting(), 0, "nor expired ones");
    }

    /// A code or a request presented from off the host is refused, and the
    /// code is burned by it. A terminal-approved request is not: it is the
    /// tab's to poll, and a remote poll neither redeems nor spends it.
    #[test]
    fn launch_grants_are_redeemed_from_loopback_only() {
        let f = Launch::new(Some(UI), false);
        let code = f
            .slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        refused(&f.exchange_from("10.1.2.3", &code, "agentd-ui", Some(UI)));
        refused(&f.exchange(&code, "agentd-ui", Some(UI)));
        let code = f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").unwrap();
        token_of(&f.exchange_from("::1", &code, "agentd-tui", None));
        let r = f.auth.launch_authorization(
            FORM,
            b"client_id=agentd-ui",
            Some("10.1.2.3".parse().unwrap()),
            Some(UI),
        );
        assert_eq!(error_of(&r), "invalid_request");
        // Approved at the terminal, then polled from off the host: refused,
        // and no session is issued for it.
        let (request, user_code) = f.requested();
        assert!(f.slot.approve_user_code(&user_code));
        let before = f.auth.sessions.list().len();
        refused(&f.poll_request_from("10.1.2.3", &request));
        assert_eq!(f.auth.sessions.list().len(), before, "no session");
        token_of(&f.poll_request_from("::1", &request));
    }

    /// A failing entropy source answers 503 and costs nothing: no code, no
    /// request, no session is issued, no code is burned, the launcher hears
    /// of nothing, and the refusal is not counted against the source.
    #[test]
    fn a_failed_launch_mint_is_503_and_burns_nothing() {
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&fail);
        let mint: Mint = Arc::new(move |n| {
            if flag.load(Ordering::SeqCst) {
                Err(std::io::Error::other("no entropy"))
            } else {
                crate::sec::random::hex_token(n)
            }
        });
        let f = Launch::with_mint(Some(UI), false, mint);
        let consumed = Arc::new(AtomicU64::new(0));
        let c = Arc::clone(&consumed);
        f.slot.on_consume(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let asked = Arc::new(AtomicU64::new(0));
        let a = Arc::clone(&asked);
        f.slot.on_request(move || {
            a.fetch_add(1, Ordering::SeqCst);
        });
        let code = f
            .slot
            .issue(LaunchBind::Origin(UI.into()), "agentd-ui")
            .unwrap();
        fail.store(true, Ordering::SeqCst);
        assert!(f.slot.issue(LaunchBind::NoOrigin, "agentd-tui").is_err());
        for _ in 0..(LAUNCH_FAILURE_BURST * 2) {
            let r = f.exchange(&code, "agentd-ui", Some(UI));
            assert_eq!(
                (r.status, error_of(&r)),
                (503, "temporarily_unavailable".to_string())
            );
            assert!(r.notices.is_empty());
        }
        let r = f.request(Some(UI));
        assert_eq!(
            (r.status, error_of(&r)),
            (503, "temporarily_unavailable".to_string())
        );
        assert!(r.notices.is_empty());
        assert!(f.auth.sessions().list().is_empty(), "no session issued");
        assert_eq!(consumed.load(Ordering::SeqCst), 0, "no code burned");
        assert_eq!(asked.load(Ordering::SeqCst), 0, "no request made");
        assert!(
            format!("{:?}", f.slot).contains("requests: {}"),
            "{:?}",
            f.slot
        );
        fail.store(false, Ordering::SeqCst);
        token_of(&f.exchange(&code, "agentd-ui", Some(UI)));
        assert_eq!(consumed.load(Ordering::SeqCst), 1);
    }

    /// Which grants the token endpoint redeems follows what is installed,
    /// and the metadata says so.
    #[test]
    fn the_token_endpoint_redeems_the_installed_grants() {
        let only_launch = Launch::new(None, false);
        let body = format!(
            "grant_type={}&device_code=x&client_id=cli",
            enc(DEVICE_CODE_GRANT)
        );
        let r = only_launch
            .auth
            .token(FORM, body.as_bytes(), Some(LOCAL.parse().unwrap()), None);
        assert_eq!(error_of(&r), "unsupported_grant_type");
        assert_eq!(only_launch.auth.metadata().status, 404);
        assert_eq!(only_launch.auth.verification().status, 404);
        assert_eq!(
            only_launch
                .auth
                .device_authorization(FORM, b"client_id=cli", Some(LOCAL.parse().unwrap()))
                .status,
            404
        );
        // Exactly one of code and request_code.
        for body in [
            format!("grant_type={}&client_id=agentd-tui", enc(LAUNCH_GRANT_TYPE)),
            format!(
                "grant_type={}&code=a&request_code=b&client_id=agentd-tui",
                enc(LAUNCH_GRANT_TYPE)
            ),
            format!("grant_type={}&code=a", enc(LAUNCH_GRANT_TYPE)),
        ] {
            let r =
                only_launch
                    .auth
                    .token(FORM, body.as_bytes(), Some(LOCAL.parse().unwrap()), None);
            assert_eq!(error_of(&r), "invalid_request", "{body}");
        }
        let both = Launch::new(Some(UI), true);
        let v = json_of(&both.auth.metadata());
        assert_eq!(
            v["grant_types_supported"],
            json!([DEVICE_CODE_GRANT, LAUNCH_GRANT_TYPE])
        );
        // The device grant alone does not redeem a launch code.
        let f = Fixture::new(json!({"enabled": true}));
        let body = format!("grant_type={}&code=x&client_id=cli", enc(LAUNCH_GRANT_TYPE));
        let r = f
            .auth
            .token(FORM, body.as_bytes(), Some(LOCAL.parse().unwrap()), None);
        assert_eq!(error_of(&r), "unsupported_grant_type");
    }

    #[test]
    fn the_launched_origin_is_admitted_once_and_only_with_a_slot() {
        let configured = vec!["https://ui.example".to_string()];
        assert_eq!(admitted_origins(&configured, None), configured);
        let tui = LaunchSlot::new(None).unwrap();
        assert_eq!(admitted_origins(&configured, Some(&tui)), configured);
        let ui = LaunchSlot::new(Some(UI)).unwrap();
        assert_eq!(
            admitted_origins(&configured, Some(&ui)),
            ["https://ui.example", UI]
        );
        let already = vec!["http://127.0.0.1:4555".to_string()];
        assert_eq!(admitted_origins(&already, Some(&ui)), already);
        assert!(LaunchSlot::new(Some("127.0.0.1:4555")).is_err());
    }
}
