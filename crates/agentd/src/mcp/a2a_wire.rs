// SPDX-License-Identifier: AGPL-3.0-only
//! A2A **client-side wire helpers** (RFC 0020 §3, A2A spec §9): the `TaskState`
//! enum (the proto `TASK_STATE_*` strings) and the request/response shaping the
//! A2A client ([`crate::mcp::a2a_client`]) uses to delegate to a remote peer and
//! read its `Task` objects back. (The v1 A2A **server** surface was removed with
//! the mode cut-over; the v2 A2A server is [`crate::runtime::a2a_server`].)

use serde_json::{Value, json};

/// A2A `TaskState` — the exact enum strings from `a2a.proto`. A still-running
/// remote run is `WORKING`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    Canceled,
    Rejected,
    InputRequired,
    AuthRequired,
    Unspecified,
}

impl TaskState {
    /// The wire string (verbatim from the proto enum).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Submitted => "TASK_STATE_SUBMITTED",
            TaskState::Working => "TASK_STATE_WORKING",
            TaskState::Completed => "TASK_STATE_COMPLETED",
            TaskState::Failed => "TASK_STATE_FAILED",
            TaskState::Canceled => "TASK_STATE_CANCELED",
            TaskState::Rejected => "TASK_STATE_REJECTED",
            TaskState::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            TaskState::AuthRequired => "TASK_STATE_AUTH_REQUIRED",
            TaskState::Unspecified => "TASK_STATE_UNSPECIFIED",
        }
    }

    /// Parse a wire `TASK_STATE_*` string back into a [`TaskState`] — the A2A
    /// client reads a Task's status off a remote peer. An unrecognized string is
    /// `Unspecified` (a peer speaking a newer enum is treated as not-yet-terminal,
    /// so the client keeps polling rather than mistaking it for terminal).
    pub fn from_wire(s: &str) -> TaskState {
        match s {
            "TASK_STATE_SUBMITTED" => TaskState::Submitted,
            "TASK_STATE_WORKING" => TaskState::Working,
            "TASK_STATE_COMPLETED" => TaskState::Completed,
            "TASK_STATE_FAILED" => TaskState::Failed,
            "TASK_STATE_CANCELED" => TaskState::Canceled,
            "TASK_STATE_REJECTED" => TaskState::Rejected,
            "TASK_STATE_INPUT_REQUIRED" => TaskState::InputRequired,
            "TASK_STATE_AUTH_REQUIRED" => TaskState::AuthRequired,
            _ => TaskState::Unspecified,
        }
    }

    /// Whether this state is **terminal** — the A2A client stops polling once a
    /// Task reaches one. `Unspecified` and the input/auth-required interaction
    /// states are treated as non-terminal (keep waiting on the deadline).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled | TaskState::Rejected
        )
    }
}

/// Build the `params` for a `SendMessage` request carrying `objective` as a
/// single text `Part` of one `ROLE_USER` message (`message_id` minted by the
/// caller). The optional `output_contract` rides as a second text part so the
/// remote agent gets the same delegation contract a local subagent would.
pub fn send_message_params(
    objective: &str,
    output_contract: Option<&str>,
    message_id: &str,
) -> Value {
    let mut parts = vec![json!({ "text": objective })];
    if let Some(contract) = output_contract.filter(|c| !c.is_empty()) {
        parts.push(json!({ "text": format!("Required output: {contract}") }));
    }
    json!({
        "message": {
            "messageId": message_id,
            "role": "ROLE_USER",
            "parts": parts,
        }
    })
}

/// The `id` (task handle) of a `Task` value returned by `SendMessage` / `GetTask`.
/// Empty if absent (a malformed reply the client surfaces as an error).
pub fn task_id_of(task: &Value) -> String {
    task.get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The [`TaskState`] of a `Task` value (`status.state`). A missing/garbled status
/// reads as `Unspecified` (non-terminal — the client keeps polling).
pub fn task_state_of(task: &Value) -> TaskState {
    task.get("status")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(TaskState::from_wire)
        .unwrap_or(TaskState::Unspecified)
}

/// Concatenate the text `Part`s of a completed `Task`'s terminal artifact(s) —
/// the **distillate** the client returns to the delegating model. Parts are
/// joined with newlines, across every artifact, in order.
pub fn artifact_text_of(task: &Value) -> String {
    let mut out = String::new();
    let Some(artifacts) = task.get("artifacts").and_then(Value::as_array) else {
        return out;
    };
    for artifact in artifacts {
        if let Some(parts) = artifact.get("parts").and_then(Value::as_array) {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_wire_roundtrips_and_terminal() {
        for s in [
            "TASK_STATE_WORKING",
            "TASK_STATE_COMPLETED",
            "TASK_STATE_FAILED",
            "TASK_STATE_CANCELED",
            "TASK_STATE_REJECTED",
        ] {
            assert_eq!(TaskState::from_wire(s).as_str(), s);
        }
        assert!(TaskState::Completed.is_terminal() && !TaskState::Working.is_terminal());
        assert_eq!(
            TaskState::from_wire("TASK_STATE_NEWER"),
            TaskState::Unspecified
        );
        let p = send_message_params("do it", Some("json"), "m1");
        assert_eq!(p["message"]["parts"][0]["text"], "do it");
        assert_eq!(p["message"]["parts"][1]["text"], "Required output: json");
        let task = json!({"id": "t1", "status": {"state": "TASK_STATE_COMPLETED"}, "artifacts": [{"parts": [{"text": "a"}, {"text": "b"}]}]});
        assert_eq!(task_id_of(&task), "t1");
        assert_eq!(task_state_of(&task), TaskState::Completed);
        assert_eq!(artifact_text_of(&task), "a\nb");
    }
}
