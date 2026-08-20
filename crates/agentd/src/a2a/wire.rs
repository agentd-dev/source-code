// SPDX-License-Identifier: AGPL-3.0-only
//! **The A2A wire, built from the specification's own types.**
//!
//! agentd used to hand-write this JSON, and the failures that produced were all
//! silent: the wire is proto3 JSON, so `"agent"` where `ROLE_AGENT` belongs, or
//! epoch milliseconds where a `google.protobuf.Timestamp` belongs, is valid JSON
//! that a peer's generated types simply refuse — in the peer, in production,
//! with no error we would ever see.
//!
//! So nothing here writes JSON. Every projection constructs an [`a2a_rs`] domain
//! type — the same types a peer deserializes into, generated from the spec's
//! protocol buffers — and lets that crate serialize it. Enum spellings, field
//! names and timestamp formats stop being things we can get wrong.
//!
//! The reverse direction (a request's `Message`, a peer's `Task`) is parsed with
//! the same types, so a shape we cannot read is rejected at the edge with a real
//! error rather than silently misinterpreted.

use a2a_rs::domain::{
    Artifact, Message, Part, Role, Task as WireTask, TaskArtifactUpdateEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};
use buffa::MessageField;
use buffa_types::google::protobuf::{Struct, Timestamp};
use serde_json::{Value, json};

use crate::a2a::tasks::{State, Task};

/// A `google.protobuf.Timestamp` from the epoch milliseconds agentd stores.
pub fn stamp(ms: u64) -> Timestamp {
    Timestamp {
        seconds: (ms / 1000) as i64,
        nanos: ((ms % 1000) * 1_000_000) as i32,
        ..Default::default()
    }
}

/// The RFC 3339 rendering of an epoch-millisecond instant, as the wire carries
/// it. (Serializing through the proto type keeps one definition of "the format".)
pub fn timestamp_string(ms: u64) -> String {
    serde_json::to_value(stamp(ms))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl State {
    /// The spec's enum value for this state.
    pub fn to_wire(self) -> TaskState {
        match self {
            State::Submitted => TaskState::TASK_STATE_SUBMITTED,
            State::Working => TaskState::TASK_STATE_WORKING,
            State::InputRequired => TaskState::TASK_STATE_INPUT_REQUIRED,
            State::Completed => TaskState::TASK_STATE_COMPLETED,
            State::Failed => TaskState::TASK_STATE_FAILED,
            State::Canceled => TaskState::TASK_STATE_CANCELED,
            State::Rejected => TaskState::TASK_STATE_REJECTED,
        }
    }
}

/// A proto `Struct` from a JSON object — the spec's extension point, and the
/// only place agentd's own vocabulary appears on the wire.
fn metadata(v: Value) -> MessageField<Struct> {
    match serde_json::from_value::<Struct>(v) {
        Ok(s) => MessageField::some(s),
        // Unreachable for the objects we build; an empty extension is the right
        // degradation either way — never a malformed task.
        Err(_) => MessageField::none(),
    }
}

/// A status `Message` the agent authored, addressed to its task and context.
pub fn agent_message(task_id: &str, context_id: &str, text: &str) -> Message {
    let mut m = Message::agent_text(text.to_string(), format!("{task_id}.status"));
    m.task_id = task_id.to_string();
    m.context_id = context_id.to_string();
    m
}

/// The `TaskStatus` for a durable task: its state, when it last moved, and the
/// explanation attached to that move (an `input-required` prompt, a terminal
/// reason) when there is one.
fn status_of(t: &Task) -> TaskStatus {
    let message = t
        .message
        .as_deref()
        .map(|m| agent_message(&t.id, &t.context_id, m));
    let mut s = TaskStatus::new(t.state.to_wire(), message);
    // `TaskStatus::new` stamps *now*; the honest value is when the task moved.
    s.timestamp = MessageField::some(stamp(t.updated));
    s
}

/// What agentd knows about a task that the spec has no field for. Namespaced so
/// it cannot collide with a field the spec adds later, and confined to
/// `metadata`, so a strict peer can ignore all of it.
fn agentd_metadata(t: &Task) -> Value {
    let mut m = json!({
        "agentd/link": t.link,
        "agentd/created": timestamp_string(t.created),
    });
    if let Some(p) = &t.principal {
        m["agentd/principal"] = json!(p);
    }
    // A gate's answer shape, so a client can render the right control rather
    // than a text box. Namespaced like everything else agentd adds, so a spec
    // peer that does not know it simply ignores it.
    if let Some(sch) = &t.ask_schema {
        m["agentd/ask_schema"] = sch.clone();
    }
    if !t.history.is_empty() {
        // A proto `Struct` has one number type (double), so the stored epoch
        // milliseconds would render as `1786977070754.0`. Rendering the moment
        // the same way the spec renders every other instant is both prettier and
        // exact.
        let history: Vec<Value> = t
            .history
            .iter()
            .map(|h| {
                let mut h = h.clone();
                if let Some(ms) = h.get("ts").and_then(Value::as_u64) {
                    h["ts"] = json!(timestamp_string(ms));
                }
                h
            })
            .collect();
        m["agentd/statusHistory"] = json!(history);
    }
    m
}

/// The artifacts a task has delivered: its terminal result, plus any artifact
/// ids the surface resolves separately.
fn artifacts_of(t: &Task) -> Vec<Artifact> {
    let mut out = Vec::new();
    if let Some(r) = &t.result {
        let text = match r {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out.push(Artifact {
            artifact_id: format!("{}.result", t.id),
            parts: vec![Part::text(text)],
            ..Default::default()
        });
    }
    for a in &t.artifacts {
        out.push(Artifact {
            artifact_id: a.clone(),
            ..Default::default()
        });
    }
    out
}

/// The artifact carrying a task's terminal result, when it produced one. This
/// is what a streaming caller receives as the answer.
pub fn result_artifact(t: &Task) -> Option<Artifact> {
    artifacts_of(t)
        .into_iter()
        .next()
        .filter(|a| !a.parts.is_empty())
}

/// The full A2A `Task` — what `GetTask`, `CancelTask` and a `SendMessage` reply
/// carry.
pub fn task(t: &Task) -> WireTask {
    let mut w = WireTask::new(t.id.clone(), t.context_id.clone());
    w.status = MessageField::some(status_of(t));
    w.artifacts = artifacts_of(t);
    w.metadata = metadata(agentd_metadata(t));
    w
}

/// The light `Task` a listing carries: the same object without the artifacts,
/// which a listing does not resolve. It is a `Task` and not a summary shape of
/// our own — a peer deserializes the array as `Task`s.
pub fn task_summary(t: &Task) -> WireTask {
    let mut w = WireTask::new(t.id.clone(), t.context_id.clone());
    w.status = MessageField::some(status_of(t));
    w.metadata = metadata(agentd_metadata(t));
    w
}

/// A `TaskStatusUpdateEvent` — one frame of a stream.
///
/// This is the port-facing event type; a2a-rs converts it into the tag-free
/// `StreamResponse` union the wire actually carries, so the `kind` discriminator
/// here never reaches a peer.
pub fn status_event(
    task_id: &str,
    context_id: &str,
    state: TaskState,
    message: Option<&str>,
    at_ms: u64,
) -> TaskStatusUpdateEvent {
    let mut s = TaskStatus::new(
        state,
        message.map(|m| agent_message(task_id, context_id, m)),
    );
    s.timestamp = MessageField::some(stamp(at_ms));
    TaskStatusUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        kind: "status-update".to_string(),
        status: s,
        metadata: None,
    }
}

/// A `TaskArtifactUpdateEvent` — the other frame kind.
pub fn artifact_event(
    task_id: &str,
    context_id: &str,
    artifact: Artifact,
    last_chunk: bool,
) -> TaskArtifactUpdateEvent {
    TaskArtifactUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        kind: "artifact-update".to_string(),
        artifact,
        append: None,
        last_chunk: Some(last_chunk),
        metadata: None,
    }
}

/// The text a caller sent, concatenated across the message's text parts. A
/// non-text part (a file, a data command) contributes nothing here — commands
/// are read separately, by [`command`].
pub fn message_text(m: &Message) -> String {
    let mut out = String::new();
    for p in &m.parts {
        if let Some(a2a_rs::domain::part::Content::Text(t)) = &p.content {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// agentd's command envelope, if this message carries one: a DataPart shaped
/// `{"agentd": {"op": "...", ...}}`. This is an agentd extension riding the
/// spec's data part, not an A2A concept.
pub fn command(m: &Message) -> Option<(String, Value)> {
    for p in &m.parts {
        let Some(a2a_rs::domain::part::Content::Data(d)) = &p.content else {
            continue;
        };
        let Ok(v) = serde_json::to_value(d) else {
            continue;
        };
        let inner = v.get("data").unwrap_or(&v);
        let Some(env) = inner.get("agentd") else {
            continue;
        };
        if let Some(op) = env.get("op").and_then(Value::as_str) {
            return Some((op.to_string(), env.clone()));
        }
    }
    None
}

/// Whether a message came from the human/peer side rather than the agent.
pub fn is_from_caller(m: &Message) -> bool {
    m.role.as_known() != Some(Role::ROLE_AGENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::tasks::Link;

    /// The whole point of building the wire from their types: the spellings the
    /// spec fixes come out right without us naming them.
    #[test]
    fn the_projection_is_proto3_json() {
        let mut t = Task::new(
            "task-1",
            "ctx-1",
            Some("user:a"),
            Link::Run { id: "r1".into() },
        );
        t.set_result(json!("the answer"));
        t.transition(State::Completed, Some("done".into()));

        let v = serde_json::to_value(task(&t)).expect("serialize");
        assert_eq!(v["id"], "task-1");
        assert_eq!(v["contextId"], "ctx-1");
        assert_eq!(v["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(v["status"]["message"]["role"], "ROLE_AGENT");
        assert_eq!(v["status"]["message"]["taskId"], "task-1");
        assert!(
            v["status"]["timestamp"]
                .as_str()
                .is_some_and(|s| s.ends_with('Z')),
            "timestamps are RFC 3339: {v}"
        );
        assert_eq!(v["artifacts"][0]["artifactId"], "task-1.result");
        assert_eq!(v["artifacts"][0]["parts"][0]["text"], "the answer");
        assert_eq!(v["metadata"]["agentd/principal"], "user:a");
        assert!(v["history"].is_null(), "history is repeated Message: {v}");

        // The listing is the same object without artifacts — never a flatter
        // shape a peer would fail to read as a Task.
        let s = serde_json::to_value(task_summary(&t)).expect("serialize");
        assert_eq!(s["status"]["state"], v["status"]["state"]);
        assert!(s["state"].is_null());
        assert!(s["artifacts"].is_null());
    }

    #[test]
    fn a_task_we_emit_is_a_task_we_can_read_back() {
        let t = Task::new("t", "c", None, Link::Turn { ctx: "c".into() });
        let v = serde_json::to_value(task(&t)).unwrap();
        let back: WireTask = serde_json::from_value(v).expect("round trip through their type");
        assert_eq!(back.id, "t");
        assert_eq!(
            back.status.as_option().unwrap().state.as_known(),
            Some(TaskState::TASK_STATE_SUBMITTED)
        );
    }

    #[test]
    fn text_and_commands_are_read_from_their_parts() {
        let mut m = Message::user_text("please".into(), "m1".into());
        assert_eq!(message_text(&m), "please");
        assert!(command(&m).is_none());
        assert!(is_from_caller(&m));

        m.parts.push(Part::data(
            serde_json::from_value(json!({"agentd": {"op": "status"}})).unwrap(),
        ));
        let (op, env) = command(&m).expect("a command DataPart");
        assert_eq!(op, "status");
        assert_eq!(env["op"], "status");
        // The text half is unchanged by the command riding alongside it.
        assert_eq!(message_text(&m), "please");
    }
}
