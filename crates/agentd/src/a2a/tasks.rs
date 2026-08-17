// SPDX-License-Identifier: AGPL-3.0-only
//! **A2A tasks** (RFC 0029 §4, RFC 0025 §3.3 `task`): a durable unit of work a
//! principal started — a root-turn answer, a workflow run, or a subagent —
//! projected as an A2A `Task` (spec shape, `TASK_STATE_*`). Tasks survive
//! restarts (`GetTask` works across lives), stream status/artifact frames from
//! run/turn events, and cascade-cancel per RFC 0027 §6.

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
    #[serde(skip)]
    pub dirty: bool,
}

impl Task {
    pub fn new(id: &str, context_id: &str, principal: Option<&str>, link: Link) -> Task {
        let now = now_ms();
        Task {
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

    /// The A2A `Task` object (RFC 0020 shape) — artifacts as text/data parts is
    /// the transport's job; here artifacts are ids the surface resolves.
    pub fn to_a2a(&self) -> Value {
        let mut status =
            json!({"state": self.state.wire(), "timestamp": a2a_timestamp(self.updated)});
        if let Some(m) = &self.message {
            status["message"] = agent_message(&self.id, &self.context_id, m);
        }
        json!({
            "id": self.id,
            "contextId": self.context_id,
            "status": status,
            "artifacts": self.artifact_parts(),
            // `history` is `repeated Message` in the spec, so the state
            // transitions cannot live there — a peer would read them as empty
            // messages. They go under `metadata`, which is what proto3 leaves
            // open for exactly this.
            "metadata": self.a2a_metadata(),
        })
    }

    /// The agentd-specific facts a peer may find useful but the spec does not
    /// name: who owns the task, what it is attached to, and how its state moved.
    /// Namespaced so they cannot collide with a future spec field.
    fn a2a_metadata(&self) -> Value {
        let mut m = json!({"agentd/link": self.link, "agentd/created": self.created});
        if let Some(p) = &self.principal {
            m["agentd/principal"] = json!(p);
        }
        if !self.history.is_empty() {
            m["agentd/statusHistory"] = json!(self.history);
        }
        m
    }

    fn artifact_parts(&self) -> Vec<Value> {
        let mut parts = Vec::new();
        if let Some(r) = &self.result {
            let text = match r {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            parts.push(
                json!({"artifactId": format!("{}.result", self.id), "parts": [{"text": text}]}),
            );
        }
        for a in &self.artifacts {
            parts.push(json!({"artifactId": a, "parts": []}));
        }
        parts
    }

    /// The light projection `ListTasks` returns: the same `Task` shape as
    /// [`Task::to_a2a`] minus the artifacts (which a listing does not resolve).
    /// It has to *be* a `Task` — a peer deserializes the array as one, and a
    /// flat `state` here would simply fail to parse.
    pub fn summary(&self) -> Value {
        let mut status =
            json!({"state": self.state.wire(), "timestamp": a2a_timestamp(self.updated)});
        if let Some(m) = &self.message {
            status["message"] = agent_message(&self.id, &self.context_id, m);
        }
        json!({
            "id": self.id,
            "contextId": self.context_id,
            "status": status,
            "metadata": self.a2a_metadata(),
        })
    }
}

/// A `TaskStatus.timestamp` on the wire.
///
/// The field is a `google.protobuf.Timestamp`, which proto3 JSON renders as an
/// RFC 3339 UTC **string** — not the epoch milliseconds we keep internally. A
/// peer's generated type rejects the integer outright.
pub fn a2a_timestamp(ms: u64) -> String {
    crate::obs::log::rfc3339_millis(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
}

/// A `Message` from the agent side of a task.
///
/// The role is the proto enum name, not the English word: the A2A wire is
/// proto3 JSON, so `ROLE_AGENT` is what a peer's generated types accept —
/// `"agent"` fails to deserialize. (Same reason the states are `TASK_STATE_*`.)
fn agent_message(task_id: &str, context_id: &str, text: &str) -> Value {
    json!({
        "messageId": format!("{task_id}.status"),
        "taskId": task_id,
        "contextId": context_id,
        "role": "ROLE_AGENT",
        "parts": [{"text": text}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle_and_a2a_projection() {
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
        t.set_result(json!({"answer": 42}));
        t.transition(State::Completed, Some("done".into()));
        let a = t.to_a2a();
        assert_eq!(a["id"], "task-1");
        assert_eq!(a["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(a["status"]["message"]["parts"][0]["text"], "done");
        assert!(
            a["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["artifactId"] == "task-1.result")
        );
        assert!(
            a["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["artifactId"] == "art-9")
        );
        assert!(t.state.is_terminal());
        assert_eq!(State::from_run("refused"), State::Rejected);
        assert_eq!(State::from_run("running"), State::Working);

        // The wire is proto3 JSON, and every way of getting that wrong is
        // silent — a peer's generated types just refuse to deserialize.
        assert_eq!(a["status"]["message"]["role"], "ROLE_AGENT");
        assert!(
            a["status"]["timestamp"]
                .as_str()
                .is_some_and(|s| s.ends_with('Z')),
            "`timestamp` is a google.protobuf.Timestamp ⇒ RFC 3339: {a}"
        );
        assert!(
            a["history"].is_null(),
            "`history` is `repeated Message`; status transitions belong in metadata: {a}"
        );
        assert_eq!(a["metadata"]["agentd/principal"], "user:a");
        assert_eq!(
            a["metadata"]["agentd/statusHistory"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        // The listing is the same object, minus the artifacts a listing does
        // not resolve — never a different, flatter shape.
        let s = t.summary();
        assert_eq!(s["id"], a["id"]);
        assert_eq!(s["status"]["state"], a["status"]["state"]);
        assert!(s["state"].is_null(), "no bare `state` on the wire: {s}");
        assert!(s["artifacts"].is_null());
        // Round trip.
        let v = serde_json::to_value(&t).unwrap();
        let back: Task = serde_json::from_value(v).unwrap();
        assert_eq!(back.state, t.state);
        assert!(!back.dirty);
    }
}
