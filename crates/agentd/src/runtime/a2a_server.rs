// SPDX-License-Identifier: Apache-2.0
//! The **A2A v2 transport binding** (RFC 0029): the HTTPS listener that turns
//! A2A requests into runtime work, and the durable-task lifecycle behind it.
//!
//! Two halves meet here. The **transport** ([`A2aAuth`], [`A2aHandler`]) runs
//! on the framework's per-connection threads: it resolves the caller to a
//! [`Principal`], enforces the authorization matrix, and posts each request to
//! the single-writer loop as [`Event::A2a`], blocking on a per-request oneshot.
//! The **binding** (`impl Runtime`) runs on the loop: it creates/advances
//! durable [`Task`]s, routes natural-language messages to conversation turns
//! and command DataParts to the registry, and answers `GetTask`/`ListTasks`/
//! `CancelTask` and the operator admin family. Reads that must not stall the
//! loop — a blocking `SendMessage`, a stream — are served by the transport
//! thread polling a **shared task-snapshot map** the loop keeps current.
//!
//! Identity note: `PeerOrigin` is two-valued, so the caller's evidence — the
//! presented bearer AND the verified mTLS leaf identity (subject CN + SANs, RFC
//! 0029 §10.3) — is threaded from `authenticate` to `dispatch` via per-connection
//! **thread-locals** (one connection = one thread = one request). The serve
//! framework now surfaces the client-cert subject/SANs (`net::x509`), so
//! `san`/`sub` principal rules match a client cert directly (a SPIFFE X.509-SVID's
//! `spiffe://…` arrives as a URI SAN); an all-empty-principals listener keeps the
//! "any verified cert ⇒ operator" default. Inbound AAuth-agent attribution
//! (`aauth_agent`) still awaits an inbound AAuth verifier (agentd signs, but does
//! not yet verify, AAuth) — the one remaining identity axis.

use crate::a2a::tasks::{Link, State, Task};
use crate::a2a::{CallerIdentity, Principal, Resolver};
use crate::obs::log::Logger;
use crate::runtime::events::{Event, kinds};
use crate::runtime::reactor::{PendingKind, Runtime};
use serde_json::{Value, json};
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The A2A methods this surface serves (PascalCase dialect, RFC 0029 §3).
/// `SubscribeToEvents` (RFC 0032) is served only while `interface.enabled`.
pub const METHODS: &[&str] = &[
    "SendMessage",
    "SendStreamingMessage",
    "GetTask",
    "CancelTask",
    "ListTasks",
    "SubscribeToTask",
    "SubscribeToEvents",
    "CreateTaskPushNotificationConfig",
    "GetTaskPushNotificationConfig",
    "ListTaskPushNotificationConfigs",
    "DeleteTaskPushNotificationConfig",
    "GetExtendedAgentCard",
];
/// A2A error: no such task.
pub const TASK_NOT_FOUND: i64 = -32001;
/// A2A error: the operation is not supported over this surface (yet).
pub const UNSUPPORTED_OPERATION: i64 = -32004;
/// The interface feed ring capacity (RFC 0032 §4): the replay window a
/// reconnecting client can resume across without a re-bootstrap.
pub const FEED_RING: usize = 1024;

// ---- the interface event feed (RFC 0032) -----------------------------------

/// Who may see a feed event.
#[derive(Debug, Clone, PartialEq)]
pub enum FeedVis {
    /// Every authenticated subscriber (lifecycle notices).
    All,
    /// Operators only (global state sections, audit, logs).
    Operator,
    /// The owning principal (and operators). `None` owner ⇒ operator-only.
    Owner(Option<String>),
}

/// The global observation feed (RFC 0032 §4): a bounded ring of state-change
/// events the loop pushes and the `SubscribeToEvents` transport threads drain.
/// Events carry a monotonic `seq` so a reconnecting client resumes from a
/// cursor (`fromSeq`); an overrun evicts the oldest (the client re-bootstraps
/// via the `status` command when its cursor predates the window). Same
/// The loop writes; the listener's stream tasks only read.
pub struct SharedFeed {
    inner: Mutex<FeedInner>,
    /// `interface.debug` — gates the debug event kinds (audit, logs). Atomic
    /// because the operator can toggle it at runtime (`config.set`).
    debug: std::sync::atomic::AtomicBool,
}

struct FeedInner {
    seq: u64,
    buf: std::collections::VecDeque<Value>,
    /// Events evicted to date (a subscriber whose cursor predates the window
    /// learns it fell behind).
    dropped: u64,
}

impl SharedFeed {
    pub fn new(debug: bool) -> SharedFeed {
        SharedFeed {
            inner: Mutex::new(FeedInner {
                seq: 0,
                buf: std::collections::VecDeque::with_capacity(FEED_RING),
                dropped: 0,
            }),
            debug: std::sync::atomic::AtomicBool::new(debug),
        }
    }

    /// Whether debug event kinds flow (runtime-togglable via `config.set`).
    pub fn debug(&self) -> bool {
        self.debug.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_debug(&self, on: bool) {
        self.debug.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one event; returns its `seq`.
    pub fn push(&self, kind: &str, vis: FeedVis, data: Value) -> u64 {
        let vis_tag = match vis {
            FeedVis::All => json!("all"),
            FeedVis::Operator => json!("op"),
            FeedVis::Owner(None) => json!("op"),
            FeedVis::Owner(Some(p)) => json!(p),
        };
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.seq += 1;
        let seq = g.seq;
        let ev = json!({"seq": seq, "ts": crate::state::now_ms(), "kind": kind, "data": data, "_vis": vis_tag});
        if g.buf.len() == FEED_RING {
            g.buf.pop_front();
            g.dropped += 1;
        }
        g.buf.push_back(ev);
        seq
    }

    /// The events visible to `principal` with `seq > after` (oldest-first, up to
    /// `max`), plus the cursor to resume from (the newest seq scanned — it
    /// advances past invisible events too).
    pub fn since(
        &self,
        after: u64,
        principal: &str,
        is_operator: bool,
        max: usize,
    ) -> (Vec<Value>, u64) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        let mut cursor = after;
        for ev in g.buf.iter() {
            let seq = ev["seq"].as_u64().unwrap_or(0);
            if seq <= after {
                continue;
            }
            if out.len() >= max {
                break;
            }
            cursor = seq;
            let visible = match ev["_vis"].as_str() {
                Some("all") => true,
                Some("op") => is_operator,
                Some(owner) => is_operator || owner == principal,
                None => is_operator,
            };
            if visible {
                let mut e = ev.clone();
                if let Value::Object(o) = &mut e {
                    o.remove("_vis");
                }
                out.push(e);
            }
        }
        (out, cursor)
    }

    /// The ring window: (newest seq, oldest seq held, dropped-to-date).
    pub fn bounds(&self) -> (u64, u64, u64) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let oldest = g
            .buf
            .front()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(g.seq);
        (g.seq, oldest, g.dropped)
    }
}

// ---- pairing-code login (RFC 0032 §13) --------------------------------------

/// The pairing window (how often the code rotates).
const PAIR_WINDOW_SECS: u64 = 60;
/// Failed attempts allowed per window before pairing locks out.
const PAIR_MAX_FAILS: usize = 5;

/// Pairing-code login state: a per-process random seed derives a 6-digit code
/// per 60-second window (`HMAC(seed, window)` — no timer thread needed); a
/// correct code (current or previous window, constant-time, rate-limited)
/// mints a high-entropy **session token** that rides `Authorization: Bearer`
/// like any other credential. Sessions live in memory: a restart revokes all.
pub struct PairingState {
    seed: [u8; 32],
    role: crate::config::v2::Role,
    ttl_ms: u64,
    sessions: Mutex<std::collections::HashMap<String, (crate::config::v2::Role, u64)>>,
    /// Recent failed-attempt timestamps (ms) — the rate limiter.
    fails: Mutex<Vec<u64>>,
}

impl PairingState {
    /// Build with fresh randomness. Fails without an OS entropy source.
    pub fn new(role: crate::config::v2::Role, ttl: Duration) -> Result<PairingState, String> {
        Ok(PairingState {
            seed: os_random_32()?,
            role,
            ttl_ms: ttl.as_millis() as u64,
            sessions: Mutex::new(std::collections::HashMap::new()),
            fails: Mutex::new(Vec::new()),
        })
    }

    fn code_for(&self, window: u64) -> String {
        let mac = crate::sha::hmac_sha256(&self.seed, &window.to_be_bytes());
        let n = u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]) % 1_000_000;
        format!("{n:06}")
    }

    /// The current code and how long it stays valid (ms).
    pub fn current_code(&self) -> (String, u64) {
        let now = crate::state::now_ms();
        let window = now / 1000 / PAIR_WINDOW_SECS;
        let expires_in = (window + 1) * PAIR_WINDOW_SECS * 1000 - now;
        (self.code_for(window), expires_in)
    }

    /// Verify a presented code (current or previous window — clock/typing
    /// grace), constant-time, rate-limited. `Ok(token, expires_ms)` mints a
    /// session; `Err` is the client-facing message.
    pub fn pair(&self, code: &str) -> Result<(String, u64), String> {
        let now = crate::state::now_ms();
        {
            let mut fails = self.fails.lock().unwrap_or_else(|e| e.into_inner());
            fails.retain(|t| now.saturating_sub(*t) < PAIR_WINDOW_SECS * 1000);
            if fails.len() >= PAIR_MAX_FAILS {
                return Err("too many pairing attempts; wait a minute".into());
            }
        }
        let window = now / 1000 / PAIR_WINDOW_SECS;
        let code = code.trim().replace([' ', '-'], "");
        let hit = crate::sha::ct_eq(self.code_for(window).as_bytes(), code.as_bytes())
            | crate::sha::ct_eq(
                self.code_for(window.saturating_sub(1)).as_bytes(),
                code.as_bytes(),
            );
        if !hit {
            self.fails
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(now);
            return Err("wrong pairing code".into());
        }
        let token = format!("pat-{}", crate::sha::to_hex(&os_random_32()?));
        let expires = now + self.ttl_ms;
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|_, (_, exp)| *exp > now);
        sessions.insert(token.clone(), (self.role, expires));
        Ok((token, expires))
    }

    /// Resolve a presented bearer against the live sessions.
    pub fn check_bearer(&self, bearer: &str) -> Option<crate::config::v2::Role> {
        if !bearer.starts_with("pat-") {
            return None;
        }
        let now = crate::state::now_ms();
        let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get(bearer)
            .filter(|(_, exp)| *exp > now)
            .map(|(role, _)| *role)
    }

    pub fn role(&self) -> crate::config::v2::Role {
        self.role
    }
    pub fn session_count(&self) -> usize {
        let now = crate::state::now_ms();
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|(_, exp)| *exp > now)
            .count()
    }
}

/// 32 bytes of OS randomness (`/dev/urandom`) — dependency-free.
fn os_random_32() -> Result<[u8; 32], String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut buf))
            .map_err(|e| format!("/dev/urandom: {e}"))?;
        Ok(buf)
    }
    #[cfg(not(unix))]
    {
        let _ = &mut buf;
        Err("pairing needs an OS entropy source (unix /dev/urandom)".into())
    }
}

// ---- the request handed to the loop ----------------------------------------

/// A mutation/read posted to the single-writer loop, answered on `reply`.
#[derive(Debug)]
pub struct A2aRequest {
    pub method: String,
    pub params: Value,
    pub principal: Principal,
    pub reply: SyncSender<Value>,
}

/// The transport's post-office into the loop + the shared view.
pub struct A2aBridge {
    events_tx: Sender<Event>,
    /// The interface event feed (RFC 0032) — `None` unless `interface.enabled`.
    feed: Option<Arc<SharedFeed>>,
    resolver: Arc<Resolver>,
    pub request_timeout: Duration,
    pub stream_deadline: Duration,
}

impl A2aBridge {
    /// The listener's post office into the loop.
    pub fn new(events_tx: Sender<Event>, resolver: Resolver) -> Arc<A2aBridge> {
        Self::with_feed(events_tx, resolver, None)
    }

    /// [`A2aBridge::new`] with the interface feed attached (RFC 0032).
    pub fn with_feed(
        events_tx: Sender<Event>,
        resolver: Resolver,
        feed: Option<Arc<SharedFeed>>,
    ) -> Arc<A2aBridge> {
        Arc::new(A2aBridge {
            events_tx,
            feed,
            resolver: Arc::new(resolver),
            request_timeout: Duration::from_secs(120),
            stream_deadline: Duration::from_secs(600),
        })
    }

    /// Resolve the caller from the transport's evidence: the verified mTLS
    /// identity (subject CN + SANs — RFC 0029 §10.3) and the presented bearer.
    /// The resolver tries the configured `san`/`sub` principal rules FIRST, then
    /// the management/loopback operator fallback, so a matched cert identity wins
    /// its declared role while an all-empty-principals listener keeps the "any
    /// verified cert ⇒ operator" default.
    pub fn principal_of(
        &self,
        mgmt: bool,
        bearer: Option<&str>,
        subject: Option<String>,
        sans: Vec<String>,
    ) -> Principal {
        let id = CallerIdentity {
            management: mgmt,
            loopback: mgmt,
            subject,
            sans,
            ..Default::default()
        };
        self.resolver.resolve(&id, bearer)
    }

    /// The interface observation feed, when `interface.enabled` armed one.
    pub fn feed(&self) -> Option<Arc<SharedFeed>> {
        self.feed.clone()
    }

    /// Post a request to the loop and wait for its reply. Blocking — an async
    /// caller (the A2A ports) runs this on a blocking thread.
    pub fn call(&self, method: &str, params: Value, principal: Principal) -> Value {
        self.call_loop(method, params, principal)
    }

    /// Post a request to the loop and wait for its reply.
    fn call_loop(&self, method: &str, params: Value, principal: Principal) -> Value {
        let (reply_tx, reply_rx) = sync_channel(1);
        let req = A2aRequest {
            method: method.to_string(),
            params,
            principal,
            reply: reply_tx,
        };
        if self.events_tx.send(Event::A2a(Box::new(req))).is_err() {
            return err_obj(rpc_internal(), "the runtime is shutting down");
        }
        reply_rx
            .recv_timeout(self.request_timeout)
            .unwrap_or_else(|_| err_obj(rpc_internal(), "the runtime did not answer in time"))
    }
}

// ---- wire helpers ----------------------------------------------------------

/// Strip an optional `a2a.` prefix.
fn bare(m: &str) -> &str {
    m.strip_prefix("a2a.").unwrap_or(m)
}

/// The default client chrome (RFC 0032 §12) when `interface.display` is unset.
fn default_display_top() -> Vec<String> {
    ["name", "version", "instance", "debug"]
        .map(String::from)
        .to_vec()
}
fn default_display_bottom() -> Vec<String> {
    [
        "conn", "endpoint", "draining", "active", "turns", "tokens", "screen", "keys",
    ]
    .map(String::from)
    .to_vec()
}

/// The principal a live pairing session resolves to (RFC 0032 §13).
pub fn paired_principal(role: crate::config::v2::Role) -> Principal {
    use crate::config::v2::Role;
    match role {
        Role::Operator => Principal {
            id: "operator".into(),
            role: Role::Operator,
            grants: vec!["*".into()],
            rate: None,
            budget: None,
        },
        other => Principal {
            id: "user:paired".into(),
            role: other,
            grants: Vec::new(),
            rate: None,
            budget: None,
        },
    }
}

/// A command DataPart's op (`{"data": {"agentd": {"op": "<tool>", …}}}`).
pub fn command_op(message: &Value) -> Option<String> {
    message["parts"].as_array()?.iter().find_map(|p| {
        p.get("data")
            .and_then(|d| d.get("agentd"))
            .and_then(|a| a.get("op"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// The full command DataPart object (`{op, ...args}`).
fn command_data(message: &Value) -> Option<Value> {
    message["parts"]
        .as_array()?
        .iter()
        .find_map(|p| p.get("data").and_then(|d| d.get("agentd")).cloned())
}

/// The concatenated text of a message's text parts.
fn message_text(message: &Value) -> String {
    message["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A stable fingerprint of a JSON value with the always-moving fields
/// (`age_ms`, `uptime_ms`) excluded — the feed's change detector: equal
/// fingerprint ⇒ no event, so quiet state emits nothing at the tick rate.
fn fingerprint(v: &Value) -> u64 {
    use std::hash::{Hash, Hasher};
    fn walk<H: Hasher>(v: &Value, h: &mut H) {
        match v {
            Value::Object(o) => {
                for (k, x) in o {
                    if k == "age_ms" || k == "uptime_ms" {
                        continue;
                    }
                    k.hash(h);
                    walk(x, h);
                }
            }
            Value::Array(a) => {
                for x in a {
                    walk(x, h);
                }
            }
            Value::String(s) => s.hash(h),
            Value::Number(n) => n.to_string().hash(h),
            Value::Bool(b) => b.hash(h),
            Value::Null => 0u8.hash(h),
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    walk(v, &mut h);
    h.finish()
}

/// Truncate every string in a JSON tree to `max` bytes (marking the cut) — the
/// debug reads bound their payloads with this so a huge tool result cannot
/// balloon an interface reply.
fn truncate_strings(v: Value, max: usize) -> Value {
    match v {
        Value::String(s) if s.len() > max => {
            let mut cut = max;
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            Value::String(format!("{}…(+{} bytes)", &s[..cut], s.len() - cut))
        }
        Value::Array(a) => Value::Array(a.into_iter().map(|x| truncate_strings(x, max)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, x)| (k, truncate_strings(x, max)))
                .collect(),
        ),
        other => other,
    }
}

fn err_obj(code: i64, msg: &str) -> Value {
    json!({"_error": {"code": code, "message": msg}})
}

fn rpc_internal() -> i64 {
    ::mcp::rpc::INTERNAL_ERROR
}

/// A fresh durable task id.
///
/// A ULID, not the reactor's `seq` counter: `seq` starts at 0 in every life
/// while tasks are RESTORED from the store, so a counter-minted id names a task
/// from a PREVIOUS life. That collision is not benign — an id that already
/// exists makes `a2a_send` read the message as a continuation, so the caller is
/// handed someone else's task (and its history) while an unrelated message
/// advances that task's state. ULID is what every other durable id in the store
/// is minted from (runs, inbox events, artifacts) for exactly this reason, and
/// it keeps ids time-sortable.
fn new_task_id() -> String {
    format!("task-{}", crate::state::ulid::new())
}

// ---- the effective-configuration view ---------------------------------------

/// The redaction marker a [`crate::config::v2::Secret`]'s `Debug` writes — the
/// one spelling this codebase uses for "a credential was here".
const REDACTED: &str = "***";

/// The effective settings document with every credential it carries replaced by
/// [`REDACTED`].
///
/// `settings_doc` is the merged files←env←flags layer. A FILE may only carry
/// `{{secret:…}}` references (RFC 0030 §5 refuses an inline one), but an env- or
/// flag-supplied credential sits INLINE in that document — so answering the
/// `config` command with the raw doc echoes live credentials back over a remote
/// protocol surface, which the secret discipline forbids everywhere (RFC 0012
/// §3.7).
///
/// Which values are credentials is not guessed from key names: the walk is
/// driven by the config JSON Schema, where every `Secret`-typed field is
/// declared with the one shared `secret` node and every header map with the one
/// shared `string_map` node (`config::v2::schema`). The schema/struct drift test
/// keeps a new field from being silently missing here, so the one act that
/// redacts a `Secret` added tomorrow is declaring it as a secret in the schema —
/// the same act that already makes it a credential everywhere else — rather than
/// a separate list of key names someone has to remember to extend.
fn redact_settings(doc: &Value) -> Value {
    let schema = crate::config::v2::schema::schema();
    let defs = schema.get("$defs").cloned().unwrap_or(Value::Null);
    redact_by_schema(doc, &schema, &defs)
}

/// One node of [`redact_settings`]'s schema-guided walk.
fn redact_by_schema(v: &Value, node: &Value, defs: &Value) -> Value {
    let node = match node
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("#/$defs/"))
    {
        Some(name) => defs.get(name).unwrap_or(node),
        None => node,
    };
    // A `oneOf` describes the same value several ways; a value is only as public
    // as its least public reading, so every branch gets to redact.
    if let Some(alts) = node.get("oneOf").and_then(Value::as_array) {
        return alts
            .iter()
            .fold(v.clone(), |acc, alt| redact_by_schema(&acc, alt, defs));
    }
    if is_secret_node(node) {
        return redact_value(v);
    }
    match v {
        Value::Object(o) => {
            let props = node.get("properties");
            // `additionalProperties` is `false` (closed object) or `true` (the
            // open `WorkflowRef`) far more often than it is a schema.
            let extra = node.get("additionalProperties").filter(|a| a.is_object());
            let headers = is_header_map_node(node);
            Value::Object(
                o.iter()
                    .map(|(k, x)| {
                        let out = match props.and_then(|p| p.get(k)).or(extra) {
                            // Header NAMES survive, values never do (see
                            // `is_header_map_node`).
                            _ if headers => redact_value(x),
                            Some(child) => redact_by_schema(x, child, defs),
                            // A key the schema does not describe. The only open
                            // node in the document is `WorkflowRef` — an inline
                            // dialect-3 workflow, whose own credentials are
                            // `{{secret:…}}` references — so this passes through
                            // rather than blanking a whole workflow definition.
                            None => x.clone(),
                        };
                        (k.clone(), out)
                    })
                    .collect(),
            )
        }
        Value::Array(a) => match node.get("items") {
            Some(items) => {
                Value::Array(a.iter().map(|x| redact_by_schema(x, items, defs)).collect())
            }
            None => v.clone(),
        },
        other => other.clone(),
    }
}

/// Is this schema node the shared `secret` node — the one every `Secret`-typed
/// field is declared with (`config::v2::schema`)?
fn is_secret_node(node: &Value) -> bool {
    node["type"] == "string"
        && node["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("a secret"))
}

/// Is this schema node the shared `string_map` node — an object of plain string
/// values? Every one of them in the settings schema is a HEADER map, and a
/// header value is credential-bearing by nature (an `Authorization: Bearer …`
/// from env is inline), which is why the v1 view already exposed header NAMES
/// only (`config::Config::effective_view`).
fn is_header_map_node(node: &Value) -> bool {
    node.get("properties").is_none() && node["additionalProperties"]["type"] == "string"
}

/// Replace one credential-bearing value.
///
/// A value that is EXACTLY one `{{secret:NAME}}` / `{{secret-file:PATH}}`
/// reference survives: it NAMES a credential instead of being one, and that
/// name is what makes the effective document useful to the operator reading it.
/// Anything else — an inline env/flag credential, or a template that merely
/// embeds a reference next to inline material — is replaced whole.
fn redact_value(v: &Value) -> Value {
    match v {
        Value::String(s) if is_bare_secret_ref(s) => v.clone(),
        Value::String(_) => json!(REDACTED),
        other => other.clone(),
    }
}

/// Whether `s` is a lone `{{secret:NAME}}` / `{{secret-file:PATH}}` reference
/// (nothing before it, nothing after it, no second ref inside).
fn is_bare_secret_ref(s: &str) -> bool {
    let Some(inner) = s
        .strip_prefix("{{secret:")
        .or_else(|| s.strip_prefix("{{secret-file:"))
    else {
        return false;
    };
    match inner.strip_suffix("}}") {
        Some(name) => !name.is_empty() && !name.contains('{') && !name.contains('}'),
        None => false,
    }
}

// ---- the runtime binding (runs on the single-writer loop) -------------------

impl Runtime {
    /// Handle one A2A request (posted by the transport). Never blocks: work
    /// that takes time (a turn, a run) starts here and is polled by the caller.
    pub(crate) fn on_a2a_request(&mut self, req: A2aRequest) {
        let A2aRequest {
            method,
            params,
            principal,
            reply,
        } = req;
        // The listener pre-mints the id of the task this request will create,
        // because the protocol layer subscribes to a task's updates before the
        // work starts. Whichever path creates it — a conversation turn or a
        // command — takes the id from here, so the caller is watching the task
        // it is actually given.
        self.reserved_task_id = params["message"]["taskId"]
            .as_str()
            .filter(|s| !s.is_empty() && !self.tasks.contains_key(*s))
            .map(str::to_string);
        let out = match bare(&method) {
            "SendMessage" | "SendStreamingMessage" => self.a2a_send(&principal, &params),
            // The listener asks for the id a new task will have BEFORE
            // dispatching the send. The protocol layer subscribes to a task's
            // updates first and processes the message second, so that no
            // transition is missed — which means the id has to exist before the
            // work does. Minting stays here so one place owns the shape of a
            // task id (see `new_task_id`).
            "NewTaskId" => json!({"id": new_task_id()}),
            "GetTask" => self.a2a_get_task(&principal, &params),
            "ListTasks" => self.a2a_list_tasks(&principal),
            "CancelTask" => self.a2a_cancel_task(&principal, &params),
            "PushConfigSet" => self.a2a_push_set(&principal, &params),
            "PushConfigGet" => self.a2a_push_get(&principal, &params),
            "PushConfigList" => self.a2a_push_list(&principal, &params),
            "PushConfigDelete" => self.a2a_push_delete(&principal, &params),
            "GetAgentCard" => self.a2a_agent_card(),
            "GetExtendedAgentCard" => self.a2a_extended_card(&principal),
            "Pair" => self.a2a_pair(&params),
            m if crate::a2a::principals::is_admin(m) => {
                self.a2a_admin(&principal, bare(&method), &params)
            }
            other => err_obj(
                UNSUPPORTED_OPERATION,
                &format!("unsupported method: {other}"),
            ),
        };
        self.reserved_task_id = None;
        // Audit every A2A call: who (principal + role), what (method + command
        // op), and the outcome — the plan §3.11 audit trail.
        let op = params.get("message").and_then(command_op);
        let outcome = if out.get("_error").is_some() {
            "error"
        } else {
            "ok"
        };
        let target = out["task"]["id"]
            .as_str()
            .map(|id| json!({"task": id}))
            .unwrap_or(Value::Null);
        let request_id = params["message"]["messageId"].as_str();
        self.audit_a2a(
            bare(&method),
            op.as_deref(),
            &principal,
            outcome,
            target,
            request_id,
        );
        let _ = reply.send(out);
    }

    /// `SendMessage`/`SendStreamingMessage`: a command DataPart routes to the
    /// registry; natural language becomes a conversation turn. Either way a
    /// durable task tracks it.
    fn a2a_send(&mut self, principal: &Principal, params: &Value) -> Value {
        if self.draining {
            return err_obj(-32000, "the agent is draining");
        }
        let message = &params["message"];
        // An `a2a` START NODE registers its command. A workflow declaring
        // `{kind: a2a, command: "review.start"}` is what makes `review.start`
        // something a peer may ask for — otherwise the built-in list would be
        // the entire command surface and a start node could never be reached,
        // because an unknown op is refused before the message ever becomes an
        // inbox event. A registered command therefore skips command dispatch
        // and takes the ordinary message path: written ahead to the durable
        // inbox, then matched against the start nodes (roles included) by the
        // reactor. A built-in wins, so a workflow cannot shadow `status`.
        let declared =
            command_op(message).is_some_and(|op| self.workflow_declares_a2a_command(&op));
        if !declared && let Some(op) = command_op(message) {
            return self.a2a_command(principal, &op, message);
        }
        let text = message_text(message);
        // A command DataPart carries no text, and that is not an empty message.
        if text.trim().is_empty() && !declared {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                "message has no text or command part",
            );
        }
        let message_id = message["messageId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("msg"));
        // Continue an existing task (answering an input-required gate) or start
        // a fresh conversation. An id for a task that does not exist yet is the
        // listener's reservation, and `task_create` takes it.
        let existing = message["taskId"].as_str().and_then(|tid| {
            self.tasks
                .get(tid)
                .map(|t| (tid.to_string(), t.context_id.clone(), t.principal.clone()))
        });
        // A LIVE human gate on the addressed task (RFC 0032 §16): the reply
        // resolves the suspended asker directly — the tool call returns the
        // text to the model, the `human` step completes with it — instead of
        // becoming a new conversation turn.
        if let Some((tid, ctx, owner)) = &existing
            && (owner.as_deref() == Some(principal.id.as_str()) || principal.is_operator())
            && let Some(i) = self
                .pending
                .iter()
                .position(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == tid))
        {
            // Every attached client sees the answer (the cross-client transcript).
            self.feed_push(
                "message",
                FeedVis::Owner(Some(principal.id.clone())),
                json!({"contextId": ctx, "taskId": tid, "messageId": message_id, "principal": principal.id, "text": text}),
            );
            self.human_answer(i, &text, "human");
            return json!({"task": self.tasks.get(tid).map(Task::to_a2a).unwrap_or(Value::Null)});
        }
        let (task_id, ctx_id) = match existing {
            Some((tid, ctx, owner))
                if owner.as_deref() == Some(principal.id.as_str()) || principal.is_operator() =>
            {
                if let Some(t) = self.tasks.get_mut(&tid) {
                    t.transition(State::Working, None);
                }
                (tid, ctx)
            }
            _ => {
                let ctx = message["contextId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| self.next_id("a2a"));
                let tid = self.task_create(&ctx, principal, Link::Turn { ctx: ctx.clone() });
                (tid, ctx)
            }
        };
        // Write-ahead the message; the loop turns it into a conversation turn.
        let payload = json!({"context_id": ctx_id, "text": text, "parts": message["parts"],
        "task": task_id, "message_id": message_id,
        "role": match principal.role {
            crate::config::v2::Role::Operator => "operator",
            crate::config::v2::Role::User => "user",
            crate::config::v2::Role::Agent => "agent",
            crate::config::v2::Role::Anonymous => "anonymous",
        }});
        match self.accept_event(kinds::A2A_MESSAGE, Some(principal.id.clone()), payload) {
            Ok(inbox_id) => {
                self.event_to_task.insert(inbox_id, task_id.clone());
                if let Some(t) = self.tasks.get_mut(&task_id) {
                    t.transition(State::Working, None);
                }
                self.task_sync(&task_id);
                // Surface the prompt on the interface feed (RFC 0032 §4): this
                // is what lets a SECOND display client render the transcript a
                // first client is driving — the reply follows as the task's
                // terminal artifact on its `task` events.
                self.feed_push(
                    "message",
                    FeedVis::Owner(Some(principal.id.clone())),
                    json!({"contextId": ctx_id, "taskId": task_id, "messageId": message_id, "principal": principal.id, "text": text}),
                );
                json!({"task": self.tasks.get(&task_id).map(Task::to_a2a).unwrap_or(Value::Null)})
            }
            Err(e) => {
                self.a2a_task_fail(&task_id, &e);
                err_obj(rpc_internal(), &e)
            }
        }
    }

    /// A command DataPart (RFC 0029 §5). The synchronous subset completes at
    /// once; `workflow.run` links its task to the run it starts. The
    /// `interface.*` / debug reads (RFC 0032) are **taskless** — pure reads
    /// that create no durable task, so a display client can poll them freely.
    /// Whether any loaded workflow has an `a2a` start node declaring `op` as its
    /// command. This is what turns a start node into a registered part of the
    /// A2A command surface (see the call site in `a2a_send`).
    fn workflow_declares_a2a_command(&self, op: &str) -> bool {
        self.workflows.values().any(|w| {
            w.start_steps().into_iter().any(|s| {
                s.kind == "a2a" && s.spec.get("command").and_then(Value::as_str) == Some(op)
            })
        })
    }

    fn a2a_command(&mut self, principal: &Principal, op: &str, message: &Value) -> Value {
        if !principal.may_command(op) {
            return err_obj(
                -32003,
                &format!("command {op:?} not granted to {}", principal.id),
            );
        }
        let data = command_data(message).unwrap_or_else(|| json!({}));
        // The taskless interface reads + controls (RFC 0032 §5, §13–14).
        match op {
            "interface.info" => return self.interface_info(),
            "conversation.get" => return self.interface_conversation_get(principal, &data),
            "run.get" => return self.interface_run_get(principal, &data),
            "subagent.get" => return self.interface_subagent_get(&data),
            "debug.events" => return self.interface_debug_events(&data),
            "pairing.code" => return self.interface_pairing_code(),
            "config.set" => return self.interface_config_set(&data),
            _ => {}
        }
        let ctx = message["contextId"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("a2a"));
        // Surface MUTATING commands on the interface feed so every attached
        // display client sees what its peers asked for (RFC 0032 §4). Read ops
        // (`status`, `config`, `workflow.status`) stay off the feed — they are
        // the observation plumbing itself, and N clients polling them would
        // spam every transcript.
        if matches!(
            op,
            "workflow.run"
                | "workflow.cancel"
                | "workflow.signal"
                | "subagent.send"
                | "subagent.kill"
        ) {
            self.feed_push(
                "command",
                FeedVis::Owner(Some(principal.id.clone())),
                json!({"op": op, "principal": principal.id, "contextId": ctx}),
            );
        }
        match op {
            "status" => {
                let s = self.status_value();
                let text = format!(
                    "{} runs, {} subagents, {} conversations; budget active: {}",
                    s["runs"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["subagents"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["conversations"].as_array().map(|a| a.len()).unwrap_or(0),
                    s["budget"]["active"]
                );
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    Some(text),
                    Some(s),
                )
            }
            // The effective merged configuration (`agent://config/effective`) —
            // operator-only (via `may_command`). Redacted on the way out: the
            // merged doc carries env/flag-supplied credentials INLINE, and
            // operator-only is not the same as public (see `redact_settings`).
            // What survives is the `{{secret:…}}` reference, never a value.
            "config" => self.task_complete_now(
                &ctx,
                principal,
                Link::Turn { ctx: ctx.clone() },
                State::Completed,
                Some("effective configuration".into()),
                Some(json!({"config": redact_settings(&self.settings_doc)})),
            ),
            "workflow.run" => {
                let name = data["name"]
                    .as_str()
                    .or_else(|| data["workflow"].as_str())
                    .unwrap_or("")
                    .to_string();
                if !self.workflows.contains_key(&name) {
                    return err_obj(
                        ::mcp::rpc::INVALID_PARAMS,
                        &format!("no such workflow {name:?}"),
                    );
                }
                let run_id = format!("{}-{}", name, crate::state::ulid::new());
                let task_id = self.task_create(&ctx, principal, Link::Run { id: run_id.clone() });
                let payload = json!({
                    "workflow": name,
                    "run_id": run_id,
                    "inputs": data.get("inputs").cloned().unwrap_or_else(|| json!({})),
                    "payload": {"requested_by": principal.id},
                    "task": task_id,
                    "conversation": ctx,
                });
                match self.accept_event(kinds::WORKFLOW_RUN, Some(principal.id.clone()), payload) {
                    Ok(_) => {
                        if let Some(t) = self.tasks.get_mut(&task_id) {
                            t.transition(State::Working, None);
                        }
                        self.task_sync(&task_id);
                        json!({"task": self.tasks.get(&task_id).map(Task::to_a2a).unwrap_or(Value::Null)})
                    }
                    Err(e) => err_obj(rpc_internal(), &e),
                }
            }
            "workflow.status" => {
                let view: Vec<Value> = match data["run"].as_str() {
                    Some(id) => self
                        .runs
                        .get(id)
                        .map(|r| vec![run_view(id, r)])
                        .unwrap_or_default(),
                    None => self
                        .runs
                        .iter()
                        .filter(|(_, r)| {
                            principal.is_operator()
                                || r.principal.as_deref() == Some(principal.id.as_str())
                        })
                        .map(|(id, r)| run_view(id, r))
                        .collect(),
                };
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    None,
                    Some(json!({"runs": view})),
                )
            }
            "workflow.cancel" => match data["run"].as_str() {
                Some(id) if self.runs.contains_key(id) => {
                    self.cancel_run(id, "cancelled over A2A");
                    self.task_complete_now(
                        &ctx,
                        principal,
                        Link::Run { id: id.to_string() },
                        State::Completed,
                        Some(format!("run {id} cancelled")),
                        None,
                    )
                }
                _ => err_obj(TASK_NOT_FOUND, "no such run"),
            },
            // ---- steering (RFC 0029 §5 — now dispatched) ------------------
            "workflow.signal" => {
                let name = data["name"].as_str().unwrap_or("").to_string();
                if name.is_empty() {
                    return err_obj(::mcp::rpc::INVALID_PARAMS, "workflow.signal needs a name");
                }
                let payload = data.get("payload").cloned().unwrap_or(Value::Null);
                let target = data["run"].as_str().map(str::to_string);
                let delivered =
                    self.deliver_signal(&name, payload, target.as_deref(), Some(&principal.id));
                self.task_complete_now(
                    &ctx,
                    principal,
                    Link::Turn { ctx: ctx.clone() },
                    State::Completed,
                    Some(format!("signal {name:?} delivered to {delivered}")),
                    Some(json!({"signal": name, "delivered": delivered})),
                )
            }
            "subagent.send" | "subagent.kill" | "subagent.status" => {
                // Reuse the internal tool implementations verbatim.
                let mut args = data.clone();
                if op == "subagent.send"
                    && args.get("message").is_none()
                    && let Some(t) = data["text"].as_str()
                {
                    args["message"] = json!(t);
                }
                let tool_caller = crate::runtime::tools::ToolCaller {
                    principal: Some(principal.id.clone()),
                    ..Default::default()
                };
                match self.subagent_tool(&tool_caller, op, args) {
                    crate::runtime::tools::ToolOutcome::Ready(v, false) => self.task_complete_now(
                        &ctx,
                        principal,
                        Link::Turn { ctx: ctx.clone() },
                        State::Completed,
                        None,
                        Some(v),
                    ),
                    crate::runtime::tools::ToolOutcome::Ready(v, true) => err_obj(
                        ::mcp::rpc::INVALID_PARAMS,
                        v.as_str().unwrap_or("subagent op failed"),
                    ),
                    _ => err_obj(rpc_internal(), "unexpected deferred subagent op"),
                }
            }
            "plan.get" => {
                let id = data["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| crate::context::ROOT.to_string());
                match self.contexts.get(&id) {
                    Some(c)
                        if principal.is_operator()
                            || c.principal.as_deref() == Some(principal.id.as_str()) =>
                    {
                        self.task_complete_now(
                            &ctx,
                            principal,
                            Link::Turn { ctx: ctx.clone() },
                            State::Completed,
                            None,
                            Some(json!({"conversation": id, "plan": c.plan, "progress": c.plan.as_ref().map(|p| p.progress())})),
                        )
                    }
                    _ => err_obj(TASK_NOT_FOUND, "no such conversation"),
                }
            }
            other => err_obj(
                UNSUPPORTED_OPERATION,
                &format!(
                    "command {other:?} is not available over A2A yet; send a natural-language message instead"
                ),
            ),
        }
    }

    // ---- the interface surface (RFC 0032) ---------------------------------

    /// Push an event onto the interface feed (a no-op unless `interface.enabled`).
    pub(crate) fn feed_push(&self, kind: &str, vis: FeedVis, data: Value) {
        if let Some(feed) = &self.a2a_feed {
            feed.push(kind, vis, data);
        }
    }

    /// A gate error for a debug read while debug is off.
    fn debug_gate(&self) -> Option<Value> {
        if !self.settings.interface.enabled {
            return Some(err_obj(
                UNSUPPORTED_OPERATION,
                "the interface surface is disabled (set interface.enabled: true)",
            ));
        }
        if !self.settings.interface.debug {
            return Some(err_obj(
                UNSUPPORTED_OPERATION,
                "debug reads are disabled (set interface.debug: true)",
            ));
        }
        None
    }

    /// `interface.info` — what this instance's interface serves. The client's
    /// first call: it learns whether the surface is on, whether debug panes may
    /// render, which ops exist, and what to render in its chrome (§12).
    fn interface_info(&self) -> Value {
        if !self.settings.interface.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the interface surface is disabled (set interface.enabled: true)",
            );
        }
        let debug = self.settings.interface.debug;
        let mut ops = vec!["interface.info", "config.set"];
        if debug {
            ops.extend([
                "conversation.get",
                "run.get",
                "subagent.get",
                "debug.events",
            ]);
        }
        if self.a2a_pairing.is_some() {
            ops.push("pairing.code");
        }
        let display = &self.settings.interface.display;
        json!({"interface": {
            "enabled": true,
            "debug": debug,
            "version": crate::VERSION,
            "instance": self.instance,
            "model": self.model,
            "protocol": 1,
            "feed": {"ring": FEED_RING, "method": "SubscribeToEvents"},
            "ops": ops,
            "display": {
                "top": display.top.clone().unwrap_or_else(default_display_top),
                "bottom": display.bottom.clone().unwrap_or_else(default_display_bottom),
            },
            "pairing": {"enabled": self.a2a_pairing.is_some()},
        }})
    }

    /// `pairing.code` (operator): the CURRENT rotating code + its remaining
    /// validity — what the operator reads out to whoever is connecting.
    fn interface_pairing_code(&self) -> Value {
        if !self.settings.interface.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the interface surface is disabled (set interface.enabled: true)",
            );
        }
        let Some(p) = &self.a2a_pairing else {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "pairing is disabled (set interface.pairing.enabled: true)",
            );
        };
        let (code, expires_in) = p.current_code();
        json!({"pairing": {
            "code": code,
            "expires_in_ms": expires_in,
            "window_ms": PAIR_WINDOW_SECS * 1000,
            "role": format!("{:?}", p.role()).to_lowercase(),
            "sessions": p.session_count(),
            "url": self.settings.a2a.listen,
        }})
    }

    /// `Pair {code}` — exchange the rotating code for a session token
    /// (RFC 0032 §13). The ONE method an anonymous caller may use.
    fn a2a_pair(&mut self, params: &Value) -> Value {
        let Some(p) = &self.a2a_pairing else {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "pairing is disabled (set interface.pairing.enabled: true)",
            );
        };
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if code.is_empty() {
            return err_obj(::mcp::rpc::INVALID_PARAMS, "Pair needs a code");
        }
        match p.pair(code) {
            Ok((token, expires)) => {
                self.log
                    .info("interface.paired", json!({"role": format!("{:?}", p.role()).to_lowercase(), "sessions": p.session_count()}));
                self.feed_push(
                    "pairing",
                    FeedVis::Operator,
                    json!({"paired": true, "sessions": p.session_count()}),
                );
                json!({"token": token, "expiresAt": expires, "role": format!("{:?}", p.role()).to_lowercase(),
                       "agent": {"name": "agentd", "instance": self.instance, "version": crate::VERSION}})
            }
            Err(e) => err_obj(-32003, &e),
        }
    }

    /// `config.set {path, value}` (operator): runtime updates for the
    /// WHITELISTED interface knobs (RFC 0032 §14). Everything else belongs to
    /// the config file + SIGHUP reload — this deliberately never writes files,
    /// so provenance stays with the operator's documents.
    fn interface_config_set(&mut self, data: &Value) -> Value {
        if !self.settings.interface.enabled {
            return err_obj(
                UNSUPPORTED_OPERATION,
                "the interface surface is disabled (set interface.enabled: true)",
            );
        }
        let path = data["path"].as_str().unwrap_or_default();
        let value = data.get("value").cloned().unwrap_or(Value::Null);
        let applied: Result<Value, String> = match path {
            "interface.debug" => match value.as_bool() {
                Some(on) => {
                    self.settings.interface.debug = on;
                    if let Some(feed) = &self.a2a_feed {
                        feed.set_debug(on);
                    }
                    if on {
                        // The debug reads tail the log ring — make sure it runs.
                        let cap = self
                            .settings
                            .observability
                            .events_ring
                            .map(|n| n as usize)
                            .unwrap_or(crate::obs::log::EVENTS_RING_DEFAULT);
                        crate::obs::log::install_event_ring(cap);
                    }
                    Ok(json!(on))
                }
                None => Err("interface.debug takes true|false".into()),
            },
            "interface.display.top" | "interface.display.bottom" => {
                let items: Option<Vec<String>> = value.as_array().map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                });
                match items {
                    Some(list) => {
                        if path.ends_with(".top") {
                            self.settings.interface.display.top = Some(list.clone());
                        } else {
                            self.settings.interface.display.bottom = Some(list.clone());
                        }
                        Ok(json!(list))
                    }
                    None => Err("display lists take an array of item names".into()),
                }
            }
            other => Err(format!(
                "{other:?} is not runtime-settable; settable: interface.debug, interface.display.top, interface.display.bottom — everything else is the config file + SIGHUP (docs/configuration.md §11)"
            )),
        };
        match applied {
            Ok(v) => {
                self.log
                    .info("interface.config_set", json!({"path": path, "value": v}));
                self.feed_push(
                    "config",
                    FeedVis::Operator,
                    json!({"path": path, "value": v}),
                );
                json!({"set": {"path": path, "value": v}})
            }
            Err(e) => err_obj(::mcp::rpc::INVALID_PARAMS, &e),
        }
    }

    /// `subagent.get {handle}` (debug): one subagent's detail — instruction,
    /// status, attempts, result/error (truncated) — the drill-down view.
    fn interface_subagent_get(&self, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let handle = data["handle"]
            .as_str()
            .or_else(|| data["id"].as_str())
            .unwrap_or("");
        let Some(s) = self.subagents.get(handle) else {
            return err_obj(TASK_NOT_FOUND, "no such subagent");
        };
        json!({"subagent": {
            "handle": s.handle,
            "mode": s.mode,
            "status": s.status,
            "attempt": s.attempt,
            "tokens": s.tokens,
            "instruction": truncate_strings(json!(s.instruction), 4096),
            "result": s.result.clone().map(|r| truncate_strings(r, 4096)),
            "error": s.error,
            "requested_by": s.requested_by,
            "created": s.created,
            "updated": s.updated,
            "node": s.node.map(|n| n.0),
        }})
    }

    /// `conversation.get {id, limit?}` (debug): the conversation transcript —
    /// the one read that exposes message BODIES, which is why it rides the
    /// debug gate. Ownership: the owner or an operator.
    fn interface_conversation_get(&self, principal: &Principal, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let id = data["id"].as_str().unwrap_or("");
        let limit = data["limit"].as_u64().unwrap_or(200).min(1000) as usize;
        let Some(c) = self.contexts.get(id) else {
            return err_obj(TASK_NOT_FOUND, "no such conversation");
        };
        let owner_ok =
            principal.is_operator() || c.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            // Don't disclose existence to a non-owner.
            return err_obj(TASK_NOT_FOUND, "no such conversation");
        }
        let skip = c.messages.len().saturating_sub(limit);
        let messages: Vec<Value> = c.messages[skip..]
            .iter()
            .map(|m| truncate_strings(serde_json::to_value(m).unwrap_or(Value::Null), 4096))
            .collect();
        json!({"conversation": {
            "id": id,
            "kind": c.kind,
            "version": c.version,
            "turns": c.turns,
            "est_tokens": c.est_tokens,
            "principal": c.principal,
            "task": c.task,
            "skills": c.skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            "plan": c.plan,
            "summary": if c.summary.is_empty() { Value::Null } else { serde_json::to_value(&c.summary).unwrap_or(Value::Null) },
            "total_messages": c.messages.len(),
            "messages": messages,
            "updated": c.updated,
        }})
    }

    /// `run.get {run}` (debug): a run with PER-STEP detail — status, attempts,
    /// timings, error, wait, truncated output — the projection a run-graph
    /// view renders (the plain `workflow.status` stays a histogram).
    fn interface_run_get(&self, principal: &Principal, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let id = data["run"]
            .as_str()
            .or_else(|| data["id"].as_str())
            .unwrap_or("");
        let Some(r) = self.runs.get(id) else {
            return err_obj(TASK_NOT_FOUND, "no such run");
        };
        let owner_ok =
            principal.is_operator() || r.principal.as_deref() == Some(principal.id.as_str());
        if !owner_ok {
            return err_obj(TASK_NOT_FOUND, "no such run");
        }
        let steps: serde_json::Map<String, Value> = r
            .steps
            .iter()
            .map(|(sid, st)| {
                (
                    sid.clone(),
                    json!({
                        "status": st.status,
                        "attempt": st.attempt,
                        "started": st.started,
                        "finished": st.finished,
                        "error": st.error,
                        "wait": st.wait,
                        "output": st.output.clone().map(|o| truncate_strings(o, 2048)),
                    }),
                )
            })
            .collect();
        let mut run = r.summary();
        run["steps"] = Value::Object(steps);
        run["vars"] = truncate_strings(Value::Object(r.vars.clone()), 2048);
        json!({"run": run})
    }

    /// `debug.events {after?, limit?, level?, prefix?}` (debug, operator): a
    /// cursor read of the live log ring (RFC 0016 §7.2) — the TUI's log tail.
    fn interface_debug_events(&self, data: &Value) -> Value {
        if let Some(gate) = self.debug_gate() {
            return gate;
        }
        let after = data["after"].as_u64().unwrap_or(0);
        let limit = data["limit"].as_u64().unwrap_or(200).min(500) as usize;
        let level = data["level"].as_str();
        let prefixes: Vec<&str> = data["prefix"].as_str().map(|p| vec![p]).unwrap_or_default();
        match crate::obs::log::read_event_window(after, limit, level, &prefixes) {
            Some(w) => {
                json!({"events": w.events, "newest_seq": w.newest_seq, "oldest_seq": w.oldest_seq, "dropped": w.dropped})
            }
            None => err_obj(rpc_internal(), "the event ring is not installed"),
        }
    }

    /// The feed's **section diff** (RFC 0032 §4): one hook point in the loop
    /// that catches every state transition the explicit pushes don't — runs,
    /// conversations, subagents, OS children and the slim status — by
    /// fingerprinting each item and emitting an event when it changed (or
    /// left). Rate-limited to 4 Hz; fingerprints exclude the always-moving
    /// fields (`age_ms`, `uptime_ms`) so quiet state stays quiet.
    pub(crate) fn feed_tick(&mut self) {
        if self.a2a_feed.is_none() {
            return;
        }
        if self.feed_last.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.feed_last = Instant::now();
        let mut fresh: Vec<(String, &'static str, FeedVis, Value)> = Vec::new();
        for (id, r) in &self.runs {
            fresh.push((
                format!("run:{id}"),
                "run",
                FeedVis::Owner(r.principal.clone()),
                r.summary(),
            ));
        }
        for c in self.contexts.status().as_array().into_iter().flatten() {
            let id = c["id"].as_str().unwrap_or("").to_string();
            let owner = c["principal"].as_str().map(str::to_string);
            fresh.push((
                format!("conv:{id}"),
                "conversation",
                FeedVis::Owner(owner),
                c.clone(),
            ));
        }
        for (h, s) in &self.subagents {
            fresh.push((
                format!("sub:{h}"),
                "subagent",
                FeedVis::Operator,
                json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens, "error": s.error, "updated": s.updated}),
            ));
        }
        for c in self.children.status().as_array().into_iter().flatten() {
            let node = c["node"].as_u64().unwrap_or(0);
            fresh.push((
                format!("child:{node}"),
                "child",
                FeedVis::Operator,
                c.clone(),
            ));
        }
        fresh.push((
            "status".into(),
            "status",
            FeedVis::Operator,
            json!({
                "instance": self.instance,
                "model": self.model,
                "draining": self.draining,
                "inbox_pending": self.inbox_queue.len(),
                "counters": {"turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs_started": self.counters.runs_started, "runs_finished": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out},
                "budget": self.governor.status(crate::state::now_ms()),
                "store": {"kind": self.durable.store_kind(), "degraded": self.durable.is_degraded()},
            }),
        ));
        // Diff against the marks; emit changed items, then departures.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut pushes: Vec<(&'static str, FeedVis, Value)> = Vec::new();
        for (key, kind, vis, data) in fresh {
            let mark = fingerprint(&data);
            seen.insert(key.clone());
            if self.feed_marks.get(&key) != Some(&mark) {
                self.feed_marks.insert(key, mark);
                pushes.push((kind, vis, data));
            }
        }
        let gone: Vec<String> = self
            .feed_marks
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        for key in gone {
            self.feed_marks.remove(&key);
            if let Some((section, id)) = key.split_once(':') {
                let kind: &'static str = match section {
                    "run" => "run.removed",
                    "conv" => "conversation.removed",
                    "sub" => "subagent.removed",
                    _ => "child.removed",
                };
                pushes.push((kind, FeedVis::Operator, json!({"id": id})));
            }
        }
        for (kind, vis, data) in pushes {
            self.feed_push(kind, vis, data);
        }
    }

    fn a2a_get_task(&self, principal: &Principal, params: &Value) -> Value {
        let id = params
            .get("id")
            .or_else(|| params.get("taskId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tasks.get(id) {
            Some(t)
                if principal.is_operator()
                    || t.principal.as_deref() == Some(principal.id.as_str()) =>
            {
                t.to_a2a()
            }
            // Don't disclose existence to a non-owner.
            _ => err_obj(TASK_NOT_FOUND, "task not found"),
        }
    }

    fn a2a_list_tasks(&self, principal: &Principal) -> Value {
        let tasks: Vec<Value> = self
            .tasks
            .values()
            .filter(|t| {
                principal.is_operator() || t.principal.as_deref() == Some(principal.id.as_str())
            })
            .map(|t| t.summary())
            .collect();
        // `ListTasksResult` is a fixed shape: a peer's generated type has
        // `totalSize`/`pageSize`/`nextPageToken` as non-optional. We return the
        // whole set in one page, so the token is empty and the sizes agree.
        let n = tasks.len();
        json!({"tasks": tasks, "totalSize": n, "pageSize": n, "nextPageToken": ""})
    }

    // ---- push notifications (RFC 0029 / A2A `*TaskPushNotificationConfig`) --

    /// The task this request names, if the caller may touch it.
    ///
    /// "Not yours" and "does not exist" answer identically on purpose: a caller
    /// must not be able to probe for other principals' task ids.
    fn owned_task(&self, principal: &Principal, params: &Value) -> Result<String, Value> {
        let id = params
            .get("taskId")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self.tasks.get(id) {
            Some(t)
                if principal.is_operator()
                    || t.principal.as_deref() == Some(principal.id.as_str()) =>
            {
                Ok(id.to_string())
            }
            _ => Err(err_obj(TASK_NOT_FOUND, "task not found")),
        }
    }

    fn push_enabled(&self) -> Result<(), Value> {
        if self.settings.a2a.push.enabled {
            Ok(())
        } else {
            Err(err_obj(
                -32003,
                "push notifications are not enabled (set a2a.push.enabled: true)",
            ))
        }
    }

    /// Register (or replace) a webhook for a task.
    ///
    /// The target is checked here, while the caller is present to be told why —
    /// a refused URL is a `-32602` with a reason, not a delivery that silently
    /// never happens.
    fn a2a_push_set(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let cfg = params
            .get("pushNotificationConfig")
            .or_else(|| params.get("config"))
            .cloned()
            .unwrap_or_else(|| params.clone());
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let id = cfg
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.next_id("push"));
        let target = match crate::a2a::push::from_wire(&cfg, id.clone()) {
            Ok(t) => t,
            Err(e) => return err_obj(::mcp::rpc::INVALID_PARAMS, &e),
        };
        let allow_private = self.settings.a2a.push.allow_private;
        if let Err(e) = crate::a2a::push::check_url(&target.url, allow_private) {
            return err_obj(
                ::mcp::rpc::INVALID_PARAMS,
                &format!("push url refused: {e}"),
            );
        }
        let wire = crate::a2a::push::to_wire(&task_id, &target);
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.push.retain(|p| p.id != id);
            t.push.push(target);
            t.dirty = true;
        }
        self.task_persist(&task_id);
        self.log.info(
            "a2a.push.registered",
            json!({"task": task_id, "config": id, "principal": principal.id}),
        );
        wire
    }

    fn a2a_push_get(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let want = params
            .get("pushNotificationConfigId")
            .or_else(|| params.get("configId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match self
            .tasks
            .get(&task_id)
            .and_then(|t| t.push.iter().find(|p| want.is_empty() || p.id == want))
        {
            Some(p) => crate::a2a::push::to_wire(&task_id, p),
            None => err_obj(TASK_NOT_FOUND, "no such push notification config"),
        }
    }

    fn a2a_push_list(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let configs: Vec<Value> = self
            .tasks
            .get(&task_id)
            .map(|t| {
                t.push
                    .iter()
                    .map(|p| crate::a2a::push::to_wire(&task_id, p))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "configs": configs })
    }

    fn a2a_push_delete(&mut self, principal: &Principal, params: &Value) -> Value {
        if let Err(e) = self.push_enabled() {
            return e;
        }
        let task_id = match self.owned_task(principal, params) {
            Ok(id) => id,
            Err(e) => return e,
        };
        let want = params
            .get("pushNotificationConfigId")
            .or_else(|| params.get("configId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(t) = self.tasks.get_mut(&task_id) {
            t.push.retain(|p| !want.is_empty() && p.id != want);
            t.dirty = true;
        }
        self.task_persist(&task_id);
        json!({})
    }

    fn a2a_cancel_task(&mut self, principal: &Principal, params: &Value) -> Value {
        let id = params
            .get("id")
            .or_else(|| params.get("taskId"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let owned = match self.tasks.get(&id) {
            Some(t) => {
                principal.is_operator() || t.principal.as_deref() == Some(principal.id.as_str())
            }
            None => false,
        };
        if !owned {
            return err_obj(TASK_NOT_FOUND, "task not found");
        }
        if self.tasks.get(&id).is_some_and(|t| t.state.is_terminal()) {
            return self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null);
        }
        // A live human gate on this task: unblock the asker with an error so
        // the turn/step resolves instead of dangling (RFC 0032 §16).
        if let Some(i) = self
            .pending
            .iter()
            .position(|p| matches!(&p.kind, PendingKind::Human { task, .. } if task == &id))
        {
            self.human_fail(i, "ask_human: the gate task was cancelled");
        }
        match self.tasks.get(&id).map(|t| t.link.clone()) {
            Some(Link::Run { id: run }) if self.runs.contains_key(&run) => {
                self.cancel_run(&run, "task cancelled over A2A")
            }
            Some(Link::Subagent { handle }) => {
                if let Some(node) = self.subagents.get(&handle).and_then(|s| s.node) {
                    self.children.cancel(node, "task cancelled over A2A");
                }
            }
            _ => {}
        }
        if let Some(t) = self.tasks.get_mut(&id) {
            t.transition(State::Canceled, Some("cancelled".into()));
        }
        self.task_persist(&id);
        self.task_sync(&id);
        self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null)
    }

    fn a2a_admin(&mut self, _principal: &Principal, method: &str, params: &Value) -> Value {
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("operator request")
            .to_string();
        match method.to_ascii_lowercase().as_str() {
            "drain" | "a2a.drain" | "lameduck" | "a2a.lameduck" => {
                self.begin_drain(&reason);
                json!({"ok": true, "state": "draining", "reason": reason})
            }
            "cancel" | "a2a.cancel" => {
                if let Some(run) = params.get("run").and_then(Value::as_str) {
                    self.cancel_run(run, &reason);
                    json!({"ok": true, "cancelled": run})
                } else {
                    err_obj(::mcp::rpc::INVALID_PARAMS, "cancel needs a run id")
                }
            }
            // Pause/resume (RFC 0029 §7): with a `run`, flip that run between
            // Paused and Running (the scheduler already skips Paused runs);
            // without one, hold the WHOLE instance — intake continues (inbox,
            // tasks), but no new turns dispatch and no steps schedule until
            // resume. Reversible, unlike drain.
            "pause" | "a2a.pause" => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status.is_terminal() => {
                        err_obj(::mcp::rpc::INVALID_PARAMS, "the run is already terminal")
                    }
                    Some(r) => {
                        r.status = crate::engine::RunStatus::Paused;
                        r.touch();
                        self.log
                            .info("run.paused", json!({"run": run, "reason": reason}));
                        json!({"ok": true, "paused": run})
                    }
                    None => err_obj(TASK_NOT_FOUND, "no such run"),
                },
                None => {
                    self.paused = true;
                    crate::obs::metrics::set_paused(true);
                    self.log.info("agent.paused", json!({"reason": reason}));
                    self.feed_push(
                        "lifecycle",
                        FeedVis::All,
                        json!({"paused": true, "reason": reason}),
                    );
                    json!({"ok": true, "state": "paused", "reason": reason})
                }
            },
            "resume" | "a2a.resume" => match params.get("run").and_then(Value::as_str) {
                Some(run) => match self.runs.get_mut(run) {
                    Some(r) if r.status == crate::engine::RunStatus::Paused => {
                        r.status = crate::engine::RunStatus::Running;
                        r.touch();
                        self.log.info("run.resumed", json!({"run": run}));
                        json!({"ok": true, "resumed": run})
                    }
                    Some(_) => err_obj(::mcp::rpc::INVALID_PARAMS, "the run is not paused"),
                    None => err_obj(TASK_NOT_FOUND, "no such run"),
                },
                None => {
                    self.paused = false;
                    crate::obs::metrics::set_paused(false);
                    self.log.info("agent.resumed", json!({}));
                    self.feed_push("lifecycle", FeedVis::All, json!({"paused": false}));
                    json!({"ok": true, "state": "running"})
                }
            },
            other => err_obj(
                UNSUPPORTED_OPERATION,
                &format!("unknown admin op {other:?}"),
            ),
        }
    }

    /// The A2A agent card (served over `GetAgentCard`; the framework is
    /// POST-only, so there is no `/.well-known` GET path).
    fn a2a_agent_card(&self) -> Value {
        let skills: Vec<Value> = self
            .workflows
            .values()
            .map(|w| json!({"id": w.name, "name": w.name, "description": w.description.clone().unwrap_or_default(), "tags": ["workflow"]}))
            .collect();
        // The card is a promise, so `pushNotifications` tracks whether this
        // instance will actually accept a webhook rather than whether the code
        // exists (conformance checks both directions of that).
        let mut capabilities = json!({
            "streaming": true,
            "pushNotifications": self.settings.a2a.push.enabled,
            "stateTransitionHistory": true,
        });
        // Advertise the interface surface (RFC 0032) so a display client can
        // discover it pre-auth. The card is public — only the on/off bit rides
        // here; `interface.info` (authenticated) carries the rest.
        if self.settings.interface.enabled {
            capabilities["extensions"] =
                json!([{"uri": "urn:agentd:interface", "params": {"enabled": true}}]);
        }
        let url = self.settings.a2a.listen.clone().unwrap_or_default();
        json!({
            "name": "agentd",
            "description": "A durable agent (agentd 2.0) — conversations, workflows, and subagents over A2A.",
            "version": crate::VERSION,
            // How a peer actually reaches this instance. `supportedInterfaces`
            // is the field the current card carries; a card without one parses
            // fine and tells a peer nothing it can dial, which is the worst of
            // both. The flat `url`/`preferredTransport` below are the older
            // spelling, kept because agentd's own clients read them.
            "supportedInterfaces": [
                {"url": url, "protocolBinding": "JSONRPC", "protocolVersion": "0.3.0"}
            ],
            "protocolVersion": "0.3.0",
            "url": url,
            "preferredTransport": "JSONRPC",
            "capabilities": capabilities,
            "defaultInputModes": ["text/plain", "application/json"],
            "defaultOutputModes": ["text/plain", "application/json"],
            "skills": skills,
        })
    }

    /// The **authenticated** card: the public one, plus what only a named
    /// caller may be told.
    ///
    /// The public card lists every workflow as a skill because discovery has to
    /// work before anyone is authenticated. This one lists the workflows *this
    /// principal may actually run*, which is the useful answer — a caller that
    /// reads a skill here can call it.
    fn a2a_extended_card(&self, principal: &Principal) -> Value {
        if principal.is_anonymous() {
            return err_obj(-32007, "the extended card requires an authenticated caller");
        }
        let mut card = self.a2a_agent_card();
        let skills: Vec<Value> = self
            .workflows
            .values()
            .filter(|w| principal.may_command(&format!("workflow.run:{}", w.name)))
            .map(|w| json!({"id": w.name, "name": w.name, "description": w.description.clone().unwrap_or_default(), "tags": ["workflow"]}))
            .collect();
        card["skills"] = json!(skills);
        card["supportsAuthenticatedExtendedCard"] = json!(true);
        card
    }

    // ---- task lifecycle ----------------------------------------------------

    /// Create + persist a fresh task; publish it to the shared view.
    /// Create a task, taking the listener's reserved id when the request that
    /// is being served brought one (see [`Runtime::reserved_task_id`]).
    pub(crate) fn task_create(&mut self, ctx: &str, principal: &Principal, link: Link) -> String {
        let id = match self.reserved_task_id.take() {
            Some(id) => id,
            None => new_task_id(),
        };
        let task = Task::new(&id, ctx, Some(&principal.id), link);
        self.tasks.insert(id.clone(), task);
        self.task_persist(&id);
        self.task_sync(&id);
        id
    }

    /// A command that finishes at once: create the task already terminal.
    fn task_complete_now(
        &mut self,
        ctx: &str,
        principal: &Principal,
        link: Link,
        state: State,
        text: Option<String>,
        result: Option<Value>,
    ) -> Value {
        let id = self.task_create(ctx, principal, link);
        if let Some(t) = self.tasks.get_mut(&id) {
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(state, text);
        }
        self.task_persist(&id);
        self.task_sync(&id);
        json!({"task": self.tasks.get(&id).map(Task::to_a2a).unwrap_or(Value::Null)})
    }

    /// Publish a task transition: to A2A subscribers, and onto the interface
    /// feed (RFC 0032 §4) so every attached display client converges without
    /// polling.
    ///
    /// The A2A half is also what *settles a blocking send* — the protocol layer
    /// waits on this stream rather than polling a snapshot — so a task that
    /// never publishes is a task that never finishes as far as a caller can
    /// tell.
    pub(crate) fn task_sync(&self, id: &str) {
        let Some(sink) = &self.a2a_sink else {
            return;
        };
        let allow_private = self.settings.a2a.push.allow_private;
        match self.tasks.get(id) {
            Some(t) => {
                sink.status(
                    &t.id,
                    &t.context_id,
                    t.state.to_wire(),
                    t.message.as_deref(),
                    t.updated,
                );
                if t.state.is_terminal()
                    && let Some(a) = crate::a2a::wire::result_artifact(t)
                {
                    sink.artifact(&t.id, &t.context_id, a.clone());
                }
                // A caller that asked to be told rather than to watch. Fired
                // from here because this is the one place every transition
                // passes through, whatever caused it.
                if !t.push.is_empty() {
                    sink.push(t, allow_private);
                }
                self.feed_push(
                    "task",
                    FeedVis::Owner(t.principal.clone()),
                    json!({"task": t.to_a2a(), "link": t.link, "principal": t.principal}),
                );
            }
            None => {
                self.feed_push("task.removed", FeedVis::Operator, json!({"id": id}));
            }
        }
    }

    /// Persist a task if dirty (durable across restarts — `GetTask` survives).
    pub(crate) fn task_persist(&mut self, id: &str) {
        if !self.tasks.get(id).is_some_and(|t| t.dirty) {
            return;
        }
        let encoded = self.tasks.get(id).map(serde_json::to_value);
        match encoded {
            Some(Ok(v)) => {
                if let Err(e) = self.durable.put(crate::state::Kind::Task, id, v, None) {
                    self.log.warn(
                        "a2a.task.persist.fail",
                        json!({"task": id, "err": e.to_string()}),
                    );
                } else if let Some(t) = self.tasks.get_mut(id) {
                    t.dirty = false;
                }
            }
            Some(Err(e)) => self.log.warn(
                "a2a.task.encode.fail",
                json!({"task": id, "err": e.to_string()}),
            ),
            None => {}
        }
    }

    /// Resolve the task bound to a completed conversation turn (via its inbox
    /// event) and drive it to a terminal / input-required state.
    pub(crate) fn a2a_task_for_event(
        &mut self,
        event: Option<&str>,
        state: State,
        text: Option<String>,
        result: Option<Value>,
    ) {
        let Some(ev) = event else { return };
        let Some(task_id) = self.event_to_task.remove(ev) else {
            return;
        };
        if let Some(t) = self.tasks.get_mut(&task_id) {
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(state, text);
        }
        self.task_persist(&task_id);
        self.task_sync(&task_id);
    }

    /// Drive the task bound to a finished run to match the run's outcome.
    pub(crate) fn a2a_task_for_run(
        &mut self,
        task_id: &str,
        status: &str,
        output: Option<&Value>,
        error: Option<&str>,
    ) {
        if !self.tasks.contains_key(task_id) {
            return;
        }
        let state = State::from_run(status);
        if let Some(t) = self.tasks.get_mut(task_id) {
            if let Some(o) = output {
                t.set_result(o.clone());
            }
            t.transition(state, error.map(str::to_string));
        }
        self.task_persist(task_id);
        self.task_sync(task_id);
    }

    fn a2a_task_fail(&mut self, id: &str, err: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.transition(State::Failed, Some(err.to_string()));
        }
        self.task_persist(id);
        self.task_sync(id);
    }

    /// Restore durable tasks (called at startup); seed the shared view and
    /// re-arm run-linked human gates (RFC 0032 §16).
    pub(crate) fn restore_a2a_tasks(&mut self, envs: &[crate::store::Envelope]) {
        for env in envs {
            match serde_json::from_value::<Task>(env.state.clone()) {
                Ok(t) => {
                    let id = t.id.clone();
                    self.tasks.insert(id.clone(), t);
                    self.task_sync(&id);
                }
                Err(e) => self.log.warn(
                    "restore.task.corrupt",
                    json!({"id": env.id, "err": e.to_string()}),
                ),
            }
        }
        self.rebuild_human_asks();
    }
}

/// A compact run view for `workflow.status`.
fn run_view(id: &str, r: &crate::engine::RunState) -> Value {
    json!({"run": id, "workflow": r.workflow, "status": r.status.as_str(), "output": r.output, "error": r.error})
}

// ---- listener startup ------------------------------------------------------

/// What [`spawn_a2a_listener`] hands the runtime: the interface feed (when `interface.enabled`), the pairing state (when
/// `interface.pairing.enabled`), and the live listener — which must be kept,
/// because dropping it stops serving.
pub(crate) struct A2aServing {
    pub feed: Option<Arc<SharedFeed>>,
    pub pairing: Option<Arc<PairingState>>,
    pub listener: crate::a2a::serve::Listener,
}

/// Bind and start the A2A listener.
///
/// Everything protocol-shaped below this line belongs to `a2a-rs`; what is
/// assembled here is the identity posture — whether a credential is required at
/// all, which one counts, and what a bare loopback connection means — plus the
/// TLS identity that makes a client certificate readable in the first place.
pub(crate) fn spawn_a2a_listener(
    a2a: &crate::config::v2::A2a,
    interface: &crate::config::v2::Interface,
    events_tx: Sender<Event>,
    resolver: Resolver,
    env: &dyn Fn(&str) -> Option<String>,
    _write_timeout: Duration,
    log: Logger,
) -> Result<A2aServing, String> {
    use std::path::Path;
    let listen = a2a.listen.as_deref().ok_or("a2a.listen is not set")?;
    let crate::config::ServeTarget::Http {
        bind,
        tls: tls_scheme,
    } = crate::config::ServeTarget::parse(listen).map_err(|e| format!("a2a.listen: {e}"))?;
    let server_bearer = match &a2a.bearer {
        Some(b) => {
            Some(crate::sec::secret::resolve(&b.0, env).map_err(|e| format!("a2a.bearer: {e}"))?)
        }
        None => None,
    };
    // Pairing-code login (RFC 0032 §13): armed with the interface. On a
    // NON-loopback listener it also counts as "client auth exists" — an
    // uncredentialed caller then gets through as anonymous (able to call
    // exactly `Pair` + the public card) instead of 401.
    let pairing = if interface.enabled && interface.pairing.enabled {
        let role = interface
            .pairing
            .role
            .unwrap_or(crate::config::v2::Role::Operator);
        let ttl = interface
            .pairing
            .ttl
            .map(|d| d.0)
            .unwrap_or(Duration::from_secs(12 * 3600));
        Some(Arc::new(
            PairingState::new(role, ttl).map_err(|e| format!("interface.pairing: {e}"))?,
        ))
    } else {
        None
    };
    let loopback_listener = crate::net::http::is_loopback_host(crate::config::serve_host_of(&bind));
    let require_auth = a2a.tls.client_ca.is_some()
        || server_bearer.is_some()
        || (pairing.is_some() && !loopback_listener);

    let tls = if tls_scheme {
        let cert = a2a
            .tls
            .cert
            .as_deref()
            .ok_or("a2a.tls.cert is required for https")?;
        let key = a2a
            .tls
            .key
            .as_deref()
            .ok_or("a2a.tls.key is required for https")?;
        let acceptor = crate::net::tls::TlsAcceptor::from_paths(
            Path::new(cert),
            Path::new(key),
            a2a.tls.client_ca.as_deref().map(Path::new),
        )
        .map_err(|e| format!("a2a tls: {e}"))?;
        Some(acceptor.server_config())
    } else {
        None
    };

    // The interface feed (RFC 0032) exists only while `interface.enabled`.
    let feed = interface
        .enabled
        .then(|| Arc::new(SharedFeed::new(interface.debug)));
    let bridge = A2aBridge::with_feed(events_tx, resolver, feed.clone());

    let listener = crate::a2a::serve::spawn(
        &bind,
        crate::a2a::serve::Opts {
            auth: crate::a2a::serve::Auth {
                require_auth,
                server_bearer,
                pairing: pairing.clone(),
            },
            extra_origins: interface.origins.clone(),
            tls,
            request_timeout: bridge.request_timeout,
            stream_deadline: bridge.stream_deadline,
        },
        Arc::clone(&bridge),
        feed.clone(),
        log.clone(),
    )?;

    log.info("a2a.listen", json!({"authority": listen, "bound": listener.bound, "tls": tls_scheme, "mtls": a2a.tls.client_ca.is_some(), "require_auth": require_auth, "interface": interface.enabled, "interface_debug": interface.enabled && interface.debug, "pairing": pairing.is_some()}));
    Ok(A2aServing {
        feed,
        pairing,
        listener,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_ulids_so_two_lives_cannot_mint_the_same_one() {
        // The whole point of the ULID: `seq` restarts at 0 with the process,
        // and the ids of a previous life are still in the store.
        let a = new_task_id();
        let b = new_task_id();
        assert_ne!(a, b);
        assert!(a.starts_with("task-"), "the id keeps its prefix: {a}");
        assert_eq!(a.len(), "task-".len() + 26, "a 26-char ULID: {a}");
        assert!(a < b, "still time-sortable: {a} < {b}");
    }

    #[test]
    fn the_config_view_redacts_every_credential_the_schema_declares() {
        // Env/flag values land INLINE in the effective document, which is what
        // the `config` command answers with — so each of these is a live
        // credential on a remote surface until it is redacted.
        let doc = json!({
            "intelligence": {
                "model": "gpt-5",
                "token": "sk-inline-from-env",
                "headers": {"Authorization": "Bearer sk-inline-header", "X-Tenant": "acme"},
                "auth": {"kind": "static", "token": "sk-inline-auth", "value": "sk-inline-value"}
            },
            "a2a": {"listen": "https://0.0.0.0:8443", "bearer": "{{secret:A2A_BEARER}}",
                    "peers": [{"name": "p", "endpoint": "https://p", "headers": {"X-Key": "k-inline"}}]},
            "mcp": {"servers": [{"name": "s", "endpoint": "https://s",
                    "oauth": {"token_url": "u", "client_id": "c", "client_secret": "cs-inline"}}]},
            "security": {"aauth": {"provider": "p", "enroll_token": "et-inline"}},
            "webhooks": {"default_auth": {"hmac": {"secret": "hs-inline"}}}
        });
        let r = redact_settings(&doc);
        assert!(
            !r.to_string().contains("inline"),
            "no credential survives the view: {r}"
        );
        assert_eq!(r["intelligence"]["token"], REDACTED);
        assert_eq!(r["intelligence"]["auth"]["token"], REDACTED);
        assert_eq!(r["intelligence"]["auth"]["value"], REDACTED);
        assert_eq!(r["intelligence"]["headers"]["Authorization"], REDACTED);
        assert_eq!(r["a2a"]["peers"][0]["headers"]["X-Key"], REDACTED);
        assert_eq!(r["mcp"]["servers"][0]["oauth"]["client_secret"], REDACTED);
        assert_eq!(r["security"]["aauth"]["enroll_token"], REDACTED);
        assert_eq!(r["webhooks"]["default_auth"]["hmac"]["secret"], REDACTED);
        // The view still has to be worth reading: structure, non-secret values
        // and header NAMES stay, and a bare reference names its secret.
        assert_eq!(r["intelligence"]["model"], "gpt-5");
        assert_eq!(r["a2a"]["listen"], "https://0.0.0.0:8443");
        assert_eq!(r["mcp"]["servers"][0]["oauth"]["client_id"], "c");
        assert_eq!(r["a2a"]["bearer"], "{{secret:A2A_BEARER}}");
        assert!(r["intelligence"]["headers"].get("X-Tenant").is_some());
    }

    #[test]
    fn only_a_lone_secret_reference_survives_redaction() {
        // A reference NAMES a credential; a template that merely embeds one
        // carries inline material beside it, so it goes whole.
        assert!(is_bare_secret_ref("{{secret:TOKEN}}"));
        assert!(is_bare_secret_ref("{{secret-file:/run/secrets/tok}}"));
        assert!(!is_bare_secret_ref("Bearer {{secret:TOKEN}}"));
        assert!(!is_bare_secret_ref("{{secret:TOKEN}}-sk-tail"));
        assert!(!is_bare_secret_ref("{{secret:}}"));
        assert!(!is_bare_secret_ref("sk-plain"));
        let r = redact_settings(&json!({"intelligence": {"token": "Bearer {{secret:T}} sk-tail"}}));
        assert_eq!(r["intelligence"]["token"], REDACTED);
    }

    #[test]
    fn command_and_text_extraction() {
        let m = json!({"parts": [{"text": "please"}, {"data": {"agentd": {"op": "workflow.run", "name": "x"}}}]});
        assert_eq!(command_op(&m), Some("workflow.run".to_string()));
        assert_eq!(command_data(&m).unwrap()["name"], "x");
        assert_eq!(
            message_text(&json!({"parts": [{"text": "a"}, {"text": "b"}]})),
            "a\nb"
        );
        assert_eq!(command_op(&json!({"parts": [{"text": "hi"}]})), None);
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn mtls_san_resolves_to_the_matched_principal_role() {
        // RFC 0029 §10.3: the surfaced client-cert SAN/subject now drives the
        // principal — a SPIFFE URI SAN matches a `san` rule, and its role wins
        // over the bare management/operator fallback.
        use crate::a2a::Resolver;
        use crate::obs::log::{Comp, Level, LogCtx, Logger};

        let resolver = Resolver::build(
            &serde_json::from_value(json!({
                "principals": [
                    {"match": {"san": "spiffe://corp/ops/*"}, "role": "operator"},
                    {"match": {"san": "spiffe://corp/team/*"}, "role": "user", "grants": ["knowledge.*"]},
                ]
            }))
            .unwrap(),
            &|_| None,
        )
        .unwrap();
        let log = Logger::new(
            LogCtx {
                run_id: "t".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Agent,
                pid: 0,
                trace_id: None,
            },
            Level::Warn,
        );
        let (tx, _rx) = std::sync::mpsc::channel();
        let _ = log;
        let bridge = A2aBridge::new(tx, resolver);

        // A SPIFFE X.509-SVID (empty subject; identity in the URI SAN) under the
        // team trust path → the user role, labelled by its SAN.
        let p = bridge.principal_of(true, None, None, vec!["spiffe://corp/team/alice".into()]);
        assert_eq!(p.role, crate::config::v2::Role::User);
        assert_eq!(p.id, "user:spiffe://corp/team/alice");
        // A cert under the ops path → operator (a different rule).
        let op = bridge.principal_of(true, None, None, vec!["spiffe://corp/ops/root".into()]);
        assert!(op.is_operator());
        // A cert matching NO rule, with principals configured, is NOT operator
        // (the surfaced identity turns the allowlist on — mgmt no longer blanket-
        // grants operator once explicit rules exist).
        let anon = bridge.principal_of(true, None, None, vec!["spiffe://other/x".into()]);
        assert!(
            anon.is_anonymous(),
            "unmatched cert is denied, not operator"
        );
    }

    #[test]
    fn the_feed_scopes_replays_and_evicts() {
        let f = SharedFeed::new(true);
        // Visibility: owner events reach the owner + operators; operator events
        // only operators; `all` events everyone.
        f.push(
            "task",
            FeedVis::Owner(Some("user:a".into())),
            json!({"n": 1}),
        );
        f.push("status", FeedVis::Operator, json!({"n": 2}));
        f.push("lifecycle", FeedVis::All, json!({"n": 3}));
        f.push("task", FeedVis::Owner(None), json!({"n": 4})); // ownerless ⇒ operator
        let (op, cursor) = f.since(0, "operator", true, 100);
        assert_eq!(op.len(), 4, "operator sees all: {op:?}");
        assert_eq!(cursor, 4);
        assert!(op[0].get("_vis").is_none(), "the vis tag is stripped");
        let (a, cursor_a) = f.since(0, "user:a", false, 100);
        assert_eq!(a.len(), 2, "owner + all: {a:?}");
        assert_eq!(cursor_a, 4, "the cursor advances past invisible events");
        let (b, _) = f.since(0, "user:b", false, 100);
        assert_eq!(b.len(), 1, "only the `all` event");
        // Resume: seq > after.
        let (resumed, _) = f.since(2, "operator", true, 100);
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0]["seq"], 3);
        // Eviction: overflow the ring and confirm bounds/dropped move.
        for i in 0..(FEED_RING + 8) {
            f.push("task", FeedVis::All, json!({"i": i}));
        }
        let (newest, oldest, dropped) = f.bounds();
        assert_eq!(newest, 4 + (FEED_RING as u64) + 8);
        assert_eq!(dropped, 12, "4 seed + 8 overflow evicted");
        assert_eq!(oldest, newest - (FEED_RING as u64) + 1);
    }

    #[test]
    fn pairing_codes_rotate_verify_rate_limit_and_mint_sessions() {
        use crate::config::v2::Role;
        let p = PairingState::new(Role::Operator, Duration::from_secs(60)).unwrap();
        // Deterministic per window; distinct across windows; 6 digits.
        let w = crate::state::now_ms() / 1000 / PAIR_WINDOW_SECS;
        let (code, expires_in) = p.current_code();
        assert_eq!(code, p.code_for(w));
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert!(expires_in <= PAIR_WINDOW_SECS * 1000);
        assert_ne!(p.code_for(w), p.code_for(w + 1));
        // Two instances have different seeds ⇒ different codes (unpredictable).
        let q = PairingState::new(Role::Operator, Duration::from_secs(60)).unwrap();
        assert_ne!(p.code_for(w), q.code_for(w), "seeded from OS randomness");
        // The current AND previous window verify (grace); formatting tolerated.
        let prev = p.code_for(w.saturating_sub(1));
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        let (tok, exp) = p.pair(&spaced).unwrap();
        assert!(tok.starts_with("pat-") && tok.len() > 40, "{tok}");
        assert!(exp > crate::state::now_ms());
        let _ = p.pair(&prev).unwrap();
        assert_eq!(p.session_count(), 2);
        // The minted token resolves as a bearer; garbage does not.
        assert_eq!(p.check_bearer(&tok), Some(Role::Operator));
        assert_eq!(p.check_bearer("pat-nope"), None);
        assert_eq!(p.check_bearer("other"), None);
        // Rate limit: failures lock pairing out for the window.
        for _ in 0..PAIR_MAX_FAILS {
            assert!(p.pair("000000").is_err() || p.pair("999999").is_err());
        }
        let locked = p.pair(&p.current_code().0);
        assert!(
            locked.is_err() && locked.unwrap_err().contains("too many"),
            "even the right code is refused while locked out"
        );
        // Expired sessions stop resolving.
        let short = PairingState::new(Role::User, Duration::from_millis(1)).unwrap();
        let (t2, _) = short.pair(&short.current_code().0).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.check_bearer(&t2), None, "expired");
    }

    #[test]
    fn paired_principals_and_display_defaults() {
        use crate::config::v2::Role;
        assert!(paired_principal(Role::Operator).is_operator());
        let u = paired_principal(Role::User);
        assert_eq!(u.role, Role::User);
        assert_eq!(u.id, "user:paired");
        assert!(u.may("SendMessage", None) && !u.may_command("config.set"));
        assert!(default_display_top().contains(&"name".to_string()));
        assert!(default_display_bottom().contains(&"conn".to_string()));
        for item in default_display_top()
            .iter()
            .chain(default_display_bottom().iter())
        {
            assert!(
                crate::config::v2::DISPLAY_ITEMS.contains(&item.as_str()),
                "{item} is in the documented vocabulary"
            );
        }
    }

    #[test]
    fn fingerprints_ignore_moving_fields_and_truncation_marks_cuts() {
        let a = json!({"pid": 1, "age_ms": 100, "uptime_ms": 5});
        let b = json!({"pid": 1, "age_ms": 999, "uptime_ms": 777});
        assert_eq!(fingerprint(&a), fingerprint(&b), "age/uptime excluded");
        let c = json!({"pid": 2, "age_ms": 100});
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let big = "x".repeat(5000);
        let t = truncate_strings(json!({"out": big, "list": ["ok", "y".repeat(9000)]}), 4096);
        let out = t["out"].as_str().unwrap();
        assert!(out.len() < 5000 && out.contains("…(+904 bytes)"), "{out}");
        assert_eq!(t["list"][0], "ok");
        assert!(t["list"][1].as_str().unwrap().contains("bytes)"));
    }

    /// Callers may address a method with or without the historical `a2a.`
    /// prefix. (Frame construction and terminal classification moved to
    /// `a2a::wire` and to a2a-rs respectively.)
    #[test]
    fn a_method_may_be_addressed_with_or_without_the_prefix() {
        assert_eq!(bare("a2a.SendMessage"), "SendMessage");
        assert_eq!(bare("GetTask"), "GetTask");
    }
}
