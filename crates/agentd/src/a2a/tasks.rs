// SPDX-License-Identifier: AGPL-3.0-only
//! **A2A tasks**: a durable unit of work a principal started — a root-turn
//! answer, a workflow run, or a subagent — projected as an A2A `Task` (spec
//! shape, `TASK_STATE_*`). Tasks are persisted, so `GetTask` answers across a
//! restart; they stream status/artifact frames from run and turn events; and
//! cancelling one cancels the work it links to, which in turn cancels that
//! work's own children, so no orphan keeps running behind a cancelled task.

use crate::state::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The A2A task state (mirrors `mcp::a2a::TaskState`, kept here so the runtime
/// does not depend on the `a2a` feature-gated module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

impl State {
    pub fn wire(self) -> &'static str {
        match self {
            State::Submitted => "TASK_STATE_SUBMITTED",
            State::Working => "TASK_STATE_WORKING",
            State::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            State::Completed => "TASK_STATE_COMPLETED",
            State::Failed => "TASK_STATE_FAILED",
            State::Canceled => "TASK_STATE_CANCELED",
            State::Rejected => "TASK_STATE_REJECTED",
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            State::Completed | State::Failed | State::Canceled | State::Rejected
        )
    }
    /// The task state a run status maps to.
    pub fn from_run(status: &str) -> State {
        match status {
            "completed" => State::Completed,
            "refused" => State::Rejected,
            "cancelled" => State::Canceled,
            "running" | "suspended" | "paused" | "pending" => State::Working,
            _ => State::Failed,
        }
    }
}

/// What a task is attached to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Run {
        id: String,
    },
    Subagent {
        handle: String,
    },
    /// A short-lived turn answer for a conversation.
    Turn {
        ctx: String,
    },
}

/// A webhook a caller registered for this task's updates (A2A push
/// notifications). `token` is echoed back in `X-A2A-Notification-Token` so the
/// receiver can tell a real delivery from a stray POST; `auth` is a credential
/// agentd presents *to* the receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushTarget {
    pub id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<PushAuth>,
}

/// The spec's `AuthenticationInfo`: what goes in the `Authorization` header of
/// every delivery, as `<scheme> <credentials>`.
///
/// Any scheme the caller names is honoured, not only `Bearer` — the receiver is
/// the caller's, and what it accepts is the caller's business. The scheme is
/// kept exactly as registered; both halves were checked at registration
/// ([`crate::a2a::push::from_wire`]) so neither can smuggle a second header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushAuth {
    pub scheme: String,
    pub credentials: String,
}

/// The most messages a task keeps for its `history`; the oldest go first.
pub const MAX_TASK_MESSAGES: usize = 64;
/// The most bytes (serialized) those messages may hold together. A count
/// alone would let a handful of pasted documents make every task record — and
/// every `GetTask` — megabytes long.
pub const MAX_TASK_MESSAGE_BYTES: usize = 256 * 1024;

/// One entry of the conversation a task carries as the spec's `Task.history`.
///
/// Kept apart from [`Task::history`], which is the STATE transitions: this is
/// who said what. The reply is not here — it is the task's result artifact —
/// and neither is the status message that is current, which the task's
/// `status` already carries; history holds what came before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMessage {
    /// A caller's `Message`, in its wire form, as received — re-addressed to
    /// this task (see [`Task::record_inbound`]).
    Inbound { message: Value },
    /// A status message the agent authored: an `input-required` question, a
    /// note a gate settled with. `seq` is what names it on the wire
    /// (`<task>.status.<seq>`), while it is current and after it is superseded.
    Status { seq: u64, text: String },
}

impl TaskMessage {
    /// What an entry costs against [`MAX_TASK_MESSAGE_BYTES`].
    fn bytes(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

/// The durable task record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub context_id: String,
    #[serde(default)]
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    pub link: Link,
    /// The status message (for `input-required` and terminal explanations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The shape a gate's answer must take (`human.schema` / `ask_human`).
    ///
    /// Carried on the task because the QUESTION alone does not tell a client
    /// how to ask it. With the schema, "pick one of these three" renders as
    /// three options instead of a text box the person has to guess the wording
    /// for — and the answer is already the right shape when it comes back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_schema: Option<Value>,
    /// The command op that opened this task, when a command did (a built-in
    /// op, or a workflow's declared `a2a` command). What marks the result as
    /// the command vocabulary's data rather than an answer in prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The terminal result (a distillate / output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub updated: u64,
    /// The transition history (state, ts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Value>,
    /// The conversation, oldest first, within [`MAX_TASK_MESSAGES`] and
    /// [`MAX_TASK_MESSAGE_BYTES`]. Durable with the task, so `GetTask` after a
    /// restart still shows the prompt that started it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<TaskMessage>,
    /// The `seq` of the current status message (`message`); `0` before the
    /// task has had one. Counted, never reused, so an id a client has already
    /// rendered keeps naming the same text.
    #[serde(default)]
    pub status_seq: u64,
    /// Where to POST this task's updates, for a caller that would rather be
    /// told than hold a stream open. Durable with the task, so a restart keeps
    /// the promise the caller was given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push: Vec<PushTarget>,
    #[serde(skip)]
    pub dirty: bool,
}

impl Task {
    pub fn new(id: &str, context_id: &str, principal: Option<&str>, link: Link) -> Task {
        let now = now_ms();
        Task {
            ask_schema: None,
            id: id.to_string(),
            context_id: context_id.to_string(),
            state: State::Submitted,
            principal: principal.map(str::to_string),
            link,
            message: None,
            command: None,
            result: None,
            created: now,
            updated: now,
            history: vec![json!({"state": State::Submitted.wire(), "ts": now})],
            messages: Vec::new(),
            status_seq: 0,
            push: Vec::new(),
            dirty: true,
        }
    }

    /// Whether `principal` may see this task at all: an operator sees every
    /// task, anyone else only the ones it started. Every read of a task —
    /// get, list, cancel, the push family — asks this one question, so "not
    /// yours" cannot mean different things on different paths.
    pub fn is_visible_to(&self, principal: &crate::a2a::Principal) -> bool {
        principal.is_operator() || self.principal.as_deref() == Some(principal.id.as_str())
    }

    pub fn transition(&mut self, state: State, message: Option<String>) {
        if self.state == state && self.message == message {
            return;
        }
        self.state = state;
        // A NEW status text is a new message: numbered, and entered into the
        // conversation now, so it sits in history where it was said rather
        // than where it was superseded. Repeating the current text is the same
        // message, and keeps its id.
        if let Some(text) = message
            && self.message.as_deref() != Some(text.as_str())
        {
            self.status_seq += 1;
            self.push_message(TaskMessage::Status {
                seq: self.status_seq,
                text: text.clone(),
            });
            self.message = Some(text);
        }
        self.updated = now_ms();
        self.history
            .push(json!({"state": state.wire(), "ts": self.updated}));
        if self.history.len() > 64 {
            self.history.remove(0);
        }
        self.dirty = true;
    }

    /// Enter a caller's `Message` into the conversation.
    ///
    /// Stored as the caller sent it, with three fields set to this task's: its
    /// role is `ROLE_USER` (a caller is never the agent, whatever it claimed),
    /// and its `taskId`/`contextId` name this task — a new task's message
    /// arrives without them, and history should not say it was addressed
    /// nowhere. The caller's `messageId` is kept: it is how a client finds its
    /// own prompt again.
    pub fn record_inbound(&mut self, message: &Value) {
        let Some(o) = message.as_object() else {
            return;
        };
        let mut o = o.clone();
        o.insert("role".into(), json!("ROLE_USER"));
        o.insert("taskId".into(), json!(self.id));
        o.insert("contextId".into(), json!(self.context_id));
        self.push_message(TaskMessage::Inbound {
            message: Value::Object(o),
        });
    }

    /// Append to the conversation, dropping the oldest entries until both
    /// bounds hold again — never the one just appended. A single message over
    /// the byte bound (the listener accepts bodies well past it) stays alone:
    /// emptying the history would lose the very prompt that opened the task,
    /// and that message is already bounded by the request it arrived in.
    fn push_message(&mut self, m: TaskMessage) {
        self.messages.push(m);
        let mut total: usize = self.messages.iter().map(TaskMessage::bytes).sum();
        while self.messages.len() > 1
            && (self.messages.len() > MAX_TASK_MESSAGES || total > MAX_TASK_MESSAGE_BYTES)
        {
            total -= self.messages.remove(0).bytes();
        }
        self.dirty = true;
    }

    pub fn set_result(&mut self, v: Value) {
        self.result = Some(v);
        self.updated = now_ms();
        self.dirty = true;
    }

    /// The A2A `Task` object — what `GetTask`, `CancelTask` and a `SendMessage`
    /// reply carry. Built from the specification's own types, so the wire
    /// spellings are not ours to get wrong; see [`crate::a2a::wire`].
    #[cfg(feature = "a2a")]
    pub fn to_a2a(&self, ann: crate::a2a::wire::Annotations) -> Value {
        serde_json::to_value(crate::a2a::wire::task(self, ann)).unwrap_or(Value::Null)
    }

    /// The projection `ListTasks` returns: the same `Task`, cut to what the
    /// caller asked a listing to carry (see [`crate::a2a::wire::ListView`]).
    #[cfg(feature = "a2a")]
    pub fn summary(
        &self,
        view: crate::a2a::wire::ListView,
        ann: crate::a2a::wire::Annotations,
    ) -> Value {
        serde_json::to_value(crate::a2a::wire::task_listed(self, view, ann)).unwrap_or(Value::Null)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The durable record's own behaviour. What it looks like on the wire is
    /// `a2a::wire`'s job, and is tested there against the spec's types.
    #[test]
    fn a_task_records_its_lifecycle_and_survives_a_round_trip() {
        let mut t = Task::new(
            "task-1",
            "ctx-1",
            Some("user:a"),
            Link::Run { id: "r1".into() },
        );
        assert_eq!(t.state, State::Submitted);
        t.transition(State::Working, None);
        t.transition(State::Working, None); // idempotent
        assert_eq!(t.history.len(), 2);
        t.set_result(json!({"answer": 42}));
        t.transition(State::Completed, Some("done".into()));
        assert!(t.state.is_terminal());
        assert_eq!(t.message.as_deref(), Some("done"));
        assert_eq!(State::from_run("refused"), State::Rejected);
        assert_eq!(State::from_run("running"), State::Working);

        let v = serde_json::to_value(&t).unwrap();
        let back: Task = serde_json::from_value(v).unwrap();
        assert_eq!(back.state, t.state);
        assert_eq!(back.history.len(), t.history.len());
        assert!(!back.dirty);
    }

    /// The conversation stays within both bounds by dropping its oldest
    /// entries, and a status message keeps its number from the moment it is
    /// said: superseding it, or saying the same thing again, never renumbers
    /// what a client has already rendered.
    #[test]
    fn history_is_bounded_and_status_ids_are_stable() {
        let mut t = Task::new("t", "c", Some("user:a"), Link::Turn { ctx: "c".into() });
        let said = |n: usize| json!({"messageId": format!("m{n}"), "role": "ROLE_AGENT", "parts": [{"text": format!("hi {n}")}]});

        // An inbound message is the caller's, addressed to this task.
        t.record_inbound(&said(0));
        let TaskMessage::Inbound { message } = &t.messages[0] else {
            panic!("an inbound entry: {:?}", t.messages);
        };
        assert_eq!(message["role"], "ROLE_USER", "a caller is never the agent");
        assert_eq!(message["taskId"], "t");
        assert_eq!(message["contextId"], "c");
        assert_eq!(message["messageId"], "m0", "the caller's id is kept");

        // Status messages are numbered as they are said.
        t.transition(State::InputRequired, Some("Proceed?".into()));
        assert_eq!(t.status_seq, 1);
        t.transition(State::InputRequired, Some("Proceed?".into()));
        t.transition(State::Working, Some("Proceed?".into()));
        t.transition(State::Working, None);
        assert_eq!(t.status_seq, 1, "the same text is the same message");
        t.record_inbound(&said(1));
        t.transition(State::Working, Some("answered".into()));
        assert_eq!(t.status_seq, 2);
        let seqs: Vec<u64> = t
            .messages
            .iter()
            .filter_map(|m| match m {
                TaskMessage::Status { seq, .. } => Some(*seq),
                TaskMessage::Inbound { .. } => None,
            })
            .collect();
        assert_eq!(seqs, [1, 2], "numbered where said, never renumbered");
        assert!(
            matches!(&t.messages[1], TaskMessage::Status { seq: 1, text } if text == "Proceed?"),
            "the question sits before the answer: {:?}",
            t.messages
        );

        // The count bound: the newest 64 survive.
        for n in 2..200 {
            t.record_inbound(&said(n));
        }
        assert_eq!(t.messages.len(), MAX_TASK_MESSAGES);
        let first = |t: &Task| match &t.messages[0] {
            TaskMessage::Inbound { message } => message["messageId"].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(first(&t), json!(format!("m{}", 200 - MAX_TASK_MESSAGES)));

        // The byte bound: a few large messages push the oldest out while the
        // newest stays.
        let big = |n: usize| json!({"messageId": format!("big{n}"), "parts": [{"text": "x".repeat(60 * 1024)}]});
        for n in 0..6 {
            t.record_inbound(&big(n));
        }
        let total: usize = t.messages.iter().map(TaskMessage::bytes).sum();
        assert!(total <= MAX_TASK_MESSAGE_BYTES, "{total} bytes kept");
        assert_eq!(t.messages.len(), 4, "four 60 KiB messages fit, five do not");
        assert_eq!(first(&t), json!("big2"));

        // One message over the whole byte bound evicts everything older, and
        // is itself kept: the newest entry is never dropped.
        let huge = json!({"messageId": "huge", "parts": [{"text": "x".repeat(MAX_TASK_MESSAGE_BYTES + 1)}]});
        t.record_inbound(&huge);
        assert_eq!(t.messages.len(), 1, "{} kept", t.messages.len());
        assert_eq!(first(&t), json!("huge"));

        // Durable: the conversation and the numbering survive a round trip.
        let back: Task = serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        assert_eq!(back.messages, t.messages);
        assert_eq!(back.status_seq, 2);
    }

    #[test]
    fn a_task_is_visible_to_its_owner_and_the_operator_only() {
        use crate::a2a::Principal;
        use crate::config::v2::Role;
        let who = |id: &str, role| Principal {
            id: id.into(),
            role,
            ..Principal::anonymous()
        };
        let t = Task::new("t", "c", Some("user:a"), Link::Turn { ctx: "c".into() });
        assert!(t.is_visible_to(&who("user:a", Role::User)));
        assert!(t.is_visible_to(&who("operator", Role::Operator)));
        assert!(!t.is_visible_to(&who("user:b", Role::User)));
        // An ownerless task is the operator's alone.
        let orphan = Task::new("o", "c", None, Link::Turn { ctx: "c".into() });
        assert!(!orphan.is_visible_to(&Principal::anonymous()));
        assert!(orphan.is_visible_to(&who("operator", Role::Operator)));
    }
}
