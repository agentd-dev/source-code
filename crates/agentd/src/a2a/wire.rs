// SPDX-License-Identifier: AGPL-3.0-only
//! **The A2A wire, built from the specification's own types.**
//!
//! Hand-writing this JSON fails silently. The wire is proto3 JSON, so `"agent"`
//! where `ROLE_AGENT` belongs, or epoch milliseconds where a
//! `google.protobuf.Timestamp` belongs, is valid JSON that a peer's generated
//! types simply refuse — in the peer, in production, with no error agentd would
//! ever see.
//!
//! So nothing here writes JSON. Every projection constructs a type of the A2A
//! project's official SDK ([`a2a_types`]) — the specification's own model —
//! and every one reaches the wire through the SDK's canonical ProtoJSON
//! ([`a2a_pb::protojson_conv`]): a type's serde form is not the wire form, so
//! the SDK transcodes through the protocol buffers, as its own server does.
//! Enum spellings, field names and timestamp formats stop being things we can
//! get wrong.
//!
//! The reverse direction (a request's `Message`, a peer's `Task`) is parsed the
//! same way, so a shape we cannot read is rejected at the edge with a real
//! error rather than silently misinterpreted.

use std::collections::HashMap;

use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use a2a_types::{
    Artifact, Message, Part, PartContent, Role, StreamResponse, Task as WireTask,
    TaskArtifactUpdateEvent, TaskState, TaskStatus, TaskStatusUpdateEvent,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};

use crate::a2a::tasks::{Link, State, Task, TaskMessage};
use crate::runtime::surface::{COMMAND_EXTENSION, TASK_ANNOTATIONS_EXTENSION};

/// A protocol object as the wire carries it: canonical ProtoJSON, by the SDK's
/// own conversion. The types agentd builds always convert; `null` would mean a
/// type the SDK cannot represent, which none of ours is.
pub fn to_wire<T: ProtoJsonPayload>(value: &T) -> Value {
    protojson_conv::to_value(value).unwrap_or(Value::Null)
}

/// A protocol object read from the wire, by the SDK's own conversion.
pub fn from_wire<T: ProtoJsonPayload>(value: Value) -> Result<T, String> {
    protojson_conv::from_value(value).map_err(|e| e.to_string())
}

/// A `Message` as the wire carries it. The SDK converts whole payloads, so a
/// message travels as the `message` of a `StreamResponse`.
pub fn message_to_wire(message: &Message) -> Value {
    to_wire(&StreamResponse::Message(message.clone()))
        .get_mut("message")
        .map(Value::take)
        .unwrap_or(Value::Null)
}

/// A `Message` read from the wire (see [`message_to_wire`]).
pub fn message_from_wire(value: &Value) -> Result<Message, String> {
    match from_wire::<StreamResponse>(json!({ "message": value }))? {
        StreamResponse::Message(m) => Ok(m),
        _ => Err("not a message".to_string()),
    }
}

/// The instant an epoch-millisecond time names, as the SDK carries one.
pub fn stamp(ms: u64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(i64::try_from(ms).unwrap_or(i64::MAX)).unwrap_or_default()
}

/// The RFC 3339 rendering of an epoch-millisecond instant, as the wire carries
/// it: UTC as `Z`, the fraction only as long as it needs to be — the protobuf
/// JSON mapping of a `Timestamp`.
pub fn timestamp_string(ms: u64) -> String {
    stamp(ms).to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

impl State {
    /// The spec's enum value for this state.
    pub fn to_wire(self) -> TaskState {
        match self {
            State::Submitted => TaskState::Submitted,
            State::Working => TaskState::Working,
            State::InputRequired => TaskState::InputRequired,
            State::Completed => TaskState::Completed,
            State::Failed => TaskState::Failed,
            State::Canceled => TaskState::Canceled,
            State::Rejected => TaskState::Rejected,
        }
    }
}

/// An object's members as the spec's `metadata` — the extension point, and the
/// only place agentd's own vocabulary appears on the wire.
fn metadata(v: Value) -> Option<HashMap<String, Value>> {
    match v {
        Value::Object(o) if !o.is_empty() => Some(o.into_iter().collect()),
        _ => None,
    }
}

/// Whether a projection carries agentd's own facts about the task.
///
/// They ride only under the task-annotations URI, and only for a caller
/// that activated it: an extension a client did not ask for is not the
/// client's to parse, and a strict peer then sees nothing but the spec's
/// fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Annotations {
    Include,
    Omit,
}

/// The id of a task's `seq`-th status message. The same id while the message
/// is the task's current status and after it has moved into history, so a
/// client that rendered it once recognises it in both places.
pub fn status_message_id(task_id: &str, seq: u64) -> String {
    format!("{task_id}{STATUS_ID_INFIX}{seq}")
}

/// What every agent-authored message id carries, and no caller's may.
const STATUS_ID_INFIX: &str = ".status.";

/// Whether a caller's `messageId` falls in the agent's own namespace.
///
/// A client that rendered an id once recognises it everywhere, and dedupes by
/// it — so a caller that sent `<task>.status.<n>` first (a continuation knows
/// its task id, and the numbering is public) would shadow the agent's next
/// question in every console watching the task. Such an id is re-minted
/// rather than kept. The whole infix is reserved, not just the addressed
/// task's: which task a message will open is not the caller's to know.
pub fn is_agent_message_id(id: &str) -> bool {
    id.contains(STATUS_ID_INFIX)
}

/// The media types a caller's message may carry — the card's
/// `defaultInputModes`, and the list a refusal names.
pub const INPUT_MODES: &[&str] = &["text/plain", "application/json"];

/// A message part agentd cannot take: its declared media type, if it named
/// one. The whole message is refused for it (`-32005`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPart(pub Option<String>);

impl std::fmt::Display for UnsupportedPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "media type {} is not supported; accepted: {}",
            self.0.as_deref().unwrap_or("unspecified"),
            INPUT_MODES.join(", ")
        )
    }
}

/// What a caller's message says, as the text of a turn.
///
/// - A text part is taken as written; several are joined by newlines.
/// - A DataPart is `application/json`, which the card accepts, so it is
///   rendered for the model as a fenced JSON block — not dropped, which left a
///   JSON-only message "empty" and a mixed one silently short of its data.
///   agentd's own command envelope (`data.agentd`) is the exception: it is a
///   command, not words, and carries no text.
/// - A file — bytes or a URL — is no media type agentd takes, and refuses the
///   WHOLE message: answering a message with its file quietly removed would be
///   answering a different message.
///
/// The message is read with the spec's own `Message`; a shape that is not one
/// has no parts to read.
pub fn message_input(message: &Value) -> Result<String, UnsupportedPart> {
    let parts = message_from_wire(message)
        .map(|m| m.parts)
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        match part.content {
            PartContent::Text(text) => out.push(text),
            PartContent::Data(data) => {
                let data = whole_numbers(data);
                if data.get("agentd").is_none() {
                    let pretty = serde_json::to_string_pretty(&data).unwrap_or_default();
                    out.push(format!("```json\n{pretty}\n```"));
                }
            }
            PartContent::Raw(_) | PartContent::Url(_) => {
                return Err(UnsupportedPart(part.media_type.filter(|t| !t.is_empty())));
            }
        }
    }
    Ok(out.join("\n"))
}

/// `v` with every integral number written as an integer.
///
/// A DataPart's payload is a `google.protobuf.Value`, whose only number is a
/// double, so the `1` a caller sent comes back `1.0`. JSON has one number type
/// and the caller wrote an integer; a model reading `1.0` for a count or an id
/// is reading something the caller did not say.
fn whole_numbers(v: Value) -> Value {
    const EXACT: f64 = 9_007_199_254_740_992.0; // 2^53
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < EXACT => json!(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(whole_numbers).collect()),
        Value::Object(o) => {
            Value::Object(o.into_iter().map(|(k, v)| (k, whole_numbers(v))).collect())
        }
        other => other,
    }
}

/// A status `Message` the agent authored, addressed to its task and context.
pub fn agent_message(task_id: &str, context_id: &str, seq: u64, text: &str) -> Message {
    let mut m = Message::new(Role::Agent, vec![Part::text(text)]);
    m.message_id = status_message_id(task_id, seq);
    m.task_id = Some(task_id.to_string());
    m.context_id = Some(context_id.to_string());
    m
}

/// The `TaskStatus` for a durable task: its state, when it last moved, and the
/// explanation attached to that move (an `input-required` prompt, a terminal
/// reason) when there is one.
fn status_of(t: &Task) -> TaskStatus {
    let message = t
        .message
        .as_deref()
        .map(|m| agent_message(&t.id, &t.context_id, t.status_seq, m));
    TaskStatus {
        state: t.state.to_wire(),
        message,
        // When the task moved — not when it was read.
        timestamp: Some(stamp(t.updated)),
    }
}

/// The spec's `Task.history`: the conversation that led here, oldest first —
/// the callers' messages and the status messages the agent has moved past.
///
/// Neither the current status message (the task's `status` carries it) nor
/// the reply (the result artifact carries it) is repeated: history is what a
/// client could not otherwise see.
pub fn history_of(t: &Task) -> Vec<Message> {
    t.messages
        .iter()
        .filter_map(|m| match m {
            // Recorded from a message the SDK had already parsed, so this reads
            // back; one that somehow does not is left out rather than sent as
            // something a peer would refuse.
            TaskMessage::Inbound { message } => message_from_wire(message).ok(),
            TaskMessage::Status { seq, .. } if t.message.is_some() && *seq == t.status_seq => None,
            TaskMessage::Status { seq, text } => {
                Some(agent_message(&t.id, &t.context_id, *seq, text))
            }
        })
        .collect()
}

/// The task-annotations object: what agentd knows about a task that the
/// spec has no field for, in the shape that extension's schema publishes.
///
/// Every key is a documented name, never a serde rendering of a Rust type — a
/// client that copied `Link`'s enum spelling would break on a refactor that
/// changed nothing on the wire.
pub fn annotations(t: &Task) -> Value {
    let (kind, id) = match &t.link {
        Link::Run { id } => ("run", id),
        Link::Subagent { handle } => ("subagent", handle),
        Link::Turn { ctx } => ("turn", ctx),
    };
    // A proto `Struct` has one number type (double), so the stored epoch
    // milliseconds would render as `1786977070754.0`. Rendering the moment
    // the same way the spec renders every other instant is both prettier and
    // exact.
    let status_history: Vec<Value> = t
        .history
        .iter()
        .map(|h| {
            let ts = h.get("ts").and_then(Value::as_u64).unwrap_or(0);
            json!({"state": h["state"], "ts": timestamp_string(ts)})
        })
        .collect();
    let mut a = json!({
        "link": {"kind": kind, "id": id},
        "created": timestamp_string(t.created),
        "statusHistory": status_history,
    });
    if let Some(p) = &t.principal {
        a["principal"] = json!(p);
    }
    // A gate's answer shape, so a client can render the right control rather
    // than a text box.
    if let Some(sch) = &t.ask_schema {
        a["askSchema"] = sch.clone();
    }
    // Where the gate is also announced, for a client that shows it.
    if let Some(ch) = &t.ask_channel {
        a["askChannel"] = json!(ch);
    }
    if let Some(op) = &t.command {
        a["command"] = json!(op);
    }
    // The span the task's work is done in, so a client can find its work in
    // agentd's lines and traces — the span a `traceparent` response header
    // names too.
    if let Some(tc) = &t.trace {
        a["traceId"] = json!(tc.trace_id);
        a["spanId"] = json!(tc.span_id);
    }
    a
}

/// The artifact carrying a task's terminal result, when it produced one. This
/// is what a streaming caller receives as the answer.
///
/// Prose is a text part. Anything else is a JSON DataPart — a structured
/// result is data, and stringifying it into text made every client parse
/// prose to get it back. A command's result is marked with the command
/// extension, whose schema defines it. A task with no result has no artifact:
/// the spec requires every artifact to have parts, so an empty one is never
/// emitted.
///
/// A DataPart holds a `google.protobuf.Value`, whose one number type is a
/// double: `1` is carried as the number `1.0`. That is the spec's own
/// representation, and the SDK serializes every task reply, so it is not ours
/// to vary per path.
pub fn result_artifact(t: &Task) -> Option<Artifact> {
    let part = match t.result.as_ref()? {
        Value::Null => return None,
        Value::String(s) => Part::text(s.clone()),
        other => Part::data(other.clone()).with_media_type("application/json"),
    };
    let extensions = match (&part.content, &t.command) {
        (PartContent::Data(_), Some(_)) => Some(vec![COMMAND_EXTENSION.to_string()]),
        _ => None,
    };
    Some(Artifact {
        artifact_id: format!("{}.result", t.id),
        name: None,
        description: None,
        parts: vec![part],
        metadata: None,
        extensions,
    })
}

/// The full A2A `Task` — what `GetTask`, `CancelTask` and a `SendMessage` reply
/// carry.
pub fn task(t: &Task, ann: Annotations) -> WireTask {
    let history = history_of(t);
    WireTask {
        id: t.id.clone(),
        context_id: t.context_id.clone(),
        status: status_of(t),
        artifacts: result_artifact(t).map(|a| vec![a]),
        history: (!history.is_empty()).then_some(history),
        metadata: match ann {
            Annotations::Include => {
                metadata(json!({ (TASK_ANNOTATIONS_EXTENSION): annotations(t) }))
            }
            Annotations::Omit => None,
        },
    }
}

/// `historyLength`: the newest `n` messages of a task's history kept, the rest
/// dropped; `0` keeps none.
pub fn limit_history(w: &mut WireTask, n: u32) {
    if let Some(h) = w.history.as_mut() {
        let skip = h.len().saturating_sub(n as usize);
        h.drain(..skip);
        if h.is_empty() {
            w.history = None;
        }
    }
}

/// The most history messages a `task` feed event carries.
pub const FEED_HISTORY_MESSAGES: usize = 4;
/// The most bytes (serialized) those messages may hold together.
pub const FEED_HISTORY_BYTES: usize = 32 * 1024;

/// The task a `task` feed event carries: the full `Task` with its history cut
/// to the newest [`FEED_HISTORY_MESSAGES`] within [`FEED_HISTORY_BYTES`].
///
/// The feed is a ring every subscriber replays, and a transition fires an
/// event, so a whole conversation per event would multiply one long prompt by
/// every step of its task. The newest few are what a watching client has not
/// seen yet; one that attaches late reads the rest with `GetTask`.
pub fn task_for_feed(t: &Task) -> WireTask {
    let mut w = task(t, Annotations::Include);
    let size = |m: &Message| message_to_wire(m).to_string().len();
    let mut history = w.history.take().unwrap_or_default();
    let skip = history.len().saturating_sub(FEED_HISTORY_MESSAGES);
    let mut kept: Vec<Message> = history.drain(skip..).collect();
    let mut total: usize = kept.iter().map(size).sum();
    while total > FEED_HISTORY_BYTES && !kept.is_empty() {
        total -= size(&kept.remove(0));
    }
    w.history = (!kept.is_empty()).then_some(kept);
    w
}

/// The largest page any listing answers (`ListTasks`,
/// `ListTaskPushNotificationConfigs`): the spec's bound on `pageSize`.
pub const MAX_PAGE_SIZE: i32 = 100;
/// The page a listing answers when the caller named no `pageSize`.
pub const DEFAULT_PAGE_SIZE: i32 = 50;

/// How a `ListTasks` caller asked to see each task it is handed.
///
/// Both halves default to *less*: the spec makes a listing an index, not a
/// bulk download, so the artifacts and the history are there only when asked
/// for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListView {
    /// `historyLength`: unset omits the history; `n` keeps the newest `n`.
    pub history_length: Option<u32>,
    /// `includeArtifacts`: the artifacts ride along only when this is set.
    pub include_artifacts: bool,
}

/// A `Task` as a listing carries it: the same object `GetTask` returns, cut to
/// what the [`ListView`] asked for. It is a `Task` and not a summary shape of
/// our own — a peer deserializes the array as `Task`s.
pub fn task_listed(t: &Task, view: ListView, ann: Annotations) -> WireTask {
    let mut w = task(t, ann);
    if !view.include_artifacts {
        w.artifacts = None;
    }
    match view.history_length {
        None => w.history = None,
        Some(n) => limit_history(&mut w, n),
    }
    w
}

/// The body of one push delivery: a `StreamResponse` carrying the task, the
/// same union a streaming caller reads, so one handler can serve both ways of
/// being told (spec §4.3.3).
///
/// The whole current task rather than the event that fired: "the task, as it
/// now is" is a valid `StreamResponse`, keeps the artifacts, and lets a
/// receiver check `task.id` against what it registered for. The annotations
/// and the history are left out — a webhook is a notification to a receiver
/// that is not a party to the conversation. Its history holds every party's
/// messages (an operator's gate answer, a continuation), which are not the
/// receiver's to read, and re-sending up to the whole bound on every
/// transition would make each state change a bulk upload. The current
/// status message still rides in `status`; a party that wants the
/// conversation reads it with `GetTask`.
pub fn push_body(t: &Task) -> Value {
    let mut w = task(t, Annotations::Omit);
    w.history = None;
    to_wire(&StreamResponse::Task(w))
}

/// A `TaskStatusUpdateEvent` — one frame of a stream: the task's status as it
/// now is, built by the same [`status_of`] `GetTask` uses, so the message a
/// streaming caller sees carries the id it will find in history later.
pub fn status_event(t: &Task) -> TaskStatusUpdateEvent {
    TaskStatusUpdateEvent {
        task_id: t.id.clone(),
        context_id: t.context_id.clone(),
        status: status_of(t),
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
        artifact,
        append: None,
        last_chunk: Some(last_chunk),
        metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::tasks::Link;

    /// A message's parts, as a turn reads them: text as written, JSON data as
    /// a fenced block, the command envelope as no text at all — and a file,
    /// anywhere in the message, refuses all of it with the media type it named.
    #[test]
    fn message_input_parts() {
        let m = |parts: Value| json!({"role": "ROLE_USER", "messageId": "m", "parts": parts});
        assert_eq!(
            message_input(&m(json!([{"text": "a"}, {"text": "b"}]))),
            Ok("a\nb".to_string())
        );
        assert_eq!(
            message_input(&m(json!([{"text": "look:"}, {"data": {"k": [1, 2.5]}}]))),
            Ok("look:\n```json\n{\n  \"k\": [\n    1,\n    2.5\n  ]\n}\n```".to_string())
        );
        assert_eq!(
            message_input(&m(json!([{"data": {"agentd": {"op": "status"}}}]))),
            Ok(String::new())
        );
        assert_eq!(message_input(&m(json!([]))), Ok(String::new()));

        let url = json!({"url": "https://h.example/cat.png", "mediaType": "image/png"});
        let refused = message_input(&m(json!([{"text": "see"}, url]))).unwrap_err();
        assert_eq!(refused, UnsupportedPart(Some("image/png".into())));
        assert_eq!(
            refused.to_string(),
            "media type image/png is not supported; accepted: text/plain, application/json"
        );
        // Bytes, with no media type named, anywhere in the message.
        let raw = message_input(&m(json!([{"raw": "aGk="}, {"text": "after"}]))).unwrap_err();
        assert_eq!(
            raw.to_string(),
            "media type unspecified is not supported; accepted: text/plain, application/json"
        );
    }

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

        let v = to_wire(&task(&t, Annotations::Include));
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
        assert_eq!(
            v["metadata"][TASK_ANNOTATIONS_EXTENSION]["principal"],
            "user:a"
        );

        // The listing is the same object without artifacts — never a flatter
        // shape a peer would fail to read as a Task.
        let s = to_wire(&task_listed(&t, ListView::default(), Annotations::Include));
        assert_eq!(s["status"]["state"], v["status"]["state"]);
        assert!(s["state"].is_null());
        assert!(s["artifacts"].is_null());
        // …and asked for, the artifacts come back exactly as `GetTask` has them.
        let with = task_listed(
            &t,
            ListView {
                include_artifacts: true,
                history_length: Some(0),
            },
            Annotations::Include,
        );
        let with = to_wire(&with);
        assert_eq!(with["artifacts"], v["artifacts"]);
    }

    /// `Task.history` is the conversation: the caller's messages as they sent
    /// them (re-addressed to the task) and the status messages the task has
    /// moved past, in the order they were said — never the current status or
    /// the reply, which the task already carries. A status message has the
    /// same id in `status` and, later, in history. `historyLength` keeps the
    /// newest, and a feed event carries only the newest few.
    #[test]
    fn the_projection_carries_history() {
        let mut t = Task::new("t1", "c1", Some("user:a"), Link::Turn { ctx: "c1".into() });
        let prompt =
            json!({"messageId": "m-1", "role": "ROLE_USER", "parts": [{"text": "Do the thing"}]});
        t.record_inbound(&prompt);
        t.transition(State::InputRequired, Some("Proceed?".into()));

        // While the question is current, it is the status — not history.
        let v = to_wire(&task(&t, Annotations::Omit));
        assert_eq!(v["history"].as_array().map(Vec::len), Some(1), "{v}");
        assert_eq!(v["history"][0]["messageId"], "m-1");
        assert_eq!(v["history"][0]["role"], "ROLE_USER");
        assert_eq!(v["history"][0]["taskId"], "t1");
        assert_eq!(v["history"][0]["contextId"], "c1");
        assert_eq!(v["history"][0]["parts"][0]["text"], "Do the thing");
        let question_id = v["status"]["message"]["messageId"].clone();
        assert_eq!(question_id, json!(status_message_id("t1", 1)));

        // Answered and finished: the question moves into history under the
        // id it had as the status, between the prompt and the answer.
        let answer = json!({"messageId": "m-2", "role": "ROLE_USER", "parts": [{"text": "yes"}]});
        t.record_inbound(&answer);
        t.transition(State::Working, Some("answered".into()));
        t.set_result(json!("Done."));
        t.transition(State::Completed, None);
        let v = to_wire(&task(&t, Annotations::Omit));
        let ids: Vec<&str> = v["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["messageId"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["m-1", question_id.as_str().unwrap(), "m-2"], "{v}");
        assert_eq!(v["history"][1]["role"], "ROLE_AGENT");
        assert_eq!(v["history"][1]["parts"][0]["text"], "Proceed?");
        assert!(
            !v["history"].to_string().contains("Done."),
            "the reply is the artifact, not history: {v}"
        );
        assert_eq!(
            v["status"]["message"]["messageId"],
            json!(status_message_id("t1", 2))
        );
        // A stream's status frame names the message exactly as `GetTask` does.
        let ev = to_wire(&StreamResponse::StatusUpdate(status_event(&t)));
        assert_eq!(
            ev["statusUpdate"]["status"]["message"]["messageId"],
            v["status"]["message"]["messageId"]
        );

        // It reads back through the spec's own type, history included.
        let mut back: WireTask = from_wire(v.clone()).expect("a Task");
        assert_eq!(back.history.as_ref().map(Vec::len), Some(3));
        // `historyLength` keeps the newest.
        limit_history(&mut back, 2);
        assert_eq!(
            back.history.as_ref().unwrap()[0].message_id,
            question_id.as_str().unwrap()
        );

        // A feed event carries the newest few, within its byte bound.
        for n in 0..10 {
            t.record_inbound(
                &json!({"messageId": format!("x{n}"), "parts": [{"text": "x".repeat(10 * 1024)}]}),
            );
        }
        let f = task_for_feed(&t);
        let h = f.history.unwrap_or_default();
        assert_eq!(h.len(), 3, "four 10 KiB messages do not fit in 32 KiB");
        assert_eq!(h.last().unwrap().message_id, "x9");
        let small = Task::new("t2", "c2", None, Link::Turn { ctx: "c2".into() });
        let mut small = small;
        for n in 0..10 {
            small.record_inbound(&json!({"messageId": format!("s{n}"), "parts": [{"text": "hi"}]}));
        }
        let f = task_for_feed(&small);
        let h = f.history.unwrap_or_default();
        let ids: Vec<&str> = h.iter().map(|m| m.message_id.as_str()).collect();
        assert_eq!(ids, ["s6", "s7", "s8", "s9"]);
    }

    /// agentd's facts about a task are the task-annotations object under
    /// that extension's URI, in its documented shape — present only when the
    /// caller activated it, never on a webhook, and never under an ad-hoc
    /// `agentd/` key anywhere.
    #[test]
    fn task_annotations_are_uri_keyed() {
        let mut t = Task::new(
            "t",
            "c",
            Some("user:a"),
            Link::Subagent {
                handle: "h-1".into(),
            },
        );
        t.ask_schema = Some(json!({"type": "boolean"}));
        t.command = Some("workflow.run".into());
        t.record_inbound(&json!({"messageId": "m", "parts": [{"text": "go"}]}));
        t.set_result(json!({"ok": true}));
        t.transition(State::Completed, Some("done".into()));

        let full = to_wire(&task(&t, Annotations::Include));
        let meta = full["metadata"].as_object().expect("metadata");
        assert_eq!(
            meta.keys().collect::<Vec<_>>(),
            [TASK_ANNOTATIONS_EXTENSION]
        );
        let a = &full["metadata"][TASK_ANNOTATIONS_EXTENSION];
        assert_eq!(a["link"], json!({"kind": "subagent", "id": "h-1"}));
        assert_eq!(a["principal"], "user:a");
        assert_eq!(a["command"], "workflow.run");
        assert_eq!(a["askSchema"], json!({"type": "boolean"}));
        assert!(
            a["created"].as_str().is_some_and(|s| s.ends_with('Z')),
            "{a}"
        );
        let hist = a["statusHistory"].as_array().expect("statusHistory");
        assert_eq!(hist[0]["state"], "TASK_STATE_SUBMITTED");
        assert_eq!(hist.last().unwrap()["state"], "TASK_STATE_COMPLETED");
        assert!(
            hist.iter()
                .all(|h| h["ts"].as_str().is_some_and(|s| s.ends_with('Z'))),
            "{a}"
        );
        for (link, kind, id) in [
            (Link::Run { id: "r".into() }, "run", "r"),
            (Link::Turn { ctx: "c".into() }, "turn", "c"),
        ] {
            let mut u = t.clone();
            u.link = link;
            assert_eq!(annotations(&u)["link"], json!({"kind": kind, "id": id}));
        }

        // Not asked for, not there.
        let bare = to_wire(&task(&t, Annotations::Omit));
        assert!(bare.get("metadata").is_none(), "{bare}");

        // No projection anywhere carries an `agentd/` key.
        let every = [
            full,
            bare,
            to_wire(&task_listed(
                &t,
                ListView {
                    include_artifacts: true,
                    history_length: None,
                },
                Annotations::Include,
            )),
            to_wire(&task_for_feed(&t)),
            push_body(&t),
        ];
        for v in &every {
            assert!(!v.to_string().contains("\"agentd/"), "an agentd/ key: {v}");
        }
        assert!(push_body(&t)["task"].get("metadata").is_none());
    }

    /// A structured result is a JSON DataPart, marked with the command
    /// extension when a command produced it; prose stays a text part with no
    /// marking; and a task without a result has no artifact at all — never
    /// one without parts.
    #[test]
    fn command_results_are_marked_data_parts() {
        let done = |command: Option<&str>, result: Option<Value>| {
            let mut t = Task::new("t", "c", None, Link::Turn { ctx: "c".into() });
            t.command = command.map(str::to_string);
            if let Some(r) = result {
                t.set_result(r);
            }
            t.transition(State::Completed, None);
            to_wire(&task(&t, Annotations::Omit))
        };

        let v = done(
            Some("admin.set"),
            Some(json!({"applied": ["a2a.push.enabled"]})),
        );
        let art = &v["artifacts"][0];
        assert_eq!(art["artifactId"], "t.result");
        assert_eq!(
            art["parts"][0]["data"],
            json!({"applied": ["a2a.push.enabled"]})
        );
        assert_eq!(art["parts"][0]["mediaType"], "application/json");
        assert!(art["parts"][0].get("text").is_none(), "{art}");
        assert_eq!(art["extensions"], json!([COMMAND_EXTENSION]));

        // Data a conversation produced is data, but no command defined it.
        let v = done(None, Some(json!({"n": 1})));
        assert!(v["artifacts"][0]["parts"][0]["data"].is_object(), "{v}");
        assert!(v["artifacts"][0].get("extensions").is_none(), "{v}");

        // Prose is text, whoever produced it.
        let v = done(Some("workflow.run"), Some(json!("all done")));
        assert_eq!(v["artifacts"][0]["parts"][0]["text"], "all done");
        assert!(v["artifacts"][0].get("extensions").is_none(), "{v}");

        // No result, no artifact.
        for v in [
            done(Some("admin.drain"), None),
            done(None, Some(Value::Null)),
        ] {
            assert!(v.get("artifacts").is_none(), "no part-less artifact: {v}");
        }

        // The stream's artifact frame is the same artifact.
        let mut t = Task::new("t", "c", None, Link::Turn { ctx: "c".into() });
        t.command = Some("admin.set".into());
        t.set_result(json!({"k": "v"}));
        let a = result_artifact(&t).expect("an artifact");
        assert_eq!(a.extensions, Some(vec![COMMAND_EXTENSION.to_string()]));
        assert_eq!(a.parts.len(), 1);
    }

    /// A delivery is a `StreamResponse` with the task set — what the spec says
    /// a webhook receives — and it reads back through the spec's own type,
    /// which is the only proof that counts: a receiver built from the proto
    /// accepts it.
    #[test]
    fn a_push_body_is_a_stream_response_carrying_the_task() {
        let mut t = Task::new(
            "task-7",
            "ctx-7",
            Some("user:a"),
            Link::Run { id: "r".into() },
        );
        t.record_inbound(&json!({"messageId": "m1", "parts": [{"text": "a caller's prompt"}]}));
        t.transition(State::InputRequired, Some("Proceed?".into()));
        t.transition(State::Working, Some("going".into()));
        t.set_result(json!("answer"));
        t.transition(State::Completed, None);
        assert!(!history_of(&t).is_empty(), "the task has a conversation");
        let body = push_body(&t);
        // The conversation stays with its parties: none of it rides a push.
        assert!(body["task"].get("history").is_none(), "{body}");
        assert_eq!(body["task"]["id"], "task-7", "{body}");
        assert_eq!(body["task"]["status"]["state"], "TASK_STATE_COMPLETED");
        assert!(body.get("id").is_none(), "not a bare Task: {body}");
        assert!(body["task"].get("metadata").is_none(), "{body}");
        let back: StreamResponse = from_wire(body).expect("a StreamResponse");
        match back {
            StreamResponse::Task(t) => assert_eq!(t.id, "task-7"),
            other => panic!("the task variant, got {other:?}"),
        }
    }

    #[test]
    fn a_task_we_emit_is_a_task_we_can_read_back() {
        let t = Task::new("t", "c", None, Link::Turn { ctx: "c".into() });
        let v = to_wire(&task(&t, Annotations::Include));
        let back: WireTask = from_wire(v).expect("round trip through their type");
        assert_eq!(back.id, "t");
        assert_eq!(back.status.state, TaskState::Submitted);
    }

    /// Every id the agent mints is in the reserved namespace, whatever the
    /// task is called; an ordinary client id is not.
    #[test]
    fn agent_message_ids_are_reserved() {
        for (task, seq) in [("t", 1), ("a2a-7", 42), ("x.y", 3)] {
            assert!(is_agent_message_id(&status_message_id(task, seq)));
        }
        for id in ["m1", "7f0c-uuid", "status.1", "t.result"] {
            assert!(!is_agent_message_id(id), "{id}");
        }
    }
}
