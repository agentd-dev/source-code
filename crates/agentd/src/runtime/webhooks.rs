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
//!   3. **deduplicated** durably by its idempotency key (a replay returns the
//!      first outcome, never re-fires),
//!   4. **backpressured** per route (`parallelism` + `on_overflow`), then
//!   5. handed to the single-writer loop as an [`Event::Webhook`], which fires the
//!      run and replies (`respond: ack` → `202`).
//!
//! The listener rides the same `mcp::http_server` raw-HTTP + `net::tls` surface as
//! the A2A listener and never blocks the loop (one connection = one thread; the
//! reply arrives over a oneshot).
#![cfg(feature = "a2a")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::config::v2::{WebhookAuth, Webhooks};
use crate::obs::log::Logger;
use crate::runtime::events::Event;
use crate::state::now_ms;

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
    inflight: AtomicUsize,
    /// Per-route arrival rate (`rate: "<burst>/<per>s"`). `parallelism` bounds
    /// how many run at ONCE; this bounds how fast they ARRIVE — without it an
    /// inbound burst is written to the durable inbox as fast as the socket
    /// delivers it, which converts the burst straight into disk pressure.
    rate: Option<std::sync::Mutex<crate::supervisor::tree::TokenBucket>>,
    /// `Retry-After` for a rate refusal: roughly when a token will exist.
    retry_after_s: u32,
    /// The owning workflow declared `priority: low` — its admissions shed one
    /// pressure level earlier (at warn).
    low_priority: bool,
}

/// Decrement the route's in-flight counter on drop.
struct InflightGuard<'a>(&'a AtomicUsize);
impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct WebhookHandler {
    routes: HashMap<String, Route>,
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
        // A static `webhook` start-node route.
        if let Some(route) = self.routes.get(&path) {
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
            _guard = InflightGuard(&route.inflight);
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

/// Spawn the webhook listener from the configured `webhook` start nodes. Each
/// `nodes` entry is `(workflow, node, spec)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_webhook_listener(
    webhooks: &Webhooks,
    nodes: Vec<(String, String, Map<String, Value>, bool)>,
    callbacks: SharedCallbacks,
    events_tx: Sender<Event>,
    env: &dyn Fn(&str) -> Option<String>,
    write_timeout: Duration,
    pressure: std::sync::Arc<super::pressure::Pressure>,
    log: Logger,
) -> Result<(), String> {
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

    let mut routes = HashMap::new();
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
        let (rate, retry_after_s) = match spec.get("rate").and_then(Value::as_str) {
            None => (None, 30),
            Some(r) => {
                let (burst, per_s) = crate::supervisor::tree::parse_rate(r)
                    .map_err(|e| format!("webhook node '{workflow}/{node}': rate: {e}"))?;
                let retry = (per_s / burst.max(1) as f64).ceil().max(1.0) as u32;
                (
                    Some(std::sync::Mutex::new(
                        crate::supervisor::tree::TokenBucket::new(burst, burst as f64 / per_s),
                    )),
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
                inflight: AtomicUsize::new(0),
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

    let listener =
        ::mcp::http_server::bind_tcp(&bind).map_err(|e| format!("webhooks bind {bind}: {e}"))?;
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listen.to_string());
    let route_paths: Vec<String> = routes.keys().cloned().collect();
    let handler = Arc::new(WebhookHandler {
        routes,
        pressure,
        callbacks,
        events_tx,
        timeout: write_timeout,
        log: log.clone(),
    });
    ::mcp::http_server::spawn_accept_raw(listener, Arc::new(acceptor), handler, write_timeout)
        .map_err(|e| format!("webhooks accept: {e}"))?;
    log.info(
        "webhooks.listen",
        json!({"authority": listen, "bound": bound, "tls": tls_scheme, "routes": route_paths}),
    );
    Ok(())
}

// ---- the loop side: idempotency + fire the run ------------------------------

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
            let delivered = self.deliver_signal(&signal, payload, None, None);
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

        // Durable idempotency: a replay of the same key returns the first outcome
        // and never re-fires. The record survives a restart (Kind::Memory KV).
        if let Some(key) = &idem_key {
            let id = format!(
                "wh_idem/{workflow}/{node}/{}",
                crate::sha::sha256_hex(key.as_bytes())
            );
            if matches!(
                self.durable.get(crate::state::Kind::Memory, &id),
                Ok(Some(_))
            ) {
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
            let _ = self
                .durable
                .put(crate::state::Kind::Memory, &id, json!({"seen": true}), None);
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
                            let resumed = self.deliver_signal(&name, payload.clone(), None, None);
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
                if respond_sync {
                    // Hold the response: fire with a known run id, and answer at
                    // `on_run_terminal` with the run's result.
                    let run_id = format!("{workflow}-{}", crate::state::ulid::new());
                    self.webhook_sync.insert(run_id.clone(), reply);
                    self.fire_start_run(&workflow, &node, &spec, payload, "webhook", Some(&run_id));
                } else {
                    self.fire_start(&workflow, &node, &spec, payload, "webhook");
                    let _ = reply.send(WebhookReply::ok(
                        202,
                        "Accepted",
                        json!({"status": "accepted", "workflow": workflow}),
                    ));
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
            .unwrap_or_else(|| format!("/hooks/_cb/{token}"));
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
