// SPDX-License-Identifier: AGPL-3.0-only
//! Push delivery (RFC 0045 §5.6.2, P4): one long-lived `events/stream` per
//! binding, read on a thread of its own, its frames handed to the loop.
//!
//! The contract is MCPG 0.1.0's (MCPG RFC 0031 §2.1-2.3, §5.3): one POST
//! whose SSE body carries only `notifications/events/*`, each tagged with the
//! request's id as its subscription; `active {cursor, truncated}` first (and
//! again, `truncated: true`, where events were skipped mid-stream); an `event`
//! per occurrence carrying the position after it; `heartbeat {cursor}` data
//! frames at least every 30 s; a transient `error`; and `terminated {error}`
//! before the server ends the subscription. A reconnect is a new stream from
//! the last cursor.
//!
//! The rules are poll's: an occurrence becomes one stream event, and the
//! binding's cursor never moves past an occurrence that is not durably
//! appended (§5.1 rule 2) — the loop, the single writer, appends, flushes,
//! and only then keeps the frame's cursor. Whatever stops intake (pressure,
//! the binding's `rate`, a store that failed) closes the stream; frames still
//! queued from it are dropped, and the next stream asks again from the cursor
//! kept.
//!
//! The stream runs on agentd's own MCP socket (`McpClient::stream_raw`), so
//! request signing, the client identity and the endpoint's vetting apply to
//! it as to every other request. Each push binding costs one connection and
//! one thread.

use super::pool::Done;
use super::wire::{self, Frame};
use super::{BACKOFF_MAX_MS, EXHAUSTED_MAX_MS, State, Taken, backoff, jitter, tame};
use crate::config::settings::events::{Delivery, Replay};
use crate::mcp::client::McpClient;
use crate::runtime::events::Event;
use crate::runtime::reactor::Runtime;
use crate::state::now_ms;
use ::mcp::http::StreamHandle;
use ::mcp::rmcp_client::StreamEnd;
use serde_json::{Value, json};
use std::ops::ControlFlow;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// How long the dial, the request and the wait for the stream's first frame
/// may take.
const OPEN_BOUND: Duration = Duration::from_secs(30);
/// How long an open stream may stay silent before its server's pace is known:
/// three times the 30 s heartbeat ceiling [MCPG RFC 0031 §2.1].
const IDLE_DEFAULT: Duration = Duration::from_secs(90);
/// The least idle bound a fast heartbeat sets.
const IDLE_FLOOR: Duration = Duration::from_secs(1);
/// Silence past this many of the server's heartbeat intervals is a stream
/// gone.
const IDLE_HEARTBEATS: u32 = 3;
/// Frames one stream may have handed to the loop and not yet taken: past it
/// the reader waits, and TCP holds the server back — the flow control the
/// extension relies on.
const QUEUE: usize = 256;

/// What a stream's reader hands the loop.
#[derive(Debug)]
pub enum Pushed {
    Frame(Frame),
    Ended(StreamEnd),
}

/// The frames one stream has queued for the loop and the loop has not taken.
#[derive(Default)]
pub(crate) struct Credit {
    queued: Mutex<usize>,
    taken: Condvar,
}

impl Credit {
    /// Room for one more frame: wait while [`QUEUE`] are queued. `false` once
    /// the stream is closed — its frames are no longer wanted.
    fn reserve(&self, handle: &StreamHandle) -> bool {
        let mut q = self.queued.lock().unwrap_or_else(|e| e.into_inner());
        while *q >= QUEUE {
            if handle.closed() {
                return false;
            }
            q = self
                .taken
                .wait_timeout(q, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if handle.closed() {
            return false;
        }
        *q += 1;
        true
    }

    /// The loop took one.
    fn release(&self) {
        let mut q = self.queued.lock().unwrap_or_else(|e| e.into_inner());
        *q = q.saturating_sub(1);
        self.taken.notify_one();
    }
}

/// A binding's open stream, as the loop holds it. Dropped, it is closed.
pub(crate) struct Open {
    handle: Arc<StreamHandle>,
    credit: Arc<Credit>,
    pub(super) generation: u64,
    /// The connection it runs on: a re-dial replaces it, and the stream with
    /// it.
    client: Arc<McpClient>,
    /// Its `active` came: the server took the subscription.
    pub(super) acked: bool,
    /// The arguments it was opened with: once the binding would send others
    /// ([`super::Binding::sent_arguments`]) it is opened again with them.
    arguments: serde_json::Map<String, Value>,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.handle.close();
    }
}

/// Open one stream on a thread of its own; every frame and its end go to the
/// loop as [`Done::Push`].
fn open(
    key: String,
    generation: u64,
    client: Arc<McpClient>,
    params: serde_json::Map<String, Value>,
    events: Sender<Event>,
) -> Open {
    let arguments = params
        .get("arguments")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let handle = Arc::new(StreamHandle::new(IDLE_DEFAULT));
    let credit = Arc::new(Credit::default());
    let (h, c, conn) = (
        Arc::clone(&handle),
        Arc::clone(&credit),
        Arc::clone(&client),
    );
    let _ = std::thread::Builder::new()
        .name("agentd-events-push".into())
        .spawn(move || read(&key, generation, &conn, params, &events, &h, &c));
    Open {
        handle,
        credit,
        generation,
        client,
        acked: false,
        arguments,
    }
}

/// The reader: frames to the loop, in order, until the stream ends. The idle
/// bound follows the server's own heartbeat interval once two heartbeats
/// have shown it.
fn read(
    key: &str,
    generation: u64,
    client: &McpClient,
    params: serde_json::Map<String, Value>,
    events: &Sender<Event>,
    handle: &StreamHandle,
    credit: &Credit,
) {
    let send = |what: Pushed| {
        events
            .send(Event::McpEvents(Box::new(Done::Push {
                key: key.to_string(),
                generation,
                what,
            })))
            .is_ok()
    };
    let mut beat: Option<Instant> = None;
    let end = client.stream_raw(
        wire::method::STREAM,
        params,
        wire::client_extensions(),
        OPEN_BOUND,
        handle,
        |n| {
            let Some(frame) = wire::frame(&n) else {
                return ControlFlow::Continue(()); // not a frame this reads
            };
            if matches!(frame, Frame::Heartbeat { .. }) {
                if let Some(prev) = beat {
                    handle.set_idle(idle_bound(prev.elapsed()));
                }
                beat = Some(Instant::now());
            }
            let last = matches!(frame, Frame::Terminated(_));
            if !credit.reserve(handle) || !send(Pushed::Frame(frame)) || last {
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        },
    );
    let _ = send(Pushed::Ended(end));
}

/// The idle bound a heartbeat interval of `interval` sets.
fn idle_bound(interval: Duration) -> Duration {
    (interval * IDLE_HEARTBEATS).clamp(IDLE_FLOOR, IDLE_DEFAULT)
}

/// Why a stream that could not be opened moves the binding to its next
/// listed mode: a definitive refusal of push itself — the method unknown,
/// push not offered for the type, push not allowed this principal, or the
/// request refused outright. `None`: anything else, which push retries.
fn refusal_moves_on(end: &StreamEnd) -> Option<&'static str> {
    match end {
        StreamEnd::Refused(e) => match wire::code::normalize(e.code) {
            wire::code::METHOD_NOT_FOUND => Some("method_not_found"),
            wire::code::FORBIDDEN => Some("forbidden"),
            wire::code::UNSUPPORTED if !schema_changed(e) => Some("unsupported"),
            _ => None,
        },
        StreamEnd::Status(400 | 403 | 404 | 405) => Some("refused"),
        _ => None,
    }
}

fn schema_changed(e: &::mcp::rpc::RpcError) -> bool {
    e.data
        .as_ref()
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
        == Some("schema_changed")
}

/// How a stream's end is named in `mcp.events.stream.closed`.
fn end_reason(end: &StreamEnd) -> &'static str {
    match end {
        StreamEnd::Answered(_) => "server_closed",
        StreamEnd::Refused(_) => "refused",
        StreamEnd::Stopped => "stopped",
        StreamEnd::Silent => "silent",
        StreamEnd::Status(_) => "status",
        StreamEnd::Failed(_) => "dropped",
    }
}

impl Runtime {
    /// Every pass of the loop: close the streams that should not be open —
    /// the binding moved off push, was refused or is being decided again, its
    /// connection was replaced, or pressure or the store stops intake — and
    /// open each push binding's stream that is due and not held back.
    pub(super) fn events_push_due(&mut self, now: u64) {
        let pressure = self.pressure.refusal(false);
        let degraded = self.durable.is_degraded();
        let keys: Vec<String> = self.mcp_events.bindings.keys().cloned().collect();
        for key in keys {
            let Some(b) = self.mcp_events.bindings.get_mut(&key) else {
                continue;
            };
            if let Some(o) = &b.push {
                let connected = self
                    .mcp
                    .get(&b.server)
                    .is_some_and(|c| Arc::ptr_eq(c, &o.client));
                let why = if b.mode != Some(Delivery::Push) {
                    Some("mode_changed")
                } else if !connected {
                    Some("redialed")
                } else if b.state == State::Active && o.arguments != b.sent_arguments() {
                    // Another holder came or went, or the instruction moved:
                    // opened again at once, from the cursor it reached.
                    Some("arguments_changed")
                } else {
                    match b.state {
                        State::Discovering => Some("rediscovering"),
                        State::Refused(_) => Some("refused"),
                        State::Paused(_) => Some("paused"),
                        State::Active if pressure.is_some() => Some("pressure"),
                        State::Active if degraded => Some("store_degraded"),
                        State::Active => None,
                    }
                };
                match why {
                    None => continue,
                    Some(cause @ ("pressure" | "store_degraded")) => {
                        self.events_push_close(&key, cause, None);
                        self.events_pause(&key, cause);
                        continue;
                    }
                    Some(why) => self.events_push_close(&key, why, None),
                }
            }
            let Some(b) = self.mcp_events.bindings.get_mut(&key) else {
                continue;
            };
            if !matches!(b.state, State::Active | State::Paused(_))
                || b.mode != Some(Delivery::Push)
                || b.push.is_some()
                || now < b.due_at
            {
                continue;
            }
            b.refill(now);
            let held = if pressure.is_some() {
                Some("pressure")
            } else if degraded {
                Some("store_degraded")
            } else if b.rate.is_some() && b.tokens < 1.0 {
                b.due_at = b.refill_at(now);
                Some("rate")
            } else {
                None
            };
            if let Some(cause) = held {
                self.events_pause(&key, cause);
                continue;
            }
            // The records first: a stream from a cursor the store could not
            // give back would move past whatever it held (§5.1 rule 2).
            if !b.loaded && self.events_load_records(&key).is_err() {
                self.events_pause(&key, "store_degraded");
                continue;
            }
            self.events_push_open(&key, now);
        }
    }

    fn events_push_open(&mut self, key: &str, now: u64) {
        let Some(client) = self
            .mcp_events
            .bindings
            .get(key)
            .and_then(|b| self.mcp.get(&b.server).cloned())
        else {
            return; // not connected: a reload or a re-dial brings it
        };
        let server = self.mcp_events.bindings[key].server.clone();
        // A stream is one request toward the service's `rate`.
        if crate::mcp::pace::take(&server).is_err() {
            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                b.due_at = now + 1_000;
            }
            return;
        }
        self.mcp_events.next_generation += 1;
        let generation = self.mcp_events.next_generation;
        let events = self.events_tx.clone();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if let State::Paused(cause) = b.state {
            b.state = State::Active;
            self.log.info(
                "mcp.events.resumed",
                json!({"server": b.server, "event_type": b.name, "cause": cause}),
            );
        }
        let max_age = match b.replay {
            Replay::Within(d) => Some(d.as_millis() as u64),
            Replay::None => None,
        };
        let params =
            wire::stream_params(&b.name, &b.sent_arguments(), b.cursor.as_deref(), max_age);
        b.push = Some(open(key.to_string(), generation, client, params, events));
    }

    /// Close `key`'s stream, said with `reason`; its frames still queued are
    /// dropped as they come.
    pub(super) fn events_push_close(&mut self, key: &str, reason: &str, extra: Option<Value>) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        let Some(open) = b.push.take() else {
            return;
        };
        let mut line = json!({"server": b.server, "event_type": b.name, "reason": reason,
                              "acked": open.acked});
        if let Some(Value::Object(m)) = extra {
            line.as_object_mut().expect("an object").extend(m);
        }
        drop(open);
        self.log.info("mcp.events.stream.closed", line);
    }

    /// A frame, or the end, of a stream.
    pub(super) fn events_pushed(&mut self, key: &str, generation: u64, what: Pushed) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return; // removed while its stream was open
        };
        // Another stream's — one closed since — is not this binding's now:
        // applied, it could move the cursor past what was never appended.
        let Some(open) = b.push.as_mut().filter(|o| o.generation == generation) else {
            return;
        };
        match what {
            Pushed::Frame(frame) => {
                open.credit.release();
                // Refused or decided again since it was sent: taken from it
                // nothing more, and the stream is closed at the next pass.
                if b.state == State::Active {
                    self.events_push_frame(key, frame);
                }
            }
            Pushed::Ended(end) => self.events_push_ended(key, end),
        }
    }

    fn events_push_frame(&mut self, key: &str, frame: Frame) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        match frame {
            Frame::Active { cursor, truncated } => {
                let open = b.push.as_mut().expect("held by the caller");
                if !open.acked {
                    open.acked = true;
                    b.failures = 0;
                    self.log.info(
                        "mcp.events.stream.opened",
                        json!({"server": server, "event_type": name,
                               "cursor_present": b.cursor.is_some(), "truncated": truncated}),
                    );
                }
                if truncated {
                    crate::obs::metrics::record_mcp_event_gap(&server, &name, "truncated");
                    self.log.warn(
                        "mcp.events.gap",
                        json!({"server": server, "event_type": name, "cause": "truncated"}),
                    );
                }
                self.events_push_commit(key, false, cursor, now);
            }
            Frame::Event(raw) => {
                let cursor = raw
                    .get("cursor")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string);
                if cursor
                    .as_ref()
                    .is_some_and(|c| c.len() > super::bridge::MAX_CURSOR_BYTES)
                {
                    return self.events_push_retry(
                        key,
                        "oversize_cursor",
                        json!({"err": format!("a cursor over {} bytes", super::bridge::MAX_CURSOR_BYTES)}),
                    );
                }
                b.refill(now);
                let appended = match self.events_take(key, &raw, "push", now, None) {
                    Taken::Appended => true,
                    Taken::Duplicate | Taken::Rejected(_) => false,
                    Taken::Rate => {
                        self.events_push_close(key, "rate", None);
                        if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                            b.due_at = b.refill_at(now);
                        }
                        return self.events_pause_for(key, "rate", None);
                    }
                    Taken::Refused(why) => {
                        let cause = if self.pressure.refusal(false).is_some() {
                            "pressure"
                        } else {
                            "store_degraded"
                        };
                        self.events_push_close(key, cause, None);
                        return self.events_pause_for(key, cause, Some(&why));
                    }
                };
                self.events_push_commit(key, appended, cursor, now);
            }
            Frame::Heartbeat { cursor } => self.events_push_commit(key, false, cursor, now),
            Frame::Error(e) => self.log.warn(
                "mcp.events.error",
                json!({"server": server, "event_type": name,
                       "code": wire::code::normalize(e.code), "err": tame(&e.message)}),
            ),
            Frame::Terminated(e) => {
                self.log.error(
                    "mcp.events.terminated",
                    json!({"server": server, "event_type": name,
                           "code": wire::code::normalize(e.code), "mode": "push"}),
                );
                self.events_push_close(key, "terminated", None);
                self.events_push_refused(key, e, true);
            }
        }
    }

    /// Keep what a frame brought: what was appended is flushed before the
    /// cursor moves — and before anything says it is kept — then the dedup set
    /// and the cursor's record are written (§5.1 rule 2). A flush that failed
    /// closes the stream at the cursor already kept.
    fn events_push_commit(&mut self, key: &str, appended: bool, cursor: Option<String>, now: u64) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let moving = cursor.is_some() && cursor != b.cursor;
        if (appended || moving)
            && let Err(e) = self.durable.flush(true)
        {
            self.events_push_close(key, "store_degraded", None);
            return self.events_pause_for(
                key,
                "store_degraded",
                Some(&format!("the stream head was not saved: {e}")),
            );
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.seen.expire(now);
        if appended {
            let _ = self.durable.put(
                crate::state::Kind::Memory,
                &b.seen_key(),
                json!({"entries": b.seen.to_json()}),
                None,
            );
        }
        if moving {
            b.cursor = cursor;
        }
        if moving || !b.recorded {
            self.events_record(key);
        }
    }

    fn events_push_ended(&mut self, key: &str, end: StreamEnd) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let acked = b.push.as_ref().is_some_and(|o| o.acked);
        crate::obs::metrics::record_mcp_events_request(
            &b.server,
            wire::method::STREAM,
            match &end {
                StreamEnd::Answered(_) | StreamEnd::Stopped => "ok",
                StreamEnd::Refused(_) => "refused",
                _ => "failed",
            },
        );
        if !acked && let Some(reason) = refusal_moves_on(&end) {
            let detail = match &end {
                StreamEnd::Refused(e) => json!({"code": e.code, "err": tame(&e.message)}),
                StreamEnd::Status(s) => json!({"status": s}),
                _ => json!({}),
            };
            self.events_push_close(key, end_reason(&end), Some(detail));
            return self.events_push_fallback(key, reason);
        }
        match end {
            StreamEnd::Refused(e) => {
                let extra = json!({"code": e.code, "err": tame(&e.message)});
                self.events_push_close(key, "refused", Some(extra));
                self.events_push_refused(key, e, false);
            }
            StreamEnd::Status(s) => self.events_push_retry(key, "status", json!({"status": s})),
            StreamEnd::Failed(m) => {
                self.events_push_retry(key, "dropped", json!({"err": tame(&m)}))
            }
            other => self.events_push_retry(key, end_reason(&other), json!({})),
        }
    }

    /// The server refused the stream, or ended it with `terminated`: what
    /// the code says follows (§5.13), as for a poll.
    fn events_push_refused(&mut self, key: &str, e: ::mcp::rpc::RpcError, terminated: bool) {
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        match wire::code::normalize(e.code) {
            // The type is gone: list again, which refuses it if so.
            wire::code::NOT_FOUND => self.events_rediscover(key),
            wire::code::FORBIDDEN => self.events_refuse(key, "forbidden", &e.message),
            wire::code::INVALID_PARAMS => self.events_refuse(key, "invalid_arguments", &e.message),
            wire::code::UNSUPPORTED if schema_changed(&e) => {
                if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                    b.cursor = None;
                    let _ = self
                        .durable
                        .delete(crate::state::Kind::Memory, &b.cursor_key());
                }
                self.log.warn(
                    "mcp.events.gap",
                    json!({"server": server, "event_type": name, "cause": "schema_changed"}),
                );
                crate::obs::metrics::record_mcp_event_gap(&server, &name, "schema_changed");
                self.events_rediscover(key);
            }
            // The type stopped offering push.
            wire::code::UNSUPPORTED => self.events_push_fallback(key, "unsupported"),
            wire::code::METHOD_NOT_FOUND if terminated => {
                self.events_push_fallback(key, "method_not_found")
            }
            code => {
                let max = if code == wire::code::RESOURCE_EXHAUSTED {
                    EXHAUSTED_MAX_MS
                } else {
                    BACKOFF_MAX_MS
                };
                self.events_push_backoff(key, max);
            }
        }
    }

    /// A failure that may pass: close what is open, and open again from the
    /// cursor kept, after a backoff with jitter. Push is never given up on
    /// for one.
    fn events_push_retry(&mut self, key: &str, reason: &str, extra: Value) {
        let wait = self.events_push_backoff(key, BACKOFF_MAX_MS);
        let mut extra = extra;
        extra["retry_ms"] = json!(wait);
        if self
            .mcp_events
            .bindings
            .get(key)
            .is_some_and(|b| b.push.is_some())
        {
            self.events_push_close(key, reason, Some(extra));
        }
    }

    /// The next stream waits the backoff the failures in a row have earned
    /// (from 1 s, doubling, to `max`, with jitter); how long.
    fn events_push_backoff(&mut self, key: &str, max: u64) -> u64 {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return 0;
        };
        let wait = backoff(b.failures, max);
        b.failures += 1;
        b.due_at = now_ms() + wait;
        wait
    }

    /// The server will not stream this binding: on to the operator's next
    /// listed mode the server offers, said once — and push is passed over
    /// until a reload or a re-dial decides afresh. None left: refused.
    fn events_push_fallback(&mut self, key: &str, reason: &'static str) {
        self.events_push_close(key, "fallback", None);
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if !b.demoted.contains(&Delivery::Push) {
            b.demoted.push(Delivery::Push);
        }
        let offered: Vec<String> = self
            .mcp_events
            .listings
            .get(&b.server)
            .and_then(|l| l.descriptors.as_ref())
            .and_then(|d| d.get(&b.name))
            .map(|d| d.delivery.clone())
            .unwrap_or_default();
        let next = b
            .delivery
            .iter()
            .copied()
            .filter(|m| !b.demoted.contains(m))
            .find(|m| offered.iter().any(|o| o == m.as_str()));
        let Some(to) = next else {
            let detail = format!(
                "events/stream was refused ({reason}) and no other listed mode is offered: the operator lists {:?}, the server offers {offered:?}",
                b.delivery.iter().map(|m| m.as_str()).collect::<Vec<_>>()
            );
            return self.events_refuse(key, "not_supported", &detail);
        };
        b.mode = Some(to);
        b.said_mode = Some(to);
        b.armed = false;
        b.failures = 0;
        b.due_at = now_ms() + jitter(super::BACKOFF_MIN_MS) / 10;
        self.log.warn(
            "mcp.events.delivery",
            json!({"server": b.server, "event_type": b.name, "binding": b.key,
                   "mode": to.as_str(), "reason": reason}),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_bound_is_three_heartbeats_within_its_floor_and_ceiling() {
        assert_eq!(idle_bound(Duration::from_secs(20)), Duration::from_secs(60));
        assert_eq!(idle_bound(Duration::from_millis(100)), IDLE_FLOOR);
        assert_eq!(idle_bound(Duration::from_secs(45)), IDLE_DEFAULT);
    }

    #[test]
    fn only_a_refusal_of_push_itself_moves_to_the_next_mode() {
        let refused = |code: i64, data: Option<Value>| {
            StreamEnd::Refused(::mcp::rpc::RpcError {
                code,
                message: "no".into(),
                data,
            })
        };
        assert_eq!(
            refusal_moves_on(&refused(-32601, None)),
            Some("method_not_found")
        );
        // Either numbering.
        for code in [-32014, -32026] {
            assert_eq!(
                refusal_moves_on(&refused(
                    code,
                    Some(json!({"feature": "deliveryMode", "value": "push"}))
                )),
                Some("unsupported")
            );
            assert_eq!(
                refusal_moves_on(&refused(code, Some(json!({"reason": "schema_changed"})))),
                None,
                "a schema change is the type's, not the mode's"
            );
        }
        for code in [-32012, -32024] {
            assert_eq!(refusal_moves_on(&refused(code, None)), Some("forbidden"));
        }
        assert_eq!(refusal_moves_on(&StreamEnd::Status(404)), Some("refused"));
        // What may pass is push's to retry.
        for end in [
            refused(-32013, None),
            refused(-32025, None),
            refused(-32603, None),
            StreamEnd::Status(503),
            StreamEnd::Silent,
            StreamEnd::Failed("reset".into()),
            StreamEnd::Answered(json!({})),
        ] {
            assert_eq!(refusal_moves_on(&end), None, "{end:?}");
        }
    }

    #[test]
    fn a_reader_waits_for_room_and_stops_waiting_once_closed() {
        let credit = Arc::new(Credit::default());
        let handle = Arc::new(StreamHandle::new(IDLE_DEFAULT));
        for _ in 0..QUEUE {
            assert!(credit.reserve(&handle));
        }
        // Full: the next waits until the loop takes one.
        let (c, h) = (Arc::clone(&credit), Arc::clone(&handle));
        let waiter = std::thread::spawn(move || c.reserve(&h));
        std::thread::sleep(Duration::from_millis(150));
        assert!(!waiter.is_finished(), "held back while the queue is full");
        credit.release();
        assert!(waiter.join().unwrap());
        // Closed: no more room is waited for.
        let (c, h) = (Arc::clone(&credit), Arc::clone(&handle));
        let waiter = std::thread::spawn(move || c.reserve(&h));
        std::thread::sleep(Duration::from_millis(50));
        handle.close();
        assert!(!waiter.join().unwrap());
    }
}
