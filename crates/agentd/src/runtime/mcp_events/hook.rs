// SPDX-License-Identifier: AGPL-3.0-only
//! Webhook delivery (RFC 0045 §5.6.3; SEP-3415 "Webhook-Based Delivery").
//!
//! A webhook binding has its own route on the webhook listener,
//! `/hooks/_mcp/<binding key>`: stable across restarts, because the callback
//! URL is part of the server's subscription key. Every arm is a fresh
//! subscription (`events/unsubscribe`, then `events/subscribe` from the last
//! persisted cursor) under a secret minted for it and held in memory only, so
//! what the server abandoned while agentd was away is replayed. A refresh at
//! 80% of the lease keeps it.
//!
//! The listener verifies each request's Standard Webhooks signature before
//! anything else, answers the server's `verification` challenge itself, and
//! hands an occurrence to the loop, which answers `2xx` only once it is
//! durably appended. A refused delivery (`429`, `503`) freezes the cursor —
//! the server's watermark would move past it if it gave the event up — and a
//! refusal outstanding for five minutes pauses the binding until it can be
//! re-armed from the held cursor.

use super::pool::{Done, Failure, Job};
use super::{State, backoff, tame, wire};
use crate::config::settings::events::{Delivery, Replay};
use crate::runtime::events::Event;
use crate::runtime::reactor::Runtime;
use crate::sec::stdwebhooks;
use crate::state::{Kind, now_ms};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::mpsc::{Sender, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the webhook bindings' routes live; `/hooks/_` is agentd's.
pub const PATH_PREFIX: &str = "/hooks/_mcp/";
/// The delivery size profile [SEP "Delivery profile"]: a body over it is
/// answered `413` before it is read, which the server treats as final.
pub const MAX_BODY: usize = 256 * 1024;
/// The longest verification challenge echoed back.
const MAX_CHALLENGE_BYTES: usize = 1024;
/// `Retry-After` on a delivery that cannot be taken yet.
const RETRY_AFTER_S: u32 = 5;
/// How long a refused delivery may stay outstanding before the binding
/// pauses: well inside the draft's example retry window of 10–15 minutes.
const REFUSAL_PAUSE_MS: u64 = 5 * 60 * 1000;
/// The most refused deliveries tracked; past it the binding pauses at once.
const MAX_REFUSED: usize = 1024;
/// The lease to suggest when the binding names none.
pub const DEFAULT_TTL_MS: u64 = 3_600_000;
/// When the server grants no expiry, the refresh that still moves the
/// cursor and surfaces delivery problems comes this often.
const UNBOUNDED_REFRESH_MS: u64 = 3_600_000;
/// A refresh never comes sooner than this after the last — or than half the
/// lease, when the lease is shorter.
const MIN_REFRESH_MS: u64 = 5_000;
/// Consecutive `CallbackEndpointError`s before the next listed mode.
const CALLBACK_FAILURES: u32 = 5;

/// One binding's route, shared by the loop (which arms it) and the listener
/// (which verifies against it).
pub struct Route {
    pub server: String,
    pub name: String,
    /// The secret the live subscription signs with.
    pub secret: Option<Vec<u8>>,
    /// The secret an arm in flight subscribes with: the server's
    /// `verification` challenge arrives under it before the subscribe answers.
    pub pending: Option<Vec<u8>>,
    /// The subscription `id` the server derived, once it answered.
    pub id: Option<String>,
    /// Deliveries are taken: the subscribe answered and the loop applied it.
    pub armed: bool,
}

/// Every webhook binding's route, by binding key.
pub type Routes = Arc<Mutex<BTreeMap<String, Route>>>;

/// The callback URL of binding `key`, on `base` (`webhooks.url`).
pub fn url(base: &str, key: &str) -> String {
    format!("{}{PATH_PREFIX}{key}", base.trim_end_matches('/'))
}

/// One verified delivery — an occurrence or a control envelope — for the loop.
pub struct Arrival {
    pub key: String,
    pub webhook_id: String,
    pub body: Value,
    pub reply: SyncSender<Answer>,
}

impl std::fmt::Debug for Arrival {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arrival")
            .field("key", &self.key)
            .field("webhook_id", &self.webhook_id)
            .finish_non_exhaustive()
    }
}

/// The loop's answer to a delivery.
pub struct Answer {
    pub status: u16,
    pub reason: &'static str,
    pub retry_after: Option<u32>,
    pub body: Value,
}

impl Answer {
    fn ok(status: &str) -> Answer {
        Answer {
            status: 200,
            reason: "OK",
            retry_after: None,
            body: json!({"status": status}),
        }
    }
    fn retry(status: u16, after: u32, why: &str) -> Answer {
        Answer {
            status,
            reason: if status == 429 {
                "Too Many Requests"
            } else {
                "Service Unavailable"
            },
            retry_after: Some(after.max(1)),
            body: json!({"error": why}),
        }
    }
    /// Final for this delivery [SEP: `410` and `413` are non-retryable].
    fn gone(why: &str) -> Answer {
        Answer {
            status: 410,
            reason: "Gone",
            retry_after: None,
            body: json!({"error": why}),
        }
    }
    fn too_large() -> Answer {
        Answer {
            status: 413,
            reason: "Payload Too Large",
            retry_after: None,
            body: json!({"error": "the occurrence is over the size this binding keeps"}),
        }
    }
}

fn respond(a: Answer) -> ::mcp::http_server::RawResponse {
    let mut r = ::mcp::http_server::RawResponse::json(
        a.status,
        a.reason,
        serde_json::to_vec(&a.body).unwrap_or_default(),
    );
    if let Some(s) = a.retry_after {
        r.headers.push(("Retry-After", s.to_string()));
    }
    r
}

/// Serve one request under [`PATH_PREFIX`], on the listener's thread.
pub fn serve(
    routes: &Routes,
    req: &::mcp::http_server::RawRequest,
    events: &Sender<Event>,
    timeout: Duration,
    log: &crate::obs::log::Logger,
) -> ::mcp::http_server::RawResponse {
    let key = req.path().trim_start_matches(PATH_PREFIX).to_string();
    let route = {
        let g = routes.lock().unwrap_or_else(|e| e.into_inner());
        g.get(&key).map(|r| {
            (
                r.server.clone(),
                r.name.clone(),
                r.secret.clone(),
                r.pending.clone(),
                r.id.clone(),
                r.armed,
            )
        })
    };
    // No binding here (removed, or never one): final, so the server stops
    // retrying a subscription agentd no longer holds.
    let Some((server, name, secret, pending, id, armed)) = route else {
        return respond(Answer::gone("no MCP Events binding at this path"));
    };
    if !req.method.eq_ignore_ascii_case("POST") {
        return ::mcp::http_server::RawResponse::text(
            405,
            "Method Not Allowed",
            b"deliveries are POST".to_vec(),
        );
    }
    let keys: Vec<&[u8]> = secret
        .iter()
        .chain(pending.iter())
        .map(Vec::as_slice)
        .collect();
    if keys.is_empty() {
        // Not armed yet (a restart, a re-arm in flight): nothing to verify
        // against, so nothing is read, and the server retries.
        return respond(Answer::retry(503, RETRY_AFTER_S, "not armed yet"));
    }
    let signed = stdwebhooks::Signed {
        id: req.header("webhook-id"),
        timestamp: req.header("webhook-timestamp"),
        signature: req.header("webhook-signature"),
    };
    let now_s = now_ms() / 1000;
    let verified = stdwebhooks::verify(&signed, &req.body, &keys, now_s);
    let sub_id = req.header("x-mcp-subscription-id");
    let denied = match (&verified, sub_id) {
        (Err(r), _) => Some(r.as_str()),
        (Ok(_), None) => Some("missing_header"),
        (Ok(_), Some(got)) if id.as_deref().is_some_and(|want| want != got) => {
            Some("subscription_mismatch")
        }
        _ => None,
    };
    if let Some(reason) = denied {
        crate::obs::metrics::record_mcp_event(&server, &name, "webhook", "unauthenticated");
        log.warn(
            "mcp.events.hook.denied",
            json!({"server": server, "event_type": name, "reason": reason}),
        );
        return ::mcp::http_server::RawResponse::text(
            401,
            "Unauthorized",
            b"authentication failed".to_vec(),
        );
    }
    let Ok(Value::Object(body)) = serde_json::from_slice::<Value>(&req.body) else {
        return respond(Answer::gone("the body is not a JSON object"));
    };
    // The endpoint's own: proven by echoing the nonce, under a signature that
    // verified — never for a third party [SEP "Endpoint verification"].
    if body.get("type").and_then(Value::as_str) == Some("verification") {
        return match body.get("challenge").and_then(Value::as_str) {
            Some(c) if c.len() <= MAX_CHALLENGE_BYTES => ::mcp::http_server::RawResponse::json(
                200,
                "OK",
                serde_json::to_vec(&json!({"challenge": c})).unwrap_or_default(),
            ),
            _ => respond(Answer::gone("a verification without a usable challenge")),
        };
    }
    if !armed {
        // Before the subscribe answer reached the loop [SEP "Subscribe and
        // delivery race"].
        return respond(Answer::retry(503, RETRY_AFTER_S, "not armed yet"));
    }
    let webhook_id = signed.id.unwrap_or_default().to_string();
    let (tx, rx) = sync_channel(1);
    let arrival = Arrival {
        key,
        webhook_id,
        body: Value::Object(body),
        reply: tx,
    };
    if events.send(Event::McpEventHook(Box::new(arrival))).is_err() {
        return respond(Answer::retry(503, RETRY_AFTER_S, "shutting down"));
    }
    match rx.recv_timeout(timeout) {
        Ok(a) => respond(a),
        // Not answered in time: retried, and the dedup set drops a copy that
        // was appended after all.
        Err(_) => respond(Answer::retry(
            503,
            RETRY_AFTER_S,
            "the runtime did not answer",
        )),
    }
}

/// The dedup handle of a refused delivery.
fn refusal_key(webhook_id: &str) -> String {
    crate::sha::sha256_hex(webhook_id.as_bytes())[..32].to_string()
}

fn parse_ms(t: &str) -> Option<u64> {
    super::bridge::timestamp_ms(t)
}

impl Runtime {
    /// The routes the listener serves for MCP Events.
    #[cfg(feature = "a2a")]
    pub(crate) fn events_hook_routes(&self) -> Routes {
        Arc::clone(&self.mcp_events.hooks)
    }

    /// Every binding that may deliver by webhook has a route — answering
    /// `503` until it is armed — and no other key has one, so a removed
    /// binding's late deliveries are answered `410`.
    pub(super) fn events_sync_routes(&mut self) {
        let wanted: BTreeMap<String, (String, String)> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.delivery.contains(&Delivery::Webhook))
            .map(|b| (b.key.clone(), (b.server.clone(), b.name.clone())))
            .collect();
        let mut g = self
            .mcp_events
            .hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        g.retain(|k, _| wanted.contains_key(k));
        for (k, (server, name)) in wanted {
            g.entry(k).or_insert(Route {
                server,
                name,
                secret: None,
                pending: None,
                id: None,
                armed: false,
            });
        }
    }

    fn events_route<R>(&self, key: &str, f: impl FnOnce(&mut Route) -> R) -> Option<R> {
        let mut g = self
            .mcp_events
            .hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        g.get_mut(key).map(f)
    }

    /// A delivery from the listener, answered.
    pub(crate) fn events_hook_delivery(&mut self, d: Arrival) {
        let answer = self.events_hook_answer(&d.key, &d.webhook_id, d.body);
        let _ = d.reply.send(answer);
    }

    fn events_hook_answer(&mut self, key: &str, webhook_id: &str, body: Value) -> Answer {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return Answer::gone("no MCP Events binding at this path");
        };
        if b.mode != Some(Delivery::Webhook) || !b.armed {
            return Answer::retry(503, RETRY_AFTER_S, "not armed");
        }
        if let Some(kind) = body.get("type").and_then(Value::as_str) {
            match kind {
                "gap" => self.events_hook_gap(key, body.get("cursor")),
                "terminated" => self.events_hook_terminated(key, &body["error"]),
                _ => {} // a control envelope this build does not know
            }
            return Answer::ok("accepted");
        }
        let refusal = refusal_key(webhook_id);
        if self.pressure.refusal(false).is_some() {
            self.events_hook_refused(key, refusal, now);
            return Answer::retry(429, 30, "under pressure");
        }
        if self.durable.is_degraded() {
            self.events_hook_refused(key, refusal, now);
            return Answer::retry(503, 30, "the store is degraded");
        }
        if let Some(b) = self.mcp_events.bindings.get_mut(key) {
            b.refill(now);
        }
        let cursor = body
            .get("cursor")
            .and_then(Value::as_str)
            .filter(|c| c.len() <= super::bridge::MAX_CURSOR_BYTES)
            .map(str::to_string);
        match self.events_take(key, &body, "webhook", now) {
            super::Taken::Rejected("oversize") => Answer::too_large(),
            super::Taken::Rejected(_) => Answer::gone("malformed occurrence"),
            super::Taken::Rate => {
                self.events_hook_refused(key, refusal, now);
                let after = self
                    .mcp_events
                    .bindings
                    .get(key)
                    .map_or(RETRY_AFTER_S as u64, |b| {
                        b.refill_at(now).saturating_sub(now).div_ceil(1000)
                    });
                Answer::retry(429, after as u32, "the binding's rate is spent")
            }
            super::Taken::Refused(_) => {
                self.events_hook_refused(key, refusal, now);
                Answer::retry(503, 30, "the stream refused it")
            }
            // Kept, or kept already: `2xx` only once the stream head is
            // saved — a duplicate's earlier append may still be unflushed.
            taken @ (super::Taken::Appended | super::Taken::Duplicate) => {
                if let Err(e) = self.durable.flush(true) {
                    self.log
                        .warn("mcp.events.hook.unsaved", json!({"err": e.to_string()}));
                    self.events_hook_refused(key, refusal, now);
                    return Answer::retry(503, 30, "the stream head was not saved");
                }
                let Some(b) = self.mcp_events.bindings.get_mut(key) else {
                    return Answer::ok("appended");
                };
                b.refused.remove(&refusal);
                b.seen.expire(now);
                if matches!(taken, super::Taken::Appended) {
                    let _ = self.durable.put(
                        Kind::Memory,
                        &b.seen_key(),
                        json!({"entries": b.seen.to_json()}),
                        None,
                    );
                }
                if cursor.is_some() {
                    self.events_hook_watermark(key, cursor);
                }
                Answer::ok(if matches!(taken, super::Taken::Appended) {
                    "appended"
                } else {
                    "duplicate"
                })
            }
        }
    }

    /// A delivery refused for a reason that may pass: held until it is taken
    /// on a retry, and while any is held no watermark is persisted — the
    /// server's would move past it if it gave the delivery up.
    fn events_hook_refused(&mut self, key: &str, refusal: String, now: u64) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.refused.len() >= MAX_REFUSED && !b.refused.contains_key(&refusal) {
            return self.events_hook_pause(key, "refusal");
        }
        b.refused.entry(refusal).or_insert(now);
    }

    /// Persist the server's watermark — unless a refused delivery is still
    /// outstanding.
    fn events_hook_watermark(&mut self, key: &str, cursor: Option<String>) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if !b.refused.is_empty() || (b.cursor == cursor && b.recorded) {
            return;
        }
        if matches!(b.replay, Replay::None) {
            return; // always re-armed from now
        }
        b.cursor = cursor;
        self.events_record(key);
    }

    /// `{"type":"gap","cursor":…}`: events were skipped between refreshes.
    fn events_hook_gap(&mut self, key: &str, cursor: Option<&Value>) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        crate::obs::metrics::record_mcp_event_gap(&server, &name, "truncated");
        self.log.warn(
            "mcp.events.gap",
            json!({"server": server, "event_type": name, "cause": "truncated"}),
        );
        let cursor = cursor
            .and_then(Value::as_str)
            .filter(|c| c.len() <= super::bridge::MAX_CURSOR_BYTES)
            .map(str::to_string);
        if cursor.is_some() {
            self.events_hook_watermark(key, cursor);
        }
    }

    /// `{"type":"terminated","error":…}`: the server ended the subscription.
    fn events_hook_terminated(&mut self, key: &str, error: &Value) {
        let now = now_ms();
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        // The subscription no longer exists; a refresh still out comes back
        // to a binding with no secret, and is replaced by a fresh arm.
        b.armed = false;
        b.secret = None;
        b.subscribed = false;
        let (server, name) = (b.server.clone(), b.name.clone());
        self.events_route(key, |r| {
            r.armed = false;
            r.secret = None;
            r.pending = None;
            r.id = None;
        });
        self.log.error(
            "mcp.events.terminated",
            json!({"server": server, "event_type": name, "code": code}),
        );
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("terminated");
        match code {
            wire::code::FORBIDDEN => self.events_refuse(key, "forbidden", message),
            // The type is gone or changed: list again, which refuses it if so.
            wire::code::NOT_FOUND | wire::code::UNSUPPORTED => self.events_rediscover(key),
            _ => {
                if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                    b.failures += 1;
                    b.due_at = now + backoff(b.failures, super::BACKOFF_MAX_MS);
                }
            }
        }
    }

    /// Arm, refresh, pause and resume the webhook bindings that are due.
    pub(super) fn events_hook_due(&mut self, now: u64) {
        let pressure = self.pressure.refusal(false).is_some();
        let degraded = self.durable.is_degraded();
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.mode == Some(Delivery::Webhook) && !b.in_flight)
            .map(|b| b.key.clone())
            .collect();
        for key in keys {
            let Some(b) = self.mcp_events.bindings.get(&key) else {
                continue;
            };
            match b.state.clone() {
                State::Active => {}
                State::Paused(cause) => {
                    if pressure || degraded || now < b.due_at {
                        continue;
                    }
                    // Re-armed from the held cursor: a fresh subscription
                    // replays what was refused meanwhile.
                    let b = self.mcp_events.bindings.get_mut(&key).expect("held");
                    b.state = State::Active;
                    b.armed = false;
                    b.due_at = now;
                    self.log.info(
                        "mcp.events.resumed",
                        json!({"server": b.server, "event_type": b.name, "cause": cause}),
                    );
                }
                _ => continue,
            }
            let b = &self.mcp_events.bindings[&key];
            let stale = b
                .refused
                .values()
                .min()
                .is_some_and(|t| now >= t + REFUSAL_PAUSE_MS);
            if b.armed && stale {
                self.events_hook_pause(&key, "refusal");
                continue;
            }
            if !b.loaded && self.events_load_records(&key).is_err() {
                self.events_pause_for(&key, "store_degraded", None);
                continue;
            }
            let b = &self.mcp_events.bindings[&key];
            if !b.armed && now >= b.due_at {
                self.events_dispatch_arm(&key, true, now);
            } else if b.armed && now >= b.refresh_at {
                self.events_dispatch_arm(&key, false, now);
            }
        }
    }

    /// A refusal outstanding too long: the subscription is dropped, the
    /// cursor held; a resume re-arms from it.
    fn events_hook_pause(&mut self, key: &str, cause: &'static str) {
        self.events_hook_stop(key, false);
        self.events_pause_for(key, cause, None);
    }

    /// Mint a secret and subscribe (`fresh`), or refresh the live
    /// subscription with the same key and secret.
    fn events_dispatch_arm(&mut self, key: &str, fresh: bool, now: u64) {
        let Some(base) = self.settings.webhooks.url.clone() else {
            return; // refused at load; nothing to deliver to
        };
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let Some(client) = self.mcp.get(&b.server).cloned() else {
            return; // not connected: a reload or a re-dial brings it
        };
        let server = b.server.clone();
        if crate::mcp::pace::take(&server).is_err() {
            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                if fresh {
                    b.due_at = now + 1_000;
                } else {
                    b.refresh_at = now + 1_000;
                }
            }
            return;
        }
        let secret = if fresh {
            match stdwebhooks::mint_secret() {
                Ok(s) => s,
                Err(e) => {
                    self.log.error(
                        "mcp.events.arm.fail",
                        json!({"server": server, "err": format!("no randomness for a webhook secret: {e}")}),
                    );
                    if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                        b.failures += 1;
                        b.due_at = now + backoff(b.failures, super::BACKOFF_MAX_MS);
                    }
                    return;
                }
            }
        } else {
            match b.secret.clone() {
                Some(s) => s,
                None => return self.events_hook_rearm(key, now),
            }
        };
        let callback = url(&base, key);
        let b = self.mcp_events.bindings.get_mut(key).expect("held above");
        let max_age = match b.replay {
            Replay::Within(d) => Some(d.as_millis() as u64),
            Replay::None => None,
        };
        let subscribe = wire::subscribe_params(
            &b.name,
            &b.arguments,
            &callback,
            &secret,
            b.cursor.as_deref(),
            max_age,
            b.ttl_ms,
        );
        let unsubscribe = wire::unsubscribe_params(&b.name, &b.arguments, &callback);
        let prior = if fresh { b.prior.take() } else { None };
        let mut lost = 0;
        if fresh {
            // What was refused is replayed from the held cursor — unless
            // there is none to replay from, and then it is lost, and said.
            if b.cursor.is_none() {
                lost = b.refused.len();
            }
            b.refused.clear();
            b.secret = Some(secret.clone());
        }
        b.subscribed = true;
        b.in_flight = true;
        self.mcp_events.next_generation += 1;
        let generation = self.mcp_events.next_generation;
        b.generation = generation;
        if fresh {
            // A new subscription: its id is learnt from its answer, and the
            // old one's secret signs nothing agentd takes any more.
            let pending = stdwebhooks::decode_secret(&secret).ok();
            self.events_route(key, |r| {
                r.pending = pending;
                r.secret = None;
                r.id = None;
                r.armed = false;
            });
        }
        if lost > 0 {
            let name = self.mcp_events.bindings[key].name.clone();
            crate::obs::metrics::record_mcp_event_gap(&server, &name, "no_replay");
            self.log.warn(
                "mcp.events.gap",
                json!({"server": server, "event_type": name, "cause": "no_replay", "refused": lost}),
            );
        }
        if let Some(pool) = &self.mcp_events.pool {
            pool.submit(Job::Arm {
                key: key.to_string(),
                generation,
                server,
                client,
                prior,
                fresh,
                unsubscribe,
                subscribe,
                bound: super::REQUEST_BOUND,
            });
        }
    }

    /// Arm again, fresh, at once.
    fn events_hook_rearm(&mut self, key: &str, now: u64) {
        if let Some(b) = self.mcp_events.bindings.get_mut(key) {
            b.armed = false;
            b.due_at = now;
        }
        self.events_route(key, |r| r.armed = false);
    }

    /// An `events/subscribe` answered.
    pub(super) fn events_armed(
        &mut self,
        key: &str,
        fresh: bool,
        result: Result<wire::Subscribed, Failure>,
    ) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.in_flight = false;
        let wanted = b.mode == Some(Delivery::Webhook) && b.state == State::Active;
        let sub = match result {
            Ok(sub) if wanted => sub,
            // Made while the binding moved on (refused, paused, another mode):
            // not kept.
            Ok(_) => return self.events_hook_stop(key, false),
            Err(_) if !wanted => return,
            Err(e) => return self.events_arm_failed(key, e),
        };
        // The route verifies under the secret this subscription was made or
        // refreshed with. None — the binding was stopped or reset while the
        // request was out — or made on a connection a reload has since
        // replaced: what the server holds is not agentd's to verify, and a
        // fresh arm replaces it (letting go of it first).
        let verifier = b
            .secret
            .as_deref()
            .and_then(|s| stdwebhooks::decode_secret(s).ok());
        let Some(verifier) = verifier.filter(|_| b.prior.is_none()) else {
            return self.events_hook_rearm(key, now);
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        let lease_end = sub.refresh_before.as_deref().and_then(parse_ms);
        // At 80% of the lease — never sooner than the floor that keeps a
        // tiny grant from becoming a refresh storm, and never so late that
        // the floor outlasts the lease itself.
        b.refresh_at = match lease_end {
            Some(end) => {
                let left = end.saturating_sub(now);
                now + (left * 8 / 10).max(MIN_REFRESH_MS.min(left / 2)).max(1_000)
            }
            None => now + UNBOUNDED_REFRESH_MS,
        };
        b.armed = true;
        b.failures = 0;
        b.hook_failures = 0;
        let cursor_present = b.cursor.is_some();
        let id = sub.id.clone();
        self.events_route(key, |r| {
            r.secret = Some(verifier);
            r.pending = None;
            r.id = Some(id);
            r.armed = true;
        });
        let status = sub.delivery_status.clone().unwrap_or_default();
        if !fresh && status.failing() {
            // Deliveries failed — perhaps ones agentd never saw. The refresh
            // reactivated them, but its watermark may be past them: re-armed
            // from the last cursor agentd kept, so they are replayed.
            self.log.warn(
                "mcp.events.delivery_suspended",
                json!({"server": server, "event_type": name,
                       "last_error": status.last_error.as_deref().map(tame),
                       "active": status.active}),
            );
            return self.events_hook_rearm(key, now);
        }
        if status.throttled == Some(true) {
            self.log.info(
                "mcp.events.throttled",
                json!({"server": server, "event_type": name, "retry_after_ms": status.retry_after_ms}),
            );
        }
        if sub.truncated {
            crate::obs::metrics::record_mcp_event_gap(&server, &name, "truncated");
            self.log.warn(
                "mcp.events.gap",
                json!({"server": server, "event_type": name, "cause": "truncated"}),
            );
        }
        // An absent cursor is a type without replay, never a reason to drop
        // the one held; one over the cap is not kept.
        match sub.cursor {
            Some(c) if c.len() <= super::bridge::MAX_CURSOR_BYTES => {
                self.events_hook_watermark(key, Some(c));
            }
            Some(_) => self.log.warn(
                "mcp.events.arm.fail",
                json!({"server": server, "event_type": name,
                       "err": format!("events/subscribe: a cursor over {} bytes, not kept", super::bridge::MAX_CURSOR_BYTES)}),
            ),
            None => {}
        }
        // The record exists from the first arm, so a re-arm after downtime of
        // a type without replay says what it could not recover.
        if !self.mcp_events.bindings[key].recorded {
            self.events_record(key);
        }
        if fresh {
            self.log.info(
                "mcp.events.armed",
                json!({"server": server, "event_type": name, "mode": "webhook",
                       "cursor_present": cursor_present, "truncated": sub.truncated}),
            );
        }
    }

    /// An arm or refresh refused: what the code says follows (§5.13).
    fn events_arm_failed(&mut self, key: &str, e: Failure) {
        let now = now_ms();
        self.events_route(key, |r| {
            r.pending = None;
            r.armed = false;
        });
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.armed = false;
        let (server, name) = (b.server.clone(), b.name.clone());
        let mut wait_max = super::BACKOFF_MAX_MS;
        match &e {
            Failure::Rpc(r) => match r.code {
                wire::code::FORBIDDEN => return self.events_refuse(key, "forbidden", &r.message),
                wire::code::INVALID_PARAMS => {
                    return self.events_refuse(key, "invalid_arguments", &r.message);
                }
                wire::code::METHOD_NOT_FOUND => {
                    return self.events_refuse(key, "method_not_found", &r.message);
                }
                wire::code::NOT_FOUND => return self.events_rediscover(key),
                wire::code::UNSUPPORTED => {
                    return self.events_next_mode(key, "not_supported");
                }
                wire::code::CALLBACK_ENDPOINT_ERROR => {
                    let reason = r
                        .data
                        .as_ref()
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        .map(tame);
                    self.log.error(
                        "mcp.events.callback_failed",
                        json!({"server": server, "event_type": name, "reason": reason,
                               "url": self.settings.webhooks.url}),
                    );
                    let b = self.mcp_events.bindings.get_mut(key).expect("held above");
                    b.hook_failures += 1;
                    if b.hook_failures >= CALLBACK_FAILURES {
                        return self.events_next_mode(key, "callback_failed");
                    }
                }
                wire::code::RESOURCE_EXHAUSTED => wait_max = super::EXHAUSTED_MAX_MS,
                _ => {}
            },
            Failure::Transport(_) | Failure::TooLarge => {}
        }
        let b = self.mcp_events.bindings.get_mut(key).expect("held above");
        b.failures += 1;
        let wait = backoff(b.failures, wait_max);
        b.due_at = now + wait;
        self.log.warn(
            "mcp.events.arm.fail",
            json!({"server": server, "event_type": name, "err": tame(&e.to_string()), "retry_ms": wait}),
        );
    }

    /// The operator's next listed mode the server offers, or refused.
    fn events_next_mode(&mut self, key: &str, cause: &'static str) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let offered: Vec<String> = self
            .mcp_events
            .listings
            .get(&b.server)
            .and_then(|l| l.descriptors.as_ref())
            .and_then(|d| d.get(&b.name))
            .map(|d| d.delivery.clone())
            .unwrap_or_default();
        let from = b.mode;
        let next = b
            .delivery
            .iter()
            .copied()
            .skip_while(|m| Some(*m) != from)
            .skip(1)
            .find(|m| offered.iter().any(|o| o == m.as_str()));
        let Some(to) = next else {
            return self.events_refuse(
                key,
                "not_supported",
                &format!(
                    "no listed mode after {:?} is offered",
                    from.map(Delivery::as_str)
                ),
            );
        };
        if from == Some(Delivery::Webhook) {
            self.events_hook_stop(key, false);
        }
        let b = self.mcp_events.bindings.get_mut(key).expect("held above");
        // Not chosen again at the next listing; a reload or a re-dial
        // decides afresh.
        if let Some(f) = from {
            b.demoted.push(f);
        }
        b.mode = Some(to);
        b.armed = false;
        b.failures = 0;
        b.hook_failures = 0;
        b.due_at = now_ms();
        self.log.warn(
            "mcp.events.mode_changed",
            json!({"server": b.server, "event_type": b.name,
                   "from": from.map(Delivery::as_str), "to": to.as_str(), "cause": cause}),
        );
    }

    /// The type may be gone: list the server again; the binding is armed
    /// against what it then lists, after the backoff this earns.
    pub(super) fn events_rediscover(&mut self, key: &str) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.state = State::Discovering;
        b.failures += 1;
        let server = b.server.clone();
        if let Some(l) = self.mcp_events.listings.get_mut(&server) {
            l.due_at = 0;
            l.descriptors = None;
        }
    }

    /// Stop delivering to this binding: its route answers `503` until it is
    /// armed again, and a subscription that may exist on the server is let
    /// go — eagerly; a lease would end it too. One request per binding is
    /// out at a time, so an unsubscribe never overtakes a later subscribe:
    /// with an arm in flight, its answer — no longer wanted — lets it go.
    /// `removing`: the binding goes now, and nothing waits for the answer.
    pub(super) fn events_hook_stop(&mut self, key: &str, removing: bool) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.armed = false;
        b.secret = None;
        self.events_route(key, |r| {
            r.armed = false;
            r.secret = None;
            r.pending = None;
            r.id = None;
        });
        let b = self.mcp_events.bindings.get_mut(key).expect("held above");
        // A request still out answers for itself: an arm's answer, no longer
        // wanted (or its binding gone), lets go of what it made.
        if b.in_flight || !(b.subscribed || b.prior.is_some()) {
            return;
        }
        let current = std::mem::take(&mut b.subscribed);
        let prior = b.prior.take();
        let (server, name, arguments) = (b.server.clone(), b.name.clone(), b.arguments.clone());
        self.mcp_events.next_generation += 1;
        let generation = self.mcp_events.next_generation;
        let b = self.mcp_events.bindings.get_mut(key).expect("held above");
        b.generation = generation;
        let sent =
            self.events_unsubscribe(&server, &name, &arguments, key, generation, current, prior);
        if sent
            && !removing
            && let Some(b) = self.mcp_events.bindings.get_mut(key)
        {
            b.in_flight = true;
        }
    }

    /// Submit an `events/unsubscribe` for binding `key`, on `prior` (the
    /// connection it was made on, when a reload replaced it) or the live one.
    /// Whether it was sent.
    #[allow(clippy::too_many_arguments)]
    fn events_unsubscribe(
        &mut self,
        server: &str,
        name: &str,
        arguments: &Map<String, Value>,
        key: &str,
        generation: u64,
        current: bool,
        prior: Option<Arc<crate::mcp::client::McpClient>>,
    ) -> bool {
        let (Some(base), Some(pool)) =
            (self.settings.webhooks.url.as_deref(), &self.mcp_events.pool)
        else {
            return false;
        };
        let client = current.then(|| self.mcp.get(server).cloned()).flatten();
        if client.is_none() && prior.is_none() {
            return false;
        }
        pool.submit(Job::Unsubscribe {
            key: key.to_string(),
            generation,
            server: server.to_string(),
            client,
            prior,
            params: wire::unsubscribe_params(name, arguments, &url(base, key)),
            bound: super::REQUEST_BOUND,
        });
        true
    }

    /// A reload replaced `server`'s connection — its credential, and so the
    /// principal the subscriptions are keyed by, may have changed. Each live
    /// webhook subscription is unsubscribed on the old connection before it
    /// is armed on the new one (the arm carries it), so the old one does not
    /// go on delivering under a secret agentd no longer holds.
    pub(crate) fn events_connection_replaced(
        &mut self,
        server: &str,
        old: Arc<crate::mcp::client::McpClient>,
    ) {
        let now = now_ms();
        for b in self.mcp_events.bindings.values_mut() {
            if b.server == server && b.subscribed {
                b.prior = Some(Arc::clone(&old));
                b.subscribed = false;
                b.armed = false;
                b.due_at = now;
            }
        }
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.server == server)
            .map(|b| b.key.clone())
            .collect();
        for k in keys {
            self.events_route(&k, |r| r.armed = false);
        }
    }

    /// An `events/unsubscribe` answered: counted, and the binding may send
    /// its next request.
    fn events_unsubscribed(
        &mut self,
        key: &str,
        generation: u64,
        server: &str,
        result: Result<(), Failure>,
    ) {
        if let Some(b) = self.mcp_events.bindings.get_mut(key)
            && b.generation == generation
        {
            b.in_flight = false;
        }
        let outcome = match &result {
            Ok(()) => "ok",
            // No such subscription is what an eager unsubscribe expects at
            // times (an arm's first step, a lease that ran out).
            Err(Failure::Rpc(r)) if r.code == wire::code::NOT_FOUND => "ok",
            Err(Failure::Rpc(_)) => "refused",
            Err(_) => "failed",
        };
        crate::obs::metrics::record_mcp_events_request(server, wire::method::UNSUBSCRIBE, outcome);
    }

    /// Route a pool answer that belongs to webhook delivery.
    pub(super) fn events_hook_done(&mut self, done: Done) {
        match done {
            Done::Arm {
                key,
                generation,
                fresh,
                result,
                server,
                client,
                unsubscribe,
            } => {
                let Some(b) = self
                    .mcp_events
                    .bindings
                    .get(&key)
                    .filter(|b| b.generation == generation)
                else {
                    // Removed while its arm was out: what it made is let go,
                    // on the connection it was made on. Removed and declared
                    // again: the new binding arms afresh after it — an arm
                    // unsubscribes first, and the last subscribe's secret is
                    // the one that holds.
                    crate::obs::metrics::record_mcp_events_request(
                        &server,
                        wire::method::SUBSCRIBE,
                        if result.is_ok() { "ok" } else { "failed" },
                    );
                    if let Some(b) = self.mcp_events.bindings.get_mut(&key) {
                        if result.is_ok() {
                            b.subscribed = true;
                            if !b.in_flight && b.mode == Some(Delivery::Webhook) {
                                self.events_hook_rearm(&key, now_ms());
                            }
                        }
                        return;
                    }
                    if result.is_ok()
                        && let Some(pool) = &self.mcp_events.pool
                    {
                        pool.submit(Job::Unsubscribe {
                            key: String::new(),
                            generation: 0,
                            server,
                            client: Some(client),
                            prior: None,
                            params: unsubscribe,
                            bound: super::REQUEST_BOUND,
                        });
                    }
                    return;
                };
                crate::obs::metrics::record_mcp_events_request(
                    &b.server,
                    wire::method::SUBSCRIBE,
                    match &result {
                        Ok(_) => "ok",
                        Err(Failure::Rpc(_)) => "refused",
                        Err(_) => "failed",
                    },
                );
                self.events_armed(&key, fresh, result);
            }
            Done::Unsubscribe {
                key,
                generation,
                server,
                result,
            } => self.events_unsubscribed(&key, generation, &server, result),
            _ => {}
        }
    }
}
