// SPDX-License-Identifier: Apache-2.0
//! Reactive routing — the rule that turns MCP resource updates into runs.
//! RFC 0008 §reactive-routing.
//!
//! This is the pure heart of the signature feature: "listen for MCP resources,
//! act when they appear." A **subscription** is `(server, resource_uri)`; a
//! **route** binds a match (exact URI or a `prefix*` glob) to a disposition —
//! `spawn` a fresh agent per event, or `continue` a warm session — with a
//! debounce. The rules (RFC 0008):
//!
//! - **Exactly one owner.** Every `updated{uri}` matches exactly one route:
//!   an exact match wins; otherwise the longest-prefix glob. No fan-out. No
//!   match → dropped + counted.
//! - **spawn-vs-continue is a route property**, deterministic, not a per-event
//!   guess.
//! - **Debounce + newest-wins coalesce.** A burst on one URI collapses to a
//!   single delivery; the agent re-reads current state anyway (notify-then-read,
//!   RFC 0004), so coalescing is safe and bounds the queue by distinct URIs.
//!
//! No I/O here — the reactor calls `on_updated`/`due` and acts on the
//! deliveries (`resources/read` then spawn-or-continue). `mode.rs`/`mcp` wire
//! it to real notifications.

use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Where a matched update goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// Start a fresh root agent per event (stateless reaction).
    Spawn,
    /// Deliver into one warm session, in order (stateful reaction).
    Continue(String),
}

/// A structured-content condition on a reactive delivery (pivot Phase 5.2). The
/// router matches a resource URI; a `Condition` ADDITIONALLY gates on the
/// resource's parsed-JSON CONTENT — evaluated post-read (notify-then-read, so the
/// router itself stays content-free) — so a route/subscription fires only when the
/// resource reaches a wanted state. This is the predicate half of the reactive
/// gaps: it turns "fire on any update" into "fire on the update I'm waiting for",
/// and underpins the `await_resource` in-turn wait (a one-shot conditional
/// continue). RFC 0008 §reactive-routing.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// RFC 6901 JSON Pointer into the resource content (`""` = the whole document).
    pointer: String,
    op: CondOp,
}

/// The comparison a [`Condition`] applies at its pointer.
#[derive(Debug, Clone, PartialEq)]
enum CondOp {
    /// The pointer resolves to a present, non-null value.
    Exists,
    /// Deep-equals the given JSON value.
    Eq(Value),
    /// Does NOT deep-equal the given JSON value.
    Ne(Value),
    /// Numeric strictly-greater-than (the target parses as a number).
    Gt(f64),
    /// Numeric strictly-less-than.
    Lt(f64),
    /// A string that contains the substring, or an array containing the string.
    Contains(String),
}

impl Condition {
    /// Parse a condition from the self-tool arg shape
    /// `{"pointer": "/status", "op": "eq", "value": "ready"}`. `op` defaults to
    /// `exists`; `pointer` defaults to `""` (the whole document). Returns a
    /// human-readable error the caller surfaces as a refused tool-result (never a
    /// crash) — a malformed predicate must not arm a route.
    pub fn from_json(v: &Value) -> Result<Condition, String> {
        let pointer = v
            .get("pointer")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // A JSON Pointer is empty (whole doc) or starts with '/'.
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(format!(
                "condition 'pointer' must be a JSON Pointer (empty or starting with '/'): {pointer:?}"
            ));
        }
        let op = v.get("op").and_then(Value::as_str).unwrap_or("exists");
        let value = v.get("value").cloned();
        let op = match op {
            "exists" => CondOp::Exists,
            "eq" => CondOp::Eq(value.ok_or_else(|| "condition op 'eq' requires 'value'".to_string())?),
            "ne" => CondOp::Ne(value.ok_or_else(|| "condition op 'ne' requires 'value'".to_string())?),
            "gt" => CondOp::Gt(
                value
                    .and_then(|x| x.as_f64())
                    .ok_or_else(|| "condition op 'gt' requires a numeric 'value'".to_string())?,
            ),
            "lt" => CondOp::Lt(
                value
                    .and_then(|x| x.as_f64())
                    .ok_or_else(|| "condition op 'lt' requires a numeric 'value'".to_string())?,
            ),
            "contains" => CondOp::Contains(
                value
                    .and_then(|x| x.as_str().map(str::to_string))
                    .ok_or_else(|| "condition op 'contains' requires a string 'value'".to_string())?,
            ),
            other => return Err(format!("unknown condition op: {other:?}")),
        };
        Ok(Condition { pointer, op })
    }

    /// Evaluate against a resource's parsed-JSON content. A pointer that does not
    /// resolve is a non-match for every op (except a deliberate `ne`, which holds
    /// when the wanted value is simply absent). Never panics.
    pub fn eval(&self, content: &Value) -> bool {
        let at = content.pointer(&self.pointer);
        match &self.op {
            CondOp::Exists => at.is_some_and(|v| !v.is_null()),
            CondOp::Eq(want) => at == Some(want),
            CondOp::Ne(want) => at != Some(want),
            CondOp::Gt(n) => at.and_then(Value::as_f64).is_some_and(|x| x > *n),
            CondOp::Lt(n) => at.and_then(Value::as_f64).is_some_and(|x| x < *n),
            CondOp::Contains(s) => match at {
                Some(Value::String(hay)) => hay.contains(s.as_str()),
                Some(Value::Array(arr)) => arr.iter().any(|e| e.as_str() == Some(s.as_str())),
                _ => false,
            },
        }
    }
}

/// How a route matches resource URIs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Match {
    Exact(String),
    /// Prefix glob (the pattern without its trailing `*`).
    Prefix(String),
}

impl Match {
    fn parse(pattern: &str) -> Match {
        match pattern.strip_suffix('*') {
            Some(prefix) => Match::Prefix(prefix.to_string()),
            None => Match::Exact(pattern.to_string()),
        }
    }

    fn matches(&self, uri: &str) -> bool {
        match self {
            Match::Exact(u) => u == uri,
            Match::Prefix(p) => uri.starts_with(p.as_str()),
        }
    }

    /// Specificity for tie-breaking: exact beats any glob; among globs the
    /// longest prefix wins. Returns `(is_exact, prefix_len)`.
    fn specificity(&self) -> (bool, usize) {
        match self {
            Match::Exact(u) => (true, u.len()),
            Match::Prefix(p) => (false, p.len()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    matcher: Match,
    disposition: Disposition,
    debounce: Duration,
    /// Optional content predicate (pivot Phase 5.2): the route fires only when the
    /// updated resource's content satisfies it. `None` = fire on any update (the
    /// v1 behaviour of every config `--subscribe`/`--continue` route).
    condition: Option<Condition>,
}

impl Route {
    pub fn new(pattern: &str, disposition: Disposition, debounce: Duration) -> Route {
        Route {
            matcher: Match::parse(pattern),
            disposition,
            debounce,
            condition: None,
        }
    }

    /// Attach a content predicate (builder). A self-subscribe/await with a
    /// condition arms the route this way; config routes leave it `None`.
    pub fn with_condition(mut self, condition: Option<Condition>) -> Route {
        self.condition = condition;
        self
    }

    /// Whether this route is an exact match for `uri` — used for dynamic
    /// self-subscribe dedup + unsubscribe (RFC 0008 §self-scheduling).
    fn is_exact(&self, uri: &str) -> bool {
        matches!(&self.matcher, Match::Exact(u) if u == uri)
    }
}

/// A debounced, ready-to-act delivery. Carries the route's optional content
/// predicate ([`Condition`]) so the reactor can gate the reaction on the resource
/// content it reads (notify-then-read). Not `Eq` (a `Condition` may hold a JSON
/// number, which is not `Eq`).
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub uri: String,
    pub disposition: Disposition,
    pub condition: Option<Condition>,
}

/// The reactive router. Holds the route table and the per-URI debounce state.
pub struct Router {
    routes: Vec<Route>,
    /// uri → (fire-at, disposition, condition). Newest-wins coalesce: the entry is
    /// set on the first event of a burst and fires once after the debounce. The
    /// owning route is deterministic per URI (exact-else-longest-glob), so the
    /// coalesced condition is stable across the burst.
    pending: HashMap<String, (Instant, Disposition, Option<Condition>)>,
    dropped: u64,
}

impl Router {
    pub fn new(routes: Vec<Route>) -> Router {
        Router {
            routes,
            pending: HashMap::new(),
            dropped: 0,
        }
    }

    /// Number of unmatched updates dropped (a no-route counter; `on_updated`
    /// returns false on a miss so the caller can log/count it). Currently only
    /// readable via this accessor in tests; not yet surfaced in a log or metric.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether an exact route for `uri` already exists (dedup a self-subscribe).
    pub fn has_exact(&self, uri: &str) -> bool {
        self.routes.iter().any(|r| r.is_exact(uri))
    }

    /// Add a route at runtime — an agent self-subscribing to a resource
    /// (RFC 0008). The caller dedups via [`Router::has_exact`].
    pub fn add_route(&mut self, route: Route) {
        self.routes.push(route);
    }

    /// Remove every exact route for `uri` (a self-unsubscribe) plus any pending
    /// delivery for it. Returns the number of routes removed.
    pub fn remove_exact(&mut self, uri: &str) -> usize {
        let before = self.routes.len();
        self.routes.retain(|r| !r.is_exact(uri));
        self.pending.remove(uri);
        before - self.routes.len()
    }

    /// Record a `notifications/resources/updated` for `uri`. Returns true if a
    /// route owns it (armed/coalesced), false if it was dropped (no route).
    pub fn on_updated(&mut self, uri: &str, now: Instant) -> bool {
        // Resolve the owner into owned values first, so the immutable borrow of
        // the route table is released before we mutate `pending`.
        let armed = self
            .best_match(uri)
            .map(|r| (now + r.debounce, r.disposition.clone(), r.condition.clone()));
        match armed {
            Some(entry) => {
                // Coalesce: arm only on the first event of a burst; later events
                // in the window fold into the same pending delivery.
                self.pending.entry(uri.to_string()).or_insert(entry);
                true
            }
            None => {
                self.dropped += 1;
                false
            }
        }
    }

    /// The earliest pending fire time. Intended for a timer-armed reactor; the
    /// shipped `run_reactive` instead polls on a fixed `TICK` and calls
    /// `due(now)` each tick, so this is currently used only in unit tests.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|(at, ..)| *at).min()
    }

    /// Number of distinct URIs currently armed/coalesced and not yet fired — the
    /// reactive backlog this replica sees (RFC 0019 §5.1, the `agentd_pending_events`
    /// scaling signal). Cheap (a `HashMap::len`); read each reactive tick.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The fire-at `Instant` of the oldest (earliest-armed) pending delivery, for
    /// the reaction-lag signal (RFC 0019 §5.1). Each entry's stored instant is
    /// `armed_at + debounce`, so the minimum is the oldest pending item's deadline;
    /// the caller derives a `lag_ms` from it. `None` when nothing is pending.
    pub fn oldest_pending(&self) -> Option<Instant> {
        self.pending.values().map(|(at, ..)| *at).min()
    }

    /// Drain every delivery whose debounce has elapsed by `now`.
    pub fn due(&mut self, now: Instant) -> Vec<Delivery> {
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (at, ..))| *at <= now)
            .map(|(uri, _)| uri.clone())
            .collect();
        ready
            .into_iter()
            .map(|uri| {
                let (_, disposition, condition) = self.pending.remove(&uri).expect("present");
                Delivery {
                    uri,
                    disposition,
                    condition,
                }
            })
            .collect()
    }

    /// The exactly-one-owner choice: exact match else longest-prefix glob.
    fn best_match(&self, uri: &str) -> Option<&Route> {
        self.routes
            .iter()
            .filter(|r| r.matcher.matches(uri))
            .max_by_key(|r| r.matcher.specificity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn exact_beats_glob() {
        let routes = vec![
            Route::new("file:///*", Disposition::Spawn, ms(0)),
            Route::new(
                "file:///inbox.json",
                Disposition::Continue("s1".into()),
                ms(0),
            ),
        ];
        let r = Router::new(routes);
        // exact owner
        assert_eq!(
            r.best_match("file:///inbox.json").unwrap().disposition,
            Disposition::Continue("s1".into())
        );
        // glob owner for anything else
        assert_eq!(
            r.best_match("file:///other.json").unwrap().disposition,
            Disposition::Spawn
        );
    }

    #[test]
    fn longest_prefix_glob_wins() {
        let routes = vec![
            Route::new("db://*", Disposition::Spawn, ms(0)),
            Route::new(
                "db://orders/*",
                Disposition::Continue("orders".into()),
                ms(0),
            ),
        ];
        let r = Router::new(routes);
        assert_eq!(
            r.best_match("db://orders/42").unwrap().disposition,
            Disposition::Continue("orders".into())
        );
        assert_eq!(
            r.best_match("db://users/7").unwrap().disposition,
            Disposition::Spawn
        );
    }

    #[test]
    fn no_match_is_dropped_and_counted() {
        let mut r = Router::new(vec![Route::new("file:///*", Disposition::Spawn, ms(0))]);
        let t0 = Instant::now();
        assert!(!r.on_updated("http://x", t0));
        assert_eq!(r.dropped(), 1);
        assert!(r.due(t0).is_empty());
    }

    #[test]
    fn debounce_coalesces_a_burst_to_one_delivery() {
        let mut r = Router::new(vec![Route::new(
            "file:///in.json",
            Disposition::Spawn,
            ms(100),
        )]);
        let t0 = Instant::now();
        assert!(r.on_updated("file:///in.json", t0));
        assert!(r.on_updated("file:///in.json", t0 + ms(10))); // within window → coalesced
        assert!(r.on_updated("file:///in.json", t0 + ms(50)));
        // not due before the debounce of the *first* event
        assert!(r.due(t0 + ms(60)).is_empty());
        // exactly one delivery after the debounce
        let due = r.due(t0 + ms(120));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].uri, "file:///in.json");
    }

    #[test]
    fn distinct_uris_deliver_separately() {
        let mut r = Router::new(vec![Route::new("db://*", Disposition::Spawn, ms(10))]);
        let t0 = Instant::now();
        r.on_updated("db://a", t0);
        r.on_updated("db://b", t0);
        let due = r.due(t0 + ms(20));
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn next_deadline_is_earliest() {
        let mut r = Router::new(vec![Route::new("db://*", Disposition::Spawn, ms(100))]);
        let t0 = Instant::now();
        r.on_updated("db://a", t0);
        r.on_updated("db://b", t0 + ms(30));
        assert_eq!(r.next_deadline(), Some(t0 + ms(100))); // from the first event
    }

    #[test]
    fn dynamic_self_subscribe_routes_then_unsubscribes() {
        let mut r = Router::new(vec![]);
        let t0 = Instant::now();
        // an unrouted update is dropped
        assert!(!r.on_updated("file:///watch.json", t0));
        assert_eq!(r.dropped(), 1);

        // a self-subscribe adds an exact route (deduped)
        assert!(!r.has_exact("file:///watch.json"));
        r.add_route(Route::new("file:///watch.json", Disposition::Spawn, ms(0)));
        assert!(r.has_exact("file:///watch.json"));

        // now the same update is owned + delivers
        assert!(r.on_updated("file:///watch.json", t0));
        assert_eq!(r.due(t0).len(), 1);

        // a self-unsubscribe removes the route + any pending; later updates drop
        r.on_updated("file:///watch.json", t0); // arm a pending
        assert_eq!(r.remove_exact("file:///watch.json"), 1);
        assert!(!r.has_exact("file:///watch.json"));
        assert!(r.due(t0).is_empty(), "pending dropped on unsubscribe");
        assert!(!r.on_updated("file:///watch.json", t0));
    }

    // ── condition predicates (pivot Phase 5.2) ───────────────────────────────

    use serde_json::json;

    #[test]
    fn condition_from_json_parses_each_op_and_rejects_bad_input() {
        // Defaults: no op → exists, no pointer → whole doc.
        let c = Condition::from_json(&json!({})).unwrap();
        assert!(c.eval(&json!({"any": 1})), "exists on the whole doc");
        // Each op round-trips through eval.
        let eq = Condition::from_json(&json!({"pointer": "/status", "op": "eq", "value": "ready"}))
            .unwrap();
        assert!(eq.eval(&json!({"status": "ready"})));
        assert!(!eq.eval(&json!({"status": "working"})));
        let gt =
            Condition::from_json(&json!({"pointer": "/n", "op": "gt", "value": 10})).unwrap();
        assert!(gt.eval(&json!({"n": 11})));
        assert!(!gt.eval(&json!({"n": 10})));
        let contains =
            Condition::from_json(&json!({"pointer": "/tags", "op": "contains", "value": "urgent"}))
                .unwrap();
        assert!(contains.eval(&json!({"tags": ["low", "urgent"]})));
        assert!(!contains.eval(&json!({"tags": ["low"]})));
        // Malformed: non-pointer, missing value, unknown op, non-numeric gt.
        assert!(Condition::from_json(&json!({"pointer": "status"})).is_err());
        assert!(Condition::from_json(&json!({"op": "eq"})).is_err());
        assert!(Condition::from_json(&json!({"op": "nope"})).is_err());
        assert!(Condition::from_json(&json!({"op": "gt", "value": "x"})).is_err());
    }

    #[test]
    fn condition_eval_on_missing_pointer_is_a_non_match() {
        let c = Condition::from_json(&json!({"pointer": "/missing", "op": "eq", "value": 1}))
            .unwrap();
        assert!(!c.eval(&json!({"present": 1})), "absent pointer never matches eq");
        // ne holds when the wanted value is simply absent.
        let ne = Condition::from_json(&json!({"pointer": "/missing", "op": "ne", "value": 1}))
            .unwrap();
        assert!(ne.eval(&json!({"present": 1})));
    }

    #[test]
    fn a_conditional_route_carries_its_condition_into_the_delivery() {
        // The router matches by URI (content-free); the condition rides along on the
        // Delivery for the reactor to evaluate post-read.
        let cond = Condition::from_json(&json!({"pointer": "/ready", "op": "eq", "value": true}))
            .unwrap();
        let route = Route::new("file:///w.json", Disposition::Spawn, ms(0))
            .with_condition(Some(cond.clone()));
        let mut r = Router::new(vec![route]);
        let t0 = Instant::now();
        assert!(r.on_updated("file:///w.json", t0));
        let due = r.due(t0);
        assert_eq!(due.len(), 1);
        assert_eq!(
            due[0].condition,
            Some(cond),
            "the delivery carries the route's condition"
        );
        // An unconditional route delivers with no condition (v1 fire-on-any).
        let mut r2 = Router::new(vec![Route::new("file:///w.json", Disposition::Spawn, ms(0))]);
        assert!(r2.on_updated("file:///w.json", t0));
        assert_eq!(r2.due(t0)[0].condition, None);
    }
}
