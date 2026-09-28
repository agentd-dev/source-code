// SPDX-License-Identifier: AGPL-3.0-only
//! The events/v1 observation feed: the ring the loop pushes state changes
//! onto, and the section diff that finds the changes nobody pushed.

use super::FEED_RING;
use crate::runtime::reactor::Runtime;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

impl FeedVis {
    /// Whether the caller `principal` (an operator or not) may see what this
    /// tags. The ONE visibility rule: the feed filters a subscriber's events
    /// with it and the `status` op filters a caller's document with it, so
    /// what a principal can poll and what it can watch are the same thing and
    /// cannot drift apart.
    pub fn admits(&self, principal: &str, is_operator: bool) -> bool {
        match self {
            FeedVis::All => true,
            FeedVis::Operator | FeedVis::Owner(None) => is_operator,
            FeedVis::Owner(Some(owner)) => is_operator || owner == principal,
        }
    }
}

/// The global observation feed: a bounded ring of state-change events the loop
/// pushes and the `SubscribeToEvents` transport threads drain.
///
/// Events carry a monotonic `seq`, so a reconnecting client resumes from its
/// cursor (`fromSeq`) instead of replaying everything. The ring is bounded, so
/// an overrun evicts the oldest event; a client whose cursor predates the
/// window is told so and re-bootstraps with the `status` command rather than
/// silently missing state. Only the loop writes; the listener's stream tasks
/// only read.
pub struct SharedFeed {
    inner: Mutex<FeedInner>,
    /// `a2a.introspection.enabled` — gates the `audit` kind, and is what
    /// every `hello` tells a subscriber. Atomic because the operator can
    /// toggle it at runtime (`admin.set`, a reload).
    introspection: std::sync::atomic::AtomicBool,
}

struct FeedInner {
    seq: u64,
    /// Each event beside who may see it — kept out of the event itself, so
    /// nothing can serve an event with its tag still on.
    buf: std::collections::VecDeque<(FeedVis, Value)>,
    /// Events evicted to date (a subscriber whose cursor predates the window
    /// learns it fell behind).
    dropped: u64,
}

impl SharedFeed {
    pub fn new(introspection: bool) -> SharedFeed {
        SharedFeed {
            inner: Mutex::new(FeedInner {
                seq: 0,
                buf: std::collections::VecDeque::with_capacity(FEED_RING),
                dropped: 0,
            }),
            introspection: std::sync::atomic::AtomicBool::new(introspection),
        }
    }

    /// Whether introspection is on right now (runtime-togglable via
    /// `admin.set` and a reload): the `audit` kind flows only while it is.
    pub fn introspection(&self) -> bool {
        self.introspection
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_introspection(&self, on: bool) {
        self.introspection
            .store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one event; returns its `seq`.
    ///
    /// A debug build refuses, loudly, an event events/v1 does not define: a
    /// kind outside [`FeedKind::ALL`], data off its kind's schema, or a
    /// visibility the kind may not carry. Every e2e runs a debug daemon, so a
    /// push that drifted from the published contract stops the suite at the
    /// push rather than reaching a client that trusted the schema. A release
    /// build does not pay for the check.
    ///
    /// [`FeedKind::ALL`]: crate::runtime::surface::events::FeedKind::ALL
    pub fn push(&self, kind: &str, vis: FeedVis, data: Value) -> u64 {
        #[cfg(debug_assertions)]
        if let Err(why) = conforms(kind, &vis, &data) {
            panic!("feed push outside events/v1: {why}");
        }
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.seq += 1;
        let seq = g.seq;
        let ev = json!({"seq": seq, "ts": crate::state::now_ms(), "kind": kind, "data": data});
        if g.buf.len() == FEED_RING {
            g.buf.pop_front();
            g.dropped += 1;
        }
        g.buf.push_back((vis, ev));
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
        for (vis, ev) in g.buf.iter() {
            let seq = ev["seq"].as_u64().unwrap_or(0);
            if seq <= after {
                continue;
            }
            if out.len() >= max {
                break;
            }
            cursor = seq;
            if vis.admits(principal, is_operator) {
                out.push(ev.clone());
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
            .and_then(|(_, e)| e["seq"].as_u64())
            .unwrap_or(g.seq);
        (g.seq, oldest, g.dropped)
    }
}

/// Whether an event is one events/v1 defines: its kind, its data against the
/// kind's schema, and its visibility against who the kind may reach.
#[cfg(debug_assertions)]
fn conforms(kind: &str, vis: &FeedVis, data: &Value) -> Result<(), String> {
    use crate::runtime::surface::events::{Audience, check_event};
    let k = check_event(kind, data)?;
    let fits = match k.audience() {
        Audience::All => *vis == FeedVis::All,
        // An owned item with no owner is the operator's, and an item whose
        // owner cannot be found (activity for a vanished task) is too.
        Audience::Owner => matches!(vis, FeedVis::Owner(_) | FeedVis::Operator),
        Audience::Operator => *vis == FeedVis::Operator,
    };
    if fits {
        Ok(())
    } else {
        Err(format!(
            "{kind} pushed as {vis:?}, but it reaches {:?}",
            k.audience()
        ))
    }
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

impl Runtime {
    /// Push an event onto the observation feed (a no-op unless `a2a.events.enabled`).
    pub(crate) fn feed_push(&self, kind: &str, vis: FeedVis, data: Value) {
        if let Some(feed) = &self.a2a_feed {
            feed.push(kind, vis, data);
        }
    }

    /// Push a task's `task` event: `{task}`, the spec's `Task` and nothing
    /// beside it, seen by its owner (and operators).
    ///
    /// The task carries its recent history — the prompt that opened it, the
    /// answer to a gate — so a second client watching the same principal
    /// renders the transcript from the core object, and there is no separate
    /// event restating what somebody said. What it is linked to and who owns
    /// it are the task's annotations, not fields of an agentd envelope.
    pub(crate) fn feed_task(&self, t: &crate::a2a::tasks::Task) {
        let task = serde_json::to_value(crate::a2a::wire::task_for_feed(t)).unwrap_or(Value::Null);
        self.feed_push(
            "task",
            FeedVis::Owner(t.principal.clone()),
            json!({ "task": task }),
        );
    }

    /// The feed's **section diff**: one hook point in the loop
    /// that catches every state transition the explicit pushes don't — runs,
    /// conversations, subagents, OS children and the slim status — by
    /// fingerprinting each item and emitting an event when it changed (or
    /// left). Rate-limited to 4 Hz; fingerprints exclude the always-moving
    /// fields (`age_ms`, `uptime_ms`) so quiet state stays quiet.
    ///
    /// The items are the status document's own ([`Runtime::status_items`]),
    /// each tagged with who may see it, so a principal watches exactly what
    /// it can poll.
    pub(crate) fn feed_tick(&mut self) {
        if self.a2a_feed.is_none() {
            return;
        }
        if self.feed_last.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.feed_last = Instant::now();
        // Activity is pushed as it happens (`activity.rs`), filtered to the
        // changes worth a frame; diffing it here would re-send it on every
        // token the unit spends.
        let items = self
            .status_items()
            .into_iter()
            .filter(|i| i.kind != "activity");
        for (kind, vis, data) in diff_items(&mut self.feed_marks, items) {
            self.feed_push(kind, vis, data);
        }
    }

    /// Who may see the activity of a unit answering `task`: the task's owner
    /// (and operators), else operators only — for its updates and for its
    /// departure alike.
    pub(crate) fn activity_vis(&self, task: Option<&str>) -> FeedVis {
        match task
            .and_then(|t| self.tasks.get(t))
            .and_then(|t| t.principal.clone())
        {
            Some(p) => FeedVis::Owner(Some(p)),
            None => FeedVis::Operator,
        }
    }
}

/// The event a departed item of `kind` announces.
fn removed_kind(kind: &str) -> &'static str {
    match kind {
        "run" => "run.removed",
        "conversation" => "conversation.removed",
        "subagent" => "subagent.removed",
        "activity" => "activity.removed",
        _ => "child.removed",
    }
}

/// Diff `items` against `marks` (key → fingerprint, kind, visibility): the
/// events for every item that changed, then a `*.removed` for every item
/// that left, and the marks brought up to date.
///
/// A departure is seen by whoever could see the item: the mark remembers the
/// item's visibility for exactly this. Sending every departure to operators
/// alone left an owner's client holding a run or a conversation that was
/// gone — and sending it to everyone would name another principal's ids.
fn diff_items(
    marks: &mut std::collections::BTreeMap<String, (u64, &'static str, FeedVis)>,
    items: impl IntoIterator<Item = crate::runtime::reactor::StatusItem>,
) -> Vec<(&'static str, FeedVis, Value)> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut pushes: Vec<(&'static str, FeedVis, Value)> = Vec::new();
    for item in items {
        let print = fingerprint(&item.data);
        seen.insert(item.key.clone());
        // The visibility is compared too, so the mark a departure inherits
        // is always the item's latest.
        if marks.get(&item.key).map(|m| (m.0, &m.2)) != Some((print, &item.vis)) {
            marks.insert(item.key, (print, item.kind, item.vis.clone()));
            pushes.push((item.kind, item.vis, item.data));
        }
    }
    let gone: Vec<String> = marks
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    for key in gone {
        if let Some((_, kind, vis)) = marks.remove(&key)
            && let Some((_, id)) = key.split_once(':')
        {
            pushes.push((removed_kind(kind), vis, json!({"id": id})));
        }
    }
    pushes
}

#[cfg(test)]
mod tests {
    use super::super::introspection::truncate_strings;
    use super::*;

    /// A `task` event's data: the smallest Task events/v1 accepts.
    fn task_data(id: &str) -> Value {
        json!({"task": {"id": id, "contextId": "c", "status": {"state": "TASK_STATE_WORKING"}}})
    }

    #[test]
    fn the_feed_scopes_replays_and_evicts() {
        let f = SharedFeed::new(true);
        // Visibility: owner events reach the owner + operators; operator events
        // only operators; `all` events everyone.
        f.push(
            "task",
            FeedVis::Owner(Some("user:a".into())),
            task_data("t1"),
        );
        f.push("task.removed", FeedVis::Operator, json!({"id": "t0"}));
        f.push("lifecycle", FeedVis::All, json!({"paused": false}));
        f.push("task", FeedVis::Owner(None), task_data("t4")); // ownerless ⇒ operator
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
        for _ in 0..(FEED_RING + 8) {
            f.push("lifecycle", FeedVis::All, json!({"paused": false}));
        }
        let (newest, oldest, dropped) = f.bounds();
        assert_eq!(newest, 4 + (FEED_RING as u64) + 8);
        assert_eq!(dropped, 12, "4 seed + 8 overflow evicted");
        assert_eq!(oldest, newest - (FEED_RING as u64) + 1);
    }

    /// An item that leaves is announced to whoever saw it: its owner's
    /// client drops the run it was showing, and no other principal learns
    /// the id. Operators see every departure.
    #[test]
    fn departures_inherit_visibility() {
        use crate::runtime::reactor::StatusItem;
        let item = |key: &str, kind: &'static str, vis: FeedVis| StatusItem {
            key: key.into(),
            kind,
            vis,
            data: json!({"id": key}),
        };
        let mut marks = std::collections::BTreeMap::new();
        let first = diff_items(
            &mut marks,
            [
                item("run:r1", "run", FeedVis::Owner(Some("user:a".into()))),
                item(
                    "conv:c1",
                    "conversation",
                    FeedVis::Owner(Some("user:a".into())),
                ),
                item("sub:s1", "subagent", FeedVis::Operator),
                item(
                    "activity:7",
                    "activity",
                    FeedVis::Owner(Some("user:a".into())),
                ),
            ],
        );
        assert_eq!(first.len(), 4, "every new item is an event: {first:?}");
        assert!(
            diff_items(
                &mut marks,
                [
                    item("run:r1", "run", FeedVis::Owner(Some("user:a".into()))),
                    item(
                        "conv:c1",
                        "conversation",
                        FeedVis::Owner(Some("user:a".into()))
                    ),
                    item("sub:s1", "subagent", FeedVis::Operator),
                    item(
                        "activity:7",
                        "activity",
                        FeedVis::Owner(Some("user:a".into()))
                    ),
                ],
            )
            .is_empty(),
            "quiet state is quiet"
        );

        // Everything leaves.
        let gone = diff_items(&mut marks, []);
        assert!(marks.is_empty(), "the marks are dropped with the items");
        let by_kind = |k: &str| {
            gone.iter()
                .find(|(kind, _, _)| *kind == k)
                .unwrap_or_else(|| panic!("no {k}: {gone:?}"))
                .clone()
        };
        for (kind, id) in [
            ("run.removed", "r1"),
            ("conversation.removed", "c1"),
            ("activity.removed", "7"),
        ] {
            let (_, vis, data) = by_kind(kind);
            assert_eq!(vis, FeedVis::Owner(Some("user:a".into())), "{kind}");
            assert_eq!(data, json!({"id": id}), "{kind}");
        }
        assert_eq!(by_kind("subagent.removed").1, FeedVis::Operator);

        // Through the ring: the owner sees its departures, a stranger none.
        let f = SharedFeed::new(false);
        for (kind, vis, data) in gone {
            f.push(kind, vis, data);
        }
        let kinds = |who: &str, op: bool| -> Vec<String> {
            f.since(0, who, op, 100)
                .0
                .iter()
                .map(|e| e["kind"].as_str().unwrap_or("").to_string())
                .collect()
        };
        let mut owner = kinds("user:a", false);
        owner.sort();
        assert_eq!(
            owner,
            ["activity.removed", "conversation.removed", "run.removed"]
        );
        assert!(kinds("user:b", false).is_empty(), "a stranger sees none");
        assert_eq!(kinds("operator", true).len(), 4, "the operator sees all");
    }

    /// A debug build refuses every push events/v1 does not define — an
    /// unknown or removed kind, data off its schema, a kind pushed to an
    /// audience it may not reach — and stores none of them; a conforming
    /// push goes through.
    #[test]
    #[cfg(debug_assertions)]
    fn a_malformed_push_panics_in_debug_builds() {
        let f = SharedFeed::new(true);
        let refused = |kind: &str, vis: FeedVis, data: Value| {
            let why =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.push(kind, vis, data)))
                    .expect_err("the push should have panicked");
            let why = why.downcast_ref::<String>().cloned().unwrap_or_default();
            assert!(why.contains("outside events/v1"), "{kind}: {why}");
        };
        refused("message", FeedVis::All, json!({"text": "hi"}));
        refused("pairing", FeedVis::Operator, json!({}));
        refused("run.removed", FeedVis::Operator, json!({"id": 7}));
        refused(
            "auth",
            FeedVis::Operator,
            json!({"event": "launch", "sid": "ls_1", "client_id": "agentd-tui"}),
        );
        refused(
            "auth",
            FeedVis::All,
            json!({"event": "launch", "sid": "ls_1", "client_id": "agentd-tui", "scope": "operator"}),
        );
        refused("task", FeedVis::All, task_data("t"));
        assert_eq!(f.bounds(), (0, 0, 0), "nothing refused was stored");
        assert_eq!(
            f.push(
                "task",
                FeedVis::Owner(Some("user:a".into())),
                task_data("t")
            ),
            1
        );
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
}
