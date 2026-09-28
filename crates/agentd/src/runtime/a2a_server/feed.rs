// SPDX-License-Identifier: AGPL-3.0-only
//! The interface observation feed: the ring the loop pushes state changes
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
    /// `a2a.introspection.enabled` — gates the introspection event kinds
    /// (audit, logs). Atomic because the operator can toggle it at runtime
    /// (`admin.set`, a reload).
    debug: std::sync::atomic::AtomicBool,
}

struct FeedInner {
    seq: u64,
    buf: std::collections::VecDeque<Value>,
    /// Events evicted to date (a subscriber whose cursor predates the window
    /// learns it fell behind).
    dropped: u64,
}

impl SharedFeed {
    pub fn new(debug: bool) -> SharedFeed {
        SharedFeed {
            inner: Mutex::new(FeedInner {
                seq: 0,
                buf: std::collections::VecDeque::with_capacity(FEED_RING),
                dropped: 0,
            }),
            debug: std::sync::atomic::AtomicBool::new(debug),
        }
    }

    /// Whether debug event kinds flow (runtime-togglable via `admin.set`).
    pub fn debug(&self) -> bool {
        self.debug.load(std::sync::atomic::Ordering::Relaxed)
    }
    pub fn set_debug(&self, on: bool) {
        self.debug.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Append one event; returns its `seq`.
    pub fn push(&self, kind: &str, vis: FeedVis, data: Value) -> u64 {
        let vis_tag = match vis {
            FeedVis::All => json!("all"),
            FeedVis::Operator => json!("op"),
            FeedVis::Owner(None) => json!("op"),
            FeedVis::Owner(Some(p)) => json!(p),
        };
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.seq += 1;
        let seq = g.seq;
        let ev = json!({"seq": seq, "ts": crate::state::now_ms(), "kind": kind, "data": data, "_vis": vis_tag});
        if g.buf.len() == FEED_RING {
            g.buf.pop_front();
            g.dropped += 1;
        }
        g.buf.push_back(ev);
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
        for ev in g.buf.iter() {
            let seq = ev["seq"].as_u64().unwrap_or(0);
            if seq <= after {
                continue;
            }
            if out.len() >= max {
                break;
            }
            cursor = seq;
            let visible = match ev["_vis"].as_str() {
                Some("all") => true,
                Some("op") => is_operator,
                Some(owner) => is_operator || owner == principal,
                None => is_operator,
            };
            if visible {
                let mut e = ev.clone();
                if let Value::Object(o) = &mut e {
                    o.remove("_vis");
                }
                out.push(e);
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
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(g.seq);
        (g.seq, oldest, g.dropped)
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
    pub(crate) fn feed_tick(&mut self) {
        if self.a2a_feed.is_none() {
            return;
        }
        if self.feed_last.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.feed_last = Instant::now();
        let mut fresh: Vec<(String, &'static str, FeedVis, Value)> = Vec::new();
        for (id, r) in &self.runs {
            fresh.push((
                format!("run:{id}"),
                "run",
                FeedVis::Owner(r.principal.clone()),
                r.summary(),
            ));
        }
        for c in self.contexts.status().as_array().into_iter().flatten() {
            let id = c["id"].as_str().unwrap_or("").to_string();
            let owner = c["principal"].as_str().map(str::to_string);
            fresh.push((
                format!("conv:{id}"),
                "conversation",
                FeedVis::Owner(owner),
                c.clone(),
            ));
        }
        for (h, s) in &self.subagents {
            fresh.push((
                format!("sub:{h}"),
                "subagent",
                FeedVis::Operator,
                json!({"handle": s.handle, "mode": s.mode, "status": s.status, "tokens": s.tokens, "error": s.error, "updated": s.updated}),
            ));
        }
        for c in self.children.status().as_array().into_iter().flatten() {
            let node = c["node"].as_u64().unwrap_or(0);
            fresh.push((
                format!("child:{node}"),
                "child",
                FeedVis::Operator,
                c.clone(),
            ));
        }
        fresh.push((
            "status".into(),
            "status",
            FeedVis::Operator,
            json!({
                "instance": self.instance,
                "model": self.model,
                "draining": self.draining,
                "inbox_pending": self.inbox_queue.len(),
                "counters": {"turns": self.counters.turns, "tool_calls": self.counters.tool_calls, "runs_started": self.counters.runs_started, "runs_finished": self.counters.runs_finished, "tokens_in": self.counters.tokens_in, "tokens_out": self.counters.tokens_out},
                "budget": self.governor.status(crate::state::now_ms()),
                "store": {"kind": self.durable.store_kind(), "degraded": self.durable.is_degraded()},
            }),
        ));
        // Diff against the marks; emit changed items, then departures.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut pushes: Vec<(&'static str, FeedVis, Value)> = Vec::new();
        for (key, kind, vis, data) in fresh {
            let mark = fingerprint(&data);
            seen.insert(key.clone());
            if self.feed_marks.get(&key) != Some(&mark) {
                self.feed_marks.insert(key, mark);
                pushes.push((kind, vis, data));
            }
        }
        let gone: Vec<String> = self
            .feed_marks
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        for key in gone {
            self.feed_marks.remove(&key);
            if let Some((section, id)) = key.split_once(':') {
                let kind: &'static str = match section {
                    "run" => "run.removed",
                    "conv" => "conversation.removed",
                    "sub" => "subagent.removed",
                    _ => "child.removed",
                };
                pushes.push((kind, FeedVis::Operator, json!({"id": id})));
            }
        }
        for (kind, vis, data) in pushes {
            self.feed_push(kind, vis, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::introspection::truncate_strings;
    use super::*;

    #[test]
    fn the_feed_scopes_replays_and_evicts() {
        let f = SharedFeed::new(true);
        // Visibility: owner events reach the owner + operators; operator events
        // only operators; `all` events everyone.
        f.push(
            "task",
            FeedVis::Owner(Some("user:a".into())),
            json!({"n": 1}),
        );
        f.push("status", FeedVis::Operator, json!({"n": 2}));
        f.push("lifecycle", FeedVis::All, json!({"n": 3}));
        f.push("task", FeedVis::Owner(None), json!({"n": 4})); // ownerless ⇒ operator
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
        for i in 0..(FEED_RING + 8) {
            f.push("task", FeedVis::All, json!({"i": i}));
        }
        let (newest, oldest, dropped) = f.bounds();
        assert_eq!(newest, 4 + (FEED_RING as u64) + 8);
        assert_eq!(dropped, 12, "4 seed + 8 overflow evicted");
        assert_eq!(oldest, newest - (FEED_RING as u64) + 1);
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
