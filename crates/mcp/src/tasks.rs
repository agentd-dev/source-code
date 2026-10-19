// SPDX-License-Identifier: AGPL-3.0-only
//! **Following an MCP task** (the Tasks extension, SEP-2663): the state one
//! task needs between polls, kept apart from any socket so the same rules
//! hold wherever a task is followed — inline, inside a call
//! ([`crate::client::McpClient::follow_task`]), or durably by a host that
//! keeps [`Tracker`] in its store.
//!
//! The rules it holds:
//!
//! * **The server's poll interval, within bounds.** `pollIntervalMs` is
//!   honoured, never below [`POLL_FLOOR`] nor above [`POLL_CEILING`], and
//!   [`POLL_DEFAULT`] when the server names none.
//! * **An input request is asked once.** A key answered — or being asked — is
//!   not asked again when the next poll lists it again, as an eventually
//!   consistent server does after a `tasks/update`.
//! * **A request is answered as it would be directly.** A form elicitation
//!   goes to the host's [`crate::inbound::Handler`] when the connection
//!   declares `elicitation`, and is declined when it does not; a URL
//!   elicitation is declined; sampling and roots, which agentd never declares,
//!   end the task.
//! * **A seed is not the whole task.** A `CreateTaskResult` carries the base
//!   task only, so a seed in any status but `working` has not said where it
//!   got to: it is fetched with `tasks/get` at [`POLL_FLOOR`], and a final
//!   status counts once its `result` or `error` has come.
//! * **The TTL is a backstop.** A task still open more than its `ttlMs` (plus
//!   [`TTL_GRACE`]) after it was first seen is no longer usable. Counted from
//!   the first sight, never from `createdAt`, so a clock ahead of the server's
//!   never ends one early.

use crate::inbound::Answer;
use crate::wire::{Task, TaskStatus};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;

/// The shortest wait between two polls of one task.
pub const POLL_FLOOR: Duration = Duration::from_millis(250);
/// The wait when the server names no `pollIntervalMs`.
pub const POLL_DEFAULT: Duration = Duration::from_secs(2);
/// The longest wait between two polls of one task, whatever the server asks.
pub const POLL_CEILING: Duration = Duration::from_secs(600);
/// How long past its `ttlMs` a task may stay open before it is given up.
pub const TTL_GRACE: Duration = Duration::from_secs(30);
/// The bound on each `tasks/*` request.
pub const REQUEST_BOUND: Duration = Duration::from_secs(30);

/// When to poll `task` next: its `pollIntervalMs`, clamped.
pub fn poll_interval(task: &Task) -> Duration {
    task.poll_interval_ms
        .map(Duration::from_millis)
        .unwrap_or(POLL_DEFAULT)
        .clamp(POLL_FLOOR, POLL_CEILING)
}

/// One input request a task asked for, as the host is to treat it.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// A form elicitation: put to a person where the connection can.
    Elicit {
        key: String,
        message: String,
        requested_schema: Value,
    },
    /// A request answered with `decline` at once: a URL elicitation, which
    /// has nothing a person could type.
    Decline { key: String, method: String },
    /// A request agentd never declares (`sampling/createMessage`,
    /// `roots/list`, anything else): the task cannot go on.
    Unsupported { key: String, method: String },
}

impl Ask {
    pub fn key(&self) -> &str {
        match self {
            Ask::Elicit { key, .. } | Ask::Decline { key, .. } | Ask::Unsupported { key, .. } => {
                key
            }
        }
    }

    pub fn method(&self) -> &str {
        match self {
            Ask::Elicit { .. } => "elicitation/create",
            Ask::Decline { method, .. } | Ask::Unsupported { method, .. } => method,
        }
    }
}

/// Read one entry of `inputRequests`: `{method, params}`.
pub fn ask_of(key: &str, request: &Value) -> Ask {
    let method = request["method"].as_str().unwrap_or_default().to_string();
    let params = &request["params"];
    match method.as_str() {
        "elicitation/create" => match params["mode"].as_str() {
            // A form is the default mode.
            None | Some("form") => Ask::Elicit {
                key: key.to_string(),
                message: params["message"].as_str().unwrap_or_default().to_string(),
                requested_schema: params
                    .get("requestedSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            },
            _ => Ask::Decline {
                key: key.to_string(),
                method,
            },
        },
        _ => Ask::Unsupported {
            key: key.to_string(),
            method,
        },
    }
}

/// An elicitation's answer as `inputResponses` carries it: an `ElicitResult`.
pub fn elicit_result(answer: Option<&Answer>) -> Value {
    match answer {
        Some(Answer::Accept(content)) => json!({"action": "accept", "content": content}),
        Some(Answer::Decline) => json!({"action": "decline"}),
        // Nothing could ask, or nobody answered: cancel is the honest outcome.
        Some(Answer::Cancel) | None => json!({"action": "cancel"}),
    }
}

/// What one look at a task changed.
#[derive(Debug, Default, PartialEq)]
pub struct Seen {
    /// The status before, when it moved.
    pub moved_from: Option<TaskStatus>,
    /// Input requests not asked before, in key order.
    pub asks: Vec<Ask>,
}

/// One task as its follower knows it: the server's last word, when agentd
/// first saw it, and the input keys already dealt with.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tracker {
    pub task: Task,
    /// Unix ms agentd first saw the task: what the TTL backstop counts from.
    pub first_seen_ms: u64,
    /// Input keys answered: never asked again.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub answered: BTreeSet<String>,
    /// Answers given and not yet acknowledged by the server, by key: sent
    /// again before the next poll. A person's words, so never stored.
    #[serde(skip)]
    pub unsent: std::collections::BTreeMap<String, Value>,
}

impl Tracker {
    pub fn new(task: Task, now_ms: u64) -> Tracker {
        Tracker {
            task,
            first_seen_ms: now_ms,
            answered: BTreeSet::new(),
            unsent: Default::default(),
        }
    }

    /// Record `response` as the answer to `key`: the key is never asked
    /// again, and the answer waits in [`Self::unsent`] until the server
    /// acknowledges it.
    pub fn answer(&mut self, key: &str, response: Value) {
        self.answered.insert(key.to_string());
        self.unsent.insert(key.to_string(), response);
    }

    /// Has the task ended, its outcome in hand? A seed in a final status has
    /// not: its `result` or `error` comes with the next `tasks/get`.
    pub fn ended(&self) -> bool {
        self.task.status.is_terminal() && self.task.missing().is_none()
    }

    /// Give up the answers the server has not acknowledged: their keys leave
    /// [`Self::answered`], so whoever follows the task next deals with them.
    /// For a task handed on, whose next follower cannot send words it was
    /// never given.
    pub fn release_unsent(&mut self) {
        for key in std::mem::take(&mut self.unsent).into_keys() {
            self.answered.remove(&key);
        }
    }

    /// Take the server's newest word on the task. A different task's, or one
    /// that moves an ended task anywhere, changes nothing: a final state is
    /// final, and a late poll or push must not undo it.
    pub fn observe(&mut self, next: Task) -> Seen {
        if next.task_id != self.task.task_id || self.ended() {
            return Seen::default();
        }
        let moved_from = (next.status != self.task.status).then_some(self.task.status);
        let mut asks = Vec::new();
        if next.status == TaskStatus::InputRequired
            && let Some(requests) = &next.input_requests
        {
            for (key, request) in requests {
                if !self.answered.contains(key) {
                    asks.push(ask_of(key, request));
                }
            }
        }
        self.task = next;
        Seen { moved_from, asks }
    }

    /// Mark `key` dealt with: it is not asked again.
    pub fn answering(&mut self, key: &str) {
        self.answered.insert(key.to_string());
    }

    /// When to poll next: a seed that has not said where it got to at the
    /// floor, any other task at its own interval.
    pub fn interval(&self) -> Duration {
        match self.task.missing() {
            Some(_) => POLL_FLOOR,
            None => poll_interval(&self.task),
        }
    }

    /// Is the task past its TTL and still open?
    pub fn expired(&self, now_ms: u64) -> bool {
        !self.ended()
            && self.task.ttl_ms.is_some_and(|ttl| {
                now_ms
                    > self
                        .first_seen_ms
                        .saturating_add(ttl)
                        .saturating_add(TTL_GRACE.as_millis() as u64)
            })
    }
}

/// How a followed task ended, or where it was when the follower stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Followed {
    /// It reached a final status.
    Ended(Tracker),
    /// The caller's bound passed while it was still open.
    Open(Tracker),
}

/// Why following a task stopped short of its end.
#[derive(Debug, Clone, PartialEq)]
pub enum Lost {
    /// The server does not know the task (`-32602`): never created, purged,
    /// or expired.
    Unknown(String),
    /// Open past its TTL: the server may have discarded it.
    Expired,
    /// It asked for something agentd never declares; it was cancelled.
    Unsupported(String),
    /// A `tasks/*` request failed in a way that did not pass.
    Failed(String),
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Lost::Unknown(m) => write!(
                f,
                "the server does not know the task (unknown or expired): {m}"
            ),
            Lost::Expired => write!(f, "the task was still open past its ttlMs"),
            Lost::Unsupported(m) => write!(
                f,
                "the task asked for {m}, which agentd does not support; it was cancelled"
            ),
            Lost::Failed(m) => write!(f, "{m}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str, extra: Value) -> Task {
        let mut v = json!({"taskId": "t-1", "status": status, "createdAt": "2026-10-19T10:00:00Z",
                           "lastUpdatedAt": "2026-10-19T10:00:00Z", "ttlMs": 60000, "pollIntervalMs": 500});
        if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
            o.extend(e);
        }
        Task::read(&v).expect("a task")
    }

    fn elicit(message: &str) -> Value {
        json!({"method": "elicitation/create", "params": {"mode": "form", "message": message,
               "requestedSchema": {"type": "object", "properties": {"name": {"type": "string"}}}}})
    }

    #[test]
    fn a_status_without_its_member_is_not_a_task() {
        for (status, member) in [
            ("input_required", "inputRequests"),
            ("completed", "result"),
            ("failed", "error"),
        ] {
            let v = json!({"taskId": "t", "status": status, "createdAt": "", "lastUpdatedAt": "", "ttlMs": null});
            let e = Task::read(&v).expect_err(status);
            assert!(e.contains(member), "{status}: {e}");
        }
        let unknown = json!({"taskId": "t", "status": "paused", "ttlMs": null});
        assert!(
            Task::read(&unknown).is_err(),
            "a status the extension does not name"
        );
        let empty = json!({"taskId": "", "status": "working", "ttlMs": null});
        assert!(Task::read(&empty).is_err(), "an empty taskId");
        // `ttlMs: null` is unlimited, and so is a server that leaves it out.
        let t = Task::read(&json!({"taskId": "t", "status": "working"})).unwrap();
        assert_eq!(t.ttl_ms, None);
    }

    #[test]
    fn a_completed_task_carries_its_tool_result_an_error_one_included() {
        let t = task(
            "completed",
            json!({"result": {"content": [{"type": "text", "text": "no"}], "isError": true}}),
        );
        let r = t.tool_result().unwrap().unwrap();
        assert!(r.is_error());
        assert_eq!(r.text(), "no");
    }

    #[test]
    fn the_poll_interval_is_the_servers_within_the_floor_and_the_ceiling() {
        let mut t = task("working", json!({}));
        t.poll_interval_ms = Some(5000);
        assert_eq!(poll_interval(&t), Duration::from_secs(5));
        t.poll_interval_ms = Some(1);
        assert_eq!(poll_interval(&t), POLL_FLOOR);
        t.poll_interval_ms = Some(24 * 3600 * 1000);
        assert_eq!(poll_interval(&t), POLL_CEILING);
        t.poll_interval_ms = None;
        assert_eq!(poll_interval(&t), POLL_DEFAULT);
    }

    #[test]
    fn an_input_key_is_asked_once_across_polls() {
        let mut tr = Tracker::new(task("working", json!({})), 0);
        let asked = json!({"inputRequests": {"name": elicit("Your name?")}});
        let seen = tr.observe(task("input_required", asked.clone()));
        assert_eq!(seen.moved_from, Some(TaskStatus::Working));
        assert_eq!(seen.asks.len(), 1);
        assert_eq!(seen.asks[0].key(), "name");
        tr.answering("name");
        // The server lists it again before the update shows: not asked again.
        let again = tr.observe(task("input_required", asked));
        assert_eq!(again, Seen::default());
        // A new key on the same task is.
        let more =
            json!({"inputRequests": {"name": elicit("Your name?"), "city": elicit("Your city?")}});
        let seen = tr.observe(task("input_required", more));
        assert_eq!(seen.asks.iter().map(Ask::key).collect::<Vec<_>>(), ["city"]);
    }

    #[test]
    fn a_final_status_is_final() {
        let mut tr = Tracker::new(task("working", json!({})), 0);
        tr.observe(task("cancelled", json!({})));
        let late = tr.observe(task("working", json!({})));
        assert_eq!(late, Seen::default());
        assert_eq!(tr.task.status, TaskStatus::Cancelled);
        // Nor does another task's word change this one.
        let mut tr = Tracker::new(task("working", json!({})), 0);
        let mut other = task("completed", json!({"result": {"content": []}}));
        other.task_id = "t-2".into();
        assert_eq!(tr.observe(other), Seen::default());
        assert_eq!(tr.task.status, TaskStatus::Working);
    }

    #[test]
    fn requests_are_read_as_they_would_be_sent_directly() {
        assert!(matches!(ask_of("a", &elicit("q")), Ask::Elicit { .. }));
        let url =
            json!({"method": "elicitation/create", "params": {"mode": "url", "url": "https://x"}});
        assert!(matches!(ask_of("b", &url), Ask::Decline { .. }));
        let sampling = json!({"method": "sampling/createMessage", "params": {}});
        assert_eq!(
            ask_of("c", &sampling),
            Ask::Unsupported {
                key: "c".into(),
                method: "sampling/createMessage".into()
            }
        );
        assert_eq!(
            elicit_result(Some(&Answer::Accept(json!({"name": "Luca"})))),
            json!({"action": "accept", "content": {"name": "Luca"}})
        );
        assert_eq!(elicit_result(None), json!({"action": "cancel"}));
    }

    #[test]
    fn a_seed_that_moved_on_is_fetched_at_the_floor_before_it_counts() {
        // A `CreateTaskResult` is the base task only: a job that finished at
        // once says `completed`, and nothing of its result.
        let v = json!({"taskId": "t-1", "status": "completed", "createdAt": "", "lastUpdatedAt": "",
                       "ttlMs": 60000, "pollIntervalMs": 5000});
        assert!(Task::read(&v).is_err(), "a poll's answer must carry it");
        let seed = Task::seed(&v).expect("a seed in any status");
        let mut tr = Tracker::new(seed, 0);
        assert!(!tr.ended(), "its outcome is not in hand");
        assert_eq!(tr.interval(), POLL_FLOOR, "fetched at once, not in 5 s");
        let done = task(
            "completed",
            json!({"result": {"content": [{"type": "text", "text": "at once"}]}}),
        );
        assert_eq!(tr.observe(done), Seen::default());
        assert!(tr.ended());
        assert_eq!(tr.task.tool_result().unwrap().unwrap().text(), "at once");
        assert_eq!(tr.interval(), Duration::from_millis(500));
        // A seed that is asking says what only `tasks/get` lists.
        let asking = Task::seed(&json!({"taskId": "t-1", "status": "input_required"})).unwrap();
        let tr = Tracker::new(asking, 0);
        assert_eq!(tr.interval(), POLL_FLOOR);
        // A cancelled seed carries all a cancelled task does.
        let cancelled = Task::seed(&json!({"taskId": "t-1", "status": "cancelled"})).unwrap();
        assert!(Tracker::new(cancelled, 0).ended());
    }

    #[test]
    fn an_answer_the_server_did_not_acknowledge_is_released_for_the_next_follower() {
        let mut tr = Tracker::new(task("working", json!({})), 0);
        tr.answer(
            "name",
            json!({"action": "accept", "content": {"name": "Luca"}}),
        );
        tr.answering("city");
        tr.release_unsent();
        assert!(tr.unsent.is_empty());
        assert!(
            !tr.answered.contains("name"),
            "its words are not handed on, so its key is not either"
        );
        assert!(
            tr.answered.contains("city"),
            "a key dealt with stays dealt with"
        );
    }

    #[test]
    fn the_ttl_counts_from_the_first_sight_plus_a_grace() {
        let tr = Tracker::new(task("working", json!({})), 1_000);
        let grace = TTL_GRACE.as_millis() as u64;
        assert!(!tr.expired(1_000 + 60_000 + grace));
        assert!(tr.expired(1_000 + 60_000 + grace + 1));
        let mut unlimited = tr.clone();
        unlimited.task.ttl_ms = None;
        assert!(!unlimited.expired(u64::MAX));
    }
}
