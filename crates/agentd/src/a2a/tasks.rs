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
/// receiver can tell a real delivery from a stray POST; `bearer` is a
/// credential agentd presents *to* the receiver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushTarget {
    pub id: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<String>,
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
    /// Artifact ids delivered on this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
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
            artifacts: Vec::new(),
            result: None,
            created: now,
            updated: now,
            history: vec![json!({"state": State::Submitted.wire(), "ts": now})],
            push: Vec::new(),
            dirty: true,
        }
    }

    pub fn transition(&mut self, state: State, message: Option<String>) {
        if self.state == state && self.message == message {
            return;
        }
        self.state = state;
        if message.is_some() {
            self.message = message;
        }
        self.updated = now_ms();
        self.history
            .push(json!({"state": state.wire(), "ts": self.updated}));
        if self.history.len() > 64 {
            self.history.remove(0);
        }
        self.dirty = true;
    }

    pub fn add_artifact(&mut self, id: &str) {
        if !self.artifacts.iter().any(|a| a == id) {
            self.artifacts.push(id.to_string());
            self.updated = now_ms();
            self.dirty = true;
        }
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
    pub fn to_a2a(&self) -> Value {
        serde_json::to_value(crate::a2a::wire::task(self)).unwrap_or(Value::Null)
    }

    /// The light projection `ListTasks` returns: the same `Task` minus the
    /// artifacts a listing does not resolve.
    #[cfg(feature = "a2a")]
    pub fn summary(&self) -> Value {
        serde_json::to_value(crate::a2a::wire::task_summary(self)).unwrap_or(Value::Null)
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
        t.add_artifact("art-9");
        t.add_artifact("art-9"); // idempotent
        assert_eq!(t.artifacts.len(), 1);
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
}
