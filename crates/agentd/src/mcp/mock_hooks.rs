// SPDX-License-Identifier: AGPL-3.0-only
//! The mock's MCP Events webhook delivery (SEP-3415 "Webhook-Based
//! Delivery"), the server side agentd's webhook bindings are tested against:
//! `events/subscribe` with the endpoint-verification handshake, signed
//! deliveries over HTTPS (trusting the CA the mock was started with), bounded
//! per-event retries after which an event is abandoned and the watermark moves
//! past it, `deliveryStatus` on a refresh, and the `gap` and `terminated`
//! envelopes. Every delivery and its answer is logged (`MOCK_HOOK_*`) for the
//! tests to read.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FORBIDDEN: i64 = -32024;
const NOT_FOUND: i64 = -32023;
const INVALID_PARAMS: i64 = -32602;
const CALLBACK_ENDPOINT_ERROR: i64 = -32027;

/// One webhook subscription.
struct Hook {
    id: String,
    url: String,
    topic: String,
    key: Vec<u8>,
    /// Unix ms the lease ends.
    expires_at: u64,
    /// The next occurrence (by the topic's sequence) to deliver.
    next: u64,
    /// Everything at or before it was acknowledged or abandoned.
    watermark: u64,
    attempt: u32,
    first_try: u64,
    retry_at: u64,
    active: bool,
    last_error: Option<&'static str>,
}

/// How the sender retries.
struct Policy {
    attempts: u32,
    window_ms: u64,
    ttl_cap_ms: u64,
    /// The next unsubscribes answered with an internal error.
    fail_unsubscribe: u32,
    /// Answer every subscribe `Unsupported` (webhook not offered after all).
    refuse: bool,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            attempts: 4,
            window_ms: 2_000,
            ttl_cap_ms: 3_600_000,
            fail_unsubscribe: 0,
            refuse: false,
        }
    }
}

#[derive(Default)]
struct Inner {
    hooks: BTreeMap<String, Hook>,
    verified: BTreeSet<(String, String)>,
    policy: Policy,
    sender: bool,
}

/// The mock's webhook state.
#[derive(Default)]
pub struct Hooks {
    inner: Mutex<Inner>,
    ca: Mutex<Option<Vec<u8>>>,
}

fn refusal(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code": code, "message": message});
    if let Some(d) = data {
        e["data"] = d;
    }
    e
}

fn now_ms() -> u64 {
    crate::state::now_ms()
}

fn rfc3339(ms: u64) -> String {
    crate::obs::log::rfc3339_millis(std::time::UNIX_EPOCH + Duration::from_millis(ms))
}

/// The subscription key [SEP "Subscription identity"].
fn sub_key(principal: &str, url: &str, name: &str, args: &Value) -> String {
    crate::sha::sha256_hex(
        crate::engine::model::canonical(&json!([principal, url, name, args])).as_bytes(),
    )
}

/// What a POST to an endpoint came to: a status, or a connection category.
enum Posted {
    Status(u16, Vec<u8>),
    Failed(&'static str),
}

impl Hooks {
    /// Trust `pem` for deliveries (the test CA).
    pub fn set_ca(&self, pem: Vec<u8>) {
        *self.ca.lock().unwrap_or_else(|e| e.into_inner()) = Some(pem);
    }

    /// POST `body` to `url`, signed under `key` as `id`.
    fn post(&self, url: &str, key: &[u8], id: &str, sub_id: &str, body: &[u8]) -> Posted {
        let Ok(u) = crate::net::http::Url::parse(url) else {
            return Posted::Failed("connection_refused");
        };
        let Ok(tcp) = crate::net::http::connect_tcp(&u.host, u.port, Duration::from_secs(5)) else {
            return Posted::Failed("connection_refused");
        };
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(10)));
        let ca = self.ca.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(ca) = ca else {
            return Posted::Failed("tls_error");
        };
        let Ok(mut tls) = crate::net::tls::connect_with_ca(tcp, &u.host, &ca, None) else {
            return Posted::Failed("tls_error");
        };
        let ts = (now_ms() / 1000).to_string();
        let sig = crate::sec::stdwebhooks::sign(key, id, ts.parse().unwrap_or(0), body);
        let headers = [
            ("content-type", "application/json"),
            ("webhook-id", id),
            ("webhook-timestamp", ts.as_str()),
            ("webhook-signature", sig.as_str()),
            ("x-mcp-subscription-id", sub_id),
        ];
        match crate::net::http::send(&mut tls, &u.host_header(), "POST", &u.path, &headers, body) {
            Ok(r) => Posted::Status(r.status, r.body),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Posted::Failed("timeout"),
            Err(_) => Posted::Failed("connection_refused"),
        }
    }

    /// `events/subscribe` [SEP "Subscribing"].
    pub fn subscribe(
        self: &Arc<Self>,
        events: &Arc<super::mock_events::Events>,
        principal: Option<&str>,
        params: &Value,
    ) -> Result<Value, Value> {
        let Some(principal) = principal else {
            return Err(refusal(
                FORBIDDEN,
                "Forbidden: webhook subscriptions need an authenticated principal",
                None,
            ));
        };
        let name = params["name"].as_str().unwrap_or_default();
        if name != "mock.tick" {
            return Err(refusal(
                NOT_FOUND,
                "Not found",
                Some(json!({"kind": "event"})),
            ));
        }
        if self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .policy
            .refuse
        {
            eprintln!("MOCK_HOOK_REFUSED");
            return Err(refusal(
                -32026,
                "Unsupported",
                Some(json!({"feature": "deliveryMode", "value": "webhook"})),
            ));
        }
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let topic = args["topic"].as_str().unwrap_or("default").to_string();
        if topic == "forbidden" {
            return Err(refusal(FORBIDDEN, "Forbidden", None));
        }
        let url = params["delivery"]["url"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !url.starts_with("https://") {
            return Err(refusal(INVALID_PARAMS, "delivery.url must be https", None));
        }
        let secret = params["delivery"]["secret"].as_str().unwrap_or_default();
        let Ok(key) = crate::sec::stdwebhooks::decode_secret(secret) else {
            return Err(refusal(
                INVALID_PARAMS,
                "delivery.secret is not a whsec_ value of 24-64 bytes",
                None,
            ));
        };
        let k = sub_key(principal, &url, name, &args);
        let id = format!("sub_{}", &k[..16]);
        eprintln!(
            "MOCK_HOOK_SUBSCRIBE {}",
            json!({"id": id, "url": url, "topic": topic, "cursor": params["cursor"]})
        );
        // Endpoint verification, once per (principal, url) [SEP "Endpoint
        // verification"]: a signed challenge the endpoint must echo.
        let fresh_endpoint = !self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .verified
            .contains(&(principal.to_string(), url.clone()));
        if fresh_endpoint {
            let nonce = format!(
                "n-{}",
                &crate::sha::sha256_hex(format!("{k}{}", now_ms()).as_bytes())[..16]
            );
            let body = serde_json::to_vec(&json!({"type": "verification", "challenge": nonce}))
                .unwrap_or_default();
            let echoed = match self.post(
                &url,
                &key,
                &format!("msg_verification_{}", &nonce[2..]),
                &id,
                &body,
            ) {
                Posted::Status(s, b) if (200..300).contains(&s) => {
                    serde_json::from_slice::<Value>(&b)
                        .ok()
                        .and_then(|v| v["challenge"].as_str().map(str::to_string))
                        == Some(nonce.clone())
                }
                Posted::Status(..) => false,
                Posted::Failed(reason) => {
                    eprintln!("MOCK_HOOK_VERIFY_FAILED {reason}");
                    return Err(refusal(
                        CALLBACK_ENDPOINT_ERROR,
                        "Callback endpoint error",
                        Some(json!({"reason": reason})),
                    ));
                }
            };
            if !echoed {
                eprintln!("MOCK_HOOK_VERIFY_FAILED challenge_failed");
                return Err(refusal(
                    CALLBACK_ENDPOINT_ERROR,
                    "Callback endpoint error",
                    Some(json!({"reason": "challenge_failed"})),
                ));
            }
            eprintln!("MOCK_HOOK_VERIFIED {url}");
        }
        let (head, floor) = events.position(&topic);
        let now = now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.verified.insert((principal.to_string(), url.clone()));
        let ttl = params["ttlMs"]
            .as_u64()
            .unwrap_or(g.policy.ttl_cap_ms)
            .min(g.policy.ttl_cap_ms);
        let mut truncated = false;
        let status;
        if let Some(h) = g.hooks.get_mut(&k).filter(|h| h.expires_at > now) {
            // A refresh: the secret replaced, the lease granted again, a
            // suspended subscription reactivated; the cursor a no-op.
            status = json!({"active": h.active, "lastError": h.last_error,
                            "lastDeliveryAt": null});
            h.key = key;
            h.expires_at = now + ttl;
            h.active = true;
            h.last_error = None;
        } else {
            // A fresh subscription: from the cursor (replay), or from now.
            let start = match params["cursor"]
                .as_str()
                .and_then(|c| c.strip_prefix('c'))
                .and_then(|n| n.parse::<u64>().ok())
            {
                Some(from) if from < floor => {
                    truncated = true;
                    floor
                }
                Some(from) => from.min(head),
                None => head,
            };
            status = Value::Null;
            g.hooks.insert(
                k.clone(),
                Hook {
                    id: id.clone(),
                    url,
                    topic,
                    key,
                    expires_at: now + ttl,
                    next: start + 1,
                    watermark: start,
                    attempt: 0,
                    first_try: 0,
                    retry_at: 0,
                    active: true,
                    last_error: None,
                },
            );
        }
        let h = &g.hooks[&k];
        let mut out = json!({"resultType": "complete", "id": h.id,
                             "refreshBefore": rfc3339(h.expires_at),
                             "cursor": format!("c{}", h.watermark), "truncated": truncated});
        if !status.is_null() {
            out["deliveryStatus"] = status;
        }
        eprintln!("MOCK_HOOK_ANSWER {out}");
        if !g.sender {
            g.sender = true;
            let hooks = Arc::clone(self);
            let events = Arc::clone(events);
            let _ = std::thread::Builder::new()
                .name("mock-hook-sender".into())
                .spawn(move || hooks.send_loop(&events));
        }
        Ok(out)
    }

    /// `events/unsubscribe` [SEP "Unsubscribing"].
    pub fn unsubscribe(&self, principal: Option<&str>, params: &Value) -> Result<Value, Value> {
        let Some(principal) = principal else {
            return Err(refusal(FORBIDDEN, "Forbidden", None));
        };
        let name = params["name"].as_str().unwrap_or_default();
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let url = params["delivery"]["url"].as_str().unwrap_or_default();
        let k = sub_key(principal, url, name, &args);
        {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if g.policy.fail_unsubscribe > 0 {
                g.policy.fail_unsubscribe -= 1;
                eprintln!("MOCK_HOOK_UNSUBSCRIBE_FAILED {url}");
                return Err(refusal(-32603, "Internal error", None));
            }
        }
        let removed = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hooks
            .remove(&k);
        eprintln!(
            "MOCK_HOOK_UNSUBSCRIBE {}",
            json!({"url": url, "found": removed.is_some()})
        );
        match removed {
            Some(_) => Ok(json!({"resultType": "complete"})),
            None => Err(refusal(
                NOT_FOUND,
                "Not found",
                Some(json!({"kind": "subscription"})),
            )),
        }
    }

    /// Deliver what is due, forever.
    fn send_loop(&self, events: &super::mock_events::Events) {
        loop {
            std::thread::sleep(Duration::from_millis(25));
            let now = now_ms();
            // One attempt per live subscription per pass, in order: the next
            // occurrence after the last acknowledged or abandoned one.
            let due: Vec<(String, String, Vec<u8>, String, u64, Value)> = {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                g.hooks.retain(|_, h| h.expires_at > now);
                g.hooks
                    .iter()
                    .filter(|(_, h)| h.active && now >= h.retry_at)
                    .filter_map(|(k, h)| {
                        let o = events.occurrence(&h.topic, h.next)?;
                        Some((
                            k.clone(),
                            h.url.clone(),
                            h.key.clone(),
                            h.id.clone(),
                            h.next,
                            o,
                        ))
                    })
                    .collect()
            };
            for (k, url, key, id, seq, mut o) in due {
                o["cursor"] = json!(format!("c{seq}"));
                let event_id = o["eventId"].as_str().unwrap_or("evt").to_string();
                let body = serde_json::to_vec(&o).unwrap_or_default();
                let posted = self.post(&url, &key, &event_id, &id, &body);
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                let policy = (g.policy.attempts, g.policy.window_ms);
                let Some(h) = g.hooks.get_mut(&k) else {
                    continue;
                };
                if h.next != seq {
                    continue; // moved on meanwhile (a gap)
                }
                let outcome = match posted {
                    Posted::Status(s, _) if (200..300).contains(&s) => Ok(s),
                    // Final for this event, the subscription unaffected.
                    Posted::Status(s @ (410 | 413), _) => Err((s, None)),
                    Posted::Status(s, _) => {
                        Err((s, Some(if s < 500 { "http_4xx" } else { "http_5xx" })))
                    }
                    Posted::Failed(reason) => Err((0, Some(reason))),
                };
                match outcome {
                    Ok(s) => {
                        eprintln!(
                            "MOCK_HOOK_DELIVERED {}",
                            json!({"event": event_id, "status": s})
                        );
                        h.watermark = seq;
                        h.next = seq + 1;
                        h.attempt = 0;
                    }
                    Err((s, None)) => {
                        eprintln!(
                            "MOCK_HOOK_REJECTED {}",
                            json!({"event": event_id, "status": s})
                        );
                        h.watermark = seq;
                        h.next = seq + 1;
                        h.attempt = 0;
                    }
                    Err((s, Some(category))) => {
                        h.last_error = Some(category);
                        if h.attempt == 0 {
                            h.first_try = now;
                        }
                        h.attempt += 1;
                        eprintln!(
                            "MOCK_HOOK_FAILED {}",
                            json!({"event": event_id, "status": s, "attempt": h.attempt})
                        );
                        if h.attempt >= policy.0 || now.saturating_sub(h.first_try) >= policy.1 {
                            // Retries exhausted: abandoned, and the watermark
                            // moves past it with nothing said on the wire.
                            eprintln!("MOCK_HOOK_ABANDONED {}", json!({"event": event_id}));
                            h.watermark = seq;
                            h.next = seq + 1;
                            h.attempt = 0;
                        } else {
                            h.retry_at = now + 100 * (1u64 << h.attempt.min(5));
                        }
                    }
                }
            }
        }
    }

    /// A control tool on the webhook side, by name.
    pub fn control(
        &self,
        events: &super::mock_events::Events,
        name: &str,
        args: &Value,
    ) -> Option<Value> {
        match name {
            // Retries: `attempts` and `window_ms`; the lease cap, `ttl_cap_ms`.
            "mock.hook_policy" => {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(n) = args["attempts"].as_u64() {
                    g.policy.attempts = n as u32;
                }
                if let Some(n) = args["window_ms"].as_u64() {
                    g.policy.window_ms = n;
                }
                if let Some(n) = args["ttl_cap_ms"].as_u64() {
                    g.policy.ttl_cap_ms = n;
                }
                if let Some(n) = args["fail_unsubscribe"].as_u64() {
                    g.policy.fail_unsubscribe = n as u32;
                }
                if let Some(b) = args["refuse"].as_bool() {
                    g.policy.refuse = b;
                }
                Some(json!({"ok": true}))
            }
            // Suspend every subscription, as after repeated failures.
            "mock.hook_suspend" => {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                for h in g.hooks.values_mut() {
                    h.active = false;
                    h.last_error = Some("http_5xx");
                }
                Some(json!({"suspended": g.hooks.len()}))
            }
            // A gap: skip to the head and say so with the envelope.
            "mock.hook_gap" => {
                let targets: Vec<(String, Vec<u8>, String, String)> = {
                    let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.hooks
                        .values_mut()
                        .map(|h| {
                            let (head, _) = events.position(&h.topic);
                            h.next = head + 1;
                            h.watermark = head;
                            (
                                h.url.clone(),
                                h.key.clone(),
                                h.id.clone(),
                                format!("c{head}"),
                            )
                        })
                        .collect()
                };
                let mut answers = Vec::new();
                for (url, key, id, cursor) in targets {
                    let body = serde_json::to_vec(&json!({"type": "gap", "cursor": cursor}))
                        .unwrap_or_default();
                    answers.push(self.status(self.post(&url, &key, "msg_gap_1", &id, &body)));
                }
                Some(json!({"answers": answers}))
            }
            // The server ends every subscription with `code`.
            "mock.hook_terminate" => {
                let code = args["code"].as_i64().unwrap_or(FORBIDDEN);
                let gone: Vec<Hook> = {
                    let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    std::mem::take(&mut g.hooks).into_values().collect()
                };
                let mut answers = Vec::new();
                for h in gone {
                    let body = serde_json::to_vec(&json!({"type": "terminated",
                        "error": {"code": code, "message": "terminated by the mock"}}))
                    .unwrap_or_default();
                    answers.push(self.status(self.post(
                        &h.url,
                        &h.key,
                        "msg_terminated_1",
                        &h.id,
                        &body,
                    )));
                }
                Some(json!({"answers": answers}))
            }
            // A third party: a delivery and a challenge to every subscribed
            // URL, signed with a key the endpoint never gave anyone.
            "mock.hook_forge" => {
                let targets: Vec<(String, String)> = {
                    let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    g.hooks
                        .values()
                        .map(|h| (h.url.clone(), h.id.clone()))
                        .collect()
                };
                let forged = [9u8; 32];
                let mut answers = Vec::new();
                for (url, id) in targets {
                    let occurrence =
                        serde_json::to_vec(&json!({"eventId": "evt-forged", "name": "mock.tick",
                        "timestamp": "2026-10-10T00:00:00Z", "data": {"n": 999}}))
                        .unwrap_or_default();
                    let challenge = serde_json::to_vec(
                        &json!({"type": "verification", "challenge": "n-forged"}),
                    )
                    .unwrap_or_default();
                    answers.push(json!({
                        "delivery": self.status(self.post(&url, &forged, "evt-forged", &id, &occurrence)),
                        "challenge": self.status(self.post(&url, &forged, "msg_verification_forged", &id, &challenge)),
                    }));
                }
                Some(json!({"answers": answers}))
            }
            // What the server holds, for a test to look at.
            "mock.hook_state" => {
                let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                let subs: Map<String, Value> = g
                    .hooks
                    .values()
                    .map(|h| {
                        (
                            h.id.clone(),
                            json!({"topic": h.topic, "watermark": h.watermark, "active": h.active}),
                        )
                    })
                    .collect();
                Some(json!({"subscriptions": subs}))
            }
            _ => None,
        }
    }

    fn status(&self, p: Posted) -> Value {
        match p {
            Posted::Status(s, _) => json!(s),
            Posted::Failed(r) => json!(r),
        }
    }
}
