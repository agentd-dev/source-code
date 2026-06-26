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
}

impl Route {
    pub fn new(pattern: &str, disposition: Disposition, debounce: Duration) -> Route {
        Route { matcher: Match::parse(pattern), disposition, debounce }
    }

    /// Whether this route is an exact match for `uri` — used for dynamic
    /// self-subscribe dedup + unsubscribe (RFC 0008 §self-scheduling).
    fn is_exact(&self, uri: &str) -> bool {
        matches!(&self.matcher, Match::Exact(u) if u == uri)
    }
}

/// A debounced, ready-to-act delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub uri: String,
    pub disposition: Disposition,
}

/// The reactive router. Holds the route table and the per-URI debounce state.
pub struct Router {
    routes: Vec<Route>,
    /// uri → (fire-at, disposition). Newest-wins coalesce: the entry is set on
    /// the first event of a burst and fires once after the debounce.
    pending: HashMap<String, (Instant, Disposition)>,
    dropped: u64,
}

impl Router {
    pub fn new(routes: Vec<Route>) -> Router {
        Router { routes, pending: HashMap::new(), dropped: 0 }
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
        let armed = self.best_match(uri).map(|r| (now + r.debounce, r.disposition.clone()));
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
        self.pending.values().map(|(at, _)| *at).min()
    }

    /// Drain every delivery whose debounce has elapsed by `now`.
    pub fn due(&mut self, now: Instant) -> Vec<Delivery> {
        let ready: Vec<String> =
            self.pending.iter().filter(|(_, (at, _))| *at <= now).map(|(uri, _)| uri.clone()).collect();
        ready
            .into_iter()
            .map(|uri| {
                let (_, disposition) = self.pending.remove(&uri).expect("present");
                Delivery { uri, disposition }
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
            Route::new("file:///inbox.json", Disposition::Continue("s1".into()), ms(0)),
        ];
        let r = Router::new(routes);
        // exact owner
        assert_eq!(
            r.best_match("file:///inbox.json").unwrap().disposition,
            Disposition::Continue("s1".into())
        );
        // glob owner for anything else
        assert_eq!(r.best_match("file:///other.json").unwrap().disposition, Disposition::Spawn);
    }

    #[test]
    fn longest_prefix_glob_wins() {
        let routes = vec![
            Route::new("db://*", Disposition::Spawn, ms(0)),
            Route::new("db://orders/*", Disposition::Continue("orders".into()), ms(0)),
        ];
        let r = Router::new(routes);
        assert_eq!(
            r.best_match("db://orders/42").unwrap().disposition,
            Disposition::Continue("orders".into())
        );
        assert_eq!(r.best_match("db://users/7").unwrap().disposition, Disposition::Spawn);
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
        let mut r = Router::new(vec![Route::new("file:///in.json", Disposition::Spawn, ms(100))]);
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
}
