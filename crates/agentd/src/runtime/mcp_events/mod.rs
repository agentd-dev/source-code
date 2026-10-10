// SPDX-License-Identifier: AGPL-3.0-only
//! MCP Events (RFC 0045, SEP-3415): the bindings that bring a server's event
//! occurrences into declared streams.
//!
//! Each binding is discovered against its server's `events/list`, then polled
//! on the events worker pool; every occurrence becomes one stream event
//! (§5.4), and the binding's cursor never moves past an occurrence that is
//! not durably appended (§5.1 rule 2). A binding the server cannot honour is
//! refused, loudly, with its reason; one that fails in a way that may pass
//! backs off with jitter and keeps its cursor (§5.13).

pub mod bridge;
pub mod pool;
pub mod wire;

use super::reactor::Runtime;
use crate::config::settings::events::{Delivery, EventInto, Replay};
use crate::state::{Kind, now_ms};
use bridge::{Checked, Seen};
use pool::{Done, Failure, Job, Pool};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use wire::Descriptor;

/// The most occurrences one poll asks for.
const MAX_EVENTS: u32 = 50;
/// Polls in a row a binding makes while the server says `hasMore` and each
/// page held something new; past it, the next poll waits its interval.
const MAX_DRAIN: u32 = 10;
/// The floor under a server's `nextPollMs` [SEP L300].
const POLL_FLOOR_MS: u64 = 1_000;
/// The interval when a server names none.
const POLL_DEFAULT_MS: u64 = 30_000;
/// The longest a server's `nextPollMs` may hold a binding off: past it, a
/// server could park a binding for good.
const POLL_CEILING_MS: u64 = 15 * 60 * 1000;
/// How often a server's event types are listed again; the only way, beside
/// `list_changed`, a changed descriptor is seen (§5.3).
const RELIST_MS: u64 = 15 * 60 * 1000;
/// The backoff after a failure that may pass: from 1 s, doubling, to 5 min.
const BACKOFF_MIN_MS: u64 = 1_000;
const BACKOFF_MAX_MS: u64 = 5 * 60 * 1000;
/// The longest a server's `ResourceExhausted` holds a binding off.
const EXHAUSTED_MAX_MS: u64 = 15 * 60 * 1000;
/// How long one `events/*` request may take.
const REQUEST_BOUND: Duration = Duration::from_secs(30);

/// Where a binding is (§5.14).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum State {
    /// Waiting for its server's listing.
    Discovering,
    /// Polling.
    Active,
    /// Not polled while `cause` holds: `pressure`, `rate`, `store_degraded`.
    Paused(&'static str),
    /// Refused until the config, the connection or the server's listing
    /// changes: `reason` is one of §5.12's.
    Refused(&'static str),
}

impl State {
    fn name(&self) -> &'static str {
        match self {
            State::Discovering => "discovering",
            State::Active => "active",
            State::Paused(_) => "paused",
            State::Refused(_) => "refused",
        }
    }
}

/// Why a page stopped before its last occurrence.
enum Stop {
    /// The binding's `rate` ran dry.
    Rate,
    /// An append the stream refused (pressure), or a flush that failed.
    Refused(String),
}

/// One binding, live.
pub(crate) struct Binding {
    pub(crate) key: String,
    pub(crate) server: String,
    pub(crate) name: String,
    pub(crate) arguments: Map<String, Value>,
    delivery: Vec<Delivery>,
    into: EventInto,
    replay: Replay,
    /// The intake ceiling: `(burst, window seconds)`, and what is left of it.
    rate: Option<(u32, f64)>,
    tokens: f64,
    refilled_at: u64,
    pub(crate) state: State,
    mode: Option<Delivery>,
    cursor: Option<String>,
    /// The records were read (once per life).
    loaded: bool,
    seen: Seen,
    due_at: u64,
    in_flight: bool,
    drain: u32,
    failures: u32,
    payload_schema: Option<Value>,
    /// A payload failed its schema: list the server again.
    schema_relist: bool,
    /// That re-list was asked for already; it is asked for once per
    /// payload schema, not on every page that fails it.
    schema_relisted: bool,
    /// Bumped at every poll dispatched: an answer for an older one — a
    /// binding removed and declared again while its poll was out — is not
    /// this binding's.
    generation: u64,
}

impl Binding {
    fn cursor_key(&self) -> String {
        format!("_mcpev/{}/cursor", self.key)
    }

    fn seen_key(&self) -> String {
        format!("_mcpev/{}/seen", self.key)
    }

    /// The tokens the intake ceiling holds now.
    fn refill(&mut self, now: u64) {
        if let Some((burst, per_s)) = self.rate {
            let rate = f64::from(burst) / per_s.max(0.001);
            let elapsed = now.saturating_sub(self.refilled_at) as f64 / 1000.0;
            self.tokens = (self.tokens + elapsed * rate).min(f64::from(burst));
            self.refilled_at = now;
        }
    }

    /// When the ceiling next holds a whole token.
    fn refill_at(&self, now: u64) -> u64 {
        match self.rate {
            Some((burst, per_s)) => {
                let rate = f64::from(burst) / per_s.max(0.001);
                now + ((1.0 - self.tokens).max(0.0) / rate * 1000.0).ceil() as u64
            }
            None => now,
        }
    }
}

/// One server's listing.
#[derive(Default)]
struct Listing {
    descriptors: Option<BTreeMap<String, Descriptor>>,
    in_flight: bool,
    due_at: u64,
    failures: u32,
    /// Why the server cannot be used for Events at all, while it holds: the
    /// reason, and what to say.
    refused: Option<(&'static str, String)>,
}

/// The loop's MCP Events state.
#[derive(Default)]
pub(crate) struct Events {
    pub(crate) bindings: BTreeMap<String, Binding>,
    listings: BTreeMap<String, Listing>,
    pool: Option<Pool>,
    /// The next poll generation; never reused within a life.
    next_generation: u64,
}

impl Events {
    /// `(active, paused, refused)` bindings, for the status counters.
    pub(crate) fn counts(&self) -> (u64, u64, u64) {
        let mut c = (0, 0, 0);
        for b in self.bindings.values() {
            match b.state {
                State::Active => c.0 += 1,
                State::Paused(_) => c.1 += 1,
                State::Refused(_) => c.2 += 1,
                State::Discovering => {}
            }
        }
        c
    }
}

/// `base` with up to ±10% jitter: polling clients MUST jitter (2026-07-28
/// caching), so many bindings on one server do not poll in step.
fn jitter(base: u64) -> u64 {
    let mut b = [0u8; 8];
    if crate::sec::random::fill(&mut b).is_err() {
        return base;
    }
    let spread = base / 10;
    if spread == 0 {
        return base;
    }
    let offset = u64::from_le_bytes(b) % spread.saturating_mul(2).saturating_add(1);
    (base - spread).saturating_add(offset)
}

/// The backoff after `failures` failures in a row, capped at `max`.
fn backoff(failures: u32, max: u64) -> u64 {
    jitter(
        BACKOFF_MIN_MS
            .saturating_mul(1u64 << failures.min(20))
            .min(max),
    )
}

/// Strip control characters and cap at 200 bytes: server text in a log line
/// must not be able to drive an operator's terminal (§5.12).
fn tame(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.len() > 200 {
        let mut cut = 200;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

impl Runtime {
    /// Bring the live bindings in line with the settings: every binding the
    /// config declares, keyed by its identity. A binding that is unchanged
    /// keeps its state; a removed one stops and its records go; a changed
    /// `into`, `replay` or `rate` applies in place. `redialed` names the
    /// servers whose connection was replaced, whose event types are listed
    /// again. Called at boot and on every reload (§5.7).
    pub(crate) fn events_sync(&mut self, reason: &str, redialed: &[String]) {
        let now = now_ms();
        let mut wanted: BTreeMap<String, ()> = BTreeMap::new();
        let declared: Vec<(String, crate::config::settings::events::EventBinding)> = self
            .settings
            .mcp
            .servers
            .iter()
            .flat_map(|s| s.events.iter().map(move |b| (s.name.clone(), b.clone())))
            .collect();
        for (server, b) in declared {
            let key = bridge::binding_hash(&server, &b.name, &b.arguments);
            wanted.insert(key.clone(), ());
            let rate = b
                .rate
                .as_deref()
                .and_then(|r| crate::supervisor::tree::parse_rate(r).ok());
            match self.mcp_events.bindings.get_mut(&key) {
                Some(live) => {
                    // The same subscription: what changed applies in place.
                    if live.delivery != b.delivery {
                        live.delivery = b.delivery.clone();
                        live.mode = None;
                        live.state = State::Discovering;
                    }
                    live.into = b.into.clone();
                    live.replay = b.replay;
                    if live.rate != rate {
                        live.rate = rate;
                        live.tokens = rate.map_or(0.0, |(burst, _)| f64::from(burst));
                    }
                }
                None => {
                    self.mcp_events.bindings.insert(
                        key.clone(),
                        Binding {
                            key,
                            server: server.clone(),
                            name: b.name.clone(),
                            arguments: b.arguments.clone(),
                            delivery: b.delivery.clone(),
                            into: b.into.clone(),
                            replay: b.replay,
                            rate,
                            tokens: rate.map_or(0.0, |(burst, _)| f64::from(burst)),
                            refilled_at: now,
                            state: State::Discovering,
                            mode: None,
                            cursor: None,
                            loaded: false,
                            seen: Seen::default(),
                            due_at: now,
                            in_flight: false,
                            drain: 0,
                            failures: 0,
                            payload_schema: None,
                            schema_relist: false,
                            schema_relisted: false,
                            generation: 0,
                        },
                    );
                }
            }
        }
        let removed: Vec<String> = self
            .mcp_events
            .bindings
            .keys()
            .filter(|k| !wanted.contains_key(*k))
            .cloned()
            .collect();
        for key in removed {
            if let Some(b) = self.mcp_events.bindings.remove(&key) {
                let _ = self.durable.delete(Kind::Memory, &b.cursor_key());
                let _ = self.durable.delete(Kind::Memory, &b.seen_key());
                self.log.info(
                    "mcp.events.removed",
                    json!({"server": b.server, "event_type": b.name, "reason": reason}),
                );
            }
        }
        // Servers no binding names any more, and the ones dialed afresh.
        let servers: std::collections::BTreeSet<String> = self
            .mcp_events
            .bindings
            .values()
            .map(|b| b.server.clone())
            .collect();
        self.mcp_events
            .listings
            .retain(|s, _| servers.contains(s) && !redialed.contains(s));
        // A reload or a re-dial is when a refusal is looked at again (a new
        // credential, a server changed); a listing alone clears only what it
        // decided (`events_evaluate`).
        for b in self.mcp_events.bindings.values_mut() {
            if (redialed.contains(&b.server) || matches!(b.state, State::Refused(_)))
                && !b.in_flight
            {
                b.state = State::Discovering;
                b.mode = None;
            }
        }
        if self.mcp_events.bindings.is_empty() {
            crate::obs::metrics::set_event_subscriptions(Vec::new());
        }
        if !self.mcp_events.bindings.is_empty() && self.mcp_events.pool.is_none() {
            self.mcp_events.pool = Some(Pool::start(self.events_tx.clone()));
        }
        // A binding added (or whose modes changed) on a server already listed
        // is held against that listing now, not at the next re-list — and one
        // on a server already refused is refused with it, not left waiting.
        let pending: Vec<(String, Option<(&'static str, String)>)> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.state == State::Discovering)
            .filter_map(|b| {
                let l = self.mcp_events.listings.get(&b.server)?;
                match &l.refused {
                    Some(r) => Some((b.key.clone(), Some(r.clone()))),
                    None => l.descriptors.is_some().then(|| (b.key.clone(), None)),
                }
            })
            .collect();
        for (key, refused) in pending {
            match refused {
                Some((reason, detail)) => self.events_refuse(&key, reason, &detail),
                None => self.events_evaluate(&key),
            }
        }
    }

    /// Every pass of the loop: list what needs listing, poll what is due.
    pub(crate) fn events_tick(&mut self) {
        if self.mcp_events.bindings.is_empty() {
            return;
        }
        let now = now_ms();
        self.events_list_due(now);
        self.events_poll_due(now);
        let active = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.state == State::Active)
            .count() as u64;
        crate::obs::metrics::set_event_subscriptions(vec![("poll".to_string(), active)]);
    }

    /// Ask each server with a binding for its event types, when it has none
    /// yet or its re-list is due — once the connection shows it can be asked.
    fn events_list_due(&mut self, now: u64) {
        let servers: std::collections::BTreeSet<String> = self
            .mcp_events
            .bindings
            .values()
            .map(|b| b.server.clone())
            .collect();
        for server in servers {
            let schema_relist = self
                .mcp_events
                .bindings
                .values()
                .any(|b| b.server == server && b.schema_relist);
            let listing = self.mcp_events.listings.entry(server.clone()).or_default();
            if listing.in_flight || (now < listing.due_at && !schema_relist) {
                continue;
            }
            let Some(client) = self.mcp.get(&server).cloned() else {
                continue; // not connected: a reload or a re-dial brings it
            };
            // The extension is defined only from 2026-07-28 [SEP L37].
            let revision = client.protocol_version();
            let refusal = if revision.as_deref() != Some(wire::REVISION) {
                Some("revision")
            } else {
                let caps = json!({
                    "extensions": client.capability("extensions"),
                    "events": client.capability("events"),
                });
                wire::declared(&caps).is_none().then_some("no_capability")
            };
            if let Some(reason) = refusal {
                let detail = match reason {
                    "revision" => format!(
                        "the connection reached {}; MCP Events is defined only from {} — no events/* request is sent",
                        revision.as_deref().unwrap_or("no revision"),
                        wire::REVISION
                    ),
                    _ => "the server does not declare the io.modelcontextprotocol/events extension — no events/* request is sent".to_string(),
                };
                let listing = self.mcp_events.listings.entry(server.clone()).or_default();
                listing.refused = Some((reason, detail.clone()));
                listing.due_at = u64::MAX;
                self.events_refuse_server(&server, reason, &detail);
                continue;
            }
            let listing = self.mcp_events.listings.entry(server.clone()).or_default();
            listing.refused = None;
            listing.in_flight = true;
            for b in self.mcp_events.bindings.values_mut() {
                if b.server == server {
                    b.schema_relist = false;
                }
            }
            if let Some(pool) = &self.mcp_events.pool {
                pool.submit(Job::List {
                    server,
                    client,
                    bound: REQUEST_BOUND,
                });
            }
        }
    }

    /// Refuse every binding on `server` for `reason`.
    fn events_refuse_server(&mut self, server: &str, reason: &'static str, detail: &str) {
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.server == server)
            .map(|b| b.key.clone())
            .collect();
        for key in keys {
            self.events_refuse(&key, reason, detail);
        }
    }

    /// Refuse one binding, said once per refusal.
    fn events_refuse(&mut self, key: &str, reason: &'static str, detail: &str) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.state == State::Refused(reason) {
            return;
        }
        b.state = State::Refused(reason);
        b.mode = None;
        self.log.error(
            "mcp.events.refused",
            json!({"server": b.server, "event_type": b.name, "reason": reason, "detail": tame(detail)}),
        );
    }

    /// Dispatch every poll that is due and not held back.
    fn events_poll_due(&mut self, now: u64) {
        let pressure = self.pressure.refusal(false);
        let degraded = self.durable.is_degraded();
        let keys: Vec<String> = self.mcp_events.bindings.keys().cloned().collect();
        for key in keys {
            let held = {
                let Some(b) = self.mcp_events.bindings.get_mut(&key) else {
                    continue;
                };
                if !matches!(b.state, State::Active | State::Paused(_)) || b.in_flight {
                    continue;
                }
                if now < b.due_at {
                    continue;
                }
                b.refill(now);
                if pressure.is_some() {
                    Some("pressure")
                } else if degraded {
                    Some("store_degraded")
                } else if b.rate.is_some() && b.tokens < 1.0 {
                    b.due_at = b.refill_at(now);
                    Some("rate")
                } else {
                    None
                }
            };
            // The records are read before the first poll: a poll from a
            // cursor the store could not give back would move past whatever
            // it held, with nothing said (§5.1 rule 2).
            let unread = self
                .mcp_events
                .bindings
                .get(&key)
                .is_some_and(|b| !b.loaded);
            if held.is_none() && unread && self.events_load_records(&key).is_err() {
                self.events_pause(&key, "store_degraded");
                continue;
            }
            match held {
                Some(cause) => self.events_pause(&key, cause),
                None => self.events_dispatch_poll(&key, now),
            }
        }
    }

    fn events_pause(&mut self, key: &str, cause: &'static str) {
        self.events_pause_for(key, cause, None);
    }

    /// Pause a binding for `cause`, said once per pause; held at its cursor.
    fn events_pause_for(&mut self, key: &str, cause: &'static str, detail: Option<&str>) {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.state != State::Paused(cause) {
            b.state = State::Paused(cause);
            let mut line = json!({"server": b.server, "event_type": b.name, "cause": cause});
            if let Some(d) = detail {
                line["detail"] = json!(tame(d));
            }
            self.log.warn("mcp.events.paused", line);
        }
        if cause != "rate" {
            b.failures += 1;
            b.due_at = now_ms() + backoff(b.failures, BACKOFF_MAX_MS).min(30_000);
        }
    }

    fn events_dispatch_poll(&mut self, key: &str, now: u64) {
        let Some(client) = self
            .mcp_events
            .bindings
            .get(key)
            .and_then(|b| self.mcp.get(&b.server).cloned())
        else {
            return;
        };
        // The service's `rate` models the upstream's limit, which tool calls
        // and `events/*` both spend: a dry bucket defers the poll.
        let server = self.mcp_events.bindings[key].server.clone();
        if crate::mcp::pace::take(&server).is_err() {
            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                b.due_at = now + 1_000;
            }
            return;
        }
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
        let max_events = match b.rate {
            Some(_) => (b.tokens.floor() as u32).clamp(1, MAX_EVENTS),
            None => MAX_EVENTS,
        };
        let max_age = match b.replay {
            Replay::Within(d) => Some(d.as_millis() as u64),
            Replay::None => None,
        };
        let params = wire::poll_params(
            &b.name,
            &b.arguments,
            b.cursor.as_deref(),
            max_age,
            max_events,
        );
        b.in_flight = true;
        self.mcp_events.next_generation += 1;
        let generation = self.mcp_events.next_generation;
        if let Some(b) = self.mcp_events.bindings.get_mut(key) {
            b.generation = generation;
        }
        if let Some(pool) = &self.mcp_events.pool {
            pool.submit(Job::Poll {
                key: key.to_string(),
                generation,
                client,
                params,
                bound: REQUEST_BOUND,
            });
        }
    }

    /// An answer from the pool.
    pub(crate) fn on_events_done(&mut self, done: Done) {
        let outcome = |failed: Option<&Failure>| match failed {
            None => "ok",
            Some(Failure::Rpc(_)) => "refused",
            Some(_) => "failed",
        };
        match done {
            Done::List { server, result } => {
                crate::obs::metrics::record_mcp_events_request(
                    &server,
                    wire::method::LIST,
                    outcome(result.as_ref().err()),
                );
                self.events_listed(&server, result)
            }
            Done::Poll {
                key,
                generation,
                result,
            } => {
                let Some(b) = self.mcp_events.bindings.get(&key) else {
                    return; // removed while its poll was out
                };
                crate::obs::metrics::record_mcp_events_request(
                    &b.server,
                    wire::method::POLL,
                    outcome(result.as_ref().err()),
                );
                // An answer for a poll this binding did not send — it was
                // removed and declared again meanwhile — is dropped: applied,
                // it could move the cursor past what was never appended.
                if b.generation != generation {
                    return;
                }
                self.events_polled(&key, result)
            }
        }
    }

    fn events_listed(&mut self, server: &str, result: Result<Vec<Descriptor>, Failure>) {
        let now = now_ms();
        let Some(listing) = self.mcp_events.listings.get_mut(server) else {
            return; // no binding names the server any more
        };
        listing.in_flight = false;
        let descriptors = match result {
            Ok(d) => d,
            Err(Failure::Rpc(e)) if e.code == wire::code::METHOD_NOT_FOUND => {
                listing.due_at = now + jitter(RELIST_MS);
                self.events_refuse_server(
                    server,
                    "method_not_found",
                    &format!("events/list: {} — the server (or a gateway in front of it) does not serve MCP Events", e.message),
                );
                return;
            }
            Err(Failure::TooLarge) => {
                listing.due_at = now + jitter(RELIST_MS);
                self.events_refuse_server(server, "list_too_large", &Failure::TooLarge.to_string());
                return;
            }
            Err(e) => {
                listing.failures += 1;
                let wait = backoff(listing.failures, BACKOFF_MAX_MS);
                listing.due_at = now + wait;
                self.log.warn(
                    "mcp.events.list.fail",
                    json!({"server": server, "err": tame(&e.to_string()), "retry_ms": wait}),
                );
                return;
            }
        };
        listing.failures = 0;
        listing.due_at = now + jitter(RELIST_MS);
        let types = descriptors.len();
        listing.descriptors = Some(
            descriptors
                .into_iter()
                .map(|d| (d.name.clone(), d))
                .collect(),
        );
        self.log.info(
            "mcp.events.listed",
            json!({"server": server, "types": types}),
        );
        // A listing clears what a listing decides; what the server said to a
        // poll — not this credential, not this mode — waits for a reload or a
        // re-dial.
        let keys: Vec<String> = self
            .mcp_events
            .bindings
            .values()
            .filter(|b| b.server == server)
            .filter(|b| !matches!(b.state, State::Refused("forbidden" | "not_supported")))
            .map(|b| b.key.clone())
            .collect();
        for key in keys {
            self.events_evaluate(&key);
        }
    }

    /// Hold one binding against its server's listing (§5.3 step 4): the type
    /// listed, a mode both sides accept, arguments its `inputSchema` takes.
    /// Re-run at every listing, so a refusal clears once the server changes.
    fn events_evaluate(&mut self, key: &str) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get(key) else {
            return;
        };
        let Some(descriptors) = self
            .mcp_events
            .listings
            .get(&b.server)
            .and_then(|l| l.descriptors.as_ref())
        else {
            return;
        };
        let Some(d) = descriptors.get(&b.name).cloned() else {
            return self.events_refuse(
                key,
                "unknown_event",
                "the server's events/list does not name this event type",
            );
        };
        // The first mode the operator listed that the server offers and this
        // build delivers. Webhook arrives with P3, push with P4.
        let mode = b
            .delivery
            .iter()
            .copied()
            .find(|m| *m == Delivery::Poll && d.delivery.iter().any(|x| x == m.as_str()));
        let Some(mode) = mode else {
            let detail = format!(
                "the operator lists {:?}, the server offers {:?}",
                b.delivery.iter().map(|m| m.as_str()).collect::<Vec<_>>(),
                d.delivery
            );
            return self.events_refuse(key, "no_common_mode", &detail);
        };
        if let Some(schema) = &d.input_schema
            && let Err(errs) =
                crate::jsonschema::validate(schema, &Value::Object(b.arguments.clone()))
        {
            let detail = crate::jsonschema::explain(&errs);
            return self.events_refuse(key, "invalid_arguments", &detail);
        }
        let was = b.state.clone();
        if !b.loaded {
            // A read that fails is retried before the first poll.
            let _ = self.events_load_records(key);
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        if b.payload_schema != d.payload_schema {
            b.schema_relisted = false;
        }
        b.payload_schema = d.payload_schema.clone();
        b.mode = Some(mode);
        if matches!(was, State::Discovering | State::Refused(_)) {
            b.state = State::Active;
            // Re-armed after a failure (a type the server said it lost, a
            // schema it changed): the backoff the failure earned still holds.
            b.due_at = if b.failures > 0 {
                now + backoff(b.failures, BACKOFF_MAX_MS)
            } else {
                now
            };
            self.log.info(
                "mcp.events.armed",
                json!({"server": b.server, "event_type": b.name, "mode": mode.as_str(),
                       "cursor_present": b.cursor.is_some()}),
            );
        }
    }

    /// Read a binding's records, once per life: its cursor and its dedup set.
    /// Only a record that is not there starts the binding from "now" [SEP
    /// L690]; a read that failed is an error, and the binding is not polled
    /// until a read succeeds.
    fn events_load_records(&mut self, key: &str) -> Result<(), ()> {
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return Ok(());
        };
        let record = self.durable.get(Kind::Memory, &b.cursor_key());
        let seen = self.durable.get(Kind::Memory, &b.seen_key());
        let (Ok(record), Ok(seen)) = (record, seen) else {
            return Err(());
        };
        b.loaded = true;
        if let Some(seen) = seen {
            b.seen = Seen::from_json(&seen.state["entries"]);
        }
        let Some(record) = record.map(|e| e.state) else {
            return Ok(()); // a new binding starts from "now"
        };
        b.cursor = match b.replay {
            // `replay: none` always re-arms from now.
            Replay::None => None,
            Replay::Within(_) => record["cursor"].as_str().map(str::to_string),
        };
        if b.cursor.is_none() && matches!(b.replay, Replay::Within(_)) {
            // A type without replay, re-armed after downtime: whatever
            // happened in between is gone, and said so.
            self.log.warn(
                "mcp.events.gap",
                json!({"server": b.server, "event_type": b.name, "cause": "no_replay"}),
            );
            crate::obs::metrics::record_mcp_event_gap(&b.server, &b.name, "no_replay");
        }
        Ok(())
    }

    fn events_polled(&mut self, key: &str, result: Result<wire::PollPage, Failure>) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return; // removed while its poll was out
        };
        b.in_flight = false;
        if b.state != State::Active {
            return; // refused or paused while its poll was out
        }
        let page = match result {
            Ok(p) => p,
            Err(e) => return self.events_poll_failed(key, e),
        };
        if page
            .cursor
            .as_ref()
            .is_some_and(|c| c.len() > bridge::MAX_CURSOR_BYTES)
        {
            return self.events_poll_failed(
                key,
                Failure::Transport(format!(
                    "events/poll: a cursor over {} bytes",
                    bridge::MAX_CURSOR_BYTES
                )),
            );
        }
        let (server, name, stream, subject, schema) = (
            b.server.clone(),
            b.name.clone(),
            b.into.stream.clone(),
            b.into.subject.clone(),
            b.payload_schema.clone(),
        );
        let max_value_bytes = self.settings.store.max_value_bytes.map(|n| n as usize);
        let source = bridge::source(&server);
        let mut appended = 0u64;
        let mut fresh = 0u64;
        // Why the page stopped short, if it did: the cursor then holds, and
        // what was appended is in the dedup set, so the poll that resumes
        // appends the rest and nothing twice.
        let mut stopped: Option<Stop> = None;
        for raw in &page.events {
            match bridge::check(&server, &name, raw, schema.as_ref(), max_value_bytes) {
                Checked::Rejected { cause, event_id } => {
                    crate::obs::metrics::record_mcp_event(&server, &name, "poll", "rejected");
                    self.log.warn(
                        "mcp.events.rejected",
                        json!({"server": server, "event_type": name, "event_id": event_id.as_deref().map(tame), "cause": cause}),
                    );
                }
                Checked::Append {
                    id,
                    event_id,
                    data,
                    schema_ok,
                } => {
                    let seen_key = bridge::seen_key(&event_id);
                    let b = &self.mcp_events.bindings[key];
                    if b.seen.contains(&seen_key) {
                        crate::obs::metrics::record_mcp_event(&server, &name, "poll", "duplicate");
                        continue;
                    }
                    // The intake ceiling holds whatever the server sends: one
                    // that ignored `maxEvents` is stopped here, not trusted.
                    if b.rate.is_some() && b.tokens < 1.0 {
                        stopped = Some(Stop::Rate);
                        break;
                    }
                    fresh += 1;
                    let occurred_at = data["occurred_at"].as_str().and_then(bridge::timestamp_ms);
                    match self.append(&stream, &subject, None, data, &id, &source) {
                        Ok(_) => {
                            appended += 1;
                            crate::obs::metrics::record_mcp_event(
                                &server,
                                &name,
                                "poll",
                                if schema_ok {
                                    "appended"
                                } else {
                                    "schema_mismatch"
                                },
                            );
                            if let Some(at) = occurred_at {
                                crate::obs::metrics::set_mcp_event_lag(
                                    &server,
                                    &name,
                                    now.saturating_sub(at),
                                );
                            }
                            if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                                let evicted = b.seen.insert(seen_key, now);
                                crate::obs::metrics::record_mcp_events_dedup_evicted(
                                    &server, &name, evicted,
                                );
                                if b.rate.is_some() {
                                    b.tokens = (b.tokens - 1.0).max(0.0);
                                }
                                if !schema_ok && !b.schema_relisted {
                                    b.schema_relist = true;
                                    b.schema_relisted = true;
                                }
                            }
                            if !schema_ok {
                                self.log.warn(
                                    "mcp.events.schema_mismatch",
                                    json!({"server": server, "event_type": name, "event_id": tame(&event_id)}),
                                );
                            }
                        }
                        // Over the store's limit: no retry of it fits, so the
                        // cursor moves past it, said like an oversize one.
                        Err(crate::runtime::streams::AppendError::TooLarge(_)) => {
                            crate::obs::metrics::record_mcp_event(
                                &server, &name, "poll", "rejected",
                            );
                            self.log.warn(
                                "mcp.events.rejected",
                                json!({"server": server, "event_type": name, "event_id": tame(&event_id), "cause": "oversize"}),
                            );
                        }
                        Err(crate::runtime::streams::AppendError::Refused(e)) => {
                            crate::obs::metrics::record_mcp_event(
                                &server,
                                &name,
                                "poll",
                                "refused_pressure",
                            );
                            stopped = Some(Stop::Refused(e));
                            break;
                        }
                    }
                }
            }
        }
        // Nothing moves past an occurrence that is not durably kept: whenever
        // this pass appended or is about to move the cursor, the stream is
        // flushed first (§5.1 rule 2) — a page of duplicates after a flush
        // that failed included.
        let moving = stopped.is_none()
            && self
                .mcp_events
                .bindings
                .get(key)
                .is_some_and(|b| b.cursor != page.cursor);
        let flushed = if appended > 0 || moving {
            self.durable.flush(true).map(|_| ())
        } else {
            Ok(())
        };
        if let Err(e) = &flushed {
            stopped = Some(Stop::Refused(format!("the stream head was not saved: {e}")));
        }
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        b.seen.expire(now);
        if appended > 0 && flushed.is_ok() {
            let _ = self.durable.put(
                Kind::Memory,
                &b.seen_key(),
                json!({"entries": b.seen.to_json()}),
                None,
            );
        }
        match stopped {
            Some(Stop::Rate) => {
                b.due_at = b.refill_at(now);
                return self.events_pause_for(key, "rate", None);
            }
            Some(Stop::Refused(why)) => {
                let cause = if self.pressure.refusal(false).is_some() {
                    "pressure"
                } else {
                    "store_degraded"
                };
                return self.events_pause_for(key, cause, Some(&why));
            }
            None => {}
        }
        b.failures = 0;
        if page.truncated {
            crate::obs::metrics::record_mcp_event_gap(&b.server, &b.name, "truncated");
            self.log.warn(
                "mcp.events.gap",
                json!({"server": b.server, "event_type": b.name, "cause": "truncated"}),
            );
        }
        if page.cursor != b.cursor {
            b.cursor = page.cursor.clone();
            let record = json!({"mode": b.mode.map(Delivery::as_str), "cursor": b.cursor,
                                "state": b.state.name(), "since": now});
            let _ = self
                .durable
                .put(Kind::Memory, &b.cursor_key(), record, None);
        }
        // Drain a backlog at once while each page holds something new — but
        // a bounded number of times: a server that always says `hasMore` is
        // not a reason to poll in a loop (§5.6.1).
        b.due_at = if page.has_more && fresh > 0 && b.drain < MAX_DRAIN {
            b.drain += 1;
            now
        } else {
            b.drain = 0;
            now + jitter(
                page.next_poll_ms
                    .unwrap_or(POLL_DEFAULT_MS)
                    .clamp(POLL_FLOOR_MS, POLL_CEILING_MS),
            )
        };
    }

    /// A poll that failed: what the code says follows (§5.13).
    fn events_poll_failed(&mut self, key: &str, e: Failure) {
        let now = now_ms();
        let Some(b) = self.mcp_events.bindings.get_mut(key) else {
            return;
        };
        let (server, name) = (b.server.clone(), b.name.clone());
        // List the server again; the binding is armed against what it then
        // lists, after the backoff this failure earns — a server that keeps
        // answering the same way is not asked in a loop.
        let rediscover = |rt: &mut Runtime| {
            if let Some(l) = rt.mcp_events.listings.get_mut(&server) {
                l.due_at = 0;
                l.descriptors = None;
            }
            if let Some(b) = rt.mcp_events.bindings.get_mut(key) {
                b.state = State::Discovering;
                b.failures += 1;
            }
        };
        match &e {
            Failure::Rpc(r) => match r.code {
                // The type is gone: list again, which refuses it if so.
                wire::code::NOT_FOUND => rediscover(self),
                wire::code::FORBIDDEN => {
                    self.events_refuse(key, "forbidden", &r.message);
                }
                wire::code::INVALID_PARAMS => {
                    self.events_refuse(key, "invalid_arguments", &r.message);
                }
                wire::code::METHOD_NOT_FOUND => {
                    self.events_refuse(key, "method_not_found", &r.message);
                }
                // The type changed in place: a cursor minted under the old
                // schema is answered so again, so it is let go — what happened
                // in between is lost, and said so [SEP L221].
                wire::code::UNSUPPORTED
                    if r.data
                        .as_ref()
                        .and_then(|d| d.get("reason"))
                        .and_then(Value::as_str)
                        == Some("schema_changed") =>
                {
                    if let Some(b) = self.mcp_events.bindings.get_mut(key) {
                        b.cursor = None;
                        let _ = self.durable.delete(Kind::Memory, &b.cursor_key());
                    }
                    self.log.warn(
                        "mcp.events.gap",
                        json!({"server": server, "event_type": name, "cause": "schema_changed"}),
                    );
                    crate::obs::metrics::record_mcp_event_gap(&server, &name, "schema_changed");
                    rediscover(self);
                }
                wire::code::UNSUPPORTED => {
                    // A delivery mode the type does not offer; P2 has no
                    // other mode to move to.
                    self.events_refuse(key, "not_supported", &r.message);
                }
                code => {
                    let max = if code == wire::code::RESOURCE_EXHAUSTED {
                        EXHAUSTED_MAX_MS
                    } else {
                        BACKOFF_MAX_MS
                    };
                    let b = self.mcp_events.bindings.get_mut(key).expect("held above");
                    b.failures += 1;
                    let wait = backoff(b.failures, max);
                    b.due_at = now + wait;
                    let level_error = code == wire::code::HEADER_MISMATCH;
                    let line = json!({"server": server, "event_type": name, "code": code,
                                      "err": tame(&r.message), "retry_ms": wait});
                    if level_error {
                        self.log.error("mcp.events.poll.fail", line);
                    } else {
                        self.log.warn("mcp.events.poll.fail", line);
                    }
                }
            },
            Failure::Transport(m) => {
                b.failures += 1;
                let wait = backoff(b.failures, BACKOFF_MAX_MS);
                b.due_at = now + wait;
                self.log.warn(
                    "mcp.events.poll.fail",
                    json!({"server": server, "event_type": name, "err": tame(m), "retry_ms": wait}),
                );
            }
            Failure::TooLarge => {}
        }
    }
}
