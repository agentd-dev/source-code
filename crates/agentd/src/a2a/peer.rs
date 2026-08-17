// SPDX-License-Identifier: AGPL-3.0-only
//! **Talking to another agent**: the outbound half of A2A, in the
//! specification's types.
//!
//! What agentd sends a peer and what it reads back are [`a2a_rs`] domain types —
//! the same ones the peer's own generated code uses. Nothing here writes or
//! pattern-matches JSON by hand, which is the point: a delegation that fails
//! because we spelled `role` or a task state our own way would fail *silently*,
//! in the peer, as a message that never arrived or a task that never looked
//! finished.
//!
//! What is **not** delegated to a2a-rs is the transport. agentd presents real
//! credentials to a peer — a bearer that may be an OAuth token it refreshes, an
//! mTLS client identity, a per-request AWS SigV4 signature, an AAuth HTTP
//! Message Signature (RFC 0031 §7) — and it does so from a blocking turn worker
//! with no async runtime in sight. a2a-rs's own client is reqwest with a static
//! bearer, so adopting it would mean dropping four kinds of peer authentication
//! to gain a wire format we can build from its types anyway. The types are the
//! part that has to be right; the socket is agentd's.

use a2a_rs::domain::{Message, Part, Task, TaskState};
use serde_json::{Value, json};

/// The `SendMessage` params for delegating an objective.
///
/// The objective is one text part; an output contract, when the caller has one,
/// is a second — a peer sees two parts of one message, which is how the spec
/// carries a prompt with a constraint attached.
pub fn send_message_params(
    objective: &str,
    output_contract: Option<&str>,
    message_id: &str,
) -> Value {
    let mut m = Message::user_text(objective.to_string(), message_id.to_string());
    if let Some(contract) = output_contract.filter(|c| !c.is_empty()) {
        m.parts
            .push(Part::text(format!("Required output: {contract}")));
    }
    json!({ "message": serde_json::to_value(&m).unwrap_or(Value::Null) })
}

/// A `Task` a peer sent us, read with the spec's own type.
///
/// `None` is a reply we could not read as a task at all — which the caller
/// surfaces as an error rather than treating as "not finished yet", because a
/// peer we cannot parse is not a peer we should keep polling.
pub fn task_of(v: &Value) -> Option<Task> {
    let body = match v.get("task") {
        Some(t) => t,
        None => v,
    };
    serde_json::from_value(body.clone()).ok()
}

/// The task handle to poll, from a reply that may or may not be one.
pub fn task_id_of(v: &Value) -> String {
    task_of(v).map(|t| t.id).unwrap_or_default()
}

/// Where a task has got to. An unreadable or absent status reads as
/// unspecified — non-terminal, so the caller keeps waiting rather than
/// declaring a result it does not have.
pub fn task_state_of(v: &Value) -> TaskState {
    task_of(v)
        .and_then(|t| t.status.as_option().and_then(|s| s.state.as_known()))
        .unwrap_or(TaskState::TASK_STATE_UNSPECIFIED)
}

/// Whether a state ends the delegation.
pub fn is_terminal(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::TASK_STATE_COMPLETED
            | TaskState::TASK_STATE_FAILED
            | TaskState::TASK_STATE_CANCELED
            | TaskState::TASK_STATE_REJECTED
    )
}

/// The **distillate**: the text of a finished task's artifacts, in order,
/// newline-joined. This is what the delegating model receives as the answer, so
/// it is deliberately the whole of what the peer produced rather than the first
/// artifact — a peer that answers in two parts has not answered in one.
pub fn artifact_text_of(v: &Value) -> String {
    let Some(task) = task_of(v) else {
        return String::new();
    };
    let mut out = String::new();
    for artifact in &task.artifacts {
        for part in &artifact.parts {
            if let Some(a2a_rs::domain::part::Content::Text(t)) = &part.content {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}

/// How a peer's terminal state reads to the caller.
pub fn describe(state: TaskState) -> &'static str {
    match state {
        TaskState::TASK_STATE_COMPLETED => "completed",
        TaskState::TASK_STATE_FAILED => "failed",
        TaskState::TASK_STATE_CANCELED => "canceled",
        TaskState::TASK_STATE_REJECTED => "rejected",
        TaskState::TASK_STATE_INPUT_REQUIRED => "input-required",
        TaskState::TASK_STATE_AUTH_REQUIRED => "auth-required",
        TaskState::TASK_STATE_WORKING => "working",
        TaskState::TASK_STATE_SUBMITTED => "submitted",
        _ => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_objective_goes_out_as_a_user_message() {
        let p = send_message_params("summarise the incident", Some("one paragraph"), "m1");
        let m = &p["message"];
        // Built by their type, so the role is the proto enum name — a peer that
        // deserializes strictly would refuse anything else.
        assert_eq!(m["role"], "ROLE_USER");
        assert_eq!(m["messageId"], "m1");
        assert_eq!(m["parts"][0]["text"], "summarise the incident");
        assert_eq!(m["parts"][1]["text"], "Required output: one paragraph");
        // …and it round-trips back through the same type.
        let back: Message = serde_json::from_value(m.clone()).expect("their Message");
        assert_eq!(back.parts.len(), 2);
    }

    #[test]
    fn a_peers_reply_is_read_with_the_specs_type() {
        let reply = json!({"task": {
            "id": "t-9",
            "contextId": "c-1",
            "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2026-08-17T14:00:00Z"},
            "artifacts": [
                {"artifactId": "a1", "parts": [{"text": "first"}]},
                {"artifactId": "a2", "parts": [{"text": "second"}]}
            ]
        }});
        assert_eq!(task_id_of(&reply), "t-9");
        assert_eq!(task_state_of(&reply), TaskState::TASK_STATE_COMPLETED);
        assert!(is_terminal(task_state_of(&reply)));
        assert_eq!(artifact_text_of(&reply), "first\nsecond");
    }

    #[test]
    fn an_unfinished_or_unreadable_reply_is_not_mistaken_for_an_answer() {
        let working = json!({"task": {"id": "t", "contextId": "c", "status": {"state": "TASK_STATE_WORKING"}}});
        assert!(!is_terminal(task_state_of(&working)));
        assert_eq!(artifact_text_of(&working), "");

        // A reply we cannot read is unspecified — non-terminal, so the caller
        // waits (and eventually times out) instead of returning an empty answer
        // as though the peer had finished.
        let garbage = json!({"nope": true});
        assert_eq!(task_state_of(&garbage), TaskState::TASK_STATE_UNSPECIFIED);
        assert!(!is_terminal(task_state_of(&garbage)));
    }
}
