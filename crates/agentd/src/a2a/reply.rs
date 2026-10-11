// SPDX-License-Identifier: AGPL-3.0-only
//! Replies to command ops that answer with a Message rather than a Task.
//!
//! A read creates no task: `SendMessage` may answer with a Message when no
//! work is tracked, and a display client polls these reads — a durable task
//! per poll filled the task list every client of the same principal lists.
//! The Message is built from the A2A SDK's own types and put on the wire by its
//! canonical ProtoJSON, so its shape is the specification's
//! `SendMessageResponse{message}` by construction rather than by agentd
//! spelling `role` or `mediaType` the way it believes the proto does.

use a2a_types::{Message, Part, Role};
use serde_json::{Value, json};

/// The `{message}` answer to a read op: `doc` as one JSON DataPart of an
/// agent message in `context_id`, marked with the command extension that
/// defines the document.
///
/// The document itself is placed after serialization rather than carried
/// through the typed part: the SDK holds a DataPart as a
/// `google.protobuf.Value`, whose only number is a double, so a sequence
/// cursor or a millisecond timestamp would come back as `3.0` — and one past
/// 2^53 would come back wrong. JSON is a valid rendering of a `Value` as it
/// stands, so the bytes a caller reads are the document the op produced.
pub fn read_reply(context_id: &str, doc: Value) -> Value {
    let mut message = Message::new(
        Role::Agent,
        vec![Part::data(json!({})).with_media_type("application/json")],
    );
    message.message_id = format!("msg-{}", crate::state::ulid::new());
    message.context_id = Some(context_id.to_string());
    message.extensions = Some(vec![crate::runtime::surface::COMMAND_EXTENSION.to_string()]);
    let mut message = crate::a2a::wire::message_to_wire(&message);
    if let Some(part) = message
        .get_mut("parts")
        .and_then(|p| p.get_mut(0))
        .and_then(Value::as_object_mut)
    {
        part.insert("data".to_string(), doc);
    }
    json!({"message": message})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply reads back as the SDK's own `SendMessageResponse` — exactly
    /// one of task or message, the message an agent's, carrying the document
    /// unchanged in a JSON DataPart.
    #[test]
    fn a_read_reply_is_a_spec_message_carrying_the_document() {
        let doc = json!({"runs": [{"run": "r1", "status": "running"}], "n": 3});
        let v = read_reply("ctx-1", doc.clone());
        let resp: a2a_types::SendMessageResponse =
            crate::a2a::wire::from_wire(v.clone()).expect("a SendMessageResponse");
        // An integer past 2^53 is carried exactly, which a protobuf double
        // would not do — a JSON reader gets the cursor the op produced.
        let big = json!({"seq": 9_007_199_254_740_993u64});
        assert_eq!(
            read_reply("ctx-1", big.clone())["message"]["parts"][0]["data"],
            big
        );
        assert!(v.get("task").is_none(), "a read creates no task: {v}");
        let m = &v["message"];
        assert_eq!(m["role"], "ROLE_AGENT");
        assert_eq!(m["contextId"], "ctx-1");
        assert!(
            m["messageId"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg-"))
        );
        assert_eq!(m["parts"][0]["data"], doc);
        assert_eq!(m["parts"][0]["mediaType"], "application/json");
        assert_eq!(
            m["extensions"],
            json!([crate::runtime::surface::COMMAND_EXTENSION])
        );
        drop(resp);
    }
}
