// SPDX-License-Identifier: AGPL-3.0-only
//! Anthropic Messages API adapter. Pure translation, no I/O — every dial goes
//! through `intel::client`, so this module stays testable without a network.
//!
//! Three differences from the OpenAI dialect are absorbed here and never
//! surface to the loop: `system` is a top-level field rather than a message,
//! tool calls and results are content *blocks* rather than message fields, and
//! tool arguments arrive as a JSON object rather than a stringified one.

use crate::wire::intel::{Message, ReasoningParts, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

pub const DEFAULT_PATH: &str = "/v1/messages";
/// The dialect's name in a reasoning part's origin.
pub const NAME: &str = "anthropic";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub fn build_request(req: &Request, token: Option<&str>) -> (Vec<u8>, Vec<(String, String)>) {
    // System messages are hoisted into the top-level `system` field.
    let system: String = req
        .messages
        .iter()
        .filter_map(|m| match m {
            Message::System(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter_map(|m| message_to_anthropic(m, &req.model))
        .collect();

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    if !system.is_empty() {
        body.insert("system".into(), json!(system));
    }
    // One `output_config` carries both the thinking's effort and the
    // answer's format: the API takes one object, so each fills its key.
    let mut output_config = Map::new();
    match &req.reasoning {
        // Current Claude models think adaptively, at an effort
        // (`output_config.effort`); a model that names a `budget` is asked
        // with the older budget form, which only models before adaptive
        // thinking take. Either way `max_tokens` leaves room for the
        // thinking above the answer, and no temperature is sent.
        Some(r) => {
            match r.budget {
                Some(budget) => {
                    body.insert(
                        "thinking".into(),
                        json!({"type": "enabled", "budget_tokens": budget}),
                    );
                }
                None => {
                    body.insert("thinking".into(), json!({"type": "adaptive"}));
                    output_config.insert("effort".into(), json!(r.effort.as_str()));
                }
            }
            body.insert(
                "max_tokens".into(),
                json!(req.max_tokens as u64 + r.budget_tokens()),
            );
        }
        // No temperature either: current Claude models refuse one (400),
        // and the models that take one run at their own default.
        None => {
            body.insert("max_tokens".into(), json!(req.max_tokens));
        }
    }
    // The answer's schema, when it has a constrained form; one that has
    // none (a free-form object, `oneOf`, …) is asked in the prompt alone.
    if let Some(format) = output_format(req) {
        output_config.insert("format".into(), format);
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }
    body.insert("messages".into(), json!(messages));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
            .collect();
        body.insert("tools".into(), json!(tools));
    }

    let bytes = serde_json::to_vec(&Value::Object(body)).unwrap_or_default();
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        (
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        ),
    ];
    if let Some(tok) = token {
        headers.push(("x-api-key".to_string(), tok.to_string()));
    }
    (bytes, headers)
}

/// The `output_config.format` a request's schema is asked with — on the
/// Messages API, and for a Claude model on Bedrock — or `None` when it has no
/// schema or the schema no constrained form.
pub fn output_format(req: &Request) -> Option<Value> {
    let schema = super::structured::constrained(req.output_schema.as_ref()?)?;
    Some(json!({"type": "json_schema", "schema": schema}))
}

fn message_to_anthropic(m: &Message, model: &str) -> Option<Value> {
    match m {
        Message::System(_) => None, // hoisted into `system`
        Message::User(s) => Some(json!({"role": "user", "content": s})),
        Message::Assistant {
            text,
            tool_calls,
            reasoning,
        } => {
            // This model's own reasoning goes back exactly as it came — every
            // block, thinking interleaved with tool use, in order: its
            // signatures bind it to this model and this conversation.
            if let Some(content) = ReasoningParts::for_model(reasoning, NAME, model) {
                return Some(json!({"role": "assistant", "content": content}));
            }
            let mut blocks: Vec<Value> = Vec::new();
            if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                blocks.push(json!({"type": "text", "text": t}));
            }
            for tc in tool_calls {
                blocks.push(json!({"type": "tool_use", "id": tc.id, "name": tc.name, "input": tc.arguments}));
            }
            Some(json!({"role": "assistant", "content": blocks}))
        }
        Message::ToolResult {
            id,
            content,
            is_error,
        } => Some(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": id,
                "content": content,
                "is_error": is_error,
            }]
        })),
    }
}

pub fn parse_response(body: &[u8]) -> Result<Response, String> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| format!("intel: bad JSON response: {e}"))?;

    if v.get("type").and_then(Value::as_str) == Some("error") {
        let msg = v
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(format!("intel: provider error: {msg}"));
    }

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls = Vec::new();
    let mut thought = false;
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("thinking") | Some("redacted_thinking") => thought = true,
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        text_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    tool_calls.push(ToolCall {
                        id: b
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        name: b
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        arguments: b.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                _ => {}
            }
        }
    }

    let stop_reason = match v.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        _ => StopReason::Other,
    };

    let usage = v.get("usage").map(|u| Usage {
        input_tokens: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
    });

    let text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join(""))
    };
    Ok(Response {
        text,
        tool_calls,
        stop_reason,
        usage: usage.unwrap_or_default(),
        fallback: None,
        decision: None,
        // A message that thought is kept whole, in its order, to go back to
        // this model while the turn goes on; the endpoint stamps its origin.
        reasoning: thought.then(|| ReasoningParts {
            origin: String::new(),
            content: v
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::intel::ToolDef;

    /// Current models think adaptively at an effort; a model naming a
    /// `budget` is asked with the budget form. `max_tokens` leaves room for
    /// the thinking, and no temperature is sent. A message that thought is
    /// kept whole and in order, and goes back verbatim to the model that
    /// wrote it — to no other.
    #[test]
    fn thinking_is_asked_for_kept_and_returned_to_its_own_model() {
        use crate::wire::intel::{Effort, ReasoningAsk};
        let ask = |budget: Option<u64>| Request {
            model: "claude-opus-5-5".into(),
            messages: vec![Message::user("hi")],
            max_tokens: 1000,
            temperature: Some(0.0),
            reasoning: Some(ReasoningAsk {
                effort: Effort::High,
                budget,
            }),
            ..Default::default()
        };
        let body =
            |r: &Request| -> Value { serde_json::from_slice(&build_request(r, None).0).unwrap() };
        let adaptive = body(&ask(None));
        assert_eq!(adaptive["thinking"], json!({"type": "adaptive"}));
        assert_eq!(adaptive["output_config"]["effort"], "high");
        assert_eq!(adaptive["max_tokens"], 1000 + 24_576);
        assert!(adaptive.get("temperature").is_none(), "{adaptive}");
        let budgeted = body(&ask(Some(4096)));
        assert_eq!(
            budgeted["thinking"],
            json!({"type": "enabled", "budget_tokens": 4096})
        );
        assert_eq!(budgeted["max_tokens"], 5096);
        assert!(budgeted.get("output_config").is_none());

        let answer = json!({"content": [
            {"type": "thinking", "thinking": "first, the dates", "signature": "sig-1"},
            {"type": "tool_use", "id": "t1", "name": "lookup", "input": {}},
            {"type": "thinking", "thinking": "then the totals", "signature": "sig-2"},
            {"type": "tool_use", "id": "t2", "name": "sum", "input": {}}
        ], "stop_reason": "tool_use"});
        let mut r = parse_response(answer.to_string().as_bytes()).unwrap();
        assert_eq!(r.tool_calls.len(), 2);
        let parts = r.reasoning.as_mut().unwrap();
        parts.origin = ReasoningParts::origin(NAME, "claude-opus-5-5");
        let msg = Message::Assistant {
            text: None,
            tool_calls: r.tool_calls.clone(),
            reasoning: r.reasoning.clone(),
        };
        let same = message_to_anthropic(&msg, "claude-opus-5-5").unwrap();
        assert_eq!(
            same["content"], answer["content"],
            "verbatim, interleaved, in order"
        );
        let other = message_to_anthropic(&msg, "claude-haiku-4-5").unwrap();
        assert!(
            other["content"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["type"] == "tool_use"),
            "another model is sent the message without them: {other}"
        );
    }

    /// A schema-bearing request asks for the answer in `output_config.format`
    /// — beside the thinking's effort in the same object — closed and
    /// stripped; a schema with no constrained form is asked in the prompt.
    #[test]
    fn an_output_schema_is_asked_in_output_config_beside_the_effort() {
        use crate::wire::intel::{Effort, ReasoningAsk};
        let schema = json!({"type": "object", "properties": {
            "answer": {"type": "string", "enum": ["approve", "deny", "UNDECIDED"]},
            "why": {"type": "string", "minLength": 1}
        }, "required": ["answer"]});
        let mut req = Request {
            model: "claude-opus-5-5".into(),
            messages: vec![Message::user("hi")],
            max_tokens: 100,
            reasoning: Some(ReasoningAsk {
                effort: Effort::Low,
                budget: None,
            }),
            output_schema: Some(schema),
            ..Default::default()
        };
        let body =
            |r: &Request| -> Value { serde_json::from_slice(&build_request(r, None).0).unwrap() };
        let v = body(&req);
        assert_eq!(v["output_config"]["effort"], "low", "{v}");
        let format = &v["output_config"]["format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["schema"]["additionalProperties"], false);
        assert_eq!(
            format["schema"]["properties"]["why"],
            json!({"type": "string"})
        );

        // Without reasoning, the format alone.
        req.reasoning = None;
        let v = body(&req);
        assert_eq!(v["output_config"].as_object().unwrap().len(), 1, "{v}");
        assert!(v["output_config"].get("format").is_some());

        // A free-form answer has no constrained form: no format, and with no
        // effort either, no output_config at all.
        req.output_schema = Some(json!({"type": "object"}));
        assert!(body(&req).get("output_config").is_none());
    }

    /// No temperature reaches a Claude model: current ones refuse it.
    #[test]
    fn a_claude_model_is_sent_no_temperature() {
        let req = Request {
            model: "claude-sonnet-5".into(),
            messages: vec![Message::user("hi")],
            max_tokens: 64,
            temperature: Some(0.0),
            ..Default::default()
        };
        let v: Value = serde_json::from_slice(&build_request(&req, None).0).unwrap();
        assert!(v.get("temperature").is_none(), "{v}");
        assert_eq!(v["max_tokens"], 64);
    }

    #[test]
    fn build_hoists_system_and_headers() {
        let req = Request {
            model: "claude-x".into(),
            messages: vec![Message::system("be terse"), Message::user("hi")],
            tools: vec![ToolDef {
                name: "t".into(),
                description: "d".into(),
                input_schema: json!({}),
            }],
            max_tokens: 100,
            temperature: None,
            decision: None,
            reasoning: None,
            output_schema: None,
        };
        let (body, headers) = build_request(&req, Some("sk-ant"));
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["system"], "be terse");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["tools"][0]["name"], "t");
        assert!(
            headers
                .iter()
                .any(|(k, val)| k == "x-api-key" && val == "sk-ant")
        );
        assert!(headers.iter().any(|(k, _)| k == "anthropic-version"));
    }

    #[test]
    fn parse_text_and_tool_use() {
        let body = br#"{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"tu_1","name":"read","input":{"p":1}}],"stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":7}}"#;
        let r = parse_response(body).unwrap();
        assert_eq!(r.text.as_deref(), Some("hi"));
        assert_eq!(r.tool_calls[0].name, "read");
        assert_eq!(r.tool_calls[0].arguments["p"], 1);
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.total(), 12);
    }
}
