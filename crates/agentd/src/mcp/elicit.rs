// SPDX-License-Identifier: AGPL-3.0-only
//! **Elicitation → `ask_human`**: letting an MCP server ask the operator.
//!
//! MCP servers may send the client an `elicitation/create` request — "I need a
//! value from the person before I can continue". agentd is unusually well
//! placed to answer one: `ask_human` already suspends the asker, renders the
//! question as an answerable row in every attached display client, survives a
//! daemon restart, and has a configured fallback for when nobody is watching.
//! Elicitation is that machinery with a different caller.
//!
//! The wiring problem is where the two live. MCP connections are held by the
//! **turn worker child** (the supervisor makes no model or MCP calls), while
//! `ask_human` runs on the **supervisor**, which owns the tasks and the gates.
//! The child already has a round-trip for exactly this — `AgentMsg::ToolRequest`
//! out, a reply slot back — and both halves of it are shareable (`Arc<Mutex<_>>`
//! writer, `Arc<Replies>`), so the handler runs on the MCP event thread and
//! blocks there rather than on the agent's own thread. A server waiting on an
//! elicitation therefore does not stall the turn that is talking to it.
//!
//! **What we can honestly promise about the schema.** The spec wants the
//! response `content` to match the server's `requestedSchema`. A human answers
//! in prose. So: a reply that parses as a JSON object is passed through; a
//! single-property schema binds the text (coerced to the declared primitive);
//! anything else cannot be guaranteed to conform, and we return `cancel` rather
//! than hand a server data that violates the contract it asked for.

use crate::subagent::control::{Up, send_up};
use crate::subagent::protocol::AgentMsg;
use crate::subagent::replies::{Replies, Reply};
use mcp::inbound::{Answer, Handler, Inbound};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bridges an MCP server's elicitation to the supervisor's `ask_human`.
pub struct ElicitationBridge {
    up: Up,
    replies: Arc<Replies>,
    cancel: Arc<AtomicBool>,
    /// How long to wait for a human before giving the server a `cancel`. The
    /// gate itself may outlive this; the server does not have to.
    timeout: Duration,
}

impl ElicitationBridge {
    pub fn new(
        up: Up,
        replies: Arc<Replies>,
        cancel: Arc<AtomicBool>,
        timeout: Duration,
    ) -> ElicitationBridge {
        ElicitationBridge {
            up,
            replies,
            cancel,
            timeout,
        }
    }
}

impl Handler for ElicitationBridge {
    fn handle(&self, req: Inbound) -> Option<Answer> {
        let Inbound::Elicit {
            message,
            requested_schema,
        } = req
        else {
            // `roots/list` is not wired: we do not advertise the capability, so
            // this arm is unreachable in practice.
            return None;
        };
        if self.cancel.load(Ordering::Relaxed) {
            return Some(Answer::Cancel);
        }

        let id = self.replies.next_id();
        send_up(
            &self.up,
            &AgentMsg::ToolRequest {
                id,
                name: "ask_human".to_string(),
                args: json!({
                    "question": message,
                    "schema": requested_schema.clone(),
                }),
            },
        );

        let deadline = Instant::now() + self.timeout;
        match self.replies.wait(id, deadline, &self.cancel) {
            Some(Reply::Tool { result, is_error }) => {
                if is_error {
                    // The ask failed or the configured fallback refused it.
                    return Some(Answer::Cancel);
                }
                if result
                    .get("timed_out")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return Some(Answer::Cancel);
                }
                Some(shape_reply(
                    result.get("reply").unwrap_or(&Value::Null),
                    &requested_schema,
                ))
            }
            // Cancelled, channel gone, or past the deadline.
            _ => Some(Answer::Cancel),
        }
    }
}

/// Fit a human's answer to the server's `requestedSchema`, or decline to.
///
/// Kept pure and separately tested: it is the only place where a free-text
/// answer meets a typed contract, and getting it wrong means handing a server
/// data that violates the schema it asked for.
pub(crate) fn shape_reply(reply: &Value, schema: &Value) -> Answer {
    // Already structured (a client answered with JSON, or the gate carried an
    // object through) — pass it on.
    if reply.is_object() {
        return Answer::Accept(reply.clone());
    }

    let text = match reply {
        Value::String(s) => s.trim().to_string(),
        Value::Null => return Answer::Cancel,
        other => other.to_string(),
    };
    if text.is_empty() {
        return Answer::Cancel;
    }

    // A human may have typed JSON at a JSON-shaped question.
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&text) {
        return Answer::Accept(Value::Object(m));
    }

    // A single-property schema is unambiguous: the answer IS that property.
    let props = schema.get("properties").and_then(Value::as_object);
    if let Some(props) = props
        && props.len() == 1
        && let Some((key, spec)) = props.iter().next()
    {
        let declared = spec.get("type").and_then(Value::as_str).unwrap_or("string");
        if let Some(v) = coerce(&text, declared) {
            return Answer::Accept(json!({ key: v }));
        }
        return Answer::Cancel;
    }

    // Multi-property (or unschema'd) — prose cannot be guaranteed to conform,
    // and a server that asked for a shape deserves a refusal over a violation.
    Answer::Cancel
}

/// Coerce a human's text to the schema's declared primitive. `None` when it
/// plainly is not one (a word where a number was demanded).
fn coerce(text: &str, declared: &str) -> Option<Value> {
    match declared {
        "string" => Some(Value::String(text.to_string())),
        "boolean" => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" | "ok" => Some(Value::Bool(true)),
            "false" | "no" | "n" | "0" => Some(Value::Bool(false)),
            _ => None,
        },
        "integer" => text.parse::<i64>().ok().map(|n| json!(n)),
        "number" => text
            .parse::<f64>()
            .ok()
            .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_prop(name: &str, ty: &str) -> Value {
        json!({"type": "object", "properties": { name: {"type": ty} }})
    }

    fn accepted(a: Answer) -> Value {
        match a {
            Answer::Accept(v) => v,
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn a_structured_reply_passes_through() {
        let r = shape_reply(&json!({"env": "staging"}), &one_prop("env", "string"));
        assert_eq!(accepted(r)["env"], "staging");
    }

    #[test]
    fn a_single_property_schema_binds_the_text() {
        let r = shape_reply(&json!("staging"), &one_prop("env", "string"));
        assert_eq!(accepted(r), json!({"env": "staging"}));

        // …coercing to the declared primitive.
        let r = shape_reply(&json!("42"), &one_prop("count", "integer"));
        assert_eq!(accepted(r), json!({"count": 42}));
        let r = shape_reply(&json!("yes"), &one_prop("confirm", "boolean"));
        assert_eq!(accepted(r), json!({"confirm": true}));

        // A word where a number was demanded is not silently stringified.
        assert!(matches!(
            shape_reply(&json!("soon"), &one_prop("count", "integer")),
            Answer::Cancel
        ));
    }

    #[test]
    fn typed_json_is_honoured_over_the_single_property_shortcut() {
        let r = shape_reply(
            &json!(r#"{"env":"prod","force":true}"#),
            &one_prop("env", "string"),
        );
        let v = accepted(r);
        assert_eq!(v["env"], "prod");
        assert_eq!(v["force"], true);
    }

    #[test]
    fn prose_against_a_multi_property_schema_is_declined_not_guessed() {
        // The server asked for a shape; handing it something that violates the
        // schema is worse than telling it nobody answered.
        let schema = json!({"type": "object", "properties": {
            "env": {"type": "string"}, "force": {"type": "boolean"}
        }});
        assert!(matches!(
            shape_reply(&json!("just do it on staging"), &schema),
            Answer::Cancel
        ));
    }

    #[test]
    fn nothing_to_say_is_a_cancel() {
        assert!(matches!(
            shape_reply(&Value::Null, &one_prop("x", "string")),
            Answer::Cancel
        ));
        assert!(matches!(
            shape_reply(&json!("   "), &one_prop("x", "string")),
            Answer::Cancel
        ));
    }
}
