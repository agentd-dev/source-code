// SPDX-License-Identifier: AGPL-3.0-only
//! The inbound **webhook** surface. A dedicated HTTP listener turns a
//! signed request into a workflow run: each `webhook` start node registers a
//! route (`path`, `methods`, per-node `auth`, `parallelism`, `on_overflow`,
//! `idempotency`), and an inbound request is
//!
//!   1. routed by path (+ method),
//!   2. **authenticated** per-node — HMAC-SHA256 over the raw body
//!      (GitHub/Stripe-style `X-Signature: sha256=…`), a required-header match, or
//!      a bearer — verified in constant time,
//!   3. **deduplicated** durably by its idempotency key (a replay of a
//!      delivery that was kept answers `duplicate` and never re-fires; one
//!      that was refused is processed again),
//!   4. **backpressured** per route (`parallelism` + `on_overflow`), then
//!   5. handed to the single-writer loop as an [`Event::Webhook`], which fires the
//!      run and replies (`respond: ack` → `202`).
//!
//! The listener rides the `mcp` crate's raw-HTTP server (`::mcp::http_server`)
//! over `net::tls` and never blocks the loop (one connection = one thread; the
//! reply arrives over a oneshot).
#![cfg(feature = "a2a")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::config::settings::{WebhookAuth, Webhooks};
use crate::engine::model::RESERVED_HOOK_PREFIX;
use crate::obs::log::Logger;
use crate::runtime::events::Event;
use crate::runtime::starts::Admission;
use crate::state::{Durable, Kind, now_ms};

/// Where the idempotency markers live in `Kind::Memory`. `_`-prefixed like
/// every other key agentd writes there, because `memory.set`, `get` and
/// `delete` refuse that prefix: a model — one steered by an injected payload
/// included — can neither pre-seed a marker, so that a real delivery is
/// answered as a duplicate, nor delete one, so that a replay fires twice.
pub(crate) const IDEM_PREFIX: &str = "_wh_idem/";

/// How long a marker answers a replay of its key: seven days.
///
/// A marker must outlive every retry its sender may still make, or a retry of
/// a delivery that was kept fires twice. The longest horizons among the
/// common senders are three days — Stripe retries for up to three days, and
/// GitHub redelivers on request any delivery from the past three days — and
/// a redelivery an operator starts after a long weekend comes later still.
/// Seven days covers both. A longer TTL costs only store space,
/// which the sweep bounds; a shorter one would let a late retry through.
pub(crate) const IDEM_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How often the tick sweeps expired markers, and how soon it comes back
/// while a sweep has more to look at than one pass reads.
const IDEM_SWEEP_EVERY: Duration = Duration::from_secs(600);
const IDEM_SWEEP_BACKLOG: Duration = Duration::from_secs(1);

/// Markers one sweep pass reads. Each read is a store call on the
/// single-writer loop, so a pass is bounded and a large set is worked through
/// across passes.
const IDEM_SWEEP_BUDGET: usize = 512;

/// Where the marker sweep is: when it runs next (`None`: now), and the last
/// marker the previous pass read when it stopped on its budget.
#[derive(Debug, Default)]
pub(crate) struct IdemSweep {
    next: Option<std::time::Instant>,
    cursor: Option<String>,
}

/// The marker id for a delivery: per route, keyed by a hash of the sender's
/// idempotency key so the key's bytes never become part of a store key.
pub(crate) fn idem_marker_id(workflow: &str, node: &str, key: &str) -> String {
    format!(
        "{IDEM_PREFIX}{workflow}/{node}/{}",
        crate::sha::sha256_hex(key.as_bytes())
    )
}

/// Whether a stored marker still answers a replay at `now`. A marker without
/// a readable time answers nothing: refusing a delivery as a duplicate needs
/// evidence that it was kept, and recently.
fn marker_live(state: &Value, now: u64) -> bool {
    state
        .get("at")
        .and_then(Value::as_u64)
        .is_some_and(|at| now < at.saturating_add(IDEM_TTL_MS))
}

/// Whether the marker `id` still answers a replay at `now`. An expired marker
/// that the sweep has not reached yet answers nothing, so expiry does not
/// depend on the sweep.
fn is_replay(d: &Durable, id: &str, now: u64) -> bool {
    matches!(d.get(Kind::Memory, id), Ok(Some(env)) if marker_live(&env.state, now))
}

/// One sweep pass's result.
#[derive(Debug, PartialEq)]
struct SweepPass {
    removed: usize,
    /// The last marker read when the budget ran out; `None` when the pass
    /// reached the end.
    resume_after: Option<String>,
}

/// Delete the markers past their TTL, reading at most `budget` of them, in
/// key order after `after`.
fn sweep_markers(
    d: &Durable,
    now: u64,
    after: Option<&str>,
    budget: usize,
) -> Result<SweepPass, crate::store::StoreError> {
    let mut ids: Vec<String> = d
        .list(Kind::Memory)?
        .iter()
        .filter_map(|ks| crate::store::parse_key(d.prefix(), d.instance(), &ks.key))
        .map(|(_, id)| id)
        .filter(|id| id.starts_with(IDEM_PREFIX) && after.is_none_or(|a| *id > a))
        .map(str::to_string)
        .collect();
    ids.sort();
    let more = ids.len() > budget;
    ids.truncate(budget);
    let mut removed = 0;
    for id in &ids {
        if let Ok(Some(env)) = d.get(Kind::Memory, id)
            && !marker_live(&env.state, now)
            && d.delete(Kind::Memory, id).is_ok()
        {
            removed += 1;
        }
    }
    Ok(SweepPass {
        removed,
        resume_after: if more { ids.pop() } else { None },
    })
}

/// The answer to a delivery whose start node did not fire. A refusal that can
/// pass is `503`, so the sender retries; an `inputs` mapping that cannot
/// render this delivery is `422`, because retrying it fails the same way.
fn not_fired(admitted: Admission) -> WebhookReply {
    let cause = match admitted {
        Admission::Accepted => "accepted",
        Admission::Shed => "shed",
        Admission::Frozen => "frozen",
        Admission::InboxFailed => "inbox_failed",
        Admission::InputsInvalid => "inputs_invalid",
    };
    if admitted == Admission::InputsInvalid {
        WebhookReply::ok(
            422,
            "Unprocessable Entity",
            json!({"error": "the delivery does not render the start node's inputs", "cause": cause}),
        )
    } else {
        WebhookReply::ok(
            503,
            "Service Unavailable",
            json!({"error": "the run was not started", "cause": cause}),
        )
    }
}

/// A webhook request awaiting a loop-computed reply.
#[derive(Debug)]
pub struct WebhookRequest {
    pub workflow: String,
    pub node: String,
    /// The dedup key (from the idempotency header), if any.
    pub idem_key: Option<String>,
    /// `{method, path, headers, body, raw_body}` handed to the workflow `inputs`.
    pub payload: Value,
    /// `respond: sync` — hold the HTTP response until the fired run reaches a
    /// terminal status and return its result (vs. `ack` → immediate `202`).
    pub respond_sync: bool,
    /// Set when this is an **await callback** (a `wait: {on: webhook}` resume): the
    /// signal token to deliver, and the path to unregister.
    pub callback: Option<(String, String)>, // (signal, path)
    pub reply: SyncSender<WebhookReply>,
}

/// A dynamically-registered webhook-**await** callback: an inbound request at
/// `path` resumes the suspended run/step by delivering `signal`. Populated by the
/// loop when a `wait: {on: webhook}` step suspends; read by the listener thread.
pub struct Callback {
    pub signal: String,
    pub verify: Verify,
    pub expires_ms: u64,
}

/// The await-callback registry shared between the loop (writer) and the listener
/// (reader).
pub type SharedCallbacks = std::sync::Arc<std::sync::Mutex<HashMap<String, Callback>>>;

/// The loop's answer to a webhook request.
pub struct WebhookReply {
    pub status: u16,
    pub reason: &'static str,
    pub body: Value,
}

impl WebhookReply {
    fn ok(status: u16, reason: &'static str, body: Value) -> WebhookReply {
        WebhookReply {
            status,
            reason,
            body,
        }
    }
}

/// A resolved per-node verification (secrets resolved at spawn time).
pub enum Verify {
    None,
    Hmac {
        secret: Vec<u8>,
        header: String,
        prefix: String,
    },
    Header {
        name: String,
        equals: String,
    },
    Bearer {
        token: String,
    },
}

/// Refuse an `algo` we do not implement.
///
/// The verifier calls `hmac_sha256` unconditionally, and `algo` was read by
/// nobody: `algo: sha512` validated clean, computed SHA-256, and every external
/// sender signing SHA-512 got a 401 with nothing anywhere explaining why. A
/// silent downgrade in a security field is the worst of the three outcomes —
/// worse than not offering the knob, and worse than refusing it — because the
/// operator believes something stronger is in force and no signal contradicts
/// them.
///
/// Refusing is the honest half of the choice. Implementing other digests would
/// also be honest; this is the smaller change and it closes the misbelief.
fn check_hmac_algo(algo: Option<&str>, at: &str) -> Result<(), String> {
    match algo {
        None => Ok(()),
        Some(a) if a.eq_ignore_ascii_case("sha256") => Ok(()),
        Some(a) => Err(format!(
            "{at}.algo {a:?} is not implemented — agentd computes HMAC-SHA256 only; \
             use `algo: sha256` (or omit it) and have senders sign SHA-256"
        )),
    }
}

impl Verify {
    fn check(&self, req: &::mcp::http_server::RawRequest) -> bool {
        match self {
            Verify::None => true,
            Verify::Hmac {
                secret,
                header,
                prefix,
            } => {
                let Some(sig) = req.header(&header.to_ascii_lowercase()) else {
                    return false;
                };
                let sig = sig.strip_prefix(prefix.as_str()).unwrap_or(sig);
                let mac = crate::sha::hmac_sha256(secret, &req.body);
                crate::sha::ct_eq(crate::sha::to_hex(&mac).as_bytes(), sig.as_bytes())
            }
            Verify::Header { name, equals } => req
                .header(&name.to_ascii_lowercase())
                .is_some_and(|v| crate::sha::ct_eq(v.as_bytes(), equals.as_bytes())),
            Verify::Bearer { token } => req
                .header("authorization")
                .and_then(|a| {
                    a.strip_prefix("Bearer ")
                        .or_else(|| a.strip_prefix("bearer "))
                })
                .is_some_and(|t| crate::sha::ct_eq(t.as_bytes(), token.as_bytes())),
        }
    }
}

#[derive(Clone, Copy)]
enum Overflow {
    Reject,
    Drop,
    Queue,
}

struct Route {
    workflow: String,
    node: String,
    /// Uppercase methods; empty = any.
    methods: Vec<String>,
    verify: Verify,
    parallelism: Option<usize>,
    on_overflow: Overflow,
    /// The idempotency-key header (lowercased); `None` = no dedup.
    idem_header: Option<String>,
    /// `respond: sync` — hold the response for the run's terminal result.
    respond_sync: bool,
    /// Live state, shared rather than owned, so a reload that rebuilds this
    /// route can CARRY IT OVER: resetting the counter under requests that are
    /// still running would let the parallelism gate admit past its bound, and
    /// resetting the bucket would hand a caller a fresh burst allowance every
    /// time an unrelated config key changed.
    inflight: Arc<AtomicUsize>,
    /// Per-route arrival rate (`rate: "<burst>/<per>s"`). `parallelism` bounds
    /// how many run at ONCE; this bounds how fast they ARRIVE — without it an
    /// inbound burst is written to the durable inbox as fast as the socket
    /// delivers it, which converts the burst straight into disk pressure.
    rate: Option<Arc<std::sync::Mutex<crate::supervisor::tree::TokenBucket>>>,
    /// `Retry-After` for a rate refusal: roughly when a token will exist.
    retry_after_s: u32,
    /// The owning workflow declared `priority: low` — its admissions shed one
    /// pressure level earlier (at warn).
    low_priority: bool,
}

/// Decrement the route's in-flight counter on drop.
///
/// Holds an `Arc` rather than a borrow: a reload can swap the route map while
/// a request is in flight, and the counter this guard decrements must be the
/// one it incremented, not whichever route happens to sit at that path later.
struct InflightGuard(Arc<AtomicUsize>);
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct WebhookHandler {
    /// Swappable: a reload rebuilds the table and installs it here, so a
    /// rotated route secret or a workflow's new `webhook` node takes effect
    /// without a restart. Read once per request; written only by a reload.
    routes: std::sync::RwLock<Arc<RouteMap>>,
    /// Admission-gates inbound requests under disk/memory pressure (429).
    pressure: std::sync::Arc<super::pressure::Pressure>,
    /// The dynamic `wait: {on: webhook}` await callbacks (shared with the loop).
    callbacks: SharedCallbacks,
    events_tx: Sender<Event>,
    timeout: Duration,
    log: Logger,
}

impl ::mcp::http_server::RawHandler for WebhookHandler {
    fn handle(&self, req: &::mcp::http_server::RawRequest) -> ::mcp::http_server::RawResponse {
        use ::mcp::http_server::RawResponse as R;
        let path = req.path().to_string();
        // A static `webhook` start-node route. The table is cloned out of the
        // lock so a concurrent reload never blocks (or is blocked by) a
        // request in flight.
        let routes = self.routes();
        if let Some(route) = routes.get(&path) {
            return self.handle_start(req, route, &path);
        }
        // A dynamic `wait: {on: webhook}` await callback (registered by the loop).
        let matched = {
            let mut map = self.callbacks.lock().unwrap();
            match map.get(&path) {
                Some(cb) if cb.expires_ms >= now_ms() => {
                    Some((cb.signal.clone(), cb.verify.check(req)))
                }
                Some(_) => {
                    map.remove(&path);
                    None
                }
                None => None,
            }
        };
        if let Some((signal, ok)) = matched {
            if !ok {
                self.log.warn(
                    "webhook.denied",
                    json!({"path": path, "reason": "auth", "kind": "callback"}),
                );
                return R::text(401, "Unauthorized", b"authentication failed".to_vec());
            }
            let payload = self.payload(req, &path);
            let cb_path = path.clone();
            return self.dispatch(move |tx| WebhookRequest {
                workflow: String::new(),
                node: String::new(),
                idem_key: None,
                payload,
                respond_sync: false,
                callback: Some((signal, cb_path)),
                reply: tx,
            });
        }
        R::text(404, "Not Found", b"no webhook at this path".to_vec())
    }
}

impl WebhookHandler {
    /// The route table currently being served.
    fn routes(&self) -> Arc<RouteMap> {
        self.routes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Install a rebuilt route table — a reload of `webhooks.default_auth` or
    /// of the `workflows[]` the routes come from.
    ///
    /// Requests already dispatched keep the route they matched; the next
    /// request matches against this table.
    fn set_routes(&self, routes: RouteMap) {
        *self.routes.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(routes);
    }

    /// Build the table a reload would serve, carrying live state across,
    /// WITHOUT installing it: a route that cannot be built (an unresolvable
    /// secret, a bad `rate`) refuses the reload while it is staged, and
    /// [`install_routes`](Self::install_routes) puts the table in only once
    /// the whole reload applies.
    pub(crate) fn stage_routes(
        &self,
        nodes: Vec<WebhookNode>,
        webhooks: &Webhooks,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<StagedRoutes, String> {
        let prev = self.routes();
        let map = build_routes(nodes, webhooks, env, Some(&prev))?;
        let paths: Vec<String> = map.keys().cloned().collect();
        Ok(StagedRoutes { map, paths })
    }

    /// Serve a staged table. Returns its paths.
    pub(crate) fn install_routes(&self, staged: StagedRoutes) -> Vec<String> {
        self.set_routes(staged.map);
        staged.paths
    }

    /// The `{method, path, headers, body, raw_body}` payload handed to the workflow.
    fn payload(&self, req: &::mcp::http_server::RawRequest, path: &str) -> Value {
        let body_json = serde_json::from_slice::<Value>(&req.body).ok();
        json!({
            "method": req.method,
            "path": path,
            "headers": header_map(req),
            "body": body_json.unwrap_or_else(|| json!(String::from_utf8_lossy(&req.body))),
            "raw_body": String::from_utf8_lossy(&req.body),
        })
    }

    /// Post a webhook request to the loop and block for its reply.
    fn dispatch(
        &self,
        build: impl FnOnce(SyncSender<WebhookReply>) -> WebhookRequest,
    ) -> ::mcp::http_server::RawResponse {
        use ::mcp::http_server::RawResponse as R;
        let (tx, rx) = sync_channel(1);
        if self
            .events_tx
            .send(Event::Webhook(Box::new(build(tx))))
            .is_err()
        {
            return R::text(
                503,
                "Service Unavailable",
                b"runtime shutting down".to_vec(),
            );
        }
        match rx.recv_timeout(self.timeout) {
            Ok(reply) => R::json(
                reply.status,
                reply.reason,
                serde_json::to_vec(&reply.body).unwrap_or_default(),
            ),
            Err(_) => R::text(
                504,
                "Gateway Timeout",
                b"the runtime did not answer".to_vec(),
            ),
        }
    }

    /// Serve a static `webhook` start-node route: method + auth + backpressure +
    /// idempotency, then fire the workflow.
    fn handle_start(
        &self,
        req: &::mcp::http_server::RawRequest,
        route: &Route,
        path: &str,
    ) -> ::mcp::http_server::RawResponse {
        use ::mcp::http_server::RawResponse as R;
        if !route.methods.is_empty()
            && !route
                .methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&req.method))
        {
            return R::text(405, "Method Not Allowed", b"method not allowed".to_vec());
        }
        if !route.verify.check(req) {
            self.log
                .warn("webhook.denied", json!({"path": path, "reason": "auth"}));
            return R::text(401, "Unauthorized", b"authentication failed".to_vec());
        }

        // Admission, after authentication: an unauthenticated caller learns
        // nothing about our load, and a legitimate one gets the honest HTTP
        // answer — 429 with a Retry-After — instead of an inbox write the disk
        // may be too full to keep.
        if let Some(cause) = self.pressure.refusal(route.low_priority) {
            let mut r = R::text(
                429,
                "Too Many Requests",
                format!("shedding: {cause}").into_bytes(),
            );
            r.headers.push(("Retry-After", "30".into()));
            return r;
        }
        if let Some(bucket) = &route.rate
            && !bucket.lock().unwrap_or_else(|e| e.into_inner()).try_take()
        {
            let mut r = R::text(429, "Too Many Requests", b"rate limited".to_vec());
            r.headers
                .push(("Retry-After", route.retry_after_s.to_string()));
            return r;
        }
        // Inbound backpressure (bounds concurrent handling per route; run-duration
        // limits are the workflow's own `concurrency`).
        let _guard;
        if let Some(max) = route.parallelism {
            if route.inflight.load(Ordering::SeqCst) >= max {
                match route.on_overflow {
                    Overflow::Reject => {
                        self.log.warn(
                            "webhook.overflow",
                            json!({"path": path, "action": "reject"}),
                        );
                        return R::text(
                            503,
                            "Service Unavailable",
                            b"webhook at capacity".to_vec(),
                        );
                    }
                    Overflow::Drop => {
                        self.log
                            .warn("webhook.overflow", json!({"path": path, "action": "drop"}));
                        return R::json(
                            200,
                            "OK",
                            br#"{"status":"dropped","reason":"at_capacity"}"#.to_vec(),
                        );
                    }
                    Overflow::Queue => {}
                }
            }
            route.inflight.fetch_add(1, Ordering::SeqCst);
            _guard = InflightGuard(Arc::clone(&route.inflight));
        }
        let idem_key = route
            .idem_header
            .as_ref()
            .and_then(|h| req.header(h))
            .map(str::to_string);
        let payload = self.payload(req, path);
        let (workflow, node, respond_sync) = (
            route.workflow.clone(),
            route.node.clone(),
            route.respond_sync,
        );
        self.dispatch(move |tx| WebhookRequest {
            workflow,
            node,
            idem_key,
            payload,
            respond_sync,
            callback: None,
            reply: tx,
        })
    }
}

/// A subset of inbound headers exposed to the workflow (drops hop-by-hop /
/// signature headers, keeps the useful metadata).
fn header_map(req: &::mcp::http_server::RawRequest) -> Value {
    let mut m = Map::new();
    for (k, v) in &req.headers {
        if matches!(
            k.as_str(),
            "authorization" | "connection" | "content-length" | "host"
        ) {
            continue;
        }
        m.insert(k.clone(), json!(v));
    }
    Value::Object(m)
}

/// Build a route's [`Verify`] from the node's `auth` (a raw Value), falling back
/// to the listener `default_auth`. Secrets resolve through `env`.
fn build_verify(
    node_auth: Option<&Value>,
    default: Option<&WebhookAuth>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Verify, String> {
    // Prefer the node's own auth; else the listener default.
    if let Some(a) = node_auth {
        if a.get("none").and_then(Value::as_bool) == Some(true) {
            return Ok(Verify::None);
        }
        if let Some(h) = a.get("hmac").and_then(Value::as_object) {
            let secret_ref = h
                .get("secret")
                .and_then(Value::as_str)
                .ok_or("webhook auth.hmac.secret is required")?;
            check_hmac_algo(h.get("algo").and_then(Value::as_str), "webhook auth.hmac")?;
            let secret = crate::sec::secret::resolve(secret_ref, env)
                .map_err(|e| format!("webhook auth.hmac.secret: {e}"))?;
            return Ok(Verify::Hmac {
                secret: secret.into_bytes(),
                header: h
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or("X-Signature")
                    .to_string(),
                prefix: h
                    .get("prefix")
                    .and_then(Value::as_str)
                    .unwrap_or("sha256=")
                    .to_string(),
            });
        }
        if let Some(hd) = a.get("header").and_then(Value::as_object) {
            let name = hd
                .get("name")
                .and_then(Value::as_str)
                .ok_or("webhook auth.header.name is required")?;
            let eq = hd
                .get("equals")
                .and_then(Value::as_str)
                .ok_or("webhook auth.header.equals is required")?;
            let equals = crate::sec::secret::resolve(eq, env)
                .map_err(|e| format!("webhook auth.header.equals: {e}"))?;
            return Ok(Verify::Header {
                name: name.to_string(),
                equals,
            });
        }
        if let Some(b) = a.get("bearer").and_then(Value::as_str) {
            let token = crate::sec::secret::resolve(b, env)
                .map_err(|e| format!("webhook auth.bearer: {e}"))?;
            return Ok(Verify::Bearer { token });
        }
    }
    if let Some(d) = default {
        return build_verify_typed(d, env);
    }
    // No auth declared and no default — allowed only on a loopback bind (the
    // config validator warns on a non-loopback listener without a default).
    Ok(Verify::None)
}

fn build_verify_typed(
    d: &WebhookAuth,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Verify, String> {
    if d.none {
        return Ok(Verify::None);
    }
    if let Some(h) = &d.hmac {
        let secret_ref = h
            .secret
            .as_ref()
            .ok_or("webhooks.default_auth.hmac.secret is required")?;
        check_hmac_algo(h.algo.as_deref(), "webhooks.default_auth.hmac")?;
        let secret = crate::sec::secret::resolve(&secret_ref.0, env)
            .map_err(|e| format!("webhooks.default_auth.hmac.secret: {e}"))?;
        return Ok(Verify::Hmac {
            secret: secret.into_bytes(),
            header: h.header.clone().unwrap_or_else(|| "X-Signature".into()),
            prefix: h.prefix.clone().unwrap_or_else(|| "sha256=".into()),
        });
    }
    if let Some(b) = &d.bearer {
        let token = crate::sec::secret::resolve(&b.0, env)
            .map_err(|e| format!("webhooks.default_auth.bearer: {e}"))?;
        return Ok(Verify::Bearer { token });
    }
    if let Some(hd) = &d.header {
        let name = hd.name.clone().ok_or("webhooks.default_auth.header.name")?;
        let equals = crate::sec::secret::resolve(
            &hd.equals
                .as_ref()
                .ok_or("webhooks.default_auth.header.equals")?
                .0,
            env,
        )
        .map_err(|e| format!("webhooks.default_auth.header.equals: {e}"))?;
        return Ok(Verify::Header { name, equals });
    }
    Ok(Verify::None)
}

/// The idempotency-key header for a node (`None` = dedup off). Default: on, by the
/// standard `Idempotency-Key` header (best practice).
fn idem_header(spec: &Map<String, Value>) -> Option<String> {
    match spec.get("idempotency") {
        Some(Value::Bool(false)) => None,
        Some(Value::String(s)) if s == "header" => Some("idempotency-key".into()),
        Some(Value::String(s)) => Some(s.to_ascii_lowercase()),
        _ => Some("idempotency-key".into()),
    }
}

fn overflow_of(spec: &Map<String, Value>) -> Overflow {
    match spec.get("on_overflow").and_then(Value::as_str) {
        Some("drop") => Overflow::Drop,
        Some("queue") => Overflow::Queue,
        _ => Overflow::Reject,
    }
}

/// The live route table, swapped wholesale by a reload.
type RouteMap = HashMap<String, Route>;

/// A route table built for a reload and not yet served.
pub(crate) struct StagedRoutes {
    map: RouteMap,
    paths: Vec<String>,
}

/// Compile the `webhook` start nodes into a route table.
///
/// `prev` is the table currently being served, when this is a REBUILD: live
/// per-route state (the in-flight counter, the rate bucket) is carried across
/// for paths whose shaping knobs are unchanged, so reloading an unrelated key
/// neither resets a rate limiter nor loses count of requests still running.
fn build_routes(
    nodes: Vec<WebhookNode>,
    webhooks: &Webhooks,
    env: &dyn Fn(&str) -> Option<String>,
    prev: Option<&RouteMap>,
) -> Result<RouteMap, String> {
    let mut routes: RouteMap = HashMap::new();
    for (workflow, node, spec, low_priority) in nodes {
        let path = spec
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("webhook node '{workflow}/{node}': path is required"))?
            .to_string();
        let methods = spec
            .get("methods")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|m| m.to_ascii_uppercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let verify = build_verify(spec.get("auth"), webhooks.default_auth.as_ref(), env)
            .map_err(|e| format!("webhook node '{workflow}/{node}': {e}"))?;
        let parallelism = spec
            .get("parallelism")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let path_for_rate = path.clone();
        let (rate, retry_after_s) = match spec.get("rate").and_then(Value::as_str) {
            None => (None, 30),
            Some(r) => {
                let (burst, per_s) = crate::supervisor::tree::parse_rate(r)
                    .map_err(|e| format!("webhook node '{workflow}/{node}': rate: {e}"))?;
                let retry = (per_s / burst.max(1) as f64).ceil().max(1.0) as u32;
                (
                    Some(match prev.and_then(|m| m.get(&path_for_rate)) {
                        // Same rate spec ⇒ same bucket, so a reload cannot be
                        // used (accidentally or otherwise) to refill an
                        // exhausted allowance.
                        Some(r) if r.rate.is_some() && r.retry_after_s == retry => {
                            Arc::clone(r.rate.as_ref().expect("is_some checked"))
                        }
                        _ => Arc::new(std::sync::Mutex::new(
                            crate::supervisor::tree::TokenBucket::new(burst, burst as f64 / per_s),
                        )),
                    }),
                    retry,
                )
            }
        };
        if let Some(prev) = routes.insert(
            path.clone(),
            Route {
                workflow: workflow.clone(),
                node: node.clone(),
                methods,
                verify,
                parallelism,
                on_overflow: overflow_of(&spec),
                idem_header: idem_header(&spec),
                respond_sync: spec.get("respond").and_then(Value::as_str) == Some("sync"),
                // Carry the live counter and bucket over from the route
                // that held this path before the rebuild, when the knob that
                // shapes them is unchanged. A rebuilt route is the same route
                // to a caller mid-request; only its rules changed.
                inflight: prev
                    .and_then(|m| m.get(&path))
                    .filter(|r| r.parallelism == parallelism)
                    .map(|r| Arc::clone(&r.inflight))
                    .unwrap_or_default(),
                rate,
                retry_after_s,
                low_priority,
            },
        ) {
            return Err(format!(
                "two webhook nodes bind the same path '{path}' ('{}/{}' and '{workflow}/{node}')",
                prev.workflow, prev.node
            ));
        }
    }

    Ok(routes)
}

/// Spawn the webhook listener from the configured `webhook` start nodes. Each
/// `nodes` entry is `(workflow, node, spec)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_webhook_listener(
    webhooks: &Webhooks,
    nodes: Vec<WebhookNode>,
    callbacks: SharedCallbacks,
    events_tx: Sender<Event>,
    env: &dyn Fn(&str) -> Option<String>,
    write_timeout: Duration,
    pressure: std::sync::Arc<super::pressure::Pressure>,
    log: Logger,
) -> Result<Arc<WebhookHandler>, String> {
    use std::path::Path;
    let listen = webhooks
        .listen
        .as_deref()
        .ok_or("webhooks.listen is not set")?;
    let crate::config::ServeTarget::Http {
        bind,
        tls: tls_scheme,
    } = crate::config::ServeTarget::parse(listen).map_err(|e| format!("webhooks.listen: {e}"))?
    else {
        return Err("webhooks.listen does not support unix://; use https://".into());
    };

    let acceptor = if tls_scheme {
        let cert = webhooks
            .tls
            .cert
            .as_deref()
            .ok_or("webhooks.tls.cert is required for https")?;
        let key = webhooks
            .tls
            .key
            .as_deref()
            .ok_or("webhooks.tls.key is required for https")?;
        let tls = crate::net::tls::TlsAcceptor::from_paths(
            Path::new(cert),
            Path::new(key),
            webhooks.tls.client_ca.as_deref().map(Path::new),
        )
        .map_err(|e| format!("webhooks tls: {e}"))?;
        ::mcp::http_server::HttpAcceptor::Tls(tls)
    } else {
        ::mcp::http_server::HttpAcceptor::Plain
    };

    let routes = build_routes(nodes, webhooks, env, None)?;

    let listener =
        ::mcp::http_server::bind_tcp(&bind).map_err(|e| format!("webhooks bind {bind}: {e}"))?;
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen.to_string());
    let route_paths: Vec<String> = routes.keys().cloned().collect();
    let handler = Arc::new(WebhookHandler {
        routes: std::sync::RwLock::new(Arc::new(routes)),
        pressure,
        callbacks,
        events_tx,
        timeout: write_timeout,
        log: log.clone(),
    });
    ::mcp::http_server::spawn_accept_raw(
        listener,
        Arc::new(acceptor),
        Arc::clone(&handler) as Arc<dyn ::mcp::http_server::RawHandler>,
        write_timeout,
    )
    .map_err(|e| format!("webhooks accept: {e}"))?;
    log.info(
        "webhooks.listen",
        json!({"authority": listen, "bound": bound, "tls": tls_scheme, "routes": route_paths}),
    );
    Ok(handler)
}

// ---- the loop side: idempotency + fire the run ------------------------------

/// One `webhook` start node, as the route builder wants it:
/// `(workflow, node, spec, low_priority)`.
pub(crate) type WebhookNode = (String, String, Map<String, Value>, bool);

impl crate::runtime::reactor::Runtime {
    /// Every `webhook` start node across the armed workflows.
    ///
    /// One definition, used by both the startup bind and the reload rebuild —
    /// two copies of "which nodes are routes" would drift the way the
    /// long-lived-start lists did.
    pub(crate) fn webhook_nodes(&self) -> Vec<WebhookNode> {
        webhook_nodes_of(&self.workflows)
    }
}

/// The `webhook` start nodes of a workflow set — the running one, or the one
/// a reload has staged and not yet installed.
pub(crate) fn webhook_nodes_of(
    workflows: &std::collections::BTreeMap<String, Arc<crate::engine::Workflow>>,
) -> Vec<WebhookNode> {
    workflows
        .values()
        .flat_map(|wf| {
            let low = wf.priority == crate::engine::model::Priority::Low;
            wf.steps
                .values()
                .filter(|s| s.kind == "webhook")
                .map(|s| (wf.name.clone(), s.id.clone(), s.spec.clone(), low))
                .collect::<Vec<_>>()
        })
        .collect()
}

impl crate::runtime::reactor::Runtime {
    /// Handle a webhook request on the single-writer loop: deduplicate durably by
    /// idempotency key, then fire the workflow's `webhook` start node.
    pub(crate) fn on_webhook_request(&mut self, req: WebhookRequest) {
        let WebhookRequest {
            workflow,
            node,
            idem_key,
            payload,
            respond_sync,
            callback,
            reply,
        } = req;

        // An await callback (`wait: {on: webhook}`): deliver the signal to resume
        // the suspended run/step, and unregister the one-shot route.
        if let Some((signal, path)) = callback {
            self.webhook_callbacks.lock().unwrap().remove(&path);
            // An operator-configured route: the runtime is the sender.
            let delivered = self.deliver_signal(
                &signal,
                payload,
                None,
                None,
                &super::waits::SignalSender::Runtime,
            );
            self.log.info(
                "webhook.callback",
                json!({"path": path, "resumed": delivered}),
            );
            let ok = delivered > 0;
            let _ = reply.send(WebhookReply::ok(
                if ok { 200 } else { 404 },
                if ok { "OK" } else { "Not Found" },
                json!({"status": if ok { "resumed" } else { "no-waiter" }, "resumed": delivered}),
            ));
            return;
        }

        // Durable idempotency: a replay of a key whose delivery was KEPT answers
        // `duplicate` and never re-fires. The marker is written below, only once
        // the run is in the inbox or the event is on its stream — a delivery
        // that was refused leaves no marker, so the sender's retry is processed.
        let marker = idem_key
            .as_deref()
            .map(|key| idem_marker_id(&workflow, &node, key));
        if let (Some(id), Some(key)) = (&marker, &idem_key)
            && is_replay(&self.durable, id, now_ms())
        {
            self.log.info(
                "webhook.duplicate",
                json!({"workflow": workflow, "node": node}),
            );
            let _ = reply.send(WebhookReply::ok(
                200,
                "OK",
                json!({"status": "duplicate", "idempotency_key": key}),
            ));
            return;
        }

        // Fire the webhook start node (its `inputs` mapping sees `payload`).
        let spec = self
            .workflows
            .get(&workflow)
            .and_then(|w| w.step(&node))
            .map(|s| s.spec.clone());
        match spec {
            Some(spec) => {
                // `filter` (CEL over the delivery): selects which arrivals are
                // interesting to this route. One endpoint typically receives a
                // provider's whole event feed, so a drop is a successful
                // delivery that started no run — NOT a 4xx, which senders read
                // as a broken hook and answer with retries or by disabling it.
                // The filter sees the payload's fields at the top level, the
                // same shape the sibling `signal:` template renders against.
                if let Some(filter) = spec.get("filter").and_then(Value::as_str) {
                    let vars: Vec<(&str, &Value)> = payload
                        .as_object()
                        .map(|o| o.iter().map(|(k, v)| (k.as_str(), v)).collect())
                        .unwrap_or_default();
                    if crate::cel::eval_bool(filter.trim().trim_start_matches("CEL:").trim(), &vars)
                        != Ok(true)
                    {
                        self.log.info(
                            "webhook.filtered",
                            json!({"workflow": workflow, "node": node}),
                        );
                        let _ = reply.send(WebhookReply::ok(
                            202,
                            "Accepted",
                            json!({"status": "filtered", "workflow": workflow}),
                        ));
                        return;
                    }
                }
                // Declarative webhook→signal: `signal: "resolved/{{ body.alert_id }}"`
                // on the start node fires the named signal with the webhook
                // payload — the hook→workflow.signal→finish boilerplate
                // collapses into one field. The run still fires (it is the
                // audit trail); a workflow that exists only to relay is just
                // the start plus a finish.
                if let Some(tpl) = spec.get("signal").and_then(Value::as_str) {
                    let data: crate::engine::template::Data = payload
                        .as_object()
                        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default();
                    match crate::engine::template::render_str(tpl, &data) {
                        Ok(Value::String(name)) if !name.is_empty() => {
                            let resumed = self.deliver_signal(
                                &name,
                                payload.clone(),
                                None,
                                None,
                                &super::waits::SignalSender::Runtime,
                            );
                            self.log.info(
                                "webhook.signal",
                                json!({"workflow": workflow, "node": node,
                                       "signal": name, "resumed": resumed}),
                            );
                        }
                        Ok(other) => self.log.warn(
                            "webhook.signal.invalid",
                            json!({"workflow": workflow, "node": node,
                                   "err": format!("signal template must render to a string, got {other}")}),
                        ),
                        Err(e) => self.log.warn(
                            "webhook.signal.invalid",
                            json!({"workflow": workflow, "node": node, "err": e}),
                        ),
                    }
                }
                // `into: {stream, subject}` — APPEND the request instead of
                // firing a run (RFC 0035 §5). This is what gives a webhook
                // replay-after-downtime: a run fired at request time is lost
                // if nothing was ready for it, while an appended event waits
                // on a durable stream for whatever consumes it, including a
                // consumer that does not exist yet. Verification, filtering
                // and rate limiting have all already happened above — the
                // append is the last step, not a bypass of the gate.
                if let Some(into) = spec.get("into") {
                    let stream = into.get("stream").and_then(Value::as_str).unwrap_or("");
                    let subject = into.get("subject").and_then(Value::as_str).unwrap_or("");
                    // The idempotency key doubles as the event id, so a
                    // retried delivery appends under the same id and the
                    // consumer's dedup ring drops the copy.
                    let id = idem_key
                        .clone()
                        .unwrap_or_else(|| crate::state::ulid::new().to_string());
                    let correlation = idem_key.as_deref();
                    // `_acked`: the `202` below tells the sender the event is
                    // kept, so the stream head is saved before it is sent.
                    let (status, body) = match self.append_event_acked(
                        stream,
                        subject,
                        correlation,
                        payload,
                        &id,
                        &workflow,
                    ) {
                        Ok(seq) => {
                            self.log.info(
                                "webhook.into",
                                json!({"workflow": workflow, "node": node,
                                           "stream": stream, "subject": subject, "seq": seq}),
                            );
                            self.mark_delivered(marker.as_deref(), &workflow, &node);
                            (
                                202,
                                json!({"status": "appended", "stream": stream, "seq": seq}),
                            )
                        }
                        // A refused append (an undeclared stream, or
                        // pressure) must be visible to the CALLER, not
                        // just the log: a webhook that answers 202 while
                        // dropping the event is the silent-loss shape this
                        // whole feature exists to remove.
                        Err(e) => {
                            self.log.warn(
                                "webhook.into.refused",
                                json!({"workflow": workflow, "node": node,
                                           "stream": stream, "err": e}),
                            );
                            (503, json!({"error": "stream append refused"}))
                        }
                    };
                    let reason = if status == 202 {
                        "Accepted"
                    } else {
                        "Service Unavailable"
                    };
                    let _ = reply.send(WebhookReply::ok(status, reason, body));
                    return;
                }
                if respond_sync {
                    // Hold the response: fire with a known run id, and answer at
                    // `on_run_terminal` with the run's result.
                    let run_id = format!("{workflow}-{}", crate::state::ulid::new());
                    self.webhook_sync.insert(run_id.clone(), reply);
                    let admitted = self.fire_start_run(
                        &workflow,
                        &node,
                        &spec,
                        payload,
                        "webhook",
                        Some(&run_id),
                    );
                    if admitted == Admission::Accepted {
                        self.mark_delivered(marker.as_deref(), &workflow, &node);
                    } else if let Some(reply) = self.webhook_sync.remove(&run_id) {
                        // No run will reach a terminal status to answer it.
                        let _ = reply.send(not_fired(admitted));
                    }
                } else {
                    let admitted = self.fire_start(&workflow, &node, &spec, payload, "webhook");
                    // A `202` for a firing that was refused would tell the sender
                    // to stop retrying an event nothing kept.
                    let answer = if admitted == Admission::Accepted {
                        self.mark_delivered(marker.as_deref(), &workflow, &node);
                        WebhookReply::ok(
                            202,
                            "Accepted",
                            json!({"status": "accepted", "workflow": workflow}),
                        )
                    } else {
                        not_fired(admitted)
                    };
                    let _ = reply.send(answer);
                }
            }
            None => {
                let _ = reply.send(WebhookReply::ok(
                    404,
                    "Not Found",
                    json!({"error": "unknown webhook node"}),
                ));
            }
        }
    }

    /// Record that a delivery was kept, so a replay of its idempotency key is
    /// answered `duplicate`. Called in the same pass as the firing or append it
    /// records, and only after it succeeded.
    ///
    /// A marker that cannot be written does not undo the delivery, which
    /// happened: the sender still gets its `2xx`, and a replay is processed
    /// again — a second run, or a second copy of the event under the same id,
    /// which stream consumers deduplicate. The line says so.
    fn mark_delivered(&mut self, marker: Option<&str>, workflow: &str, node: &str) {
        let Some(id) = marker else {
            return;
        };
        if let Err(e) = self.durable.put(
            crate::state::Kind::Memory,
            id,
            json!({"at": now_ms()}),
            None,
        ) {
            self.log.warn(
                "webhook.idempotency.unrecorded",
                json!({"workflow": workflow, "node": node, "err": e.to_string()}),
            );
        }
    }

    /// The tick's share of marker expiry: at most once per
    /// [`IDEM_SWEEP_EVERY`] (or sooner while a pass is still working through a
    /// backlog), delete the markers whose [`IDEM_TTL_MS`] has passed.
    ///
    /// A marker past its TTL already answers nothing — the read checks the
    /// age — so this is about the store, which would otherwise keep one record
    /// per delivery for ever. A store that cannot `list` cannot be swept; its
    /// expired markers stay where they are, ignored, and are replaced by the
    /// next delivery that reuses their key.
    pub(crate) fn sweep_idem_markers(&mut self) {
        let now = std::time::Instant::now();
        if self.idem_sweep.next.is_some_and(|next| now < next) {
            return;
        }
        let cursor = self.idem_sweep.cursor.take();
        let wait = match sweep_markers(
            &self.durable,
            now_ms(),
            cursor.as_deref(),
            IDEM_SWEEP_BUDGET,
        ) {
            Ok(pass) => {
                if pass.removed > 0 {
                    self.log.info(
                        "webhook.idempotency.expired",
                        json!({"markers": pass.removed}),
                    );
                }
                self.idem_sweep.cursor = pass.resume_after;
                if self.idem_sweep.cursor.is_some() {
                    IDEM_SWEEP_BACKLOG
                } else {
                    IDEM_SWEEP_EVERY
                }
            }
            Err(crate::store::StoreError::Unsupported(_)) => IDEM_SWEEP_EVERY,
            Err(e) => {
                self.log.warn(
                    "webhook.idempotency.sweep.fail",
                    json!({"err": e.to_string()}),
                );
                IDEM_SWEEP_EVERY
            }
        };
        self.idem_sweep.next = Some(now + wait);
    }

    /// Answer a `respond: sync` webhook whose run has just reached a terminal
    /// status (called from `on_run_terminal`). No-op for other runs.
    pub(crate) fn webhook_sync_reply(&mut self, run_id: &str) {
        let Some(reply) = self.webhook_sync.remove(run_id) else {
            return;
        };
        let (status, body) = match self.runs.get(run_id) {
            Some(run) => {
                let ok = run.status.as_str() == "completed";
                (
                    if ok { 200 } else { 502 },
                    json!({"status": run.status.as_str(), "output": run.output, "error": run.error}),
                )
            }
            None => (410, json!({"status": "gone"})),
        };
        let reason = if status == 200 { "OK" } else { "Bad Gateway" };
        let _ = reply.send(WebhookReply::ok(status, reason, body));
    }

    /// A `wait: {on: webhook}` step: register a one-shot callback route and suspend
    /// the run on a signal the inbound callback will deliver. The callback URL is
    /// logged (`webhook.await.armed`); a fixed `webhook.path` lets the workflow
    /// author hand a known URL to the external service before waiting.
    pub(crate) fn webhook_wait(
        &mut self,
        run_id: &str,
        step_id: &str,
        spec: &Map<String, Value>,
        timeout_ms: Option<u64>,
    ) {
        use crate::engine::run::StepStatus;
        let Some(base) = self.settings.webhooks.listen.clone() else {
            self.finish_step_pub(
                run_id,
                step_id,
                StepStatus::Failed,
                None,
                Some("wait webhook: webhooks.listen is not set".into()),
                0,
            );
            return;
        };
        let token = crate::state::ulid::new();
        let wcfg = spec.get("webhook").and_then(Value::as_object);
        let path = wcfg
            .and_then(|w| w.get("path"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{RESERVED_HOOK_PREFIX}cb/{token}"));
        let env = |k: &str| std::env::var(k).ok();
        let verify = match build_verify(
            wcfg.and_then(|w| w.get("auth")),
            self.settings.webhooks.default_auth.as_ref(),
            &env,
        ) {
            Ok(v) => v,
            Err(e) => {
                self.finish_step_pub(
                    run_id,
                    step_id,
                    StepStatus::Failed,
                    None,
                    Some(format!("wait webhook: {e}")),
                    0,
                );
                return;
            }
        };
        let expires_ms = now_ms() + timeout_ms.unwrap_or(3_600_000);
        self.webhook_callbacks.lock().unwrap().insert(
            path.clone(),
            Callback {
                signal: token.clone(),
                verify,
                expires_ms,
            },
        );
        let url = format!("{}{}", base.trim_end_matches('/'), path);
        // Emit the callback URL to a run blackboard var (so a concurrent step can
        // hand it to an external service). A fixed `webhook.path` is the reliable
        // pattern; `emit_url_to` covers the dynamic-URL case.
        if let Some(var) = wcfg
            .and_then(|w| w.get("emit_url_to"))
            .and_then(Value::as_str)
            && let Some(run) = self.runs.get_mut(run_id)
        {
            run.vars.insert(var.to_string(), json!(url.clone()));
        }
        self.log.info(
            "webhook.await.armed",
            json!({"run": run_id, "step": step_id, "path": path, "url": url}),
        );
        self.suspend_wait(
            run_id,
            step_id,
            super::waits::wait_record("signal", json!({"signal": token}), timeout_ms),
        );
    }
}

#[cfg(test)]
mod idempotency_tests {
    use super::*;
    use crate::context::memory::Memory;
    use crate::state::Policy;
    use crate::store::memory::MemoryStore;

    fn durable() -> Durable {
        Durable::new(
            Arc::new(MemoryStore::new()),
            "agentd",
            "i",
            Policy::default(),
            None,
        )
    }

    /// The marker namespace is one the memory surface refuses: a model can
    /// neither forge a marker (a real delivery answered `duplicate`) nor
    /// remove one (a replay fired twice).
    #[test]
    fn memory_set_refuses_a_webhook_marker_key() {
        let d = durable();
        let id = idem_marker_id("on-hook", "h", "evt-1");
        let mut m = Memory::new(1024, 10);
        let e = m
            .set(&d, &id, json!({"at": now_ms()}), None, None)
            .unwrap_err();
        assert!(e.contains("reserved"), "{e}");
        assert!(m.delete(&d, &id).is_err(), "nor can it delete one");
        assert!(m.get(&d, &id).is_err(), "nor read one");
        assert!(
            d.get(Kind::Memory, &id).unwrap().is_none(),
            "nothing was written"
        );
    }

    #[test]
    fn a_marker_answers_replays_for_its_ttl_and_then_nothing() {
        let at = 1_000_000;
        let m = json!({"at": at});
        assert!(marker_live(&m, at));
        assert!(marker_live(&m, at + IDEM_TTL_MS - 1));
        assert!(!marker_live(&m, at + IDEM_TTL_MS), "expired at the TTL");
        assert!(
            !marker_live(&json!({"seen": true}), at),
            "no time, no evidence"
        );
        // The lookup a delivery makes applies the same rule, whether or not
        // the sweep has removed the marker yet.
        let d = durable();
        let id = idem_marker_id("w", "h", "k");
        assert!(!is_replay(&d, &id, at), "no marker, no replay");
        d.put(Kind::Memory, &id, m, None).unwrap();
        assert!(is_replay(&d, &id, at + 1));
        assert!(!is_replay(&d, &id, at + IDEM_TTL_MS), "an expired marker");
    }

    /// The sweep deletes what has expired and nothing else — not a live
    /// marker, not another `_` system key, not a user key — and a pass that
    /// runs out of budget resumes where it stopped.
    #[test]
    fn markers_expire_and_the_sweep_removes_only_expired_markers() {
        let d = durable();
        let now = 10 * IDEM_TTL_MS;
        let old = now - IDEM_TTL_MS;
        let put = |id: &str, at: u64| {
            d.put(Kind::Memory, id, json!({"at": at}), None).unwrap();
        };
        let expired_a = idem_marker_id("w", "h", "a");
        let expired_b = idem_marker_id("w", "h", "b");
        let live = idem_marker_id("w", "h", "c");
        put(&expired_a, old);
        put(&expired_b, old - 1);
        put(&live, now - 1);
        put("_workflows/w", old);
        put("notes", old);

        // One marker per pass: the cursor carries the sweep across passes.
        let mut after: Option<String> = None;
        let mut removed = 0;
        for _ in 0..4 {
            let pass = sweep_markers(&d, now, after.as_deref(), 1).unwrap();
            removed += pass.removed;
            after = pass.resume_after;
            if after.is_none() {
                break;
            }
        }
        assert_eq!(after, None, "the sweep reached the end");
        assert_eq!(removed, 2);
        assert!(d.get(Kind::Memory, &expired_a).unwrap().is_none());
        assert!(d.get(Kind::Memory, &expired_b).unwrap().is_none());
        assert!(d.get(Kind::Memory, &live).unwrap().is_some(), "live stays");
        assert!(d.get(Kind::Memory, "_workflows/w").unwrap().is_some());
        assert!(d.get(Kind::Memory, "notes").unwrap().is_some());
    }
}

#[cfg(test)]
mod rate_tests {
    use crate::supervisor::tree::parse_rate;

    #[test]
    fn rate_strings_parse_and_bad_ones_say_why() {
        assert_eq!(parse_rate("20/1s"), Ok((20, 1.0)));
        assert_eq!(parse_rate("8/2s"), Ok((8, 2.0)));
        assert_eq!(parse_rate(" 5 / 0.5s "), Ok((5, 0.5)));
        assert_eq!(parse_rate("3/10sec"), Ok((3, 10.0)));
        assert_eq!(parse_rate("3/10"), Ok((3, 10.0)));
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("0/1s").is_err());
        assert!(parse_rate("5/0s").is_err());
        assert!(parse_rate("5/-1s").is_err());
        assert!(parse_rate("x/1s").is_err());
    }
}
