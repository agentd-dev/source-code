// SPDX-License-Identifier: Apache-2.0
//! Anthropic Messages API adapter. RFC 0006 §two-adapters.
//!
//! The second (and last) in-binary dialect. Pure translation, no I/O. Differs
//! from OpenAI in three ways the loop never sees: `system` is a top-level
//! field (not a message), tool calls/results are content *blocks*, and tool
//! arguments are a JSON object (not a stringified one).

use crate::wire::intel::{Message, Request, Response, StopReason, ToolCall, Usage};
use serde_json::{Map, Value, json};

pub const DEFAULT_PATH: &str = "/v1/messages";
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
        .filter_map(message_to_anthropic)
        .collect();

    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("max_tokens".into(), json!(req.max_tokens));
    if !system.is_empty() {
        body.insert("system".into(), json!(system));
    }
    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
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

fn message_to_anthropic(m: &Message) -> Option<Value> {
    match m {
        Message::System(_) => None, // hoisted into `system`
        Message::User(s) => Some(json!({"role": "user", "content": s})),
        Message::Assistant { text, tool_calls } => {
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
    if let Some(blocks) = v.get("content").and_then(Value::as_array) {
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::intel::ToolDef;

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
