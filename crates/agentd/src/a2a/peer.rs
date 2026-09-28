// SPDX-License-Identifier: AGPL-3.0-only
//! **Talking to another agent**: the outbound half of A2A, in the
//! specification's types.
//!
//! What agentd sends a peer and what it reads back are [`a2a_rs`] domain types —
//! the same ones the peer's own generated code uses. Nothing here writes or
//! pattern-matches JSON by hand, which is the point: a delegation that fails
//! because we spelled `role` or a task state our own way would fail *silently*,
//! in the peer, as a message that never arrived or a task that never looked
//! finished. (It did: `a2a.send` once wrote `"role": "user"` by hand, and every
//! 1.0 peer — agentd included — refused it as an unknown enum name.)
//!
//! The one deliberate exception is a DataPart's document. The SDK holds it as a
//! `google.protobuf.Value`, whose only number is a double, so a sequence cursor
//! would travel as `3.0` and one past 2^53 would travel wrong. The part is
//! built typed, and its document is placed into the serialized part afterwards
//! — the same rule [`crate::a2a::reply`] follows on the way in.
//!
//! What is **not** delegated to a2a-rs is the transport. agentd presents real
//! credentials to a peer — a bearer that may be an OAuth token it refreshes, an
//! mTLS client identity, a per-request AWS SigV4 signature, an AAuth HTTP
//! Message Signature — and it does so from a blocking turn worker
//! with no async runtime in sight. a2a-rs's own client is reqwest with a static
//! bearer, so adopting it would mean dropping four kinds of peer authentication
//! to gain a wire format we can build from its types anyway. The types are the
//! part that has to be right; the socket is agentd's.

use a2a_rs::domain::generated::{
    GetTaskRequest, SendMessageConfiguration, SendMessageRequest, TaskArtifactUpdateEvent,
};
use a2a_rs::domain::{
    AgentCard, AgentInterface, Message, PROTOCOL_BINDING_JSONRPC, Part, Role, Task, TaskState,
    part::Content,
};
use buffa::MessageField;
use serde_json::{Value, json};

use crate::runtime::surface::{A2A_PROTOCOL_VERSION, COMMAND_EXTENSION, UNIX_BINDING};

/// A message on its way out, with the DataPart documents that are placed into
/// it after serialization (by part index) rather than carried through the
/// SDK's double-only `Value`.
struct Outgoing {
    message: Message,
    documents: Vec<(usize, Value)>,
}

impl Outgoing {
    fn new(message_id: &str) -> Outgoing {
        Outgoing {
            message: Message {
                role: Role::ROLE_USER.into(),
                message_id: message_id.to_string(),
                ..Default::default()
            },
            documents: Vec::new(),
        }
    }

    /// Append a DataPart whose document is `doc`, byte for byte.
    fn push_document(&mut self, doc: Value) {
        self.documents.push((self.message.parts.len(), doc));
        self.message.parts.push(Part::data(Default::default()));
    }

    /// The `SendMessageRequest` params, as the SDK serializes them.
    ///
    /// `return_immediately` is the unary `SendMessage` contract: agentd never
    /// wants the peer to hold the connection until the task settles — a send is
    /// done once accepted, and a delegation polls — so it says so explicitly
    /// rather than inheriting a peer's blocking default. A streaming send has
    /// no such choice to make and carries no configuration.
    fn into_params(self, tenant: Option<&str>, return_immediately: bool) -> Value {
        let mut req = SendMessageRequest {
            tenant: tenant.unwrap_or_default().to_string(),
            message: MessageField::some(self.message),
            ..Default::default()
        };
        if return_immediately {
            req.configuration = MessageField::some(SendMessageConfiguration {
                return_immediately: true,
                ..Default::default()
            });
        }
        let mut v = serde_json::to_value(&req).unwrap_or(Value::Null);
        for (i, doc) in self.documents {
            if let Some(part) = v
                .pointer_mut(&format!("/message/parts/{i}"))
                .and_then(Value::as_object_mut)
            {
                part.insert("data".to_string(), doc);
            }
        }
        v
    }
}

/// Is this part agentd's command envelope (`{"data": {"agentd": {"op": …}}}`)?
fn is_command_part(part: &Value) -> bool {
    part.pointer("/data/agentd/op").is_some()
}

/// The `SendMessage` / `SendStreamingMessage` params for delegating an
/// objective.
///
/// The objective is one text part; an output contract, when the caller has one,
/// is a further text part — a peer sees parts of one message, which is how the
/// spec carries a prompt with a constraint attached. A `command` envelope
/// (`{"op": …, …args}`) rides as the COMMAND DataPart the peer's `a2a` start
/// nodes match on: text conventions reach the peer's MODEL, a DataPart reaches
/// its REGISTRY, deterministically. A message carrying one is marked with the
/// command extension in `Message.extensions`, which is how a 1.0 peer tells a
/// command from a message that merely contains data.
pub fn send_message_params_cmd(
    objective: &str,
    command: Option<&Value>,
    output_contract: Option<&str>,
    message_id: &str,
    tenant: Option<&str>,
    return_immediately: bool,
) -> Value {
    let mut out = Outgoing::new(message_id);
    // A command-only message: no empty text part on the wire.
    if !objective.trim().is_empty() {
        out.message.parts.push(Part::text(objective.to_string()));
    }
    if let Some(env) = command {
        out.push_document(json!({ "agentd": env }));
        out.message.extensions = vec![COMMAND_EXTENSION.to_string()];
    }
    if let Some(contract) = output_contract.filter(|c| !c.is_empty()) {
        out.message
            .parts
            .push(Part::text(format!("Required output: {contract}")));
    }
    out.into_params(tenant, return_immediately)
}

/// The `SendMessage` params for a fire-and-forget send of caller-supplied
/// `parts`: a typed `ROLE_USER` message with `returnImmediately`.
///
/// A caller may hand over a parts ARRAY, a bare string (the common case: one
/// text part), nothing, or a lone document (one DataPart). Each part is read
/// as the spec's `Part`, so a part no peer could parse is refused here, by
/// index, instead of travelling to be refused there. A message that carries a
/// command part is marked with the command extension, exactly as
/// [`send_message_params_cmd`] marks one.
pub fn send_message_params_parts(
    parts: &Value,
    context: Option<&str>,
    message_id: &str,
    tenant: Option<&str>,
) -> Result<Value, String> {
    let raw: Vec<Value> = match parts {
        Value::Array(a) => a.clone(),
        Value::String(t) => vec![json!({ "text": t })],
        Value::Null => Vec::new(),
        other => vec![json!({ "data": other })],
    };
    let mut out = Outgoing::new(message_id);
    if let Some(ctx) = context.filter(|c| !c.is_empty()) {
        out.message.context_id = ctx.to_string();
    }
    for (i, p) in raw.iter().enumerate() {
        let part: Part = serde_json::from_value(p.clone())
            .map_err(|e| format!("a2a: part {i} is not an A2A Part: {e}"))?;
        if let Some(doc) = p
            .get("data")
            .filter(|_| matches!(part.content, Some(Content::Data(_))))
        {
            out.documents.push((out.message.parts.len(), doc.clone()));
        }
        out.message.parts.push(part);
    }
    if raw.iter().any(is_command_part) {
        out.message.extensions = vec![COMMAND_EXTENSION.to_string()];
    }
    Ok(out.into_params(tenant, true))
}

/// The extensions a `SendMessage` request's message is marked with — which are
/// exactly the ones its `A2A-Extensions` header must activate. Read from the
/// message itself so the header can never disagree with the mark.
pub fn extensions_of(params: &Value) -> Vec<String> {
    params
        .get("message")
        .and_then(|m| serde_json::from_value::<Message>(m.clone()).ok())
        .map(|m| m.extensions)
        .unwrap_or_default()
}

/// The `GetTask` params for polling `task_id`, echoing the tenant the peer's
/// interface declared.
pub fn get_task_params(task_id: &str, tenant: Option<&str>) -> Value {
    let req = GetTaskRequest {
        tenant: tenant.unwrap_or_default().to_string(),
        id: task_id.to_string(),
        ..Default::default()
    };
    serde_json::to_value(&req).unwrap_or(Value::Null)
}

/// What a peer answered a `SendMessage` with: the spec allows either a task
/// (work is tracked) or a message (the answer is immediate — agentd's own read
/// commands reply this way, creating no task).
pub enum Reply {
    Task(Task),
    Message(Message),
}

/// Read a reply as a task or a message. A bare object is a task (`GetTask`
/// answers with one unwrapped).
pub fn reply_of(v: &Value) -> Option<Reply> {
    if let Some(m) = v.get("message") {
        return serde_json::from_value(m.clone()).ok().map(Reply::Message);
    }
    task_of(v).map(Reply::Task)
}

/// A `Task` a peer sent us, read with the spec's own type.
///
/// `None` is a reply we could not read as a task at all — which the caller
/// surfaces as an error rather than treating as "not finished yet", because a
/// peer we cannot parse is not a peer we should keep polling. A `{message}`
/// reply is not a task either, however leniently its fields would default.
pub fn task_of(v: &Value) -> Option<Task> {
    if v.get("message").is_some() {
        return None;
    }
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

/// The readable pieces of `parts`, in order: a text part as its text, a
/// DataPart as its document in compact JSON. `raw` is the same parts array as
/// the peer sent it, where a document is taken from so its numbers read as
/// written (see the module note). File parts carry nothing a model can read
/// as an answer and are skipped.
fn rendered(parts: &[Part], raw: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        match &part.content {
            Some(Content::Text(t)) => out.push(t.clone()),
            Some(Content::Data(d)) => {
                let doc = raw
                    .and_then(|r| r.get(i))
                    .and_then(|p| p.get("data"))
                    .cloned()
                    .or_else(|| serde_json::to_value(d).ok());
                if let Some(doc) = doc {
                    out.push(doc.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// The **distillate**: the text of a finished task's artifacts, in order,
/// newline-joined, with a DataPart rendered as its JSON document. This is what
/// the delegating model receives as the answer, so it is deliberately the whole
/// of what the peer produced rather than the first artifact — a peer that
/// answers in two parts has not answered in one, and a peer that answers with
/// a document has answered.
pub fn artifact_text_of(v: &Value) -> String {
    let Some(task) = task_of(v) else {
        return String::new();
    };
    let raw_task = v.get("task").unwrap_or(v);
    let mut out = Vec::new();
    for (j, artifact) in task.artifacts.iter().enumerate() {
        let raw = raw_task.pointer(&format!("/artifacts/{j}/parts"));
        out.extend(rendered(&artifact.parts, raw));
    }
    out.join("\n")
}

/// The answer a reply carries: a `{message}` reply's parts, rendered as
/// [`artifact_text_of`] renders an artifact's, or a task's distillate.
pub fn reply_text_of(v: &Value) -> String {
    match reply_of(v) {
        Some(Reply::Message(m)) => rendered(&m.parts, v.pointer("/message/parts")).join("\n"),
        _ => artifact_text_of(v),
    }
}

/// One `artifactUpdate` stream frame: the artifact it belongs to, whether it
/// appends to that artifact's earlier chunks, and its rendered text.
pub fn artifact_update_of(update: &Value) -> Option<(String, bool, String)> {
    let ev: TaskArtifactUpdateEvent = serde_json::from_value(update.clone()).ok()?;
    let artifact = ev.artifact.as_option()?;
    let text = rendered(&artifact.parts, update.pointer("/artifact/parts")).join("\n");
    Some((artifact.artifact_id.clone(), ev.append, text))
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

/// `major.minor` of a protocol version; a patch component is ignored, as the
/// listener's `A2A-Version` gate ignores it.
fn major_minor(v: &str) -> Option<(&str, &str)> {
    let mut it = v.trim().split('.');
    Some((it.next()?, it.next()?))
}

/// A peer's Agent Card, read with the SDK's `AgentCard`: what a client must
/// consult before it speaks — which interface, which tenant, whether the peer
/// streams, and which extensions it will not work without.
pub struct PeerCard(AgentCard);

impl PeerCard {
    /// Parse a card body. `None` is a body that is not an `AgentCard`, which
    /// the caller treats like a card it could not fetch.
    pub fn parse(body: &[u8]) -> Option<PeerCard> {
        serde_json::from_slice(body).ok().map(PeerCard)
    }

    /// The first interface this client can speak to the configured endpoint:
    /// JSON-RPC at A2A 1.0 for an `http(s)://` peer, and agentd's declared unix
    /// binding at 1.0 for a `unix://` one — a unix socket is never labelled
    /// JSONRPC, and an HTTP peer's unix interface is not one we can dial.
    ///
    /// The interface's `url` selects nothing: agentd always dials the URL the
    /// operator configured, so a card cannot redirect a delegation (or the
    /// credentials it carries) somewhere else. What the selection decides is
    /// whether the peer speaks this protocol at all, and under which tenant.
    pub fn select_interface(&self, unix: bool) -> Result<&AgentInterface, String> {
        let want_binding = if unix {
            UNIX_BINDING
        } else {
            PROTOCOL_BINDING_JSONRPC
        };
        let want_version = major_minor(A2A_PROTOCOL_VERSION);
        let mut found = Vec::new();
        for i in &self.0.supported_interfaces {
            if i.protocol_binding == want_binding
                && major_minor(&i.protocol_version) == want_version
            {
                return Ok(i);
            }
            found.push(format!("{}@{}", i.protocol_binding, i.protocol_version));
        }
        Err(format!(
            "a2a: peer card advertises no {want_binding} interface at A2A {A2A_PROTOCOL_VERSION} (found: {})",
            if found.is_empty() {
                "none".to_string()
            } else {
                found.join(", ")
            }
        ))
    }

    /// The extensions the peer declares `required`.
    pub fn required_extensions(&self) -> impl Iterator<Item = &str> {
        self.0
            .capabilities
            .as_option()
            .into_iter()
            .flat_map(|c| c.extensions.iter())
            .filter(|e| e.required)
            .map(|e| e.uri.as_str())
    }

    /// The first extension the peer requires that this client does not
    /// implement. A client must not talk to a peer on terms it cannot honour,
    /// so this is a refusal, not a warning.
    pub fn requires_unknown_extension<'a>(&'a self, known: &[&str]) -> Option<&'a str> {
        self.required_extensions().find(|uri| !known.contains(uri))
    }

    /// Whether the peer serves `SendStreamingMessage`. Only an explicit `true`
    /// counts: the spec has a peer answer the streaming methods with
    /// UnsupportedOperation unless its card says it streams.
    pub fn streams(&self) -> bool {
        self.0
            .capabilities
            .as_option()
            .and_then(|c| c.streaming)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_objective_goes_out_as_a_user_message() {
        let p = send_message_params_cmd(
            "summarise the incident",
            None,
            Some("one paragraph"),
            "m1",
            None,
            true,
        );
        let m = &p["message"];
        // Built by their type, so the role is the proto enum name — a peer that
        // deserializes strictly would refuse anything else.
        assert_eq!(m["role"], "ROLE_USER");
        assert_eq!(m["messageId"], "m1");
        assert_eq!(m["parts"][0]["text"], "summarise the incident");
        assert_eq!(m["parts"][1]["text"], "Required output: one paragraph");
        assert_eq!(p["configuration"]["returnImmediately"], true);
        // A plain objective uses no extension, so nothing marks it.
        assert!(extensions_of(&p).is_empty(), "{p}");
        assert!(p.get("tenant").is_none(), "no tenant unless declared: {p}");
        // …and it round-trips back through the same type.
        let back: SendMessageRequest = serde_json::from_value(p.clone()).expect("their request");
        assert_eq!(back.message.as_option().map(|m| m.parts.len()), Some(2));
    }

    /// The plain `send` path: one typed builder, whatever shape the caller
    /// handed over. A command is marked as the extension it belongs to, and a
    /// document keeps its numbers exactly.
    #[test]
    fn a_send_is_a_typed_user_message_that_returns_immediately() {
        let p = send_message_params_parts(&json!("hello"), Some("conv-1"), "m2", Some("t1"))
            .expect("text");
        assert_eq!(p["message"]["role"], "ROLE_USER");
        assert_eq!(p["message"]["contextId"], "conv-1");
        assert_eq!(p["message"]["parts"], json!([{"text": "hello"}]));
        assert_eq!(p["configuration"]["returnImmediately"], true);
        assert_eq!(p["tenant"], "t1");
        assert!(extensions_of(&p).is_empty());

        let seq = 9_007_199_254_740_993u64;
        let cmd = json!([{"data": {"agentd": {"op": "stream.forwarded", "seq": seq}}}]);
        let p = send_message_params_parts(&cmd, None, "m3", None).expect("command");
        assert_eq!(extensions_of(&p), vec![COMMAND_EXTENSION.to_string()]);
        assert_eq!(
            p["message"]["parts"][0]["data"]["agentd"]["seq"],
            json!(seq)
        );

        let bad = send_message_params_parts(&json!([{"text": 3}]), None, "m4", None);
        assert!(bad.is_err_and(|e| e.contains("part 0")));
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

    /// A command's answer comes back either way the spec allows: a read op
    /// answers with a `{message}` carrying a document, a run with a `{task}`
    /// whose artifact may itself be a document. Both read as the answer — and
    /// the message is never mistaken for a task that has not finished.
    #[test]
    fn command_replies_are_read_as_task_or_message() {
        let doc =
            json!({"runs": [{"run": "r1", "status": "running"}], "seq": 9_007_199_254_740_993u64});
        let as_message = crate::a2a::reply::read_reply("ctx", doc.clone());
        assert!(matches!(reply_of(&as_message), Some(Reply::Message(_))));
        assert!(task_of(&as_message).is_none(), "a message is not a task");
        assert_eq!(task_id_of(&as_message), "");
        let text = reply_text_of(&as_message);
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), doc);

        let as_task = json!({"task": {
            "id": "t-1", "contextId": "c",
            "status": {"state": "TASK_STATE_COMPLETED"},
            "artifacts": [{"artifactId": "a", "parts": [{"data": doc.clone()}, {"text": "done"}]}]
        }});
        assert!(matches!(reply_of(&as_task), Some(Reply::Task(_))));
        let text = reply_text_of(&as_task);
        let mut lines = text.lines();
        assert_eq!(
            serde_json::from_str::<Value>(lines.next().unwrap()).unwrap(),
            doc
        );
        assert_eq!(lines.next(), Some("done"));
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
