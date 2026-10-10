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
use crate::runtime::streams::AppendError;
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

/// How often the tick looks for markers to sweep, and how soon it comes back
/// while a sweep has more to delete than one pass may.
const IDEM_SWEEP_EVERY: Duration = Duration::from_secs(600);
const IDEM_SWEEP_BACKLOG: Duration = Duration::from_secs(1);

/// Markers one sweep pass deletes. Each delete is a store call on the
/// single-writer loop, so a pass is bounded and a large bucket is worked
/// through across passes.
const IDEM_SWEEP_BUDGET: usize = 512;

/// The record that says which marker bucket is the oldest that may still
/// hold markers. Under the marker prefix, so `memory.*` refuses it too.
const IDEM_FLOOR: &str = "_wh_idem/floor";

/// Where the marker sweep is: when it runs next (`None`: now), the oldest
/// bucket that may hold markers (read from the store once a life, `None`
/// until then), and the markers of that bucket still to delete.
#[derive(Debug, Default)]
pub(crate) struct IdemSweep {
    next: Option<std::time::Instant>,
    floor: Option<Option<u64>>,
    queue: Vec<String>,
    /// Whether the store can `list` (asked once a life). One that cannot
    /// keeps an index of each bucket's markers, which the sweep reads in
    /// place of a listing.
    listable: Option<bool>,
    /// The index page markers are appended to now: `(bucket, page, ids)`.
    page: Option<(u64, u64, Vec<String>)>,
    /// Sweeping by index: the next page of the floor bucket to read.
    index_next: u64,
}

/// Marker ids one index page holds.
const INDEX_PAGE: usize = 128;

/// The index of bucket `b`'s markers, for a store that cannot `list`: pages
/// `p0`, `p1`, … and a `head` naming the last (absent: page 0). Beside the
/// buckets, never inside one, so no route's markers can collide with it.
fn index_page_id(bucket: u64, page: u64) -> String {
    format!("{IDEM_PREFIX}x{bucket}/p{page}")
}

fn index_head_id(bucket: u64) -> String {
    format!("{IDEM_PREFIX}x{bucket}/head")
}

/// Whether the store can `list`: asked once, with a listing of one key.
fn listable(d: &Durable, sweep: &mut IdemSweep) -> Result<bool, crate::store::StoreError> {
    if let Some(l) = sweep.listable {
        return Ok(l);
    }
    let l = match d.list_prefix(Kind::Memory, IDEM_FLOOR) {
        Ok(_) => true,
        Err(crate::store::StoreError::Unsupported(_)) => false,
        Err(e) => return Err(e),
    };
    sweep.listable = Some(l);
    Ok(l)
}

/// Add `id` to its bucket's index — before the marker is written, so the
/// index never misses a marker that exists.
fn index_marker(
    d: &Durable,
    sweep: &mut IdemSweep,
    bucket: u64,
    id: &str,
) -> Result<(), crate::store::StoreError> {
    let (page, ids) = match sweep.page.take() {
        Some((b, page, ids)) if b == bucket => (page, ids),
        // The first marker of this bucket in this life: carry on from the
        // page its head names.
        _ => {
            let page = d
                .get(Kind::Memory, &index_head_id(bucket))?
                .and_then(|e| e.state.get("page").and_then(Value::as_u64))
                .unwrap_or(0);
            let ids = d
                .get(Kind::Memory, &index_page_id(bucket, page))?
                .map(|e| page_ids(&e.state))
                .unwrap_or_default();
            (page, ids)
        }
    };
    let (page, mut ids) = if ids.len() >= INDEX_PAGE {
        d.put(
            Kind::Memory,
            &index_head_id(bucket),
            json!({"page": page + 1}),
            None,
        )?;
        (page + 1, Vec::new())
    } else {
        (page, ids)
    };
    ids.push(id.to_string());
    d.put(
        Kind::Memory,
        &index_page_id(bucket, page),
        json!({"ids": ids}),
        None,
    )?;
    sweep.page = Some((bucket, page, ids));
    Ok(())
}

fn page_ids(state: &Value) -> Vec<String> {
    state
        .get("ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

/// The bucket a marker written at `at` lives in: [`IDEM_TTL_MS`]-wide,
/// numbered from the epoch.
///
/// The bucket is what makes expiry cheap to find. A marker in bucket `b` was
/// written before `(b + 1) × TTL`, so it has expired by `(b + 2) × TTL`: every
/// bucket older than the one before the current one holds only expired
/// markers, and the sweep deletes it whole without reading one. Live markers
/// are never listed or read by it, however many a busy route keeps.
fn idem_bucket(at: u64) -> u64 {
    at / IDEM_TTL_MS
}

/// The marker id for a delivery seen at `at`: per bucket and route, keyed by
/// a hash of the sender's idempotency key so the key's bytes never become
/// part of a store key.
pub(crate) fn idem_marker_id(workflow: &str, node: &str, key: &str, at: u64) -> String {
    format!(
        "{IDEM_PREFIX}b{}/{workflow}/{node}/{}",
        idem_bucket(at),
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

/// Whether a delivery of `key` to `workflow`/`node` is a replay at `now`: a
/// live marker in the current bucket or the one before it — no older bucket
/// can hold one. An expired marker that the sweep has not reached yet
/// answers nothing, so expiry does not depend on the sweep.
fn is_replay(d: &Durable, workflow: &str, node: &str, key: &str, now: u64) -> bool {
    [now, now.saturating_sub(IDEM_TTL_MS)].iter().any(|&at| {
        matches!(
            d.get(Kind::Memory, &idem_marker_id(workflow, node, key, at)),
            Ok(Some(env)) if marker_live(&env.state, now)
        )
    })
}

/// The oldest bucket that may hold markers, as recorded; `None` when no
/// marker was ever written.
fn idem_floor(d: &Durable) -> Result<Option<u64>, crate::store::StoreError> {
    Ok(d.get(Kind::Memory, IDEM_FLOOR)?
        .and_then(|env| env.state.get("bucket").and_then(Value::as_u64)))
}

/// Write the marker for a delivery of `key` kept at `now`.
///
/// The recorded floor is made to cover the marker's bucket first: the sweep
/// starts at the floor, so a marker below it would never be deleted. The
/// floor is read once a life and written when it moves down — once ever (the
/// first marker), unless the clock goes back.
fn record_marker(
    d: &Durable,
    sweep: &mut IdemSweep,
    workflow: &str,
    node: &str,
    key: &str,
    now: u64,
) -> Result<(), crate::store::StoreError> {
    let bucket = idem_bucket(now);
    let floor = match sweep.floor {
        Some(f) => f,
        None => *sweep.floor.insert(idem_floor(d)?),
    };
    if floor.is_none_or(|f| bucket < f) {
        d.put(Kind::Memory, IDEM_FLOOR, json!({"bucket": bucket}), None)?;
        sweep.floor = Some(Some(bucket));
    }
    let id = idem_marker_id(workflow, node, key, now);
    if !listable(d, sweep)? {
        index_marker(d, sweep, bucket, &id)?;
    }
    d.put(Kind::Memory, &id, json!({"at": now}), None)?;
    Ok(())
}

/// One sweep pass's result.
#[derive(Debug, PartialEq)]
struct SweepPass {
    removed: usize,
    /// Whether there is more to delete now — a later pass should come soon.
    more: bool,
}

/// One bounded sweep pass: delete up to `budget` markers from the oldest
/// bucket that has expired whole, listing that bucket only when the last
/// listing of it is used up, and moving the floor past a bucket that lists
/// empty.
fn sweep_pass(
    d: &Durable,
    sweep: &mut IdemSweep,
    now: u64,
    budget: usize,
) -> Result<SweepPass, crate::store::StoreError> {
    let floor = match sweep.floor {
        Some(f) => f,
        None => *sweep.floor.insert(idem_floor(d)?),
    };
    // Nothing was ever marked, or every expired bucket is gone.
    let Some(floor) = floor.filter(|&f| f + 1 < idem_bucket(now)) else {
        return Ok(SweepPass {
            removed: 0,
            more: false,
        });
    };
    if sweep.queue.is_empty() && !listable(d, sweep)? {
        // By index: the floor bucket's next page — its ids, and the page —
        // until the pages the head names are done; then the head goes and
        // the floor moves on.
        let head = d
            .get(Kind::Memory, &index_head_id(floor))?
            .and_then(|e| e.state.get("page").and_then(Value::as_u64))
            .unwrap_or(0);
        if sweep.index_next > head {
            d.delete(Kind::Memory, &index_head_id(floor))?;
            d.put(Kind::Memory, IDEM_FLOOR, json!({"bucket": floor + 1}), None)?;
            sweep.floor = Some(Some(floor + 1));
            sweep.index_next = 0;
            return Ok(SweepPass {
                removed: 0,
                more: floor + 2 < idem_bucket(now),
            });
        }
        let page = index_page_id(floor, sweep.index_next);
        if let Some(env) = d.get(Kind::Memory, &page)? {
            sweep.queue = page_ids(&env.state);
            sweep.queue.push(page);
        }
        sweep.index_next += 1;
        if sweep.queue.is_empty() {
            return Ok(SweepPass {
                removed: 0,
                more: true,
            });
        }
    } else if sweep.queue.is_empty() {
        sweep.queue = d
            .list_prefix(Kind::Memory, &format!("{IDEM_PREFIX}b{floor}/"))?
            .iter()
            .filter_map(|ks| crate::store::parse_key(d.prefix(), d.instance(), &ks.key))
            .map(|(_, id)| id.to_string())
            .collect();
        if sweep.queue.is_empty() {
            d.put(Kind::Memory, IDEM_FLOOR, json!({"bucket": floor + 1}), None)?;
            sweep.floor = Some(Some(floor + 1));
            return Ok(SweepPass {
                removed: 0,
                more: floor + 2 < idem_bucket(now),
            });
        }
    }
    let take = sweep.queue.len().min(budget);
    let mut removed = 0;
    for id in sweep.queue.drain(..take) {
        // An index page is not a marker: it is deleted, not counted.
        let marker = !id.starts_with(&format!("{IDEM_PREFIX}x"));
        if d.delete(Kind::Memory, &id).is_ok() && marker {
            removed += 1;
        }
    }
    // A bucket is listed again once its queue is used up, so a delete that
    // failed is retried, and the floor moves only past a bucket seen empty.
    Ok(SweepPass {
        removed,
        more: true,
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
        Admission::TooLarge => "too_large",
    };
    if admitted == Admission::InputsInvalid {
        WebhookReply::ok(
            422,
            "Unprocessable Entity",
            json!({"error": "the delivery does not render the start node's inputs", "cause": cause}),
        )
    } else if admitted == Admission::TooLarge {
        too_large(cause)
    } else {
        WebhookReply::ok(
            503,
            "Service Unavailable",
            json!({"error": "the run was not started", "cause": cause}),
        )
    }
}

/// The answer to a delivery too large for the store to keep: `413`, because
/// a retry of the same delivery is refused the same way.
fn too_large(cause: &str) -> WebhookReply {
    WebhookReply::ok(
        413,
        "Payload Too Large",
        json!({"error": "the delivery is larger than the store keeps (store.max_value_bytes)",
               "cause": cause}),
    )
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
    /// The MCP Events webhook bindings' routes (shared with the loop).
    #[cfg(feature = "mcp-events")]
    mcp_hooks: crate::runtime::mcp_events::hook::Routes,
    events_tx: Sender<Event>,
    timeout: Duration,
    log: Logger,
}

impl ::mcp::http_server::RawHandler for WebhookHandler {
    fn handle(&self, req: &::mcp::http_server::RawRequest) -> ::mcp::http_server::RawResponse {
        use ::mcp::http_server::RawResponse as R;
        let path = req.path().to_string();
        // An MCP Events webhook binding's route: its own verification, no
        // listener default auth — a server cannot send a bearer [SEP].
        #[cfg(feature = "mcp-events")]
        if path.starts_with(crate::runtime::mcp_events::hook::PATH_PREFIX) {
            return crate::runtime::mcp_events::hook::serve(
                &self.mcp_hooks,
                req,
                &self.events_tx,
                self.timeout,
                &self.log,
            );
        }
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

    /// An MCP Events delivery is at most 256 KiB [SEP "Delivery profile"],
    /// refused `413` before it is read.
    fn body_limit(&self, _method: &str, path: &str) -> usize {
        #[cfg(feature = "mcp-events")]
        if path.starts_with(crate::runtime::mcp_events::hook::PATH_PREFIX) {
            return crate::runtime::mcp_events::hook::MAX_BODY;
        }
        let _ = path;
        ::mcp::http_server::MAX_BODY
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
    #[cfg(feature = "mcp-events")] mcp_hooks: crate::runtime::mcp_events::hook::Routes,
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
        #[cfg(feature = "mcp-events")]
        mcp_hooks,
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
            // What the caller posted, the waiting run's own definition
            // carries already (`wait {on: webhook}`, `config::taint`).
            let delivered = self.deliver_signal(
                &signal,
                payload,
                None,
                None,
                &super::waits::SignalSender::Runtime,
                &Default::default(),
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
        let marker = idem_key.as_deref();
        if let Some(key) = marker
            && is_replay(&self.durable, &workflow, &node, key, now_ms())
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
                // the start plus a finish. The signal goes out only once the
                // delivery is kept (`deliver_kept`): a refused one is retried
                // by its sender, and a signal sent on each attempt would wake
                // a waiter once per retry.
                let signal = spec
                    .get("signal")
                    .and_then(Value::as_str)
                    .map(|tpl| (tpl.to_string(), payload.clone()));
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
                    let answer = match self.append_event_acked(
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
                            self.deliver_kept(marker, &workflow, &node, signal);
                            WebhookReply::ok(
                                202,
                                "Accepted",
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
                                           "stream": stream, "err": e.to_string()}),
                            );
                            match e {
                                AppendError::TooLarge(_) => too_large("too_large"),
                                AppendError::Refused(_) => WebhookReply::ok(
                                    503,
                                    "Service Unavailable",
                                    json!({"error": "stream append refused"}),
                                ),
                            }
                        }
                    };
                    let _ = reply.send(answer);
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
                        self.deliver_kept(marker, &workflow, &node, signal);
                    } else if let Some(reply) = self.webhook_sync.remove(&run_id) {
                        // No run will reach a terminal status to answer it.
                        let _ = reply.send(not_fired(admitted));
                    }
                } else {
                    let admitted = self.fire_start(&workflow, &node, &spec, payload, "webhook");
                    // A `202` for a firing that was refused would tell the sender
                    // to stop retrying an event nothing kept.
                    let answer = if admitted == Admission::Accepted {
                        self.deliver_kept(marker, &workflow, &node, signal);
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

    /// What follows a delivery that was kept — its run admitted or its event
    /// appended, in this same pass: the route's `signal:` goes out, and the
    /// idempotency marker is written so a replay is answered `duplicate`.
    /// Neither happens for a delivery that was refused, because its sender
    /// retries it.
    fn deliver_kept(
        &mut self,
        key: Option<&str>,
        workflow: &str,
        node: &str,
        signal: Option<(String, Value)>,
    ) {
        if let Some((tpl, payload)) = signal {
            self.webhook_signal(&tpl, payload, workflow, node);
        }
        self.mark_delivered(key, workflow, node);
    }

    /// Fire a route's `signal:` template with the delivery's payload.
    fn webhook_signal(&mut self, tpl: &str, payload: Value, workflow: &str, node: &str) {
        let data: crate::engine::template::Data = payload
            .as_object()
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        match crate::engine::template::render_str(tpl, &data) {
            Ok(Value::String(name)) if !name.is_empty() => {
                // A relayed signal's receivers carry the route already
                // (`config::taint`), from their own definitions.
                let resumed = self.deliver_signal(
                    &name,
                    payload,
                    None,
                    None,
                    &super::waits::SignalSender::Runtime,
                    &Default::default(),
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

    /// Record that a delivery of `key` was kept, so a replay of it is answered
    /// `duplicate`. Called in the same pass as the firing or append it
    /// records, and only after it succeeded.
    ///
    /// A marker that cannot be written does not undo the delivery, which
    /// happened: the sender still gets its `2xx`, and a replay is processed
    /// again — a second run, or a second copy of the event under the same id,
    /// which stream consumers deduplicate. The line says so.
    fn mark_delivered(&mut self, key: Option<&str>, workflow: &str, node: &str) {
        let Some(key) = key else {
            return;
        };
        if let Err(e) = record_marker(
            &self.durable,
            &mut self.idem_sweep,
            workflow,
            node,
            key,
            now_ms(),
        ) {
            self.log.warn(
                "webhook.idempotency.unrecorded",
                json!({"workflow": workflow, "node": node, "err": e.to_string()}),
            );
        }
    }

    /// The tick's share of marker expiry: at most once per
    /// [`IDEM_SWEEP_EVERY`] (or sooner while a pass is still working through a
    /// backlog), delete the markers of buckets that have expired whole.
    ///
    /// A marker past its TTL already answers nothing — the read checks the
    /// age — so this is about the store, which would otherwise keep one record
    /// per delivery for ever. A store that cannot `list` is swept through the
    /// index its markers were recorded in.
    pub(crate) fn sweep_idem_markers(&mut self) {
        let now = std::time::Instant::now();
        if self.idem_sweep.next.is_some_and(|next| now < next) {
            return;
        }
        let wait = match sweep_pass(
            &self.durable,
            &mut self.idem_sweep,
            now_ms(),
            IDEM_SWEEP_BUDGET,
        ) {
            Ok(pass) => {
                if pass.removed > 0 {
                    self.log.info(
                        "webhook.idempotency.expired",
                        json!({"markers": pass.removed}),
                    );
                }
                if pass.more {
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
        // The URL handed out is the one callers reach: `webhooks.url` behind a
        // proxy, the bound `listen` otherwise.
        let Some(base) = self
            .settings
            .webhooks
            .url
            .clone()
            .or_else(|| self.settings.webhooks.listen.clone())
        else {
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
        // The route and its auth — as written, references and all, never a
        // resolved secret — go in the durable wait record, so a restart can
        // serve the callback again (`restore_webhook_callbacks`).
        let route = json!({"path": path, "auth": wcfg.and_then(|w| w.get("auth")).cloned()});
        self.suspend_wait(
            run_id,
            step_id,
            super::waits::wait_record(
                "signal",
                json!({"signal": token, "webhook": route}),
                timeout_ms,
            ),
        );
    }

    /// At boot: serve again the callback of every `wait {on: webhook}` still
    /// suspended, from its durable record — the callback map lives in memory,
    /// and without this a restarted wait could only time out. A route whose
    /// auth no longer resolves fails its step, saying why.
    pub(crate) fn restore_webhook_callbacks(&mut self) {
        use crate::engine::run::StepStatus;
        let mut routes: Vec<(String, String, String, String, Option<Value>, u64)> = Vec::new();
        for (rid, run) in &self.runs {
            if run.status.is_terminal() {
                continue;
            }
            for (sid, st) in &run.steps {
                let Some(w) = st
                    .wait
                    .as_ref()
                    .filter(|_| st.status == StepStatus::Suspended)
                else {
                    continue;
                };
                let (Some(signal), Some(path)) =
                    (w["signal"].as_str(), w["webhook"]["path"].as_str())
                else {
                    continue;
                };
                let expires = w["deadline_ms"]
                    .as_u64()
                    .unwrap_or_else(|| w["since_ms"].as_u64().unwrap_or_else(now_ms) + 3_600_000);
                let auth = w["webhook"].get("auth").filter(|a| !a.is_null()).cloned();
                routes.push((
                    rid.clone(),
                    sid.clone(),
                    signal.to_string(),
                    path.to_string(),
                    auth,
                    expires,
                ));
            }
        }
        let env = |k: &str| std::env::var(k).ok();
        for (run, step, signal, path, auth, expires_ms) in routes {
            match build_verify(
                auth.as_ref(),
                self.settings.webhooks.default_auth.as_ref(),
                &env,
            ) {
                Ok(verify) => {
                    self.webhook_callbacks.lock().unwrap().insert(
                        path.clone(),
                        Callback {
                            signal,
                            verify,
                            expires_ms,
                        },
                    );
                    self.log.info(
                        "webhook.await.restored",
                        json!({"run": run, "step": step, "path": path}),
                    );
                }
                Err(e) => self.finish_step_pub(
                    &run,
                    &step,
                    StepStatus::Failed,
                    None,
                    Some(format!("wait webhook: {e}")),
                    0,
                ),
            }
        }
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
        let mut m = Memory::new(1024, 10);
        for id in [
            idem_marker_id("on-hook", "h", "evt-1", now_ms()),
            IDEM_FLOOR.to_string(),
        ] {
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
    }

    #[test]
    fn a_marker_answers_replays_for_its_ttl_and_then_nothing() {
        // Late in a bucket, so the TTL runs into the next one: the lookup
        // has to find it there.
        let at = 5 * IDEM_TTL_MS + IDEM_TTL_MS - 10;
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
        assert!(!is_replay(&d, "w", "h", "k", at), "no marker, no replay");
        d.put(Kind::Memory, &idem_marker_id("w", "h", "k", at), m, None)
            .unwrap();
        assert!(is_replay(&d, "w", "h", "k", at + 1));
        assert!(
            is_replay(&d, "w", "h", "k", at + IDEM_TTL_MS - 1),
            "from the next bucket"
        );
        assert!(
            !is_replay(&d, "w", "h", "k", at + IDEM_TTL_MS),
            "an expired marker"
        );
        assert!(!is_replay(&d, "w", "h", "other", at + 1), "another key");
        assert!(!is_replay(&d, "w", "h2", "k", at + 1), "another route");
    }

    /// A store that counts what the sweep asks of it.
    struct Counting {
        inner: MemoryStore,
        ops: std::sync::Mutex<Vec<(&'static str, String)>>,
    }
    impl crate::store::Store for Counting {
        fn put(
            &self,
            key: &str,
            seq: u64,
            env: &Value,
        ) -> Result<crate::store::PutOutcome, crate::store::StoreError> {
            self.inner.put(key, seq, env)
        }
        fn get(
            &self,
            key: &str,
            seq: Option<u64>,
        ) -> Result<Option<Value>, crate::store::StoreError> {
            self.ops.lock().unwrap().push(("get", key.to_string()));
            self.inner.get(key, seq)
        }
        fn list(
            &self,
            prefix: &str,
        ) -> Result<Vec<crate::store::KeySeq>, crate::store::StoreError> {
            self.ops.lock().unwrap().push(("list", prefix.to_string()));
            self.inner.list(prefix)
        }
        fn delete(&self, key: &str) -> Result<(), crate::store::StoreError> {
            self.ops.lock().unwrap().push(("delete", key.to_string()));
            self.inner.delete(key)
        }
        fn kind(&self) -> &'static str {
            "memory"
        }
    }

    /// A store with no `list` (an MCP or HTTP store without one): every
    /// write goes to `inner`, listing is refused.
    struct NoList {
        inner: MemoryStore,
    }
    impl crate::store::Store for NoList {
        fn put(
            &self,
            key: &str,
            seq: u64,
            env: &Value,
        ) -> Result<crate::store::PutOutcome, crate::store::StoreError> {
            self.inner.put(key, seq, env)
        }
        fn get(
            &self,
            key: &str,
            seq: Option<u64>,
        ) -> Result<Option<Value>, crate::store::StoreError> {
            self.inner.get(key, seq)
        }
        fn list(
            &self,
            _prefix: &str,
        ) -> Result<Vec<crate::store::KeySeq>, crate::store::StoreError> {
            Err(crate::store::StoreError::Unsupported("list"))
        }
        fn delete(&self, key: &str) -> Result<(), crate::store::StoreError> {
            self.inner.delete(key)
        }
        fn kind(&self) -> &'static str {
            "memory"
        }
    }

    /// On a store that cannot `list`, the markers are swept through the index
    /// they were recorded in — more than a page of them in one bucket, across
    /// a restart of the writer — and the index goes with them.
    #[test]
    fn a_store_without_list_is_swept_through_the_marker_index() {
        let inner = MemoryStore::new();
        let store = Arc::new(NoList { inner });
        let d = Durable::new(store.clone(), "agentd", "i", Policy::default(), None);
        let now = 10 * IDEM_TTL_MS + 5;
        let old_at = 7 * IDEM_TTL_MS + 1;
        let mut old = Vec::new();
        // Two lives write into the same expired bucket: the second carries on
        // from the page the first left.
        for life in 0..2 {
            let mut writer = IdemSweep::default();
            for i in 0..(INDEX_PAGE + 3) {
                let key = format!("k{life}-{i}");
                record_marker(&d, &mut writer, "w", "h", &key, old_at + i as u64).unwrap();
                old.push(idem_marker_id("w", "h", &key, old_at + i as u64));
            }
        }
        let mut writer = IdemSweep::default();
        record_marker(&d, &mut writer, "w", "h", "live", now - 1).unwrap();
        let live = idem_marker_id("w", "h", "live", now - 1);
        assert!(
            d.get(Kind::Memory, &index_head_id(7)).unwrap().is_some(),
            "more than a page: a head names the last"
        );

        let mut sweep = IdemSweep::default();
        let mut removed = 0;
        for _ in 0..200 {
            let pass = sweep_pass(&d, &mut sweep, now, 64).unwrap();
            removed += pass.removed;
            if !pass.more {
                break;
            }
        }
        assert_eq!(removed, old.len());
        for gone in &old {
            assert!(d.get(Kind::Memory, gone).unwrap().is_none(), "{gone}");
        }
        assert!(d.get(Kind::Memory, &live).unwrap().is_some());
        for page in 0..3 {
            assert!(
                d.get(Kind::Memory, &index_page_id(7, page))
                    .unwrap()
                    .is_none(),
                "page {page} went with its markers"
            );
        }
        assert!(d.get(Kind::Memory, &index_head_id(7)).unwrap().is_none());
        assert_eq!(idem_floor(&d).unwrap(), Some(idem_bucket(now) - 1));
    }

    /// The sweep deletes what has expired and nothing else — not a live
    /// marker, not another `_` system key, not a user key — and the work it
    /// does is bounded by what it deletes: it lists only buckets that have
    /// expired whole, reads no marker, and deletes at most its budget a pass.
    #[test]
    fn markers_expire_and_the_sweep_touches_only_expired_buckets() {
        let store = Arc::new(Counting {
            inner: MemoryStore::new(),
            ops: Default::default(),
        });
        let d = Durable::new(store.clone(), "agentd", "i", Policy::default(), None);
        let now = 10 * IDEM_TTL_MS + 5;
        // Recorded the way a delivery records them, by a life of its own.
        let mut writer = IdemSweep::default();
        let mut mark = |key: &str, at: u64| {
            record_marker(&d, &mut writer, "w", "h", key, at).unwrap();
            idem_marker_id("w", "h", key, at)
        };
        // Two buckets that have expired whole, three markers between them;
        // one marker in the previous bucket that is still live, and one in
        // the current bucket.
        let old_a = mark("a", 7 * IDEM_TTL_MS + 1);
        let old_b = mark("b", 7 * IDEM_TTL_MS + 2);
        let old_c = mark("c", 8 * IDEM_TTL_MS + 1);
        let live_prev = mark("d", now - IDEM_TTL_MS + 1);
        let live_now = mark("e", now - 1);
        d.put(Kind::Memory, "_workflows/w", json!({"at": 1}), None)
            .unwrap();
        d.put(Kind::Memory, "notes", json!({"at": 1}), None)
            .unwrap();
        store.ops.lock().unwrap().clear();

        let mut sweep = IdemSweep::default();
        let mut removed = 0;
        let mut passes = 0;
        loop {
            let pass = sweep_pass(&d, &mut sweep, now, 1).unwrap();
            assert!(pass.removed <= 1, "the budget bounds a pass");
            removed += pass.removed;
            passes += 1;
            if !pass.more {
                break;
            }
            assert!(passes < 20, "the sweep ends");
        }
        let ops = store.ops.lock().unwrap().clone();
        assert_eq!(removed, 3);
        for gone in [&old_a, &old_b, &old_c] {
            assert!(d.get(Kind::Memory, gone).unwrap().is_none(), "{gone}");
        }
        for kept in [&live_prev, &live_now, "_workflows/w", "notes"] {
            assert!(d.get(Kind::Memory, kept).unwrap().is_some(), "{kept}");
        }
        assert_eq!(idem_floor(&d).unwrap(), Some(idem_bucket(now) - 1));

        let marker = |k: &str| k.contains(IDEM_PREFIX) && !k.ends_with("/floor");
        assert!(
            !d.list_prefix(Kind::Memory, &format!("{IDEM_PREFIX}x"))
                .unwrap()
                .iter()
                .any(|_| true),
            "a store that lists keeps no index"
        );
        assert!(
            !ops.iter().any(|(op, k)| *op == "get" && marker(k)),
            "no marker is read: {ops:?}"
        );
        let listed: Vec<&String> = ops
            .iter()
            .filter(|(op, _)| *op == "list")
            .map(|(_, k)| k)
            .collect();
        // Only buckets that expired whole — and, once a life, the one-key
        // listing that asks whether the store lists at all.
        assert!(
            listed.iter().all(|k| k.ends_with("/_wh_idem/b7/")
                || k.ends_with("/_wh_idem/b8/")
                || k.ends_with("/_wh_idem/floor")),
            "only buckets that expired whole are listed: {listed:?}"
        );
        assert_eq!(
            listed.iter().filter(|k| k.ends_with("/floor")).count(),
            1,
            "{listed:?}"
        );
        // Nothing more to do now: the next pass touches nothing but the
        // floor it already holds.
        store.ops.lock().unwrap().clear();
        let pass = sweep_pass(&d, &mut sweep, now, 1).unwrap();
        assert_eq!(
            pass,
            SweepPass {
                removed: 0,
                more: false
            }
        );
        assert!(store.ops.lock().unwrap().is_empty());
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
